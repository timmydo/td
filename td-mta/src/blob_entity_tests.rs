#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::ports::{Clock, Deadline, Time};
use std::sync::atomic::{AtomicU64, Ordering};
struct Timer {
    now: AtomicU64,
    calls: AtomicU64,
    expire_at: AtomicU64,
}
impl Timer {
    fn new() -> Self {
        Self {
            now: AtomicU64::new(1),
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        }
    }
    fn after(&self, samples: u64) {
        self.expire_at.store(
            self.calls.load(Ordering::Relaxed) + samples,
            Ordering::Relaxed,
        );
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let sample = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            monotonic: Tick(if sample >= self.expire_at.load(Ordering::Relaxed) {
                100
            } else {
                self.now.load(Ordering::Relaxed)
            }),
            utc_ms: 0,
        })
    }
}
fn allowance() -> Charge {
    Charge {
        io_bytes: 1_000_000,
        records: 1_000_000,
        output_bytes: 1_000_000,
        unlinks: 0,
    }
}
fn meter(charge: Charge) -> Meter {
    Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge)
}
fn settings(start: u64, end: u64) -> Settings {
    Settings {
        extent: start..end,
        header_limit: 1000,
        context: metadata::Context::Normal,
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_>) -> Result<usize, Error> {
    for turn in 1..10000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
        assert!(cursor.value().is_none());
    }
    panic!("native entity did not finish")
}
#[test]
fn entity_handoffs_preserve_original_owners_and_disjoint_header_views() {
    let first = b"Content-Type: multipart/digest; boundary=abc\r\n\r\nbody-one";
    let second = b"\nFrom: nested\n\nbody-two";
    let base = 20000;
    let mut stored = vec![b'x'; base];
    stored.extend_from_slice(first);
    let next = stored.len();
    stored.extend_from_slice(second);
    let end = stored.len();
    stored.extend_from_slice(b"Content-Type: image/png\n\noutside");
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(&stored, &timer, |mut source| {
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let source_ptr = std::ptr::from_mut(&mut source);
        let work_ptr = std::ptr::from_mut(&mut work);
        let budget_ptr = std::ptr::from_mut(&mut budget);
        let mut arena = [0; 4096];
        let mut cursor = Cursor::new(
            &mut source,
            settings(base as u64, next as u64),
            &mut arena,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let retained = cursor.finish(Tick(1)).unwrap();
        assert_eq!(std::ptr::from_mut(retained.source), source_ptr);
        assert_eq!(std::ptr::from_mut(retained.work), work_ptr);
        assert_eq!(std::ptr::from_mut(retained.budget), budget_ptr);
        assert_eq!(
            retained.value.classified.media,
            mime_structure::Media::Multipart
        );
        assert!(retained.value.classified.digest);
        assert_eq!(retained.value.boundary(), Some(b"abc".as_slice()));
        assert_eq!(retained.value.selection.end, retained.headers.end);
        assert_eq!(retained.headers.base, base as u64);
        assert!(retained.value.classified.type_start >= base as u64);
        let first_view = retained.headers;
        let saved = first_view.bytes.to_vec();
        let mut selected = settings(next as u64, end as u64);
        selected.context = metadata::Context::DigestChild;
        let mut cursor = Cursor::new(
            retained.source,
            selected,
            retained.remaining,
            retained.work,
            retained.budget,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let retained = cursor.finish(Tick(1)).unwrap();
        assert_eq!(
            retained.value.classified.media,
            mime_structure::Media::MessageRfc822
        );
        assert_eq!(retained.value.boundary(), None);
        assert_eq!(retained.headers.end.body_start, next as u64 + 1);
        retained.remaining.fill(0xa5);
        assert_eq!(first_view.bytes, saved);
        assert_eq!(std::ptr::from_mut(retained.source), source_ptr);
        assert_eq!(std::ptr::from_mut(retained.work), work_ptr);
        assert_eq!(std::ptr::from_mut(retained.budget), budget_ptr);
        assert!(retained.remaining.len() > 3900);
        retained.source.check_deadline().unwrap();
    });
}
#[test]
fn malformed_entities_and_all_job_cuts_preserve_the_original_error() {
    let source_bytes = b"Content-Type: multipart/mixed;boundary=abc\n\nbody";
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(source_bytes, &timer, |mut source| {
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut buffer = [0; 256];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, source_bytes.len() as u64),
            &mut buffer,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let retained = cursor.finish(Tick(1)).unwrap();
        let left = retained.work.remaining();
        let used = Charge {
            io_bytes: allowance().io_bytes - left.io_bytes,
            records: allowance().records - left.records,
            output_bytes: allowance().output_bytes - left.output_bytes,
            unlinks: 0,
        };
        for (kind, total) in [
            (0, used.io_bytes),
            (1, used.records),
            (2, used.output_bytes),
        ] {
            for cut in 0..=total {
                let mut charge = allowance();
                match kind {
                    0 => charge.io_bytes = cut,
                    1 => charge.records = cut,
                    _ => charge.output_bytes = cut,
                }
                let mut work = meter(charge);
                let mut budget = HeaderBudget::new();
                let mut buffer = [0; 256];
                let mut cursor = Cursor::new(
                    retained.source,
                    settings(0, source_bytes.len() as u64),
                    &mut buffer,
                    &mut work,
                    &mut budget,
                    Tick(1),
                )
                .unwrap();
                let result = drain(&mut cursor);
                if cut == total {
                    assert!(result.is_ok());
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Work(_)), "{error:?}");
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
                }
            }
        }
    });
    let source_bytes = b"Content-Type: multipart/mixed\n\nbody";
    crate::store_fs::with_pinned_fixture(source_bytes, &timer, |mut source| {
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut buffer = [0; 256];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, source_bytes.len() as u64),
            &mut buffer,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        let error = Error::Interpretation(mime_structure::Error::NotParsable);
        assert_eq!(drain(&mut cursor), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
    });
}
#[test]
fn consuming_read_handoff_expiry_does_not_become_an_internal_state_error() {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut buffer = [0; 1];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, 1),
            &mut buffer,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        // Outer pre-check, collector pre/post, then the consuming handoff.
        timer.after(7);
        let error = Error::Policy(PolicyError::Deadline);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(cursor.reader.is_none() && cursor.source.is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
        assert!(source.is_failed());
    });
}
#[test]
fn outer_native_fence_withholds_would_be_completion_and_cached_results() {
    let bytes = b"Content-Type: multipart/mixed;boundary=abc\n\nbody";
    let timer = Timer::new();
    let mut turns = 0;
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut buffer = [0; 256];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, bytes.len() as u64),
            &mut buffer,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        turns = drain(&mut cursor).unwrap();
    });
    for completed in [false, true] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut buffer = [0; 256];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, bytes.len() as u64),
                &mut buffer,
                &mut work,
                &mut budget,
                Tick(1),
            )
            .unwrap();
            for _ in 0..if completed { turns } else { turns - 1 } {
                cursor.poll(Tick(1)).unwrap();
            }
            assert_eq!(cursor.value().is_some(), completed);
            timer.after(3);
            let error = Error::Policy(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(cursor.interpreter.is_none() && cursor.work.is_some());
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
            assert!(source.is_failed());
        });
    }
}
#[test]
fn premature_and_expired_handoffs_never_return_passive_success() {
    for completed in [false, true] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut buffer = [0; 1];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, 1),
                &mut buffer,
                &mut work,
                &mut budget,
                Tick(1),
            )
            .unwrap();
            if completed {
                drain(&mut cursor).unwrap();
            }
            assert!(matches!(
                cursor.finish(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            ));
        });
    }
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
        let mut work = meter(allowance());
        let mut budget = HeaderBudget::new();
        let mut buffer = [0; 1];
        let cursor = Cursor::new(
            &mut source,
            settings(0, 1),
            &mut buffer,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::State)));
    });
}

