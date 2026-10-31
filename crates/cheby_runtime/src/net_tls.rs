//! TLS for `std::http`'s client (D-1011): `rustls` with the `ring`
//! provider, sans-IO, over a `Connection`'s socket on the poller (D-983,
//! D-984). Not built on Windows, where `sys_connect_tls` is fatal until M6.
//!
//! - **Roots** ([`roots`]): built once per process, the Mozilla roots of
//!   `webpki-roots` and, only in a runtime built with debug assertions, the
//!   PEM certificates of the file that the hidden test variable names. A
//!   release runtime never reads the variable, and nothing else adds a root:
//!   no platform verifier and no `SSL_CERT_FILE` (D-1011).
//! - **The session** ([`TlsClient`]): a `rustls::ClientConnection` behind a
//!   mutex that is held for one step and never across a wait. A step runs
//!   system calls on the transport it is given until it is done or a call
//!   would block, and then says which direction to wait for ([`Step`]);
//!   `net.rs` waits on the poller and calls the step again. So the steps
//!   run the same over a socket and over the in-memory pipes of the unit
//!   tests (`net_tls_tests.rs`). GAP(tls-connection): the session lives in
//!   the `Connection` handle's state, and `read_bytes`, `write_bytes`,
//!   `shutdown_write` and `close` go through it.
//! - **Failures** ([`Failure`]): a failed system call is an IO failure; a
//!   certificate the roots do not vouch for, a name the certificate does
//!   not cover, an alert, a peer that does not speak TLS and an end of the
//!   stream in the middle of the handshake are TLS failures
//!   (GAP(tls-failure-kinds)).
//!
//! The module keeps no thread-local storage (D-686): the root store and
//! the configuration are process-wide statics.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};

use crate::sched::io::Direction;

/// The process's root store (D-1011): the roots of `webpki-roots`, plus, in
/// a runtime built with debug assertions, the PEM certificates of the file
/// that `CHEBY_RT_TEST_TLS_ROOT` names. Built once.
pub(crate) fn roots() -> &'static Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        #[cfg(debug_assertions)]
        test_root::add(&mut store);
        Arc::new(store)
    })
}

/// The test root of a debug runtime (D-1011). The variable's name and the
/// code that reads it exist only with debug assertions, so a release
/// runtime never reads it.
#[cfg(debug_assertions)]
mod test_root {
    use rustls::RootCertStore;
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    use crate::panic::fatal;

    /// The hidden variable that names the test root's PEM file.
    const VARIABLE: &str = "CHEBY_RT_TEST_TLS_ROOT";

    /// Adds every certificate of the file that [`VARIABLE`] names to
    /// `store`; nothing when it is unset. A file that cannot be read or
    /// holds no certificate is a fatal runtime error: the test that set it
    /// is broken.
    pub(super) fn add(store: &mut RootCertStore) {
        let Some(path) = std::env::var_os(VARIABLE) else {
            return;
        };
        let shown = std::path::Path::new(&path).display().to_string();
        let pem = std::fs::read(&path)
            .unwrap_or_else(|error| fatal(format_args!("{VARIABLE}: {shown}: {error}")));
        let mut added = 0_usize;
        for certificate in CertificateDer::pem_slice_iter(&pem) {
            let certificate = certificate
                .unwrap_or_else(|error| fatal(format_args!("{VARIABLE}: {shown}: {error}")));
            if let Err(error) = store.add(certificate) {
                fatal(format_args!("{VARIABLE}: {shown}: {error}"));
            }
            added = added.saturating_add(1);
        }
        if added == 0 {
            fatal(format_args!("{VARIABLE}: {shown} holds no certificate"));
        }
    }
}

/// The client configuration of every TLS connection (D-1011): the `ring`
/// provider, its safe default protocol versions (TLS 1.2 and 1.3) and
/// [`roots`]. Built once.
///
/// # Errors
///
/// `rustls`'s error if the provider offers no protocol version, which a
/// build with `tls12` and `ring` never sees.
pub(crate) fn config() -> Result<&'static Arc<ClientConfig>, Failure> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, rustls::Error>> = OnceLock::new();
    CONFIG
        .get_or_init(|| client_config(Arc::clone(roots())).map(Arc::new))
        .as_ref()
        .map_err(|error| Failure::Tls(error.to_string()))
}

/// A client configuration with the `ring` provider that trusts `roots`.
///
/// # Errors
///
/// `rustls`'s error if the provider offers no protocol version.
pub(crate) fn client_config(roots: Arc<RootCertStore>) -> Result<ClientConfig, rustls::Error> {
    Ok(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Why a TLS step failed (module documentation, "Failures").
#[derive(Debug)]
pub(crate) enum Failure {
    /// A system call failed.
    Io(io::Error),
    /// The connection could not be secured, or its records are not valid
    /// TLS: `rustls`'s text.
    Tls(String),
}

/// How far a step got.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step<T> {
    /// The step is done.
    Done(T),
    /// A system call in this direction would block: wait for it, then call
    /// the step again.
    Blocked(Direction),
}

/// A client session over one connection, its server name the host the
/// caller connected to (D-1011, D-1182).
pub(crate) struct TlsClient {
    /// Held for one step, never across a wait (module documentation).
    session: Mutex<ClientConnection>,
}

impl fmt::Debug for TlsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsClient").finish_non_exhaustive()
    }
}

/// Whether `error` means that the call would block.
fn would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

