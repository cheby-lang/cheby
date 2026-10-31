//! Tests of the TLS core (D-1011): a [`TlsClient`] paired in memory with a
//! `rustls::ServerConnection`, no socket and no scheduler. The server
//! presents the test CA's leaf for `localhost` and `127.0.0.1`
//! (`crates/cheby_runtime/tests/tls/`); the client trusts the test CA, or
//! only `webpki-roots`, through its own configuration, never through the
//! process's [`roots`] and its test variable. Every test is offline.
//! GAP(tls-test-ca): the README there gives the OpenSSL commands that made
//! the certificates; the CA's key was deleted after it signed the leaf.
//!
//! The socket-level tests (the poller's waits, a cancelled handshake) are
//! `net_tests.rs`'s `tls_*`, which reuse [`server_config`] and
//! [`trusting_test_ca`].

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection};

use super::*;

/// The test CA (`tests/tls/README.md`).
pub(crate) const CA_PEM: &[u8] = include_bytes!("../tests/tls/ca.pem");
/// The leaf for `localhost` and `127.0.0.1`, signed by the test CA.
const LEAF_PEM: &[u8] = include_bytes!("../tests/tls/leaf.pem");
/// The leaf's private key.
const LEAF_KEY_PEM: &[u8] = include_bytes!("../tests/tls/leaf-key.pem");

/// A server configuration with the `ring` provider that presents the
/// leaf.
pub(crate) fn server_config() -> Arc<ServerConfig> {
    let chain = CertificateDer::pem_slice_iter(LEAF_PEM)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(LEAF_KEY_PEM).unwrap();
    Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
    )
}

/// A client configuration that trusts the test CA only.
pub(crate) fn trusting_test_ca() -> Arc<ClientConfig> {
    let mut store = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(CA_PEM) {
        store.add(certificate.unwrap()).unwrap();
    }
    Arc::new(client_config(Arc::new(store)).unwrap())
}

/// A client configuration that trusts the roots of `webpki-roots` only,
/// as a release runtime does.
fn trusting_webpki_roots() -> Arc<ClientConfig> {
    let mut store = RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(client_config(Arc::new(store)).unwrap())
}

/// The client's end of an in-memory connection: what the server sent and
/// the client has not read, and what the client wrote. A read with
/// nothing to read would block, as a non-blocking socket's does, until the
/// server's end is closed.
#[derive(Debug, Default)]
struct Pipe {
    incoming: VecDeque<u8>,
    outgoing: Vec<u8>,
    closed: bool,
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.incoming.is_empty() {
            return if self.closed {
                Ok(0)
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            };
        }
        self.incoming.read(buffer)
    }
}

impl Write for Pipe {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.outgoing.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A client and a server joined in memory.
struct Pair {
    client: TlsClient,
    pipe: Pipe,
    server: ServerConnection,
    /// The server's last failure, if it failed.
    server_failure: Option<rustls::Error>,
}

impl Pair {
    /// A client for `host` with `config` and a server with the leaf.
    fn new(config: &Arc<ClientConfig>, host: &str) -> Self {
        Self {
            client: TlsClient::new(config, host).unwrap(),
            pipe: Pipe::default(),
            server: ServerConnection::new(server_config()).unwrap(),
            server_failure: None,
        }
    }

    /// Moves what the client wrote to the server, lets it process, and
    /// moves what it answers to the client.
    fn exchange(&mut self) {
        let sent = std::mem::take(&mut self.pipe.outgoing);
        let mut sent = sent.as_slice();
        while !sent.is_empty() {
            self.server.read_tls(&mut sent).unwrap();
            if let Err(error) = self.server.process_new_packets() {
                self.server_failure = Some(error);
                break;
            }
        }
        let mut answer = Vec::new();
        while self.server.wants_write() {
            self.server.write_tls(&mut answer).unwrap();
        }
        self.pipe.incoming.extend(answer);
    }

    /// Runs the client's handshake against the server: its outcome, after
    /// at most a few rounds (an in-memory pipe blocks only reads).
    fn handshake(&mut self) -> Result<(), Failure> {
        for _ in 0..16 {
            let step = self.client.handshake(&mut self.pipe);
            self.exchange();
            if step? == Step::Done(()) {
                return Ok(());
            }
        }
        Err(Failure::Io(io::Error::other("the handshake did not end")))
    }

