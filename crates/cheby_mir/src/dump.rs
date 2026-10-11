//! The stable textual form of MIR (D1), used by goldens and by
//! the determinism check.
//!
//! ```text
//! module hello_world
//!
//! fn hello_world::main() -> value check str#0 {
//!   str#0 = "src/main.cheby:5:4"
//!   str#1 = "Hello, world!"
//! bb0:
//!   _0: value = const str#1
//!   tail_call std::io::println(_0)
//! }
//! ```
//!
//! Grammar:
//!
//! - The header: `module <path>`, `tests <path>` or `entries`.
//! - Each function: `fn <key>[ [c] ](<params>) -> <repr>[ check str#K] {`,
//!   then the string table `str#N = "<text>"`, then blocks
//!   `bbN(<params>):` with statements `_N: <repr> = <rvalue>`,
//!   `<rvalue>` (evaluated for effect), `dup _N`, `drop _N` and one
//!   terminator, then `}`. The hidden `ctx` parameter is not printed.
//! - Keys: `<item path>`, `shim <path>`, `closure <owner>#<n>`,
//!   `const_init <path>`, `test <path>`, `entry main(<path>)`,
//!   `entry test(<path>)`, `entry const(<path>)`, `entry repl(<n>)`,
//!   `trampoline <n>`, `tuple_dict_fn <iface fn>/<arity>`,
//!   `dyn_forward <iface fn> for dyn <interface>`.
//! - Constants: `int N`, `float F`, `bool B`, `<sized repr> N`, `str#N`,
//!   `nil`, `ctor <path>`, `fn <path>`, `code <key>`, `empty_list`,
//!   `static#N`, `builtin_desc <name>`.
//! - Layouts: `string`, `bytes`, `boxed_int`, `boxed_float`, `boxed_f32`,
//!   `boxed_u64`, `ctor <path>`, `tuple/N`, `closure/N`,
//!   `dict/<fns>+<embedded>`, `dyn`, `desc/N`, `desc_nominal`.
//! - Rvalues: `<operand>`, `call <callee>(<args>)`,
//!   `call_foreign "<symbol>"(borrow <args>) -> <ctype>`,
//!   `call_runtime <helper>(<args>)` with `borrow ` before each borrowed
//!   argument, `alloc <layout>(<fields>)`, `field _N.<i> of <layout>`,
//!   `read_layout _N`, `load_const <path>`, `box <repr> <op>`,
//!   `unbox <repr> <op>`, `checked <op> <repr> <a>, <b> at str#K`,
//!   `compare <op> <repr> <a>, <b>`, `float_arith <op> <repr> <a>, <b>`,
//!   `bitwise <op> <repr> <a>, <b>`, `float_neg <repr> <op>`, `not <op>`,
//!   `read_tag <op> of <option|result|order|type path>`.
//! - After the functions, in this order and each line once:
//!   `layout <layout>: <Type>.<Ctor> tag <n> key <hex>[ record]
//!   [<fields>]` per own layout in unit order, the same with
//!   `foreign_layout` per foreign layout in [`crate::LayoutRef`] order
//!   (fields `[name: ]<repr> @<offset>`, then ` hooks(<eq>, <hash>,
//!   <debug>)` when set), `callee <path>(<reprs>) -> <repr>` per direct
//!   callee by `DefId`, and `static#N <kind> <module>$<name> =
//!   <layout>(<consts>)` followed by ` str#K = "<text>"` per string of
//!   its own table, per static in id order. A blank line separates them
//!   from the functions when there are any.

use std::fmt::Write as _;

use cheby_iface::Ice;

use crate::ir::{
    Abi, BuiltinLayout, Callee, Const, EntryKind, FnKey, Function, LayoutRef, Local, MirModule,
    Operand, Rvalue, Stmt, TagType, Terminator, UnitKind, UseKind,
};
use crate::layout::LayoutDesc;
use crate::repr::Repr;

