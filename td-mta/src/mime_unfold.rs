//! Byte-preserving header unfolding before form-specific text processing.
pub use crate::mime_base64::{Progress, Status};
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
};
const STEP_TRANSITIONS: usize = 256;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Held {
    #[default]
    None,
    Cr,
    Lf,
    CrLf,
}
#[derive(Clone, Copy, Default)]
pub struct Decoder {
    held: Held,
    output: u16,
    pending: u8,
    eof: bool,
    failure: Option<Stop>,
}
impl Decoder {
    pub const fn failure(&self) -> Option<Stop> {
        self.failure
    }
    fn flush(&mut self) {
        (self.output, self.pending) = match self.held {
            Held::None => (0, 0),
            Held::Cr => (u16::from(b'\r'), 1),
            Held::Lf => (u16::from(b'\n'), 1),
            Held::CrLf => (u16::from(b'\r') | (u16::from(b'\n') << 8), 2),
        };
        self.held = Held::None;
    }
    /// Retain the unconsumed suffix and EOF flag through NeedOutput/Yield.
    /// On refusal, discard partial output; no partial value is successful.
    /// The caller brackets deterministic turns with fresh clock/cancellation checks.
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
        let mut step = Progress {
            consumed: 0,
            written: 0,
            status: Status::Yield,
        };
        for _ in 0..STEP_TRANSITIONS {
            meter.charge(now, Charge::default())?;
            if self.pending != 0 {
                let Some(target) = output.get_mut(step.written) else {
                    step.status = Status::NeedOutput;
                    return Ok(step);
                };
                meter.charge(
                    now,
                    Charge {
                        output_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                *target = self.output as u8;
                self.output >>= 8;
                self.pending -= 1;
                step.written += 1;
                continue;
            }
            if self.eof {
                step.status = Status::Complete;
                return Ok(step);
            }
            let Some(byte) = input.get(step.consumed).copied() else {
                if !last {
                    step.status = Status::NeedInput;
                    return Ok(step);
                }
                self.flush();
                self.eof = true;
                continue;
            };
            meter.charge(
                now,
                Charge {
                    io_bytes: 1,
                    ..Charge::default()
                },
            )?;
            match self.held {
                Held::Cr if byte == b'\n' => {
                    self.held = Held::CrLf;
                    step.consumed += 1;
                }
                Held::Lf | Held::CrLf if matches!(byte, b' ' | b'\t') => {
                    self.held = Held::None;
                    self.output = u16::from(byte);
                    self.pending = 1;
                    step.consumed += 1;
                }
                Held::Cr | Held::Lf | Held::CrLf => self.flush(),
                Held::None => {
                    step.consumed += 1;
                    self.held = match byte {
                        b'\r' => Held::Cr,
                        b'\n' => Held::Lf,
                        _ => {
                            self.output = u16::from(byte);
                            self.pending = 1;
                            Held::None
                        }
                    };
                }
            }
        }
        Ok(step)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn meter(io: u64, output: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                output_bytes: output,
                ..Charge::default()
            },
        )
    }
    fn unfold(bytes: &[u8], split: usize, capacity: usize) -> Vec<u8> {
        let mut decoder = Decoder::default();
        assert!(std::mem::size_of_val(&decoder) <= 16);
        let mut work = meter(100_000, 100_000);
        let mut result = Vec::new();
        for (input, last) in [(&bytes[..split], false), (&bytes[split..], true)] {
            let mut consumed = 0;
            for _ in 0..10000 {
                let mut output = [0; 4];
                let step = decoder
                    .poll(
                        &input[consumed..],
                        &mut output[..capacity],
                        last,
                        Tick(1),
                        &mut work,
                    )
                    .unwrap();
                assert!(step.consumed + step.written <= STEP_TRANSITIONS);
                consumed += step.consumed;
                result.extend_from_slice(&output[..step.written]);
                match step.status {
                    Status::NeedInput => {
                        assert!(!last);
                        assert_eq!(consumed, input.len());
                        break;
                    }
                    Status::Complete => {
                        assert_eq!(consumed, input.len());
                        assert_eq!(
                            decoder
                                .poll(b"ignored", &mut [], true, Tick(100), &mut work)
                                .unwrap(),
                            Progress {
                                consumed: 0,
                                written: 0,
                                status: Status::Complete
                            }
                        );
                        return result;
                    }
                    _ => {}
                }
            }
        }
        panic!("unfolder did not complete");
    }
    #[test]
    fn every_fragment_and_short_output_preserves_exact_nonfold_bytes() {
        for (input, expected) in [
            (b"".as_slice(), b"".as_slice()),
            (b" a\0\xff\t".as_slice(), b" a\0\xff\t".as_slice()),
            (b"a\r\n b".as_slice(), b"a b".as_slice()),
            (b"a\n\tb".as_slice(), b"a\tb".as_slice()),
            (b"\r\n \r\n\tend".as_slice(), b" \tend".as_slice()),
            (b"\r".as_slice(), b"\r".as_slice()),
            (b"\n".as_slice(), b"\n".as_slice()),
            (b"\r\n".as_slice(), b"\r\n".as_slice()),
            (b"\r a".as_slice(), b"\r a".as_slice()),
            (b"\r\r\n a".as_slice(), b"\r a".as_slice()),
            (b"\r\n\na".as_slice(), b"\r\n\na".as_slice()),
            (b"\r\n\r\n a".as_slice(), b"\r\n a".as_slice()),
        ] {
            for split in 0..=input.len() {
                for capacity in 1..=4 {
                    assert_eq!(
                        unfold(input, split, capacity),
                        expected,
                        "{input:?} split{split} cap{capacity}"
                    );
                }
            }
        }
    }
    #[test]
    fn backpressure_checkpoint_replay_and_lookahead_charge_exact_work() {
        let mut decoder = Decoder::default();
        let mut work = meter(10, 10);
        assert_eq!(
            decoder
                .poll(b"\r", &mut [], false, Tick(1), &mut work)
                .unwrap(),
            Progress {
                consumed: 1,
                written: 0,
                status: Status::NeedInput
            }
        );
        let checkpoint = decoder;
        assert_eq!(
            decoder
                .poll(b"a", &mut [], true, Tick(1), &mut work)
                .unwrap(),
            Progress {
                consumed: 0,
                written: 0,
                status: Status::NeedOutput
            }
        );
        let mut output = [0; 2];
        assert_eq!(
            decoder
                .poll(b"a", &mut output, true, Tick(1), &mut work)
                .unwrap(),
            Progress {
                consumed: 1,
                written: 2,
                status: Status::Complete
            }
        );
        assert_eq!(output, *b"\ra");
        assert_eq!(
            (work.remaining().io_bytes, work.remaining().output_bytes),
            (7, 8)
        );
        decoder = checkpoint;
        assert_eq!(
            decoder
                .poll(b"\n ", &mut output, true, Tick(1), &mut work)
                .unwrap(),
            Progress {
                consumed: 2,
                written: 1,
                status: Status::Complete
            }
        );
        assert_eq!(output[0], b' ');
        assert_eq!(
            (work.remaining().io_bytes, work.remaining().output_bytes),
            (5, 7)
        );
        let mut decoder = Decoder::default();
        let mut exact = meter(3, 3);
        let mut partial = [0xee; 3];
        assert_eq!(
            decoder.poll(b"\r\na", &mut partial, true, Tick(1), &mut exact),
            Err(Stop::IoBytes)
        );
        assert_eq!(partial, [b'\r', b'\n', 0xee]);
        let mut fresh = meter(100, 100);
        let before = fresh.remaining();
        assert_eq!(
            decoder.poll(b"a", &mut [0; 3], true, Tick(1), &mut fresh),
            Err(Stop::IoBytes)
        );
        assert_eq!(fresh.remaining(), before);
    }
    #[test]
    fn transition_ceiling_and_refusal_keep_work_bounded() {
        let mut decoder = Decoder::default();
        let mut work = meter(10000, 10000);
        let input = [b'x'; 4096];
        let mut output = [0; 4096];
        let step = decoder
            .poll(&input, &mut output, false, Tick(1), &mut work)
            .unwrap();
        assert_eq!(
            step,
            Progress {
                consumed: 128,
                written: 128,
                status: Status::Yield
            }
        );
        assert!(output[..128].iter().all(|byte| *byte == b'x'));
        for (io, out, tick, reason) in [
            (0, 1, 1, Stop::IoBytes),
            (1, 0, 1, Stop::OutputBytes),
            (1, 1, 100, Stop::Deadline),
        ] {
            let mut decoder = Decoder::default();
            let mut work = meter(io, out);
            assert_eq!(
                decoder.poll(b"a", &mut [0; 1], true, Tick(tick), &mut work),
                Err(reason)
            );
            let mut fresh = meter(10, 10);
            let before = fresh.remaining();
            assert_eq!(
                decoder.poll(b"a", &mut [0; 1], true, Tick(1), &mut fresh),
                Err(reason)
            );
            assert_eq!(fresh.remaining(), before);
            assert_eq!(decoder.failure(), Some(reason));
        }
    }
}
