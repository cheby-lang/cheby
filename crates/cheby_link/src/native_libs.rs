//! The system libraries the runtime archive needs, per target (D-933),
//! and the build resolver of E0053 over them (D-943).
//!
//! GAP(aot-native-libs): each target's list is what
//! `rustc --print native-static-libs` prints for `cheby_rt_exe`, in its
//! order, each library as the linker takes it (`-l<name>`) with the file
//! the system loader opens for it, and the macOS minimum version is the
//! one the archive was built with (`rustc --print deployment-target` for
//! the target, without `MACOSX_DEPLOYMENT_TARGET`). `cargo xtask
//! check-native-libs` compares the list with the archive crate's print for
//! the host and opens every recorded file, so a list that drifts from Rust
//! or a file name that drifts from the system fails there, not as a false
//! E0053 (D-943). Windows has no list: executables there are M6's (D-935).

/// One system library of the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeLib {
    /// The name the linker takes, `-l<name>`.
    pub name: &'static str,
    /// The file the system loader opens for it.
    pub file: &'static str,
}

/// macOS (aarch64; D-892): `-liconv -lSystem -lc -lm`.
const MACOS: &[NativeLib] = &[
    NativeLib {
        name: "iconv",
        file: "/usr/lib/libiconv.2.dylib",
    },
    NativeLib {
        name: "System",
        file: "/usr/lib/libSystem.B.dylib",
    },
    NativeLib {
        name: "c",
        file: "/usr/lib/libc.dylib",
    },
    NativeLib {
        name: "m",
        file: "/usr/lib/libm.dylib",
    },
];

/// Linux with glibc, x86-64 and aarch64:
/// `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`.
const LINUX_GNU: &[NativeLib] = &[
    NativeLib {
        name: "gcc_s",
        file: "libgcc_s.so.1",
    },
    NativeLib {
        name: "util",
        file: "libutil.so.1",
    },
    NativeLib {
        name: "rt",
        file: "librt.so.1",
    },
    NativeLib {
        name: "pthread",
        file: "libpthread.so.0",
    },
    NativeLib {
        name: "m",
        file: "libm.so.6",
    },
    NativeLib {
        name: "dl",
        file: "libdl.so.2",
    },
    NativeLib {
        name: "c",
        file: "libc.so.6",
    },
];

/// The macOS minimum version of the archive: Rust's default deployment
/// target for `aarch64-apple-darwin`.
const MACOS_MIN_VERSION: &str = "11.0";

/// The host target's list, in the order the linker takes it; empty on a
/// target without executables in M3 (D-935).
#[must_use]
pub const fn native_libs() -> &'static [NativeLib] {
    if cfg!(target_os = "macos") {
        MACOS
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        LINUX_GNU
    } else {
        &[]
    }
}

/// The macOS minimum version the archive was built with, on macOS.
#[must_use]
pub const fn macos_min_version() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some(MACOS_MIN_VERSION)
    } else {
        None
    }
}

/// The libraries' link arguments, `-l<name>`, in order.
#[must_use]
pub fn link_args() -> Vec<String> {
    native_libs()
        .iter()
        .map(|lib| format!("-l{}", lib.name))
        .collect()
}

/// The symbols of `symbols` that no library of the list defines, in their
/// order: the last step of D-943's build resolver, after the runtime's
/// exported names (and, from N8a on, the package's own library). A
/// library is opened by its recorded file name; one that cannot be opened
/// defines nothing, which `cargo xtask check-native-libs` rules out.
#[must_use]
pub fn undefined<'a>(symbols: &[&'a str]) -> Vec<&'a str> {
    if symbols.is_empty() {
        return Vec::new();
    }
    let handles: Vec<loader::Library> = native_libs()
        .iter()
        .filter_map(|lib| loader::Library::open(lib.file).ok())
        .collect();
    symbols
        .iter()
        .copied()
        .filter(|symbol| !handles.iter().any(|lib| lib.defines(symbol)))
        .collect()
}

/// Opens `lib` by its recorded file name, as the build resolver does.
///
/// # Errors
///
/// The loader's message when the file cannot be opened.
pub fn open(lib: &NativeLib) -> Result<(), String> {
    loader::Library::open(lib.file).map(drop)
}

#[cfg(unix)]
mod loader {
    use std::ffi::{CStr, CString};

    /// A library opened with `dlopen`, closed when dropped.
    pub(super) struct Library(*mut libc::c_void);

    impl Library {
        pub(super) fn open(file: &str) -> Result<Self, String> {
            let name = CString::new(file).map_err(|e| e.to_string())?;
            // SAFETY: `name` is a valid NUL-terminated string that outlives
            // the call; the libraries of the list are system libraries
            // already loaded into every process, so opening them runs no
            // new initializers.
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if handle.is_null() {
                // SAFETY: `dlerror` returns null or a NUL-terminated string
                // that stays valid until the next `dl*` call on this
                // thread, and it is copied before any.
                let message = unsafe {
                    let error = libc::dlerror();
                    if error.is_null() {
                        String::from("dlopen failed")
                    } else {
                        CStr::from_ptr(error).to_string_lossy().into_owned()
                    }
                };
                return Err(format!("{file}: {message}"));
            }
            Ok(Self(handle))
        }

        pub(super) fn defines(&self, symbol: &str) -> bool {
            let Ok(name) = CString::new(symbol) else {
                return false;
            };
            // SAFETY: `self.0` is a live handle from `dlopen`, and `name` is
            // a valid NUL-terminated string; `dlsym` only looks the name up.
            !unsafe { libc::dlsym(self.0, name.as_ptr()) }.is_null()
        }
    }

    impl Drop for Library {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a live handle from `dlopen`, closed once.
            unsafe {
                libc::dlclose(self.0);
            }
        }
    }
}

#[cfg(not(unix))]
mod loader {
    /// No loader off Unix: executables there are M6's (D-935).
    pub(super) struct Library;

    impl Library {
        pub(super) fn open(file: &str) -> Result<Self, String> {
            Err(format!("{file}: executables on Windows are M6 (D-935)"))
        }

        #[expect(
            clippy::unused_self,
            reason = "the shape of the Unix loader's method; no library opens here"
        )]
        pub(super) fn defines(&self, _symbol: &str) -> bool {
            false
        }
    }
}
