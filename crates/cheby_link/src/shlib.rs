//! A package's shared library (D-941): what `cheby run`, `cheby test`
//! and the REPL load with `dlopen`, and what `cheby build`'s resolver of
//! E0053 opens (D-943).
//!
//! - One library per package with a `[native]` table: the forwarder
//!   object, the package's C objects in `sources` order, then its
//!   `libraries` as `-l<name>`. On Linux `cc -shared -Wl,-z,defs`, on
//!   macOS `cc -dynamiclib`, so an undefined name in the package's C code
//!   fails the link at once, as it would in an executable. A package that
//!   names only `libraries` gets the forwarder alone, linked with them, so
//!   the system's loader finds them the same way.
//! - The forwarder ([`FORWARDER_SOURCE`]) defines `cheby_ffi_table`, the
//!   table of the running `cheby`'s `cheby_ffi_` functions, one entry per
//!   function in `cheby.h`'s declaration order and a null word last
//!   (D-1196, D-1226), and each of `cheby.h`'s functions as a
//!   call through its entry, so the package's C code reaches the runtime
//!   of the process that loaded it (D-941). It is compiled beside the
//!   embedded `cheby.h`, which it includes. An executable links no
//!   forwarder: its C objects call the runtime archive's definitions.
//! - `cc` runs in a fresh private directory (`cheby-objects-<pid>-<n>`, as
//!   the link's, D-1112) with relative names, under the output name
//!   `lib<package>.so` or `lib<package>.dylib`, so no path of the machine
//!   reaches the library (macOS takes its install name from it). The
//!   library's bytes come back to the caller, which stores them
//!   (D-1197).
//!
//! [`defined_symbols`] reads the global names a C object defines, for
//! D-944's check that no package defines a name the runtime exports.

use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::link::{CC, OBJECTS_PREFIX, PrivateDir, REMOVED_VARS, native_object_file, sweep_dead};

/// The forwarder's C source (D-941): `cheby_ffi_table`, one entry per
/// function of `cheby.h` in the header's declaration order
/// (`cheby_runtime::ffi::NAMES`, D-1226) and the null word
/// that ends it (D-1196), and a definition of each function that calls
/// through its entry. It includes the embedded `cheby.h`, written beside
/// it, so the compiler checks every definition against the header. The
/// running `cheby` fills the table when it loads the library
/// (`cheby_jit::NativeLibrary::open`).
pub const FORWARDER_SOURCE: &str = "\
/* The forwarder of a package's shared library (D-941): each function of
   cheby.h jumps through this table, which the running cheby fills with its
   runtime's functions in cheby.h's declaration order when it loads the
   library; the null word ends it (D-1196). */
#include \"cheby.h\"

void *cheby_ffi_table[26] = { 0 };

cheby_value *cheby_ffi_dup(cheby_value *v) {
    return ((cheby_value * (*)(cheby_value *))cheby_ffi_table[0])(v);
}

void cheby_ffi_drop(cheby_value *v) {
    ((void (*)(cheby_value *))cheby_ffi_table[1])(v);
}

cheby_value *cheby_ffi_string_new(const char *bytes, size_t len) {
    return ((cheby_value * (*)(const char *, size_t))cheby_ffi_table[2])(bytes, len);
}

const char *cheby_ffi_string_bytes(cheby_value *s, size_t *len) {
    return ((const char * (*)(cheby_value *, size_t *))cheby_ffi_table[3])(s, len);
}

cheby_value *cheby_ffi_bytes_new(const uint8_t *data, size_t len) {
    return ((cheby_value * (*)(const uint8_t *, size_t))cheby_ffi_table[4])(data, len);
}

const uint8_t *cheby_ffi_bytes_data(cheby_value *b, size_t *len) {
    return ((const uint8_t * (*)(cheby_value *, size_t *))cheby_ffi_table[5])(b, len);
}

cheby_value *cheby_ffi_int_new(int64_t value) {
    return ((cheby_value * (*)(int64_t))cheby_ffi_table[6])(value);
}

int64_t cheby_ffi_int_get(cheby_value *v) {
    return ((int64_t (*)(cheby_value *))cheby_ffi_table[7])(v);
}

cheby_value *cheby_ffi_float_new(double value) {
    return ((cheby_value * (*)(double))cheby_ffi_table[8])(value);
}

double cheby_ffi_float_get(cheby_value *v) {
    return ((double (*)(cheby_value *))cheby_ffi_table[9])(v);
}

