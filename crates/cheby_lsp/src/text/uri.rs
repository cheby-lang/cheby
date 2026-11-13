//! `file:` URIs to paths and back (A §6.1).
//!
//! `lsp_types::Uri` has no path conversion, so this module has its own.
//! A path becomes `file://` and its bytes, with unreserved characters and
//! `/` kept and every other byte percent-encoded. A Windows drive keeps its
//! colon (`file:///C:/x`), and a UNC path names its server as the host
//! (`file://server/share/x`). Reading a URI decodes `%3A` and `%3a` in a
//! drive (`file:///c%3A/x`, as VS Code writes it), accepts an empty host
//! and `localhost`, and gives a UNC path for any other host on Windows.
//!
//! The rules take the platform as a parameter ([`Platform`]), so Windows
//! paths are tested on every platform.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::str::FromStr;

use lsp_types::Uri;

/// Whose path rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// `/`-separated paths from the root `/`, made of bytes.
    Unix,
    /// Drive (`C:\x`) and UNC (`\\server\share\x`) paths, `\` or `/`
    /// separated, made of Unicode text.
    Windows,
}

impl Platform {
    /// The platform this server runs on.
    pub const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };
}

/// Why a path has no `file:` URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UriError {
    /// The path is not absolute, so it names no file on its own.
    Relative(PathBuf),
    /// A Windows path that is not valid Unicode.
    NotUnicode(PathBuf),
    /// The URI made from the path did not parse, which is a bug here.
    Invalid(String),
}

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relative(path) => write!(f, "`{}` is not an absolute path", path.display()),
            Self::NotUnicode(path) => write!(f, "`{}` is not valid Unicode", path.display()),
            Self::Invalid(uri) => write!(f, "`{uri}` is not a valid URI"),
        }
    }
}

impl std::error::Error for UriError {}

/// The `file:` URI of `path`, which must be absolute.
///
/// # Errors
///
/// [`UriError::Relative`] for a path that is not absolute, and
/// [`UriError::NotUnicode`] for a Windows path that is not Unicode.
pub fn path_to_uri(path: &Path) -> Result<Uri, UriError> {
    let bytes = path_bytes(path).ok_or_else(|| UriError::NotUnicode(path.to_path_buf()))?;
    let text = to_uri_bytes(&bytes, Platform::CURRENT)
        .ok_or_else(|| UriError::Relative(path.to_path_buf()))?;
    Uri::from_str(&text).map_err(|_| UriError::Invalid(text))
}

/// The path `uri` names. `None` for another scheme, a host this platform
/// cannot name, a percent-escape that is broken or decodes to a separator
/// or NUL, or a Windows path that is not Unicode or has no drive or host.
/// A query and a fragment are ignored.
#[must_use]
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    bytes_to_path(to_path_bytes(uri.as_str(), Platform::CURRENT)?)
}

/// The `file:` URI of `path` under `platform`'s rules, or `None` when the
/// path is not absolute there. Tests use it to run every platform's rules.
#[cfg(test)]
#[must_use]
pub fn to_uri_on(path: &str, platform: Platform) -> Option<String> {
    to_uri_bytes(path.as_bytes(), platform)
}

/// The path `uri` names under `platform`'s rules, as text: see
/// [`uri_to_path`].
#[cfg(test)]
#[must_use]
pub fn to_path_on(uri: &str, platform: Platform) -> Option<String> {
    String::from_utf8(to_path_bytes(uri, platform)?).ok()
}

/// `path` with `.` and `..` removed and a drive letter uppercased, read
/// lexically: no symlink is read. A Windows verbatim prefix (`\\?\C:`,
/// `\\?\UNC\server\share`) becomes its plain form, so a canonical path and
/// the client's spelling of it meet. A `..` at the root stays at the root,
/// and a leading `..` of a relative path is kept.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    out.push(format!("{}:", char::from(letter.to_ascii_uppercase())));
                }
                Prefix::VerbatimUNC(server, share) => {
                    let mut unc = std::ffi::OsString::from(r"\\");
                    unc.push(server);
                    unc.push(r"\");
                    unc.push(share);
                    out.push(unc);
                }
                Prefix::Verbatim(_) | Prefix::DeviceNS(_) | Prefix::UNC(..) => {
                    out.push(prefix.as_os_str());
                }
            },
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::ParentDir | Component::CurDir) | None => out.push(".."),
            },
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

