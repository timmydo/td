#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::blob_body_lists::tests::{Storage, Timer};
use crate::blob_value_parts::tests::{exercise, SOURCE};
use crate::body_value_options::Options;
fn selected(
    bytes: &[u8],
    mask: u8,
    cap: u64,
    test: impl FnOnce(blob_value_parts::Retained<'_, '_, '_, '_, '_, '_, '_, '_, '_>, &Timer),
) {
    exercise(bytes, |original, facts, timer| {
        let mut ordinals = [0; 8];
        let mut membership = [0; 8];
        let mut selection = blob_value_parts::Cursor::new(
            original,
            facts,
            &mut ordinals,
            &mut membership,
            Options {
                fetch_text: mask & 1 != 0,
                fetch_html: mask & 2 != 0,
                fetch_all: mask & 4 != 0,
                max_bytes: cap,
            },
            Tick(1),
        )
        .unwrap();
        for _ in 0..1000 {
            if selection.poll(Tick(1)).unwrap() == blob_value_parts::Status::Complete {
                test(selection.finish(Tick(1)).unwrap(), timer);
                return;
            }
        }
        panic!("selection stalled");
    });
}
fn drain(
    cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>,
    width: usize,
) -> Result<Vec<u8>, Error> {
    let clock = Timer::new();
    let mut storage = Storage::new();
    let mut scratch = Scratch::new();
    let mut input = [0; 3];
    let mut checkpoints = Checkpoints::default();
    let mut bytes = Vec::new();
    for _ in 0..1000 {
        let mut output = [0xa5; 128];
        let p = cursor.poll(Tick(1), &mut output[..width])?;
        assert!(p.written <= 64);
        assert!(output[p.written..].iter().all(|&b| b == 0xa5));
        bytes.extend_from_slice(&output[..p.written]);
        match p.status {
            Status::Complete => return Ok(bytes),
            Status::NeedValue(_) => {
                let mut child = cursor.begin_value(
                    Buffers {
                        metadata: storage.backing(),
                        scratch: &mut scratch,
                        input: &mut input,
                        checkpoints: &mut checkpoints,
                    },
                    &clock,
                    Tick(1),
                )?;
                let mut done = false;
                for _ in 0..10000 {
                    let mut output = [0xa5; 128];
                    let p = child.poll(Tick(1), &mut output[..width])?;
                    assert!(p.written <= crate::body_value::json::MAX_CHUNK_BYTES);
                    assert!(output[p.written..].iter().all(|&b| b == 0xa5));
                    bytes.extend_from_slice(&output[..p.written]);
                    if p.status == blob_body_value::Status::Complete {
                        done = true;
                        break;
                    }
                }
                assert!(done);
                assert!(child.value().is_some());
                child.finish(Tick(1))?;
            }
            _ => {}
        }
    }
    panic!("bodyValues stalled");
}
#[test]
fn selected_map_streams_once_in_order_reusing_child_buffers() {
    for mask in 0..8 {
        for width in [1, 7, 64, 128] {
            selected(SOURCE, mask, 0, |original, _| {
                let pointers = (
                    std::ptr::from_mut(original.original.structure.source),
                    std::ptr::from_mut(original.original.structure.work),
                    std::ptr::from_mut(original.original.structure.budget),
                    original.facts.as_ptr(),
                    original.ordinals.as_ptr(),
                );
                let mut cursor = Cursor::new(original, Tick(1)).unwrap();
                assert_eq!(
                    cursor.poll(Tick(1), &mut []).unwrap().status,
                    Status::NeedOutput
                );
                let actual = drain(&mut cursor, width).unwrap();
                let mut expected = String::from("\"bodyValues\":{");
                let pairs: &[(u16, &str)] = if mask & 4 != 0 {
                    &[(2, "p"), (3, "<h>"), (5, "c")]
                } else if mask & 3 != 0 {
                    &[(2, "p"), (3, "<h>")]
                } else {
                    &[]
                };
                for (i, (id, value)) in pairs.iter().enumerate() {
                    if i != 0 {
                        expected.push(',');
                    }
                    expected.push_str(&format!("\"{id}\":{{\"value\":\"{value}\",\"isEncodingProblem\":false,\"isTruncated\":false}}"));
                }
                expected.push('}');
                assert_eq!(actual, expected.as_bytes());
                assert_eq!(
                    cursor.poll(Tick(1), &mut []).unwrap().status,
                    Status::Complete
                );
                let retained = cursor.finish(Tick(1)).unwrap();
                assert_eq!(
                    pointers,
                    (
                        std::ptr::from_mut(retained.original.structure.source),
                        std::ptr::from_mut(retained.original.structure.work),
                        std::ptr::from_mut(retained.original.structure.budget),
                        retained.facts.as_ptr(),
                        retained.ordinals.as_ptr(),
                    )
                );
            });
        }
    }
}
#[test]
fn capped_html_and_invalid_text_keep_value_diagnostics() {
    for (bytes, cap, value, problem) in [
        (
            b"Content-Type: text/html;charset=UTF-8\r\n\r\n<b>abc</b>".as_slice(),
            2,
            "",
            false,
        ),
        (
            b"Content-Type: text/plain;charset=UTF-8\r\n\r\na\xff",
            1,
            "a",
            true,
        ),
    ] {
        selected(bytes, 4, cap, |original, _| {
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            assert_eq!(drain(&mut cursor, 1).unwrap(), format!("\"bodyValues\":{{\"1\":{{\"value\":\"{value}\",\"isEncodingProblem\":{problem},\"isTruncated\":true}}}}").as_bytes());
            cursor.finish(Tick(1)).unwrap();
        });
    }
}
#[test]
#[allow(clippy::drop_non_drop, clippy::forget_non_drop)]
fn abandoned_child_retires_parent_and_early_finish_checks_native_deadline() {
    for mode in 0..4 {
        selected(b"\r\nabc", 4, 0, |original, timer| {
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            seek_value(&mut cursor);
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut input = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let clock = Timer::new();
            let child = cursor
                .begin_value(
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
            let error = match mode {
                0 => {
                    drop(child);
                    Error::State
                }
                1 => {
                    std::mem::forget(child);
                    Error::State
                }
                2 => {
                    timer.after(0);
                    let error = child.finish(Tick(1)).unwrap_err();
                    assert_eq!(
                        error,
                        Error::Metadata(blob_part::Error::Policy(PolicyError::Deadline))
                    );
                    error
                }
                _ => {
                    let error = child.finish(Tick(1)).unwrap_err();
                    assert_eq!(
                        error,
                        Error::Metadata(blob_part::Error::Interpretation(bound::Error::Metadata(
                            label_json::Error::InvalidState
                        )))
                    );
                    error
                }
            };
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}
#[test]
fn parent_expiry_and_output_quota_hide_provisional_map() {
    use crate::admission::work::Stop;
    for mode in 0..5 {
        selected(b"\r\nabc", 0, 0, |original, timer| {
            if mode == 4 {
                let remaining = original.original.structure.work.remaining().output_bytes;
                original
                    .original
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
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            if mode == 1 || mode == 2 {
                drain(&mut cursor, 64).unwrap();
            }
            if mode < 3 {
                timer.after(0);
            }
            let error = if mode < 3 {
                Error::Policy(PolicyError::Deadline)
            } else {
                Error::Admission(crate::nfc::Error::Work(if mode == 3 {
                    Stop::Deadline
                } else {
                    Stop::OutputBytes
                }))
            };
            if mode == 2 {
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else {
                let mut output = [0xa5; 64];
                assert_eq!(
                    cursor.poll(Tick(if mode == 3 { 100 } else { 1 }), &mut output),
                    Err(error)
                );
                assert_eq!(output, [0xa5; 64]);
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
            }
        });
    }
}

#[test]
fn corrupt_selected_keys_and_changed_facts_cannot_complete() {
    for mode in 0..9 {
        selected(SOURCE, 4, 0, |original, _| {
            let mut facts = original.facts.to_vec();
            match mode {
                0 => original.ordinals[0] = 0,
                1 => original.ordinals[1] = original.ordinals[0],
                2 => original.ordinals[0] = u16::MAX,
                4 => original.ordinals[1] = u16::MAX,
                5 => facts[1].end.part.body_start += 1,
                6 => facts[1].end.node.class.named = !facts[1].end.node.class.named,
                7 => facts[1].end.part.ordinal = 3,
                8 => facts[1].kind = blob_value_parts::Kind::Other,
                _ => facts[1].kind = blob_value_parts::Kind::Html,
            }
            let original = blob_value_parts::Retained {
                facts: &facts,
                ..original
            };
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            let expected = if mode == 3 || mode == 5 || mode == 6 {
                Error::Metadata(blob_part::Error::Interpretation(bound::Error::InvalidState))
            } else {
                Error::State
            };
            assert_eq!(drain(&mut cursor, 1), Err(expected));
            if mode == 4 {
                assert_eq!(cursor.key, *b",\"65535\":");
                assert_eq!(cursor.control.key_len, 9);
            }
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut []), Err(expected));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
        });
    }
}

#[test]
fn value_child_expiry_is_sticky_after_active_and_cached_encoding() {
    for mode in 0..4 {
        selected(b"\r\nabc", 4, 0, |original, timer| {
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            seek_value(&mut cursor);
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut input = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let clock = Timer::new();
            let mut child = cursor
                .begin_value(
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
            for _ in 0..10000 {
                if ((mode == 0 || mode == 3) && child.encoded.is_some()) || child.value().is_some()
                {
                    break;
                }
                child.poll(Tick(1), &mut [0; 64]).unwrap();
            }
            assert!(child.encoded.is_some());
            if mode == 1 || mode == 2 {
                assert!(child.value().is_some());
            }
            timer.after(if mode == 3 { 9 } else { 0 });
            let error = Error::Value(blob_body_value::Error::Policy(PolicyError::Deadline));
            if mode == 2 {
                assert_eq!(child.finish(Tick(1)), Err(error));
            } else {
                let mut output = [0xa5; 64];
                assert_eq!(child.poll(Tick(1), &mut output), Err(error));
                if mode == 3 {
                    assert_ne!(output, [0xa5; 64]);
                }
                assert!(child.value().is_none());
                assert_eq!(child.check_deadline(Tick(1)), Err(error));
                assert_eq!(child.finish(Tick(1)), Err(error));
            }
            assert_eq!(cursor.poll(Tick(1), &mut []), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}

fn seek_value(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>) {
    for _ in 0..100 {
        if let Status::NeedValue(1) = cursor.poll(Tick(1), &mut [0; 64]).unwrap().status {
            return;
        }
    }
    panic!("parent did not request its value");
}

#[test]
fn child_job_expiry_partial_finish_and_output_quota_retire_the_map() {
    use crate::{admission::work::Stop, body_value::json};
    for mode in 0..3 {
        let bytes = format!("\r\n{}", "x".repeat(1024));
        selected(bytes.as_bytes(), 4, 0, |original, _| {
            if mode == 2 {
                let remaining = original.original.structure.work.remaining().output_bytes;
                original
                    .original
                    .structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: remaining - 256,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            let mut cursor = Cursor::new(original, Tick(1)).unwrap();
            seek_value(&mut cursor);
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut input = [0; 3];
            let mut checkpoints = Checkpoints::default();
            let clock = Timer::new();
            let mut child = cursor
                .begin_value(
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
            for _ in 0..10000 {
                if child.encoded.is_some() {
                    break;
                }
                child.poll(Tick(1), &mut []).unwrap();
            }
            assert!(child.encoded.is_some());
            assert_eq!(child.poll(Tick(1), &mut [0; 1]).unwrap().written, 1);
            assert!(child.value().is_none());
            let expected = if mode == 0 {
                Error::Value(blob_body_value::Error::Metadata(bound::Error::InvalidState))
            } else {
                Error::Value(blob_body_value::Error::Json(json::Error::Work(
                    if mode == 1 {
                        Stop::Deadline
                    } else {
                        Stop::OutputBytes
                    },
                )))
            };
            if mode == 0 {
                assert_eq!(child.finish(Tick(1)), Err(expected));
            } else {
                if mode == 1 {
                    clock.after(0);
                }
                let mut failure = None;
                for _ in 0..10000 {
                    match child.poll(Tick(1), &mut [0; 64]) {
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                        Ok(progress) => {
                            assert_ne!(progress.status, blob_body_value::Status::Complete)
                        }
                    }
                }
                assert_eq!(failure, Some(expected));
                assert_eq!(child.finish(Tick(1)), Err(expected));
            }
            assert_eq!(cursor.check_deadline(Tick(1)), Err(expected));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
        });
    }
}
