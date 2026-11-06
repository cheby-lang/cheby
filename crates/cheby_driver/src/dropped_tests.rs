//! The rule on dropped bodies (D-464): a body that leaves THIR with no
//! error anywhere in the package is an internal compiler error, in every
//! command, cold and warm, and in `:reload`.
//!
//! The checker has no known silent path, so `frontend.rs`'s test hook drops
//! every body named `dropped_by_test`. An entry of the REPL is checked in
//! `cheby_types`, and that site's test is in `cheby_types/src/repl.rs`.
//! `session.rs`'s hook stops a session definition of that name when it is
//! checked again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::aot::{BuildOptions, build_objects};
use crate::cache::{BuildId, CacheLog, Store};
use crate::session::{Output, Session, SessionOptions, Start};
use crate::test::{TestOptions, prepare_test, prepare_test_cached};

/// The build ID of every test store.
const BUILD: BuildId = BuildId([12; 16]);

/// What every command prints for [`DROPPED`].
const ICE: &str = "internal compiler error: the body of `dropped_by_test` in module `app` was \
                   dropped with no diagnostic (D-464)";

/// A root module whose body the hook drops, and that nothing else names.
const DROPPED: &str = "fn dropped_by_test() -> Int {\n  1\n}\n\nfn main() {\n}\n";

/// `DROPPED` with the body kept.
const KEPT: &str = "fn kept() -> Int {\n  1\n}\n\nfn main() {\n  assert kept() == 1\n}\n";

/// A module with an error in a signature (E0042).
const BAD_SIGNATURE: &str = "pub fn bad(x: Missing) -> Int {\n  1\n}\n";