/// The client's spelling of each open document, by normalized path, so a
/// response names a document as the client did (A §6.1).
#[derive(Clone, Debug, Default)]
pub struct UriMap {
    uris: BTreeMap<PathBuf, Uri>,
}

impl UriMap {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `uri` as the client's spelling of the file it names, and
    /// returns that file's normalized path. `None`, recording nothing, when
    /// `uri` names no file here.
    pub fn insert(&mut self, uri: &Uri) -> Option<PathBuf> {
        let path = normalize(&uri_to_path(uri)?);
        self.uris.insert(path.clone(), uri.clone());
        Some(path)
    }

    /// Forgets the spelling of `path`, returning it.
    pub fn remove(&mut self, path: &Path) -> Option<Uri> {
        self.uris.remove(&normalize(path))
    }

    /// The client's spelling of `path`, if one is recorded.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&Uri> {
        self.uris.get(&normalize(path))
    }

    /// The URI to send for `path`: the client's spelling if one is
    /// recorded, else [`path_to_uri`] of the path.
    ///
    /// # Errors
    ///
    /// As [`path_to_uri`], for a path with no recorded spelling.
    pub fn uri_of(&self, path: &Path) -> Result<Uri, UriError> {
        match self.get(path) {
            Some(uri) => Ok(uri.clone()),
            None => path_to_uri(path),
        }
    }
}

/// The bytes of `path`: its own bytes on Unix, its UTF-8 text elsewhere.
#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the version for other platforms"
)]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Some(path.as_os_str().as_bytes().to_vec())
}

/// The bytes of `path`: its own bytes on Unix, its UTF-8 text elsewhere.
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    path.to_str().map(|text| text.as_bytes().to_vec())
}

/// The path made of `bytes`: any bytes on Unix, UTF-8 text elsewhere.
#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the version for other platforms"
)]
fn bytes_to_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// The path made of `bytes`: any bytes on Unix, UTF-8 text elsewhere.
#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// The URI of the path `path` under `platform`'s rules.
fn to_uri_bytes(path: &[u8], platform: Platform) -> Option<String> {
    let mut uri = String::from("file://");
    match platform {
        Platform::Unix => {
            if path.first() != Some(&b'/') {
                return None;
            }
            encode(&mut uri, path, b"/");
        }
        Platform::Windows => windows_to_uri(&mut uri, path)?,
    }
    Some(uri)
}

/// Appends the authority and path of the Windows path `path` to `uri`.
fn windows_to_uri(uri: &mut String, path: &[u8]) -> Option<()> {
    let path = match path {
        // `\\?\UNC\server\share`: a UNC path.
        [
            b'\\',
            b'\\',
            b'?',
            b'\\',
            b'U',
            b'N',
            b'C',
            b'\\',
            rest @ ..,
        ] => {
            return windows_unc_to_uri(uri, rest);
        }
        // `\\?\C:\x`: a drive path.
        [b'\\', b'\\', b'?', b'\\', rest @ ..] => rest,
        // `\\.\` device paths and other `\\?` forms name no file.
        [a, b, b'.' | b'?', ..] if is_separator(*a) && is_separator(*b) => return None,
        [a, b, rest @ ..] if is_separator(*a) && is_separator(*b) => {
            return windows_unc_to_uri(uri, rest);
        }
        _ => path,
    };
    match path {
        [letter, b':', sep, rest @ ..] if letter.is_ascii_alphabetic() && is_separator(*sep) => {
            uri.push('/');
            uri.push(char::from(*letter));
            uri.push_str(":/");
            encode(uri, &slashes(rest), b"/");
            Some(())
        }
        _ => None,
    }
}

