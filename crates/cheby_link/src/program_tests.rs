//! Program-object tests `link_program_*` (D-915, D-917, D-925, D-930):
//! the program object of a hand-built entries record, read back with the
//! `object` crate, and linked by `cc` against a stub archive.
//!
//! Mach-O is written on aarch64 hosts only: macOS is aarch64 only
//! (D-892), so on an x86-64 host these tests cover ELF alone.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test offsets on small, known values"
)]

use std::collections::BTreeMap;
use std::path::Path;

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationFlags, RelocationTarget, SectionKind, SymbolScope, elf, macho};

use cheby_codegen_clif::isa;
use cheby_codegen_clif::record::{
    CodeRecord, RecordFunction, RecordSymbol, SymbolKind, SymbolLinkage,
};
use cheby_runtime::abi;

use crate::LinkError;
use crate::program::{ProgramSpec, program_object};

/// The object formats written for the host architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Elf,
    MachO,
}

impl Format {
    /// ELF, and Mach-O on aarch64 hosts only (D-892).
    const ALL: &[Self] = if cfg!(target_arch = "aarch64") {
        &[Self::Elf, Self::MachO]
    } else {
        &[Self::Elf]
    };

    /// The host's own format, the one `cc` links.
    fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::MachO
        } else {
            Self::Elf
        }
    }

    fn isa(self) -> cranelift_codegen::isa::OwnedTargetIsa {
        let arch = std::env::consts::ARCH;
        let triple = match self {
            Self::Elf => format!("{arch}-unknown-linux-gnu"),
            Self::MachO => format!("{arch}-apple-darwin"),
        };
        isa::isa_for(&triple).unwrap()
    }

    /// A symbol's name as the object spells it.
    fn strip(self, name: &str) -> &str {
        match self {
            Self::Elf => name,
            Self::MachO => name.strip_prefix('_').unwrap_or(name),
        }
    }
}

const ENTRY: &str = "cb$entry$main$app";
const RECORDS: [&str; 2] = ["cb$mod$app", "cb$mod$std$io"];

/// A function that only returns, for the host architecture.
fn ret() -> Vec<u8> {
    if cfg!(target_arch = "aarch64") {
        vec![0xc0, 0x03, 0x5f, 0xd6]
    } else {
        vec![0xc3]
    }
}

/// An entries record by hand: `main`'s entry thunk and the trampolines of
/// `arities`, each a function that returns, all local.
fn entries(arities: &[u32]) -> CodeRecord {
    let mut names = vec![ENTRY.to_owned()];
    names.extend(arities.iter().map(|n| format!("cb$tramp${n}")));
    let symbols = names
        .iter()
        .map(|name| RecordSymbol {
            name: name.clone(),
            kind: SymbolKind::Function,
            linkage: SymbolLinkage::Local,
        })
        .collect();
    let functions = (0_u32..)
        .zip(&names)
        .map(|(symbol, _)| RecordFunction {
            symbol,
            align: 16,
            bytes: ret(),
            relocs: Vec::new(),
        })
        .collect();
    CodeRecord {
        symbols,
        functions,
        data: Vec::new(),
    }
}

fn records() -> Vec<String> {
    RECORDS.map(str::to_owned).to_vec()
}

fn spec(records: &[String]) -> ProgramSpec<'_> {
    ProgramSpec {
        module_records: records,
        entry: ENTRY,
        main_stack: 8 << 20,
        fiber_stack: 1 << 20,
        unwind_edges: true,
    }
}

fn program(format: Format) -> Vec<u8> {
    let records = records();
    program_object(format.isa(), &spec(&records), &entries(&[0, 2])).unwrap()
}

/// Every symbol of the object, by its name without Mach-O's underscore:
/// whether it is defined, and its scope.
fn symbols(format: Format, bytes: &[u8]) -> BTreeMap<String, (bool, SymbolScope)> {
    let file = ::object::File::parse(bytes).unwrap();
    file.symbols()
        .filter(|s| !s.name().unwrap_or("").is_empty() && s.kind() != ::object::SymbolKind::File)
        .filter(|s| s.kind() != ::object::SymbolKind::Section)
        .map(|s| {
            (
                format.strip(s.name().unwrap()).to_owned(),
                (s.is_definition(), s.scope()),
            )
        })
        .collect()
}

