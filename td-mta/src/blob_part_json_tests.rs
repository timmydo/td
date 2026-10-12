#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Meter},
    mime_part_headers::{self, label_json},
    mime_structure,
    ports::{Clock, Deadline, Error as PolicyError, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
pub(crate) struct Timer {
    calls: AtomicU64,
    expires: AtomicU64,
}
impl Timer {
    pub(crate) fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            expires: AtomicU64::new(u64::MAX),
        }
    }
    pub(crate) fn after(&self, n: u64) {
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
struct Storage {
    heads: [u8; 256],
    charset: [u8; 256],
    filename: [u8; 256],
    id: [u8; 256],
    language: [u8; 256],
    location: [u8; 256],
}
impl Storage {
    fn new() -> Self {
        Self {
            heads: [0; 256],
            charset: [0; 256],
            filename: [0; 256],
            id: [0; 256],
            language: [0; 256],
            location: [0; 256],
        }
    }
    fn backing(&mut self) -> label_json::Backing<'_> {
        label_json::Backing {
            headers: mime_part_headers::Backing {
                heads: &mut self.heads,
                charset: &mut self.charset,
                filename: &mut self.filename,
            },
            labels: crate::mime_label_fields::json::Backing {
                content_id: &mut self.id,
                content_language: &mut self.language,
            },
            content_location: &mut self.location,
        }
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

pub(crate) fn exercise_part(
    ordinal: u16,
    bytes: &[u8],
    timer: &Timer,
    test: impl FnOnce(blob_part::Retained<'_, '_, '_, '_, '_, '_>),
) {
    crate::store_fs::with_pinned_fixture(bytes, timer, |mut source| {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut arena = [0; 2048];
        let mut body = [0; 7];
        let mut parts = [Part::default(); 8];
        let mut headers = [None; 8];
        let settings = blob_structure::Settings {
            extent: 0..bytes.len() as u64,
            limits: mime_structure::Limits {
                header_bytes: 1024,
                mime_depth: 4,
                mime_parts: 8,
            },
        };
        let mut cursor = blob_structure::Cursor::new(
            &mut source,
            settings,
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
        let mut cursor = blob_part::Cursor::new(
            cursor.finish(Tick(1)).unwrap(),
            ordinal,
            storage.backing(),
            &mut scratch,
            Tick(1),
        )
        .unwrap();
        for _ in 0..100_000 {
            if cursor.poll(Tick(1)).unwrap() == mime_part_headers::Status::Complete {
                break;
            }
        }
        test(cursor.finish(Tick(1)).unwrap());
    });
}
fn exercise(
    bytes: &[u8],
    timer: &Timer,
    test: impl FnOnce(blob_part::Retained<'_, '_, '_, '_, '_, '_>),
) {
    exercise_part(1, bytes, timer, test);
}
pub(crate) const BYTES: &[u8] = concat!(
    "Content-Type: text/html;charset=UTF-8\r\n",
    "Content-Disposition: attachment;filename*=utf-8''caf%C3%A9.html\r\n",
    "Content-ID: <x@example.org>\r\nContent-Language: en, fr\r\n",
    "Content-Location: ../p\r\n\r\n<b>text</b>"
)
.as_bytes();
const EXPECTED: &str = r#""partId":"1","size":11,"type":"text/html","charset":"UTF-8","name":"café.html","disposition":"attachment","cid":"x@example.org","language":["en","fr"],"location":"../p""#;
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_>, width: usize) -> Result<Vec<u8>, Error> {
    let mut result = Vec::new();
    let mut output = vec![0; width];
    for _ in 0..100_000 {
        let progress = cursor.poll(Tick(1), &mut output)?;
        assert!(progress.written <= 64);
        result.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            return Ok(result);
        }
    }
    panic!("native metadata JSON did not finish");
}
#[test]
fn native_metadata_members_match_literal_wire_at_short_and_wide_output_widths() {
    for width in [1, 7, 64, 128] {
        let timer = Timer::new();
        exercise(BYTES, &timer, |retained| {
            let pointers = (
                std::ptr::from_mut(retained.structure.source),
                std::ptr::from_mut(retained.structure.work),
                std::ptr::from_mut(retained.structure.budget),
                std::ptr::from_mut(retained.scratch),
            );
            let mut cursor = Cursor::new(retained, Tick(1)).unwrap();
            let progress = cursor.poll(Tick(1), &mut []).unwrap();
            assert_eq!(progress.written, 0);
            assert_eq!(progress.status, Status::NeedOutput);
            assert_eq!(drain(&mut cursor, width).unwrap(), EXPECTED.as_bytes());
            let retained = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.end.part.size, 11);
            assert_eq!(
                (
                    std::ptr::from_mut(retained.structure.source),
                    std::ptr::from_mut(retained.structure.work),
                    std::ptr::from_mut(retained.structure.budget),
                    std::ptr::from_mut(retained.scratch)
                ),
                pointers
            );
        });
    }
}
#[test]
fn output_quota_is_exact_and_refusal_hides_completion() {
    for allowance in [0, 1, EXPECTED.len() - 1, EXPECTED.len()] {
        let timer = Timer::new();
        exercise(BYTES, &timer, |retained| {
            retained
                .structure
                .work
                .charge(
                    Tick(1),
                    Charge {
                        output_bytes: retained.structure.work.remaining().output_bytes
                            - allowance as u64,
                        ..Charge::default()
                    },
                )
                .unwrap();
            let mut cursor = Cursor::new(retained, Tick(1)).unwrap();
            let result = drain(&mut cursor, 7);
            if allowance == EXPECTED.len() {
                assert_eq!(result.unwrap(), EXPECTED.as_bytes());
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
                let error = result.unwrap_err();
                assert_eq!(
                    error,
                    Error::Interpretation(crate::mime_response::bound::Error::Admission(
                        crate::nfc::Error::Work(crate::admission::work::Stop::OutputBytes)
                    ))
                );
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
        });
    }
}
#[test]
fn native_post_turn_cached_and_finish_expiry_withhold_success() {
    for mode in 0..3 {
        let timer = Timer::new();
        exercise(BYTES, &timer, |retained| {
            let mut cursor = Cursor::new(retained, Tick(1)).unwrap();
            if mode > 0 {
                drain(&mut cursor, 64).unwrap();
                assert!(cursor.value().is_some());
            }
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(5);
            let error = Error::Policy(PolicyError::Deadline);
            if mode < 2 {
                let mut output = [0xa5; 64];
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                if mode == 0 {
                    assert_eq!(&output[..9], &EXPECTED.as_bytes()[..9]);
                }
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            } else {
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            }
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, 5);
        });
    }
}
#[test]
fn job_expiry_and_unfinished_finish_refuse() {
    for expired in [false, true] {
        let timer = Timer::new();
        exercise(BYTES, &timer, |retained| {
            let mut cursor = Cursor::new(retained, Tick(1)).unwrap();
            if expired {
                let error = cursor.poll(Tick(100), &mut [0; 64]).unwrap_err();
                assert_eq!(
                    error,
                    Error::Interpretation(crate::mime_response::bound::Error::Admission(
                        crate::nfc::Error::Work(crate::admission::work::Stop::Deadline)
                    ))
                );
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
                assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            } else {
                assert!(matches!(
                    cursor.finish(Tick(1)),
                    Err(Error::Interpretation(
                        crate::mime_response::bound::Error::InvalidState
                    ))
                ));
            }
        });
    }
}
fn counters(work: &Meter, budget: &HeaderBudget) -> [u64; 5] {
    let left = work.remaining();
    [
        budget.source_bytes_remaining(),
        budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
#[test]
fn native_and_resident_nested_members_and_all_costs_agree() {
    let long = format!("Content-Location: ../{}\r\n\r\nbody", "a".repeat(180));
    let nested = b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nplain\r\n--x\r\nContent-Type: application/octet-stream\r\n\r\nabc\r\n--x--\r\n";
    for (source, count) in [
        (BYTES, 1),
        (b"\r\nplain".as_slice(), 1),
        (long.as_bytes(), 1),
        (nested.as_slice(), 3),
    ] {
        for ordinal in 1..=count {
            for width in [1, 7, 64, 128] {
                let (expected, expected_cost, expected_end) =
                    part_json::tests::exercise(source, 0, ordinal as usize, |mut cursor| {
                        let before = part_json::tests::snapshot(&cursor);
                        let mut bytes = Vec::new();
                        for _ in 0..100_000 {
                            let mut output = [0; 128];
                            let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
                            bytes.extend_from_slice(&output[..progress.written]);
                            if progress.status == Status::Complete {
                                break;
                            }
                        }
                        let (end, work, budget, _) = cursor.finish(Tick(1)).unwrap();
                        let after = counters(work, budget);
                        (
                            (
                                bytes,
                                std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                                end,
                            ),
                            true,
                        )
                    });
                let timer = Timer::new();
                exercise_part(ordinal, source, &timer, |retained| {
                    let before = counters(retained.structure.work, retained.structure.budget);
                    let identity = (
                        retained.structure.parts.as_ptr(),
                        retained.structure.parts.len(),
                        retained.structure.headers.as_ptr(),
                        retained.structure.headers.len(),
                        retained.structure.remaining.as_ptr(),
                        retained.structure.remaining.len(),
                        retained.structure.header_bytes,
                    );
                    let mut cursor = Cursor::new(retained, Tick(1)).unwrap();
                    assert_eq!(drain(&mut cursor, width).unwrap(), expected);
                    let retained = cursor.finish(Tick(1)).unwrap();
                    let after = counters(retained.structure.work, retained.structure.budget);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        expected_cost
                    );
                    assert_eq!(retained.end, expected_end);
                    assert_eq!(
                        (
                            retained.structure.parts.as_ptr(),
                            retained.structure.parts.len(),
                            retained.structure.headers.as_ptr(),
                            retained.structure.headers.len(),
                            retained.structure.remaining.as_ptr(),
                            retained.structure.remaining.len(),
                            retained.structure.header_bytes
                        ),
                        identity
                    );
                });
            }
        }
    }
}
#[test]
fn constructor_fences_and_late_native_precedence_are_exact() {
    for expiry in 1..=5 {
        let timer = Timer::new();
        exercise(BYTES, &timer, |retained| {
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(expiry);
            let result = Cursor::new(retained, Tick(1));
            if expiry == 5 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::Policy(PolicyError::Deadline))));
            }
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, expiry.min(4));
        });
    }
    for late in [false, true] {
        let timer = Timer::new();
        exercise(BYTES, &timer, |retained| {
            if late {
                timer.after(3);
            }
            let error = if late {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Interpretation(crate::mime_response::bound::Error::Admission(
                    crate::nfc::Error::Work(crate::admission::work::Stop::Deadline),
                ))
            };
            assert!(matches!(Cursor::new(retained,Tick(100)),Err(found) if found == error));
        });
    }
    let timer = Timer::new();
    exercise(BYTES, &timer, |retained| {
        let mut cursor = Cursor::new(retained, Tick(1)).unwrap();
        timer.after(3);
        assert_eq!(
            cursor.poll(Tick(100), &mut [0; 64]),
            Err(Error::Policy(PolicyError::Deadline))
        );
    });
}
