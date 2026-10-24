#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::Stop,
    header_select::SourceEnd,
    ports::{Clock, Deadline, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
struct Timer {
    calls: AtomicU64,
    expire_at: AtomicU64,
}
impl Timer {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        }
    }
    fn after(&self, count: u64) {
        self.expire_at.store(
            self.calls.load(Ordering::Relaxed) + count,
            Ordering::Relaxed,
        );
    }
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            monotonic: Tick(if n >= self.expire_at.load(Ordering::Relaxed) {
                100
            } else {
                1
            }),
            utc_ms: 0,
        })
    }
}
fn capacity() -> Charge {
    Charge {
        io_bytes: 10_000_000,
        records: 10_000_000,
        output_bytes: 10_000_000,
        unlinks: 0,
    }
}
fn meter(cap: Charge) -> Meter {
    Meter::new(Deadline::after(Tick(0), 100).unwrap(), cap)
}
fn settings(base: u64, length: usize) -> Settings {
    Settings {
        extent: base..base + length as u64,
        limits: Limits {
            header_bytes: 512,
            mime_depth: 8,
            mime_parts: 16,
        },
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_>) -> Result<usize, Error> {
    for turn in 1..1_000_000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
    }
    panic!("native structure did not finish")
}
#[test]
fn native_fragments_match_complete_literal_tree_and_keep_original_owners() {
    let source = concat!("Content-Type: multipart/mixed;boundary=x\r\n\r\npre\r\n--x\r\n",
        "Content-Type: multipart/digest;boundary=y\r\n\r\n--y\r\n\r\nFrom: a\r\n\r\nbody\r\n--y--\r\nepi\r\n--x\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\nYWJj!\r\n--x\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n\r\na=       z\r\n--x--\r\nepilogue").as_bytes();
    let base = 20000;
    let mut bytes = vec![b'x'; base];
    bytes.extend_from_slice(source);
    bytes.extend_from_slice(b"outside\n--x\n");
    let mut expected = [Part::default(); 16];
    let mut work = meter(capacity());
    let mut budget = HeaderBudget::new();
    let mut resident = mime_structure::Cursor::new(
        source,
        base as u64,
        SourceEnd::Eof,
        settings(base as u64, source.len()).limits,
        &mut expected,
        &mut work,
        &mut budget,
    )
    .unwrap();
    while resident.poll(Tick(1)).unwrap() != Status::Complete {}
    let expected_headers = resident.header_bytes().unwrap();
    let (expected, _, _) = resident.finish(Tick(1)).unwrap();
    assert_eq!(expected.len(), 5);
    assert_eq!(expected[2].media, mime_structure::Media::MessageRfc822);
    assert_eq!(expected[3].size, 3);
    assert_eq!(expected[4].size, 10);
    for width in [1, 3, 97, 4096] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(&bytes, &timer, |mut source_pin| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let identities = (
                std::ptr::from_mut(&mut source_pin),
                std::ptr::from_mut(&mut work),
                std::ptr::from_mut(&mut budget),
            );
            let mut arena = [0; 2048];
            let mut body = vec![0; width];
            let mut parts = [Part::default(); 16];
            let mut headers = [None; 16];
            let mut cursor = Cursor::new(
                &mut source_pin,
                settings(base as u64, source.len()),
                Buffers {
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
            drain(&mut cursor).unwrap();
            let retained = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.parts, expected);
            assert_eq!(retained.header_bytes, expected_headers);
            assert_eq!(
                (
                    std::ptr::from_mut(retained.source),
                    std::ptr::from_mut(retained.work),
                    std::ptr::from_mut(retained.budget)
                ),
                identities
            );
            assert_eq!(retained.headers.len(), expected.len());
            let saved: Vec<Vec<u8>> = retained
                .headers
                .iter()
                .map(|h| h.unwrap().bytes.to_vec())
                .collect();
            retained.remaining.fill(0xa5);
            for ((header, part), saved) in retained.headers.iter().zip(retained.parts).zip(saved) {
                let header = header.unwrap();
                assert_eq!(header.base, part.entity_start);
                assert_eq!(header.end.body_start, part.body_start);
                assert_eq!(header.bytes, saved);
            }
            retained.source.check_deadline().unwrap();
        });
    }
}
#[test]
fn all_native_work_cuts_retire_without_a_partial_tree() {
    let bytes = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x--\nepi";
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 1024];
        let mut body = [0; 7];
        let mut parts = [Part::default(); 4];
        let mut headers = [None; 4];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, bytes.len()),
            Buffers {
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
        drain(&mut cursor).unwrap();
        let retained = cursor.finish(Tick(1)).unwrap();
        let left = retained.work.remaining();
        for (kind, total) in [
            (0, capacity().io_bytes - left.io_bytes),
            (1, capacity().records - left.records),
            (2, capacity().output_bytes - left.output_bytes),
        ] {
            for cut in 0..=total {
                let mut cap = capacity();
                match kind {
                    0 => cap.io_bytes = cut,
                    1 => cap.records = cut,
                    _ => cap.output_bytes = cut,
                }
                let mut work = meter(cap);
                let mut budget = HeaderBudget::new();
                let mut arena = [0; 1024];
                let mut body = [0; 7];
                let mut parts = [Part::default(); 4];
                let mut headers = [None; 4];
                let mut cursor = Cursor::new(
                    retained.source,
                    settings(0, bytes.len()),
                    Buffers {
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
                let result = drain(&mut cursor);
                if cut == total {
                    assert!(result.is_ok());
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Work(_)), "{error:?}");
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(matches!(cursor.finish(Tick(1)),Err(found) if found==error));
                }
            }
        }
    });
}
#[test]
fn native_handoff_and_final_publication_expiry_preserve_exact_refusal() {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 8];
        let mut body = [0; 1];
        let mut parts = [Part::default(); 1];
        let mut headers = [None; 1];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, 1),
            Buffers {
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
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        let before = timer.calls.load(Ordering::Relaxed);
        timer.after(7);
        let error = Error::Policy(PolicyError::Deadline);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(timer.calls.load(Ordering::Relaxed) - before, 7);
        assert!(cursor.reader.is_none() && cursor.source.is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)),Err(found) if found==error));
    });
    for cached in [false, true] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 8];
            let mut body = [0; 1];
            let mut parts = [Part::default(); 1];
            let mut headers = [None; 1];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, 1),
                Buffers {
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
            while !cursor.parsed || cursor.checked != cursor.used {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            }
            if cached {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
            }
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(3);
            let error = Error::Policy(PolicyError::Deadline);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, 3);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert!(matches!(cursor.finish(Tick(1)),Err(found) if found==error));
        });
    }
}
#[test]
fn every_unfinished_stage_and_expired_finish_withholds_descriptors() {
    let timer = Timer::new();
    let mut turns = 0;
    crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 8];
        let mut body = [0; 1];
        let mut parts = [Part::default(); 1];
        let mut headers = [None; 1];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, 1),
            Buffers {
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
        turns = drain(&mut cursor).unwrap();
    });
    for cut in 0..=turns {
        for expired in [false, true] {
            let timer = Timer::new();
            crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
                let mut work = meter(capacity());
                let mut budget = HeaderBudget::new();
                let mut arena = [0; 8];
                let mut body = [0; 1];
                let mut parts = [Part::default(); 1];
                let mut headers = [None; 1];
                let mut cursor = Cursor::new(
                    &mut source,
                    settings(0, 1),
                    Buffers {
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
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                let result = cursor.finish(Tick(if expired { 100 } else { 1 }));
                if expired {
                    assert!(matches!(result, Err(Error::Work(Stop::Deadline))));
                } else if cut + 1 < turns {
                    assert!(matches!(result, Err(Error::State)));
                } else {
                    assert!(result.is_ok());
                }
            });
        }
    }
}
#[test]
fn capacity_header_limits_and_correspondence_refuse_the_whole_tree() {
    let bytes = b"X: y\n\nbody";
    for kind in 0..5 {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 128];
            let mut body = [0; 8];
            let mut parts = [Part::default(); 1];
            let mut headers = [None; 1];
            let mut settings = settings(0, bytes.len());
            if kind == 3 {
                settings.limits.header_bytes = 1;
            }
            let mut cursor = Cursor::new(
                &mut source,
                settings,
                Buffers {
                    header_bytes: if kind == 0 { &mut [] } else { &mut arena },
                    body: &mut body,
                    parts: if kind == 1 { &mut [] } else { &mut parts },
                    headers: if kind == 2 { &mut [] } else { &mut headers },
                },
                &mut work,
                &mut budget,
                Tick(1),
            )
            .unwrap();
            let expected = match kind {
                0 => Error::Headers(root_headers::Error::Capacity),
                1 | 2 => Error::Interpretation(mime_structure::Error::OutputCapacity),
                3 => Error::Headers(root_headers::Error::Headers(
                    crate::mime_headers::Error::HeaderLimit,
                )),
                _ => {
                    while !cursor.parsed {
                        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                    }
                    cursor.headers[0].as_mut().unwrap().end.body_start += 1;
                    Error::State
                }
            };
            assert_eq!(drain(&mut cursor), Err(expected));
            assert_eq!(cursor.poll(Tick(1)), Err(expected));
            assert!(matches!(cursor.finish(Tick(1)), Err(error) if error == expected));
        });
    }
}
#[test]
fn native_read_turns_stay_bounded_with_large_body_and_replay() {
    let mut bytes = b"Content-Transfer-Encoding: quoted-printable\n\na=".to_vec();
    bytes.extend(std::iter::repeat_n(b' ', 12_000));
    bytes.push(b'z');
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(&bytes, &timer, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 1024];
        let mut body = [0; 8192];
        let mut parts = [Part::default(); 1];
        let mut headers = [None; 1];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, bytes.len()),
            Buffers {
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
        let mut reads = 0;
        for _ in 0..1_000_000 {
            let before = cursor.work.as_ref().map(|work| work.remaining());
            let status = cursor.poll(Tick(1)).unwrap();
            let after = cursor.work.as_ref().map(|work| work.remaining());
            if let (Some(before), Some(after)) = (before, after) {
                let io = before.io_bytes - after.io_bytes;
                assert!(io <= 4096);
                assert!(before.records - after.records <= 32);
                assert!(before.output_bytes - after.output_bytes <= 96);
                if io > 352 {
                    reads += 1;
                }
            }
            if status == Status::Complete {
                break;
            }
        }
        assert!(
            reads >= 6,
            "long QP replay must reread discarded native chunks"
        );
        let retained = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.parts[0].size, 12_003);
        assert_eq!(
            retained.parts[0].problems(),
            mime_structure::ENCODING_PROBLEM
        );
    });
}

