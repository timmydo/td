//! Stable MIME base64 octets for POLICY.md's versioned part locators.
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
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: usize,
    pub written: usize,
    pub status: Status,
}
/// Fixed decoder state only. The source position and charged job meter are external.
#[derive(Clone, Copy, Default)]
pub struct Decoder {
    bits: u32,
    sextets: u8,
    output: u32,
    pending: u8,
    padding: u8,
    ended: bool,
    eof: bool,
    problem: bool,
    failure: Option<Stop>,
}
impl Decoder {
    /// Provisional until Complete: later padding or trailing bytes can add a problem.
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    pub const fn failure(&self) -> Option<Stop> {
        self.failure
    }
    /// Decode a bounded turn into caller output. `last` marks this input's end as EOF;
    /// retain the unconsumed suffix and repeat `last` until Complete. Empty output
    /// is backpressure, not failure. Errors retire this owner; discard partial output.
    /// The coordinator brackets turns with fresh clock/cancellation checks.
    pub fn poll(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        last: bool,
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
        let result = self.advance(input, output, last, now, meter);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        last: bool,
        now: Tick,
        meter: &mut Meter,
    ) -> Result<Progress, Stop> {
        meter.charge(now, Charge::default())?;
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
                *destination = (self.output >> 16) as u8;
                self.output <<= 8;
                self.pending -= 1;
                progress.written += 1;
            } else if self.eof {
                progress.status = Status::Complete;
                return Ok(progress);
            } else if let Some(&byte) = input.get(progress.consumed) {
                meter.charge(
                    now,
                    Charge {
                        io_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                self.byte(byte);
                progress.consumed += 1;
            } else if last {
                meter.charge(now, Charge::default())?;
                self.eof = true;
                if self.ended {
                    self.problem |= self.padding != 0;
                } else {
                    self.problem |= self.sextets != 0;
                    self.tail();
                }
            } else {
                progress.status = Status::NeedInput;
                return Ok(progress);
            }
        }
        Ok(progress)
    }
    fn tail(&mut self) {
        match self.sextets {
            2 => {
                self.output = (self.bits >> 4) << 16;
                self.pending = 1;
                self.problem |= self.bits & 15 != 0;
            }
            3 => {
                self.output = (self.bits >> 2) << 8;
                self.pending = 2;
                self.problem |= self.bits & 3 != 0;
            }
            _ => self.problem |= self.sextets != 0,
        }
        self.bits = 0;
        self.sextets = 0;
    }
    fn byte(&mut self, byte: u8) {
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            return;
        }
        if self.ended {
            if byte == b'=' && self.padding != 0 {
                self.padding -= 1;
            } else {
                self.problem = true;
            }
            return;
        }
        if byte == b'=' {
            self.ended = true;
            self.padding = u8::from(self.sextets == 2);
            self.problem |= self.sextets < 2;
            self.tail();
            return;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => {
                self.problem = true;
                return;
            }
        };
        self.bits = (self.bits << 6) | u32::from(value);
        self.sextets += 1;
        if self.sextets == 4 {
            self.output = self.bits;
            self.pending = 3;
            self.bits = 0;
            self.sextets = 0;
        }
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
    fn fragmented(bytes: &[u8], split: usize, width: usize) -> (Vec<u8>, bool) {
        let mut decoder = Decoder::default();
        let mut meter = meter(bytes.len() as u64, bytes.len() as u64);
        let mut decoded = Vec::new();
        let mut consumed = 0;
        let mut first = true;
        for _ in 0..bytes.len() * 4 + 10 {
            let end = if first { split } else { bytes.len() };
            let mut output = [0; 3];
            let step = decoder
                .poll(
                    &bytes[consumed..end],
                    &mut output[..width],
                    !first,
                    Tick(1),
                    &mut meter,
                )
                .unwrap();
            consumed += step.consumed;
            decoded.extend_from_slice(&output[..step.written]);
            if step.status == Status::NeedInput {
                first = false;
            }
            if step.status == Status::Complete {
                assert_eq!(consumed, bytes.len());
                assert_eq!(meter.remaining().io_bytes, 0);
                assert_eq!(
                    meter.remaining().output_bytes,
                    bytes.len() as u64 - decoded.len() as u64
                );
                return (decoded, decoder.is_encoding_problem());
            }
        }
        panic!("decoder did not complete");
    }
    #[test]
    fn stable_octets_and_diagnostics_survive_every_split_and_short_output() {
        let cases: &[(&[u8], &[u8], bool)] = &[
            (b"", b"", false),
            (b"TQ==", b"M", false),
            (b"TWE=", b"Ma", false),
            (b"TWFu", b"Man", false),
            (b"TQ", b"M", true),
            (b"TWE", b"Ma", true),
            (b"T!Q==Z", b"M", true),
            (b"T", b"", true),
            (b"Zg==", b"f", false),
            (b"Zm8=", b"fo", false),
            (b"Zm9v", b"foo", false),
            (b"Zm9vYg==", b"foob", false),
            (b"Zm9vYmE=", b"fooba", false),
            (b"Zm9vYmFy", b"foobar", false),
            (b" T\tQ\r\n= \t=\r\n", b"M", false),
            (b"TQ=", b"M", true),
            (b"TQ===", b"M", true),
            (b"TR==", b"M", true),
            (b"TWF=", b"Ma", true),
            (b"TWE==", b"Ma", true),
            (b"TWFu=", b"Man", true),
            (b"=TWFu", b"", true),
            (b"T=WFu", b"", true),
            (b"TQ=Z=", b"M", true),
            (b"TQ==TWFu", b"M", true),
            (b"T\x0b\x0c\x00\xffQ==", b"M", true),
            (b"AAECA/7/", b"\x00\x01\x02\x03\xfe\xff", false),
        ];
        for &(input, expected, problem) in cases {
            for split in 0..=input.len() {
                for width in 1..=3 {
                    let (bytes, actual) = fragmented(input, split, width);
                    assert_eq!(
                        bytes, expected,
                        "input {input:?} split {split} width {width}"
                    );
                    assert_eq!(actual, problem, "input {input:?} split {split}");
                }
            }
        }
    }
    #[test]
    fn every_tail_pad_bit_and_padding_count_has_deterministic_output() {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for (value, &symbol) in alphabet.iter().enumerate() {
            let input = [b'T', symbol, b'=', b'='];
            let (output, problem) = fragmented(&input, 2, 1);
            assert_eq!(output, [0x4c | (value as u8 >> 4)]);
            assert_eq!(problem, value & 15 != 0);
            let input = [b'T', b'W', symbol, b'='];
            let (output, problem) = fragmented(&input, 3, 1);
            assert_eq!(output, [b'M', 0x60 | (value as u8 >> 2)]);
            assert_eq!(problem, value & 3 != 0);
        }
    }
    #[test]
    fn step_ceiling_backpressure_and_checkpoint_preserve_pending_octets() {
        assert!(std::mem::size_of::<Decoder>() <= 32);
        let mut decoder = Decoder::default();
        let mut budget = meter(4096, 4096);
        let step = decoder
            .poll(&[b' '; 4096], &mut [], true, Tick(1), &mut budget)
            .unwrap();
        assert_eq!(
            step,
            Progress {
                consumed: STEP_TRANSITIONS,
                written: 0,
                status: Status::Yield
            }
        );
        assert_eq!(budget.remaining().io_bytes, 4096 - STEP_TRANSITIONS as u64);
        let mut decoder = Decoder::default();
        let step = decoder
            .poll(b"TWFu", &mut [], true, Tick(1), &mut budget)
            .unwrap();
        assert_eq!(
            step,
            Progress {
                consumed: 4,
                written: 0,
                status: Status::NeedOutput
            }
        );
        let checkpoint = decoder;
        for mut restored in [decoder, checkpoint] {
            let mut output = [0; 3];
            let step = restored
                .poll(b"", &mut output, true, Tick(1), &mut budget)
                .unwrap();
            assert_eq!(
                step,
                Progress {
                    consumed: 0,
                    written: 3,
                    status: Status::Complete
                }
            );
            assert_eq!(output, *b"Man");
            assert_eq!(
                restored
                    .poll(b"ignored", &mut output, false, Tick(1), &mut budget)
                    .unwrap()
                    .consumed,
                0
            );
        }
        assert_eq!(
            budget.remaining().io_bytes,
            4096 - STEP_TRANSITIONS as u64 - 4
        );
        assert_eq!(budget.remaining().output_bytes, 4096 - 6);
        let mut decoder = Decoder::default();
        let mut charged = meter(7, 2);
        assert_eq!(
            decoder
                .poll(b"T", &mut [0; 1], false, Tick(1), &mut charged)
                .unwrap()
                .status,
            Status::NeedInput
        );
        let checkpoint = decoder;
        for mut restored in [decoder, checkpoint] {
            let mut output = [0; 1];
            assert_eq!(
                restored
                    .poll(b"Q==", &mut output, true, Tick(1), &mut charged)
                    .unwrap()
                    .status,
                Status::Complete
            );
            assert_eq!(output, *b"M");
            let mut stopped = meter(0, 0);
            assert_eq!(
                stopped.charge(
                    Tick(1),
                    Charge {
                        io_bytes: 1,
                        ..Charge::default()
                    }
                ),
                Err(Stop::IoBytes)
            );
            let before = stopped.remaining();
            assert_eq!(
                restored
                    .poll(b"ignored", &mut output, true, Tick(100), &mut stopped)
                    .unwrap(),
                Progress {
                    consumed: 0,
                    written: 0,
                    status: Status::Complete
                }
            );
            assert_eq!(restored.failure(), None);
            assert_eq!(stopped.remaining(), before);
            assert_eq!(output, *b"M");
        }
        assert_eq!(charged.remaining(), Charge::default());
        let input = b"TWFu".repeat(1000);
        let (output, problem) = fragmented(&input, 0, 3);
        assert_eq!(output, b"Man".repeat(1000));
        assert!(!problem);
    }
    #[test]
    fn budget_and_deadline_failure_retire_state_without_hiding_partial_output() {
        for (input_cap, output_cap, now, expected) in [
            (3, 3, Tick(1), Stop::IoBytes),
            (4, 1, Tick(1), Stop::OutputBytes),
            (4, 3, Tick(100), Stop::Deadline),
        ] {
            let mut decoder = Decoder::default();
            let mut budget = meter(input_cap, output_cap);
            let mut output = [0; 3];
            assert_eq!(
                decoder.poll(b"TWFu", &mut output, true, now, &mut budget),
                Err(expected)
            );
            assert_eq!(decoder.failure(), Some(expected));
            if expected == Stop::OutputBytes {
                assert_eq!(output, [b'M', 0, 0]);
            }
            let mut fresh = meter(100, 100);
            assert_eq!(
                decoder.poll(b"", &mut output, true, Tick(1), &mut fresh),
                Err(expected)
            );
            assert_eq!(
                fresh.remaining(),
                Charge {
                    io_bytes: 100,
                    output_bytes: 100,
                    ..Charge::default()
                }
            );
        }
    }
}
