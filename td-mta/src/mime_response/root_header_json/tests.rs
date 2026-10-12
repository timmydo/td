#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::root_headers::Cursor as Reader;
use crate::{
    admission::work::Charge,
    header_property,
    ports::{Clock, Deadline, Time},
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
static FIXTURE: Mutex<()> = Mutex::new(());
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
            io_bytes: 64 * 1024 * 1024,
            records: 1_000_000,
            output_bytes: 1_000_000,
            unlinks: 0,
        },
    )
}
fn property(key: &str) -> Property<'_> {
    let mut p = header_property::Cursor::new(key, header_property::Context::Email);
    let mut work = work();
    for _ in 0..1000 {
        if let header_property::Status::Complete(value) = p.poll(Tick(1), &mut work).unwrap() {
            return value.unwrap();
        }
    }
    panic!("property completion");
}
fn collect(cursor: &mut Reader<'_, '_, '_, '_, '_>) {
    for _ in 0..1000 {
        if cursor.poll(Tick(1)).unwrap() == crate::root_headers::Status::Complete {
            return;
        }
    }
    panic!("input completion");
}
fn drain(cursor: &mut Cursor<'_, '_, '_>, width: usize) -> Vec<u8> {
    let mut result = Vec::new();
    let mut buffer = [0; 64];
    for _ in 0..100_000 {
        let progress = cursor.poll(Tick(1), &mut buffer[..width]).unwrap();
        result.extend_from_slice(&buffer[..progress.written]);
        if progress.status == Status::Complete {
            return result;
        }
    }
    panic!("JSON completion");
}
const CASES: &[(&str, &str)] = &[
    ("subject", "\"café\""),
    (
        "header:Subject:asRaw:all",
        "[\" first\",\" =?UTF-8?Q?caf=C3=A9?=\"]",
    ),
    (
        "from",
        "[{\"name\":\"Alice\",\"email\":\"a@example.test\"}]",
    ),
    (
        "header:From:asGroupedAddresses",
        "[{\"name\":null,\"addresses\":[{\"name\":\"Alice\",\"email\":\"a@example.test\"}]}]",
    ),
    ("messageId", "[\"x@example.test\"]"),
    ("sentAt", "\"1997-11-21T15:55:06Z\""),
    ("header:List-Help:asURLs", "[\"https://example.test/help\"]"),
    ("header:X-Absent:asText", "null"),
    ("header:Absent:asText", "null"),
    ("header:Absent:asText:all", "[]"),
];
#[test]
fn source_ordered_headers_and_all_value_forms_share_original_custody() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let timer = Timer(AtomicU64::new(1));
    let bytes = b"Subject: first\r\nSubject: =?UTF-8?Q?caf=C3=A9?=\r\nFrom: Alice <a@example.test>\r\nMessage-ID: <x@example.test>\r\nDate: Fri, 21 Nov 1997 09:55:06 -0600\r\nList-Help: <https://example.test/help>\r\n\r\nbody";
    crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
        let mut work = work();
        let mut buffer = [0; 512];
        let mut reader = Reader::new(&mut source, 500, &mut buffer, &mut work, Tick(1)).unwrap();
        collect(&mut reader);
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut input = reader
            .into_json(&mut budget, &mut scratch, Tick(1))
            .unwrap();
        for width in [1, 7, 64] {
            for &(key, expected) in CASES {
                let before = input.budget.source_bytes_remaining();
                let mut cursor = input
                    .project(Selection::Value(property(key)), Tick(1))
                    .unwrap();
                assert_eq!(
                    cursor.poll(Tick(1), &mut []).unwrap().status,
                    Status::NeedOutput
                );
                let json = drain(&mut cursor, width);
                assert_eq!(String::from_utf8(json).unwrap(), expected, "{key}");
                assert_eq!(
                    cursor.finish(Tick(1)).unwrap(),
                    Diagnostics {
                        is_encoding_problem: false,
                        has_unverified_leap: false
                    }
                );
                assert!(input.budget.source_bytes_remaining() < before);
            }
            let before = input.budget.source_bytes_remaining();
            let mut cursor = input.project(Selection::Headers, Tick(1)).unwrap();
            assert_eq!(
                cursor.poll(Tick(1), &mut []).unwrap().status,
                Status::NeedOutput
            );
            let json = drain(&mut cursor, width);
            assert_eq!(
                String::from_utf8(json).unwrap(),
                concat!(
                    "[{\"name\":\"Subject\",\"value\":\" first\"},",
                    "{\"name\":\"Subject\",\"value\":\" =?UTF-8?Q?caf=C3=A9?=\"},",
                    "{\"name\":\"From\",\"value\":\" Alice <a@example.test>\"},",
                    "{\"name\":\"Message-ID\",\"value\":\" <x@example.test>\"},",
                    "{\"name\":\"Date\",\"value\":\" Fri, 21 Nov 1997 09:55:06 -0600\"},",
                    "{\"name\":\"List-Help\",\"value\":\" <https://example.test/help>\"}]"
                )
            );
            cursor.finish(Tick(1)).unwrap();
            assert!(input.budget.source_bytes_remaining() < before);
        }
        assert!(input.budget.source_bytes_remaining() < 16 * 1024 * 1024);
        input.finish(Tick(1)).unwrap();
    });
}
#[test]
fn abandoned_projection_and_expiry_retire_the_whole_input() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for fault in [
        "drop",
        "premature",
        "request",
        "native",
        "finish-request",
        "finish-native",
    ] {
        let timer = Timer(AtomicU64::new(1));
        crate::store_fs::with_pinned_fixture(b"Subject: test\r\n\r\nbody", &timer, |mut source| {
            let mut work = work();
            let mut buffer = [0; 64];
            let mut reader = Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
            collect(&mut reader);
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut input = reader
                .into_json(&mut budget, &mut scratch, Tick(1))
                .unwrap();
            let mut cursor = input
                .project(Selection::Value(property("subject")), Tick(1))
                .unwrap();
            if fault.starts_with("finish") {
                drain(&mut cursor, 1);
            }
            if fault.ends_with("native") {
                timer.0.store(100, Ordering::Relaxed);
            }
            match fault {
                "drop" => drop(cursor),
                "premature" => assert_eq!(cursor.finish(Tick(1)), Err(Error::State)),
                "finish-request" => {
                    assert!(cursor.finish(Tick(100)).is_err());
                }
                "finish-native" => {
                    assert_eq!(
                        cursor.finish(Tick(1)),
                        Err(Error::Policy(PolicyError::Deadline))
                    );
                }
                _ => {
                    let now = if fault == "request" {
                        Tick(100)
                    } else {
                        Tick(1)
                    };
                    let error = cursor.poll(now, &mut [0; 1]).unwrap_err();
                    assert_eq!(cursor.poll(Tick(1), &mut [0; 1]), Err(error));
                    drop(cursor);
                }
            }
            assert!(input.check_deadline(Tick(1)).is_err());
            assert!(input.project(Selection::Headers, Tick(1)).is_err());
        });
    }
}

