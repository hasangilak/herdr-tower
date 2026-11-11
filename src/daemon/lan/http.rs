//! Just enough HTTP/1.1 for the phone API: one request per connection
//! (`Connection: close`), a body by `Content-Length` only, bounded sizes.
//! The head is read first (`read_head`) so the caller can check who is
//! calling before it reads a body (`read_body`) of the size that call allows.

use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{Result, bail};
use serde_json::Value;

pub const MAX_HEAD: usize = 16 * 1024;
/// A multipart call: six files and a recording, with room for the framing.
pub const MAX_BODY: usize = 100 * 1024 * 1024;
/// A plain WAV upload (two minutes of 48 kHz stereo 16-bit fit).
pub const MAX_VOICE_BODY: usize = 24 * 1024 * 1024;
/// A JSON call.
pub const MAX_JSON_BODY: usize = 1024 * 1024;
/// Before a device is known: `POST /v1/pair`.
pub const MAX_PAIR_BODY: usize = 4 * 1024;

#[derive(Debug, Default)]
pub struct Request {
    pub method: String,
    /// Without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Lowercase names.
    pub headers: HashMap<String, String>,
    /// What `Content-Length` says the body is.
    pub content_length: usize,
    pub body: Vec<u8>,
    /// The tower's address the call came in on (which listener: the LAN's or the tailnet's).
    pub local: Option<std::net::SocketAddr>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
    pub fn json(&self) -> Result<Value> {
        if self.body.is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        Ok(serde_json::from_slice(&self.body)?)
    }
    /// Path segments: `/v1/agents/t3/answer` -> ["v1", "agents", "t3", "answer"].
    pub fn segments(&self) -> Vec<String> {
        self.path.split('/').filter(|s| !s.is_empty()).map(|s| decode(s, false)).collect()
    }
}

/// Percent-decoding; `+` is a space only in a query (`plus`).
fn decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                Ok(v) => {
                    out.push(v);
                    i += 3;
                    continue;
                }
                Err(_) => out.push(b'%'),
            },
            b'+' if plus => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads a request's head. The body is left unread: what came with the head
/// is returned beside it, for `read_body`. Errors are the client's fault
/// (answer 400 and close).
pub fn read_head(r: &mut impl Read) -> Result<(Request, Vec<u8>)> {
    let mut buf = Vec::with_capacity(2048);
    let mut byte = [0u8; 1024];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
        let n = r.read(&mut byte)?;
        if n == 0 {
            bail!("connection closed before the request was complete");
        }
        buf.extend_from_slice(&byte[..n]);
    };
    if head_end > MAX_HEAD {
        bail!("request head too large");
    }
    let head = std::str::from_utf8(&buf[..head_end])?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else { bail!("bad request line") };
    if !version.starts_with("HTTP/1.") {
        bail!("only HTTP/1.x");
    }
    let mut req = Request { method: method.to_ascii_uppercase(), ..Default::default() };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    req.path = path.to_string();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        req.query.insert(decode(k, true), decode(v, true));
    }
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { bail!("bad header line") };
        req.headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    if req.headers.get("transfer-encoding").is_some_and(|t| !t.eq_ignore_ascii_case("identity")) {
        bail!("send a Content-Length body, not chunked");
    }
    req.content_length = match req.headers.get("content-length") {
        Some(v) => v.parse().map_err(|_| anyhow::anyhow!("bad Content-Length"))?,
        None => 0,
    };
    let mut rest = buf.split_off(head_end + 4);
    rest.truncate(req.content_length);
    Ok((req, rest))
}

/// Reads the body after `read_head` (`rest`: what came with the head), at
/// most `max` bytes. It grows as bytes arrive: a big `Content-Length` alone
/// reserves nothing.
pub fn read_body(r: &mut impl Read, req: &mut Request, mut rest: Vec<u8>, max: usize) -> Result<()> {
    let len = req.content_length;
    if len > max {
        bail!("body too large");
    }
    if rest.len() < len {
        r.take((len - rest.len()) as u64).read_to_end(&mut rest)?;
    }
    if rest.len() < len {
        bail!("connection closed before the body was complete");
    }
    req.body = rest;
    Ok(())
}