/// A package in a fresh temporary directory, with a store beside it.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_driver_dropped_{}_{n}_{name}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("package/src")).unwrap();
        std::fs::write(
            dir.join("package/cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let fixture = Self(dir);
        for (path, text) in files {
            fixture.write(path, text);
        }
        fixture
    }

    fn package(&self) -> PathBuf {
        self.0.join("package")
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.package().join("src").join(path), text).unwrap();
    }

    /// A command's cache over the fixture's store, logging to `log`.
    fn cache(&self, log: &str) -> Cache {
        let store = Store::open(&self.0.join("cache"), BUILD).unwrap();
        Cache::with_store(store, CacheLog::new(Some(self.0.join(log))))
    }

    /// The lines of `log`.
    fn log(&self, log: &str) -> Vec<String> {
        std::fs::read_to_string(self.0.join(log))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn build_options(&self) -> BuildOptions {
        BuildOptions {
            output: Some(self.0.join("app-exe")),
            ..BuildOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The message of `result`, which must be an error.
fn error<T>(result: Result<T, Error>) -> String {
    match result {
        Ok(_) => panic!("the command passed"),
        Err(e) => e.message().to_owned(),
    }
}

/// `summaries` is `None` here for `Build`, and for std in every mode, so
/// the rule must not depend on it.
#[test]
fn every_command_stops_without_a_cache() {
    let fixture = Fixture::new("cold", &[("main.cheby", DROPPED)]);
    let dir = fixture.package();
    assert_eq!(error(check(&dir)), ICE);
    assert_eq!(error(prepare_run(&dir, RunOptions::default())), ICE);
    assert_eq!(error(prepare_test(&dir, &TestOptions::default())), ICE);
    assert_eq!(
        error(build_objects(&dir, &fixture.build_options(), None)),
        ICE
    );
}

/// A warm command must not pass after a cold internal error: no front
/// record of the module may exist.
#[test]
fn every_command_stops_again_on_a_warm_cache() {
    let fixture = Fixture::new("warm", &[("main.cheby", DROPPED)]);
    let dir = fixture.package();
    let options = CheckOptions::default();
    for run in 0..3 {
        let cache = fixture.cache(&format!("check-{run}"));
        assert_eq!(error(check_cached(&dir, &options, &cache)), ICE, "{run}");
        let cache = fixture.cache(&format!("run-{run}"));
        let prepared = prepare_run_cached(&dir, RunOptions::default(), &cache);
        assert_eq!(error(prepared), ICE, "{run}");
        let cache = fixture.cache(&format!("test-{run}"));
        let prepared = prepare_test_cached(&dir, &TestOptions::default(), &cache);
        assert_eq!(error(prepared), ICE, "{run}");
        let cache = fixture.cache(&format!("build-{run}"));
        let built = build_objects(&dir, &fixture.build_options(), Some(&cache));
        assert_eq!(error(built), ICE, "{run}");
    }
    // Nothing of the failure stays: the fixed source checks.
    fixture.write("main.cheby", KEPT);
    let cache = fixture.cache("fixed");
    let out = check_cached(&dir, &options, &cache).unwrap();
    assert!(!out.has_errors(), "{}", out.render());
}

/// A constant, a test and a body in a module other than the root are
/// bodies too, and the message names the module.
#[test]
fn the_message_names_the_module_and_the_item() {
    for (source, what) in [
        ("const dropped_by_test: Int = 1\n", "a constant"),
        ("test \"dropped_by_test\" {\n  assert 1 == 2\n}\n", "a test"),
        ("pub fn dropped_by_test() -> Int {\n  1\n}\n", "a function"),
    ] {
        let fixture = Fixture::new(
            "module",
            &[
                ("main.cheby", "import app::other\n\nfn main() {\n}\n"),
                ("other.cheby", source),
            ],
        );
        assert_eq!(
            error(check(&fixture.package())),
            "internal compiler error: the body of `dropped_by_test` in module `app::other` was \
             dropped with no diagnostic (D-464)",
            "{what}"
        );
    }
}

/// With an error anywhere in the package the body is explained, or may be:
/// the error is printed, and no internal error hides it.
#[test]
fn an_error_in_another_module_stands() {
    let fixture = Fixture::new(
        "error",
        &[
            (
                "main.cheby",
                "import app::other\n\nfn dropped_by_test() -> Int {\n  1\n}\n\nfn main() {\n}\n",
            ),
            ("other.cheby", BAD_SIGNATURE),
        ],
    );
    let out = check(&fixture.package()).unwrap();
    assert!(out.render().contains("error[E0042]"), "{}", out.render());
}

/// A caller that lost its body to a sibling's signature has no error of
/// its own, and it gets no front record: a hit would say that its bodies
/// are complete.
#[test]
fn a_module_without_all_its_bodies_is_not_stored() {
    let caller = "import app::other\n\nfn call() -> Int {\n  other::bad(1)\n}\n\nfn main() {\n  \
                  assert call() == 1\n}\n";
    let fixture = Fixture::new(
        "stored",
        &[("main.cheby", caller), ("other.cheby", BAD_SIGNATURE)],
    );
    let dir = fixture.package();
    let options = CheckOptions::default();
    let cold = check_cached(&dir, &options, &fixture.cache("cold")).unwrap();
    let warm_cache = fixture.cache("warm");
    let warm = check_cached(&dir, &options, &warm_cache).unwrap();
    warm_cache.finish(false).unwrap();
    assert_eq!(cold.render(), warm.render());
    assert!(cold.render().contains("error[E0042]"), "{}", cold.render());
    let lines = fixture.log("warm");
    assert!(
        lines.iter().any(|line| line == "front app miss"),
        "{lines:?}"
    );
}

/// Both streams in one string, in the order of writes.
#[derive(Default)]
struct Buffer {
    text: String,
}

impl Output for Buffer {
    fn stdout(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn stderr(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

/// Starts a session in `dir`, on the thread that calls this.
fn session(dir: &Path) -> Box<Session> {
    let options = SessionOptions {
        threads: Some(2),
        ..SessionOptions::default()
    };
    match Session::start(dir, options).unwrap() {
        Start::Ready(session, _) => session,
        Start::Failed(why) => panic!("the session did not start:\n{why}"),
    }
}

/// `body` on a thread with the command's stack (D-601).
fn on_a_big_stack(body: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap();
}

/// A module of a session, with a function that shows which version runs,
/// through a type whose path a reload's new version would mark as stale.
fn value(n: u32, extra: &str) -> String {
    format!(
        "pub type Thing {{\n  Thing {{ n: Int }},\n}}\n\n\
         pub fn value() -> Thing {{\n  Thing {{ n: {n} }}\n}}\n{extra}"
    )
}

/// `:reload` stops with the internal error, changes nothing and leaves the
/// session usable: the old version runs, its type is not shown as stale,
/// and a fixed file reloads.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_reload_stops_and_the_session_goes_on() {
    on_a_big_stack(|| {
        let fixture = Fixture::new(
            "reload",
            &[
                ("main.cheby", "fn main() {\n}\n"),
                ("other.cheby", &value(1, "")),
            ],
        );
        let mut session = session(&fixture.package());
        let mut out = Buffer::default();
        session.eval("import app::other", &mut out).unwrap();
        session.eval("other::value()", &mut out).unwrap();
        let before = out.text;
        assert!(before.starts_with("Thing { n: 1 } : "), "{before}");
        assert!(!before.contains("(stale)"), "{before}");

        let dropped = "\nfn dropped_by_test() -> Int {\n  1\n}\n";
        fixture.write("other.cheby", &value(2, dropped));
        let mut out = Buffer::default();
        assert_eq!(
            error(session.reload(&mut out)),
            "internal compiler error: the body of `dropped_by_test` in module `app::other` was \
             dropped with no diagnostic (D-464)"
        );
        assert_eq!(out.text, "");

        let mut out = Buffer::default();
        session.eval("other::value()", &mut out).unwrap();
        assert_eq!(out.text, before);

        fixture.write("other.cheby", &value(3, ""));
        let mut out = Buffer::default();
        session.reload(&mut out).unwrap();
        session.eval("other::value()", &mut out).unwrap();
        assert!(
            out.text
                .starts_with("reloaded `app::other`\nThing { n: 3 } : "),
            "{}",
            out.text
        );
    });
}

/// An error in the reload stands, as in the front end.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn an_error_in_a_reload_stands() {
    on_a_big_stack(|| {
        let fixture = Fixture::new(
            "reload_error",
            &[
                ("main.cheby", "fn main() {\n}\n"),
                ("other.cheby", &value(1, "")),
            ],
        );
        let mut session = session(&fixture.package());
        let broken = "\nfn dropped_by_test() -> Int {\n  1\n}\n\nfn oops() -> Int {\n  \"x\"\n}\n";
        fixture.write("other.cheby", &value(2, broken));
        let mut out = Buffer::default();
        session.reload(&mut out).unwrap();
        assert!(out.text.contains("error[E0047]"), "{}", out.text);
    });
}

/// What the hook in `recheck_one` reports.
const RECHECK_ICE: &str = "internal compiler error: the check of `dropped_by_test` stopped";

/// An internal error in one definition checked again leaves the others to
/// their checks. `dropped_by_test` comes first, so with a loop that stops
/// there `second` runs the old module (§13.5.3, D-609). The reload stays:
/// `reloaded` is printed, and the error is returned at the end.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_reload_checks_every_definition_again_after_an_internal_error() {
    on_a_big_stack(|| {
        let fixture = Fixture::new(
            "recheck_reload",
            &[
                ("main.cheby", "fn main() {\n}\n"),
                ("other.cheby", "pub fn value() -> Int {\n  1\n}\n"),
            ],
        );
        let mut session = session(&fixture.package());
        let mut out = Buffer::default();
        for entry in [
            "import app::other",
            "fn dropped_by_test() -> Int {\n  other::value()\n}",
            "fn second() -> Int {\n  other::value()\n}",
        ] {
            session.eval(entry, &mut out).unwrap();
        }

        fixture.write("other.cheby", "pub fn value() -> Int {\n  2\n}\n");
        let mut out = Buffer::default();
        assert_eq!(error(session.reload(&mut out)), RECHECK_ICE);
        assert_eq!(out.text, "reloaded `app::other`\n");

        let mut out = Buffer::default();
        session.eval("second()", &mut out).unwrap();
        assert_eq!(out.text, "2 : Int\n");
    });
}

/// Of two internal errors the first is returned, and the module entered
/// with `:module` stays entered, although nothing after the last of them
/// enters it again (§13.5.3, D-624).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_reload_with_two_internal_errors_keeps_the_first_and_the_module() {
    on_a_big_stack(|| {
        let module = "pub fn value() -> Int {\n  1\n}\n\npriv fn hidden() -> Int {\n  7\n}\n";
        let fixture = Fixture::new(
            "recheck_two",
            &[("main.cheby", "fn main() {\n}\n"), ("other.cheby", module)],
        );
        let mut session = session(&fixture.package());
        let mut out = Buffer::default();
        for entry in [
            "import app::other",
            "fn dropped_by_test_first() -> Int {\n  other::value()\n}",
            "fn dropped_by_test_second() -> Int {\n  other::value()\n}",
        ] {
            session.eval(entry, &mut out).unwrap();
        }
        session.enter_module("app::other", &mut out).unwrap();

        fixture.write("other.cheby", &module.replace('1', "2"));
        let mut out = Buffer::default();
        assert_eq!(
            error(session.reload(&mut out)),
            "internal compiler error: the check of `dropped_by_test_first` stopped"
        );

        let mut out = Buffer::default();
        session.eval("hidden()", &mut out).unwrap();
        assert_eq!(out.text, "7 : Int\n");
    });
}

/// The same after a redefinition with a new signature (§13.5.2, D-043).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_redefinition_checks_every_definition_again_after_an_internal_error() {
    on_a_big_stack(|| {
        let fixture = Fixture::new("recheck_redefined", &[("main.cheby", "fn main() {\n}\n")]);
        let mut session = session(&fixture.package());
        let mut out = Buffer::default();
        for entry in [
            "fn value() -> Int {\n  1\n}",
            "fn dropped_by_test() -> String {\n  let v = value()\n  \"{v}\"\n}",
            "fn second() -> String {\n  let v = value()\n  \"{v}\"\n}",
        ] {
            session.eval(entry, &mut out).unwrap();
        }

        let mut out = Buffer::default();
        let redefined = session.eval("fn value() -> String {\n  \"new\"\n}", &mut out);
        assert_eq!(error(redefined), RECHECK_ICE);

        let mut out = Buffer::default();
        session.eval("second()", &mut out).unwrap();
        assert_eq!(out.text, "\"new\" : String\n");
    });
}

/// `frontend.rs`'s hook reaches every module of a reload, and the rule
/// keeps the first drop whatever the modules after it hold. `second`
/// imports `first`, so it is reloaded too, and after it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_drop_stands_when_the_next_module_of_a_reload_has_none() {
    on_a_big_stack(|| {
        let fixture = Fixture::new(
            "reload_two",
            &[
                ("main.cheby", "fn main() {\n}\n"),
                ("first.cheby", "pub fn base() -> Int {\n  1\n}\n"),
                (
                    "second.cheby",
                    "import app::first\n\npub fn next() -> Int {\n  first::base()\n}\n",
                ),
            ],
        );
        let mut session = session(&fixture.package());
        fixture.write(
            "first.cheby",
            "pub fn base() -> Int {\n  2\n}\n\nfn dropped_by_test() -> Int {\n  1\n}\n",
        );
        let mut out = Buffer::default();
        assert_eq!(
            error(session.reload(&mut out)),
            "internal compiler error: the body of `dropped_by_test` in module `app::first` was \
             dropped with no diagnostic (D-464)"
        );
    });
}