/// `main` is the only exported symbol; `cb$image`, the entry thunk, the
/// trampolines and the import words are local definitions; the module
/// records, `cheby_rt_main` and the runtime ID's symbol are imports.
#[test]
fn link_program_symbols_and_linkage() {
    for &format in Format::ALL {
        let bytes = program(format);
        let symbols = symbols(format, &bytes);
        assert_eq!(
            symbols.get("main"),
            Some(&(true, SymbolScope::Dynamic)),
            "{format:?}: {symbols:?}"
        );
        for (name, (defined, scope)) in &symbols {
            if *defined && name != "main" {
                assert_eq!(*scope, SymbolScope::Compilation, "{format:?}: {name}");
            }
        }
        let rt_id_word = format!("cb$imp${}", abi::sym::RUNTIME_ID_SYMBOL);
        for local in [
            "cb$image",
            ENTRY,
            "cb$tramp$0",
            "cb$tramp$2",
            "cb$imp$cheby_rt_main",
            rt_id_word.as_str(),
        ] {
            assert_eq!(
                symbols.get(local).map(|s| s.0),
                Some(true),
                "{format:?}: {local}"
            );
        }
        for import in RECORDS
            .iter()
            .copied()
            .chain(["cheby_rt_main", abi::sym::RUNTIME_ID_SYMBOL])
        {
            assert_eq!(
                symbols.get(import).map(|s| s.0),
                Some(false),
                "{format:?}: {import}"
            );
        }
        assert!(!symbols.contains_key("cb$tramp$1"), "{format:?}");
    }
}

