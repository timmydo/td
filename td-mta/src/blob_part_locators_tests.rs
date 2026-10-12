#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Meter},
    mime_structure::{self, Part},
    ports::{Clock, Deadline, Error as PolicyError, Time},
    wire::{PartLocator, TransferEncoding},
};
use std::sync::atomic::{AtomicU64, Ordering};
pub(crate) struct Timer {
    calls: AtomicU64,
    expires: AtomicU64,
}
impl Timer {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            expires: AtomicU64::new(u64::MAX),
        }
    }
    fn after(&self, n: u64) {
        self.expires
            .store(self.calls.load(Ordering::Relaxed) + n, Ordering::Relaxed);
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            monotonic: Tick(if n >= self.expires.load(Ordering::Relaxed) {
                100
            } else {
                1
            }),
            utc_ms: 0,
        })
    }
}
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 10_000_000,
            output_bytes: 10_000_000,
            unlinks: 0,
        },
    )
}

pub(crate) fn exercise(
    bytes: &[u8],
    extent: std::ops::Range<u64>,
    test: impl FnOnce(blob_structure::Retained<'_, '_, '_, '_, '_, '_>, &Timer),
) {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 4096];
        let mut body = [0; 7];
        let mut parts = [Part::default(); 8];
        let mut headers = [None; 8];
        let mut cursor = blob_structure::Cursor::new(
            &mut source,
            blob_structure::Settings {
                extent,
                limits: mime_structure::Limits {
                    header_bytes: 1024,
                    mime_depth: 4,
                    mime_parts: 8,
                },
            },
            blob_structure::Buffers {
                header_bytes: &mut arena,
                body: &mut body,
                parts: &mut parts,
                headers: &mut headers,
            },
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        for _ in 0..100_000 {
            if cursor.poll(Tick(1)).unwrap() == mime_structure::Status::Complete {
                break;
            }
        }
        test(cursor.finish(Tick(1)).unwrap(), &timer);
    });
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..100 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
    }
    panic!("locator mapping did not finish");
}
pub(crate) const SOURCE: &[u8] = concat!(
    "Content-Type: multipart/mixed;boundary=x\r\n\r\n",
    "--x\r\n\r\nplain\r\n",
    "--x\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n",
    "--x\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\na=3Db\r\n",
    "--x--\r\n"
)
.as_bytes();
#[test]
fn direct_leaf_wire_uses_original_pin_and_absolute_clipped_offsets() {
    for prefix in [0, 17] {
        let mut bytes = vec![b'z'; prefix];
        bytes.extend_from_slice(SOURCE);
        let end = bytes.len();
        bytes.extend_from_slice(b"outside");
        exercise(&bytes, prefix as u64..end as u64, |retained, _| {
            let parent = retained.source.id();
            let original = (
                std::ptr::from_mut(retained.source),
                std::ptr::from_mut(retained.work),
                std::ptr::from_mut(retained.budget),
                retained.parts.as_ptr(),
                retained.headers.as_ptr(),
                retained.remaining.as_ptr(),
                retained.header_bytes,
            );
            let before = retained.work.remaining();
            let header_before = (
                retained.budget.source_bytes_remaining(),
                retained.budget.steps_remaining(),
            );
            let mut cells = [Candidate::default(); 4];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            assert!(cursor.value().is_none());
            for n in 0..4 {
                assert_eq!(
                    cursor.poll(Tick(1)),
                    Ok(if n == 3 {
                        Status::Complete
                    } else {
                        Status::Yield
                    })
                );
            }
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
            let result = cursor.finish(Tick(1)).unwrap();
            assert_eq!(result.candidates.len(), 4);
            assert_eq!(result.candidates[0].ordinal(), 1);
            assert!(result.candidates[0].locator().is_none());
            assert!(result.candidates[0].wire().is_none());
            for (index, (raw, encoding)) in [
                (b"plain".as_slice(), TransferEncoding::Identity),
                (b"YWJj", TransferEncoding::Base64),
                (b"a=3Db", TransferEncoding::QuotedPrintable),
            ]
            .into_iter()
            .enumerate()
            {
                let candidate = &result.candidates[index + 1];
                let offset = bytes.windows(raw.len()).position(|b| b == raw).unwrap() as u64;
                let expected = PartLocator {
                    parent,
                    offset,
                    length: raw.len() as u64,
                    encoding,
                };
                assert_eq!(candidate.ordinal(), (index + 2) as u16);
                assert_eq!(candidate.locator(), Some(expected));
                let mut wire = [0; PartLocator::WIRE_BYTES];
                expected.encode(&mut wire).unwrap();
                assert_eq!(candidate.wire().unwrap().as_bytes(), wire);
            }
            let after = result.structure.work.remaining();
            assert_eq!(before.io_bytes, after.io_bytes);
            assert_eq!(before.records - after.records, 17);
            assert_eq!(
                retained_step_delta(header_before, result.structure.budget),
                (0, 208)
            );
            assert_eq!(
                before.output_bytes - after.output_bytes,
                3 * PartLocator::WIRE_BYTES as u64
            );
            assert_eq!(
                (
                    std::ptr::from_mut(result.structure.source),
                    std::ptr::from_mut(result.structure.work),
                    std::ptr::from_mut(result.structure.budget),
                    result.structure.parts.as_ptr(),
                    result.structure.headers.as_ptr(),
                    result.structure.remaining.as_ptr(),
                    result.structure.header_bytes
                ),
                original
            );
        });
    }
}
#[test]
fn capacity_ordinal_range_and_unfinished_refusals_are_exact() {
    for kind in 0..5 {
        exercise(b"\nbody", 0..5, |retained, _| {
            let mut cells = [Candidate::default(); 2];
            let mut parts = [retained.parts[0]];
            if kind == 2 {
                parts[0].ordinal = 2;
            }
            if kind == 3 {
                parts[0].entity_end = 6;
            }
            let retained = blob_structure::Retained {
                parts: &parts,
                ..retained
            };
            let result = Cursor::new(
                retained,
                &mut cells[..if kind == 0 {
                    0
                } else if kind == 1 {
                    2
                } else {
                    1
                }],
                Tick(1),
            );
            let error = Error::Interpretation(if kind == 0 {
                bound::Error::ResponseCapacity
            } else if kind == 3 {
                bound::Error::InvalidRange
            } else {
                bound::Error::InvalidState
            });
            if kind < 2 {
                assert!(matches!(result, Err(found) if found == error));
            } else {
                let mut cursor = result.unwrap();
                if kind < 4 {
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                }
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
        });
    }
}
#[test]
fn original_output_allowance_and_native_expiry_retire_candidates() {
    for allowance in [0, PartLocator::WIRE_BYTES - 1, PartLocator::WIRE_BYTES] {
        exercise(b"\nbody", 0..5, |retained, _| {
            retained
                .work
                .charge(
                    Tick(1),
                    Charge {
                        output_bytes: retained.work.remaining().output_bytes - allowance as u64,
                        ..Charge::default()
                    },
                )
                .unwrap();
            let mut cells = [Candidate::default(); 1];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            if allowance == PartLocator::WIRE_BYTES {
                drain(&mut cursor).unwrap();
                assert_eq!(
                    cursor
                        .finish(Tick(1))
                        .unwrap()
                        .structure
                        .work
                        .remaining()
                        .output_bytes,
                    0
                );
            } else {
                let error = Error::Interpretation(bound::Error::Admission(
                    crate::nfc::Error::Work(crate::admission::work::Stop::OutputBytes),
                ));
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
        });
    }
    for mode in 0..4 {
        exercise(b"\nbody", 0..5, |retained, timer| {
            let mut cells = [Candidate::default(); 1];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            if mode > 0 {
                drain(&mut cursor).unwrap();
                assert!(cursor.value().is_some());
            }
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(if mode == 3 { 1 } else { 5 });
            let error = Error::Policy(PolicyError::Deadline);
            if mode == 2 {
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            } else {
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
            assert_eq!(
                timer.calls.load(Ordering::Relaxed) - before,
                if mode == 3 { 1 } else { 5 }
            );
        });
    }
}

fn retained_step_delta(before: (u64, u64), budget: &HeaderBudget) -> (u64, u64) {
    (
        before.0 - budget.source_bytes_remaining(),
        before.1 - budget.steps_remaining(),
    )
}
#[test]
fn constructor_job_and_native_fences_precede_capacity_results() {
    for expiry in 1..=7 {
        exercise(b"\nbody", 0..5, |retained, timer| {
            let mut cells = [Candidate::default(); 1];
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(expiry);
            let result = Cursor::new(retained, &mut cells, Tick(1));
            if expiry == 7 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::Policy(PolicyError::Deadline))));
            }
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, expiry.min(6));
        });
    }
    for native in [false, true] {
        exercise(b"\nbody", 0..5, |retained, timer| {
            if native {
                timer.after(3);
            }
            let error = if native {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
                    crate::admission::work::Stop::Deadline,
                )))
            };
            assert!(matches!(Cursor::new(retained,&mut [],Tick(100)),Err(found) if found == error));
        });
    }
    exercise(b"\nbody", 0..5, |retained, timer| {
        timer.after(5);
        assert!(matches!(
            Cursor::new(retained, &mut [], Tick(1)),
            Err(Error::Policy(PolicyError::Deadline))
        ));
    });
    exercise(b"\nbody", 0..5, |retained, _| {
        let mut cells = [Candidate::default(); 1];
        let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
        drain(&mut cursor).unwrap();
        let error = Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
            crate::admission::work::Stop::Deadline,
        )));
        assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
    });
}
#[test]
fn clipped_root_prefix_suffix_and_inverted_leaf_ranges_refuse() {
    for kind in 0..3 {
        let mut bytes = vec![b'z'; 17];
        bytes.extend_from_slice(SOURCE);
        let end = bytes.len();
        bytes.extend_from_slice(b"outside");
        exercise(&bytes, 17..end as u64, |retained, _| {
            let mut parts = retained.parts.to_vec();
            match kind {
                0 => parts[1].body_start = 0,
                1 => parts[1].entity_end = end as u64 + 1,
                _ => parts[1].entity_end = parts[1].body_start - 1,
            }
            let retained = blob_structure::Retained {
                parts: &parts,
                ..retained
            };
            let mut cells = [Candidate::default(); 4];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            let error = Error::Interpretation(bound::Error::InvalidRange);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(cursor.value().is_none());
            assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
        });
    }
    exercise(b"\nbody", 0..5, |retained, _| {
        let retained = blob_structure::Retained {
            parts: &[],
            ..retained
        };
        assert!(matches!(
            Cursor::new(retained, &mut [], Tick(1)),
            Err(Error::Interpretation(bound::Error::InvalidState))
        ));
    });
}
#[test]
fn empty_and_embedded_leaf_ranges_pin_rounding_and_wire_policy() {
    let message = b"Content-Type: message/rfc822\r\n\r\n\r\nplain";
    for (bytes, start, offset, length) in [
        (b"\n".as_slice(), 0, 1, 0),
        (message.as_slice(), 0, 32, 7),
        (message.as_slice(), 32, 34, 5),
    ] {
        exercise(bytes, start..bytes.len() as u64, |retained, _| {
            let parent = retained.source.id();
            let before = retained.work.remaining();
            assert_eq!(retained.parts.len(), 1);
            let mut cells = [Candidate::default(); 1];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            drain(&mut cursor).unwrap();
            let result = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                before.records - result.structure.work.remaining().records,
                6
            );
            assert_eq!(
                before.output_bytes - result.structure.work.remaining().output_bytes,
                69
            );
            let candidate = &result.candidates[0];
            assert_eq!(
                candidate.locator(),
                Some(PartLocator {
                    parent,
                    offset,
                    length,
                    encoding: TransferEncoding::Identity
                })
            );
            assert_eq!(
                PartLocator::decode(candidate.wire().unwrap()).unwrap(),
                candidate.locator().unwrap()
            );
        });
    }
}