/// A warning explains no drop, in a reload as in the front end.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn a_warning_does_not_hide_a_drop_in_a_reload() {
    on_a_big_stack(|| {
        let fixture = Fixture::new(
            "reload_warning",
            &[
                ("main.cheby", "fn main() {\n}\n"),
                ("other.cheby", &value(1, "")),
            ],
        );
        let mut session = session(&fixture.package());
        let warned =
            "\nfn dropped_by_test() -> Int {\n  1\n}\n\npriv fn unused() -> Int {\n  1\n}\n";
        fixture.write("other.cheby", &value(2, warned));
        let mut out = Buffer::default();
        assert_eq!(
            error(session.reload(&mut out)),
            "internal compiler error: the body of `dropped_by_test` in module `app::other` was \
             dropped with no diagnostic (D-464)"
        );
    });
}

/// An [`Error`] that travels through an `Ice`, as a failure of std's first
/// use does, keeps one prefix, and a tool error keeps its words.
#[test]
fn an_internal_error_carried_by_an_ice_has_one_prefix() {
    let first = Error::from(Ice::new("why"));
    let carried = Error::from(Ice::new(first.ice_message()));
    assert_eq!(carried.message(), "internal compiler error: why");
    assert_eq!(Error::new("why".to_owned()).ice_message(), "why");
}
