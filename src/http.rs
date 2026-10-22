//! Incremental HTTP/1.1 request parser and response serializer.
//!
//! The parser is deliberately strict: it accepts `Content-Length` bodies only,
//! rejects `Transfer-Encoding`, enforces size limits and never allocates more
//! than the configured maximum for a single request.

use std::fmt;

/// Size limits applied while parsing.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum bytes for request line + headers (including the final CRLFCRLF).
    pub max_head: usize,
    /// Maximum `Content-Length` accepted.
    pub max_body: usize,
    /// Maximum number of header fields.
    pub max_headers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_head: 8 * 1024,
            max_body: 1024 * 1024,
            max_headers: 64,
        }
    }
}

/// A fully received request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub target: String,
    /// Minor version: 0 for HTTP/1.0, 1 for HTTP/1.1.
    pub minor: u8,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup (first match).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether the connection should stay open after this request.
    pub fn keep_alive(&self) -> bool {
        let conn = self.header("connection").map(|v| v.to_ascii_lowercase());
        let has = |tok: &str| {
            conn.as_deref()
                .map(|v| v.split(',').any(|t| t.trim() == tok))
                .unwrap_or(false)
        };
        if self.minor == 0 {
            has("keep-alive")
        } else {
            !has("close")
        }
    }
}

/// Reasons a request is rejected. Each maps to a status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    BadRequest,
    HeadTooLarge,
    BodyTooLarge,
    VersionNotSupported,
    NotImplemented,
}

impl ParseError {
    pub fn status(self) -> u16 {
        match self {
            ParseError::BadRequest => 400,
            ParseError::BodyTooLarge => 413,
            ParseError::HeadTooLarge => 431,
            ParseError::NotImplemented => 501,
            ParseError::VersionNotSupported => 505,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} ({})", self, self.status())
    }
}

impl std::error::Error for ParseError {}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Try to parse one request from the front of `buf`.
///
/// Returns `Ok(None)` when more bytes are needed, `Ok(Some((req, n)))` when a
/// full request occupying the first `n` bytes was parsed.
pub fn parse_request(buf: &[u8], limits: &Limits) -> Result<Option<(Request, usize)>, ParseError> {
    // Tolerate (RFC 9112 §2.2) a few leading empty lines between pipelined requests.
    let mut start = 0;
    while buf[start..].starts_with(b"\r\n") && start < 8 {
        start += 2;
    }
    let buf_rest = &buf[start..];
    let head_end = match find_head_end(&buf_rest[..buf_rest.len().min(limits.max_head + 4)]) {
        Some(e) if e <= limits.max_head => e,
        Some(_) => return Err(ParseError::HeadTooLarge),
        None if buf_rest.len() >= limits.max_head => return Err(ParseError::HeadTooLarge),
        None => return Ok(None),
    };
    let head = &buf_rest[..head_end - 4];
    let head = std::str::from_utf8(head).map_err(|_| ParseError::BadRequest)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::BadRequest)?;
    let (method, target, minor) = parse_request_line(request_line)?;

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if headers.len() >= limits.max_headers {
            return Err(ParseError::HeadTooLarge);
        }
        let colon = line.find(':').ok_or(ParseError::BadRequest)?;
        let name = &line[..colon];
        if name.is_empty() || !name.bytes().all(is_tchar) {
            // Also rejects obsolete line folding (leading whitespace).
            return Err(ParseError::BadRequest);
        }
        let value = line[colon + 1..].trim_matches(|c| c == ' ' || c == '\t');
        if value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
            return Err(ParseError::BadRequest);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ParseError::NotImplemented);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) || value.len() > 19 {
                return Err(ParseError::BadRequest);
            }
            let n: usize = value.parse().map_err(|_| ParseError::BadRequest)?;
            if content_length.is_some_and(|prev| prev != n) {
                return Err(ParseError::BadRequest);
            }
            content_length = Some(n);
        }
        headers.push((name.to_string(), value.to_string()));
    }

    let body_len = content_length.unwrap_or(0);
    if body_len > limits.max_body {
        return Err(ParseError::BodyTooLarge);
    }
    let total = start + head_end + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    let body = buf[start + head_end..total].to_vec();
    Ok(Some((
        Request {
            method: method.to_string(),
            target: target.to_string(),
            minor,
            headers,
            body,
        },
        total,
    )))
}

fn parse_request_line(line: &str) -> Result<(&str, &str, u8), ParseError> {
    let mut parts = line.split(' ');
    let method = parts.next().ok_or(ParseError::BadRequest)?;
    let target = parts.next().ok_or(ParseError::BadRequest)?;
    let version = parts.next().ok_or(ParseError::BadRequest)?;
    if parts.next().is_some() || method.is_empty() || !method.bytes().all(is_tchar) {
        return Err(ParseError::BadRequest);
    }
    if target.is_empty() || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(ParseError::BadRequest);
    }
    let minor = match version {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        v if v.starts_with("HTTP/") && v.len() == 8 => return Err(ParseError::VersionNotSupported),
        _ => return Err(ParseError::BadRequest),
    };
    Ok((method, target, minor))
}

