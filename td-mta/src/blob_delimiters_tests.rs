#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::{Clock, Deadline, Time};
use std::sync::atomic::{AtomicU64, Ordering};
struct Timer {
    tick: AtomicU64,
    calls: AtomicU64,
    expire_at: AtomicU64,
}
impl Timer {
    fn new() -> Self {
        Self {
            tick: AtomicU64::new(1),
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        }
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if call >= self.expire_at.load(Ordering::Relaxed) {
                100
            } else {
                self.tick.load(Ordering::Relaxed)
            }),
        })
    }
}
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            output_bytes: 0,
            unlinks: 0,
        },
    )
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_>) -> Result<Vec<Delimiter>, Error> {
    let mut events = Vec::new();
    for _ in 0..100_000 {
        let before = cursor.work.remaining();
        let position = cursor.position;
        let step = cursor.poll(Tick(1))?;
        let after = cursor.work.remaining();
        if cursor.position != position {
            assert!(before.io_bytes - after.io_bytes <= READ_BYTES as u64);
            assert_eq!(before.records - after.records, 1);
            assert_eq!(cursor.consumed, 0);
        } else {
            assert!(before.io_bytes - after.io_bytes <= 2 * 128);
            assert!(before.records - after.records <= 1);
        }
        match step {
            Status::Complete => return Ok(events),
            Status::Delimiter(value) => events.push(value),
            Status::Yield => {}
        }
    }
    panic!("native delimiter scan did not finish");
}
fn reference(bytes: &[u8], base: u64) -> Vec<Delimiter> {
    let mut work = work();
    let mut cursor = mime_delimiter::Cursor::new(bytes, base, b"b", &mut work).unwrap();
    let mut result = Vec::new();
    loop {
        match cursor.poll(Tick(1)).unwrap() {
            mime_delimiter::Status::Delimiter(value) => result.push(value),
            mime_delimiter::Status::Complete => return result,
            _ => {}
        }
    }
}
#[test]
fn native_extents_match_shared_events_across_read_boundaries() {
    let mut large = vec![b'x'; 8200];
    large.extend_from_slice(b"\r\n--b\r\nbody\r\n--b--\r\n");
    for body in [
        b"".as_slice(),
        b"--b\r",
        b"--b\r\n--b--",
        b"\r\n\n--b\n\r\n--b--\r\n",
        &large,
    ] {
        let mut stored = b"outside\r\n".to_vec();
        let base = stored.len();
        stored.extend_from_slice(body);
        let end = stored.len();
        stored.extend_from_slice(b"\n--b--\r\noutside");
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(&stored, &timer, |mut source| {
            for width in [1, 3, 127, 128, 129, 4096, 8192] {
                let mut work = work();
                let before = work.remaining();
                let mut buffer = vec![0; width];
                let mut cursor = Cursor::new(
                    &mut source,
                    base as u64..end as u64,
                    b"b",
                    &mut buffer,
                    &mut work,
                    Tick(1),
                )
                .unwrap();
                assert_eq!(drain(&mut cursor).unwrap(), reference(body, base as u64));
                let remaining = cursor.work.remaining();
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                assert_eq!(cursor.work.remaining(), remaining);
                cursor.finish(Tick(1)).unwrap();
                // Boundary copy + validation, two scanner visits and one native read per byte.
                assert_eq!(
                    before.io_bytes - work.remaining().io_bytes,
                    2 + 3 * body.len() as u64
                );
            }
        });
    }
}
#[test]
fn native_original_budget_cuts_retire_every_provisional_event() {
    let body = b"--b\r\nx\n--b--";
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(body, &timer, |mut source| {
        let mut original = work();
        let before = original.remaining();
        let mut buffer = [0; 3];
        let mut cursor = Cursor::new(
            &mut source,
            0..body.len() as u64,
            b"b",
            &mut buffer,
            &mut original,
            Tick(1),
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        let cost = Charge {
            io_bytes: before.io_bytes - original.remaining().io_bytes,
            records: before.records - original.remaining().records,
            ..Charge::default()
        };
        for (total, stop) in [
            (cost.io_bytes, Stop::IoBytes),
            (cost.records, Stop::Records),
        ] {
            for limit in 0..total {
                let mut capacity = before;
                if stop == Stop::IoBytes {
                    capacity.io_bytes = limit;
                } else {
                    capacity.records = limit;
                }
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
                let mut buffer = [0; 3];
                match Cursor::new(
                    &mut source,
                    0..body.len() as u64,
                    b"b",
                    &mut buffer,
                    &mut work,
                    Tick(1),
                ) {
                    Err(error) => assert_eq!(error, Error::Work(stop)),
                    Ok(mut cursor) => {
                        assert_eq!(drain(&mut cursor), Err(Error::Work(stop)));
                        let remaining = cursor.work.remaining();
                        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
                        assert_eq!(cursor.work.remaining(), remaining);
                        assert_eq!(cursor.finish(Tick(1)), Err(Error::Work(stop)));
                    }
                }
            }
        }
    });
}
#[test]
fn native_and_job_deadlines_remain_fresh_after_events_and_completion() {
    for native in [false, true] {
        for complete in [false, true] {
            let timer = Timer::new();
            crate::store_fs::with_pinned_fixture(b"--b\nx\n--b--", &timer, |mut source| {
                let mut work = work();
                let mut buffer = [0; 3];
                let mut cursor =
                    Cursor::new(&mut source, 0..11, b"b", &mut buffer, &mut work, Tick(1)).unwrap();
                if complete {
                    drain(&mut cursor).unwrap();
                } else {
                    loop {
                        if matches!(cursor.poll(Tick(1)).unwrap(), Status::Delimiter(_)) {
                            break;
                        }
                    }
                }
                if native {
                    timer.tick.store(100, Ordering::Relaxed);
                }
                let now = if native { Tick(1) } else { Tick(100) };
                let error = if native {
                    Error::Policy(PolicyError::Deadline)
                } else {
                    Error::Work(Stop::Deadline)
                };
                assert_eq!(cursor.poll(now), Err(error));
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)), Err(error));
            });
        }
    }
}
#[test]
fn native_post_read_expiry_withholds_progress_and_retires_the_pin() {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"--b\n", &timer, |mut source| {
        let mut work = work();
        let mut buffer = [0; 4];
        let mut cursor =
            Cursor::new(&mut source, 0..4, b"b", &mut buffer, &mut work, Tick(1)).unwrap();
        // Grammar validation, then request first input.
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        // The fixture checks twice per fence: two before read_at and
        // two on each side of the copy. Expire on its first post-read sample.
        timer
            .expire_at
            .store(timer.calls.load(Ordering::Relaxed) + 5, Ordering::Relaxed);
        let error = Error::Policy(PolicyError::Deadline);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.finish(Tick(1)), Err(error));
        assert!(source.is_failed());
        assert_eq!(buffer, *b"--b\n");
    });
}
#[test]
fn invalid_extents_empty_buffers_and_premature_finish_refuse() {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"--b", &timer, |mut source| {
        let mut work = work();
        for extent in [4..4, 0..4, std::ops::Range { start: 2, end: 1 }] {
            assert!(matches!(
                Cursor::new(&mut source, extent, b"b", &mut [0; 1], &mut work, Tick(1)),
                Err(Error::Range)
            ));
        }
        assert!(matches!(
            Cursor::new(&mut source, 0..3, b"b", &mut [], &mut work, Tick(1)),
            Err(Error::Capacity)
        ));
        let mut empty = [];
        let mut cursor =
            Cursor::new(&mut source, 3..3, b"b", &mut empty, &mut work, Tick(1)).unwrap();
        assert!(drain(&mut cursor).unwrap().is_empty());
        cursor.finish(Tick(1)).unwrap();
        let mut buffer = [0; 1];
        let cursor = Cursor::new(&mut source, 0..3, b"b", &mut buffer, &mut work, Tick(1)).unwrap();
        assert_eq!(cursor.finish(Tick(1)), Err(Error::State));
    });
}

