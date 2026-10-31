//! HTTPS end to end (D-1011): a Cheby package calls `http::get` on
//! `https://localhost:<port>/` against a `rustls` server on loopback inside
//! the test, which presents the test CA's leaf
//! (`crates/cheby_runtime/tests/tls/`). With `CHEBY_RT_TEST_TLS_ROOT`
//! naming the test CA, the debug runtime trusts it and the body arrives;
//! without it, the runtime trusts only `webpki-roots`, and the request is a
//! TLS failure. Nothing reaches the network.
//!
//! Ignored on Windows, where TLS is not built and sockets are fatal until
//! M6: the tests' bodies are built only on other targets.

/// D-1011: with the test root, the debug runtime trusts the test CA, the
/// handshake for `localhost` succeeds, and the body arrives; the server
/// read a request for `/` with `localhost` as its host.
#[test]
#[cfg_attr(
    windows,
    ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
)]
fn tls_https_get_with_the_test_root_reads_the_body() {
    #[cfg(not(windows))]
    https::with_the_test_root();
}

/// D-1011: without the test root, the runtime trusts only `webpki-roots`,
/// so the test CA's leaf is a TLS failure (`http::is_tls`), and the server
/// gets no request.
#[test]
#[cfg_attr(
    windows,
    ignore = "sockets and signals on Windows are M6 (D-935, D-983)"
)]
fn tls_https_get_without_the_test_root_is_a_tls_failure() {
    #[cfg(not(windows))]
    https::without_the_test_root();
}

/// The package, the runs and the server.
#[cfg(not(windows))]
#[expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers: a package, a listener or a run that fails fails the test"
)]
mod https {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use cheby_test_support::cache::HarnessCache;
    use cheby_test_support::process::{self, RunOutput};
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::{ServerConfig, ServerConnection, StreamOwned};

    const TMP: &str = env!("CARGO_TARGET_TMPDIR");

    /// How long `cheby run`, and the server's waits, may take before the
    /// test fails.
    const PATIENCE: Duration = Duration::from_secs(120);

    /// The hidden variable of a debug runtime's test root (D-1011).
    const TEST_ROOT: &str = "CHEBY_RT_TEST_TLS_ROOT";

    /// The body the server answers with.
    const BODY: &str = "hello over TLS";

    /// The leaf for `localhost` and `127.0.0.1` and its key.
    const LEAF_PEM: &[u8] = include_bytes!("../../cheby_runtime/tests/tls/leaf.pem");
    const LEAF_KEY_PEM: &[u8] = include_bytes!("../../cheby_runtime/tests/tls/leaf-key.pem");

    /// The package: fetches its argument and prints the body, or what kind
    /// of failure it got.
    const MAIN: &str = r#"//! Fetches the URL given as the argument and prints the body or the failure.

import std::env
import std::http
import std::io

fn main() {
  case env::args() {
    [url] => case http::get(url) {
      Ok(response) => io::println("body: {response.body}")
      Err(reason) => case http::is_tls(reason) {
        True => io::println("TLS failure")
        False => io::println("other failure: {reason}")
      }
    }
    _ => io::println("usage: app <url>")
  }
}
"#;

    /// The trusted run (module documentation).
    pub(super) fn with_the_test_root() {
        let server = Server::start();
        let output = fetch("trusted", server.port, true);
        let request = server.finish();
        let shown = format!("{}\n{}", output.stdout, output.stderr);
        assert_eq!(
            output.status,
            Some(0),
            "{}: {shown}",
            output.describe_status()
        );
        assert_eq!(output.stdout, format!("body: {BODY}\n"), "{shown}");
        let request = request.unwrap_or_else(|error| panic!("the server failed: {error}"));
        assert!(request.starts_with("GET / HTTP/1.1\r\n"), "{request}");
        assert!(
            request.to_ascii_lowercase().contains("host: localhost:"),
            "{request}"
        );
    }