#[test]
fn both_parsers_retire_on_output_refusal_and_midstream_expiry() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for headers in [false, true] {
        for fault in ["output", "native", "request"] {
            let timer = Timer(AtomicU64::new(1));
            crate::store_fs::with_pinned_fixture(
                b"Subject: a long value\r\nX: next\r\n\r\nbody",
                &timer,
                |mut source| {
                    let mut meter = work();
                    if fault == "output" {
                        meter = Meter::new(
                            Deadline::after(Tick(0), 100).unwrap(),
                            Charge {
                                io_bytes: 1_000_000,
                                records: 100_000,
                                output_bytes: 2,
                                unlinks: 0,
                            },
                        );
                    }
                    let mut buffer = [0; 128];
                    let mut reader =
                        Reader::new(&mut source, 126, &mut buffer, &mut meter, Tick(1)).unwrap();
                    collect(&mut reader);
                    let mut budget = HeaderBudget::new();
                    let mut scratch = Scratch::new();
                    let mut input = reader
                        .into_json(&mut budget, &mut scratch, Tick(1))
                        .unwrap();
                    let selection = if headers {
                        Selection::Headers
                    } else {
                        Selection::Value(property("subject"))
                    };
                    let mut cursor = input.project(selection, Tick(1)).unwrap();
                    let mut emitted = 0;
                    let mut failure = None;
                    for _ in 0..10000 {
                        if fault != "output" && emitted >= 2 {
                            if fault == "native" {
                                timer.0.store(100, Ordering::Relaxed);
                            }
                            let now = if fault == "request" {
                                Tick(100)
                            } else {
                                Tick(1)
                            };
                            let mut untouched = [0xa5; 8];
                            failure = Some(cursor.poll(now, &mut untouched).unwrap_err());
                            assert_eq!(untouched, [0xa5; 8]);
                            break;
                        }
                        match cursor.poll(Tick(1), &mut [0; 1]) {
                            Ok(progress) => {
                                emitted += progress.written;
                                assert_ne!(progress.status, Status::Complete);
                            }
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    let error = failure.unwrap();
                    assert!(emitted > 0);
                    assert_eq!(cursor.poll(Tick(1), &mut [0; 1]), Err(error));
                    drop(cursor);
                    let remaining = input.work.remaining();
                    assert_eq!(input.check_deadline(Tick(1)), Err(error));
                    assert!(
                        matches!(input.project(Selection::Headers, Tick(1)), Err(e) if e == error)
                    );
                    assert_eq!(input.work.remaining(), remaining);
                },
            );
        }
    }
}
#[test]
fn empty_and_malformed_input_preserve_whole_headers_and_diagnostics() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (bytes, expected, encoding) in [
        (b"".as_slice(), "[]", false),
        (
            b"X: one\r\nX: two\r\n\r\nbody".as_slice(),
            "[{\"name\":\"X\",\"value\":\" one\"},{\"name\":\"X\",\"value\":\" two\"}]",
            false,
        ),
        (
            b"X: \xff\r\n\r\nbody".as_slice(),
            "[{\"name\":\"X\",\"value\":\" �\"}]",
            true,
        ),
    ] {
        let timer = Timer(AtomicU64::new(1));
        crate::store_fs::with_pinned_fixture(bytes, &timer, |mut source| {
            let mut work = work();
            let mut buffer = [0; 128];
            let mut reader =
                Reader::new(&mut source, 126, &mut buffer, &mut work, Tick(1)).unwrap();
            collect(&mut reader);
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut input = reader
                .into_json(&mut budget, &mut scratch, Tick(1))
                .unwrap();
            let mut cursor = input.project(Selection::Headers, Tick(1)).unwrap();
            assert_eq!(String::from_utf8(drain(&mut cursor, 1)).unwrap(), expected);
            assert_eq!(
                cursor.finish(Tick(1)).unwrap().is_encoding_problem,
                encoding
            );
        });
    }
}

