//! Decision-log numbers (`D-NNN`) in text.

use std::collections::BTreeSet;

/// Parses one `D-NNN` reference.
///
/// # Errors
///
/// Returns a message if `s` is not `D-` followed by exactly three digits.
pub fn parse(s: &str) -> Result<u32, String> {
    let s = s.trim();
    s.strip_prefix("D-")
        .filter(|n| n.len() == 3 && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("`{s}` is not a decision number such as `D-073`"))
}

/// Every `D-NNN` mentioned in `text`: `D-` not preceded by a letter or
/// digit, followed by exactly three digits.
#[must_use]
pub fn mentioned_in(text: &str) -> BTreeSet<u32> {
    text.match_indices("D-")
        .filter(|&(start, _)| {
            !text
                .get(..start)
                .and_then(|before| before.bytes().last())
                .is_some_and(|b| b.is_ascii_alphanumeric())
        })
        .filter_map(|(start, _)| {
            let rest = text.get(start.checked_add(2)?..)?;
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits != 3 {
                return None;
            }
            rest.get(..3)?.parse().ok()
        })
        .collect()
}

/// Formats a decision number as `D-NNN`.
#[must_use]
pub fn format(n: u32) -> String {
    format!("D-{n:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_references() {
        let found = mentioned_in("(D-001, D-244) and ~~D-0123~~ xD-005 D-17 D-999");
        assert_eq!(found.into_iter().collect::<Vec<_>>(), vec![1, 244, 999]);
    }

    #[test]
    fn parses_one() {
        assert_eq!(parse(" D-073 "), Ok(73));
        assert!(parse("D-73").is_err());
    }
}
