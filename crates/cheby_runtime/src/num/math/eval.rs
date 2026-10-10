//! The evaluation of D-396 (plan §14 W1-D): a candidate implementation of
//! the seven functions against the references of [`super::reference`].
//!
//! Inputs per function: the special values (zeros, infinities, NaN,
//! subnormals, the extremes, huge arguments of sin, cos and tan, and their
//! neighbours; for `pow` and `atan2` their pairs), 10,000 random inputs
//! (half uniform over every binary64 bit pattern, half over the function's
//! interesting range), and CORE-MATH's published hard cases sampled in
//! `testdata/`. A result must equal the reference bit for bit (any NaN
//! matches any NaN; the sign of a zero counts).
//!
//! Run it with
//!
//! ```text
//! cargo test -p cheby_runtime --release num::math::eval -- --ignored --nocapture
//! ```
//!
//! (`CHEBY_MATH_EVAL_ALL=1` also lists every failure as bits.) The
//! candidate is the runtime's own implementation (D-568); the test
//! requires the verdict to equal `num::math::VERIFIED`. (The first
//! candidate, `pxfm` 0.1.30, failed on every function, D-553.)

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "test input generation with plain arithmetic"
)]

use std::fmt::Write as _;

use astro_float::Consts;

use super::VERIFIED;
use super::reference::{self, Ref, Trig, hex_float};

/// The seven functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Pow,
    Exp,
    Ln,
    Sin,
    Cos,
    Tan,
    Atan2,
}

const FUNCS: [Func; 7] = [
    Func::Pow,
    Func::Exp,
    Func::Ln,
    Func::Sin,
    Func::Cos,
    Func::Tan,
    Func::Atan2,
];

impl Func {
    fn name(self) -> &'static str {
        match self {
            Self::Pow => "pow",
            Self::Exp => "exp",
            Self::Ln => "ln",
            Self::Sin => "sin",
            Self::Cos => "cos",
            Self::Tan => "tan",
            Self::Atan2 => "atan2",
        }
    }

    fn binary(self) -> bool {
        matches!(self, Self::Pow | Self::Atan2)
    }

    /// The CORE-MATH hard cases sampled for this function.
    fn hard_cases(self) -> &'static str {
        match self {
            Self::Pow => include_str!("testdata/pow.wc"),
            Self::Exp => include_str!("testdata/exp.wc"),
            Self::Ln => include_str!("testdata/ln.wc"),
            Self::Sin => include_str!("testdata/sin.wc"),
            Self::Cos => include_str!("testdata/cos.wc"),
            Self::Tan => include_str!("testdata/tan.wc"),
            Self::Atan2 => include_str!("testdata/atan2.wc"),
        }
    }

    fn reference(self, a: f64, b: f64, cc: &mut Consts) -> Ref {
        match self {
            Self::Pow => reference::pow(a, b, cc),
            Self::Exp => reference::exp(a, cc),
            Self::Ln => reference::ln(a, cc),
            Self::Sin => reference::trig(Trig::Sin, a, cc),
            Self::Cos => reference::trig(Trig::Cos, a, cc),
            Self::Tan => reference::trig(Trig::Tan, a, cc),
            Self::Atan2 => reference::atan2(a, b, cc),
        }
    }

    /// A random input over the function's interesting range.
    fn ranged(self, rng: &mut Rng) -> (f64, f64) {
        match self {
            Self::Pow => (
                2_f64.powf(rng.uniform(-20.0, 20.0)),
                rng.uniform(-50.0, 50.0),
            ),
            Self::Exp => (rng.uniform(-746.0, 710.0), 0.0),
            Self::Ln => (f64::from_bits(rng.next() >> 1), 0.0),
            Self::Sin | Self::Cos | Self::Tan => {
                let x = if rng.next() & 1 == 0 {
                    rng.uniform(-10.0, 10.0)
                } else {
                    10_f64.powf(rng.uniform(-8.0, 22.0))
                };
                (if rng.next() & 1 == 0 { x } else { -x }, 0.0)
            }
            Self::Atan2 => (rng.uniform(-10.0, 10.0), rng.uniform(-10.0, 10.0)),
        }
    }
}

/// The implementation under evaluation: one function per [`Func`], the
/// runtime's own (D-568).
fn candidate(func: Func, a: f64, b: f64) -> f64 {
    match func {
        Func::Pow => super::pow(a, b),
        Func::Exp => super::exp(a),
        Func::Ln => super::ln(a),
        Func::Sin => super::trig(super::Trig::Sin, a),
        Func::Cos => super::trig(super::Trig::Cos, a),
        Func::Tan => super::trig(super::Trig::Tan, a),
        Func::Atan2 => super::atan2(a, b),
    }
}

/// splitmix64, seeded, so every run tests the same inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn uniform(&mut self, low: f64, high: f64) -> f64 {
        let unit = (self.next() >> 11) as f64 / (1_u64 << 53) as f64;
        low + (high - low) * unit
    }

    fn bits(&mut self) -> f64 {
        f64::from_bits(self.next())
    }
}

