//! Marker framing over already admitted logical octets.
//! This passive state grants no encoded-word grammar or decoding authority.
//! Its RFC 2047 ceiling applies to all word contexts; URI runs are one consumer.
/// RFC 2047's complete logical encoded-word ceiling, including delimiters.
pub const MAX_OCTETS: usize = 75;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    More,
    Complete,
    Rejected,
}
#[derive(Clone, Copy)]
enum Phase {
    Equal,
    Question,
    Charset,
    Encoding,
    Payload,
    Complete,
    Rejected,
}
/// Passive fixed state; the enclosing owner funds every feed before access.
/// Feed one candidate beginning with =? through its first ?= delimiter.
/// Completion frames bytes only; charset/encoding/payload validity is external.
#[derive(Clone, Copy)]
pub struct Token {
    phase: Phase,
    length: u8,
    question: bool,
}
impl Default for Token {
    fn default() -> Self {
        Self::new()
    }
}
impl Token {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Equal,
            length: 0,
            question: false,
        }
    }
    pub const fn length(&self) -> usize {
        self.length as usize
    }
    pub const fn is_complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }
    pub fn feed(&mut self, byte: u8) -> Status {
        if matches!(self.phase, Phase::Complete | Phase::Rejected) {
            self.phase = Phase::Rejected;
            return Status::Rejected;
        }
        let Some(length) = self.length.checked_add(1) else {
            self.phase = Phase::Rejected;
            return Status::Rejected;
        };
        if usize::from(length) > MAX_OCTETS {
            self.phase = Phase::Rejected;
            return Status::Rejected;
        }
        self.length = length;
        match self.phase {
            Phase::Equal if byte == b'=' => self.phase = Phase::Question,
            Phase::Question if byte == b'?' => self.phase = Phase::Charset,
            Phase::Charset if byte == b'?' => self.phase = Phase::Encoding,
            Phase::Encoding if byte == b'?' => self.phase = Phase::Payload,
            Phase::Charset | Phase::Encoding => {}
            Phase::Payload => {
                if self.question && byte == b'=' {
                    self.phase = Phase::Complete;
                    return Status::Complete;
                }
                self.question = byte == b'?';
            }
            Phase::Equal | Phase::Question | Phase::Complete | Phase::Rejected => {
                self.phase = Phase::Rejected;
                return Status::Rejected;
            }
        }
        Status::More
    }
}
const _: () = assert!(std::mem::size_of::<Token>() <= 4);
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markers_frame_without_claiming_word_grammar() {
        for source in [
            b"=?ascii?Q?a?=".as_slice(),
            b"=?ascii?Q?=3F?=",
            b"=?unknown?bad??=",
            b"=?\xff?Q?a?=",
        ] {
            let mut token = Token::new();
            for (i, byte) in source.iter().copied().enumerate() {
                assert_eq!(
                    token.feed(byte),
                    if i + 1 == source.len() {
                        Status::Complete
                    } else {
                        Status::More
                    }
                );
            }
            assert!(token.is_complete());
            assert_eq!(token.length(), source.len());
            assert_eq!(token.feed(b'x'), Status::Rejected);
            assert!(!token.is_complete());
            assert_eq!(token.feed(b'='), Status::Rejected);
        }
        let mut token = Token::new();
        for byte in b"=???" {
            assert_eq!(token.feed(*byte), Status::More);
        }
        assert_eq!(token.feed(b'?'), Status::More);
        assert_eq!(token.feed(b'='), Status::Complete);
    }
    #[test]
    fn exact_ceiling_and_first_delimiter_are_fixed() {
        for length in [MAX_OCTETS, MAX_OCTETS + 1] {
            let mut source = b"=?ascii?Q?".to_vec();
            source.resize(length - 2, b'a');
            source.extend_from_slice(b"?=");
            let mut token = Token::new();
            let mut last = Status::More;
            for byte in source {
                last = token.feed(byte);
            }
            assert_eq!(
                last,
                if length == MAX_OCTETS {
                    Status::Complete
                } else {
                    Status::Rejected
                }
            );
            assert!(token.length() <= MAX_OCTETS);
        }
        let mut token = Token::new();
        let mut ends = 0;
        for byte in b"=?ascii?Q?a?=tail?=" {
            if token.feed(*byte) == Status::Complete {
                ends += 1;
            }
        }
        assert_eq!(ends, 1);
        assert!(!token.is_complete());
    }
    #[test]
    fn passive_copy_retains_only_framing_progress() {
        let mut token = Token::new();
        for byte in b"=?ascii?Q?a" {
            assert_eq!(token.feed(*byte), Status::More);
        }
        let mut copy = token;
        assert_eq!(token.feed(b'?'), Status::More);
        assert_eq!(copy.feed(b'?'), Status::More);
        assert_eq!(token.feed(b'='), Status::Complete);
        assert_eq!(copy.feed(b'='), Status::Complete);
        assert_eq!(token.length(), copy.length());
        let mut token = Token::new();
        assert_eq!(token.feed(b'x'), Status::Rejected);
        let mut copy = token;
        assert_eq!(copy.feed(b'='), Status::Rejected);
    }
}