#[test]
fn forgotten_or_unfinished_complete_projections_cannot_reopen_input() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for headers in [false, true] {
        for complete in [false, true] {
            for forget in [false, true] {
                let timer = Timer(AtomicU64::new(1));
                crate::store_fs::with_pinned_fixture(
                    b"Subject: text\r\n\r\nbody",
                    &timer,
                    |mut source| {
                        let mut work = work();
                        let mut buffer = [0; 64];
                        let mut reader =
                            Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
                        collect(&mut reader);
                        let mut budget = HeaderBudget::new();
                        let mut scratch = Scratch::new();
                        let mut input = reader
                            .into_json(&mut budget, &mut scratch, Tick(1))
                            .unwrap();
                        let selection = if headers {
                            Selection::Headers
                        } else {
                            Selection::Value(property("subject"))
                        };
                        let mut cursor = input.project(selection, Tick(1)).unwrap();
                        if complete {
                            drain(&mut cursor, 1);
                        }
                        if forget {
                            std::mem::forget(cursor);
                        } else {
                            drop(cursor);
                        }
                        assert_eq!(input.check_deadline(Tick(1)), Err(Error::State));
                        assert!(matches!(
                            input.project(Selection::Headers, Tick(1)),
                            Err(Error::State)
                        ));
                    },
                );
            }
        }
    }
}
#[test]
fn end_disagreement_and_incomplete_or_expired_collection_refuse_handoff() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for headers in [false, true] {
        let timer = Timer(AtomicU64::new(1));
        crate::store_fs::with_pinned_fixture(b"Subject: text\r\n\r\nbody", &timer, |mut source| {
            let mut work = work();
            let mut buffer = [0; 64];
            let mut reader = Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
            collect(&mut reader);
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut input = reader
                .into_json(&mut budget, &mut scratch, Tick(1))
                .unwrap();
            let selection = if headers {
                Selection::Headers
            } else {
                Selection::Value(property("subject"))
            };
            let mut cursor = input.project(selection, Tick(1)).unwrap();
            cursor.expected.body_start += 1;
            let mut failed = false;
            for _ in 0..10000 {
                match cursor.poll(Tick(1), &mut [0; 64]) {
                    Ok(p) => assert_ne!(p.status, Status::Complete),
                    Err(e) => {
                        assert_eq!(e, Error::State);
                        failed = true;
                        break;
                    }
                }
            }
            assert!(failed);
            drop(cursor);
            assert_eq!(input.check_deadline(Tick(1)), Err(Error::State));
        });
    }
    for fault in ["incomplete", "native", "request"] {
        let timer = Timer(AtomicU64::new(1));
        crate::store_fs::with_pinned_fixture(b"Subject: text\r\n\r\nbody", &timer, |mut source| {
            let mut work = work();
            let mut buffer = [0; 64];
            let mut reader = Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
            if fault != "incomplete" {
                collect(&mut reader);
            }
            if fault == "native" {
                timer.0.store(100, Ordering::Relaxed);
            }
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let now = if fault == "request" {
                Tick(100)
            } else {
                Tick(1)
            };
            let expected = match fault {
                "incomplete" => crate::root_headers::Error::State,
                "native" => crate::root_headers::Error::Policy(PolicyError::Deadline),
                _ => crate::root_headers::Error::Work(Stop::Deadline),
            };
            assert!(
                matches!(reader.into_json(&mut budget, &mut scratch, now), Err(Error::Read(e)) if e == expected)
            );
        });
    }
}