/// The relocations of the section that holds `name`, at offsets from
/// `name`'s start, with their targets' names (a section for a target the
/// format names by section), and the symbol's bytes.
fn relocs_of(format: Format, bytes: &[u8], name: &str) -> (BTreeMap<u64, Option<String>>, Vec<u8>) {
    let file = ::object::File::parse(bytes).unwrap();
    let symbol = file
        .symbols()
        .find(|s| s.name().is_ok_and(|n| format.strip(n) == name))
        .unwrap();
    let section = file
        .section_by_index(symbol.section_index().unwrap())
        .unwrap();
    let start = symbol.address() - section.address();
    // The image's 104 bytes and its table of two module records.
    let size = 104 + 16;
    let data = section.data().unwrap();
    let mut relocs = BTreeMap::new();
    for (offset, reloc) in section.relocations() {
        if offset < start || offset >= start + size {
            continue;
        }
        let target = match reloc.target() {
            RelocationTarget::Symbol(index) => file
                .symbol_by_index(index)
                .ok()
                .and_then(|s| s.name().ok().map(|n| format.strip(n).to_owned()))
                .filter(|n| !n.is_empty()),
            _ => None,
        };
        relocs.insert(offset - start, target);
    }
    let end = usize::try_from(start).unwrap() + 104 + 16;
    let image = data[usize::try_from(start).unwrap()..end.min(data.len())].to_vec();
    (relocs, image)
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// `cb$image` holds the version, `unwind_edges`, two module records in
/// build order, the stacks and an empty constants table, with relocations
/// to its own module table, the entry thunk, the trampolines the program
/// has (0 and 2) and the module records, and none where a trampoline is
/// missing.
#[test]
fn link_program_image_words() {
    for &format in Format::ALL {
        let bytes = program(format);
        let (relocs, image) = relocs_of(format, &bytes, "cb$image");
        assert_eq!(
            u32::from_le_bytes(image[0..4].try_into().unwrap()),
            abi::IMAGE_VERSION
        );
        assert_eq!(u32::from_le_bytes(image[4..8].try_into().unwrap()), 1);
        assert_eq!(word(&image, 8), 2, "{format:?}: two module records");
        assert_eq!(word(&image, 72), 8 << 20);
        assert_eq!(word(&image, 80), 1 << 20);
        assert_eq!(word(&image, 88), 0, "{format:?}: no constants");
        assert_eq!(word(&image, 96), 0, "{format:?}: no constants");
        let offsets: Vec<u64> = relocs.keys().copied().collect();
        assert_eq!(
            offsets,
            [16, 24, 32, 48, 104, 112],
            "{format:?}: {relocs:?}"
        );
        for (offset, wanted) in [(104, RECORDS[0]), (112, RECORDS[1])] {
            assert_eq!(
                relocs[&offset].as_deref(),
                Some(wanted),
                "{format:?}: {relocs:?}"
            );
        }
        if let Some(Some(entry)) = relocs.get(&24) {
            assert_eq!(entry, ENTRY, "{format:?}");
        }
    }
}

/// Whether a relocation in text is PC-relative (D-913).
fn pc_relative(flags: RelocationFlags) -> bool {
    match flags {
        RelocationFlags::Elf { r_type } => matches!(
            r_type,
            elf::R_AARCH64_ADR_PREL_PG_HI21
                | elf::R_AARCH64_ADD_ABS_LO12_NC
                | elf::R_AARCH64_CALL26
                | elf::R_X86_64_PC32
                | elf::R_X86_64_PLT32
        ),
        RelocationFlags::MachO { r_type, .. } => matches!(
            r_type,
            macho::ARM64_RELOC_PAGE21 | macho::ARM64_RELOC_PAGEOFF12 | macho::ARM64_RELOC_BRANCH26
        ),
        _ => false,
    }
}

/// `main`'s text holds only PC-relative relocations (D-913): the image,
/// `cheby_rt_main`'s import word and the runtime ID's import word.
#[test]
fn link_program_text_is_pc_relative() {
    for &format in Format::ALL {
        let bytes = program(format);
        let file = ::object::File::parse(&*bytes).unwrap();
        let mut text_relocs = 0;
        for section in file.sections() {
            if section.kind() != SectionKind::Text {
                continue;
            }
            for (offset, reloc) in section.relocations() {
                assert!(
                    pc_relative(reloc.flags()),
                    "{format:?}: {:?} at {offset}",
                    reloc.flags()
                );
                text_relocs += 1;
            }
        }
        // Three addresses, each one or two relocations.
        assert!(text_relocs >= 3, "{format:?}: {text_relocs}");
    }
}

/// The same inputs give the same bytes, and the object names no path of
/// the build (D-264, D-934).
#[test]
fn link_program_is_byte_stable() {
    for &format in Format::ALL {
        let first = program(format);
        assert_eq!(first, program(format), "{format:?}");
        let cwd = std::env::current_dir().unwrap();
        let cwd = cwd.to_string_lossy();
        assert!(
            !first.windows(cwd.len()).any(|w| w == cwd.as_bytes()),
            "{format:?} holds {cwd}"
        );
    }
}

/// An entries record without the entry thunk, or with a global
/// definition, is refused without panicking.
#[test]
fn link_program_refuses_bad_entries() {
    let format = Format::host();
    let records = records();
    let missing = ProgramSpec {
        entry: "cb$entry$main$other",
        ..spec(&records)
    };
    let err = program_object(format.isa(), &missing, &entries(&[])).unwrap_err();
    assert!(err.to_string().contains("cb$entry$main$other"), "{err}");
    let mut hidden = entries(&[0]);
    hidden.symbols[1].linkage = SymbolLinkage::Hidden;
    let err = program_object(format.isa(), &spec(&records), &hidden).unwrap_err();
    assert!(matches!(err, LinkError::Object(_)), "{err:?}");
}

/// D-918's check: equal local copies pass, differing ones are named.
#[test]
fn link_program_copies_are_checked() {
    let a = entries(&[0]);
    let b = entries(&[0]);
    crate::link::check_copies(&[("a", &a), ("b", &b)]).unwrap();
    let mut c = entries(&[0]);
    c.functions[1].bytes.push(0);
    let err = crate::link::check_copies(&[("a", &a), ("c", &c)]).unwrap_err();
    assert!(
        err.to_string().contains("`cb$tramp$0` in `a` and `c`"),
        "{err}"
    );
}

/// Runs `cmd`, failing the test with its output if it fails.
fn run(cmd: &mut std::process::Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A stub runtime archive in `dir`: `cheby_rt_main` returns 7 when it gets
/// an image, and the runtime ID's symbol only when `with_id`.
fn stub_archive(dir: &Path, with_id: bool) -> std::path::PathBuf {
    let mut c = String::from(
        "int cheby_rt_main(int argc, char **argv, const void *image) {\n\
         (void)argc; (void)argv; return image ? 7 : 1; }\n",
    );
    if with_id {
        let symbol = abi::sym::RUNTIME_ID_SYMBOL;
        c.push_str("const unsigned char ");
        c.push_str(symbol);
        c.push_str("[16] = {0};\n");
    }
    std::fs::write(dir.join("stub.c"), c).unwrap();
    run(std::process::Command::new("cc")
        .current_dir(dir)
        .args(["-c", "-fPIC", "-o", "stub.o", "stub.c"]));
    let archive = dir.join("libstub.a");
    let _ = std::fs::remove_file(&archive);
    run(std::process::Command::new("ar")
        .current_dir(dir)
        .args(["crs", "libstub.a", "stub.o"]));
    archive
}

/// The program object imports the runtime ID's symbol, so a link against
/// an archive without it fails even when `cheby build`'s check of the
/// archive is bypassed (D-930); with the symbol, the same objects link
/// into an executable whose `main` returns `cheby_rt_main`'s status with
/// the image (D-925).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn link_program_imports_runtime_id() {
    let dir = std::env::temp_dir().join(format!("cheby-link-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let format = Format::host();
    let no_records: Vec<String> = Vec::new();
    let object = program_object(format.isa(), &spec(&no_records), &entries(&[0])).unwrap();
    let objects = vec![("cb$program".to_owned(), object)];
    let output = dir.join("prog");

    let stale = stub_archive(&dir, false);
    let err = crate::link::link(&crate::link::LinkJob {
        objects: &objects,
        archive: &stale,
        output: &output,
    })
    .unwrap_err();
    let LinkError::LinkFailed { output: text, .. } = &err else {
        panic!("not a failed link: {err:?}");
    };
    assert!(text.contains("cheby_rt_id$"), "{text}");
    assert!(!output.exists());

    let good = stub_archive(&dir, true);
    crate::link::link(&crate::link::LinkJob {
        objects: &objects,
        archive: &good,
        output: &output,
    })
    .unwrap();
    let status = std::process::Command::new(&output).status().unwrap();
    assert_eq!(status.code(), Some(7));
    // Only the executable is left in the output directory.
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(".cheby-link"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
