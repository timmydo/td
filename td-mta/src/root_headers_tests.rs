#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::{Clock, Deadline, Time};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        Ok(Time {
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
            utc_ms: 0,
        })
    }
}
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            output_bytes: 1_000_000,
            unlinks: 0,
        },
    )
}
static FIXTURE: Mutex<()> = Mutex::new(());
fn pinned(bytes: &[u8], run: impl FnOnce(PinnedBlob<'_, '_>, &Timer)) {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let timer = Timer(AtomicU64::new(1));
    crate::store_fs::with_pinned_fixture(bytes, &timer, |source| run(source, &timer));
}
fn complete(cursor: &mut Cursor<'_, '_, '_, '_, '_>) {
    for _ in 0..20_000 {
        let before = cursor.work.remaining();
        let status = cursor.poll(Tick(1)).unwrap();
        let after = cursor.work.remaining();
        assert!(before.io_bytes - after.io_bytes <= READ_BYTES as u64);
        assert!(before.records - after.records <= 256);
        if status == Status::Complete {
            return;
        }
    }
    panic!("header cursor did not complete");
}
#[test]
fn root_header_boundaries_match_shared_liberal_scanning_and_exact_limits() {
    for (bytes, limit, headers, body) in [
        (b"".as_slice(), 0, 0, 0),
        (b"Subject: x\r\n\r\nbody".as_slice(), 12, 12, 14),
        (b"X:a\n\nbody".as_slice(), 4, 4, 5),
        (b"X:a".as_slice(), 3, 3, 3),
        (b"X:a\r\nbad body\r\nmore".as_slice(), 100, 5, 5),
        (b" bad body".as_slice(), 100, 0, 0),
    ] {
        pinned(bytes, |mut source, _| {
            let mut work = work();
            let cap = bytes.len().min(limit as usize + 2);
            let mut buffer = vec![0; cap];
            let mut cursor =
                Cursor::new(&mut source, limit, &mut buffer, &mut work, Tick(1)).unwrap();
            assert!(cursor.value().is_none());
            complete(&mut cursor);
            let view = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                view.end,
                mime_headers::End {
                    header_bytes: headers,
                    body_start: body
                }
            );
            assert_eq!(view.bytes, &bytes[..view.bytes.len()]);
            assert_eq!(
                view.source_end,
                if view.bytes.len() == bytes.len() {
                    SourceEnd::Eof
                } else {
                    SourceEnd::Prefix
                }
            );
        });
    }
}
#[test]
fn multi_read_headers_and_large_uninterpreted_body_keep_only_bounded_input() {
    let mut bytes = b"Subject: ".to_vec();
    bytes.extend(vec![b'a'; 8192]);
    bytes.extend_from_slice(b"\r\nX: b\r\n\r\n");
    let body = bytes.len();
    bytes.extend(vec![b'x'; 100_000]);
    pinned(&bytes, |mut source, _| {
        let mut work = work();
        let mut buffer = vec![0; 9002];
        let mut cursor = Cursor::new(&mut source, 9000, &mut buffer, &mut work, Tick(1)).unwrap();
        complete(&mut cursor);
        let view = cursor.finish(Tick(1)).unwrap();
        assert_eq!(view.end.body_start, body as u64);
        assert_eq!(view.end.header_bytes, body as u64 - 2);
        assert_eq!(view.source_end, SourceEnd::Prefix);
        assert!(view.bytes.len() <= 9002);
        assert_eq!(view.bytes, &bytes[..view.bytes.len()]);
        assert!(work.remaining().io_bytes > 970_000);
    });
}
#[test]
fn capacity_header_limit_work_and_both_deadlines_withhold_results() {
    let bytes = b"Subject: long\r\n\r\nbody";
    pinned(bytes, |mut source, _| {
        let mut work = work();
        assert!(matches!(
            Cursor::new(&mut source, 100, &mut [0; 2], &mut work, Tick(1)),
            Err(Error::Capacity)
        ));
    });
    pinned(bytes, |mut source, _| {
        let mut work = work();
        let mut buffer = [0; 100];
        let mut cursor = Cursor::new(&mut source, 4, &mut buffer, &mut work, Tick(1)).unwrap();
        let mut failed = false;
        for _ in 0..100 {
            match cursor.poll(Tick(1)) {
                Err(error) => {
                    assert_eq!(error, Error::Headers(mime_headers::Error::HeaderLimit));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    failed = true;
                    break;
                }
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => panic!("over-limit header completed"),
            }
        }
        assert!(failed);
    });
    for fault in ["io", "records", "request-deadline", "native-deadline"] {
        pinned(bytes, |mut source, timer| {
            let mut work = work();
            if fault == "io" {
                work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        records: 1000,
                        ..Charge::default()
                    },
                );
            }
            if fault == "records" {
                work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes: 1000,
                        ..Charge::default()
                    },
                );
            }
            let mut buffer = [0; 100];
            let mut cursor =
                Cursor::new(&mut source, 100, &mut buffer, &mut work, Tick(1)).unwrap();
            let now = if fault == "request-deadline" {
                Tick(100)
            } else {
                Tick(1)
            };
            if fault == "native-deadline" {
                timer.0.store(100, Ordering::Relaxed);
            }
            let error = cursor.poll(now).unwrap_err();
            assert_eq!(
                error,
                match fault {
                    "io" => Error::Work(Stop::IoBytes),
                    "records" => Error::Work(Stop::Records),
                    "request-deadline" => Error::Work(Stop::Deadline),
                    _ => Error::Policy(PolicyError::Deadline),
                }
            );
            let remaining = cursor.work.remaining();
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.work.remaining(), remaining);
            assert!(cursor.value().is_none());
            assert!(matches!(cursor.finish(Tick(1)),Err(found) if found == error));
        });
    }
}
#[test]
fn complete_input_still_requires_fresh_finish_and_premature_finish_refuses() {
    for native in [false, true] {
        pinned(b"X:a\r\n\r\nbody", |mut source, timer| {
            let mut work = work();
            let mut buffer = [0; 32];
            let mut cursor = Cursor::new(&mut source, 30, &mut buffer, &mut work, Tick(1)).unwrap();
            complete(&mut cursor);
            if native {
                timer.0.store(100, Ordering::Relaxed);
            }
            let error = if native {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Work(Stop::Deadline)
            };
            assert!(
                matches!(cursor.finish(if native { Tick(1) } else { Tick(100) }),Err(found) if found == error)
            );
        });
    }
    pinned(b"X:a\n\n", |mut source, _| {
        let mut work = work();
        let mut buffer = [0; 10];
        let cursor = Cursor::new(&mut source, 8, &mut buffer, &mut work, Tick(1)).unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::State)));
    });
}

