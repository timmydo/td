#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_body_lists::tests::{Storage, Timer};
use crate::blob_value_parts::tests::exercise;
fn selected(
    bytes: &[u8],
    test: impl FnOnce(selection::Retained<'_, '_, '_, '_, '_, '_, '_>, &Timer),
) {
    exercise(bytes, |original, _, timer| {
        let mut selector = selection::Cursor::new(original, Tick(1)).unwrap();
        for _ in 0..100 {
            if selector.poll(Tick(1)).unwrap() == selection::Status::Complete {
                test(selector.finish(Tick(1)).unwrap(), timer);
                return;
            }
        }
        panic!("selector stalled");
    });
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let mut wire = Vec::new();
    for _ in 0..10000 {
        let mut output = [0xa5; 64];
        let p = cursor.poll(Tick(1), &mut output[..width])?;
        assert!(p.written <= 6);
        assert!(output[p.written..].iter().all(|&b| b == 0xa5));
        wire.extend_from_slice(&output[..p.written]);
        if p.status == Status::Complete {
            return Ok(wire);
        }
    }
    panic!("preview stalled");
}
const CASES: &[(&[u8], &str, Option<u16>)] = &[
    (b"\r\nhello", "hello", Some(1)),
    (b"Content-Type: text/html;charset=latin1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<b>caf=E9</b>", "café", Some(1)),
    (b"Content-Type: image/png\r\n\r\ni", "", None),
    (b"Content-Type: text/plain\r\nContent-Disposition: attachment\r\n\r\na", "", None),
    (b"Content-Type: multipart/alternative;boundary=x\r\n\r\n--x\r\nContent-Type: text/html\r\n\r\nhtml\r\n--x\r\nContent-Type: text/plain\r\n\r\nplain\r\n--x--\r\n", "plain", Some(3)),
    (b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: image/png\r\n\r\ni\r\n--x\r\nContent-Type: text/html\r\n\r\n<b>html</b>\r\n--x\r\nContent-Type: text/plain\r\n\r\nplain\r\n--x--\r\n", "html", Some(3)),
    (b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nlater\r\n--x--\r\n", "", Some(2)),
];
#[test]
fn selection_interpretation_and_json_preserve_original_owners() {
    for &(source, text, ordinal) in CASES {
        for width in [1, 7, 64] {
            selected(source, |original, _| {
                let pointers = (
                    std::ptr::from_mut(original.original.structure.source),
                    std::ptr::from_mut(original.original.structure.work),
                    std::ptr::from_mut(original.original.structure.budget),
                    original.original.structure.parts.as_ptr(),
                    original.original.structure.headers.as_ptr(),
                    original.original.nodes.as_ptr(),
                    original.original.lists.text.as_ptr(),
                    original.original.lists.html.as_ptr(),
                );
                let mut storage = Storage::new();
                let mut scratch = Scratch::new();
                let mut input = [0; 3];
                let mut checkpoints = Checkpoints::default();
                let backing = (input.as_mut_ptr(), std::ptr::from_mut(&mut checkpoints));
                let clock = Timer::new();
                let mut cursor = Cursor::new(
                    original,
                    Buffers {
                        metadata: storage.backing(),
                        scratch: &mut scratch,
                        input: &mut input,
                        checkpoints: &mut checkpoints,
                    },
                    &clock,
                    Tick(1),
                )
                .unwrap();
                assert!(cursor.value().is_none());
                assert_eq!(
                    drain(&mut cursor, width).unwrap(),
                    format!("\"{text}\"").as_bytes()
                );
                assert_eq!(
                    cursor.poll(Tick(1), &mut []).unwrap().status,
                    Status::Complete
                );
                assert_eq!(cursor.poll(Tick(1), &mut [0; 64]).unwrap().written, 0);
                let retained = cursor.finish(Tick(1)).unwrap();
                assert_eq!(retained.original.choice.map(|c| c.ordinal), ordinal);
                assert_eq!(retained.diagnostics.is_some(), ordinal.is_some());
                assert_eq!(
                    backing,
                    (
                        retained.input.as_mut_ptr(),
                        std::ptr::from_mut(retained.checkpoints)
                    )
                );
                let original = retained.original.original;
                assert_eq!(
                    pointers,
                    (
                        std::ptr::from_mut(original.structure.source),
                        std::ptr::from_mut(original.structure.work),
                        std::ptr::from_mut(original.structure.budget),
                        original.structure.parts.as_ptr(),
                        original.structure.headers.as_ptr(),
                        original.nodes.as_ptr(),
                        original.lists.text.as_ptr(),
                        original.lists.html.as_ptr(),
                    )
                );
            });
        }
    }
}
#[test]
fn inconsistent_choice_and_classification_refuse_before_preview_output() {
    for mode in 0..3 {
        selected(b"\r\nabc", |mut original, _| {
            let mut nodes = original.original.nodes.to_vec();
            if mode == 0 {
                original.choice.as_mut().unwrap().format = Format::Html;
            }
            if mode == 1 {
                nodes[0].depth += 1;
            }
            if mode == 2 {
                original.choice.as_mut().unwrap().ordinal = 0;
            }
            let original = selection::Retained {
                original: blob_body_lists::Retained {
                    nodes: &nodes,
                    ..original.original
                },
                choice: original.choice,
            };
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut input = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let clock = Timer::new();
            let result = Cursor::new(
                original,
                Buffers {
                    metadata: storage.backing(),
                    scratch: &mut scratch,
                    input: &mut input,
                    checkpoints: &mut checkpoints,
                },
                &clock,
                Tick(1),
            );
            if mode == 2 {
                assert!(result.is_err());
                return;
            }
            let mut cursor = result.unwrap();
            for _ in 0..1000 {
                let mut output = [0xa5; 64];
                let result = cursor.poll(Tick(1), &mut output);
                assert_eq!(output, [0xa5; 64]);
                if let Err(error) = result {
                    assert_eq!(error, Error::State);
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    return;
                }
            }
            panic!("bad choice accepted");
        });
    }
}
#[test]
fn native_expiry_and_empty_output_quota_hide_completion() {
    for source in [b"\r\nabc".as_slice(), b"Content-Type: image/png\r\n\r\ni"] {
        for mode in 0..5 {
            selected(source, |original, timer| {
                let mut storage = Storage::new();
                let mut scratch = Scratch::new();
                let mut input = [0; 3];
                let mut checkpoints = Checkpoints::default();
                let clock = Timer::new();
                if mode == 0 {
                    timer.after(0);
                }
                if mode == 4 {
                    let work = &mut *original.original.structure.work;
                    work.charge(
                        Tick(1),
                        Charge {
                            output_bytes: work.remaining().output_bytes,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
                }
                let result = Cursor::new(
                    original,
                    Buffers {
                        metadata: storage.backing(),
                        scratch: &mut scratch,
                        input: &mut input,
                        checkpoints: &mut checkpoints,
                    },
                    &clock,
                    Tick(1),
                );
                if mode == 0 {
                    assert!(result.is_err());
                    return;
                }
                let mut cursor = result.unwrap();
                if mode == 2 || mode == 3 {
                    drain(&mut cursor, 64).unwrap();
                }
                if mode != 4 {
                    timer.after(0);
                }
                if mode == 3 {
                    assert!(cursor.finish(Tick(1)).is_err());
                    return;
                }
                let error = drain(&mut cursor, 64).unwrap_err();
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}
#[test]
fn empty_preview_spends_exactly_two_records_and_output_bytes() {
    selected(b"Content-Type: image/png\r\n\r\ni", |original, _| {
        let before = original.original.structure.work.remaining();
        let steps = original.original.structure.budget.steps_remaining();
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        let mut input = [];
        let mut checkpoints = Checkpoints::default();
        let clock = Timer::new();
        let mut cursor = Cursor::new(
            original,
            Buffers {
                metadata: storage.backing(),
                scratch: &mut scratch,
                input: &mut input,
                checkpoints: &mut checkpoints,
            },
            &clock,
            Tick(1),
        )
        .unwrap();
        assert_eq!(
            cursor.poll(Tick(1), &mut []).unwrap().status,
            Status::NeedOutput
        );
        assert_eq!(drain(&mut cursor, 64).unwrap(), b"\"\"");
        let retained = cursor.finish(Tick(1)).unwrap();
        let work = retained.original.original.structure.work.remaining();
        assert_eq!(before.records - work.records, 2);
        assert_eq!(before.output_bytes - work.output_bytes, 2);
        assert_eq!(before.io_bytes, work.io_bytes);
        assert_eq!(
            steps,
            retained
                .original
                .original
                .structure
                .budget
                .steps_remaining()
        );
    });
}

#[test]
fn nonbody_choices_and_invalid_decoder_backing_refuse() {
    for source in [
        b"Content-Type: text/calendar\r\nContent-Disposition: attachment\r\n\r\nsecret".as_slice(),
        b"Content-Type: text/plain\r\nContent-Disposition: attachment\r\n\r\nsecret",
    ] {
        selected(source, |mut original, _| {
            original.choice = Some(selection::Choice {
                ordinal: 1,
                format: Format::Plain,
            });
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut input = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let clock = Timer::new();
            let mut cursor = Cursor::new(
                original,
                Buffers {
                    metadata: storage.backing(),
                    scratch: &mut scratch,
                    input: &mut input,
                    checkpoints: &mut checkpoints,
                },
                &clock,
                Tick(1),
            )
            .unwrap();
            for _ in 0..1000 {
                let mut output = [0xa5; 64];
                let result = cursor.poll(Tick(1), &mut output);
                assert_eq!(output, [0xa5; 64]);
                if let Err(error) = result {
                    assert_eq!(error, Error::State);
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    return;
                }
            }
            panic!("nonbody accepted");
        });
    }
    for length in [0, crate::mime_input::INPUT_BYTES + 1] {
        selected(b"\r\nx", |original, _| {
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut input = vec![0; length];
            let mut checkpoints = Checkpoints::default();
            let clock = Timer::new();
            assert_eq!(
                Cursor::new(
                    original,
                    Buffers {
                        metadata: storage.backing(),
                        scratch: &mut scratch,
                        input: &mut input,
                        checkpoints: &mut checkpoints
                    },
                    &clock,
                    Tick(1)
                )
                .err(),
                Some(Error::Backing)
            );
        });
    }
}
#[test]
fn scratch_handoff_job_deadlines_and_empty_postcopy_fence() {
    for source in [b"\r\nabc".as_slice(), b"Content-Type: image/png\r\n\r\ni"] {
        for mode in 0..4 {
            selected(source, |original, timer| {
                let mut storage = Storage::new();
                let mut scratch = Scratch::new();
                let pointer = std::ptr::from_mut(&mut scratch);
                let mut input = [0; 3];
                let mut checkpoints = Checkpoints::default();
                let clock = Timer::new();
                let mut cursor = Cursor::new(
                    original,
                    Buffers {
                        metadata: storage.backing(),
                        scratch: &mut scratch,
                        input: &mut input,
                        checkpoints: &mut checkpoints,
                    },
                    &clock,
                    Tick(1),
                )
                .unwrap();
                if mode == 0 {
                    drain(&mut cursor, 64).unwrap();
                    let retained = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(std::ptr::from_mut(retained.scratch), pointer);
                    let mut storage = Storage::new();
                    let mut part = blob_part::Cursor::new(
                        retained.original.original.structure,
                        1,
                        storage.backing(),
                        retained.scratch,
                        Tick(1),
                    )
                    .unwrap();
                    while part.poll(Tick(1)).unwrap() != blob_part::Status::Complete {}
                    part.finish(Tick(1)).unwrap();
                    return;
                }
                if mode == 1 {
                    assert!(cursor.poll(Tick(100), &mut []).is_err());
                    assert!(cursor.value().is_none());
                    return;
                }
                if mode == 2 {
                    drain(&mut cursor, 64).unwrap();
                    if matches!(cursor.stage, Some(Stage::Preview(_))) {
                        clock.after(0);
                        assert!(cursor.poll(Tick(1), &mut []).is_err());
                    } else {
                        assert!(cursor.poll(Tick(100), &mut []).is_err());
                    }
                    assert!(cursor.value().is_none());
                    return;
                }
                if matches!(cursor.stage, Some(Stage::Empty(..))) {
                    timer.after(9);
                    let mut output = [0xa5; 64];
                    assert_eq!(
                        cursor.poll(Tick(1), &mut output),
                        Err(Error::Policy(crate::ports::Error::Deadline))
                    );
                    assert_eq!(output[0], b'"');
                    assert!(cursor.value().is_none());
                    assert_eq!(
                        cursor.finish(Tick(1)).err(),
                        Some(Error::Policy(crate::ports::Error::Deadline))
                    );
                }
            });
        }
    }
}
#[test]
fn output_exhaustion_after_metadata_is_a_decoding_failure() {
    let mut source = b"\r\n".to_vec();
    source.extend_from_slice(&[b'a'; 700]);
    selected(&source, |original, _| {
        let work = &mut *original.original.structure.work;
        work.charge(
            Tick(1),
            Charge {
                output_bytes: work.remaining().output_bytes - 512,
                ..Charge::default()
            },
        )
        .unwrap();
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        let mut input = [0; 3];
        let mut checkpoints = Checkpoints::default();
        let clock = Timer::new();
        let mut cursor = Cursor::new(
            original,
            Buffers {
                metadata: storage.backing(),
                scratch: &mut scratch,
                input: &mut input,
                checkpoints: &mut checkpoints,
            },
            &clock,
            Tick(1),
        )
        .unwrap();
        let mut decoding = false;
        for _ in 0..10000 {
            decoding |= matches!(cursor.stage, Some(Stage::Preview(_)));
            if let Err(error) = cursor.poll(Tick(1), &mut [0; 64]) {
                assert!(decoding);
                assert_eq!(
                    error,
                    Error::Preview(blob_preview::Error::Json(crate::preview_json::Error::Work(
                        Stop::OutputBytes
                    )))
                );
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                return;
            }
        }
        panic!("output allowance did not exhaust");
    });
}
