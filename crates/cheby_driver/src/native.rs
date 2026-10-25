//! A package's native code (D-940 to D-944): the C objects of its
//! `[native]` table, cached, and its shared library, loaded.
//!
//! Every command that runs or builds code (the front end's `Build` mode:
//! `cheby run`, `cheby test`, `cheby build` and the REPL) calls [`prepare`]
//! once the package checks without errors; `cheby check` compiles no C and
//! opens no library (D-532, D-941). [`prepare`]:
//!
//! 1. On Windows, a table that names sources or libraries is a tool error
//!    until M6 (D-1202, plan §7 Q6 (2)).
//! 2. Writes `cheby.h` ([`cheby_link::cc::HEADER`], D-945) into the
//!    cache's include directory `v1/include/`.
//! 3. Compiles each source once, in `sources` order ([`cheby_link::cc`],
//!    D-942), through the cache: a C object is a record under a
//!    [`KeyKind::CObj`] key of the source's package-relative path and
//!    bytes, the compiler's identity (its resolved path and `cc
//!    --version`), the flags in their fixed form and `cheby.h`'s hash,
//!    never a machine path (D-1199: the relative path is
//!    keyed since `__FILE__` and ELF's file symbol name it). The record
//!    names the object's blob and every file the compile read, in D-942's
//!    three forms, each with its hash; a hit needs every hash unchanged
//!    (ccache's direct mode), so an edited header recompiles.
//! 4. Refuses a C object that defines a name the runtime exports (D-944),
//!    reading the object's symbol table, before anything links or loads.
//! 5. Links the package's shared library ([`cheby_link::shlib`], D-941)
//!    from the forwarder, the objects and the `libraries`, stored as a
//!    plain file `v1/lib/<key>.so` (`.dylib` on macOS) under a
//!    [`KeyKind::NativeLib`] key of its inputs, so changed C code gets a
//!    new path (D-712), and opens it with `dlopen(RTLD_NOW | RTLD_LOCAL)`,
//!    never to close it (D-1197). A package that names
//!    only `libraries` gets the forwarder alone.
//!
//! Without a cache (some tests), the objects stay in memory and the
//! library is written into a private temporary directory that is removed
//! once the library is open: the loader keeps its mapping.
//!
//! `CHEBY_CACHE_LOG` lists `cobj <source>` and `nlib <package>` lookups
//! (D-973). The tool errors' texts are plan §7 Q6 (4)'s
//! (D-1201).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cheby_jit::NativeLibrary;
use cheby_link::cc::{self, CcError, CompilerIdentity, Dependency, HEADER, HEADER_NAME};
use cheby_link::shlib::{self, ShlibError};
use cheby_package::NativeTable;

use crate::cache::{Hash128, Key, KeyKind, Store};
use crate::{Cache, Error};

/// The directory of packages' shared libraries under `v1/`
/// (D-1197): plain files, trimmed on D-961's rule.
pub(crate) const LIB_DIR: &str = "lib";

/// The directory under `v1/` that holds `cheby.h` for C compiles (D-945).
pub(crate) const INCLUDE_DIR: &str = "include";

/// The magic and version that start a C object's record.
const COBJ_MAGIC: &[u8; 8] = b"chcobj01";

/// E0053's note in every command (the user's answer to plan §7 Q5 (5) (b),
/// D-1203): package code takes part in every
/// resolver since D-941 and D-943.
pub(crate) const E0053_NOTE: &str = "the symbol must be defined by the Cheby runtime, the \
     package's native code or libraries, or the running process";

/// The tool error of a `[native]` table on Windows until M6
/// (D-1202, plan §7 Q6 (2)).
pub(crate) const WINDOWS_NOT_YET: &str =
    "`[native]` is not available yet on Windows (planned for M6)";