/// An HTTP response to serialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    /// Append the wire form to `out`. `Content-Length` and `Connection` are
    /// always emitted by the serializer.
    pub fn write_to(&self, out: &mut Vec<u8>, keep_alive: bool, head_only: bool) {
        use std::io::Write;
        let _ = write!(out, "HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (k, v) in &self.headers {
            let _ = write!(out, "{k}: {v}\r\n");
        }
        let _ = write!(
            out,
            "Content-Length: {}\r\nConnection: {}\r\n\r\n",
            self.body.len(),
            if keep_alive { "keep-alive" } else { "close" }
        );
        if !head_only {
            out.extend_from_slice(&self.body);
        }
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[u8]) -> Result<Option<(Request, usize)>, ParseError> {
        parse_request(s, &Limits::default())
    }

    #[test]
    fn simple_get() {
        let (r, n) = p(b"GET /a?b=1 HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.target, "/a?b=1");
        assert_eq!(r.minor, 1);
        assert_eq!(r.header("HOST"), Some("x"));
        assert_eq!(n, 32);
        assert!(r.keep_alive());
    }

    #[test]
    fn incomplete_head_and_body() {
        assert_eq!(p(b"GET / HTTP/1.1\r\nHost: x\r\n").unwrap(), None);
        assert_eq!(
            p(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab").unwrap(),
            None
        );
    }

    #[test]
    fn body_and_pipelining() {
        let raw = b"POST /e HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET / HTTP/1.1\r\n\r\n";
        let (r, n) = p(raw).unwrap().unwrap();
        assert_eq!(r.body, b"abc");
        let (r2, n2) = p(&raw[n..]).unwrap().unwrap();
        assert_eq!(r2.method, "GET");
        assert_eq!(n + n2, raw.len());
    }

    #[test]
    fn every_split_point_gives_same_result() {
        let raw = b"POST /x HTTP/1.1\r\nA: 1\r\nContent-Length: 4\r\n\r\nwxyz";
        let full = p(raw).unwrap().unwrap();
        for cut in 0..raw.len() {
            assert_eq!(p(&raw[..cut]).unwrap(), None, "cut {cut}");
        }
        assert_eq!(full.1, raw.len());
    }

    #[test]
    fn keep_alive_rules() {
        let r = |s: &[u8]| p(s).unwrap().unwrap().0;
        assert!(!r(b"GET / HTTP/1.0\r\n\r\n").keep_alive());
        assert!(r(b"GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n").keep_alive());
        assert!(!r(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").keep_alive());
        assert!(!r(b"GET / HTTP/1.1\r\nConnection: foo, Close\r\n\r\n").keep_alive());
    }

    #[test]
    fn rejections() {
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(ParseError::NotImplemented)
        );
        assert_eq!(
            p(b"GET / HTTP/2.0\r\n\r\n"),
            Err(ParseError::VersionNotSupported)
        );
        assert_eq!(p(b"GET / FOO\r\n\r\n"), Err(ParseError::BadRequest));
        assert_eq!(p(b"GET  / HTTP/1.1\r\n\r\n"), Err(ParseError::BadRequest));
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nBad Header: x\r\n\r\n"),
            Err(ParseError::BadRequest)
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\n folded\r\n\r\n"),
            Err(ParseError::BadRequest)
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nContent-Length: -1\r\n\r\n"),
            Err(ParseError::BadRequest)
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nContent-Length: +1\r\n\r\n"),
            Err(ParseError::BadRequest)
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"),
            Err(ParseError::BadRequest)
        );
        assert_eq!(
            p(b"GET / HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\n"),
            Err(ParseError::BadRequest)
        );
    }

    #[test]
    fn limits() {
        let lim = Limits {
            max_head: 64,
            max_body: 4,
            max_headers: 2,
        };
        let big = [b'a'; 100];
        assert_eq!(parse_request(&big, &lim), Err(ParseError::HeadTooLarge));
        assert_eq!(
            parse_request(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n", &lim),
            Err(ParseError::BodyTooLarge)
        );
        assert_eq!(
            parse_request(b"GET / HTTP/1.1\r\na: 1\r\nb: 2\r\nc: 3\r\n\r\n", &lim),
            Err(ParseError::HeadTooLarge)
        );
    }

    #[test]
    fn serialize() {
        let mut out = Vec::new();
        Response::new(200, "hi")
            .with_header("Content-Type", "text/plain")
            .write_to(&mut out, true, false);
        assert_eq!(
            out,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nhi"
        );
        out.clear();
        Response::new(404, "nope").write_to(&mut out, false, true);
        assert!(out.ends_with(b"Content-Length: 4\r\nConnection: close\r\n\r\n"));
    }
}