#[test]
fn outer_post_scan_checks_withhold_delimiters_and_cached_completion() {
    for complete in [false, true] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"--b\n", &timer, |mut source| {
            let mut work = work();
            let mut buffer = [0; 4];
            let mut cursor =
                Cursor::new(&mut source, 0..4, b"b", &mut buffer, &mut work, Tick(1)).unwrap();
            if complete {
                assert_eq!(drain(&mut cursor).unwrap().len(), 1);
            } else {
                // Validation, input request, native read; the next turn emits a delimiter.
                for _ in 0..3 {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                }
                assert_eq!(cursor.consumed, 0);
                assert_eq!(cursor.filled, 4);
            }
            let before = cursor.work.remaining();
            // PinnedBlob::check_deadline takes two samples. There is no native
            // I/O this turn, so sample three belongs to the outer post-check.
            timer
                .expire_at
                .store(timer.calls.load(Ordering::Relaxed) + 3, Ordering::Relaxed);
            let error = Error::Policy(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.consumed, 4);
            if complete {
                assert_eq!(cursor.work.remaining(), before);
            } else {
                assert_eq!(before.io_bytes - cursor.work.remaining().io_bytes, 8);
                assert_eq!(before.records - cursor.work.remaining().records, 1);
            }
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.finish(Tick(1)), Err(error));
            assert!(source.is_failed());
        });
    }
}
