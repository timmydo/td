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
        // Small output windows cannot amortize quartet admission.
        let result = if output.len() >= 4 {
            self.advance::<true>(input, output, last, now, meter)
        } else {
            self.advance::<false>(input, output, last, now, meter)
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn advance<const GROUP: bool>(
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
        let mut transitions = 0;
        let mut prepaid = 0;
        while transitions < STEP_TRANSITIONS {
            if GROUP
                && prepaid == 0
                && self.pending == 0
                && self.sextets == 0
                && !self.ended
                && !self.eof
                && STEP_TRANSITIONS - transitions >= 7
                && meter.remaining().io_bytes >= 4
                && meter.remaining().output_bytes >= 3
            {
                if let (Some(input), Some(output)) = (
                    input.get(progress.consumed..).and_then(|v| v.get(..4)),
                    output
                        .get_mut(progress.written..)
                        .and_then(|v| v.get_mut(..3)),
                ) {
                    // Reserve the four source visits before inspecting this quartet.
                    meter.charge(
                        now,
                        Charge {
                            io_bytes: 4,
                            ..Charge::default()
                        },
                    )?;
                    let mut values = [0u8; 4];
                    let mut ordinary = true;
                    for (byte, value) in input.iter().zip(values.iter_mut()) {
                        if let Some(decoded) = sextet(*byte) {
                            *value = decoded;
                        } else {
                            ordinary = false;
                        }
                    }
                    if ordinary {
                        meter.charge(
                            now,
                            Charge {
                                output_bytes: 3,
                                ..Charge::default()
                            },
                        )?;
                        let [a, b, c, d] = values;
                        output.copy_from_slice(&[
                            (a << 2) | (b >> 4),
                            (b << 4) | (c >> 2),
                            (c << 6) | d,
                        ]);
                        progress.consumed += 4;
                        progress.written += 3;
                        transitions += 7;
                        continue;
                    }
                    // The scalar path consumes this reserved prefix within seven turns.
                    prepaid = 4;
                }
            }
            transitions += 1;
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
                if prepaid != 0 {
                    prepaid -= 1;
                } else {
                    meter.charge(
                        now,
                        Charge {
                            io_bytes: 1,
                            ..Charge::default()
                        },
                    )?;
                }
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
        let Some(value) = sextet(byte) else {
            self.problem = true;
            return;
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

const SEXTETS: &[u8] = &[
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 62, 255, 255, 255, 63, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 255,
    255, 255, 255, 255, 255, 255, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    19, 20, 21, 22, 23, 24, 25, 255, 255, 255, 255, 255, 255, 26, 27, 28, 29, 30, 31, 32, 33, 34,
    35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
];

fn sextet(byte: u8) -> Option<u8> {
    let value = *SEXTETS.get(usize::from(byte))?;
    (value != 255).then_some(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn lookup_classifies_every_octet() {
        assert_eq!(SEXTETS.len(), 256);
        for byte in u8::MIN..=u8::MAX {
            let expected = match byte {
                b'A'..=b'Z' => Some(byte - b'A'),
                b'a'..=b'z' => Some(byte - b'a' + 26),
                b'0'..=b'9' => Some(byte - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            };
            assert_eq!(sextet(byte), expected);
        }
    }

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
    fn decode_chunks(input: &[u8], fragment: usize, width: usize) -> (Vec<u8>, bool, Charge) {
        let mut decoder = Decoder::default();
        let mut work = meter(input.len() as u64, input.len() as u64);
        let mut result = Vec::new();
        let mut consumed = 0;
        let mut output = vec![0; width];
        for _ in 0..input.len() * 4 + 10 {
            let end = (consumed + fragment).min(input.len());
            let progress = decoder
                .poll(
                    &input[consumed..end],
                    &mut output,
                    end == input.len(),
                    Tick(1),
                    &mut work,
                )
                .unwrap();
            consumed += progress.consumed;
            result.extend_from_slice(&output[..progress.written]);
            if progress.status == Status::Complete {
                assert_eq!(consumed, input.len());
                return (result, decoder.is_encoding_problem(), work.remaining());
            }
        }
        panic!("decoder did not complete");
    }
    #[test]
    fn quartet_path_matches_scalar_fragments_for_every_octet_and_output_width() {
        for byte in 0..=255u8 {
            for position in 0..8 {
                let mut input = b"TWFu".repeat(70);
                input[position] = byte;
                input.extend_from_slice(b"\r\n TQ==Z");
                let expected = decode_chunks(&input, 1, 1);
                for width in [1, 2, 3, 4, 32, 257] {
                    assert_eq!(
                        decode_chunks(&input, input.len(), width),
                        expected,
                        "octet {byte} position {position} width {width}"
                    );
                }
            }
        }
    }
    #[test]
    fn quartet_path_preserves_transition_boundary_and_pending_output() {
        let input = b"TWFu".repeat(100);
        let mut decoder = Decoder::default();
        let mut work = meter(400, 300);
        let mut output = [0; 300];
        let progress = decoder
            .poll(&input, &mut output, true, Tick(1), &mut work)
            .unwrap();
        assert_eq!(
            progress,
            Progress {
                consumed: 148,
                written: 108,
                status: Status::Yield
            }
        );
        assert_eq!(&output[..108], b"Man".repeat(36));
        assert_eq!(work.remaining().io_bytes, 252);
        assert_eq!(work.remaining().output_bytes, 192);
        assert_eq!(decoder.pending, 3);
        assert_eq!(decoder.output, 0x4d616e);
        for (input_cap, output_cap, expected) in [(7, 6, Stop::IoBytes), (8, 4, Stop::OutputBytes)]
        {
            let mut decoder = Decoder::default();
            let mut work = meter(input_cap, output_cap);
            let mut output = [0; 6];
            assert_eq!(
                decoder.poll(b"TWFuTWFu", &mut output, true, Tick(1), &mut work),
                Err(expected)
            );
            assert_eq!(&output[..3], b"Man");
            if expected == Stop::OutputBytes {
                assert_eq!(output, *b"ManM\0\0");
            }
            assert_eq!(decoder.failure(), Some(expected));
        }
    }
}
