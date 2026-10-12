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

fn rescan(view: View<'_>, limit: u64, work: &mut Meter) -> mime_headers::End {
    let mut scanner = Scanner::new(view.base, limit);
    let mut position = 0;
    for _ in 0..10_000 {
        let step = scanner
            .poll(
                &view.bytes[position..],
                view.source_end == SourceEnd::Eof,
                Tick(1),
                work,
            )
            .unwrap();
        position += step.consumed;
        match step.status {
            mime_headers::Status::Complete(end) => return end,
            mime_headers::Status::NeedInput => panic!("retained header witness is incomplete"),
            _ => {}
        }
    }
    panic!("retained header witness did not complete");
}
#[test]
fn clipped_entity_offsets_and_split_witness_replay_are_exact() {
    for body in [
        b"".as_slice(),
        b"Subject: x\r\n\r\nbody",
        b"X:y",
        b"X:y\r",
        b" bad body",
        b"bad body\r\nmore",
        b"\rx",
        b"AAAA",
        b"X:y\n\n",
        b"X:y\r\n\r",
        b"X:y\r\n\rZ",
        b"X:y\r\r",
    ] {
        let mut stored = b"outer prefix\r\n".to_vec();
        let base = stored.len();
        stored.extend_from_slice(body);
        let end = stored.len();
        stored.extend_from_slice(b"Subject: outside\r\n\r\n");
        pinned(&stored, |mut source, _| {
            let mut work = work();
            let mut buffer = [0; 128];
            let mut cursor = Cursor::new_extent(
                &mut source,
                base as u64..end as u64,
                100,
                &mut buffer,
                &mut work,
                Tick(1),
            )
            .unwrap();
            complete(&mut cursor);
            let expected = cursor.value().unwrap().end;
            let consumed = cursor.consumed;
            let filled = cursor.filled;
            let (view, remaining) = cursor.finish_split(Tick(1)).unwrap();
            assert_eq!(view.base, base as u64);
            assert_eq!(view.end, expected);
            assert_eq!(view.bytes, &body[..view.bytes.len()]);
            assert_eq!(view.bytes.len(), consumed + usize::from(consumed < filled));
            assert_eq!(view.bytes.len() + remaining.len(), 128);
            assert_eq!(
                view.source_end,
                if view.bytes.len() == body.len() {
                    SourceEnd::Eof
                } else {
                    SourceEnd::Prefix
                }
            );
            remaining.fill(0xa5);
            assert_eq!(rescan(view, 100, &mut work), expected);
        });
    }
}
#[test]
fn one_arena_retains_multiple_disjoint_entity_headers() {
    let first = b"A: one\r\n\r\nbody-one";
    let second = b"B: two\n\nbody-two";
    let mut stored = first.to_vec();
    stored.extend_from_slice(second);
    pinned(&stored, |mut source, _| {
        let mut work = work();
        let mut arena = [0; 256];
        let mut cursor = Cursor::new_extent(
            &mut source,
            0..first.len() as u64,
            100,
            &mut arena,
            &mut work,
            Tick(1),
        )
        .unwrap();
        complete(&mut cursor);
        let (first_view, remaining) = cursor.finish_split(Tick(1)).unwrap();
        let saved = first_view.bytes.to_vec();
        let mut cursor = Cursor::new_extent(
            &mut source,
            first.len() as u64..stored.len() as u64,
            100,
            remaining,
            &mut work,
            Tick(1),
        )
        .unwrap();
        complete(&mut cursor);
        let (second_view, remaining) = cursor.finish_split(Tick(1)).unwrap();
        remaining.fill(0xa5);
        assert_eq!(first_view.bytes, saved);
        assert_eq!(first_view.base, 0);
        assert_eq!(second_view.base, first.len() as u64);
        assert_eq!(rescan(first_view, 100, &mut work), first_view.end);
        assert_eq!(rescan(second_view, 100, &mut work), second_view.end);
        assert!(remaining.len() > 200);
    });
}
#[test]
fn entity_range_and_split_handoff_require_original_freshness() {
    pinned(b"X:y\n\nbody", |mut source, _| {
        let mut work = work();
        let mut buffer = [0; 128];
        for extent in [20..20, 10..10, std::ops::Range { start: 3, end: 2 }, 0..20] {
            assert!(matches!(
                Cursor::new_extent(&mut source, extent, 100, &mut buffer, &mut work, Tick(1)),
                Err(Error::Range)
            ));
        }
        let mut empty = [];
        let mut cursor =
            Cursor::new_extent(&mut source, 9..9, 100, &mut empty, &mut work, Tick(1)).unwrap();
        complete(&mut cursor);
        let (view, remaining) = cursor.finish_split(Tick(1)).unwrap();
        assert!(view.bytes.is_empty());
        assert!(remaining.is_empty());
        assert_eq!(view.base, 9);
        assert_eq!(view.source_end, SourceEnd::Eof);
        assert_eq!(
            view.end,
            mime_headers::End {
                header_bytes: 0,
                body_start: 9
            }
        );
        let cursor =
            Cursor::new_extent(&mut source, 0..9, 100, &mut buffer, &mut work, Tick(1)).unwrap();
        assert!(matches!(cursor.finish_split(Tick(1)), Err(Error::State)));
    });
    for native in [false, true] {
        pinned(b"X:y\n\nbody", |mut source, timer| {
            let mut work = work();
            let mut buffer = [0; 128];
            let mut cursor =
                Cursor::new_extent(&mut source, 0..9, 100, &mut buffer, &mut work, Tick(1))
                    .unwrap();
            complete(&mut cursor);
            if native {
                timer.0.store(100, Ordering::Relaxed);
            }
            let now = if native { Tick(1) } else { Tick(100) };
            let expected = if native {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Work(Stop::Deadline)
            };
            assert!(matches!(cursor.finish_split(now), Err(error) if error == expected));
        });
    }
}

