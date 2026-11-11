//! The analysis API for the language server (D-1364, D-1365).
//!
//! The driver runs every phase. What leaves it is either plain data
//! (strings, paths, byte ranges, hashes) or a handle of one run read
//! through that run's snapshot.
//!
//! - [`PlainDiagnostic`] and its parts: diagnostics with no generation
//!   handles, made by [`plain`] at the end of a run.
//! - [`Symbol`]: the only identity that crosses snapshots (D-1365).
//! - [`exact_key`], [`normalized_key`] and [`InputsKey`]: [`Hash128`] keys
//!   over text (D-1393, D-1412), and [`NormMap`] between live and
//!   normalized text.
//! - [`Session`] over an [`Overlay`]: each run gives a [`Live`], with an
//!   immutable [`Snapshot`] for queries and [`FileReport`]s to publish.
//!   It runs the two-track algorithm (D-1347): a [`Live`] that is not the
//!   check carries a [`StrictPlan`], which [`Session::analyze_strict`]
//!   runs as the D run. A session of [`PackageKind::SingleFile`] analyzes
//!   one file that is no module of any package, under a synthetic manifest
//!   (D-1371).
//! - [`StdBase`]: every shipped std module checked once, with its plain
//!   [`StdIndex`], and [`materialize_std`] for std's files on disk
//!   (D-1369, D-1372).
//! - [`verify`]: a stateless re-check of an edit plan against a snapshot
//!   (D-1389).
//!
//! The re-exports below are what a client needs to read a snapshot: data
//! types and pure helpers of the phase crates, so that `cheby_lsp` needs no
//! edge to them. No phase function is re-exported (`resolve_module`,
//! `check_bodies*`, `build_interface`): only the driver runs phases.

mod backend_query;
mod inputs;
mod plain;
mod session;
mod snapshot;
mod std_base;
mod symbol;
mod verify;

#[cfg(test)]
mod single_tests;
#[cfg(test)]
mod std_base_tests;
#[cfg(test)]
mod sweep_tests;
#[cfg(test)]
mod two_track_tests;
#[cfg(test)]
mod verify_tests;

pub use cheby_diag::{
    Code, Diagnostic, Edit, FileId, Help, Label, LabelStyle, Severity, SourceFile, SourceMap, Span,
    TextRange, TextSize, codes,
};
pub use cheby_iface::{
    CtorFields, CtorSig, DefId, DefIndex, DefKind, DepView, DisplayCx, FnSig, GenericParam,
    Generics, Globals, Ice, IfaceItem, ImportNames, InterfaceDef, InterfaceFnSig, InterfaceSet,
    ItemSig, ModuleId, ModuleInfo, ModuleInterface, ModuleTable, Name, Namespace, PackageId,
    ParamIdx, ParamSig, SigOpts, Ty, TyKind, TypeDef, TypeDefKind, Vis, builtin, def_path, display,
    interface_fns, item_signature, missing_imports, structure_visible,
};
pub use cheby_package::{ModulePath, Target, Targets};
pub use cheby_resolve::{
    BodyRes, ImportItemRequest, ImportRequest, ImportTarget, ItemDef, LocalDecl, LocalKind,
    ModuleScope, Res, ResolvedModule, ScopeEntry, ScopeSource, field_docs, item_docs,
};
pub use cheby_types::facts::{
    BodyFacts, CallFact, CaseFact, DynFact, ExprFact, FieldFact, FieldSite, MismatchFact,
    MissingPat, ModuleFacts, OpFact, ShowFact, UnsatisfiedFact,
};
pub use cheby_types::satisfaction::{
    OpFn, QueryInput, Reason, Unsatisfied, Verdict, Want, accepts, accepts_binding, implementors,
    interfaces_of, reason_text, satisfies,
};
pub use cheby_types::{ModuleRefs, TypesInput, infer, thir};

pub use crate::cache::Hash128;
pub use backend_query::{
    BackendError, BackendFile, BackendJob, BackendRequest, BackendResult, ConstDiagnostic,
    ConstValue, ConstantsOut, FrontOutcome, Section, backend_query, cut_text,
};
pub use inputs::{Overlay, OverlayFile, TextOrigin};
pub use plain::{
    InputsKey, NormMap, Placed, PlainDiagnostic, PlainEdit, PlainHelp, PlainLabel, exact_key,
    normalize, normalized_key, plain,
};
pub use session::{FileReport, Live, PackageKind, Report, Session, SessionConfig, StrictPlan};
pub use snapshot::{
    AnalysisStatus, BaselineEntry, FileLocation, ModuleView, Snapshot, SnapshotId, SymbolTarget,
};
pub use std_base::{
    StdBase, StdField, StdIndex, StdItem, StdModuleEntry, TypeHead, materialize_std,
};
pub use symbol::{Member, Symbol, SymbolNs};
pub use verify::{VerifyOutcome, VerifyRequest, verify};

/// What crosses threads in the server: a snapshot and its reports are
/// shared by the request pool, as are the std base and what `verify` takes
/// and gives on a query thread, and a session moves to the analysis
/// thread. These fail to compile if a field ever breaks that.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    const fn movable<T: Send>() {}
    shared::<Snapshot>();
    shared::<Live>();
    shared::<Report>();
    shared::<FileReport>();
    shared::<PlainDiagnostic>();
    shared::<Symbol>();
    shared::<Overlay>();
    shared::<Globals>();
    shared::<AnalysisStatus>();
    shared::<StrictPlan>();
    shared::<StdBase>();
    shared::<StdIndex>();
    shared::<StdItem>();
    shared::<VerifyRequest<'static>>();
    shared::<VerifyOutcome>();
    movable::<Session>();
};