/// A package's native code, ready for the commands that run or build code.
#[derive(Debug, Default)]
pub(crate) struct PackageNative {
    /// The package's C objects, in `sources` order: `(source, bytes)`.
    pub objects: Vec<(String, Vec<u8>)>,
    /// The package's `libraries`, in manifest order.
    pub libraries: Vec<String>,
    /// The package's shared library, open; `None` for a table that names
    /// nothing.
    pub library: Option<Arc<NativeLibrary>>,
    /// The package root, name and table this was prepared from, so a REPL
    /// `:reload` can prepare it again (`cheby.toml` is not read again,
    /// D-625).
    pub source: Option<(PathBuf, String, NativeTable)>,
    /// The hash of everything the shared library is linked from, the same
    /// in every cache: a `:reload` whose C code changed it loads a new
    /// library (D-712, D-941).
    pub inputs: Option<Hash128>,
}

impl PackageNative {
    /// The libraries a JIT resolves in, in build order (D-941): this
    /// package's, if any.
    pub(crate) fn libraries(&self) -> Vec<Arc<NativeLibrary>> {
        self.library.iter().cloned().collect()
    }
}

/// Compiles and loads the native code of the package `package` at `root`
/// (module documentation).
///
/// # Errors
///
/// A tool error (D-464) for a missing `cc`, a failed compile or link, a C
/// object that defines a runtime export, a library that cannot be loaded,
/// a source or cache file that cannot be read or written, and on Windows.
pub(crate) fn prepare(
    root: &Path,
    package: &str,
    table: &NativeTable,
    cache: Option<&Cache>,
) -> Result<PackageNative, Error> {
    Ok(prepare_unless(root, package, table, cache, None)?.unwrap_or_default())
}

/// [`prepare`], unless the shared library's inputs hash to `known`: then
/// `None`, and no library is linked or loaded (a REPL `:reload` whose C
/// code did not change).
///
/// # Errors
///
/// As [`prepare`].
pub(crate) fn prepare_unless(
    root: &Path,
    package: &str,
    table: &NativeTable,
    cache: Option<&Cache>,
    known: Option<Hash128>,
) -> Result<Option<PackageNative>, Error> {
    let source = Some((root.to_path_buf(), package.to_owned(), table.clone()));
    if table.is_empty() {
        return Ok(Some(PackageNative {
            source,
            ..PackageNative::default()
        }));
    }
    if cfg!(windows) {
        return Err(Error::new(WINDOWS_NOT_YET.to_owned()));
    }
    let root = std::path::absolute(root)
        .map_err(|e| Error::new(format!("cannot find `{}`: {e}", root.display())))?;
    let identity = cc::compiler_identity().map_err(no_cc)?;
    let place = Place::new(cache)?;
    let header_dir = place.include_dir()?;
    let mut objects = Vec::with_capacity(table.sources.len());
    for source in &table.sources {
        let object = place.object(&root, source, &header_dir, &identity)?;
        check_exports(source, &object)?;
        objects.push((source.clone(), object));
    }
    let inputs = library_key(
        crate::cache::BuildId([0; 16]),
        package,
        &objects,
        &table.libraries,
        &identity,
    )
    .hash();
    if known == Some(inputs) {
        return Ok(None);
    }
    let library = place.library(package, &objects, &table.libraries, &identity)?;
    Ok(Some(PackageNative {
        objects,
        libraries: table.libraries.clone(),
        library: Some(library),
        source,
        inputs: Some(inputs),
    }))
}

/// D-944: a C object that defines a name the runtime exports is a tool
/// error before anything links or loads. The names are the JIT's table
/// (`cheby_runtime::symbols()`) and the two that only the runtime archive
/// exports to executables: their entry, `cheby_rt_main`, and the
/// runtime-ID symbol (D-925, D-930; the wave-6 review's R-3).
fn check_exports(source: &str, object: &[u8]) -> Result<(), Error> {
    let defined = cheby_link::shlib::defined_symbols(object).map_err(|e| {
        Error::from(cheby_iface::Ice::new(format!(
            "the object of `{source}` cannot be read: {e}"
        )))
    })?;
    let runtime = cheby_runtime::symbols();
    let archive_only = [
        cheby_link::program::RT_MAIN,
        cheby_runtime::abi::sym::RUNTIME_ID_SYMBOL,
    ];
    if let Some(symbol) = defined.iter().find(|name| {
        runtime.iter().any(|(export, _)| export == name) || archive_only.contains(&name.as_str())
    }) {
        return Err(Error::new(format!(
            "`{source}` defines `{symbol}`, a name the Cheby runtime exports; give the C \
             function another name"
        )));
    }
    Ok(())
}

