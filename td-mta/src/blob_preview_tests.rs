#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_part_json::tests::{exercise_part, Timer};
use crate::ports::Time;
use crate::{mime_structure::Media, ports::BlobReader};
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
        assert!(progress.written <= 6);
        assert!(output[progress.written..].iter().all(|&b| b == 0xa5));
        bytes.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            return Ok(bytes);
        }
    }
    panic!("native preview stalled");
}
const CASES: &[(&[u8], &str, bool, bool)] = &[
    (b"Content-Type: text/html;charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\nPGI+YWJjPC9iPg==", "abc", false, false),
    (b"Content-Type: text/html;charset=latin1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<b>caf=E9</b>", "café", false, false),
    (b"Content-Type: text/plain;charset=UTF-8\r\n\r\nh\xc3\xa9\r\n", "hé", false, false),
    (b"Content-Type: text/plain;charset=latin1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9", "café", false, false),
    (b"Content-Type: text/plain;charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\naMOp", "hé", false, false),
    (b"Content-Type: text/plain;charset=UTF-8\r\n\r\na\xff", "a�", true, false),
    (b"Content-Type: text/html;charset=UTF-8\r\n\r\n<head><title>hide</title></head><b>abc</b><img alt=' icon'>", "abc icon", false, false),
    (b"Content-Type: text/html;charset=UTF-8\r\n\r\nabc<img alt='def", "abc def", false, true),
    (b"Content-Type: text/calendar\r\n\r\nBEGIN\r\n", "BEGIN", false, false),
];
#[test]
fn native_plain_html_transfer_and_charset_values_preserve_original_owners() {
    for &(source, value, problem, malformed) in CASES {
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
                let mut cursor =
                    Cursor::new(original, &mut buffer, &mut checkpoints, &GoodClock, Tick(1))
                        .unwrap();
                assert_eq!(cursor.poll(&mut []).unwrap().status, Status::NeedOutput);
                let expected = format!("\"{value}\"");
                assert_eq!(drain(&mut cursor, width).unwrap(), expected.as_bytes());
                assert_eq!(cursor.poll(&mut []).unwrap().status, Status::Complete);
                let retained = cursor.finish().unwrap();
                assert_eq!(retained.diagnostics.is_encoding_problem, problem);
                assert_eq!(retained.diagnostics.malformed_markup, malformed);
                assert_eq!(
                    retained.diagnostics.summary.scalars as usize,
                    value.chars().count()
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
            let mut cursor =
                Cursor::new(original, &mut buffer, &mut checkpoints, &GoodClock, Tick(1)).unwrap();
            if mode >= 1 {
                drain(&mut cursor, 64).unwrap();
            } else {
                // The first frame turn stages its opening quote; the next
                // copies it before the native postcheck can expire.
                assert_eq!(cursor.poll(&mut [0; 64]).unwrap().written, 0);
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
                    Cursor::new(original, &mut buffer, &mut checkpoints, &job, Tick(100)).err(),
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
                Cursor::new(original, &mut buffer, &mut checkpoints, &job, Tick(1)).unwrap();
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
fn every_finish_and_check_fence_sample_can_refuse() {
    for finish in [false, true] {
        let samples = if finish { 8 } else { 4 };
        for expiry in 1..=samples + 1 {
            let timer = Timer::new();
            exercise_part(1, b"\r\nabc", &timer, |original| {
                let mut buffer = [0; 3];
                let mut checkpoints = Checkpoints::default();
                let mut cursor =
                    Cursor::new(original, &mut buffer, &mut checkpoints, &GoodClock, Tick(1))
                        .unwrap();
                drain(&mut cursor, 64).unwrap();
                timer.after(expiry);
                let result = if finish {
                    cursor.finish().map(|_| ())
                } else {
                    cursor.check()
                };
                assert_eq!(
                    result,
                    if expiry <= samples {
                        Err(Error::Policy(PolicyError::Deadline))
                    } else {
                        Ok(())
                    },
                    "finish={finish} expiry={expiry}"
                );
            });
        }
    }
}
#[test]
fn native_source_tail_remains_live_after_preview_cap() {
    let mut source = b"Content-Type: text/plain;charset=UTF-8\r\n\r\n".to_vec();
    source.extend_from_slice(&[b'a'; 300]);
    source.extend_from_slice(b"\xff");
    for expire in [false, true] {
        let timer = Timer::new();
        exercise_part(1, &source, &timer, |original| {
            let mut buffer = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let mut cursor =
                Cursor::new(original, &mut buffer, &mut checkpoints, &GoodClock, Tick(1)).unwrap();
            let mut wire = Vec::new();
            loop {
                let mut output = [0; 64];
                let progress = cursor.poll(&mut output).unwrap();
                assert!(progress.written <= 6);
                wire.extend_from_slice(&output[..progress.written]);
                if expire && wire.len() == 257 {
                    assert!(cursor.value().is_none());
                    timer.after(0);
                    assert_eq!(
                        cursor.poll(&mut []),
                        Err(Error::Policy(PolicyError::Deadline))
                    );
                    assert_eq!(
                        cursor.finish().err(),
                        Some(Error::Policy(PolicyError::Deadline))
                    );
                    return;
                }
                if progress.status == Status::Complete {
                    break;
                }
            }
            assert!(!expire);
            assert_eq!(wire, format!("\"{}\"", "a".repeat(256)).as_bytes());
            let retained = cursor.finish().unwrap();
            assert_eq!(retained.diagnostics.summary.scalars, 256);
            assert!(retained.diagnostics.is_encoding_problem);
        });
    }
}
