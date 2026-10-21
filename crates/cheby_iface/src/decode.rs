//! Decoding the canonical interface encoding (D-953).
//!
//! [`decode_interface`] turns section H of an interface artifact, D-460's
//! encoding byte for byte, back into a [`ModuleInterface`]. CHBI version 1
//! decodes as it is: every list and string carries its length and every
//! optional field and enum a tag (`.m3-work/wave1/K2/decode.md` lists the
//! fields). The decoder:
//!
//! - re-interns names and types into the process's [`Globals`];
//! - resolves module paths through the process's [`ModuleTable`], and
//!   references into other modules by name through a [`DepView`], so the
//!   interfaces the encoding refers to must be published (dependencies
//!   are decoded first);
//! - numbers items in encoded order: each top-level item, then its
//!   constructors or interface functions ([`crate::encoded_order`]);
//! - gives constructors and interface functions their parent's
//!   visibility, targets and deprecation, as `cheby_resolve` does
//!   (GAP(iface-decode-derived-fields): v1 does not encode them, and the
//!   debug round trip of D-954 fails if they ever differ);
//! - leaves every span `None`: spans live in the artifact's section S
//!   ([`crate::artifact`], [`apply_spans`]).
//!
//! It never panics and never recurses once per level of the input: every
//! length, tag, index and string is checked, and nothing is preallocated
//! from a length read from the input (D-422). Types are read with an
//! explicit stack into a flat arena, children before their parent, and
//! resolved in arena order, so a type nests as deep as the encoder wrote
//! it, with no limit (GAP(iface-decode-type-depth): a valid program's
//! types have no depth bound, since an alias chain adds one level per
//! line). Signatures nest one level only: a type's children must be
//! constructor signatures and an interface's children interface-function
//! signatures (or error signatures), as the encoder writes them.

use std::fmt;

use cheby_diag::{FileId, Span, TextRange, TextSize};
use cheby_package::{ModulePath, Target, Targets};
use rustc_hash::FxHashMap;

use crate::{
    AliasDef, BUILTIN_MODULE_NAME, BuiltinType, ConstSig, CtorFields, CtorSig, DefId, DefIndex,
    DefKind, DepView, FnSig, GenericParam, Generics, Globals, IfaceItem, IndexVec, InterfaceDef,
    InterfaceFnSig, ItemSig, ModuleId, ModuleInterface, ModuleTable, Name, Namespace, PackageId,
    ParamIdx, ParamSig, Ty, TyKind, TypeDef, TypeDefKind, TypeHooks, Vis, hash::MAGIC,
};

