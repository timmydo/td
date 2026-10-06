#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::super::tests::{drain as part_drain, exercise as collection};
use super::super::super::Mode;
use super::super::{Cursor as Retainer, Status};
use super::*;
use crate::{admission::work::Stop, mime_traversal::bound::ordered::tests::SOURCE};
const PARENT: BlobId = BlobId::from_bytes([0x44; 16]);
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const BASE64: &[u8] = b"Content-Transfer-Encoding: base64\r\n\r\nYWJjZA==";
const QP: &[u8] = b"Content-Transfer-Encoding: quoted-printable\r\n\r\na=62";
const UNKNOWN: &[u8] = b"Content-Transfer-Encoding: strange\r\n\r\nxyz";
fn forget<T>(value: T) {
    std::mem::forget(value);
}
fn exercise<T>(
    source: &[u8],
    base: u64,
    f: impl FnOnce(Cursor<'_, '_, '_, '_, '_, '_, '_>) -> T,
) -> T {
    collection(source, base, |mut owner, storage, scratch| {
        let count = owner.total().unwrap();
        for _ in 0..count {
            let mut child = owner.next(storage.backing(256), scratch, Tick(1)).unwrap();
            part_drain(&mut child);
            child.finish(Tick(1)).unwrap();
        }
        let mut output = [0; 2048];
        let mut retained = Retainer::new(
            owner.finish(Tick(1)).unwrap(),
            Mode::Structure,
            &mut output,
            Tick(1),
        )
        .unwrap();
        let mut done = false;
        for _ in 0..100000 {
            if retained.poll(Tick(1)).unwrap() == Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        let retained = retained.finish(Tick(1)).unwrap();
        let mut cells = [Candidate::default(); 8];
        f(Cursor::new(retained, PARENT, &mut cells[..count], Tick(1)).unwrap())
    })
}
fn costs(cursor: &Cursor<'_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &cursor.source.original.source.projected.structure;
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
fn literal_leaf_ids_use_encoded_extents_and_relative_base_for_every_tag() {
    for (source, suffix, offset, length, encoding) in [
        (
            b"\r\n".as_slice(),
            "0000000000000002000000000000000000",
            2,
            0,
            TransferEncoding::Identity,
        ),
        (
            SIMPLE,
            "0000000000000002000000000000000500",
            2,
            5,
            TransferEncoding::Identity,
        ),
        (
            BASE64,
            "0000000000000025000000000000000801",
            37,
            8,
            TransferEncoding::Base64,
        ),
        (
            QP,
            "000000000000002f000000000000000402",
            47,
            4,
            TransferEncoding::QuotedPrintable,
        ),
        (
            UNKNOWN,
            "0000000000000026000000000000000300",
            38,
            3,
            TransferEncoding::Identity,
        ),
    ] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            exercise(source, base, |mut cursor| {
                let structure = &mut cursor.source.original.source.projected.structure;
                let pointers = (
                    std::ptr::from_mut(structure.work),
                    std::ptr::from_mut(structure.budget),
                );
                let backing = cursor.candidates.as_ptr();
                let members = cursor.source.value().unwrap().members.as_ptr();
                assert!(cursor.value().is_none());
                let before = costs(&cursor);
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                let after = costs(&cursor);
                assert_eq!(
                    [
                        before[0] - after[0],
                        before[1] - after[1],
                        before[2] - after[2],
                        before[3] - after[3],
                        before[4] - after[4]
                    ],
                    [0, 69, 0, 6, 69]
                );
                let view = cursor.value().unwrap();
                let candidate = view.candidates[0];
                assert_eq!(candidate.ordinal, 1);
                assert_eq!(
                    candidate.locator,
                    Some(PartLocator {
                        parent: PARENT,
                        offset,
                        length,
                        encoding
                    })
                );
                assert_eq!(
                    candidate.wire().unwrap(),
                    format!("p1_44444444444444444444444444444444{suffix}")
                );
                assert_eq!(
                    PartLocator::decode(candidate.wire().unwrap()).unwrap(),
                    candidate.locator.unwrap()
                );
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(costs(&cursor), after);
                let mapped = cursor.finish(Tick(1)).unwrap();
                assert_eq!(mapped.value().unwrap().candidates.as_ptr(), backing);
                assert_eq!(mapped.value().unwrap().original.members.as_ptr(), members);
                let ((parent, candidates), ((mode, bytes, view), work, budget)) =
                    mapped.finish(Tick(1)).unwrap();
                assert_eq!(parent, PARENT);
                assert_eq!(mode, Mode::Structure);
                assert_eq!(bytes.as_ptr(), members);
                assert_eq!(view.fragments.len(), 1);
                assert_eq!(candidates.as_ptr(), backing);
                assert_eq!(
                    (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                    pointers
                );
            });
        }
    }
}
#[test]
fn original_digest_containers_have_no_id_and_delimiter_line_endings_are_excluded() {
    exercise(SOURCE, 17, |mut cursor| {
        let mut previous = costs(&cursor);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        let mut current = costs(&cursor);
        assert_eq!(
            std::array::from_fn::<_, 5, _>(|i| previous[i] - current[i]),
            [0, 1, 0, 2, 0]
        );
        previous = current;
        assert!(cursor.value().is_none());
        assert_eq!(cursor.candidates[0].ordinal(), 1);
        assert!(cursor.candidates[0].locator.is_none());
        assert!(cursor.candidates[0].wire().is_none());
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        current = costs(&cursor);
        assert_eq!(
            std::array::from_fn::<_, 5, _>(|i| previous[i] - current[i]),
            [0, 69, 0, 5, 69]
        );
        previous = current;
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
        current = costs(&cursor);
        assert_eq!(
            std::array::from_fn::<_, 5, _>(|i| previous[i] - current[i]),
            [0, 69, 0, 5, 69]
        );
        let view = cursor.value().unwrap();
        assert_eq!(
            view.candidates
                .iter()
                .map(|c| c.ordinal)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        for (candidate, suffix) in view.candidates[1..].iter().zip([
            "0000000000000079000000000000000400",
            "00000000000000bb000000000000000400",
        ]) {
            assert_eq!(
                candidate.wire().unwrap(),
                format!("p1_44444444444444444444444444444444{suffix}")
            );
            assert_eq!(candidate.locator.unwrap().length, 4);
        }
        cursor.finish(Tick(1)).unwrap();
    });
    exercise(
        b"Content-Type: multipart/mixed; boundary=a\n\n--a\n\nab\n--a--\n",
        17,
        |mut cursor| {
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
            let mapped = cursor.finish(Tick(1)).unwrap();
            let candidate = mapped.value().unwrap().candidates[1];
            assert_eq!(candidate.ordinal(), 2);
            assert_eq!(candidate.locator().unwrap().length, 2);
            assert_eq!(
                candidate.wire().unwrap(),
                "p1_444444444444444444444444444444440000000000000030000000000000000200"
            );
        },
    );
}
#[test]
fn every_original_prefix_requires_fresh_complete_consumption() {
    for prefix in 0..=3 {
        for fault in 0..4 {
            exercise(SOURCE, 17, |mut cursor| {
                for _ in 0..prefix {
                    cursor.poll(Tick(1)).unwrap();
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
                } else if prefix == 3 {
                    cursor.finish(Tick(1)).unwrap();
                } else {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                }
            });
        }
    }
}
#[test]
fn mapped_expiry_hides_values_and_sticks_through_release() {
    for explicit in [false, true] {
        exercise(SIMPLE, 0, |mut cursor| {
            cursor.poll(Tick(1)).unwrap();
            let mut mapped = cursor.finish(Tick(1)).unwrap();
            let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
            if explicit {
                assert_eq!(mapped.check_deadline(Tick(100)), Err(error));
                assert!(mapped.value().is_none());
                assert_eq!(mapped.finish(Tick(1)).err(), Some(error));
            } else {
                assert_eq!(mapped.finish(Tick(100)).err(), Some(error));
            }
        });
    }
}
#[test]
fn positive_cutoffs_preserve_reused_slots_exact_error_and_no_source_io() {
    let used = exercise(SIMPLE, 0, |mut cursor| {
        let before = costs(&cursor);
        cursor.poll(Tick(1)).unwrap();
        let after = costs(&cursor);
        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i])
    });
    assert_eq!(used, [0, 69, 0, 6, 69]);
    for (counter, &count) in used.iter().enumerate() {
        for cutoff in 0..=count {
            exercise(SIMPLE, 0, |mut cursor| {
                let structure = &mut cursor.source.original.source.projected.structure;
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
                            &mut 0,
                        )
                        .unwrap();
                } else {
                    let left = structure.work.remaining();
                    structure
                        .work
                        .charge(
                            Tick(1),
                            Charge {
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
                let stale = Candidate {
                    ordinal: 99,
                    locator: Some(PartLocator {
                        parent: BlobId::from_bytes([0x55; 16]),
                        offset: 999,
                        length: 999,
                        encoding: TransferEncoding::Base64,
                    }),
                    wire: [b'A'; PartLocator::WIRE_BYTES],
                };
                cursor.candidates[0] = stale;
                let result = cursor.poll(Tick(1));
                if cutoff == count {
                    assert_eq!(result, Ok(Status::Complete));
                    let mapped = cursor.finish(Tick(1)).unwrap();
                    let candidate = mapped.value().unwrap().candidates[0];
                    assert_eq!(candidate.ordinal, 1);
                    assert_eq!(candidate.locator.unwrap().parent, PARENT);
                    assert_ne!(candidate.wire, stale.wire);
                } else {
                    let error = match counter {
                        1 => Error::Admission(crate::nfc::Error::InterpretationLimit),
                        3 => Error::Admission(crate::nfc::Error::Work(Stop::Records)),
                        4 => Error::Admission(crate::nfc::Error::Work(Stop::OutputBytes)),
                        _ => panic!("nondebited counter refused"),
                    };
                    assert_eq!(result, Err(error));
                    assert_eq!(cursor.candidates[0].ordinal, stale.ordinal);
                    assert_eq!(cursor.candidates[0].locator, stale.locator);
                    assert_eq!(cursor.candidates[0].wire, stale.wire);
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(100)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                }
            });
        }
    }
}

#[test]
fn constructor_freshness_precedes_exact_candidate_capacity_and_inherited_refusal() {
    for size in [0, 2] {
        for late in [false, true] {
            exercise(SIMPLE, 0, |cursor| {
                let source = cursor.source;
                let mut cells = [Candidate::default(); 2];
                let expected = if late {
                    Error::Admission(crate::nfc::Error::Work(Stop::Deadline))
                } else if size == 0 {
                    Error::ResponseCapacity
                } else {
                    Error::InvalidState
                };
                assert_eq!(
                    Cursor::new(
                        source,
                        PARENT,
                        &mut cells[..size],
                        if late { Tick(100) } else { Tick(1) }
                    )
                    .err(),
                    Some(expected)
                );
            });
        }
    }
    exercise(SIMPLE, 0, |cursor| {
        let mut source = cursor.source;
        let error = Error::Admission(crate::nfc::Error::Work(Stop::Deadline));
        assert_eq!(source.check_deadline(Tick(100)), Err(error));
        let mut cell = [Candidate::default()];
        assert_eq!(
            Cursor::new(source, PARENT, &mut cell, Tick(1)).err(),
            Some(error)
        );
    });
}
