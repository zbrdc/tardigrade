//! Shared HTTP-face helpers for the bridges (`serve_bridge` and
//! `tardigrade-bridge`): capped request parsing and guest-reply mapping
//! with one set of limits. Contract: docs/BRIDGE_CONTRACT.md.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Largest request body the bridges will allocate for, whatever
/// Content-Length claims.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Largest single header (or request) line, and most header lines.
pub const MAX_LINE_BYTES: usize = 8192;
pub const MAX_HEADERS: usize = 100;
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
}

pub enum ReadOutcome {
    Req(Request),
    /// Content-Length over the cap: answer 413 without reading it.
    TooLarge,
    /// Malformed, oversized headers, or the client went away.
    Bad,
}

/// Reads one line of at most MAX_LINE_BYTES (through the reader's fill,
/// so an unterminated flood cannot grow memory first). None: too long,
/// EOF, or an I/O error.
pub fn read_capped_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = Vec::new();
    loop {
        let avail = reader.fill_buf().ok()?;
        if avail.is_empty() {
            return if line.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&line).into_owned())
            };
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                if line.len() + pos > MAX_LINE_BYTES {
                    return None;
                }
                line.extend_from_slice(&avail[..=pos]);
                reader.consume(pos + 1);
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            None => {
                if line.len() + avail.len() > MAX_LINE_BYTES {
                    return None;
                }
                let n = avail.len();
                line.extend_from_slice(avail);
                reader.consume(n);
            }
        }
    }
}

pub fn read_request(stream: &mut TcpStream) -> ReadOutcome {
    let Ok(clone) = stream.try_clone() else {
        return ReadOutcome::Bad;
    };
    let mut reader = BufReader::new(clone);
    let Some(request_line) = read_capped_line(&mut reader) else {
        return ReadOutcome::Bad;
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return ReadOutcome::Bad;
    };
    let method = method.to_string();
    let path = path.to_string();

    let mut content_length = 0usize;
    for _ in 0..MAX_HEADERS {
        let Some(header) = read_capped_line(&mut reader) else {
            return ReadOutcome::Bad;
        };
        let header = header.trim_end();
        if header.is_empty() {
            // End of headers.
            let mut body = vec![0u8; content_length];
            if content_length > 0 && reader.read_exact(&mut body).is_err() {
                return ReadOutcome::Bad;
            }
            return ReadOutcome::Req(Request {
                method,
                path,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            match v.trim().parse::<usize>() {
                Ok(n) if n > MAX_BODY_BYTES => return ReadOutcome::TooLarge,
                Ok(n) => content_length = n,
                // An unparsable length is a bad request, not a zero body.
                Err(_) => return ReadOutcome::Bad,
            }
        }
    }
    ReadOutcome::Bad
}

/// Only a plausible HTTP status reaches the response line; anything else
/// the guest produced is a bridge-level 502.
pub fn valid_status(v: Option<i64>) -> Option<u16> {
    match v {
        Some(n) if (100..=599).contains(&n) => Some(n as u16),
        _ => None,
    }
}

/// A content type with controls (CR/LF above all) would inject headers;
/// fall back to a fixed type instead of forwarding it.
pub fn sanitize_content_type(v: Option<&str>) -> String {
    match v {
        Some(t) if !t.is_empty() && t.chars().all(|c| c.is_ascii() && !c.is_ascii_control()) => {
            t.to_string()
        }
        _ => "text/plain".to_string(),
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

pub fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &str) {
    let extra = if status == 503 {
        "Retry-After: 1\r\n"
    } else {
        ""
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason(status),
        ctype,
        extra,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Maps one guest JSON reply line to (status, content type, body), shared
/// by both bridges so the failure taxonomy cannot drift between them.
pub fn map_reply(out: &str) -> (u16, String, String) {
    match serde_json::from_str::<serde_json::Value>(out) {
        Ok(v)
            if v.get("$error")
                .is_some_and(|e| e.as_str() == Some("command timed out")) =>
        {
            (
                504,
                "text/plain".to_string(),
                "command timed out".to_string(),
            )
        }
        Ok(v) if v.get("$error").is_some() => (
            500,
            "text/plain".to_string(),
            v.get("$error")
                .and_then(|e| e.as_str())
                .unwrap_or("error")
                .to_string(),
        ),
        Ok(v) => match valid_status(v.get("status").and_then(|s| s.as_i64())) {
            Some(status) => (
                status,
                sanitize_content_type(v.get("type").and_then(|t| t.as_str())),
                v.get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            None if v.get("status").is_some() => (
                502,
                "text/plain".to_string(),
                "guest returned an invalid status".to_string(),
            ),
            None => (
                200,
                sanitize_content_type(v.get("type").and_then(|t| t.as_str())),
                v.get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
        },
        Err(_) => (200, "text/plain".to_string(), out.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_must_be_a_real_http_status() {
        assert_eq!(valid_status(Some(200)), Some(200));
        assert_eq!(valid_status(Some(599)), Some(599));
        assert_eq!(valid_status(Some(99)), None);
        assert_eq!(valid_status(Some(600)), None);
        assert_eq!(valid_status(Some(-1)), None);
        assert_eq!(valid_status(None), None);
    }

    #[test]
    fn content_type_cannot_inject_headers() {
        assert_eq!(sanitize_content_type(Some("text/html")), "text/html");
        assert_eq!(
            sanitize_content_type(Some("text/html\r\nX-Evil: 1")),
            "text/plain"
        );
        assert_eq!(sanitize_content_type(Some("")), "text/plain");
        assert_eq!(sanitize_content_type(None), "text/plain");
    }

    #[test]
    fn capped_line_refuses_a_flood() {
        let data = vec![b'a'; MAX_LINE_BYTES + 100];
        let mut r = BufReader::new(&data[..]);
        assert!(read_capped_line(&mut r).is_none());
        let mut ok = BufReader::new(&b"GET / HTTP/1.1\r\n"[..]);
        assert_eq!(read_capped_line(&mut ok).unwrap(), "GET / HTTP/1.1\r\n");
    }

    #[test]
    fn reply_mapping_covers_the_failure_taxonomy() {
        assert_eq!(map_reply("{\"$error\":\"command timed out\"}").0, 504);
        assert_eq!(map_reply("{\"$error\":\"boom\"}").0, 500);
        assert_eq!(map_reply("{\"status\":999}").0, 502);
        assert_eq!(map_reply("{\"body\":\"hi\"}").0, 200);
        assert_eq!(map_reply("not json at all").1, "text/plain");
    }
}
