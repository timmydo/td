#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    ports::Deadline,
};
const SOURCE: &[u8] = concat!(
    "Content-Type: multipart/digest;boundary=a\r\n",
    "Content-Location: ../root\r\n\r\n--a\r\n",
    "Content-ID: <id@a>\r\nContent-Language: fr\r\n",
    "Content-Location: \r\n\r\nbody\r\n--a\r\n",
    "Content-Type: text/plain\r\nContent-Location: ../leaf\r\n\r\n",
    "Content-Location: ../body\r\n--a--\r\n"
)
.as_bytes();
struct Storage {
    heads: [u8; 256],
    charset: [u8; 256],
    filename: [u8; 256],
    id: [u8; 256],
    language: [u8; 256],
    location: [u8; 256],
}
impl Storage {
    fn new() -> Self {
        Self {
            heads: [0xa5; 256],
            charset: [0xa5; 256],
            filename: [0xa5; 256],
            id: [0xa5; 256],
            language: [0xa5; 256],
            location: [0xa5; 256],
        }
    }
    fn backing(&mut self, cap: usize) -> label_json::Backing<'_> {
        label_json::Backing {
            headers: mime_part_headers::Backing {
                heads: &mut self.heads,
                charset: &mut self.charset,
                filename: &mut self.filename,
            },
            labels: crate::mime_label_fields::json::Backing {
                content_id: &mut self.id,
                content_language: &mut self.language,
            },
            content_location: &mut self.location[..cap],
        }
    }
}
fn meter() -> Meter {
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
fn costs(work: &Meter, budget: &HeaderBudget) -> [u64; 5] {
    [
        16 * 1024 * 1024 - budget.source_bytes_remaining(),
        16_000_000 - budget.steps_remaining(),
        100_000_000 - work.remaining().io_bytes,
        10_000_000 - work.remaining().records,
        10_000_000 - work.remaining().output_bytes,
    ]
}
fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
    for turn in 1..100_000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
    }
    panic!("bound traversal did not finish")
}
fn drain_part(cursor: &mut PartCursor<'_, '_>) -> Result<usize, Error> {
    for turn in 1..100_000 {
        if cursor.poll(Tick(1))? == Status::Complete {
            return Ok(turn);
        }
        assert!(cursor.value().is_none());
    }
    panic!("bound metadata did not finish")
}
fn structure<'a, 'w>(
    source: &'a [u8],
    base: u64,
    parts: &'w mut [Part],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
) -> Structure<'a, 'w> {
    let mut cursor = Cursor::new(
        source,
        base,
        SourceEnd::Eof,
        &Limits::default(),
        parts,
        work,
        budget,
    )
    .unwrap();
    drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap()
}
fn assert_view(part: Part, view: &label_json::View<'_>, index: usize) {
    assert_eq!(view.headers.body_start, part.body_start);
    assert_eq!(view.location.selection.end.body_start, part.body_start);
    assert_eq!(
        view.headers.content_type,
        [
            b"multipart/digest".as_slice(),
            b"message/rfc822",
            b"text/plain"
        ][index]
    );
    assert_eq!(
        view.location.value,
        [
            Some(b"\"../root\"".as_slice()),
            Some(b"\"\""),
            Some(b"\"../leaf\"")
        ][index]
    );
    if index == 1 {
        assert_eq!(view.labels.content_id, Some(b"\"id@a\"".as_slice()));
        assert_eq!(view.labels.content_language, Some(b"[\"fr\"]".as_slice()));
    }
}
#[test]
fn original_source_context_owners_and_costs_match_independent_children() {
    for base in [0, 17, u64::MAX - SOURCE.len() as u64] {
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let pointers = (
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut cursor = super::super::Cursor::new(
            SOURCE,
            base,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut turns = 0;
        loop {
            turns += 1;
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                break;
            }
        }
        let (parts, work, budget) = cursor.finish(Tick(1)).unwrap();
        let mut child_turns = [0; 3];
        for (index, part) in parts.iter().enumerate() {
            let mut storage = Storage::new();
            let entity = mime_part_headers::Entity {
                source: td_header::resident::slice(
                    SOURCE,
                    base,
                    part.entity_start..part.entity_end,
                )
                .unwrap(),
                base: part.entity_start,
                source_end: SourceEnd::Eof,
                header_limit: Limits::default().header_bytes as u64,
                context: part.context(),
            };
            let mut cursor =
                label_json::Cursor::new(entity, storage.backing(256), work, budget, &mut scratch)
                    .unwrap();
            loop {
                child_turns[index] += 1;
                if cursor.poll(Tick(1)).unwrap() == mime_part_headers::Status::Complete {
                    break;
                }
            }
            let (view, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    std::ptr::from_mut(work),
                    std::ptr::from_mut(budget),
                    std::ptr::from_mut(scratch)
                ),
                pointers
            );
            assert_view(*part, &view, index);
        }
        let direct = costs(work, budget);
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let pointers = (
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut cursor = Cursor::new(
            SOURCE,
            base,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor).unwrap(), turns);
        let mut bound = cursor.finish(Tick(1)).unwrap();
        assert_eq!(bound.parts().unwrap().len(), 3);
        for (index, ordinal) in (1..=3).enumerate() {
            let mut storage = Storage::new();
            let mut cursor = bound
                .metadata(ordinal, storage.backing(256), &mut scratch)
                .unwrap();
            assert_eq!(drain_part(&mut cursor).unwrap(), child_turns[index]);
            let (view, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    std::ptr::from_mut(work),
                    std::ptr::from_mut(budget),
                    std::ptr::from_mut(scratch)
                ),
                pointers
            );
            assert_eq!(view.part.ordinal, ordinal);
            assert_view(view.part, &view.metadata, index);
        }
        let (_, work, budget) = bound.finish(Tick(1)).unwrap();
        assert_eq!(costs(work, budget), direct);
        assert!(direct.iter().all(|cost| *cost > 0));
    }
}
#[test]
fn every_traversal_prefix_requires_fresh_complete_handoff() {
    let mut parts = [Part::default(); 8];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        SOURCE,
        17,
        SourceEnd::Eof,
        &Limits::default(),
        &mut parts,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let turns = drain(&mut cursor).unwrap();
    cursor.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
    for cut in 0..=turns {
        for late in [false, true] {
            let mut parts = [Part::default(); 8];
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(
                SOURCE,
                17,
                SourceEnd::Eof,
                &Limits::default(),
                &mut parts,
                &mut work,
                &mut budget,
            )
            .unwrap();
            for _ in 0..cut {
                cursor.poll(Tick(1)).unwrap();
            }
            if late {
                assert!(matches!(
                    cursor.finish(Tick(100)),
                    Err(Error::Traversal(super::super::Error::Work(Stop::Deadline)))
                ));
                assert_eq!(work.stopped(), Some(Stop::Deadline));
            } else if cut == turns {
                cursor.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
            } else {
                assert!(matches!(
                    cursor.finish(Tick(1)),
                    Err(Error::Traversal(super::super::Error::InvalidState))
                ));
                assert_eq!(work.stopped(), None);
            }
        }
    }
    let mut parts = [Part::default(); 8];
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    assert!(matches!(
        Cursor::new(
            SOURCE,
            17,
            SourceEnd::Prefix,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget
        ),
        Err(Error::Traversal(super::super::Error::IncompleteSource))
    ));
}
#[test]
fn refusal_abandonment_and_invalid_ordinal_retire_binding() {
    fn forget<T>(value: T) {
        std::mem::forget(value);
    }
    for kind in 0..7 {
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        let error = if kind < 2 {
            bound
                .metadata(
                    if kind == 0 { 0 } else { 4 },
                    storage.backing(256),
                    &mut scratch,
                )
                .err()
                .unwrap()
        } else {
            let mut cursor = bound
                .metadata(
                    1,
                    storage.backing(if kind == 2 { 0 } else { 256 }),
                    &mut scratch,
                )
                .unwrap();
            if kind == 2 {
                let error = drain_part(&mut cursor).unwrap_err();
                assert!(cursor.value().is_none());
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                error
            } else if kind == 3 {
                let error = cursor.check_deadline(Tick(100)).unwrap_err();
                assert!(matches!(error, Error::Metadata(_)));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                error
            } else {
                if kind == 6 {
                    drain_part(&mut cursor).unwrap();
                }
                if kind == 5 {
                    forget(cursor);
                }
                Error::Abandoned
            }
        };
        assert_eq!(bound.parts(), Err(error));
        assert_eq!(bound.check_deadline(Tick(1)), Err(error));
        assert_eq!(
            bound.metadata(2, storage.backing(256), &mut scratch).err(),
            Some(error)
        );
        assert_eq!(bound.finish(Tick(1)).err(), Some(error));
        assert_eq!(
            work.stopped(),
            if kind == 3 {
                Some(Stop::Deadline)
            } else {
                None
            }
        );
    }
}
#[test]
fn every_part_prefix_and_binding_finish_are_fresh() {
    for ordinal in [1, 2] {
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        let mut cursor = bound
            .metadata(ordinal, storage.backing(256), &mut scratch)
            .unwrap();
        let turns = drain_part(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        bound.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            for trial in 0..3 {
                let mut parts = [Part::default(); 8];
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut storage = Storage::new();
                let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
                let mut cursor = bound
                    .metadata(ordinal, storage.backing(256), &mut scratch)
                    .unwrap();
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                if trial == 0 {
                    let error = cursor.check_deadline(Tick(100)).unwrap_err();
                    assert!(matches!(error, Error::Metadata(_)));
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    assert_eq!(bound.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if trial == 1 {
                    let error = cursor.finish(Tick(100)).err().unwrap();
                    assert!(matches!(error, Error::Metadata(_)));
                    assert_eq!(bound.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if cut == turns {
                    cursor.finish(Tick(1)).unwrap();
                    assert!(matches!(bound.finish(Tick(100)), Err(Error::Admission(_))));
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else {
                    let error = Error::Metadata(label_json::Error::InvalidState);
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    assert_eq!(bound.finish(Tick(1)).err(), Some(error));
                    assert_eq!(work.stopped(), None);
                }
            }
        }
    }
}

#[test]
fn defensive_descriptor_body_mismatch_retires_metadata_and_binding() {
    for finishing in [false, true] {
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        let mut cursor = bound
            .metadata(1, storage.backing(256), &mut scratch)
            .unwrap();
        if finishing {
            drain_part(&mut cursor).unwrap();
            cursor.part.body_start += 1;
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        } else {
            cursor.part.body_start += 1;
            assert_eq!(drain_part(&mut cursor), Err(Error::InvalidState));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
        }
        assert_eq!(bound.parts(), Err(Error::InvalidState));
        assert_eq!(bound.finish(Tick(1)).err(), Some(Error::InvalidState));
        assert_eq!(work.stopped(), None);
    }
}
#[test]
fn defensive_descriptor_ranges_and_identity_retire_binding() {
    for kind in 0..4 {
        let mut parts = [Part::default(); 8];
        let mut changed = [Part::default(); 1];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        changed[0] = bound.parts().unwrap()[0];
        let expected = match kind {
            0 => {
                changed[0].ordinal = 2;
                Error::PartOrdinal
            }
            1 => {
                changed[0].body_start = changed[0].entity_start - 1;
                Error::InvalidRange
            }
            2 => {
                changed[0].body_start = changed[0].entity_end + 1;
                Error::InvalidRange
            }
            _ => {
                changed[0].entity_start -= 1;
                Error::InvalidRange
            }
        };
        bound.parts = &changed;
        assert_eq!(
            bound.metadata(1, storage.backing(256), &mut scratch).err(),
            Some(expected)
        );
        assert_eq!(bound.parts(), Err(expected));
        assert_eq!(bound.finish(Tick(1)).err(), Some(expected));
        assert_eq!(work.stopped(), None);
    }
}
#[test]
fn cached_completion_is_inert_but_explicit_traversal_admission_is_fresh() {
    for complete in [false, true] {
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            SOURCE,
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        if complete {
            drain(&mut cursor).unwrap();
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        }
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Traversal(super::super::Error::Work(Stop::Deadline)))
        );
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::Traversal(super::super::Error::Work(Stop::Deadline)))
        );
        assert_eq!(work.stopped(), Some(Stop::Deadline));
    }
    for ordinal in [1, 2] {
        let mut parts = [Part::default(); 8];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut bound = structure(SOURCE, 17, &mut parts, &mut work, &mut budget);
        let mut cursor = bound
            .metadata(ordinal, storage.backing(256), &mut scratch)
            .unwrap();
        drain_part(&mut cursor).unwrap();
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        assert!(cursor.value().is_some());
        let error = cursor.check_deadline(Tick(100)).unwrap_err();
        assert!(matches!(error, Error::Metadata(_)));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        assert_eq!(bound.finish(Tick(1)).err(), Some(error));
        assert_eq!(work.stopped(), Some(Stop::Deadline));
    }
}
