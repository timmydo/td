//! Whole-record framing before native intake; this does not authenticate bytes.
use crate::TlsError;
pub(super) const PLAINTEXT_LIMIT: usize = 16_384;
pub(super) const WIRE_CAPACITY: usize = 18_437;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Protection {
    Plain,
    Tls12,
    Tls13,
}
pub(super) struct Record<'a> {
    pub(super) kind: u8,
    pub(super) body: &'a [u8],
}
impl<'a> Record<'a> {
    pub(super) fn parse(
        wire: &'a [u8],
        protection: Protection,
        handshaking: bool,
    ) -> Result<Self, TlsError> {
        let header = wire.get(..5).ok_or(TlsError::Protocol)?;
        let kind = *header.first().ok_or(TlsError::Protocol)?;
        if !matches!(kind, 20..=23) || (handshaking && kind == 21) {
            return Err(TlsError::Protocol);
        }
        let length = header.get(3..5).ok_or(TlsError::Protocol)?;
        let length = usize::from(u16::from_be_bytes(
            length.try_into().map_err(|_| TlsError::Protocol)?,
        ));
        if length >= WIRE_CAPACITY - 5 {
            return Err(TlsError::Protocol);
        }
        let body = wire.get(5..).ok_or(TlsError::Protocol)?;
        if body.len() != length {
            return Err(TlsError::Protocol);
        }
        let limit = match (protection, kind) {
            (Protection::Tls12, _) => WIRE_CAPACITY - 6,
            (Protection::Tls13, 23) => 16_640,
            _ => PLAINTEXT_LIMIT,
        };
        if length > limit {
            return Err(TlsError::Protocol);
        }
        Ok(Self { kind, body })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn wire(kind: u8, length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length + 5];
        bytes[..3].copy_from_slice(&[kind, 3, 3]);
        bytes[3..5].copy_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
        bytes
    }
    #[test]
    fn record_bounds_track_protection_not_finished() {
        for (protection, kind, limit) in [
            (Protection::Plain, 22, 16_384),
            (Protection::Tls12, 22, 18_431),
            (Protection::Tls12, 23, 18_431),
            (Protection::Tls13, 23, 16_640),
            (Protection::Tls13, 22, 16_384),
            (Protection::Tls13, 20, 16_384),
        ] {
            for handshaking in [false, true] {
                assert!(Record::parse(&wire(kind, limit), protection, handshaking).is_ok());
                assert!(matches!(
                    Record::parse(&wire(kind, limit + 1), protection, handshaking),
                    Err(TlsError::Protocol)
                ));
            }
        }
        assert!(Record::parse(&wire(21, 32), Protection::Tls12, false).is_ok());
        assert!(matches!(
            Record::parse(&wire(21, 32), Protection::Tls12, true),
            Err(TlsError::Protocol)
        ));
        for kind in [0, 19, 24, 255] {
            assert!(matches!(
                Record::parse(&wire(kind, 32), Protection::Tls12, false),
                Err(TlsError::Protocol)
            ));
        }
    }
    #[test]
    fn record_requires_one_complete_exact_frame() {
        let mut bytes = wire(23, 16_640);
        for length in 0..bytes.len() {
            assert!(matches!(
                Record::parse(&bytes[..length], Protection::Tls13, false),
                Err(TlsError::Protocol)
            ));
        }
        assert!(Record::parse(&bytes, Protection::Tls13, false).is_ok());
        bytes.push(0);
        assert!(matches!(
            Record::parse(&bytes, Protection::Tls13, false),
            Err(TlsError::Protocol)
        ));
        assert!(matches!(
            Record::parse(
                &[wire(22, 1), wire(22, 1)].concat(),
                Protection::Plain,
                true
            ),
            Err(TlsError::Protocol)
        ));
    }
}
