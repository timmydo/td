#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::super::tests::TestClock;
use super::super::super::super::tests::with_bound;
use super::super::super::tests::{costs as bare_costs, properties};
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    ports::{BlobReader, Clock, Error as PolicyError, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
fn deadline(actual: bool) -> Error {
    if actual {
        Error::Parent(PolicyError::Deadline)
    } else {
        Error::Original(super::super::super::super::super::super::Error::Admission(
            crate::nfc::Error::Work(Stop::Deadline),
        ))
    }
}
fn bound_costs(bound: &Bound<'_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
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
fn drain(child: &mut Child<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>) -> Result<(), Error> {
    for _ in 0..10000 {
        if child.poll(Tick(1))? == Status::Complete {
            return Ok(());
        }
    }
    panic!("selected collection child stalled")
}
fn collect(
    collecting: &mut Collecting<'_, '_, '_, '_, '_, '_, '_, '_, '_, '_, '_>,
) -> Result<(), Error> {
    let total = collecting.total()?;
    for _ in 0..total {
        let mut child = collecting.next(Tick(1))?;
        drain(&mut child)?;
        child.finish(Tick(1))?;
    }
    Ok(())
}
type Turn = (usize, [u64; 5]);
fn expected(source: &[u8], ordinal: u16, bits: u16) -> (Vec<u8>, Vec<Turn>) {
    let clock = TestClock::new();
    with_bound(source, 0, &clock, |bound| {
        let mut cursor =
            super::super::super::Cursor::new(bound, ordinal, properties(bits), Tick(1)).unwrap();
        let mut bytes = Vec::new();
        let mut turns = Vec::new();
        for _ in 0..10000 {
            let before = bare_costs(&cursor.inner);
            let mut out = [0; 64];
            let p = cursor.poll(Tick(1), &mut out).unwrap();
            let after = bare_costs(&cursor.inner);
            turns.push((p.written, std::array::from_fn(|i| before[i] - after[i])));
            bytes.extend_from_slice(&out[..p.written]);
            if p.status == super::super::super::Status::Complete {
                return (bytes, turns);
            }
        }
        panic!("bare selected member stalled")
    })
}
#[test]
fn every_original_ordinal_retains_selected_bytes_costs_backing_and_pin() {
    for (source, count) in [(SIMPLE, 1), (MULTIPART, 2)] {
        for bits in [0, 1, 128, 256, 512, 513, 1023] {
            let expected: Vec<_> = (1..=count)
                .map(|ordinal| expected(source, ordinal as u16, bits))
                .collect();
            for base in [0, 17, u64::MAX - source.len() as u64] {
                let clock = TestClock::new();
                with_bound(source, base, &clock, |bound| {
                    let candidates = bound.value().unwrap().candidates.as_ptr();
                    let whole = bound.value().unwrap().original.members.as_ptr();
                    let before = bound_costs(&bound);
                    let mut backing = vec![[0xa5; 512]; count];
                    let pointers: Vec<_> = backing.iter().map(|bytes| bytes.as_ptr()).collect();
                    let mut cells: Vec<_> = backing
                        .iter_mut()
                        .map(|bytes| Cell::new(if bits == 0 { &mut bytes[..0] } else { bytes }))
                        .collect();
                    let mut collecting =
                        Collecting::new(bound, properties(bits), &mut cells, Tick(1)).unwrap();
                    assert_eq!(collecting.properties(), properties(bits));
                    assert_eq!(collecting.inner.costs(), Some(before));
                    assert_eq!(collecting.total(), Ok(count));
                    let mut total_debits = [0u64; 5];
                    for (index, (bytes, turns)) in expected.iter().enumerate() {
                        assert_eq!(collecting.completed(), Ok(index));
                        let mut child = collecting.next(Tick(1)).unwrap();
                        if bits != 0 {
                            assert!(child.value().is_none());
                        }
                        for (turn, (written, debits)) in turns.iter().enumerate() {
                            let before = child.inner.costs().unwrap();
                            let status = child.poll(Tick(1)).unwrap();
                            let after = child.inner.costs().unwrap();
                            assert_eq!(
                                std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                                *debits
                            );
                            assert_eq!(before[4] - after[4], *written as u64);
                            assert_eq!(
                                status,
                                if turn + 1 == turns.len() {
                                    Status::Complete
                                } else {
                                    Status::Yield
                                }
                            );
                            for i in 0..5 {
                                total_debits[i] += debits[i];
                            }
                        }
                        let value = child.value().unwrap();
                        assert_eq!(value.properties, properties(bits));
                        assert_eq!(value.ordinal, (index + 1) as u16);
                        assert_eq!(value.members, bytes);
                        assert_eq!(value.members.as_ptr(), pointers[index]);
                        let before = child.inner.costs().unwrap();
                        assert_eq!(child.poll(Tick(100)), Ok(Status::Complete));
                        assert_eq!(child.inner.costs(), Some(before));
                        child.finish(Tick(1)).unwrap();
                        let after = collecting.inner.costs().unwrap();
                        assert_eq!(
                            std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                            [0, 1, 0, 2, 0]
                        );
                        total_debits[1] += 1;
                        total_debits[3] += 2;
                        assert_eq!(collecting.completed(), Ok(index + 1));
                    }
                    let serialized = collecting.finish(Tick(1)).unwrap();
                    let value = serialized.value().unwrap();
                    assert_eq!(value.properties, properties(bits));
                    assert_eq!(value.original.original.candidates.as_ptr(), candidates);
                    let (bound, props, cells) = serialized.finish(Tick(1)).unwrap();
                    assert_eq!(props, properties(bits));
                    assert_eq!(bound.value().unwrap().original.members.as_ptr(), whole);
                    let after = bound_costs(&bound);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        total_debits
                    );
                    for (index, cell) in cells.iter().enumerate() {
                        let value = cell.value().unwrap();
                        assert_eq!(value.ordinal, (index + 1) as u16);
                        assert_eq!(value.members, expected[index].0);
                        assert_eq!(value.members.as_ptr(), pointers[index]);
                    }
                    let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                    assert_eq!(parent.read_at(0, &mut [0; 2]).unwrap(), 2);
                    for (bytes, (expected, _)) in backing.iter().zip(expected.iter()) {
                        assert!(bytes[expected.len()..].iter().all(|b| *b == 0xa5));
                    }
                });
            }
        }
    }
}
#[test]
fn all_keeps_legacy_collection_turns_and_per_turn_costs() {
    for (source, count) in [(SIMPLE, 1), (MULTIPART, 2)] {
        let clock = TestClock::new();
        let legacy = with_bound(source, 0, &clock, |bound| {
            let mut backing = vec![[0; 512]; count];
            let mut cells: Vec<_> = backing.iter_mut().map(|b| Cell::new(b)).collect();
            let mut collecting = shared::Collecting::new(bound, &mut cells, Tick(1)).unwrap();
            let mut turns = Vec::new();
            for _ in 0..count {
                let mut child = collecting.next(Tick(1)).unwrap();
                loop {
                    let before = child.costs().unwrap();
                    let p = child.poll(Tick(1)).unwrap();
                    let after = child.costs().unwrap();
                    turns.push((p, std::array::from_fn::<_, 5, _>(|i| before[i] - after[i])));
                    if p == Status::Complete {
                        break;
                    }
                }
                child.finish(Tick(1)).unwrap();
            }
            turns
        });
        with_bound(source, 0, &clock, |bound| {
            let mut backing = vec![[0; 512]; count];
            let mut cells: Vec<_> = backing.iter_mut().map(|b| Cell::new(b)).collect();
            let mut collecting =
                Collecting::new(bound, Properties::ALL, &mut cells, Tick(1)).unwrap();
            let mut turns = legacy.into_iter();
            for _ in 0..count {
                let mut child = collecting.next(Tick(1)).unwrap();
                loop {
                    let (p, debits) = turns.next().unwrap();
                    let before = child.inner.costs().unwrap();
                    assert_eq!(child.poll(Tick(1)), Ok(p));
                    let after = child.inner.costs().unwrap();
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        debits
                    );
                    if p == Status::Complete {
                        break;
                    }
                }
                child.finish(Tick(1)).unwrap();
            }
            assert!(turns.next().is_none());
            assert!(collecting.finish(Tick(1)).unwrap().value().is_some());
        });
    }
}
#[test]
fn cardinality_is_exact_and_both_deadline_domains_precede_capacity() {
    for length in [0, 2] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut backing = vec![[0; 64]; length];
            let mut cells: Vec<_> = backing.iter_mut().map(|b| Cell::new(b)).collect();
            let error = if length == 0 {
                Error::Original(super::super::super::super::super::super::Error::ResponseCapacity)
            } else {
                Error::InvalidState
            };
            assert_eq!(
                Collecting::new(bound, Properties::NONE, &mut cells, Tick(1)).err(),
                Some(error)
            );
        });
    }
    for actual in [false, true] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            assert_eq!(
                Collecting::new(bound, Properties::NONE, &mut [], tick).err(),
                Some(deadline(actual))
            );
        });
    }
    for actual in [false, true] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let tick = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let mut backing = [[0; 64]; 2];
            let mut cells: Vec<_> = backing.iter_mut().map(|b| Cell::new(b)).collect();
            assert_eq!(
                Collecting::new(bound, Properties::NONE, &mut cells, tick).err(),
                Some(deadline(actual))
            );
        });
    }
}
#[test]
fn dropped_child_poisons_parent_even_when_none_is_already_complete() {
    for bits in [0, 512, 1023] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = [0; 512];
            let mut cells = [Cell::new(&mut output)];
            let mut collecting =
                Collecting::new(bound, properties(bits), &mut cells, Tick(1)).unwrap();
            let mut child = collecting.next(Tick(1)).unwrap();
            child.poll(Tick(1)).unwrap();
            drop(child);
            let error = Error::Original(super::super::super::super::super::super::Error::Abandoned);
            assert_eq!(collecting.completed(), Err(error));
            assert_eq!(collecting.check_deadline(Tick(100)), Err(error));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
        });
    }
}
#[test]
fn incomplete_parent_and_premature_child_cannot_form_whole_collection() {
    let clock = TestClock::new();
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut output = [0; 512];
        let mut cells = [Cell::new(&mut output)];
        let collecting = Collecting::new(bound, properties(512), &mut cells, Tick(1)).unwrap();
        assert_eq!(collecting.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
    for prefix in [0, 1, 2] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut output = [0; 512];
            let mut cells = [Cell::new(&mut output)];
            let mut collecting =
                Collecting::new(bound, properties(512), &mut cells, Tick(1)).unwrap();
            let mut child = collecting.next(Tick(1)).unwrap();
            for _ in 0..prefix {
                assert_eq!(child.poll(Tick(1)), Ok(Status::Yield));
            }
            assert_eq!(child.finish(Tick(1)), Err(Error::InvalidState));
            assert_eq!(collecting.completed(), Err(Error::InvalidState));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(Error::InvalidState));
        });
    }
}
#[test]
fn explicit_and_consuming_complete_owners_keep_both_freshness_domains() {
    for bits in [0, 512, 1023] {
        for actual in [false, true] {
            for phase in 0..7 {
                let clock = TestClock::new();
                with_bound(SIMPLE, 0, &clock, |bound| {
                    let mut output = [0; 512];
                    let mut cells = [Cell::new(&mut output)];
                    let mut collecting =
                        Collecting::new(bound, properties(bits), &mut cells, Tick(1)).unwrap();
                    if phase <= 1 {
                        let tick = if actual {
                            clock.expire();
                            Tick(1)
                        } else {
                            Tick(100)
                        };
                        let error = deadline(actual);
                        if phase == 0 {
                            assert_eq!(collecting.next(tick).err(), Some(error));
                        } else {
                            assert_eq!(collecting.check_deadline(tick), Err(error));
                        }
                        assert_eq!(collecting.completed(), Err(error));
                        assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
                        return;
                    }
                    let mut child = collecting.next(Tick(1)).unwrap();
                    if (4..=5).contains(&phase) {
                        drain(&mut child).unwrap();
                    } else if phase == 3 && bits != 0 {
                        assert_eq!(child.poll(Tick(1)), Ok(Status::Yield));
                    }
                    let tick = if actual {
                        clock.expire();
                        Tick(1)
                    } else {
                        Tick(100)
                    };
                    let error = deadline(actual);
                    if phase == 4 {
                        assert_eq!(child.finish(tick), Err(error));
                    } else if phase == 5 || phase == 6 || bits == 0 {
                        assert_eq!(child.check_deadline(tick), Err(error));
                        assert!(child.value().is_none());
                        assert_eq!(child.poll(Tick(1)), Err(error));
                        assert_eq!(child.finish(Tick(1)), Err(error));
                    } else {
                        assert_eq!(child.poll(tick), Err(error));
                        assert!(child.value().is_none());
                        assert_eq!(child.finish(Tick(1)), Err(error));
                    }
                    assert_eq!(collecting.completed(), Err(error));
                    assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
                });
            }
        }
    }
    for quota in [false, true] {
        let clock = FenceClock {
            calls: AtomicU64::new(0),
            expire_at: AtomicU64::new(u64::MAX),
        };
        with_bound(SIMPLE, 0, &clock, |bound| {
            if quota {
                let work = &mut *bound
                    .original
                    .source
                    .original
                    .source
                    .projected
                    .structure
                    .work;
                work.charge(
                    Tick(1),
                    Charge {
                        records: work.remaining().records,
                        ..Charge::default()
                    },
                )
                .unwrap();
            }
            let mut cells = [Cell::new(&mut [])];
            let mut collecting =
                Collecting::new(bound, Properties::NONE, &mut cells, Tick(1)).unwrap();
            let child = collecting.next(Tick(1)).unwrap();
            let before = clock.calls.load(Ordering::SeqCst);
            clock.expire_at.store(before + 9, Ordering::SeqCst);
            let error = Error::Parent(PolicyError::Deadline);
            assert_eq!(child.finish(Tick(1)), Err(error));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 9);
            assert_eq!(collecting.completed(), Err(error));
            clock.expire_at.store(u64::MAX, Ordering::SeqCst);
            assert_eq!(collecting.next(Tick(1)).err(), Some(error));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
            assert_eq!(clock.calls.load(Ordering::SeqCst), before + 9);
            assert!(cells[0].value().is_none());
        });
    }
    for bits in [0, 512, 1023] {
        for actual in [false, true] {
            for phase in 0..4 {
                let clock = TestClock::new();
                with_bound(SIMPLE, 0, &clock, |bound| {
                    let mut output = [0; 512];
                    let mut cells = [Cell::new(&mut output)];
                    let mut collecting =
                        Collecting::new(bound, properties(bits), &mut cells, Tick(1)).unwrap();
                    collect(&mut collecting).unwrap();
                    if phase == 0 {
                        let tick = if actual {
                            clock.expire();
                            Tick(1)
                        } else {
                            Tick(100)
                        };
                        assert_eq!(collecting.finish(tick).err(), Some(deadline(actual)));
                    } else {
                        let mut serialized = collecting.finish(Tick(1)).unwrap();
                        if phase == 1 {
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            let error = deadline(actual);
                            assert_eq!(serialized.check_deadline(tick), Err(error));
                            assert!(serialized.value().is_none());
                            assert_eq!(serialized.finish(Tick(1)).err(), Some(error));
                        } else if phase == 2 {
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            assert_eq!(serialized.finish(tick).err(), Some(deadline(actual)));
                        } else {
                            let (mut bound, _, _) = serialized.finish(Tick(1)).unwrap();
                            let tick = if actual {
                                clock.expire();
                                Tick(1)
                            } else {
                                Tick(100)
                            };
                            assert_eq!(bound.check_deadline(tick), Err(deadline(actual)));
                        }
                    }
                });
            }
        }
    }
}
#[test]
fn short_nonempty_child_window_refusal_propagates_exactly_to_parent() {
    for bits in [512, 513, 1023] {
        for capacity in [0, 1, 63, 64] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut output = [0; 64];
                let mut cells = [Cell::new(&mut output[..capacity])];
                let mut collecting =
                    Collecting::new(bound, properties(bits), &mut cells, Tick(1)).unwrap();
                let mut child = collecting.next(Tick(1)).unwrap();
                let error = Error::Original(
                    super::super::super::super::super::super::Error::ResponseCapacity,
                );
                assert_eq!(drain(&mut child), Err(error));
                assert!(child.value().is_none());
                assert_eq!(child.poll(Tick(1)), Err(error));
                assert_eq!(child.finish(Tick(1)), Err(error));
                assert_eq!(collecting.completed(), Err(error));
                assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}
#[test]
fn none_still_pays_original_ordinal_handoff_and_refuses_its_quotas() {
    for kind in [0, 1] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |mut bound| {
            let s = &mut bound.original.source.original.source.projected.structure;
            let expected = if kind == 0 {
                let left = s.budget.steps_remaining();
                s.budget.charge(s.work, Tick(1), 0, left, &mut 0).unwrap();
                crate::nfc::Error::InterpretationLimit
            } else {
                let left = s.work.remaining().records;
                s.work
                    .charge(
                        Tick(1),
                        Charge {
                            records: left,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
                crate::nfc::Error::Work(Stop::Records)
            };
            let error = Error::Original(
                super::super::super::super::super::super::Error::Admission(expected),
            );
            let mut cells = [Cell::new(&mut [])];
            let mut collecting =
                Collecting::new(bound, Properties::NONE, &mut cells, Tick(1)).unwrap();
            let mut child = collecting.next(Tick(1)).unwrap();
            let before = child.inner.costs().unwrap();
            assert_eq!(child.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(child.inner.costs(), Some(before));
            assert_eq!(child.finish(Tick(1)), Err(error));
            assert_eq!(collecting.completed(), Err(error));
            assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
        });
    }
}

/// All original matching, generated fragments, file and cell setup stay cold.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let source = if trial == 0 { MULTIPART } else { SIMPLE };
        let count = if trial == 0 { 2 } else { 1 };
        let base = if trial == 0 {
            u64::MAX - source.len() as u64
        } else {
            0
        };
        let clock = TestClock::new();
        with_bound(source, base, &clock, |bound| {
            let mut backing = vec![[0xa5; 512]; count];
            let length = if trial == 1 || trial == 6 { 0 } else { 512 };
            let mut cells: Vec<_> = backing
                .iter_mut()
                .map(|b| Cell::new(&mut b[..length]))
                .collect();
            let props = properties(if trial == 1 {
                0
            } else if trial == 0 {
                513
            } else {
                512
            });
            if trial == 7 {
                clock.expire();
            }
            snapshot();
            if trial == 7 {
                assert_eq!(
                    Collecting::new(bound, props, &mut cells, Tick(1)).err(),
                    Some(Error::Parent(PolicyError::Deadline))
                );
            } else {
                let mut collecting = Collecting::new(bound, props, &mut cells, Tick(1)).unwrap();
                if trial == 2 || trial == 6 {
                    let mut child = collecting.next(Tick(1)).unwrap();
                    let error = if trial == 6 {
                        Error::Original(
                            super::super::super::super::super::super::Error::ResponseCapacity,
                        )
                    } else {
                        loop {
                            let before = child.inner.costs().unwrap();
                            assert_eq!(child.poll(Tick(1)), Ok(Status::Yield));
                            if child.inner.costs().unwrap()[4] < before[4] {
                                break;
                            }
                        }
                        clock.expire();
                        Error::Parent(PolicyError::Deadline)
                    };
                    assert_eq!(child.poll(Tick(1)), Err(error));
                    assert_eq!(child.finish(Tick(1)), Err(error));
                    assert_eq!(collecting.finish(Tick(1)).err(), Some(error));
                } else {
                    collect(&mut collecting).unwrap();
                    if trial == 3 {
                        clock.expire();
                        assert_eq!(
                            collecting.finish(Tick(1)).err(),
                            Some(Error::Parent(PolicyError::Deadline))
                        );
                    } else {
                        let serialized = collecting.finish(Tick(1)).unwrap();
                        assert_eq!(serialized.value().unwrap().properties, props);
                        if trial == 4 {
                            clock.expire();
                            assert_eq!(
                                serialized.finish(Tick(1)).err(),
                                Some(Error::Parent(PolicyError::Deadline))
                            );
                        } else {
                            let (mut bound, actual, cells) = serialized.finish(Tick(1)).unwrap();
                            assert_eq!(actual, props);
                            assert_eq!(cells.len(), count);
                            if trial == 5 {
                                clock.expire();
                                assert_eq!(
                                    bound.check_deadline(Tick(1)),
                                    Err(Error::Parent(PolicyError::Deadline))
                                );
                            }
                            drop(bound);
                        }
                    }
                }
            }
            snapshot();
        });
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
