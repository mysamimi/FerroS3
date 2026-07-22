//! Pure HTTP/1.1 request-parsing and response-formatting logic for the FreeBSD blocking
//! server in `main.rs`. It lives here — outside any `#[cfg(target_os = "freebsd")]` gate —
//! so it compiles and is unit-tested on every platform, even though the socket glue that
//! calls it only runs on FreeBSD.

use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use std::io::{self, BufRead};

/// Hard cap on a request body we will buffer, so an attacker-supplied `Content-Length`
/// (or an endless chunked stream) can't exhaust memory before auth even runs.
pub const MAX_BODY_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// How the request body is framed.
#[derive(Debug, PartialEq, Eq)]
pub enum BodyPlan {
    /// No body.
    Empty,
    /// `Content-Length: n` bytes follow.
    Fixed(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
}

/// The request line + headers, parsed from the connection.
pub struct RequestHead {
    pub method: Method,
    pub target: String,
    pub headers: HeaderMap,
    /// True when the request line declared HTTP/1.1 (affects keep-alive defaults).
    pub http11: bool,
}

/// Parse the request line and headers from `reader`, leaving it positioned at the body.
pub fn parse_request_head<R: BufRead>(reader: &mut R) -> io::Result<RequestHead> {
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty request"));
    }

    let mut parts = request_line.trim_end_matches(['\r', '\n']).split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?;
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing target"))?
        .to_string();
    let http11 = parts.next() == Some("HTTP/1.1");
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid method"))?;

    let mut headers = HeaderMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF before the blank line; treat what we have as the head
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed header"))?;
        let header_name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid header name"))?;
        let header_value = HeaderValue::from_str(value.trim())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid header value"))?;
        headers.append(header_name, header_value);
    }

    Ok(RequestHead { method, target, headers, http11 })
}

/// Whether the client can receive further responses on this connection. HTTP/1.1
/// defaults to keep-alive unless the request says `Connection: close`; HTTP/1.0
/// defaults to close unless it says `Connection: keep-alive`.
pub fn request_keep_alive(http11: bool, headers: &HeaderMap) -> bool {
    let connection = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if http11 {
        !connection.eq_ignore_ascii_case("close")
    } else {
        connection.eq_ignore_ascii_case("keep-alive")
    }
}

/// Decide how the body is framed. Per RFC 7230, `Transfer-Encoding: chunked` takes
/// precedence over `Content-Length`.
pub fn body_plan(headers: &HeaderMap) -> BodyPlan {
    if let Some(te) = headers.get(header::TRANSFER_ENCODING).and_then(|v| v.to_str().ok()) {
        if te.to_ascii_lowercase().contains("chunked") {
            return BodyPlan::Chunked;
        }
    }
    match headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(0) | None => BodyPlan::Empty,
        Some(n) => BodyPlan::Fixed(n),
    }
}

/// Whether the client asked to withhold the body until a `100 Continue` (`Expect:
/// 100-continue`). The blocking server must send that interim response or standard
/// clients stall.
pub fn wants_100_continue(headers: &HeaderMap) -> bool {
    headers
        .get(header::EXPECT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
}

/// Read the request body according to `plan`, refusing to buffer more than `limit` bytes
/// (so a huge `Content-Length` fails fast instead of pre-allocating gigabytes).
pub fn read_body<R: BufRead>(reader: &mut R, plan: BodyPlan, limit: usize) -> io::Result<Vec<u8>> {
    match plan {
        BodyPlan::Empty => Ok(Vec::new()),
        BodyPlan::Fixed(n) => {
            if n > limit {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
            }
            let mut buf = vec![0u8; n];
            reader.read_exact(&mut buf)?;
            Ok(buf)
        }
        BodyPlan::Chunked => read_chunked(reader, limit),
    }
}

/// Decode a `Transfer-Encoding: chunked` body, bounded by `limit`.
fn read_chunked<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        // Chunk size is hex, optionally followed by `;ext`.
        let size_hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // Consume the (possibly empty) trailer section up to the final blank line.
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        if body.len() + size > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        // Each chunk is followed by CRLF.
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(body)
}

