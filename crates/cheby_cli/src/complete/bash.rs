//! Bash's side of completion (D-1960): the script and the answers.
//!
//! `clap_complete`'s own bash adapter is not used, because bash breaks a
//! word at each character of `COMP_WORDBREAKS`, which has `:` and `=`, and
//! inserts a candidate as it is written:
//!
//! - Bash 4 and later give the words split there, so `list::ma` arrives as
//!   `list`, `::` and `ma`. [`rejoin`] puts them together again.
//! - Every bash replaces only what follows the last break of the word, so
//!   the candidate `list::map` would give `list::list::map`. [`reply`]
//!   leaves out what bash keeps.
//! - A candidate with a space, as a test's name has, would become several
//!   words. [`reply`] writes it with `\` before each character the shell
//!   reads specially, or inside the quotes the word began with, and
//!   [`unquote`] reads the words that way.
//!
//! The script passes the words as bash has them, the index of the one to
//! complete, `COMP_WORDBREAKS`, since a user may have taken `:` out of it,
//! and whether a space must be part of a candidate, which it must before
//! bash 4, where the script cannot turn the shell's own space off for one
//! answer.

use std::ffi::OsString;

use clap_complete::env::EnvCompleter;

/// Between two candidates of an answer: the script splits at it.
const SEPARATOR: char = '\u{b}';

/// The breaks of a word that a path, a flag with its value or a URL may
/// have. The other characters of `COMP_WORDBREAKS` are quotes, spaces and
/// the shell's operators, which end a word for the shell too.
const BREAKS: &[char] = &[':', '=', '@'];

/// Bash, by its own script.
pub(super) struct Bash;

impl EnvCompleter for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn is(&self, name: &str) -> bool {
        name == "bash"
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn std::io::Write,
    ) -> Result<(), std::io::Error> {
        let script = r#"
_NAME_complete() {
    local IFS=$'\013'
    local space=true
    if compopt +o nospace 2> /dev/null; then
        space=false
    fi
    COMPREPLY=( $( \
        _CHEBY_INDEX="$COMP_CWORD" \
        _CHEBY_BREAKS="$COMP_WORDBREAKS" \
        _CHEBY_SPACE="$space" \
        VAR="bash" \
        COMPLETER -- "${COMP_WORDS[@]}" \
    ) )
    if [[ $? != 0 ]]; then
        unset COMPREPLY
    elif [[ $space == false ]] && [[ "${COMPREPLY-}" =~ [=/:]$ ]]; then
        compopt -o nospace
    fi
}
if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -o nospace -o bashdefault -o nosort -F _NAME_complete BIN
else
    complete -o nospace -o bashdefault -F _NAME_complete BIN
fi
"#
        .replace("NAME", &name.replace('-', "_"))
        .replace("BIN", bin)
        .replace("VAR", var)
        // Last, so that nothing is replaced inside the path.
        .replace("COMPLETER", &quoted(completer));
        writeln!(buf, "{script}")
    }

    fn write_complete(
        &self,
        cmd: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&std::path::Path>,
        buf: &mut dyn std::io::Write,
    ) -> Result<(), std::io::Error> {
        let variable = |name: &str| std::env::var(name).unwrap_or_default();
        let index: usize = variable("_CHEBY_INDEX").parse().unwrap_or_default();
        let wordbreaks = variable("_CHEBY_BREAKS");
        let breaks: Vec<char> = BREAKS
            .iter()
            .copied()
            .filter(|&c| wordbreaks.contains(c))
            .collect();
        let space = variable("_CHEBY_SPACE") == "true";
        let raw: Vec<String> = args
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        let (words, index) = rejoin(&raw, index, &breaks);
        let typed = words.get(index).cloned().unwrap_or_default();
        let mut plain: Vec<OsString> = words.iter().map(|word| unquote(word).into()).collect();
        // The word after the last one is the empty word to complete.
        if plain.len() <= index {
            plain.resize(index.saturating_add(1), OsString::new());
        }
        let found = clap_complete::engine::complete(cmd, plain, index, current_dir)?;
        let mut first = true;
        for candidate in &found {
            let value = candidate.get_value().to_string_lossy();
            let Some(text) = reply(&typed, &value, &breaks, space) else {
                continue;
            };
            if !first {
                write!(buf, "{SEPARATOR}")?;
            }
            first = false;
            write!(buf, "{text}")?;
        }
        Ok(())
    }
}

