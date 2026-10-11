//! Strict bounded HTTP/1.1 request framing, independent of transport and authority.
use std::fmt;

pub const HEAD_BYTES: usize = 32 * 1024;
pub const LINE_BYTES: usize = 8192;
pub const HEADER_COUNT: usize = 100;
pub const CHUNK_LINE_BYTES: usize = 1024;
/// One call never consumes more wire input than this scheduling quantum.
pub const QUANTUM: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Syntax,
    Limit,
    Unsupported,
    Incomplete,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Syntax => "invalid HTTP request framing",
            Self::Limit => "HTTP request limit exceeded",
            Self::Unsupported => "unsupported HTTP request framing",
            Self::Incomplete => "incomplete HTTP request",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub written: usize,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Framing {
    Length(u64),
    Chunked,
}

/// Contains credentials: deliberately has no Debug implementation. Values borrow
/// the complete head. Host is syntax only, never routing or URL authority.
pub struct Head<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub host: &'a str,
    pub authorization: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub framing: Framing,
    pub close: bool,
    pub expect_continue: bool,
}

/// Feed through the empty CRLF line only. There is no automatic reset or
/// next-request parsing; retire this owner before reusing its backing.
pub struct HeadReader<B: AsRef<[u8]> + AsMut<[u8]>> {
    storage: B,
    used: usize,
    line: usize,
    cr: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<B: AsRef<[u8]> + AsMut<[u8]>> HeadReader<B> {
    pub fn new(storage: B) -> Self {
        Self {
            storage,
            used: 0,
            line: 0,
            cr: false,
            complete: false,
            failure: None,
        }
    }
    pub fn feed(&mut self, input: &[u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let mut consumed = 0;
        if !self.complete {
            for &byte in input.iter().take(QUANTUM) {
                if let Err(error) = self.byte(byte) {
                    self.failure = Some(error);
                    return Err(error);
                }
                consumed += 1;
                if self.complete {
                    break;
                }
            }
        }
        Ok(Progress {
            consumed,
            written: 0,
            complete: self.complete,
        })
    }
    fn byte(&mut self, byte: u8) -> Result<(), Error> {
        if self.used >= HEAD_BYTES || self.line >= LINE_BYTES {
            return Err(Error::Limit);
        }
        *self
            .storage
            .as_mut()
            .get_mut(self.used)
            .ok_or(Error::Limit)? = byte;
        self.used += 1;
        self.line += 1;
        if self.cr {
            if byte != b'\n' {
                return Err(Error::Syntax);
            }
            self.cr = false;
            self.complete = self.line == 2;
            self.line = 0;
            if self.complete {
                parse(self.storage.as_ref().get(..self.used).ok_or(Error::Limit)?)?;
            }
        } else if byte == b'\r' {
            self.cr = true;
        } else if byte == b'\n' || !(byte == b'\t' || (32..=126).contains(&byte)) {
            return Err(Error::Syntax);
        }
        Ok(())
    }
    pub fn head(&self) -> Result<Head<'_>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        parse(self.storage.as_ref().get(..self.used).ok_or(Error::Limit)?)
    }
    /// EOF before the complete head is a terminal failure.
    pub fn eof(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete {
            self.failure = Some(Error::Incomplete);
            return Err(Error::Incomplete);
        }
        Ok(())
    }
}
impl<B: AsRef<[u8]> + AsMut<[u8]>> Drop for HeadReader<B> {
    fn drop(&mut self) {
        self.storage.as_mut().fill(0);
        std::hint::black_box(self.storage.as_mut());
    }
}

fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
fn field(line: &str) -> Result<(&str, &str), Error> {
    let (name, value) = line.split_once(':').ok_or(Error::Syntax)?;
    if name.is_empty() || !name.bytes().all(token) {
        return Err(Error::Syntax);
    }
    if !value.bytes().all(|b| b == b'\t' || (32..=126).contains(&b)) {
        return Err(Error::Syntax);
    }
    Ok((name, value.trim_matches([' ', '\t'])))
}
fn once<'a>(slot: &mut Option<&'a str>, value: &'a str) -> Result<(), Error> {
    if slot.replace(value).is_some() {
        return Err(Error::Syntax);
    }
    Ok(())
}
fn decimal(value: &str) -> Result<u64, Error> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Syntax);
    }
    value.parse().map_err(|_| Error::Limit)
}
fn target(value: &str) -> bool {
    if !value.starts_with('/') {
        return false;
    }
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            if !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !(byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&byte)) {
            return false;
        }
    }
    true
}
fn authority(value: &str) -> bool {
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return false;
        };
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        let port = if suffix.is_empty() {
            None
        } else {
            let Some(port) = suffix.strip_prefix(':') else {
                return false;
            };
            Some(port)
        };
        (host, port)
    } else {
        let (host, port) = value
            .split_once(':')
            .map_or((value, None), |(h, p)| (h, Some(p)));
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        {
            return false;
        }
        (host, port)
    };
    !host.is_empty() && port.is_none_or(|p| decimal(p).is_ok_and(|p| (1..=65535).contains(&p)))
}
fn parse(bytes: &[u8]) -> Result<Head<'_>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Syntax)?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next().ok_or(Error::Syntax)?.split(' ');
    let method = request.next().ok_or(Error::Syntax)?;
    let uri = request.next().ok_or(Error::Syntax)?;
    if method.is_empty() || !method.bytes().all(token) || !target(uri) {
        return Err(Error::Syntax);
    }
    let version = request.next().ok_or(Error::Syntax)?;
    if request.next().is_some() {
        return Err(Error::Syntax);
    }
    match version.as_bytes() {
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() => {}
        _ => return Err(Error::Syntax),
    }
    if version != "HTTP/1.1" {
        return Err(Error::Unsupported);
    }
    let mut host = None;
    let mut authorization = None;
    let mut content_type = None;
    let mut origin = None;
    let mut length = None;
    let mut transfer = None;
    let mut connection = None;
    let mut expect = None;
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            break;
        }
        if index >= HEADER_COUNT {
            return Err(Error::Limit);
        }
        let (name, value) = field(line)?;
        if name.eq_ignore_ascii_case("host") {
            once(&mut host, value)?;
        } else if name.eq_ignore_ascii_case("authorization") {
            once(&mut authorization, value)?;
        } else if name.eq_ignore_ascii_case("content-type") {
            once(&mut content_type, value)?;
        } else if name.eq_ignore_ascii_case("origin") {
            once(&mut origin, value)?;
        } else if name.eq_ignore_ascii_case("content-length") {
            once(&mut length, value)?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            once(&mut transfer, value)?;
        } else if name.eq_ignore_ascii_case("connection") {
            once(&mut connection, value)?;
        } else if name.eq_ignore_ascii_case("expect") {
            once(&mut expect, value)?;
        } else if ["content-encoding", "upgrade", "trailer"]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
        {
            return Err(Error::Unsupported);
        }
    }
    let host = host.filter(|h| authority(h)).ok_or(Error::Syntax)?;
    let framing = match (length, transfer) {
        (Some(_), Some(_)) => return Err(Error::Syntax),
        (_, Some(t)) if t.eq_ignore_ascii_case("chunked") => Framing::Chunked,
        (_, Some(_)) => return Err(Error::Unsupported),
        (Some(l), None) => Framing::Length(decimal(l)?),
        (None, None) => Framing::Length(0),
    };
    let mut close = false;
    if let Some(value) = connection {
        for item in value.split(',').map(|v| v.trim_matches([' ', '\t'])) {
            if item.eq_ignore_ascii_case("close") {
                close = true;
            } else if !item.eq_ignore_ascii_case("keep-alive") {
                return Err(Error::Unsupported);
            }
        }
    }
    if expect.is_some_and(|v| !v.eq_ignore_ascii_case("100-continue")) {
        return Err(Error::Unsupported);
    }
    Ok(Head {
        method,
        target: uri,
        host,
        authorization,
        content_type,
        origin,
        framing,
        close,
        expect_continue: expect.is_some(),
    })
}