/// The text of a unit.
///
/// # Errors
///
/// An [`Ice`] if the unit mentions a def it has no name for, or a local
/// it does not declare.
pub fn dump(module: &MirModule) -> Result<String, Ice> {
    let mut out = String::new();
    match module.unit {
        UnitKind::Code(_) => {
            let _ = writeln!(out, "module {}", module.path);
        }
        UnitKind::Tests(_) => {
            let _ = writeln!(out, "tests {}", module.path);
        }
        UnitKind::Entries => out.push_str("entries\n"),
    }
    for f in &module.functions {
        out.push('\n');
        dump_function_into(&mut out, module, f)?;
    }
    let mut tail = String::new();
    for desc in &module.layouts {
        layout_line(&mut tail, module, "layout", desc)?;
    }
    for desc in &module.foreign_layouts {
        layout_line(&mut tail, module, "foreign_layout", desc)?;
    }
    for decl in &module.callees {
        let params: Vec<&str> = decl.sig.params.iter().map(|r| r.name()).collect();
        let _ = writeln!(
            tail,
            "callee {}({}) -> {}",
            def(module, decl.def)?,
            params.join(", "),
            decl.sig.ret
        );
    }
    for s in &module.statics {
        let fields = s
            .fields
            .iter()
            .map(|c| constant(module, c))
            .collect::<Result<Vec<_>, Ice>>()?
            .join(", ");
        let _ = write!(
            tail,
            "static#{} {} {}${} = {}({fields})",
            s.id.as_u32(),
            s.kind.name(),
            s.module,
            s.name,
            layout(module, s.layout)?
        );
        for (id, text) in s.strings.iter_enumerated() {
            let _ = write!(tail, " str#{} = \"{}\"", id.as_u32(), text.escape_debug());
        }
        tail.push('\n');
    }
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(&tail);
    }
    Ok(out)
}

fn layout_line(out: &mut String, m: &MirModule, what: &str, desc: &LayoutDesc) -> Result<(), Ice> {
    let fields: Vec<String> = desc
        .fields
        .iter()
        .map(|f| match &f.name {
            Some(name) => format!("{name}: {} @{}", f.repr, f.offset),
            None => format!("{} @{}", f.repr, f.offset),
        })
        .collect();
    let _ = write!(
        out,
        "{what} {}: {}.{} tag {} key {:016x}{} [{}]",
        layout(m, desc.layout)?,
        desc.type_name,
        desc.ctor_name,
        desc.tag,
        desc.type_key,
        if desc.record { " record" } else { "" },
        fields.join(", ")
    );
    if let Some([eq, hash, debug]) = desc.hooks {
        let _ = write!(
            out,
            " hooks({}, {}, {})",
            def(m, eq)?,
            def(m, hash)?,
            def(m, debug)?
        );
    }
    out.push('\n');
    Ok(())
}

/// The text of one function of `module`.
///
/// # Errors
///
/// As [`dump`].
pub fn dump_function(module: &MirModule, f: &Function) -> Result<String, Ice> {
    let mut out = String::new();
    dump_function_into(&mut out, module, f)?;
    Ok(out)
}

