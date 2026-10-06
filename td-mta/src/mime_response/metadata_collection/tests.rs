#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::mime_response::metadata::tests::with_bound;
use crate::mime_response::metadata_collection::*;
use crate::mime_response::pinned::tests::TestClock;
use crate::ports::{BlobReader, Clock, Error as PolicyError, Time};
use std::sync::atomic::{AtomicU64, Ordering};
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
fn deadline(actual: bool) -> Error {
    if actual {
        Error::Parent(PolicyError::Deadline)
    } else {
        Error::Original(OriginalError::Admission(crate::nfc::Error::Work(
            crate::admission::work::Stop::Deadline,
        )))
    }
}
fn drain(child: &mut Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..10000 {
        if child.poll(Tick(1))? == RetainStatus::Complete {
            return Ok(());
        }
    }
    panic!("collection child stalled")
}
fn costs(bound: &Bound<'_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let s = &bound.original.source.original.source.projected.structure;
    let w = s.work.remaining();
    [
        s.budget.source_bytes_remaining(),
        s.budget.steps_remaining(),
        w.io_bytes,
        w.records,
        w.output_bytes,
    ]
}
#[test]
fn all_original_ordinals_match_original_fragment_suffix_backing_and_pin() {
    for (source, count) in [(SIMPLE, 1), (MULTIPART, 2)] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            let clock = TestClock::new();
            with_bound(source, base, &clock, |bound| {
                let original = bound.value().unwrap();
                let candidates = original.candidates.as_ptr();
                let whole = original.original.members.as_ptr();
                let source_ptr = bound
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure
                    .source
                    .as_ptr();
                let expected: Vec<Vec<u8>> = original
                    .original
                    .original
                    .fragments
                    .iter()
                    .zip(original.candidates.iter())
                    .map(|(cell, candidate)| {
                        let fragment = cell.value().unwrap().fragment;
                        let suffix = if let Some(wire) = candidate.wire() {
                            format!(",\"blobId\":\"{wire}\"")
                        } else {
                            ",\"blobId\":null".into()
                        };
                        [fragment, suffix.as_bytes()].concat()
                    })
                    .collect();
                let mut backing = vec![[0xa5; 512]; count];
                let identities: Vec<_> = backing.iter().map(|bytes| bytes.as_ptr()).collect();
                let mut cells: Vec<_> = backing.iter_mut().map(|bytes| Cell::new(bytes)).collect();
                let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
                assert_eq!(collecting.total(), Ok(count));
                for index in 0..count {
                    assert_eq!(collecting.completed(), Ok(index));
                    let mut child = collecting.next(Tick(1)).unwrap();
                    assert!(child.value().is_none());
                    drain(&mut child).unwrap();
                    assert_eq!(child.value().unwrap().ordinal, (index + 1) as u16);
                    assert_eq!(child.value().unwrap().members, expected[index]);
                    let before = costs(&child.cursor.as_ref().unwrap().emitter.source);
                    assert_eq!(child.poll(Tick(100)), Ok(RetainStatus::Complete));
                    assert_eq!(
                        costs(&child.cursor.as_ref().unwrap().emitter.source),
                        before
                    );
                    child.finish(Tick(1)).unwrap();
                    let after = costs(collecting.source.as_ref().unwrap());
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        [0, 1, 0, 2, 0]
                    );
                    assert_eq!(
                        collecting.cells[index].value().unwrap().members.as_ptr(),
                        identities[index]
                    );
                    assert_eq!(collecting.completed(), Ok(index + 1));
                }
                let mut serialized = collecting.finish(Tick(1)).unwrap();
                serialized.check_deadline(Tick(1)).unwrap();
                assert_eq!(
                    serialized.value().unwrap().original.candidates.as_ptr(),
                    candidates
                );
                let (bound, cells) = serialized.finish(Tick(1)).unwrap();
                assert_eq!(bound.value().unwrap().original.members.as_ptr(), whole);
                assert_eq!(
                    bound
                        .original
                        .source
                        .original
                        .source
                        .projected
                        .structure
                        .source
                        .as_ptr(),
                    source_ptr
                );
                for (cell, expected) in cells.iter().zip(expected.iter()) {
                    assert_eq!(cell.value().unwrap().members, expected);
                }
                let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                for (bytes, expected) in backing.iter().zip(expected.iter()) {
                    assert!(bytes[expected.len()..].iter().all(|b| *b == 0xa5));
                }
            });
        }
    }
}
#[test]
fn constructor_checks_both_domains_before_cell_count() {
    for count in [0, 1, 2] {
        for mode in 0..3 {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut backing = [[0; 512]; 2];
                let mut cells: Vec<_> = backing[..count].iter_mut().map(|b| Cell::new(b)).collect();
                let now = if mode == 1 { Tick(100) } else { Tick(1) };
                if mode == 2 {
                    clock.expire();
                }
                let result = Collecting::new(bound, &mut cells, now);
                if mode != 0 {
                    assert_eq!(result.err(), Some(deadline(mode == 2)));
                } else if count == 0 {
                    assert_eq!(
                        result.err(),
                        Some(Error::Original(OriginalError::ResponseCapacity))
                    );
                } else if count == 2 {
                    assert_eq!(result.err(), Some(Error::InvalidState));
                } else {
                    assert!(result.is_ok());
                }
            });
        }
    }
}
#[test]
fn abandonment_and_premature_child_finish_poison_parent() {
    for mode in 0..4 {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut backing = [0; 512];
            let mut cells = [Cell::new(&mut backing)];
            let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
            let mut child = collecting.next(Tick(1)).unwrap();
            if mode == 1 {
                drain(&mut child).unwrap();
            }
            let error = if mode == 2 {
                assert_eq!(child.finish(Tick(1)), Err(Error::InvalidState));
                Error::InvalidState
            } else {
                if mode == 3 {
                    std::mem::forget(child);
                } else {
                    drop(child);
                }
                Error::Original(OriginalError::Abandoned)
            };
            assert_eq!(collecting.completed(), Err(error));
            assert_eq!(collecting.total(), Err(error));
            assert_eq!(collecting.next(Tick(1)).err(), Some(error));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
            assert!(cells[0].value().is_none());
        });
    }
}
#[test]
fn partial_parent_cannot_finish_and_used_cells_cannot_renew_collection() {
    let clock = TestClock::new();
    with_bound(MULTIPART, 0, &clock, |bound| {
        let mut backing = [[0; 512]; 2];
        let mut cells: Vec<_> = backing.iter_mut().map(|b| Cell::new(b)).collect();
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        let mut child = collecting.next(Tick(1)).unwrap();
        drain(&mut child).unwrap();
        child.finish(Tick(1)).unwrap();
        assert_eq!(collecting.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
    let clock = TestClock::new();
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut backing = [0; 512];
        let mut cells = [Cell::new(&mut backing)];
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        let mut child = collecting.next(Tick(1)).unwrap();
        drain(&mut child).unwrap();
        child.finish(Tick(1)).unwrap();
        let (bound, released) = collecting.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
        assert!(released[0].value().is_some());
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        assert_eq!(collecting.next(Tick(1)).err(), Some(Error::InvalidState));
        assert_eq!(collecting.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
}
#[test]
fn abandoned_empty_cells_refuse_a_fresh_original_owner() {
    let clock = TestClock::new();
    let mut backing = [0; 512];
    let mut cells = [Cell::new(&mut backing)];
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        drop(collecting.next(Tick(1)).unwrap());
        assert_eq!(
            collecting.finish(Tick(1)).err(),
            Some(Error::Original(OriginalError::Abandoned))
        );
    });
    assert!(cells[0].output.is_none());
    assert!(cells[0].value().is_none());
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        assert_eq!(collecting.next(Tick(1)).err(), Some(Error::InvalidState));
        assert_eq!(collecting.next(Tick(1)).err(), Some(Error::InvalidState));
        assert_eq!(collecting.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
}

#[test]
fn child_capacity_refusal_and_acceptance_quotas_propagate() {
    for kind in 0..5 {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut backing = [0xa5; 512];
            let mut cells = [Cell::new(&mut backing[..if kind == 0 { 8 } else { 512 }])];
            let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
            let mut child = collecting.next(Tick(1)).unwrap();
            let error = if kind == 0 {
                let e = Error::Original(OriginalError::ResponseCapacity);
                assert_eq!(drain(&mut child), Err(e));
                assert!(child.value().is_none());
                assert_eq!(child.poll(Tick(1)), Err(e));
                assert_eq!(child.finish(Tick(1)), Err(e));
                e
            } else {
                drain(&mut child).unwrap();
                let structure = &mut child
                    .cursor
                    .as_mut()
                    .unwrap()
                    .emitter
                    .source
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure;
                let left = structure.work.remaining();
                if kind == 3 {
                    let steps = structure.budget.steps_remaining();
                    structure
                        .budget
                        .charge(
                            structure.work,
                            Tick(1),
                            0,
                            steps,
                            &mut crate::nfc::Credit::new(),
                        )
                        .unwrap();
                } else {
                    structure
                        .work
                        .charge(
                            Tick(1),
                            Charge {
                                records: if kind == 2 {
                                    left.records
                                } else if kind == 4 {
                                    left.records - 1
                                } else {
                                    0
                                },
                                output_bytes: if kind == 1 { left.output_bytes } else { 0 },
                                ..Charge::default()
                            },
                        )
                        .unwrap();
                }
                if kind == 1 {
                    child.finish(Tick(1)).unwrap();
                    assert_eq!(collecting.completed(), Ok(1));
                    return;
                }
                let nfc = if kind == 3 {
                    crate::nfc::Error::InterpretationLimit
                } else {
                    crate::nfc::Error::Work(crate::admission::work::Stop::Records)
                };
                let e = Error::Original(OriginalError::Admission(nfc));
                assert_eq!(child.finish(Tick(1)), Err(e));
                e
            };
            assert_eq!(collecting.completed(), Err(error));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
            assert!(cells[0].value().is_none());
        });
    }
}
#[test]
fn all_collection_and_child_boundaries_remain_fresh() {
    for phase in 0..7 {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut backing = [0; 512];
                let mut cells = [Cell::new(&mut backing)];
                let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
                if phase == 0 {
                    if actual {
                        clock.expire();
                    }
                    let tick = if actual { Tick(1) } else { Tick(100) };
                    assert_eq!(collecting.next(tick).err(), Some(deadline(actual)));
                    assert_eq!(collecting.finish(Tick(1)).err(), Some(deadline(actual)));
                    return;
                }
                let mut child = collecting.next(Tick(1)).unwrap();
                if phase >= 3 {
                    drain(&mut child).unwrap();
                } else if phase == 2 {
                    child.poll(Tick(1)).unwrap();
                }
                if phase <= 3 {
                    if actual {
                        clock.expire();
                    }
                    let tick = if actual { Tick(1) } else { Tick(100) };
                    let e = deadline(actual);
                    if phase == 3 {
                        assert_eq!(child.finish(tick), Err(e));
                    } else {
                        assert_eq!(child.poll(tick), Err(e));
                        assert!(child.value().is_none());
                        assert_eq!(child.finish(Tick(1)), Err(e));
                    }
                    assert_eq!(collecting.finish(Tick(1)).err(), Some(e));
                    return;
                }
                child.finish(Tick(1)).unwrap();
                if phase == 4 {
                    if actual {
                        clock.expire();
                    }
                    let tick = if actual { Tick(1) } else { Tick(100) };
                    assert_eq!(collecting.finish(tick).err(), Some(deadline(actual)));
                    return;
                }
                let mut serialized = collecting.finish(Tick(1)).unwrap();
                if actual {
                    clock.expire();
                }
                let tick = if actual { Tick(1) } else { Tick(100) };
                if phase == 5 {
                    assert_eq!(serialized.check_deadline(tick), Err(deadline(actual)));
                    assert!(serialized.value().is_none());
                    assert_eq!(serialized.finish(Tick(1)).err(), Some(deadline(actual)));
                } else {
                    assert_eq!(serialized.finish(tick).err(), Some(deadline(actual)));
                }
            });
        }
    }
}
struct FenceClock {
    calls: AtomicU64,
    expire_at: AtomicU64,
}
impl Clock for FenceClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if n >= self.expire_at.load(Ordering::SeqCst) {
                u64::MAX
            } else {
                1
            }),
        })
    }
}
#[test]
fn post_acceptance_actual_pin_fence_wins_even_when_original_quota_refuses() {
    for quota in [false, true] {
        let clock = FenceClock {
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        };
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut backing = [0; 512];
            let mut cells = [Cell::new(&mut backing)];
            let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
            let mut child = collecting.next(Tick(1)).unwrap();
            drain(&mut child).unwrap();
            if quota {
                let work = &mut child
                    .cursor
                    .as_mut()
                    .unwrap()
                    .emitter
                    .source
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure
                    .work;
                let left = work.remaining().records;
                work.charge(
                    Tick(1),
                    Charge {
                        records: left,
                        ..Charge::default()
                    },
                )
                .unwrap();
            }
            let before = clock.calls.load(Ordering::SeqCst);
            // Four fresh pin admissions each sample before and after; expire
            // at the first sample of the final post-acceptance fence.
            clock.expire_at.store(before + 9, Ordering::SeqCst);
            let e = Error::Parent(PolicyError::Deadline);
            assert_eq!(child.finish(Tick(1)), Err(e));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 9);
            assert_eq!(collecting.completed(), Err(e));
            clock.expire_at.store(u64::MAX, Ordering::SeqCst);
            assert_eq!(collecting.next(Tick(1)).err(), Some(e));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(e));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 9);
            assert!(cells[0].value().is_none());
        });
    }
}
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let source = if trial == 1 { MULTIPART } else { SIMPLE };
        let clock = TestClock::new();
        let base = if trial == 0 {
            u64::MAX - source.len() as u64
        } else {
            0
        };
        with_bound(source, base, &clock, |bound| {
            let mut backing = [[0; 512]; 2];
            let mut cells: Vec<_> = backing[..if trial == 1 { 2 } else { 1 }]
                .iter_mut()
                .map(|b| Cell::new(b))
                .collect();
            if trial == 7 {
                clock.expire();
            }
            snapshot();
            match Collecting::new(bound, &mut cells, Tick(1)) {
                Err(e) => {
                    assert_eq!(trial, 7);
                    assert_eq!(e, Error::Parent(PolicyError::Deadline));
                }
                Ok(mut collecting) => {
                    if trial == 6 {
                        let child = collecting.next(Tick(1)).unwrap();
                        drop(child);
                        assert_eq!(
                            collecting.finish(Tick(1)).err(),
                            Some(Error::Original(OriginalError::Abandoned))
                        );
                    } else {
                        while collecting.completed().unwrap() < collecting.total().unwrap() {
                            let mut child = collecting.next(Tick(1)).unwrap();
                            if trial == 2 {
                                child.poll(Tick(1)).unwrap();
                                clock.expire();
                                assert_eq!(
                                    child.poll(Tick(1)),
                                    Err(Error::Parent(PolicyError::Deadline))
                                );
                                drop(child);
                                break;
                            }
                            drain(&mut child).unwrap();
                            if trial == 3 {
                                clock.expire();
                                assert_eq!(
                                    child.finish(Tick(1)),
                                    Err(Error::Parent(PolicyError::Deadline))
                                );
                                break;
                            }
                            child.finish(Tick(1)).unwrap();
                        }
                        if trial == 2 || trial == 3 {
                            assert_eq!(
                                collecting.finish(Tick(1)).err(),
                                Some(Error::Parent(PolicyError::Deadline))
                            );
                        } else if trial == 4 {
                            clock.expire();
                            assert_eq!(
                                collecting.finish(Tick(1)).err(),
                                Some(Error::Parent(PolicyError::Deadline))
                            );
                        } else {
                            let serialized = collecting.finish(Tick(1)).unwrap();
                            if trial == 5 {
                                clock.expire();
                                assert_eq!(
                                    serialized.finish(Tick(1)).err(),
                                    Some(Error::Parent(PolicyError::Deadline))
                                );
                            } else {
                                let (bound, cells) = serialized.finish(Tick(1)).unwrap();
                                assert_eq!(cells.len(), if trial == 1 { 2 } else { 1 });
                                drop(bound);
                            }
                        }
                    }
                }
            }
            snapshot();
        });
    }
}

#[test]
fn next_past_last_original_cell_is_sticky_invalid_state() {
    let clock = TestClock::new();
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut backing = [0; 512];
        let mut cells = [Cell::new(&mut backing)];
        let mut collecting = Collecting::new(bound, &mut cells, Tick(1)).unwrap();
        let mut child = collecting.next(Tick(1)).unwrap();
        drain(&mut child).unwrap();
        child.finish(Tick(1)).unwrap();
        assert_eq!(collecting.next(Tick(1)).err(), Some(Error::InvalidState));
        assert_eq!(collecting.next(Tick(1)).err(), Some(Error::InvalidState));
        assert_eq!(collecting.completed(), Err(Error::InvalidState));
        assert_eq!(collecting.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
}