/// `text` as one word of a bash script.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The words of the command line from `words`, which bash 4 and later
/// split at each run of `breaks`, and the index of the word that holds the
/// word at `index`. A word that is only breaks joins the word before it and
/// the word after it, as in `list`, `::`, `ma`. Bash drops the spaces
/// between words, so `list :: ma` joins too; a lone `::` is no argument of
/// `cheby`. The first word is the command and never joins, and neither does
/// an empty word, which is the word to complete after a space.
fn rejoin(words: &[String], index: usize, breaks: &[char]) -> (Vec<String>, usize) {
    let is_break = |word: &str| !word.is_empty() && word.chars().all(|c| breaks.contains(&c));
    let mut joined: Vec<String> = Vec::new();
    let mut at = 0;
    let mut open = false;
    for (i, word) in words.iter().enumerate() {
        let joins = i > 1 && !word.is_empty() && (open || is_break(word));
        match joined.last_mut() {
            Some(last) if joins => last.push_str(word),
            Some(_) | None => joined.push(word.clone()),
        }
        open = i > 0 && is_break(word);
        if i == index {
            at = joined.len().saturating_sub(1);
        }
    }
    if index >= words.len() {
        at = joined
            .len()
            .saturating_add(index.saturating_sub(words.len()));
    }
    (joined, at)
}

/// The value of the word `word` as bash reads it: without its quotes and
/// the `\` before a character. A quote that is not closed runs to the end,
/// as in the word being typed.
fn unquote(word: &str) -> String {
    let mut value = String::new();
    let mut chars = word.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\\') => value.extend(chars.next()),
            (None, '\'' | '"') => quote = Some(c),
            (Some('"'), '\\') if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                value.extend(chars.next());
            }
            (Some(open), _) if c == open => quote = None,
            _ => value.push(c),
        }
    }
    value
}

