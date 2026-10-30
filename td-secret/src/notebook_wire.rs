//! Bounded private notebook frames. The launcher, not this codec, admits peers.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(crate) const LIMIT: usize = 4 * 1024 * 1024 + 16384;
pub(crate) const IDLE: Duration = Duration::from_secs(30);
const FRAME_TIME: Duration = Duration::from_secs(5);

pub(crate) struct Bytes(pub Vec<u8>);
impl Bytes {
    pub fn push(&mut self, byte: u8) -> Result<(), String> {
        self.extend(&[byte])
    }

    pub fn extend(&mut self, bytes: &[u8]) -> Result<(), String> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= LIMIT)
            .ok_or("notebook output exceeds its bound")?;
        if length > self.0.capacity() {
            let capacity = length
                .checked_next_power_of_two()
                .unwrap_or(LIMIT)
                .min(LIMIT);
            let mut replacement = Vec::with_capacity(capacity);
            replacement.extend_from_slice(&self.0);
            self.0.fill(0);
            std::hint::black_box(&mut self.0);
            self.0 = replacement;
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}

impl Drop for Bytes {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

pub(crate) struct Wire(UnixStream);
impl Wire {
    pub fn new(stream: UnixStream) -> Result<Self, String> {
        stream
            .set_nonblocking(false)
            .map_err(|_| "configure notebook endpoint")?;
        Ok(Self(stream))
    }

    pub fn receive(&mut self, timeout: Duration) -> Result<Bytes, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or("notebook deadline overflow")?;
        let mut header = [0; 4];
        self.read(&mut header, deadline)?;
        let count = u32::from_be_bytes(header) as usize;
        if count == 0 || count > LIMIT {
            return Err("invalid notebook frame size".into());
        }
        let mut bytes = Bytes(vec![0; count]);
        let deadline = Instant::now()
            .checked_add(FRAME_TIME)
            .ok_or("notebook deadline overflow")?;
        self.read(&mut bytes.0, deadline)?;
        Ok(bytes)
    }

    fn read(&mut self, mut bytes: &mut [u8], deadline: Instant) -> Result<(), String> {
        while !bytes.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|v| !v.is_zero())
                .ok_or("notebook frame expired")?;
            self.0
                .set_read_timeout(Some(remaining))
                .map_err(|_| "set notebook read bound")?;
            let count = match self.0.read(bytes) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result.map_err(|_| "read notebook frame")?,
            };
            if count == 0 {
                return Err("notebook authority disconnected".into());
            }
            bytes = bytes
                .get_mut(count..)
                .ok_or("invalid notebook read count")?;
        }
        Ok(())
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > LIMIT {
            return Err("invalid notebook reply size".into());
        }
        self.0
            .set_write_timeout(Some(FRAME_TIME))
            .map_err(|_| "set notebook write bound")?;
        let header = u32::try_from(bytes.len())
            .map_err(|_| "notebook reply overflow")?
            .to_be_bytes();
        let deadline = Instant::now()
            .checked_add(FRAME_TIME)
            .ok_or("notebook deadline overflow")?;
        for mut part in [header.as_slice(), bytes] {
            while !part.is_empty() {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|v| !v.is_zero())
                    .ok_or("notebook reply expired")?;
                self.0
                    .set_write_timeout(Some(remaining))
                    .map_err(|_| "set notebook write bound")?;
                let count = match self.0.write(part) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    result => result.map_err(|_| "write notebook frame")?,
                };
                if count == 0 {
                    return Err("notebook authority stopped reading".into());
                }
                part = part.get(count..).ok_or("invalid notebook write count")?;
            }
        }
        Ok(())
    }
}

pub(crate) struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
    pub fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let (value, rest) = self
            .0
            .split_at_checked(count)
            .ok_or("truncated notebook message")?;
        self.0 = rest;
        Ok(value)
    }
    pub fn byte(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "missing notebook tag".into())
    }
    pub fn boolean(&mut self) -> Result<bool, String> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("invalid notebook boolean".into()),
        }
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "invalid notebook field".into())
    }
    pub fn number(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    pub fn field(&mut self, limit: usize) -> Result<&'a [u8], String> {
        let count = u32::from_be_bytes(self.array()?) as usize;
        if count > limit {
            return Err("oversized notebook field".into());
        }
        self.take(count)
    }
    pub fn text(&mut self, limit: usize) -> Result<&'a str, String> {
        std::str::from_utf8(self.field(limit)?).map_err(|_| "invalid notebook text".into())
    }
    pub fn finish(self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing notebook data".into())
        }
    }
}

pub(crate) fn field(out: &mut Bytes, bytes: &[u8]) -> Result<(), String> {
    let length = u32::try_from(bytes.len()).map_err(|_| "notebook field overflow")?;
    if out.0.len().saturating_add(4).saturating_add(bytes.len()) > LIMIT {
        return Err("notebook output exceeds its bound".into());
    }
    out.extend(&length.to_be_bytes())?;
    out.extend(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn growing_plaintext_keeps_content_and_refuses_the_bound() {
        let mut bytes = Bytes(vec![42]);
        for count in [10, 100, 1000, 10000] {
            bytes.extend(&vec![17; count]).unwrap();
        }
        assert_eq!(bytes.0.len(), 11111);
        assert_eq!(bytes.0[0], 42);
        assert!(bytes.0[1..].iter().all(|byte| *byte == 17));
        assert!(bytes.extend(&vec![0; LIMIT]).is_err());
        assert_eq!(bytes.0.len(), 11111);
        // Source confinement checks the property safe Rust cannot observe
        // after deallocation: retired allocations are cleared before replace.
        let source = include_str!("notebook_wire.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(source.contains("self.0.fill(0);\n            std::hint::black_box(&mut self.0);\n            self.0 = replacement;"));
        for source in [
            include_str!("portable_service.rs"),
            include_str!("portable_service_tokens.rs"),
        ] {
            let source = source.split("#[cfg(test)]").next().unwrap();
            assert!(!source.contains("out.0.extend"));
            assert!(!source.contains("out.0.push"));
        }
    }

    #[test]
    fn bounded_fields_reject_truncation_overflow_boolean_and_trailing_data() {
        let mut bytes = Bytes(vec![1]);
        field(&mut bytes, b"secret").unwrap();
        for n in 0..bytes.0.len() {
            let mut reader = Reader::new(&bytes.0[..n]);
            assert!(reader.boolean().and_then(|_| reader.field(6)).is_err());
        }
        let mut reader = Reader::new(&bytes.0);
        assert!(reader.boolean().unwrap());
        assert!(reader.field(5).is_err());
        assert!(Reader::new(&[2]).boolean().is_err());
        assert!(Reader::new(&[0]).finish().is_err());
    }
    #[test]
    fn framed_disconnect_and_oversized_headers_are_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&u32::MAX.to_be_bytes()).unwrap();
        assert!(Wire::new(b).unwrap().receive(IDLE).is_err());
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0, 0, 3, 1]).unwrap();
        drop(a);
        assert!(Wire::new(b).unwrap().receive(IDLE).is_err());
    }
}
