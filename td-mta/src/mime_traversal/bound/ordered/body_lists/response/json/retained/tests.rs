#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::tests::{Lists, ALTERNATIVE};
use super::super::super::{tests::selection, Projecting};
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    mime_body_lists::Node,
    mime_traversal::{
        bound::ordered::tests::{meter, Storage, SOURCE},
        Part as Descriptor,
    },
};
fn forget<T>(value: T) {
    std::mem::forget(value);
}
fn exercise<T>(
    source: &[u8],
    base: u64,
    ordinal: usize,
    cap: usize,
    f: impl FnOnce(Cursor<'_, '_>, (*mut Meter, *mut HeaderBudget, *mut Scratch)) -> (T, bool),
) -> T {
    let mut parts = [Descriptor::default(); 8];
    let mut nodes = [Node::default(); 8];
    let mut lists = Lists::new();
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut storage = Storage::new();
    let mut scratch = Scratch::new();
    let mut output = [0xa5; 1024];
    let identity = output.as_ptr();
    let original = (
        std::ptr::from_mut(&mut work),
        std::ptr::from_mut(&mut budget),
        std::ptr::from_mut(&mut scratch),
    );
    let selected = selection(
        source,
        base,
        &mut parts,
        &mut nodes,
        &mut lists,
        &mut work,
        &mut budget,
    );
    let mut parent = Projecting::new(selected, Tick(1)).unwrap();
    for n in 1..=ordinal {
        let mut child = parent
            .next(storage.backing(256), &mut scratch, Tick(1))
            .unwrap();
        let mut done = false;
        for _ in 0..100000 {
            if child.poll(Tick(1)).unwrap() == Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        if n == ordinal {
            let cursor = Cursor::new(child, &mut output[..cap], Tick(1)).unwrap();
            assert_eq!(cursor.window.provisional().unwrap().as_ptr(), identity);
            let (result, complete) = f(cursor, original);
            if complete {
                assert_eq!(parent.completed().unwrap(), ordinal);
            } else {
                assert!(parent.completed().is_err());
            }
            assert!(output[cap..].iter().all(|b| *b == 0xa5));
            return result;
        }
        child.finish(Tick(1)).unwrap();
    }
    panic!("missing ordinal")
}
fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
    for turn in 1..10000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
    }
    panic!("retention did not finish")
}
fn costs(cursor: &Cursor<'_, '_>) -> [u64; 5] {
    framing_costs(&cursor.framing)
}
fn framing_costs(framing: &super::super::Cursor<'_>) -> [u64; 5] {
    let remaining = framing.scalars.work.remaining();
    [
        framing.scalars.budget.source_bytes_remaining(),
        framing.scalars.budget.steps_remaining(),
        remaining.io_bytes,
        remaining.records,
        remaining.output_bytes,
    ]
}
fn bare(source: &[u8], base: u64, ordinal: usize) -> (Vec<u8>, [u64; 5]) {
    exercise(source, base, ordinal, 1024, |cursor, original| {
        let before = costs(&cursor);
        let mut framing = cursor.framing;
        let mut output = [0xa5; 1024];
        let mut used = 0;
        let mut done = false;
        for _ in 0..10000 {
            let progress = framing.poll(Tick(1), &mut output[used..]).unwrap();
            used += progress.written;
            if progress.status == super::super::Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        let after = framing_costs(&framing);
        let charged = std::array::from_fn(|i| before[i] - after[i]);
        let (_, work, budget, scratch) = framing.finish(Tick(1)).unwrap();
        assert_eq!(
            (
                std::ptr::from_mut(work),
                std::ptr::from_mut(budget),
                std::ptr::from_mut(scratch)
            ),
            original
        );
        ((output[..used].to_vec(), charged), true)
    })
}
#[test]
fn exact_retention_preserves_original_fragment_costs_identity_and_owner_release() {
    for source in [SOURCE, ALTERNATIVE] {
        for ordinal in 1..=if source == SOURCE { 3 } else { 4 } {
            for base in [0, 17, u64::MAX - source.len() as u64] {
                let oracle = bare(source, base, ordinal);
                let (expected, charged) =
                    exercise(source, base, ordinal, 1024, |mut cursor, _original| {
                        let pointers = (
                            std::ptr::from_mut(cursor.framing.scalars.work),
                            std::ptr::from_mut(cursor.framing.scalars.budget),
                            std::ptr::from_mut(cursor.framing.scratch),
                        );
                        assert_eq!(pointers, _original);
                        let before = costs(&cursor);
                        let identity = cursor.window.provisional().unwrap().as_ptr();
                        assert!(cursor.value().is_none());
                        drain(&mut cursor).unwrap();
                        let expected = cursor.value().unwrap().fragment.to_vec();
                        let after = costs(&cursor);
                        let charged: [u64; 5] = std::array::from_fn(|i| before[i] - after[i]);
                        let (retained, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
                        assert_eq!(retained.end.part.ordinal as usize, ordinal);
                        assert_eq!(retained.fragment, expected);
                        assert_eq!(retained.fragment.as_ptr(), identity);
                        assert_eq!(
                            (
                                std::ptr::from_mut(work),
                                std::ptr::from_mut(budget),
                                std::ptr::from_mut(scratch)
                            ),
                            pointers
                        );
                        assert_eq!(charged[4] as usize, expected.len());
                        assert_eq!((&expected, charged), (&oracle.0, oracle.1));
                        ((expected, charged), true)
                    });
                exercise(
                    source,
                    base,
                    ordinal,
                    expected.len(),
                    |mut cursor, _original| {
                        let before = costs(&cursor);
                        drain(&mut cursor).unwrap();
                        let after = costs(&cursor);
                        assert_eq!(
                            std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                            charged
                        );
                        assert_eq!(cursor.value().unwrap().fragment, expected);
                        let saved = costs(&cursor);
                        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                        assert_eq!(costs(&cursor), saved);
                        cursor.finish(Tick(1)).unwrap();
                        ((), true)
                    },
                );
            }
        }
    }
}
#[test]
fn every_capacity_cut_hides_fragment_and_retires_original_parent() {
    let size = exercise(SOURCE, 0, 2, 1024, |mut cursor, _original| {
        drain(&mut cursor).unwrap();
        let size = cursor.value().unwrap().fragment.len();
        cursor.finish(Tick(1)).unwrap();
        (size, true)
    });
    for cap in 0..size {
        exercise(SOURCE, 0, 2, cap, |mut cursor, _original| {
            assert_eq!(drain(&mut cursor), Err(Error::ResponseCapacity));
            assert!(cursor.value().is_none());
            let saved = costs(&cursor);
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::ResponseCapacity)
            );
            assert_eq!(cursor.poll(Tick(1)), Err(Error::ResponseCapacity));
            assert_eq!(costs(&cursor), saved);
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::ResponseCapacity));
            ((), false)
        });
    }
    exercise(SOURCE, 0, 2, 0, |mut cursor, _original| {
        assert_eq!(
            cursor.poll(Tick(100)),
            Err(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
        );
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
        );
        ((), false)
    });
}
#[test]
fn every_retention_prefix_requires_fresh_completion_and_forgetting_retires_parent() {
    let turns = exercise(SOURCE, 0, 2, 1024, |mut cursor, _original| {
        let turns = drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        (turns, true)
    });
    for cut in 0..=turns {
        for trial in 0..4 {
            exercise(SOURCE, 0, 2, 1024, |mut cursor, _original| {
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                if trial == 0 {
                    let error = cursor.check_deadline(Tick(100)).unwrap_err();
                    assert_eq!(
                        error,
                        Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
                    );
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                } else if trial == 1 {
                    assert_eq!(
                        cursor.finish(Tick(100)).err(),
                        Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
                    );
                } else if trial == 2 {
                    if cut == turns {
                        cursor.finish(Tick(1)).unwrap();
                        return ((), true);
                    }
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                } else {
                    forget(cursor);
                }
                ((), false)
            });
        }
    }
}
#[test]
fn original_output_refusal_remains_sticky() {
    exercise(SOURCE, 0, 2, 1024, |mut cursor, _original| {
        let left = cursor.framing.scalars.work.remaining().output_bytes;
        cursor
            .framing
            .scalars
            .work
            .charge(
                Tick(1),
                Charge {
                    output_bytes: left,
                    ..Charge::default()
                },
            )
            .unwrap();
        let error = drain(&mut cursor).unwrap_err();
        assert_eq!(
            error,
            Error::Admission(crate::nfc::Error::Work(Stop::OutputBytes))
        );
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(100)), Err(error));
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        ((), false)
    });
}

