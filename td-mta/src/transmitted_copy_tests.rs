#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{ports::Tick, store_fs::with_pinned_fixture};

struct Now(AtomicU64);
impl Clock for Now {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn limits(maximum: u64) -> Limits {
    Limits {
        header_bytes: 8192,
        message_bytes: maximum,
        deadline: deadline(),
    }
}
fn meter() -> Meter {
    Meter::new(
        deadline(),
        Charge {
            io_bytes: 1_000_000,
            output_bytes: 1_000_000,
            records: 100_000,
            unlinks: 0,
        },
    )
}
#[test]
fn native_copy_removes_only_blind_headers_and_streams_every_body_byte() {
    let mut message = b"From: a@b.test\r\nBcc: hidden@b.test\r\n more@b.test\r\nResent-Bcc: other@b.test\r\nSubject: keep\r\n\r\n".to_vec();
    let body: Vec<u8> = (0..21000).map(|n| (n % 256) as u8).collect();
    message.extend_from_slice(&body);
    let clock = Now(AtomicU64::new(1));
    with_pinned_fixture(&message, &clock, |mut pin| {
        let mut output = Vec::new();
        let n = write(
            &mut pin,
            &mut [0; 8194],
            limits(30000),
            &clock,
            &mut meter(),
            |bytes| {
                assert!(bytes.len() <= 4096);
                output.extend_from_slice(bytes);
                Ok(())
            },
        )
        .unwrap();
        let mut expected = b"From: a@b.test\r\nSubject: keep\r\n\r\n".to_vec();
        expected.extend_from_slice(&body);
        assert_eq!(output, expected);
        assert_eq!(n, expected.len() as u64);
        let mut original = vec![0; message.len()];
        assert_eq!(pin.read_at(0, &mut original).unwrap(), message.len());
        assert_eq!(original, message);
    });
}
#[test]
fn exact_filtered_size_and_original_work_are_enforced() {
    let input = b"X: value\r\nBcc: hidden@b.test\r\n\r\nbody";
    let expected = b"X: value\r\n\r\nbody";
    let clock = Now(AtomicU64::new(1));
    with_pinned_fixture(input, &clock, |mut pin| {
        let mut full = meter();
        assert_eq!(
            write(
                &mut pin,
                &mut [0; 8194],
                limits(expected.len() as u64),
                &clock,
                &mut full,
                |_| Ok(())
            )
            .unwrap(),
            expected.len() as u64
        );
        assert_eq!(
            write(
                &mut pin,
                &mut [0; 8194],
                limits(expected.len() as u64 - 1),
                &clock,
                &mut meter(),
                |_| Ok(())
            ),
            Err(Error::Quota)
        );
        let allowance = meter().remaining();
        let used = Charge {
            io_bytes: allowance.io_bytes - full.remaining().io_bytes,
            output_bytes: allowance.output_bytes - full.remaining().output_bytes,
            records: allowance.records - full.remaining().records,
            unlinks: 0,
        };
        for kind in 0..3 {
            let total = match kind {
                0 => used.io_bytes,
                1 => used.output_bytes,
                _ => used.records,
            };
            for amount in [0, total - 1, total] {
                let mut allowed = used;
                match kind {
                    0 => allowed.io_bytes = amount,
                    1 => allowed.output_bytes = amount,
                    _ => allowed.records = amount,
                }
                let result = write(
                    &mut pin,
                    &mut [0; 8194],
                    limits(1000),
                    &clock,
                    &mut Meter::new(deadline(), allowed),
                    |_| Ok(()),
                );
                if amount == total {
                    assert_eq!(result.unwrap(), expected.len() as u64);
                } else {
                    assert_eq!(result, Err(Error::Capacity), "kind={kind} amount={amount}");
                }
            }
        }
    });
}
#[test]
fn invalid_headers_bounds_and_caller_limits_refuse() {
    let clock = Now(AtomicU64::new(1));
    for input in [
        b"X: a\rBcc: hidden\r\n\r\nbody".as_slice(),
        b"X: a\n\nbody",
        b"X: no separator",
    ] {
        with_pinned_fixture(input, &clock, |mut pin| {
            assert_eq!(
                write(
                    &mut pin,
                    &mut [0; 8194],
                    limits(1000),
                    &clock,
                    &mut meter(),
                    |_| Ok(())
                ),
                Err(Error::Invalid)
            );
        });
    }
    with_pinned_fixture(b"X: value\r\n\r\nbody", &clock, |mut pin| {
        for bounds in [
            Limits {
                message_bytes: 0,
                ..limits(1000)
            },
            Limits {
                header_bytes: 0,
                ..limits(1000)
            },
            Limits {
                header_bytes: crate::limits::MIB as u64 + 1,
                ..limits(1000)
            },
            Limits {
                deadline: Deadline::after(Tick(0), 101).unwrap(),
                ..limits(1000)
            },
        ] {
            assert_eq!(
                write(
                    &mut pin,
                    &mut [0; 8194],
                    bounds,
                    &clock,
                    &mut meter(),
                    |_| panic!("invalid caller reached sink")
                ),
                Err(Error::Invalid)
            );
        }
        assert_eq!(
            write(
                &mut pin,
                &mut [0; 2],
                limits(1000),
                &clock,
                &mut meter(),
                |_| Ok(())
            ),
            Err(Error::Capacity)
        );
        assert_eq!(
            write(
                &mut pin,
                &mut [0; 8194],
                Limits {
                    header_bytes: 4,
                    ..limits(1000)
                },
                &clock,
                &mut meter(),
                |_| Ok(())
            ),
            Err(Error::Capacity)
        );
    });
}
#[test]
fn phase_expiry_regression_and_sink_failure_retire_provisional_output() {
    for (next, expected) in [(50, Error::Deadline), (0, Error::Invalid)] {
        let clock = Now(AtomicU64::new(1));
        with_pinned_fixture(b"X: value\r\n\r\nbody", &clock, |mut pin| {
            let mut bytes = Vec::new();
            let mut original = meter();
            assert_eq!(
                write(
                    &mut pin,
                    &mut [0; 8194],
                    Limits {
                        deadline: Deadline::after(Tick(0), 50).unwrap(),
                        ..limits(1000)
                    },
                    &clock,
                    &mut original,
                    |chunk| {
                        bytes.extend_from_slice(chunk);
                        clock.0.store(next, Ordering::Relaxed);
                        Ok(())
                    }
                ),
                Err(expected)
            );
            assert!(!bytes.is_empty());
            assert_eq!(original.deadline(), deadline().into());
        });
    }
    let clock = Now(AtomicU64::new(1));
    with_pinned_fixture(b"\r\nbody", &clock, |mut pin| {
        let mut calls = 0;
        assert_eq!(
            write(
                &mut pin,
                &mut [0; 8194],
                limits(1000),
                &clock,
                &mut meter(),
                |_| {
                    calls += 1;
                    Err(Error::Busy)
                }
            ),
            Err(Error::Busy)
        );
        assert_eq!(calls, 1);
    });
}
#[test]
fn original_native_pin_deadline_cannot_be_extended_by_copy_limits() {
    let clock = Now(AtomicU64::new(1));
    with_pinned_fixture(b"X: first\r\nY: second\r\n\r\nbody", &clock, |mut pin| {
        let mut calls = 0;
        let longer = Deadline::after(Tick(0), 200).unwrap();
        let mut budget = Meter::new(longer, meter().remaining());
        assert_eq!(
            write(
                &mut pin,
                &mut [0; 8194],
                Limits {
                    deadline: longer,
                    ..limits(1000)
                },
                &clock,
                &mut budget,
                |_| {
                    calls += 1;
                    clock.0.store(100, Ordering::Relaxed);
                    Ok(())
                }
            ),
            Err(Error::Deadline)
        );
        assert!(pin.is_failed());
        assert_eq!(calls, 1);
    });
}
