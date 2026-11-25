//! Git URLs of dependencies and their canonical form (§13.4, D-468).

/// A git URL that a dependency may have (D-1879): `https://`, `ssh://` or
/// the scp-style `git@host:path`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitUrl {
    source: String,
    canonical: String,
}

/// The bytes a URL may hold. None of them means anything to a shell, to
/// git's options or in a symbol, and `#`, `$` and `%` are not among them.
fn allowed(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~/@:+".contains(&byte)
}

impl GitUrl {
    /// Checks `text`.
    ///
    /// # Errors
    ///
    /// Why it is no URL that a dependency may have, as a phrase that
    /// follows the URL.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        if text.is_empty() || !text.bytes().all(allowed) {
            return Err("holds a character that a git URL may not have");
        }
        let after_scheme = text
            .strip_prefix("https://")
            .or_else(|| text.strip_prefix("ssh://"));
        let (authority, path) = if let Some(rest) = after_scheme {
            rest.split_once('/').ok_or("names no repository")?
        } else if text.contains("://") {
            return Err("has a scheme other than `https` and `ssh`");
        } else {
            text.split_once(':')
                .filter(|(authority, _)| !authority.contains('/'))
                .ok_or("is no git URL")?
        };
        let host = match authority.rsplit_once('@') {
            Some((user, _)) if user.contains(':') => return Err("holds a password"),
            Some((user, _)) if user.is_empty() || user.starts_with('-') || user.contains('@') => {
                return Err("has an invalid user name");
            }
            Some((_, host)) => host,
            None => authority,
        };
        let (name, port) = match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        };
        let name_ok = name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        let port_ok = port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !name_ok || !port_ok {
            return Err("has an invalid host");
        }
        let mut path = path.trim_start_matches('/');
        loop {
            let shorter = path.strip_suffix('/').or_else(|| path.strip_suffix(".git"));
            match shorter {
                Some(shorter) => path = shorter,
                None => break,
            }
        }
        if path.is_empty() {
            return Err("names no repository");
        }
        let segment_ok = |s: &str| {
            !s.is_empty() && s != "." && s != ".." && !s.starts_with('-') && !s.contains(['@', ':'])
        };
        if !path.split('/').all(segment_ok) {
            return Err("has an invalid path");
        }
        Ok(Self {
            source: text.to_owned(),
            canonical: format!("{}/{path}", host.to_ascii_lowercase()),
        })
    }

    /// The URL as written, which git gets.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The canonical form, which identifies the repository (D-468).
    #[must_use]
    pub fn canonical(&self) -> &str {
        &self.canonical
    }
}

/// Whether `tag` can be the name of a git tag, by the rules of `git
/// check-ref-format`, and is no option.
#[must_use]
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"~^:?*[\\".contains(&b))
        && !tag.starts_with('-')
        && !tag.contains("..")
        && !tag.contains("@{")
        && tag != "@"
        && tag.split('/').all(|s| {
            !s.is_empty()
                && !s.starts_with('.')
                && s.strip_suffix(".lock").is_none()
                && !s.ends_with('.')
        })
}

/// Whether `rev` is a commit's hash or the start of one, of at least seven
/// digits.
#[must_use]
pub fn is_valid_rev(rev: &str) -> bool {
    (7..=64).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{GitUrl, is_valid_rev, is_valid_tag};

    /// The forms of D-468 are one URL.
    #[test]
    fn canonical_urls() {
        for text in [
            "https://github.com/example/cheby-json",
            "https://GitHub.com/example/cheby-json.git",
            "https://github.com/example/cheby-json/",
            "https://github.com/example/cheby-json.git/",
            "https://me@github.com/example/cheby-json",
            "ssh://git@github.com/example/cheby-json.git",
            "git@github.com:example/cheby-json.git",
            "github.com:example/cheby-json",
        ] {
            let url = GitUrl::parse(text).unwrap();
            assert_eq!(url.canonical(), "github.com/example/cheby-json", "{text}");
            assert_eq!(url.source(), text);
        }
        let other = |text| GitUrl::parse(text).unwrap().canonical().to_owned();
        assert_eq!(other("https://h:8443/A/b"), "h:8443/A/b");
        assert_eq!(other("git@h:~me/b"), "h/~me/b");
    }

    /// What git could take for an option, a command, a file or another
    /// transport, and what may not be in an identity.
    #[test]
    fn refused_urls() {
        for text in [
            "",
            "../json",
            "/json",
            "file:///json",
            "http://h/a",
            "git://h/a",
            "ext::sh -c id",
            "ext::sh",
            "--upload-pack=x",
            "-x:y",
            "-u@h:y",
            "https://h",
            "https://h/",
            "https://h/.git",
            "https://h/a/../b",
            "https://h/a//b",
            "https://h/-a",
            "https://h/a#b",
            "https://h/a%2e",
            "https://h/a?b",
            "https://h/a b",
            "https://-h/a",
            "https://h:/a",
            "https://h:x/a",
            "https://me:secret@h/a",
            "https://@h/a",
            "h:a:b",
            "git@h:a@b",
        ] {
            assert!(GitUrl::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn tags_and_revs() {
        for tag in ["v1.2.0", "release/1", "a-b_c+d"] {
            assert!(is_valid_tag(tag), "{tag}");
        }
        for tag in [
            "", "-v", "a..b", "a b", "a^", "a:b", "a@{1}", "@", "/a", "a/", ".a", "a.lock", "a.",
            "é",
        ] {
            assert!(!is_valid_tag(tag), "{tag}");
        }
        assert!(is_valid_rev("4f2a9c1") && is_valid_rev(&"a".repeat(40)));
        for rev in ["4f2a9c", "main", "4f2a9c1^", "HEAD~1", ""] {
            assert!(!is_valid_rev(rev), "{rev}");
        }
    }
}
