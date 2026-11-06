//! Tests of the decoder, the artifact, the debug round trip and recording
//! views (D-953, D-954, D-955).

use std::sync::Arc;

use cheby_diag::{FileId, Span, TextRange};
use cheby_package::{ModulePath, Target, Targets};

use crate::{
    AliasDef, ConstSig, CtorFields, CtorSig, DecodeError, DefId, DefIndex, DefKind, DepView, FnSig,
    GenericParam, Generics, Globals, IfaceItem, IndexVec, InterfaceDef, InterfaceFnSig,
    InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, ParamIdx,
    ParamSig, ReadLog, Ty, TyKind, TypeDef, TypeDefKind, Vis, apply_spans, builtin,
    check_roundtrip, decode_interface, encode, encoded_order, hash_encoding, interface_hash,
    read_artifact, write_artifact,
};

/// Two modules of one package: `app::dep` and `app`, which reads it.
struct Fixture {
    globals: Globals,
    modules: ModuleTable,
    set: InterfaceSet,
    dep: ModuleId,
    app: ModuleId,
}

fn span(start: u32, len: u32) -> Span {
    Span::new(FileId::new(1), TextRange::at(start, len))
}

fn item(g: &Globals, name: &str, kind: DefKind, vis: Vis, sig: ItemSig) -> IfaceItem {
    IfaceItem {
        name: g.names.intern(name),
        kind,
        vis,
        parent: None,
        span: Some(span(name.len().try_into().unwrap(), 3)),
        targets: Targets::ALL,
        deprecated: None,
        sig,
    }
}

fn child(g: &Globals, name: &str, kind: DefKind, parent: u32, sig: ItemSig) -> IfaceItem {
    IfaceItem {
        parent: Some(DefIndex::from_u32(parent)),
        ..item(g, name, kind, Vis::Pub, sig)
    }
}

fn type_sig(ctors: &[u32], exposed: bool) -> ItemSig {
    ItemSig::Type(TypeDef {
        generics: Generics::empty(),
        kind: TypeDefKind::Adt {
            ctors: ctors.iter().copied().map(DefIndex::from_u32).collect(),
        },
        exposed,
        home: None,
        hooks: None,
    })
}

fn fixture() -> Fixture {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let dep = modules
        .add(ModuleInfo {
            path: ModulePath::root("app").child("dep"),
            package: PackageId::LOCAL,
            file: None,
        })
        .unwrap();
    let app = modules
        .add(ModuleInfo {
            path: ModulePath::root("app"),
            package: PackageId::LOCAL,
            file: None,
        })
        .unwrap();
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let g = &globals;
    let float = Ty::FLOAT;
    // app::dep: `pub exposed type Shape { Circle(Float), Square { side: Float } }`
    // and `pub interface Area { fn area(Self) -> Float }`.
    let dep_items = vec![
        item(g, "Shape", DefKind::Type, Vis::Pub, type_sig(&[1, 2], true)),
        child(
            g,
            "Circle",
            DefKind::Ctor,
            0,
            ItemSig::Ctor(CtorSig {
                ty: DefIndex::from_u32(0),
                tag: 0,
                fields: CtorFields::Positional(Box::new([float])),
            }),
        ),
        child(
            g,
            "Square",
            DefKind::Ctor,
            0,
            ItemSig::Ctor(CtorSig {
                ty: DefIndex::from_u32(0),
                tag: 1,
                fields: CtorFields::Named(Box::new([(g.names.intern("side"), float)])),
            }),
        ),
        item(
            g,
            "Area",
            DefKind::Interface,
            Vis::Pub,
            ItemSig::Interface(InterfaceDef {
                embeds: Box::new([]),
                fns: Box::new([DefIndex::from_u32(4)]),
            }),
        ),
        child(
            g,
            "area",
            DefKind::InterfaceFn,
            3,
            ItemSig::InterfaceFn(InterfaceFnSig {
                params: Box::new([g.types.param(ParamIdx::from_u32(0))]),
                ret: float,
            }),
        ),
    ];
    let dep_iface = ModuleInterface::new(
        dep,
        PackageId::LOCAL,
        IndexVec::from_raw(dep_items).unwrap(),
    )
    .unwrap();
    set.insert(Arc::new(dep_iface)).unwrap();
    Fixture {
        globals,
        modules,
        set,
        dep,
        app,
    }
}

