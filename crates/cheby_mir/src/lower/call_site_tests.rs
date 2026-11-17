//! Lowering's safety net for `@std_call_site` (D-1628, D-1631): THIR built
//! by hand, as no checked program has it, that calls a function with the
//! attribute or holds it as a value, without the checker's replacement.
//! Each is an internal error, never code that would panic in the
//! function's body at run time (D-1630). The function's own item lowers.

use cheby_diag::{FileId, SourceMap, Span, TextRange, TextSize};
use cheby_iface::{
    DefId, DefIndex, DefKind, DepView, FnSig, Generics, Globals, IfaceItem, IndexVec, InterfaceSet,
    ItemSig, ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, ParamSig, Ty, Vis,
    builtin,
};
use cheby_package::{ModulePath, Targets};
use cheby_types::thir::{self, Callee, ExprKind, Lit};

use crate::Defs;
use crate::lower::{LowerCx, lower_items};

const MODULE: ModuleId = ModuleId::from_u32(1);

/// `here(tag: String)` with `@std_call_site(here_at)`, its constructor,
/// a function that calls `here` and one that returns it as a value.
const HERE: u32 = 0;
const HERE_AT: u32 = 1;
const CALLS: u32 = 2;
const VALUE: u32 = 3;

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

fn sp(file: FileId, n: u32) -> Span {
    Span::new(
        file,
        TextRange::at(TextSize::from(n), TextSize::from(1_u32)),
    )
}

/// A body whose value is `kind`, of type `ty`, after `exprs`.
fn body(mut exprs: Vec<thir::Expr>, kind: ExprKind, ty: Ty, ret: Ty, span: Span) -> thir::Body {
    exprs.push(thir::Expr { kind, ty, span });
    let exprs = IndexVec::from_raw(exprs).unwrap();
    let value = exprs.indices().last().unwrap();
    thir::Body {
        params: Vec::new(),
        locals: IndexVec::new(),
        exprs,
        value,
        ret,
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the whole hand-built module: interface items and THIR"
)]
fn an_unreplaced_call_or_value_is_an_internal_error() {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut sources = SourceMap::new();
    let file = sources.add_file("std/probe.cheby", " ".repeat(16)).unwrap();
    modules
        .add(ModuleInfo {
            path: ModulePath::root("std").child("probe"),
            package: PackageId::STD,
            file: Some(file),
        })
        .unwrap();
    let param = |name: &str, ty: Ty| ParamSig {
        name: globals.names.intern(name),
        ty,
    };
    let sig = |params: Box<[ParamSig]>, ret: Ty, call_site: Option<&str>| FnSig {
        generics: Generics::empty(),
        params,
        ret,
        clamped: Box::new([]),
        initializer: None,
        call_site: call_site.map(|name| globals.names.intern(name)),
    };
    let here_ty = globals.types.func([Ty::STRING], Ty::NIL);
    let mut items = IndexVec::new();
    for (n, name, sig) in [
        (
            HERE,
            "here",
            sig(
                Box::new([param("tag", Ty::STRING)]),
                Ty::NIL,
                Some("here_at"),
            ),
        ),
        (
            HERE_AT,
            "here_at",
            sig(
                Box::new([
                    param("tag", Ty::STRING),
                    param("path", Ty::STRING),
                    param("line", Ty::INT),
                    param("column", Ty::INT),
                ]),
                Ty::NIL,
                None,
            ),
        ),
        (CALLS, "calls", sig(Box::new([]), Ty::NIL, None)),
        (VALUE, "value", sig(Box::new([]), here_ty, None)),
    ] {
        items
            .push(IfaceItem {
                name: globals.names.intern(name),
                kind: DefKind::Fn,
                vis: Vis::Package,
                parent: None,
                span: Some(sp(file, n)),
                targets: Targets::ALL,
                deprecated: None,
                sig: ItemSig::Fn(sig),
            })
            .unwrap();
    }
    let iface = ModuleInterface::new(MODULE, PackageId::STD, items).unwrap();
    let set = InterfaceSet::new(builtin::interface(&globals));
    let nil = |n: u32| {
        body(
            Vec::new(),
            ExprKind::Ctor {
                ctor: builtin::NIL,
                ty_args: Box::new([]),
            },
            Ty::NIL,
            Ty::NIL,
            sp(file, n),
        )
    };
    let item = |n: u32, body: thir::Body| thir::FnDef {
        def: def(n),
        span: sp(file, n),
        native_external: None,
        body: Some(body),
        entry: None,
    };
    let call = body(
        vec![thir::Expr {
            kind: ExprKind::Lit(Lit::Str("x".into())),
            ty: Ty::STRING,
            span: sp(file, 8),
        }],
        ExprKind::Call {
            callee: Callee::Fn {
                def: def(HERE),
                ty_args: Box::new([]),
                dicts: Box::new([]),
            },
            args: Box::new([thir::ExprId::from_u32(0)]),
        },
        Ty::NIL,
        Ty::NIL,
        sp(file, 9),
    );
    let value = body(
        Vec::new(),
        ExprKind::FnRef {
            def: def(HERE),
            ty_args: Box::new([]),
            dicts: Box::new([]),
        },
        here_ty,
        here_ty,
        sp(file, 10),
    );
    let module = thir::Module {
        module: MODULE,
        fns: vec![
            item(HERE, nil(HERE)),
            item(HERE_AT, nil(HERE_AT)),
            item(CALLS, call),
            item(VALUE, value),
        ],
        consts: Vec::new(),
        tests: Vec::new(),
    };
    let deps = DepView::new(&set, &[]);
    let cx = LowerCx {
        defs: Defs {
            own: Some(&iface),
            deps: &deps,
            modules: &modules,
            globals: &globals,
        },
        sources: &sources,
    };
    let lower = |n: u32| lower_items(&module, &|d| d == def(n), &cx);
    // The written function and its constructor lower as items.
    lower(HERE).unwrap();
    lower(HERE_AT).unwrap();
    for n in [CALLS, VALUE] {
        let ice = lower(n).unwrap_err();
        assert!(
            ice.message().contains(
                "a call of a function with `@std_call_site` reached MIR lowering unreplaced: \
                 `std::probe::here`"
            ),
            "{}",
            ice.message()
        );
    }
}