/// Reads one whole request, its body up to `MAX_BODY`.
#[cfg(test)]
pub fn read_request(r: &mut impl Read) -> Result<Request> {
    let (mut req, rest) = read_head(r)?;
    read_body(r, &mut req, rest, MAX_BODY)?;
    Ok(req)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, v: &Value) -> Response {
        Response { status, content_type: "application/json", body: serde_json::to_vec(v).unwrap_or_default() }
    }
    pub fn error(status: u16, code: &str, message: &str) -> Response {
        Self::json(status, &serde_json::json!({ "error": { "code": code, "message": message } }))
    }
    pub fn bytes(content_type: &'static str, body: Vec<u8>) -> Response {
        Response { status: 200, content_type, body }
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

pub fn write_response(w: &mut impl Write, r: &Response) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(&r.body)?;
    w.flush()?;
    Ok(())
}

/// The head of a Server-Sent Events stream; events follow until either side closes.
pub fn write_sse_head(w: &mut impl Write) -> Result<()> {
    w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Accel-Buffering: no\r\n\r\n")?;
    w.flush()?;
    Ok(())
}

/// One SSE event (`data` is one line of JSON).
pub fn sse_event(id: u64, event: &str, data: &Value) -> String {
    format!("id: {id}\nevent: {event}\ndata: {}\n\n", serde_json::to_string(data).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_request() {
        let raw = b"POST /v1/agents/vps%2Ft3/answer?x=1&y=a+b HTTP/1.1\r\nHost: m\r\nAuthorization: Bearer htk_x\r\nContent-Length: 13\r\n\r\n{\"text\":\"hi\"}";
        let r = read_request(&mut &raw[..]).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.segments(), ["v1", "agents", "vps/t3", "answer"]);
        assert_eq!(r.query["y"], "a b");
        assert_eq!(r.header("authorization"), Some("Bearer htk_x"));
        assert_eq!(r.json().unwrap()["text"], "hi");
    }

    #[test]
    fn refuses_what_it_wont_handle() {
        assert!(read_request(&mut &b"GET / HTTP/2\r\n\r\n"[..]).is_err());
        assert!(read_request(&mut &b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"[..]).is_err());
        let big = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(read_request(&mut big.as_bytes()).is_err());
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD + 10));
        assert!(read_request(&mut huge.as_bytes()).is_err());
        assert!(read_request(&mut &b"GET / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab"[..]).is_err(), "body cut short");
    }

    #[test]
    fn plus_is_a_space_only_in_the_query() {
        let raw = b"GET /v1/sessions/a+b%2Bc/show?q=x+y%2Bz HTTP/1.1\r\n\r\n";
        let r = read_request(&mut &raw[..]).unwrap();
        assert_eq!(r.segments(), ["v1", "sessions", "a+b+c", "show"]);
        assert_eq!(r.query["q"], "x y+z");
    }

    #[test]
    fn the_head_comes_before_the_body() {
        // A huge Content-Length: the head reads, nothing is reserved, and the body is refused by its limit.
        let raw = format!("POST /v1/pair HTTP/1.1\r\nContent-Length: {}\r\n\r\n{{\"code\":", MAX_BODY);
        let mut r = raw.as_bytes();
        let (mut req, rest) = read_head(&mut r).unwrap();
        assert_eq!((req.content_length, rest.as_slice()), (MAX_BODY, &b"{\"code\":"[..]));
        assert!(read_body(&mut r, &mut req, rest, MAX_PAIR_BODY).is_err());
        assert!(req.body.is_empty());
        // What came with the head, and the rest after it.
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 6\r\n\r\nabcdefEXTRA";
        let mut r = &raw[..];
        let (mut req, rest) = read_head(&mut r).unwrap();
        assert_eq!(rest, b"abcdef", "nothing past Content-Length");
        read_body(&mut r, &mut req, rest, 6).unwrap();
        assert_eq!(req.body, b"abcdef");
    }

    #[test]
    fn writes_responses_and_events() {
        let mut out = vec![];
        write_response(&mut out, &Response::error(401, "unauthorized", "no token")).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(s.contains("Connection: close") && s.ends_with(r#"{"error":{"code":"unauthorized","message":"no token"}}"#));
        assert_eq!(sse_event(3, "ping", &serde_json::json!({})), "id: 3\nevent: ping\ndata: {}\n\n");
    }
}
