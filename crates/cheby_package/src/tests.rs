//! Tests of manifest reading and module discovery, on the examples and on
//! small packages written to a temporary directory.

use std::path::{Path, PathBuf};

use cheby_diag::{Diagnostic, FileId, SourceMap, render_all};

use super::*;

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples")
}

fn examples() -> Vec<PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir(examples_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join(MANIFEST_FILE).is_file())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 32, "examples not found");
    dirs
}

/// Parses `text` as a manifest and renders its diagnostics.
fn parse(text: &str) -> (Option<Manifest>, String) {
    let mut sources = SourceMap::new();
    let file = sources.add_file(MANIFEST_FILE, text).unwrap();
    let mut diags = Vec::new();
    let manifest = Manifest::parse(file, text, &mut diags);
    sources.sort(&mut diags);
    (manifest, render_all(&diags, &sources))
}

/// A fresh, empty temporary directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheby_package_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, files: &[&str]) {
    for file in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
}

fn discover_paths(root: &Path, package: &str) -> (Vec<String>, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let modules = discover(root, package, &mut diags).unwrap();
    let paths = modules
        .iter()
        .map(|m| format!("{} {}", m.display, m.path))
        .collect();
    (paths, diags)
}

#[test]
fn every_example_manifest_loads() {
    for dir in examples() {
        let mut sources = SourceMap::new();
        let mut diags = Vec::new();
        let manifest = Manifest::load(&dir, &mut sources, &mut diags)
            .unwrap()
            .unwrap_or_else(|| panic!("{}: {}", dir.display(), render_all(&diags, &sources)));
        let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
        // `NNN_name`: the package name is the directory name after the number.
        assert_eq!(dir_name.get(4..), Some(manifest.name.as_str()));
        assert_eq!(manifest.version.to_string(), "0.1.0");
        if manifest.dependencies.is_empty() {
            assert!(
                diags.is_empty(),
                "{dir_name}: {}",
                render_all(&diags, &sources)
            );
        } else {
            // Dependencies are read, then rejected until M5.
            assert_eq!(dir_name, "030_shop_service");
            assert_eq!(manifest.dependencies.len(), 1);
            let dep = manifest.dependencies.first().unwrap();
            assert_eq!(dep.key, "sql");
            assert_eq!(dep.reference, GitReference::Tag("v1.4.0".to_owned()));
            assert_eq!(
                manifest.targets.iter().collect::<Vec<_>>(),
                [Target::Native]
            );
            let rendered = render_all(&diags, &sources);
            assert!(
                rendered.contains("error[E0036]: dependencies are not supported before M5"),
                "{rendered}"
            );
        }
    }
}

#[test]
fn every_example_module_path() {
    for dir in examples() {
        let mut sources = SourceMap::new();
        let mut diags = Vec::new();
        let manifest = Manifest::load(&dir, &mut sources, &mut diags)
            .unwrap()
            .unwrap();
        let mut diags = Vec::new();
        let modules = discover(&dir, &manifest.name, &mut diags).unwrap();
        assert!(diags.is_empty(), "{}", dir.display());
        let root: Vec<_> = modules.iter().filter(|m| m.path.is_root()).collect();
        assert_eq!(root.len(), 1, "{}: one root module", dir.display());
        for module in &modules {
            assert_eq!(module.path.package(), manifest.name);
            let stem = module.display.strip_prefix("src/").unwrap();
            let stem = stem.strip_suffix(".cheby").unwrap();
            let expected = if stem == "main" {
                manifest.name.clone()
            } else {
                format!("{}::{}", manifest.name, stem.replace('/', "::"))
            };
            assert_eq!(module.path.to_string(), expected);
            assert!(dir.join(&module.file).is_file());
        }
        // Every `.cheby` file is a module, and nothing else is.
        let cheby_files = std::fs::read_dir(dir.join("src"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "cheby")
            })
            .count();
        assert_eq!(modules.len(), cheby_files, "{}", dir.display());
    }
}

