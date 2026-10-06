#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::{
    super::tests::{Lists, ALTERNATIVE},
    tests::selection,
};
use super::*;
use crate::{
    admission::work::Stop,
    mime_body_lists::Node,
    mime_traversal::{
        bound::ordered::tests::{meter, Storage, SOURCE},
        Part as Descriptor,
    },
};
fn forget<T>(value: T) {
    std::mem::forget(value);
}
pub(super) fn exercise<T>(
    source: &[u8],
    base: u64,
    f: impl FnOnce(Collecting<'_, '_, '_, '_, '_>, &mut Storage, &mut Scratch) -> T,
) -> T {
    let mut parts = [Descriptor::default(); 8];
    let mut nodes = [Node::default(); 8];
    let mut lists = Lists::new();
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut storage = Storage::new();
    let mut scratch = Scratch::new();
    let original = (
        std::ptr::from_mut(&mut work),
        std::ptr::from_mut(&mut budget),
    );
    let original_tables = (parts.as_ptr(), nodes.as_ptr());
    let mut outputs = [[0xa5; 512]; 8];
    let original_windows = outputs.each_ref().map(|o| o.as_ptr());
    let mut cells = outputs.each_mut().map(|o| Cell::new(o));
    let selected = selection(
        source,
        base,
        &mut parts,
        &mut nodes,
        &mut lists,
        &mut work,
        &mut budget,
    );
    let selected_lists = selected.value().unwrap().lists;
    let original_lists = (
        selected_lists.text.as_ptr(),
        selected_lists.html.as_ptr(),
        selected_lists.attachments.as_ptr(),
    );
    let total = selected.value().unwrap().structure.parts.len();
    let owner = Collecting::new(selected, &mut cells[..total], Tick(1)).unwrap();
    assert_eq!(
        (
            std::ptr::from_mut(owner.projecting.structure.work),
            std::ptr::from_mut(owner.projecting.structure.budget)
        ),
        original
    );
    assert_eq!(
        (
            owner.projecting.structure.parts().unwrap().as_ptr(),
            owner.projecting.nodes.as_ptr()
        ),
        original_tables
    );
    assert_eq!(
        (
            owner.projecting.lists.text.as_ptr(),
            owner.projecting.lists.html.as_ptr(),
            owner.projecting.lists.attachments.as_ptr()
        ),
        original_lists
    );
    for (cell, identity) in owner.cells.iter().zip(original_windows) {
        assert_eq!(cell.output.as_ref().unwrap().as_ptr(), identity);
    }
    f(owner, &mut storage, &mut scratch)
}
pub(super) fn drain(child: &mut Child<'_, '_, '_>) -> usize {
    for turn in 1..100000 {
        if child.poll(Tick(1)).unwrap() == Status::Complete {
            return turn;
        }
    }
    panic!("collection child did not finish")
}
fn bare(source: &[u8], base: u64) -> (Vec<Vec<u8>>, [u64; 5]) {
    exercise(source, base, |owner, storage, scratch| {
        let before = snapshot(&owner);
        let mut projecting = owner.projecting;
        let mut fragments = Vec::new();
        let mut output = [0; 512];
        for _ in 0..projecting.total().unwrap() {
            let mut part = projecting
                .next(storage.backing(256), scratch, Tick(1))
                .unwrap();
            let mut done = false;
            for _ in 0..100000 {
                if part.poll(Tick(1)).unwrap() == Status::Complete {
                    done = true;
                    break;
                }
            }
            assert!(done);
            let mut framing =
                super::super::json::retained::Cursor::new(part, &mut output, Tick(1)).unwrap();
            done = false;
            for _ in 0..100000 {
                if framing.poll(Tick(1)).unwrap() == Status::Complete {
                    done = true;
                    break;
                }
            }
            assert!(done);
            let (retained, _, _, _) = framing.finish(Tick(1)).unwrap();
            fragments.push(retained.fragment.to_vec());
        }
        let (_, work, budget) = projecting.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
        let left = work.remaining();
        let after = [
            budget.source_bytes_remaining(),
            budget.steps_remaining(),
            left.io_bytes,
            left.records,
            left.output_bytes,
        ];
        (fragments, std::array::from_fn(|i| before[i] - after[i]))
    })
}
#[test]
fn whole_original_collection_preserves_preorder_backing_and_owner_release() {
    for source in [SOURCE, ALTERNATIVE] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            let oracle = bare(source, base);
            exercise(source, base, |mut owner, storage, scratch| {
                let before = snapshot(&owner);
                let total = owner.total().unwrap();
                let tables = (
                    owner.projecting.structure.parts().unwrap().as_ptr(),
                    owner.projecting.nodes.as_ptr(),
                );
                let list_pointers = (
                    owner.projecting.lists.text.as_ptr(),
                    owner.projecting.lists.html.as_ptr(),
                    owner.projecting.lists.attachments.as_ptr(),
                );
                let pointers = (
                    std::ptr::from_mut(owner.projecting.structure.work),
                    std::ptr::from_mut(owner.projecting.structure.budget),
                );
                for index in 0..total {
                    let identity = owner.cells[index].output.as_ref().unwrap().as_ptr();
                    let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
                    assert!(child.value().is_none());
                    drain(&mut child);
                    assert_eq!(child.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(child.value().unwrap().end.part.ordinal as usize, index + 1);
                    assert_eq!(child.value().unwrap().fragment.as_ptr(), identity);
                    child.finish(Tick(1)).unwrap();
                    assert_eq!(owner.completed(), Ok(index + 1));
                }
                let serialized = owner.finish(Tick(1)).unwrap();
                let view = serialized.value().unwrap();
                assert_eq!(
                    (
                        view.selected.structure.parts.as_ptr(),
                        view.selected.structure.nodes.as_ptr()
                    ),
                    tables
                );
                assert_eq!(
                    (
                        view.selected.lists.text.as_ptr(),
                        view.selected.lists.html.as_ptr(),
                        view.selected.lists.attachments.as_ptr()
                    ),
                    list_pointers
                );
                assert_eq!(view.fragments.len(), total);
                for (index, cell) in view.fragments.iter().enumerate() {
                    let fragment = cell.value().unwrap();
                    assert_eq!(fragment.fragment, oracle.0[index]);
                    assert_eq!(fragment.end.part, view.selected.structure.parts[index]);
                    assert_eq!(fragment.end.node, view.selected.structure.nodes[index]);
                    assert!(fragment.fragment.starts_with(b"\"partId\":"));
                }
                let (view, work, budget) = serialized.finish(Tick(1)).unwrap();
                assert_eq!(view.fragments.len(), total);
                let left = work.remaining();
                let after = [
                    budget.source_bytes_remaining(),
                    budget.steps_remaining(),
                    left.io_bytes,
                    left.records,
                    left.output_bytes,
                ];
                let charged = std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]);
                let mut expected = oracle.1;
                expected[1] += 2 * total as u64;
                expected[3] += 3 * total as u64;
                assert_eq!(charged, expected);
                assert_eq!(
                    (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                    pointers
                );
            });
        }
    }
}
#[test]
fn every_child_prefix_late_premature_and_forgotten_retires_whole_collection() {
    let turns = exercise(SOURCE, 17, |mut owner, storage, scratch| {
        let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
        drain(&mut child)
    });
    for prefix in 0..=turns {
        for fault in 0..4 {
            exercise(SOURCE, 17, |mut owner, storage, scratch| {
                let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
                for _ in 0..prefix {
                    child.poll(Tick(1)).unwrap();
                }
                let error = match fault {
                    0 => child.finish(Tick(100)).unwrap_err(),
                    1 => {
                        let error = child.check_deadline(Tick(100)).unwrap_err();
                        assert!(child.value().is_none());
                        assert_eq!(child.poll(Tick(1)), Err(error));
                        assert_eq!(child.check_deadline(Tick(1)), Err(error));
                        assert_eq!(child.finish(Tick(1)).err(), Some(error));
                        error
                    }
                    2 => {
                        forget(child);
                        Error::Abandoned
                    }
                    _ => {
                        if prefix == turns {
                            child.finish(Tick(1)).unwrap();
                            assert_eq!(owner.completed(), Ok(1));
                            return;
                        }
                        assert_eq!(child.finish(Tick(1)).err(), Some(Error::InvalidState));
                        Error::InvalidState
                    }
                };
                assert_eq!(owner.completed().err(), Some(error));
                assert_eq!(owner.check_deadline(Tick(1)), Err(error));
                assert_eq!(
                    owner.next(storage.backing(256), scratch, Tick(1)).err(),
                    Some(error)
                );
                assert_eq!(owner.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}
#[test]
fn incomplete_extra_part_capacity_and_consumed_cell_are_sticky() {
    exercise(SOURCE, 0, |owner, _, _| {
        assert_eq!(owner.finish(Tick(1)).err(), Some(Error::InvalidState))
    });
    exercise(SOURCE, 0, |mut owner, storage, scratch| {
        owner.cells[0].output = Some(&mut []);
        let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
        let mut refusal = None;
        for _ in 0..100000 {
            if let Err(e) = child.poll(Tick(1)) {
                refusal = Some(e);
                break;
            }
        }
        assert_eq!(refusal, Some(Error::ResponseCapacity));
        assert!(child.value().is_none());
        assert_eq!(child.finish(Tick(1)).err(), refusal);
        assert_eq!(owner.completed().err(), refusal);
    });
    exercise(SOURCE, 0, |mut owner, storage, scratch| {
        owner.cells[0].output = None;
        assert_eq!(
            owner.next(storage.backing(256), scratch, Tick(1)).err(),
            Some(Error::InvalidState)
        );
        assert_eq!(owner.completed().err(), Some(Error::InvalidState));
    });
    exercise(SOURCE, 0, |mut owner, storage, scratch| {
        for _ in 0..owner.total().unwrap() {
            let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
            drain(&mut child);
            child.finish(Tick(1)).unwrap();
        }
        assert_eq!(
            owner.next(storage.backing(256), scratch, Tick(1)).err(),
            Some(Error::InvalidState)
        );
        assert_eq!(owner.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
}
#[test]
fn completed_serialized_owner_stays_fresh_and_constructor_admits_before_capacity() {
    for fault in 0..3 {
        exercise(SOURCE, 0, |mut owner, storage, scratch| {
            for _ in 0..owner.total().unwrap() {
                let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
                drain(&mut child);
                child.finish(Tick(1)).unwrap();
            }
            let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
            if fault == 0 {
                assert_eq!(owner.finish(Tick(100)).err(), Some(error));
                return;
            }
            let mut serialized = owner.finish(Tick(1)).unwrap();
            assert!(serialized.value().is_some());
            if fault == 1 {
                assert_eq!(serialized.finish(Tick(100)).err(), Some(error));
                return;
            }
            assert_eq!(serialized.check_deadline(Tick(100)), Err(error));
            assert!(serialized.value().is_none());
            assert_eq!(serialized.finish(Tick(1)).err(), Some(error));
        });
    }
    for late in [false, true] {
        let mut parts = [Descriptor::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut lists = Lists::new();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let selected = selection(
            SOURCE,
            0,
            &mut parts,
            &mut nodes,
            &mut lists,
            &mut work,
            &mut budget,
        );
        let error =
            Collecting::new(selected, &mut [], if late { Tick(100) } else { Tick(1) }).err();
        assert_eq!(
            error,
            Some(if late {
                Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
            } else {
                Error::ResponseCapacity
            })
        );
        assert_eq!(
            work.stopped(),
            if late { Some(Stop::Deadline) } else { None }
        );
    }
}

fn snapshot(owner: &Collecting<'_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &owner.projecting.structure;
    let left = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
#[test]
fn every_original_quota_cutoff_includes_slot_consumption_and_is_sticky() {
    let used = exercise(SOURCE, 0, |mut owner, storage, scratch| {
        let before = snapshot(&owner);
        let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
        drain(&mut child);
        child.finish(Tick(1)).unwrap();
        let after = snapshot(&owner);
        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i])
    });
    for (kind, &used_value) in used.iter().enumerate() {
        for cut in 0..=used_value {
            exercise(SOURCE, 0, |mut owner, storage, scratch| {
                let structure = &mut owner.projecting.structure;
                if kind < 2 {
                    let left = if kind == 0 {
                        structure.budget.source_bytes_remaining()
                    } else {
                        structure.budget.steps_remaining()
                    };
                    structure
                        .budget
                        .charge(
                            structure.work,
                            Tick(1),
                            if kind == 0 { left - cut } else { 0 },
                            if kind == 1 { left - cut } else { 0 },
                            &mut 0,
                        )
                        .unwrap();
                } else {
                    let left = structure.work.remaining();
                    structure
                        .work
                        .charge(
                            Tick(1),
                            crate::admission::work::Charge {
                                io_bytes: if kind == 2 { left.io_bytes - cut } else { 0 },
                                records: if kind == 3 { left.records - cut } else { 0 },
                                output_bytes: if kind == 4 {
                                    left.output_bytes - cut
                                } else {
                                    0
                                },
                                ..Default::default()
                            },
                        )
                        .unwrap();
                }
                let result = (|| {
                    let mut child = owner.next(storage.backing(256), scratch, Tick(1))?;
                    for _ in 0..100000 {
                        match child.poll(Tick(1)) {
                            Ok(Status::Complete) => return child.finish(Tick(1)),
                            Ok(Status::Yield) => {}
                            Err(error) => {
                                assert_eq!(child.check_deadline(Tick(1)), Err(error));
                                assert!(child.value().is_none());
                                assert_eq!(child.finish(Tick(1)).err(), Some(error));
                                return Err(error);
                            }
                        }
                    }
                    panic!("quota child did not terminate")
                })();
                if cut == used_value {
                    result.unwrap();
                    assert_eq!(owner.completed(), Ok(1));
                    return;
                }
                let error = result.unwrap_err();
                assert!(owner.cells[0].retained.is_none());
                if matches!(kind, 1 | 3) && cut + 1 == used_value {
                    assert_eq!(owner.projecting.next, 1);
                }
                assert_eq!(owner.completed().err(), Some(error));
                assert_eq!(owner.check_deadline(Tick(1)), Err(error));
                assert_eq!(owner.finish(Tick(1)).err(), Some(error));
            });
        }
    }
}

#[test]
fn surplus_cells_cannot_join_whole_original_collection() {
    let mut parts = [Descriptor::default(); 8];
    let mut nodes = [Node::default(); 8];
    let mut lists = Lists::new();
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut output = [[0; 512]; 4];
    let mut cells = output.each_mut().map(|out| Cell::new(out));
    let selected = selection(
        SOURCE,
        0,
        &mut parts,
        &mut nodes,
        &mut lists,
        &mut work,
        &mut budget,
    );
    assert_eq!(selected.value().unwrap().structure.parts.len(), 3);
    assert_eq!(
        Collecting::new(selected, &mut cells, Tick(1)).err(),
        Some(Error::InvalidState)
    );
    assert_eq!(work.stopped(), None);
}
