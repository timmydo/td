//! Incremental JSON string framing with caller-owned scalar and admission policy.
//! No allocation, normalization, character filtering, clock or I/O is supplied.

#![cfg_attr(clippy, deny(warnings))]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Source(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "JSON string source: {error}"),
            Self::InvalidState => f.write_str("invalid JSON string framing state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scalar {
    Yield,
    Value(char),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedOutput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub status: Status,
}
#[derive(Clone, Copy)]
enum Phase {
    Open,
    Source,
    Close,
    DrainClose,
    Complete,
}
/// Bind one logical source and admission policy for the frame's lifetime,
/// including final admission.
/// Each poll returns at most one scalar. Zero-byte admission must perform the
/// same live checks as nonzero output. The caller bounds source work and owns
/// character policy; ordinary JSON permits all Rust chars, including noncharacters.
/// Any refusal retires all previously emitted bytes. Do not rewrap an advanced
/// source in a fresh frame after abandonment or failure.
pub trait Source {
    type Context: Copy;
    type Error: Copy;
    fn charge_output(&mut self, context: Self::Context, bytes: u64) -> Result<(), Self::Error>;
    fn poll(&mut self, context: Self::Context) -> Result<Scalar, Self::Error>;
}
/// Fixed framing state; the owner retains source identity across short borrows.
/// Each call invokes at most one scalar poll or copies at most six already-paid
/// bytes. Quotes/escapes/UTF-8 consume exactly their serialized length before
/// copying. Empty output performs only live admission; cached Complete is inert.
/// All output is provisional through successful completion and final admission.
/// The frame owns no source, heap storage, or admission budget.
pub struct Frame<E: Copy> {
    pending: [u8; 6],
    used: usize,
    position: usize,
    phase: Phase,
    failure: Option<Error<E>>,
}
impl<E: Copy> Frame<E> {
    pub const fn new() -> Self {
        Self {
            pending: [0; 6],
            used: 0,
            position: 0,
            phase: Phase::Open,
            failure: None,
        }
    }
    /// Explicit post-turn/final admission check; refusal retires even Complete.
    pub fn check_admission<S: Source<Error = E>>(
        &mut self,
        source: &mut S,
        now: S::Context,
    ) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = source.charge_output(now, 0).map_err(Error::Source);
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
        if matches!(self.phase, Phase::Complete) {
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
    fn stage<S: Source<Error = E>>(
        &mut self,
        source: &mut S,
        now: S::Context,
        used: usize,
    ) -> Result<(), Error<E>> {
        if used == 0 || used > self.pending.len() {
            return Err(Error::InvalidState);
        }
        source
            .charge_output(now, used as u64)
            .map_err(Error::Source)?;
        self.used = used;
        self.position = 0;
        Ok(())
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
        if self.position < self.used {
            let count = self
                .used
                .checked_sub(self.position)
                .ok_or(Error::InvalidState)?
                .min(output.len());
            let end = self
                .position
                .checked_add(count)
                .ok_or(Error::InvalidState)?;
            let bytes = self
                .pending
                .get(self.position..end)
                .ok_or(Error::InvalidState)?;
            output
                .get_mut(..count)
                .ok_or(Error::InvalidState)?
                .copy_from_slice(bytes);
            self.position = end;
            if self.position == self.used && matches!(self.phase, Phase::DrainClose) {
                self.phase = Phase::Complete;
            }
            return Ok(Progress {
                written: count,
                status: if matches!(self.phase, Phase::Complete) {
                    Status::Complete
                } else {
                    Status::Yield
                },
            });
        }
        match self.phase {
            Phase::Open | Phase::Close => {
                self.pending = [b'"', 0, 0, 0, 0, 0];
                self.stage(source, now, 1)?;
                self.phase = if matches!(self.phase, Phase::Open) {
                    Phase::Source
                } else {
                    Phase::DrainClose
                };
            }
            Phase::Source => match source.poll(now).map_err(Error::Source)? {
                Scalar::Yield => {}
                Scalar::Value(value) => {
                    let used = encode(value, &mut self.pending).ok_or(Error::InvalidState)?;
                    self.stage(source, now, used)?;
                }
                Scalar::Complete => self.phase = Phase::Close,
            },
            Phase::DrainClose | Phase::Complete => return Err(Error::InvalidState),
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
pub(crate) fn encode(value: char, output: &mut [u8; 6]) -> Option<usize> {
    let short = match value {
        '"' => Some(b'"'),
        '\\' => Some(b'\\'),
        '\u{8}' => Some(b'b'),
        '\u{c}' => Some(b'f'),
        '\n' => Some(b'n'),
        '\r' => Some(b'r'),
        '\t' => Some(b't'),
        _ => None,
    };
    if let Some(byte) = short {
        *output = [b'\\', byte, 0, 0, 0, 0];
        return Some(2);
    }
    if value <= '\u{1f}' {
        let code = value as u8;
        let nibble = |value: u8| {
            if value < 10 {
                b'0' + value
            } else {
                b'a' + value - 10
            }
        };
        *output = [
            b'\\',
            b'u',
            b'0',
            b'0',
            nibble(code >> 4),
            nibble(code & 15),
        ];
        return Some(6);
    }
    let mut bytes = [0; 4];
    let text = value.encode_utf8(&mut bytes);
    output
        .get_mut(..text.len())?
        .copy_from_slice(text.as_bytes());
    Some(text.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn scalar_encoding_covers_all_unicode_without_surrogates_or_unescaped_controls() {
        let mut output = [0; 6];
        for code in 0..=0x10ffff {
            let Some(value) = char::from_u32(code) else {
                continue;
            };
            let used = encode(value, &mut output).unwrap();
            let text = std::str::from_utf8(output.get(..used).unwrap()).unwrap();
            if value <= '\u{1f}' || matches!(value, '"' | '\\') {
                assert!(text.starts_with('\\'));
                assert!(matches!(used, 2 | 6));
                assert!(!text.chars().any(|ch| ch <= '\u{1f}'));
                let decoded = if let Some(hex) = text.strip_prefix(r"\u") {
                    char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap()
                } else {
                    match text {
                        "\\\"" => '"',
                        "\\\\" => '\\',
                        "\\b" => '\u{8}',
                        "\\f" => '\u{c}',
                        "\\n" => '\n',
                        "\\r" => '\r',
                        "\\t" => '\t',
                        _ => panic!("invalid JSON escape"),
                    }
                };
                assert_eq!(decoded, value);
            } else {
                assert_eq!(text.chars().count(), 1);
                assert_eq!(text.chars().next(), Some(value));
            }
        }
    }
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Stop {
        Admission,
        Output,
        Input,
    }
    struct Scalars<'a> {
        input: std::str::Chars<'a>,
        output: u64,
        polls: usize,
        checks: usize,
        input_error: bool,
    }
    impl<'a> Scalars<'a> {
        fn new(input: &'a str, output: u64) -> Self {
            Self {
                input: input.chars(),
                output,
                polls: 0,
                checks: 0,
                input_error: false,
            }
        }
    }
    impl Source for Scalars<'_> {
        type Context = bool;
        type Error = Stop;
        fn charge_output(&mut self, live: bool, bytes: u64) -> Result<(), Stop> {
            self.checks += 1;
            if !live {
                return Err(Stop::Admission);
            }
            self.output = self.output.checked_sub(bytes).ok_or(Stop::Output)?;
            Ok(())
        }
        fn poll(&mut self, _: bool) -> Result<Scalar, Stop> {
            self.polls += 1;
            if self.input_error {
                return Err(Stop::Input);
            }
            if self.polls % 2 == 1 {
                return Ok(Scalar::Yield);
            }
            Ok(self.input.next().map_or(Scalar::Complete, Scalar::Value))
        }
    }
    fn drain(
        frame: &mut Frame<Stop>,
        source: &mut Scalars<'_>,
        width: usize,
    ) -> (Vec<u8>, Result<(), Error<Stop>>) {
        let mut emitted = Vec::new();
        for _ in 0..1000 {
            let before = source.polls;
            let mut output = [0xa5; 8];
            let step = frame.poll(source, true, output.get_mut(..width).unwrap());
            assert!(source.polls - before <= 1);
            match step {
                Ok(progress) => {
                    assert!(progress.written <= 6);
                    emitted.extend_from_slice(output.get(..progress.written).unwrap());
                    assert!(output
                        .get(progress.written..)
                        .unwrap()
                        .iter()
                        .all(|byte| *byte == 0xa5));
                    if progress.status == Status::Complete {
                        return (emitted, Ok(()));
                    }
                }
                Err(error) => {
                    assert_eq!(output, [0xa5; 8]);
                    let before = (source.polls, source.checks, source.output);
                    assert_eq!(frame.poll(source, true, &mut output), Err(error));
                    assert_eq!(frame.check_admission(source, true), Err(error));
                    assert_eq!((source.polls, source.checks, source.output), before);
                    return (emitted, Err(error));
                }
            }
        }
        panic!("JSON frame did not finish");
    }
    #[test]
    fn short_drains_match_the_value_writer_and_charge_exact_wire_bytes() {
        for input in [
            "",
            "\"\\\0\u{1f}\n\t\r\u{8}\u{c}",
            "/é例🐈\u{2028}\u{fdd0}",
            "e\u{301}",
        ] {
            let expected = crate::Json::Str(input.to_owned()).to_string();
            for width in 1..=8 {
                let mut source = Scalars::new(input, expected.len() as u64);
                let mut frame = Frame::new();
                assert!(std::mem::size_of_val(&frame) <= 32);
                assert_eq!(
                    drain(&mut frame, &mut source, width),
                    (expected.as_bytes().to_vec(), Ok(()))
                );
                assert_eq!(source.output, 0);
                let before = (source.polls, source.checks);
                assert_eq!(
                    frame.poll(&mut source, false, &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::Complete
                    })
                );
                assert_eq!((source.polls, source.checks), before);
                assert_eq!(
                    frame.check_admission(&mut source, false),
                    Err(Error::Source(Stop::Admission))
                );
                assert_eq!(
                    frame.poll(&mut source, true, &mut [0]),
                    Err(Error::Source(Stop::Admission))
                );
            }
        }
    }
    #[test]
    fn every_output_cutoff_and_input_failure_retire_the_provisional_string() {
        let input = "\0a\\🐈";
        let expected = crate::Json::Str(input.to_owned()).to_string();
        let mut late = false;
        for bytes in 0..expected.len() {
            let mut source = Scalars::new(input, bytes as u64);
            let (emitted, result) = drain(&mut Frame::new(), &mut source, 1);
            assert_eq!(result, Err(Error::Source(Stop::Output)));
            assert!(expected.as_bytes().starts_with(&emitted));
            late |= emitted.len() == expected.len() - 1;
        }
        assert!(late);
        let mut source = Scalars::new(input, 100);
        source.input_error = true;
        assert_eq!(
            drain(&mut Frame::new(), &mut source, 1),
            (b"\"".to_vec(), Err(Error::Source(Stop::Input)))
        );
    }
    #[test]
    fn empty_output_and_partial_escape_drains_check_live_admission() {
        let mut source = Scalars::new("\0", 8);
        let mut frame = Frame::new();
        for _ in 0..3 {
            assert_eq!(
                frame.poll(&mut source, true, &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::NeedOutput
                })
            );
            assert_eq!(source.polls, 0);
            assert_eq!(source.output, 8);
        }
        let mut partial = false;
        for _ in 0..100 {
            frame.poll(&mut source, true, &mut [0]).unwrap();
            if frame.used == 6 && frame.position == 1 {
                partial = true;
                break;
            }
        }
        assert!(partial);
        assert_eq!(source.output, 1);
        let before = (frame.position, frame.used, source.output, source.polls);
        assert_eq!(
            frame.poll(&mut source, true, &mut []),
            Ok(Progress {
                written: 0,
                status: Status::NeedOutput
            })
        );
        assert_eq!(
            (frame.position, frame.used, source.output, source.polls),
            before
        );
        let mut output = [0xa5; 8];
        assert_eq!(
            frame.poll(&mut source, false, &mut output),
            Err(Error::Source(Stop::Admission))
        );
        assert_eq!(output, [0xa5; 8]);
        assert_eq!(
            (frame.position, frame.used, source.output, source.polls),
            before
        );
    }
}
