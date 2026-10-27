//! Minimal blocking HTTP/1.1 client over `std::net::TcpStream`.
//!
//! The composition root polls the fox-services HTTP APIs of the configured
//! nodes; this client is all it needs: `GET` of a JSON document with a
//! bounded body. No redirects, no chunked-encoding surprises (decoded when
//! present), `https` refused (fox-services serves plain HTTP on the LAN).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// What went wrong fetching a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The URL is not a plain `http://host[:port]/path` URL.
    BadUrl(String),
    /// DNS resolution found no address.
    Unresolvable(String),
    /// `connect`, a write or a read failed or timed out.
    Io(String),
    /// The status line is missing or not `200`.
    Status(u16, String),
    /// The body is absent, over the cap, or not valid JSON.
    Body(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadUrl(detail) => write!(formatter, "bad URL ({detail})"),
            Self::Unresolvable(host) => write!(formatter, "cannot resolve {host}"),
            Self::Io(detail) => write!(formatter, "network error ({detail})"),
            Self::Status(code, reason) => write!(formatter, "http {code} ({reason})"),
            Self::Body(detail) => write!(formatter, "bad body ({detail})"),
        }
    }
}

impl std::error::Error for HttpError {}

/// Upper bound of one JSON body (the stats/projects documents are small;
/// anything bigger is a misconfiguration, not a snapshot).
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// `GET` the URL and parse the body as JSON.
pub fn get_json(
    base_url: &str,
    path: &str,
    timeout: Duration,
) -> Result<serde_json::Value, HttpError> {
    let body = get_bytes(base_url, path, timeout)?;
    serde_json::from_slice(&body).map_err(|error| HttpError::Body(error.to_string()))
}

fn get_bytes(base_url: &str, path: &str, timeout: Duration) -> Result<Vec<u8>, HttpError> {
    let (host, port, host_header) = split_base(base_url)?;
    let address = resolve(&host, port)?;
    let mut stream = TcpStream::connect_timeout(&address, timeout).map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            HttpError::Io(format!("connection timed out after {timeout:?}"))
        } else {
            HttpError::Io(trim_error(&error))
        }
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .and(stream.set_write_timeout(Some(timeout)))
        .map_err(|error| HttpError::Io(trim_error(&error)))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| HttpError::Io(io_error(&error, timeout)))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&chunk[..read]);
                if raw.len() > MAX_BODY_BYTES + 65536 {
                    return Err(HttpError::Body("response over the size cap".into()));
                }
            }
            Err(error) => return Err(HttpError::Io(io_error(&error, timeout))),
        }
    }
    let head_end = find_crlf_crlf(&raw).ok_or_else(|| HttpError::Body("no header block".into()))?;
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Body("no status line".into()))?;
    if code != 200 {
        let reason = status_line.splitn(3, ' ').nth(2).unwrap_or("").to_owned();
        return Err(HttpError::Status(code, reason));
    }
    let mut chunked = false;
    for line in lines {
        if line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    let mut body = raw[head_end..].to_vec();
    if chunked {
        body = decode_chunked(&body)?;
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(HttpError::Body("response over the size cap".into()));
    }
    Ok(body)
}

/// `(host, port, host_header)` of an `http://` base URL.
fn split_base(base_url: &str) -> Result<(String, u16, String), HttpError> {
    let rest = base_url
        .strip_prefix("http://")
        .ok_or_else(|| HttpError::BadUrl("only http:// is supported".into()))?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(HttpError::BadUrl(
            "path, query or fragment in base URL".into(),
        ));
    }
    if let Some((host, port)) = rest.rsplit_once(':')
        && !port.is_empty()
        && let Ok(port) = port.parse::<u16>()
        && !host.is_empty()
        && !host.ends_with(']')
    {
        let header = if port == 80 {
            host.to_owned()
        } else {
            rest.to_owned()
        };
        return Ok((unstrip(host), port, header));
    }
    Ok((unstrip(rest), 80, rest.to_owned()))
}

fn unstrip(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned()
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, HttpError> {
    (host, port)
        .to_socket_addrs()
        .map_err(|_| HttpError::Unresolvable(host.to_owned()))?
        .next()
        .ok_or_else(|| HttpError::Unresolvable(host.to_owned()))
}

fn find_crlf_crlf(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| HttpError::Body("bad chunk header".into()))?;
        let size_text = std::str::from_utf8(&rest[..end])
            .map_err(|_| HttpError::Body("bad chunk header".into()))?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Body("bad chunk size".into()))?;
        rest = &rest[end + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size + 2 || out.len() + size > MAX_BODY_BYTES {
            return Err(HttpError::Body("bad chunk data".into()));
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    Ok(out)
}

fn trim_error(error: &std::io::Error) -> String {
    let text = error.to_string();
    let mut short = text.as_str();
    for prefix in [
        "failed to lookup address information: ",
        "Connection refused (os error 111)",
        "Connection timed out (os error 110)",
    ] {
        if short.contains(prefix) {
            short = short.split(prefix).nth(1).unwrap_or(short);
        }
    }
    short.chars().take(120).collect()
}

/// Reads a socket error as a short message: on a socket with timeouts a
/// `WouldBlock` is an expired deadline, never a bare OS string.
fn io_error(error: &std::io::Error, timeout: Duration) -> String {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        format!("timed out after {timeout:?}")
    } else {
        trim_error(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn serve_once(body: &[u8], headers: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let owned = body.to_vec();
        let headers = headers.to_owned();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{headers}\r\n",
                owned.len()
            );
            socket.write_all(head.as_bytes()).unwrap();
            socket.write_all(&owned).unwrap();
        });
        base
    }

    #[test]
    fn json_round_trip() {
        let base = serve_once(br#"{"projects": [], "count": 0}"#, "");
        let value = get_json(&base, "/api/docker/projects", Duration::from_secs(5)).unwrap();
        assert_eq!(value["count"], 0);
    }

    #[test]
    fn chunked_bodies_decode() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n3\r\n 1}\r\n0\r\n\r\n")
                .unwrap();
        });
        let value = get_json(&base, "/x", Duration::from_secs(5)).unwrap();
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn bad_urls_and_statuses_are_errors() {
        assert!(matches!(
            get_json("https://x/", "/x", Duration::from_secs(1)),
            Err(HttpError::BadUrl(_))
        ));
        assert!(matches!(
            get_json("http://x/y", "/x", Duration::from_secs(1)),
            Err(HttpError::BadUrl(_))
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request);
            socket
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        assert!(matches!(
            get_json(&base, "/x", Duration::from_secs(5)),
            Err(HttpError::Status(502, _))
        ));
    }

    #[test]
    fn split_base_handles_ports() {
        assert_eq!(
            split_base("http://192.168.1.63:8210").unwrap(),
            (
                "192.168.1.63".to_owned(),
                8210,
                "192.168.1.63:8210".to_owned()
            )
        );
        assert_eq!(
            split_base("http://axiom/").unwrap(),
            ("axiom".to_owned(), 80, "axiom".to_owned())
        );
    }
}