#[test]
fn sequential_large_multipart_boundaries_cross_native_read_chunks() {
    for padding in 4092..=4098 {
        let mut bytes = b"Content-Type: multipart/mixed;boundary=x\r\n\r\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'p', padding));
        bytes.extend_from_slice(b"\r\n--x\r\n\r\n");
        let body_start = bytes.len();
        bytes.extend(std::iter::repeat_n(b'a', 16_384));
        bytes.extend_from_slice(b"\r\n--x--\r\nepilogue");
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(&bytes, &timer, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 2048];
            let mut body = [0; 4096];
            let mut parts = [Part::default(); 2];
            let mut headers = [None; 2];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, bytes.len()),
                Buffers {
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
            drain(&mut cursor).unwrap();
            let retained = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.parts.len(), 2);
            assert_eq!(retained.parts[0].media, mime_structure::Media::Multipart);
            assert_eq!(retained.parts[0].problems(), 0);
            assert_eq!(retained.parts[1].body_start, body_start as u64);
            assert_eq!(retained.parts[1].entity_end, (body_start + 16_384) as u64);
            assert_eq!(retained.parts[1].size, 16_384);
            assert_eq!(retained.parts[1].problems(), 0);
        });
    }
}
#[test]
fn empty_body_backing_refuses_as_output_capacity() {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 8];
        let mut parts = [Part::default(); 1];
        let mut headers = [None; 1];
        let result = Cursor::new(
            &mut source,
            settings(0, 1),
            Buffers {
                header_bytes: &mut arena,
                body: &mut [],
                parts: &mut parts,
                headers: &mut headers,
            },
            &mut work,
            &mut budget,
            Tick(1),
        );
        assert!(matches!(
            result,
            Err(Error::Interpretation(mime_structure::Error::OutputCapacity))
        ));
    });
}
#[test]
fn ordinary_folded_and_eof_headers_match_resident_accounting() {
    for bytes in [
        b"Subject: one\r\n continued\nReceived: a\r\nX-Root: y\nContent-Type: multipart/mixed;boundary=x\r\n\r\n--x\nSubject: child\n folded\r\nX-Child: z\n\nbody\r\n--x--".as_slice(),
        b"Subject: header only\r\n folded\nX-Last: end",
        b"X-One: a\n\nbody",
    ] {
        compare_native(bytes, bytes.len(), 16, 8);
    }
}
fn compare_native(bytes: &[u8], length: usize, max_parts: u16, max_depth: u16) {
    let mut options = settings(0, length);
    options.limits.mime_parts = usize::from(max_parts);
    options.limits.mime_depth = usize::from(max_depth);
    let mut parts = vec![Part::default(); usize::from(max_parts)];
    let mut work = meter(capacity());
    let mut budget = HeaderBudget::new();
    let mut resident = mime_structure::Cursor::new(
        &bytes[..length],
        0,
        SourceEnd::Eof,
        options.limits,
        &mut parts,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let expected = loop {
        match resident.poll(Tick(1)) {
            Ok(Status::Yield) => (),
            Ok(Status::Complete) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let expected_headers = resident.header_bytes();
    let expected_parts = resident.finish(Tick(1)).ok().map(|(p, _, _)| p.to_vec());
    for width in [1, 7, 4096] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 4096];
            let mut body = vec![0; width];
            let mut parts = vec![Part::default(); usize::from(max_parts)];
            let mut headers = vec![None; usize::from(max_parts)];
            let mut cursor = Cursor::new(
                &mut source,
                Settings {
                    extent: 0..length as u64,
                    limits: options.limits,
                },
                Buffers {
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
            assert_eq!(
                drain(&mut cursor).map(|_| ()),
                expected.map_err(Error::from)
            );
            if expected.is_ok() {
                let retained = cursor.finish(Tick(1)).unwrap();
                assert_eq!(retained.parts, expected_parts.as_ref().unwrap());
                assert_eq!(Some(retained.header_bytes), expected_headers);
            }
        });
    }
}
#[test]
fn native_limits_and_unclosed_extents_match_resident() {
    let bytes=b"Content-Type: multipart/mixed;boundary=x\n\n--x\nContent-Type: multipart/mixed;boundary=y\n\n--y\n\na\n--y--\n--x--\n";
    for (parts, depth) in [(3, 3), (2, 2), (3, 2), (4, 4)] {
        compare_native(bytes, bytes.len(), parts, depth);
    }
    let bytes = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x\n\nb\n--x--\n";
    let end = bytes.windows(7).position(|w| w == b"a\n--x\n\n").unwrap() + 1;
    compare_native(bytes, end, 16, 8);
    compare_native(b"\nabc\n--x--\n", 4, 16, 8);
}
#[test]
fn correspondence_corruption_and_finish_checks_refuse() {
    for kind in 0..6 {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"X: y\n\nbody", &timer, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 1024];
            let mut body = [0; 8];
            let mut parts = [Part::default(); 1];
            let mut headers = [None; 1];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, 10),
                Buffers {
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
            while !cursor.parsed {
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            }
            match kind {
                0 => cursor.headers[0].as_mut().unwrap().base += 1,
                1 => cursor.headers[0].as_mut().unwrap().end.header_bytes += 1,
                2 => cursor.headers[0] = None,
                3 => cursor.used = 0,
                _ => {
                    drain(&mut cursor).unwrap();
                    if kind == 4 {
                        cursor.checked_headers += 1;
                    } else {
                        cursor.used += 1;
                        cursor.checked += 1;
                    }
                    assert!(matches!(cursor.finish(Tick(1)), Err(Error::State)));
                    return;
                }
            }
            assert_eq!(drain(&mut cursor), Err(Error::State));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::State));
            assert!(matches!(cursor.finish(Tick(1)), Err(Error::State)));
        });
    }
}
#[test]
fn native_finish_pre_and_post_fences_expire_at_exact_samples() {
    for expiry in [1, 3, 5] {
        let timer = Timer::new();
        crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 8];
            let mut body = [0; 1];
            let mut parts = [Part::default(); 1];
            let mut headers = [None; 1];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, 1),
                Buffers {
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
            drain(&mut cursor).unwrap();
            let before = timer.calls.load(Ordering::Relaxed);
            timer.after(expiry);
            let result = cursor.finish(Tick(1));
            if expiry == 5 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::Policy(PolicyError::Deadline))));
            }
            assert_eq!(timer.calls.load(Ordering::Relaxed) - before, expiry.min(4));
        });
    }
}
#[test]
fn native_constructor_refuses_extent_outside_pin() {
    let timer = Timer::new();
    crate::store_fs::with_pinned_fixture(b"\n", &timer, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 8];
        let mut body = [0; 1];
        let mut parts = [Part::default(); 1];
        let mut headers = [None; 1];
        let result = Cursor::new(
            &mut source,
            settings(0, 2),
            Buffers {
                header_bytes: &mut arena,
                body: &mut body,
                parts: &mut parts,
                headers: &mut headers,
            },
            &mut work,
            &mut budget,
            Tick(1),
        );
        assert!(matches!(
            result,
            Err(Error::Headers(root_headers::Error::Range))
        ));
    });
}

