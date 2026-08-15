//! The transport, as pure functions over bytes.
//!
//! rote serves its protocol with `tiny_http` and speaks it with about a hundred
//! lines of hand-written client. A client crate would be the fourth-largest
//! dependency in the tree for three request shapes against a loopback socket
//! that this process started.
//!
//! Everything here takes and returns bytes, so the parts that are easy to get
//! wrong — a token comparison that leaks timing, an `Origin` check that accepts
//! a suffix, a response parser that trusts a `Content-Length` — are tested
//! without a socket, a port, or a running server.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// How long a client waits on a request. Generous because the engine may be
/// inside a recompute holding the manifest lock.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Refuse a response body larger than this rather than allocating on trust.
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// Refuse a request body larger than this. A verb is a few hundred bytes.
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).with_context(|| {
            format!(
                "cannot parse the daemon's reply ({} bytes, status {})",
                self.body.len(),
                self.status
            )
        })
    }

    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Case-insensitive header lookup. HTTP field names are case-insensitive and a
/// client that assumes otherwise works until it meets a different server.
pub fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Compare two secrets without leaking their common prefix through timing.
///
/// `==` on byte slices short-circuits at the first difference, which over
/// enough requests tells an attacker how much of a token they have guessed.
/// The length is compared first and separately — that much is public.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Is this `Origin` one of ours?
///
/// Exact match on scheme, host and port. Anything looser is the bug: a check
/// that accepts a prefix lets `http://127.0.0.1.evil.com` through, and one that
/// accepts a suffix lets `http://evil-127.0.0.1` through.
fn is_loopback_origin(origin: &str, port: u16) -> bool {
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|h| origin == format!("http://{h}:{port}"))
}

/// Is this `Host` a loopback name?
///
/// The DNS-rebinding defence. A page on `evil.com` can make a browser resolve
/// its own name to 127.0.0.1 and then talk to whatever is listening; what it
/// cannot do is change the `Host` header the browser sends.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        // Not a port if it is part of a bracketless IPv6 address.
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// Why a request was turned away, in the order the checks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    ForbiddenOrigin,
    ForbiddenHost,
    Unauthorized,
}

/// Every check a loopback request must pass before anything looks at its path.
///
/// One call rather than four exported predicates, because the *order* is part of
/// the rule and an order assembled at the call site is an order that can be
/// assembled differently next time. Returns a verdict rather than a response, so
/// this stays a function over bytes and the daemon keeps its own opinions about
/// status codes.
pub fn authorize(
    headers: &[(String, String)],
    path: &str,
    query: &str,
    port: u16,
    token: &str,
) -> Option<Rejection> {
    let find = |name: &str| header_of(headers, name);

    // A hostile page can point its own name at 127.0.0.1; it cannot forge these.
    if let Some(origin) = find("Origin") {
        if !is_loopback_origin(origin, port) {
            return Some(Rejection::ForbiddenOrigin);
        }
    }
    if let Some(host) = find("Host") {
        if !is_loopback_host(host) {
            return Some(Rejection::ForbiddenHost);
        }
    }

    // `EventSource` cannot set headers, so the stream accepts a query token —
    // and so does the page itself, because a browser address bar cannot set one
    // either. Exactly those two paths.
    //
    // Widening this rather than exempting `/` from authorization: an exempt path
    // would be the first unauthenticated route in a daemon whose whole doctrine
    // is that this runs before routing, and it would make the port
    // fingerprintable by any page on the machine.
    let query_token_ok = matches!(path, "/events" | "/");
    let presented = find("Authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            query_token_ok
                .then(|| query.split('&').find_map(|kv| kv.strip_prefix("token=")))
                .flatten()
        });

    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => None,
        _ => Some(Rejection::Unauthorized),
    }
}

/// Is this content type JSON?
///
/// Requiring it is what stops a hostile page firing a simple-CORS form POST at
/// every port on your loopback: a form can only send three content types, and
/// this is not one of them.
pub(crate) fn is_json_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .map(|t| t.trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false)
}