/// The tool error of a missing `cc` (plan §7 Q6 (4)).
#[expect(
    clippy::needless_pass_by_value,
    reason = "passed to `map_err`, which hands over the error"
)]
fn no_cc(error: CcError) -> Error {
    Error::new(format!(
        "`[native]` needs a C compiler, but `cc` could not be run from PATH: {error}"
    ))
}

/// A failed compile's tool error, its text as [`CcError`] words it.
fn cc_error(error: CcError) -> Error {
    match error {
        CcError::NoCc(_) => no_cc(error),
        other => Error::new(other.to_string()),
    }
}

/// A failed shared library's tool error.
fn shlib_error(error: ShlibError) -> Error {
    match error {
        ShlibError::NoCc(message) => no_cc(CcError::NoCc(message)),
        other => Error::new(other.to_string()),
    }
}

/// Where the include directory, the C objects and the library go: the
/// command's cache, or a private temporary directory.
enum Place<'a> {
    Cache(&'a Cache),
    Temporary(TempDir),
}

impl<'a> Place<'a> {
    fn new(cache: Option<&'a Cache>) -> Result<Self, Error> {
        match cache {
            Some(cache) => Ok(Self::Cache(cache)),
            None => TempDir::new().map(Self::Temporary),
        }
    }

    /// The directory of `cheby.h`, with the header written into it when
    /// it is missing or differs (D-945).
    fn include_dir(&self) -> Result<PathBuf, Error> {
        let dir = match self {
            Self::Cache(cache) => cache.store.layout_dir().join(INCLUDE_DIR),
            Self::Temporary(temp) => temp.path.join(INCLUDE_DIR),
        };
        let header = dir.join(HEADER_NAME);
        if std::fs::read(&header).ok().as_deref() != Some(HEADER.as_bytes()) {
            create_dir(&dir)?;
            write_atomically(&dir, &header, HEADER.as_bytes())?;
        }
        Ok(dir)
    }

    /// The C object of `source`, from the cache when its record and every
    /// file it read are unchanged, else compiled (and stored).
    fn object(
        &self,
        root: &Path,
        source: &str,
        header_dir: &Path,
        identity: &CompilerIdentity,
    ) -> Result<Vec<u8>, Error> {
        let bytes = std::fs::read(root.join(source))
            .map_err(|e| Error::new(format!("cannot read `{source}`: {e}")))?;
        let compile = || {
            cc::compile(&cc::CompileJob {
                root,
                source,
                header_dir,
            })
            .map_err(cc_error)
        };
        let Self::Cache(cache) = self else {
            return Ok(compile()?.object);
        };
        let key = object_key(cache.store.build_id(), source, &bytes, identity);
        let hit = cache
            .reads
            .then(|| cached_object(&cache.store, &key, root))
            .flatten();
        cache.log.record(KeyKind::CObj.tag(), source, hit.is_some());
        if let Some(object) = hit {
            return Ok(object);
        }
        let compiled = compile()?;
        let record = encode_record(&cache.store, &compiled, root)?;
        let store_error = |e: crate::cache::StoreError| Error::new(e.to_string());
        cache.store.put_record(&key, &record).map_err(store_error)?;
        Ok(compiled.object)
    }