/// A module that uses every kind of item, signature and type, its own
/// definitions, a dependency's and the builtin module's, with a private
/// item and a test that the encoding leaves out.
#[expect(clippy::too_many_lines, reason = "one literal item per kind")]
fn rich(f: &Fixture) -> ModuleInterface {
    let g = &f.globals;
    let t = &g.types;
    let shape = t.named(DefId::new(f.dep, DefIndex::from_u32(0)), []);
    let area = DefId::new(f.dep, DefIndex::from_u32(3));
    let point = t.named(DefId::new(f.app, DefIndex::from_u32(2)), []);
    let describe = DefId::new(f.app, DefIndex::from_u32(5));
    let t0 = t.param(ParamIdx::from_u32(0));
    let items = vec![
        // 0: a private helper, left out.
        item(
            g,
            "helper",
            DefKind::Fn,
            Vis::Priv,
            ItemSig::Fn(FnSig {
                generics: Generics::empty(),
                params: Box::new([]),
                ret: Ty::NIL,
                clamped: Box::new([]),
            }),
        ),
        // 1: a test, left out.
        item(g, "a test", DefKind::Test, Vis::Priv, ItemSig::Test),
        // 2, 3: `pub exposed type Point { x: Int, y: Int }`.
        item(g, "Point", DefKind::Type, Vis::Pub, type_sig(&[3], true)),
        child(
            g,
            "Point",
            DefKind::Ctor,
            2,
            ItemSig::Ctor(CtorSig {
                ty: DefIndex::from_u32(2),
                tag: 0,
                fields: CtorFields::Named(Box::new([
                    (g.names.intern("x"), Ty::INT),
                    (g.names.intern("y"), Ty::INT),
                ])),
            }),
        ),
        // 4: `type Handle` (external, package-visible, native only).
        IfaceItem {
            targets: Targets::only(Target::Native),
            ..item(
                g,
                "Handle",
                DefKind::Type,
                Vis::Package,
                ItemSig::Type(TypeDef {
                    generics: Generics::empty(),
                    kind: TypeDefKind::External,
                    exposed: false,
                    home: Some(ModulePath::root("std").child("io")),
                    hooks: Some(crate::TypeHooks {
                        equal: g.names.intern("equal_handle"),
                        hash: g.names.intern("hash_handle"),
                        debug: g.names.intern("debug_handle"),
                    }),
                }),
            )
        },
        // 5, 6: `interface Describe: dep::Area { fn describe(Self) -> String }`.
        IfaceItem {
            vis: Vis::Package,
            ..item(
                g,
                "Describe",
                DefKind::Interface,
                Vis::Package,
                ItemSig::Interface(InterfaceDef {
                    embeds: Box::new([area]),
                    fns: Box::new([DefIndex::from_u32(6)]),
                }),
            )
        },
        IfaceItem {
            vis: Vis::Package,
            ..child(
                g,
                "describe",
                DefKind::InterfaceFn,
                5,
                ItemSig::InterfaceFn(InterfaceFnSig {
                    params: Box::new([t0]),
                    ret: Ty::STRING,
                }),
            )
        },
        // 7: `@deprecated("use paint") @target(native)
        // pub fn draw<T: Describe>(s: dep::Shape, p: Point, xs: List<T>) -> Option<Point>`.
        IfaceItem {
            deprecated: Some("use paint".into()),
            targets: Targets::only(Target::Native),
            ..item(
                g,
                "draw",
                DefKind::Fn,
                Vis::Pub,
                ItemSig::Fn(FnSig {
                    generics: Generics {
                        params: IndexVec::from_raw(vec![GenericParam {
                            name: g.names.intern("T"),
                            bounds: Box::new([describe]),
                        }])
                        .unwrap(),
                    },
                    params: Box::new([
                        ParamSig {
                            name: g.names.intern("s"),
                            ty: shape,
                        },
                        ParamSig {
                            name: g.names.intern("p"),
                            ty: point,
                        },
                        ParamSig {
                            name: g.names.intern("xs"),
                            ty: t.named(builtin::LIST, [t0]),
                        },
                    ]),
                    ret: t.named(builtin::OPTION, [point]),
                    clamped: Box::new([2]),
                }),
            )
        },
        // 8: `pub type Pair = (Point, dep::Shape)`.
        item(
            g,
            "Pair",
            DefKind::Alias,
            Vis::Pub,
            ItemSig::Alias(AliasDef {
                generics: Generics::empty(),
                target: t.intern(TyKind::Tuple(Box::new([point, shape]))),
            }),
        ),
        // 9: `pub const ORIGIN: Point`.
        item(
            g,
            "ORIGIN",
            DefKind::Const,
            Vis::Pub,
            ItemSig::Const(ConstSig { ty: point }),
        ),
        // 10: `pub fn apply(f: fn(Int) -> Never, d: dyn dep::Area)`.
        item(
            g,
            "apply",
            DefKind::Fn,
            Vis::Pub,
            ItemSig::Fn(FnSig {
                generics: Generics::empty(),
                params: Box::new([
                    ParamSig {
                        name: g.names.intern("f"),
                        ty: t.func([Ty::INT], Ty::NEVER),
                    },
                    ParamSig {
                        name: g.names.intern("d"),
                        ty: t.intern(TyKind::Dyn {
                            iface: area,
                            args: Box::new([]),
                        }),
                    },
                ]),
                ret: Ty::NIL,
                clamped: Box::new([]),
            }),
        ),
        // 11: a function whose signature had an error.
        item(g, "broken", DefKind::Fn, Vis::Pub, ItemSig::Error),
    ];
    let mut iface =
        ModuleInterface::new(f.app, PackageId::LOCAL, IndexVec::from_raw(items).unwrap()).unwrap();
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    iface.hash = Some(interface_hash(&iface, &deps, &f.modules, &f.globals).unwrap());
    iface
}

