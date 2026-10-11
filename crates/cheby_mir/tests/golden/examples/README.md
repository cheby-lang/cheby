# MIR goldens: `examples`

The MIR that `cheby run` compiles for the examples 003, 024 and 025 (D2: the dumps of the
examples are reviewed and pinned), written by `golden_tests::example_goldens` with
`test_support::lower_package` and re-blessed with `CHEBY_BLESS=1`. Each golden holds the
units of the example's own modules and the entry unit, after reference counting and
validation; the units of `std` modules are left out, so a change to `std` does not move
them. Read every golden before keeping it.
