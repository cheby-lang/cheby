//! SHA-256 and the checksum of a package's files (D-1878).

use std::fmt::Write as _;

use crate::PkgError;

/// One file of a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct File {
    /// Relative to the package's root, with `/`.
    pub path: String,
    pub executable: bool,
    pub bytes: Vec<u8>,
}

/// The lowercase hex SHA-256 of `parts`, one after the other.
#[cfg(not(windows))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the function has no implementation on Windows"
)]
pub(crate) fn sha256(parts: &[&[u8]]) -> Result<String, PkgError> {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for part in parts {
        context.update(part);
    }
    let mut hex = String::with_capacity(64);
    for byte in context.finish().as_ref() {
        // Writing to a string cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

#[cfg(windows)]
pub(crate) fn sha256(_parts: &[&[u8]]) -> Result<String, PkgError> {
    let mut message = String::new();
    let _ = write!(
        message,
        "git dependencies are not available yet on Windows (planned for M6)"
    );
    Err(PkgError::new(message))
}

/// The checksum of a package's files: over each, sorted by path, its path,
/// whether it is executable, and its contents, each with its length.
pub(crate) fn checksum(files: &[File]) -> Result<String, PkgError> {
    let mut sorted: Vec<&File> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut parts: Vec<Vec<u8>> = vec![b"cheby-package-v1\n".to_vec()];
    for file in sorted {
        let mut head = length(file.path.len());
        head.extend_from_slice(file.path.as_bytes());
        head.push(u8::from(file.executable));
        head.extend_from_slice(&length(file.bytes.len()));
        parts.push(head);
        parts.push(file.bytes.clone());
    }
    let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Ok(format!("sha256:{}", sha256(&parts)?))
}

fn length(n: usize) -> Vec<u8> {
    u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes().to_vec()
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{File, checksum, sha256};

    #[test]
    fn sha256_of_abc() {
        assert_eq!(
            sha256(&[b"a", b"bc"]).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// The order of files does not matter; paths, modes and contents do.
    #[test]
    fn what_a_checksum_covers() {
        let file = |path: &str, executable, bytes: &[u8]| File {
            path: path.to_owned(),
            executable,
            bytes: bytes.to_vec(),
        };
        let base = checksum(&[file("a", false, b"x"), file("b", false, b"y")]).unwrap();
        assert_eq!(
            base,
            checksum(&[file("b", false, b"y"), file("a", false, b"x")]).unwrap()
        );
        for other in [
            [file("a", true, b"x"), file("b", false, b"y")],
            [file("a", false, b"xb"), file("b", false, b"y")],
            [file("a", false, b"x"), file("c", false, b"y")],
            [file("ab", false, b"x"), file("", false, b"y")],
        ] {
            assert_ne!(base, checksum(&other).unwrap());
        }
    }
}
