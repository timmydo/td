//! Incremental arrays of JSON strings with caller-owned source and admission.
#![cfg_attr(clippy, deny(warnings))]

pub use crate::string::{Progress, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Source(E),
    InvalidState(Role),
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "JSON string array source: {error}"),
            Self::InvalidState(_) => f.write_str("invalid JSON string array state"),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for Error<E> {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Yield,
    Begin,
    Scalar(char),
    End,
    Complete,
}
/// Admission/error context; both roles use the same bound source and policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Array,
    String,
}
/// Emit Begin/Scalar*/End for each string, then Complete for the whole array.
/// Bound each poll independently and keep one logical source bound to the frame.
/// Zero-byte charges must perform live admission; all output stays provisional.
pub trait Source {
    type Context: Copy;
    type Error: Copy;
    fn charge_output(
        &mut self,
        now: Self::Context,
        bytes: u64,
        role: Role,
    ) -> Result<(), Self::Error>;
    fn poll(&mut self, now: Self::Context, role: Role) -> Result<Event, Self::Error>;
}
#[derive(Clone, Copy)]
enum Phase {
    Open,
    DrainOpen,
    Next,
    Comma,
    DrainComma,
    String,
    Close,
    DrainClose,
    Complete,
}
/// One source poll, one shared string turn or one punctuation byte per call.
/// Strings use the shared six-byte escaping buffer; no values are retained.
/// Quotes, commas and brackets are paid once before copying. Empty output
/// checks admission without advancing the source; cached Complete is inert.
/// Any source/protocol/admission refusal retires the entire array. A completed
/// frame still needs final live admission and caller-owned atomic publication.
/// Live framing state is neither Copy nor Clone.
pub struct Frame<E: Copy> {
    string: Option<crate::string::Frame<Error<E>>>,
    phase: Phase,
    first: bool,
    failure: Option<Error<E>>,
}
impl<E: Copy> Frame<E> {
    pub const fn new() -> Self {
        Self {
            string: None,
            phase: Phase::Open,
            first: true,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && matches!(self.phase, Phase::Complete)
    }
    pub fn check_admission<S: Source<Error = E>>(
        &mut self,
        source: &mut S,
        now: S::Context,
    ) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = source
            .charge_output(now, 0, Role::Array)
            .map_err(Error::Source);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll<S: Source<Error = E>>(
        &mut self,
        source: &mut S,
        now: S::Context,
        output: &mut [u8],
    ) -> Result<Progress, Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.is_complete() {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = self.step(source, now, output);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step<S: Source<Error = E>>(
        &mut self,
        source: &mut S,
        now: S::Context,
        output: &mut [u8],
    ) -> Result<Progress, Error<E>> {
        self.check_admission(source, now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        match self.phase {
            Phase::Open | Phase::Comma | Phase::Close => {
                source
                    .charge_output(now, 1, Role::Array)
                    .map_err(Error::Source)?;
                self.phase = match self.phase {
                    Phase::Open => Phase::DrainOpen,
                    Phase::Comma => Phase::DrainComma,
                    Phase::Close => Phase::DrainClose,
                    _ => return Err(Error::InvalidState(Role::Array)),
                };
            }
            Phase::DrainOpen | Phase::DrainComma | Phase::DrainClose => {
                let (byte, next) = match self.phase {
                    Phase::DrainOpen => (b'[', Phase::Next),
                    Phase::DrainComma => (b',', Phase::String),
                    Phase::DrainClose => (b']', Phase::Complete),
                    _ => return Err(Error::InvalidState(Role::Array)),
                };
                *output.get_mut(0).ok_or(Error::InvalidState(Role::Array))? = byte;
                self.phase = next;
                return Ok(Progress {
                    written: 1,
                    status: if self.is_complete() {
                        Status::Complete
                    } else {
                        Status::Yield
                    },
                });
            }
            Phase::Next => match source.poll(now, Role::Array).map_err(Error::Source)? {
                Event::Yield => {}
                Event::Begin => {
                    self.string = Some(crate::string::Frame::new());
                    self.phase = if self.first {
                        Phase::String
                    } else {
                        Phase::Comma
                    };
                    self.first = false;
                }
                Event::Complete => self.phase = Phase::Close,
                Event::Scalar(_) | Event::End => return Err(Error::InvalidState(Role::Array)),
            },
            Phase::String => {
                let mut progress = self
                    .string
                    .as_mut()
                    .ok_or(Error::InvalidState(Role::String))?
                    .poll(&mut StringSource(source), now, output)
                    .map_err(flatten)?;
                if progress.status == Status::Complete {
                    self.string = None;
                    self.phase = Phase::Next;
                    progress.status = Status::Yield;
                }
                return Ok(progress);
            }
            Phase::Complete => return Err(Error::InvalidState(Role::Array)),
        }
        Ok(Progress {
            written: 0,
            status: Status::Yield,
        })
    }
}
impl<E: Copy> Default for Frame<E> {
    fn default() -> Self {
        Self::new()
    }
}
fn flatten<E: Copy>(error: crate::string::Error<Error<E>>) -> Error<E> {
    match error {
        crate::string::Error::Source(inner) => inner,
        crate::string::Error::InvalidState => Error::InvalidState(Role::String),
    }
}
struct StringSource<'a, S>(&'a mut S);
impl<S: Source> crate::string::Source for StringSource<'_, S> {
    type Context = S::Context;
    type Error = Error<S::Error>;
    fn charge_output(&mut self, now: Self::Context, bytes: u64) -> Result<(), Self::Error> {
        self.0
            .charge_output(now, bytes, Role::String)
            .map_err(Error::Source)
    }
    fn poll(&mut self, now: Self::Context) -> Result<crate::string::Scalar, Self::Error> {
        match self.0.poll(now, Role::String).map_err(Error::Source)? {
            Event::Yield => Ok(crate::string::Scalar::Yield),
            Event::Scalar(value) => Ok(crate::string::Scalar::Value(value)),
            Event::End => Ok(crate::string::Scalar::Complete),
            Event::Begin | Event::Complete => Err(Error::InvalidState(Role::String)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Fault {
        Deadline,
        Output,
        Input,
        Admission,
    }
    struct Input<'a> {
        events: &'a [Event],
        position: usize,
        remaining: u64,
        paid: u64,
        checks: usize,
        reads: usize,
        fail_check: Option<usize>,
        fail_read: Option<usize>,
    }
    impl<'a> Input<'a> {
        fn new(events: &'a [Event]) -> Self {
            Self {
                events,
                position: 0,
                remaining: 100_000,
                paid: 0,
                checks: 0,
                reads: 0,
                fail_check: None,
                fail_read: None,
            }
        }
        fn counters(&self) -> (usize, usize, usize, u64) {
            (self.position, self.checks, self.reads, self.paid)
        }
    }
    impl Source for Input<'_> {
        type Context = bool;
        type Error = Fault;
        fn charge_output(&mut self, live: bool, bytes: u64, _: Role) -> Result<(), Fault> {
            self.checks += 1;
            if !live {
                return Err(Fault::Deadline);
            }
            if self.fail_check == Some(self.checks) {
                return Err(Fault::Admission);
            }
            if bytes > self.remaining {
                return Err(Fault::Output);
            }
            self.remaining -= bytes;
            self.paid += bytes;
            Ok(())
        }
        fn poll(&mut self, _: bool, _: Role) -> Result<Event, Fault> {
            self.reads += 1;
            if self.fail_read == Some(self.reads) {
                return Err(Fault::Input);
            }
            let event = self
                .events
                .get(self.position)
                .copied()
                .unwrap_or(Event::Complete);
            self.position += 1;
            Ok(event)
        }
    }
    fn drain(
        frame: &mut Frame<Fault>,
        source: &mut Input<'_>,
        width: usize,
    ) -> (Vec<u8>, Result<usize, Error<Fault>>) {
        let mut bytes = Vec::new();
        let mut output = [0; 6];
        for turn in 1..1000 {
            let reads = source.reads;
            let progress = frame.poll(source, true, &mut output[..width]);
            assert!(source.reads - reads <= 1);
            match progress {
                Ok(progress) => {
                    assert!(progress.written <= 6);
                    bytes.extend_from_slice(&output[..progress.written]);
                    if progress.status == Status::Complete {
                        assert!(frame.is_complete());
                        let counters = source.counters();
                        assert_eq!(
                            frame.poll(source, false, &mut []),
                            Ok(Progress {
                                written: 0,
                                status: Status::Complete
                            })
                        );
                        assert_eq!(source.counters(), counters);
                        return (bytes, Ok(turn));
                    }
                    assert!(!frame.is_complete());
                }
                Err(error) => {
                    assert!(!frame.is_complete());
                    let counters = source.counters();
                    assert_eq!(frame.poll(source, true, &mut output), Err(error));
                    assert_eq!(frame.check_admission(source, true), Err(error));
                    assert_eq!(source.counters(), counters);
                    return (bytes, Err(error));
                }
            }
        }
        panic!("string array did not finish")
    }
    #[test]
    fn roles_preserve_array_and_string_source_context() {
        struct Audited<'a> {
            input: Input<'a>,
            paid: [u64; 2],
            in_string: bool,
        }
        impl Source for Audited<'_> {
            type Context = bool;
            type Error = Fault;
            fn charge_output(&mut self, now: bool, bytes: u64, role: Role) -> Result<(), Fault> {
                let index = match role {
                    Role::Array => 0,
                    Role::String => 1,
                };
                self.paid[index] += bytes;
                self.input.charge_output(now, bytes, role)
            }
            fn poll(&mut self, now: bool, role: Role) -> Result<Event, Fault> {
                let event = self.input.poll(now, role)?;
                let expected = match event {
                    Event::Begin | Event::Complete => Role::Array,
                    Event::Scalar(_) | Event::End => Role::String,
                    Event::Yield => {
                        if self.in_string {
                            Role::String
                        } else {
                            Role::Array
                        }
                    }
                };
                assert_eq!(role, expected);
                match event {
                    Event::Begin => self.in_string = true,
                    Event::End => self.in_string = false,
                    _ => {}
                }
                Ok(event)
            }
        }
        let mut source = Audited {
            input: Input::new(EVENTS),
            paid: [0; 2],
            in_string: false,
        };
        let mut frame = Frame::new();
        let mut bytes = Vec::new();
        for _ in 0..1000 {
            let mut output = [0; 1];
            let progress = frame.poll(&mut source, true, &mut output).unwrap();
            bytes.extend_from_slice(&output[..progress.written]);
            if progress.status == Status::Complete {
                break;
            }
        }
        assert!(frame.is_complete());
        assert_eq!(source.paid, [4, bytes.len() as u64 - 4]);
        assert_eq!(source.input.paid, bytes.len() as u64);
    }
    const EVENTS: &[Event] = &[
        Event::Yield,
        Event::Begin,
        Event::Scalar('a'),
        Event::Yield,
        Event::Scalar('"'),
        Event::Scalar('\\'),
        Event::Scalar('\n'),
        Event::End,
        Event::Yield,
        Event::Begin,
        Event::End,
        Event::Begin,
        Event::Scalar('🐈'),
        Event::Scalar('\u{fdd0}'),
        Event::End,
        Event::Complete,
    ];
    const JSON: &str = "[\"a\\\"\\\\\\n\",\"\",\"🐈\u{fdd0}\"]";
    #[test]
    fn literal_arrays_short_drains_exact_charges_and_cached_completion() {
        assert!(std::mem::size_of::<Frame<u8>>() <= 96);
        for (events, expected) in [
            (EVENTS, JSON),
            (&[Event::Complete][..], "[]"),
            (&[Event::Begin, Event::End, Event::Complete][..], "[\"\"]"),
            (
                &[
                    Event::Begin,
                    Event::Scalar('x'),
                    Event::End,
                    Event::Complete,
                ][..],
                "[\"x\"]",
            ),
        ] {
            for width in 1..=6 {
                let mut source = Input::new(events);
                let mut frame = Frame::new();
                let counters = source.counters();
                assert_eq!(
                    frame.poll(&mut source, true, &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::NeedOutput
                    })
                );
                assert_eq!(source.position, counters.0);
                assert_eq!(source.reads, counters.2);
                assert_eq!(source.paid, counters.3);
                let (bytes, result) = drain(&mut frame, &mut source, width);
                assert!(result.is_ok());
                assert_eq!(bytes, expected.as_bytes());
                assert_eq!(source.paid, expected.len() as u64);
                assert!(crate::parse(expected).is_ok());
                assert_eq!(frame.check_admission(&mut source, true), Ok(()));
                assert_eq!(
                    frame.check_admission(&mut source, false),
                    Err(Error::Source(Fault::Deadline))
                );
                assert!(!frame.is_complete());
                let counters = source.counters();
                assert_eq!(
                    frame.poll(&mut source, true, &mut []),
                    Err(Error::Source(Fault::Deadline))
                );
                assert_eq!(source.counters(), counters);
            }
        }
    }
    #[test]
    fn malformed_event_protocol_retires_whole_array() {
        for (events, role) in [
            (&[Event::End][..], Role::Array),
            (&[Event::Scalar('x')][..], Role::Array),
            (&[Event::Begin, Event::Begin][..], Role::String),
            (&[Event::Begin, Event::Complete][..], Role::String),
            (&[Event::Begin, Event::Scalar('x')][..], Role::String),
            (&[Event::Begin, Event::End, Event::End][..], Role::Array),
        ] {
            let mut source = Input::new(events);
            let mut frame = Frame::new();
            let (_, result) = drain(&mut frame, &mut source, 1);
            assert_eq!(result, Err(Error::InvalidState(role)));
        }
    }
    #[test]
    fn every_output_source_and_admission_cut_is_sticky() {
        let mut source = Input::new(EVENTS);
        let mut frame = Frame::new();
        assert!(drain(&mut frame, &mut source, 1).1.is_ok());
        let baseline = source.counters();
        let mut partial = [false; 3];
        for (kind, seen) in partial.iter_mut().enumerate() {
            let count = match kind {
                0 => baseline.3 as usize,
                1 => baseline.2,
                2 => baseline.1,
                _ => panic!("bad cut"),
            };
            for cut in 0..count {
                let mut source = Input::new(EVENTS);
                match kind {
                    0 => source.remaining = cut as u64,
                    1 => source.fail_read = Some(cut + 1),
                    2 => source.fail_check = Some(cut + 1),
                    _ => panic!("bad cut"),
                }
                let mut frame = Frame::new();
                let (bytes, result) = drain(&mut frame, &mut source, 1);
                *seen |= !bytes.is_empty();
                let fault = match kind {
                    0 => Fault::Output,
                    1 => Fault::Input,
                    2 => Fault::Admission,
                    _ => panic!("bad cut"),
                };
                assert_eq!(result, Err(Error::Source(fault)));
            }
        }
        assert_eq!(partial, [true; 3]);
        let mut source = Input::new(EVENTS);
        source.remaining = baseline.3;
        let mut frame = Frame::new();
        assert!(drain(&mut frame, &mut source, 1).1.is_ok());
        assert_eq!(source.remaining, 0);
    }
    #[test]
    fn fresh_refusal_at_every_progress_cut_retires_even_complete() {
        let mut source = Input::new(EVENTS);
        let mut frame = Frame::new();
        let turns = drain(&mut frame, &mut source, 1).1.unwrap();
        for cut in 0..=turns {
            let mut source = Input::new(EVENTS);
            let mut frame = Frame::new();
            let mut byte = [0];
            for _ in 0..cut {
                frame.poll(&mut source, true, &mut byte).unwrap();
            }
            assert_eq!(
                frame.check_admission(&mut source, false),
                Err(Error::Source(Fault::Deadline))
            );
            assert!(!frame.is_complete());
            let counters = source.counters();
            assert_eq!(
                frame.poll(&mut source, true, &mut byte),
                Err(Error::Source(Fault::Deadline))
            );
            assert_eq!(source.counters(), counters);
        }
    }
}
