//! The driving protocol's pure half, shared by every driven consumer: one
//! length-prefixed frame, the tab-separated ASCII envelope, the scalar and
//! byte codecs and the two response lines. No listener, thread, clock or
//! filesystem; the socket and worker adapters and a consumer's replay feed
//! it explicit bytes. A consumer parses its own verbs behind `Parse` and
//! maps `Error` into its own error type, whose codes travel in a `Refusal`.

pub const MAX_FRAME: usize = 1024 * 1024;

/// What the transport itself can refuse; a consumer's parser adds its own
/// codes behind `ErrorCode`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A malformed frame, envelope or field.
    Protocol,
    /// A well-formed request or reply past a ceiling.
    Limit,
}

impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Limit => "limit",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// An error a refusal line can carry: one stable code per variant, in the
/// grammar `valid_code` states, since the code stands unquoted in the
/// tab-separated line. A consumer pins its codes against it in its tests.
pub trait ErrorCode {
    fn code(&self) -> &'static str;
}

/// One to 32 bytes of lowercase ASCII letters, digits and hyphens.
pub fn valid_code(code: &str) -> bool {
    (1..=32).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl ErrorCode for Error {
    fn code(&self) -> &'static str {
        Error::code(*self)
    }
}

/// A request refused before or after parsing: the recovered ID (zero when
/// the failure came before one was read) and the error whose code the
/// response line carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal<E = Error> {
    pub id: u64,
    pub error: E,
}

impl<E: ErrorCode> Refusal<E> {
    /// `1 ID error CODE HEX`, the diagnostic being the code's own bytes.
    pub fn response(&self) -> String {
        let code = self.error.code();
        format!("1\t{}\terror\t{code}\t{}", self.id, hex(code.as_bytes()))
    }
}

/// `1 ID ok BODY`, the success line every consumer answers with.
pub fn ok(id: u64, body: &str) -> String {
    format!("1\t{id}\tok\t{body}")
}

/// A request type the worker parses on its own thread, before any consumer
/// state is touched. The refusal is complete: its response line is written
/// back without consulting the consumer.
pub trait Parse: Sized + Send + 'static {
    type Error: ErrorCode;
    fn parse(payload: &[u8]) -> std::result::Result<Self, Refusal<Self::Error>>;
}

/// One length-prefixed frame. Any refusal poisons it and drops partial text.
#[derive(Default)]
pub struct Decoder {
    header: [u8; 4],
    header_used: usize,
    payload: Vec<u8>,
    used: usize,
    failed: bool,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let result = self.append(bytes);
        if result.is_err() {
            self.failed = true;
            self.payload = Vec::new();
        }
        result
    }

    fn append(&mut self, mut bytes: &[u8]) -> Result<()> {
        if self.failed {
            return Err(Error::Protocol);
        }
        if self.header_used < 4 {
            let take = bytes.len().min(4 - self.header_used);
            self.header
                .get_mut(self.header_used..self.header_used + take)
                .ok_or(Error::Protocol)?
                .copy_from_slice(bytes.get(..take).ok_or(Error::Protocol)?);
            self.header_used += take;
            bytes = bytes.get(take..).ok_or(Error::Protocol)?;
            if self.header_used < 4 {
                return Ok(());
            }
            let size =
                usize::try_from(u32::from_be_bytes(self.header)).map_err(|_| Error::Limit)?;
            if size == 0 {
                return Err(Error::Protocol);
            }
            if size > MAX_FRAME {
                return Err(Error::Limit);
            }
            self.payload = vec![0; size];
        }
        let end = self.used.checked_add(bytes.len()).ok_or(Error::Limit)?;
        self.payload
            .get_mut(self.used..end)
            .ok_or(Error::Protocol)?
            .copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    pub fn payload(&self) -> Option<&[u8]> {
        (!self.failed && self.header_used == 4 && self.used == self.payload.len())
            .then_some(self.payload.as_slice())
    }

    /// EOF before a complete payload is an error, not a shorter request.
    pub fn finish(self) -> Result<Vec<u8>> {
        if self.payload().is_none() {
            return Err(Error::Protocol);
        }
        Ok(self.payload)
    }
}