/// Why an encoding or an artifact could not be decoded. Every case is a
/// miss for the cache: the module is checked from source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes are not a well-formed encoding or artifact.
    Malformed(&'static str),
    /// The format tag is right but its version is not this compiler's.
    Version(u8),
    /// A module path the process's module table does not know, or a
    /// module whose interface the view cannot read.
    UnknownModule(String),
    /// A definition the referenced module's interface does not hold.
    UnknownItem(String),
    /// An artifact's section H does not hash to its recorded hash.
    HashMismatch,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed interface: {what}"),
            Self::Version(v) => write!(f, "interface format version {v} is not supported"),
            Self::UnknownModule(path) => write!(f, "interface refers to unknown module `{path}`"),
            Self::UnknownItem(path) => write!(f, "interface refers to unknown item `{path}`"),
            Self::HashMismatch => f.write_str("interface does not match its hash"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// D-953: decodes section H (CHBI) of `module`, of package `package`,
/// into an interface, re-interning names and types into `globals`,
/// resolving module paths through `modules` and references into other
/// modules through `deps`, and numbering items in encoded order. The
/// interface's `hash` is the hash of `h`.
///
/// `deps` must read every module the encoding refers to; a dependency's
/// interface is decoded and published before its dependents'.
///
/// # Errors
///
/// [`DecodeError::Malformed`] for bytes that are not an encoding of
/// `module`, [`DecodeError::Version`] for another CHBI version,
/// [`DecodeError::UnknownModule`] for a path `modules` does not know or a
/// module `deps` cannot read, and [`DecodeError::UnknownItem`] for a name
/// a referenced interface does not hold.
pub fn decode_interface(
    module: ModuleId,
    package: PackageId,
    h: &[u8],
    deps: &DepView<'_>,
    modules: &ModuleTable,
    globals: &Globals,
) -> Result<ModuleInterface, DecodeError> {
    let raw = parse(h)?;
    let own_path = modules
        .get(module)
        .ok_or(DecodeError::Malformed(
            "the module is not in the module table",
        ))?
        .path
        .segments();
    if raw.path.len() != own_path.len() || raw.path.iter().zip(own_path).any(|(a, b)| a != b) {
        return Err(DecodeError::Malformed("the encoding is another module's"));
    }
    let numbering = Numbering::new(&raw.items)?;
    let mut cx = Resolver {
        module,
        deps,
        modules,
        globals,
        numbering: &numbering,
        tys: Vec::new(),
    };
    cx.resolve_types(&raw.tys)?;
    let mut items: Vec<IfaceItem> = Vec::new();
    for item in &raw.items {
        let index = next_index(&items)?;
        items.push(IfaceItem {
            name: globals.names.intern(item.name),
            kind: item.kind,
            vis: item.vis,
            parent: None,
            span: None,
            targets: item.targets,
            deprecated: item.deprecated.map(Into::into),
            sig: cx.sig(&item.sig, index, None)?,
        });
        let (children, kind): (&[(&str, RawSig<'_>)], DefKind) = match &item.sig {
            RawSig::Type {
                kind: RawTypeKind::Adt(ctors),
                ..
            } => (ctors, DefKind::Ctor),
            RawSig::Interface { fns, .. } => (fns, DefKind::InterfaceFn),
            _ => (&[], DefKind::Fn),
        };
        for (name, sig) in children {
            let child = next_index(&items)?;
            items.push(IfaceItem {
                name: globals.names.intern(name),
                kind,
                vis: item.vis,
                parent: Some(index),
                span: None,
                targets: item.targets,
                deprecated: item.deprecated.map(Into::into),
                sig: cx.sig(sig, child, Some(index))?,
            });
        }
    }
    let items = IndexVec::from_raw(items).map_err(|_| DecodeError::Malformed("too many items"))?;
    let mut iface = ModuleInterface::new(module, package, items)
        .map_err(|_| DecodeError::Malformed("an item's parent comes after it"))?;
    iface.hash = Some(crate::hash_encoding(h));
    Ok(iface)
}

/// Gives the items of a decoded interface their spans from an artifact's
/// section S (D-953): `spans` lists, in the interface's item order (which
/// is encoded order), each item's start and length in `file`, the
/// module's own source.
///
/// # Errors
///
/// [`DecodeError::Malformed`] if `spans` does not have one entry per item.
pub fn apply_spans(
    iface: &mut ModuleInterface,
    file: FileId,
    spans: &[Option<(u32, u32)>],
) -> Result<(), DecodeError> {
    if spans.len() != iface.items.len() {
        return Err(DecodeError::Malformed(
            "the span section does not match the items",
        ));
    }
    for (index, span) in spans.iter().enumerate() {
        let index = u32::try_from(index)
            .map(DefIndex::from_u32)
            .map_err(|_| DecodeError::Malformed("too many items"))?;
        let span = span.map(|(start, len)| {
            let start = TextSize::from(start);
            Span::new(file, TextRange::at(start, TextSize::from(len)))
        });
        iface
            .set_span(index, span)
            .map_err(|_| DecodeError::Malformed("the span section does not match the items"))?;
    }
    Ok(())
}

fn next_index(items: &[IfaceItem]) -> Result<DefIndex, DecodeError> {
    u32::try_from(items.len())
        .map(DefIndex::from_u32)
        .map_err(|_| DecodeError::Malformed("too many items"))
}

// ---------------------------------------------------------------------
// Parsing: bytes to a tree whose references are still names.
// ---------------------------------------------------------------------

struct RawInterface<'a> {
    path: Vec<&'a str>,
    items: Vec<RawItem<'a>>,
    /// Every type of the encoding, each after its children.
    tys: Vec<RawTyNode<'a>>,
}

struct RawItem<'a> {
    kind: DefKind,
    vis: Vis,
    name: &'a str,
    targets: Targets,
    deprecated: Option<&'a str>,
    sig: RawSig<'a>,
}

/// A reference to a definition: module path, namespace or parent, name.
struct RawRef<'a> {
    path: Vec<&'a str>,
    target: RawTarget<'a>,
    name: &'a str,
}

enum RawTarget<'a> {
    Type,
    Value,
    Ctor(&'a str),
    InterfaceFn(&'a str),
}

/// A type: the index of its node in the reader's arena.
#[derive(Clone, Copy)]
struct RawTy(usize);

/// A type of the arena, whose children come before it.
enum RawTyNode<'a> {
    Error,
    Never,
    Named(RawRef<'a>, Vec<RawTy>),
    Tuple(Vec<RawTy>),
    Fn(Vec<RawTy>, RawTy),
    Param(u32),
    Dyn(RawRef<'a>, Vec<RawTy>),
}

/// A type whose children are still being read.
struct OpenTy<'a> {
    head: OpenHead<'a>,
    /// How many children are still to be read.
    left: u64,
    children: Vec<RawTy>,
}

enum OpenHead<'a> {
    Named(RawRef<'a>),
    Tuple,
    /// A function type: its parameters, then its return type.
    Fn,
    Dyn(RawRef<'a>),
}

impl<'a> OpenTy<'a> {
    fn close(self) -> Result<RawTyNode<'a>, DecodeError> {
        let children = self.children;
        Ok(match self.head {
            OpenHead::Named(def) => RawTyNode::Named(def, children),
            OpenHead::Tuple => RawTyNode::Tuple(children),
            OpenHead::Fn => {
                let mut params = children;
                let ret = params.pop().ok_or(DecodeError::Malformed(
                    "a function type without a return type",
                ))?;
                RawTyNode::Fn(params, ret)
            }
            OpenHead::Dyn(def) => RawTyNode::Dyn(def, children),
        })
    }
}

type RawGenerics<'a> = Vec<(&'a str, Vec<RawRef<'a>>)>;

enum RawSig<'a> {
    Fn {
        generics: RawGenerics<'a>,
        params: Vec<(&'a str, RawTy)>,
        ret: RawTy,
        clamped: Vec<u32>,
    },
    Type {
        generics: RawGenerics<'a>,
        exposed: bool,
        home: Option<Vec<&'a str>>,
        hooks: Option<[&'a str; 3]>,
        kind: RawTypeKind<'a>,
    },
    Ctor {
        tag: u32,
        fields: RawFields<'a>,
    },
    Alias {
        generics: RawGenerics<'a>,
        target: RawTy,
    },
    Const(RawTy),
    Interface {
        embeds: Vec<RawRef<'a>>,
        fns: Vec<(&'a str, RawSig<'a>)>,
    },
    InterfaceFn {
        params: Vec<RawTy>,
        ret: RawTy,
    },
    Test,
    Error,
}

enum RawTypeKind<'a> {
    Builtin(BuiltinType),
    Adt(Vec<(&'a str, RawSig<'a>)>),
    External,
}

enum RawFields<'a> {
    Unit,
    Positional(Vec<RawTy>),
    Named(Vec<(&'a str, RawTy)>),
}

fn parse(h: &[u8]) -> Result<RawInterface<'_>, DecodeError> {
    let mut r = Reader {
        bytes: h,
        at: 0,
        tys: Vec::new(),
    };
    let magic = r.take(MAGIC.len())?;
    if magic != MAGIC {
        return match (magic.split_last(), MAGIC.split_last()) {
            (Some((&version, tag)), Some((_, want))) if tag == want => {
                Err(DecodeError::Version(version))
            }
            _ => Err(DecodeError::Malformed("not an interface encoding")),
        };
    }
    let path = r.path()?;
    let count = r.u32()?;
    let mut items = Vec::new();
    for _ in 0..count {
        items.push(r.item()?);
    }
    if r.at != h.len() {
        return Err(DecodeError::Malformed("trailing bytes"));
    }
    Ok(RawInterface {
        path,
        items,
        tys: r.tys,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    /// The types read so far, each after its children.
    tys: Vec<RawTyNode<'a>>,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(DecodeError::Malformed("length overflow"))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(DecodeError::Malformed("truncated"))?;
        self.at = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        let b = self
            .bytes
            .get(self.at)
            .copied()
            .ok_or(DecodeError::Malformed("truncated"))?;
        self.at = self.at.saturating_add(1);
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| DecodeError::Malformed("truncated"))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn len(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u32()?).map_err(|_| DecodeError::Malformed("length overflow"))
    }

    fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::Malformed("a flag that is neither 0 nor 1")),
        }
    }

    fn str(&mut self) -> Result<&'a str, DecodeError> {
        let n = self.len()?;
        std::str::from_utf8(self.take(n)?)
            .map_err(|_| DecodeError::Malformed("a name is not UTF-8"))
    }

    fn path(&mut self) -> Result<Vec<&'a str>, DecodeError> {
        let count = self.u32()?;
        if count == 0 {
            return Err(DecodeError::Malformed("a module path without segments"));
        }
        let mut segments = Vec::new();
        for _ in 0..count {
            segments.push(self.str()?);
        }
        Ok(segments)
    }

    fn item(&mut self) -> Result<RawItem<'a>, DecodeError> {
        let kind = match self.byte()? {
            0 => DefKind::Fn,
            1 => DefKind::Type,
            2 => DefKind::Alias,
            3 => DefKind::Const,
            4 => DefKind::Interface,
            _ => {
                return Err(DecodeError::Malformed(
                    "an item kind not allowed at the top level",
                ));
            }
        };
        let vis = match self.byte()? {
            1 => Vis::Package,
            2 => Vis::Pub,
            _ => {
                return Err(DecodeError::Malformed(
                    "an item visibility not allowed in an interface",
                ));
            }
        };
        let name = self.str()?;
        let targets = match self.byte()? {
            0 => Targets::NONE,
            1 => Targets::only(Target::Native),
            2 => Targets::only(Target::Js),
            3 => Targets::ALL,
            _ => return Err(DecodeError::Malformed("an unknown targets tag")),
        };
        let deprecated = if self.bool()? {
            Some(self.str()?)
        } else {
            None
        };
        let sig = self.sig()?;
        Ok(RawItem {
            kind,
            vis,
            name,
            targets,
            deprecated,
            sig,
        })
    }

    fn reference(&mut self) -> Result<RawRef<'a>, DecodeError> {
        let path = self.path()?;
        let target = match self.byte()? {
            0 => RawTarget::Type,
            1 => RawTarget::Value,
            2 => RawTarget::Ctor(self.str()?),
            3 => RawTarget::InterfaceFn(self.str()?),
            _ => return Err(DecodeError::Malformed("an unknown reference tag")),
        };
        let name = self.str()?;
        Ok(RawRef { path, target, name })
    }

    /// Reads one type, however deep, with an explicit stack of the types
    /// whose children are still being read: no recursion per level.
    fn ty(&mut self) -> Result<RawTy, DecodeError> {
        let mut open: Vec<OpenTy<'a>> = Vec::new();
        loop {
            let mut done = match self.byte()? {
                0 => Some(RawTyNode::Error),
                1 => Some(RawTyNode::Never),
                5 => Some(RawTyNode::Param(self.u32()?)),
                tag @ (2 | 3 | 4 | 6) => {
                    let head = match tag {
                        2 => OpenHead::Named(self.reference()?),
                        3 => OpenHead::Tuple,
                        4 => OpenHead::Fn,
                        _ => OpenHead::Dyn(self.reference()?),
                    };
                    // A function type's return type follows its parameters.
                    let left = u64::from(self.u32()?).saturating_add(u64::from(tag == 4));
                    open.push(OpenTy {
                        head,
                        left,
                        children: Vec::new(),
                    });
                    None
                }
                _ => return Err(DecodeError::Malformed("an unknown type tag")),
            };
            // Close every type whose children have all been read, and hand
            // each to its parent.
            loop {
                let node = match done.take() {
                    Some(node) => node,
                    None => match open.pop() {
                        Some(top) if top.left == 0 => top.close()?,
                        Some(top) => {
                            open.push(top);
                            break;
                        }
                        None => break,
                    },
                };
                let id = RawTy(self.tys.len());
                self.tys.push(node);
                match open.last_mut() {
                    None => return Ok(id),
                    Some(parent) => {
                        parent.children.push(id);
                        parent.left = parent.left.saturating_sub(1);
                    }
                }
            }
        }
    }

    fn tys(&mut self) -> Result<Vec<RawTy>, DecodeError> {
        let count = self.u32()?;
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(self.ty()?);
        }
        Ok(out)
    }

    fn generics(&mut self) -> Result<RawGenerics<'a>, DecodeError> {
        let count = self.u32()?;
        let mut out = Vec::new();
        for _ in 0..count {
            let name = self.str()?;
            let bounds = self.u32()?;
            let mut refs = Vec::new();
            for _ in 0..bounds {
                refs.push(self.reference()?);
            }
            out.push((name, refs));
        }
        Ok(out)
    }

    /// A type's constructors (`child` is the constructor tag, 2) or an
    /// interface's functions (the interface-function tag, 6): each a name
    /// and a signature of that kind or an error signature, as the encoder
    /// writes them. Neither kind holds signatures, so signatures nest one
    /// level only and a hostile input cannot recurse (D-422).
    fn named_sigs(&mut self, child: u8) -> Result<Vec<(&'a str, RawSig<'a>)>, DecodeError> {
        let count = self.u32()?;
        let mut out = Vec::new();
        for _ in 0..count {
            let name = self.str()?;
            let tag = self.byte()?;
            if tag != child && tag != SIG_ERROR {
                return Err(DecodeError::Malformed(
                    "a constructor or interface function of another kind",
                ));
            }
            out.push((name, self.sig_of(tag)?));
        }
        Ok(out)
    }

    fn sig(&mut self) -> Result<RawSig<'a>, DecodeError> {
        let tag = self.byte()?;
        self.sig_of(tag)
    }

    /// The signature whose tag `tag` was just read.
    fn sig_of(&mut self, tag: u8) -> Result<RawSig<'a>, DecodeError> {
        Ok(match tag {
            0 => {
                let generics = self.generics()?;
                let count = self.u32()?;
                let mut params = Vec::new();
                for _ in 0..count {
                    let name = self.str()?;
                    params.push((name, self.ty()?));
                }
                let ret = self.ty()?;
                let count = self.u32()?;
                let mut clamped = Vec::new();
                for _ in 0..count {
                    clamped.push(self.u32()?);
                }
                RawSig::Fn {
                    generics,
                    params,
                    ret,
                    clamped,
                }
            }
            1 => {
                let generics = self.generics()?;
                let exposed = self.bool()?;
                let home = if self.bool()? {
                    Some(self.path()?)
                } else {
                    None
                };
                let hooks = if self.bool()? {
                    Some([self.str()?, self.str()?, self.str()?])
                } else {
                    None
                };
                let kind = match self.byte()? {
                    0 => RawTypeKind::Builtin(builtin_type(self.byte()?)?),
                    1 => {
                        let ctors = self.named_sigs(SIG_CTOR)?;
                        RawTypeKind::Adt(ctors)
                    }
                    2 => RawTypeKind::External,
                    _ => return Err(DecodeError::Malformed("an unknown type-definition tag")),
                };
                RawSig::Type {
                    generics,
                    exposed,
                    home,
                    hooks,
                    kind,
                }
            }
            SIG_CTOR => {
                let tag = self.u32()?;
                let fields = match self.byte()? {
                    0 => RawFields::Unit,
                    1 => RawFields::Positional(self.tys()?),
                    2 => {
                        let count = self.u32()?;
                        let mut fields = Vec::new();
                        for _ in 0..count {
                            let name = self.str()?;
                            fields.push((name, self.ty()?));
                        }
                        RawFields::Named(fields)
                    }
                    _ => return Err(DecodeError::Malformed("an unknown constructor-fields tag")),
                };
                RawSig::Ctor { tag, fields }
            }
            3 => {
                let generics = self.generics()?;
                RawSig::Alias {
                    generics,
                    target: self.ty()?,
                }
            }
            4 => RawSig::Const(self.ty()?),
            5 => {
                let count = self.u32()?;
                let mut embeds = Vec::new();
                for _ in 0..count {
                    embeds.push(self.reference()?);
                }
                let fns = self.named_sigs(SIG_INTERFACE_FN)?;
                RawSig::Interface { embeds, fns }
            }
            SIG_INTERFACE_FN => {
                let params = self.tys()?;
                RawSig::InterfaceFn {
                    params,
                    ret: self.ty()?,
                }
            }
            7 => RawSig::Test,
            SIG_ERROR => RawSig::Error,
            _ => return Err(DecodeError::Malformed("an unknown signature tag")),
        })
    }
}

