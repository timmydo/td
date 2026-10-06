#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    header_select::SourceEnd,
    limits::Limits,
    mime_body_lists::{Class, Disposition, Media},
    mime_part_headers,
    mime_traversal::{self, bound, Status},
    ports::Deadline,
};
pub(super) const SOURCE: &[u8] = concat!(
    "Content-Type: multipart/digest;boundary=a\r\n",
    "Content-Location: ../root\r\n\r\n--a\r\n",
    "Content-ID: <id@a>\r\nContent-Language: fr\r\n\r\nbody\r\n--a\r\n",
    "Content-Type: text/plain\r\nContent-Location: ../leaf\r\n\r\n",
    "body\r\n--a--\r\n"
)
.as_bytes();
pub(super) struct Storage {
    heads: [u8; 256],
    charset: [u8; 256],
    name: [u8; 256],
    id: [u8; 256],
    language: [u8; 256],
    location: [u8; 256],
}
impl Storage {
    pub(super) fn new() -> Self {
        Self {
            heads: [0xa5; 256],
            charset: [0xa5; 256],
            name: [0xa5; 256],
            id: [0xa5; 256],
            language: [0xa5; 256],
            location: [0xa5; 256],
        }
    }
    pub(super) fn backing(&mut self, cap: usize) -> label_json::Backing<'_> {
        label_json::Backing {
            headers: mime_part_headers::Backing {
                heads: &mut self.heads,
                charset: &mut self.charset,
                filename: &mut self.name,
            },
            labels: crate::mime_label_fields::json::Backing {
                content_id: &mut self.id,
                content_language: &mut self.language,
            },
            content_location: &mut self.location[..cap],
        }
    }
}
pub(super) fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 10_000_000,
            output_bytes: 10_000_000,
            ..Charge::default()
        },
    )
}
pub(super) fn structure<'a, 'w>(
    source: &'a [u8],
    base: u64,
    parts: &'w mut [mime_traversal::Part],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
) -> Structure<'a, 'w> {
    let mut cursor = bound::Cursor::new(
        source,
        base,
        SourceEnd::Eof,
        &Limits::default(),
        parts,
        work,
        budget,
    )
    .unwrap();
    for _ in 0..100000 {
        if cursor.poll(Tick(1)).unwrap() == Status::Complete {
            return cursor.finish(Tick(1)).unwrap();
        }
    }
    panic!("ordered fixture traversal did not complete")
}
fn drain(part: &mut Part<'_, '_>) -> Result<usize, Error> {
    for turn in 1..100000 {
        if part.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
    }
    panic!("ordered fixture metadata did not complete")
}
fn costs(work: &Meter, budget: &HeaderBudget) -> [u64; 5] {
    [
        16 * 1024 * 1024 - budget.source_bytes_remaining(),
        16_000_000 - budget.steps_remaining(),
        100_000_000 - work.remaining().io_bytes,
        10_000_000 - work.remaining().records,
        10_000_000 - work.remaining().output_bytes,
    ]
}
pub(super) fn complete(
    owner: &mut Classifying<'_, '_, '_>,
    storage: &mut Storage,
    scratch: &mut Scratch,
    count: usize,
) {
    for _ in 0..count {
        let mut part = owner.next(storage.backing(256), scratch).unwrap();
        drain(&mut part).unwrap();
        part.finish(Tick(1)).unwrap();
    }
}
#[test]
fn whole_ordered_nodes_match_independent_parts_costs_and_original_owners() {
    for base in [0, 17, u64::MAX - SOURCE.len() as u64] {
        let mut parts = [mime_traversal::Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut source = structure(SOURCE, base, &mut parts, &mut work, &mut budget);
        let mut independent = [Node::default(); 3];
        let mut turns = [0; 3];
        for (index, node) in independent.iter_mut().enumerate() {
            let mut cursor = source
                .metadata(
                    u16::try_from(index + 1).unwrap(),
                    storage.backing(256),
                    &mut scratch,
                )
                .unwrap();
            loop {
                turns[index] += 1;
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            *node = cursor.finish_classified(Tick(1)).unwrap().0.node;
        }
        let (parts, work, budget) = source.finish(Tick(1)).unwrap();
        let original_parts = parts.to_vec();
        let direct = costs(work, budget);
        let mut parts = [mime_traversal::Part::default(); 8];
        let forged = Node {
            parent: 42,
            depth: 63,
            class: Class {
                media: Media::Other,
                disposition: Disposition::Attachment,
                named: true,
            },
        };
        let mut nodes = [forged; 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let pointers = (
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut storage = Storage::new();
        let bound = structure(SOURCE, base, &mut parts, &mut work, &mut budget);
        let mut owner = Classifying::new(bound, &mut nodes).unwrap();
        assert_eq!(owner.total(), Ok(3));
        assert_eq!(owner.completed(), Ok(0));
        owner.check_deadline(Tick(1)).unwrap();
        for (index, &expected) in turns.iter().enumerate() {
            let mut part = owner.next(storage.backing(256), &mut scratch).unwrap();
            part.check_deadline(Tick(1)).unwrap();
            assert_eq!(drain(&mut part).unwrap(), expected);
            assert_eq!(part.poll(Tick(100)), Ok(Status::Complete));
            let (view, work, budget, scratch) = part.finish(Tick(1)).unwrap();
            assert_eq!(view.part.ordinal, u16::try_from(index + 1).unwrap());
            assert_eq!(
                (
                    std::ptr::from_mut(work),
                    std::ptr::from_mut(budget),
                    std::ptr::from_mut(scratch)
                ),
                pointers
            );
            assert_eq!(owner.completed(), Ok(index + 1));
        }
        let mut classified = owner.finish(Tick(1)).unwrap();
        classified.check_deadline(Tick(1)).unwrap();
        assert_eq!(classified.parts().unwrap(), original_parts);
        assert_eq!(classified.nodes().unwrap(), independent);
        assert_eq!(classified.nodes().unwrap().len(), 3);
        for (index, &media) in [Media::Multipart, Media::Other, Media::Plain]
            .iter()
            .enumerate()
        {
            assert_eq!(
                classified.nodes().unwrap()[index],
                Node {
                    parent: if index == 0 { 0 } else { 1 },
                    depth: if index == 0 { 1 } else { 2 },
                    class: Class {
                        media,
                        disposition: Disposition::Other,
                        named: false
                    }
                }
            );
        }
        let (view, work, budget) = classified.finish(Tick(1)).unwrap();
        assert_eq!(view.parts, original_parts);
        assert_eq!(view.nodes, independent);
        assert_eq!(
            (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
            (pointers.0, pointers.1)
        );
        assert_eq!(costs(work, budget), direct);
        assert_eq!(&nodes[3..], &[forged; 5]);
    }
}
#[test]
fn every_completed_part_prefix_requires_fresh_whole_completion() {
    for count in 0..=3 {
        for late in 0..3 {
            let mut parts = [mime_traversal::Part::default(); 8];
            let mut nodes = [Node::default(); 3];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut storage = Storage::new();
            let bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
            let mut owner = Classifying::new(bound, &mut nodes).unwrap();
            complete(&mut owner, &mut storage, &mut scratch, count);
            if late != 0 {
                if late == 2 {
                    let error = owner.check_deadline(Tick(100)).unwrap_err();
                    assert_eq!(owner.total(), Err(error));
                    assert_eq!(owner.completed(), Err(error));
                    assert_eq!(owner.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                    continue;
                }
                assert!(matches!(
                    owner.finish(Tick(100)),
                    Err(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
                ));
                assert_eq!(work.stopped(), Some(Stop::Deadline));
            } else if count == 3 {
                let mut classified = owner.finish(Tick(1)).unwrap();
                assert_eq!(classified.nodes().unwrap().len(), 3);
                let error = classified.check_deadline(Tick(100)).unwrap_err();
                assert!(matches!(
                    error,
                    Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
                ));
                assert_eq!(classified.nodes(), Err(error));
                assert_eq!(classified.parts(), Err(error));
                assert_eq!(classified.finish(Tick(1)).err(), Some(error));
                assert_eq!(work.stopped(), Some(Stop::Deadline));
            } else {
                assert_eq!(owner.finish(Tick(1)).err(), Some(Error::InvalidState));
                assert_eq!(work.stopped(), None);
            }
        }
    }
}
#[test]
fn refusal_forgetting_and_extra_part_retire_ordered_owner() {
    fn forget<T>(value: T) {
        std::mem::forget(value);
    }
    for kind in 0..6 {
        let mut parts = [mime_traversal::Part::default(); 8];
        let mut nodes = [Node::default(); 3];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        let mut owner = Classifying::new(bound, &mut nodes).unwrap();
        complete(
            &mut owner,
            &mut storage,
            &mut scratch,
            if kind == 5 { 3 } else { 1 },
        );
        let error = if kind == 5 {
            owner
                .next(storage.backing(256), &mut scratch)
                .err()
                .unwrap()
        } else {
            let mut part = owner
                .next(
                    storage.backing(if kind == 0 { 0 } else { 256 }),
                    &mut scratch,
                )
                .unwrap();
            if kind == 0 {
                // Digest child lacks a location, so use its healthy finish, then
                // refuse the third part's present location with zero backing.
                drain(&mut part).unwrap();
                part.finish(Tick(1)).unwrap();
                let mut part = owner.next(storage.backing(0), &mut scratch).unwrap();
                let error = drain(&mut part).unwrap_err();
                assert_eq!(part.finish(Tick(1)).err(), Some(error));
                error
            } else if kind == 1 {
                let error = part.check_deadline(Tick(100)).unwrap_err();
                assert_eq!(part.finish(Tick(1)).err(), Some(error));
                error
            } else if kind == 2 {
                part.finish(Tick(1)).err().unwrap()
            } else {
                if kind == 4 {
                    drain(&mut part).unwrap();
                }
                forget(part);
                Error::Abandoned
            }
        };
        assert_eq!(owner.completed(), Err(error));
        assert_eq!(owner.check_deadline(Tick(1)), Err(error));
        assert_eq!(
            owner.next(storage.backing(256), &mut scratch).err(),
            Some(error)
        );
        assert_eq!(owner.finish(Tick(1)).err(), Some(error));
        assert_eq!(
            work.stopped(),
            if kind == 1 {
                Some(Stop::Deadline)
            } else {
                None
            }
        );
    }
    let mut parts = [mime_traversal::Part::default(); 8];
    let mut nodes = [Node::default(); 2];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
    let before = costs(bound.work, bound.budget);
    assert_eq!(
        Classifying::new(bound, &mut nodes).err(),
        Some(Error::NodeCapacity)
    );
    assert_eq!(costs(&work, &budget), before);
    let mut scratch = Scratch::new();
    let mut storage = Storage::new();
    let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
    let child = bound
        .metadata(1, storage.backing(256), &mut scratch)
        .unwrap();
    forget(child);
    let before = costs(bound.work, bound.budget);
    assert_eq!(
        Classifying::new(bound, &mut nodes).err(),
        Some(Error::Abandoned)
    );
    assert_eq!(costs(&work, &budget), before);
}
#[test]
fn every_ordered_child_prefix_and_complete_owner_finish_freshly_admit() {
    for ordinal in 1..=3 {
        let mut parts = [mime_traversal::Part::default(); 8];
        let mut nodes = [Node::default(); 3];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        let mut owner = Classifying::new(bound, &mut nodes).unwrap();
        complete(&mut owner, &mut storage, &mut scratch, ordinal - 1);
        let mut part = owner.next(storage.backing(256), &mut scratch).unwrap();
        let turns = drain(&mut part).unwrap();
        part.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            for late in [false, true] {
                let mut parts = [mime_traversal::Part::default(); 8];
                let mut nodes = [Node::default(); 3];
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut storage = Storage::new();
                let bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
                let mut owner = Classifying::new(bound, &mut nodes).unwrap();
                complete(&mut owner, &mut storage, &mut scratch, ordinal - 1);
                let mut part = owner.next(storage.backing(256), &mut scratch).unwrap();
                for _ in 0..cut {
                    part.poll(Tick(1)).unwrap();
                }
                if late {
                    let error = part.finish(Tick(100)).err().unwrap();
                    assert!(matches!(error, Error::Metadata(_)));
                    assert_eq!(owner.completed(), Err(error));
                    assert_eq!(owner.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if cut != turns {
                    let error = Error::Metadata(label_json::Error::InvalidState);
                    assert_eq!(part.finish(Tick(1)).err(), Some(error));
                    assert_eq!(owner.completed(), Err(error));
                    assert_eq!(owner.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), None);
                } else {
                    let (view, _, _, _) = part.finish(Tick(1)).unwrap();
                    assert_eq!(view.part.ordinal, u16::try_from(ordinal).unwrap());
                    assert_eq!(owner.completed(), Ok(ordinal));
                    if ordinal != 3 {
                        assert_eq!(owner.finish(Tick(1)).err(), Some(Error::InvalidState));
                        assert_eq!(work.stopped(), None);
                    } else {
                        let classified = owner.finish(Tick(1)).unwrap();
                        assert_eq!(classified.nodes().unwrap().len(), 3);
                        assert_eq!(
                            classified.finish(Tick(100)).err(),
                            Some(Error::Admission(crate::nfc::Error::Work(Stop::Deadline)))
                        );
                        assert_eq!(work.stopped(), Some(Stop::Deadline));
                    }
                }
            }
        }
    }
}
