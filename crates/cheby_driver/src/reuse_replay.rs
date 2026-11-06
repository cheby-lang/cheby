//! The reuse report from records (K4d; D-965, D-1223,
//! D-1257).
//!
//! `--reuse-report`'s lines are made while lowering (D-717, D-852), and a
//! unit whose code record hits is not lowered (D-1221). So the lines of
//! each package unit a command lowers are cached beside its code or tests
//! record, and replayed when the record hits, so `cheby run --reuse-report`
//! and `cheby test --reuse-report` read records like any command and still
//! print what a cold run prints (D-268).
//!
//! - **Where.** A unit's lines lie under their own key, derived from the
//!   unit's code key (`KeyKind::Code` over the code key and the tag
//!   `reuse-lines`), never inside the record's bytes, so `RECORD_VERSION`
//!   stays 3 (D-1253). A stale code key finds no lines.
//! - **What.** Each line of the unit's whole MIR, with the item that owns
//!   the function it comes from (`None` for a function every build keeps),
//!   its line and column in the module's own file, and its text. The path
//!   is the module's file as the command's source map names it (D-421),
//!   rendered at replay; the report's text has no color.
//! - **Which lines print.** A command prints the lines of the functions it
//!   reaches (D-717's view, `backend.rs`'s `reached_units`): a replayed line
//!   prints when its owner is reached or a selected test, by name.
//! - **When they are written.** Only by a command that asks for the
//!   report (the lines cost a reuse analysis of every function), beside
//!   each fresh package unit with a key. A command that asks for the report
//!   counts a record without its lines as a miss: it lowers the unit, which
//!   writes them.
//! - Under `CHEBY_RC=naive` the report is its one line and nothing is read
//!   or written. `CHEBY_DUMP_DIR` reads nothing (D-965) and still writes.

use std::collections::{BTreeMap, BTreeSet};

use cheby_diag::SourceMap;
use cheby_iface::{DefId, Ice, ModuleId, PackageId};
use cheby_mir::{FnKey, MirModule, MirOptions, RcMode, UnitKind};

use crate::backend::{Lowered, Placed};
use crate::cache::front::{sealed, unsealed};
use crate::cache::{BuildId, Key, KeyKind, StoreShare};
use crate::frontend::Frontend;
use crate::reuse_report::{self, Line, Types};

/// The first bytes of a unit's cached reuse lines.
const MAGIC: &[u8; 4] = b"CBRL";
/// The version of their encoding.
const VERSION: u8 = 1;

/// One cached line: the item owning its function (`None` when every build
/// keeps the function), its line and column in the module's file, and its
/// text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReuseLine {
    /// The owning item's name, as the unit names it (`Parent::name` for an
    /// interface function, the name of a test).
    pub owner: Option<String>,
    /// The 1-based line.
    pub line: u32,
    /// The 1-based column.
    pub col: u32,
    /// The text after the location.
    pub text: String,
}

/// The reuse report's lines of one unit, as `--reuse-report` prints them,
/// structured for replay with D-421's paths and the command's color
/// (D-965).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReuseLines {
    /// The lines, sorted.
    pub lines: Vec<ReuseLine>,
}

impl ReuseLines {
    /// The sealed bytes stored beside the unit's record.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        put_u32(&mut out, len32(self.lines.len()));
        for line in &self.lines {
            match &line.owner {
                None => out.push(0),
                Some(owner) => {
                    out.push(1);
                    put_str(&mut out, owner);
                }
            }
            put_u32(&mut out, line.line);
            put_u32(&mut out, line.col);
            put_str(&mut out, &line.text);
        }
        sealed(out)
    }

    /// The lines of `bytes`, or `None` when they are malformed or of
    /// another version: a miss, never an error. Never panics.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Bytes(unsealed(bytes)?);
        if r.take(MAGIC.len())? != MAGIC || r.take(1)? != [VERSION] {
            return None;
        }
        let n = r.u32()?;
        let mut lines = Vec::new();
        for _ in 0..n {
            let owner = match r.take(1)? {
                [0] => None,
                [1] => Some(r.str()?),
                _ => return None,
            };
            lines.push(ReuseLine {
                owner,
                line: r.u32()?,
                col: r.u32()?,
                text: r.str()?,
            });
        }
        r.0.is_empty().then_some(Self { lines })
    }
}

