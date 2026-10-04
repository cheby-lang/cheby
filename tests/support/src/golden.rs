//! Reading, comparing and blessing golden files.

use std::path::Path;

/// Reads a golden file, normalizing line endings. `None` if it is absent.
///
/// # Errors
///
/// Returns a message if the file exists but cannot be read.
pub fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(crate::process::normalize(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes `content` to `path`. An empty `content` removes the file
/// instead, unless `keep_empty` is set.
///
/// # Errors
///
/// Returns a message if the file cannot be written or removed.
pub fn write(path: &Path, content: &str, keep_empty: bool) -> Result<(), String> {
    if content.is_empty() && !keep_empty {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    } else {
        std::fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Compares `actual` with `expected` and describes the first difference.
///
/// # Errors
///
/// Returns a description of the first differing line.
pub fn compare(what: &str, expected: &str, actual: &str) -> Result<(), String> {
    if expected == actual {
        return Ok(());
    }
    let mut expected_lines = expected.split_inclusive('\n');
    let mut actual_lines = actual.split_inclusive('\n');
    let mut line: usize = 1;
    loop {
        match (expected_lines.next(), actual_lines.next()) {
            (Some(e), Some(a)) if e == a => line = line.saturating_add(1),
            (e, a) => {
                return Err(format!(
                    "{what} differs at line {line}\n  expected: {}\n  actual:   {}",
                    show(e),
                    show(a)
                ));
            }
        }
    }
}

fn show(line: Option<&str>) -> String {
    line.map_or_else(|| "<end of output>".to_owned(), |line| format!("{line:?}"))
}

#[cfg(test)]
mod tests {
    use super::compare;

    #[test]
    fn reports_the_first_differing_line() {
        assert!(compare("stdout", "a\nb\n", "a\nb\n").is_ok());
        let err = compare("stdout", "a\nb\n", "a\nc\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        let err = compare("stdout", "a\n", "a\nb\n").unwrap_err();
        assert!(err.contains("<end of output>"), "{err}");
    }
}
