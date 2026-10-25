#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    body_charset::Plan,
    ports::{BlobReader, Deadline, Time},
    wire::TransferEncoding,
};
use std::sync::atomic::{AtomicU64, Ordering};

struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, PolicyError> {
        Ok(Time {
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
            utc_ms: 0,
        })
    }
}
struct Bytes<'a> {
    bytes: &'a [u8],
    reads: usize,
    fail_at: Option<usize>,
}
impl BlobReader for Bytes<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
        self.reads += 1;
        if self.fail_at.is_some_and(|at| offset as usize >= at) {
            return Err(PolicyError::Corrupt);
        }
        let bytes = self
            .bytes
            .get(offset as usize..)
            .ok_or(PolicyError::Invalid)?;
        let count = bytes.len().min(output.len()).min(2);
        output[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
}
fn work(output_bytes: u64) -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100000,
            records: 100000,
            output_bytes,
            unlinks: 0,
        },
    )
}
fn input<'a>(source: &'a mut Bytes<'_>, encoding: TransferEncoding, charset: Plan) -> Input<'a> {
    let length = source.len();
    Input {
        source,
        offset: 0,
        length,
        encoding,
        charset,
    }
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_>, width: usize) -> Vec<u8> {
    let mut result = Vec::new();
    for _ in 0..100000 {
        let mut output = [0xa5; 64];
        let p = cursor.poll(&mut output[..width]).unwrap();
        assert!(p.written <= MAX_CHUNK_BYTES);
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        result.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return result;
        }
        assert!(cursor.value().is_none());
    }
    panic!("body value did not complete")
}
#[test]
fn literal_body_json_combines_decoding_filtering_and_final_diagnostics() {
    let cases: &[(&[u8], TransferEncoding, Plan, u64, &str)] = &[
        (b"", TransferEncoding::Identity, Plan::from_label(Some(b"utf-8")), 0,
         r#"{"value":"","isEncodingProblem":false,"isTruncated":false}"#),
        (b"a\r\n\"\\\0\te\xcc\x81\r", TransferEncoding::Identity, Plan::from_label(Some(b"utf-8")), 0,
         "{\"value\":\"a\\n\\\"\\\\\\u0000\\té\\r\",\"isEncodingProblem\":false,\"isTruncated\":false}"),
        (b"Y2Fmw6kNCg==", TransferEncoding::Base64, Plan::from_label(Some(b"utf-8")), 0,
         "{\"value\":\"café\\n\",\"isEncodingProblem\":false,\"isTruncated\":false}"),
        (b"caf=E9", TransferEncoding::QuotedPrintable, Plan::from_label(Some(b"latin1")), 0,
         "{\"value\":\"café\",\"isEncodingProblem\":false,\"isTruncated\":false}"),
        (b"caf\xc3\xa9", TransferEncoding::Identity, Plan::from_label(None), 0,
         "{\"value\":\"café\",\"isEncodingProblem\":true,\"isTruncated\":false}"),
        (b"a\xff", TransferEncoding::Identity, Plan::from_label(Some(b"utf-8")), 1,
         r#"{"value":"a","isEncodingProblem":true,"isTruncated":true}"#),
        (b"a\xef\xb7\x90", TransferEncoding::Identity, Plan::from_label(Some(b"utf-8")), 1,
         r#"{"value":"a","isEncodingProblem":true,"isTruncated":true}"#),
        (b"a", TransferEncoding::Identity, Plan::from_label(Some(b"unknown")), 1,
         r#"{"value":"a","isEncodingProblem":true,"isTruncated":false}"#),
    ];
    for &(bytes, encoding, charset, cap, expected) in cases {
        for width in [1, 3, 64] {
            let timer = Timer(AtomicU64::new(1));
            let mut source = Bytes {
                bytes,
                reads: 0,
                fail_at: None,
            };
            let mut work = work(10000);
            let mut buffer = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let mut cursor = Cursor::new(
                input(&mut source, encoding, charset),
                &mut buffer,
                &mut checkpoints,
                cap,
                &mut work,
                &timer,
            )
            .unwrap();
            assert_eq!(cursor.poll(&mut []).unwrap().status, Status::NeedOutput);
            assert_eq!(drain(&mut cursor, width), expected.as_bytes());
            let diagnostics = cursor.value().unwrap();
            assert_eq!(cursor.finish().unwrap(), diagnostics);
            if !bytes.is_empty() {
                assert!(source.reads > 0);
            }
        }
    }
}
#[test]
fn truncation_uses_utf8_bytes_before_json_escaping_and_validates_the_tail() {
    for (cap, value, truncated) in [
        (0, "é\n\0z", false),
        (1, "", true),
        (2, "é", true),
        (3, "é\n", true),
        (4, "é\n\0", true),
        (5, "é\n\0z", false),
    ] {
        let timer = Timer(AtomicU64::new(1));
        let mut source = Bytes {
            bytes: b"\xc3\xa9\r\n\0z",
            reads: 0,
            fail_at: None,
        };
        let mut work = work(10000);
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new(
            input(
                &mut source,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            cap,
            &mut work,
            &timer,
        )
        .unwrap();
        let bytes = drain(&mut cursor, 1);
        assert_eq!(
            cursor.finish().unwrap(),
            Diagnostics {
                is_encoding_problem: false,
                is_truncated: truncated
            }
        );
        let text = std::str::from_utf8(&bytes).unwrap();
        let json = td_json::parse(text).unwrap();
        assert_eq!(json["value"].as_str(), Some(value));
        assert_eq!(json["isTruncated"].as_bool(), Some(truncated));
        assert_eq!(source.reads, source.bytes.len());
        // Identity transfer funds its six bytes; Plain and JSON add their output.
        assert_eq!(
            10000 - work.remaining().output_bytes,
            source.bytes.len() as u64 + value.len() as u64 + bytes.len() as u64
        );
    }
}
#[test]
fn late_input_failure_after_the_cap_never_completes_a_partial_value() {
    let timer = Timer(AtomicU64::new(1));
    let mut source = Bytes {
        bytes: b"abc",
        reads: 0,
        fail_at: Some(2),
    };
    let mut work = work(10000);
    let mut buffer = [0; 1];
    let mut checkpoints = Checkpoints::default();
    let mut cursor = Cursor::new(
        input(
            &mut source,
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
        ),
        &mut buffer,
        &mut checkpoints,
        1,
        &mut work,
        &timer,
    )
    .unwrap();
    let mut refused = false;
    let mut prefix = Vec::new();
    for _ in 0..1000 {
        let mut output = [0xa5; 1];
        match cursor.poll(&mut output) {
            Ok(p) => {
                assert_ne!(p.status, Status::Complete);
                prefix.extend_from_slice(&output[..p.written]);
            }
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Text(mime_text::Error::Input(crate::mime_input::Error::Policy(
                        PolicyError::Corrupt
                    )))
                );
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(&mut output), Err(error));
                assert_eq!(cursor.check(), Err(error));
                assert_eq!(output, [0xa5]);
                assert_eq!(cursor.finish(), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(prefix, br#"{"value":"a"#);
}
#[test]
fn every_output_budget_cut_refuses_without_successful_metadata() {
    let expected = br#"{"value":"a","isEncodingProblem":false,"isTruncated":false}"#;
    let required = expected.len() as u64 + 2; // One decoded byte and one Plain byte.
    for cap in 0..=required {
        let timer = Timer(AtomicU64::new(1));
        let mut source = Bytes {
            bytes: b"a",
            reads: 0,
            fail_at: None,
        };
        let mut work = work(cap);
        let mut buffer = [0; 2];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new(
            input(
                &mut source,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            0,
            &mut work,
            &timer,
        )
        .unwrap();
        let mut done = false;
        let mut prefix = Vec::new();
        for _ in 0..1000 {
            let mut output = [0xa5; 1];
            match cursor.poll(&mut output) {
                Ok(p) => {
                    prefix.extend_from_slice(&output[..p.written]);
                    if p.status == Status::Complete {
                        assert_eq!(cap, required);
                        assert_eq!(prefix, expected);
                        assert_eq!(
                            cursor.finish().unwrap(),
                            Diagnostics {
                                is_encoding_problem: false,
                                is_truncated: false,
                            }
                        );
                        done = true;
                        break;
                    }
                }
                Err(error) => {
                    assert!(cap < required);
                    assert_eq!(error, Error::Work(Stop::OutputBytes));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(&mut output), Err(error));
                    assert_eq!(cursor.finish(), Err(error));
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
    }
}
#[test]
fn premature_finish_and_fresh_final_clock_checks_withhold_success() {
    for fault in ["premature", "expiry", "regression"] {
        let timer = Timer(AtomicU64::new(2));
        let mut source = Bytes {
            bytes: b"",
            reads: 0,
            fail_at: None,
        };
        let mut work = work(10000);
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new(
            input(
                &mut source,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            0,
            &mut work,
            &timer,
        )
        .unwrap();
        if fault == "premature" {
            assert_eq!(cursor.finish(), Err(Error::State));
            continue;
        }
        drain(&mut cursor, 1);
        timer
            .0
            .store(if fault == "expiry" { 100 } else { 1 }, Ordering::Relaxed);
        let error = if fault == "expiry" {
            Error::Work(Stop::Deadline)
        } else {
            Error::Policy(PolicyError::Invalid)
        };
        assert_eq!(cursor.poll(&mut []), Err(error));
        assert!(cursor.value().is_none());
        timer.0.store(2, Ordering::Relaxed);
        assert_eq!(cursor.finish(), Err(error));
    }
}

#[test]
fn expiry_on_final_post_copy_check_hides_complete_diagnostics() {
    struct Cutoff {
        calls: AtomicU64,
        at: AtomicU64,
    }
    impl Clock for Cutoff {
        fn sample(&self) -> Result<Time, PolicyError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(Time {
                monotonic: Tick(if call >= self.at.load(Ordering::Relaxed) {
                    100
                } else {
                    1
                }),
                utc_ms: 0,
            })
        }
    }
    let timer = Cutoff {
        calls: AtomicU64::new(0),
        at: AtomicU64::new(u64::MAX),
    };
    let mut source = Bytes {
        bytes: b"",
        reads: 0,
        fail_at: None,
    };
    let mut work = work(10000);
    let mut buffer = [0; 1];
    let mut checkpoints = Checkpoints::default();
    let mut cursor = Cursor::new(
        input(
            &mut source,
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
        ),
        &mut buffer,
        &mut checkpoints,
        0,
        &mut work,
        &timer,
    )
    .unwrap();
    let expected = br#"{"value":"","isEncodingProblem":false,"isTruncated":false}"#;
    let mut prefix = Vec::new();
    for _ in 0..1000 {
        if prefix.len() == expected.len() - 1 {
            break;
        }
        let mut output = [0; 1];
        let progress = cursor.poll(&mut output).unwrap();
        assert_ne!(progress.status, Status::Complete);
        prefix.extend_from_slice(&output[..progress.written]);
    }
    assert_eq!(prefix, expected[..expected.len() - 1]);
    // Pre-turn check and literal charge succeed; the post-copy check expires.
    timer
        .at
        .store(timer.calls.load(Ordering::Relaxed) + 3, Ordering::Relaxed);
    let mut output = [0xa5];
    assert_eq!(cursor.poll(&mut output), Err(Error::Work(Stop::Deadline)));
    assert_eq!(output, [b'}']);
    assert!(cursor.value().is_none());
    output.fill(0xa5);
    assert_eq!(cursor.poll(&mut output), Err(Error::Work(Stop::Deadline)));
    assert_eq!(output, [0xa5]);
    assert_eq!(cursor.finish(), Err(Error::Work(Stop::Deadline)));
}

#[test]
fn post_turn_clock_failure_precedes_step_refusal_and_stays_terminal() {
    struct Fault(AtomicU64);
    impl Clock for Fault {
        fn sample(&self) -> Result<Time, PolicyError> {
            let call = self.0.fetch_add(1, Ordering::Relaxed) + 1;
            if call == 4 {
                return Err(PolicyError::Corrupt);
            }
            Ok(Time {
                monotonic: Tick(1),
                utc_ms: 0,
            })
        }
    }
    let timer = Fault(AtomicU64::new(0));
    let mut source = Bytes {
        bytes: b"",
        reads: 0,
        fail_at: None,
    };
    let mut work = work(0);
    let mut buffer = [0; 1];
    let mut checkpoints = Checkpoints::default();
    let mut cursor = Cursor::new(
        input(
            &mut source,
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
        ),
        &mut buffer,
        &mut checkpoints,
        0,
        &mut work,
        &timer,
    )
    .unwrap();
    let mut output = [0xa5];
    let error = Error::Policy(PolicyError::Corrupt);
    assert_eq!(cursor.poll(&mut output), Err(error));
    assert_eq!(output, [0xa5]);
    assert!(cursor.value().is_none());
    assert_eq!(cursor.poll(&mut output), Err(error));
    assert_eq!(cursor.check(), Err(error));
    assert_eq!(cursor.finish(), Err(error));
    assert_eq!(timer.0.load(Ordering::Relaxed), 4);
    assert_eq!(work.stopped(), Some(Stop::OutputBytes));
}

#[test]
fn html_planning_and_replay_match_literal_json_at_tag_and_scalar_cuts() {
    let cases: &[(&[u8], TransferEncoding, Plan, u64, &str)] = &[
        (
            b"",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            1,
            r#"{"value":"","isEncodingProblem":false,"isTruncated":false}"#,
        ),
        (
            b"<b>hello</b>",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            2,
            r#"{"value":"","isEncodingProblem":false,"isTruncated":true}"#,
        ),
        (
            b"<b>hello</b>",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            3,
            r#"{"value":"<b>","isEncodingProblem":false,"isTruncated":true}"#,
        ),
        (
            b"<b>hello</b>",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            10,
            r#"{"value":"<b>hello","isEncodingProblem":false,"isTruncated":true}"#,
        ),
        (
            b"<b>hello</b>",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            12,
            r#"{"value":"<b>hello</b>","isEncodingProblem":false,"isTruncated":false}"#,
        ),
        (
            b"x<a title='>'>z",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            12,
            r#"{"value":"x","isEncodingProblem":false,"isTruncated":true}"#,
        ),
        (
            b"x<unfinished",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            0,
            r#"{"value":"x<unfinished","isEncodingProblem":false,"isTruncated":false}"#,
        ),
        (
            b"<b>caf=E9</b>",
            TransferEncoding::QuotedPrintable,
            Plan::from_label(Some(b"latin1")),
            5,
            r#"{"value":"<b>ca","isEncodingProblem":false,"isTruncated":true}"#,
        ),
        (
            b"PGI+w6k8L2I+",
            TransferEncoding::Base64,
            Plan::from_label(None),
            5,
            "{\"value\":\"<b>é\",\"isEncodingProblem\":true,\"isTruncated\":true}",
        ),
        (
            b"\xc3\xa9\r\n\0<x>",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            4,
            "{\"value\":\"é\\n\\u0000\",\"isEncodingProblem\":false,\"isTruncated\":true}",
        ),
        (
            b"<b>\xff\xef\xb7\x90",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            2,
            r#"{"value":"","isEncodingProblem":true,"isTruncated":true}"#,
        ),
    ];
    for &(bytes, encoding, charset, cap, expected) in cases {
        for width in [1, 3, 64] {
            let timer = Timer(AtomicU64::new(1));
            let mut source = Bytes {
                bytes,
                reads: 0,
                fail_at: None,
            };
            let mut work = work(100000);
            let mut buffer = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let mut cursor = Cursor::new_html(
                input(&mut source, encoding, charset),
                &mut buffer,
                &mut checkpoints,
                cap,
                &mut work,
                &timer,
            )
            .unwrap();
            assert_eq!(cursor.poll(&mut []).unwrap().status, Status::NeedOutput);
            assert_eq!(drain(&mut cursor, width), expected.as_bytes(), "cap={cap}");
            let diagnostics = cursor.value().unwrap();
            assert_eq!(cursor.finish().unwrap(), diagnostics);
        }
    }
}

#[test]
fn html_exact_budget_funds_both_full_passes_even_after_the_prefix() {
    let expected = b"{\"value\":\"a\",\"isEncodingProblem\":false,\"isTruncated\":true}";
    // Two identity passes and two Plain passes of five bytes, plus wire bytes.
    let required = 20 + expected.len() as u64;
    for allowance in 0..=required {
        let timer = Timer(AtomicU64::new(1));
        let mut source = Bytes {
            bytes: b"a<b>x",
            reads: 0,
            fail_at: None,
        };
        let mut work = work(allowance);
        let before = work.remaining();
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new_html(
            input(
                &mut source,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            2,
            &mut work,
            &timer,
        )
        .unwrap();
        let mut output = Vec::new();
        let result = loop {
            let mut byte = [0; 1];
            match cursor.poll(&mut byte) {
                Err(error) => break Err(error),
                Ok(step) => {
                    output.extend_from_slice(&byte[..step.written]);
                    if step.status == Status::Complete {
                        break Ok(());
                    }
                }
            }
        };
        if allowance < required {
            assert_eq!(result, Err(Error::Work(Stop::OutputBytes)));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(), Err(Error::Work(Stop::OutputBytes)));
        } else {
            result.unwrap();
            assert_eq!(output, expected);
            assert_eq!(
                cursor.finish().unwrap(),
                Diagnostics {
                    is_encoding_problem: false,
                    is_truncated: true,
                }
            );
            assert_eq!(source.reads, 10);
            // Ten bytes each: source reads, transfer visits, charset visits.
            assert_eq!(before.io_bytes - work.remaining().io_bytes, 30);
            // Ten scalar records + ten Plain inputs + five tag steps,
            // plus the checkpoint save and restart.
            assert_eq!(before.records - work.remaining().records, 27);
            assert_eq!(work.remaining().output_bytes, 0);
        }
    }
}

#[test]
fn html_first_and_replay_pass_io_failures_retire_provisional_json() {
    struct Fault {
        pass: usize,
        failing_pass: usize,
        reads: usize,
    }
    impl BlobReader for Fault {
        fn len(&self) -> u64 {
            5
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            self.reads += 1;
            if offset == 0 {
                self.pass += 1;
            }
            if self.pass == self.failing_pass && offset == 4 {
                return Err(PolicyError::Corrupt);
            }
            output[0] = b"a<b>x"[offset as usize];
            Ok(1)
        }
    }
    for failing_pass in [1, 2] {
        let timer = Timer(AtomicU64::new(1));
        let mut source = Fault {
            pass: 0,
            failing_pass,
            reads: 0,
        };
        let mut work = work(10000);
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new_html(
            Input {
                source: &mut source,
                offset: 0,
                length: 5,
                encoding: TransferEncoding::Identity,
                charset: Plan::from_label(Some(b"utf-8")),
            },
            &mut buffer,
            &mut checkpoints,
            2,
            &mut work,
            &timer,
        )
        .unwrap();
        let mut output = Vec::new();
        let error = loop {
            let mut bytes = [0; 1];
            match cursor.poll(&mut bytes) {
                Err(error) => break error,
                Ok(step) => {
                    assert_ne!(step.status, Status::Complete);
                    output.extend_from_slice(&bytes[..step.written]);
                }
            }
        };
        assert_eq!(
            error,
            Error::Text(mime_text::Error::Input(crate::mime_input::Error::Policy(
                PolicyError::Corrupt
            )))
        );
        assert_eq!(
            output,
            if failing_pass == 1 {
                b"{\"value\":\"".as_slice()
            } else {
                b"{\"value\":\"a".as_slice()
            }
        );
        assert!(cursor.value().is_none());
        let mut untouched = [0xa5; 4];
        assert_eq!(cursor.poll(&mut untouched), Err(error));
        assert_eq!(untouched, [0xa5; 4]);
        assert_eq!(cursor.finish(), Err(error));
        assert_eq!(source.reads, 5 * failing_pass);
    }
}

#[test]
fn html_restart_and_complete_handoffs_keep_the_original_deadline() {
    for complete in [false, true] {
        let timer = Timer(AtomicU64::new(1));
        let mut source = Bytes {
            bytes: b"a<b>x",
            reads: 0,
            fail_at: None,
        };
        let mut work = work(10000);
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new_html(
            input(
                &mut source,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            2,
            &mut work,
            &timer,
        )
        .unwrap();
        if complete {
            drain(&mut cursor, 1);
        } else {
            // A finite guard makes a stalled scheduler fail rather than hang.
            for _ in 0..1000 {
                assert_ne!(cursor.poll(&mut [0; 1]).unwrap().status, Status::Complete);
                if matches!(cursor.source.html, Some(Html::Restart(_))) {
                    break;
                }
            }
            assert!(matches!(cursor.source.html, Some(Html::Restart(_))));
        }
        timer.0.store(100, Ordering::Relaxed);
        assert_eq!(cursor.poll(&mut [0; 1]), Err(Error::Work(Stop::Deadline)));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.finish(), Err(Error::Work(Stop::Deadline)));
        assert_eq!(source.reads, if complete { 10 } else { 5 });
    }
}

#[test]
fn html_replay_refuses_changed_normalized_lengths_and_split_scalars() {
    struct Changed<'a> {
        first: &'a [u8],
        second: &'a [u8],
        pass: usize,
    }
    impl BlobReader for Changed<'_> {
        fn len(&self) -> u64 {
            self.first.len() as u64
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            if offset == 0 {
                self.pass += 1;
            }
            let bytes = if self.pass == 1 {
                self.first
            } else {
                self.second
            };
            output[0] = bytes[offset as usize];
            Ok(1)
        }
    }
    let cases: &[(&[u8], &[u8], u64)] = &[
        (b"ab", b"\r\n", 2),              // Short normalized replay.
        (b"\r\n", b"ab", 2),              // Extra text for an untruncated plan.
        (b"ab<x>", "aé<x".as_bytes(), 2), // Prefix splits a replay scalar.
        (b"ab", b"\r\n", 1),              // Truncated plan loses its entire tail.
    ];
    for &(first, second, cap) in cases {
        assert_eq!(first.len(), second.len());
        let mut source = Changed {
            first,
            second,
            pass: 0,
        };
        let timer = Timer(AtomicU64::new(1));
        let mut work = work(10000);
        let mut buffer = [0; 1];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new_html(
            Input {
                source: &mut source,
                offset: 0,
                length: first.len() as u64,
                encoding: TransferEncoding::Identity,
                charset: Plan::from_label(Some(b"utf-8")),
            },
            &mut buffer,
            &mut checkpoints,
            cap,
            &mut work,
            &timer,
        )
        .unwrap();
        let error = loop {
            match cursor.poll(&mut [0; 1]) {
                Err(error) => break error,
                Ok(step) => assert_ne!(step.status, Status::Complete),
            }
        };
        assert_eq!(error, Error::ReplayMismatch);
        assert!(cursor.value().is_none());
        let mut untouched = [0xa5; 4];
        assert_eq!(cursor.poll(&mut untouched), Err(error));
        assert_eq!(untouched, [0xa5; 4]);
        assert_eq!(cursor.finish(), Err(error));
    }
}

#[test]
fn html_io_and_record_budget_cuts_and_mid_replay_expiry_are_terminal() {
    for records in [false, true] {
        let required = if records { 27 } else { 30 };
        for allowance in 0..=required {
            let timer = Timer(AtomicU64::new(1));
            let mut source = Bytes {
                bytes: b"a<b>x",
                reads: 0,
                fail_at: None,
            };
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: if records { 10000 } else { allowance },
                    records: if records { allowance } else { 10000 },
                    output_bytes: 10000,
                    unlinks: 0,
                },
            );
            let mut buffer = [0; 1];
            let mut checkpoints = Checkpoints::default();
            let mut cursor = Cursor::new_html(
                input(
                    &mut source,
                    TransferEncoding::Identity,
                    Plan::from_label(Some(b"utf-8")),
                ),
                &mut buffer,
                &mut checkpoints,
                2,
                &mut work,
                &timer,
            )
            .unwrap();
            let result = loop {
                match cursor.poll(&mut [0; 1]) {
                    Err(error) => break Err(error),
                    Ok(step) if step.status == Status::Complete => break Ok(()),
                    Ok(_) => {}
                }
            };
            if allowance == required {
                result.unwrap();
                cursor.finish().unwrap();
            } else {
                let error = Error::Work(if records {
                    Stop::Records
                } else {
                    Stop::IoBytes
                });
                assert_eq!(result, Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(&mut [0; 1]), Err(error));
                assert_eq!(cursor.finish(), Err(error));
            }
        }
    }
    let timer = Timer(AtomicU64::new(1));
    let mut source = Bytes {
        bytes: b"a<b>x",
        reads: 0,
        fail_at: None,
    };
    let mut work = work(10000);
    let mut buffer = [0; 1];
    let mut checkpoints = Checkpoints::default();
    let mut cursor = Cursor::new_html(
        input(
            &mut source,
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
        ),
        &mut buffer,
        &mut checkpoints,
        2,
        &mut work,
        &timer,
    )
    .unwrap();
    for _ in 0..1000 {
        assert_ne!(cursor.poll(&mut [0; 1]).unwrap().status, Status::Complete);
        if matches!(
            cursor.source.html,
            Some(Html::Replay {
                seen_tail: true,
                ..
            })
        ) {
            break;
        }
    }
    assert!(matches!(
        cursor.source.html,
        Some(Html::Replay {
            seen_tail: true,
            ..
        })
    ));
    timer.0.store(100, Ordering::Relaxed);
    assert_eq!(cursor.poll(&mut [0; 1]), Err(Error::Work(Stop::Deadline)));
    assert!(cursor.value().is_none());
    assert_eq!(cursor.finish(), Err(Error::Work(Stop::Deadline)));
    assert!(source.reads > 5 && source.reads < 10);
}

#[test]
fn unlimited_html_uses_one_fully_validated_pass() {
    let expected = br#"{"value":"a<b>x","isEncodingProblem":false,"isTruncated":false}"#;
    let mut source = Bytes {
        bytes: b"a<b>x",
        reads: 0,
        fail_at: None,
    };
    let timer = Timer(AtomicU64::new(1));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 15,
            records: 10,
            output_bytes: 10 + expected.len() as u64,
            unlinks: 0,
        },
    );
    let mut buffer = [0; 1];
    let mut checkpoints = Checkpoints::default();
    let mut cursor = Cursor::new_html(
        input(
            &mut source,
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
        ),
        &mut buffer,
        &mut checkpoints,
        0,
        &mut work,
        &timer,
    )
    .unwrap();
    assert_eq!(drain(&mut cursor, 1), expected);
    assert!(!cursor.finish().unwrap().is_truncated);
    assert_eq!(source.reads, 5);
    assert_eq!(work.remaining(), Charge::default());
}

#[test]
fn monotonic_only_body_clock_preserves_output_diagnostics_and_charges() {
    struct MonotonicOnly;
    impl Clock for MonotonicOnly {
        fn sample(&self) -> Result<Time, PolicyError> {
            Err(PolicyError::Busy)
        }
        fn monotonic(&self) -> Result<Tick, PolicyError> {
            Ok(Tick(1))
        }
    }
    fn render(clock: &dyn Clock, html: bool, cap: u64) -> (Vec<u8>, Diagnostics, Charge) {
        let mut source = Bytes {
            bytes: b"<b>caf\xc3\xa9</b>\r\n\xfftail",
            reads: 0,
            fail_at: None,
        };
        let mut work = work(100000);
        let mut buffer = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let selected = input(
            &mut source,
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
        );
        let mut cursor = if html {
            Cursor::new_html(
                selected,
                &mut buffer,
                &mut checkpoints,
                cap,
                &mut work,
                clock,
            )
            .unwrap()
        } else {
            Cursor::new(
                selected,
                &mut buffer,
                &mut checkpoints,
                cap,
                &mut work,
                clock,
            )
            .unwrap()
        };
        let output = drain(&mut cursor, 3);
        let diagnostics = cursor.finish().unwrap();
        (output, diagnostics, work.remaining())
    }
    let baseline = Timer(AtomicU64::new(1));
    for html in [false, true] {
        for cap in [0, 5] {
            assert_eq!(
                render(&baseline, html, cap),
                render(&MonotonicOnly, html, cap)
            );
        }
    }
}