#[test]
fn tentative_names_use_resident_capacity_not_recognized_header_limit() {
    for suffix in [b" body".as_slice(), b"\nbody".as_slice(), b"".as_slice()] {
        let mut bytes = vec![b'A'; 9000];
        bytes.extend_from_slice(suffix);
        pinned(&bytes, |mut source, _| {
            let mut work = work();
            let mut buffer = vec![0; bytes.len()];
            let mut cursor = Cursor::new(&mut source, 4, &mut buffer, &mut work, Tick(1)).unwrap();
            complete(&mut cursor);
            let view = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                view.end,
                mime_headers::End {
                    header_bytes: 0,
                    body_start: 0
                }
            );
            assert_eq!(view.source_end, SourceEnd::Eof);
            assert_eq!(view.bytes, bytes);
        });
        pinned(&bytes, |mut source, _| {
            let mut work = work();
            let mut buffer = [0; 6];
            let mut cursor = Cursor::new(&mut source, 4, &mut buffer, &mut work, Tick(1)).unwrap();
            let mut failed = false;
            for _ in 0..100 {
                if let Err(error) = cursor.poll(Tick(1)) {
                    assert_eq!(error, Error::Capacity);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(cursor.value().is_none());
                    failed = true;
                    break;
                }
            }
            assert!(failed);
        });
    }
}

#[test]
fn read_edges_and_scanner_work_keep_exact_limits() {
    for name in [false, true] {
        for pad in [4091, 4092, 4093] {
            let mut bytes = if name { vec![] } else { b"X:".to_vec() };
            bytes.extend(vec![b'a'; pad]);
            if name {
                bytes.extend_from_slice(b":x");
            }
            bytes.extend_from_slice(b"\r\n");
            let headers = bytes.len();
            bytes.extend_from_slice(b"\r\nbody");
            pinned(&bytes, |mut source, _| {
                let mut work = work();
                let mut buffer = vec![0; headers + 2];
                let mut cursor =
                    Cursor::new(&mut source, headers as u64, &mut buffer, &mut work, Tick(1))
                        .unwrap();
                complete(&mut cursor);
                let view = cursor.finish(Tick(1)).unwrap();
                assert_eq!(
                    view.end,
                    mime_headers::End {
                        header_bytes: headers as u64,
                        body_start: headers as u64 + 2
                    }
                );
            });
        }
    }
    pinned(b"X:a\r\n\r\nbody", |mut source, _| {
        let mut meter = work();
        assert!(matches!(
            Cursor::new(&mut source, 5, &mut [0; 6], &mut meter, Tick(1)),
            Err(Error::Capacity)
        ));
        let mut buffer = [0; 32];
        let mut cursor =
            Cursor::new(&mut source, u64::MAX, &mut buffer, &mut meter, Tick(1)).unwrap();
        complete(&mut cursor);
        assert_eq!(cursor.finish(Tick(1)).unwrap().end.header_bytes, 5);
    });
    pinned(b"X:a\r\n\r\nbody", |mut source, _| {
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 7,
                records: 1000,
                ..Charge::default()
            },
        );
        let mut buffer = [0; 7];
        let mut cursor = Cursor::new(&mut source, 5, &mut buffer, &mut meter, Tick(1)).unwrap();
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        let error = Error::Headers(mime_headers::Error::Work(Stop::IoBytes));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(cursor.value().is_none());
    });
}
