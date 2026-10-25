//! The `std::http` head kernels (D-1003, D-985, D-986): the request and
//! response heads of HTTP/1.1, parsed by the `httparse` crate (D-1036).
//!
//! `std::http` is written in Cheby over `std::net` (D-1003, ADR-0066):
//! these two kernels are its only native code besides TLS. Each takes the
//! bytes read so far, borrowed, and returns what they hold. Neither waits
//! nor takes anything, so they run on the calling fiber's thread with no
//! context (D-983, D-985) and a cancellation never meets them.
//!
//! GAP(http-head-kernel): the kernels' shapes.
//!
//! - `sys_parse_response_head(head: Bytes)` returns `Ok(None)` until the
//!   bytes hold a complete head, then `Ok(Some((status, headers,
//!   length)))`: the status code, the headers in wire order with their
//!   names in lower case, and the head's length in bytes, its empty line
//!   included, so the body starts there.
//! - `sys_parse_request_head(head: Bytes)` is the same with the request
//!   line in place of the status: `Ok(Some((line, headers, length)))`, the
//!   line written `<method> <target> HTTP/1.<minor>` with single spaces,
//!   the method's token and the target as received, so the server splits
//!   it at its two spaces. (A tuple of five would be W0001's warning, D-243.)
//!   `std::http`'s client never calls it: the server does (S5), and
//!   `std/http.cheby` binds it package-visible before its caller, as
//!   D-1182 bound `sys_connect_tls`.
//! - A head that is not valid HTTP/1.1 (or 1.0) is `Err((400, message))`,
//!   and so is a header value that is not UTF-8 (D-311, D-1007). A head
//!   longer than [`HEAD_LIMIT`], or bytes past the limit that still hold
//!   no complete head, is `Err((431, message))` (D-1013, D-1074). The codes
//!   are the statuses the server answers with; the client reads both as
//!   `is_invalid_response`.
//! - Header values lose the spaces and tabs around them (RFC 9110 §5.5).

use httparse::{EMPTY_HEADER, Header, Status};

use crate::abi::Word;
use crate::io::err_pair;
use crate::unwind::guard::guarded;
use crate::{list, values};

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["http"];

/// The longest head either kernel accepts: 64 KiB, the server's limit
/// (D-1013) and the client's (D-1074).
pub(crate) const HEAD_LIMIT: usize = 64 * 1024;

/// The error kind of a head that is not valid HTTP/1.1.
pub(crate) const INVALID: i64 = 400;

/// The error kind of a head over [`HEAD_LIMIT`].
pub(crate) const TOO_LARGE: i64 = 431;

/// How many headers the first attempt makes room for; a head with more
/// is parsed again with room for every header that fits in the limit.
const FIRST_HEADERS: usize = 64;

/// The most headers a head within the limit can hold: each line has a
/// name, a colon and a line ending, at least four bytes.
const MAX_HEADERS: usize = HEAD_LIMIT / 4;

/// What a kernel found in the bytes it was given.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeadOutcome<T> {
    /// The bytes end before the head does.
    Partial,
    /// A complete head and its length in bytes, its empty line included.
    Complete(T, usize),
    /// The kind ([`INVALID`] or [`TOO_LARGE`]) and a message.
    Failed(i64, String),
}

/// A response's head.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResponseHead {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
}

/// A request's head.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RequestHead {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) minor: u8,
    pub(crate) headers: Vec<(String, String)>,
}

impl RequestHead {
    /// The request line as the kernel returns it: `<method> <target>
    /// HTTP/1.<minor>`.
    pub(crate) fn line(&self) -> String {
        format!("{} {} HTTP/1.{}", self.method, self.target, self.minor)
    }
}

/// One parse with room for `room` headers: `httparse`'s status with the
/// head's own parts, or its error.
type Attempt<T> = Result<Status<(T, usize)>, httparse::Error>;

/// Runs `parse` on at most one byte more than the limit, with room for
/// [`FIRST_HEADERS`] headers and then, if they did not fit, for
/// [`MAX_HEADERS`], and applies the limit to what it found.
fn parse_head<T>(
    data: &[u8],
    parse: impl for<'b> Fn(&mut [Header<'b>], &'b [u8]) -> Attempt<T>,
) -> HeadOutcome<T> {
    let window = data.get(..HEAD_LIMIT.saturating_add(1)).unwrap_or(data);
    let mut first = [EMPTY_HEADER; FIRST_HEADERS];
    let attempt = match parse(&mut first, window) {
        Err(httparse::Error::TooManyHeaders) => {
            let mut all = vec![EMPTY_HEADER; MAX_HEADERS];
            parse(&mut all, window)
        }
        other => other,
    };
    let too_large = || HeadOutcome::Failed(TOO_LARGE, "the head is over 64 KiB".to_owned());
    match attempt {
        Ok(Status::Complete((head, length))) if length <= HEAD_LIMIT => {
            HeadOutcome::Complete(head, length)
        }
        // A head over the limit, or more headers than fit in it.
        Ok(Status::Complete(_)) | Err(httparse::Error::TooManyHeaders) => too_large(),
        Ok(Status::Partial) if data.len() > HEAD_LIMIT => too_large(),
        Ok(Status::Partial) => HeadOutcome::Partial,
        Err(error) => invalid(&error.to_string()),
    }
}

