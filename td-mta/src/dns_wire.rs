//! Bounded DNS names and address questions. No lookup or response authority.

pub const MAX_PACKET_BYTES: usize = 65_535;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_POINTERS: usize = 32;
pub const MAX_QUERY_BYTES: usize = 12 + MAX_NAME_BYTES + 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
}

/// Canonical label-length wire form, including the root octet. Only ASCII
/// letters fold; arbitrary label octets and label boundaries remain distinct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Name {
    bytes: [u8; MAX_NAME_BYTES],
    used: u16,
}

impl Name {
    /// Configured endpoint syntax, not a presentation-format DNS name parser.
    pub fn from_host(host: &str) -> Result<Self, Error> {
        crate::config::values::dns_name(host).map_err(|_| Error::Invalid)?;
        let mut name = Self::empty();
        for label in host.split('.') {
            name.push(u8::try_from(label.len()).map_err(|_| Error::Invalid)?)?;
            for byte in label.bytes() {
                name.push(byte.to_ascii_lowercase())?;
            }
        }
        name.push(0)?;
        Ok(name)
    }

    /// Decode one name and return bytes consumed at `start`, excluding pointer
    /// targets. Compression pointers must point backwards; at most 32 are
    /// followed. This checks structure, not whether targets were prior fields.
    pub fn read(packet: &[u8], start: usize) -> Result<(Self, usize), Error> {
        if packet.len() > MAX_PACKET_BYTES {
            return Err(Error::Capacity);
        }
        let mut name = Self::empty();
        let mut position = start;
        let mut consumed = 0usize;
        let mut indirect = false;
        let mut pointers = 0usize;
        loop {
            let tag = *packet.get(position).ok_or(Error::Invalid)?;
            let next = position.checked_add(1).ok_or(Error::Invalid)?;
            if tag & 0xc0 == 0xc0 {
                let low = *packet.get(next).ok_or(Error::Invalid)?;
                let target = usize::from(u16::from(tag & 0x3f) << 8 | u16::from(low));
                if target >= position {
                    return Err(Error::Invalid);
                }
                pointers += 1;
                if pointers > MAX_POINTERS {
                    return Err(Error::Capacity);
                }
                if !indirect {
                    consumed += 2;
                }
                indirect = true;
                position = target;
                continue;
            }
            if tag & 0xc0 != 0 {
                return Err(Error::Invalid);
            }
            name.push(tag)?;
            let end = next.checked_add(usize::from(tag)).ok_or(Error::Invalid)?;
            for byte in packet.get(next..end).ok_or(Error::Invalid)? {
                name.push(byte.to_ascii_lowercase())?;
            }
            if !indirect {
                consumed += 1 + usize::from(tag);
            }
            if tag == 0 {
                return Ok((name, consumed));
            }
            position = end;
        }
    }

    pub fn wire(&self) -> &[u8] {
        // The private used counter advances only after a successful cell write.
        self.bytes.get(..usize::from(self.used)).unwrap_or_default()
    }

    fn empty() -> Self {
        Self {
            bytes: [0; MAX_NAME_BYTES],
            used: 0,
        }
    }

    fn push(&mut self, byte: u8) -> Result<(), Error> {
        *self
            .bytes
            .get_mut(usize::from(self.used))
            .ok_or(Error::Capacity)? = byte;
        self.used += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressType {
    A,
    Aaaa,
}
impl AddressType {
    pub const fn code(self) -> u16 {
        match self {
            Self::A => 1,
            Self::Aaaa => 28,
        }
    }
}

/// One recursive IN A/AAAA question. The network owner supplies an unpredictable
/// ID and matches the peer, ID and complete question before consuming a reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Question {
    name: Name,
    kind: AddressType,
    id: u16,
}
impl Question {
    pub fn new(host: &str, kind: AddressType, id: u16) -> Result<Self, Error> {
        Ok(Self {
            name: Name::from_host(host)?,
            kind,
            id,
        })
    }

