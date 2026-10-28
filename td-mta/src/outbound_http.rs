//! Bounded single-origin HTTPS requests with incremental body delivery.
use crate::{
    bounded::TextBuffer,
    clock::{RuntimeClock, TlsClockSource},
    config::endpoint::HttpsUri,
    ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    transport::TcpTransport,
};
use std::{
    io::{self, BufRead, BufReader, Read},
    net::{SocketAddr, TcpStream},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use td_crypto::{ClientConfig, ClockHandle, TlsProtocol, TlsSession, TrustStore};

const MAX_HEAD: usize = 16384;
pub const MAX_BODY: usize = 64 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    Crypto,
    Protocol,
    Transport,
    Deadline,
    Sink,
    Status(u16),
}
pub struct Response {
    pub status: u16,
    pub nonce: Option<String>,
    pub location: Option<String>,
    pub retry_after: Option<String>,
    pub content_type: Option<String>,
    pub bytes: usize,
}
pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub body: &'a [u8],
    pub content_type: &'a str,
    pub accept: &'a str,
    pub authorization: Option<&'a str>,
    pub body_limit: usize,
}
pub struct Client<'a> {
    pub executable: &'a Path,
    pub origin: &'a str,
    pub trust: &'a TrustStore,
    pub deadline: Instant,
}
fn pause(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        return Err(Error::Deadline);
    }
    std::thread::sleep(Duration::from_millis(1));
    Ok(())
}
fn header(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && value.bytes().all(|b| (32..127).contains(&b))
}
impl Client<'_> {
    pub fn exchange(
        &self,
        request: Request<'_>,
        sink: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<Response, Error> {
        self.exchange_inner(request, None, sink)
    }
    /// Refuse non-200 status after bounded headers, before any sink call.
    /// The socket is discarded without accepting or retaining its error body.
    pub fn exchange_success(
        &self,
        request: Request<'_>,
        sink: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<Response, Error> {
        self.exchange_inner(request, Some(200), sink)
    }
    fn exchange_inner(
        &self,
        request: Request<'_>,
        expected_status: Option<u16>,
        mut sink: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<Response, Error> {
        let uri = validate(self.origin, &request)?;
        let deadline = self.deadline.min(Instant::now() + Duration::from_secs(60));
        let mut transport = connect(self, uri, deadline)?;
        exchange_on(
            &mut transport,
            uri,
            request,
            deadline,
            false,
            expected_status,
            &mut sink,
        )
        .map(|(response, _)| response)
    }
}
type Connection = TlsIo<TcpTransport, Box<[u8; TLS_WIRE_BYTES]>>;

#[path = "outbound_http_reuse.rs"]
mod reuse;
pub use reuse::ReusableClient;

fn validate<'a>(origin: &str, request: &Request<'a>) -> Result<HttpsUri<'a>, Error> {
    if !matches!(request.method, "GET" | "HEAD" | "POST")
        || request.body.len() > 1024 * 1024
        || (request.method != "POST" && !request.body.is_empty())
        || request.body_limit > MAX_BODY
        || !header(request.content_type, 128)
        || !header(request.accept, 256)
        || request
            .authorization
            .is_some_and(|value| !header(value, 4096))
    {
        return Err(Error::Invalid);
    }
    let origin = HttpsUri::parse(origin).map_err(|_| Error::Invalid)?;
    let uri = HttpsUri::parse(request.url).map_err(|_| Error::Invalid)?;
    if !origin.same_origin(uri) {
        return Err(Error::Invalid);
    }
    Ok(uri)
}
fn connect(client: &Client<'_>, uri: HttpsUri<'_>, deadline: Instant) -> Result<Connection, Error> {
    let addresses = crate::postgres_resolver::resolve_host(
        client.executable,
        uri.host(),
        deadline.min(Instant::now() + Duration::from_secs(5)),
    )
    .map_err(|_| Error::Transport)?;
    let mut socket = None;
    for address in addresses {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if let Ok(connected) = TcpStream::connect_timeout(
            &SocketAddr::new(address, uri.port()),
            remaining.min(Duration::from_secs(2)),
        ) {
            socket = Some(connected);
            break;
        }
    }
    connected(
        socket.ok_or(Error::Transport)?,
        client.trust,
        uri.host(),
        deadline,
    )
}
fn connected(
    socket: TcpStream,
    trust: &TrustStore,
    host: &str,
    deadline: Instant,
) -> Result<Connection, Error> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let now = clock.sample().map_err(|_| Error::Deadline)?.monotonic;
    let duration = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Deadline)?;
    let cap = Deadline::after(now, duration.as_millis() as u64).map_err(|_| Error::Deadline)?;
    let config = Arc::new(
        ClientConfig::new(
            trust,
            Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone()))),
            TlsProtocol::Http1,
        )
        .map_err(|_| Error::Crypto)?,
    );
    let session = TlsSession::client(config, host).map_err(|_| Error::Crypto)?;
    let mut transport = TlsIo::new(
        TcpTransport::from_stream(socket).map_err(transport_error)?,
        session,
        clock,
        cap,
        cap,
        Box::new([0; TLS_WIRE_BYTES]),
        Box::new([0; TLS_WIRE_BYTES]),
    )
    .map_err(|refusal| transport_error(refusal.error()))?;
    while transport.handshake().map_err(transport_error)?.is_none() {
        if !transport
            .wait_handshake_input(deadline)
            .map_err(transport_error)?
        {
            pause(deadline)?;
        }
    }
    Ok(transport)
}
fn exchange_on(
    transport: &mut Connection,
    uri: HttpsUri<'_>,
    request: Request<'_>,
    deadline: Instant,
    keep_alive: bool,
    expected_status: Option<u16>,
    sink: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(Response, bool), Error> {
    let mut target_bytes = [0; 4097];
    let mut target = TextBuffer::new(&mut target_bytes);
    uri.write_request_target(&mut target)
        .map_err(|_| Error::Invalid)?;
    let target = target.as_str().map_err(|_| Error::Invalid)?;
    let host = if uri.port() == 443 {
        uri.host().to_owned()
    } else {
        format!("{}:{}", uri.host(), uri.port())
    };
    let method = request.method;
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: td-mta\r\nAccept: {}\r\nAccept-Encoding: identity\r\nConnection: {connection}\r\n", request.accept);
    if let Some(auth) = request.authorization {
        head.push_str(&format!("Authorization: {auth}\r\n"));
    }
    if method == "POST" {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            request.content_type,
            request.body.len()
        ));
    }
    head.push_str("\r\n");
    for mut part in [head.as_bytes(), request.body] {
        while !part.is_empty() {
            pause_check(deadline)?;
            match transport.write(part).map_err(transport_error)? {
                IoProgress::Bytes(n) if n > 0 => part = part.get(n..).ok_or(Error::Protocol)?,
                IoProgress::Pending => pause(deadline)?,
                _ => return Err(Error::Transport),
            }
        }
    }
    while transport.flush().map_err(transport_error)? != FlushProgress::Complete {
        pause(deadline)?;
    }
    let reader = Wire {
        transport,
        deadline,
    };
    receive_framed(
        reader,
        method == "HEAD",
        request.body_limit,
        expected_status,
        sink,
    )
}
fn pause_check(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
struct Wire<'a> {
    transport: &'a mut Connection,
    deadline: Instant,
}
impl Read for Wire<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.transport.read(output).map_err(transport_io_error)? {
                IoProgress::Bytes(n) if n > 0 && n <= output.len() => return Ok(n),
                IoProgress::Closed => return Ok(0),
                IoProgress::Pending => {
                    if !self
                        .transport
                        .wait_input(self.deadline)
                        .map_err(transport_io_error)?
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                _ => return Err(io::ErrorKind::InvalidData.into()),
            }
        }
    }
}
fn transport_error(error: crate::ports::Error) -> Error {
    if error == crate::ports::Error::Deadline {
        Error::Deadline
    } else {
        Error::Transport
    }
}
fn transport_io_error(error: crate::ports::Error) -> io::Error {
    if error == crate::ports::Error::Deadline {
        io::ErrorKind::TimedOut.into()
    } else {
        io::ErrorKind::ConnectionAborted.into()
    }
}
fn io_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut {
        Error::Deadline
    } else {
        Error::Transport
    }
}
fn line(reader: &mut impl BufRead, maximum: usize) -> Result<Vec<u8>, Error> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(io_error)?;
        if available.is_empty() {
            return Err(Error::Protocol);
        }
        let end = available
            .iter()
            .position(|b| *b == b'\n')
            .and_then(|n| n.checked_add(1));
        let n = end.unwrap_or(available.len());
        if line.len().checked_add(n).is_none_or(|size| size > maximum) {
            return Err(Error::Capacity);
        }
        line.extend_from_slice(available.get(..n).ok_or(Error::Protocol)?);
        reader.consume(n);
        if end.is_some() {
            if !line.ends_with(b"\r\n") {
                return Err(Error::Protocol);
            }
            line.truncate(line.len().saturating_sub(2));
            return Ok(line);
        }
    }
}
fn unique(slot: &mut Option<String>, value: &str) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::Protocol);
    }
    *slot = Some(value.to_owned());
    Ok(())
}
fn number(value: &str) -> Result<usize, Error> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    value.parse().map_err(|_| Error::Capacity)
}
fn copy_exact(
    reader: &mut impl Read,
    mut count: usize,
    sink: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut buffer = [0; 8192];
    while count > 0 {
        let limit = count.min(buffer.len());
        let n = reader
            .read(buffer.get_mut(..limit).ok_or(Error::Protocol)?)
            .map_err(io_error)?;
        if n == 0 {
            return Err(Error::Protocol);
        }
        sink(buffer.get(..n).ok_or(Error::Protocol)?)?;
        count = count.checked_sub(n).ok_or(Error::Protocol)?;
    }
    Ok(())
}
/// Sink output is provisional until this returns success. Each socket has one
/// response and is discarded, so bytes beyond its framed body are never reused.
#[cfg(test)]
fn receive(
    input: impl Read,
    head: bool,
    maximum: usize,
    sink: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<Response, Error> {
    receive_checked(input, head, maximum, None, sink)
}
#[cfg(test)]
fn receive_checked(
    input: impl Read,
    head: bool,
    maximum: usize,
    expected_status: Option<u16>,
    sink: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<Response, Error> {
    receive_framed(input, head, maximum, expected_status, sink).map(|(response, _)| response)
}
fn receive_framed(
    input: impl Read,
    head: bool,
    maximum: usize,
    expected_status: Option<u16>,
    sink: &mut impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(Response, bool), Error> {
    let mut reader = BufReader::with_capacity(8192, input);
    let status_line = line(&mut reader, MAX_HEAD)?;
    let mut used = status_line.len() + 2;
    let status_line = std::str::from_utf8(&status_line).map_err(|_| Error::Protocol)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next();
    if !matches!(version, Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(Error::Protocol);
    }
    let code = parts.next().ok_or(Error::Protocol)?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    let status: u16 = code.parse().map_err(|_| Error::Protocol)?;
    if !(200..600).contains(&status) {
        return Err(Error::Protocol);
    }
    let mut response = Response {
        status,
        nonce: None,
        location: None,
        retry_after: None,
        content_type: None,
        bytes: 0,
    };
    let (mut length, mut chunked) = (None, false);
    let mut reusable = version == Some("HTTP/1.1");
    for index in 0..=100 {
        let bytes = line(
            &mut reader,
            MAX_HEAD.checked_sub(used).ok_or(Error::Capacity)?,
        )?;
        used = used.checked_add(bytes.len() + 2).ok_or(Error::Capacity)?;
        if bytes.is_empty() {
            break;
        }
        if index == 100 {
            return Err(Error::Capacity);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::Protocol)?;
        let (name, value) = text.split_once(':').ok_or(Error::Protocol)?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b < 32 && b != b'\t' || b == 127)
        {
            return Err(Error::Protocol);
        }
        let value = value.trim_matches([' ', '\t']);
        match name.to_ascii_lowercase().as_str() {
            "content-length" => {
                if length.is_some() {
                    return Err(Error::Protocol);
                }
                length = Some(number(value)?);
            }
            "transfer-encoding" => {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(Error::Protocol);
                }
                chunked = true;
            }
            "content-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err(Error::Protocol)
            }
            "connection" => {
                // Unknown connection options may alter framing semantics.
                for option in value.split(',').map(str::trim) {
                    if !option.eq_ignore_ascii_case("keep-alive") {
                        reusable = false;
                    }
                }
            }
            "upgrade" => reusable = false,
            "replay-nonce" => unique(&mut response.nonce, value)?,
            "location" => unique(&mut response.location, value)?,
            "retry-after" => unique(&mut response.retry_after, value)?,
            "content-type" => unique(&mut response.content_type, value)?,
            _ => {}
        }
    }
    if length.is_some() && chunked {
        return Err(Error::Protocol);
    }
    if expected_status.is_some_and(|expected| status != expected) {
        return Err(Error::Status(status));
    }
    if head || status == 204 || status == 304 {
        // Keep no-body responses single-use; this also avoids reusing a
        // connection whose sender incorrectly supplied body framing.
        return Ok((response, false));
    }
    if chunked {
        for _ in 0..65536 {
            let bytes = line(&mut reader, 18)?;
            if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_hexdigit) {
                return Err(Error::Protocol);
            }
            let n = usize::from_str_radix(
                std::str::from_utf8(&bytes).map_err(|_| Error::Protocol)?,
                16,
            )
            .map_err(|_| Error::Capacity)?;
            response.bytes = response
                .bytes
                .checked_add(n)
                .filter(|n| *n <= maximum)
                .ok_or(Error::Capacity)?;
            copy_exact(&mut reader, n, sink)?;
            if !line(&mut reader, 2)?.is_empty() {
                return Err(Error::Protocol);
            }
            if n == 0 {
                return Ok((response, reusable && reader.buffer().is_empty()));
            }
        }
        return Err(Error::Capacity);
    }
    if let Some(n) = length {
        if n > maximum {
            return Err(Error::Capacity);
        }
        copy_exact(&mut reader, n, sink)?;
        response.bytes = n;
    } else {
        reusable = false;
        let mut buffer = [0; 8192];
        loop {
            let n = reader.read(&mut buffer).map_err(io_error)?;
            if n == 0 {
                break;
            }
            response.bytes = response
                .bytes
                .checked_add(n)
                .filter(|n| *n <= maximum)
                .ok_or(Error::Capacity)?;
            sink(buffer.get(..n).ok_or(Error::Protocol)?)?;
        }
    }
    Ok((response, reusable && reader.buffer().is_empty()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    struct Fragmented<'a> {
        bytes: &'a [u8],
        width: usize,
    }
    impl Read for Fragmented<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(self.width).min(self.bytes.len());
            out.get_mut(..n)
                .unwrap()
                .copy_from_slice(self.bytes.get(..n).unwrap());
            self.bytes = self.bytes.get(n..).unwrap();
            Ok(n)
        }
    }
    fn decode(bytes: &[u8], width: usize, limit: usize) -> Result<(Response, Vec<u8>), Error> {
        let mut body = Vec::new();
        let response = receive(Fragmented { bytes, width }, false, limit, &mut |part| {
            body.extend_from_slice(part);
            Ok(())
        })?;
        Ok((response, body))
    }
    #[test]
    fn success_only_refuses_busy_before_touching_even_a_zero_capacity_sink() {
        let bytes = b"HTTP/1.1 429 Busy\r\nContent-Length: 1000000000\r\n\r\nerror bytes";
        for width in 1..bytes.len() {
            let mut calls = 0;
            assert_eq!(
                receive_checked(
                    Fragmented { bytes, width },
                    false,
                    0,
                    Some(200),
                    &mut |_| {
                        calls += 1;
                        Err(Error::Sink)
                    }
                )
                .err(),
                Some(Error::Status(429))
            );
            assert_eq!(calls, 0);
        }
        let busy = b"HTTP/1.1 429 Busy\r\nContent-Length: 4\r\n\r\noops";
        let mut called = false;
        assert_eq!(
            receive_checked(busy.as_slice(), false, 64, Some(200), &mut |_| {
                called = true;
                Err(Error::Sink)
            })
            .err(),
            Some(Error::Status(429))
        );
        assert!(!called);
        let malformed = b"HTTP/1.1 429 Busy\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx";
        assert_eq!(
            receive_checked(malformed.as_slice(), false, 0, Some(200), &mut |_| Err(
                Error::Sink
            ))
            .err(),
            Some(Error::Protocol)
        );
        let mut bytes = Vec::new();
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(
            receive_checked(ok.as_slice(), false, 5, Some(200), &mut |part| {
                bytes.extend_from_slice(part);
                Ok(())
            })
            .unwrap()
            .bytes,
            5
        );
        assert_eq!(bytes, b"hello");
        assert_eq!(
            receive_checked(ok.as_slice(), false, 4, Some(200), &mut |_| Ok(())).err(),
            Some(Error::Capacity)
        );
    }
    #[test]
    fn all_framings_survive_every_small_fragment_width() {
        for wire in [b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".as_slice(),b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n",b"HTTP/1.0 200 OK\r\n\r\nhello"] {
            for width in 1..wire.len() {let (response,body)=decode(wire,width,5).unwrap();assert_eq!(response.bytes,5);assert_eq!(body,b"hello");}
            assert!(decode(wire,1,4).is_err());
            if wire.windows(14).any(|w|w==b"Content-Length") || wire.windows(17).any(|w|w==b"Transfer-Encoding") {
                for cut in 0..wire.len() {assert!(decode(wire.get(..cut).unwrap(),3,5).is_err(),"cut {cut}");}
            }
        }
    }
    #[test]
    fn ambiguous_and_unbounded_framing_is_refused() {
        for head in [
            "Content-Length: 1\r\nContent-Length: 1",
            "Content-Length: 1\r\nTransfer-Encoding: chunked",
            "Transfer-Encoding: gzip",
            "Content-Encoding: gzip",
            "Content-Length: +1",
            "Content-Length: 999999999999999999999999",
            "Replay-Nonce: a\r\nReplay-Nonce: b",
            " Bad: folded",
        ] {
            assert!(decode(
                format!("HTTP/1.1 200 OK\r\n{head}\r\n\r\nx").as_bytes(),
                2,
                10
            )
            .is_err());
        }
        assert!(decode(
            format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "x".repeat(MAX_HEAD)).as_bytes(),
            19,
            10
        )
        .is_err());
        for body in [
            "1;x=y\r\nx\r\n0\r\n\r\n",
            "0\r\nTrailer: value\r\n\r\n",
            "1\r\nx\n0\r\n\r\n",
        ] {
            assert!(decode(
                format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}").as_bytes(),
                1,
                10
            )
            .is_err());
        }
    }
    #[test]
    fn streaming_sink_and_transport_failures_never_report_success() {
        let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(
            receive(bytes.as_slice(), false, 5, &mut |_| Err(Error::Sink)).err(),
            Some(Error::Sink)
        );
        struct Timeout;
        impl Read for Timeout {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::TimedOut.into())
            }
        }
        assert_eq!(
            receive(Timeout, false, 5, &mut |_| Ok(())).err(),
            Some(Error::Deadline)
        );
        let mut input = io::Cursor::new(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_BODY}\r\n\r\n").into_bytes(),
        )
        .chain(io::repeat(42).take(MAX_BODY as u64));
        let mut largest = 0;
        let mut total = 0;
        let response = receive(&mut input, false, MAX_BODY, &mut |part| {
            largest = largest.max(part.len());
            total += part.len();
            assert!(part.iter().all(|b| *b == 42));
            Ok(())
        })
        .unwrap();
        assert_eq!(response.bytes, MAX_BODY);
        assert_eq!(total, MAX_BODY);
        assert!(largest <= 8192);
    }

    #[test]
    fn reuse_requires_explicit_framing_and_no_buffered_tail() {
        for (wire, head, allowed) in [
            ("HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx", false, true),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n",
                false,
                true,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: keep-alive\r\n\r\nx",
                false,
                true,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: keep-alive, close\r\n\r\nx",
                false,
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: unknown\r\n\r\nx",
                false,
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nUpgrade: h2c\r\n\r\nx",
                false,
                false,
            ),
            (
                "HTTP/1.0 200 OK\r\nContent-Length: 1\r\nConnection: keep-alive\r\n\r\nx",
                false,
                false,
            ),
            ("HTTP/1.1 200 OK\r\n\r\nx", false, false),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nxEXTRA",
                false,
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 123\r\n\r\n",
                true,
                false,
            ),
            ("HTTP/1.1 204 Empty\r\n\r\n", false, false),
            ("HTTP/1.1 304 Cached\r\n\r\n", false, false),
        ] {
            let (_, reusable) =
                receive_framed(wire.as_bytes(), head, 5, None, &mut |_| Ok(())).unwrap();
            assert_eq!(reusable, allowed, "{wire:?}");
        }
    }
    #[test]
    fn transport_deadlines_keep_their_category() {
        for (input, expected) in [
            (crate::ports::Error::Deadline, Error::Deadline),
            (crate::ports::Error::Tls, Error::Transport),
            (crate::ports::Error::Invalid, Error::Transport),
        ] {
            assert_eq!(transport_error(input), expected);
            assert_eq!(io_error(transport_io_error(input)), expected);
        }
    }
}