/// What bash inserts for the candidate `value` where the word `typed`
/// stands, or `None` for a candidate that cannot stand there.
///
/// - After a quote, bash replaces the whole word and closes the quote
///   itself: the answer is the quote and the value as it is written inside
///   it. A value with `'` cannot be written inside `'…'`.
/// - Otherwise bash replaces what follows the last of `breaks` in `typed`:
///   the answer is the rest of the value, with `\` before each character
///   the shell reads specially.
///
/// With `space`, the script cannot ask bash for the space after a
/// candidate, so an answer outside quotes ends with one, unless more is
/// typed after it: a directory, or a flag before its `=`.
fn reply(typed: &str, value: &str, breaks: &[char], space: bool) -> Option<String> {
    let open = value.ends_with(['/', '=', ':']);
    let text = match typed.chars().next() {
        Some(quote @ '"') => {
            let mut text = String::from(quote);
            for c in value.chars() {
                if matches!(c, '"' | '\\' | '$' | '`') {
                    text.push('\\');
                }
                text.push(c);
            }
            text
        }
        Some(quote @ '\'') => {
            if value.contains(quote) {
                return None;
            }
            format!("{quote}{value}")
        }
        Some(_) | None => {
            let kept = typed
                .rfind(breaks)
                .map_or("", |last| typed.get(..=last).unwrap_or_default());
            let rest = value.strip_prefix(unquote(kept).as_str())?;
            let mut text = String::new();
            for c in rest.chars() {
                if c.is_whitespace() || r#"\"'$`&;|<>()*?[]!#{}"#.contains(c) {
                    text.push('\\');
                }
                text.push(c);
            }
            if space && !open {
                text.push(' ');
            }
            text
        }
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &[&str]) -> Vec<String> {
        text.iter().map(|&word| word.to_owned()).collect()
    }

    /// Bash 4 splits a word at `:` and `=`; the words come back whole,
    /// and the index follows the word it was in.
    #[test]
    fn split_words_are_joined_again() {
        let breaks = [':', '='];
        let join = |text: &[&str], index| rejoin(&words(text), index, &breaks);
        assert_eq!(
            join(&["cheby", "doc", "list", "::", "ma"], 4),
            (words(&["cheby", "doc", "list::ma"]), 2)
        );
        assert_eq!(
            join(&["cheby", "doc", "list", "::"], 3),
            (words(&["cheby", "doc", "list::"]), 2)
        );
        assert_eq!(
            join(&["cheby", "run", "a", "::", "b", "::", "c", "--", "x"], 6),
            (words(&["cheby", "run", "a::b::c", "--", "x"]), 2)
        );
        assert_eq!(
            join(&["cheby", "check", "--target", "=", "na"], 4),
            (words(&["cheby", "check", "--target=na"]), 2)
        );
        // A word that follows a joined one stands alone, and so does the
        // empty word after a space.
        assert_eq!(
            join(&["cheby", "test", "--module", "a", "::", "b", "ke"], 6),
            (words(&["cheby", "test", "--module", "a::b", "ke"]), 4)
        );
        assert_eq!(
            join(&["cheby", "doc", ""], 2),
            (words(&["cheby", "doc", ""]), 2)
        );
        assert_eq!(
            join(&["cheby", "doc", "list", "::", ""], 4),
            (words(&["cheby", "doc", "list::", ""]), 3)
        );
        // Bash 3 and a `COMP_WORDBREAKS` without `:` leave the words whole.
        assert_eq!(
            join(&["cheby", "doc", "list::ma"], 2),
            (words(&["cheby", "doc", "list::ma"]), 2)
        );
        assert_eq!(
            rejoin(&words(&["cheby", "doc", "list", "::", "ma"]), 4, &[]),
            (words(&["cheby", "doc", "list", "::", "ma"]), 4)
        );
        // An index past the words is the empty word that bash left out.
        assert_eq!(join(&["cheby", "doc"], 2), (words(&["cheby", "doc"]), 2));
    }

    #[test]
    fn a_word_is_read_as_bash_reads_it() {
        assert_eq!(unquote(r"keywords\ parse"), "keywords parse");
        assert_eq!(unquote(r#""keywords parse"#), "keywords parse");
        assert_eq!(unquote(r#""a \"b\" \n""#), r#"a "b" \n"#);
        assert_eq!(unquote("'it''s'"), "its");
        assert_eq!(unquote(r"'a\b'"), r"a\b");
        assert_eq!(unquote("list::ma"), "list::ma");
        assert_eq!(unquote(""), "");
    }

    /// The answer is what follows the last break, escaped; in quotes it is
    /// the whole value.
    #[test]
    fn an_answer_is_what_bash_replaces() {
        let breaks = [':', '='];
        let answer = |typed, value| reply(typed, value, &breaks, false);
        assert_eq!(answer("list::ma", "list::map"), Some("map".to_owned()));
        assert_eq!(answer("list::", "list::map"), Some("map".to_owned()));
        assert_eq!(answer("li", "list"), Some("list".to_owned()));
        assert_eq!(answer("", "std::list"), Some("std::list".to_owned()));
        assert_eq!(
            answer("--target=na", "--target=native"),
            Some("native".to_owned())
        );
        assert_eq!(
            answer("key", "keywords parse (all)"),
            Some(r"keywords\ parse\ \(all\)".to_owned())
        );
        assert_eq!(
            answer(r"keywords\ p", "keywords parse"),
            Some(r"keywords\ parse".to_owned())
        );
        assert_eq!(
            answer("\"key", r#"keys "a" cost $1"#),
            Some(r#""keys \"a\" cost \$1"#.to_owned())
        );
        assert_eq!(
            answer("'key", "keys cost $1"),
            Some("'keys cost $1".to_owned())
        );
        assert_eq!(
            reply("\"key", "keys", &breaks, true),
            Some("\"keys".to_owned())
        );
        assert_eq!(answer("'it", "it's"), None);
        assert_eq!(answer("\"sr", "src/"), Some("\"src/".to_owned()));
        // A user's `COMP_WORDBREAKS` without `:`: bash replaces the word.
        assert_eq!(
            reply("list::ma", "list::map", &['='], false),
            Some("list::map".to_owned())
        );
        // Before bash 4 the space is the answer's own.
        assert_eq!(
            reply("list::ma", "list::map", &breaks, true),
            Some("map ".to_owned())
        );
        assert_eq!(reply("sr", "src/", &breaks, true), Some("src/".to_owned()));
    }

    #[test]
    fn the_script_names_the_function_the_variable_and_the_binary() {
        let mut script = Vec::new();
        Bash.write_registration(
            "CHEBY_COMPLETE",
            "cheby",
            "cheby",
            "/opt/my tools/cheby",
            &mut script,
        )
        .unwrap();
        let script = String::from_utf8(script).unwrap();
        assert!(script.contains("_cheby_complete() {"), "{script}");
        assert!(script.contains("CHEBY_COMPLETE=\"bash\""), "{script}");
        assert!(
            script.contains("'/opt/my tools/cheby' -- \"${COMP_WORDS[@]}\""),
            "{script}"
        );
        assert!(script.contains("-F _cheby_complete cheby\n"), "{script}");
    }
}
