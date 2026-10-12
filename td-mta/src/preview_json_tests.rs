#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    body_charset::Plan,
    ports::{Deadline, Time},
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
fn input<'a, 's>(
    source: &'a mut Bytes<'s>,
    encoding: TransferEncoding,
    charset: Plan,
) -> InputSource<'a, Bytes<'s>> {
    let length = source.len();
    InputSource {
        source,
        offset: 0,
        length,
        encoding,
        charset,
    }
}
fn drain<S: BlobReader + ?Sized>(
    cursor: &mut Cursor<'_, '_, '_, '_, S>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut result = Vec::new();
    for _ in 0..100000 {
        let mut output = [0xa5; 64];
        let p = cursor.poll(&mut output[..width])?;
        assert!(p.written <= 6);
        assert!(output[p.written..].iter().all(|b| *b == 0xa5));
        result.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return Ok(result);
        }
        assert!(cursor.value().is_none());
    }
    panic!("preview did not complete")
}
#[test]
fn literal_plain_html_transfer_and_charset_cases_keep_original_owners() {
    type Case<'a> = (
        &'a [u8],
        TransferEncoding,
        Plan,
        Format,
        &'a str,
        bool,
        bool,
    );
    let cases: &[Case<'_>] = &[
        (
            b"",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            Format::Plain,
            "\"\"",
            false,
            false,
        ),
        (
            b" a\r\n\"\\\0 b ",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            Format::Plain,
            "\"a \\\"\\\\� b\"",
            false,
            false,
        ),
        (
            b"Y2Fmw6kNCg==",
            TransferEncoding::Base64,
            Plan::from_label(Some(b"utf-8")),
            Format::Plain,
            "\"café\"",
            false,
            false,
        ),
        (
            b"caf=E9",
            TransferEncoding::QuotedPrintable,
            Plan::from_label(Some(b"latin1")),
            Format::Plain,
            "\"café\"",
            false,
            false,
        ),
        (
            b"caf\xc3\xa9",
            TransferEncoding::Identity,
            Plan::from_label(None),
            Format::Plain,
            "\"café\"",
            true,
            false,
        ),
        (
            b"a\xff",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            Format::Plain,
            "\"a�\"",
            true,
            false,
        ),
        (
            b"<head><title>hidden</title><body><p>Hello &amp; <b>mail</b></p><img alt='icon'>",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            Format::Html,
            "\"Hello & mail icon\"",
            false,
            false,
        ),
        (
            b"<img title='unfinished",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            Format::Html,
            "\"unfinished\"",
            false,
            true,
        ),
        (
            b"Price < 5. Don't miss <b>this</b>!",
            TransferEncoding::Identity,
            Plan::from_label(Some(b"utf-8")),
            Format::Html,
            "\"Price < 5. Don't miss this!\"",
            false,
            true,
        ),
    ];
    for &(bytes, encoding, charset, format, expected, encoding_problem, malformed) in cases {
        for width in [1, 3, 64] {
            let timer = Timer(AtomicU64::new(1));
            let mut source = Bytes {
                bytes,
                reads: 0,
                fail_at: None,
            };
            let source_ptr = std::ptr::from_mut(&mut source);
            let mut work = work(100000);
            let work_ptr = std::ptr::from_mut(&mut work);
            let mut buffer = [0; 3];
            let buffer_ptr = buffer.as_mut_ptr();
            let mut checkpoints = Checkpoints::default();
            let checkpoints_ptr = std::ptr::from_mut(&mut checkpoints);
            let mut cursor = Cursor::new(
                input(&mut source, encoding, charset),
                &mut buffer,
                &mut checkpoints,
                format,
                &mut work,
                &timer,
                Tick(1),
            )
            .unwrap();
            let before = cursor.source.work.remaining();
            assert_eq!(cursor.poll(&mut []).unwrap().status, Status::NeedOutput);
            assert_eq!(cursor.source.work.remaining(), before);
            assert_eq!(drain(&mut cursor, width).unwrap(), expected.as_bytes());
            for output in [&mut [][..], &mut [0; 8][..]] {
                assert_eq!(
                    cursor.poll(output).unwrap(),
                    Progress {
                        written: 0,
                        status: Status::Complete,
                    }
                );
            }
            let diagnostics = cursor.value().unwrap();
            assert_eq!(diagnostics.is_encoding_problem, encoding_problem);
            assert_eq!(diagnostics.malformed_markup, malformed);
            let retained = cursor.finish().unwrap();
            assert_eq!(std::ptr::from_mut(retained.source), source_ptr);
            assert_eq!(std::ptr::from_mut(retained.work), work_ptr);
            assert_eq!(std::ptr::from_mut(retained.checkpoints), checkpoints_ptr);
            assert_eq!(retained.buffer.as_mut_ptr(), buffer_ptr);
            assert_eq!(retained.diagnostics, diagnostics);
        }
    }
}
#[test]
fn display_cap_does_not_skip_source_tail_or_html_suffix() {
    let mut invalid = vec![b'a'; 300];
    invalid.push(0xff);
    let html = format!("{}<img title='unfinished", "a".repeat(300));
    for (source, format, encoding_problem, malformed) in [
        (invalid.as_slice(), Format::Plain, true, false),
        (html.as_bytes(), Format::Html, false, true),
    ] {
        let timer = Timer(AtomicU64::new(1));
        let mut bytes = Bytes {
            bytes: source,
            reads: 0,
            fail_at: None,
        };
        let mut work = work(100000);
        let mut buffer = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new(
            input(
                &mut bytes,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            format,
            &mut work,
            &timer,
            Tick(1),
        )
        .unwrap();
        assert_eq!(
            drain(&mut cursor, 1).unwrap(),
            format!("\"{}\"", "a".repeat(256)).as_bytes()
        );
        let retained = cursor.finish().unwrap();
        assert_eq!(retained.diagnostics.summary.scalars, 256);
        assert_eq!(retained.diagnostics.is_encoding_problem, encoding_problem);
        assert_eq!(retained.diagnostics.malformed_markup, malformed);
        assert!(retained.source.reads >= 150);
    }
}
#[test]
fn each_transform_and_json_wire_are_charged_without_replacing_the_budget() {
    for (source, format, cost) in [
        (b"a".as_slice(), Format::Plain, 5),
        (b"<b>a</b>".as_slice(), Format::Html, 13),
    ] {
        let timer = Timer(AtomicU64::new(1));
        let mut bytes = Bytes {
            bytes: source,
            reads: 0,
            fail_at: None,
        };
        let mut work = work(1000);
        let mut buffer = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new(
            input(
                &mut bytes,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            format,
            &mut work,
            &timer,
            Tick(1),
        )
        .unwrap();
        assert_eq!(drain(&mut cursor, 64).unwrap(), b"\"a\"");
        let retained = cursor.finish().unwrap();
        assert_eq!(1000 - retained.work.remaining().output_bytes, cost);
    }
}
#[test]
fn late_source_failure_empty_cached_and_finish_deadlines_refuse() {
    for mode in 0..5 {
        let data = vec![b'a'; 400];
        let timer = Timer(AtomicU64::new(1));
        let mut bytes = Bytes {
            bytes: &data,
            reads: 0,
            fail_at: (mode == 0).then_some(300),
        };
        let mut work = work(if mode == 1 { 1 } else { 100000 });
        let mut buffer = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let mut cursor = Cursor::new(
            input(
                &mut bytes,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            Format::Plain,
            &mut work,
            &timer,
            Tick(1),
        )
        .unwrap();
        if mode <= 1 {
            let error = drain(&mut cursor, 3).unwrap_err();
            if mode == 1 {
                assert_eq!(error, Error::Work(Stop::OutputBytes));
            } else {
                assert_eq!(
                    error,
                    Error::Text(mime_text::Error::Input(crate::mime_input::Error::Policy(
                        PolicyError::Corrupt
                    )))
                );
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(&mut []), Err(error));
            assert_eq!(cursor.finish().err(), Some(error));
        } else {
            if mode != 2 {
                drain(&mut cursor, 3).unwrap();
            }
            timer.0.store(100, Ordering::Relaxed);
            if mode == 4 {
                assert_eq!(cursor.finish().err(), Some(Error::Work(Stop::Deadline)));
            } else {
                assert_eq!(cursor.poll(&mut []), Err(Error::Work(Stop::Deadline)));
                assert!(cursor.value().is_none());
            }
        }
    }
}

#[test]
fn html_failures_and_constructor_clock_refusals_stay_terminal() {
    for mode in 0..4 {
        let timer = Timer(AtomicU64::new(if mode == 0 { 100 } else { 1 }));
        let mut bytes = Bytes {
            bytes: b"<b>long text</b>&amp suffix",
            reads: 0,
            fail_at: (mode == 2).then_some(12),
        };
        let mut work = work(if mode == 3 { 10 } else { 100000 });
        let mut buffer = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let result = Cursor::new(
            input(
                &mut bytes,
                TransferEncoding::Identity,
                Plan::from_label(Some(b"utf-8")),
            ),
            &mut buffer,
            &mut checkpoints,
            Format::Html,
            &mut work,
            &timer,
            Tick(if mode == 1 { 2 } else { 1 }),
        );
        if mode < 2 {
            assert_eq!(
                result.err(),
                Some(if mode == 0 {
                    Error::Work(Stop::Deadline)
                } else {
                    Error::Policy(PolicyError::Invalid)
                })
            );
            continue;
        }
        let mut cursor = result.unwrap();
        let error = drain(&mut cursor, 1).unwrap_err();
        if mode == 3 {
            assert_eq!(error, Error::Work(Stop::OutputBytes));
        } else {
            assert_eq!(
                error,
                Error::Text(mime_text::Error::Input(crate::mime_input::Error::Policy(
                    PolicyError::Corrupt
                )))
            );
        }
        assert_eq!(cursor.poll(&mut []), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.finish().err(), Some(error));
    }
}
