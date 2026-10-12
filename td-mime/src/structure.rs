//! Complete resident MIME structure; source authorization is external.
use crate::{
    base64, decode_work, delimiter,
    header_select::SourceEnd,
    metadata,
    nfc::HeaderBudget,
    parameter::protocol,
    quoted_printable,
    time::Tick,
    work::{Charge, Meter, Stop},
};
#[path = "structure/classify.rs"]
pub mod classify;
#[path = "structure/entity.rs"]
pub mod entity;
#[path = "structure/stream.rs"]
pub mod stream;
pub const MAX_DEPTH: usize = 64;
/// Parsing ceilings, independent of any service configuration or account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub header_bytes: usize,
    pub mime_depth: usize,
    pub mime_parts: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            header_bytes: 256 * 1024,
            mime_depth: 32,
            mime_parts: 1024,
        }
    }
}
impl From<&Limits> for Limits {
    fn from(value: &Limits) -> Self {
        *value
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Media {
    #[default]
    Other,
    TextPlain,
    MessageRfc822,
    Multipart,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Encoding {
    #[default]
    Identity,
    Base64,
    QuotedPrintable,
}
/// Passive structure evidence, never a blob ID or source authorization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Part {
    pub entity_start: u64,
    pub entity_end: u64,
    pub body_start: u64,
    pub size: u64,
    pub type_start: u64,
    pub type_end: u64,
    pub ordinal: u16,
    /// Zero for the root; otherwise the parent's preorder ordinal.
    pub parent: u16,
    pub depth: u8,
    pub media: Media,
    pub encoding: Encoding,
    /// Parse-problem flags plus DIGEST_CHILD_CONTEXT; use problems() for errors.
    pub diagnostics: u8,
}
pub const UNKNOWN_ENCODING: u8 = 1;
pub const ENCODING_PROBLEM: u8 = 2;
pub const MISSING_CLOSE: u8 = 4;
pub const IGNORED_SUFFIX: u8 = 8;
pub const PARAMETER_PROBLEM: u8 = 16;
/// Context evidence, not a parse problem; independent of the selected MIME type.
pub const DIGEST_CHILD_CONTEXT: u8 = 32;
pub const PROBLEM_FLAGS: u8 =
    UNKNOWN_ENCODING | ENCODING_PROBLEM | MISSING_CLOSE | IGNORED_SUFFIX | PARAMETER_PROBLEM;
impl Part {
    /// Only parse-problem bits; context evidence does not imply a malformed part.
    pub const fn problems(&self) -> u8 {
        self.diagnostics & PROBLEM_FLAGS
    }
    /// Original header default context; passive evidence grants no source authority.
    pub const fn context(&self) -> metadata::Context {
        if self.diagnostics & DIGEST_CHILD_CONTEXT != 0 {
            metadata::Context::DigestChild
        } else {
            metadata::Context::Normal
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    IncompleteSource,
    InvalidLimits,
    InvalidState,
    DepthLimit,
    PartLimit,
    HeaderLimit,
    OutputCapacity,
    NotParsable,
    Work(Stop),
    InterpretationLimit,
    Metadata(metadata::Error),
    Parameter(protocol::Error),
    Delimiter(delimiter::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRange => "invalid MIME entity range",
            Self::IncompleteSource => "incomplete MIME entity source",
            Self::InvalidLimits => "invalid MIME traversal limits",
            Self::InvalidState => "invalid MIME traversal state",
            Self::DepthLimit => "MIME depth limit",
            Self::PartLimit => "MIME part limit",
            Self::HeaderLimit => "MIME aggregate header limit",
            Self::OutputCapacity => "MIME descriptor capacity",
            Self::NotParsable => "MIME structure is not parsable",
            Self::Work(_) => "MIME traversal work refusal",
            Self::InterpretationLimit => "MIME header interpretation limit",
            Self::Metadata(_) => "MIME metadata refusal",
            Self::Parameter(_) => "MIME boundary parameter refusal",
            Self::Delimiter(_) => "MIME delimiter refusal",
        })
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Work(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::Parameter(error) => Some(error),
            Self::Delimiter(error) => Some(error),
            _ => None,
        }
    }
}
impl From<decode_work::Error> for Error {
    fn from(error: decode_work::Error) -> Self {
        match error {
            decode_work::Error::Work(stop) => Self::Work(stop),
            decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
/// Resident adapter over the shared explicit-frame traversal.
/// Input is the complete authorized entity; source authorization is external.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::structure::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::structure::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    base: u64,
    max_headers: u64,
    core: stream::Cursor<'a, 'w>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        base: u64,
        source_end: SourceEnd,
        limits: impl Into<Limits>,
        parts: &'w mut [Part],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        if source_end != SourceEnd::Eof {
            return Err(Error::IncompleteSource);
        }
        let end = base
            .checked_add(u64::try_from(source.len()).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        let limits = limits.into();
        Ok(Self {
            source,
            base,
            max_headers: u64::try_from(limits.header_bytes).map_err(|_| Error::InvalidLimits)?,
            core: stream::Cursor::new(base..end, limits, parts)?,
            work,
            budget,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.core.check_deadline(now, self.work, self.budget)
    }
    pub fn parts(&self) -> Result<Option<&[Part]>, Error> {
        self.core.parts()
    }
    pub fn header_bytes(&self) -> Option<u64> {
        self.core.header_bytes()
    }
    /// Passive original source labels; these grant no file or account authority.
    pub fn input(&self) -> (&'a [u8], u64, u64) {
        (self.source, self.base, self.max_headers)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<(&'w [Part], &'w mut Meter, &'w mut HeaderBudget), Error> {
        let parts = self.core.finish(now, self.work, self.budget)?;
        Ok((parts, self.work, self.budget))
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if self.core.parts()?.is_some() {
            return Ok(Status::Complete);
        }
        let result = (|| {
            let input = match self.core.request()? {
                stream::Request::None => stream::Input::None,
                stream::Request::Headers { start, end, .. } => stream::Input::Headers {
                    bytes: td_header::resident::slice(self.source, self.base, start..end)
                        .ok_or(Error::InvalidRange)?,
                    source_end: SourceEnd::Eof,
                },
                stream::Request::Body { start, end } => stream::Input::Body {
                    offset: start,
                    bytes: td_header::resident::slice(self.source, self.base, start..end)
                        .ok_or(Error::InvalidRange)?,
                },
            };
            self.core.poll(input, now, self.work, self.budget)
        })();
        self.core.outcome(result)
    }
}

const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 16 * 1024
);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::time::Deadline;
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(), Error> {
        for _ in 0..1_000_000 {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            let result = cursor.poll(Tick(1));
            let after = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            assert!(before.0.io_bytes - after.0.io_bytes <= 352);
            assert!(before.0.records - after.0.records <= 32);
            assert!(before.0.output_bytes - after.0.output_bytes <= 96);
            assert!(before.1 - after.1 <= 352);
            assert!(before.2 - after.2 <= 512);
            if result? == Status::Complete {
                return Ok(());
            }
            assert!(cursor.parts().unwrap().is_none());
        }
        panic!("traversal did not finish")
    }
    fn parse(source: &[u8]) -> Result<(Vec<Part>, u64), Error> {
        let mut backing = [Part::default(); 64];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source,
            17,
            SourceEnd::Eof,
            Limits::default(),
            &mut backing,
            &mut work,
            &mut budget,
        )?;
        drain(&mut cursor)?;
        Ok((
            cursor.parts()?.unwrap().to_vec(),
            cursor.header_bytes().unwrap(),
        ))
    }
    #[test]
    fn resident_range_failure_retires_all_observations() {
        let mut parts = [Part::default(); 1];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            b"\nbody",
            17,
            SourceEnd::Eof,
            Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        // Inject an impossible adapter/source mismatch to pin the error path.
        cursor.source = &[];
        let error = Error::InvalidRange;
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.parts(), Err(error));
        assert_eq!(cursor.header_bytes(), None);
        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
    }
    #[test]
    fn nested_preorder_exact_extents_sizes_and_digest_defaults() {
        const {
            assert!(std::mem::size_of::<Part>() <= 64);
        }
        assert!(
            std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>()
                <= 16 * 1024
        );
        let source = concat!(
            "Content-Type: multipart/mixed; boundary=a\r\n\r\npre\r\n--a\r\n",
            "Content-Type: multipart/digest; boundary=b\r\n\r\n--b\r\n",
            "\r\nFrom: nested\r\n\r\nbody\r\n--b--\r\nepi\r\n--a\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--a\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\nx=20y=\r\n--a--\r\nepi"
        )
        .as_bytes();
        let (parts, headers) = parse(source).unwrap();
        assert_eq!(parts.len(), 5);
        assert_eq!(
            parts.iter().map(|part| part.context()).collect::<Vec<_>>(),
            [
                metadata::Context::Normal,
                metadata::Context::Normal,
                metadata::Context::DigestChild,
                metadata::Context::Normal,
                metadata::Context::Normal,
            ]
        );
        assert_eq!(
            parts
                .iter()
                .map(|p| (p.ordinal, p.parent, p.depth, p.media, p.size))
                .collect::<Vec<_>>(),
            [
                (
                    1,
                    0,
                    1,
                    Media::Multipart,
                    (source.len() - source.windows(4).position(|w| w == b"\r\n\r\n").unwrap() - 4)
                        as u64
                ),
                (
                    2,
                    1,
                    2,
                    Media::Multipart,
                    b"--b\r\n\r\nFrom: nested\r\n\r\nbody\r\n--b--\r\nepi".len() as u64,
                ),
                (3, 2, 3, Media::MessageRfc822, 20),
                (4, 1, 2, Media::TextPlain, 3),
                (5, 1, 2, Media::TextPlain, 3),
            ]
        );
        // Independent offsets selected from literal separator/delimiter positions.
        let first_a = source.windows(5).position(|w| w == b"--a\r\n").unwrap();
        let second_a = source
            .windows(5)
            .enumerate()
            .skip(first_a + 5)
            .find(|(_, w)| *w == b"--a\r\n")
            .unwrap()
            .0;
        assert_eq!(parts[1].entity_start, 17 + first_a as u64 + 5);
        assert_eq!(parts[1].entity_end, 17 + second_a as u64 - 2);
        assert_eq!(parts[1].size, parts[1].entity_end - parts[1].body_start);
        assert_eq!(parts[2].size, parts[2].entity_end - parts[2].body_start);
        assert_eq!(headers, 43 + 44 + 35 + 45);
        assert_eq!(parts[4].diagnostics & ENCODING_PROBLEM, ENCODING_PROBLEM);
    }
    #[test]
    fn retained_metadata_replays_original_digest_context_and_absolute_ranges() {
        use crate::{nfc::Scratch, part_headers};
        use part_headers::label_json;
        let source = concat!(
            "Content-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\n",
            "Content-Type: multipart/digest; boundary=b\r\n\r\n--b\r\n",
            "Content-ID: <id@a>\r\nContent-Language: fr\r\nContent-Location: \r\n",
            "\r\nFrom: inner\r\n\r\nbody\r\n--b\r\n",
            "Content-Type: text/plain\r\nContent-Location: ../a\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\nYQ!\r\n--b\r\n",
            "Content-Type: bad\r\nContent-Location: a%\r\n\r\nbody\r\n--b--\r\n--a\r\n",
            "\r\nContent-Type: image/png\r\nContent-Location: ../body\r\n--a--\r\n"
        )
        .as_bytes();
        let expected_types: [&[u8]; 6] = [
            b"multipart/mixed",
            b"multipart/digest",
            b"message/rfc822",
            b"text/plain",
            b"message/rfc822",
            b"text/plain",
        ];
        let expected_locations: [Option<&[u8]>; 6] =
            [None, None, Some(b"\"\""), Some(b"\"../a\""), None, None];
        for base in [0, 17, u64::MAX - source.len() as u64] {
            let mut parts = [Part::default(); 64];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let work_ptr = std::ptr::from_mut(&mut work);
            let budget_ptr = std::ptr::from_mut(&mut budget);
            let mut cursor = Cursor::new(
                source,
                base,
                SourceEnd::Eof,
                Limits::default(),
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let (parts, work, budget) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(parts.len(), expected_types.len());
            for (index, part) in parts.iter().enumerate() {
                let mut heads = [0; 256];
                let mut charset = [0; 256];
                let mut filename = [0; 256];
                let mut id = [0; 256];
                let mut language = [0; 256];
                let mut location = [0; 256];
                let mut scratch = Scratch::new();
                let scratch_ptr = std::ptr::from_mut(&mut scratch);
                let entity = part_headers::Entity {
                    source: td_header::resident::slice(
                        source,
                        base,
                        part.entity_start..part.entity_end,
                    )
                    .unwrap(),
                    base: part.entity_start,
                    source_end: SourceEnd::Eof,
                    header_limit: Limits::default().header_bytes as u64,
                    context: part.context(),
                };
                let mut cursor = label_json::Cursor::new(
                    entity,
                    label_json::Backing {
                        headers: part_headers::Backing {
                            heads: &mut heads,
                            charset: &mut charset,
                            filename: &mut filename,
                        },
                        labels: crate::label_fields::json::Backing {
                            content_id: &mut id,
                            content_language: &mut language,
                        },
                        content_location: &mut location,
                    },
                    work,
                    budget,
                    &mut scratch,
                )
                .unwrap();
                let mut complete = false;
                for _ in 0..100_000 {
                    if cursor.poll(Tick(1)).unwrap() == part_headers::Status::Complete {
                        complete = true;
                        break;
                    }
                    assert!(cursor.value().is_none());
                }
                assert!(complete);
                let (view, returned_work, returned_budget, returned_scratch) =
                    cursor.finish(Tick(1)).unwrap();
                assert_eq!(std::ptr::from_mut(returned_work), work_ptr);
                assert_eq!(std::ptr::from_mut(returned_budget), budget_ptr);
                assert_eq!(std::ptr::from_mut(returned_scratch), scratch_ptr);
                assert_eq!(view.headers.content_type, expected_types[index]);
                assert_eq!(view.headers.body_start, part.body_start);
                assert_eq!(view.location.selection.end.body_start, part.body_start);
                assert_eq!(view.location.value, expected_locations[index]);
                assert_eq!(
                    part.context(),
                    if (2..=4).contains(&index) {
                        metadata::Context::DigestChild
                    } else {
                        metadata::Context::Normal
                    }
                );
                if index == 3 {
                    assert_eq!(part.diagnostics, ENCODING_PROBLEM | DIGEST_CHILD_CONTEXT);
                }
                if index == 2 {
                    assert_eq!(view.labels.content_id, Some(b"\"id@a\"".as_slice()));
                    assert_eq!(view.labels.content_language, Some(b"[\"fr\"]".as_slice()));
                }
            }
        }
    }
    #[test]
    fn nested_digest_context_and_post_append_problems_remain_independent() {
        let source = concat!(
            "Content-Type: multipart/digest;boundary=a\n\n--a\n",
            "Content-Type: multipart/mixed;boundary=b\n\n--b\n\ninside\n",
            "--a\nContent-Type: multipart/digest;boundary=c\n\n",
            "--c-tail\n\ninner\n--c--\n--a--"
        )
        .as_bytes();
        let (parts, _) = parse(source).unwrap();
        assert_eq!(parts.len(), 5);
        let expected = [
            (metadata::Context::Normal, Media::Multipart, 0),
            (
                metadata::Context::DigestChild,
                Media::Multipart,
                MISSING_CLOSE,
            ),
            (metadata::Context::Normal, Media::TextPlain, 0),
            (
                metadata::Context::DigestChild,
                Media::Multipart,
                IGNORED_SUFFIX,
            ),
            (metadata::Context::DigestChild, Media::MessageRfc822, 0),
        ];
        for (part, (context, media, problems)) in parts.iter().zip(expected) {
            assert_eq!(part.context(), context);
            assert_eq!(part.media, media);
            assert_eq!(part.problems(), problems);
            assert_eq!(part.diagnostics & PROBLEM_FLAGS, problems);
        }
        assert_eq!(parts[1].diagnostics, DIGEST_CHILD_CONTEXT | MISSING_CLOSE);
        assert_eq!(parts[3].diagnostics, DIGEST_CHILD_CONTEXT | IGNORED_SUFFIX);
        let context_only = Part {
            diagnostics: DIGEST_CHILD_CONTEXT | 64 | 128,
            ..Part::default()
        };
        assert_eq!(context_only.problems(), 0);
        assert_eq!(context_only.context(), metadata::Context::DigestChild);
    }
    #[test]
    fn recovery_outer_precedence_empty_children_and_no_implicit_recursion() {
        for (source, count, media, diagnostics) in [
            (b"".as_slice(), 1, Media::TextPlain, 0),
            (
                concat!(
                    "Content-Type: message/global\n\n",
                    "Content-Type: multipart/mixed;boundary=x\n\n--x\n"
                )
                .as_bytes(),
                1,
                Media::Other,
                0,
            ),
            (
                b"Content-Type: multipart/mixed;boundary=x\n\n--x\n--x--",
                2,
                Media::Multipart,
                0,
            ),
            (
                b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\nbody",
                2,
                Media::Multipart,
                MISSING_CLOSE,
            ),
            (
                b"Content-Type: multipart/mixed;boundary=x\n\n--x-tail\n\nbody\n--x--tail",
                2,
                Media::Multipart,
                IGNORED_SUFFIX,
            ),
        ] {
            let (parts, _) = parse(source).unwrap();
            assert_eq!(parts.len(), count);
            assert_eq!(parts[0].media, media);
            assert_eq!(parts[0].diagnostics, diagnostics);
        }
        for source in [
            b"Content-Type: multipart/mixed\n\nraw".as_slice(),
            b"Content-Type: multipart/mixed;boundary=x\n\n--x--",
            b"Content-Type: multipart/mixed;boundary=x\nContent-Transfer-Encoding: base64\n\nLS14",
            b"Content-Type: multipart/mixed;boundary=ok;boundary*=utf-8''bad%3B\n\n--ok\n",
            // Outer 'a' wins over 'ab', leaving the inner container no opening.
            concat!(
                "Content-Type: multipart/mixed;boundary=a\n\n--a\n",
                "Content-Type: multipart/mixed;boundary=ab\n\n",
                "--ab\n\nbody\n--ab--\n--a--"
            )
            .as_bytes(),
        ] {
            assert_eq!(parse(source), Err(Error::NotParsable));
        }
        let source = b"Content-Type: multipart/mixed;boundary=a\n\n--a\nContent-Type: multipart/mixed;boundary=b\n\n--b\n\ninside\n--a\n\nafter\n--a--";
        let (parts, _) = parse(source).unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[1].diagnostics, MISSING_CLOSE);
        assert_eq!(parts[2].size, 6);
        assert_eq!(parts[3].size, 5);
        // Same-length patterns remain distinct across suspended frame state.
        assert_eq!(parts[2].parent, 2);
        assert_eq!(parts[3].parent, 1);
        let (parts, headers) =
            parse(b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nX: y\r\n--x--").unwrap();
        assert_eq!(headers, 42 + 4); // Delimiter-leading CRLF belongs to the delimiter.
        assert_eq!(parts[1].body_start, parts[1].entity_end);
        assert_eq!(parts[1].size, 0);
    }
    #[test]
    fn exact_transfer_sizes_ignore_charset_and_diagnose_unknown_or_malformed() {
        for (source, size, flag) in [
            (
                b"Content-Transfer-Encoding: base64\n\nYQ==".as_slice(),
                1,
                0,
            ),
            (
                b"Content-Transfer-Encoding: base64\n\nYQ",
                1,
                ENCODING_PROBLEM,
            ),
            (
                b"Content-Transfer-Encoding: quoted-printable\n\na=20b  \r\n",
                5,
                0,
            ),
            (
                b"Content-Transfer-Encoding: quoted-printable\n\nx=",
                1,
                ENCODING_PROBLEM,
            ),
            (
                b"Content-Transfer-Encoding: custom\n\n\xff\0\r\n",
                4,
                UNKNOWN_ENCODING,
            ),
            (b"Content-Type: text/plain; charset=unknown\n\n\xff", 1, 0),
            (
                b"Content-Type: invalid;\nContent-Type: text/plain\n\nabc",
                3,
                0,
            ),
        ] {
            let (parts, _) = parse(source).unwrap();
            assert_eq!(parts[0].size, size);
            assert_eq!(parts[0].diagnostics, flag);
        }
        let run = format!(
            "Content-Transfer-Encoding: quoted-printable\n\na{}b{}\n",
            " ".repeat(5000),
            " ".repeat(5000)
        );
        let (parts, _) = parse(run.as_bytes()).unwrap();
        assert_eq!(parts[0].size, 5003);
        assert_eq!(parts[0].diagnostics, ENCODING_PROBLEM);
    }
    #[test]
    fn boundary_length_and_complete_head_invariants_refuse_input_cleanly() {
        for length in [0, 1, 69, 70, 71, 200] {
            let boundary = "x".repeat(length);
            let source=format!("Content-Type: multipart/mixed;boundary=\"{boundary}\"\n\n--{boundary}\n\na\n--{boundary}--");
            let result = parse(source.as_bytes());
            if (1..=70).contains(&length) {
                assert_eq!(result.unwrap().0.len(), 2);
            } else {
                assert_eq!(result, Err(Error::NotParsable));
            }
        }
        for source in [
            b"Content-Type: multipart\n\nraw".as_slice(),
            b"Content-Type: multipart/\n\nraw",
        ] {
            let (parts, _) = parse(source).unwrap();
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0].media, Media::TextPlain);
            assert_eq!(parts[0].size, 3);
        }
    }
    #[test]
    fn diagnostics_case_spelling_type_spans_and_incomplete_handoff() {
        for parameter in ["boundary=x;boundary*=utf-8''%zz", "boundary*=''x"] {
            let source = format!("Content-Type: multipart/mixed;{parameter}\n\n--x\n\na\n--x--");
            let (parts, _) = parse(source.as_bytes()).unwrap();
            assert_eq!(parts[0].diagnostics, PARAMETER_PROBLEM);
        }
        for label in ["7bit", "8bit", "binary"] {
            let source = format!("Content-Transfer-Encoding: {label}\n\nabc");
            let (parts, _) = parse(source.as_bytes()).unwrap();
            assert_eq!(parts[0].diagnostics, 0);
            assert_eq!(parts[0].size, 3);
            assert_eq!((parts[0].type_start, parts[0].type_end), (0, 0));
        }
        let source = concat!(
            "Content-Type: MULTIPART/Mixed;boundary=x\n",
            "Content-Transfer-Encoding: X-CUSTOM\n\n--x\n",
            "Content-Transfer-Encoding: BASE64\n\nYQ==\n--x--"
        )
        .as_bytes();
        let (parts, _) = parse(source).unwrap();
        assert_eq!(parts[0].media, Media::Multipart);
        assert_eq!(parts[0].diagnostics, UNKNOWN_ENCODING);
        assert_eq!(parts[1].size, 1);
        assert_eq!(parts[1].encoding, Encoding::Base64);
        let start = (parts[0].type_start - 17) as usize;
        let end = (parts[0].type_end - 17) as usize;
        assert_eq!(&source[start..end], b" MULTIPART/Mixed;boundary=x");
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidState)));
        // This isolates the largest header/parameter and maximal validation phases.
        let source = format!(
            concat!(
                "X: {}\nContent-Type: multipart/mixed;",
                " boundary*0*=utf-8''{}; boundary*1={}\n\n",
                "--{}\n\na\n--{}--"
            ),
            "a".repeat(8192),
            "x".repeat(35),
            "x".repeat(35),
            "x".repeat(70),
            "x".repeat(70)
        );
        assert_eq!(parse(source.as_bytes()).unwrap().0.len(), 2);
        // 32 counted octets followed by EOF can copy a cell in the same turn.
        let source = format!(
            "Content-Transfer-Encoding: base64\n\n{}YQ==",
            "YWJj".repeat(21)
        );
        assert_eq!(parse(source.as_bytes()).unwrap().0[0].size, 64);
    }
    #[test]
    fn original_work_and_live_deadline_cuts_hide_all_provisional_parts() {
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x--";
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut turns = 0;
        loop {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            turns += 1;
            let status = cursor.poll(Tick(1)).unwrap();
            let after = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            assert!(before.0.io_bytes - after.0.io_bytes <= 352);
            assert!(before.0.records - after.0.records <= 32);
            assert!(before.0.output_bytes - after.0.output_bytes <= 96);
            assert!(before.1 - after.1 <= 352);
            assert!(before.2 - after.2 <= 512);
            if status == Status::Complete {
                break;
            }
        }
        let left = cursor.work.remaining();
        let used = Charge {
            io_bytes: initial.io_bytes - left.io_bytes,
            records: initial.records - left.records,
            output_bytes: initial.output_bytes - left.output_bytes,
            ..Charge::default()
        };
        let identity = (
            std::ptr::from_ref(cursor.work),
            std::ptr::from_ref(cursor.budget),
        );
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.len(), 2);
        assert_eq!(
            identity,
            (std::ptr::from_ref(work), std::ptr::from_ref(budget))
        );
        for kind in 0..3 {
            let amount = match kind {
                0 => used.io_bytes,
                1 => used.records,
                _ => used.output_bytes,
            };
            for limit in 0..amount {
                let mut caps = used;
                let stop = match kind {
                    0 => {
                        caps.io_bytes = limit;
                        Stop::IoBytes
                    }
                    1 => {
                        caps.records = limit;
                        Stop::Records
                    }
                    _ => {
                        caps.output_bytes = limit;
                        Stop::OutputBytes
                    }
                };
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), caps);
                let mut budget = HeaderBudget::new();
                let mut cursor = Cursor::new(
                    source,
                    0,
                    SourceEnd::Eof,
                    Limits::default(),
                    &mut parts,
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                let error = drain(&mut cursor).unwrap_err();
                assert_eq!(error, Error::Work(stop));
                assert_eq!(cursor.parts(), Err(error));
                assert_eq!(cursor.header_bytes(), None);
                let before = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                );
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                assert_eq!(
                    before,
                    (
                        cursor.work.remaining(),
                        cursor.budget.source_bytes_remaining(),
                        cursor.budget.steps_remaining()
                    )
                );
                assert!(cursor.finish(Tick(1)).is_err());
            }
        }
        for cut in 0..=turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                source,
                0,
                SourceEnd::Eof,
                Limits::default(),
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            for _ in 0..cut {
                let _ = cursor.poll(Tick(1)).unwrap();
            }
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.parts(), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        assert!(matches!(
            cursor.finish(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        ));
    }
    #[test]
    fn structural_limits_ranges_and_caller_capacity_never_publish_prefixes() {
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\nX: y\n\nbody\n--x--";
        let mut parts = [Part::default(); 8];
        let header_total = b"Content-Type: multipart/mixed;boundary=x\n".len() + b"X: y\n".len();
        for (limits, capacity, expected) in [
            (
                Limits {
                    mime_depth: 1,
                    ..Limits::default()
                },
                8,
                Error::DepthLimit,
            ),
            (
                Limits {
                    mime_depth: 1,
                    mime_parts: 1,
                    ..Limits::default()
                },
                8,
                Error::DepthLimit,
            ),
            (
                Limits {
                    mime_depth: 2,
                    mime_parts: 2,
                    ..Limits::default()
                },
                1,
                Error::OutputCapacity,
            ),
            (Limits::default(), 0, Error::OutputCapacity),
            (
                Limits {
                    header_bytes: header_total - 1,
                    ..Limits::default()
                },
                8,
                Error::HeaderLimit,
            ),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                source,
                0,
                SourceEnd::Eof,
                limits,
                &mut parts[..capacity],
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor), Err(expected));
            assert_eq!(cursor.parts(), Err(expected));
        }
        {
            let source_end = SourceEnd::Prefix;
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            assert!(matches!(
                Cursor::new(
                    source,
                    0,
                    source_end,
                    Limits::default(),
                    &mut parts,
                    &mut work,
                    &mut budget
                ),
                Err(Error::IncompleteSource)
            ));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        assert!(matches!(
            Cursor::new(
                source,
                u64::MAX,
                SourceEnd::Eof,
                Limits::default(),
                &mut parts,
                &mut work,
                &mut budget
            ),
            Err(Error::InvalidRange)
        ));
        for limit in [0, 65] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let limits = Limits {
                mime_depth: limit,
                ..Limits::default()
            };
            assert!(matches!(
                Cursor::new(
                    source,
                    0,
                    SourceEnd::Eof,
                    limits,
                    &mut parts,
                    &mut work,
                    &mut budget
                ),
                Err(Error::InvalidLimits)
            ));
        }
    }
    #[test]
    fn aggregate_interpretation_and_entity_headers_never_reset() {
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\nX: y\n\na\n--x--";
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let used = (
            initial.0 - cursor.budget.source_bytes_remaining(),
            initial.1 - cursor.budget.steps_remaining(),
        );
        let total = cursor.header_bytes().unwrap();
        let _ = cursor.finish(Tick(1)).unwrap();
        for bytes in [true, false] {
            let amount = if bytes { used.0 } else { used.1 };
            for limit in 0..amount {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut credit = 0;
                let (visits, steps) = if bytes {
                    (budget.source_bytes_remaining() - limit, 0)
                } else {
                    (0, budget.steps_remaining() - limit)
                };
                budget
                    .charge_local(&mut work, Tick(1), visits, steps, &mut credit)
                    .unwrap();
                let mut cursor = Cursor::new(
                    source,
                    0,
                    SourceEnd::Eof,
                    Limits::default(),
                    &mut parts,
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                assert_eq!(drain(&mut cursor), Err(Error::InterpretationLimit));
                assert_eq!(cursor.parts(), Err(Error::InterpretationLimit));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            }
        }
        for headers in 1..=total {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let limits = Limits {
                header_bytes: headers as usize,
                ..Limits::default()
            };
            let mut cursor = Cursor::new(
                source,
                0,
                SourceEnd::Eof,
                limits,
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            if headers < total {
                assert_eq!(drain(&mut cursor), Err(Error::HeaderLimit));
                assert_eq!(cursor.parts(), Err(Error::HeaderLimit));
            } else {
                drain(&mut cursor).unwrap();
                assert_eq!(cursor.header_bytes(), Some(total));
            }
        }
        let source = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x\n\nb\n--x--";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let limits = Limits {
            mime_depth: 2,
            mime_parts: 2,
            ..Limits::default()
        };
        let mut cursor = Cursor::new(
            source,
            0,
            SourceEnd::Eof,
            limits,
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::PartLimit));
        assert_eq!(cursor.parts(), Err(Error::PartLimit));
    }
    #[test]
    fn maximum_explicit_depth_and_descriptor_count_stay_bounded() {
        let mut source = String::new();
        for depth in 0..63 {
            source.push_str(&format!(
                "Content-Type: multipart/mixed;boundary=b{depth:02}\n\n--b{depth:02}\n"
            ));
        }
        source.push_str("\nleaf");
        for depth in (0..63).rev() {
            source.push_str(&format!("\n--b{depth:02}--"));
        }
        let limits = Limits {
            mime_depth: 64,
            mime_parts: 4096,
            ..Limits::default()
        };
        let mut parts = vec![Part::default(); 4096];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            source.as_bytes(),
            0,
            SourceEnd::Eof,
            limits,
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let selected = cursor.parts().unwrap().unwrap();
        assert_eq!(selected.len(), 64);
        assert_eq!(selected[63].depth, 64);
        assert_eq!(selected[63].size, 4);
        let _ = cursor.finish(Tick(1)).unwrap();
        let mut wide = String::from("Content-Type: multipart/mixed;boundary=x\n\n");
        for _ in 0..4095 {
            wide.push_str("--x\n\na\n");
        }
        wide.push_str("--x--");
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            wide.as_bytes(),
            0,
            SourceEnd::Eof,
            limits,
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let selected = cursor.parts().unwrap().unwrap();
        assert_eq!(selected.len(), 4096);
        assert_eq!(selected[4095].ordinal, 4096);
        assert_eq!(selected[4095].size, 1);
    }
}