impl TlsClient {
    /// A session with `config` for the server `host`, a DNS name or an IP
    /// address; nothing is sent yet.
    ///
    /// # Errors
    ///
    /// A TLS failure for a host that is neither a DNS name nor an IP
    /// address.
    pub(crate) fn new(config: &Arc<ClientConfig>, host: &str) -> Result<Self, Failure> {
        let name = ServerName::try_from(host.to_owned())
            .map_err(|error| Failure::Tls(format!("{host}: {error}")))?;
        let session = ClientConnection::new(Arc::clone(config), name)
            .map_err(|error| Failure::Tls(error.to_string()))?;
        Ok(Self {
            session: Mutex::new(session),
        })
    }

    /// The handshake's next step over `io`: done when the session is
    /// secured and everything it queued is sent.
    ///
    /// # Errors
    ///
    /// An IO failure of `io`, or a TLS failure: the server's certificate,
    /// its name, an alert, records that are not TLS, or the end of the
    /// stream before the handshake ends.
    pub(crate) fn handshake<T: Read + Write>(&self, io: &mut T) -> Result<Step<()>, Failure> {
        let mut session = self.session.lock();
        loop {
            if let Step::Blocked(dir) = send(&mut session, io)? {
                return Ok(Step::Blocked(dir));
            }
            if !session.is_handshaking() {
                return Ok(Step::Done(()));
            }
            match session.read_tls(io) {
                Ok(0) => {
                    return Err(Failure::Tls(
                        "the server closed the connection during the handshake".to_owned(),
                    ));
                }
                Ok(_) => process(&mut session, io)?,
                Err(error) if would_block(&error) => return Ok(Step::Blocked(Direction::Read)),
                Err(error) => return Err(Failure::Io(error)),
            }
        }
    }

    /// The next step of a read into `buffer`: the count of plaintext bytes,
    /// 0 at the end of the stream.
    ///
    /// GAP(tls-connection): a server that closes the connection without
    /// `close_notify` ends the stream as one that sends it does, as
    /// HTTP/1.1's framing checks a response's length itself.
    ///
    /// # Errors
    ///
    /// An IO failure of `io`, or a TLS failure: records that are not valid.
    pub(crate) fn read<T: Read + Write>(
        &self,
        io: &mut T,
        buffer: &mut [u8],
    ) -> Result<Step<usize>, Failure> {
        let mut session = self.session.lock();
        loop {
            match session.reader().read(buffer) {
                Ok(count) => return Ok(Step::Done(count)),
                Err(error) if would_block(&error) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(Step::Done(0));
                }
                Err(error) => return Err(Failure::Io(error)),
            }
            // What the session queued while reading (a key update's answer,
            // an alert), as far as the socket takes it now; a later write
            // or read sends the rest.
            let _ = send(&mut session, io)?;
            match session.read_tls(io) {
                Ok(_) => process(&mut session, io)?,
                Err(error) if would_block(&error) => return Ok(Step::Blocked(Direction::Read)),
                Err(error) => return Err(Failure::Io(error)),
            }
        }
    }

    /// The next step of a write of `data`: what the session queued before
    /// is sent first, then as much of `data` as the session takes is
    /// encrypted and sent as far as the socket takes it now. Done with the
    /// count of bytes of `data` taken, at least one for non-empty `data`.
    ///
    /// # Errors
    ///
    /// An IO failure of `io`, or a TLS failure.
    pub(crate) fn write<T: Read + Write>(
        &self,
        io: &mut T,
        data: &[u8],
    ) -> Result<Step<usize>, Failure> {
        let mut session = self.session.lock();
        if let Step::Blocked(dir) = send(&mut session, io)? {
            return Ok(Step::Blocked(dir));
        }
        let count = session.writer().write(data).map_err(Failure::Io)?;
        let _ = send(&mut session, io)?;
        Ok(Step::Done(count))
    }

    /// The next step of sending everything the session queued: done when
    /// nothing is left.
    ///
    /// # Errors
    ///
    /// An IO failure of `io`.
    pub(crate) fn flush<T: Read + Write>(&self, io: &mut T) -> Result<Step<()>, Failure> {
        send(&mut self.session.lock(), io)
    }

    /// Queues `close_notify`, which ends the client's direction of the
    /// session; [`TlsClient::flush`] sends it.
    pub(crate) fn close_notify(&self) {
        self.session.lock().send_close_notify();
    }
}

/// Sends what `session` queued until nothing is left (done) or the
/// transport would block.
///
/// # Errors
///
/// An IO failure of `io`, a transport that takes no byte among them.
fn send<T: Write>(session: &mut ClientConnection, io: &mut T) -> Result<Step<()>, Failure> {
    while session.wants_write() {
        match session.write_tls(io) {
            Ok(0) => return Err(Failure::Io(io::ErrorKind::WriteZero.into())),
            Ok(_) => {}
            Err(error) if would_block(&error) => return Ok(Step::Blocked(Direction::Write)),
            Err(error) => return Err(Failure::Io(error)),
        }
    }
    Ok(Step::Done(()))
}

/// Processes the records `session` read. On a failure, the alert it
/// queued is sent if the transport takes it now.
///
/// # Errors
///
/// A TLS failure with `rustls`'s text.
fn process<T: Write>(session: &mut ClientConnection, io: &mut T) -> Result<(), Failure> {
    match session.process_new_packets() {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = send(session, io);
            Err(Failure::Tls(error.to_string()))
        }
    }
}

#[cfg(test)]
#[path = "net_tls_tests.rs"]
pub(crate) mod tests;
