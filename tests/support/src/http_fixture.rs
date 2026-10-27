//! The HTTP fixture of the examples (D-1027): a small static HTTP/1.1
//! server on `std::net::TcpListener`, with a thread per connection and no
//! dependency beyond `std`.
//!
//! [`url_for`] starts a server for a site directory the first time a
//! process asks, on `127.0.0.1` with a port the operating system chooses,
//! and returns `http://127.0.0.1:<port>`. Later calls in the process, from
//! any thread, share that server, so every trial of a harness run uses one
//! server per site. [`Server::start`] gives a server of its own, for a test
//! that needs two ports.
//!
//! A server serves the files under its site directory (an example's
//! `site/`, next to `cheby.toml`, D-415): a path maps to the file of the
//! same name, `/` to `index.html`, and anything else is `404`, a path that
//! would leave the directory included (D-1027).
//!
//! D-1248: it answers `GET` and `HEAD` only, and any
//! other method with `405` and `allow: GET, HEAD`; a request line that is
//! not `<method> <target> HTTP/1.<n>`, or a head that does not end within
//! 64 KiB, gets `400`. The query and fragment of the target are ignored,
//! its segments are taken literally (no percent-decoding), and an empty,
//! `.` or `..` segment, a directory, or a file whose real path is outside
//! the site is `404`. Every response has `content-type` (`text/html` for a
//! `.html` file and for `/`, `text/plain` otherwise), `content-length` and
//! `connection: close`, and the server closes each connection after its
//! response.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// The longest request head a server reads.
const MAX_HEAD: usize = 64 * 1024;

/// How long a connection may wait for its client to send or receive.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// The servers [`url_for`] started, by the canonical path of their site.
/// A static is never dropped, so they serve until the process ends.
static SERVERS: Mutex<BTreeMap<PathBuf, Server>> = Mutex::new(BTreeMap::new());

/// D-1027: the URL of the one server this process runs for `site`, started
/// on first use on `127.0.0.1` with a port the operating system chooses:
/// `http://127.0.0.1:<port>`. Every later call for the same directory,
/// from any thread, returns the same URL.
///
/// # Errors
///
/// Returns a message if `site` is not a directory or the server cannot
/// start.
pub fn url_for(site: &Path) -> Result<String, String> {
    let key = canonical_site(site)?;
    // A test that panicked while holding the lock left the map whole.
    let mut servers = SERVERS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(server) = servers.get(&key) {
        return Ok(server.url());
    }
    let server = Server::start(&key)?;
    let url = server.url();
    servers.insert(key, server);
    Ok(url)
}

/// A server of its own on `127.0.0.1` with a port the operating system
/// chooses. Dropping it stops it from accepting connections; the ones it
/// accepted finish their response.
#[derive(Debug)]
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Starts a server for the files under `site`.
    ///
    /// # Errors
    ///
    /// Returns a message if `site` is not a directory, or if the listener
    /// or its thread cannot be created.
    pub fn start(site: &Path) -> Result<Server, String> {
        let root = canonical_site(site)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| format!("http fixture: cannot listen on 127.0.0.1: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("http fixture: no local address: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        thread::Builder::new()
            .name("http-fixture".to_owned())
            .spawn(move || accept_loop(&listener, &root, &flag))
            .map_err(|e| format!("http fixture: cannot start its thread: {e}"))?;
        Ok(Server { addr, stop })
    }

    /// `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accepting thread, which then sees the flag and drops
        // the listener. Best effort: a failure leaves a thread that serves
        // until the process ends.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
    }
}

/// `site` as a canonical path, which must be a directory.
fn canonical_site(site: &Path) -> Result<PathBuf, String> {
    let root = site
        .canonicalize()
        .map_err(|e| format!("http fixture: {}: {e}", site.display()))?;
    if root.is_dir() {
        Ok(root)
    } else {
        Err(format!("http fixture: {}: not a directory", site.display()))
    }
}

/// Accepts connections until `stop` is set, each served on a thread of
/// its own.
fn accept_loop(listener: &TcpListener, root: &Path, stop: &AtomicBool) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else {
            // Out of descriptors, or a connection reset before it was
            // accepted: wait a little rather than spin.
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        let root = root.to_path_buf();
        // A connection whose thread cannot start is dropped, which closes
        // it; its client sees a reset.
        let _ = thread::Builder::new()
            .name("http-fixture-conn".to_owned())
            .spawn(move || serve(stream, &root));
    }
}