#[test]
fn aggregate_header_exhaustion_retires_construction_or_active_projection() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for headers in [false, true] {
        for already_exhausted in [false, true] {
            let timer = Timer(AtomicU64::new(1));
            crate::store_fs::with_pinned_fixture(
                b"Subject: text\r\n\r\nbody",
                &timer,
                |mut source| {
                    let mut work = work();
                    let mut buffer = [0; 64];
                    let mut reader =
                        Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
                    collect(&mut reader);
                    let mut budget = HeaderBudget::new();
                    let mut scratch = Scratch::new();
                    let mut input = reader
                        .into_json(&mut budget, &mut scratch, Tick(1))
                        .unwrap();
                    let bytes = input.budget.source_bytes_remaining();
                    let result = input.budget.charge(
                        input.work,
                        Tick(1),
                        if already_exhausted {
                            bytes + 1
                        } else {
                            bytes - 1
                        },
                        0,
                        &mut crate::nfc::Credit::new(),
                    );
                    assert_eq!(result.is_err(), already_exhausted);
                    let selection = if headers {
                        Selection::Headers
                    } else {
                        Selection::Value(property("subject"))
                    };
                    let error = match input.project(selection, Tick(1)) {
                        Err(error) => error,
                        Ok(mut cursor) => {
                            let mut failure = None;
                            for _ in 0..10000 {
                                match cursor.poll(Tick(1), &mut [0; 64]) {
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                    Ok(p) => assert_ne!(p.status, Status::Complete),
                                }
                            }
                            failure.unwrap()
                        }
                    };
                    assert_eq!(input.check_deadline(Tick(1)), Err(error));
                    let remaining = input.work.remaining();
                    assert!(
                        matches!(input.project(Selection::Headers, Tick(1)), Err(e) if e == error)
                    );
                    assert_eq!(input.work.remaining(), remaining);
                },
            );
        }
    }
}