cheby_value *cheby_ffi_bool_new(bool value) {
    return ((cheby_value * (*)(bool))cheby_ffi_table[10])(value);
}

bool cheby_ffi_bool_get(cheby_value *v) {
    return ((bool (*)(cheby_value *))cheby_ffi_table[11])(v);
}

cheby_value *cheby_ffi_nil(void) {
    return ((cheby_value * (*)(void))cheby_ffi_table[12])();
}

cheby_value *cheby_ffi_some(cheby_value *payload) {
    return ((cheby_value * (*)(cheby_value *))cheby_ffi_table[13])(payload);
}

cheby_value *cheby_ffi_none(void) {
    return ((cheby_value * (*)(void))cheby_ffi_table[14])();
}

cheby_value *cheby_ffi_option_get(cheby_value *option) {
    return ((cheby_value * (*)(cheby_value *))cheby_ffi_table[15])(option);
}

cheby_value *cheby_ffi_ok(cheby_value *payload) {
    return ((cheby_value * (*)(cheby_value *))cheby_ffi_table[16])(payload);
}

cheby_value *cheby_ffi_err(cheby_value *payload) {
    return ((cheby_value * (*)(cheby_value *))cheby_ffi_table[17])(payload);
}

bool cheby_ffi_result_is_ok(cheby_value *result) {
    return ((bool (*)(cheby_value *))cheby_ffi_table[18])(result);
}

cheby_value *cheby_ffi_result_get(cheby_value *result) {
    return ((cheby_value * (*)(cheby_value *))cheby_ffi_table[19])(result);
}

cheby_value *cheby_ffi_list_new(cheby_value **items, size_t n) {
    return ((cheby_value * (*)(cheby_value **, size_t))cheby_ffi_table[20])(items, n);
}

size_t cheby_ffi_list_length(cheby_value *list) {
    return ((size_t (*)(cheby_value *))cheby_ffi_table[21])(list);
}

cheby_value *cheby_ffi_list_get(cheby_value *list, size_t index) {
    return ((cheby_value * (*)(cheby_value *, size_t))cheby_ffi_table[22])(list, index);
}

int cheby_ffi_write_stdout(const char *bytes, size_t len) {
    return ((int (*)(const char *, size_t))cheby_ffi_table[23])(bytes, len);
}

int cheby_ffi_write_stderr(const char *bytes, size_t len) {
    return ((int (*)(const char *, size_t))cheby_ffi_table[24])(bytes, len);
}
";

/// The number of entries of the forwarder's table: one per function of
/// `cheby.h`, then the null word that ends it (D-1196).
pub const FFI_TABLE_LEN: usize = cheby_runtime::ffi::NAMES.len() + 1;

/// The symbol of the forwarder's table (D-941).
pub const FFI_TABLE: &str = "cheby_ffi_table";

/// The flags of the forwarder's compile: the C compile's fixed ones
/// (D-942), with nothing to include and no path to map, since it is
/// compiled by its relative name in a private directory.
const FORWARDER_FLAGS: [&str; 4] = ["-c", "-O2", "-fPIC", "-g0"];

/// The link flags of a shared library on the host (D-941). On Linux,
/// `--no-as-needed` keeps every `libraries` entry a dependency of the
/// library even when its own code references none of their names (a
/// package that names only `libraries`): a toolchain that links with
/// `--as-needed` by default would drop them, and the library would no
/// longer reach the names its package's externals bind.
#[must_use]
pub fn flags() -> Vec<String> {
    if cfg!(target_os = "macos") {
        vec!["-dynamiclib".to_owned()]
    } else {
        vec![
            "-shared".to_owned(),
            "-Wl,-z,defs".to_owned(),
            "-Wl,--no-as-needed".to_owned(),
        ]
    }
}

/// The library's file name for the package `package`: `lib<package>.so`,
/// or `lib<package>.dylib` on macOS.
#[must_use]
pub fn file_name(package: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{package}.dylib")
    } else {
        format!("lib{package}.so")
    }
}

/// What one shared library takes.
#[derive(Clone, Copy, Debug)]
pub struct SharedJob<'a> {
    /// The package's name, which names the library.
    pub package: &'a str,
    /// The package's C objects in `sources` order: `(source, bytes)`.
    pub objects: &'a [(String, Vec<u8>)],
    /// The package's `libraries`, in manifest order.
    pub libraries: &'a [String],
}