/// Build the HTTP response head (status line + headers, ending with the blank line).
///
/// For a HEAD response the handler sets `Content-Length` to the object size but sends an
/// empty body; we must echo that size, not overwrite it with the empty body's length.
///
/// `body_len: None` means the body length is unknown up front (a stream without an exact
/// size hint): no `Content-Length` is written and the body is delimited by connection
/// close, so the caller must pass `keep_alive: false`.
pub fn format_response_head(
    status: StatusCode,
    headers: &HeaderMap,
    body_len: Option<u64>,
    head_only: bool,
    keep_alive: bool,
) -> String {
    let reason = status.canonical_reason().unwrap_or("");
    let mut head = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason);
    head.push_str(if keep_alive {
        "Connection: keep-alive\r\n"
    } else {
        "Connection: close\r\n"
    });

    let content_length = if head_only {
        Some(
            headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0),
        )
    } else {
        body_len
    };
    if let Some(n) = content_length {
        head.push_str(&format!("Content-Length: {}\r\n", n));
    }

    for (name, value) in headers.iter() {
        if name == header::CONTENT_LENGTH || name == header::CONNECTION {
            continue; // already written above
        }
        if let Ok(value_str) = value.to_str() {
            head.push_str(name.as_str());
            head.push_str(": ");
            head.push_str(value_str);
            head.push_str("\r\n");
        }
    }

    head.push_str("\r\n");
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_request_line_and_headers() {
        let raw = b"PUT /bucket/key HTTP/1.1\r\nHost: example\r\nContent-Length: 3\r\n\r\nabc";
        let mut r = Cursor::new(&raw[..]);
        let head = parse_request_head(&mut r).unwrap();
        assert_eq!(head.method, Method::PUT);
        assert_eq!(head.target, "/bucket/key");
        assert_eq!(head.headers.get("content-length").unwrap(), "3");
        // Reader is positioned at the body.
        assert_eq!(read_body(&mut r, body_plan(&head.headers), MAX_BODY_BYTES).unwrap(), b"abc");
    }

    #[test]
    fn body_plan_prefers_chunked_over_content_length() {
        let h = headers(&[("Transfer-Encoding", "chunked"), ("Content-Length", "5")]);
        assert_eq!(body_plan(&h), BodyPlan::Chunked);
    }

    #[test]
    fn body_plan_fixed_and_empty() {
        assert_eq!(body_plan(&headers(&[("Content-Length", "42")])), BodyPlan::Fixed(42));
        assert_eq!(body_plan(&headers(&[("Content-Length", "0")])), BodyPlan::Empty);
        assert_eq!(body_plan(&headers(&[])), BodyPlan::Empty);
    }

    #[test]
    fn fixed_body_over_limit_is_refused_without_allocating() {
        let mut r = Cursor::new(&b""[..]);
        let err = read_body(&mut r, BodyPlan::Fixed(usize::MAX), MAX_BODY_BYTES).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reads_fixed_body() {
        let mut r = Cursor::new(&b"hello world"[..]);
        assert_eq!(read_body(&mut r, BodyPlan::Fixed(5), MAX_BODY_BYTES).unwrap(), b"hello");
    }

    #[test]
    fn decodes_chunked_body() {
        // "Wiki" + "pedia" in two chunks.
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut r = Cursor::new(&raw[..]);
        let body = read_body(&mut r, BodyPlan::Chunked, MAX_BODY_BYTES).unwrap();
        assert_eq!(body, b"Wikipedia");
    }

    #[test]
    fn chunked_body_over_limit_is_refused() {
        let raw = b"5\r\nhello\r\n0\r\n\r\n";
        let mut r = Cursor::new(&raw[..]);
        let err = read_body(&mut r, BodyPlan::Chunked, 3).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn detects_expect_100_continue() {
        assert!(wants_100_continue(&headers(&[("Expect", "100-continue")])));
        assert!(wants_100_continue(&headers(&[("Expect", "100-Continue")])));
        assert!(!wants_100_continue(&headers(&[])));
    }

    #[test]
    fn head_response_preserves_content_length() {
        // HEAD: handler set Content-Length to the object size but sends an empty body.
        let h = headers(&[("Content-Length", "12345"), ("ETag", "\"abc\"")]);
        let out = format_response_head(StatusCode::OK, &h, None, true, false);
        assert!(out.contains("Content-Length: 12345\r\n"), "{out}");
        // HeaderMap normalises names to lowercase; HTTP header names are case-insensitive.
        assert!(out.contains("etag: \"abc\"\r\n"), "{out}");
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(out.ends_with("\r\n\r\n"));
    }

    #[test]
    fn get_response_uses_body_length() {
        let h = headers(&[("Content-Type", "application/octet-stream")]);
        let out = format_response_head(StatusCode::OK, &h, Some(987), false, false);
        assert!(out.contains("Content-Length: 987\r\n"), "{out}");
        // Connection: close is always present and written once.
        assert_eq!(out.matches("Connection: close").count(), 1);
    }

    #[test]
    fn keep_alive_response_head() {
        let h = headers(&[]);
        let out = format_response_head(StatusCode::OK, &h, Some(5), false, true);
        assert!(out.contains("Connection: keep-alive\r\n"), "{out}");
        assert!(!out.contains("Connection: close"), "{out}");
    }

    #[test]
    fn unknown_length_body_omits_content_length() {
        // A stream without an exact size hint is delimited by connection close.
        let h = headers(&[]);
        let out = format_response_head(StatusCode::OK, &h, None, false, false);
        assert!(!out.contains("Content-Length"), "{out}");
        assert!(out.contains("Connection: close\r\n"), "{out}");
    }

    #[test]
    fn parse_request_head_detects_http_version() {
        let mut r = Cursor::new(&b"GET /k HTTP/1.1\r\n\r\n"[..]);
        assert!(parse_request_head(&mut r).unwrap().http11);
        let mut r = Cursor::new(&b"GET /k HTTP/1.0\r\n\r\n"[..]);
        assert!(!parse_request_head(&mut r).unwrap().http11);
    }

    #[test]
    fn request_keep_alive_defaults_by_version() {
        // HTTP/1.1: keep-alive unless the client says close.
        assert!(request_keep_alive(true, &headers(&[])));
        assert!(!request_keep_alive(true, &headers(&[("Connection", "close")])));
        assert!(!request_keep_alive(true, &headers(&[("Connection", "Close")])));
        // HTTP/1.0: close unless the client asks for keep-alive.
        assert!(!request_keep_alive(false, &headers(&[])));
        assert!(request_keep_alive(false, &headers(&[("Connection", "keep-alive")])));
    }
}