#[test]
fn batched_structure_preserves_tree_and_work_with_fewer_native_checks() {
    let mut bytes = b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\n".to_vec();
    for _ in 0..2048 {
        bytes.extend_from_slice(
            b"QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB\r\n",
        );
    }
    bytes.extend_from_slice(b"!\r\n--x--\r\nepilogue");
    let run = |batched| {
        let native = Timer::new();
        let job = Timer::new();
        let mut result = None;
        crate::store_fs::with_pinned_fixture(&bytes, &native, |mut source| {
            let mut work = meter(capacity());
            let mut budget = HeaderBudget::new();
            let mut arena = [0; 1024];
            let mut body = [0; 4096];
            let mut parts = [Part::default(); 4];
            let mut headers = [None; 4];
            let mut cursor = Cursor::new(
                &mut source,
                settings(0, bytes.len()),
                Buffers {
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
            loop {
                let before = job.calls.load(Ordering::Relaxed);
                let status = if batched {
                    cursor.poll_batch(&job).unwrap()
                } else {
                    cursor.poll(Tick(1)).unwrap()
                };
                assert!(job.calls.load(Ordering::Relaxed) - before <= 65);
                if status == Status::Complete {
                    break;
                }
            }
            let retained = cursor.finish(Tick(1)).unwrap();
            result = Some((
                retained.parts.to_vec(),
                retained.header_bytes,
                retained.work.remaining(),
                native.calls.load(Ordering::Relaxed),
            ));
        });
        result.unwrap()
    };
    let scalar = run(false);
    let batched = run(true);
    assert_eq!(batched.0, scalar.0);
    assert_eq!(batched.1, scalar.1);
    assert_eq!(batched.2, scalar.2);
    assert_eq!(batched.0.len(), 2);
    assert!(batched.3 * 4 < scalar.3, "{} vs {}", batched.3, scalar.3);
}

#[test]
fn batched_structure_clock_expiry_is_sticky_inside_the_batch() {
    let bytes = b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\na\n--x--\nepi";
    let native = Timer::new();
    let job = Timer::new();
    crate::store_fs::with_pinned_fixture(bytes, &native, |mut source| {
        let mut work = meter(capacity());
        let mut budget = HeaderBudget::new();
        let mut arena = [0; 1024];
        let mut body = [0; 7];
        let mut parts = [Part::default(); 4];
        let mut headers = [None; 4];
        let mut cursor = Cursor::new(
            &mut source,
            settings(0, bytes.len()),
            Buffers {
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
        job.after(3);
        let error = cursor.poll_batch(&job).unwrap_err();
        assert_eq!(job.calls.load(Ordering::Relaxed), 3);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.poll_batch(&native), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    });
}