struct SampleTimer {
    samples: AtomicU64,
    expires: AtomicU64,
}
impl Clock for SampleTimer {
    fn sample(&self) -> Result<Time, PolicyError> {
        let sample = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Time {
            monotonic: Tick(if sample >= self.expires.load(Ordering::Relaxed) {
                100
            } else {
                1
            }),
            utc_ms: 0,
        })
    }
}
#[test]
fn post_turn_and_cached_completion_checks_retire_both_parsers() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for headers in [false, true] {
        for fault in [
            "post-turn",
            "cached-native",
            "cached-request",
            "idle-native",
            "idle-request",
        ] {
            let timer = SampleTimer {
                samples: AtomicU64::new(0),
                expires: AtomicU64::new(u64::MAX),
            };
            crate::store_fs::with_pinned_fixture(
                b"Subject: text\r\n\r\nbody",
                &timer,
                |mut source| {
                    let mut work = work();
                    let mut buffer = [0; 64];
                    let mut reader =
                        Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
                    collect(&mut reader);
                    let mut budget = HeaderBudget::new();
                    let mut scratch = Scratch::new();
                    let mut input = reader
                        .into_json(&mut budget, &mut scratch, Tick(1))
                        .unwrap();
                    let selection = if headers {
                        Selection::Headers
                    } else {
                        Selection::Value(property("subject"))
                    };
                    let mut cursor = input.project(selection, Tick(1)).unwrap();
                    if fault != "post-turn" {
                        drain(&mut cursor, 1);
                    }
                    if fault.starts_with("idle") {
                        cursor.finish(Tick(1)).unwrap();
                        let expected = if fault.ends_with("native") {
                            timer.expires.store(0, Ordering::Relaxed);
                            Error::Policy(PolicyError::Deadline)
                        } else {
                            Error::Work(Stop::Deadline)
                        };
                        let now = if fault.ends_with("request") {
                            Tick(100)
                        } else {
                            Tick(1)
                        };
                        assert_eq!(input.check_deadline(now), Err(expected));
                        let remaining = input.work.remaining();
                        assert_eq!(input.check_deadline(Tick(1)), Err(expected));
                        assert_eq!(input.work.remaining(), remaining);
                        assert_eq!(input.finish(Tick(1)), Err(expected));
                    } else {
                        if fault == "post-turn" {
                            timer.expires.store(
                                timer.samples.load(Ordering::Relaxed) + 3,
                                Ordering::Relaxed,
                            );
                        } else if fault == "cached-native" {
                            timer.expires.store(0, Ordering::Relaxed);
                        }
                        let now = if fault == "cached-request" {
                            Tick(100)
                        } else {
                            Tick(1)
                        };
                        let mut bytes = [0xa5; 64];
                        let error = cursor.poll(now, &mut bytes).unwrap_err();
                        if fault != "cached-request" {
                            assert_eq!(error, Error::Policy(PolicyError::Deadline));
                        }
                        if fault == "post-turn" && headers {
                            assert_eq!(bytes[0], b'[');
                        }
                        assert_eq!(cursor.poll(Tick(1), &mut bytes), Err(error));
                        drop(cursor);
                        assert_eq!(input.finish(Tick(1)), Err(error));
                    }
                },
            );
        }
    }
}
#[test]
fn admitted_unknown_text_grammar_preserves_existing_refusal() {
    let _guard = FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for key in ["header:Unknown:asText", "header:Unknown:asText:all"] {
        let timer = Timer(AtomicU64::new(1));
        crate::store_fs::with_pinned_fixture(
            b"Unknown: value\r\n\r\nbody",
            &timer,
            |mut source| {
                let mut work = work();
                let mut buffer = [0; 64];
                let mut reader =
                    Reader::new(&mut source, 62, &mut buffer, &mut work, Tick(1)).unwrap();
                collect(&mut reader);
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut input = reader
                    .into_json(&mut budget, &mut scratch, Tick(1))
                    .unwrap();
                let mut cursor = input
                    .project(Selection::Value(property(key)), Tick(1))
                    .unwrap();
                let mut failed = false;
                let mut prefix = Vec::new();
                for _ in 0..1000 {
                    let mut output = [0xa5; 64];
                    match cursor.poll(Tick(1), &mut output) {
                        Ok(p) => {
                            assert_ne!(p.status, Status::Complete);
                            prefix.extend_from_slice(&output[..p.written]);
                        }
                        Err(error) => {
                            assert_eq!(
                                error,
                                Error::Value(header_value::Error::UnsupportedGrammar)
                            );
                            assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                            assert_eq!(output, [0xa5; 64]);
                            failed = true;
                            break;
                        }
                    }
                }
                assert!(failed);
                assert_eq!(
                    prefix,
                    if key.ends_with(":all") {
                        b"[".as_slice()
                    } else {
                        b""
                    }
                );
                drop(cursor);
                assert_eq!(
                    input.finish(Tick(1)),
                    Err(Error::Value(header_value::Error::UnsupportedGrammar))
                );
            },
        );
    }
}
