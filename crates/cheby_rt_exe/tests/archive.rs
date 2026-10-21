//! The runtime archive defines every symbol generated code may name
//! (D-932) and the runtime ID's symbol (D-930), as global definitions a
//! linker finds by name.
//!
//! The test reads `libcheby_rt_exe.a` from the profile directory of its
//! own target directory, where `cargo build -p cheby_rt_exe` writes it. A
//! missing archive fails the test, never skips it (D-929).

#[cfg(test)]
mod archive {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use cheby_runtime::abi;
    use object::read::archive::ArchiveFile;
    use object::{BinaryFormat, Object as _, ObjectSymbol as _};

    /// The archive's path: `target/<profile>/libcheby_rt_exe.a` next to
    /// the `deps/` directory this test runs from.
    fn archive_path() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let deps = exe.parent().unwrap();
        let profile = deps.parent().unwrap();
        profile.join("libcheby_rt_exe.a")
    }

    /// Every defined global symbol of every object in the archive, with
    /// the number of its definitions; Mach-O's leading underscore removed.
    fn defined_globals(bytes: &[u8]) -> BTreeMap<String, usize> {
        let archive = ArchiveFile::parse(bytes).unwrap();
        let mut out = BTreeMap::new();
        let mut objects = 0_usize;
        for member in archive.members() {
            let member = member.unwrap();
            let data = member.data(bytes).unwrap();
            let Ok(file) = object::File::parse(data) else {
                continue;
            };
            objects = objects.saturating_add(1);
            let macho = file.format() == BinaryFormat::MachO;
            for symbol in file.symbols() {
                if !symbol.is_definition() || !symbol.is_global() {
                    continue;
                }
                let Ok(name) = symbol.name() else {
                    continue;
                };
                let name = if macho {
                    name.strip_prefix('_').unwrap_or(name)
                } else {
                    name
                };
                let count = out.entry(name.to_owned()).or_insert(0_usize);
                *count = count.saturating_add(1);
            }
        }
        assert!(objects > 0, "the archive holds no object file");
        out
    }

    #[test]
    #[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
    fn the_archive_defines_every_runtime_symbol() {
        let path = archive_path();
        let bytes = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "cannot read the runtime archive {} ({error}); \
                 run `cargo build -p cheby_rt_exe` with this profile first (D-929)",
                path.display()
            )
        });
        let defined = defined_globals(&bytes);
        let missing: Vec<&str> = cheby_runtime::symbols()
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| !defined.contains_key(*name))
            .collect();
        assert!(
            missing.is_empty(),
            "the archive {} does not define these names of `symbols()` (D-932): {missing:?}",
            path.display()
        );
        assert_eq!(
            defined.get(abi::sym::RUNTIME_ID_SYMBOL),
            Some(&1),
            "the archive {} must define `{}` once (D-930); is it stale? \
             run `cargo build -p cheby_rt_exe`",
            path.display(),
            abi::sym::RUNTIME_ID_SYMBOL
        );
        // No other runtime ID's symbol: the archive comes from one runtime.
        let ids: Vec<&String> = defined
            .keys()
            .filter(|name| name.starts_with("cheby_rt_id$"))
            .collect();
        assert_eq!(ids, [abi::sym::RUNTIME_ID_SYMBOL]);
        // The entry an executable's `main` calls (D-925).
        #[cfg(unix)]
        assert!(defined.contains_key("cheby_rt_main"));
    }
}