/// The signature tags of a constructor, an interface function and an
/// error (`hash.rs` writes them).
const SIG_CTOR: u8 = 2;
const SIG_INTERFACE_FN: u8 = 6;
const SIG_ERROR: u8 = 9;

const fn builtin_type(tag: u8) -> Result<BuiltinType, DecodeError> {
    Ok(match tag {
        0 => BuiltinType::Int,
        1 => BuiltinType::Float,
        2 => BuiltinType::String,
        3 => BuiltinType::List,
        4 => BuiltinType::Never,
        5 => BuiltinType::I8,
        6 => BuiltinType::I16,
        7 => BuiltinType::I32,
        8 => BuiltinType::I64,
        9 => BuiltinType::U8,
        10 => BuiltinType::U16,
        11 => BuiltinType::U32,
        12 => BuiltinType::U64,
        13 => BuiltinType::F32,
        14 => BuiltinType::Map,
        15 => BuiltinType::Set,
        16 => BuiltinType::Bytes,
        _ => return Err(DecodeError::Malformed("an unknown builtin type")),
    })
}

// ---------------------------------------------------------------------
// Numbering and resolution.
// ---------------------------------------------------------------------

/// The decoded module's own names: each top-level item's index by
/// namespace and name (the first one wins, as in `ModuleInterface::new`),
/// and each parent's children by name.
struct Numbering<'a> {
    types: FxHashMap<&'a str, DefIndex>,
    values: FxHashMap<&'a str, DefIndex>,
    children: FxHashMap<DefIndex, Vec<(&'a str, DefIndex)>>,
}

