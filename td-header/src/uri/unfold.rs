//! URI wire whitespace removal before decoding; surrounding CFWS is external.
use crate::{Charge, Work};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    Work(E),
    InvalidState,
}

impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed URI fold"),
            Self::Work(error) => write!(f, "URI unfolding work: {error}"),
            Self::InvalidState => f.write_str("invalid URI unfolding state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    /// Literal octet and offset within the supplied spelling slice.
    /// Provisional; retires on any later refusal. The caller rebases offsets.
    Octet {
        byte: u8,
        position: usize,
    },
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Text,
    Cr,
    Fold,
    Complete,
}
/// Supply one selected URI wire spelling, excluding surrounding CFWS and the
/// final header line ending. Every octet retires after any later refusal.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_header::uri::unfold::Cursor<'_, ()>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_header::uri::unfold::Cursor<'_, ()>>();
/// ```
pub struct Cursor<'a, E: Copy> {
    source: &'a [u8],
    position: usize,
    phase: Phase,
    failure: Option<Error<E>>,
}
impl<'a, E: Copy> Cursor<'a, E> {
    /// Supply the selected spelling slice; offsets start at zero in this slice.
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            position: 0,
            phase: Phase::Text,
            failure: None,
        }
    }
    /// Healthy complete unfolding; no enclosing validity or admission follows.
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && matches!(self.phase, Phase::Complete)
    }
    fn outcome<T>(&mut self, result: Result<T, Error<E>>) -> Result<T, Error<E>> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Admit fresh zero-count work; refusal retires even cached completion.
    pub fn check_work(&mut self, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(Charge::default()).map_err(Error::Work);
        self.outcome(result)
    }
    /// Admit one octet or EOF; removed wire whitespace yields.
    /// Returned octets remain provisional. Healthy cached Complete is inert.
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
    fn step(&mut self, work: &mut impl Work<Error = E>) -> Result<Status, Error<E>> {
        // One admitted source visit or EOF decision per turn; runs of wire
        // whitespace never create an unbounded scan inside one poll.
        work.charge(Charge {
            visits: u64::from(self.position < self.source.len()),
            records: 1,
        })
        .map_err(Error::Work)?;
        let Some(byte) = self.source.get(self.position).copied() else {
            if !matches!(self.phase, Phase::Text) {
                return Err(Error::Malformed);
            }
            self.phase = Phase::Complete;
            return Ok(Status::Complete);
        };
        let position = self.position;
        self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
        match self.phase {
            Phase::Text => match byte {
                b' ' | b'\t' => Ok(Status::Yield),
                b'\r' => {
                    self.phase = Phase::Cr;
                    Ok(Status::Yield)
                }
                b'\n' => {
                    self.phase = Phase::Fold;
                    Ok(Status::Yield)
                }
                _ => Ok(Status::Octet { byte, position }),
            },
            Phase::Cr if byte == b'\n' => {
                self.phase = Phase::Fold;
                Ok(Status::Yield)
            }
            Phase::Fold if matches!(byte, b' ' | b'\t') => {
                self.phase = Phase::Text;
                Ok(Status::Yield)
            }
            Phase::Cr | Phase::Fold => Err(Error::Malformed),
            Phase::Complete => Err(Error::InvalidState),
        }
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
            assert!(charge.visits <= 1 && charge.records <= 1);
            if self.cut == Some(call) {
                return Err(77);
            }
            self.visits += charge.visits;
            self.records += charge.records;
            Ok(())
        }
    }
    fn run(input: &[u8]) -> Result<(Vec<u8>, Budget), Error<u8>> {
        let mut cursor = Cursor::new(input);
        assert!(std::mem::size_of_val(&cursor) <= 64);
        let mut work = Budget::default();
        let mut output = Vec::new();
        for _ in 0..=input.len() {
            match cursor.poll(&mut work)? {
                Status::Octet { byte, position } => {
                    assert_eq!(input.get(position), Some(&byte));
                    output.push(byte);
                }
                Status::Yield => {}
                Status::Complete => {
                    assert!(cursor.is_complete());
                    let before = work.calls;
                    assert_eq!(cursor.poll(&mut work), Ok(Status::Complete));
                    assert_eq!(work.calls, before);
                    return Ok((output, work));
                }
            }
        }
        panic!("no bounded completion");
    }
    #[test]
    fn literal_whitespace_removal_preserves_spelling_and_offsets() {
        for (input, expected) in [
            (&b""[..], &b""[..]),
            (&b" \t\r\n \n\t"[..], &b""[..]),
            (
                &b" http://Example/a%\r\n 2F\tb?x=#f "[..],
                &b"http://Example/a%2Fb?x=#f"[..],
            ),
            (&b"=?utf-8?Q?a_\r\n b?="[..], &b"=?utf-8?Q?a_b?="[..]),
            (&b"(x)\\\"\0\xff"[..], &b"(x)\\\"\0\xff"[..]),
        ] {
            let (output, work) = run(input).unwrap();
            assert_eq!(output, expected);
            assert_eq!(work.visits, input.len() as u64);
            assert_eq!(work.records, input.len() as u64 + 1);
        }
        let input = vec![b' '; 8192];
        let (output, work) = run(&input).unwrap();
        assert!(output.is_empty());
        assert_eq!(work.records, 8193);
    }
    #[test]
    fn exact_positions_across_folds_and_repeated_octets() {
        let mut cursor = Cursor::new(b"a \t\r\n aa\n\t%2Fa");
        let mut work = Budget::default();
        let mut positions = Vec::new();
        for _ in 0..=14 {
            match cursor.poll(&mut work).unwrap() {
                Status::Octet { byte, position } => positions.push((byte, position)),
                Status::Complete => break,
                Status::Yield => {}
            }
        }
        assert!(cursor.is_complete());
        assert_eq!(
            positions,
            [
                (b'a', 0),
                (b'a', 6),
                (b'a', 7),
                (b'%', 10),
                (b'2', 11),
                (b'F', 12),
                (b'a', 13)
            ]
        );
        assert_eq!((work.visits, work.records), (14, 15));
    }
    #[test]
    fn malformed_folds_retire_provisional_octets() {
        for input in [
            &b"a\r"[..],
            &b"a\r\n"[..],
            &b"a\n"[..],
            &b"a\rX"[..],
            &b"a\r\nX"[..],
            &b"a\nX"[..],
            &b"a\r\n\r\n "[..],
        ] {
            let mut cursor = Cursor::new(input);
            let mut work = Budget::default();
            assert_eq!(
                cursor.poll(&mut work),
                Ok(Status::Octet {
                    byte: b'a',
                    position: 0
                })
            );
            let mut error = None;
            for _ in 0..=input.len() {
                match cursor.poll(&mut work) {
                    Err(found) => {
                        error = Some(found);
                        break;
                    }
                    Ok(Status::Complete) => panic!("bad fold completed"),
                    _ => {}
                }
            }
            assert_eq!(error, Some(Error::Malformed));
            assert!(!cursor.is_complete());
            let mut replacement = Budget::default();
            assert_eq!(cursor.poll(&mut replacement), Err(Error::Malformed));
            assert_eq!(cursor.check_work(&mut replacement), Err(Error::Malformed));
            assert_eq!(replacement.calls, 0);
        }
    }
    #[test]
    fn every_callback_cut_is_sticky_including_eof() {
        let healthy = b"a%\r\n 2F\n b\tx";
        let (_, work) = run(healthy).unwrap();
        for cut in 0..work.calls {
            let mut cursor = Cursor::new(healthy);
            let mut budget = Budget {
                cut: Some(cut),
                ..Budget::default()
            };
            let mut error = None;
            for _ in 0..=healthy.len() {
                match cursor.poll(&mut budget) {
                    Err(found) => {
                        error = Some(found);
                        break;
                    }
                    Ok(Status::Complete) => panic!("cut completed"),
                    _ => {}
                }
            }
            assert_eq!(error, Some(Error::Work(77)));
            assert_eq!(budget.calls, cut + 1);
            assert_eq!(budget.visits, cut as u64);
            assert!(!cursor.is_complete());
            let mut replacement = Budget::default();
            assert_eq!(cursor.poll(&mut replacement), Err(Error::Work(77)));
            assert_eq!(cursor.check_work(&mut replacement), Err(Error::Work(77)));
            assert_eq!(replacement.calls, 0);
        }
    }
    #[test]
    fn fresh_admission_retires_cached_completion() {
        let mut cursor = Cursor::new(b"");
        let mut budget = Budget::default();
        assert_eq!(cursor.poll(&mut budget), Ok(Status::Complete));
        assert_eq!((budget.visits, budget.records, budget.calls), (0, 1, 1));
        assert_eq!(cursor.check_work(&mut budget), Ok(()));
        assert_eq!((budget.visits, budget.records, budget.calls), (0, 1, 2));
        budget.cut = Some(2);
        assert_eq!(cursor.check_work(&mut budget), Err(Error::Work(77)));
        assert!(!cursor.is_complete());
        let mut replacement = Budget::default();
        assert_eq!(cursor.poll(&mut replacement), Err(Error::Work(77)));
        assert_eq!(replacement.calls, 0);
    }
}
