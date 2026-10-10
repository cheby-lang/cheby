# MIR goldens: `data`

MIR dumps that pin the lowering of `crates/cheby_mir/src/lower/data.rs`, owned by W2-D
(plan §4.2 of the M1 back-end plan). Each golden is `<name>.mir`, written by a test
with `test_support::lower_source` and re-blessed with `CHEBY_BLESS=1`. Read every
golden before keeping it.