#[derive(Clone, Copy)]
enum State {
    Size,
    Data(u64),
    DataCr,
    DataLf,
    EndCr,
    EndLf,
    Complete,
}
/// Streaming body decoder. The caller chooses an endpoint-specific decoded
/// limit and an aggregate wire limit (including chunk overhead) before reading.
/// Emitted bytes remain provisional until complete; errors require closure.
pub struct BodyReader {
    state: State,
    chunked: bool,
    line: [u8; CHUNK_LINE_BYTES],
    used: usize,
    cr: bool,
    decoded_left: u64,
    wire_left: u64,
    failure: Option<Error>,
}
impl BodyReader {
    pub fn new(framing: Framing, decoded_limit: u64, wire_limit: u64) -> Result<Self, Error> {
        let state = match framing {
            Framing::Length(n) if n > decoded_limit || n > wire_limit => return Err(Error::Limit),
            Framing::Length(0) => State::Complete,
            Framing::Length(n) => State::Data(n),
            Framing::Chunked => State::Size,
        };
        Ok(Self {
            state,
            chunked: framing == Framing::Chunked,
            line: [0; CHUNK_LINE_BYTES],
            used: 0,
            cr: false,
            decoded_left: decoded_limit,
            wire_left: wire_limit,
            failure: None,
        })
    }
    pub fn feed(&mut self, input: &[u8], output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let mut consumed = 0;
        let mut written = 0;
        for &byte in input.iter().take(QUANTUM) {
            if matches!(self.state, State::Complete)
                || (matches!(self.state, State::Data(_)) && written == output.len())
            {
                break;
            }
            if let Err(error) = self.byte(byte, output, &mut written) {
                self.failure = Some(error);
                return Err(error);
            }
            consumed += 1;
        }
        Ok(Progress {
            consumed,
            written,
            complete: matches!(self.state, State::Complete),
        })
    }
    fn byte(&mut self, byte: u8, output: &mut [u8], written: &mut usize) -> Result<(), Error> {
        self.wire_left = self.wire_left.checked_sub(1).ok_or(Error::Limit)?;
        self.state = match self.state {
            State::Data(left) => {
                self.decoded_left = self.decoded_left.checked_sub(1).ok_or(Error::Limit)?;
                *output.get_mut(*written).ok_or(Error::Limit)? = byte;
                *written += 1;
                match left.checked_sub(1).ok_or(Error::Syntax)? {
                    0 if self.chunked => State::DataCr,
                    0 => State::Complete,
                    n => State::Data(n),
                }
            }
            State::DataCr if byte == b'\r' => State::DataLf,
            State::DataLf if byte == b'\n' => State::Size,
            State::EndCr if byte == b'\r' => State::EndLf,
            State::EndLf if byte == b'\n' => State::Complete,
            State::Size => {
                if self.cr {
                    if byte != b'\n' {
                        return Err(Error::Syntax);
                    }
                    let size = chunk_size(self.line.get(..self.used).ok_or(Error::Limit)?)?;
                    if size > self.decoded_left || size > self.wire_left {
                        return Err(Error::Limit);
                    }
                    self.used = 0;
                    self.cr = false;
                    if size == 0 {
                        State::EndCr
                    } else {
                        State::Data(size)
                    }
                } else {
                    if byte == b'\r' {
                        self.cr = true;
                    } else {
                        if !(byte == b'\t' || (32..=126).contains(&byte)) {
                            return Err(Error::Syntax);
                        }
                        *self.line.get_mut(self.used).ok_or(Error::Limit)? = byte;
                        self.used += 1;
                    }
                    State::Size
                }
            }
            _ => return Err(Error::Syntax),
        };
        Ok(())
    }
    pub fn eof(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.state, State::Complete) {
            self.failure = Some(Error::Incomplete);
            return Err(Error::Incomplete);
        }
        Ok(())
    }
}
fn chunk_size(line: &[u8]) -> Result<u64, Error> {
    let size_end = line
        .iter()
        .position(|b| !b.is_ascii_hexdigit())
        .unwrap_or(line.len());
    if size_end == 0 {
        return Err(Error::Syntax);
    }
    let mut size = 0u64;
    for &byte in line.get(..size_end).ok_or(Error::Syntax)? {
        let digit = char::from(byte).to_digit(16).ok_or(Error::Syntax)?;
        size = size
            .checked_mul(16)
            .and_then(|v| v.checked_add(u64::from(digit)))
            .ok_or(Error::Limit)?;
    }
    let mut rest = line.get(size_end..).ok_or(Error::Syntax)?;
    while !rest.is_empty() {
        rest = trim_ows(rest);
        rest = rest.strip_prefix(b";").ok_or(Error::Syntax)?;
        rest = trim_ows(rest);
        let end = rest.iter().position(|b| !token(*b)).unwrap_or(rest.len());
        if end == 0 {
            return Err(Error::Syntax);
        }
        rest = rest.get(end..).ok_or(Error::Syntax)?;
        let after = trim_ows(rest);
        if let Some(value) = after.strip_prefix(b"=") {
            rest = trim_ows(value);
            if let Some(value) = rest.strip_prefix(b"\"") {
                let mut escaped = false;
                let mut end = None;
                for (i, &b) in value.iter().enumerate() {
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        end = Some(i + 1);
                        break;
                    }
                }
                rest = value
                    .get(end.ok_or(Error::Syntax)?..)
                    .ok_or(Error::Syntax)?;
            } else {
                let end = rest.iter().position(|b| !token(*b)).unwrap_or(rest.len());
                if end == 0 {
                    return Err(Error::Syntax);
                }
                rest = rest.get(end..).ok_or(Error::Syntax)?;
            }
        }
    }
    Ok(size)
}
fn trim_ows(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    bytes.get(end..).unwrap_or_default()
}

#[cfg(test)]
#[path = "http_request_tests.rs"]
mod tests;
