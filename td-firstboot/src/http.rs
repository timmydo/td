//! A minimal HTTP/1.1 client for link-local metadata services: one request
//! per connection, a bounded response, and no redirects, proxies or TLS.

use std::io::{Read, Write};
use std::net::{SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

/// A response's whole size, head and body.
const MAX_RESPONSE: usize = 256 * 1024;
const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

impl Response {
    /// The body of a 200, as text; a 404 is `None`; anything else fails.
    pub(crate) fn text_or_absent(self, what: &str) -> Result<Option<String>, String> {
        match self.status {
            200 => String::from_utf8(self.body)
                .map(Some)
                .map_err(|_| format!("{what} is not UTF-8")),
            404 => Ok(None),
            status => Err(format!("{what}: HTTP {status}")),
        }
    }
}

/// `method path` to `address`, with `headers` and `body`, connected, sent
/// and read to the response's end within `timeout`.
pub(crate) fn request(
    address: SocketAddrV4,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let deadline = Instant::now() + timeout;
    let fail = |error: String| format!("{method} http://{address}{path}: {error}");
    let left = || {
        Some(deadline.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .ok_or_else(|| fail(format!("no response within {timeout:?}")))
    };
    let io = |error: std::io::Error| fail(error.to_string());
    let mut stream = TcpStream::connect_timeout(&SocketAddr::V4(address), left()?).map_err(io)?;
    stream.set_write_timeout(Some(left()?)).map_err(io)?;
    stream
        .write_all(&head(address, method, path, headers, body.len()))
        .and_then(|()| stream.write_all(body))
        .map_err(io)?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    // A server that keeps the connection open past a whole response is
    // not waited on.
    loop {
        stream.set_read_timeout(Some(left()?)).map_err(io)?;
        let read = stream.read(&mut buffer).map_err(io)?;
        bytes.extend_from_slice(buffer.get(..read).unwrap_or_default());
        if bytes.len() > MAX_RESPONSE {
            return Err(fail(format!("response past {MAX_RESPONSE} bytes")));
        }
        if let Some(response) = parse(&bytes, read == 0).map_err(fail)? {
            return Ok(response);
        }
    }
}

fn head(
    address: SocketAddrV4,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    length: usize,
) -> Vec<u8> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: td-firstboot\r\nConnection: close\r\n",
        address.ip()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if length > 0 || method != "GET" {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// The response in `bytes`, its body ended by `Content-Length`, chunked
/// transfer coding or the connection's end; `None` while more is to come
/// and the connection is not `closed`.
fn parse(bytes: &[u8], closed: bool) -> Result<Option<Response>, String> {
    let Some(end) = bytes
        .windows(4)
        .take(MAX_HEAD)
        .position(|window| window == b"\r\n\r\n")
    else {
        if closed || bytes.len() >= MAX_HEAD + 3 {
            return Err("no complete response head".into());
        }
        return Ok(None);
    };
    let head = std::str::from_utf8(bytes.get(..end).ok_or("short head")?)
        .map_err(|_| "a response head that is not UTF-8")?;
    let rest = bytes.get(end + 4..).ok_or("short head")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or("no status line")?;
    let mut fields = status_line.splitn(3, ' ');
    let version = fields.next().unwrap_or_default();
    let status = fields
        .next()
        .filter(|code| code.len() == 3)
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|_| version == "HTTP/1.1" || version == "HTTP/1.0")
        .ok_or_else(|| format!("a malformed status line {status_line:?}"))?;
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("a malformed header")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| "a malformed Content-Length")?;
            if length.is_some_and(|length| length != parsed) {
                return Err("conflicting Content-Length".into());
            }
            length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.eq_ignore_ascii_case("chunked") {
                return Err(format!("an unsupported Transfer-Encoding {value:?}"));
            }
            chunked = true;
        }
    }
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(length) = length {
        rest.get(..length).map(<[u8]>::to_vec)
    } else {
        closed.then(|| rest.to_vec())
    };
    match body {
        Some(body) => Ok(Some(Response { status, body })),
        None if closed => Err("a body cut short by the connection's end".into()),
        None => Ok(None),
    }
}