/// Appends `server/share…`, the part of a UNC path after its leading
/// `\\`, to `uri`.
fn windows_unc_to_uri(uri: &mut String, rest: &[u8]) -> Option<()> {
    let host_len = rest
        .iter()
        .position(|&b| is_separator(b))
        .unwrap_or(rest.len());
    let (host, tail) = rest.split_at_checked(host_len)?;
    if host.is_empty() {
        return None;
    }
    encode(uri, host, b"");
    encode(uri, &slashes(tail), b"/");
    Some(())
}

/// The bytes of the path `uri` names under `platform`'s rules.
fn to_path_bytes(uri: &str, platform: Platform) -> Option<Vec<u8>> {
    let scheme = uri.get(..5)?;
    if !scheme.eq_ignore_ascii_case("file:") {
        return None;
    }
    let rest = uri.get(5..)?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (host, path) = match rest.strip_prefix("//") {
        Some(authority) => {
            authority.split_at_checked(authority.find('/').unwrap_or(authority.len()))?
        }
        None => ("", rest),
    };
    if host.contains(['@', ':', '[']) {
        return None;
    }
    let host = decode(host, b"/\\")?;
    let local = host.is_empty() || host.eq_ignore_ascii_case(b"localhost");
    match platform {
        Platform::Unix => {
            if !local || !path.starts_with('/') {
                return None;
            }
            decode(path, b"/")
        }
        Platform::Windows => {
            let path = decode(path, b"/\\")?;
            match path.as_slice() {
                [b'/', letter, b':', rest @ ..]
                    if local && letter.is_ascii_alphabetic() && matches!(rest, [] | [b'/', ..]) =>
                {
                    let mut out = vec![*letter, b':'];
                    out.extend(if rest.is_empty() {
                        vec![b'\\']
                    } else {
                        backslashes(rest)
                    });
                    Some(out)
                }
                _ if local => None,
                [] | [b'/', ..] => {
                    let mut out = b"\\\\".to_vec();
                    out.extend_from_slice(&host);
                    out.extend(backslashes(&path));
                    Some(out)
                }
                _ => None,
            }
        }
    }
}

/// Appends `bytes` to `out`, keeping unreserved characters and those in
/// `keep`, and percent-encoding every other byte.
fn encode(out: &mut String, bytes: &[u8], keep: &[u8]) {
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) || keep.contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
}

/// The uppercase hex digit of the low four bits of `nibble`.
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble & 0x0f), 16).map_or('0', |c| c.to_ascii_uppercase())
}

/// Percent-decodes `text`. `None` for a broken escape, or an escape that
/// decodes to NUL or to a byte in `forbidden`, since that would change
/// where the path's separators are.
fn decode(text: &str, forbidden: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let high = char::from(bytes.next()?).to_digit(16)?;
        let low = char::from(bytes.next()?).to_digit(16)?;
        let decoded = u8::try_from(high.checked_mul(16)?.checked_add(low)?).ok()?;
        if decoded == 0 || forbidden.contains(&decoded) {
            return None;
        }
        out.push(decoded);
    }
    Some(out)
}

/// Whether `byte` separates a Windows path.
fn is_separator(byte: u8) -> bool {
    byte == b'\\' || byte == b'/'
}

/// `bytes` with each Windows separator written `/`.
fn slashes(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|&b| if is_separator(b) { b'/' } else { b })
        .collect()
}

