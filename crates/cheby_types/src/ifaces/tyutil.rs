//! Small operations on closed types for satisfaction: substitution of type
//! parameters, one-way matching and the parameters a type mentions.

use cheby_iface::{Ice, ParamIdx, Ty, TyInterner, TyKind};

/// `ty` with every `Param(i)` replaced by `args[i]`, in one pass: the
/// replacements are not substituted again. A parameter without a
/// replacement is an [`Ice`].
pub(crate) fn subst(types: &TyInterner, ty: Ty, args: &[Ty]) -> Result<Ty, Ice> {
    let kind = match types.kind(ty) {
        TyKind::Error | TyKind::Never => return Ok(ty),
        TyKind::Param(param) => {
            return usize::try_from(param.as_u32())
                .ok()
                .and_then(|n| args.get(n))
                .copied()
                .ok_or_else(|| Ice::new(format!("no type argument for {param:?}")));
        }
        TyKind::Named { def, args: parts } => TyKind::Named {
            def: *def,
            args: subst_all(types, parts, args)?,
        },
        TyKind::Tuple(parts) => TyKind::Tuple(subst_all(types, parts, args)?),
        TyKind::Fn { params, ret } => TyKind::Fn {
            params: subst_all(types, params, args)?,
            ret: subst(types, *ret, args)?,
        },
        TyKind::Dyn { iface, args: parts } => TyKind::Dyn {
            iface: *iface,
            args: subst_all(types, parts, args)?,
        },
    };
    Ok(types.intern(kind))
}

fn subst_all(types: &TyInterner, tys: &[Ty], args: &[Ty]) -> Result<Box<[Ty]>, Ice> {
    tys.iter().map(|&t| subst(types, t, args)).collect()
}

/// One-way matching: binds the type parameters of `pattern`, which index
/// `bound`, so that `pattern` becomes `target`. `target`'s own parameters
/// are rigid. Returns whether it matched; `bound` may be partly filled
/// when it did not.
pub(super) fn match_ty(
    types: &TyInterner,
    pattern: Ty,
    target: Ty,
    bound: &mut [Option<Ty>],
) -> Result<bool, Ice> {
    match (types.kind(pattern), types.kind(target)) {
        (TyKind::Param(param), _) => {
            let slot = usize::try_from(param.as_u32())
                .ok()
                .and_then(|n| bound.get_mut(n))
                .ok_or_else(|| Ice::new(format!("type parameter {param:?} out of range")))?;
            if let Some(prev) = slot {
                return Ok(*prev == target);
            }
            *slot = Some(target);
            Ok(true)
        }
        (TyKind::Named { def: a, args: xs }, TyKind::Named { def: b, args: ys }) if a == b => {
            match_all(types, xs, ys, bound)
        }
        (TyKind::Dyn { iface: a, args: xs }, TyKind::Dyn { iface: b, args: ys }) if a == b => {
            match_all(types, xs, ys, bound)
        }
        (TyKind::Tuple(xs), TyKind::Tuple(ys)) => match_all(types, xs, ys, bound),
        (
            TyKind::Fn {
                params: xs,
                ret: rx,
            },
            TyKind::Fn {
                params: ys,
                ret: ry,
            },
        ) => Ok(match_all(types, xs, ys, bound)? && match_ty(types, *rx, *ry, bound)?),
        _ => Ok(pattern == target),
    }
}

fn match_all(
    types: &TyInterner,
    xs: &[Ty],
    ys: &[Ty],
    bound: &mut [Option<Ty>],
) -> Result<bool, Ice> {
    if xs.len() != ys.len() {
        return Ok(false);
    }
    for (&x, &y) in xs.iter().zip(ys) {
        if !match_ty(types, x, y, bound)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `ty` mentions the type parameter `param`.
pub(super) fn mentions(types: &TyInterner, ty: Ty, param: ParamIdx) -> bool {
    let mut found = false;
    walk_params(types, ty, &mut |p| found |= p == param);
    found
}

/// The type parameters `ty` mentions, in first-occurrence order, without
/// duplicates.
pub(super) fn params_of(types: &TyInterner, ty: Ty) -> Vec<ParamIdx> {
    let mut out = Vec::new();
    walk_params(types, ty, &mut |p| {
        if !out.contains(&p) {
            out.push(p);
        }
    });
    out
}

fn walk_params(types: &TyInterner, ty: Ty, f: &mut dyn FnMut(ParamIdx)) {
    match types.kind(ty) {
        TyKind::Error | TyKind::Never => {}
        TyKind::Param(param) => f(*param),
        TyKind::Named { args: parts, .. }
        | TyKind::Tuple(parts)
        | TyKind::Dyn { args: parts, .. } => {
            for &part in parts {
                walk_params(types, part, f);
            }
        }
        TyKind::Fn { params, ret } => {
            for &part in params {
                walk_params(types, part, f);
            }
            walk_params(types, *ret, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use cheby_iface::{Globals, builtin};

    use super::*;

    #[test]
    fn substitution_matching_and_parameters() {
        let g = Globals::new();
        let t = &g.types;
        let p0 = t.param(ParamIdx::from_u32(0));
        let p1 = t.param(ParamIdx::from_u32(1));
        let list = |x| t.named(builtin::LIST, [x]);
        let pair = t.intern(TyKind::Tuple(Box::new([p0, list(p1)])));
        // One pass: the replacement of `p0` mentions `p1` and stays.
        let replaced = subst(t, pair, &[p1, Ty::INT]).unwrap();
        let expected = t.intern(TyKind::Tuple(Box::new([p1, list(Ty::INT)])));
        assert_eq!(replaced, expected);
        assert!(subst(t, p1, &[Ty::INT]).is_err());

        let mut bound = [None, None];
        let target = t.intern(TyKind::Tuple(Box::new([Ty::STRING, list(Ty::BOOL)])));
        assert!(match_ty(t, pair, target, &mut bound).unwrap());
        assert_eq!(bound, [Some(Ty::STRING), Some(Ty::BOOL)]);
        // A parameter bound twice must agree.
        let twice = t.func([p0, p0], Ty::NIL);
        let mut bound = [None];
        assert!(!match_ty(t, twice, t.func([Ty::INT, Ty::STRING], Ty::NIL), &mut bound).unwrap());
        let mut bound = [None];
        assert!(match_ty(t, twice, t.func([Ty::INT, Ty::INT], Ty::NIL), &mut bound).unwrap());
        // The target's parameters are rigid.
        let mut bound = [None];
        assert!(!match_ty(t, list(Ty::INT), list(p0), &mut bound).unwrap());

        assert!(mentions(t, pair, ParamIdx::from_u32(1)));
        assert!(!mentions(t, list(Ty::INT), ParamIdx::from_u32(0)));
        assert_eq!(
            params_of(t, t.func([list(p1), p0], p1)),
            [ParamIdx::from_u32(1), ParamIdx::from_u32(0)]
        );
    }
}
