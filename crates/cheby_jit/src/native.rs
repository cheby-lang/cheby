//! A package's native code in the JIT (D-941): its shared library, opened
//! with `dlopen(path, RTLD_NOW | RTLD_LOCAL)` and never closed, and the
//! resolver of D-455 over the runtime, the package libraries and the
//! process.
//!
//! The order is D-941's: the runtime's symbols (`cheby_runtime::symbols()`)
//! first, then the package libraries in the order they were added (build
//! order), then the process (`dlsym(RTLD_DEFAULT)`). `RTLD_LOCAL` keeps a
//! library's names out of the process's table, so the third step never
//! finds them by accident and two packages' libraries do not meet.
//!
//! A library is never unloaded: code compiled against it may run until
//! the process exits (a REPL session's detached fibers, D-712), and the
//! forwarder's table it holds is the running `cheby`'s: [`NativeLibrary::open`]
//! fills it with the runtime's `cheby_ffi_` functions (D-941, D-945).

use std::path::{Path, PathBuf};

/// A package's shared library, opened with `dlopen(RTLD_NOW |
/// RTLD_LOCAL)` and never closed (D-941).
#[derive(Debug)]
pub struct NativeLibrary {
    /// The `dlopen` handle, as an integer so the library is `Send` and
    /// `Sync` (a handle may be used from any thread).
    handle: usize,
    /// The file it was opened from.
    path: PathBuf,
}

impl NativeLibrary {
    /// Opens the shared library at `path`, resolving every name it needs
    /// now (`RTLD_NOW`), so a missing one fails here rather than at a
    /// call.
    ///
    /// # Errors
    ///
    /// The loader's message when the library cannot be opened; on Windows
    /// always, since native code there is M6's (D-935).
    pub fn open(path: &Path) -> Result<Self, String> {
        let handle = loader::open(path)?;
        let library = Self {
            handle,
            path: path.to_path_buf(),
        };
        library.fill_table();
        Ok(library)
    }

    /// Fills the forwarder's `cheby_ffi_table` with this process's
    /// `cheby_ffi_` functions, in `cheby.h`'s declaration order
    /// (`cheby_runtime::ffi::table`, D-941, D-1226), so the
    /// package's C code reaches the runtime that loaded it. A library
    /// without the table (not a package's) is left alone.
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "the symbol is the C array `void *cheby_ffi_table[]`, which the C compiler aligns \
                  as a pointer"
    )]
    fn fill_table(&self) {
        let Some(table) = self.lookup(cheby_runtime::ffi::TABLE_SYMBOL) else {
            return;
        };
        let table = table.cast_mut().cast::<*const u8>();
        for (index, address) in cheby_runtime::ffi::table().into_iter().enumerate() {
            // SAFETY: the forwarder defines `cheby_ffi_table` as a writable
            // array of one pointer per function of `cheby.h` and a null word
            // (`cheby_link::shlib::FORWARDER_SOURCE`), and a library's key
            // covers the forwarder's source (D-1197), so this library's
            // table has `table()`'s length plus one. Nothing runs the
            // library's C code before `open` returns.
            unsafe { table.add(index).write(address) };
        }
    }

    /// The file the library was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The address of `symbol` in the library or the libraries it links
    /// (`dlsym` on its handle), if it defines it.
    #[must_use]
    pub fn lookup(&self, symbol: &str) -> Option<*const u8> {
        loader::lookup(self.handle, symbol)
    }
}

/// What separates a native symbol from its library generation in the name
/// a batch imports it under ([`generation_symbol`]): no C identifier holds
/// `$`.
const GENERATION_MARK: &str = "$cheby$gen";

