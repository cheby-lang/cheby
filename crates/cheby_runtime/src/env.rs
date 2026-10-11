//! `std::env` kernels (D-347): the program's arguments and name, and
//! environment variables. None can fail; text from the operating system
//! that is not valid UTF-8 has each invalid sequence replaced with U+FFFD
//! (D-311).
//!
//! The arguments and the name come from the running program's state in
//! [`crate::entry`], which `run_main` sets from its `RunConfig` (and a test
//! runner or the REPL through [`crate::entry::set_program`]). Under
//! `cheby run` and `cheby test` the name is the package's name (D-369).
//! When no host has set a name, `program_name` falls back to the first
//! argument the operating system passed to the process (D-369's
//! "otherwise").

use std::ffi::OsString;

use crate::abi::Word;
use crate::entry;
use crate::list;
use crate::panic::fatal;
use crate::{string, values};

/// Appends every `std::env` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["env"];

/// `env::args`: the arguments, without the program name (D-347).
#[unsafe(no_mangle)]
pub extern "C" fn cheby_env_args() -> Word {
    list::from_iter(entry::program_args().iter().map(|arg| values::string(arg)))
}

/// `env::program_name`: the package's name under the toolchain (D-369),
/// else the process's first argument, with invalid UTF-8 replaced.
#[unsafe(no_mangle)]
pub extern "C" fn cheby_env_program_name() -> Word {
    let name = entry::program_name_if_set().unwrap_or_else(|| {
        std::env::args_os()
            .next()
            .map(|first| lossy(&first))
            .unwrap_or_default()
    });
    values::string(&name)
}

/// `env::get`: `Some(value)` for a set variable, `Some("")` when it is set
/// but empty, and `None` when it is not set or `name` cannot name a
/// variable: empty, or containing `=` or U+0000 (D-347, D-370).
///
/// # Safety
///
/// `name` is a live `String`, borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_env_get(name: Word) -> Word {
    // SAFETY: forwarded from the caller.
    let name = unsafe { string::as_str_unchecked(name) }
        .unwrap_or_else(|| fatal(format_args!("{name:#x} is not a String")));
    values::option(variable(name).map(|value| values::string(&value)))
}

/// The value of the variable `name`, with invalid UTF-8 replaced, or
/// `None`.
fn variable(name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    std::env::var_os(name).map(|value| lossy(&value))
}

/// Whether a variable can have this name (D-370).
fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0'])
}

/// Text from the operating system with invalid UTF-8 replaced (D-311).
fn lossy(text: &OsString) -> String {
    text.to_string_lossy().into_owned()
}

/// One kernel: symbol, address, C parameter types and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every `std::env` kernel.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 3] {
    [
        ("cheby_env_args", cheby_env_args as *const u8, &[], "Word"),
        (
            "cheby_env_program_name",
            cheby_env_program_name as *const u8,
            &[],
            "Word",
        ),
        (
            "cheby_env_get",
            cheby_env_get as *const u8,
            &["Word"],
            "Word",
        ),
    ]
}

/// The C signatures of the kernels, for the cross-check against `std/`
/// in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::live;
    use crate::rc;

    /// The strings of a list word, which is then dropped.
    fn strings(list: Word) -> Vec<String> {
        // SAFETY: a live list of live strings, owned by the test.
        unsafe {
            let out = list::iter(list)
                .map(|word| string::as_str(word).unwrap().to_owned())
                .collect();
            rc::drop(list);
            out
        }
    }

    /// The text of an `Option<String>` word, which is then dropped.
    fn option_text(word: Word) -> Option<String> {
        // SAFETY: a live option owned by the test.
        unsafe {
            let out = values::read_option(word).map(|text| values::str_of(text).to_owned());
            rc::drop(word);
            out
        }
    }

    #[test]
    fn args_and_name_come_from_the_program_state() {
        // The program state is global; `entry`'s tests set it too.
        let _guard = crate::alloc::tests::serial();
        let before = live();
        entry::set_program(
            vec!["a b".to_owned(), String::new(), "é".to_owned()],
            "app".to_owned(),
        );
        assert_eq!(strings(cheby_env_args()), ["a b", "", "é"]);
        let name = cheby_env_program_name();
        // SAFETY: a live string owned by the test.
        unsafe {
            assert_eq!(values::str_of(name), "app");
            rc::drop(name);
        }
        entry::set_program(Vec::new(), String::new());
        assert_eq!(strings(cheby_env_args()), Vec::<String>::new());
        assert_eq!(live(), before);
    }

    #[test]
    fn impossible_names_are_not_set() {
        let before = live();
        for name in ["", "A=B", "=", "NUL\0NAME"] {
            let word = values::string(name);
            // SAFETY: a live string, borrowed for the call.
            assert_eq!(
                option_text(unsafe { cheby_env_get(word) }),
                None,
                "{name:?}"
            );
            // SAFETY: owned by the test.
            unsafe { rc::drop(word) };
        }
        assert_eq!(live(), before);
    }

    #[test]
    fn set_and_unset_variables() {
        let before = live();
        // `PATH` is set in every test environment; a long random name is
        // not.
        let path = values::string("PATH");
        let missing = values::string("CHEBY_SURELY_NOT_SET_7F3A9C");
        // SAFETY: live strings, borrowed for the calls, then dropped.
        unsafe {
            assert_eq!(
                option_text(cheby_env_get(path)),
                std::env::var_os("PATH").map(|v| v.to_string_lossy().into_owned())
            );
            assert_eq!(option_text(cheby_env_get(missing)), None);
            rc::drop(path);
            rc::drop(missing);
        }
        assert_eq!(live(), before);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let text = OsString::from_vec(vec![b'a', 0xFF, b'b']);
            assert_eq!(lossy(&text), "a\u{FFFD}b");
        }
        assert!(valid_name("HOME"));
        assert!(!valid_name("A=B"));
    }
}