/// Open a connection with both timeouts set.
///
/// Both, deliberately: a read timeout alone still lets a connect hang, and this
/// runs on the interactive path of `rote skip`.
pub fn connect(port: u16, timeout: Duration) -> Result<TcpStream> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("cannot reach the rote daemon on port {port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

/// Write a request. `body` implies `POST` with a JSON content type.
pub fn write_request(
    w: &mut impl Write,
    method: &str,
    path: &str,
    port: u16,
    token: &str,
    body: Option<&[u8]>,
) -> Result<()> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Connection: close\r\n"
    );
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    if let Some(b) = body {
        w.write_all(b)?;
    }
    w.flush()?;
    Ok(())
}

/// Read a status line and headers. Leaves the reader positioned at the body.
pub fn read_head(r: &mut impl BufRead) -> Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        bail!("the daemon closed the connection without replying");
    }
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("cannot parse the status line: {:?}", line.trim_end()))?;

    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

/// Read a whole response.
///
/// `Content-Length` is required. The daemon never chunk-encodes anything but
/// the event stream, so refusing chunked outright is a parser we do not have to
/// write and cannot get wrong — and if one ever appears, it is a loud error
/// rather than a silently truncated body.
pub fn read_response(stream: TcpStream) -> Result<Response> {
    let mut r = BufReader::new(stream);
    let (status, headers) = read_head(&mut r)?;

    if let Some(te) = header_of(&headers, "Transfer-Encoding") {
        bail!("the daemon sent a {te}-encoded reply, which this client does not read");
    }
    let len: usize = match header_of(&headers, "Content-Length") {
        Some(v) => v
            .parse()
            .with_context(|| format!("cannot parse Content-Length: {v:?}"))?,
        None => bail!("the daemon replied without a Content-Length"),
    };
    if len > MAX_RESPONSE {
        bail!("the daemon's reply is {len} bytes, which is more than this client will read");
    }

    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .context("the daemon's reply ended early")?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// One round trip.
pub fn send(
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Response> {
    let mut stream = connect(port, timeout)?;
    write_request(&mut stream, method, path, port, token, body)?;
    read_response(stream)
}

/// One parsed `text/event-stream` frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub event: Option<String>,
    pub data: String,
}

/// Read frames off an event stream until it ends.
///
/// Comment lines (`:`) are heartbeats and are skipped, which is exactly what
/// they are for — an invisible write that proves the socket is still there.
pub struct Frames<R: BufRead> {
    reader: R,
}

impl<R: BufRead> Frames<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }
}

impl<R: BufRead> Iterator for Frames<R> {
    type Item = Result<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut frame = Frame::default();
        let mut saw_field = false;
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None, // the stream ended
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
            let line = line.trim_end_matches(['\r', '\n']);