fn rich_bytes(f: &Fixture) -> (ModuleInterface, Vec<u8>) {
    let iface = rich(f);
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    let bytes = encode(&iface, &deps, &f.modules, &f.globals).unwrap();
    (iface, bytes)
}

#[test]
fn iface_decode_roundtrips_every_kind_of_item() {
    let f = fixture();
    let (iface, bytes) = rich_bytes(&f);
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    check_roundtrip(&iface, &deps, &f.modules, &f.globals).unwrap();
    let decoded = decode_interface(
        f.app,
        PackageId::LOCAL,
        &bytes,
        &deps,
        &f.modules,
        &f.globals,
    )
    .unwrap();
    // The private helper and the test are left out; everything else is
    // numbered in encoded order with children after their parent.
    assert_eq!(decoded.items.len(), 10);
    assert_eq!(
        decoded.items.len(),
        encoded_order(&iface, &f.globals).unwrap().len()
    );
    assert!(decoded.items.iter().all(|i| i.span.is_none()));
    assert_eq!(decoded.hash, iface.hash);
    assert_eq!(decoded.hash, Some(hash_encoding(&bytes)));
    // Re-encoding the decoded interface gives the same bytes.
    assert_eq!(
        encode(&decoded, &deps, &f.modules, &f.globals).unwrap(),
        bytes
    );
    // Constructors and interface functions take their parent's fields.
    let names = &f.globals.names;
    let describe = decoded
        .items
        .iter()
        .find(|i| names.get(i.name) == "describe")
        .unwrap();
    assert_eq!(describe.vis, Vis::Package);
    assert_eq!(describe.kind, DefKind::InterfaceFn);
}

#[test]
fn iface_decode_roundtrips_the_builtin_module() {
    let g = Globals::new();
    let modules = ModuleTable::new();
    let set = InterfaceSet::new(builtin::interface(&g));
    let iface = builtin::interface(&g);
    let none = [];
    let deps = DepView::new(&set, &none);
    check_roundtrip(&iface, &deps, &modules, &g).unwrap();
}

/// A fact the encoding does not hold (here a constructor less visible
/// than its type) is caught and named.
#[test]
fn iface_roundtrip_names_the_first_differing_item() {
    let f = fixture();
    let mut iface = rich(&f);
    let point_ctor = DefIndex::from_u32(3);
    let mut items = iface.items.clone().into_raw();
    items[3].vis = Vis::Package;
    iface = ModuleInterface::new(
        iface.module,
        iface.package,
        IndexVec::from_raw(items).unwrap(),
    )
    .unwrap();
    assert_eq!(iface.items.get(point_ctor).unwrap().vis, Vis::Package);
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    let err = check_roundtrip(&iface, &deps, &f.modules, &f.globals).unwrap_err();
    assert!(err.message().contains("`app`"), "{}", err.message());
    assert!(err.message().contains("`Point`"), "{}", err.message());
    assert!(err.message().contains("visibility"), "{}", err.message());
}