    /// The package's shared library, opened: from the cache's file of its
    /// key when there is one, else linked and stored first.
    fn library(
        &self,
        package: &str,
        objects: &[(String, Vec<u8>)],
        libraries: &[String],
        identity: &CompilerIdentity,
    ) -> Result<Arc<NativeLibrary>, Error> {
        let link = || {
            shlib::link_shared(&shlib::SharedJob {
                package,
                objects,
                libraries,
            })
            .map_err(shlib_error)
        };
        let open = |path: &Path| {
            NativeLibrary::open(path).map(Arc::new).map_err(|e| {
                Error::new(format!(
                    "cannot load the native code of package `{package}`: {e}"
                ))
            })
        };
        match self {
            Self::Temporary(temp) => {
                let path = temp.path.join(shlib::file_name(package));
                std::fs::write(&path, link()?)
                    .map_err(|e| Error::new(format!("cannot write `{}`: {e}", path.display())))?;
                open(&path)
            }
            Self::Cache(cache) => {
                let store = &cache.store;
                let key = library_key(store.build_id(), package, objects, libraries, identity);
                let dir = store.layout_dir().join(LIB_DIR);
                let extension = if cfg!(target_os = "macos") {
                    "dylib"
                } else {
                    "so"
                };
                let path = dir.join(format!("{}.{extension}", key.hash().to_hex()));
                let hit = cache.reads && path.is_file();
                cache.log.record(KeyKind::NativeLib.tag(), package, hit);
                if hit {
                    touch(&path);
                    // A damaged file is linked again.
                    if let Ok(library) = open(&path) {
                        return Ok(library);
                    }
                }
                let bytes = link()?;
                create_dir(&dir)?;
                write_atomically(&dir, &path, &bytes)?;
                open(&path)
            }
        }
    }
}

/// The key of a C object (module documentation, D-942,
/// D-1199).
fn object_key(
    build: crate::cache::BuildId,
    source: &str,
    bytes: &[u8],
    identity: &CompilerIdentity,
) -> Key {
    let mut builder = Key::builder(KeyKind::CObj, build)
        .str(source)
        .bytes(bytes)
        .bytes(identity.path.as_os_str().as_encoded_bytes())
        .bytes(&identity.version);
    for flag in cc::KEY_FLAGS {
        builder = builder.str(flag);
    }
    builder.hash(Hash128::of(HEADER.as_bytes())).finish()
}

/// The key of a package's shared library: everything that goes into its
/// link (D-941, D-1197).
fn library_key(
    build: crate::cache::BuildId,
    package: &str,
    objects: &[(String, Vec<u8>)],
    libraries: &[String],
    identity: &CompilerIdentity,
) -> Key {
    let mut builder = Key::builder(KeyKind::NativeLib, build)
        .str(package)
        .bytes(identity.path.as_os_str().as_encoded_bytes())
        .bytes(&identity.version)
        .str(&shlib::flags().join(" "))
        .str(shlib::FORWARDER_SOURCE)
        .u64(u64::try_from(objects.len()).unwrap_or(u64::MAX));
    for (source, bytes) in objects {
        builder = builder.str(source).hash(Hash128::of(bytes));
    }
    builder = builder.u64(u64::try_from(libraries.len()).unwrap_or(u64::MAX));
    for library in libraries {
        builder = builder.str(library);
    }
    builder.finish()
}

/// The file a recorded dependency names now: a package file under `root`,
/// the header as embedded, or the absolute path.
fn dependency_hash(dependency: &Dependency, root: &Path) -> Option<Hash128> {
    match dependency {
        Dependency::Header => Some(Hash128::of(HEADER.as_bytes())),
        Dependency::Package(relative) => std::fs::read(root.join(relative))
            .ok()
            .map(|b| Hash128::of(&b)),
        Dependency::Other(path) => std::fs::read(path).ok().map(|b| Hash128::of(&b)),
    }
}

