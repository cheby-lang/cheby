//! Tests of the `std::http` head kernels (D-1003, D-1013, D-1074,
//! D-1210): complete and partial heads, repeated and
//! mixed-case headers, heads over the limit and invalid bytes, for both
//! kernels, then the Cheby values the kernels return and their C
//! signatures against `std/http.cheby`. No test starts a runtime: the
//! kernels never wait.

use std::fmt::Write as _;
use std::path::Path;

use super::*;
use crate::alloc::live;
use crate::rc;

/// The pairs of `(name, value)` strings.
fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// `count` header lines `h<n>: <value of n>`.
fn header_lines(count: usize, value: impl Fn(usize) -> String) -> String {
    (0..count).fold(String::new(), |mut out, n| {
        write!(out, "h{n}: {}\r\n", value(n)).unwrap();
        out
    })
}

/// A response head with `headers`, then a body.
fn response_bytes(headers: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\n{headers}\r\nbody").into_bytes()
}

#[test]
fn http_head_response_complete() {
    let data = b"HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\ncontent-length: 2\r\n\r\nno";
    let head_length = data.len() - 2;
    assert_eq!(
        parse_response(data),
        HeadOutcome::Complete(
            ResponseHead {
                status: 404,
                headers: pairs(&[("content-type", "text/plain"), ("content-length", "2")]),
            },
            head_length
        )
    );
    // A status line without a reason phrase, and HTTP/1.0.
    let bare = b"HTTP/1.0 204\r\n\r\n";
    assert_eq!(
        parse_response(bare),
        HeadOutcome::Complete(
            ResponseHead {
                status: 204,
                headers: Vec::new(),
            },
            bare.len()
        )
    );
}

#[test]
fn http_head_response_partial() {
    let data = response_bytes("a: 1\r\nb: 2\r\n");
    let head_length = data.len() - 4;
    for end in 0..head_length {
        assert_eq!(
            parse_response(&data[..end]),
            HeadOutcome::Partial,
            "{end} bytes"
        );
    }
    assert!(matches!(
        parse_response(&data[..head_length]),
        HeadOutcome::Complete(_, length) if length == head_length
    ));
}

#[test]
fn http_head_response_repeated_and_mixed_case_headers() {
    let data =
        response_bytes("Set-Cookie: a=1\r\nX-MiXeD:  spaced \t\r\nset-cookie: b=2\r\nEmpty:\r\n");
    let HeadOutcome::Complete(head, _) = parse_response(&data) else {
        panic!("not complete");
    };
    assert_eq!(
        head.headers,
        pairs(&[
            ("set-cookie", "a=1"),
            ("x-mixed", "spaced"),
            ("set-cookie", "b=2"),
            ("empty", ""),
        ])
    );
}

#[test]
fn http_head_response_with_many_headers() {
    let lines = header_lines(500, |n| n.to_string());
    let HeadOutcome::Complete(head, _) = parse_response(&response_bytes(&lines)) else {
        panic!("not complete");
    };
    assert_eq!(head.headers.len(), 500);
    assert_eq!(
        head.headers.last(),
        Some(&("h499".to_owned(), "499".to_owned()))
    );
}

#[test]
fn http_head_response_over_the_limit() {
    // A head of exactly the limit is accepted; one byte more is not.
    let fixed = "HTTP/1.1 200 OK\r\nx: \r\n\r\n".len();
    let exact = response_bytes(&format!("x: {}\r\n", "a".repeat(HEAD_LIMIT - fixed)));
    assert!(matches!(
        parse_response(&exact),
        HeadOutcome::Complete(_, length) if length == HEAD_LIMIT
    ));
    let over = response_bytes(&format!("x: {}\r\n", "a".repeat(HEAD_LIMIT - fixed + 1)));
    assert!(matches!(
        parse_response(&over),
        HeadOutcome::Failed(TOO_LARGE, _)
    ));
    // Bytes past the limit that still hold no complete head.
    let mut unfinished = b"HTTP/1.1 200 OK\r\nx: ".to_vec();
    unfinished.resize(HEAD_LIMIT + 1, b'a');
    assert!(matches!(
        parse_response(&unfinished),
        HeadOutcome::Failed(TOO_LARGE, _)
    ));
    unfinished.truncate(HEAD_LIMIT);
    assert_eq!(parse_response(&unfinished), HeadOutcome::Partial);
}

