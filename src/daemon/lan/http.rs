//! Just enough HTTP/1.1 for the phone API: one request per connection
//! (`Connection: close`), a body by `Content-Length` only, bounded sizes.

use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{Result, bail};
use serde_json::Value;

pub const MAX_HEAD: usize = 16 * 1024;
/// Enough for a minute of 48 kHz stereo 16-bit WAV.
pub const MAX_BODY: usize = 24 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct Request {
    pub method: String,
    /// Without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Lowercase names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
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
        self.path.split('/').filter(|s| !s.is_empty()).map(decode).collect()
    }
}

fn decode(s: &str) -> String {
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
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads one request. Errors are the client's fault (answer 400 and close).
pub fn read_request(r: &mut impl Read) -> Result<Request> {
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
        req.query.insert(decode(k), decode(v));
    }
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { bail!("bad header line") };
        req.headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    if req.headers.get("transfer-encoding").is_some_and(|t| !t.eq_ignore_ascii_case("identity")) {
        bail!("send a Content-Length body, not chunked");
    }
    let len: usize = match req.headers.get("content-length") {
        Some(v) => v.parse().map_err(|_| anyhow::anyhow!("bad Content-Length"))?,
        None => 0,
    };
    if len > MAX_BODY {
        bail!("body too large");
    }
    let mut body = buf[head_end + 4..].to_vec();
    if body.len() > len {
        body.truncate(len);
    }
    if body.len() < len {
        let start = body.len();
        body.resize(len, 0);
        r.read_exact(&mut body[start..])?;
    }
    req.body = body;
    Ok(req)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub extra: Vec<(&'static str, String)>,
}

impl Response {
    pub fn json(status: u16, v: &Value) -> Response {
        Response { status, content_type: "application/json", body: serde_json::to_vec(v).unwrap_or_default(), extra: vec![] }
    }
    pub fn error(status: u16, code: &str, message: &str) -> Response {
        Self::json(status, &serde_json::json!({ "error": { "code": code, "message": message } }))
    }
    pub fn bytes(content_type: &'static str, body: Vec<u8>) -> Response {
        Response { status: 200, content_type, body, extra: vec![] }
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
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    );
    for (k, v) in &r.extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
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
    fn writes_responses_and_events() {
        let mut out = vec![];
        write_response(&mut out, &Response::error(401, "unauthorized", "no token")).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(s.contains("Connection: close") && s.ends_with(r#"{"error":{"code":"unauthorized","message":"no token"}}"#));
        assert_eq!(sse_event(3, "ping", &serde_json::json!({})), "id: 3\nevent: ping\ndata: {}\n\n");
    }
}