/// Why a shared library could not be made.
#[derive(Debug)]
pub enum ShlibError {
    /// `cc` could not be started: the system's message.
    NoCc(String),
    /// The forwarder's compile or the link failed: what failed, `cc`'s
    /// status and its output, unparsed.
    Failed {
        package: String,
        status: String,
        output: String,
    },
    /// A temporary file could not be written or read back.
    Io(String),
}

impl fmt::Display for ShlibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCc(message) | Self::Io(message) => write!(f, "{message}"),
            // D-1201: plan §7 Q6 (4)'s text.
            Self::Failed {
                package,
                status,
                output,
            } => {
                write!(
                    f,
                    "linking the native code of package `{package}` with `cc` failed ({status})"
                )?;
                let output = output.trim_end();
                if !output.is_empty() {
                    write!(f, ":\n{output}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ShlibError {}

/// Links `job` into a shared library and returns its bytes (module
/// documentation).
///
/// # Errors
///
/// [`ShlibError::NoCc`] when `cc` cannot be started,
/// [`ShlibError::Failed`] with its output when the forwarder's compile or
/// the link fails, [`ShlibError::Io`] when a temporary file cannot be
/// written or read.
pub fn link_shared(job: &SharedJob<'_>) -> Result<Vec<u8>, ShlibError> {
    let tmp = std::env::temp_dir();
    sweep_dead(&tmp, OBJECTS_PREFIX);
    let dir =
        PrivateDir::create(&tmp, OBJECTS_PREFIX).map_err(|e| ShlibError::Io(e.to_string()))?;
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes)
            .map_err(|e| ShlibError::Io(format!("cannot write `{}`: {e}", path.display())))
    };
    write(crate::cc::HEADER_NAME, crate::cc::HEADER.as_bytes())?;
    write("forwarder.c", FORWARDER_SOURCE.as_bytes())?;
    run(
        dir.path(),
        job.package,
        FORWARDER_FLAGS
            .iter()
            .map(|flag| (*flag).to_owned())
            .chain(["-o", "forwarder.o", "forwarder.c"].map(str::to_owned)),
    )?;
    let mut inputs = vec!["forwarder.o".to_owned()];
    for (index, (_, bytes)) in job.objects.iter().enumerate() {
        let name = native_object_file(index);
        write(&name, bytes)?;
        inputs.push(name);
    }
    let output = file_name(job.package);
    run(
        dir.path(),
        job.package,
        flags()
            .into_iter()
            .chain(["-o".to_owned(), output.clone()])
            .chain(inputs)
            .chain(job.libraries.iter().map(|name| format!("-l{name}"))),
    )?;
    let path = dir.path().join(&output);
    std::fs::read(&path)
        .map_err(|e| ShlibError::Io(format!("cannot read `{}`: {e}", path.display())))
}

/// Runs `cc` with `args` in `dir`.
fn run(
    dir: &Path,
    package: &str,
    args: impl IntoIterator<Item = String>,
) -> Result<(), ShlibError> {
    let mut cmd = Command::new(CC);
    for var in REMOVED_VARS {
        cmd.env_remove(var);
    }
    cmd.env("SOURCE_DATE_EPOCH", "0")
        .current_dir(dir)
        .args(args);
    let output = cmd.output().map_err(|e| ShlibError::NoCc(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Err(ShlibError::Failed {
        package: package.to_owned(),
        status: output.status.to_string(),
        output: text,
    })
}

/// The global names the object `bytes` defines, sorted, as C spells them
/// (Mach-O's leading `_` removed).
///
/// # Errors
///
/// The `object` crate's message when the bytes are not an object.
pub fn defined_symbols(bytes: &[u8]) -> Result<Vec<String>, String> {
    use object::{Object as _, ObjectSymbol as _};
    let file = object::File::parse(bytes).map_err(|e| e.to_string())?;
    let mach_o = matches!(file.format(), object::BinaryFormat::MachO);
    let mut names: Vec<String> = file
        .symbols()
        .filter(|symbol| symbol.is_global() && !symbol.is_undefined())
        .filter_map(|symbol| symbol.name().ok().map(str::to_owned))
        .map(|name| {
            if mach_o {
                name.strip_prefix('_')
                    .map_or_else(|| name.clone(), str::to_owned)
            } else {
                name
            }
        })
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}
