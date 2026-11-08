//! `multipart/form-data` bodies (RFC 7578), as OkHttp's `MultipartBody` sends
//! them: text fields and files, each with a name.

use anyhow::{Result, bail};

#[derive(Debug, PartialEq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// The boundary in a `Content-Type: multipart/form-data; boundary=...` header.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string()).filter(|b| !b.is_empty() && b.len() <= 200)
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// `name="text"` -> Some("text") in a header line's parameters.
fn param(line: &str, key: &str) -> Option<String> {
    line.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(key).then(|| v.trim().trim_matches('"').to_string())
    })
}

pub fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>> {
    let delim = format!("--{boundary}").into_bytes();
    let Some(mut at) = find(body, &delim, 0) else { bail!("no multipart boundary in the body") };
    let mut parts = vec![];
    loop {
        at += delim.len();
        if body[at..].starts_with(b"--") {
            return Ok(parts);
        }
        if !body[at..].starts_with(b"\r\n") {
            bail!("bad multipart framing");
        }
        at += 2;
        let Some(head_end) = find(body, b"\r\n\r\n", at) else { bail!("a multipart part without headers") };
        let head = String::from_utf8_lossy(&body[at..head_end]).into_owned();
        let data_start = head_end + 4;
        let mut next_delim = b"\r\n".to_vec();
        next_delim.extend_from_slice(&delim);
        let Some(data_end) = find(body, &next_delim, data_start) else { bail!("a multipart part that never ends") };
        let mut part = Part { name: String::new(), filename: None, content_type: None, data: body[data_start..data_end].to_vec() };
        for line in head.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    part.name = param(v, "name").unwrap_or_default();
                    part.filename = param(v, "filename");
                }
                "content-type" => part.content_type = Some(v.trim().to_string()),
                _ => {}
            }
        }
        parts.push(part);
        if parts.len() > 64 {
            bail!("too many parts");
        }
        at = data_end + 2;
    }
}

/// A body for tests (and the docs): fields, then files.
#[cfg(test)]
pub fn build(boundary: &str, fields: &[(&str, &str)], files: &[(&str, &str, &str, &[u8])]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, value) in fields {
        b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    for (name, filename, ctype, data) in files {
        b.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {ctype}\r\n\r\n").as_bytes(),
        );
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_and_files() {
        assert_eq!(boundary("multipart/form-data; boundary=abc123").as_deref(), Some("abc123"));
        assert_eq!(boundary("multipart/form-data; charset=utf-8; boundary=\"x y\"").as_deref(), Some("x y"));
        assert_eq!(boundary("application/json"), None);
        let png: &[u8] = b"\x89PNG\r\n\x1a\nbinary\r\n--not-the-boundary";
        let body = build("b0und", &[("text", "what is wrong here?")], &[("image", "a.png", "image/png", png), ("image", "b.jpg", "image/jpeg", b"\xff\xd8\xff")]);
        let parts = parse(&body, "b0und").unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!((parts[0].name.as_str(), parts[0].data.as_slice()), ("text", &b"what is wrong here?"[..]));
        assert_eq!(parts[1].filename.as_deref(), Some("a.png"));
        assert_eq!(parts[1].content_type.as_deref(), Some("image/png"));
        assert_eq!(parts[1].data, png, "binary data with CRLFs and dashes comes through whole");
        assert_eq!(parts[2].data, b"\xff\xd8\xff");
    }

    #[test]
    fn refuses_broken_bodies() {
        assert!(parse(b"no boundary here", "b").is_err());
        assert!(parse(b"--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\nnever ends", "b").is_err());
        assert!(parse(b"--bXX", "b").is_err());
        assert_eq!(parse(b"--b--\r\n", "b").unwrap(), vec![]);
    }
}
