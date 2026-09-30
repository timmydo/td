//! Streaming raw ClientHello SNI checks before the backend discards IP literals.
use crate::TlsError;

#[derive(Clone, Eq, PartialEq)]
struct Name {
    bytes: [u8; 253],
    len: usize,
}
impl Name {
    fn new() -> Self {
        Self {
            bytes: [0; 253],
            len: 0,
        }
    }
    fn text(&self) -> Result<Option<&str>, TlsError> {
        if self.len == 0 {
            return Ok(None);
        }
        let bytes = self.bytes.get(..self.len).ok_or(TlsError::Crypto)?;
        std::str::from_utf8(bytes)
            .map(Some)
            .map_err(|_| TlsError::Protocol)
    }
}

pub(super) struct RawHellos {
    header: [u8; 4],
    header_len: usize,
    remaining: usize,
    hello: Option<Hello>,
    first: Option<Name>,
    hellos: u8,
}
impl RawHellos {
    pub(super) fn new() -> Self {
        Self {
            header: [0; 4],
            header_len: 0,
            remaining: 0,
            hello: None,
            first: None,
            hellos: 0,
        }
    }
    /// Only plaintext handshake fragments enter here. The caller performs record
    /// framing and backpressure first, and feeds native state only after success.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<(), TlsError> {
        for &byte in bytes {
            if self.header_len < 4 {
                *self
                    .header
                    .get_mut(self.header_len)
                    .ok_or(TlsError::Crypto)? = byte;
                self.header_len += 1;
                if self.header_len != 4 {
                    continue;
                }
                let [kind, a, b, c] = self.header;
                self.remaining = usize::from(a) * 65536 + usize::from(b) * 256 + usize::from(c);
                if self.remaining > 65535 {
                    return Err(TlsError::Capacity);
                }
                if kind == 1 {
                    self.hellos = self.hellos.checked_add(1).ok_or(TlsError::Protocol)?;
                    if self.hellos > 2 {
                        return Err(TlsError::Protocol);
                    }
                    self.hello = Some(Hello::new(self.remaining)?);
                } else if self.first.is_none() {
                    return Err(TlsError::Protocol);
                }
                if self.remaining == 0 {
                    self.complete()?;
                }
                continue;
            }
            if let Some(hello) = self.hello.as_mut() {
                hello.feed(byte)?;
            }
            self.remaining = self.remaining.checked_sub(1).ok_or(TlsError::Crypto)?;
            if self.remaining == 0 {
                self.complete()?;
            }
        }
        Ok(())
    }
    fn complete(&mut self) -> Result<(), TlsError> {
        if let Some(hello) = self.hello.take() {
            if hello.stage != Stage::Complete {
                return Err(TlsError::Protocol);
            }
            match &self.first {
                Some(first) if first != &hello.name => return Err(TlsError::Protocol),
                None => self.first = Some(hello.name),
                _ => {}
            }
        }
        self.header_len = 0;
        Ok(())
    }
    pub(super) fn name(&self) -> Result<Option<&str>, TlsError> {
        self.first.as_ref().ok_or(TlsError::Protocol)?.text()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Stage {
    Fixed,
    SessionLen,
    Session,
    CipherLen,
    Cipher,
    CompressionLen,
    Compression,
    ExtensionsLen,
    ExtensionType,
    ExtensionLen,
    SkipExtension,
    SniListLen,
    SniType,
    SniNameLen,
    SniName,
    Complete,
}
struct Hello {
    stage: Stage,
    total: usize,
    offset: usize,
    skip: usize,
    high: Option<u8>,
    extension_type: u16,
    extension_end: usize,
    sni_seen: bool,
    name: Name,
}
impl Hello {
    fn new(total: usize) -> Result<Self, TlsError> {
        if total < 38 {
            return Err(TlsError::Protocol);
        }
        Ok(Self {
            stage: Stage::Fixed,
            total,
            offset: 0,
            skip: 34,
            high: None,
            extension_type: 0,
            extension_end: 0,
            sni_seen: false,
            name: Name::new(),
        })
    }
    fn word(&mut self, byte: u8) -> Option<u16> {
        if let Some(high) = self.high.take() {
            Some(u16::from_be_bytes([high, byte]))
        } else {
            self.high = Some(byte);
            None
        }
    }
    fn bounded_end(&self, count: usize, limit: usize) -> Result<usize, TlsError> {
        self.offset
            .checked_add(count)
            .filter(|end| *end <= limit)
            .ok_or(TlsError::Protocol)
    }
    fn skip_to(&mut self, count: usize, stage: Stage) -> Result<(), TlsError> {
        self.bounded_end(count, self.total)?;
        self.skip = count;
        self.stage = stage;
        Ok(())
    }
    fn extension_done(&mut self) -> Result<(), TlsError> {
        if self.offset != self.extension_end {
            return Err(TlsError::Protocol);
        }
        self.stage = if self.offset == self.total {
            Stage::Complete
        } else {
            Stage::ExtensionType
        };
        Ok(())
    }
    fn feed(&mut self, byte: u8) -> Result<(), TlsError> {
        self.offset = self
            .offset
            .checked_add(1)
            .filter(|n| *n <= self.total)
            .ok_or(TlsError::Protocol)?;
        match self.stage {
            Stage::Fixed
            | Stage::Session
            | Stage::Cipher
            | Stage::Compression
            | Stage::SkipExtension => {
                self.skip = self.skip.checked_sub(1).ok_or(TlsError::Crypto)?;
                if self.skip == 0 {
                    self.stage = match self.stage {
                        Stage::Fixed => Stage::SessionLen,
                        Stage::Session => Stage::CipherLen,
                        Stage::Cipher => Stage::CompressionLen,
                        Stage::Compression if self.offset == self.total => Stage::Complete,
                        Stage::Compression => Stage::ExtensionsLen,
                        Stage::SkipExtension => {
                            self.extension_done()?;
                            return Ok(());
                        }
                        _ => return Err(TlsError::Crypto),
                    };
                }
            }
            Stage::SessionLen => {
                if byte > 32 {
                    return Err(TlsError::Protocol);
                }
                if byte == 0 {
                    self.stage = Stage::CipherLen;
                } else {
                    self.skip_to(usize::from(byte), Stage::Session)?;
                }
            }
            Stage::CipherLen => {
                if let Some(count) = self.word(byte) {
                    if count == 0 || count % 2 != 0 {
                        return Err(TlsError::Protocol);
                    }
                    self.skip_to(usize::from(count), Stage::Cipher)?;
                }
            }
            Stage::CompressionLen => {
                if byte == 0 {
                    return Err(TlsError::Protocol);
                }
                self.skip_to(usize::from(byte), Stage::Compression)?;
            }
            Stage::ExtensionsLen => {
                if let Some(count) = self.word(byte) {
                    if self.bounded_end(usize::from(count), self.total)? != self.total {
                        return Err(TlsError::Protocol);
                    }
                    self.stage = if count == 0 {
                        Stage::Complete
                    } else {
                        Stage::ExtensionType
                    };
                }
            }
            Stage::ExtensionType => {
                if let Some(kind) = self.word(byte) {
                    self.extension_type = kind;
                    self.stage = Stage::ExtensionLen;
                }
            }
            Stage::ExtensionLen => {
                if let Some(count) = self.word(byte) {
                    self.extension_end = self.bounded_end(usize::from(count), self.total)?;
                    if self.extension_type == 0 {
                        if self.sni_seen || count < 6 {
                            return Err(TlsError::Protocol);
                        }
                        self.sni_seen = true;
                        self.stage = Stage::SniListLen;
                    } else if count == 0 {
                        self.extension_done()?;
                    } else {
                        self.skip_to(usize::from(count), Stage::SkipExtension)?;
                    }
                }
            }
            Stage::SniListLen => {
                if let Some(count) = self.word(byte) {
                    if count < 4
                        || self.bounded_end(usize::from(count), self.extension_end)?
                            != self.extension_end
                    {
                        return Err(TlsError::Protocol);
                    }
                    self.stage = Stage::SniType;
                }
            }
            Stage::SniType => {
                if byte != 0 {
                    return Err(TlsError::Protocol);
                }
                self.stage = Stage::SniNameLen;
            }
            Stage::SniNameLen => {
                if let Some(count) = self.word(byte) {
                    let count = usize::from(count);
                    if count == 0
                        || count > self.name.bytes.len()
                        || self.bounded_end(count, self.extension_end)? != self.extension_end
                    {
                        return Err(TlsError::Protocol);
                    }
                    self.stage = Stage::SniName;
                }
            }
            Stage::SniName => {
                *self
                    .name
                    .bytes
                    .get_mut(self.name.len)
                    .ok_or(TlsError::Protocol)? = byte.to_ascii_lowercase();
                self.name.len += 1;
                if self.offset == self.extension_end {
                    let name = self.name.text()?.ok_or(TlsError::Protocol)?;
                    crate::identity::validate_name(name).map_err(|_| TlsError::Protocol)?;
                    self.extension_done()?;
                }
            }
            Stage::Complete => return Err(TlsError::Protocol),
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn extension(kind: u16, value: &[u8]) -> Vec<u8> {
        [
            kind.to_be_bytes().as_slice(),
            u16::try_from(value.len()).unwrap().to_be_bytes().as_slice(),
            value,
        ]
        .concat()
    }
    fn sni(name: &[u8]) -> Vec<u8> {
        let item = [
            &[0],
            u16::try_from(name.len()).unwrap().to_be_bytes().as_slice(),
            name,
        ]
        .concat();
        extension(
            0,
            &[
                u16::try_from(item.len()).unwrap().to_be_bytes().as_slice(),
                &item,
            ]
            .concat(),
        )
    }
    fn hello(extensions: Option<&[u8]>) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend_from_slice(&[1; 32]);
        body.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
        if let Some(extensions) = extensions {
            body.extend_from_slice(&u16::try_from(extensions.len()).unwrap().to_be_bytes());
            body.extend_from_slice(extensions);
        }
        let mut message = vec![1];
        message.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
        message.extend_from_slice(&body);
        message
    }
    #[test]
    fn raw_hello_fragmentation_names_and_retry() {
        let bytes = hello(Some(
            &[
                extension(1234, &[0; 79]),
                sni(b"Mail.Example.test"),
                extension(1235, &[]),
            ]
            .concat(),
        ));
        for chunk in 1..=bytes.len() {
            let mut scan = RawHellos::new();
            for part in bytes.chunks(chunk) {
                scan.feed(part).unwrap();
            }
            assert_eq!(scan.name().unwrap(), Some("mail.example.test"));
            for part in hello(Some(&sni(b"mail.EXAMPLE.test"))).chunks(chunk) {
                scan.feed(part).unwrap();
            }
            assert_eq!(scan.name().unwrap(), Some("mail.example.test"));
            assert_eq!(scan.feed(&bytes), Err(TlsError::Protocol));
        }
        for extensions in [None, Some(&[][..])] {
            let mut scan = RawHellos::new();
            let message = hello(extensions);
            for byte in &message {
                scan.feed(&[*byte]).unwrap();
            }
            assert_eq!(scan.name().unwrap(), None);
            scan.feed(&message).unwrap();
        }
        for (first, retry) in [
            (Some(b"a.test".as_slice()), Some(b"b.test".as_slice())),
            (None, Some(b"a.test".as_slice())),
            (Some(b"a.test".as_slice()), None),
        ] {
            let mut scan = RawHellos::new();
            scan.feed(&hello(first.map(sni).as_deref())).unwrap();
            assert_eq!(
                scan.feed(&hello(retry.map(sni).as_deref())),
                Err(TlsError::Protocol)
            );
        }
    }
    #[test]
    fn raw_hello_refuses_malformed_sni_before_completion() {
        for name in [
            b"127.0.0.1".as_slice(),
            b"::1",
            b"[::1]",
            b"a.test.",
            b"a_b.test",
            b"",
            b"\xff.test",
            &[b'a'; 254],
        ] {
            let bytes = hello(Some(&sni(name)));
            for chunk in [1, 7, 16384] {
                let mut scan = RawHellos::new();
                assert!(bytes
                    .chunks(chunk)
                    .try_for_each(|part| scan.feed(part))
                    .is_err());
                assert!(scan.name().is_err());
            }
        }
        let mut cases = vec![hello(Some(&[sni(b"a.test"), sni(b"a.test")].concat()))];
        let valid = hello(Some(&sni(b"a.test")));
        // All offsets here include the four-byte handshake header.
        for (offset, value) in [
            (38, 33),
            (39, 255),
            (40, 1),
            (43, 0),
            (45, 255),
            (49, 255),
            (50, 255),
            (51, 255),
            (52, 0),
            (53, 1),
            (54, 255),
            (55, 0),
        ] {
            let mut invalid = valid.clone();
            invalid[offset] = value;
            cases.push(invalid);
        }
        for bytes in cases {
            let mut scan = RawHellos::new();
            assert!(scan.feed(&bytes).is_err(), "accepted mutation: {bytes:?}");
        }
        for end in 0..valid.len() {
            let mut scan = RawHellos::new();
            scan.feed(&valid[..end]).unwrap();
            assert!(scan.name().is_err());
        }
    }
    #[test]
    fn raw_hello_fixed_state_skips_large_extensions_and_bounds_messages() {
        assert!(std::mem::size_of::<RawHellos>() <= 1024);
        let bytes = hello(Some(
            &[extension(1234, &vec![42; 60000]), sni(b"a.test")].concat(),
        ));
        let mut scan = RawHellos::new();
        for part in bytes.chunks(37) {
            scan.feed(part).unwrap();
        }
        assert_eq!(scan.name().unwrap(), Some("a.test"));
        scan.feed(&[16, 0, 0, 3, 1, 2, 3]).unwrap();
        assert_eq!(scan.name().unwrap(), Some("a.test"));
        assert_eq!(
            RawHellos::new().feed(&[1, 1, 0, 0]),
            Err(TlsError::Capacity)
        );
        assert_eq!(
            RawHellos::new().feed(&[16, 0, 0, 0]),
            Err(TlsError::Protocol)
        );
    }
}
