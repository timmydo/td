//! Stable quoted-printable bytes with bounded whitespace lookahead/replay.
use crate::{
    time::Tick,
    work::{Charge, Meter, Stop},
};

pub const STEP_TRANSITIONS: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    NeedInput,
    NeedOutput,
    Yield,
    /// Discard the old input suffix and resume at Decoder::position().
    Reposition,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub written: usize,
    pub status: Status,
}
#[derive(Clone, Copy)]
enum Mode {
    Normal,
    Equal,
    Hex(u8),
    Space { start: u64, equal: bool },
    Cr { start: u64, equal: bool },
    Replay { end: u64 },
}
/// The source owner supplies bytes beginning at position(), relative to its
/// immutable extent. Copying this state never copies or refunds the live meter.
#[derive(Clone, Copy)]
pub struct Decoder {
    length: u64,
    position: u64,
    mode: Mode,
    output: u16,
    pending: u8,
    eof: bool,
    problem: bool,
    failure: Option<Stop>,
}
impl Decoder {
    pub const fn new(length: u64) -> Self {
        Self {
            length,
            position: 0,
            mode: Mode::Normal,
            output: 0,
            pending: 0,
            eof: false,
            problem: false,
            failure: None,
        }
    }
    pub const fn position(&self) -> u64 {
        self.position
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    pub const fn failure(&self) -> Option<Stop> {
        self.failure
    }
    /// Supply a fragment starting at position(); bytes past length are ignored.
    /// Reposition invalidates the unconsumed suffix, even if output was written.
    /// Other progress consumes the reported prefix. Discard all provisional
    /// body output on error. The owner brackets turns with fresh clock checks.
    pub fn poll(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Stop> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.eof && self.pending == 0 {
            return Ok(Progress {
                consumed: 0,
                written: 0,
                status: Status::Complete,
            });
        }
        let result = self.advance(input, output, now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Stop> {
        meter.charge(now, Charge::default())?;
        let input_start = self.position;
        let mut progress = Progress {
            consumed: 0,
            written: 0,
            status: Status::Yield,
        };
        for _ in 0..STEP_TRANSITIONS {
            if self.pending != 0 {
                let Some(destination) = output.get_mut(progress.written) else {
                    progress.status = Status::NeedOutput;
                    return Ok(progress);
                };
                meter.charge(
                    now,
                    Charge {
                        output_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                *destination = (self.output >> 8) as u8;
                self.output <<= 8;
                self.pending -= 1;
                progress.written += 1;
                continue;
            }
            if self.eof {
                progress.status = Status::Complete;
                return Ok(progress);
            }
            // Replay terminates before interpreting the lookahead again.
            if let Mode::Replay { end } = self.mode {
                if self.position == end {
                    self.mode = Mode::Normal;
                }
            }
            let byte = if self.position == self.length {
                None
            } else {
                let Some(&byte) = input.get(progress.consumed) else {
                    progress.status = Status::NeedInput;
                    return Ok(progress);
                };
                Some(byte)
            };
            meter.charge(
                now,
                Charge {
                    io_bytes: u64::from(byte.is_some()),
                    ..Charge::default()
                },
            )?;
            let before = self.position;
            if self.step(byte) {
                if self.position >= input_start {
                    // The target lies in the prefix visited during this turn.
                    progress.consumed = (self.position - input_start) as usize;
                    continue;
                }
                progress.status = Status::Reposition;
                return Ok(progress);
            }
            if self.position > before {
                progress.consumed += 1;
            }
        }
        Ok(progress)
    }
    fn emit(&mut self, first: u8, second: Option<u8>) {
        self.output = u16::from(first) << 8 | u16::from(second.unwrap_or(0));
        self.pending = if second.is_some() { 2 } else { 1 };
    }
    fn replay(&mut self, start: u64, equal: bool) -> bool {
        let end = self.position;
        self.mode = Mode::Replay { end };
        self.position = start;
        if equal {
            self.problem = true;
            self.emit(b'=', None);
        }
        true
    }
    fn step(&mut self, byte: Option<u8>) -> bool {
        match self.mode {
            Mode::Normal => match byte {
                None => self.eof = true,
                Some(b'=') => {
                    self.position += 1;
                    self.mode = Mode::Equal;
                }
                Some(b' ' | b'\t') => {
                    self.mode = Mode::Space {
                        start: self.position,
                        equal: false,
                    };
                    self.position += 1;
                }
                Some(b'\r') => {
                    self.mode = Mode::Cr {
                        start: self.position,
                        equal: false,
                    };
                    self.position += 1;
                }
                Some(value) => {
                    self.problem |= !(33..=126).contains(&value);
                    self.position += 1;
                    self.emit(value, None);
                }
            },
            Mode::Equal => match byte {
                None => {
                    self.problem = true;
                    self.eof = true;
                }
                Some(b' ' | b'\t') => {
                    self.mode = Mode::Space {
                        start: self.position,
                        equal: true,
                    };
                    self.position += 1;
                }
                Some(b'\r') => {
                    self.mode = Mode::Cr {
                        start: self.position,
                        equal: true,
                    };
                    self.position += 1;
                }
                Some(b'\n') => {
                    self.problem = true;
                    self.position += 1;
                    self.mode = Mode::Normal;
                }
                Some(value) if hex(value).is_some() => {
                    self.position += 1;
                    self.mode = Mode::Hex(value);
                }
                Some(_) => {
                    self.problem = true;
                    self.emit(b'=', None);
                    self.mode = Mode::Normal;
                }
            },
            Mode::Hex(first) => {
                if let Some(value) = byte.and_then(hex) {
                    if let Some(high) = hex(first) {
                        self.emit(high * 16 + value, None);
                    }
                    self.position += 1;
                } else {
                    self.problem = true;
                    self.emit(b'=', Some(first));
                }
                self.mode = Mode::Normal;
            }
            Mode::Space { start, equal } => match byte {
                None => {
                    self.problem |= equal;
                    self.eof = true;
                }
                Some(b' ' | b'\t') => self.position += 1,
                Some(b'\r') => {
                    self.position += 1;
                    self.mode = Mode::Cr { start, equal };
                }
                Some(b'\n') => {
                    self.position += 1;
                    self.problem = true;
                    if !equal {
                        self.emit(b'\n', None);
                    }
                    self.mode = Mode::Normal;
                }
                Some(_) => return self.replay(start, equal),
            },
            Mode::Cr { start, equal } => {
                if byte == Some(b'\n') {
                    self.position += 1;
                    if !equal {
                        self.emit(b'\r', Some(b'\n'));
                    }
                    self.mode = Mode::Normal;
                } else {
                    self.problem = true;
                    return self.replay(start, equal);
                }
            }
            Mode::Replay { .. } => {
                if let Some(value) = byte {
                    self.position += 1;
                    self.emit(value, None);
                }
            }
        }
        false
    }
}
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::time::Deadline;
    fn meter(input: u64, output: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: input,
                output_bytes: output,
                ..Charge::default()
            },
        )
    }
    fn decode(bytes: &[u8], split: usize, fragment: usize, width: usize) -> (Vec<u8>, bool, u64) {
        let mut decoder = Decoder::new(bytes.len() as u64);
        let capacity = bytes.len() as u64 * 8 + 10;
        let mut budget = meter(capacity, capacity);
        let mut result = Vec::new();
        let mut output = [0; 4];
        for _ in 0..bytes.len() * 10 + 20 {
            let start = decoder.position() as usize;
            let end = (start + fragment).min(if start < split { split } else { bytes.len() });
            let step = decoder
                .poll(
                    &bytes[start..end],
                    &mut output[..width],
                    Tick(1),
                    &mut budget,
                )
                .unwrap();
            assert!(step.consumed <= end - start);
            if step.status != Status::Reposition {
                assert_eq!(decoder.position(), (start + step.consumed) as u64);
            } else {
                assert!(decoder.position() < (start + step.consumed + 1) as u64);
            }
            result.extend_from_slice(&output[..step.written]);
            if step.status == Status::Complete {
                assert_eq!(decoder.position(), bytes.len() as u64);
                let remaining = budget.remaining();
                assert_eq!(remaining.output_bytes, capacity - result.len() as u64);
                assert_eq!(
                    decoder
                        .poll(b"ignored", &mut [], Tick(100), &mut budget)
                        .unwrap(),
                    Progress {
                        consumed: 0,
                        written: 0,
                        status: Status::Complete
                    }
                );
                assert_eq!(budget.remaining(), remaining);
                return (
                    result,
                    decoder.is_encoding_problem(),
                    capacity - remaining.io_bytes,
                );
            }
        }
        panic!("QP did not complete");
    }
    #[test]
    fn policy_octets_and_diagnostics_survive_splits_rewinds_and_short_output() {
        let cases: &[(&[u8], &[u8], bool)] = &[
            (b"", b"", false),
            (b"a=20\r\nb \t\r\nc=0A=QZ=", b"a \r\nb\r\nc\n=QZ", true),
            (b"ab= \t\r\ncd", b"abcd", false),
            (b"ab=\ncd", b"abcd", true),
            (b"ab=", b"ab", true),
            (b"ab= \t", b"ab", true),
            (b"a \tb", b"a \tb", false),
            (b"a \t", b"a", false),
            (b"=20=09 \t\r\n", b" \t\r\n", false),
            (b"=20=09 \t\n", b" \t\n", true),
            (b"=00=ff=Ff=7f", b"\0\xff\xff\x7f", false),
            (b"a\0\x01\x7f\xff", b"a\0\x01\x7f\xff", true),
            (b"a\r\nb", b"a\r\nb", false),
            (b"a\rb", b"a\rb", true),
            (b"a\r", b"a\r", true),
            (b"a \t\rb", b"a \t\rb", true),
            (b"a \t\r", b"a \t\r", true),
            (b"a= \t\rb", b"a= \t\rb", true),
            (b"a= \t\r", b"a= \t\r", true),
            (b"=q=2q=2=0D", b"=q=2q=2\r", true),
            (b"==41", b"=A", true),
            (b"=4", b"=4", true),
            (b"= \t41", b"= \t41", true),
            (b"=4 \t\r\n", b"=4\r\n", true),
        ];
        for &(bytes, expected, problem) in cases {
            for split in 0..=bytes.len() {
                for fragment in [1, 2, 7, 6144] {
                    for width in 1..=4 {
                        let (actual, diagnostic, _) = decode(bytes, split, fragment, width);
                        assert_eq!(
                            actual, expected,
                            "{bytes:?} split {split} fragment {fragment} width {width}"
                        );
                        assert_eq!(diagnostic, problem, "{bytes:?}");
                    }
                }
            }
        }
        for first in b"0123456789ABCDEFabcdef" {
            for second in b"0123456789ABCDEFabcdef" {
                let bytes = [b'=', *first, *second];
                let digits = [*first, *second];
                let expected =
                    u8::from_str_radix(std::str::from_utf8(&digits).unwrap(), 16).unwrap();
                assert_eq!(decode(&bytes, 1, 1, 1).0, [expected]);
            }
        }
    }
    #[test]
    fn long_runs_use_bounded_replay_and_exact_live_work() {
        assert!(std::mem::size_of::<Decoder>() <= 64);
        for length in [255, 256, 257, 65537] {
            for leading in [b"a=".as_slice(), b"a"] {
                let mut bytes = leading.to_vec();
                for i in 0..length {
                    bytes.push(if i % 2 == 0 { b' ' } else { b'\t' });
                }
                let prefix = bytes.clone();
                for suffix in [b"\r\n".as_slice(), b"\n", b"", b"x", b"\rx"] {
                    bytes.truncate(prefix.len());
                    bytes.extend_from_slice(suffix);
                    let (decoded, problem, visits) = decode(&bytes, 17.min(bytes.len()), 7, 4);
                    if matches!(suffix, b"\r\n" | b"\n" | b"") {
                        let mut expected = b"a".to_vec();
                        if leading == b"a" {
                            expected.extend_from_slice(suffix);
                        }
                        assert_eq!(decoded, expected);
                        assert_eq!(
                            problem,
                            suffix == b"\n" || (suffix.is_empty() && leading == b"a=")
                        );
                    } else {
                        assert_eq!(decoded, bytes);
                        assert_eq!(problem, leading == b"a=" || suffix == b"\rx");
                    }
                    assert!(visits <= bytes.len() as u64 * 2 + 3);
                }
            }
        }
        let mut decoder = Decoder::new(4096);
        let mut budget = meter(8192, 8192);
        let step = decoder
            .poll(&[b' '; 4096], &mut [], Tick(1), &mut budget)
            .unwrap();
        assert_eq!(
            step,
            Progress {
                consumed: STEP_TRANSITIONS,
                written: 0,
                status: Status::Yield
            }
        );
        assert_eq!(decoder.position(), 256);
        assert_eq!(budget.remaining().io_bytes, 8192 - 256);
        let mut decoder = Decoder::new(4);
        let mut budget = meter(20, 20);
        assert_eq!(
            decoder
                .poll(b"= ", &mut [], Tick(1), &mut budget)
                .unwrap()
                .status,
            Status::NeedInput
        );
        let step = decoder.poll(b"\tx", &mut [], Tick(1), &mut budget).unwrap();
        assert_eq!(step.status, Status::Reposition);
        assert_eq!(decoder.position(), 1);
        let saved = decoder;
        for mut replay in [decoder, saved] {
            let mut output = [0; 4];
            let step = replay
                .poll(b" \tx", &mut output, Tick(1), &mut budget)
                .unwrap();
            assert_eq!(step.status, Status::Complete);
            assert_eq!(output, *b"= \tx");
        }
        assert_eq!(budget.remaining().io_bytes, 20 - 4 - 6);
        assert_eq!(budget.remaining().output_bytes, 20 - 8);
    }
    #[test]
    fn resident_runs_avoid_reposition_and_bare_cr_runs_remain_linear() {
        let bytes = b"a b".repeat(3000);
        let mut decoder = Decoder::new(bytes.len() as u64);
        let mut budget = meter(100000, 100000);
        let mut out = [0; 6144];
        let mut decoded = Vec::new();
        let mut done = false;
        let mut repositions = 0;
        for turn in 0..200 {
            let at = decoder.position() as usize;
            let step = decoder
                .poll(&bytes[at..], &mut out, Tick(1), &mut budget)
                .unwrap();
            decoded.extend_from_slice(&out[..step.written]);
            repositions += usize::from(step.status == Status::Reposition);
            if step.status == Status::Complete {
                assert!(turn < 160);
                done = true;
                break;
            }
        }
        assert!(done);
        assert!(repositions < 160);
        assert_eq!(decoded, bytes);
        assert!(!decoder.is_encoding_problem());
        let mut chained = b" \r".repeat(5000);
        chained.push(b'x');
        let (decoded, problem, visits) = decode(&chained, 37, 7, 4);
        assert_eq!(decoded, chained);
        assert!(problem);
        assert!(visits <= chained.len() as u64 * 3);
        let mut decoder = Decoder::new(1);
        let mut budget = meter(1, 1);
        let before = budget.remaining();
        assert_eq!(
            decoder.poll(&[], &mut out, Tick(1), &mut budget).unwrap(),
            Progress {
                consumed: 0,
                written: 0,
                status: Status::NeedInput
            }
        );
        assert_eq!(decoder.position(), 0);
        assert_eq!(budget.remaining(), before);
    }

    #[test]
    fn output_backpressure_pending_hex_and_refusals_remain_terminal() {
        let mut decoder = Decoder::new(3);
        let mut budget = meter(10, 10);
        let step = decoder.poll(b"=4x", &mut [], Tick(1), &mut budget).unwrap();
        assert_eq!(step.status, Status::NeedOutput);
        assert_eq!(decoder.position(), 2);
        let saved = decoder;
        for mut replay in [decoder, saved] {
            let mut output = [0; 3];
            let step = replay
                .poll(b"x", &mut output, Tick(1), &mut budget)
                .unwrap();
            assert_eq!(step.status, Status::Complete);
            assert_eq!(output, *b"=4x");
        }
        for (io, output, now, expected) in [
            (2, 10, Tick(1), Stop::IoBytes),
            (10, 1, Tick(1), Stop::OutputBytes),
            (10, 10, Tick(100), Stop::Deadline),
        ] {
            let mut decoder = Decoder::new(3);
            let mut budget = meter(io, output);
            let mut out = [0; 3];
            assert_eq!(
                decoder.poll(b"abc", &mut out, now, &mut budget),
                Err(expected)
            );
            if expected == Stop::OutputBytes {
                assert_eq!(out, [b'a', 0, 0]);
            }
            let mut fresh = meter(100, 100);
            let before = fresh.remaining();
            assert_eq!(decoder.failure(), Some(expected));
            assert_eq!(
                decoder.poll(b"abc", &mut out, Tick(1), &mut fresh),
                Err(expected)
            );
            assert_eq!(fresh.remaining(), before);
        }
        let mut decoder = Decoder::new(0);
        assert_eq!(
            decoder
                .poll(b"outside", &mut [], Tick(1), &mut meter(0, 0))
                .unwrap()
                .status,
            Status::Complete
        );
    }
}