#[test]
fn original_steps_records_and_late_mapping_refusals_remain_sticky() {
    for (steps, records, late) in [
        (68, 100, false),
        (69, 5, false),
        (69, 6, false),
        (69, 0, true),
    ] {
        exercise(b"\nbody", 0..5, |retained, timer| {
            retained
                .budget
                .charge(
                    retained.work,
                    Tick(1),
                    0,
                    retained.budget.steps_remaining() - steps,
                    &mut Credit::new(),
                )
                .unwrap();
            retained
                .work
                .charge(
                    Tick(1),
                    Charge {
                        records: retained.work.remaining().records - records,
                        ..Charge::default()
                    },
                )
                .unwrap();
            let mut cells = [Candidate::default(); 1];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            if late {
                timer.after(5);
            }
            if steps == 69 && records == 6 {
                drain(&mut cursor).unwrap();
                assert_eq!(
                    cursor
                        .finish(Tick(1))
                        .unwrap()
                        .structure
                        .work
                        .remaining()
                        .records,
                    0
                );
                return;
            }
            let error = if late {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Interpretation(bound::Error::Admission(if steps < 69 {
                    crate::nfc::Error::InterpretationLimit
                } else {
                    crate::nfc::Error::Work(crate::admission::work::Stop::Records)
                }))
            };
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
        });
    }
}
#[test]
fn cached_poll_and_finish_sample_job_time_and_refused_cells_are_provisional() {
    for finish in [false, true] {
        exercise(b"\nbody", 0..5, |retained, _| {
            let mut cells = [Candidate::default(); 1];
            let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
            drain(&mut cursor).unwrap();
            let error = Error::Interpretation(bound::Error::Admission(crate::nfc::Error::Work(
                crate::admission::work::Stop::Deadline,
            )));
            if finish {
                assert!(matches!(cursor.finish(Tick(100)), Err(found) if found == error));
            } else {
                assert_eq!(cursor.poll(Tick(100)), Err(error));
                assert!(cursor.value().is_none());
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
        });
    }
    exercise(b"\nbody", 0..5, |retained, timer| {
        let mut cells = [Candidate::default(); 1];
        let mut cursor = Cursor::new(retained, &mut cells, Tick(1)).unwrap();
        timer.after(5);
        assert_eq!(
            cursor.poll(Tick(1)),
            Err(Error::Policy(PolicyError::Deadline))
        );
        assert!(cursor.value().is_none());
        assert!(matches!(
            cursor.finish(Tick(1)),
            Err(Error::Policy(PolicyError::Deadline))
        ));
        assert!(cells[0].locator().is_some());
    });
}
