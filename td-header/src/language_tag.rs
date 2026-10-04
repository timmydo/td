//! Passive tag spelling; the caller admits each source read before feeding it.
/// No source, work, clock, registry or metadata authority is owned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tag {
    length: u8,
    subtag: bool,
    invalid: bool,
}
impl Tag {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            length: 0,
            subtag: false,
            invalid: false,
        }
    }
    /// A true return is provisional; a trailing hyphen is not complete.
    pub fn feed(&mut self, byte: u8) -> bool {
        if self.invalid {
            return false;
        }
        if byte == b'-' {
            if self.length == 0 {
                self.invalid = true;
                return false;
            }
            self.length = 0;
            self.subtag = true;
            return true;
        }
        let admitted = if self.subtag {
            byte.is_ascii_alphanumeric()
        } else {
            byte.is_ascii_alphabetic()
        };
        if !admitted || self.length >= 8 {
            self.invalid = true;
            return false;
        }
        self.length += 1;
        true
    }
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.invalid && !self.subtag && self.length == 0
    }
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !self.invalid && self.length != 0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_lengths_and_empty_dangling_or_bad_components() {
        for source in [
            b"a".as_slice(),
            b"abcdefgh",
            b"en-US",
            b"x-a1b2C3d4",
            b"en-12345678-de",
        ] {
            let mut tag = Tag::new();
            assert!(source.iter().copied().all(|b| tag.feed(b)));
            assert!(tag.is_complete());
            assert!(!tag.is_empty());
            assert!(std::mem::size_of_val(&tag) <= 3);
        }
        for source in [
            b"".as_slice(),
            b"-en",
            b"en-",
            b"en--US",
            b"123",
            b"abcdefghi",
            b"en-123456789",
            b"en_US",
            b"en\xff",
        ] {
            let mut tag = Tag::new();
            let admitted = source.iter().copied().all(|b| tag.feed(b));
            assert!(!(admitted && tag.is_complete()), "{source:?}");
            assert_eq!(tag.is_empty(), source.is_empty());
        }
    }
    #[test]
    fn every_octet_has_exact_primary_and_subtag_admission() {
        for byte in 0..=255 {
            let mut primary = Tag::new();
            assert_eq!(primary.feed(byte), byte.is_ascii_alphabetic());
            let mut subtag = Tag::new();
            assert!(subtag.feed(b'e') && subtag.feed(b'n') && subtag.feed(b'-'));
            assert_eq!(subtag.feed(byte), byte.is_ascii_alphanumeric());
        }
    }
    #[test]
    fn rejected_input_never_restores_a_complete_tag() {
        let mut tag = Tag::new();
        assert!(tag.feed(b'e') && tag.feed(b'n'));
        assert!(!tag.feed(b'_'));
        assert!(!tag.feed(b'U'));
        assert!(!tag.is_complete() && !tag.is_empty());
    }
}
