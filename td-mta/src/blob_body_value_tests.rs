#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_part_json::tests::{exercise_part, Timer};
use crate::ports::Time;
struct GoodClock;
impl Clock for GoodClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(1),
        })
    }
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    for _ in 0..10000 {
        let mut output = [0xa5; 128];
        let progress = cursor.poll(&mut output[..width])?;
        assert!(progress.written <= json::MAX_CHUNK_BYTES);
        assert!(output[progress.written..].iter().all(|&b| b == 0xa5));
        bytes.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("native body value stalled");
}
const CASES: &[(&[u8], u64, &str, bool, bool)] = &[
    (
        b"Content-Type: text/plain;charset=UTF-8\r\n\r\nh\xc3\xa9\r\n",
        0,
        "hé\\n",
        false,
        false,
    ),
    (
        b"Content-Type: text/plain;charset=latin1\r\n\
            Content-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9",
        0,
        "café",
        false,
        false,
    ),
    (
        b"Content-Type: text/plain;charset=UTF-8\r\n\
            Content-Transfer-Encoding: base64\r\n\r\naMOp",
        2,
        "h",
        false,
        true,
    ),
    (
        b"Content-Type: text/plain;charset=UTF-8\r\n\r\na\xff",
        1,
        "a",
        true,
        true,
    ),
    (
        b"Content-Type: text/html;charset=UTF-8\r\n\r\n<b>abc</b>",
        2,
        "",
        false,
        true,
    ),
    (
        b"Content-Type: text/html;charset=UTF-8\r\n\r\n<b>abc</b>",
        4,
        "<b>a",
        false,
        true,
    ),
    (
        b"Content-Type: text/calendar\r\n\r\nBEGIN\r\n",
        0,
        "BEGIN\\n",
        false,
        false,
    ),
];
#[test]
fn native_plain_html_transfer_and_charset_values_preserve_original_owners() {
    for &(source, cap, value, problem, truncated) in CASES {
        for width in [1, 7, 64, 128] {
            let timer = Timer::new();
            exercise_part(1, source, &timer, |original| {
                let pointers = (
                    std::ptr::from_mut(original.structure.source),
                    std::ptr::from_mut(original.structure.work),
                    std::ptr::from_mut(original.structure.budget),
                    std::ptr::from_mut(original.scratch),
                    original.structure.parts.as_ptr(),
                    original.structure.headers.as_ptr(),
                    original.structure.remaining.as_ptr(),
                );
                let mut buffer = [0; 3];
                let mut checkpoints = Checkpoints::default();
                let backing = (buffer.as_mut_ptr(), std::ptr::from_mut(&mut checkpoints));
                let mut cursor = Cursor::new(
                    original,
                    &mut buffer,
                    &mut checkpoints,
                    cap,
                    &GoodClock,
                    Tick(1),
                )
                .unwrap();
                assert_eq!(cursor.poll(&mut []).unwrap().status, Status::NeedOutput);
                let expected = format!("{{\"value\":\"{value}\",\"isEncodingProblem\":{problem},\"isTruncated\":{truncated}}}");
                assert_eq!(drain(&mut cursor, width).unwrap(), expected.as_bytes());
                assert_eq!(cursor.poll(&mut []).unwrap().status, Status::Complete);
                let retained = cursor.finish().unwrap();
                assert_eq!(
                    retained.diagnostics,
                    Diagnostics {
                        is_encoding_problem: problem,
                        is_truncated: truncated
                    }
                );
                let original = retained.original;
                assert_eq!(
                    pointers,
                    (
                        std::ptr::from_mut(original.structure.source),
                        std::ptr::from_mut(original.structure.work),
                        std::ptr::from_mut(original.structure.budget),
                        std::ptr::from_mut(original.scratch),
                        original.structure.parts.as_ptr(),
                        original.structure.headers.as_ptr(),
                        original.structure.remaining.as_ptr()
                    )
                );
                assert_eq!(
                    backing,
                    (
                        retained.buffer.as_mut_ptr(),
                        std::ptr::from_mut(retained.checkpoints)
                    )
                );
            });
        }
    }
}
#[test]
fn nontext_inconsistent_descriptor_short_buffer_and_unfinished_finish_refuse() {
    for mode in 0..9 {
        let timer = Timer::new();
        exercise_part(
            1,
            if mode == 0 {
                b"Content-Type: image/png\r\n\r\nx"
            } else {
                b"\r\nx"
            },
            &timer,
            |mut original| {
                let mut parts = original.structure.parts.to_vec();
                match mode {
                    1 => original.metadata.part.body_start += 1,
                    4 => original.metadata.metadata.headers.body_start += 1,
                    5 => original.metadata.part.ordinal = 0,
                    6 | 7 => {
                        original.metadata.part.media = if mode == 6 {
                            Media::Multipart
                        } else {
                            Media::MessageRfc822
                        };
                        parts[0] = original.metadata.part;
                    }
                    8 => {
                        original.metadata.part.entity_end = original.structure.source.len() + 1;
                        parts[0] = original.metadata.part;
                    }
                    _ => {}
                }
                let original = blob_part::Retained {
                    structure: blob_structure::Retained {
                        parts: &parts,
                        ..original.structure
                    },
                    ..original
                };
                let mut buffer = [0; 3];
                let mut checkpoints = Checkpoints::default();
                let result = Cursor::new(
                    original,
                    &mut buffer[..if mode == 2 { 0 } else { 3 }],
                    &mut checkpoints,
                    0,
                    &GoodClock,
                    Tick(1),
                );
                let expected = if mode == 2 {
                    Error::Json(json::Error::Text(crate::mime_text::Error::Input(
                        crate::mime_input::Error::InvalidBacking,
                    )))
                } else {
                    Error::Metadata(bound::Error::InvalidState)
                };
                if mode == 3 {
                    assert_eq!(result.unwrap().finish().err(), Some(expected));
                } else {
                    assert_eq!(result.err(), Some(expected));
                }
            },
        );
    }
}
#[test]
fn native_expiry_hides_active_cached_and_provisional_output() {
    for mode in 0..4 {
        let timer = Timer::new();
        exercise_part(1, b"\r\nabc", &timer, |original| {
            let mut buffer = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let mut cursor = Cursor::new(
                original,
                &mut buffer,
                &mut checkpoints,
                0,
                &GoodClock,
                Tick(1),
            )
            .unwrap();
            if mode >= 1 {
                drain(&mut cursor, 64).unwrap();
            }
            timer.after(if mode == 3 { 3 } else { 5 });
            let error = Error::Policy(PolicyError::Deadline);
            if mode == 2 {
                assert_eq!(cursor.finish().err(), Some(error));
            } else {
                if mode == 3 {
                    assert_eq!(cursor.check(), Err(error));
                } else {
                    let mut output = [0xa5; 64];
                    assert_eq!(cursor.poll(&mut output), Err(error));
                    if mode == 0 {
                        assert_ne!(output, [0xa5; 64]);
                    }
                }
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(&mut []), Err(error));
                assert_eq!(cursor.finish().err(), Some(error));
            }
        });
    }
}

