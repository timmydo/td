//! Exact comment grammar proofs for separately selected fallback display names.
use crate::{
    decode_work::Work,
    header_cfws,
    time::Tick,
    work::{Charge, Meter},
};
pub use header_cfws::Error;
#[path = "header_comment/decode.rs"]
pub mod decode;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
/// Validates exactly one complete parenthesized comment, without outer CFWS.
pub struct Cursor<'a> {
    source: &'a [u8],
    inner: header_cfws::Cursor<'a>,
    seen: bool,
    complete: bool,
    failure: Option<Error>,
}
/// Grammar proof only; the mailbox owner chooses whether to use this name.
///
/// ```compile_fail
/// let _ = td_mime::header_comment::Validated { source: b"(name)" };
/// ```
#[derive(Clone, Copy)]
pub struct Validated<'a> {
    source: &'a [u8],
}
impl<'a> Validated<'a> {
    pub const fn decode(self) -> decode::Cursor<'a> {
        decode::Cursor::new(self)
    }
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            inner: header_cfws::Cursor::new(source, 0),
            seen: false,
            complete: false,
            failure: None,
        }
    }
    pub fn into_validated(self) -> Result<Validated<'a>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete {
            return Err(Error::InvalidState);
        }
        Ok(Validated {
            source: self.source,
        })
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    pub(crate) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Status::Complete);
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        match self.inner.poll_with_work(now, work)? {
            header_cfws::Status::Yield => Ok(Status::Yield),
            header_cfws::Status::Comment(comment) => {
                if self.seen || comment.start != 0 || comment.end != self.source.len() {
                    return Err(Error::Malformed);
                }
                self.seen = true;
                Ok(Status::Yield)
            }
            header_cfws::Status::Complete(end) => {
                if !self.seen || end.position != self.source.len() {
                    return Err(Error::Malformed);
                }
                self.complete = true;
                Ok(Status::Complete)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{time::Deadline, work::Stop};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1_000_000,
                records: 1_000_000,
                ..Charge::default()
            },
        )
    }
    fn validate(input: &[u8]) -> Result<Validated<'_>, Error> {
        let mut cursor = Cursor::new(input);
        assert!(std::mem::size_of_val(&cursor) <= 96);
        let mut meter = work();
        for _ in 0..100_000 {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), &mut meter)?;
            let after = meter.remaining();
            assert!(
                before.records - after.records <= 33 && before.io_bytes - after.io_bytes <= 160
            );
            if status == Status::Complete {
                assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
                assert_eq!(after, meter.remaining());
                return cursor.into_validated();
            }
        }
        panic!("comment proof did not finish");
    }
    #[test]
    fn proof_requires_exactly_one_whole_comment_and_final_success() {
        for source in ["()", "(name)", "(a(b\\)c))", "(\\\0)", "(例\r\n 🐈)"] {
            let proof = validate(source.as_bytes()).unwrap();
            assert!(std::mem::size_of_val(&proof) <= 16);
            assert!(std::ptr::eq(proof.source, source.as_bytes()));
        }
        for input in [
            b"".as_slice(),
            b"name",
            b" (name)",
            b"(name) ",
            b"()(two)",
            b"(name",
            b"(\xff)",
            b"(\0)",
        ] {
            assert!(
                matches!(validate(input), Err(Error::Malformed)),
                "{input:?}"
            );
        }
        assert!(matches!(
            validate("(".repeat(33).as_bytes()),
            Err(Error::NestingLimit)
        ));
        let mut cursor = Cursor::new(b"(name)");
        assert_eq!(cursor.poll(Tick(1), &mut work()), Ok(Status::Yield));
        assert!(matches!(cursor.into_validated(), Err(Error::InvalidState)));
    }
    #[test]
    fn comment_validation_refusal_cannot_be_replaced_with_a_fresh_meter() {
        let mut cursor = Cursor::new(b"(name)");
        let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        assert_eq!(
            cursor.poll(Tick(1), &mut limited),
            Err(Error::Work(Stop::Records))
        );
        let mut meter = work();
        let before = meter.remaining();
        assert_eq!(
            cursor.poll(Tick(1), &mut meter),
            Err(Error::Work(Stop::Records))
        );
        assert_eq!(meter.remaining(), before);
        assert!(matches!(
            cursor.into_validated(),
            Err(Error::Work(Stop::Records))
        ));
    }
}
