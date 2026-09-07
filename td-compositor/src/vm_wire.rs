//! td-owned VM messages. The carrier supplies isolation; request IDs only
//! correlate replies. No guest message can request a host operation.
use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)] // Used by the host runner that includes this codec.
pub const PORT: &str = "org.td.vm.1";
#[allow(dead_code)] // Used by the guest worker that includes this codec.
pub const FEED_FILE: &str = "/run/td-compositor/1000/vm-feed";
pub const MAX_TEXT: usize = 65536;
pub const MAX_LINE: usize = MAX_TEXT * 2 + 128;
pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const SNAPSHOT: &str = "snapshot";
pub const PUT: &str = "put";
pub const GET: &str = "get";
pub const FEED: &str = "feed";
pub const OK: &str = "ok";
pub const ERROR: &str = "error";

#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    pub verb: String,
    pub revision: u64,
    pub data: Vec<u8>,
}

impl Message {
    pub fn new(id: u64, verb: &str, revision: u64, data: Vec<u8>) -> Self {
        Self {
            id,
            verb: verb.into(),
            revision,
            data,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.data.len() > MAX_TEXT || !valid_verb(&self.verb) || self.id == 0 {
            return Err("invalid VM message".into());
        }
        let mut line = format!(
            "TDVM1 {} {} {} {} ",
            self.id,
            self.verb,
            self.revision,
            self.data.len()
        );
        const HEX: &[u8] = b"0123456789abcdef";
        for byte in &self.data {
            for nibble in [byte >> 4, byte & 15] {
                line.push(char::from(
                    *HEX.get(usize::from(nibble)).ok_or("hex digit")?,
                ));
            }
        }
        line.push('\n');
        Ok(line.into_bytes())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_LINE {
            return Err("VM message exceeds limit".into());
        }
        let line = std::str::from_utf8(bytes).map_err(|_| "non-ASCII VM header")?;
        let mut fields = line
            .strip_suffix('\n')
            .ok_or("truncated VM message")?
            .split(' ');
        if fields.next() != Some("TDVM1") {
            return Err("unsupported VM protocol".into());
        }
        let id = decimal(fields.next())?;
        let verb = fields.next().ok_or("missing VM verb")?;
        let revision = decimal(fields.next())?;
        let length = decimal(fields.next())?;
        let hex = fields.next().ok_or("missing VM payload")?;
        if id == 0
            || !valid_verb(verb)
            || fields.next().is_some()
            || hex.len() > MAX_TEXT * 2
            || length > MAX_TEXT as u64
            || hex.len() as u64 != length * 2
        {
            return Err("invalid VM message fields".into());
        }
        let mut data = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().as_chunks::<2>().0 {
            let high = unhex(*pair.first().ok_or("hex pair")?)?;
            let low = unhex(*pair.get(1).ok_or("hex pair")?)?;
            data.push(high * 16 + low);
        }
        Ok(Self::new(id, verb, revision, data))
    }
}

fn valid_verb(verb: &str) -> bool {
    matches!(verb, SNAPSHOT | PUT | GET | FEED | OK | ERROR)
}

fn decimal(value: Option<&str>) -> Result<u64, String> {
    let value = value.ok_or("missing VM number")?;
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err("invalid VM number".into());
    }
    value.parse().map_err(|_| "VM number overflow".into())
}

fn unhex(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("invalid VM hex payload".into()),
    }
}

pub fn text(bytes: &[u8]) -> Result<&str, String> {
    if bytes.len() > MAX_TEXT {
        return Err("clipboard exceeds 64 KiB".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "clipboard is not UTF-8")?;
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("clipboard contains terminal controls".into());
    }
    Ok(text)
}

/// The initial feed route is deliberately the host alias on QEMU user-net.
/// No guest-supplied URL or host fetch command crosses this protocol.
pub fn feed(bytes: &[u8]) -> Result<&str, String> {
    let value = std::str::from_utf8(bytes).map_err(|_| "invalid feed endpoint")?;
    if value.is_empty() {
        return Ok(value);
    }
    let port = value
        .strip_prefix("http://10.0.2.2:")
        .ok_or("feed must be http://10.0.2.2:PORT")?;
    let port = decimal(Some(port))?;
    if !(1..=65535).contains(&port) {
        return Err("invalid feed port".into());
    }
    Ok(value)
}

/// New carriers begin with a newline to separate abandoned bytes from new frames.
/// An oversized or expired frame is discarded through its next delimiter.
#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
    started: Option<Instant>,
    discard: bool,
}