#[test]
fn iface_decode_reports_what_it_cannot_read() {
    let f = fixture();
    let (_, bytes) = rich_bytes(&f);
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    let decode = |bytes: &[u8], deps: &DepView<'_>, modules: &ModuleTable| {
        decode_interface(f.app, PackageId::LOCAL, bytes, deps, modules, &f.globals)
    };
    // Another version, another format.
    let mut v2 = bytes.clone();
    v2[4] = 2;
    assert_eq!(
        decode(&v2, &deps, &f.modules).unwrap_err(),
        DecodeError::Version(2)
    );
    let mut other = bytes.clone();
    other[0] = b'X';
    assert!(matches!(
        decode(&other, &deps, &f.modules),
        Err(DecodeError::Malformed(_))
    ));
    // Trailing bytes.
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(
        decode(&longer, &deps, &f.modules),
        Err(DecodeError::Malformed(_))
    ));
    // The module the dependency's path names is not in this process's
    // table: a miss.
    let mut table = ModuleTable::new();
    table
        .add(ModuleInfo {
            path: ModulePath::root("other"),
            package: PackageId::LOCAL,
            file: None,
        })
        .unwrap();
    table
        .add(ModuleInfo {
            path: ModulePath::root("app"),
            package: PackageId::LOCAL,
            file: None,
        })
        .unwrap();
    assert_eq!(
        decode(&bytes, &deps, &table).unwrap_err(),
        DecodeError::UnknownModule("app::dep".into())
    );
    // A view that cannot read the dependency.
    let none = [];
    let blind = DepView::new(&f.set, &none);
    assert_eq!(
        decode(&bytes, &blind, &f.modules).unwrap_err(),
        DecodeError::UnknownModule("app::dep".into())
    );
    // The dependency no longer defines `Shape`.
    let mut set = InterfaceSet::new(builtin::interface(&f.globals));
    set.insert(Arc::new(ModuleInterface::empty(f.dep, PackageId::LOCAL)))
        .unwrap();
    let emptied = DepView::new(&set, &allowed);
    assert!(matches!(
        decode(&bytes, &emptied, &f.modules),
        Err(DecodeError::UnknownItem(_))
    ));
    // The encoding of another module.
    assert!(matches!(
        decode_interface(
            f.dep,
            PackageId::LOCAL,
            &bytes,
            &deps,
            &f.modules,
            &f.globals
        ),
        Err(DecodeError::Malformed(_))
    ));
}

