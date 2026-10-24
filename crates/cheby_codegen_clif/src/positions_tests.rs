//! Source positions in code generation (D-938): `set_srcloc` from each
//! statement's position, the rows of each function, and positions in the
//! code record (version 2). Positions change no machine code, no
//! relocation and no CLIF dump (D-1171).

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "test arithmetic on small, known values"
)]

use cheby_iface::ModuleId;
use cheby_mir::{MirModule, Pos, PosRef, Positions, UnitKind};

use crate::record::{CodeRecord, RECORD_VERSION, RecordError, RecordingModule};
use crate::record_tests::{code, code_model_unit, located_unit, named, record};
use crate::{CodegenOptions, SourceRow, UnitCx, compile_unit, isa};

/// `unit` with positions: statement `i` of block `b` of function `n` at
/// line `10 n + b + 1`, column `i + 1`, the terminator after the
/// statements, each function's entry at line `100 + n`, column 4, and
/// the unit's source `src/main.cheby`.
fn with_positions(mut unit: MirModule) -> MirModule {
    unit.source = Some("src/main.cheby".into());
    for (n, f) in unit.functions.iter_mut().enumerate() {
        let mut table = Vec::new();
        for (b, block) in f.blocks.iter_mut().enumerate() {
            block.pos = (0..=block.stmts.len())
                .map(|i| {
                    table.push(Pos {
                        line: (10 * n + b + 1) as u32,
                        column: (i + 1) as u32,
                    });
                    PosRef::from_bits(table.len() as u32)
                })
                .collect();
        }
        table.push(Pos {
            line: (100 + n) as u32,
            column: 4,
        });
        let entry = PosRef::from_bits(table.len() as u32);
        f.positions = Positions { table, entry };
    }
    unit
}

/// The units the tests run on: every kind of code without positions.
fn units() -> [MirModule; 3] {
    [
        code_model_unit(),
        located_unit(code()),
        located_unit(UnitKind::Tests(ModuleId::from_u32(1))),
    ]
}

/// The CLIF text of each function of `unit` compiled alone, and what it
/// defined.
fn compiled(unit: &MirModule) -> (Vec<String>, Vec<crate::DefinedCode>) {
    let mut module = RecordingModule::new(isa::host_isa().unwrap());
    let code = compile_unit(
        &mut module,
        unit,
        &UnitCx {
            named_outside: &named,
        },
        &CodegenOptions {
            capture_clif: true,
            ..CodegenOptions::default()
        },
    )
    .unwrap();
    (code.clif, code.defined)
}

/// The machine code is the same with positions on and off: every
/// function's bytes and relocations, every symbol and data object; only
/// the rows and the source differ.
#[test]
fn positions_change_no_machine_code() {
    for unit in units() {
        let plain = record(&unit);
        let positioned = record(&with_positions(unit.clone()));
        assert_eq!(plain.symbols, positioned.symbols, "{}", unit.path);
        assert_eq!(plain.data, positioned.data, "{}", unit.path);
        assert_eq!(plain.functions.len(), positioned.functions.len());
        for (a, b) in plain.functions.iter().zip(&positioned.functions) {
            assert_eq!(a.symbol, b.symbol);
            assert_eq!(a.bytes, b.bytes, "{}", unit.path);
            assert_eq!(a.relocs, b.relocs, "{}", unit.path);
            assert_eq!(a.positions.len(), 0);
        }
        assert!(plain.source.is_none());
        assert_eq!(positioned.source.as_deref(), Some("src/main.cheby"));
        assert!(
            positioned.functions.iter().any(|f| !f.positions.is_empty()),
            "{}",
            unit.path
        );
    }
}

/// `Decls::describe` prints no source location (D-869's debug tags and
/// D-938's locations are metadata): a function's CLIF text is the same
/// with positions on and off (D-1171).
#[test]
fn positions_dump_unchanged() {
    for unit in units() {
        let (plain, _) = compiled(&unit);
        let (positioned, defined) = compiled(&with_positions(unit.clone()));
        assert_eq!(plain, positioned, "{}", unit.path);
        assert!(plain.iter().all(|text| !text.contains(" @")), "{plain:#?}");
        assert!(defined.iter().any(|d| !d.positions.is_empty()));
    }
}