#[test]
fn every_incomplete_stage_refuses_finish_and_deadline() {
    for stage in 0..3 {
        for expired in [false, true] {
            let timer = Timer::new();
            crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
                let mut work = meter(allowance());
                let mut budget = HeaderBudget::new();
                let mut buffer = [0; 1];
                let mut cursor = Cursor::new(
                    &mut source,
                    settings(0, 1),
                    &mut buffer,
                    &mut work,
                    &mut budget,
                    Tick(1),
                )
                .unwrap();
                if stage > 0 {
                    while cursor.reader.is_some() {
                        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                    }
                    assert!(cursor.source.is_some() && cursor.work.is_some());
                }
                if stage > 1 {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                    assert!(cursor.interpreter.is_some());
                }
                let expected = if expired {
                    Error::Work(Stop::Deadline)
                } else {
                    Error::State
                };
                assert!(
                    matches!(cursor.finish(Tick(if expired { 100 } else { 1 })), Err(error) if error == expected)
                );
            });
        }
    }
}
#[test]
fn collector_refusals_and_synthetic_disagreement_are_sticky() {
    for mismatch in [false, true] {
        let timer = Timer::new();
        let bytes = b"X: y\n\nbody";
        crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
            let mut work = meter(allowance());
            let mut budget = HeaderBudget::new();
            let mut buffer = [0; 64];
            let mut selected = settings(0, bytes.len() as u64);
            if !mismatch {
                selected.header_limit = 0;
            }
            let mut cursor = Cursor::new(
                &mut source,
                selected,
                &mut buffer,
                &mut work,
                &mut budget,
                Tick(1),
            )
            .unwrap();
            let expected = if mismatch {
                while cursor.reader.is_some() {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                }
                cursor.headers.as_mut().unwrap().end.body_start += 1;
                Error::State
            } else {
                Error::Headers(root_headers::Error::Headers(
                    crate::mime_headers::Error::HeaderLimit,
                ))
            };
            assert_eq!(drain(&mut cursor), Err(expected));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(expected));
            assert!(matches!(cursor.finish(Tick(1)), Err(error) if error == expected));
        });
    }
}
#[test]
fn aggregate_interpretation_cuts_accumulate_across_entities() {
    let bytes = b"Content-Type: text/plain\n\n";
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = meter(Charge {
            io_bytes: 100_000_000,
            records: 100_000_000,
            ..allowance()
        });
        let mut budget = HeaderBudget::new();
        let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut arena = [0; 256];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, bytes.len() as u64),
            &mut arena,
            &mut work,
            &mut budget,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let retained = cursor.finish(Tick(1)).unwrap();
        let costs = (
            initial.0 - retained.budget.source_bytes_remaining(),
            initial.1 - retained.budget.steps_remaining(),
        );
        assert!(costs.0 > 0 && costs.1 > 0);
        for (steps, cost) in [(false, costs.0), (true, costs.1)] {
            for cut in 0..=cost {
                let mut work = meter(Charge {
                    io_bytes: 100_000_000,
                    records: 100_000_000,
                    ..allowance()
                });
                let mut budget = HeaderBudget::new();
                let amount = if steps { initial.1 } else { initial.0 } - cost - cut;
                budget
                    .charge(
                        &mut work,
                        Tick(1),
                        if steps { 0 } else { amount },
                        if steps { amount } else { 0 },
                        &mut Credit::new(),
                    )
                    .unwrap();
                let mut arena = [0; 256];
                let mut cursor = Cursor::new(
                    retained.source,
                    settings(0, bytes.len() as u64),
                    &mut arena,
                    &mut work,
                    &mut budget,
                    Tick(1),
                )
                .unwrap();
                drain(&mut cursor).unwrap();
                let first = cursor.finish(Tick(1)).unwrap();
                assert_eq!(
                    if steps {
                        first.budget.steps_remaining()
                    } else {
                        first.budget.source_bytes_remaining()
                    },
                    cut
                );
                let mut cursor = Cursor::new(
                    first.source,
                    settings(0, bytes.len() as u64),
                    first.remaining,
                    first.work,
                    first.budget,
                    Tick(1),
                )
                .unwrap();
                if cut == cost {
                    drain(&mut cursor).unwrap();
                } else {
                    let error = Error::Interpretation(mime_structure::Error::InterpretationLimit);
                    assert_eq!(drain(&mut cursor), Err(error));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(matches!(cursor.finish(Tick(1)), Err(found) if found == error));
                }
            }
        }
    });
}