#[test]
fn native_constructor_and_job_quota_refusals_are_terminal() {
    use crate::admission::work::{Charge, Stop};
    for mode in 0..7 {
        let timer = Timer::new();
        let job = Timer::new();
        exercise_part(1, b"\r\nabc", &timer, |original| {
            let mut buffer = [0; 3];
            let mut checkpoints = Checkpoints::default();
            if mode < 2 {
                timer.after(3);
                assert_eq!(
                    Cursor::new(
                        original,
                        &mut buffer[..if mode == 0 { 0 } else { 3 }],
                        &mut checkpoints,
                        0,
                        &job,
                        Tick(if mode == 1 { 100 } else { 1 })
                    )
                    .err(),
                    Some(Error::Policy(PolicyError::Deadline))
                );
                return;
            }
            if mode == 6 {
                assert_eq!(
                    Cursor::new(original, &mut buffer, &mut checkpoints, 0, &job, Tick(100)).err(),
                    Some(Error::Metadata(bound::Error::Admission(
                        crate::nfc::Error::Work(Stop::Deadline)
                    )))
                );
                return;
            }
            if mode == 2 {
                let remaining = original.structure.work.remaining().output_bytes;
                original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: remaining,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut cursor =
                Cursor::new(original, &mut buffer, &mut checkpoints, 0, &job, Tick(1)).unwrap();
            if mode >= 3 {
                drain(&mut cursor, 64).unwrap();
                job.after(0);
            }
            if mode == 5 {
                timer.after(3);
            }
            let expected = if mode == 5 {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Json(json::Error::Work(if mode == 2 {
                    Stop::OutputBytes
                } else {
                    Stop::Deadline
                }))
            };
            if mode == 4 {
                assert_eq!(cursor.finish().err(), Some(expected));
            } else {
                let mut output = [0xa5; 64];
                assert_eq!(cursor.poll(&mut output), Err(expected));
                assert_eq!(output, [0xa5; 64]);
                timer.after(0);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(&mut []), Err(expected));
                assert_eq!(cursor.finish().err(), Some(expected));
            }
        });
    }
}