/// A chunked body, or `None` until its last chunk has arrived.
fn dechunk(mut rest: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            return Ok(None);
        };
        let line = std::str::from_utf8(rest.get(..line_end).unwrap_or_default())
            .map_err(|_| "a chunk size that is not UTF-8")?;
        let size = line.split(';').next().unwrap_or_default().trim();
        if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("a malformed chunk size".into());
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| "a malformed chunk size")?;
        rest = rest.get(line_end + 2..).unwrap_or_default();
        if size == 0 {
            return Ok(Some(body));
        }
        let end = size.checked_add(2).ok_or("a malformed chunk size")?;
        match rest.get(size..end) {
            None => return Ok(None),
            Some(b"\r\n") => {}
            Some(_) => return Err("an unterminated chunk".into()),
        }
        body.extend_from_slice(rest.get(..size).unwrap_or_default());
        rest = rest.get(end..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    #[test]
    fn a_response_is_read_by_length_chunks_or_its_end() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello trailing";
        let hello = Some(Response {
            status: 200,
            body: b"hello".to_vec(),
        });
        // Complete whether or not the connection has closed.
        assert_eq!(parse(plain, false).unwrap(), hello);
        assert_eq!(parse(plain, true).unwrap(), hello);
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n";
        assert_eq!(parse(chunked, false).unwrap(), hello);
        let closed = b"HTTP/1.0 404 Not Found\r\nServer: x\r\n\r\ngone";
        assert_eq!(parse(closed, false).unwrap(), None);
        assert_eq!(
            parse(closed, true).unwrap(),
            Some(Response {
                status: 404,
                body: b"gone".to_vec()
            })
        );
        // Each prefix of a response waits for more while the connection
        // is open, and is refused once it has closed.
        for whole in [&plain[..43], &chunked[..]] {
            for cut in 0..whole.len() - 2 {
                assert_eq!(parse(&whole[..cut], false).unwrap(), None, "{cut}");
                assert!(parse(&whole[..cut], true).is_err(), "{cut}");
            }
        }
        for bad in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
            b"HTTP/2 200\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nno colon\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n+2\r\nab\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabcd",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n",
            b"HTTP/1.1 200 OK\r\n",
        ] {
            assert!(
                parse(bad, true).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn a_request_names_its_host_and_closes() {
        let address = SocketAddrV4::new([169, 254, 169, 254].into(), 80);
        assert_eq!(
            String::from_utf8(head(address, "PUT", "/t", &[("X-A", "1")], 0)).unwrap(),
            "PUT /t HTTP/1.1\r\nHost: 169.254.169.254\r\nUser-Agent: td-firstboot\r\n\
             Connection: close\r\nX-A: 1\r\nContent-Length: 0\r\n\r\n"
        );
        assert!(!String::from_utf8(head(address, "GET", "/", &[], 0))
            .unwrap()
            .contains("Content-Length"));
    }

    #[test]
    fn only_a_200_is_text_and_a_404_is_absent() {
        let response = |status| Response {
            status,
            body: b"x".to_vec(),
        };
        assert_eq!(
            response(200).text_or_absent("x").unwrap().as_deref(),
            Some("x")
        );
        assert_eq!(response(404).text_or_absent("x").unwrap(), None);
        assert!(response(500).text_or_absent("x").is_err());
    }

    /// Against a local listener, as a metadata service answers, but keeping
    /// the connection open until the client has its response.
    #[test]
    fn a_request_round_trips_over_loopback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            panic!("an IPv4 listener has an IPv4 address");
        };
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buffer = [0; 512];
            while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                seen.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            wait.recv().unwrap();
            String::from_utf8(seen).unwrap()
        });
        let response = request(
            address,
            "GET",
            "/latest",
            &[("X-T", "v")],
            b"",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(response.body, b"ok");
        done.send(()).unwrap();
        assert!(server
            .join()
            .unwrap()
            .starts_with("GET /latest HTTP/1.1\r\n"));
    }
}
