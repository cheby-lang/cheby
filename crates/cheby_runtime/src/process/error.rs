//! A failed `std::process` operation as Rust data (D-981, D-1915), with
//! `std::io`'s kind codes (`io.rs`).

use std::io;

use crate::abi::Word;
use crate::file::FileError;
use crate::io::{KIND_BROKEN_PIPE, err_pair, os_message};
pub(super) use crate::io::{
    KIND_CLOSED, KIND_INVALID_DATA, KIND_INVALID_INPUT, KIND_NOT_FOUND, KIND_OTHER,
    KIND_PERMISSION_DENIED,
};
use crate::sched::io::is_closed_error;

/// A failed operation: its kind code and, for kind 0 only, the system's
/// message. A pool job may make it; the fiber turns it into the `Err` pair
/// ([`ProcError::word`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcError {
    pub(crate) kind: i64,
    pub(crate) message: String,
}

impl From<FileError> for ProcError {
    /// A `File`'s failure as a child's stream (D-1909, D-1924).
    fn from(error: FileError) -> Self {
        Self {
            kind: error.kind,
            message: error.message,
        }
    }
}

impl ProcError {
    /// A failure of a kind whose text `std::io` fixes.
    pub(super) fn fixed(kind: i64) -> Self {
        Self {
            kind,
            message: String::new(),
        }
    }

    /// Kind 12.
    pub(super) fn closed() -> Self {
        Self::fixed(KIND_CLOSED)
    }

    /// Kind 13.
    pub(super) fn invalid_input() -> Self {
        Self::fixed(KIND_INVALID_INPUT)
    }

    /// Kind 0 with `message`.
    pub(super) fn other(message: &str) -> Self {
        Self {
            kind: KIND_OTHER,
            message: message.to_owned(),
        }
    }

    /// Kind 0 with `error`'s message.
    pub(super) fn of_other(error: &io::Error) -> Self {
        Self::other(&os_message(error))
    }

    /// The failure of a start (D-1916): not found 1 (the program or the
    /// directory), permission denied 2, every other failure 0.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "`io::ErrorKind` is non-exhaustive"
    )]
    pub(super) fn of_start(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::fixed(KIND_NOT_FOUND),
            io::ErrorKind::PermissionDenied => Self::fixed(KIND_PERMISSION_DENIED),
            _ => Self::of_other(error),
        }
    }

    /// The failure of a read or write of a pipe: 12 after a close, 14 for
    /// `EPIPE` (D-1915), every other failure 0.
    pub(super) fn of_pipe(error: &io::Error) -> Self {
        if is_closed_error(error) {
            Self::closed()
        } else if error.kind() == io::ErrorKind::BrokenPipe {
            Self::fixed(KIND_BROKEN_PIPE)
        } else {
            Self::of_other(error)
        }
    }

    /// The `Err` pair, made on the fiber.
    pub(super) fn word(&self) -> Word {
        err_pair(self.kind, &self.message)
    }
}
