//! Passive validation of already admitted logical MIME parameter octets.
//! The caller owns source admission, complete-value validation and publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Boundary,
    Token,
}
/// Fixed pure state; a valid prefix is never complete-value authority.
#[derive(Clone, Copy, Debug)]
pub struct Validator {
    kind: Kind,
    count: u8,
    space: bool,
    invalid: bool,
}
impl Validator {
    pub const fn new(kind: Kind) -> Self {
        Self {
            kind,
            count: 0,
            space: false,
            invalid: false,
        }
    }
    /// One bounded classification; admission of this octet is external.
    pub fn feed(&mut self, byte: u8) {
        match self.kind {
            Kind::Boundary => {
                self.invalid |= self.count >= 70 || !boundary_octet(byte);
                self.count = self.count.saturating_add(1).min(71);
                self.space = byte == b' ';
            }
            Kind::Token => {
                self.invalid |= !crate::mime_token_octet(byte);
                self.count = 1;
            }
        }
    }
    /// Evaluate only after complete selected data has been consumed.
    pub const fn is_valid(&self) -> bool {
        !self.invalid && self.count != 0 && !self.space
    }
    /// Irrecoverable alphabet/length fault; trailing space is deferred to EOF.
    pub const fn is_invalid(&self) -> bool {
        self.invalid
    }
}
fn boundary_octet(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b' ' | b'\''
                | b'('
                | b')'
                | b'+'
                | b'_'
                | b','
                | b'-'
                | b'.'
                | b'/'
                | b':'
                | b'='
                | b'?'
        )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhaustive_alphabet_and_length_edges() {
        for byte in 0..=u8::MAX {
            let mut boundary = Validator::new(Kind::Boundary);
            boundary.feed(byte);
            let allowed = b"'()+_,-./:=?".contains(&byte) || byte.is_ascii_alphanumeric();
            assert_eq!(boundary.is_valid(), allowed);
            let mut token = Validator::new(Kind::Token);
            token.feed(byte);
            let token_allowed =
                (33..=126).contains(&byte) && !b"()<>@,;:\"/[]?=".contains(&byte) && byte != b'\\';
            assert_eq!(token.is_valid(), token_allowed);
        }
        for length in 0..=256 {
            let mut boundary = Validator::new(Kind::Boundary);
            for _ in 0..length {
                boundary.feed(b'a');
            }
            assert_eq!(boundary.is_valid(), (1..=70).contains(&length));
        }
        let mut boundary = Validator::new(Kind::Boundary);
        boundary.feed(b' ');
        assert!(!boundary.is_valid());
        boundary.feed(b'x');
        assert!(boundary.is_valid());
        boundary.feed(b' ');
        assert!(!boundary.is_valid());
        boundary.feed(b'x');
        assert!(boundary.is_valid());
        boundary.feed(b'\t');
        boundary.feed(b'x');
        assert!(!boundary.is_valid());
        let mut token = Validator::new(Kind::Token);
        assert!(!token.is_valid());
        for _ in 0..1000 {
            token.feed(b'x');
        }
        assert!(token.is_valid());
        token.feed(b' ');
        token.feed(b'x');
        assert!(!token.is_valid());
        assert!(std::mem::size_of::<Validator>() <= 4);
    }
}