            if line.is_empty() {
                // A blank line dispatches the frame — but only if there was one.
                if saw_field {
                    return Some(Ok(frame));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                frame.event = Some(rest.trim().to_string());
                saw_field = true;
            } else if let Some(rest) = line.strip_prefix("data:") {
                if saw_field && !frame.data.is_empty() {
                    frame.data.push('\n');
                }
                frame.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                saw_field = true;
            }
            // Anything else (a comment, an unknown field) is ignored per spec.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_comparison_does_not_short_circuit() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abcde"), "length differs");
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn only_an_exact_loopback_origin_is_ours() {
        assert!(is_loopback_origin("http://127.0.0.1:5000", 5000));
        assert!(is_loopback_origin("http://localhost:5000", 5000));
        // A different port is a different server.
        assert!(!is_loopback_origin("http://127.0.0.1:5001", 5000));
        // The two shapes a sloppy check lets through.
        assert!(!is_loopback_origin("http://127.0.0.1.evil.com:5000", 5000));
        assert!(!is_loopback_origin("http://evil-127.0.0.1:5000", 5000));
        assert!(!is_loopback_origin("https://127.0.0.1:5000", 5000));
        assert!(!is_loopback_origin("http://evil.com", 5000));
    }

    #[test]
    fn a_host_header_must_name_the_loopback() {
        // The DNS-rebinding defence: a hostile page can point its own name at
        // 127.0.0.1, but it cannot change the Host header the browser sends.
        assert!(is_loopback_host("127.0.0.1:5000"));
        assert!(is_loopback_host("localhost:5000"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("[::1]:5000"));
        assert!(!is_loopback_host("evil.com:5000"));
        assert!(!is_loopback_host("evil.com"));
        assert!(!is_loopback_host("127.0.0.1.evil.com:5000"));
    }

    #[test]
    fn only_json_bodies_are_accepted() {
        assert!(is_json_content_type("application/json"));
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("APPLICATION/JSON"));
        // The three a browser form can send, none of which may reach a verb.
        assert!(!is_json_content_type("text/plain"));
        assert!(!is_json_content_type("application/x-www-form-urlencoded"));
        assert!(!is_json_content_type("multipart/form-data"));
    }

    #[test]
    fn a_request_carries_its_token_and_length() {
        let mut out = Vec::new();
        write_request(&mut out, "POST", "/command", 5000, "deadbeef", Some(b"{}")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("POST /command HTTP/1.1\r\n"), "{text}");
        assert!(
            text.contains("Authorization: Bearer deadbeef\r\n"),
            "{text}"
        );
        assert!(
            text.contains("Content-Type: application/json\r\n"),
            "{text}"
        );
        assert!(text.contains("Content-Length: 2\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n{}"), "{text}");

        // A GET carries neither.
        let mut out = Vec::new();
        write_request(&mut out, "GET", "/health", 5000, "t", None).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Content-Length"), "{text}");
        assert!(text.contains("Host: 127.0.0.1:5000\r\n"), "{text}");
    }

    #[test]
    fn a_head_parses_case_insensitively() {
        let raw = b"HTTP/1.1 200 OK\r\nCONTENT-length: 5\r\nX-Thing: a\r\n\r\nhello";
        let mut r = BufReader::new(&raw[..]);
        let (status, headers) = read_head(&mut r).unwrap();
        assert_eq!(status, 200);
        assert_eq!(header_of(&headers, "content-length"), Some("5"));
        assert_eq!(header_of(&headers, "X-THING"), Some("a"));
        assert_eq!(header_of(&headers, "absent"), None);
    }

    #[test]
    fn frames_are_dispatched_on_a_blank_line_and_heartbeats_are_skipped() {
        let raw =
            ":\n\nevent: snapshot\ndata: {\"a\":1}\n\n:\n\nevent: heartbeat\ndata: {\"b\":2}\n\n";
        let frames: Vec<Frame> = Frames::new(BufReader::new(raw.as_bytes()))
            .map(|f| f.unwrap())
            .collect();
        assert_eq!(
            frames,
            vec![
                Frame {
                    event: Some("snapshot".into()),
                    data: "{\"a\":1}".into()
                },
                Frame {
                    event: Some("heartbeat".into()),
                    data: "{\"b\":2}".into()
                },
            ],
            "a comment-only block must not dispatch an empty frame"
        );
    }

    #[test]
    fn a_multi_line_data_field_is_joined_with_newlines() {
        // Per the SSE spec. rote never sends one, but a parser that silently
        // dropped the second line would corrupt JSON rather than fail loudly.
        let raw = "data: one\ndata: two\n\n";
        let frames: Vec<Frame> = Frames::new(BufReader::new(raw.as_bytes()))
            .map(|f| f.unwrap())
            .collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "one\ntwo");
    }

    #[test]
    fn a_truncated_stream_ends_rather_than_yielding_a_partial_frame() {
        let raw = "event: snapshot\ndata: {\"a\":1}\n";
        let frames: Vec<Frame> = Frames::new(BufReader::new(raw.as_bytes()))
            .map(|f| f.unwrap())
            .collect();
        assert!(frames.is_empty(), "no blank line, no frame");
    }
}