pub fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() {
        return Err(Error::Protocol);
    }
    if payload.len() > MAX_FRAME {
        return Err(Error::Limit);
    }
    let length = u32::try_from(payload.len()).map_err(|_| Error::Limit)?;
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(payload);
    Ok(framed)
}

/// A validated request line: version one, the caller's ID, the verb and its
/// remaining fields, still unsplit past what the consumer takes.
pub struct Envelope<'a> {
    pub id: u64,
    pub name: &'a str,
    pub args: std::str::Split<'a, char>,
}

/// Validates the envelope only; the verb and its fields are the consumer's.
/// Errors before a valid ID is read use zero; the refusal is lifted into the
/// consumer's error type so its parser can `?` straight through.
pub fn envelope<E: From<Error>>(input: &[u8]) -> std::result::Result<Envelope<'_>, Refusal<E>> {
    let mut id = 0;
    let result = (|| {
        if input.len() > MAX_FRAME {
            return Err(Error::Limit);
        }
        let input = std::str::from_utf8(input).map_err(|_| Error::Protocol)?;
        if !input.is_ascii() || input.bytes().any(|b| b < b' ' && b != b'\t' || b == 127) {
            return Err(Error::Protocol);
        }
        let mut args = input.split('\t');
        if args.next() != Some("1") {
            return Err(Error::Protocol);
        }
        id = decimal(args.next().ok_or(Error::Protocol)?)?;
        let name = args.next().ok_or(Error::Protocol)?;
        Ok((name, args))
    })();
    result
        .map(|(name, args)| Envelope { id, name, args })
        .map_err(|error| Refusal {
            id,
            error: error.into(),
        })
}

/// Digits only: no sign, space, exponent or overflow. Leading zeros pass.
pub fn decimal(text: &str) -> Result<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    text.parse().map_err(|_| Error::Protocol)
}

pub fn size(text: &str) -> Result<usize> {
    usize::try_from(decimal(text)?).map_err(|_| Error::Protocol)
}

pub fn boolean(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(Error::Protocol),
    }
}

/// Lowercase hex, two digits per byte; the empty string is `-`.
pub fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        for index in [usize::from(byte >> 4), usize::from(byte & 15)] {
            if let Some(&digit) = DIGITS.get(index) {
                out.push(char::from(digit));
            }
        }
    }
    out
}

