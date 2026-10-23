# `std::http`

An HTTP/1.1 client and server. Example 031 fetches pages with [`get`](#get) (M3, D-477), and example 030 serves a JSON API with [`serve`](#serve) (M5, D-446). In the form `cheby doc` prints:

```cheby
pub exposed type Method {
  Get,
  Head,
  Post,
  Put,
  Patch,
  Delete,
  Options,
  Other(String),
}

pub exposed type Request {
  method: Method,
  path: String,
  segments: List<String>,
  query: List<(String, String)>,
  headers: List<(String, String)>,
  body: String,
}

pub exposed type Response {
  status: Int,
  headers: List<(String, String)>,
  body: String,
}

pub type Error
```

- **Plain data.** `Method`, `Request` and `Response` are `exposed`, so other packages read their fields, build them with record syntax and match `http::Get` in patterns, as 030 and 031 do ([§4.3.4](../spec/04-declarations.md#434-opacity-and-exposed), D-048). They hold no handle, so `==`, hashing and debug printing are the built-in structural ones, and 030 can compare routes and lists of methods with `==`. Their fields and variants freeze at 1.0 (D-075, D-1004).
- **`Method`.** The request methods of RFC 9110 that programs name, and `Other` for any other token, kept exactly as received, such as `Other("PURGE")`. A token that names one of the other variants is always that variant: the server never produces `Other("GET")` (D-1005).
- **`Request`.** What a handler of [`serve`](#serve) receives (D-1004):
  - `path` is the path of the request target as received, without the query, such as `/orders/17` or `/a%20b`.
  - `segments` is `path` split at `/`, with empty segments dropped and each segment decoded by [`url::decode_segment`](url.md#decode_segment), so `/orders//17/` gives `["orders", "17"]` and `/a%2Fb` gives `["a/b"]`. `.` and `..` are kept as they are (D-1069).
  - `query` is the query string as name and value pairs, in order, with repeated names kept, decoded by [`url::decode_query`](url.md#decode_query) as an HTML form decodes it: `+` is a space and `%XX` is a byte. A pair without `=` has the value `""` (D-1069).
  - `headers` and `body` follow the rules below. A form body, sent as `application/x-www-form-urlencoded`, is decoded with the same decoder, as `url::decode_query(request.body)` (D-1069).
- **`Response`.** What a handler returns, and what [`get`](#get) and [`send`](#send) return. `status` is the status code. A response built with record syntax is as valid as one from [`json`](#json) or [`text`](#text).
- **Headers.** A header list holds name and value pairs in the order they were sent, with repeated names kept, as `set-cookie` needs. Names that `std::http` produces or receives are lower-case ASCII, as HTTP/2 and `fetch` write them. [`header`](#header) and [`request_header`](#request_header) match names without regard to ASCII case, so a list built by hand with `Content-Type` is still found (D-1006).
- **Bodies are text.** `body` is a `String`. A response body that is not valid UTF-8 makes `get` return `Err`, and a request body, path or query that does not decode to valid UTF-8 makes `serve` answer `400` without calling the handler (D-311). Binary bodies come later through new functions, since the records cannot gain fields after 1.0 (D-1007).
- **`Error`.** Opaque, because network failures follow the operating system and the host (D-304, ADR-0051). Callers ask the predicates under [Errors](#errors), and reach the IO failure underneath with [`io_error`](#io_error). Its `show` gives a fixed text for each kind, and debug printing gives the same text, so 031's `"{reason:?}"` and 030's `{reason:?}` print `connection refused`, not internal fields (D-134, D-345). Two errors are `==` when their kinds are the same and, for an IO failure, their `io::Error`s are `==`, through `@std_hooks` (D-342, D-368, D-1016).
- **Protocol.** HTTP/1.1 over TCP, from [`std::net`](net.md). The client also speaks HTTPS (D-1011). There is no HTTP/2 or HTTP/3, and the server speaks only plain HTTP.
- **Written in Cheby.** On native targets every function is written in Cheby over `std::net`, `std::fiber` and private kernels that parse a request or response head and run the TLS state machine. A handler is therefore an ordinary Cheby call on a fiber, and every wait is a socket wait on the poller, which `fiber::timeout` can cancel (D-484, D-675, D-1003, ADR-0066). [How requests run on fibers](#how-requests-run-on-fibers) shows the structure.
- **Constants.** [`json`](#json), [`text`](#text), [`with_header`](#with_header), [`add_header`](#add_header), [`header`](#header), [`request_header`](#request_header) and [`method_name`](#method_name) are Cheby over `std::json`, `std::list` and `std::string`, so they run in constants, and a constant may hold a `Response`. The client and the server do not: reaching them while computing a constant is E0077 (D-090, D-537, D-991).
- **`std::json` inside this module.** `std/http.cheby` writes `import std::json as jsons`, because its function `json` and the module name `json` would share the value namespace (§7.3, §7.4, D-111). The plural follows std's own aliases, such as `import std::map as maps` in `std/set.cheby` and `import std::list as lists` in `std/json.cheby`. Signatures on this page name the type `json::Value`, as `cheby doc` prints it. The tests on this page are written as they sit in `std/http.cheby` apart from the `http::` prefixes (D-662), so they write `jsons::`. A reader who copies one into a package writes `import std::json as jsons` too.
- **`std::url` inside this module.** `std/http.cheby` writes `import std::url as urls`, because [`get`](#get) and [`send`](#send) take a parameter `url`, and a binding must not have the name of an imported module (D-062, D-1053). The server's decoders and the encoders a caller uses to build a URL are the functions of [`std::url`](url.md), so the two cannot drift (D-1069). Code on this page writes `urls::`, as `std/http.cheby` does.
- **On JS.** OQ-P14, with M4. The direction: `get` and `send` use the host's `fetch` through a private `@async` kernel, and `serve` takes requests from the host's server API through a queue that a Cheby loop reads, so handlers still run inside `serve`'s scope (D-207). The native client is shaped so that `fetch` can match it: it follows redirects, encodes URLs as the WHATWG parser does, lower-cases header names and decodes bodies strictly (D-1070).

The tests that need a server start one on the loopback address, with a port the operating system chose, in a detached fiber that `cheby test` tears down when the test ends (D-723, D-990). Requests to other machines have an _Example_.

## Client

Every status is `Ok`, so a caller such as 031 sees its `404` with its body (D-297). Redirects with status 301, 302, 303, 307 and 308 are followed, at most 10 in a row, as `fetch` does: 303, and 301 or 302 after a method other than `GET` or `HEAD`, continue as `GET` without a body, and `authorization` and `cookie` headers are dropped when a redirect leaves the origin. The client has no timeout of its own: a caller wraps it in `fiber::timeout`. Each request opens one connection and closes it afterwards. No content encoding is asked for, so bodies arrive uncompressed (D-232, D-1008).

The client reads a response head of at most 64 KiB, the server's own limit, and a longer head is `Err` for which [`is_invalid_response`](#is_invalid_response) holds. The number is informative. Node's own limit is 16 KiB, so on JS a head between 16 and 64 KiB may fail there first. A body has no limit in tier 3: a body larger than memory ends the program with `out of memory` ([§11.8.2](../spec/11-errors-and-panics.md#1182-out-of-memory), D-642, D-1074).

### `get`

```cheby
pub fn get(url: String) -> Result<Response, Error>
```

Sends a `GET` request for `url` and returns the final response, after following redirects. It is `send(Get, url, [], "")` (D-1009).

A `url` is absolute, with the scheme `http` or `https` in any case, a host name, an IPv4 address or an IPv6 address in brackets, an optional port, and an optional path and query. A fragment, such as `#top`, is not sent. A host name outside ASCII is an invalid URL until the client converts such names (D-1008).

The client sends the path and the query as `fetch` does, after the WHATWG URL parser (D-1070):

- It percent-encodes the path with WHATWG's path percent-encode set and the query with the query percent-encode set that the parser applies to `http` and `https` URLs, so a space, `"`, `<` and `>` are encoded in both, and so is every character outside ASCII, as UTF-8.
- An existing `%XX` is sent as it is, so a URL that is already encoded is not encoded twice.
- `.` and `..` segments are removed from the path, also when they are written as `%2e` or `%2E`, so `/a/./b/../c` is sent as `/a/c`.

This does not separate the parts of a query: `"{base}/search?q={term}"` with `term = "a&b=c"` still sends two parameters. A caller builds the query with [`url::encode_query`](url.md#encode_query) and a segment with [`url::encode_segment`](url.md#encode_segment) (D-1069).

> **Open (OQ-http-1):** international domain names. `fetch` accepts a host name outside ASCII, and the native client rejects it as an invalid URL, so the two targets differ (D-232). _Proposed:_ until the native client converts such names, the JS kernel rejects them too, with `is_invalid_url`, so both targets give the same result. M4 settles this with the JS side (OQ-P14).

- **Fails:** `Err` for which [`is_invalid_url`](#is_invalid_url) holds when `url` is not such a URL, including a relative one or one with a user name. `Err` with an IO failure ([`io_error`](#io_error)) when the host cannot be found, the connection is refused or reset, or the operating system fails otherwise. `Err` for which [`is_tls`](#is_tls) holds when an HTTPS connection cannot be secured, [`is_invalid_response`](#is_invalid_response) when the server's answer is not HTTP/1.1, its head is over 64 KiB or its body is not valid UTF-8 (D-1074), and [`is_too_many_redirects`](#is_too_many_redirects) after 10 redirects in a row.
- **Cost:** O(n) in the size of the response, plus the network. Informative.

```cheby
test "get accepts only absolute http and https URLs" {
  let assert Err(reason) = http::get("ftp://example.com/")
  assert http::is_invalid_url(reason)
  assert "{reason}" == "invalid URL"
  assert "{reason:?}" == "invalid URL"
  let assert Err(relative) = http::get("/just/a/path")
  assert http::is_invalid_url(relative)
}

test "a server answers a client over loopback" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() {
    http::serve_on(listener, fn(request) {
      case request.method, request.segments {
        http::Get, ["greet", name] => http::text(200, "hello {name}")
        _, _ => http::text(404, "no such page")
      }
    })
  })
  let base = "http://127.0.0.1:{port}"
  let assert Ok(found) = http::get(base + "/greet/ada")
  assert found.status == 200
  assert found.body == "hello ada"
  assert http::header(found, "content-type") == Some("text/plain; charset=utf-8")
  let assert Ok(missing) = http::get(base + "/nowhere")
  assert missing.status == 404
  assert missing.body == "no such page"
}

test "redirects are followed, and a loop of them ends" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() {
    http::serve_on(listener, fn(request) {
      case request.path {
        "/old" => http::text(301, "") |> http::with_header(_, "location", "/new")
        "/new" => http::text(200, "moved here")
        _ => http::text(302, "") |> http::with_header(_, "location", "/loop")
      }
    })
  })
  let assert Ok(moved) = http::get("http://127.0.0.1:{port}/old")
  assert moved.status == 200
  assert moved.body == "moved here"
  let assert Err(reason) = http::get("http://127.0.0.1:{port}/loop")
  assert http::is_too_many_redirects(reason)
}

test "the client encodes a URL as fetch does" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() {
    http::serve_on(listener, fn(request) { http::text(200, request.path) })
  })
  let assert Ok(response) = http::get("http://127.0.0.1:{port}/a b/./x/%2e%2E/y/c%2Fd")
  assert response.body == "/a%20b/y/c%2Fd"
}
```

_Examples_, not `test`s because they need the internet. The second builds its query with `std::url`, so a `term` such as `a&b=c` stays one parameter (D-1069):

```cheby
fn status_of(url: String) -> String {
  case http::get(url) {
    Ok(response) => "{response.status}"
    Err(reason) => "failed: {reason}"
  }
}

fn search(base: String, term: String) -> Result<http::Response, http::Error> {
  http::get(base + "/search?" + urls::encode_query([("q", term)]))
}
```

### `send`

```cheby
pub fn send(method: Method, url: String, headers: List<(String, String)>, body: String) -> Result<Response, Error>
```

Sends a request with `method`, `headers` and `body` to `url`, and returns the final response, after following redirects. The client sets `host`, `content-length` and `connection` itself and ignores those names in `headers`. It also sends `user-agent: cheby` and `accept: */*` unless `headers` names them. `body` is sent as UTF-8. An empty `body` sends no `content-length` for `GET`, `HEAD`, `DELETE` and `OPTIONS`. The function is not named `request`, because the module needs a parameter of that name (D-051, D-1009).

- **Fails:** the cases of [`get`](#get), and `Err` for which [`is_invalid_header`](#is_invalid_header) holds when a header name is not an HTTP token, a value contains a line break or U+0000, or the method of `Other` is not a token. Invalid headers are found before any connection is made.
- **Cost:** O(n) in the size of the request and the response, plus the network. Informative.

```cheby
test "send rejects a header that could split the request" {
  let assert Err(reason) = http::send(http::Get, "http://127.0.0.1:9/", [("x-note", "a\r\nb")], "")
  assert http::is_invalid_header(reason)
  assert "{reason}" == "invalid header"
}

test "send carries a method, headers and a body" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() {
    http::serve_on(listener, fn(request) {
      let token = option::unwrap_or(http::request_header(request, "X-Token"), "none")
      http::text(200, http::method_name(request.method) + " " + token + " " + request.body)
    })
  })
  let url = "http://127.0.0.1:{port}/orders"
  let assert Ok(response) = http::send(http::Post, url, [("x-token", "abc")], "one tea")
  assert response.body == "POST abc one tea"
}
```

## Server

A server must survive a bad request. Each connection is a fiber in a scope that the server opens, and each request's handler runs inside a scope of its own on that fiber. A handler that panics therefore fails only its own scope: its request gets `500`, its panic report is printed to standard error as a detached fiber's is (D-669), and nothing else is cancelled. This is the one place in std where a panic does not cancel its siblings (D-040, §10.3). A cancellation from outside the server, such as a `fiber::timeout` around it, still cancels every connection and handler (D-1012, ADR-0067).

> **Open (OQ-http-2):** whether a handler's panic report should also name the request's method and path. _Proposed:_ the report is the plain report of a detached fiber, with the panic's message and location only, as written below (D-669, D-1012).

### `serve`

```cheby
pub fn serve(port: Int, handler: fn(Request) -> Response) -> Result<Nil, Error>
```

Listens on `port` on every interface, and answers each request with `handler`, as [`serve_on`](#serve_on) does. It listens on `::` and accepts IPv4 connections on the same socket, or on `0.0.0.0` where the host has no IPv6 (D-1015). It returns only when listening fails, or accepting fails in a way that does not pass. 030 calls it with the port from `PORT`.

- **Fails:** `Err` with an IO failure ([`io_error`](#io_error)) for which `io::is_invalid_input` holds when `port` is outside 0 to 65535, `io::is_address_in_use` when the port is taken, and `io::is_permission_denied` when the program may not use it, and for every other failure of the operating system in listening or accepting.
- **Cost:** per request, O(n) in the size of the request and the response, plus `handler`. Informative.

```cheby
test "serve rejects a port out of range" {
  let assert Err(reason) = http::serve(70_000, fn(_) { http::text(200, "") })
  let assert Some(cause) = http::io_error(reason)
  assert io::is_invalid_input(cause)
}
```

_Example_, not a `test` because it never returns:

```cheby
fn main() -> Result<Nil, http::Error> {
  http::serve(8080, fn(request) {
    case request.method, request.segments {
      http::Get, [] => http::text(200, "home")
      _, _ => http::text(404, "no such page")
    }
  })
}
```

### `serve_on`

```cheby
pub fn serve_on(listener: net::Listener, handler: fn(Request) -> Response) -> Result<Nil, Error>
```

Answers each request that arrives on `listener` with `handler`. A program that needs to know its port, such as a test, listens on port 0 with `net::listen`, reads `net::local_port` and then calls `serve_on` (D-1014).

Every connection is a fiber in a scope that `serve_on` opens. It reads requests one after another, keeping the connection open between them unless the client asks to close it. Each request's `handler` call runs in a scope of its own on that fiber (D-1012). The limits below are informative (D-1013):

1. **Before the handler.** A request whose head is not valid HTTP/1.1 gets `400`, a head over 64 KiB gets `431`, a body over 10 MiB gets `413`, a transfer coding other than `chunked` gets `501`, and a path, query or body that does not decode to valid UTF-8 gets `400`. The connection is then closed, and `handler` is not called. A request with `expect: 100-continue` gets `100 Continue` before its body is read.
2. **The handler.** `handler` is called with the `Request` on the connection's fiber. A `HEAD` request reaches it as `Head`, and its response is sent without the body.
3. **A panic.** If `handler` panics, the request gets `500` with the body `internal server error`, the panic report is printed to standard error, and the connection stays open for the next request. No other request is affected.
4. **The response.** The status line, then the response's headers in order, then `content-length` and `date`, which the server sets itself and which replace any `content-length`, `transfer-encoding`, `connection` or `date` in `headers`. A status outside 100 to 999, or a header that [`send`](#send) would reject, is replaced by `500` with the body `internal server error`, so a handler can never split a response.
5. **Idle connections.** A connection with no new request for 60 seconds is closed, and so is one whose request head is not complete 30 seconds after it began.
6. **Errors of accepting.** A failure to accept that passes, such as too many open files or a connection aborted before it was accepted, is retried after a pause of 100 milliseconds. Any other failure ends `serve_on`: it cancels every connection and every handler in flight with `fiber::cancel` (D-1060), waits until they have unwound, and returns `Err`. It does not wait for idle connections to reach their limit.
7. **A closed listener.** When accepting fails with `io::is_closed`, because the program closed `listener` with [`net::close_listener`](net.md#close_listener), `serve_on` ends in the same way, but returns `Ok(Nil)`: closing the listener is how a program, or one of its own handlers, stops a server on purpose. A handler that closes its own server's listener is cancelled with the rest, so its response may not be sent. Requests in flight are cancelled, not drained (D-1063, D-1064).
8. **Cancellation.** When the fiber that called `serve_on` is cancelled, every connection and every handler in flight is cancelled with it ([§10.7](../spec/10-concurrency.md#107-cancellation)).

- **Fails:** `Err` with an IO failure when accepting fails in a way that does not pass, other than a closed listener.
- **Cost:** per request, O(n) in the size of the request and the response, plus `handler`. Informative.

```cheby
test "segments and query are decoded" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() {
    http::serve_on(listener, fn(request) {
      let pairs = list::map(request.query, fn((key, value)) { key + "=" + value })
      http::text(200, string::join(request.segments, "|") + " " + string::join(pairs, "&"))
    })
  })
  let assert Ok(response) = http::get("http://127.0.0.1:{port}/a%2Fb//c/?x=1&y=two+words&flag")
  assert response.body == "a/b|c x=1&y=two words&flag="
}

test "a handler that panics gives that request a 500 and nothing else" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() {
    http::serve_on(listener, fn(request) {
      case request.path {
        "/crash" => panic as "handler bug"
        _ => http::text(200, "fine")
      }
    })
  })
  let base = "http://127.0.0.1:{port}"
  let assert Ok(crashed) = http::get(base + "/crash")
  assert crashed.status == 500
  let assert Ok(healthy) = http::get(base + "/")
  assert healthy.status == 200
}

test "closing the listener stops serve_on" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  let assert Ok(outcome) = fiber::scope(fn(owner) {
    let server = fiber::spawn(owner, fn() {
      http::serve_on(listener, fn(_) { http::text(200, "up") })
    })
    let assert Ok(first) = http::get("http://127.0.0.1:{port}/")
    assert first.body == "up"
    let assert Ok(Nil) = net::close_listener(listener)
    fiber::join(server)
  })
  assert outcome == Ok(Nil)
}
```

## Building responses

`std::http` has two builders, [`json`](#json) and [`text`](#text), [`with_header`](#with_header) to set a header of any response, and [`add_header`](#add_header) to add one beside others of the same name (D-1017, D-1052). `with_header` returns a copy with one part replaced, the meaning of a `with_` function whose last parameter is not a callback (D-1051).

### `json`

```cheby
pub fn json(status: Int, value: json::Value) -> Response
```

Returns a response with `status`, the header `content-type: application/json`, and `value` as compact JSON text, exactly as `json::to_string` writes it.

- **Cost:** O(n) in the size of `value`. Informative.

```cheby
test "json builds a response with a compact body and its content type" {
  let response = http::json(201, jsons::Object([("id", jsons::Int(1)), ("ok", jsons::Bool(True))]))
  assert response.status == 201
  assert response.body == r#"{"id":1,"ok":true}"#
  assert response.headers == [("content-type", "application/json")]
}
```

### `text`

```cheby
pub fn text(status: Int, body: String) -> Response
```

Returns a response with `status`, the header `content-type: text/plain; charset=utf-8`, and `body`.

- **Cost:** O(1). Informative.

```cheby
test "text builds a plain response" {
  let response = http::text(404, "no such page")
  assert response == http::Response {
    status: 404,
    headers: [("content-type", "text/plain; charset=utf-8")],
    body: "no such page",
  }
}
```

### `with_header`

```cheby
pub fn with_header(response: Response, name: String, value: String) -> Response
```

Returns `response` with the header `name` set to `value`. Every header of that name, in any ASCII case, is removed, and the new one is added at the end with `name` in lower case. The other headers keep their order (D-1006). The name and value are not checked here: [`serve_on`](#serve_on) refuses to send a header that could split the response.

- **Cost:** O(n) in the number of headers. Informative.

```cheby
test "with_header replaces every header of that name" {
  let response =
    http::text(200, "hi")
    |> http::with_header(_, "X-Trace", "1")
    |> http::with_header(_, "x-trace", "2")
  assert response.headers == [("content-type", "text/plain; charset=utf-8"), ("x-trace", "2")]
}
```

### `add_header`

```cheby
pub fn add_header(response: Response, name: String, value: String) -> Response
```

Returns `response` with the header `name: value` added at the end, with `name` in lower case. Every other header keeps its place, including those of the same name, so a response can carry two `set-cookie` headers, as Fetch's `append` allows beside its `set` (D-1052). As for [`with_header`](#with_header), the name and value are not checked here.

- **Cost:** O(n) in the number of headers. Informative.

```cheby
test "add_header keeps the headers of the same name" {
  let response =
    http::text(200, "hi")
    |> http::add_header(_, "Set-Cookie", "a=1")
    |> http::add_header(_, "set-cookie", "b=2")
  assert list::filter(response.headers, fn((name, _)) { name == "set-cookie" }) == [
    ("set-cookie", "a=1"),
    ("set-cookie", "b=2"),
  ]
}
```

## Reading headers

### `header`

```cheby
pub fn header(response: Response, name: String) -> Option<String>
```

Returns the value of the first header of `response` named `name`, compared without regard to ASCII case.

- **Fails:** `None` when `response` has no header of that name.
- **Cost:** O(n) in the number of headers. Informative.

```cheby
test "header matches names in any case" {
  let response = http::text(405, "") |> http::with_header(_, "allow", "POST")
  assert http::header(response, "allow") == Some("POST")
  assert http::header(response, "Allow") == Some("POST")
  assert http::header(response, "location") == None
}
```

### `request_header`

```cheby
pub fn request_header(request: Request, name: String) -> Option<String>
```

Returns the value of the first header of `request` named `name`, compared without regard to ASCII case. It is a function of its own because a module has no overloading (D-045).

- **Fails:** `None` when `request` has no header of that name.
- **Cost:** O(n) in the number of headers. Informative.

```cheby
test "request_header matches names in any case" {
  let incoming = http::Request {
    method: http::Get,
    path: "/",
    segments: [],
    query: [],
    headers: [("x-token", "abc")],
    body: "",
  }
  assert http::request_header(incoming, "X-Token") == Some("abc")
  assert http::request_header(incoming, "cookie") == None
}
```

## Methods

### `method_name`

```cheby
pub fn method_name(method: Method) -> String
```

Returns the method's token as it is sent, such as `GET`, or the token inside `Other`. It is an encoding, as `json::to_string` is, so it is not `Method`'s `show`: a module has one `show`, and `Error` takes it (D-045, D-307, D-1005).

- **Cost:** O(1). Informative.

```cheby
test "method_name gives the method's token" {
  assert http::method_name(http::Get) == "GET"
  assert http::method_name(http::Other("PURGE")) == "PURGE"
  assert list::map([http::Get, http::Post], http::method_name) == ["GET", "POST"]
}
```

## Errors

The predicates below hold for the kinds this module defines. A failure underneath in the operating system or the network is an IO failure, read with [`io_error`](#io_error) and the predicates of `std::io` (D-1016).

### `is_invalid_url`

```cheby
pub fn is_invalid_url(error: Error) -> Bool
```

Tells whether `error` means that a URL given to `get` or `send`, or one that a redirect named, is not an absolute `http` or `https` URL. Its text is `invalid URL`.

- **Cost:** O(1). Informative.

The first test of [`get`](#get) uses it.

### `is_invalid_header`

```cheby
pub fn is_invalid_header(error: Error) -> Bool
```

Tells whether `error` means that `send` was given a header name that is not an HTTP token, a value with a line break or U+0000, or an `Other` method that is not a token. Its text is `invalid header`.

- **Cost:** O(1). Informative.

The first test of [`send`](#send) uses it.

### `is_invalid_response`

```cheby
pub fn is_invalid_response(error: Error) -> Bool
```

Tells whether `error` means that the server's answer was not valid HTTP/1.1, its head was over the client's limit of 64 KiB, or its body was not valid UTF-8 (D-1074). Its text is `invalid response`.

- **Cost:** O(1). Informative.

_Example_, not a `test` because it depends on what a server sends:

```cheby
fn fetch_text(url: String) -> Result<String, String> {
  case http::get(url) {
    Ok(response) => Ok(response.body)
    Err(reason) => case http::is_invalid_response(reason) {
      True => Err("{url} did not send text")
      False => Err("cannot fetch {url}: {reason}")
    }
  }
}
```

### `is_too_many_redirects`

```cheby
pub fn is_too_many_redirects(error: Error) -> Bool
```

Tells whether `error` means that a request was redirected more than 10 times in a row. Its text is `too many redirects`.

- **Cost:** O(1). Informative.

The third test of [`get`](#get) uses it.

### `is_tls`

```cheby
pub fn is_tls(error: Error) -> Bool
```

Tells whether `error` means that an HTTPS connection could not be secured, for example because the server's certificate is not valid for its name. Its text is `TLS failure`. The client checks certificates against the Mozilla roots built into the runtime, so every machine trusts the same roots (D-1011).

- **Cost:** O(1). Informative.

This test needs no trusted certificate, so it runs offline. The plain server reads the client's TLS greeting as a bad request and answers `400` in plain text, which the client cannot read as TLS. The client writes its greeting in one piece, a few hundred bytes, and the server reads it whole before it answers, so its close is orderly and the client sees the `400`, not a reset.

```cheby
test "https to a server that does not speak TLS is a TLS failure" {
  let assert Ok(listener) = net::listen("127.0.0.1", 0)
  let port = net::local_port(listener)
  fiber::spawn_detached(fn() { http::serve_on(listener, fn(_) { http::text(200, "plain") }) })
  let assert Err(reason) = http::get("https://127.0.0.1:{port}/")
  assert http::is_tls(reason)
  assert "{reason}" == "TLS failure"
}
```

_Example_, not a `test` because it needs the internet:

```cheby
fn explain(reason: http::Error) -> String {
  case http::is_tls(reason) {
    True => "the site's certificate was not accepted"
    False => "{reason}"
  }
}
```

### `io_error`

```cheby
pub fn io_error(error: Error) -> Option<io::Error>
```

Returns the IO failure underneath `error`, such as a refused connection, a host that cannot be found, or a port already in use, so the caller can ask `std::io`'s predicates.

- **Fails:** `None` when `error` is one of this module's own kinds.
- **Cost:** O(1). Informative.

The test of [`serve`](#serve) uses it.

### `show`

```cheby
pub fn show(error: Error) -> String
```

Makes `Error` satisfy `Show` ([§8.8](../spec/08-interfaces.md#88-standard-interfaces)). It gives `invalid URL`, `invalid header`, `invalid response`, `too many redirects` or `TLS failure` for this module's kinds, and the `show` text of the `io::Error` for an IO failure, such as `connection refused` (D-345, D-1016). Debug printing with `{e:?}` gives the same text (§3.13). Neither includes the URL.

- **Cost:** O(1). Informative.

```cheby
test "an http error shows a fixed message" {
  let assert Err(reason) = http::get("not a url")
  assert http::show(reason) == "invalid URL"
  assert http::io_error(reason) == None
}
```

## How requests run on fibers

This section is informative. It shows how the functions above are built from `std::net` and `std::fiber` (D-1003, ADR-0066).

**The client.** `send` parses the URL in Cheby, then calls `net::connect`, or for `https` a TLS connect that only std can reach (D-1011). It writes the request head and body, reads until a kernel has parsed a complete response head, and reads the body by its `content-length`, its chunks or until the connection closes. It decodes the body with `string::from_bytes`, and follows a redirect by calling itself with a new URL. Every step runs on the calling fiber. A name lookup runs on the blocking pool, and every other wait is a socket wait on the poller (D-983). So 031's `fiber::timeout(deadline, fn() { fetch(path) })` cancels a slow fetch: the fiber unwinds out of `net::read_bytes`, the connection is released and closes, and the crawl records `timeout` (D-1062). A cancellation that arrives during the lookup is delivered at once too: the fiber unwinds without waiting for it, and the lookup's result is dropped when it returns (D-1065). 031's four fetches in flight are four fibers in its own scope, each waiting on its own socket, while the poller's thread runs whichever is ready.

**The server.** `serve_on` opens a scope, accepts in a loop on the calling fiber, and spawns one fiber per connection into that scope. Each connection fiber reads a request, calls the handler inside a scope of its own, writes the response and reads the next request. In outline, with the private helpers left out:

```cheby
pub fn serve_on(listener: net::Listener, handler: fn(Request) -> Response) -> Result<Nil, Error> {
  let outcome = {
    use owner <- fiber::scope()
    accept_loop(owner, listener, handler)
  }
  case outcome {
    Ok(stopped) => stopped
    // Each handler runs in a scope of its own, so only a bug in
    // `std::http` itself can fail this scope.
    Err(failure) => panic as fiber::message(failure)
  }
}

priv fn accept_loop(
  owner: fiber::Scope,
  listener: net::Listener,
  handler: fn(Request) -> Response,
) -> Result<Nil, Error> {
  case net::accept(listener) {
    Ok(connection) => {
      fiber::spawn(owner, fn() { serve_connection(connection, handler) })
      accept_loop(owner, listener, handler)
    }
    Err(reason) => case is_passing(reason), io::is_closed(reason) {
      True, _ => {
        time::sleep(duration::milliseconds(100))
        accept_loop(owner, listener, handler)
      }
      // The program closed the listener to stop the server (D-1064).
      False, True => {
        fiber::cancel(owner)
        Ok(Nil)
      }
      False, False => {
        // Ends every connection now, so that the scope's close does not
        // wait for idle ones (D-1012, D-1060).
        fiber::cancel(owner)
        Err(from_io(reason))
      }
    }
  }
}

/// Calls `handler` in a scope of its own, so that its panic reaches
/// only this request (D-1012).
priv fn answer(request: Request, handler: fn(Request) -> Response) -> Response {
  let outcome = {
    use _scope <- fiber::scope()
    handler(request)
  }
  case outcome {
    Ok(response) => response
    Err(failure) => {
      io::eprintln(fiber::show(failure))
      text(500, "internal server error")
    }
  }
}
```

A cancellation of the fiber that called `serve_on` cancels `accept_loop`'s scope, so every connection fiber unwinds out of its socket wait or its handler, and every connection closes as it is released (D-205, D-667). A request's own scope never stops a cancellation from outside: it is cancelled with its connection ([§10.7](../spec/10-concurrency.md#107-cancellation)).

### Ending on an accept failure

`serve_on` ends its connections with [`fiber::cancel`](fiber.md#cancel), called by the scope's body on its own scope (D-1060). Without it, `accept_loop` could return, but the scope's close would then wait for every connection fiber, an idle one for up to its 60-second limit (D-1012).

- `fiber::cancel(owner)` cancels every fiber spawned into the scope so far, and every scope nested in them, with the scope as the target, so each connection and handler unwinds out of its socket wait or its code (D-667).
- It never cancels the scope's body, which goes on to return `Ok(Nil)` for a closed listener or `Err` for any other failure that does not pass. The scope's close waits until the cancelled fibers have unwound, and a panic of one of them after the cancellation reached it is not recorded, so `scope` gives `Ok` with the body's value, and `serve_on` returns that value.
- A handler that closes its own server's listener is one of the cancelled fibers, so it unwinds before its response is sent (D-1064).

The head kernels are `sys_parse_request_head` and `sys_parse_response_head`. They take the bytes read so far and return the parsed head and how many bytes it used, or that more bytes are needed. With the TLS kernels, they are ordinary kernels that never wait, so they need no `@std_context` (D-983, D-985).

## Not in tier 3

Each of these can be added later without breaking a program:

- binary bodies, through new functions such as `get_bytes` and a binary response type (D-1007);
- streaming request and response bodies, and responses larger than memory;
- connection reuse, HTTP/2 and HTTP/3 (D-1008);
- an HTTPS server, client certificates, and proxies, including the `HTTP_PROXY` variables (D-1011);
- cookies, multipart forms and compressed bodies;
- draining, a graceful shutdown that lets requests in flight finish: `serve_on` stops when its listener is closed or its fiber is cancelled, and cancels the requests in flight (D-1064);
- a limit on the size of a response body for the client, with an error kind of its own, which comes with streaming bodies (D-1074);
- the client's address in `Request`, and `Request` fields for the HTTP version or the raw query;
- server limits as parameters of `serve` (D-1013).
