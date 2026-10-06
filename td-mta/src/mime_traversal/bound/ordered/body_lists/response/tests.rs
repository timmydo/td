#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::{
    tests::{costs, Lists, ALTERNATIVE},
    Selecting,
};
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    limits::Limits,
    mime_traversal::bound::ordered::tests::{complete, meter, structure, Storage, SOURCE},
    mime_traversal::Part as Descriptor,
};

pub(super) fn selection<'a, 'w, 'n>(
    source: &'a [u8],
    base: u64,
    parts: &'w mut [Descriptor],
    nodes: &'n mut [Node],
    lists: &'w mut Lists,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
) -> Selected<'a, 'w, 'n> {
    let limits = Limits {
        header_bytes: source.len() + 17,
        ..Limits::default()
    };
    let mut traversal = crate::mime_traversal::bound::Cursor::new(
        source,
        base,
        crate::header_select::SourceEnd::Eof,
        &limits,
        parts,
        work,
        budget,
    )
    .unwrap();
    let mut done = false;
    for _ in 0..100000 {
        if traversal.poll(Tick(1)).unwrap() == Status::Complete {
            done = true;
            break;
        }
    }
    assert!(done);
    let binding = traversal.finish(Tick(1)).unwrap();
    let mut owner =
        crate::mime_traversal::bound::ordered::Classifying::new(binding, nodes).unwrap();
    let count = owner.total().unwrap();
    complete(&mut owner, &mut Storage::new(), &mut Scratch::new(), count);
    let owner = owner.finish(Tick(1)).unwrap();
    let mut cursor =
        Selecting::new(owner, &Limits::default(), lists.backing([8; 4]), Tick(1)).unwrap();
    for _ in 0..100000 {
        if cursor.poll(Tick(1)).unwrap() == mime_body_lists::Status::Complete {
            return cursor.finish(Tick(1)).unwrap();
        }
    }
    panic!("selection did not finish")
}
fn drain(part: &mut Part<'_, '_>) -> Result<usize, Error> {
    for turn in 1..100000 {
        if part.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
    }
    panic!("response metadata did not finish")
}
fn replay(
    owner: &mut Projecting<'_, '_, '_>,
    count: usize,
    storage: &mut Storage,
    scratch: &mut Scratch,
) -> Vec<usize> {
    let mut turns = Vec::new();
    for _ in 0..count {
        let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
        turns.push(drain(&mut child).unwrap());
        child.finish(Tick(1)).unwrap();
    }
    turns
}
#[derive(Debug, Eq, PartialEq)]
struct Metadata {
    part: Descriptor,
    kind: Vec<u8>,
    cid: Option<Vec<u8>>,
    language: Option<Vec<u8>>,
    location: Option<Vec<u8>>,
}
fn snapshot(part: Descriptor, view: label_json::View<'_>) -> Metadata {
    Metadata {
        part,
        kind: view.headers.content_type.to_vec(),
        cid: view.labels.content_id.map(<[u8]>::to_vec),
        language: view.labels.content_language.map(<[u8]>::to_vec),
        location: view.location.value.map(<[u8]>::to_vec),
    }
}
#[test]
fn replay_matches_independent_metadata_costs_source_and_original_owners() {
    for source in [SOURCE, ALTERNATIVE] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            let mut parts = [Descriptor::default(); 8];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut storage = Storage::new();
            let mut scratch = Scratch::new();
            let mut direct = structure(source, base, &mut parts, &mut work, &mut budget);
            let count = direct.parts().unwrap().len();
            let before = costs(direct.work, direct.budget);
            let mut expected = Vec::new();
            let mut turns = Vec::new();
            for ordinal in 1..=count {
                let mut child = direct
                    .metadata(ordinal as u16, storage.backing(256), &mut scratch)
                    .unwrap();
                let mut turn = 0;
                loop {
                    turn += 1;
                    if child.poll(Tick(1)).unwrap() == Status::Complete {
                        break;
                    }
                }
                turns.push(turn);
                let (view, _, _, _) = child.finish(Tick(1)).unwrap();
                let metadata = snapshot(view.part, view.metadata);
                if source == SOURCE {
                    assert_eq!(
                        metadata.kind,
                        match ordinal {
                            1 => b"multipart/digest".as_slice(),
                            2 => b"message/rfc822".as_slice(),
                            _ => b"text/plain".as_slice(),
                        }
                    );
                    assert_eq!(
                        metadata.cid.as_deref(),
                        if ordinal == 2 {
                            Some(b"\"id@a\"".as_slice())
                        } else {
                            None
                        }
                    );
                    assert_eq!(
                        metadata.language.as_deref(),
                        if ordinal == 2 {
                            Some(b"[\"fr\"]".as_slice())
                        } else {
                            None
                        }
                    );
                    assert_eq!(
                        metadata.location.as_deref(),
                        match ordinal {
                            1 => Some(b"\"../root\"".as_slice()),
                            3 => Some(b"\"../leaf\"".as_slice()),
                            _ => None,
                        }
                    );
                }
                if source == ALTERNATIVE {
                    assert_eq!(
                        metadata.kind,
                        match ordinal {
                            1 => b"multipart/alternative".as_slice(),
                            2 => b"multipart/mixed".as_slice(),
                            3 => b"text/plain".as_slice(),
                            _ => b"image/png".as_slice(),
                        }
                    );
                    assert_eq!(
                        (
                            metadata.cid.as_deref(),
                            metadata.language.as_deref(),
                            metadata.location.as_deref()
                        ),
                        (None, None, None)
                    );
                }
                expected.push(metadata);
            }
            let direct_cost = costs(direct.work, direct.budget);
            let delta = std::array::from_fn::<_, 5, _>(|i| direct_cost[i] - before[i]);
            let mut parts = [Descriptor::default(); 8];
            let mut nodes = [Node::default(); 8];
            let mut lists = Lists::new();
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let pointers = (
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
            let original_nodes = selected.binding.nodes.to_vec();
            let original_header = selected.binding.header_limit;
            assert_eq!(original_header, source.len() as u64 + 17);
            let original_nodes_pointer = selected.binding.nodes.as_ptr();
            let original_parts_pointer = selected.binding.parts.as_ptr();
            let original_lists_pointers = (
                selected.lists.text.as_ptr(),
                selected.lists.html.as_ptr(),
                selected.lists.attachments.as_ptr(),
            );
            let before = costs(selected.work, selected.budget);
            let mut owner = Projecting::new(selected, Tick(1)).unwrap();
            assert!(std::ptr::eq(owner.structure.source, source));
            assert_eq!(owner.structure.base, base);
            assert_eq!(owner.structure.header_limit, original_header);
            assert_eq!(owner.total(), Ok(count));
            owner.check_deadline(Tick(1)).unwrap();
            for (index, expected) in expected.into_iter().enumerate() {
                assert_eq!(owner.completed(), Ok(index));
                let mut child = owner
                    .next(storage.backing(256), &mut scratch, Tick(1))
                    .unwrap();
                assert!(child.value().is_none());
                assert_eq!(drain(&mut child).unwrap(), turns[index]);
                assert!(child.value().is_some());
                assert_eq!(child.poll(Tick(100)), Ok(Status::Complete));
                let (view, work, budget, scratch) = child.finish(Tick(1)).unwrap();
                assert_eq!(view.node, original_nodes[index]);
                assert_eq!(snapshot(view.part, view.metadata), expected);
                assert_eq!(
                    (
                        std::ptr::from_mut(work),
                        std::ptr::from_mut(budget),
                        std::ptr::from_mut(scratch)
                    ),
                    pointers
                );
            }
            assert_eq!(owner.completed(), Ok(count));
            let after = costs(owner.structure.work, owner.structure.budget);
            assert_eq!(
                std::array::from_fn::<_, 5, _>(|i| after[i] - before[i]),
                delta
            );
            let mut projected = owner.finish(Tick(1)).unwrap();
            assert!(std::ptr::eq(projected.structure.source, source));
            let complete = projected.value().unwrap();
            assert_eq!(complete.structure.parts.as_ptr(), original_parts_pointer);
            assert_eq!(complete.structure.nodes.as_ptr(), original_nodes_pointer);
            assert_eq!(
                (
                    complete.lists.text.as_ptr(),
                    complete.lists.html.as_ptr(),
                    complete.lists.attachments.as_ptr()
                ),
                original_lists_pointers
            );
            assert_eq!(projected.value().unwrap().structure.nodes, original_nodes);
            projected.check_deadline(Tick(1)).unwrap();
            let (view, work, budget) = projected.finish(Tick(1)).unwrap();
            if source == SOURCE {
                assert_eq!(
                    (
                        view.lists.text,
                        view.lists.html,
                        view.lists.attachments,
                        view.lists.has_attachment
                    ),
                    (&[3][..], &[3][..], &[2][..], true)
                );
            } else {
                assert_eq!(
                    (
                        view.lists.text,
                        view.lists.html,
                        view.lists.attachments,
                        view.lists.has_attachment
                    ),
                    (&[3, 4][..], &[3, 4][..], &[][..], false)
                );
            }
            assert_eq!(
                (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                (pointers.0, pointers.1)
            );
        }
    }
}
#[test]
fn every_part_prefix_refuses_late_finish_and_retires_whole_replay() {
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
    let mut owner = Projecting::new(selected, Tick(1)).unwrap();
    let count = owner.total().unwrap();
    let turns = replay(&mut owner, count, &mut storage, &mut scratch);
    for (index, turns) in turns.into_iter().enumerate() {
        for prefix in 0..=turns {
            for explicit in [false, true] {
                let mut parts = [Descriptor::default(); 8];
                let mut nodes = [Node::default(); 8];
                let mut lists = Lists::new();
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let selected = selection(
                    SOURCE,
                    17,
                    &mut parts,
                    &mut nodes,
                    &mut lists,
                    &mut work,
                    &mut budget,
                );
                let mut owner = Projecting::new(selected, Tick(1)).unwrap();
                replay(&mut owner, index, &mut storage, &mut scratch);
                let mut child = owner
                    .next(storage.backing(256), &mut scratch, Tick(1))
                    .unwrap();
                for _ in 0..prefix {
                    child.poll(Tick(1)).unwrap();
                }
                assert_eq!(child.value().is_some(), prefix == turns);
                let error = if explicit {
                    let error = child.check_deadline(Tick(100)).unwrap_err();
                    assert!(child.value().is_none());
                    assert_eq!(child.poll(Tick(1)), Err(error));
                    assert_eq!(child.finish(Tick(1)).err(), Some(error));
                    error
                } else {
                    child.finish(Tick(100)).err().unwrap()
                };
                assert_eq!(owner.completed(), Err(error));
                assert_eq!(owner.finish(Tick(1)).err(), Some(error));
                assert_eq!(work.stopped(), Some(Stop::Deadline));
            }
        }
    }
}
#[test]
fn whole_prefixes_and_completed_owner_freshly_admit() {
    for count in 0..=3 {
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
                0,
                &mut parts,
                &mut nodes,
                &mut lists,
                &mut work,
                &mut budget,
            );
            let mut owner = Projecting::new(selected, Tick(1)).unwrap();
            replay(&mut owner, count, &mut storage, &mut scratch);
            let result = owner.finish(if late { Tick(100) } else { Tick(1) });
            if late {
                assert_eq!(
                    result.err(),
                    Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
                );
            } else if count < 3 {
                assert_eq!(result.err(), Some(Error::InvalidState));
            } else {
                let mut projected = result.unwrap();
                assert!(projected.value().is_some());
                let error = projected.check_deadline(Tick(100)).unwrap_err();
                assert!(projected.value().is_none());
                assert_eq!(projected.finish(Tick(1)).err(), Some(error));
            }
        }
    }
    for mode in 0..7 {
        let mut parts = [Descriptor::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut lists = Lists::new();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        let mut selected = selection(
            SOURCE,
            0,
            &mut parts,
            &mut nodes,
            &mut lists,
            &mut work,
            &mut budget,
        );
        let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
        if mode == 0 {
            assert_eq!(Projecting::new(selected, Tick(100)).err(), Some(error));
        } else if mode == 3 {
            assert_eq!(selected.check_deadline(Tick(100)), Err(error));
            assert_eq!(Projecting::new(selected, Tick(1)).err(), Some(error));
        } else if mode == 4 {
            let bytes = selected.budget.source_bytes_remaining();
            assert!(selected
                .budget
                .charge(selected.work, Tick(1), bytes + 1, 0, &mut 0)
                .is_err());
            assert_eq!(
                Projecting::new(selected, Tick(1)).err(),
                Some(Error::Admission(crate::nfc::Error::InterpretationLimit))
            );
        } else {
            let mut owner = Projecting::new(selected, Tick(1)).unwrap();
            if mode == 5 {
                replay(&mut owner, 3, &mut storage, &mut scratch);
                assert_eq!(
                    owner
                        .next(storage.backing(256), &mut scratch, Tick(100))
                        .err(),
                    Some(error)
                );
                assert_eq!(owner.completed(), Err(error));
                assert_eq!(owner.finish(Tick(1)).err(), Some(error));
                assert_eq!(work.stopped(), Some(Stop::Deadline));
            } else if mode == 6 {
                assert_eq!(owner.check_deadline(Tick(100)), Err(error));
                assert_eq!(owner.completed(), Err(error));
                assert_eq!(owner.finish(Tick(1)).err(), Some(error));
            } else if mode == 1 {
                assert_eq!(
                    owner
                        .next(storage.backing(0), &mut scratch, Tick(100))
                        .err(),
                    Some(error)
                );
                assert_eq!(owner.completed(), Err(error));
            } else {
                replay(&mut owner, 3, &mut storage, &mut scratch);
                assert_eq!(
                    owner.finish(Tick(1)).unwrap().finish(Tick(100)).err(),
                    Some(error)
                );
            }
        }
    }
}
#[test]
fn extra_parts_forgetting_capacity_and_original_cuts_retire_replay() {
    fn forget<T>(value: T) {
        std::mem::forget(value);
    }
    for mode in 0..8 {
        let mut parts = [Descriptor::default(); 8];
        let mut nodes = [Node::default(); 8];
        let mut lists = Lists::new();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut storage = Storage::new();
        let mut scratch = Scratch::new();
        let selected = selection(
            SOURCE,
            0,
            &mut parts,
            &mut nodes,
            &mut lists,
            &mut work,
            &mut budget,
        );
        if mode >= 4 {
            let remaining = selected.work.remaining();
            let charge = match mode {
                4 => Charge {
                    io_bytes: remaining.io_bytes - 1,
                    ..Charge::default()
                },
                5 => Charge {
                    records: remaining.records - 1,
                    ..Charge::default()
                },
                6 => Charge {
                    output_bytes: remaining.output_bytes - 1,
                    ..Charge::default()
                },
                _ => Charge::default(),
            };
            selected.work.charge(Tick(1), charge).unwrap();
            if mode == 7 {
                let remaining = selected.budget.source_bytes_remaining();
                selected
                    .budget
                    .charge(selected.work, Tick(1), remaining - 1, 0, &mut 0)
                    .unwrap();
            }
        }
        let mut owner = Projecting::new(selected, Tick(1)).unwrap();
        if mode == 0 {
            replay(&mut owner, 3, &mut storage, &mut scratch);
            assert_eq!(
                owner
                    .next(storage.backing(256), &mut scratch, Tick(1))
                    .err(),
                Some(Error::InvalidState)
            );
        } else {
            let mut child = owner
                .next(
                    storage.backing(if mode == 3 { 0 } else { 256 }),
                    &mut scratch,
                    Tick(1),
                )
                .unwrap();
            if mode == 1 {
                forget(child);
            } else if mode == 2 {
                drain(&mut child).unwrap();
                forget(child);
            } else {
                let error = drain(&mut child).unwrap_err();
                if mode == 3 {
                    assert_eq!(
                        error,
                        Error::Metadata(label_json::Error::Location(
                            crate::mime_location_fields::json::Error::Retention(
                                crate::mime_location_field::retained::Error::OutputCapacity
                            )
                        ))
                    );
                } else if mode == 7 {
                    assert_eq!(
                        error,
                        Error::Metadata(label_json::Error::Headers(
                            crate::mime_part_headers::Error::Admission(
                                crate::nfc::Error::InterpretationLimit
                            )
                        ))
                    );
                }
                assert!(child.value().is_none());
                assert!(child.finish(Tick(1)).is_err());
            }
        }
        assert!(owner.completed().is_err());
        assert!(owner.finish(Tick(1)).is_err());
        if (4..7).contains(&mode) {
            assert_eq!(
                work.stopped(),
                Some(match mode {
                    4 => Stop::IoBytes,
                    5 => Stop::Records,
                    _ => Stop::OutputBytes,
                })
            );
        }
    }
}
