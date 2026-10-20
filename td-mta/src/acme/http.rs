//! One bounded HTTPS exchange. No redirects, ambient proxy, or connection reuse.
use super::Error;
use crate::{
    bounded::TextBuffer,
    clock::{RuntimeClock, TlsClockSource},
    config::endpoint::HttpsUri,
    ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
    tls_io::{TlsIo, TLS_WIRE_BYTES},
    transport::TcpTransport,
};
use std::{
    net::{SocketAddr, TcpStream},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use td_crypto::{ClientConfig, ClockHandle, TlsProtocol, TlsSession, TrustStore};

const MAX_BODY: usize = 65536;
const MAX_WIRE: usize = 128 * 1024;
const MAX_HEAD: usize = 16384;
pub struct Response {
    pub status: u16,
    pub nonce: Option<String>,
    pub location: Option<String>,
    pub retry_after: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}
pub trait Exchange {
    fn exchange(&mut self, method: &str, url: &str, body: &[u8]) -> Result<Response, Error>;
}
pub struct Https<'a> {
    pub executable: &'a Path,
    pub directory: &'a str,
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
impl Exchange for Https<'_> {
    fn exchange(&mut self, method: &str, url: &str, body: &[u8]) -> Result<Response, Error> {
        if !matches!(method, "GET" | "HEAD" | "POST")
            || body.len() > 32768
            || (method != "POST" && !body.is_empty())
        {
            return Err(Error::Invalid);
        }
        let origin = HttpsUri::parse(self.directory).map_err(|_| Error::Invalid)?;
        let uri = HttpsUri::parse(url).map_err(|_| Error::Invalid)?;
        if !origin.same_origin(uri) {
            return Err(Error::Invalid);
        }
        let deadline = self.deadline.min(Instant::now() + Duration::from_secs(15));
        let addresses = crate::postgres_resolver::resolve_host(
            self.executable,
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
        let socket = socket.ok_or(Error::Transport)?;
        let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
        let now = clock.sample().map_err(|_| Error::Deadline)?.monotonic;
        let duration = deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        let cap = Deadline::after(now, duration.as_millis() as u64).map_err(|_| Error::Deadline)?;
        let config = Arc::new(
            ClientConfig::new(
                self.trust,
                Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone()))),
                TlsProtocol::Http1,
            )
            .map_err(|_| Error::Crypto)?,
        );
        let session = TlsSession::client(config, uri.host()).map_err(|_| Error::Crypto)?;
        let mut transport = TlsIo::new(
            TcpTransport::from_stream(socket).map_err(|_| Error::Transport)?,
            session,
            clock,
            cap,
            cap,
            Box::new([0; TLS_WIRE_BYTES]),
            Box::new([0; TLS_WIRE_BYTES]),
        )
        .map_err(|_| Error::Transport)?;
        while transport
            .handshake()
            .map_err(|_| Error::Transport)?
            .is_none()
        {
            pause(deadline)?;
        }
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
        let mut request = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: td-mta-acme\r\nAccept: application/json, application/pem-certificate-chain\r\nConnection: close\r\n");
        if method == "POST" {
            request.push_str(&format!(
                "Content-Type: application/jose+json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        request.push_str("\r\n");
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(body);
        let mut written = 0;
        while written < bytes.len() {
            match transport
                .write(bytes.get(written..).ok_or(Error::Invalid)?)
                .map_err(|_| Error::Transport)?
            {
                IoProgress::Bytes(n) if n > 0 => {
                    written = written.checked_add(n).ok_or(Error::Capacity)?
                }
                IoProgress::Pending => pause(deadline)?,
                _ => return Err(Error::Transport),
            }
        }
        while transport.flush().map_err(|_| Error::Transport)? != FlushProgress::Complete {
            pause(deadline)?;
        }
        let mut response = Vec::with_capacity(MAX_WIRE);
        let mut buffer = [0; 4096];
        loop {
            if let Some(done) = decode(&response, method == "HEAD", false)? {
                return Ok(done);
            }
            match transport.read(&mut buffer).map_err(|_| Error::Transport)? {
                IoProgress::Bytes(n) => {
                    if response
                        .len()
                        .checked_add(n)
                        .filter(|n| *n <= MAX_WIRE)
                        .is_none()
                    {
                        return Err(Error::Capacity);
                    }
                    response.extend_from_slice(buffer.get(..n).ok_or(Error::Invalid)?);
                }
                IoProgress::Pending => pause(deadline)?,
                IoProgress::Closed => {
                    return decode(&response, method == "HEAD", true)?.ok_or(Error::Protocol)
                }
            }
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
/// Exact framing, including chunked transfer. Close-delimited bodies require TLS EOF.
fn decode(bytes: &[u8], head: bool, eof: bool) -> Result<Option<Response>, Error> {
    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
        if bytes.len() > MAX_HEAD || eof {
            return Err(Error::Protocol);
        }
        return Ok(None);
    };
    if end > MAX_HEAD || bytes.len() > MAX_WIRE {
        return Err(Error::Capacity);
    }
    let headers = std::str::from_utf8(bytes.get(..end).ok_or(Error::Invalid)?)
        .map_err(|_| Error::Protocol)?;
    let mut lines = headers.split("\r\n");
    let status = lines.next().ok_or(Error::Protocol)?;
    let mut parts = status.splitn(3, ' ');
    if !matches!(parts.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
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
        body: Vec::new(),
    };
    let (mut length, mut chunked) = (None, false);
    for (index, line) in lines.enumerate() {
        if index >= 100 {
            return Err(Error::Capacity);
        }
        let (name, value) = line.split_once(':').ok_or(Error::Protocol)?;
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
            "replay-nonce" => unique(&mut response.nonce, value)?,
            "location" => unique(&mut response.location, value)?,
            "retry-after" => unique(&mut response.retry_after, value)?,
            "content-type" => unique(&mut response.content_type, value)?,
            "content-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err(Error::Protocol)
            }
            _ => (),
        }
    }
    if length.is_some() && chunked {
        return Err(Error::Protocol);
    }
    let body = bytes.get(end + 4..).ok_or(Error::Invalid)?;
    if head || status == 204 || status == 304 {
        if !body.is_empty() {
            return Err(Error::Protocol);
        }
        return Ok(Some(response));
    }
    if chunked {
        let mut position = 0usize;
        let mut chunks = 0;
        loop {
            let tail = body.get(position..).ok_or(Error::Invalid)?;
            let Some(end) = tail.windows(2).position(|w| w == b"\r\n") else {
                return if eof { Err(Error::Protocol) } else { Ok(None) };
            };
            if end > 16 || chunks >= 4096 {
                return Err(Error::Capacity);
            }
            let line = std::str::from_utf8(tail.get(..end).ok_or(Error::Invalid)?)
                .map_err(|_| Error::Protocol)?;
            if line.is_empty() || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Protocol);
            }
            let size = usize::from_str_radix(line, 16).map_err(|_| Error::Capacity)?;
            if response
                .body
                .len()
                .checked_add(size)
                .filter(|n| *n <= MAX_BODY)
                .is_none()
            {
                return Err(Error::Capacity);
            }
            position = position.checked_add(end + 2).ok_or(Error::Capacity)?;
            let next = position
                .checked_add(size)
                .and_then(|p| p.checked_add(2))
                .ok_or(Error::Capacity)?;
            if next > body.len() {
                return if eof { Err(Error::Protocol) } else { Ok(None) };
            }
            if body.get(next - 2..next) != Some(b"\r\n") {
                return Err(Error::Protocol);
            }
            response
                .body
                .extend_from_slice(body.get(position..next - 2).ok_or(Error::Invalid)?);
            position = next;
            chunks += 1;
            if size == 0 {
                if position != body.len() {
                    return Err(Error::Protocol);
                }
                return Ok(Some(response));
            }
        }
    }
    if let Some(length) = length {
        if length > MAX_BODY || body.len() > length {
            return Err(Error::Capacity);
        }
        if body.len() < length {
            return if eof { Err(Error::Protocol) } else { Ok(None) };
        }
    } else if body.len() > MAX_BODY {
        return Err(Error::Capacity);
    } else if !eof {
        return Ok(None);
    }
    response.body.extend_from_slice(body);
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_refuses_ambiguity_and_truncation() -> Result<(), Error> {
        let valid = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nReplay-Nonce: abc\r\n\r\n{}";
        for end in 0..valid.len() {
            assert!(decode(valid.get(..end).ok_or(Error::Invalid)?, false, false)?.is_none());
        }
        assert_eq!(
            decode(valid, false, false)?.ok_or(Error::Invalid)?.body,
            b"{}"
        );
        for invalid in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nReplay-Nonce: a\r\nReplay-Nonce: b\r\n\r\n",
        ] {
            assert!(decode(invalid, false, true).is_err());
        }
        assert_eq!(
            decode(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
                false,
                false
            )?
            .ok_or(Error::Invalid)?
            .body,
            b"{}"
        );
        Ok(())
    }
}