    /// The untrusted run (module documentation).
    pub(super) fn without_the_test_root() {
        let server = Server::start();
        let output = fetch("untrusted", server.port, false);
        let request = server.finish();
        let shown = format!("{}\n{}", output.stdout, output.stderr);
        assert_eq!(
            output.status,
            Some(0),
            "{}: {shown}",
            output.describe_status()
        );
        assert_eq!(output.stdout, "TLS failure\n", "{shown}");
        assert!(request.is_err(), "the server read a request: {request:?}");
    }

    /// The package `name` in this test binary's scratch directory.
    fn package(name: &str) -> PathBuf {
        let dir = Path::new(TMP)
            .join("tls")
            .join(format!("{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("cheby.toml"),
            "name = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.cheby"), MAIN).unwrap();
        dir
    }

    /// Runs `cheby run -- https://localhost:<port>/` in a fresh package,
    /// `with_root` deciding whether the runtime trusts the test CA.
    fn fetch(name: &str, port: u16, with_root: bool) -> RunOutput {
        let dir = package(name);
        let cache = HarnessCache::create(&format!("cli-tls-{name}")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cheby"));
        cmd.args(["--color=never", "run", "--"])
            .arg(format!("https://localhost:{port}/"))
            .current_dir(&dir)
            .env("NO_COLOR", "1");
        if with_root {
            let ca =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../cheby_runtime/tests/tls/ca.pem");
            cmd.env(TEST_ROOT, ca);
        } else {
            cmd.env_remove(TEST_ROOT);
        }
        cache.apply(&mut cmd);
        let output = process::run(cmd, None, PATIENCE).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        output
    }

    /// A server thread on loopback and its port: one connection, one
    /// request, one response, then `close_notify`.
    struct Server {
        port: u16,
        thread: JoinHandle<Result<String, String>>,
    }

    impl Server {
        /// Listens on `127.0.0.1` and serves one connection on a thread.
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let thread = thread::spawn(move || serve(&listener));
            Self { port, thread }
        }

        /// The request the server read, or why it read none.
        fn finish(self) -> Result<String, String> {
            self.thread
                .join()
                .unwrap_or_else(|_| Err("the server thread panicked".to_owned()))
        }
    }

    /// The server's configuration: the `ring` provider and the leaf.
    fn config() -> Result<Arc<ServerConfig>, String> {
        let chain = CertificateDer::pem_slice_iter(LEAF_PEM)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let key = PrivateKeyDer::from_pem_slice(LEAF_KEY_PEM).map_err(|error| error.to_string())?;
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|error| error.to_string())?
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .map_err(|error| error.to_string())?;
        Ok(Arc::new(config))
    }

    /// The first connection to `listener`, waited for at most
    /// [`PATIENCE`].
    fn accept(listener: &TcpListener) -> Result<TcpStream, String> {
        let started = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => return Ok(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > PATIENCE {
                        return Err("no connection came".to_owned());
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    /// Serves one connection: the request's head, then a response with
    /// [`BODY`] and `close_notify`.
    fn serve(listener: &TcpListener) -> Result<String, String> {
        let stream = accept(listener)?;
        let failed = |error: std::io::Error| error.to_string();
        stream.set_nonblocking(false).map_err(failed)?;
        stream.set_read_timeout(Some(PATIENCE)).map_err(failed)?;
        let session = ServerConnection::new(config()?).map_err(|error| error.to_string())?;
        let mut tls = StreamOwned::new(session, stream);
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match tls.read(&mut byte).map_err(failed)? {
                0 => return Err("the client closed before its request ended".to_owned()),
                _ => request.extend_from_slice(&byte),
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BODY}",
            BODY.len()
        );
        tls.write_all(response.as_bytes()).map_err(failed)?;
        tls.conn.send_close_notify();
        tls.flush().map_err(failed)?;
        Ok(String::from_utf8_lossy(&request).into_owned())
    }
}