/// Mutations of a real encoding: every byte position replaced by several
/// values, every truncation, and runs of zero and 0xFF bytes. Each must
/// decode or fail with an error, never panic.
fn never_panics_on(bytes: &[u8], decode: &dyn Fn(&[u8]) -> Result<ModuleInterface, DecodeError>) {
    assert!(decode(bytes).is_ok());
    for len in 0..bytes.len() {
        assert!(decode(&bytes[..len]).is_err(), "truncated to {len}");
    }
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for at in 0..bytes.len() {
        for value in [0x00, 0x01, 0x02, 0x07, 0x09, 0x7F, 0x80, 0xFE, 0xFF] {
            let mut mutated = bytes.to_vec();
            mutated[at] = value;
            let _ = decode(&mutated);
        }
        let mut flipped = bytes.to_vec();
        flipped[at] ^= 1 << (next() % 8);
        let _ = decode(&flipped);
        // A huge length or count in the four bytes starting here.
        let mut huge = bytes.to_vec();
        for b in huge.iter_mut().skip(at).take(4) {
            *b = 0xFF;
        }
        let _ = decode(&huge);
    }
    // Random multi-byte mutations, insertions and deletions.
    for _ in 0..4000 {
        let mut mutated = bytes.to_vec();
        for _ in 0..=(next() & 3) {
            let len = u64::try_from(mutated.len()).unwrap();
            let at = usize::try_from(next().checked_rem(len).unwrap_or(0)).unwrap();
            match next() % 3 {
                0 if at < mutated.len() => mutated[at] = u8::try_from(next() % 256).unwrap(),
                1 => mutated.insert(at, u8::try_from(next() % 256).unwrap()),
                _ if at < mutated.len() => {
                    mutated.remove(at);
                }
                _ => {}
            }
        }
        let _ = decode(&mutated);
    }
    // Deeply nested types are read without recursion: a million tuples
    // with no innermost type is truncated, and closed with `Never` it is
    // read to the end (another module's encoding for one of the callers).
    let mut deep = deep_const(1_000_000);
    assert!(matches!(decode(&deep), Err(DecodeError::Malformed(_))));
    deep.push(1);
    assert!(matches!(
        decode(&deep),
        Ok(_) | Err(DecodeError::Malformed(_))
    ));
    // Signatures nested in signatures are refused, never followed to the
    // end of the stack (review SOUND-W1-1).
    assert!(matches!(
        decode(&nested_sigs(30_000)),
        Err(DecodeError::Malformed(_))
    ));
    assert!(matches!(
        decode(&nested_sigs(1)),
        Err(DecodeError::Malformed(_))
    ));
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&u32::try_from(s.len()).unwrap().to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// The encoding of module `app` with one item, `pub const X: ((((…))))`,
/// `depth` one-element tuples deep, and no innermost type yet.
fn deep_const(depth: usize) -> Vec<u8> {
    let mut deep = b"CHBI\x01".to_vec();
    deep.extend_from_slice(&1u32.to_le_bytes());
    push_str(&mut deep, "app");
    deep.extend_from_slice(&1u32.to_le_bytes());
    deep.extend_from_slice(&[3, 2]);
    push_str(&mut deep, "X");
    deep.extend_from_slice(&[3, 0, 4]);
    for _ in 0..depth {
        deep.push(3);
        deep.extend_from_slice(&1u32.to_le_bytes());
    }
    deep
}

/// The encoding of module `app` with one type `T` whose one constructor's
/// signature is again a type signature with one constructor, `depth`
/// times, ending in a test signature: a shape the encoder never writes.
fn nested_sigs(depth: usize) -> Vec<u8> {
    let mut out = b"CHBI\x01".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    push_str(&mut out, "app");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&[1, 2]);
    push_str(&mut out, "T");
    out.extend_from_slice(&[3, 0]);
    for _ in 0..depth {
        out.push(1);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&1u32.to_le_bytes());
        push_str(&mut out, "C");
    }
    out.push(7);
    out
}

/// A type nested deeper than any stack bound decodes and round-trips:
/// a valid program's types have no depth limit, since each alias of a
/// chain adds a level (review R1).
#[test]
fn iface_decode_reads_types_of_any_depth() {
    let g = Globals::new();
    let mut modules = ModuleTable::new();
    let app = modules
        .add(ModuleInfo {
            path: ModulePath::root("app"),
            package: PackageId::LOCAL,
            file: None,
        })
        .unwrap();
    let set = InterfaceSet::new(builtin::interface(&g));
    let none = [];
    let deps = DepView::new(&set, &none);
    for depth in [300, 200_000] {
        let mut bytes = deep_const(depth);
        bytes.push(1);
        let decoded = decode_interface(app, PackageId::LOCAL, &bytes, &deps, &modules, &g).unwrap();
        let ItemSig::Const(ConstSig { ty }) = decoded.items.iter().next().unwrap().sig else {
            panic!("not a constant");
        };
        let mut ty = ty;
        let mut levels = 0;
        while let TyKind::Tuple(elems) = g.types.kind(ty) {
            ty = elems[0];
            levels += 1;
        }
        assert_eq!(levels, depth);
        assert_eq!(ty, Ty::NEVER);
        // The encoder recurses once per level, as the front end does, so
        // only the shallower one is re-encoded here.
        if depth < 1000 {
            assert_eq!(encode(&decoded, &deps, &modules, &g).unwrap(), bytes);
            check_roundtrip(&decoded, &deps, &modules, &g).unwrap();
        }
    }
}