/// A C object's record: [`COBJ_MAGIC`], the object's blob name, then each
/// dependency as its form (0 package, 1 header, 2 other), its path's
/// length (`u32`, little-endian) and bytes, and its hash. Deterministic:
/// the dependencies are sorted, and no package or cache path is in it
/// (D-942, D-972). The object goes into the store as a blob.
fn encode_record(store: &Store, compiled: &cc::Compiled, root: &Path) -> Result<Vec<u8>, Error> {
    let store_error = |e: crate::cache::StoreError| Error::new(e.to_string());
    let blob = store.put_blob(&compiled.object).map_err(store_error)?;
    let mut out = Vec::new();
    out.extend_from_slice(COBJ_MAGIC);
    out.extend_from_slice(&blob.0);
    let count = u32::try_from(compiled.dependencies.len())
        .map_err(|_| Error::from(cheby_iface::Ice::new("too many dependencies")))?;
    out.extend_from_slice(&count.to_le_bytes());
    for dependency in &compiled.dependencies {
        let (form, path): (u8, &[u8]) = match dependency {
            Dependency::Package(relative) => (0, relative.as_bytes()),
            Dependency::Header => (1, HEADER_NAME.as_bytes()),
            Dependency::Other(path) => (2, path.as_os_str().as_encoded_bytes()),
        };
        // A file that cannot be read now never matches, so the next
        // lookup compiles again.
        let hash = dependency_hash(dependency, root).unwrap_or(Hash128([0; 16]));
        let len = u32::try_from(path.len())
            .map_err(|_| Error::from(cheby_iface::Ice::new("a dependency's path is too long")))?;
        out.push(form);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(path);
        out.extend_from_slice(&hash.0);
    }
    Ok(out)
}

/// The object a record of `key` names, if the record and the blob are
/// there and every file the compile read still has its recorded hash.
fn cached_object(store: &Store, key: &Key, root: &Path) -> Option<Vec<u8>> {
    let record = store.get_record(key).ok().flatten()?;
    let (blob, dependencies) = decode_record(&record)?;
    for (dependency, hash) in &dependencies {
        if dependency_hash(dependency, root) != Some(*hash) {
            return None;
        }
    }
    store.get_blob(blob).ok().flatten()
}

/// [`encode_record`]'s inverse; `None` for a malformed record.
fn decode_record(bytes: &[u8]) -> Option<(Hash128, Vec<(Dependency, Hash128)>)> {
    let mut reader = Reader(bytes.strip_prefix(COBJ_MAGIC.as_slice())?);
    let blob = reader.hash()?;
    let count = reader.u32()?;
    let mut dependencies = Vec::new();
    for _ in 0..count {
        let form = *reader.take(1)?.first()?;
        let len = usize::try_from(reader.u32()?).ok()?;
        let path = std::str::from_utf8(reader.take(len)?).ok()?;
        let dependency = match form {
            0 => Dependency::Package(path.to_owned()),
            1 => Dependency::Header,
            2 => Dependency::Other(PathBuf::from(path)),
            _ => return None,
        };
        dependencies.push((dependency, reader.hash()?));
    }
    reader.0.is_empty().then_some((blob, dependencies))
}

/// The unread rest of a record.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn hash(&mut self) -> Option<Hash128> {
        Some(Hash128(self.take(16)?.try_into().ok()?))
    }
}

/// Creates `dir` and its parents, owner-only on Unix.
fn create_dir(dir: &Path) -> Result<(), Error> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| Error::new(format!("cannot create `{}`: {e}", dir.display())))
}

/// Writes `bytes` to `path` through a fresh file in `dir` renamed into
/// place, so a reader never sees a partial file; concurrent writers write
/// the same bytes (D-264).
fn write_atomically(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), Error> {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{n}", std::process::id()));
    let io = |what: &str, p: &Path, e: std::io::Error| {
        Error::new(format!("cannot {what} `{}`: {e}", p.display()))
    };
    std::fs::write(&tmp, bytes).map_err(|e| io("write", &tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io("write", path, e)
    })
}

/// Marks `path` as used now (D-961's trim reads the modification time).
/// Failures are ignored.
fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// A private temporary directory for a command without a cache, removed
/// when dropped: a library opened from it stays mapped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self, Error> {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("cheby-native-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