impl<'a> Numbering<'a> {
    fn new(items: &[RawItem<'a>]) -> Result<Self, DecodeError> {
        let mut out = Self {
            types: FxHashMap::default(),
            values: FxHashMap::default(),
            children: FxHashMap::default(),
        };
        let mut next: u32 = 0;
        let bump = |next: &mut u32| -> Result<DefIndex, DecodeError> {
            let index = DefIndex::from_u32(*next);
            *next = next
                .checked_add(1)
                .ok_or(DecodeError::Malformed("too many items"))?;
            Ok(index)
        };
        for item in items {
            let index = bump(&mut next)?;
            let map = match item.kind.namespace() {
                Some(Namespace::Type) => &mut out.types,
                Some(Namespace::Value) => &mut out.values,
                None => return Err(DecodeError::Malformed("a top-level item in no namespace")),
            };
            map.entry(item.name).or_insert(index);
            let children: &[(&str, RawSig<'_>)] = match &item.sig {
                RawSig::Type {
                    kind: RawTypeKind::Adt(ctors),
                    ..
                } => ctors,
                RawSig::Interface { fns, .. } => fns,
                _ => &[],
            };
            let mut named = Vec::new();
            for &(name, _) in children {
                named.push((name, bump(&mut next)?));
            }
            if !named.is_empty() {
                out.children.insert(index, named);
            }
        }
        Ok(out)
    }

    /// The child of `parent` named `name`; the first one if two share it
    /// (E0041, as name resolution finds).
    fn child(&self, parent: DefIndex, name: &str) -> Option<DefIndex> {
        self.children
            .get(&parent)?
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, index)| index)
    }

