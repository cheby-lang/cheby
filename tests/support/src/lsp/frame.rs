//! JSON-RPC frames over a byte stream: a `Content-Length` header, an
//! optional `Content-Type` header, an empty line, and that many bytes of
//! JSON. Header lines end with `\r\n`.
//!
//! [`read`] accepts nothing else, so a server that writes anything but
//! frames to standard output fails the harness (A §3.7): a panic message, a
//! `println!`, or a frame whose body is not a JSON-RPC 2.0 message.

use std::io::{BufRead, Write};

use serde_json::Value;

/// The largest body [`read`] accepts, so that garbage read as a length
/// cannot allocate without bound.
pub const MAX_BODY: usize = 256 * 1024 * 1024;

/// Writes `message` as one frame and flushes.
///
/// # Errors
///
/// Returns the error of the write.
pub fn write(out: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

/// Reads one frame: `Ok(None)` at the end of input between frames.
///
/// # Errors
///
/// Returns a message for anything that is not a frame of a JSON-RPC 2.0
/// message, or the end of input inside a frame.
pub fn read(input: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut length = None;
    let mut first = true;
    loop {
        let mut line = Vec::new();
        let read = input
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("cannot read the server's output: {e}"))?;
        if read == 0 {
            if first {
                return Ok(None);
            }
            return Err("the output ended inside a frame header".to_owned());
        }
        first = false;
        let Some(content) = line.as_slice().strip_suffix(b"\r\n".as_slice()) else {
            return Err(format!(
                "a header line that does not end with CRLF: {:?}",
                String::from_utf8_lossy(&line)
            ));
        };
        if content.is_empty() {
            break;
        }
        let text = std::str::from_utf8(content)
            .map_err(|_| format!("a header line that is not UTF-8: {content:?}"))?;
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| format!("not a header line: {text:?}"))?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err("a frame with two Content-Length headers".to_owned());
            }
            let n = value
                .trim()
                .parse::<usize>()
                .map_err(|e| format!("Content-Length {value:?}: {e}"))?;
            if n > MAX_BODY {
                return Err(format!("Content-Length {n} is larger than {MAX_BODY}"));
            }
            length = Some(n);
        } else if !name.eq_ignore_ascii_case("Content-Type") {
            return Err(format!("an unknown header: {text:?}"));
        }
    }
    let length = length.ok_or("a frame without Content-Length")?;
    let mut body = vec![0_u8; length];
    input
        .read_exact(&mut body)
        .map_err(|e| format!("the output ended inside a frame body: {e}"))?;
    let value: Value = serde_json::from_slice(&body).map_err(|e| {
        format!(
            "a frame body that is not JSON ({e}): {:?}",
            String::from_utf8_lossy(&body)
        )
    })?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(format!("a frame that is not JSON-RPC 2.0: {value}"));
    }
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frames(bytes: &[u8]) -> Result<Vec<Value>, String> {
        let mut input = bytes;
        let mut out = Vec::new();
        while let Some(v) = read(&mut input)? {
            out.push(v);
        }
        Ok(out)
    }

    #[test]
    fn round_trip() {
        let mut out = Vec::new();
        let a = json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
        let b = json!({"jsonrpc": "2.0", "id": 1, "result": "é😀"});
        write(&mut out, &a).unwrap();
        write(&mut out, &b).unwrap();
        assert_eq!(frames(&out), Ok(vec![a, b]));
        assert_eq!(frames(b""), Ok(vec![]));
    }

    #[test]
    fn content_type_is_accepted() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let mut bytes = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        assert_eq!(frames(&bytes).map(|f| f.len()), Ok(1));
    }

    #[test]
    fn anything_else_is_an_error() {
        // A panic message, a bare newline, a missing length, a short body,
        // a body that is not JSON-RPC.
        assert!(frames(b"thread 'main' panicked\n").is_err());
        assert!(frames(b"Content-Length: 2\n\n{}").is_err());
        assert!(frames(b"\r\n").is_err());
        assert!(frames(b"Content-Length: 10\r\n\r\n{}").is_err());
        assert!(frames(b"Content-Length: 2\r\n\r\n{}").is_err());
        assert!(frames(b"Content-Length: 2\r\nX-Other: 1\r\n\r\n{}").is_err());
        assert!(frames(b"Content-Length: 99999999999999\r\n\r\n").is_err());
        let ok = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let mut bytes = format!("Content-Length: {}\r\n\r\n", ok.len()).into_bytes();
        bytes.extend_from_slice(ok);
        bytes.extend_from_slice(b"hello\n");
        assert!(frames(&bytes).is_err());
    }
}