/// An invalid head with `reason`.
fn invalid<T>(reason: &str) -> HeadOutcome<T> {
    HeadOutcome::Failed(INVALID, format!("invalid HTTP head: {reason}"))
}

/// The parsed headers as pairs: names in lower case, values without the
/// spaces and tabs around them, or `None` when a value is not UTF-8.
fn header_pairs(headers: &[Header<'_>]) -> Option<Vec<(String, String)>> {
    headers
        .iter()
        .take_while(|header| !header.name.is_empty())
        .map(|header| {
            let value = std::str::from_utf8(header.value).ok()?;
            Some((
                header.name.to_ascii_lowercase(),
                value.trim_matches([' ', '\t']).to_owned(),
            ))
        })
        .collect()
}

/// The bytes of a response head (module documentation).
pub(crate) fn parse_response(data: &[u8]) -> HeadOutcome<ResponseHead> {
    let outcome = parse_head(data, |headers, window| {
        let mut response = httparse::Response::new(headers);
        Ok(match response.parse(window)? {
            Status::Complete(length) => Status::Complete((
                (
                    response.code.unwrap_or_default(),
                    header_pairs(response.headers),
                ),
                length,
            )),
            Status::Partial => Status::Partial,
        })
    });
    match outcome {
        HeadOutcome::Complete((status, Some(headers)), length) => {
            HeadOutcome::Complete(ResponseHead { status, headers }, length)
        }
        HeadOutcome::Complete((_, None), _) => invalid("a header value is not UTF-8"),
        HeadOutcome::Partial => HeadOutcome::Partial,
        HeadOutcome::Failed(kind, message) => HeadOutcome::Failed(kind, message),
    }
}

/// The bytes of a request head (module documentation).
pub(crate) fn parse_request(data: &[u8]) -> HeadOutcome<RequestHead> {
    let outcome = parse_head(data, |headers, window| {
        let mut request = httparse::Request::new(headers);
        Ok(match request.parse(window)? {
            Status::Complete(length) => Status::Complete((
                (
                    request.method.unwrap_or_default().to_owned(),
                    request.path.unwrap_or_default().to_owned(),
                    request.version.unwrap_or_default(),
                    header_pairs(request.headers),
                ),
                length,
            )),
            Status::Partial => Status::Partial,
        })
    });
    match outcome {
        HeadOutcome::Complete((method, target, minor, Some(headers)), length) => {
            HeadOutcome::Complete(
                RequestHead {
                    method,
                    target,
                    minor,
                    headers,
                },
                length,
            )
        }
        HeadOutcome::Complete((_, _, _, None), _) => invalid("a header value is not UTF-8"),
        HeadOutcome::Partial => HeadOutcome::Partial,
        HeadOutcome::Failed(kind, message) => HeadOutcome::Failed(kind, message),
    }
}

/// A `List<(String, String)>` of the headers.
fn headers_word(headers: &[(String, String)]) -> Word {
    list::from_iter(
        headers
            .iter()
            .map(|(name, value)| values::tuple(&[values::string(name), values::string(value)])),
    )
}

/// A length as an `Int`.
fn length_word(length: usize) -> Word {
    values::int_word(i64::try_from(length).unwrap_or(i64::MAX))
}

/// The kernel's result for an outcome: `Ok(None)`, `Ok(Some(head))` with
/// `complete`'s tuple, or the error pair.
fn outcome_word<T>(outcome: HeadOutcome<T>, complete: impl FnOnce(T, usize) -> Word) -> Word {
    match outcome {
        HeadOutcome::Partial => values::ok(values::none()),
        HeadOutcome::Complete(head, length) => values::ok(values::some(complete(head, length))),
        HeadOutcome::Failed(kind, message) => err_pair(kind, &message),
    }
}

/// `sys_parse_response_head`'s kernel: `Ok(None)`, `Ok(Some((status,
/// headers, length)))` or `Err((kind, message))` (module documentation).
///
/// # Safety
///
/// `head` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_http_sys_parse_response_head(head: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let data = unsafe { values::bytes_of(head) };
        outcome_word(parse_response(data), |head, length| {
            values::tuple(&[
                values::int_word(i64::from(head.status)),
                headers_word(&head.headers),
                length_word(length),
            ])
        })
    })
}

/// `sys_parse_request_head`'s kernel: `Ok(None)`, `Ok(Some((line,
/// headers, length)))` or `Err((kind, message))` (module documentation).
///
/// # Safety
///
/// `head` is a live `Bytes`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_http_sys_parse_request_head(head: Word) -> Word {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let data = unsafe { values::bytes_of(head) };
        outcome_word(parse_request(data), |head, length| {
            values::tuple(&[
                values::string(&head.line()),
                headers_word(&head.headers),
                length_word(length),
            ])
        })
    })
}

/// One kernel: symbol, address, C parameter types and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// The `std::http` kernels; neither takes the context (D-983).
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 2] {
    const W: &str = "Word";
    [
        (
            "cheby_http_sys_parse_request_head",
            cheby_http_sys_parse_request_head as *const u8,
            &[W],
            W,
        ),
        (
            "cheby_http_sys_parse_response_head",
            cheby_http_sys_parse_response_head as *const u8,
            &[W],
            W,
        ),
    ]
}

/// Appends the `std::http` kernels, with their addresses, to `out` (plan
/// §4.3.3).
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The C signatures of the `std::http` kernels, for the check against
/// `std/http.cheby` in `http_tests.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
