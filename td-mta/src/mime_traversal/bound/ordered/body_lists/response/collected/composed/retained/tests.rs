#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::tests::{drain as part_drain, exercise as collection};
use super::super::{Cursor as Composer, Status as WireStatus};
use super::*;
use crate::{
    admission::work::Stop,
    mime_traversal::bound::ordered::{body_lists::tests::ALTERNATIVE, tests::SOURCE},
};
fn forget<T>(value: T) {
    std::mem::forget(value);
}
fn exercise<T>(source: &[u8], base: u64, f: impl FnOnce(Serialized<'_, '_, '_, '_, '_>) -> T) -> T {
    collection(source, base, |mut owner, storage, scratch| {
        for _ in 0..owner.total().unwrap() {
            let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
            part_drain(&mut child);
            child.finish(Tick(1)).unwrap();
        }
        f(owner.finish(Tick(1)).unwrap())
    })
}
fn costs(owner: &Serialized<'_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &owner.projected.structure;
    let left = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
fn bare(source: &[u8], base: u64, mode: Mode) -> (Vec<u8>, [u64; 5]) {
    exercise(source, base, |owner| {
        let before = costs(&owner);
        let mut cursor = Composer::new(owner, mode, Tick(1)).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..100000 {
            let mut output = [0; 64];
            let p = cursor.poll(Tick(1), &mut output).unwrap();
            bytes.extend_from_slice(&output[..p.written]);
            if p.status == WireStatus::Complete {
                let after = costs(&cursor.source);
                return (bytes, std::array::from_fn(|i| before[i] - after[i]));
            }
        }
        panic!("bare did not finish")
    })
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_>) -> usize {
    for turn in 1..100000 {
        if cursor.poll(Tick(1)).unwrap() == Status::Complete {
            return turn;
        }
    }
    panic!("retainer did not finish")
}
#[test]
fn whole_bytes_and_all_five_costs_match_bare_original_composition() {
    for source in [SOURCE, ALTERNATIVE, b"\r\nabc".as_slice()] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for mode in [Mode::Structure, Mode::Lists] {
                let (expected, used) = bare(source, base, mode);
                exercise(source, base, |owner| {
                    let original = (
                        std::ptr::from_mut(owner.projected.structure.work),
                        std::ptr::from_mut(owner.projected.structure.budget),
                    );
                    let tables = (
                        owner.projected.structure.parts().unwrap().as_ptr(),
                        owner.cells.as_ptr(),
                    );
                    let view = owner.value().unwrap();
                    let bound = if mode == Mode::Structure {
                        16 + view
                            .fragments
                            .iter()
                            .map(|cell| cell.value().unwrap().fragment.len() + 19)
                            .sum::<usize>()
                    } else {
                        let lists = view.selected.lists;
                        66 + lists
                            .text
                            .iter()
                            .chain(lists.html)
                            .chain(lists.attachments)
                            .map(|ordinal| {
                                view.fragments[usize::from(*ordinal) - 1]
                                    .value()
                                    .unwrap()
                                    .fragment
                                    .len()
                                    + 19
                            })
                            .sum::<usize>()
                    };
                    assert!(expected.len() <= bound);
                    let before = costs(&owner);
                    let mut output = vec![0xa5; expected.len()];
                    let identity = output.as_ptr();
                    let mut cursor = Cursor::new(owner, mode, &mut output, Tick(1)).unwrap();
                    drain(&mut cursor);
                    assert_eq!(cursor.value().unwrap().members, expected);
                    let after = costs(&cursor.composer.source);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        used
                    );
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(costs(&cursor.composer.source), after);
                    let retained = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(retained.value().unwrap().members, expected);
                    let ((actual, bytes, view), work, budget) = retained.finish(Tick(1)).unwrap();
                    assert_eq!(actual, mode);
                    assert_eq!(bytes, expected);
                    assert_eq!(bytes.as_ptr(), identity);
                    assert_eq!(
                        (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                        original
                    );
                    assert_eq!(
                        (
                            view.selected.structure.parts.as_ptr(),
                            view.fragments.as_ptr()
                        ),
                        tables
                    );
                });
            }
        }
    }
}
#[test]
fn every_shorter_capacity_refuses_stickily_without_exposing_prefix() {
    for mode in [Mode::Structure, Mode::Lists] {
        let (expected, _) = bare(SOURCE, 17, mode);
        for capacity in 0..expected.len() {
            exercise(SOURCE, 17, |owner| {
                let mut output = vec![0xa5; capacity];
                let mut cursor = Cursor::new(owner, mode, &mut output, Tick(1)).unwrap();
                let mut first = None;
                for _ in 0..100000 {
                    match cursor.poll(Tick(1)) {
                        Err(e) => {
                            first = Some(e);
                            break;
                        }
                        Ok(Status::Complete) => panic!("short window completed"),
                        Ok(_) => {}
                    }
                }
                assert_eq!(first, Some(Error::ResponseCapacity));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(100)).err(), first);
                assert_eq!(cursor.finish(Tick(1)).err(), first);
            });
        }
    }
}
#[test]
fn all_prefixes_and_completed_owners_require_fresh_whole_consumption() {
    for mode in [Mode::Structure, Mode::Lists] {
        let turns = exercise(SOURCE, 17, |owner| {
            let mut output = [0; 2048];
            let mut cursor = Cursor::new(owner, mode, &mut output, Tick(1)).unwrap();
            drain(&mut cursor)
        });
        for prefix in 0..=turns {
            for fault in 0..4 {
                exercise(SOURCE, 17, |owner| {
                    let mut output = [0; 2048];
                    let mut cursor = Cursor::new(owner, mode, &mut output, Tick(1)).unwrap();
                    for _ in 0..prefix {
                        cursor.poll(Tick(1)).unwrap();
                    }
                    if prefix < turns {
                        assert!(cursor.value().is_none());
                    }
                    let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
                    if fault == 0 {
                        assert_eq!(cursor.finish(Tick(100)).err(), Some(error));
                    } else if fault == 1 {
                        assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(1)), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    } else if fault == 2 {
                        forget(cursor);
                    } else if prefix == turns {
                        cursor.finish(Tick(1)).unwrap();
                    } else {
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                    }
                });
            }
        }
        for explicit in [false, true] {
            exercise(SOURCE, 0, |owner| {
                let mut output = [0; 2048];
                let mut cursor = Cursor::new(owner, mode, &mut output, Tick(1)).unwrap();
                drain(&mut cursor);
                let mut retained = cursor.finish(Tick(1)).unwrap();
                let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
                if explicit {
                    assert_eq!(retained.check_deadline(Tick(100)), Err(error));
                    assert!(retained.value().is_none());
                    assert_eq!(retained.finish(Tick(1)).err(), Some(error));
                } else {
                    assert_eq!(retained.finish(Tick(100)).err(), Some(error));
                }
            });
        }
    }
}
#[test]
fn freshness_precedes_empty_or_full_midstream_capacity() {
    exercise(SOURCE, 0, |owner| {
        let mut cursor = Cursor::new(owner, Mode::Structure, &mut [], Tick(1)).unwrap();
        assert!(cursor.value().is_none());
        let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
        assert_eq!(cursor.poll(Tick(100)), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    });
    exercise(SOURCE, 0, |mut owner| {
        let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
        assert_eq!(owner.check_deadline(Tick(100)), Err(error));
        assert_eq!(
            Cursor::new(owner, Mode::Structure, &mut [], Tick(1)).err(),
            Some(error)
        );
    });
    exercise(SOURCE, 0, |owner| {
        assert_eq!(
            Cursor::new(owner, Mode::Structure, &mut [], Tick(100)).err(),
            Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
        );
    });
    exercise(SOURCE, 0, |owner| {
        let mut output = [0; 1];
        let mut cursor = Cursor::new(owner, Mode::Structure, &mut output, Tick(1)).unwrap();
        for _ in 0..1000 {
            cursor.poll(Tick(1)).unwrap();
            if cursor.window.provisional().unwrap().len() == 1 {
                break;
            }
        }
        assert_eq!(cursor.window.provisional().unwrap().len(), 1);
        let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
        assert_eq!(cursor.poll(Tick(100)), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
    });
}
#[test]
fn original_budget_refusal_hides_whole_retained_bytes() {
    for mode in [Mode::Structure, Mode::Lists] {
        let (_, used) = bare(SOURCE, 0, mode);
        for (counter, &used_value) in used.iter().enumerate() {
            for cutoff in 0..=used_value {
                exercise(SOURCE, 0, |mut owner| {
                    let structure = &mut owner.projected.structure;
                    if counter < 2 {
                        let left = if counter == 0 {
                            structure.budget.source_bytes_remaining()
                        } else {
                            structure.budget.steps_remaining()
                        };
                        structure
                            .budget
                            .charge(
                                structure.work,
                                Tick(1),
                                if counter == 0 { left - cutoff } else { 0 },
                                if counter == 1 { left - cutoff } else { 0 },
                                &mut crate::nfc::Credit::new(),
                            )
                            .unwrap();
                    } else {
                        let left = structure.work.remaining();
                        structure
                            .work
                            .charge(
                                Tick(1),
                                crate::admission::work::Charge {
                                    io_bytes: if counter == 2 {
                                        left.io_bytes - cutoff
                                    } else {
                                        0
                                    },
                                    records: if counter == 3 {
                                        left.records - cutoff
                                    } else {
                                        0
                                    },
                                    output_bytes: if counter == 4 {
                                        left.output_bytes - cutoff
                                    } else {
                                        0
                                    },
                                    ..Default::default()
                                },
                            )
                            .unwrap();
                    }
                    let mut output = [0; 2048];
                    let mut cursor = Cursor::new(owner, mode, &mut output, Tick(1)).unwrap();
                    let mut result = None;
                    for _ in 0..100000 {
                        match cursor.poll(Tick(1)) {
                            Ok(Status::Complete) => {
                                result = Some(Ok(()));
                                break;
                            }
                            Err(e) => {
                                result = Some(Err(e));
                                break;
                            }
                            Ok(_) => {}
                        }
                    }
                    if cutoff == used_value {
                        assert_eq!(result, Some(Ok(())));
                        cursor.finish(Tick(1)).unwrap();
                    } else {
                        let error = match counter {
                            1 => Error::Admission(crate::nfc::Error::InterpretationLimit),
                            3 => Error::Admission(crate::nfc::Error::Work(Stop::Records)),
                            4 => Error::Admission(crate::nfc::Error::Work(Stop::OutputBytes)),
                            _ => panic!("nondebited counter refused"),
                        };
                        assert_eq!(result, Some(Err(error)));
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(100)), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    }
                });
            }
        }
    }
}