#[test]
fn fresh_constructor_keeps_original_parent_and_retention_policy() {
    for late in [false, true] {
        let mut parts = [Descriptor::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut lists = Lists::new();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        let selected = selection(
            SOURCE,
            17,
            &mut parts,
            &mut nodes,
            &mut lists,
            &mut work,
            &mut budget,
        );
        let mut parent = Projecting::new(selected, Tick(1)).unwrap();
        let mut part = parent
            .next(storage.backing(256), &mut scratch, Tick(1))
            .unwrap();
        let mut done = false;
        for _ in 0..100000 {
            if part.poll(Tick(1)).unwrap() == Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        if late {
            assert!(matches!(
                Cursor::new(part, &mut [], Tick(100)).err(),
                Some(Error::Metadata(_))
            ));
        } else {
            let mut cursor = Cursor::new(part, &mut [], Tick(1)).unwrap();
            assert_eq!(cursor.poll(Tick(1)), Err(Error::ResponseCapacity));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::ResponseCapacity));
        }
        let error = parent.completed().unwrap_err();
        assert_eq!(parent.finish(Tick(1)).err(), Some(error));
        assert_eq!(
            work.stopped(),
            if late { Some(Stop::Deadline) } else { None }
        );
    }
}

#[test]
fn escaped_scalars_and_long_raw_runs_match_bare_framer_at_every_capacity() {
    let escaped = concat!(
        "Content-Type: text/plain; charset=UTF-8\r\n",
        "Content-Disposition: attachment; filename*=utf-8''e%CC%81%E4%BE%8B%F0%9F%90%88%22%5Cx\r\n",
        "Content-ID: <i@a>\r\nContent-Language: en, fr\r\nContent-Location: ../x\r\n\r\nabc"
    );
    let long = format!(
        "Content-Type: text/plain\r\nContent-Location: ../{}\r\n\r\nabc",
        "a".repeat(160)
    );
    for source in [escaped.as_bytes(), long.as_bytes()] {
        let oracle = bare(source, 17, 1);
        let size = oracle.0.len();
        for cap in 0..=size {
            exercise(source, 17, 1, cap, |mut cursor, _original| {
                let before = costs(&cursor);
                let result = drain(&mut cursor);
                if cap == size {
                    result.unwrap();
                    assert_eq!(cursor.value().unwrap().fragment, oracle.0);
                    let after = costs(&cursor);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        oracle.1
                    );
                    cursor.finish(Tick(1)).unwrap();
                    ((), true)
                } else {
                    assert_eq!(result, Err(Error::ResponseCapacity));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::ResponseCapacity));
                    ((), false)
                }
            });
        }
        for cap in [1, 64, size - 1] {
            exercise(source, 17, 1, cap, |mut cursor, _original| {
                for _ in 0..10000 {
                    if cursor.window.provisional().unwrap().len() == cap {
                        break;
                    }
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                }
                assert_eq!(cursor.window.provisional().unwrap().len(), cap);
                let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
                assert_eq!(cursor.poll(Tick(100)), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                ((), false)
            });
        }
    }
}