/// Reads one request from `stream`, writes its response, and closes the
/// connection.
fn serve(mut stream: TcpStream, root: &Path) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let response = match read_head(&mut stream) {
        Some(head) => respond(&head, root),
        None => Response::text(400, "bad request\n"),
    };
    let _ = stream.write_all(&response.bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// The request head up to its blank line, or `None` if the connection
/// ends or the head grows past [`MAX_HEAD`] first.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            head.truncate(end);
            return Some(String::from_utf8_lossy(&head).into_owned());
        }
        if head.len() > MAX_HEAD {
            return None;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => head.extend_from_slice(buf.get(..n)?),
        }
    }
}

/// The response to a request whose head is `head`.
fn respond(head: &str, root: &Path) -> Response {
    let line = head.lines().next().unwrap_or("");
    let parts: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = parts.as_slice() else {
        return Response::text(400, "bad request\n");
    };
    if !version.starts_with("HTTP/1.") {
        return Response::text(400, "bad request\n");
    }
    let head_only = match *method {
        "GET" => false,
        "HEAD" => true,
        _ => {
            let mut response = Response::text(405, "method not allowed\n");
            response.allow = true;
            return response;
        }
    };
    let path = target.split(['?', '#']).next().unwrap_or("");
    let mut response = match resolve(root, path) {
        Some((file, content_type)) => match std::fs::read(&file) {
            Ok(body) => Response {
                status: 200,
                content_type,
                body,
                allow: false,
                head_only: false,
            },
            Err(_) => Response::text(404, "not found\n"),
        },
        None => Response::text(404, "not found\n"),
    };
    response.head_only = head_only;
    response
}

/// The file `path` names under `root`, with its content type, or `None`
/// for a path that names no file inside `root`.
fn resolve(root: &Path, path: &str) -> Option<(PathBuf, &'static str)> {
    let rest = path.strip_prefix('/')?;
    let relative = if rest.is_empty() { "index.html" } else { rest };
    let mut file = root.to_path_buf();
    for segment in relative.split('/') {
        let leaves = segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains(['\\', ':', '\0']);
        if leaves {
            return None;
        }
        file.push(segment);
    }
    // A symbolic link may point outside the site.
    let real = file.canonicalize().ok()?;
    if !real.starts_with(root) || !real.is_file() {
        return None;
    }
    let content_type = if Path::new(relative)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
    {
        "text/html"
    } else {
        "text/plain"
    };
    Some((real, content_type))
}

/// A response the server writes.
#[derive(Debug)]
struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    /// Whether to add `allow: GET, HEAD` (a `405`).
    allow: bool,
    /// A `HEAD` request: the headers of the response without its body.
    head_only: bool,
}

