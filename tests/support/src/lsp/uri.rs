//! `file:` URIs for the harness's client, and the paths they name.
//!
//! The client writes URIs the way common editors do: unreserved characters
//! and `/` stay, every other byte is percent-encoded as UTF-8, and on
//! Windows the drive stays readable, `file:///C:/x`. It reads any spelling
//! a server may echo: an empty host or `localhost`, `%3A` or `%3a` in a
//! drive, a lowercase drive letter, and on Windows a UNC host. The rules
//! take the platform as a parameter, so the Windows rules run on every
//! platform.

use std::path::{Path, PathBuf};

/// The URI of the absolute `path` on this platform.
///
/// # Errors
///
/// Returns a message if `path` is not UTF-8 or not absolute.
pub fn path_to_uri(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    to_uri_on(text, cfg!(windows))
}

/// The path that `uri` names on this platform, or `None` for another
/// scheme, a malformed URI or a host this platform cannot name.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    to_path_on(uri, cfg!(windows)).map(PathBuf::from)
}

/// [`path_to_uri`] with the platform given: `windows` reads `\` and `/` as
/// separators, a drive `C:` and a UNC prefix `\\host\share`.
///
/// # Errors
///
/// Returns a message if `path` is not absolute for that platform.
pub fn to_uri_on(path: &str, windows: bool) -> Result<String, String> {
    if !windows {
        if !path.starts_with('/') {
            return Err(format!("{path} is not an absolute path"));
        }
        return Ok(format!("file://{}", encode(path)));
    }
    let slashed = strip_verbatim(path).replace('\\', "/");
    if let Some(unc) = slashed.strip_prefix("//") {
        let (host, rest) = unc.split_once('/').unwrap_or((unc, ""));
        if host.is_empty() {
            return Err(format!("{path} has an empty UNC host"));
        }
        return Ok(format!("file://{}/{}", encode(host), encode(rest)));
    }
    let mut chars = slashed.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic);
    match (drive, chars.next(), chars.next()) {
        (Some(letter), Some(':'), Some('/')) => {
            let rest = slashed.get(2..).unwrap_or_default();
            Ok(format!(
                "file:///{}:{}",
                letter.to_ascii_uppercase(),
                encode(rest)
            ))
        }
        _ => Err(format!("{path} is not an absolute Windows path")),
    }
}

/// A Windows path without its verbatim prefix, which `canonicalize` adds
/// and editors never write: `\\?\C:\x` is `C:\x`, and
/// `\\?\UNC\host\share` is `\\host\share`. Any other path is returned
/// as it is.
#[must_use]
pub fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
}

/// [`uri_to_path`] with the platform given.
#[must_use]
pub fn to_path_on(uri: &str, windows: bool) -> Option<String> {
    let scheme_len = "file://".len();
    let scheme = uri.get(..scheme_len)?;
    if !scheme.eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = uri.get(scheme_len..)?;
    let (host, path) = match rest.find('/') {
        Some(at) => (rest.get(..at)?, rest.get(at..)?),
        None => (rest, ""),
    };
    // A query or fragment is not part of a path.
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let decoded = decode(path)?;
    let local = host.is_empty() || host.eq_ignore_ascii_case("localhost");
    if !windows {
        return (local && decoded.starts_with('/')).then_some(decoded);
    }
    if !local {
        let host = decode(host)?;
        return Some(format!("\\\\{host}{}", decoded.replace('/', "\\")));
    }
    // `/C:/x` or `/c:/x`.
    let mut chars = decoded.chars();
    let (slash, letter, colon) = (chars.next(), chars.next(), chars.next());
    match (slash, letter, colon) {
        (Some('/'), Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
            let rest = decoded.get(3..).unwrap_or_default();
            let rest = if rest.is_empty() { "/" } else { rest };
            Some(format!(
                "{}:{}",
                letter.to_ascii_uppercase(),
                rest.replace('/', "\\")
            ))
        }
        _ => None,
    }
}

/// Percent-encodes every byte but unreserved characters and `/`.
fn encode(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/');
        if keep {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Decodes `%XX` escapes; `None` for a bad escape or text that is not
/// UTF-8 after decoding.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0_usize;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'%' {
            let hex = text.get(i.checked_add(1)?..i.checked_add(3)?)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i = i.checked_add(3)?;
        } else {
            out.push(byte);
            i = i.checked_add(1)?;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_paths() {
        let table = [
            ("/tmp/a/b.cheby", "file:///tmp/a/b.cheby"),
            ("/tmp/a b/c#d.cheby", "file:///tmp/a%20b/c%23d.cheby"),
            ("/tmp/é😀/x", "file:///tmp/%C3%A9%F0%9F%98%80/x"),
            ("/", "file:///"),
        ];
        for (path, uri) in table {
            assert_eq!(to_uri_on(path, false).as_deref(), Ok(uri), "{path}");
            assert_eq!(to_path_on(uri, false).as_deref(), Some(path), "{uri}");
        }
        assert!(to_uri_on("relative/x", false).is_err());
        assert_eq!(
            to_path_on("file://localhost/tmp/x", false).as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(
            to_path_on("FILE:///tmp/x", false).as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(to_path_on("file://server/tmp/x", false), None);
        assert_eq!(to_path_on("untitled:Untitled-1", false), None);
        assert_eq!(to_path_on("file:///tmp/%zz", false), None);
        assert_eq!(to_path_on("file:///tmp/%FF", false), None);
    }

    #[test]
    fn windows_paths() {
        let table = [
            (r"C:\Users\a\b.cheby", "file:///C:/Users/a/b.cheby"),
            (r"C:\a b\c", "file:///C:/a%20b/c"),
            (r"\\server\share\x.cheby", "file://server/share/x.cheby"),
        ];
        for (path, uri) in table {
            assert_eq!(to_uri_on(path, true).as_deref(), Ok(uri), "{path}");
            assert_eq!(to_path_on(uri, true).as_deref(), Some(path), "{uri}");
        }
        assert_eq!(
            to_uri_on(r"c:\x", true).as_deref(),
            Ok("file:///C:/x"),
            "the drive letter is uppercased"
        );
        // Spellings that clients send.
        for uri in ["file:///c%3A/x/y", "file:///c%3a/x/y", "file:///C:/x/y"] {
            assert_eq!(to_path_on(uri, true).as_deref(), Some(r"C:\x\y"), "{uri}");
        }
        assert_eq!(to_path_on("file:///C:", true).as_deref(), Some(r"C:\"));
        assert_eq!(to_path_on("file:///x/y", true), None);
        // Verbatim paths, as `canonicalize` gives them.
        assert_eq!(to_uri_on(r"\\?\C:\x", true).as_deref(), Ok("file:///C:/x"));
        assert_eq!(
            to_uri_on(r"\\?\UNC\server\share\x", true).as_deref(),
            Ok("file://server/share/x")
        );
        assert_eq!(strip_verbatim(r"C:\x"), r"C:\x");
        assert!(to_uri_on(r"x\y", true).is_err());
        assert!(to_uri_on(r"\\\x", true).is_err());
    }

    #[test]
    fn this_platform() {
        let path = std::env::temp_dir().join("a b").join("c.cheby");
        let uri = path_to_uri(&path).unwrap();
        assert!(uri.starts_with("file://"), "{uri}");
        assert_eq!(uri_to_path(&uri), Some(path));
    }
}