#[test]
fn iface_decode_never_panics() {
    let f = fixture();
    let (_, bytes) = rich_bytes(&f);
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    never_panics_on(&bytes, &|b| {
        decode_interface(f.app, PackageId::LOCAL, b, &deps, &f.modules, &f.globals)
    });

    let g = Globals::new();
    let modules = ModuleTable::new();
    let set = InterfaceSet::new(builtin::interface(&g));
    let none = [];
    let deps = DepView::new(&set, &none);
    let builtin = builtin::interface(&g);
    let bytes = encode(&builtin, &deps, &modules, &g).unwrap();
    never_panics_on(&bytes, &|b| {
        decode_interface(ModuleId::BUILTIN, PackageId::STD, b, &deps, &modules, &g)
    });
}

fn spans_of(iface: &ModuleInterface, g: &Globals) -> Vec<Option<(u32, u32)>> {
    encoded_order(iface, g)
        .unwrap()
        .into_iter()
        .map(|index| {
            iface
                .item(index)
                .unwrap()
                .span
                .map(|s| (s.range.start(), s.range.len()))
        })
        .collect()
}

#[test]
fn iface_artifact_roundtrip() {
    let f = fixture();
    let (iface, bytes) = rich_bytes(&f);
    let build_id = [7u8; 16];
    let mut spans = spans_of(&iface, &f.globals);
    spans[1] = None;
    let blob = write_artifact(build_id, &bytes, &spans);
    assert!(blob.starts_with(b"CHBA\x01"));
    let read = read_artifact(&blob).unwrap();
    assert_eq!(read.build_id, build_id);
    assert_eq!(Some(read.hash), iface.hash);
    assert_eq!(read.h, &bytes[..]);
    assert_eq!(read.spans, spans);
    // Decoded, with the spans of section S put back.
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    let mut decoded = decode_interface(
        f.app,
        PackageId::LOCAL,
        read.h,
        &deps,
        &f.modules,
        &f.globals,
    )
    .unwrap();
    apply_spans(&mut decoded, FileId::new(1), &read.spans).unwrap();
    for (n, (index, decoded_item)) in encoded_order(&iface, &f.globals)
        .unwrap()
        .into_iter()
        .zip(decoded.items.iter())
        .enumerate()
    {
        let expected = if n == 1 {
            None
        } else {
            iface.item(index).unwrap().span
        };
        assert_eq!(decoded_item.span, expected);
    }
    assert!(apply_spans(&mut decoded, FileId::new(1), &read.spans[1..]).is_err());
}

#[test]
fn iface_artifact_flipped_byte_in_h_is_an_error() {
    let f = fixture();
    let (iface, bytes) = rich_bytes(&f);
    let blob = write_artifact([1; 16], &bytes, &spans_of(&iface, &f.globals));
    // H starts after the magic (5), the build ID (16), the hash (16) and
    // H's length (8).
    let h_start = 5 + 16 + 16 + 8;
    for at in h_start..h_start + bytes.len() {
        let mut flipped = blob.clone();
        flipped[at] ^= 0x10;
        assert_eq!(
            read_artifact(&flipped).unwrap_err(),
            DecodeError::HashMismatch,
            "byte {at}"
        );
    }
    // So is a flipped byte of the recorded hash.
    let mut flipped = blob.clone();
    flipped[5 + 16] ^= 1;
    assert_eq!(
        read_artifact(&flipped).unwrap_err(),
        DecodeError::HashMismatch
    );
}

#[test]
fn iface_artifact_span_changes_never_change_the_hash() {
    let f = fixture();
    let (iface, bytes) = rich_bytes(&f);
    let spans = spans_of(&iface, &f.globals);
    let moved: Vec<Option<(u32, u32)>> = spans
        .iter()
        .map(|s| s.map(|(start, len)| (start + 100, len)))
        .collect();
    let a = read_artifact(&write_artifact([2; 16], &bytes, &spans))
        .unwrap()
        .hash;
    let b = read_artifact(&write_artifact([2; 16], &bytes, &moved))
        .unwrap()
        .hash;
    assert_eq!(a, b);
    // A flipped byte in S still reads, with other spans and the same hash.
    let mut blob = write_artifact([2; 16], &bytes, &spans);
    let last = blob.len() - 1;
    blob[last] ^= 1;
    let read = read_artifact(&blob).unwrap();
    assert_eq!(read.hash, a);
    assert_ne!(read.spans, spans);
    // Moving every item in the source changes no interface hash either.
    let mut items = iface.items.clone().into_raw();
    for item in &mut items {
        item.span = item
            .span
            .map(|s| Span::new(s.file, TextRange::at(s.range.start() + 9, 1)));
    }
    let moved_iface = ModuleInterface::new(
        iface.module,
        iface.package,
        IndexVec::from_raw(items).unwrap(),
    )
    .unwrap();
    let allowed = [f.dep];
    let deps = DepView::new(&f.set, &allowed);
    assert_eq!(
        interface_hash(&moved_iface, &deps, &f.modules, &f.globals).ok(),
        iface.hash
    );
}

