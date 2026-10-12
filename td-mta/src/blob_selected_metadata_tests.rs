#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::admission::work::{Charge, Stop};
use crate::blob_part_json::tests::{exercise_part, Timer};
pub(crate) fn exercise(
    bytes: &[u8],
    ordinal: u16,
    run: impl FnOnce(blob_part_json::Retained<'_, '_, '_, '_, '_, '_>, &[u8], &Candidate, &Timer),
) {
    let timer = Timer::new();
    exercise_part(ordinal, bytes, &timer, |original| {
        let mut encoding = blob_part_json::Cursor::new(original, Tick(1)).unwrap();
        let mut fragment = Vec::new();
        loop {
            let mut output = [0; 64];
            let progress = encoding.poll(Tick(1), &mut output).unwrap();
            fragment.extend_from_slice(&output[..progress.written]);
            if progress.status == Status::Complete {
                break;
            }
        }
        let completed = encoding.finish(Tick(1)).unwrap();
        let mut candidates = vec![Candidate::default(); completed.structure.parts.len()];
        let mut mapping =
            crate::blob_part_locators::Cursor::new(completed.structure, &mut candidates, Tick(1))
                .unwrap();
        while mapping.poll(Tick(1)).unwrap() != crate::blob_part_locators::Status::Complete {}
        let mapped = mapping.finish(Tick(1)).unwrap();
        let original = blob_part_json::Retained {
            structure: mapped.structure,
            end: completed.end,
            scratch: completed.scratch,
        };
        run(
            original,
            &fragment,
            &mapped.candidates[usize::from(ordinal) - 1],
            &timer,
        );
    });
}
pub(crate) fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut result = Vec::new();
    for _ in 0..10000 {
        let mut output = [0xa5; 128];
        let before = cursor.original.structure.budget.steps_remaining();
        let progress = cursor.poll(Tick(1), &mut output[..width])?;
        assert!(before - cursor.original.structure.budget.steps_remaining() <= 64);
        assert!(progress.written <= 64);
        assert!(output[progress.written..].iter().all(|&b| b == 0xa5));
        result.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            return Ok(result);
        }
    }
    panic!("selected metadata did not finish");
}
pub(crate) fn properties(mask: u16) -> Properties {
    Properties {
        part_id: mask & 1 != 0,
        size: mask & 2 != 0,
        media_type: mask & 4 != 0,
        charset: mask & 8 != 0,
        name: mask & 16 != 0,
        disposition: mask & 32 != 0,
        cid: mask & 64 != 0,
        language: mask & 128 != 0,
        location: mask & 256 != 0,
        blob_id: mask & 512 != 0,
    }
}
pub(crate) fn counters(original: &blob_part_json::Retained<'_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let work = original.structure.work.remaining();
    [
        original.structure.budget.source_bytes_remaining(),
        original.structure.budget.steps_remaining(),
        work.io_bytes,
        work.records,
        work.output_bytes,
    ]
}
#[test]
fn every_selection_mask_matches_literal_members_and_preserves_owners() {
    exercise(
        crate::blob_part_json::tests::BYTES,
        1,
        |mut original, fragment, candidate, _| {
            let initial = (
                std::ptr::from_mut(original.structure.source),
                std::ptr::from_mut(original.structure.work),
                std::ptr::from_mut(original.structure.budget),
                std::ptr::from_mut(original.scratch),
                original.structure.parts.as_ptr(),
                original.structure.headers.as_ptr(),
                original.structure.remaining.as_ptr(),
            );
            let blob = format!("\"blobId\":\"{}\"", candidate.wire().unwrap());
            let members = [
                r#""partId":"1""#,
                r#""size":11"#,
                r#""type":"text/html""#,
                r#""charset":"UTF-8""#,
                r#""name":"café.html""#,
                r#""disposition":"attachment""#,
                r#""cid":"x@example.org""#,
                r#""language":["en","fr"]"#,
                r#""location":"../p""#,
                &blob,
            ];
            for mask in 0..1024 {
                let selected = properties(mask);
                let before = counters(&original);
                let mut cursor =
                    Cursor::new(original, fragment, candidate, selected, Tick(1)).unwrap();
                assert_eq!(
                    cursor.poll(Tick(1), &mut []).unwrap().status,
                    if mask == 0 {
                        Status::Complete
                    } else {
                        Status::NeedOutput
                    }
                );
                let got = drain(&mut cursor, [1, 7, 64, 128][usize::from(mask) % 4]).unwrap();
                let expected = members
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| (mask & (1 << i) != 0).then_some(*m))
                    .collect::<Vec<_>>()
                    .join(",");
                assert_eq!(got, expected.as_bytes(), "mask {mask}");
                assert_eq!(cursor.value(), Some((cursor.original.end, selected)));
                let result = cursor.finish(Tick(1)).unwrap();
                assert_eq!(result.fragment.as_ptr(), fragment.as_ptr());
                assert!(std::ptr::eq(result.candidate, candidate));
                assert_eq!(result.properties, selected);
                original = result.original;
                let after = counters(&original);
                assert_eq!(before[0], after[0]);
                assert_eq!(before[2], after[2]);
                assert_eq!(before[4] - after[4], got.len() as u64);
                if mask == 0 {
                    assert_eq!(before, after);
                }
            }
            assert_eq!(
                (
                    std::ptr::from_mut(original.structure.source),
                    std::ptr::from_mut(original.structure.work),
                    std::ptr::from_mut(original.structure.budget),
                    std::ptr::from_mut(original.scratch),
                    original.structure.parts.as_ptr(),
                    original.structure.headers.as_ptr(),
                    original.structure.remaining.as_ptr()
                ),
                initial
            );
        },
    );
}
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
#[test]
fn multipart_null_and_nested_leaf_and_late_native_refusals() {
    for ordinal in [1, 2] {
        exercise(MULTIPART, ordinal, |original, fragment, candidate, _| {
            let mut cursor =
                Cursor::new(original, fragment, candidate, properties(512), Tick(1)).unwrap();
            let got = drain(&mut cursor, 1).unwrap();
            let expected = if ordinal == 1 {
                "\"blobId\":null".to_owned()
            } else {
                format!("\"blobId\":\"{}\"", candidate.wire().unwrap())
            };
            assert_eq!(got, expected.as_bytes());
            cursor.finish(Tick(1)).unwrap();
        });
    }
    for mode in 0..5 {
        exercise(b"\ntext", 1, |original, fragment, candidate, timer| {
            if mode == 0 {
                timer.after(3);
                assert!(matches!(
                    Cursor::new(original, fragment, candidate, Properties::ALL, Tick(1)),
                    Err(Error::Parent(crate::ports::Error::Deadline))
                ));
                return;
            }
            let mut cursor =
                Cursor::new(original, fragment, candidate, Properties::ALL, Tick(1)).unwrap();
            if mode >= 2 {
                drain(&mut cursor, 64).unwrap();
            }
            timer.after(if mode == 4 { 3 } else { 5 });
            let error = Error::Parent(crate::ports::Error::Deadline);
            if mode == 3 {
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else {
                if mode == 4 {
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                } else {
                    let mut output = [0xa5; 64];
                    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                    if mode == 1 {
                        assert_ne!(output, [0xa5; 64]);
                    }
                }
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]), Err(error));
            }
        });
    }
}
#[test]
fn mismatched_evidence_and_unfinished_output_refuse() {
    for mode in 0..4 {
        exercise(MULTIPART, 2, |mut original, fragment, candidate, _| {
            let mut parts = original.structure.parts.to_vec();
            if mode == 0 {
                original.end.part.ordinal = 1;
            }
            if mode == 1 {
                parts[1].body_start += 1;
            }
            let original = blob_part_json::Retained {
                structure: crate::blob_structure::Retained {
                    parts: &parts,
                    ..original.structure
                },
                ..original
            };
            let empty = Candidate::default();
            let result = Cursor::new(
                original,
                fragment,
                if mode == 2 { &empty } else { candidate },
                Properties::ALL,
                Tick(1),
            );
            if mode < 3 {
                assert!(matches!(result, Err(Error::InvalidState)));
            } else {
                assert!(matches!(
                    result.unwrap().finish(Tick(1)),
                    Err(Error::InvalidState)
                ));
            }
        });
    }
}
#[test]
fn original_quota_and_completed_job_refusals_are_terminal() {
    for mode in 0..5 {
        exercise(b"\ntext", 1, |original, fragment, candidate, _| {
            if mode < 2 {
                let left = original.structure.work.remaining();
                original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if mode == 0 { left.records } else { 0 },
                            output_bytes: if mode == 1 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut cursor =
                Cursor::new(original, fragment, candidate, Properties::ALL, Tick(1)).unwrap();
            let stop = if mode == 0 {
                Stop::Records
            } else if mode == 1 {
                Stop::OutputBytes
            } else {
                Stop::Deadline
            };
            let error = Error::Original(bound::Error::Admission(crate::nfc::Error::Work(stop)));
            if mode >= 2 {
                drain(&mut cursor, 64).unwrap();
            }
            if mode == 4 {
                assert_eq!(cursor.finish(Tick(100)).err(), Some(error));
            } else {
                let result = if mode == 3 {
                    cursor.check_deadline(Tick(100))
                } else {
                    cursor
                        .poll(Tick(if mode == 2 { 100 } else { 1 }), &mut [0; 64])
                        .map(|_| ())
                };
                assert_eq!(result, Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            }
        });
    }
}

#[test]
fn foreign_parent_ranges_and_transfer_tags_refuse_before_emission() {
    for mode in 0..5 {
        exercise(b"\ntext", 1, |original, fragment, _, timer| {
            let mut part = original.end.part;
            let mut id = *original.structure.source.id().as_bytes();
            match mode {
                0 | 4 => id[0] ^= 1,
                1 => part.body_start += 1,
                2 => part.entity_end -= 1,
                _ => part.encoding = Encoding::Base64,
            }
            let candidate = crate::mime_response::locators::candidate(
                part,
                1,
                crate::mime_response::locators::Parent {
                    id: crate::ids::BlobId::from_bytes(id),
                    base: 0,
                    length: original.structure.source.len(),
                },
                original.structure.work,
                original.structure.budget,
                &mut Credit::new(),
                Tick(1),
            )
            .unwrap();
            if mode == 4 {
                timer.after(3);
            }
            let expected = if mode == 4 {
                Error::Parent(crate::ports::Error::Deadline)
            } else {
                Error::SourceMismatch
            };
            assert!(
                matches!(Cursor::new(original, fragment, &candidate, Properties::ALL, Tick(1)), Err(found) if found == expected)
            );
        });
    }
}

#[test]
fn native_expiry_overrides_job_and_quota_refusals_without_copying() {
    for mode in 0..5 {
        exercise(b"\ntext", 1, |original, fragment, candidate, timer| {
            let error = Error::Parent(crate::ports::Error::Deadline);
            if mode == 0 {
                timer.after(3);
                assert_eq!(
                    Cursor::new(original, fragment, candidate, Properties::ALL, Tick(100)).err(),
                    Some(error)
                );
                return;
            }
            if mode <= 2 {
                let left = original.structure.work.remaining();
                original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if mode == 1 { left.records } else { 0 },
                            output_bytes: if mode == 2 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut cursor =
                Cursor::new(original, fragment, candidate, Properties::ALL, Tick(1)).unwrap();
            if mode >= 3 {
                drain(&mut cursor, 64).unwrap();
            }
            timer.after(if mode <= 2 { 5 } else { 3 });
            if mode == 4 {
                assert_eq!(cursor.finish(Tick(100)).err(), Some(error));
            } else {
                let mut output = [0xa5; 64];
                assert_eq!(
                    cursor.poll(Tick(if mode == 3 { 100 } else { 1 }), &mut output),
                    Err(error)
                );
                assert_eq!(output, [0xa5; 64]);
                assert_eq!(cursor.value(), None);
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            }
        });
    }
}

#[test]
fn leaf_without_locator_and_multipart_with_locator_refuse() {
    for multipart in [false, true] {
        exercise(
            if multipart { MULTIPART } else { b"\ntext" },
            1,
            |original, fragment, _, _| {
                let mut part = original.end.part;
                part.media = if multipart {
                    Media::TextPlain
                } else {
                    Media::Multipart
                };
                let candidate = crate::mime_response::locators::candidate(
                    part,
                    1,
                    crate::mime_response::locators::Parent {
                        id: original.structure.source.id(),
                        base: 0,
                        length: original.structure.source.len(),
                    },
                    original.structure.work,
                    original.structure.budget,
                    &mut Credit::new(),
                    Tick(1),
                )
                .unwrap();
                assert_eq!(
                    Cursor::new(original, fragment, &candidate, Properties::ALL, Tick(1)).err(),
                    Some(Error::InvalidState)
                );
            },
        );
    }
}