    /// Reads what the client can read now, at most `limit` bytes.
    fn read(&mut self, limit: usize) -> Step<Vec<u8>> {
        let mut buffer = vec![0_u8; limit];
        match self.client.read(&mut self.pipe, &mut buffer).unwrap() {
            Step::Done(count) => {
                buffer.truncate(count);
                Step::Done(buffer)
            }
            Step::Blocked(dir) => Step::Blocked(dir),
        }
    }
}

/// The text of a TLS failure, or a panic for anything else.
fn tls_text(outcome: Result<(), Failure>) -> String {
    match outcome {
        Err(Failure::Tls(text)) => text,
        other => panic!("not a TLS failure: {other:?}"),
    }
}

/// D-1011: a server whose certificate the roots vouch for, under the name
/// the client asked for, secures the connection, by DNS name and by IP
/// address; data goes both ways, and the server's `close_notify` ends the
/// client's stream.
#[test]
fn tls_trusted_server_connects() {
    for host in ["localhost", "127.0.0.1"] {
        let mut pair = Pair::new(&trusting_test_ca(), host);
        pair.handshake().unwrap();
        assert!(pair.server_failure.is_none(), "{host}");
        assert!(!pair.server.is_handshaking(), "{host}");

        assert_eq!(
            pair.client
                .write(&mut pair.pipe, b"GET / HTTP/1.1\r\n\r\n")
                .unwrap(),
            Step::Done(18)
        );
        assert_eq!(pair.client.flush(&mut pair.pipe).unwrap(), Step::Done(()));
        pair.exchange();
        let mut request = vec![0_u8; 64];
        let count = pair.server.reader().read(&mut request).unwrap();
        assert_eq!(&request[..count], b"GET / HTTP/1.1\r\n\r\n", "{host}");

        assert_eq!(pair.read(64), Step::Blocked(Direction::Read), "{host}");
        pair.server
            .writer()
            .write_all(b"HTTP/1.1 200 OK\r\n")
            .unwrap();
        pair.server.send_close_notify();
        pair.exchange();
        assert_eq!(
            pair.read(64),
            Step::Done(b"HTTP/1.1 200 OK\r\n".to_vec()),
            "{host}"
        );
        assert_eq!(pair.read(64), Step::Done(Vec::new()), "{host}: the end");
    }
}

/// D-1011: a server whose certificate no root vouches for (the test CA,
/// which a release runtime does not trust) is a TLS failure, and the
/// server hears the client's alert.
#[test]
fn tls_untrusted_root_is_a_tls_failure() {
    let mut pair = Pair::new(&trusting_webpki_roots(), "localhost");
    let text = tls_text(pair.handshake());
    assert!(text.contains("UnknownIssuer"), "{text}");
    assert!(pair.server_failure.is_some(), "the server heard no alert");
}

/// D-1011: a certificate that does not cover the name the client asked
/// for is a TLS failure, though the root vouches for it.
#[test]
fn tls_wrong_name_is_a_tls_failure() {
    for host in ["example.com", "127.0.0.2"] {
        let mut pair = Pair::new(&trusting_test_ca(), host);
        let text = tls_text(pair.handshake());
        assert!(
            text.contains(&format!("certificate not valid for name \"{host}\"")),
            "{host}: {text}"
        );
    }
}

/// GAP(tls-failure-kinds): a peer that answers in plain HTTP is a TLS
/// failure, not an IO failure, and so is the end of the stream in the
/// middle of the handshake.
#[test]
fn tls_a_plain_peer_and_an_early_end_are_tls_failures() {
    let config = trusting_test_ca();
    let mut client = TlsClient::new(&config, "localhost").unwrap();
    let mut pipe = Pipe::default();
    assert_eq!(
        client.handshake(&mut pipe).unwrap(),
        Step::Blocked(Direction::Read)
    );
    assert!(!pipe.outgoing.is_empty(), "no ClientHello");
    pipe.incoming
        .extend(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
    let _ = tls_text(client.handshake(&mut pipe).map(|_| ()));

    client = TlsClient::new(&config, "localhost").unwrap();
    pipe = Pipe::default();
    assert_eq!(
        client.handshake(&mut pipe).unwrap(),
        Step::Blocked(Direction::Read)
    );
    pipe.closed = true;
    let text = tls_text(client.handshake(&mut pipe).map(|_| ()));
    assert!(text.contains("during the handshake"), "{text}");
}

/// A host that is neither a DNS name nor an IP address cannot be a server
/// name: a TLS failure before anything is sent.
#[test]
fn tls_a_host_that_is_no_server_name_is_a_tls_failure() {
    match TlsClient::new(&trusting_test_ca(), "not a name") {
        Err(Failure::Tls(text)) => assert!(text.contains("not a name"), "{text}"),
        other => panic!("{other:?}"),
    }
}

/// An IO failure of the transport stays an IO failure.
#[test]
fn tls_a_failed_system_call_is_an_io_failure() {
    /// A transport whose every call fails with a reset.
    struct Reset;
    impl Read for Reset {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::ConnectionReset.into())
        }
    }
    impl Write for Reset {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::ConnectionReset.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let client = TlsClient::new(&trusting_test_ca(), "localhost").unwrap();
    match client.handshake(&mut Reset) {
        Err(Failure::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::ConnectionReset),
        other => panic!("{other:?}"),
    }
}

/// D-1011: the process's roots are built once and hold every root of
/// `webpki-roots`; the configuration is built once over them.
#[test]
fn tls_roots_and_config_are_built_once() {
    assert!(std::ptr::eq(roots(), roots()));
    assert!(roots().len() >= webpki_roots::TLS_SERVER_ROOTS.len());
    let first = config().unwrap();
    assert!(std::ptr::eq(first, config().unwrap()));
}
