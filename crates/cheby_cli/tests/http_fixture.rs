//! The HTTP fixture end to end (D-1027): a package whose `args` holds
//! `{http_fixture}` runs twice, against two servers of its own on two
//! ports, each run's arguments and environment from
//! `Example::run_args_with`, the expansion the conformance harness uses.
//! Both runs print the same text, so one `expected.stdout` holds for any
//! port: the output prints paths, never the port.
//!
//! Every `cheby` gets a fresh `CHEBY_CACHE` (D-1128).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cheby_test_support::cache::{FreshDir, HarnessCache};
use cheby_test_support::example::{Example, HTTP_FIXTURE, TestConfig};
use cheby_test_support::http_fixture::Server;
use cheby_test_support::process::{self, RunOutput};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
/// Before `CHEBY_HARNESS_SCALE` (D-722).
const TIMEOUT: Duration = Duration::from_secs(60);

/// The program: fetches `/`, `/about` and `/old` from the site at its
/// first argument with `http::get`, in `cheby fmt`'s form.
const MAIN: &str = r#"//! Fetches three pages from the site at the URL of the first argument and
//! prints each one's status, path and body, never the URL.

import std::env
import std::http
import std::io
import std::list
import std::string

fn main() {
  case env::args() {
    [base, ..] => list::each(["/", "/about", "/old"], fn(path) { fetch(base, path) })
    [] => io::println("usage: fetch_site URL")
  }
}

/// Prints the status of `path` on the site at `base`, and its body.
fn fetch(base: String, path: String) {
  case http::get(base + path) {
    Ok(response) => {
      let body = string::trim(response.body)
      io::println("{response.status} {path} {body}")
    }
    Err(reason) => io::println("error {path}: {reason}")
  }
}
"#;

/// The one golden both runs must print.
const EXPECTED: &str = "200 / <h1>Home</h1>\n200 /about About us\n404 /old not found\n";

/// The test package in a fresh directory, as an example: `cheby.toml`,
/// `src/main.cheby`, `site/index.html`, `site/about`, an `args` file
/// holding the placeholder, and `test.toml`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: failing to write the package fails the test"
)]
fn package(prefix: &str) -> (FreshDir, Example) {
    std::fs::create_dir_all(TMP).unwrap();
    let dir = FreshDir::create_in(Path::new(TMP), prefix).unwrap();
    let files = [
        ("cheby.toml", "name = \"fetch_site\"\nversion = \"0.1.0\"\n"),
        ("test.toml", "milestone = \"M3\"\n"),
        ("src/main.cheby", MAIN),
        ("site/index.html", "<h1>Home</h1>\n"),
        ("site/about", "About us\n"),
        ("args", &format!("{HTTP_FIXTURE}\n")),
    ];
    for (path, text) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let example = Example {
        name: "fetch_site".to_owned(),
        dir: dir.path().to_path_buf(),
        config: TestConfig::read(&dir.path().join("test.toml")).unwrap(),
    };
    (dir, example)
}

/// `cheby run` of `example` with its arguments and environment expanded
/// by `run_args_with`, whose resolver gives `url` for the package's
/// `site/`, with a fresh cache.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a failing expansion or spawn fails the test"
)]
fn run_against(example: &Example, url: &str) -> RunOutput {
    let goldens = example.goldens().unwrap();
    let site = example.dir.join("site");
    let resolver = |asked: &Path| -> Result<String, String> {
        if asked == site {
            Ok(url.to_owned())
        } else {
            Err(format!("unexpected site {}", asked.display()))
        }
    };
    let run = example.run_args_with(&goldens, &resolver).unwrap();
    assert_eq!(run.args, [url]);
    let cache = HarnessCache::create("cli-http-fixture").unwrap();
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "run", "--"])
        .args(&run.args)
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&example.dir)
        .env("NO_COLOR", "1");
    cache.apply(&mut cmd);
    let timeout = process::scaled_timeout(TIMEOUT).unwrap();
    process::run(cmd, None, timeout).unwrap()
}

/// The status of a plain `GET path` to the server at `url`.
#[expect(
    clippy::unwrap_used,
    reason = "a test helper: a failing request fails the test"
)]
fn status_of(url: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(url.strip_prefix("http://").unwrap()).unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: fixture\r\n\r\n").as_bytes())
        .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    text.lines().next().unwrap_or("").to_owned()
}

#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn http_fixture_cli_two_ports_one_golden() {
    let (_dir, example) = package("http-fixture-cli-two-ports");
    let site = example.dir.join("site");
    let first = Server::start(&site).unwrap();
    let second = Server::start(&site).unwrap();
    assert_ne!(first.url(), second.url());
    let outputs = [first.url(), second.url()].map(|url| (run_against(&example, &url), url));
    for (out, url) in &outputs {
        assert_eq!(out.status, Some(0), "{url}: {}", out.stderr);
        assert_eq!(out.stderr, "", "{url}");
        assert_eq!(out.stdout, EXPECTED, "{url}");
        let port = url.rsplit(':').next().unwrap();
        assert!(!out.stdout.contains(port), "{url}: {}", out.stdout);
    }
    let [(a, _), (b, _)] = &outputs;
    assert_eq!(a.stdout, b.stdout);
}

/// The site's server answers `/old` with `404` and `/about` with `200`
/// to a plain request too, not only to `http::get`.
#[test]
fn http_fixture_cli_old_is_404() {
    let (_dir, example) = package("http-fixture-cli-old");
    let server = Server::start(&example.dir.join("site")).unwrap();
    assert_eq!(status_of(&server.url(), "/old"), "HTTP/1.1 404 Not Found");
    assert_eq!(status_of(&server.url(), "/about"), "HTTP/1.1 200 OK");
}
