//! Provisional MIME parameter octets; selection and charset conversion are external.
use crate::{delimited, language_tag, mime_token_octet, Charge, Work};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Ordinary,
    ExtendedInitial,
    ExtendedContinuation,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Charset,
    Language,
    Data,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Octet { role: Role, value: u8 },
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    Work(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed MIME parameter value"),
            Self::Work(e) => write!(f, "MIME value work: {e}"),
            Self::InvalidState => f.write_str("invalid MIME value state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
#[derive(Clone, Copy)]
enum Phase {
    Validate,
    Project,
    Complete,
}
/// One complete raw token or quoted value, with no field-selection authority.
/// Events are provisional until Complete and may not be published after failure.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_header::mime_value::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_header::mime_value::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    mode: Mode,
    quoted: bool,
    phase: Phase,
    position: usize,
    end: usize,
    role: Role,
    language: language_tag::Tag,
    delimited: Option<delimited::Cursor<'a, E>>,
    failure: Option<Error<E>>,
}

fn attribute(b: u8) -> bool {
    mime_token_octet(b) && !matches!(b, b'\'' | b'%' | b'*')
}
fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
impl<'a, E: Copy> Cursor<'a, E> {
    #[must_use]
    pub const fn new(source: &'a [u8], quoted: bool, mode: Mode) -> Self {
        Self {
            source,
            mode,
            quoted,
            phase: Phase::Validate,
            position: 0,
            end: source.len(),
            role: match mode {
                Mode::ExtendedInitial => Role::Charset,
                _ => Role::Data,
            },
            language: language_tag::Tag::new(),
            delimited: if quoted {
                Some(delimited::Cursor::new(
                    source,
                    0,
                    delimited::Kind::QuotedString,
                ))
            } else {
                None
            },
            failure: None,
        }
    }
    pub fn check_work(&mut self, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = work.charge(Charge::default()).map_err(Error::Work);
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    pub fn poll(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(work);
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn byte(&self, at: usize, work: &mut impl Work<Error = E>) -> Result<Option<u8>, Error<E>> {
        if at > self.end {
            return Err(Error::InvalidState);
        }
        work.charge(Charge {
            visits: u64::from(at < self.end),
            records: 0,
        })
        .map_err(Error::Work)?;
        if at == self.end {
            Ok(None)
        } else {
            Ok(self.source.get(at).copied())
        }
    }
    fn atom(
        &self,
        at: usize,
        work: &mut impl Work<Error = E>,
    ) -> Result<Option<(u8, usize)>, Error<E>> {
        crate::projection::atom(at, self.quoted, |at| self.byte(at, work))
            .map(|atom| atom.map(|atom| (atom.value, atom.next)))
            .map_err(|error| match error {
                crate::projection::Error::Read(error) => error,
                crate::projection::Error::IncompletePair => Error::Malformed,
                crate::projection::Error::InvalidState => Error::InvalidState,
            })
    }
    fn step(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        match self.phase {
            Phase::Validate => {
                if let Some(child) = self.delimited.as_mut() {
                    let status = child.poll(work).map_err(|e| match e {
                        delimited::Error::Malformed => Error::Malformed,
                        delimited::Error::Work(e) => Error::Work(e),
                        delimited::Error::InvalidState => Error::InvalidState,
                    })?;
                    if let delimited::Status::Complete(extent) = status {
                        if extent.end != self.source.len() {
                            return Err(Error::Malformed);
                        }
                        self.position = 1;
                        self.end = extent.end.checked_sub(1).ok_or(Error::InvalidState)?;
                        self.delimited = None;
                        self.phase = Phase::Project;
                    }
                } else {
                    for _ in 0..32 {
                        work.charge(Charge {
                            visits: 0,
                            records: 1,
                        })
                        .map_err(Error::Work)?;
                        match self.byte(self.position, work)? {
                            Some(b) if mime_token_octet(b) => {
                                self.position =
                                    self.position.checked_add(1).ok_or(Error::InvalidState)?
                            }
                            Some(_) => return Err(Error::Malformed),
                            None => {
                                if self.position == 0 {
                                    return Err(Error::Malformed);
                                }
                                self.position = 0;
                                self.phase = Phase::Project;
                                break;
                            }
                        }
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Project => {
                work.charge(Charge {
                    visits: 0,
                    records: 1,
                })
                .map_err(Error::Work)?;
                let Some((mut value, mut next)) = self.atom(self.position, work)? else {
                    if self.role != Role::Data {
                        return Err(Error::Malformed);
                    }
                    self.phase = Phase::Complete;
                    return Ok(Status::Complete);
                };
                if self.mode != Mode::Ordinary {
                    match self.role {
                        Role::Charset | Role::Language => {
                            if value == b'\'' {
                                if self.role == Role::Language
                                    && !self.language.is_empty()
                                    && !self.language.is_complete()
                                {
                                    return Err(Error::Malformed);
                                }
                                self.role = if self.role == Role::Charset {
                                    Role::Language
                                } else {
                                    Role::Data
                                };
                                self.position = next;
                                return Ok(Status::Yield);
                            }
                            if self.role == Role::Charset {
                                if !attribute(value) {
                                    return Err(Error::Malformed);
                                }
                            } else if !self.language.feed(value) {
                                return Err(Error::Malformed);
                            }
                        }
                        Role::Data if value == b'%' => {
                            let (a, end) = self.atom(next, work)?.ok_or(Error::Malformed)?;
                            let (b, end) = self.atom(end, work)?.ok_or(Error::Malformed)?;
                            value = hex(a)
                                .and_then(|a| hex(b).map(|b| (a << 4) | b))
                                .ok_or(Error::Malformed)?;
                            next = end;
                        }
                        Role::Data if !attribute(value) => return Err(Error::Malformed),
                        _ => {}
                    }
                }
                self.position = next;
                Ok(Status::Octet {
                    role: self.role,
                    value,
                })
            }
            Phase::Complete => Ok(Status::Complete),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Counter {
        visits: u64,
        records: u64,
        cut: Option<u64>,
    }
    impl Work for Counter {
        type Error = u8;
        fn charge(&mut self, c: Charge) -> Result<(), u8> {
            if self.cut.is_some_and(|n| self.visits + self.records >= n) {
                return Err(9);
            }
            self.visits += c.visits;
            self.records += c.records;
            Ok(())
        }
    }
    fn drain(source: &[u8], quoted: bool, mode: Mode) -> Result<[Vec<u8>; 3], Error<u8>> {
        let mut cursor = Cursor::new(source, quoted, mode);
        let mut work = Counter::default();
        let mut result = [Vec::new(), Vec::new(), Vec::new()];
        assert!(std::mem::size_of_val(&cursor) <= 160);
        for _ in 0..10000 {
            let before = (work.visits, work.records);
            let status = cursor.poll(&mut work)?;
            assert!(work.visits - before.0 <= 160);
            assert!(work.records - before.1 <= 32);
            match status {
                Status::Yield => {}
                Status::Octet { role, value } => result[match role {
                    Role::Charset => 0,
                    Role::Language => 1,
                    Role::Data => 2,
                }]
                .push(value),
                Status::Complete => {
                    let before = (work.visits, work.records);
                    work.cut = Some(0);
                    assert_eq!(cursor.poll(&mut work), Ok(Status::Complete));
                    assert_eq!(before, (work.visits, work.records));
                    return Ok(result);
                }
            }
        }
        panic!("not complete")
    }
    #[test]
    fn ordinary_values_preserve_octets_unquote_and_unfold() {
        for (source, quoted, expected) in [
            (b"plain".as_slice(), false, b"plain".as_slice()),
            (b"\"\"", true, b""),
            (b"\"a\\\0b\"", true, b"a\0b"),
            (b"\"a\\\"b\\\\c\"", true, b"a\"b\\c"),
            (b"\"a\r\n \tb\n\tc\"", true, b"a \tb\tc"),
            (b"\"a\\\r\\\n\\ b\"", true, b"a b"),
            (b"\"a\\\r\\\nb\"", true, b"a\r\nb"),
            (b"\"a\\\nb\"", true, b"a\nb"),
            ("\"café\"".as_bytes(), true, "café".as_bytes()),
            (
                b"\"%00=?utf-8?Q?literal?=\"",
                true,
                b"%00=?utf-8?Q?literal?=",
            ),
        ] {
            assert_eq!(
                drain(source, quoted, Mode::Ordinary).unwrap(),
                [Vec::new(), Vec::new(), expected.to_vec()]
            )
        }
    }
    #[test]
    fn extended_prefix_and_percent_are_distinct_from_charset_decoding() {
        for (source, quoted, mode, expected) in [
            (
                b"UTF-8'en-US'%E2%82%AC%00".as_slice(),
                false,
                Mode::ExtendedInitial,
                [b"UTF-8".as_slice(), b"en-US", b"\xe2\x82\xac\0"],
            ),
            (
                b"\"''a%20b\"",
                true,
                Mode::ExtendedInitial,
                [b"".as_slice(), b"", b"a b"],
            ),
            (
                b"\"utf-8''a\\%32%33\"",
                true,
                Mode::ExtendedInitial,
                [b"utf-8".as_slice(), b"", b"a23"],
            ),
            (
                b"%ff%a0",
                false,
                Mode::ExtendedContinuation,
                [b"".as_slice(), b"", b"\xff\xa0"],
            ),
        ] {
            assert_eq!(
                drain(source, quoted, mode).unwrap(),
                expected.map(Vec::from)
            )
        }
        assert_eq!(
            drain(b"''%3D%3Futf-8%3FQ%3Fx%3F%3D", false, Mode::ExtendedInitial).unwrap(),
            [Vec::new(), Vec::new(), b"=?utf-8?Q?x?=".to_vec()]
        );
        for value in 0..=255u8 {
            let source = format!("%{value:02X}");
            assert_eq!(
                drain(source.as_bytes(), false, Mode::ExtendedContinuation).unwrap()[2],
                vec![value]
            );
        }
    }
    #[test]
    fn malformed_tail_or_prefix_never_completes() {
        for (source, quoted, mode) in [
            (b"".as_slice(), false, Mode::Ordinary),
            (b"a b", false, Mode::Ordinary),
            (b"\"a\"tail", true, Mode::Ordinary),
            (b"\"\xff\"", true, Mode::Ordinary),
            (b"\"a\r\nb\"", true, Mode::Ordinary),
            (b"utf-8", false, Mode::ExtendedInitial),
            (b"utf-8'", false, Mode::ExtendedInitial),
            (b"utf-8'en_US'a", false, Mode::ExtendedInitial),
            (b"utf-8'-en'a", false, Mode::ExtendedInitial),
            (b"utf-8'en-'a", false, Mode::ExtendedInitial),
            (b"utf-8'1'a", false, Mode::ExtendedInitial),
            (b"utf-8'abcdefghi'a", false, Mode::ExtendedInitial),
            (b"utf-8'en--US'a", false, Mode::ExtendedInitial),
            (b"utf%38''a", false, Mode::ExtendedInitial),
            (b"abc%", false, Mode::ExtendedContinuation),
            (b"abc%2", false, Mode::ExtendedContinuation),
            (b"abc%2x", false, Mode::ExtendedContinuation),
            (b"utf-8'en'a'b", false, Mode::ExtendedInitial),
            (b"a'b", false, Mode::ExtendedContinuation),
            (b"a*b", false, Mode::ExtendedContinuation),
            (b"\"a b\"", true, Mode::ExtendedContinuation),
            ("\"é\"".as_bytes(), true, Mode::ExtendedContinuation),
        ] {
            assert_eq!(
                drain(source, quoted, mode),
                Err(Error::Malformed),
                "{source:?}"
            )
        }
    }
    #[test]
    fn every_callback_cut_and_late_admission_stays_failed() {
        let source = b"\"utf-8'en'%E2%82%AC\"";
        let mut cursor = Cursor::new(source, true, Mode::ExtendedInitial);
        let mut full = Counter::default();
        while cursor.poll(&mut full).unwrap() != Status::Complete {}
        let amount = full.visits + full.records;
        for cut in 0..=amount {
            let mut cursor = Cursor::new(source, true, Mode::ExtendedInitial);
            let mut work = Counter {
                cut: Some(cut),
                ..Counter::default()
            };
            loop {
                match cursor.poll(&mut work) {
                    Ok(Status::Complete) => panic!("cut admitted completion"),
                    Ok(_) => {}
                    Err(e) => {
                        assert_eq!(e, Error::Work(9));
                        let mut fresh = Counter::default();
                        assert_eq!(cursor.poll(&mut fresh), Err(e));
                        assert_eq!(cursor.check_work(&mut fresh), Err(e));
                        assert_eq!((fresh.visits, fresh.records), (0, 0));
                        break;
                    }
                }
            }
        }
        full.cut = Some(0);
        assert_eq!(cursor.check_work(&mut full), Err(Error::Work(9)));
        assert_eq!(cursor.poll(&mut Counter::default()), Err(Error::Work(9)));
    }
    #[test]
    fn long_values_keep_fixed_turns_without_trimming_or_prefix_success() {
        for (source, pattern, expected) in [
            (
                format!("\"{}\"", "a".repeat(64 * 1024)),
                b"a".as_slice(),
                64 * 1024,
            ),
            (
                format!("\"{}\"", "🐈".repeat(10_000)),
                "🐈".as_bytes(),
                40_000,
            ),
        ] {
            let mut cursor = Cursor::new(source.as_bytes(), true, Mode::Ordinary);
            let mut work = Counter::default();
            let mut count = 0;
            loop {
                let before = (work.visits, work.records);
                let status = cursor.poll(&mut work).unwrap();
                assert!(work.visits - before.0 <= 160);
                assert!(work.records - before.1 <= 32);
                match status {
                    Status::Octet {
                        role: Role::Data,
                        value,
                    } => {
                        assert_eq!(value, pattern[count % pattern.len()]);
                        count += 1;
                    }
                    Status::Yield => {}
                    Status::Complete => {
                        assert_eq!(count, expected);
                        break;
                    }
                    _ => panic!("unexpected event"),
                }
            }
        }
        let source = format!("{}%", "a".repeat(4096));
        assert_eq!(
            drain(source.as_bytes(), false, Mode::ExtendedContinuation),
            Err(Error::Malformed)
        );
    }
}
