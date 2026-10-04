//! Bounded encoded-word recognition/decoding; the caller owns lexical placement.
pub mod decode;

pub(crate) const MAX_TOKEN_OCTETS: usize = 75;

use crate::{
    admission::work::{Charge, Meter, Stop},
    mime_charset::Charset,
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Text,
    Phrase,
    Comment,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Encoding {
    B,
    Q,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Word<'a> {
    payload: &'a [u8],
    language: Option<&'a [u8]>,
    charset: Charset,
    encoding: Encoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Span {
    start: u8,
    end: u8,
}
impl Span {
    fn within(source: &[u8], part: &[u8]) -> Option<Self> {
        let start = (part.as_ptr() as usize).checked_sub(source.as_ptr() as usize)?;
        let end = start.checked_add(part.len())?;
        source.get(start..end)?;
        Some(Self {
            start: u8::try_from(start).ok()?,
            end: u8::try_from(end).ok()?,
        })
    }
    fn view(self, source: &[u8]) -> Option<&[u8]> {
        source.get(usize::from(self.start)..usize::from(self.end))
    }
}
/// Private relative metadata for already recognized bytes in movable scratch.
/// The owner keeps logical bytes immutable and retains placement/admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Descriptor {
    length: u8,
    payload: Span,
    language: Option<Span>,
    charset: Charset,
    encoding: Encoding,
}
impl Descriptor {
    pub(crate) fn resume(self, source: &[u8]) -> Option<Word<'_>> {
        if source.len() != usize::from(self.length) {
            return None;
        }
        Some(Word {
            payload: self.payload.view(source)?,
            language: match self.language {
                Some(span) => Some(span.view(source)?),
                None => None,
            },
            charset: self.charset,
            encoding: self.encoding,
        })
    }
}
impl<'a> Word<'a> {
    /// Metadata only; the exact recognized token remains immutable in its owner.
    pub(crate) fn descriptor(self, source: &[u8]) -> Option<Descriptor> {
        if source.len() > MAX_TOKEN_OCTETS {
            return None;
        }
        Some(Descriptor {
            length: u8::try_from(source.len()).ok()?,
            payload: Span::within(source, self.payload)?,
            language: match self.language {
                Some(language) => Some(Span::within(source, language)?),
                None => None,
            },
            charset: self.charset,
            encoding: self.encoding,
        })
    }
    /// Supply a complete token whose surrounding grammar permits an encoded word.
    /// None means retain the literal token, never decode a convenient prefix.
    pub fn recognize(
        token: &'a [u8],
        context: Context,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Option<Self>, Stop> {
        Self::recognize_charged(token, context, |charge| meter.charge(now, charge))
    }
    pub(crate) fn recognize_with_work(
        token: &'a [u8],
        context: Context,
        now: Tick,
        work: &mut impl crate::decode_work::Work,
    ) -> Result<Option<Self>, crate::decode_work::Error> {
        Self::recognize_charged(token, context, |charge| work.charge(now, charge))
    }
    fn recognize_charged<E>(
        token: &'a [u8],
        context: Context,
        mut charge: impl FnMut(Charge) -> Result<(), E>,
    ) -> Result<Option<Self>, E> {
        charge(Charge {
            records: 1,
            ..Charge::default()
        })?;
        if !(9..=MAX_TOKEN_OCTETS).contains(&token.len()) {
            return Ok(None);
        }
        // Precharge three bounded scans and one fixed charset lookup (above).
        let visits = (token.len() as u64) * 3;
        charge(Charge {
            io_bytes: visits,
            records: visits,
            ..Charge::default()
        })?;
        Ok(Self::syntax(token, context))
    }
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }
    pub const fn language(self) -> Option<&'a [u8]> {
        self.language
    }
    pub const fn charset(self) -> Charset {
        self.charset
    }
    pub const fn encoding(self) -> Encoding {
        self.encoding
    }

    fn syntax(token: &'a [u8], context: Context) -> Option<Self> {
        let inner = token.strip_prefix(b"=?")?.strip_suffix(b"?=")?;
        let mut fields = inner.splitn(3, |byte| *byte == b'?');
        let label = fields.next()?;
        let encoding = match fields.next()? {
            b"b" | b"B" => Encoding::B,
            b"q" | b"Q" => Encoding::Q,
            _ => return None,
        };
        let payload = fields.next()?;
        if payload.is_empty() {
            return None;
        }
        let mut qualified = label.splitn(2, |byte| *byte == b'*');
        let charset_label = qualified.next()?;
        let language = qualified.next();
        if charset_label.is_empty()
            || !charset_label.iter().copied().all(token_char)
            || language.is_some_and(|tag| !language_shape(tag))
        {
            return None;
        }
        let charset = Charset::parse(charset_label)?;
        if !payload.iter().copied().all(|byte| {
            if !(b'!'..=b'~').contains(&byte) || byte == b'?' {
                return false;
            }
            if encoding == Encoding::B {
                // Bad base64 is still a recognized word; its decoder repairs it.
                return true;
            }
            match context {
                Context::Text => true,
                Context::Comment => !matches!(byte, b'(' | b')' | b'\\'),
                Context::Phrase => byte.is_ascii_alphanumeric() || b"!*+-/=_".contains(&byte),
            }
        }) {
            return None;
        }
        Some(Self {
            payload,
            language,
            charset,
            encoding,
        })
    }
}
fn token_char(byte: u8) -> bool {
    // RFC 2047 verified erratum 506 restores the missing backslash.
    byte != b'\\' && (b'!'..=b'~').contains(&byte) && !b"()<>@,;:\"/[]?.=".contains(&byte)
}
fn language_shape(tag: &[u8]) -> bool {
    let mut shape = td_header::language_tag::Tag::new();
    tag.iter().copied().all(|byte| shape.feed(byte)) && shape.is_complete()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::ports::Deadline;
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10000,
                records: 10000,
                ..Charge::default()
            },
        )
    }
    fn parse(token: &[u8], context: Context) -> Option<Word<'_>> {
        Word::recognize(token, context, Tick(1), &mut meter()).unwrap()
    }
    #[test]
    fn relative_descriptor_reconstructs_relocated_recognized_views() {
        assert!(std::mem::size_of::<Descriptor>() <= 16);
        for token in [b"=?utf-8*en-GB?Q?a=CC=81?=".as_slice(), b"=?ascii?B?Zm9v?="] {
            let source = token.to_vec();
            let relocated = token.to_vec();
            let word = parse(&source, Context::Text).unwrap();
            let descriptor = word.descriptor(&source).unwrap();
            assert_eq!(word.descriptor(&relocated), None);
            assert_eq!(
                descriptor.resume(&relocated),
                parse(&relocated, Context::Text)
            );
            assert_eq!(
                descriptor.resume(relocated.get(..relocated.len() - 1).unwrap()),
                None
            );
            assert!(std::ptr::eq(
                descriptor.resume(&relocated).unwrap().payload(),
                parse(&relocated, Context::Text).unwrap().payload()
            ));
        }
    }
    #[test]
    fn known_labels_encodings_and_language_qualifiers_are_borrowed() {
        assert!(std::mem::size_of::<Word<'_>>() <= 48);
        for (label, charset) in [
            ("UTF-8", Charset::Utf8),
            ("utf8", Charset::Utf8),
            ("US-ASCII", Charset::Ascii),
            ("ascii", Charset::Ascii),
            ("ISO-8859-1", Charset::Latin1),
            ("iso_8859-1", Charset::Latin1),
            ("latin1", Charset::Latin1),
            ("windows-1252", Charset::Windows1252),
            ("CP1252", Charset::Windows1252),
        ] {
            for (encoding, expected) in [
                ('B', Encoding::B),
                ('b', Encoding::B),
                ('Q', Encoding::Q),
                ('q', Encoding::Q),
            ] {
                let token = format!("=?{label}?{encoding}?a_B=20?=");
                let word = parse(token.as_bytes(), Context::Text).unwrap();
                assert_eq!(word.charset(), charset);
                assert_eq!(word.encoding(), expected);
                assert_eq!(word.payload(), b"a_B=20");
                assert_eq!(word.language(), None);
            }
        }
        for tag in [
            "EN",
            "en-GB",
            "zh-Hant-TW",
            "es-419",
            "x-private",
            "i-klingon",
        ] {
            let token = format!("=?utf-8*{tag}?Q?hello?=");
            let word = parse(token.as_bytes(), Context::Text).unwrap();
            assert_eq!(word.language(), Some(tag.as_bytes()));
            assert_eq!(word.charset(), Charset::Utf8);
        }
    }
    #[test]
    fn complete_syntax_context_and_payload_validity_are_distinct() {
        assert!(!token_char(b'\\'));
        // Verified erratum 504 forbids backslash, not double quote, in Q comments.
        assert!(parse(b"=?utf-8?Q?a\"b?=", Context::Comment).is_some());
        for (token, context) in [
            (b"=?utf-8?B?(a)?=".as_slice(), Context::Comment),
            (b"=?utf-8?B?a,b?=", Context::Phrase),
        ] {
            assert!(parse(token, context).is_some());
        }
        for token in [
            b"".as_slice(),
            b"=?utf-8?Q??=",
            b"=?utf-8?X?a?=",
            b"=?utf-8?QQ?a?=",
            b"=?unknown?Q?a?=",
            b"=?utf-8?Q?a b?=",
            b"=?utf-8?Q?a\tb?=",
            b"=?utf-8?Q?a\r\nb?=",
            b"=?utf-8?Q?a\0b?=",
            b"=?utf-8?Q?\xff?=",
            b"=?utf-8?Q?a?b?=",
            b"x=?utf-8?Q?a?=",
            b"=?utf-8?Q?a?=x",
            b"=?utf-8?Q?a?= =?utf-8?Q?b?=",
            b"=?utf-8?Q?a",
            b"=?utf-8*?Q?a?=",
            b"=?utf-8*en_uk?Q?a?=",
            b"=?utf-8*en--GB?Q?a?=",
            b"=?utf-8*en*gb?Q?a?=",
            b"=?utf-8*abcdefghi?Q?a?=",
            b"=?utf-8*419?Q?a?=",
            b"=?ansi_x3.4-1968?Q?a?=",
        ] {
            for context in [Context::Text, Context::Phrase, Context::Comment] {
                assert_eq!(parse(token, context), None, "{token:?}");
            }
        }
        for (payload, text, phrase, comment) in [
            ("hello=20world", true, true, true),
            ("(a)", true, false, false),
            ("a\\b", true, false, false),
            ("a,b", true, false, true),
            ("!*+-/=_", true, true, true),
        ] {
            let token = format!("=?utf-8?Q?{payload}?=");
            for (context, expected) in [
                (Context::Text, text),
                (Context::Phrase, phrase),
                (Context::Comment, comment),
            ] {
                assert_eq!(parse(token.as_bytes(), context).is_some(), expected);
            }
        }
        for token in [
            b"=?utf-8?Q?=?=".as_slice(),
            b"=?utf-8?Q?=QZ?=",
            b"=?utf-8?B?Z?=",
            b"=?utf-8?B?----?=",
        ] {
            assert!(parse(token, Context::Text).is_some());
        }
    }
    #[test]
    fn exact_ceiling_and_atomic_precharge_bound_every_candidate() {
        for len in [74, 75, 76, 1024 * 1024] {
            let token = format!("=?utf-8?Q?{}?=", "a".repeat(len - 12));
            assert_eq!(token.len(), len);
            let mut meter = meter();
            let result =
                Word::recognize(token.as_bytes(), Context::Text, Tick(1), &mut meter).unwrap();
            assert_eq!(result.is_some(), len <= 75);
            assert_eq!(
                10000 - meter.remaining().io_bytes,
                if len <= 75 { len as u64 * 3 } else { 0 }
            );
            assert_eq!(
                10000 - meter.remaining().records,
                if len <= 75 { len as u64 * 3 + 1 } else { 1 }
            );
        }
        let token = b"=?utf-8?Q?a?=";
        for (io, records, tick, error) in [
            (0, 100, 1, Stop::IoBytes),
            (100, 0, 1, Stop::Records),
            (100, 1, 1, Stop::Records),
            (100, 100, 100, Stop::Deadline),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: io,
                    records,
                    ..Charge::default()
                },
            );
            assert_eq!(
                Word::recognize(token, Context::Text, Tick(tick), &mut work),
                Err(error)
            );
            assert_eq!(
                Word::recognize(b"", Context::Text, Tick(1), &mut work),
                Err(error)
            );
        }
        let exact = token.len() as u64 * 3;
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: exact,
                records: exact + 1,
                ..Charge::default()
            },
        );
        assert!(Word::recognize(token, Context::Text, Tick(1), &mut work)
            .unwrap()
            .is_some());
        assert_eq!(work.remaining(), Charge::default());
    }
}