impl Response {
    fn text(status: u16, body: &str) -> Response {
        Response {
            status,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
            allow: false,
            head_only: false,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "Unknown",
        };
        let allow = if self.allow {
            "allow: GET, HEAD\r\n"
        } else {
            ""
        };
        let mut out = format!(
            "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\ncontent-length: {}\r\n{allow}\
             connection: close\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        if !self.head_only {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use super::{Server, url_for};
    use crate::cache::FreshDir;

    /// A scratch directory with `site/index.html`, `site/about`,
    /// `site/blog/first-post` and a `secret` beside `site/`.
    fn site(prefix: &str) -> FreshDir {
        let dir = FreshDir::create(prefix).unwrap();
        let site = dir.path().join("site");
        std::fs::create_dir_all(site.join("blog")).unwrap();
        std::fs::write(site.join("index.html"), "<h1>Home</h1>\n").unwrap();
        std::fs::write(site.join("about"), "About us\n").unwrap();
        std::fs::write(site.join("blog/first-post"), "First post\n").unwrap();
        std::fs::write(dir.path().join("secret"), "outside the site\n").unwrap();
        dir
    }

    /// Sends `request` as it is to the server at `url` and returns the
    /// status, the head and the body.
    fn raw(url: &str, request: &str) -> (u16, String, String) {
        let addr = url.strip_prefix("http://").unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, head.to_owned(), body.to_owned())
    }

    fn get(url: &str, path: &str) -> (u16, String, String) {
        raw(
            url,
            &format!("GET {path} HTTP/1.1\r\nhost: fixture\r\n\r\n"),
        )
    }

    fn port(url: &str) -> u16 {
        url.strip_prefix("http://127.0.0.1:")
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn http_fixture_two_calls_share_one_server() {
        let dir = site("http-fixture-two-calls");
        let site = dir.path().join("site");
        let first = url_for(&site).unwrap();
        assert!(first.starts_with("http://127.0.0.1:"), "{first}");
        assert_ne!(port(&first), 0);
        // The same directory by another spelling is the same site.
        let second = url_for(&site.join("blog/..")).unwrap();
        assert_eq!(first, second);
        assert_eq!(get(&first, "/about").0, 200);
    }

    #[test]
    fn http_fixture_eight_threads_share_one_server() {
        let dir = site("http-fixture-eight-threads");
        let site = Arc::new(dir.path().join("site"));
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let site = Arc::clone(&site);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let url = url_for(&site).unwrap();
                    assert_eq!(get(&url, "/").0, 200);
                    url
                })
            })
            .collect();
        let urls: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(urls.iter().all(|u| *u == urls[0]), "{urls:?}");
        assert_eq!(url_for(&site).unwrap(), urls[0]);
    }

    #[test]
    fn http_fixture_two_sites_get_two_servers() {
        let a = site("http-fixture-site-a");
        let b = site("http-fixture-site-b");
        let url_a = url_for(&a.path().join("site")).unwrap();
        let url_b = url_for(&b.path().join("site")).unwrap();
        assert_ne!(url_a, url_b);
    }

    #[test]
    fn http_fixture_serves_the_files() {
        let dir = site("http-fixture-files");
        let server = Server::start(&dir.path().join("site")).unwrap();
        let url = server.url();
        let (status, head, body) = get(&url, "/about");
        assert_eq!(status, 200);
        assert_eq!(body, "About us\n");
        assert!(head.contains("\r\ncontent-type: text/plain\r\n"), "{head}");
        assert!(head.contains("\r\ncontent-length: 9\r\n"), "{head}");
        assert!(head.contains("\r\nconnection: close"), "{head}");
        assert_eq!(get(&url, "/blog/first-post").2, "First post\n");
        assert_eq!(get(&url, "/index.html").2, "<h1>Home</h1>\n");
        // The query and the fragment are not part of the path.
        assert_eq!(get(&url, "/about?x=1#top").2, "About us\n");
        // HEAD: the same head, no body.
        let (status, head, body) = raw(&url, "HEAD /about HTTP/1.1\r\nhost: fixture\r\n\r\n");
        assert_eq!(status, 200);
        assert!(head.contains("\r\ncontent-length: 9\r\n"), "{head}");
        assert_eq!(body, "");
        let (status, head, _) = raw(&url, "POST /about HTTP/1.1\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(status, 405);
        assert!(head.contains("\r\nallow: GET, HEAD\r\n"), "{head}");
        assert_eq!(raw(&url, "nonsense\r\n\r\n").0, 400);
    }

    #[test]
    fn http_fixture_root_is_index_html() {
        let dir = site("http-fixture-root");
        let server = Server::start(&dir.path().join("site")).unwrap();
        let (status, head, body) = get(&server.url(), "/");
        assert_eq!(status, 200);
        assert_eq!(body, "<h1>Home</h1>\n");
        assert!(head.contains("\r\ncontent-type: text/html\r\n"), "{head}");
    }

    #[test]
    fn http_fixture_anything_else_is_404() {
        let dir = site("http-fixture-404");
        let server = Server::start(&dir.path().join("site")).unwrap();
        let url = server.url();
        for path in [
            "/old", "/blog", "/blog/", "/about/", "//about", "about", "*",
        ] {
            let (status, _, body) = get(&url, path);
            assert_eq!(status, 404, "{path}");
            assert_eq!(body, "not found\n", "{path}");
        }
    }

    #[test]
    fn http_fixture_dot_dot_never_leaves_the_site() {
        let dir = site("http-fixture-dot-dot");
        let server = Server::start(&dir.path().join("site")).unwrap();
        let url = server.url();
        for path in [
            "/../secret",
            "/blog/../../secret",
            "/blog/../about",
            "/./about",
            "/..",
            "/%2e%2e/secret",
        ] {
            let (status, _, body) = get(&url, path);
            assert_eq!(status, 404, "{path}");
            assert!(!body.contains("outside"), "{path}");
        }
        // A link out of the site is not followed either.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("secret"), dir.path().join("site/link"))
                .unwrap();
            assert_eq!(get(&url, "/link").0, 404);
        }
    }

    #[test]
    fn http_fixture_needs_a_directory() {
        let dir = site("http-fixture-not-a-dir");
        let err = url_for(&dir.path().join("missing")).unwrap_err();
        assert!(err.starts_with("http fixture: "), "{err}");
        let err = Server::start(&dir.path().join("secret")).unwrap_err();
        assert!(err.ends_with("not a directory"), "{err}");
        assert!(Server::start(Path::new("")).is_err());
    }

    #[test]
    fn http_fixture_own_servers_have_their_own_ports() {
        let dir = site("http-fixture-own");
        let site = dir.path().join("site");
        let a = Server::start(&site).unwrap();
        let b = Server::start(&site).unwrap();
        assert_ne!(a.url(), b.url());
        assert_eq!(get(&a.url(), "/about").2, get(&b.url(), "/about").2);
        // Dropping one stops only that one.
        drop(a);
        assert_eq!(get(&b.url(), "/").0, 200);
    }
}