#[test]
fn exhaustive_scanner_completion_witness_needs_at_most_one_unconsumed_byte() {
    const ALPHABET: &[u8] = b"X: \r\n";
    for length in 0..=6u32 {
        for mut encoded in 0..ALPHABET.len().pow(length) {
            let mut prefix = vec![0; length as usize];
            for byte in &mut prefix {
                *byte = ALPHABET[encoded % ALPHABET.len()];
                encoded /= ALPHABET.len();
            }
            for suffix in [b"".as_slice(), b"\n\nbody tail"] {
                let mut bytes = prefix.clone();
                bytes.extend_from_slice(suffix);
                for width in [1, 3, bytes.len().max(1)] {
                    let mut meter = work();
                    let mut scanner = Scanner::new(10000, 100);
                    let mut filled = width.min(bytes.len());
                    let mut consumed = 0;
                    let mut end = None;
                    for _ in 0..100 {
                        let step = scanner
                            .poll(
                                &bytes[consumed..filled],
                                filled == bytes.len(),
                                Tick(1),
                                &mut meter,
                            )
                            .unwrap();
                        consumed += step.consumed;
                        match step.status {
                            mime_headers::Status::Complete(value) => {
                                end = Some(value);
                                break;
                            }
                            mime_headers::Status::NeedInput => {
                                assert_eq!(consumed, filled);
                                assert!(filled < bytes.len());
                                filled = (filled + width).min(bytes.len());
                            }
                            _ => {}
                        }
                    }
                    let end = end.unwrap();
                    let retained = consumed + usize::from(consumed < filled);
                    let view = View {
                        bytes: &bytes[..retained],
                        base: 10000,
                        end,
                        source_end: if retained == bytes.len() {
                            SourceEnd::Eof
                        } else {
                            SourceEnd::Prefix
                        },
                    };
                    assert_eq!(
                        rescan(view, 100, &mut meter),
                        end,
                        "source={bytes:?}, width={width}"
                    );
                }
            }
        }
    }
}
#[test]
fn deep_entity_multi_read_limit_and_split_match_zero_base() {
    let mut entity = b"X:".to_vec();
    entity.extend(vec![b'a'; 8189]);
    entity.extend_from_slice(b"\n\n");
    entity.extend(vec![b'b'; 20000]);
    for base in [0, 20000] {
        let mut stored = vec![b'z'; base];
        stored.extend_from_slice(&entity);
        let end = stored.len() as u64;
        pinned(&stored, |mut source, _| {
            for split in [false, true] {
                let mut meter = work();
                let mut buffer = vec![0; 8194];
                let mut cursor = Cursor::new_extent(
                    &mut source,
                    base as u64..end,
                    8192,
                    &mut buffer,
                    &mut meter,
                    Tick(1),
                )
                .unwrap();
                complete(&mut cursor);
                assert!(cursor.filled > 2 * READ_BYTES);
                let view = if split {
                    cursor.finish_split(Tick(1)).unwrap().0
                } else {
                    cursor.finish(Tick(1)).unwrap()
                };
                assert_eq!(view.end.header_bytes, 8192);
                assert_eq!(view.end.body_start, base as u64 + 8193);
                assert_eq!(view.source_end, SourceEnd::Prefix);
                assert_eq!(rescan(view, 8192, &mut meter), view.end);
            }
            let mut meter = work();
            let mut buffer = vec![0; 8193];
            let mut cursor = Cursor::new_extent(
                &mut source,
                base as u64..end,
                8191,
                &mut buffer,
                &mut meter,
                Tick(1),
            )
            .unwrap();
            let mut failure = None;
            for _ in 0..20000 {
                match cursor.poll(Tick(1)) {
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                    Ok(Status::Complete) => panic!("oversized header was accepted"),
                    Ok(Status::Yield) => {}
                }
            }
            assert_eq!(
                failure,
                Some(Error::Headers(mime_headers::Error::HeaderLimit))
            );
            assert!(matches!(
                cursor.finish_split(Tick(1)),
                Err(Error::Headers(mime_headers::Error::HeaderLimit))
            ));
        });
    }
}