/// The special values and their neighbours.
fn specials() -> Vec<f64> {
    let mut values = vec![
        0.0,
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        1.0,
        -1.0,
        0.5,
        2.0,
        -2.0,
        3.0,
        10.0,
        0.1,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::from_bits(0x000f_ffff_ffff_ffff),
        f64::from_bits(0x0008_0000_0000_0000),
        1.0e-300,
        1.0e-310,
        1.0e10,
        1.0e15,
        1.0e16,
        1.0e17,
        1.0e18,
        1.0e19,
        1.0e20,
        1.0e21,
        1.0e22,
        -1.0e22,
        1.0e100,
        1.0e300,
        -1.0e300,
        2_f64.powi(1023),
        // The binary64 closest to a multiple of π/2 (6381956970095103 ×
        // 2^797, Kahan and McDonald), the hardest argument reduction.
        6_381_956_970_095_103.0 * 2_f64.powi(797),
        -6_381_956_970_095_103.0 * 2_f64.powi(797),
        std::f64::consts::PI,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_4,
        709.782_712_893_384,
        -745.133_219_101_941_1,
        -708.4,
    ];
    let neighbours: Vec<f64> = values
        .iter()
        .flat_map(|x| [x.next_up(), x.next_down()])
        .collect();
    values.extend(neighbours);
    values
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Special,
    Random,
    Hard,
}

fn inputs(func: Func, rng: &mut Rng) -> Vec<(Kind, f64, f64)> {
    let specials = specials();
    let mut inputs = Vec::new();
    if func.binary() {
        for &a in &specials {
            for &b in specials.iter().step_by(3) {
                inputs.push((Kind::Special, a, b));
            }
        }
    } else {
        inputs.extend(specials.iter().map(|&x| (Kind::Special, x, 0.0)));
    }
    for i in 0..10_000 {
        let (a, b) = if i % 2 == 0 {
            (rng.bits(), rng.bits())
        } else {
            func.ranged(rng)
        };
        inputs.push((Kind::Random, a, b));
    }
    for line in func.hard_cases().lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(',').map(hex_float);
        let a = parts
            .next()
            .flatten()
            .unwrap_or_else(|| panic!("bad hard case {line:?}"));
        let b = match parts.next() {
            Some(b) => b.unwrap_or_else(|| panic!("bad hard case {line:?}")),
            None => 0.0,
        };
        inputs.push((Kind::Hard, a, b));
    }
    inputs
}

/// The outcome of one function.
#[derive(Debug, Default)]
struct Outcome {
    passed: usize,
    failed: Vec<(Kind, f64, f64, f64, f64)>,
    dropped: Vec<(Kind, f64, f64)>,
}

fn same(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

fn evaluate(func: Func, cc: &mut Consts) -> Outcome {
    let mut rng = Rng(0x00c0_ffee_0396 ^ (func as u64));
    let mut outcome = Outcome::default();
    for (kind, a, b) in inputs(func, &mut rng) {
        match func.reference(a, b, cc) {
            Ref::Dropped => outcome.dropped.push((kind, a, b)),
            Ref::Value(want) => {
                let got = candidate(func, a, b);
                if same(got, want) {
                    outcome.passed += 1;
                } else {
                    outcome.failed.push((kind, a, b, got, want));
                }
            }
        }
    }
    outcome
}

fn report(func: Func, outcome: &Outcome) -> String {
    let mut text = String::new();
    let count = |kind| outcome.failed.iter().filter(|f| f.0 == kind).count();
    let _ = writeln!(
        text,
        "{}: {} passed, {} failed (special {}, random {}, hard {}), {} dropped",
        func.name(),
        outcome.passed,
        outcome.failed.len(),
        count(Kind::Special),
        count(Kind::Random),
        count(Kind::Hard),
        outcome.dropped.len(),
    );
    for (kind, a, b, got, want) in outcome.failed.iter().take(8) {
        let args = if func.binary() {
            format!(
                "{a:e} ({:#018x}), {b:e} ({:#018x})",
                a.to_bits(),
                b.to_bits()
            )
        } else {
            format!("{a:e} ({:#018x})", a.to_bits())
        };
        let _ = writeln!(
            text,
            "  {kind:?} {}({args}): got {got:e} ({:#018x}), want {want:e} ({:#018x})",
            func.name(),
            got.to_bits(),
            want.to_bits()
        );
    }
    // CHEBY_MATH_EVAL_ALL=1 lists every failure as hexadecimal bits, for
    // an independent check (such as mpmath at a few thousand bits).
    if std::env::var_os("CHEBY_MATH_EVAL_ALL").is_some() {
        for (kind, a, b, got, want) in &outcome.failed {
            let _ = writeln!(
                text,
                "FAIL {} {kind:?} {:016x} {:016x} {:016x} {:016x}",
                func.name(),
                a.to_bits(),
                b.to_bits(),
                got.to_bits(),
                want.to_bits()
            );
        }
    }
    for (kind, a, b) in &outcome.dropped {
        let _ = writeln!(
            text,
            "  dropped {kind:?} ({a:e}, {b:e}): reference undecided"
        );
    }
    text
}

#[test]
#[ignore = "D-396 evaluation, about 10 s in release: see the module docs"]
fn evaluation() {
    let mut cc = Consts::new().unwrap();
    let mut verified = true;
    for func in FUNCS {
        let outcome = evaluate(func, &mut cc);
        print!("{}", report(func, &outcome));
        // A dropped input is one the reference could not decide; it is
        // reported, not held against the candidate.
        verified &= outcome.failed.is_empty();
    }
    assert_eq!(
        verified, VERIFIED,
        "the evaluation's verdict differs from num::math::VERIFIED"
    );
}

#[test]
fn hard_case_files_parse() {
    let mut rng = Rng(1);
    for func in FUNCS {
        let hard = inputs(func, &mut rng)
            .iter()
            .filter(|(kind, ..)| *kind == Kind::Hard)
            .count();
        assert!(hard >= 1_000, "{}: {hard} hard cases", func.name());
    }
}
