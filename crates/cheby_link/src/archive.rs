//! The runtime archive `libcheby_rt_exe.a` (D-928): where `cheby build`
//! finds it (D-929) and whether it was built from the runtime this
//! compiler was built with (D-930).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use object::read::archive::ArchiveFile;
use object::{Object as _, ObjectSymbol as _};

use cheby_runtime::abi;

use crate::LinkError;

/// The archive's file name.
pub const ARCHIVE_FILE: &str = "libcheby_rt_exe.a";

/// The hidden variable that names the archive instead (D-929).
pub const ARCHIVE_VAR: &str = "CHEBY_RT_ARCHIVE";

/// The runtime archive: the absolute path `override_path` names (the
/// value of `CHEBY_RT_ARCHIVE`, D-929), or [`ARCHIVE_FILE`] in the
/// directory of `exe`, the running `cheby`.
///
/// # Errors
///
/// [`LinkError::RelativeArchiveVar`] for a relative `override_path`, and
/// [`LinkError::MissingArchive`] when the file does not exist.
pub fn find_archive(override_path: Option<&OsStr>, exe: &Path) -> Result<PathBuf, LinkError> {
    let path = match override_path.filter(|path| !path.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(LinkError::RelativeArchiveVar(path));
            }
            path
        }
        None => exe
            .parent()
            .map_or_else(|| PathBuf::from(ARCHIVE_FILE), |dir| dir.join(ARCHIVE_FILE)),
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(LinkError::MissingArchive(path))
    }
}

/// Checks that `archive` defines this compiler's runtime ID's symbol
/// `abi::sym::RUNTIME_ID_SYMBOL` (with Mach-O's leading underscore), so
/// it was built from the same runtime sources (D-930). It reads the
/// archive's symbol index, and the members' symbol tables only when the
/// archive has no index.
///
/// # Errors
///
/// [`LinkError::BadArchive`] when the file cannot be read or is not an
/// archive, and [`LinkError::StaleArchive`] when the symbol is missing.
pub fn check_runtime_id(archive: &Path) -> Result<(), LinkError> {
    let bad = |message: String| LinkError::BadArchive {
        path: archive.to_owned(),
        message,
    };
    let bytes = std::fs::read(archive).map_err(|e| bad(e.to_string()))?;
    let file = ArchiveFile::parse(&*bytes).map_err(|e| bad(e.to_string()))?;
    let symbol = abi::sym::RUNTIME_ID_SYMBOL;
    let wanted = |name: &[u8]| {
        name == symbol.as_bytes() || name.strip_prefix(b"_") == Some(symbol.as_bytes())
    };
    if let Some(index) = file.symbols().map_err(|e| bad(e.to_string()))? {
        for entry in index {
            let entry = entry.map_err(|e| bad(e.to_string()))?;
            if wanted(entry.name()) {
                return Ok(());
            }
        }
    } else if members_define(&file, &bytes, &wanted) {
        return Ok(());
    }
    Err(LinkError::StaleArchive {
        path: archive.to_owned(),
        symbol: symbol.to_owned(),
    })
}

/// Whether a member object of an archive without a symbol index defines
/// a global symbol `wanted` accepts.
fn members_define(file: &ArchiveFile<'_>, bytes: &[u8], wanted: &dyn Fn(&[u8]) -> bool) -> bool {
    file.members().flatten().any(|member| {
        let Ok(data) = member.data(bytes) else {
            return false;
        };
        let Ok(object) = object::File::parse(data) else {
            return false;
        };
        object
            .symbols()
            .any(|s| s.is_definition() && s.is_global() && s.name_bytes().is_ok_and(wanted))
    })
}
