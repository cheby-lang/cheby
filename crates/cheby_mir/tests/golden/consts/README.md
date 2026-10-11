# MIR goldens: `consts`

MIR dumps that pin the lowering of `crates/cheby_mir/src/lower/consts.rs`, owned by W3-D
(plan §4.2 of the M1 back-end plan). Each golden is `<name>.mir`, written by a test
with `test_support::lower_source` and re-blessed with `CHEBY_BLESS=1`. Read every
golden before keeping it.