#[test]
fn module_paths_of_a_tictactoe_package() {
    let (paths, diags) = discover_paths(&examples_dir().join("004_tictactoe"), "tictactoe");
    assert_eq!(diags.len(), 0);
    assert_eq!(
        paths,
        [
            "src/ai.cheby tictactoe::ai",
            "src/board.cheby tictactoe::board",
            "src/main.cheby tictactoe",
            "src/player.cheby tictactoe::player",
        ]
    );
}

#[test]
fn discovery_rules() {
    let root = temp_dir("rules");
    write(
        &root,
        &[
            "src/main.cheby",
            "src/web.cheby",
            "src/web/router.cheby",
            "src/web/router/deep.cheby",
            "src/tools/migrate.cheby",
            "src/main/inner.cheby",
            "src/random_ffi.mjs",
            "src/notes.txt",
            "src/.hidden.cheby",
            "src/.cache/x.cheby",
            "src/HttpClient.cheby",
            "src/web-server/x.cheby",
            "src/type.cheby",
            "src/a.b.cheby",
            "src/js-helpers/helper.mjs",
        ],
    );
    let (paths, diags) = discover_paths(&root, "my_app");
    assert_eq!(
        paths,
        [
            "src/main.cheby my_app",
            "src/main/inner.cheby my_app::main::inner",
            "src/tools/migrate.cheby my_app::tools::migrate",
            "src/web.cheby my_app::web",
            "src/web/router.cheby my_app::web::router",
            "src/web/router/deep.cheby my_app::web::router::deep",
        ]
    );
    let sources = SourceMap::new();
    let rendered = render_all(&diags, &sources);
    assert_eq!(
        rendered,
        "\
error[E0037]: `src/HttpClient.cheby` is not a valid module name
  = note: the name of a file under `src/` becomes part of a module path, so it must be a snake_case identifier that is not a keyword
  = help: rename it to `src/http_client.cheby`

error[E0037]: `src/a.b.cheby` is not a valid module name
  = note: the name of a file under `src/` becomes part of a module path, so it must be a snake_case identifier that is not a keyword
  = help: rename it to `src/a_b.cheby`

error[E0037]: `src/js-helpers` is not a valid module name
  = note: the name of a directory under `src/` becomes part of a module path, so it must be a snake_case identifier that is not a keyword
  = help: rename it to `src/js_helpers`

error[E0037]: `src/type.cheby` is not a valid module name
  = note: the name of a file under `src/` becomes part of a module path, so it must be a snake_case identifier that is not a keyword
  = help: rename it to `src/type_.cheby`

error[E0037]: `src/web-server` is not a valid module name
  = note: the name of a directory under `src/` becomes part of a module path, so it must be a snake_case identifier that is not a keyword
  = help: rename it to `src/web_server`

"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn dangling_links_to_other_files_are_ignored() {
    let root = temp_dir("dangling");
    write(&root, &["src/main.cheby", "src/js/helper.mjs"]);
    std::os::unix::fs::symlink("/nonexistent", root.join("src/js/broken.mjs")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", root.join("src/gone")).unwrap();
    let (paths, diags) = discover_paths(&root, "app");
    assert_eq!(paths, ["src/main.cheby app"]);
    assert_eq!(diags.len(), 0);
    // A dangling `.cheby` link names a module that cannot be read.
    std::os::unix::fs::symlink("/nonexistent", root.join("src/lost.cheby")).unwrap();
    let mut diags = Vec::new();
    assert!(discover(&root, "app", &mut diags).is_err());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn missing_src_is_a_tool_error() {
    let root = temp_dir("no_src");
    let mut diags = Vec::new();
    let err = discover(&root, "my_app", &mut diags).unwrap_err();
    assert_eq!(err.to_string(), "the package has no `src` directory");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn missing_manifest_is_a_tool_error() {
    let root = temp_dir("no_manifest");
    let mut sources = SourceMap::new();
    let mut diags = Vec::new();
    let err = Manifest::load(&root, &mut sources, &mut diags).unwrap_err();
    assert!(err.message().starts_with("cheby.toml: "), "{err}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn manifest_not_utf8() {
    let root = temp_dir("bad_utf8");
    std::fs::write(root.join(MANIFEST_FILE), b"name = \"a\xff\"\n").unwrap();
    let mut sources = SourceMap::new();
    let mut diags = Vec::new();
    let manifest = Manifest::load(&root, &mut sources, &mut diags).unwrap();
    assert!(manifest.is_none());
    assert_eq!(
        render_all(&diags, &sources),
        "\
error[E0036]: `cheby.toml` is not valid UTF-8
 --> cheby.toml:1:10
  |
1 | name = \"a\u{fffd}\"
  |          ^
  |
  = help: save the file as UTF-8

"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_full_manifest() {
    let (manifest, rendered) = parse(
        "name = \"my_app\"\nversion = \"1.2.3-rc.1+build.7\"\ndescription = \"An app\"\n\
         licences = [\"MIT\", \"Apache-2.0\"]\ntargets = [\"js\"]\n\n[const-eval]\ntime-limit = \"10s\"\n",
    );
    assert_eq!(rendered, "");
    let manifest = manifest.unwrap();
    assert_eq!(manifest.name, "my_app");
    assert_eq!(manifest.version.to_string(), "1.2.3-rc.1+build.7");
    assert_eq!(manifest.version.major, 1);
    assert_eq!(manifest.description.as_deref(), Some("An app"));
    assert_eq!(manifest.licences, ["MIT", "Apache-2.0"]);
    assert_eq!(manifest.targets.iter().collect::<Vec<_>>(), [Target::Js]);
    assert_eq!(manifest.root_module().to_string(), "my_app");
    assert_eq!(manifest.dependencies, []);
    assert_eq!(manifest.file, FileId::new(0));
    assert_eq!(
        manifest.const_eval_time_limit,
        std::time::Duration::from_secs(10)
    );
    assert_eq!(
        manifest.const_eval_memory_limit,
        Manifest::DEFAULT_MEMORY_LIMIT
    );
}

/// D-536: `[const-eval]` sets the limits of constant evaluation; without
/// it they are 10 s and 1 GiB.
#[test]
fn const_eval_limits_parse() {
    let base = "name = \"app\"\nversion = \"0.1.0\"\n";
    let (manifest, rendered) = parse(base);
    assert_eq!(rendered, "");
    let manifest = manifest.unwrap();
    assert_eq!(manifest.const_eval_time_limit, Manifest::DEFAULT_TIME_LIMIT);
    assert_eq!(
        Manifest::DEFAULT_TIME_LIMIT,
        std::time::Duration::from_secs(10)
    );
    assert_eq!(manifest.const_eval_memory_limit, 1 << 30);
    for (time, millis) in [("250ms", 250), ("3s", 3000), ("0010s", 10_000)] {
        for (memory, bytes) in [("64KiB", 64 << 10), ("5MiB", 5 << 20), ("2GiB", 2 << 30)] {
            let (manifest, rendered) = parse(&format!(
                "{base}[const-eval]\ntime-limit = \"{time}\"\nmemory-limit = \"{memory}\"\n"
            ));
            assert_eq!(rendered, "");
            let manifest = manifest.unwrap();
            assert_eq!(
                manifest.const_eval_time_limit,
                std::time::Duration::from_millis(millis)
            );
            assert_eq!(manifest.const_eval_memory_limit, bytes);
        }
    }
    // One key alone leaves the other at its default.
    let (manifest, _) = parse(&format!("{base}[const-eval]\nmemory-limit = \"1MiB\"\n"));
    let manifest = manifest.unwrap();
    assert_eq!(manifest.const_eval_time_limit, Manifest::DEFAULT_TIME_LIMIT);
    assert_eq!(manifest.const_eval_memory_limit, 1 << 20);
}

/// D-536: any other key or value in `[const-eval]` is E0036, and the
/// manifest is still usable, with the defaults for the bad values.
#[test]
fn const_eval_errors() {
    let (manifest, rendered) = parse(
        "name = \"app\"\nversion = \"0.1.0\"\n\n[const-eval]\ntime-limit = \"10\"\n\
         memory-limit = 1024\nstack-limit = \"1MiB\"\n",
    );
    let manifest = manifest.unwrap();
    assert_eq!(manifest.const_eval_time_limit, Manifest::DEFAULT_TIME_LIMIT);
    assert_eq!(
        manifest.const_eval_memory_limit,
        Manifest::DEFAULT_MEMORY_LIMIT
    );
    assert_eq!(
        rendered,
        "\
error[E0036]: `10` is not a valid `time-limit`
 --> cheby.toml:5:14
  |
5 | time-limit = \"10\"
  |              ^^^^
  |
  = help: write an integer followed by `ms` or `s`, such as `\"10s\"`

error[E0036]: `memory-limit` must be a string
 --> cheby.toml:6:16
  |
6 | memory-limit = 1024
  |                ^^^^ this is an integer

error[E0036]: unknown key `stack-limit` in `[const-eval]`
 --> cheby.toml:7:1
  |
7 | stack-limit = \"1MiB\"
  | ^^^^^^^^^^^ unknown key
  |
  = help: the keys are `time-limit` and `memory-limit`

"
    );
    for (bad, expected) in [
        (
            "time-limit = \"10 s\"",
            "`10 s` is not a valid `time-limit`",
        ),
        ("time-limit = \"s\"", "`s` is not a valid `time-limit`"),
        ("time-limit = \"-5s\"", "`-5s` is not a valid `time-limit`"),
        (
            "time-limit = \"1.5s\"",
            "`1.5s` is not a valid `time-limit`",
        ),
        ("time-limit = \"10m\"", "`10m` is not a valid `time-limit`"),
        (
            "time-limit = \"0ms\"",
            "`time-limit` must be greater than zero",
        ),
        (
            "time-limit = \"99999999999999999999s\"",
            "`time-limit` `99999999999999999999s` is too large",
        ),
        (
            "memory-limit = \"1GB\"",
            "`1GB` is not a valid `memory-limit`",
        ),
        (
            "memory-limit = \"1gib\"",
            "`1gib` is not a valid `memory-limit`",
        ),
        (
            "memory-limit = \"0KiB\"",
            "`memory-limit` must be greater than zero",
        ),
        (
            "memory-limit = \"17179869184GiB\"",
            "`memory-limit` `17179869184GiB` is too large",
        ),
    ] {
        let (manifest, rendered) = parse(&format!(
            "name = \"app\"\nversion = \"0.1.0\"\n[const-eval]\n{bad}\n"
        ));
        assert!(manifest.is_some());
        assert!(
            rendered.contains(&format!("error[E0036]: {expected}\n")),
            "{bad}: {rendered}"
        );
        assert_eq!(rendered.matches("error[").count(), 1, "{rendered}");
    }
    let (_, rendered) = parse("name = \"app\"\nversion = \"0.1.0\"\nconst-eval = 3\n");
    assert!(
        rendered.contains("error[E0036]: `const-eval` must be a table"),
        "{rendered}"
    );
}

/// D-660: without `[runtime]` the stacks are 8 MiB for fibers and 64 MiB
/// for the root fiber.
#[test]
fn runtime_defaults() {
    let (manifest, rendered) = parse("name = \"app\"\nversion = \"0.1.0\"\n");
    assert_eq!(rendered, "");
    let manifest = manifest.unwrap();
    assert_eq!(manifest.fiber_stack, 8 << 20);
    assert_eq!(manifest.main_stack, 64 << 20);
    assert_eq!(Manifest::DEFAULT_FIBER_STACK, 8 << 20);
    assert_eq!(Manifest::DEFAULT_MAIN_STACK, 64 << 20);
    assert_eq!(Manifest::MIN_STACK, 1 << 20);
    assert_eq!(Manifest::MAX_STACK, 16 << 30);
}

/// D-660, D-536: `[runtime]`'s sizes take `memory-limit`'s form, in
/// bytes and unrounded (D-796); one key alone leaves the
/// other at its default.
#[test]
fn runtime_sizes_parse() {
    let base = "name = \"app\"\nversion = \"0.1.0\"\n";
    for (text, bytes) in [
        ("1MiB", 1 << 20),
        ("1024KiB", 1 << 20),
        ("1025KiB", 1025 << 10),
        ("0016MiB", 16 << 20),
        ("256MiB", 256 << 20),
        ("16GiB", 16 << 30),
        ("16777216KiB", 16 << 30),
    ] {
        let (manifest, rendered) = parse(&format!(
            "{base}[runtime]\nfiber-stack = \"{text}\"\nmain-stack = \"{text}\"\n"
        ));
        assert_eq!(rendered, "", "{text}");
        let manifest = manifest.unwrap();
        assert_eq!(manifest.fiber_stack, bytes, "{text}");
        assert_eq!(manifest.main_stack, bytes, "{text}");
    }
    let (manifest, rendered) = parse(&format!("{base}[runtime]\nmain-stack = \"2MiB\"\n"));
    assert_eq!(rendered, "");
    let manifest = manifest.unwrap();
    assert_eq!(manifest.fiber_stack, Manifest::DEFAULT_FIBER_STACK);
    assert_eq!(manifest.main_stack, 2 << 20);
    let (manifest, _) = parse(&format!("{base}[runtime]\nfiber-stack = \"3MiB\"\n"));
    let manifest = manifest.unwrap();
    assert_eq!(manifest.fiber_stack, 3 << 20);
    assert_eq!(manifest.main_stack, Manifest::DEFAULT_MAIN_STACK);
}

/// D-660: a size outside 1 MiB to 16 GiB is E0036, zero included, and the
/// manifest keeps the default.
#[test]
fn runtime_sizes_out_of_range() {
    for (bad, expected) in [
        (
            "fiber-stack = \"0MiB\"",
            "`fiber-stack` must be from 1MiB to 16GiB",
        ),
        (
            "fiber-stack = \"1023KiB\"",
            "`fiber-stack` must be from 1MiB to 16GiB",
        ),
        (
            "main-stack = \"512KiB\"",
            "`main-stack` must be from 1MiB to 16GiB",
        ),
        (
            "main-stack = \"16777217KiB\"",
            "`main-stack` must be from 1MiB to 16GiB",
        ),
        (
            "fiber-stack = \"17GiB\"",
            "`fiber-stack` must be from 1MiB to 16GiB",
        ),
        (
            "fiber-stack = \"17179869184GiB\"",
            "`fiber-stack` `17179869184GiB` is too large",
        ),
    ] {
        let (manifest, rendered) = parse(&format!(
            "name = \"app\"\nversion = \"0.1.0\"\n[runtime]\n{bad}\n"
        ));
        let manifest = manifest.unwrap();
        assert_eq!(manifest.fiber_stack, Manifest::DEFAULT_FIBER_STACK);
        assert_eq!(manifest.main_stack, Manifest::DEFAULT_MAIN_STACK);
        assert!(
            rendered.contains(&format!("error[E0036]: {expected}\n")),
            "{bad}: {rendered}"
        );
        assert_eq!(rendered.matches("error[").count(), 1, "{rendered}");
    }
}

/// D-660, D-536: a value of the wrong form is E0036.
#[test]
fn runtime_sizes_of_the_wrong_form() {
    for (bad, expected) in [
        (
            "fiber-stack = \"8MB\"",
            "`8MB` is not a valid `fiber-stack`",
        ),
        (
            "fiber-stack = \"8 MiB\"",
            "`8 MiB` is not a valid `fiber-stack`",
        ),
        ("main-stack = \"MiB\"", "`MiB` is not a valid `main-stack`"),
        (
            "main-stack = \"-1MiB\"",
            "`-1MiB` is not a valid `main-stack`",
        ),
        (
            "main-stack = \"1.5GiB\"",
            "`1.5GiB` is not a valid `main-stack`",
        ),
        ("main-stack = \"64\"", "`64` is not a valid `main-stack`"),
        ("fiber-stack = 8388608", "`fiber-stack` must be a string"),
    ] {
        let (manifest, rendered) = parse(&format!(
            "name = \"app\"\nversion = \"0.1.0\"\n[runtime]\n{bad}\n"
        ));
        assert!(manifest.is_some());
        assert!(
            rendered.contains(&format!("error[E0036]: {expected}\n")),
            "{bad}: {rendered}"
        );
        assert_eq!(rendered.matches("error[").count(), 1, "{rendered}");
    }
}

/// D-660, D-444: any other key in `[runtime]` is E0036 with a help line
/// naming the two keys, and `runtime` must be a table.
#[test]
fn runtime_unknown_keys_and_wrong_type() {
    let (manifest, rendered) = parse(
        "name = \"app\"\nversion = \"0.1.0\"\n\n[runtime]\nstack = \"8MiB\"\n\
         fiber-stack = \"16MiB\"\n",
    );
    let manifest = manifest.unwrap();
    assert_eq!(manifest.fiber_stack, 16 << 20);
    assert_eq!(
        rendered,
        "\
error[E0036]: unknown key `stack` in `[runtime]`
 --> cheby.toml:5:1
  |
5 | stack = \"8MiB\"
  | ^^^^^ unknown key
  |
  = help: the keys are `fiber-stack` and `main-stack`

"
    );
    let (_, rendered) = parse("name = \"app\"\nversion = \"0.1.0\"\nruntime = \"8MiB\"\n");
    assert!(
        rendered.contains("error[E0036]: `runtime` must be a table"),
        "{rendered}"
    );
}

#[test]
fn targets_default_to_both() {
    let (manifest, _) = parse("name = \"a\"\nversion = \"0.0.0\"\n");
    assert_eq!(
        manifest.unwrap().targets.iter().collect::<Vec<_>>(),
        [Target::Native, Target::Js]
    );
}

#[test]
fn versions() {
    for ok in [
        "0.0.0",
        "1.2.3",
        "10.20.30",
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-0.3.7",
        "1.0.0-x.7.z.92",
        "1.0.0+20130313144700",
        "1.0.0-beta+exp.sha.5114f85",
        "1.0.0-x-y-z.--",
    ] {
        let v = Version::parse(ok).unwrap_or_else(|| panic!("{ok}"));
        assert_eq!(v.to_string(), ok);
    }
    for bad in [
        "",
        "1",
        "1.0",
        "1.0.0.0",
        "01.0.0",
        "1.01.0",
        "1.0.0-",
        "1.0.0+",
        "1.0.0-01",
        "1.0.0-a..b",
        "v1.0.0",
        "1.0.0-a_b",
        " 1.0.0",
        "1.0.x",
        "99999999999999999999.0.0",
    ] {
        assert!(Version::parse(bad).is_none(), "{bad}");
    }
}

#[test]
fn invalid_toml() {
    let (manifest, rendered) = parse("name = \"a\nversion = \"0.1.0\"\n");
    assert!(manifest.is_none());
    assert!(
        rendered.starts_with("error[E0036]: `cheby.toml` is not valid TOML\n --> cheby.toml:1:"),
        "{rendered}"
    );
}

#[test]
fn invalid_name_and_version() {
    let (manifest, rendered) = parse("name = \"MyApp\"\nversion = \"1\"\n");
    assert!(manifest.is_none());
    assert_eq!(
        rendered,
        "\
error[E0036]: package name `MyApp` is not a valid name
 --> cheby.toml:1:8
  |
1 | name = \"MyApp\"
  |        ^^^^^^^
  |
  = note: a package name is a snake_case identifier that is not a keyword
  = help: write `\"my_app\"`

error[E0036]: `1` is not a semantic version
 --> cheby.toml:2:11
  |
2 | version = \"1\"
  |           ^^^
  |
  = help: write a version such as `0.1.0`

"
    );
}

#[test]
fn missing_keys_and_wrong_types() {
    let (manifest, rendered) = parse("targets = \"native\"\ncolour = 1\n");
    assert!(manifest.is_none());
    assert_eq!(
        rendered,
        "\
error[E0036]: `cheby.toml` has no `name`
  = help: add the package name, such as `name = \"my_app\"`

error[E0036]: `cheby.toml` has no `version`
  = help: add the package version, such as `version = \"0.1.0\"`

error[E0036]: `targets` must be a list of targets
 --> cheby.toml:1:11
  |
1 | targets = \"native\"
  |           ^^^^^^^^ this is a string

error[E0036]: unknown key `colour` in `cheby.toml`
 --> cheby.toml:2:1
  |
2 | colour = 1
  | ^^^^^^ unknown key
  |
  = help: the keys are `name`, `version`, `description`, `licences`, `targets`, `[dependencies]`, `[const-eval]` and `[runtime]`

"
    );
}

#[test]
fn std_and_bad_targets() {
    let (manifest, rendered) =
        parse("name = \"std\"\nversion = \"0.1.0\"\ntargets = [\"js\", \"wasm\", \"js\"]\n");
    assert!(manifest.is_none());
    assert!(
        rendered.contains("error[E0036]: the package name `std` is reserved"),
        "{rendered}"
    );
    assert!(
        rendered.contains("error[E0036]: unknown target `wasm`"),
        "{rendered}"
    );
    assert!(
        rendered.contains("error[E0036]: target `js` is listed twice"),
        "{rendered}"
    );
    let (_, rendered) = parse("name = \"a\"\nversion = \"0.1.0\"\ntargets = []\n");
    assert!(
        rendered.contains("error[E0036]: `targets` is empty"),
        "{rendered}"
    );
}

#[test]
fn dependency_errors_point_at_the_right_key() {
    // The second of `tag` and `rev` in source order is the one reported,
    // and the key is checked against the package name even when its entry
    // has another error (D-238).
    let (_, rendered) = parse(
        "name = \"p\"\nversion = \"1.0.0\"\n[dependencies]\np = { git = \"x\", tag = \"v\", rev = \"y\" }\n",
    );
    assert!(
        rendered.contains("has both `tag` and `rev`\n --> cheby.toml:4:29"),
        "{rendered}"
    );
    assert!(
        rendered.contains("dependency key `p` is the package's own name"),
        "{rendered}"
    );
}

#[test]
fn dependencies_are_read_then_rejected() {
    let (manifest, rendered) = parse(
        "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
         json = { git = \"https://example.com/json\", rev = \"4f2a9c1\" }\n\
         std = { git = \"x\", tag = \"v1\" }\n\
         app = { git = \"x\", tag = \"v1\" }\n\
         Bad = { git = \"x\", tag = \"v1\", rev = \"y\" }\n\
         nogit = { tag = \"v1\" }\n\
         extra = { git = \"x\", tag = \"v1\", branch = \"main\" }\n",
    );
    let manifest = manifest.unwrap();
    let keys: Vec<_> = manifest
        .dependencies
        .iter()
        .map(|d| d.key.as_str())
        .collect();
    // Source order (toml's `preserve_order`).
    assert_eq!(keys, ["json", "app"]);
    for expected in [
        "error[E0036]: dependencies are not supported before M5\n --> cheby.toml:4:2",
        "error[E0036]: the dependency key `std` is reserved for the standard library",
        "error[E0036]: dependency key `app` is the package's own name",
        "error[E0036]: dependency key `Bad` is not a valid name",
        "error[E0036]: dependency `Bad` has both `tag` and `rev`",
        "error[E0036]: dependency `nogit` has no `git`",
        "error[E0036]: unknown key `branch` in dependency `extra`",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?} in\n{rendered}"
        );
    }
}
