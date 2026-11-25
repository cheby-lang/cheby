//! Sessions with a server, for an example that serves (D-1892).
//!
//! `session` holds a request per line, `METHOD PATH` and, after a space, a
//! body. Empty lines and lines that start with `#` are skipped.
//! `expected.session` holds the response to each, its status and its body.
//! The requests are sent one after another, each on a connection of its own.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::process;

/// How long a server may take to listen, and a response to arrive, before
/// `CHEBY_HARNESS_SCALE`.
const WAIT: Duration = Duration::from_secs(20);

/// A server, stopped when dropped.
#[derive(Debug)]
struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs the server that `command` makes for a port, sends it the requests of
/// `session`, stops it, and returns the responses as `expected.session` holds
/// them.
///
/// # Errors
///
/// A message when the server does not listen or a response does not arrive,
/// and when the server wrote anything, with what it wrote.
pub fn run(session: &str, command: impl FnOnce(u16) -> Command) -> Result<String, String> {
    let wait = process::scaled_timeout(WAIT)?;
    let reserved = Port::reserve()?;
    let port = reserved.number;
    let mut cmd = command(port);
    process::clear_inherited(&mut cmd);
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {cmd:?}: {e}"))?;
    let mut server = Server { child };
    let outcome = listening(&mut server, port, wait).and_then(|()| {
        let mut out = String::new();
        for line in session
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            out.push_str(&request(line, port, wait)?);
            out.push('\n');
        }
        Ok(out)
    });
    let _ = server.child.kill();
    let mut said = String::new();
    if let Some(mut stdout) = server.child.stdout.take() {
        let _ = stdout.read_to_string(&mut said);
    }
    if let Some(mut stderr) = server.child.stderr.take() {
        let _ = stderr.read_to_string(&mut said);
    }
    match outcome {
        Ok(out) if said.is_empty() => Ok(out),
        Ok(_) => Err(format!("the server wrote:\n{said}")),
        Err(e) => Err(format!("{e}\nthe server wrote:\n{said}")),
    }
}

/// The ports of sessions. No system hands these out by itself.
const PORTS: std::ops::Range<u16> = 20_000..30_000;

/// A TCP port that nothing listens on and no other session has while this
/// lives.
struct Port {
    number: u16,
    /// What other sessions see: the UDP port of the same number, which a TCP
    /// listener does not use and the system frees when the process ends.
    _reserved: UdpSocket,
}

impl Port {
    fn reserve() -> Result<Self, String> {
        // Trials that run at once start at different ports.
        let skipped = usize::try_from(std::process::id() % 10_000).unwrap_or(0);
        for number in PORTS.cycle().skip(skipped).take(PORTS.len()) {
            let Ok(reserved) = UdpSocket::bind((Ipv4Addr::LOCALHOST, number)) else {
                continue;
            };
            // A server on every address leaves this one free to bind.
            if TcpStream::connect((Ipv4Addr::LOCALHOST, number)).is_err()
                && TcpListener::bind((Ipv4Addr::LOCALHOST, number)).is_ok()
            {
                return Ok(Self {
                    number,
                    _reserved: reserved,
                });
            }
        }
        Err("no free port".to_owned())
    }
}

fn listening(server: &mut Server, port: u16, wait: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = server.child.try_wait() {
            return Err(format!("the server ended: {status}"));
        }
        if start.elapsed() > wait {
            return Err("the server does not listen".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Sends the request of `line` and returns the status and the body.
fn request(line: &str, port: u16, wait: Duration) -> Result<String, String> {
    let mut parts = line.splitn(3, ' ');
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(format!("`{line}` is no request"));
    };
    let body = parts.next().unwrap_or("");
    let failed = |e: std::io::Error| format!("{method} {path}: {e}");
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, wait).map_err(failed)?;
    stream.set_read_timeout(Some(wait)).map_err(failed)?;
    stream.set_write_timeout(Some(wait)).map_err(failed)?;
    let text = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes()).map_err(failed)?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).map_err(failed)?;
    let (head, content) = answer
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{method} {path}: the answer has no end of its head"))?;
    let status = head
        .split(' ')
        .nth(1)
        .ok_or_else(|| format!("{method} {path}: the answer has no status"))?;
    Ok(format!("{status} {content}"))
}