/// The name under which code compiled after a REPL `:reload` that changed
/// the package's native code imports `symbol` (D-712, D-941): Cranelift
/// resolves a name once per JIT, and the session's import words serve
/// every batch (D-1105), so the new code needs names of its own to reach
/// the new library while old code keeps the old one. A JIT resolves it in
/// the libraries of `generation` ([`crate::Jit::add_library_generation`])
/// (D-1197). Generation 0 is the plain symbol.
#[must_use]
pub fn generation_symbol(symbol: &str, generation: u32) -> String {
    if generation == 0 {
        symbol.to_owned()
    } else {
        format!("{symbol}{GENERATION_MARK}{generation}")
    }
}

/// The symbol a name of [`generation_symbol`] stands for, and its
/// generation; a plain symbol is generation 0.
#[must_use]
pub fn split_generation(name: &str) -> (&str, u32) {
    name.rsplit_once(GENERATION_MARK)
        .and_then(|(symbol, digits)| {
            let generation = digits.parse::<u32>().ok()?;
            (generation > 0 && !digits.starts_with('0')).then_some((symbol, generation))
        })
        .unwrap_or((name, 0))
}

/// D-941's resolver: the address `symbol` resolves to in the runtime's
/// symbols, then in `libraries` in order, then in the process; `None`
/// when nothing defines it. The JIT binds a native symbol to the same
/// address, and the driver decides E0053 with it before anything runs
/// (D-943).
#[must_use]
pub fn resolve(symbol: &str, libraries: &[std::sync::Arc<NativeLibrary>]) -> Option<*const u8> {
    if let Some(&(_, address)) = cheby_runtime::symbols()
        .iter()
        .find(|(name, _)| *name == symbol)
    {
        return Some(address);
    }
    libraries
        .iter()
        .find_map(|library| library.lookup(symbol))
        .or_else(|| loader::lookup_process(symbol))
}

#[cfg(unix)]
mod loader {
    use std::ffi::{CStr, CString};
    use std::path::Path;

    pub(super) fn open(path: &Path) -> Result<usize, String> {
        use std::os::unix::ffi::OsStrExt as _;
        let name = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: `name` is a valid NUL-terminated string that outlives the
        // call. The library is the package's own native code, which the
        // program asked for (D-941); running its initializers is what
        // loading it means, as for a linked executable.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: `dlerror` returns null or a NUL-terminated string that
            // stays valid until the next `dl*` call on this thread, and it
            // is copied before any.
            let message = unsafe {
                let error = libc::dlerror();
                if error.is_null() {
                    String::from("dlopen failed")
                } else {
                    CStr::from_ptr(error).to_string_lossy().into_owned()
                }
            };
            return Err(message);
        }
        Ok(handle.expose_provenance())
    }

    pub(super) fn lookup(handle: usize, symbol: &str) -> Option<*const u8> {
        let name = CString::new(symbol).ok()?;
        let handle = std::ptr::with_exposed_provenance_mut::<libc::c_void>(handle);
        // SAFETY: `handle` came from a successful `dlopen` and is never
        // closed, so it stays valid; `name` is a valid NUL-terminated
        // string; `dlsym` only looks the name up.
        let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
        (!address.is_null()).then_some(address.cast_const().cast::<u8>())
    }

    pub(super) fn lookup_process(symbol: &str) -> Option<*const u8> {
        let name = CString::new(symbol).ok()?;
        // SAFETY: `name` is a valid NUL-terminated string that outlives the
        // call, and `RTLD_DEFAULT` is a valid handle; `dlsym` only reads.
        let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!address.is_null()).then_some(address.cast_const().cast::<u8>())
    }
}

#[cfg(not(unix))]
mod loader {
    use std::path::Path;

    pub(super) fn open(path: &Path) -> Result<usize, String> {
        Err(format!(
            "{}: native code on Windows is M6 (D-935)",
            path.display()
        ))
    }

    pub(super) fn lookup(_handle: usize, _symbol: &str) -> Option<*const u8> {
        None
    }

    /// Only the runtime's symbols are known off Unix, as in the JIT.
    pub(super) fn lookup_process(_symbol: &str) -> Option<*const u8> {
        None
    }
}