/// A function's rows (D-938): sorted by strictly increasing offset inside
/// its code, each a position of the function's table, the first at the
/// entry's position (the prologue and the stack check), and no two
/// consecutive rows at one line and column.
#[test]
fn positions_rows_follow_the_statements() {
    let unit = with_positions(located_unit(code()));
    let (_, defined) = compiled(&unit);
    let lines: Vec<u32> = unit
        .functions
        .iter()
        .flat_map(|f| f.positions.table.iter().map(|p| p.line))
        .collect();
    let mut with_rows = 0;
    for d in &defined {
        if d.positions.is_empty() {
            continue;
        }
        with_rows += 1;
        assert!(
            d.positions.windows(2).all(|w| w[0].offset < w[1].offset),
            "{:?}",
            d.positions
        );
        assert!(d.positions.iter().all(|r| r.offset < d.len));
        assert!(d.positions.iter().all(|r| lines.contains(&r.line)));
        assert!(
            d.positions
                .windows(2)
                .all(|w| (w[0].line, w[0].column) != (w[1].line, w[1].column))
        );
        // The entry (prologue, stack check) first: line 100 or 101.
        assert!(
            d.positions[0].line >= 100 && d.positions[0].column == 4,
            "{:?}",
            d.positions
        );
    }
    assert!(with_rows >= 2, "{defined:?}");
}

/// A record with positions survives its encoding both ways, names its
/// source once, and keeps each function's rows.
#[test]
fn record_positions_round_trip() {
    for unit in units() {
        let r = record(&with_positions(unit));
        assert_eq!(r.source.as_deref(), Some("src/main.cheby"));
        let bytes = r.encode();
        let decoded = CodeRecord::decode(&bytes).unwrap();
        assert_eq!(decoded, r);
        assert_eq!(decoded.encode(), bytes);
        // The path once, right after the version: 1, its length, its bytes.
        let path = b"src/main.cheby";
        assert_eq!(bytes[8], 1);
        assert_eq!(bytes[9..13], (path.len() as u32).to_le_bytes());
        assert_eq!(&bytes[13..13 + path.len()], path);
    }
}

/// One unit with positions compiled twice gives the same record bytes
/// (D-264): the rows depend only on the unit.
#[test]
fn record_positions_same_unit_twice_same_bytes() {
    let unit = with_positions(code_model_unit());
    assert_eq!(record(&unit).encode(), record(&unit).encode());
}

/// A record of version 1, which had no positions, is refused like any
/// other version, and the version is 2 (D-938).
#[test]
fn record_positions_old_version_refused() {
    assert_eq!(RECORD_VERSION, 2);
    let mut bytes = record(&with_positions(code_model_unit())).encode();
    bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
    assert_eq!(CodeRecord::decode(&bytes), Err(RecordError::Version(1)));
}

/// What a record refuses about positions: an absolute or otherwise not
/// diagnostic source path (D-421), rows without a source, rows out of
/// order, outside their function or at line or column 0, and numbers not
/// in their shortest LEB128 form.
#[test]
fn record_positions_checked() {
    let good = record(&with_positions(code_model_unit()));
    assert_eq!(good.check(), Ok(()));
    for source in [
        "/abs/src/main.cheby",
        "C:/src/main.cheby",
        "src\\main.cheby",
        "../main.cheby",
        "src//main.cheby",
        "./src/main.cheby",
        "",
    ] {
        let mut bad = good.clone();
        bad.source = Some(source.to_owned());
        assert!(
            matches!(bad.check(), Err(RecordError::Malformed(_))),
            "{source}"
        );
        assert!(CodeRecord::decode(&bad.encode()).is_err(), "{source}");
    }
    let k = good
        .functions
        .iter()
        .position(|f| f.positions.len() >= 2)
        .unwrap();
    let mut no_source = good.clone();
    no_source.source = None;
    assert_eq!(
        no_source.check(),
        Err(RecordError::Malformed("positions without a source"))
    );
    let mut swapped = good.clone();
    swapped.functions[k].positions.swap(0, 1);
    assert!(swapped.check().is_err());
    let mut outside = good.clone();
    let len = outside.functions[k].bytes.len() as u32;
    outside.functions[k].positions.push(SourceRow {
        offset: len,
        line: 1,
        column: 1,
    });
    assert!(outside.check().is_err());
    let mut zero = good.clone();
    zero.functions[k].positions[0].line = 0;
    assert!(zero.check().is_err());
    assert!(CodeRecord::decode(&zero.encode()).is_err());
    // A row's column 1 written as two bytes, `0x81 0x00`, is refused.
    let mut one = good.clone();
    for f in &mut one.functions {
        f.positions.clear();
    }
    one.functions[0].positions = vec![SourceRow {
        offset: 0,
        line: 1,
        column: 1,
    }];
    let bytes = one.encode();
    let short = [0, 0, 1];
    let at = bytes
        .windows(short.len() + 4)
        .position(|w| w[..4] == 1_u32.to_le_bytes() && w[4..] == short)
        .unwrap()
        + 4;
    let mut long = bytes[..at + 2].to_vec();
    long.extend_from_slice(&[0x81, 0x00]);
    long.extend_from_slice(&bytes[at + 3..]);
    assert_eq!(CodeRecord::decode(&bytes), Ok(one));
    assert!(CodeRecord::decode(&long).is_err());
}
