//! Complete resident Content-Language value syntax with provisional tag extents.
//! RFC 3282 section 2 ABNF with RFC 3066 section 2.1 tag spelling.
use crate::{cfws, language_tag::Tag, Charge, Work};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Tag(Extent),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    NestingLimit,
    Work(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed Content-Language value"),
            Self::NestingLimit => f.write_str("Content-Language comment nesting limit"),
            Self::Work(error) => write!(f, "Content-Language work: {error}"),
            Self::InvalidState => f.write_str("invalid Content-Language state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
impl<E> From<cfws::Error<E>> for Error<E> {
    fn from(error: cfws::Error<E>) -> Self {
        match error {
            cfws::Error::Malformed => Self::Malformed,
            cfws::Error::NestingLimit => Self::NestingLimit,
            cfws::Error::Work(error) => Self::Work(error),
            cfws::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Before,
    Tag,
    After,
    Separator,
    Complete,
}
/// Source is one complete field value, excluding its final line ending.
/// Every tag retires if full-list validation or enclosing admission fails.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_header::language_list::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_header::language_list::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    position: usize,
    start: usize,
    tag: Tag,
    cfws: Option<cfws::Cursor<'a, E>>,
    phase: Phase,
    failure: Option<Error<E>>,
}
impl<'a, E: Copy> Cursor<'a, E> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            position: 0,
            start: 0,
            tag: Tag::new(),
            cfws: Some(cfws::Cursor::new(source, 0)),
            phase: Phase::Before,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && matches!(self.phase, Phase::Complete)
    }
    pub fn check_work(&mut self, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(Charge::default()).map_err(Error::Work);
        self.outcome(result)
    }
    fn outcome<T>(&mut self, result: Result<T, Error<E>>) -> Result<T, Error<E>> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(work);
        self.outcome(result)
    }
    fn peek(&self, work: &mut impl Work<Error = E>) -> Result<Option<u8>, Error<E>> {
        work.charge(Charge {
            visits: u64::from(self.position < self.source.len()),
            records: 1,
        })
        .map_err(Error::Work)?;
        Ok(self.source.get(self.position).copied())
    }
    fn step(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        match self.phase {
            Phase::Before | Phase::After => {
                let cursor = self.cfws.as_mut().ok_or(Error::InvalidState)?;
                if let cfws::Status::Complete(end) = cursor.poll(work)? {
                    self.position = end.position;
                    self.cfws = None;
                    if matches!(self.phase, Phase::Before) {
                        self.start = self.position;
                        self.tag = Tag::new();
                        self.phase = Phase::Tag;
                    } else {
                        self.phase = Phase::Separator;
                    }
                }
                Ok(Status::Yield)
            }
            Phase::Tag => {
                if let Some(byte) = self.peek(work)? {
                    if byte.is_ascii_alphanumeric() || byte == b'-' {
                        if !self.tag.feed(byte) {
                            return Err(Error::Malformed);
                        }
                        self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                        return Ok(Status::Yield);
                    }
                }
                if !self.tag.is_complete() {
                    return Err(Error::Malformed);
                }
                // CFWS funds its own delimiter or EOF reread.
                self.phase = Phase::After;
                self.cfws = Some(cfws::Cursor::new(self.source, self.position));
                Ok(Status::Tag(Extent {
                    start: self.start,
                    end: self.position,
                }))
            }
            Phase::Separator => match self.peek(work)? {
                None => {
                    self.phase = Phase::Complete;
                    Ok(Status::Complete)
                }
                Some(b',') => {
                    self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                    self.phase = Phase::Before;
                    self.cfws = Some(cfws::Cursor::new(self.source, self.position));
                    Ok(Status::Yield)
                }
                Some(_) => Err(Error::Malformed),
            },
            Phase::Complete => Ok(Status::Complete),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Counting {
        calls: usize,
        visits: u64,
        records: u64,
        fail: Option<usize>,
    }
    impl Work for Counting {
        type Error = u8;
        fn charge(&mut self, charge: Charge) -> Result<(), u8> {
            if self.fail == Some(self.calls) {
                return Err(77);
            }
            self.calls += 1;
            self.visits += charge.visits;
            self.records += charge.records;
            Ok(())
        }
    }
    fn drain<'a>(
        cursor: &mut Cursor<'a, u8>,
        source: &'a [u8],
        work: &mut Counting,
    ) -> (Vec<&'a [u8]>, Result<(), Error<u8>>) {
        let mut tags = Vec::new();
        assert!(std::mem::size_of_val(cursor) <= 192);
        for _ in 0..100_000 {
            let before = (work.visits, work.records);
            let status = cursor.poll(work);
            assert!(work.visits - before.0 <= 160);
            assert!(work.records - before.1 <= 32);
            match status {
                Ok(Status::Yield) => assert!(!cursor.is_complete()),
                Ok(Status::Tag(extent)) => {
                    tags.push(source.get(extent.start..extent.end).unwrap());
                    assert!(!cursor.is_complete());
                }
                Ok(Status::Complete) => {
                    assert!(cursor.is_complete());
                    let calls = work.calls;
                    assert_eq!(cursor.poll(work), Ok(Status::Complete));
                    assert_eq!(work.calls, calls);
                    return (tags, Ok(()));
                }
                Err(error) => {
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(&mut Counting::default()), Err(error));
                    return (tags, Err(error));
                }
            }
        }
        panic!("language list did not finish")
    }
    #[test]
    fn rfc_examples_comments_folds_duplicates_and_original_case() {
        for (source, expected) in [
            (b"en-scouse".as_slice(), vec![b"en-scouse".as_slice()]),
            (b"i-mingo", vec![b"i-mingo".as_slice()]),
            (
                b"en, fr (This is a dictionary)",
                vec![b"en".as_slice(), b"fr"],
            ),
            (
                b"da, de, el, en, fr, it",
                vec![b"da".as_slice(), b"de", b"el", b"en", b"fr", b"it"],
            ),
            (
                b"(before) EN (after),\r\n\t en, x-Ab12",
                vec![b"EN".as_slice(), b"en", b"x-Ab12"],
            ),
            (
                "(é🐈)abcdefgh-12345678".as_bytes(),
                vec![b"abcdefgh-12345678".as_slice()],
            ),
        ] {
            assert_eq!(
                drain(&mut Cursor::new(source), source, &mut Counting::default()),
                (expected, Ok(()))
            );
        }
    }
    #[test]
    fn incomplete_or_malformed_tail_retires_all_provisional_tags() {
        for source in [
            b"".as_slice(),
            b" ",
            b",en",
            b"en,",
            b"en,,fr",
            b"en fr",
            b"en (x) fr",
            b"*",
            b"1en",
            b"en-",
            b"en--US",
            b"abcdefghi",
            b"en-123456789",
            b"en_US",
            b"en;q=1",
            b"en,\xff",
            b"en (bad",
            b"en (\\",
            b"en\r\nfr",
            b"en (\xff)",
        ] {
            assert_eq!(
                drain(&mut Cursor::new(source), source, &mut Counting::default()).1,
                Err(Error::Malformed),
                "{source:?}"
            );
        }
        let nested = format!("en {}x{}", "(".repeat(33), ")".repeat(33));
        assert_eq!(
            drain(
                &mut Cursor::new(nested.as_bytes()),
                nested.as_bytes(),
                &mut Counting::default()
            )
            .1,
            Err(Error::NestingLimit)
        );
    }
    #[test]
    fn every_work_cut_and_explicit_late_admission_stay_failed() {
        let source = b"(x) en-US, fr (tail)";
        let mut healthy = Counting::default();
        let expected = drain(&mut Cursor::new(source), source, &mut healthy);
        assert_eq!(expected.1, Ok(()));
        for cut in 0..healthy.calls {
            let mut work = Counting {
                fail: Some(cut),
                ..Counting::default()
            };
            let mut cursor = Cursor::new(source);
            let (tags, result) = drain(&mut cursor, source, &mut work);
            assert_eq!(result, Err(Error::Work(77)));
            assert!(expected.0.starts_with(&tags));
            assert_eq!(
                cursor.check_work(&mut Counting::default()),
                Err(Error::Work(77))
            );
        }
        let mut cursor = Cursor::new(source);
        assert_eq!(
            drain(&mut cursor, source, &mut Counting::default()).1,
            Ok(())
        );
        assert_eq!(
            cursor.check_work(&mut Counting {
                fail: Some(0),
                ..Counting::default()
            }),
            Err(Error::Work(77))
        );
        assert!(!cursor.is_complete());
        assert_eq!(cursor.poll(&mut Counting::default()), Err(Error::Work(77)));
    }
    #[test]
    fn long_tags_and_comments_keep_fixed_state_and_turns() {
        let source = format!("({})x{}", "🐈".repeat(4096), "-abcdefgh".repeat(4096));
        let (tags, result) = drain(
            &mut Cursor::new(source.as_bytes()),
            source.as_bytes(),
            &mut Counting::default(),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].len(), 1 + 9 * 4096);
    }
}