#[test]
fn concrete_and_generic_sources_match_bytes_and_original_work_costs() {
    struct Source<'a>(&'a [u8]);
    impl BlobReader for Source<'_> {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            let bytes = self.0.get(offset as usize..).ok_or(PolicyError::Invalid)?;
            let n = output.len().min(bytes.len());
            output[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        }
    }
    for bytes in [
        b"Content-Type: text/plain;charset=UTF-8\r\n\r\ncaf\xc3\xa9".as_slice(),
        b"Content-Type: text/html;charset=UTF-8\r\n\r\n<b>abc</b>",
    ] {
        for cap in [0, 2, 4] {
            for width in [1, 64] {
                let timer = Timer::new();
                exercise_part(1, bytes, &timer, |original| {
                    let before = original.structure.work.remaining();
                    let part = original.metadata.part;
                    let charset = Plan::from_label(original.metadata.metadata.headers.charset);
                    let html = original.metadata.metadata.headers.content_type == b"text/html";
                    let mut buffer = [0; 3];
                    let mut checkpoints = Checkpoints::default();
                    let mut native = Cursor::new(
                        original,
                        &mut buffer,
                        &mut checkpoints,
                        cap,
                        &GoodClock,
                        Tick(1),
                    )
                    .unwrap();
                    let native_bytes = drain(&mut native, width).unwrap();
                    let completed = native.finish().unwrap();
                    let after = completed.original.structure.work.remaining();
                    let native_costs = [
                        before.io_bytes - after.io_bytes,
                        before.records - after.records,
                        before.output_bytes - after.output_bytes,
                    ];
                    let mut work = crate::admission::work::Meter::new(
                        crate::ports::Deadline::after(Tick(0), 100).unwrap(),
                        before,
                    );
                    let mut source = Source(bytes);
                    let input = crate::mime_text::Input {
                        source: &mut source,
                        offset: part.body_start,
                        length: part.entity_end - part.body_start,
                        encoding: TransferEncoding::Identity,
                        charset,
                    };
                    let mut generic = if html {
                        json::Cursor::new_html(
                            input,
                            completed.buffer,
                            completed.checkpoints,
                            cap,
                            &mut work,
                            &GoodClock,
                        )
                    } else {
                        json::Cursor::new(
                            input,
                            completed.buffer,
                            completed.checkpoints,
                            cap,
                            &mut work,
                            &GoodClock,
                        )
                    }
                    .unwrap();
                    let mut generic_bytes = Vec::new();
                    loop {
                        let mut output = [0; 64];
                        let p = generic.poll(&mut output[..width]).unwrap();
                        generic_bytes.extend_from_slice(&output[..p.written]);
                        if p.status == Status::Complete {
                            break;
                        }
                    }
                    assert_eq!(generic.finish().unwrap(), completed.diagnostics);
                    let after = work.remaining();
                    assert_eq!(native_bytes, generic_bytes);
                    assert_eq!(
                        native_costs,
                        [
                            before.io_bytes - after.io_bytes,
                            before.records - after.records,
                            before.output_bytes - after.output_bytes
                        ]
                    );
                });
            }
        }
    }
}