#[test]
fn http_head_response_invalid_bytes() {
    for data in [
        &b"HTTP/1.1 abc OK\r\n\r\n"[..],
        b"HTTP/2.0 200 OK\r\n\r\n",
        b"<html>\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nbad name: x\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nx: a\x00b\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nx: \xFF\xFE\r\n\r\n",
    ] {
        assert!(
            matches!(parse_response(data), HeadOutcome::Failed(INVALID, _)),
            "{}",
            String::from_utf8_lossy(data)
        );
    }
}

#[test]
fn http_head_request_complete() {
    let data = b"POST /a%20b?x=1 HTTP/1.1\r\nHost: example\r\nContent-Length: 3\r\n\r\none";
    assert_eq!(
        parse_request(data),
        HeadOutcome::Complete(
            RequestHead {
                method: "POST".to_owned(),
                target: "/a%20b?x=1".to_owned(),
                minor: 1,
                headers: pairs(&[("host", "example"), ("content-length", "3")]),
            },
            data.len() - 3
        )
    );
    let old = b"PURGE / HTTP/1.0\r\n\r\n";
    assert_eq!(
        parse_request(old),
        HeadOutcome::Complete(
            RequestHead {
                method: "PURGE".to_owned(),
                target: "/".to_owned(),
                minor: 0,
                headers: Vec::new(),
            },
            old.len()
        )
    );
}

#[test]
fn http_head_request_partial() {
    let data = b"GET / HTTP/1.1\r\nhost: a\r\n\r\n";
    for end in 0..data.len() {
        assert_eq!(
            parse_request(&data[..end]),
            HeadOutcome::Partial,
            "{end} bytes"
        );
    }
}

#[test]
fn http_head_request_repeated_and_mixed_case_headers() {
    let data = b"GET / HTTP/1.1\r\nCookie: a=1\r\nCOOKIE: b=2\r\nX-Token: abc\r\n\r\n";
    let HeadOutcome::Complete(head, _) = parse_request(data) else {
        panic!("not complete");
    };
    assert_eq!(
        head.headers,
        pairs(&[("cookie", "a=1"), ("cookie", "b=2"), ("x-token", "abc")])
    );
}

#[test]
fn http_head_request_over_the_limit() {
    let target = format!("/{}", "a".repeat(HEAD_LIMIT));
    let data = format!("GET {target} HTTP/1.1\r\n\r\n");
    assert!(matches!(
        parse_request(data.as_bytes()),
        HeadOutcome::Failed(TOO_LARGE, _)
    ));
    let lines = header_lines(20_000, |_| "x".to_owned());
    let many = format!("GET / HTTP/1.1\r\n{lines}\r\n");
    assert!(matches!(
        parse_request(many.as_bytes()),
        HeadOutcome::Failed(TOO_LARGE, _)
    ));
}

#[test]
fn http_head_request_invalid_bytes() {
    for data in [
        &b"GET / HTTP/1.2\r\n\r\n"[..],
        b"GET\r\n\r\n",
        b"G(T / HTTP/1.1\r\n\r\n",
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03",
        b"GET / HTTP/1.1\r\nx y: 1\r\n\r\n",
        b"GET / HTTP/1.1\r\nx: \xC3\x28\r\n\r\n",
    ] {
        assert!(
            matches!(parse_request(data), HeadOutcome::Failed(INVALID, _)),
            "{}",
            String::from_utf8_lossy(data)
        );
    }
}

/// The text of a borrowed `String` word.
fn text(word: Word) -> String {
    // SAFETY: a live `String`, borrowed.
    unsafe { values::str_of(word) }.to_owned()
}

/// The `(name, value)` pairs of a borrowed `List<(String, String)>`.
fn header_list(word: Word) -> Vec<(String, String)> {
    // SAFETY: a live list of live pairs of strings, borrowed.
    unsafe {
        list::iter(word)
            .map(|pair| {
                let parts = values::read_tuple(pair);
                (text(parts[0]), text(parts[1]))
            })
            .collect()
    }
}