pub fn unhex(value: &str) -> Result<Vec<u8>> {
    if value == "-" {
        return Ok(Vec::new());
    }
    if value.is_empty() || value.len() > MAX_FRAME || !value.len().is_multiple_of(2) {
        return Err(Error::Protocol);
    }
    let digit = |b: u8| -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(Error::Protocol),
        }
    };
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = digit(*pair.first().ok_or(Error::Protocol)?)?;
            let low = digit(*pair.get(1).ok_or(Error::Protocol)?)?;
            Ok(high * 16 + low)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn line(input: &[u8]) -> std::result::Result<(u64, String, Vec<String>), Refusal> {
        let Envelope { id, name, args } = envelope::<Error>(input)?;
        Ok((id, name.into(), args.map(String::from).collect()))
    }

    #[test]
    fn every_frame_split_and_single_byte_delivery_wait_for_complete_payload() {
        let payload = b"1\t17\ttext\t1\t0\t0\t4";
        let bytes = frame(payload).unwrap();
        for split in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(bytes.get(..split).unwrap()).unwrap();
            assert_eq!(
                decoder.payload(),
                (split == bytes.len()).then_some(payload.as_slice())
            );
            decoder.push(bytes.get(split..).unwrap()).unwrap();
            assert_eq!(decoder.payload(), Some(payload.as_slice()));
            assert_eq!(decoder.finish().unwrap(), payload);
        }
        let mut decoder = Decoder::default();
        for (index, byte) in bytes.iter().enumerate() {
            decoder.push(std::slice::from_ref(byte)).unwrap();
            assert_eq!(decoder.payload().is_some(), index + 1 == bytes.len());
        }
        assert_eq!(
            line(&decoder.finish().unwrap()).unwrap(),
            (
                17,
                "text".into(),
                vec!["1".into(), "0".into(), "0".into(), "4".into()]
            )
        );
    }

    #[test]
    fn frame_limits_truncation_and_trailing_bytes_never_publish_partial_requests() {
        let bytes = frame(b"1\t0\tstate").unwrap();
        for end in 0..bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(bytes.get(..end).unwrap()).unwrap();
            assert_eq!(decoder.finish(), Err(Error::Protocol));
        }
        for length in [0u32, MAX_FRAME as u32 + 1, u32::MAX] {
            let mut decoder = Decoder::default();
            assert!(decoder.push(&length.to_be_bytes()).is_err());
            assert!(decoder.payload.is_empty());
            assert_eq!(decoder.push(&bytes), Err(Error::Protocol));
            assert!(decoder.payload().is_none());
        }
        assert_eq!(
            Decoder::default().push(&(MAX_FRAME as u32 + 1).to_be_bytes()),
            Err(Error::Limit)
        );
        let mut decoder = Decoder::default();
        decoder.push(&bytes).unwrap();
        assert_eq!(decoder.push(b"x"), Err(Error::Protocol));
        assert!(decoder.payload().is_none());
        let mut joined = bytes.clone();
        joined.extend_from_slice(&bytes);
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(&joined), Err(Error::Protocol));
        assert!(decoder.payload().is_none());
        // Empty chunks make no progress and poison nothing.
        let mut decoder = Decoder::default();
        decoder.push(b"").unwrap();
        decoder.push(&bytes).unwrap();
        decoder.push(b"").unwrap();
        assert_eq!(decoder.finish().unwrap(), b"1\t0\tstate");
        assert_eq!(frame(b""), Err(Error::Protocol));
        assert_eq!(frame(&vec![b'x'; MAX_FRAME + 1]), Err(Error::Limit));
        let limit = frame(&vec![b'x'; MAX_FRAME]).unwrap();
        let mut decoder = Decoder::default();
        decoder.push(&limit).unwrap();
        assert_eq!(decoder.finish().unwrap().len(), MAX_FRAME);
    }

    #[test]
    fn envelope_grammar_is_ascii_versioned_and_recovers_ids_before_the_verb() {
        for (input, error) in [
            ("", Error::Protocol),
            ("2\t12\tstate", Error::Protocol),
            ("1\t+1\tstate", Error::Protocol),
            ("1\t-1\tstate", Error::Protocol),
            ("1\t 1\tstate", Error::Protocol),
            ("1\t18446744073709551616\tstate", Error::Protocol),
            ("1\t12\tstate\n", Error::Protocol),
            ("1\t12\tst\u{7f}ate", Error::Protocol),
            ("1\t12\tstáte", Error::Protocol),
            ("\t12\tstate", Error::Protocol),
            ("1\t\tstate", Error::Protocol),
        ] {
            assert_eq!(
                line(input.as_bytes()),
                Err(Refusal { id: 0, error }),
                "{input:?}"
            );
        }
        assert_eq!(
            line(&[0xff]),
            Err(Refusal {
                id: 0,
                error: Error::Protocol
            })
        );
        assert_eq!(
            line(&vec![b'\t'; MAX_FRAME + 1]),
            Err(Refusal {
                id: 0,
                error: Error::Limit
            })
        );
        assert!(line(&vec![b'\t'; MAX_FRAME]).is_err());
        // A missing verb is refused under the ID already read.
        assert_eq!(
            line(b"1\t12"),
            Err(Refusal {
                id: 12,
                error: Error::Protocol
            })
        );
        // The verb and everything after it are the consumer's to judge.
        assert_eq!(line(b"1\t000\tstate").unwrap(), (0, "state".into(), vec![]));
        assert_eq!(
            line(b"1\t18446744073709551615\tnew\t").unwrap(),
            (u64::MAX, "new".into(), vec!["".into()])
        );
        assert_eq!(
            line(b"1\t12\t\ta\t\tb").unwrap(),
            (12, "".into(), vec!["a".into(), "".into(), "b".into()])
        );
        // A consumer's own error type receives the lifted refusal.
        #[derive(Debug, PartialEq, Eq)]
        enum Own {
            Wire(Error),
        }
        impl From<Error> for Own {
            fn from(error: Error) -> Self {
                Self::Wire(error)
            }
        }
        assert_eq!(
            envelope::<Own>(b"1\t9\t\x01").map(|_| ()),
            Err(Refusal {
                id: 0,
                error: Own::Wire(Error::Protocol)
            })
        );
        assert_eq!(envelope::<Own>(b"1\t9\tx").map(|e| e.id), Ok(9));
    }

    #[test]
    fn scalar_codecs_and_response_lines_are_exact() {
        assert_eq!(decimal("0"), Ok(0));
        assert_eq!(decimal("007"), Ok(7));
        assert_eq!(decimal("18446744073709551615"), Ok(u64::MAX));
        for invalid in [
            "",
            "+1",
            "-1",
            "1 ",
            " 1",
            "1e3",
            "18446744073709551616",
            "١",
        ] {
            assert_eq!(decimal(invalid), Err(Error::Protocol), "{invalid:?}");
        }
        assert_eq!(size("42"), Ok(42));
        assert_eq!(boolean("0"), Ok(false));
        assert_eq!(boolean("1"), Ok(true));
        for invalid in ["", "2", "true", "01"] {
            assert_eq!(boolean(invalid), Err(Error::Protocol), "{invalid:?}");
        }
        let bytes: Vec<_> = (0..=255).collect();
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(hex(b""), "-");
        assert_eq!(hex(b"\x00\x0f\xf0\xff"), "000ff0ff");
        assert_eq!(unhex("-").unwrap(), b"");
        for invalid in ["", "A0", "g0", "0", "--", "0-", " 00"] {
            assert_eq!(unhex(invalid), Err(Error::Protocol), "{invalid:?}");
        }
        assert_eq!(unhex(&"0".repeat(MAX_FRAME + 2)), Err(Error::Protocol));
        assert_eq!(unhex(&"0".repeat(MAX_FRAME)).unwrap().len(), MAX_FRAME / 2);
        assert_eq!(ok(7, "a\tb"), "1\t7\tok\ta\tb");
        assert_eq!(ok(0, ""), "1\t0\tok\t");
        assert_eq!(
            Refusal {
                id: 9,
                error: Error::Limit
            }
            .response(),
            "1\t9\terror\tlimit\t6c696d6974"
        );
        assert_eq!(
            Refusal {
                id: 0,
                error: Error::Protocol
            }
            .response(),
            "1\t0\terror\tprotocol\t70726f746f636f6c"
        );
        assert_eq!(Error::Limit.to_string(), "limit");
        for error in [Error::Protocol, Error::Limit] {
            assert!(valid_code(error.code()));
        }
        for valid in ["a", "no-match", "stale-revision", "e2", &"x".repeat(32)] {
            assert!(valid_code(valid), "{valid:?}");
        }
        for invalid in [
            "",
            "Limit",
            "no match",
            "no\tmatch",
            "é",
            "-\n",
            &"x".repeat(33),
        ] {
            assert!(!valid_code(invalid), "{invalid:?}");
        }
        struct Stale;
        impl ErrorCode for Stale {
            fn code(&self) -> &'static str {
                "stale-revision"
            }
        }
        assert_eq!(
            Refusal {
                id: 9,
                error: Stale
            }
            .response(),
            "1\t9\terror\tstale-revision\t7374616c652d7265766973696f6e"
        );
    }

    #[test]
    fn arbitrary_bytes_have_closed_error_paths_as_headers_and_as_payloads() {
        let mut completed = 0;
        for seed in 0u64..1000 {
            let mut value = seed;
            let bytes: Vec<_> = (0..64)
                .map(|_| {
                    value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (value >> 32) as u8
                })
                .collect();
            let mut decoder = Decoder::default();
            let _ = decoder.push(&bytes);
            if let Some(payload) = decoder.payload() {
                let _ = line(payload);
            }
            let _ = decoder.finish();
            let _ = line(&bytes);
            if seed % 3 == 0 {
                let framed = frame(&bytes).unwrap();
                let mut decoder = Decoder::default();
                for part in framed.chunks((seed % 7 + 1) as usize) {
                    decoder.push(part).unwrap();
                }
                assert_eq!(decoder.payload(), Some(bytes.as_slice()));
                let _ = line(decoder.payload().unwrap());
                assert_eq!(decoder.finish().unwrap(), bytes);
                completed += 1;
            }
        }
        assert_eq!(completed, 334);
    }
}