impl Decoder {
    pub fn push(&mut self, byte: u8, now: Instant) -> Option<Result<Message, String>> {
        if self
            .started
            .is_some_and(|start| now.duration_since(start) >= TIMEOUT)
        {
            self.bytes.clear();
            self.discard = true;
        }
        if byte == b'\n' {
            self.started = None;
            if self.discard {
                self.discard = false;
                self.bytes.clear();
                return Some(Err("VM frame exceeded its size or deadline".into()));
            }
            if self.bytes.is_empty() {
                return None;
            }
            self.bytes.push(byte);
            let result = Message::decode(&self.bytes);
            self.bytes.clear();
            return Some(result);
        }
        self.started.get_or_insert(now);
        if !self.discard {
            if self.bytes.len() >= MAX_LINE - 1 {
                self.bytes.clear();
                self.discard = true;
            } else {
                self.bytes.push(byte);
            }
        }
        None
    }
}

/// Callers provide nonblocking descriptors and one absolute deadline across
/// all reads/writes in a conversation.
pub fn write_all(io: &mut impl Write, mut bytes: &[u8], deadline: Instant) -> Result<(), String> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err("VM bridge write timed out".into());
        }
        match io.write(bytes) {
            Ok(0) => return Err("VM bridge closed during write".into()),
            Ok(n) => bytes = bytes.get(n..).ok_or("VM write length")?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(format!("VM bridge write: {e}")),
        }
    }
    Ok(())
}

// One outstanding conversation only: bytes after its reply are not queued.
// Unsolicited/asynchronous guest messages require a persistent decoder instead.
#[allow(dead_code)] // The guest streams requests through Decoder directly.
pub fn receive(io: &mut impl Read, id: Option<u64>, deadline: Instant) -> Result<Message, String> {
    let mut decoder = Decoder::default();
    let mut buffer = [0; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err("VM bridge unavailable or timed out".into());
        }
        match io.read(&mut buffer) {
            Ok(0) => return Err("VM bridge disconnected".into()),
            Ok(n) => {
                for byte in buffer.get(..n).ok_or("VM read length")? {
                    if let Some(message) = decoder.push(*byte, Instant::now()) {
                        let message = message?;
                        if id.is_none_or(|id| message.id == id) {
                            return Ok(message);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(format!("VM bridge read: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;

    #[test]
    fn framing_is_bounded_and_recovers_after_abandonment() {
        let message = Message::new(7, PUT, 3, "hello\n世界\t".as_bytes().to_vec());
        let encoded = message.encode().unwrap();
        assert_eq!(Message::decode(&encoded).unwrap(), message);
        let mut decoder = Decoder::default();
        let now = Instant::now();
        for _ in 0..MAX_LINE + 12 {
            assert!(decoder.push(b'x', now).is_none());
        }
        assert!(decoder.bytes.len() <= MAX_LINE);
        assert!(decoder.push(b'\n', now).unwrap().is_err());
        let mut result = None;
        for byte in encoded {
            result = decoder.push(byte, now);
        }
        assert_eq!(result.unwrap().unwrap(), message);
        decoder.push(b'T', now);
        assert!(decoder.push(b'\n', now + TIMEOUT).unwrap().is_err());
    }

    #[test]
    fn a_new_carrier_delimiter_cannot_commit_a_truncated_payload() {
        let mut frame = Message::new(9, PUT, 7, b"abcdef".to_vec())
            .encode()
            .unwrap();
        frame.truncate(frame.len() - 7);
        frame.push(b'\n');
        assert!(Message::decode(&frame).is_err());
    }

    #[test]
    fn rejects_versions_overflow_controls_and_endpoint_authority() {
        for line in [
            "TDVM2 1 get 0 0 \n",
            "TDVM1 0 get 0 0 \n",
            "TDVM1 01 get 0 0 \n",
            "TDVM1 1 exec 0 0 \n",
            "TDVM1 1 get 0 1 a\n",
            "TDVM1 1 get 0 1 gg\n",
            "TDVM1 1 get 18446744073709551616 0 \n",
            "TDVM1 1 get 0 ",
        ] {
            assert!(Message::decode(line.as_bytes()).is_err(), "{line}");
        }
        assert!(text(b"\x1b[201~").is_err());
        assert!(text(&[255]).is_err());
        assert!(text(&vec![b'a'; MAX_TEXT + 1]).is_err());
        assert!(feed(b"http://10.0.2.2:1234").is_ok());
        for endpoint in [
            "http://127.0.0.1:1234",
            "http://10.0.2.2:0",
            "http://10.0.2.2:123/a",
            "http://10.0.2.2:123?x",
            "http://10.0.2.2:123\n",
            "http://10.0.2.2:65536",
        ] {
            assert!(feed(endpoint.as_bytes()).is_err());
        }
    }
}