fn dump_function_into(out: &mut String, m: &MirModule, f: &Function) -> Result<(), Ice> {
    let key = key_text(m, f.key)?;
    let abi = match f.abi {
        Abi::Cheby => "",
        Abi::C => " [c] ",
    };
    let params = f
        .params
        .iter()
        .map(|&p| Ok(format!("{}: {}", local(p), f.local(p)?.repr)))
        .collect::<Result<Vec<_>, Ice>>()?
        .join(", ");
    let _ = write!(out, "fn {key}{abi}({params}) -> {}", f.ret);
    if let Some(loc) = f.entry_loc {
        let _ = write!(out, " check str#{}", loc.as_u32());
    }
    out.push_str(" {\n");
    for (id, text) in f.strings.iter_enumerated() {
        let _ = writeln!(out, "  str#{} = \"{}\"", id.as_u32(), text.escape_debug());
    }
    for (id, block) in f.blocks.iter_enumerated() {
        let _ = write!(out, "bb{}", id.as_u32());
        if !block.params.is_empty() {
            let params = block
                .params
                .iter()
                .map(|&p| Ok(format!("{}: {}", local(p), f.local(p)?.repr)))
                .collect::<Result<Vec<_>, Ice>>()?
                .join(", ");
            let _ = write!(out, "({params})");
        }
        out.push_str(":\n");
        for stmt in &block.stmts {
            out.push_str("  ");
            match stmt {
                Stmt::Assign { dst, value } => {
                    let _ = write!(
                        out,
                        "{}: {} = {}",
                        local(*dst),
                        f.local(*dst)?.repr,
                        rvalue(m, value)?
                    );
                }
                Stmt::Eval(value) => out.push_str(&rvalue(m, value)?),
                Stmt::Dup(l) => {
                    let _ = write!(out, "dup {}", local(*l));
                }
                Stmt::Drop(l) => {
                    let _ = write!(out, "drop {}", local(*l));
                }
            }
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(&terminator(m, &block.term)?);
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(())
}

fn local(l: Local) -> String {
    format!("_{}", l.as_u32())
}

fn def(m: &MirModule, d: cheby_iface::DefId) -> Result<String, Ice> {
    Ok(m.name(d)?.path())
}

fn key_text(m: &MirModule, key: FnKey) -> Result<String, Ice> {
    Ok(match key {
        FnKey::Item(d) => def(m, d)?,
        FnKey::ValueShim(d) => format!("shim {}", def(m, d)?),
        FnKey::Closure { owner, index } => format!("closure {}#{index}", def(m, owner)?),
        FnKey::ConstInit(d) => format!("const_init {}", def(m, d)?),
        FnKey::Test(d) => format!("test {}", def(m, d)?),
        FnKey::Entry(EntryKind::Main(d)) => format!("entry main({})", def(m, d)?),
        FnKey::Entry(EntryKind::Test(d)) => format!("entry test({})", def(m, d)?),
        FnKey::Entry(EntryKind::Const(d)) => format!("entry const({})", def(m, d)?),
        FnKey::Entry(EntryKind::Repl(n)) => format!("entry repl({n})"),
        FnKey::Trampoline(n) => format!("trampoline {n}"),
        FnKey::TupleDictFn { iface_fn, arity } => {
            format!("tuple_dict_fn {}/{arity}", def(m, iface_fn)?)
        }
        FnKey::DynForward {
            iface_fn,
            dyn_iface,
        } => format!(
            "dyn_forward {} for dyn {}",
            def(m, iface_fn)?,
            def(m, dyn_iface)?
        ),
    })
}

fn constant(m: &MirModule, c: &Const) -> Result<String, Ice> {
    Ok(match c {
        Const::Int(n) => format!("int {n}"),
        Const::Float(bits) => format!("float {:?}", f64::from_bits(*bits)),
        Const::Bool(b) => format!("bool {b}"),
        Const::Sized { repr, bits } => format!("{repr} {}", sized_text(*repr, *bits)),
        Const::Str(id) => format!("str#{}", id.as_u32()),
        Const::Nil => "nil".to_owned(),
        Const::StaticCtor(d) => format!("ctor {}", def(m, *d)?),
        Const::FnValue(d) => format!("fn {}", def(m, *d)?),
        Const::Code(key) => format!("code <{}>", key_text(m, *key)?),
        Const::EmptyList => "empty_list".to_owned(),
        Const::Static(id) => format!("static#{}", id.as_u32()),
        Const::BuiltinDesc(d) => format!("builtin_desc {}", d.name()),
    })
}

/// A sized constant's value as written in source.
fn sized_text(repr: Repr, bits: u64) -> String {
    match repr {
        Repr::I8 | Repr::I16 | Repr::I32 | Repr::I64 | Repr::Int => bits.cast_signed().to_string(),
        Repr::F32 => {
            let low = u32::try_from(bits & u64::from(u32::MAX)).unwrap_or(0);
            format!("{:?}", f32::from_bits(low))
        }
        Repr::Float => format!("{:?}", f64::from_bits(bits)),
        Repr::Value | Repr::Bool | Repr::U8 | Repr::U16 | Repr::U32 | Repr::U64 => bits.to_string(),
    }
}

fn operand(m: &MirModule, op: &Operand) -> Result<String, Ice> {
    match op {
        Operand::Local(l) => Ok(local(*l)),
        Operand::Const(c) => Ok(format!("const {}", constant(m, c)?)),
    }
}

fn operands(m: &MirModule, ops: &[Operand], prefix: &str) -> Result<String, Ice> {
    Ok(ops
        .iter()
        .map(|op| Ok(format!("{prefix}{}", operand(m, op)?)))
        .collect::<Result<Vec<_>, Ice>>()?
        .join(", "))
}

fn callee(m: &MirModule, c: &Callee) -> Result<String, Ice> {
    Ok(match c {
        Callee::Direct(d) => def(m, *d)?,
        Callee::Code(key) => format!("<{}>", key_text(m, *key)?),
        Callee::Indirect(l) => local(*l),
    })
}

fn layout(m: &MirModule, l: LayoutRef) -> Result<String, Ice> {
    Ok(match l {
        LayoutRef::Builtin(b) => match b {
            BuiltinLayout::String => "string",
            BuiltinLayout::Bytes => "bytes",
            BuiltinLayout::BoxedInt => "boxed_int",
            BuiltinLayout::BoxedFloat => "boxed_float",
            BuiltinLayout::BoxedF32 => "boxed_f32",
            BuiltinLayout::BoxedU64 => "boxed_u64",
        }
        .to_owned(),
        LayoutRef::Ctor(d) => format!("ctor {}", def(m, d)?),
        LayoutRef::Tuple(n) => format!("tuple/{n}"),
        LayoutRef::Closure(n) => format!("closure/{n}"),
        LayoutRef::Dict { fns, embedded } => format!("dict/{fns}+{embedded}"),
        LayoutRef::Dyn => "dyn".to_owned(),
        LayoutRef::Desc { args } => format!("desc/{args}"),
        LayoutRef::DescNominal => "desc_nominal".to_owned(),
    })
}

fn rvalue(m: &MirModule, value: &Rvalue) -> Result<String, Ice> {
    Ok(match value {
        Rvalue::Use(op) => operand(m, op)?,
        Rvalue::Call { callee: c, args } => {
            format!("call {}({})", callee(m, c)?, operands(m, args, "")?)
        }
        Rvalue::CallForeign { symbol, sig, args } => format!(
            "call_foreign \"{}\"({}) -> {}",
            symbol.escape_debug(),
            operands(m, args, "borrow ")?,
            sig.ret
        ),
        Rvalue::CallRuntime { func, args } => {
            let params = func.params();
            let args = args
                .iter()
                .enumerate()
                .map(|(n, op)| {
                    let borrowed = matches!(params.get(n), Some((_, UseKind::Borrowed)));
                    let prefix = if borrowed { "borrow " } else { "" };
                    Ok(format!("{prefix}{}", operand(m, op)?))
                })
                .collect::<Result<Vec<_>, Ice>>()?
                .join(", ");
            format!("call_runtime {}({args})", func.name())
        }
        Rvalue::Alloc { layout: l, fields } => {
            format!("alloc {}({})", layout(m, *l)?, operands(m, fields, "")?)
        }
        Rvalue::Field {
            base,
            layout: l,
            field,
        } => format!("field {}.{field} of {}", local(*base), layout(m, *l)?),
        Rvalue::ReadLayout { base } => format!("read_layout {}", local(*base)),
        Rvalue::LoadConst(d) => format!("load_const {}", def(m, *d)?),
        Rvalue::Box { value, from } => format!("box {from} {}", operand(m, value)?),
        Rvalue::Unbox { value, to } => format!("unbox {to} {}", operand(m, value)?),
        Rvalue::Checked {
            op,
            lhs,
            rhs,
            repr,
            loc,
        } => format!(
            "checked {} {repr} {}, {} at str#{}",
            op.name(),
            operand(m, lhs)?,
            operand(m, rhs)?,
            loc.as_u32()
        ),
        Rvalue::Compare { op, lhs, rhs, repr } => format!(
            "compare {} {repr} {}, {}",
            op.name(),
            operand(m, lhs)?,
            operand(m, rhs)?
        ),
        Rvalue::FloatArith { op, lhs, rhs, repr } => format!(
            "float_arith {} {repr} {}, {}",
            op.name(),
            operand(m, lhs)?,
            operand(m, rhs)?
        ),
        Rvalue::Bitwise { op, lhs, rhs, repr } => format!(
            "bitwise {} {repr} {}, {}",
            op.name(),
            operand(m, lhs)?,
            operand(m, rhs)?
        ),
        Rvalue::FloatNeg { value, repr } => format!("float_neg {repr} {}", operand(m, value)?),
        Rvalue::Not(value) => format!("not {}", operand(m, value)?),
        Rvalue::ReadTag { base, ty } => {
            let ty = match ty {
                TagType::Option => "option".to_owned(),
                TagType::Result => "result".to_owned(),
                TagType::Order => "order".to_owned(),
                TagType::Adt(d) => def(m, *d)?,
            };
            format!("read_tag {} of {ty}", operand(m, base)?)
        }
    })
}

fn terminator(m: &MirModule, term: &Terminator) -> Result<String, Ice> {
    Ok(match term {
        Terminator::Return(op) => format!("return {}", operand(m, op)?),
        Terminator::TailCall { callee: c, args } => {
            format!("tail_call {}({})", callee(m, c)?, operands(m, args, "")?)
        }
        Terminator::Goto { target, args } => {
            format!("goto bb{}({})", target.as_u32(), operands(m, args, "")?)
        }
        Terminator::Branch { cond, then_, else_ } => format!(
            "branch {}, bb{}, bb{}",
            operand(m, cond)?,
            then_.as_u32(),
            else_.as_u32()
        ),
        Terminator::Switch { on, cases, default } => {
            let mut arms: Vec<String> = cases
                .iter()
                .map(|(v, b)| format!("{v} => bb{}", b.as_u32()))
                .collect();
            if let Some(b) = default {
                arms.push(format!("_ => bb{}", b.as_u32()));
            }
            format!("switch {} [{}]", operand(m, on)?, arms.join(", "))
        }
        Terminator::Panic { kind, message, loc } => {
            let msg = match message {
                Some(op) => format!(" {}", operand(m, op)?),
                None => String::new(),
            };
            format!("panic {}{msg} at str#{}", kind.name(), loc.as_u32())
        }
        Terminator::Unreachable => "unreachable".to_owned(),
    })
}
