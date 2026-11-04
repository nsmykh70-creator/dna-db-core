// SPDX-License-Identifier: Apache-2.0

//! Minimal HTTP/1.1 plumbing for the admin panel: request parsing, response
//! serialization (with the shared security-header/CSP block) and the trusted
//! proxy peer/secret checks.

use super::json::{inline_script_hashes, percent_decode};
use super::*;
use std::io::{Read, Write};
use subtle::ConstantTimeEq;

// ---------------- HTTP plumbing ----------------

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    /// raw `?a=b&c=d` query string, if any
    query: Option<String>,
    pub(crate) headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    pub(crate) fn bearer(&self) -> Option<String> {
        let auth = self.headers.get("authorization")?;
        auth.strip_prefix("Bearer ").map(|t| t.trim().to_string())
    }

    pub(crate) fn query_param(&self, key: &str) -> Option<String> {
        let q = self.query.as_ref()?;
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == key {
                    return Some(percent_decode(v));
                }
            }
        }
        None
    }

    pub(crate) fn json_body(&self) -> Option<J> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        IpAddr::V4(_) => ip,
    }
}

pub(crate) fn secret_matches(expected: &str, supplied: &str) -> bool {
    expected.len() == supplied.len()
        && bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
}

pub(crate) fn read_request<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    // Read until end of headers.
    let header_end = loop {
        if let Some(pos) = find_headers_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target, None),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body too large",
        ));
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) content_type: &'static str,
    pub(crate) body: String,
}

impl Response {
    pub(crate) fn json(status: u16, value: &J) -> Self {
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            body: value.to_string(),
        }
    }

    pub(crate) fn html(body: &str) -> Self {
        Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.to_string(),
        }
    }
}

pub(crate) fn write_response<W: Write + ?Sized>(
    stream: &mut W,
    res: &Response,
    tls: bool,
) -> std::io::Result<()> {
    let reason = match res.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        423 => "Locked",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let script_hashes = if res.content_type.starts_with("text/html") {
        inline_script_hashes(&res.body)
    } else {
        Vec::new()
    };
    let script_src = if script_hashes.is_empty() {
        "'self'".to_string()
    } else {
        format!("'self' {}", script_hashes.join(" "))
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\nPermissions-Policy: camera=(), microphone=(), geolocation=()\r\n\
         Cache-Control: no-store\r\nContent-Security-Policy: default-src 'self'; \
         script-src {}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'self'; object-src 'none'\r\n\
         Connection: close\r\n",
        res.status,
        reason,
        res.content_type,
        res.body.len(),
        script_src
    );
    if tls {
        head.push_str("Strict-Transport-Security: max-age=31536000; includeSubDomains\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(res.body.as_bytes())?;
    stream.flush()
}
