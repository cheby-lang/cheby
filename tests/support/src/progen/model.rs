//! The generator's types and declarations: what a generated program can
//! name.

/// A type the generator builds values of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ty {
    Int,
    Bool,
    Str,
    List(Box<Ty>),
    /// A record of the main module, by index.
    Rec(usize),
    /// A sum type of the main module, by index.
    Adt(usize),
    /// `Map<String, Int>`.
    Map,
    /// `Option<Int>`.
    Opt,
    /// A closure held in a local: one parameter and a result. Never a
    /// parameter, a field or a printed value.
    Fun(Box<Ty>, Box<Ty>),
}

impl Ty {
    pub(crate) fn list(item: Self) -> Self {
        Self::List(Box::new(item))
    }

    /// Whether `"{x}"` may interpolate a value of this type (it satisfies
    /// `Show`); other printable types use `{x:?}`.
    pub(crate) fn shows(&self) -> bool {
        match self {
            Self::Int | Self::Bool | Self::Str => true,
            Self::List(item) => matches!(**item, Self::Int | Self::Bool | Self::Str),
            _ => false,
        }
    }

    /// Whether `{x:?}` prints a value of this type deterministically. A
    /// map's order depends on the process's hash seed, so maps never
    /// print.
    pub(crate) fn debug_prints(&self, types: &Types) -> bool {
        match self {
            Self::Map | Self::Fun(..) => false,
            Self::List(item) => item.debug_prints(types),
            Self::Rec(index) => types
                .recs
                .get(*index)
                .is_some_and(|rec| rec.fields.iter().all(|(_, ty)| ty.debug_prints(types))),
            _ => true,
        }
    }

    /// Whether `==` may compare two values of this type (no functions and
    /// no maps, whose equality the generator does not rely on).
    pub(crate) fn comparable(&self) -> bool {
        match self {
            Self::Map | Self::Fun(..) => false,
            Self::List(item) => item.comparable(),
            _ => true,
        }
    }

    pub(crate) fn render(&self, types: &Types) -> String {
        match self {
            Self::Int => "Int".to_owned(),
            Self::Bool => "Bool".to_owned(),
            Self::Str => "String".to_owned(),
            Self::List(item) => format!("List<{}>", item.render(types)),
            Self::Rec(index) => types.rec_name(*index),
            Self::Adt(index) => types.adt_name(*index),
            Self::Map => "map::Map<String, Int>".to_owned(),
            Self::Opt => "Option<Int>".to_owned(),
            Self::Fun(param, result) => {
                format!("fn({}) -> {}", param.render(types), result.render(types))
            }
        }
    }
}

/// A record type: `type RecN { k0: T, … }`.
#[derive(Debug, Clone)]
pub(crate) struct RecDef {
    pub(crate) fields: Vec<(String, Ty)>,
}

/// The fields of one variant of a sum type.
#[derive(Debug, Clone)]
pub(crate) enum VariantKind {
    Unit,
    Positional(Vec<Ty>),
    Named(Vec<(String, Ty)>),
}

/// A sum type: `type SumN { SNA, SNB(T, …), SNC { k0: T, … } }`. Its
/// first variant never refers to the type itself, so a value can always
/// be built without recursion.
#[derive(Debug, Clone)]
pub(crate) struct AdtDef {
    pub(crate) variants: Vec<VariantKind>,
}

/// The main module's record and sum types.
#[derive(Debug, Clone, Default)]
pub(crate) struct Types {
    pub(crate) recs: Vec<RecDef>,
    pub(crate) adts: Vec<AdtDef>,
}

impl Types {
    pub(crate) fn rec_name(&self, index: usize) -> String {
        let _ = self;
        format!("Rec{index}")
    }

    pub(crate) fn adt_name(&self, index: usize) -> String {
        let _ = self;
        format!("Sum{index}")
    }

    pub(crate) fn variant_name(adt: usize, variant: usize) -> String {
        let letter = char::from(b'A'.saturating_add(u8::try_from(variant % 26).unwrap_or(0)));
        format!("S{adt}{letter}")
    }

    /// Whether `ty` mentions sum type `adt` (directly or through a list
    /// or a record), so building it may recurse.
    pub(crate) fn mentions(&self, ty: &Ty, adt: usize) -> bool {
        match ty {
            Ty::Adt(index) => *index == adt,
            Ty::List(item) => self.mentions(item, adt),
            Ty::Rec(index) => self
                .recs
                .get(*index)
                .is_some_and(|rec| rec.fields.iter().any(|(_, t)| self.mentions(t, adt))),
            _ => false,
        }
    }
}

/// How a function's body is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Statements and a result.
    Plain,
    /// A counter and an accumulator; the recursive call is a tail call.
    TailCounter,
    /// A counter; the recursive call is not a tail call.
    BodyCounter,
    /// Walks a list with `[x, ..rest]` in tail calls.
    ListWalk,
}

/// `priv`, package-visible (no keyword) or `pub`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vis {
    Priv,
    Package,
    Pub,
}

impl Vis {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Priv => "priv ",
            Self::Package => "",
            Self::Pub => "pub ",
        }
    }
}

/// A top-level function, of the main module or of the helper module.
#[derive(Debug, Clone)]
pub(crate) struct FnDef {
    /// `Some("helpers")` for a function of the helper module.
    pub(crate) module: Option<&'static str>,
    pub(crate) name: String,
    pub(crate) vis: Vis,
    pub(crate) shape: Shape,
    pub(crate) params: Vec<(String, Ty)>,
    pub(crate) ret: Ty,
    /// The index in `params` of the counter (`TailCounter`,
    /// `BodyCounter`), whose argument is always a small literal.
    pub(crate) counter: Option<usize>,
    /// An upper estimate of the work one call does, in body evaluations.
    pub(crate) cost: u64,
    /// How many places call it or name it.
    pub(crate) uses: usize,
}