#[test]
fn iface_artifact_rejects_malformed_blobs() {
    let f = fixture();
    let (iface, bytes) = rich_bytes(&f);
    let blob = write_artifact([3; 16], &bytes, &spans_of(&iface, &f.globals));
    for len in 0..blob.len() {
        assert!(read_artifact(&blob[..len]).is_err(), "truncated to {len}");
    }
    let mut v2 = blob.clone();
    v2[4] = 2;
    assert_eq!(read_artifact(&v2).unwrap_err(), DecodeError::Version(2));
    let mut longer = blob.clone();
    longer.push(0);
    assert!(matches!(
        read_artifact(&longer),
        Err(DecodeError::Malformed(_))
    ));
    // A span section that is not whole entries.
    let mut odd = write_artifact([3; 16], &bytes, &[]);
    let s_len_at = odd.len() - 8;
    odd[s_len_at] = 3;
    odd.extend_from_slice(&[0, 0, 0]);
    assert!(matches!(
        read_artifact(&odd),
        Err(DecodeError::Malformed(_))
    ));
    // A huge section length.
    let mut huge = blob.clone();
    for b in &mut huge[5 + 32..5 + 32 + 8] {
        *b = 0xFF;
    }
    assert!(matches!(
        read_artifact(&huge),
        Err(DecodeError::Malformed(_))
    ));
}

#[test]
fn iface_recording_views_note_what_they_hand_out() {
    let f = fixture();
    let log = ReadLog::new();
    let allowed = [f.dep];
    let view = DepView::recording(&f.set, &allowed, &log);
    // `allows` hands nothing out; a module outside the view is not read.
    assert!(view.allows(f.dep));
    assert!(view.get(f.app).is_none());
    let nothing: [ModuleId; 0] = [];
    assert_eq!(log.modules(), nothing);
    // The builtin module is never noted.
    assert!(view.get(ModuleId::BUILTIN).is_some());
    assert!(view.item(builtin::SOME).is_some());
    assert_eq!(log.modules(), nothing);
    assert!(
        view.item(DefId::new(f.dep, DefIndex::from_u32(0)))
            .is_some()
    );
    assert_eq!(log.modules(), [f.dep]);
    // A copy records into the same log; an unrecorded copy does not.
    let copy = view;
    let _ = copy.get(f.dep);
    assert_eq!(log.modules(), [f.dep]);
    let quiet_log = ReadLog::new();
    let quiet = DepView::recording(&f.set, &allowed, &quiet_log).unrecorded();
    assert!(quiet.get(f.dep).is_some());
    assert_eq!(quiet_log.modules(), nothing);
    // A plain view records nothing anywhere.
    let plain = DepView::new(&f.set, &allowed);
    assert!(plain.get(f.dep).is_some());
}

#[test]
fn iface_read_log_is_sorted_and_thread_safe() {
    let log = ReadLog::new();
    let ids: Vec<u32> = vec![4097, 3, 64, 63, 5000, 1, 4095, 4096, 0, 70_000];
    std::thread::scope(|scope| {
        for chunk in ids.chunks(2) {
            let log = &log;
            scope.spawn(move || {
                for _ in 0..100 {
                    for &id in chunk {
                        log.note(ModuleId::from_u32(id));
                    }
                }
            });
        }
    });
    let got: Vec<u32> = log.modules().iter().map(|m| m.as_u32()).collect();
    // The builtin module (0) is never noted.
    assert_eq!(got, [1, 3, 63, 64, 4095, 4096, 4097, 5000, 70_000]);
    assert!(format!("{log:?}").starts_with("ReadLog("));
}
