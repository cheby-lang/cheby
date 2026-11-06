//! Runtime layout ids of symbolic MIR layouts (D-454).

use cheby_iface::{DefId, Ice, builtin};
use cheby_mir::{BuiltinLayout, LayoutRef};
use cheby_runtime::abi::layout;

/// Maps symbolic layouts to runtime layout ids. Read-only during
/// translation. Code generation asks it for no user constructor: their
/// ids are assigned only when a module record is registered (D-919,
/// D-920), and code loads them from layout words, so every host passes
/// [`BuiltinLayouts`]. A missing id is an [`Ice`].
pub trait LayoutIds: Sync {
    /// The runtime layout id of `layout`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `layout` was not registered.
    fn id(&self, layout: &LayoutRef) -> Result<u32, Ice>;
}

/// The fixed id of a runtime kind or builtin constructor (D-454), or
/// `None` for user constructors.
#[must_use]
pub fn builtin_layout_id(layout_ref: &LayoutRef) -> Option<u32> {
    match *layout_ref {
        LayoutRef::Builtin(b) => Some(match b {
            BuiltinLayout::String => layout::STRING,
            BuiltinLayout::Bytes => layout::BYTES,
            BuiltinLayout::BoxedInt => layout::BOXED_INT,
            BuiltinLayout::BoxedFloat => layout::BOXED_FLOAT,
            BuiltinLayout::BoxedF32 => layout::BOXED_F32,
            BuiltinLayout::BoxedU64 => layout::BOXED_U64,
        }),
        LayoutRef::Tuple(_) => Some(layout::TUPLE),
        LayoutRef::Closure(_) => Some(layout::CLOSURE),
        LayoutRef::Dict { .. } => Some(layout::DICT),
        LayoutRef::Dyn => Some(layout::DYN),
        LayoutRef::Desc { .. } => Some(layout::DESC),
        LayoutRef::DescNominal => Some(layout::DESC_NOMINAL),
        LayoutRef::Ctor(def) => builtin_ctor_layout(def),
    }
}

/// The fixed layout id of a builtin constructor, `None` for a user one.
#[must_use]
pub fn builtin_ctor_layout(def: DefId) -> Option<u32> {
    let table = [
        (builtin::NIL, layout::NIL),
        (builtin::FALSE, layout::FALSE),
        (builtin::TRUE, layout::TRUE),
        (builtin::LESS, layout::LESS),
        (builtin::EQUAL, layout::EQUAL),
        (builtin::GREATER, layout::GREATER),
        (builtin::SOME, layout::SOME),
        (builtin::NONE, layout::NONE),
        (builtin::OK, layout::OK),
        (builtin::ERR, layout::ERR),
    ];
    table.iter().find(|(d, _)| *d == def).map(|&(_, id)| id)
}

/// The builtin layouts alone, for units that declare no constructors.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinLayouts;

impl LayoutIds for BuiltinLayouts {
    fn id(&self, layout_ref: &LayoutRef) -> Result<u32, Ice> {
        builtin_layout_id(layout_ref)
            .ok_or_else(|| Ice::new(format!("layout {layout_ref:?} was not registered")))
    }
}