/// `bytes` with each `/` written `\`.
fn backslashes(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|&b| if b == b'/' { b'\\' } else { b })
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed};

    use super::*;

    fn config() -> Config {
        Config {
            cases: 512,
            rng_seed: RngSeed::Fixed(0x0601),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// A path segment: anything but separators and NUL, often nothing.
    fn segment(forbidden: &'static str) -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 0..6).prop_map(move |chars| {
            chars
                .into_iter()
                .filter(|&c| c != '\0' && !forbidden.contains(c))
                .collect()
        })
    }

    fn segments(forbidden: &'static str) -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(segment(forbidden), 0..5)
    }

    /// Checks that `uri` parses as an `lsp_types::Uri` and maps back to
    /// `path` under `platform`.
    fn round_trip(path: &str, platform: Platform) -> Result<(), TestCaseError> {
        let uri = to_uri_on(path, platform);
        prop_assert!(uri.is_some(), "{path:?} has no URI");
        let uri = uri.unwrap_or_default();
        prop_assert!(Uri::from_str(&uri).is_ok(), "{uri} does not parse");
        let back = to_path_on(&uri, platform);
        prop_assert_eq!(back.as_deref(), Some(path), "{}", uri);
        Ok(())
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn unix_paths_round_trip(parts in segments("/")) {
            round_trip(&format!("/{}", parts.join("/")), Platform::Unix)?;
        }

        #[test]
        fn windows_drive_paths_round_trip(
            letter in prop::char::range('A', 'Z'),
            lower in any::<bool>(),
            parts in segments("/\\"),
        ) {
            let letter = if lower { letter.to_ascii_lowercase() } else { letter };
            round_trip(&format!("{letter}:\\{}", parts.join("\\")), Platform::Windows)?;
        }

        #[test]
        fn windows_unc_paths_round_trip(
            host in "[a-z0-9][a-z0-9.-]{0,9}",
            share in "[a-zA-Z0-9 $é%]{1,6}",
            parts in segments("/\\"),
        ) {
            prop_assume!(!host.eq_ignore_ascii_case("localhost"));
            let mut path = format!("\\\\{host}\\{share}");
            for part in parts {
                path.push('\\');
                path.push_str(&part);
            }
            round_trip(&path, Platform::Windows)?;
        }

        #[cfg(unix)]
        #[test]
        fn native_paths_round_trip_with_any_bytes(
            parts in prop::collection::vec(prop::collection::vec(1_u8..=255, 0..6), 0..5),
        ) {
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = Vec::new();
            for part in parts {
                bytes.push(b'/');
                bytes.extend(part.into_iter().filter(|&b| b != b'/'));
            }
            if bytes.is_empty() {
                bytes.push(b'/');
            }
            let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
            let uri = path_to_uri(&path);
            prop_assert!(uri.is_ok(), "{:?}", uri);
            let back = uri.ok().and_then(|uri| uri_to_path(&uri));
            prop_assert_eq!(back, Some(path));
        }
    }

    #[test]
    fn encoding_table() {
        let unix = [
            ("/", "file:///"),
            ("/a b/é#?%:@", "file:///a%20b/%C3%A9%23%3F%25%3A%40"),
            ("/a-b._~/c", "file:///a-b._~/c"),
            ("/x\\y", "file:///x%5Cy"),
        ];
        for (path, uri) in unix {
            assert_eq!(
                to_uri_on(path, Platform::Unix).as_deref(),
                Some(uri),
                "{path}"
            );
        }
        let windows = [
            (r"C:\", "file:///C:/"),
            (r"c:\Users\a b", "file:///c:/Users/a%20b"),
            ("C:/mixed\\seps", "file:///C:/mixed/seps"),
            (r"\\server\share\x y", "file://server/share/x%20y"),
            (r"\\?\C:\long", "file:///C:/long"),
            (r"\\?\UNC\server\share\x", "file://server/share/x"),
        ];
        for (path, uri) in windows {
            assert_eq!(
                to_uri_on(path, Platform::Windows).as_deref(),
                Some(uri),
                "{path}"
            );
        }
    }

    #[test]
    fn paths_with_no_uri() {
        for path in ["", "a/b", "./a"] {
            assert_eq!(to_uri_on(path, Platform::Unix), None, "{path}");
        }
        for path in [
            "",
            r"a\b",
            r"\rooted",
            "C:",
            "C:rel",
            r"\\.\pipe\x",
            r"\\",
            r"\\\x",
        ] {
            assert_eq!(to_uri_on(path, Platform::Windows), None, "{path}");
        }
        assert!(matches!(
            path_to_uri(Path::new("relative")),
            Err(UriError::Relative(_))
        ));
    }

    #[test]
    fn decoding_table() {
        let unix = [
            ("file:///a/b", Some("/a/b")),
            ("FILE:///a", Some("/a")),
            ("file://localhost/etc", Some("/etc")),
            ("file://LocalHost/etc", Some("/etc")),
            ("file:/a/b", Some("/a/b")),
            ("file:///a%20b/%c3%a9", Some("/a b/é")),
            ("file:///raw/é", Some("/raw/é")),
            ("file:///a?query#fragment", Some("/a")),
            ("file:///a%3Fb", Some("/a?b")),
            ("file://server/share", None),
            ("file://user@localhost/a", None),
            ("file://localhost:80/a", None),
            ("file:relative", None),
            ("file://", None),
            ("http:///a", None),
            ("untitled:Untitled-1", None),
            ("file:///a%2Fb", None),
            ("file:///a%00b", None),
            ("file:///a%zz", None),
            ("file:///a%4", None),
        ];
        for (uri, path) in unix {
            assert_eq!(to_path_on(uri, Platform::Unix).as_deref(), path, "{uri}");
        }
        let windows = [
            ("file:///c%3A/Users/x", Some(r"c:\Users\x")),
            ("file:///C%3a/x", Some(r"C:\x")),
            ("file:///C:", Some(r"C:\")),
            ("file:///C:/", Some(r"C:\")),
            ("file://localhost/D:/x", Some(r"D:\x")),
            ("file://server/share/x", Some(r"\\server\share\x")),
            ("file://server", Some(r"\\server")),
            ("file:///x/y", None),
            ("file:///C:x", None),
            ("file:///C:/a%5Cb", None),
            ("file:///C:/a%2Fb", None),
            ("file:///C:/%FF", None),
        ];
        for (uri, path) in windows {
            assert_eq!(to_path_on(uri, Platform::Windows).as_deref(), path, "{uri}");
        }
    }

    #[test]
    fn native_uri_round_trip() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\work\a b\main.cheby")
        } else {
            PathBuf::from("/work/a b/main.cheby")
        };
        let uri = path_to_uri(&path).expect("an absolute path has a URI");
        assert_eq!(uri_to_path(&uri), Some(path));
        let other = Uri::from_str("https://example.com/a").expect("a URI");
        assert_eq!(uri_to_path(&other), None);
    }

    #[test]
    fn normalize_is_lexical() {
        let cases = [
            ("/a/./b/../c", "/a/c"),
            ("/..", "/"),
            ("/a/../../b", "/b"),
            ("a/../../b", "../b"),
            ("./a", "a"),
            ("/a/b/", "/a/b"),
        ];
        for (path, expected) in cases {
            assert_eq!(
                normalize(Path::new(path)),
                PathBuf::from(expected),
                "{path}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn normalize_uppercases_drives_and_drops_verbatim() {
        let cases = [
            (r"c:\a\..\b", r"C:\b"),
            (r"\\?\c:\x", r"C:\x"),
            (r"\\?\UNC\server\share\x", r"\\server\share\x"),
        ];
        for (path, expected) in cases {
            assert_eq!(
                normalize(Path::new(path)),
                PathBuf::from(expected),
                "{path}"
            );
        }
    }

    #[test]
    fn uri_map_keeps_the_client_spelling() {
        let (spelled, path) = if cfg!(windows) {
            (
                "file:///c%3A/w/main.cheby",
                PathBuf::from(r"C:\w\main.cheby"),
            )
        } else {
            ("file:///w/./main.cheby", PathBuf::from("/w/main.cheby"))
        };
        let uri = Uri::from_str(spelled).expect("a URI");
        let mut map = UriMap::new();
        assert_eq!(map.insert(&uri), Some(path.clone()));
        assert_eq!(
            map.uri_of(&path).map(|u| u.as_str().to_owned()),
            Ok(spelled.to_owned())
        );
        assert_eq!(
            map.remove(&path).as_ref().map(|u| u.as_str()),
            Some(spelled)
        );
        let fresh = map.uri_of(&path).expect("an absolute path has a URI");
        assert_ne!(fresh.as_str(), spelled);
        let other = Uri::from_str("untitled:Untitled-1").expect("a URI");
        assert_eq!(map.insert(&other), None);
    }
}