#[test]
fn http_head_response_kernel_builds_cheby_values() {
    let before = live();
    let bytes = b"HTTP/1.1 301 Moved\r\nLocation: /new\r\n\r\nrest";
    let head = values::bytes(bytes);
    // SAFETY: a live `Bytes`, borrowed for the call; the results are owned
    // and dropped.
    unsafe {
        let result = cheby_http_sys_parse_response_head(head);
        let Ok(found) = values::read_result(result) else {
            panic!("an error");
        };
        let Some(parts) = values::read_option(found) else {
            panic!("partial");
        };
        let parts = values::read_tuple(parts);
        assert_eq!(values::word_to_i64(parts[0]), 301);
        assert_eq!(header_list(parts[1]), pairs(&[("location", "/new")]));
        assert_eq!(
            values::word_to_i64(parts[2]),
            i64::try_from(bytes.len() - 4).unwrap()
        );
        rc::drop(result);
        let partial = values::bytes(b"HTTP/1.1 200");
        let result = cheby_http_sys_parse_response_head(partial);
        let Ok(found) = values::read_result(result) else {
            panic!("an error");
        };
        assert_eq!(values::read_option(found), None);
        rc::drop(result);
        rc::drop(partial);
        let invalid = values::bytes(b"nonsense\r\n\r\n");
        let result = cheby_http_sys_parse_response_head(invalid);
        let Err(pair) = values::read_result(result) else {
            panic!("not an error");
        };
        let pair = values::read_tuple(pair);
        assert_eq!(values::word_to_i64(pair[0]), INVALID);
        assert!(text(pair[1]).starts_with("invalid HTTP head"));
        rc::drop(result);
        rc::drop(invalid);
        rc::drop(head);
    }
    assert_eq!(live(), before);
}

#[test]
fn http_head_request_kernel_builds_cheby_values() {
    let before = live();
    let bytes = b"DELETE /orders/17 HTTP/1.1\r\nX-Token: abc\r\n\r\n";
    let head = values::bytes(bytes);
    // SAFETY: as in the response's test.
    unsafe {
        let result = cheby_http_sys_parse_request_head(head);
        let Ok(found) = values::read_result(result) else {
            panic!("an error");
        };
        let Some(parts) = values::read_option(found) else {
            panic!("partial");
        };
        let parts = values::read_tuple(parts);
        assert_eq!(text(parts[0]), "DELETE /orders/17 HTTP/1.1");
        assert_eq!(header_list(parts[1]), pairs(&[("x-token", "abc")]));
        assert_eq!(
            values::word_to_i64(parts[2]),
            i64::try_from(bytes.len()).unwrap()
        );
        rc::drop(result);
        // HTTP/1.0 keeps its digit in the line.
        let old = values::bytes(b"GET /a?b HTTP/1.0\r\n\r\n");
        let result = cheby_http_sys_parse_request_head(old);
        let Ok(found) = values::read_result(result) else {
            panic!("an error");
        };
        let parts = values::read_tuple(values::read_option(found).unwrap());
        assert_eq!(text(parts[0]), "GET /a?b HTTP/1.0");
        rc::drop(old);
        rc::drop(result);
        let large = values::bytes(&vec![b'a'; HEAD_LIMIT + 10]);
        let result = cheby_http_sys_parse_request_head(large);
        let Err(pair) = values::read_result(result) else {
            panic!("not an error");
        };
        assert_eq!(values::word_to_i64(values::read_tuple(pair)[0]), TOO_LARGE);
        rc::drop(result);
        rc::drop(large);
        rc::drop(head);
    }
    assert_eq!(live(), before);
}

/// Each kernel is declared in `std/http.cheby` with its `./http.mjs`
/// binding, without `@std_context` (D-983, D-986), and its one `Bytes`
/// parameter and its `Result` are uniform words, as `signatures` says.
#[test]
fn http_head_kernels_match_their_declarations() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std/http.cheby");
    let source = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = source.lines().collect();
    let registered = signatures();
    assert_eq!(registered.len(), 2);
    for (symbol, params, result) in registered {
        assert_eq!((params, result), (&["Word"][..], "Word"), "{symbol}");
        let name = symbol.strip_prefix("cheby_http_").unwrap();
        let native = format!("@external(native, \"{symbol}\")");
        let at = lines
            .iter()
            .position(|line| line.trim() == native)
            .unwrap_or_else(|| panic!("{symbol} is not declared"));
        assert_eq!(
            lines[at + 1].trim(),
            format!("@external(js, \"./http.mjs\", \"{name}\")")
        );
        // `cheby fmt` puts a long header's parameter on its own line.
        let header = lines[at + 2..at + 5]
            .iter()
            .map(|line| line.trim())
            .collect::<String>()
            .replace(",)", ")");
        let result = match name {
            "sys_parse_response_head" => {
                "Result<Option<(Int, List<(String, String)>, Int)>, (Int, String)>"
            }
            _ => "Result<Option<(String, List<(String, String)>, Int)>, (Int, String)>",
        };
        assert!(
            header.starts_with(&format!("fn {name}(head: bytes::Bytes) -> {result}"))
                || header.starts_with(&format!("priv fn {name}(head: bytes::Bytes) -> {result}")),
            "{header}"
        );
        assert!(
            !lines[at.saturating_sub(3)..at]
                .iter()
                .any(|line| line.contains("@std_context")),
            "{symbol} takes no context"
        );
    }
}