/// The key of the lines cached beside the record under `code` (module
/// docs): derived from it, so a stale code key finds none.
pub(crate) fn lines_key(code: &Key, build: BuildId) -> Key {
    Key::builder(KeyKind::Code, build)
        .hash(code.hash())
        .str("reuse-lines")
        .finish()
}

/// The lines cached beside the record under `code`, if `front`'s command
/// reads the store and they decode.
pub(crate) fn find(front: &Frontend, code: &Key) -> Option<ReuseLines> {
    let (store, reads) = front.cache_store.as_ref()?;
    find_in(store, *reads, code)
}

/// The lines cached in `store` beside the record under `code`, when the
/// command `reads` and they decode.
pub(crate) fn find_in(store: &StoreShare, reads: bool, code: &Key) -> Option<ReuseLines> {
    if !reads {
        return None;
    }
    store
        .get_record(&lines_key(code, store.build_id()))
        .and_then(|bytes| ReuseLines::decode(&bytes))
}

/// Whether a command that prints the report may load the record under
/// `code`: only with its lines (module docs).
pub(crate) fn has_lines(front: &Frontend, code: &Key) -> bool {
    find(front, code).is_some()
}

/// Writes `lines` beside the record under `code`, behind the command
/// (D-1138); a write that fails is skipped (D-1129).
fn store(front: &Frontend, code: &Key, lines: &ReuseLines) {
    if let Some((store, _)) = front.cache_store.as_ref() {
        store_in(store, code, lines);
    }
}

/// Writes `lines` into `store` beside the record under `code` (D-1138,
/// D-1129).
pub(crate) fn store_in(store: &StoreShare, code: &Key, lines: &ReuseLines) {
    let _ = store.put_record(&lines_key(code, store.build_id()), &lines.encode());
}

/// The text of `--reuse-report` for `lowered` (D-717, D-852): the lines
/// of the reached functions of the package's own units, each fresh unit's
/// from its MIR (its whole unit's lines are cached beside its record),
/// each cached unit's replayed from its record's lines.
///
/// # Errors
///
/// An [`Ice`] for malformed MIR, or when a cached unit's lines are gone.
pub(crate) fn report(
    front: &Frontend,
    sources: &SourceMap,
    lowered: &Lowered,
    mir: MirOptions,
) -> Result<String, Ice> {
    if mir.rc == RcMode::Naive {
        return Ok(reuse_report::NAIVE_LINE.to_owned());
    }
    let types = Types {
        table: &front.globals.types,
        interfaces: &front.set,
    };
    let names = &front.globals.names;
    let kept = Kept::new(front, lowered)?;
    let mut out: Vec<Line> = Vec::new();
    for placed in &lowered.order {
        match placed {
            Placed::Fresh(i) => {
                let unit = lowered
                    .units
                    .get(*i)
                    .ok_or_else(|| Ice::new("a lowered unit went missing"))?;
                let Some(module) = unit.module().filter(|&m| is_local(front, m)) else {
                    continue;
                };
                let path = file_path(front, sources, module)?;
                let mut lines = Vec::new();
                let mut own_file = true;
                for f in &unit.functions {
                    if f.sites.is_empty() {
                        continue;
                    }
                    let owner = owner_of(unit, f.key)?;
                    let mut found = Vec::new();
                    reuse_report::function_lines(unit, f, sources, names, &types, &mut found)?;
                    for ((file, line, col), text) in found {
                        own_file &= file == path;
                        lines.push(ReuseLine {
                            owner: owner.clone(),
                            line,
                            col,
                            text,
                        });
                    }
                }
                lines.sort();
                let lines = ReuseLines { lines };
                if own_file && let Some(key) = unit_key(lowered, unit.unit) {
                    store(front, key, &lines);
                }
                out.extend(kept.lines(module, &path, &lines));
            }
            Placed::Cached { unit, .. } => {
                let Some(module) = unit_module(*unit).filter(|&m| is_local(front, m)) else {
                    continue;
                };
                let key = unit_key(lowered, *unit)
                    .ok_or_else(|| Ice::new("a cached unit without a key"))?;
                let lines = find(front, key).ok_or_else(|| {
                    Ice::new("the reuse lines of a record read for `--reuse-report` are gone")
                })?;
                let path = file_path(front, sources, module)?;
                out.extend(kept.lines(module, &path, &lines));
            }
        }
    }
    Ok(reuse_report::render(out))
}