    pub fn name(&self) -> &Name {
        &self.name
    }
    pub const fn kind(&self) -> AddressType {
        self.kind
    }
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// Only the returned prefix is a query; bytes beyond it are unchanged.
    /// Insufficient capacity writes nothing. No TCP length prefix or EDNS.
    pub fn write<'a>(&self, output: &'a mut [u8]) -> Result<&'a [u8], Error> {
        let end = 12 + self.name.wire().len();
        let length = end + 4;
        let output = output.get_mut(..length).ok_or(Error::Capacity)?;
        output.fill(0);
        output
            .get_mut(..2)
            .ok_or(Error::Capacity)?
            .copy_from_slice(&self.id.to_be_bytes());
        *output.get_mut(2).ok_or(Error::Capacity)? = 1; // RD
        *output.get_mut(5).ok_or(Error::Capacity)? = 1; // QDCOUNT
        output
            .get_mut(12..end)
            .ok_or(Error::Capacity)?
            .copy_from_slice(self.name.wire());
        output
            .get_mut(end..end + 2)
            .ok_or(Error::Capacity)?
            .copy_from_slice(&self.kind.code().to_be_bytes());
        *output.get_mut(end + 3).ok_or(Error::Capacity)? = 1; // IN
        Ok(output)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_exact_bounded_address_questions() {
        for (kind, code) in [(AddressType::A, 1), (AddressType::Aaaa, 28)] {
            let query = Question::new("MX.Example", kind, 0x1234).unwrap();
            assert_eq!(query.id(), 0x1234);
            assert_eq!(query.kind(), kind);
            let expected = [
                b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x02mx\x07example\x00\x00"
                    .as_slice(),
                &[code, 0, 1],
            ]
            .concat();
            let mut cell = [0xa5; MAX_QUERY_BYTES];
            assert_eq!(query.write(&mut cell).unwrap(), expected);
            assert!(cell[expected.len()..].iter().all(|b| *b == 0xa5));
            for size in 0..expected.len() {
                let mut short = vec![0xa5; size];
                assert_eq!(query.write(&mut short), Err(Error::Capacity));
                assert!(short.iter().all(|b| *b == 0xa5));
            }
            let (name, consumed) = Name::read(&expected, 12).unwrap();
            assert_eq!(&name, query.name());
            assert_eq!(consumed, 12);
        }
        for host in ["", "a.", "a..b", "é.test", "127.0.0.1", "a\0b"] {
            assert_eq!(Question::new(host, AddressType::A, 1), Err(Error::Invalid));
        }
    }

    #[test]
    fn names_fold_only_ascii_and_preserve_label_boundaries() {
        let (name, used) = Name::read(b"\x03A.\xff\x01B\x00tail", 0).unwrap();
        assert_eq!(used, 7);
        assert_eq!(name.wire(), b"\x03a.\xff\x01b\x00");
        assert_ne!(name, Name::read(b"\x01a\x03.\xffb\x00", 0).unwrap().0);
        assert_eq!(Name::read(&[0, 9], 0).unwrap().1, 1);
        assert!(std::mem::size_of::<Name>() <= 258);
        assert!(std::mem::size_of::<Question>() <= 264);
    }

    #[test]
    fn compressed_suffixes_count_only_original_bytes() {
        let packet = b"\x07EXAMPLE\x04test\x00\x02MX\xc0\x00\xc0\x0e";
        let expected = Name::from_host("mx.example.test").unwrap();
        assert_eq!(Name::read(packet, 14).unwrap(), (expected.clone(), 5));
        assert_eq!(Name::read(packet, 19).unwrap(), (expected, 2));
        for end in 0..14 {
            assert!(Name::read(&packet[..end], 0).is_err());
        }
        assert!(Name::read(&packet[..18], 14).is_err());
        assert!(Name::read(packet, packet.len()).is_err());
        assert!(Name::read(packet, usize::MAX).is_err());

        let mut distant = vec![0; 16_382];
        distant.extend_from_slice(b"\x01A\x00\xff\xfe");
        assert_eq!(
            Name::read(&distant, 16_385).unwrap(),
            (Name::from_host("a").unwrap(), 2)
        );
    }

    #[test]
    fn malformed_and_excessive_compression_is_bounded() {
        for packet in [
            b"\xc0\x00".as_slice(),
            b"\xc0\x02\x00",
            b"\x40",
            b"\x80",
            b"\xc0",
            b"\x01a",
            b"\x01a\xc0\x00",
        ] {
            assert!(Name::read(packet, 0).is_err(), "{packet:?}");
        }
        let mut chain = vec![0];
        let mut previous = 0usize;
        for count in 1..=MAX_POINTERS + 1 {
            let start = chain.len();
            chain.extend_from_slice(&(0xc000 | previous as u16).to_be_bytes());
            let result = Name::read(&chain, start);
            if count <= MAX_POINTERS {
                assert_eq!(result.unwrap().1, 2);
            } else {
                assert_eq!(result, Err(Error::Capacity));
            }
            previous = start;
        }
        assert_eq!(
            Name::read(&vec![0; MAX_PACKET_BYTES + 1], 0),
            Err(Error::Capacity)
        );
    }

    #[test]
    fn expanded_name_limit_includes_root_and_compressed_prefix() {
        let mut packet = Vec::new();
        for length in [63usize, 63, 63, 61] {
            packet.push(length as u8);
            packet.extend(std::iter::repeat_n(b'A', length));
        }
        packet.push(0);
        assert_eq!(packet.len(), MAX_NAME_BYTES);
        assert_eq!(
            Name::read(&packet, 0).unwrap().0.wire().len(),
            MAX_NAME_BYTES
        );
        let start = packet.len();
        packet.extend_from_slice(b"\x01x\xc0\x00");
        assert_eq!(Name::read(&packet, start), Err(Error::Capacity));
        packet.truncate(MAX_NAME_BYTES - 1);
        packet.extend_from_slice(b"\x01x\x00");
        assert_eq!(Name::read(&packet, 0), Err(Error::Capacity));
    }
}
