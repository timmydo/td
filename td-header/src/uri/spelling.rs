//! Optional surrounding CFWS for a caller-authorized URI field-value slice.
//! Selection grants no URI, folding, word-placement or publication validity.
use super::unfold;
use crate::{cfws, Charge, Work};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spelling {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(Spelling),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    NestingLimit,
    Work(E),
    InvalidState,
}
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
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed leading URI CFWS"),
            Self::NestingLimit => f.write_str("leading URI comment nesting limit"),
            Self::Work(error) => write!(f, "URI spelling work: {error}"),
            Self::InvalidState => f.write_str("invalid URI spelling state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
#[derive(Clone, Copy)]
enum Phase {
    Leading,
    Scan,
    Literal,
    Probe,
    Tail,
    Complete,
}
enum Child<'a, E: Copy> {
    Cfws(cfws::Cursor<'a, E>),
    Probe(unfold::Cursor<'a, E>),
    Empty,
}
/// Caller authorizes greedy leading CFWS and terminal CFWS beginning with WSP
/// or a fold. After leading CFWS, adjoining URI parentheses stay literal. Failed optional
/// suffix grammar stays in the selected spelling; work refusal never falls back.
/// Empty spelling is returned for caller presence policy. Final ending excluded.
/// ```compile_fail,E0277
/// fn copied<T:Copy>() {} copied::<td_header::uri::spelling::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T:Clone>() {} cloned::<td_header::uri::spelling::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    child: Child<'a, E>,
    start: usize,
    position: usize,
    candidate: usize,
    retry: usize,
    end: usize,
    phase: Phase,
    failure: Option<Error<E>>,
}
impl<'a, E: Copy> Cursor<'a, E> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            child: Child::Cfws(cfws::Cursor::new(source, 0)),
            start: 0,
            position: 0,
            candidate: 0,
            retry: 0,
            end: source.len(),
            phase: Phase::Leading,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && matches!(self.phase, Phase::Complete)
    }
    fn outcome<T>(&mut self, result: Result<T, Error<E>>) -> Result<T, Error<E>> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_work(&mut self, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(Charge::default()).map_err(Error::Work);
        self.outcome(result)
    }
    pub fn poll(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.spelling()));
        }
        let result = self.step(work);
        self.outcome(result)
    }
    fn spelling(&self) -> Spelling {
        Spelling {
            start: self.start,
            end: self.end,
        }
    }
    fn complete(&mut self) -> Status {
        self.child = Child::Empty;
        self.phase = Phase::Complete;
        Status::Complete(self.spelling())
    }
    fn scan_from(&mut self, position: usize) -> Result<(), Error<E>> {
        if position > self.source.len() {
            return Err(Error::InvalidState);
        }
        self.position = position;
        self.child = Child::Empty;
        self.phase = Phase::Scan;
        Ok(())
    }
    fn step(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        match self.phase {
            Phase::Leading => {
                let Child::Cfws(cursor) = &mut self.child else {
                    return Err(Error::InvalidState);
                };
                match cursor.poll(work)? {
                    cfws::Status::Yield | cfws::Status::Comment(_) => Ok(Status::Yield),
                    cfws::Status::Complete(end) => {
                        self.start = end.position;
                        self.scan_from(end.position)?;
                        Ok(Status::Yield)
                    }
                }
            }
            Phase::Scan | Phase::Literal => {
                work.charge(Charge {
                    visits: u64::from(self.position < self.source.len()),
                    records: 1,
                })
                .map_err(Error::Work)?;
                let Some(byte) = self.source.get(self.position).copied() else {
                    if self.position != self.source.len() {
                        return Err(Error::InvalidState);
                    }
                    return Ok(self.complete());
                };
                if matches!(self.phase, Phase::Scan) && matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
                {
                    self.candidate = self.position;
                    self.retry = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                    let source = self
                        .source
                        .get(self.position..)
                        .ok_or(Error::InvalidState)?;
                    // Learn the whitespace end before retrying a failed comment suffix.
                    self.child = Child::Probe(unfold::Cursor::new(source));
                    self.phase = Phase::Probe;
                } else {
                    self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                }
                Ok(Status::Yield)
            }
            Phase::Probe => {
                let Child::Probe(cursor) = &mut self.child else {
                    return Err(Error::InvalidState);
                };
                match cursor.poll(work) {
                    Ok(unfold::Status::Yield) => {}
                    Ok(unfold::Status::Complete) => {
                        self.end = self.candidate;
                        return Ok(self.complete());
                    }
                    Ok(unfold::Status::Octet { byte, position }) => {
                        self.retry = self
                            .candidate
                            .checked_add(position)
                            .ok_or(Error::InvalidState)?;
                        if byte == b'(' {
                            self.child =
                                Child::Cfws(cfws::Cursor::new(self.source, self.candidate));
                            self.phase = Phase::Tail;
                        } else {
                            self.scan_from(self.retry)?;
                        }
                    }
                    Err(unfold::Error::Malformed) => {
                        self.scan_from(self.retry)?;
                        self.phase = Phase::Literal;
                    }
                    Err(unfold::Error::Work(error)) => return Err(Error::Work(error)),
                    Err(unfold::Error::InvalidState) => return Err(Error::InvalidState),
                }
                Ok(Status::Yield)
            }
            Phase::Tail => {
                let Child::Cfws(cursor) = &mut self.child else {
                    return Err(Error::InvalidState);
                };
                match cursor.poll(work) {
                    Ok(cfws::Status::Yield | cfws::Status::Comment(_)) => {}
                    Ok(cfws::Status::Complete(end)) if end.position == self.source.len() => {
                        self.end = self.candidate;
                        return Ok(self.complete());
                    }
                    Ok(cfws::Status::Complete(end)) => self.scan_from(end.position)?,
                    Err(cfws::Error::Malformed | cfws::Error::NestingLimit) => {
                        self.scan_from(self.retry)?;
                        // Inner whitespace must not create a suffix in a failed comment.
                        self.phase = Phase::Literal;
                    }
                    Err(error) => return Err(Error::from(error)),
                }
                Ok(Status::Yield)
            }
            Phase::Complete => Err(Error::InvalidState),
        }
    }
    pub fn finish(self) -> Result<Spelling, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        Ok(self.spelling())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[derive(Default)]
    struct Budget {
        calls: usize,
        cut: Option<usize>,
        visits: u64,
        records: u64,
    }
    impl Work for Budget {
        type Error = u8;
        fn charge(&mut self, charge: Charge) -> Result<(), u8> {
            let call = self.calls;
            self.calls += 1;
            if self.cut == Some(call) {
                return Err(77);
            }
            self.visits += charge.visits;
            self.records += charge.records;
            Ok(())
        }
    }
    fn drain(
        cursor: &mut Cursor<'_, u8>,
        work: &mut Budget,
    ) -> Result<(Spelling, usize), Error<u8>> {
        for turn in 1..1_000_000 {
            let (visits, records) = (work.visits, work.records);
            let result = cursor.poll(work);
            assert!(work.visits - visits <= 160);
            assert!(work.records - records <= 32);
            match result? {
                Status::Yield => assert!(!cursor.is_complete()),
                Status::Complete(spelling) => {
                    assert!(cursor.is_complete());
                    let calls = work.calls;
                    assert_eq!(cursor.poll(work), Ok(Status::Complete(spelling)));
                    assert_eq!(work.calls, calls);
                    return Ok((spelling, turn));
                }
            }
        }
        panic!("spelling selector did not finish")
    }
    #[test]
    fn explicit_surrounding_cfws_preference_preserves_other_parentheses() {
        assert!(std::mem::size_of::<Cursor<'_, u8>>() <= 160);
        for (source, wanted) in [
            ("", ""),
            (" (only)\t", ""),
            ("(lead) ../a(b) (tail)", "../a(b)"),
            ("a(b)", "a(b)"),
            ("(x)a", "a"),
            ("(x)a(y)", "a(y)"),
            ("a\r (b)", "a\r (b)"),
            ("a (b) c (tail)", "a (b) c"),
            ("a (bad", "a (bad"),
            ("a (bad ", "a (bad "),
            ("a (bad (tail)", "a (bad (tail)"),
            ("a (b) (c)", "a"),
            ("http://example.com/a   ", "http://example.com/a"),
            ("a\r\n ", "a"),
            ("a\r\n b", "a\r\n b"),
            ("a\n\t(b)", "a"),
            ("(é) =?ascii?Q?a(\r\n b)?= (tail)", "=?ascii?Q?a(\r\n b)?="),
            ("a\r\n", "a\r\n"),
        ] {
            let mut cursor = Cursor::new(source.as_bytes());
            let mut work = Budget::default();
            let (spelling, _) = drain(&mut cursor, &mut work).unwrap();
            assert_eq!(source.get(spelling.start..spelling.end), Some(wanted));
            assert_eq!(cursor.finish(), Ok(spelling));
        }
        let deep = format!("a {}x{}", "(".repeat(33), ")".repeat(33));
        let malformed = format!("a {}x{} (tail)", "(".repeat(33), ")".repeat(32));
        let mut cursor = Cursor::new(malformed.as_bytes());
        let (spelling, _) = drain(&mut cursor, &mut Budget::default()).unwrap();
        assert_eq!(
            spelling,
            Spelling {
                start: 0,
                end: malformed.len()
            }
        );
        let mut cursor = Cursor::new(deep.as_bytes());
        let (spelling, _) = drain(&mut cursor, &mut Budget::default()).unwrap();
        assert_eq!(deep.get(spelling.start..spelling.end), Some(deep.as_str()));
    }
    #[test]
    fn leading_cfws_errors_and_nonfolds_remain_distinct() {
        let deep = format!("{}x{}a", "(".repeat(33), ")".repeat(33));
        for (source, error) in [
            (b"(bad".as_slice(), Error::Malformed),
            (b"(bad\\", Error::Malformed),
            (b"(\xff)a", Error::Malformed),
            (b"(a\nX)b", Error::Malformed),
            (deep.as_bytes(), Error::NestingLimit),
        ] {
            let mut cursor = Cursor::new(source);
            let mut work = Budget::default();
            assert_eq!(drain(&mut cursor, &mut work), Err(error));
            let calls = work.calls;
            assert_eq!(cursor.poll(&mut work), Err(error));
            assert_eq!(cursor.check_work(&mut work), Err(error));
            assert_eq!(work.calls, calls);
            assert_eq!(cursor.finish(), Err(error));
        }
    }
    #[test]
    fn every_callback_cut_and_fresh_completion_refusal_are_sticky() {
        for source in [
            b"(lead) a (b) c (tail)".as_slice(),
            b"a (bad",
            b"a (bad (tail)",
            b"a\r\nX",
        ] {
            let mut cursor = Cursor::new(source);
            let mut baseline = Budget::default();
            let (spelling, _) = drain(&mut cursor, &mut baseline).unwrap();
            let calls = baseline.calls;
            for cut in 0..calls {
                let mut cursor = Cursor::new(source);
                let mut work = Budget {
                    cut: Some(cut),
                    ..Budget::default()
                };
                assert_eq!(drain(&mut cursor, &mut work), Err(Error::Work(77)));
                assert!(!cursor.is_complete());
                let mut replacement = Budget::default();
                assert_eq!(cursor.poll(&mut replacement), Err(Error::Work(77)));
                assert_eq!(cursor.check_work(&mut replacement), Err(Error::Work(77)));
                assert_eq!(replacement.calls, 0);
                assert_eq!(cursor.finish(), Err(Error::Work(77)));
            }
            cursor.check_work(&mut baseline).unwrap();
            baseline.cut = Some(baseline.calls);
            assert_eq!(cursor.check_work(&mut baseline), Err(Error::Work(77)));
            assert!(!cursor.is_complete());
            assert_eq!(cursor.poll(&mut Budget::default()), Err(Error::Work(77)));
            assert_eq!(cursor.finish(), Err(Error::Work(77)));
            assert!(spelling.end <= source.len());
        }
    }
    #[test]
    fn long_whitespace_and_failed_optional_suffixes_stay_funded() {
        let source = format!("(lead) a{}(bad{}", " ".repeat(8192), "x".repeat(8192));
        let mut cursor = Cursor::new(source.as_bytes());
        let mut work = Budget::default();
        let (spelling, turns) = drain(&mut cursor, &mut work).unwrap();
        assert!(turns > 8192);
        assert!(work.visits >= source.len() as u64);
        assert_eq!(source.get(spelling.start..spelling.end), source.get(7..));
        for source in [
            format!("a{}\r\nX", " ".repeat(8192)),
            format!("a{}b", " ".repeat(8192)),
            format!("a{} (bad (tail)", " (ok)".repeat(1024)),
        ] {
            let mut cursor = Cursor::new(source.as_bytes());
            let mut work = Budget::default();
            let (spelling, _) = drain(&mut cursor, &mut work).unwrap();
            assert_eq!(
                spelling,
                Spelling {
                    start: 0,
                    end: source.len()
                }
            );
            assert!(work.visits <= 5 * source.len() as u64);
        }
    }
}