    /// The children of `parent`, in encoded order.
    fn children_of(&self, parent: DefIndex) -> Box<[DefIndex]> {
        self.children
            .get(&parent)
            .map(|kids| kids.iter().map(|&(_, index)| index).collect())
            .unwrap_or_default()
    }
}

struct Resolver<'r, 'a> {
    module: ModuleId,
    deps: &'r DepView<'r>,
    modules: &'r ModuleTable,
    globals: &'r Globals,
    numbering: &'r Numbering<'a>,
    /// The interned type of each node of the arena, once resolved.
    tys: Vec<Ty>,
}

impl Resolver<'_, '_> {
    /// Interns every type of the arena in arena order: a node's children
    /// come before it, so each is interned from types already interned,
    /// with no recursion.
    fn resolve_types(&mut self, nodes: &[RawTyNode<'_>]) -> Result<(), DecodeError> {
        let globals = self.globals;
        let types = &globals.types;
        for node in nodes {
            let ty = match node {
                RawTyNode::Error => Ty::ERROR,
                RawTyNode::Never => Ty::NEVER,
                RawTyNode::Named(def, args) => types.intern(TyKind::Named {
                    def: self.def(def)?,
                    args: self.tys(args)?,
                }),
                RawTyNode::Tuple(elems) => types.intern(TyKind::Tuple(self.tys(elems)?)),
                RawTyNode::Fn(params, ret) => types.intern(TyKind::Fn {
                    params: self.tys(params)?,
                    ret: self.ty(*ret)?,
                }),
                RawTyNode::Param(n) => types.intern(TyKind::Param(ParamIdx::from_u32(*n))),
                RawTyNode::Dyn(iface, args) => types.intern(TyKind::Dyn {
                    iface: self.def(iface)?,
                    args: self.tys(args)?,
                }),
            };
            self.tys.push(ty);
        }
        Ok(())
    }

    fn module_of(&self, path: &[&str]) -> Result<ModuleId, DecodeError> {
        if let [only] = path
            && *only == BUILTIN_MODULE_NAME
        {
            return Ok(ModuleId::BUILTIN);
        }
        let path = module_path(path)?;
        self.modules
            .find(&path)
            .ok_or_else(|| DecodeError::UnknownModule(path.to_string()))
    }

    fn def(&self, r: &RawRef<'_>) -> Result<DefId, DecodeError> {
        let module = self.module_of(&r.path)?;
        let unknown = || {
            let parent = match r.target {
                RawTarget::Ctor(p) | RawTarget::InterfaceFn(p) => format!("{p}::"),
                RawTarget::Type | RawTarget::Value => String::new(),
            };
            DecodeError::UnknownItem(format!("{}::{parent}{}", r.path.join("::"), r.name))
        };
        if module == self.module {
            let n = self.numbering;
            let index = match r.target {
                RawTarget::Type => n.types.get(r.name).copied(),
                RawTarget::Value => n.values.get(r.name).copied(),
                RawTarget::Ctor(parent) | RawTarget::InterfaceFn(parent) => {
                    n.types.get(parent).and_then(|&p| n.child(p, r.name))
                }
            };
            return index.map(|i| DefId::new(module, i)).ok_or_else(unknown);
        }
        let iface = self
            .deps
            .get(module)
            .ok_or_else(|| DecodeError::UnknownModule(r.path.join("::")))?;
        // A name nobody interned is in no interface.
        let names = &self.globals.names;
        let name = names.lookup(r.name).ok_or_else(unknown)?;
        let index = match r.target {
            RawTarget::Type => iface.lookup(Namespace::Type, name),
            RawTarget::Value => iface.lookup(Namespace::Value, name),
            RawTarget::Ctor(parent) | RawTarget::InterfaceFn(parent) => {
                let want = if matches!(r.target, RawTarget::Ctor(_)) {
                    DefKind::Ctor
                } else {
                    DefKind::InterfaceFn
                };
                names
                    .lookup(parent)
                    .and_then(|p| iface.lookup(Namespace::Type, p))
                    .and_then(|p| {
                        iface.items.iter_enumerated().find_map(|(i, item)| {
                            (item.parent == Some(p) && item.kind == want && item.name == name)
                                .then_some(i)
                        })
                    })
            }
        };
        index.map(|i| DefId::new(module, i)).ok_or_else(unknown)
    }

    /// The interned type of an arena node already resolved.
    fn ty(&self, ty: RawTy) -> Result<Ty, DecodeError> {
        self.tys
            .get(ty.0)
            .copied()
            .ok_or(DecodeError::Malformed("a type refers to a later type"))
    }

    fn tys(&self, tys: &[RawTy]) -> Result<Box<[Ty]>, DecodeError> {
        tys.iter().map(|&t| self.ty(t)).collect()
    }

    fn name(&self, text: &str) -> Name {
        self.globals.names.intern(text)
    }

    fn generics(&self, generics: &RawGenerics<'_>) -> Result<Generics, DecodeError> {
        let mut params = IndexVec::new();
        for (name, bounds) in generics {
            let bounds = bounds
                .iter()
                .map(|b| self.def(b))
                .collect::<Result<Box<[DefId]>, _>>()?;
            params
                .push(GenericParam {
                    name: self.name(name),
                    bounds,
                })
                .map_err(|_| DecodeError::Malformed("too many type parameters"))?;
        }
        Ok(Generics { params })
    }

    /// The signature of the item numbered `index`, whose parent is
    /// `parent` for a constructor or an interface function.
    fn sig(
        &self,
        sig: &RawSig<'_>,
        index: DefIndex,
        parent: Option<DefIndex>,
    ) -> Result<ItemSig, DecodeError> {
        Ok(match sig {
            RawSig::Fn {
                generics,
                params,
                ret,
                clamped,
            } => ItemSig::Fn(FnSig {
                generics: self.generics(generics)?,
                params: params
                    .iter()
                    .map(|(name, ty)| {
                        Ok(ParamSig {
                            name: self.name(name),
                            ty: self.ty(*ty)?,
                        })
                    })
                    .collect::<Result<_, DecodeError>>()?,
                ret: self.ty(*ret)?,
                clamped: clamped.as_slice().into(),
            }),
            RawSig::Type {
                generics,
                exposed,
                home,
                hooks,
                kind,
            } => ItemSig::Type(TypeDef {
                generics: self.generics(generics)?,
                kind: match kind {
                    RawTypeKind::Builtin(b) => TypeDefKind::Builtin(*b),
                    RawTypeKind::External => TypeDefKind::External,
                    RawTypeKind::Adt(_) => TypeDefKind::Adt {
                        ctors: self.numbering.children_of(index),
                    },
                },
                exposed: *exposed,
                home: home.as_deref().map(module_path).transpose()?,
                hooks: hooks.map(|[equal, hash, debug]| TypeHooks {
                    equal: self.name(equal),
                    hash: self.name(hash),
                    debug: self.name(debug),
                }),
            }),
            RawSig::Ctor { tag, fields } => ItemSig::Ctor(CtorSig {
                // A constructor's type is its parent, the item it is
                // encoded in; a constructor signature anywhere else
                // points at itself and is never read.
                ty: parent.unwrap_or(index),
                tag: *tag,
                fields: match fields {
                    RawFields::Unit => CtorFields::Unit,
                    RawFields::Positional(tys) => CtorFields::Positional(self.tys(tys)?),
                    RawFields::Named(fields) => CtorFields::Named(
                        fields
                            .iter()
                            .map(|(name, ty)| Ok((self.name(name), self.ty(*ty)?)))
                            .collect::<Result<_, DecodeError>>()?,
                    ),
                },
            }),
            RawSig::Alias { generics, target } => ItemSig::Alias(AliasDef {
                generics: self.generics(generics)?,
                target: self.ty(*target)?,
            }),
            RawSig::Const(ty) => ItemSig::Const(ConstSig { ty: self.ty(*ty)? }),
            RawSig::Interface { embeds, .. } => ItemSig::Interface(InterfaceDef {
                embeds: embeds
                    .iter()
                    .map(|e| self.def(e))
                    .collect::<Result<_, _>>()?,
                fns: self.numbering.children_of(index),
            }),
            RawSig::InterfaceFn { params, ret } => ItemSig::InterfaceFn(InterfaceFnSig {
                params: self.tys(params)?,
                ret: self.ty(*ret)?,
            }),
            RawSig::Test => ItemSig::Test,
            RawSig::Error => ItemSig::Error,
        })
    }
}

fn module_path(segments: &[&str]) -> Result<ModulePath, DecodeError> {
    let Some((first, rest)) = segments.split_first() else {
        return Err(DecodeError::Malformed("a module path without segments"));
    };
    Ok(rest
        .iter()
        .fold(ModulePath::root(first), |p, segment| p.child(segment)))
}