/// The names of the items a build reaches and of its selected tests, by
/// module: the owners whose lines print ([`reached_units`]' rule).
///
/// [`reached_units`]: crate::backend
struct Kept {
    by_module: BTreeMap<ModuleId, BTreeSet<String>>,
}

impl Kept {
    /// The kept owners of `lowered`, named through the build's interfaces.
    fn new(front: &Frontend, lowered: &Lowered) -> Result<Self, Ice> {
        let defs: BTreeSet<DefId> = lowered
            .reach
            .items()
            .chain(lowered.selected.iter().copied())
            .filter(|def| is_local(front, def.module))
            .collect();
        let mut by_module: BTreeMap<ModuleId, BTreeSet<String>> = BTreeMap::new();
        crate::borrowing::with_build_defs(front, |d| -> Result<(), Ice> {
            for &def in &defs {
                let name = d.name(def)?;
                by_module
                    .entry(def.module)
                    .or_default()
                    .insert(name.item.to_string());
            }
            Ok(())
        })?;
        Ok(Self { by_module })
    }

    /// The report lines of `lines`, of module `module` whose file is
    /// `path`, whose owner is kept.
    fn lines(&self, module: ModuleId, path: &str, lines: &ReuseLines) -> Vec<Line> {
        let kept = self.by_module.get(&module);
        lines
            .lines
            .iter()
            .filter(|l| {
                l.owner
                    .as_ref()
                    .is_none_or(|owner| kept.is_some_and(|k| k.contains(owner)))
            })
            .map(|l| ((path.to_owned(), l.line, l.col), l.text.clone()))
            .collect()
    }
}

/// The item that owns the function `key` (`backend.rs`'s `reached_units`
/// keeps a function by it), by name; `None` for one every build keeps.
fn owner_of(unit: &MirModule, key: FnKey) -> Result<Option<String>, Ice> {
    match key {
        FnKey::Item(def)
        | FnKey::ValueShim(def)
        | FnKey::ConstInit(def)
        | FnKey::Test(def)
        | FnKey::Closure { owner: def, .. }
        | FnKey::Continuation { owner: def, .. } => Ok(Some(unit.name(def)?.item.to_string())),
        _ => Ok(None),
    }
}

/// Whether `module` is one of the package's own (D-717: only they record
/// sites).
fn is_local(front: &Frontend, module: ModuleId) -> bool {
    front
        .modules
        .get(module)
        .is_some_and(|info| info.package == PackageId::LOCAL)
}

/// The module of a code or tests unit.
const fn unit_module(unit: UnitKind) -> Option<ModuleId> {
    match unit {
        UnitKind::Code(m) | UnitKind::Tests(m) => Some(m),
        UnitKind::Entries => None,
    }
}

/// The code key of a package unit, if it has one.
fn unit_key(lowered: &Lowered, unit: UnitKind) -> Option<&Key> {
    use crate::cache::code_keys::UnitPart;
    let (part, module) = match unit {
        UnitKind::Code(m) => (UnitPart::Code, m),
        UnitKind::Tests(m) => (UnitPart::Tests, m),
        UnitKind::Entries => return None,
    };
    lowered.keys.keys.get(&(part, module))
}

/// The path of `module`'s file as the command's source map names it
/// (D-421).
fn file_path(front: &Frontend, sources: &SourceMap, module: ModuleId) -> Result<String, Ice> {
    let info = front.modules.try_get(module)?;
    info.file
        .and_then(|file| sources.file(file))
        .map(|file| file.path().to_owned())
        .ok_or_else(|| Ice::new(format!("module `{}` has no file", info.path)))
}

/// `n` as a `u32`, saturating (a unit never holds 2^32 lines).
fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Appends `v`, little-endian.
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Appends `s`, length-prefixed.
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, len32(s.len()));
    out.extend_from_slice(s.as_bytes());
}

/// A reader over encoded lines.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    /// The next `n` bytes.
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    /// The next little-endian `u32`.
    fn u32(&mut self) -> Option<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    /// The next length-prefixed string.
    fn str(&mut self) -> Option<String> {
        let n = usize::try_from(self.u32()?).ok()?;
        let bytes = self.take(n)?;
        std::str::from_utf8(bytes).ok().map(str::to_owned)
    }
}
