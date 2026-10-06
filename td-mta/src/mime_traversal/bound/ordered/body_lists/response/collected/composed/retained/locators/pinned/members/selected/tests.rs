#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::tests::TestClock;
use super::super::tests::with_bound;
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    ports::{BlobReader, Error as PolicyError},
};
const SIMPLE: &[u8] = b"\r\nabc\r\n";
const MULTIPART: &[u8] =
    b"Content-Type: multipart/mixed;boundary=x\r\n\r\n--x\r\n\r\nabc\r\n--x--\r\n";
fn properties(bits: u16) -> Properties {
    Properties {
        part_id: bits & 1 != 0,
        size: bits & 2 != 0,
        media_type: bits & 4 != 0,
        charset: bits & 8 != 0,
        name: bits & 16 != 0,
        disposition: bits & 32 != 0,
        cid: bits & 64 != 0,
        language: bits & 128 != 0,
        location: bits & 256 != 0,
        blob_id: bits & 512 != 0,
    }
}
fn costs(cursor: &super::super::Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>) -> [u64; 5] {
    let structure = &cursor
        .source
        .original
        .source
        .original
        .source
        .projected
        .structure;
    let left = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
fn drain(cursor: &mut Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>, width: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..10000 {
        let mut output = [0xa5; 128];
        let before = costs(&cursor.inner);
        let scanning = cursor
            .inner
            .selection
            .as_ref()
            .is_some_and(|index| !index.ready());
        let credit = cursor.inner.credit;
        let steps = if scanning {
            cursor
                .inner
                .selection
                .as_ref()
                .unwrap()
                .remaining(cursor.inner.segments().unwrap()[0])
                .unwrap()
                .min(64) as u64
        } else {
            1
        };
        let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
        let after = costs(&cursor.inner);
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2], after[2]);
        if before == after {
            assert_eq!(progress.status, Status::Complete);
        } else {
            assert_eq!(before[1] - after[1], steps);
            assert_eq!(
                before[3] - after[3],
                1 + steps.saturating_sub(u64::from(credit)).div_ceil(16)
            );
            assert_eq!(before[4] - after[4], progress.written as u64);
        }
        assert!(progress.written <= 64);
        assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
        bytes.extend_from_slice(&output[..progress.written]);
        if progress.status == Status::Complete {
            if cursor.properties != Properties::NONE {
                assert!(progress.written > 0);
            }
            return bytes;
        }
    }
    panic!("selected part did not complete")
}
const LEAF: [&[u8]; 10] = [
    b"\"partId\":\"1\"",
    b"\"size\":5",
    b"\"type\":\"text/plain\"",
    b"\"charset\":\"us-ascii\"",
    b"\"name\":null",
    b"\"disposition\":null",
    b"\"cid\":null",
    b"\"language\":null",
    b"\"location\":null",
    b"\"blobId\":\"p1_444444444444444444444444444444440000000000000002000000000000000500\"",
];
const CONTAINER: [&[u8]; 10] = [
    b"\"partId\":null",
    b"\"size\":19",
    b"\"type\":\"multipart/mixed\"",
    b"\"charset\":null",
    b"\"name\":null",
    b"\"disposition\":null",
    b"\"cid\":null",
    b"\"language\":null",
    b"\"location\":null",
    b"\"blobId\":null",
];
fn expected_container(bits: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (field, member) in CONTAINER.into_iter().enumerate() {
        if bits & (1 << field) != 0 {
            if !bytes.is_empty() {
                bytes.push(b',');
            }
            bytes.extend_from_slice(member);
        }
    }
    bytes
}
fn expected(bits: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (field, member) in LEAF.into_iter().enumerate() {
        if bits & (1 << field) != 0 {
            if !bytes.is_empty() {
                bytes.push(b',');
            }
            bytes.extend_from_slice(member);
        }
    }
    bytes
}
#[test]
fn all_ten_field_subsets_match_literal_members_without_separators_at_edges() {
    let fragment = LEAF[..9].join(&b',');
    let blob = LEAF[9];
    let mut suffix = vec![b','];
    suffix.extend_from_slice(blob);
    let segments: [&[u8]; 5] = [&fragment, &suffix, b"", b"", b""];
    for bits in 0..1024 {
        let mut index = Index::new(bits, &fragment).unwrap();
        while !index.ready() {
            let count = index.remaining(&fragment).unwrap().min(64);
            index.scan(segments, count).unwrap();
        }
        let bytes = (0..index.total(segments).unwrap())
            .map(|offset| index.byte_at(offset, segments).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(bytes, expected(bits));
    }
    for bits in [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 513, 1023] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut cursor = Cursor::new(bound, 1, properties(bits), Tick(1)).unwrap();
            assert_eq!(drain(&mut cursor, 64), expected(bits));
            assert_eq!(cursor.value(), Some((properties(bits), 1)));
            let member = cursor.finish(Tick(1)).unwrap();
            assert_eq!(member.value(), Some((properties(bits), 1)));
            let (_, ordinal, actual) = member.finish(Tick(1)).unwrap();
            assert_eq!(ordinal, 1);
            assert_eq!(actual, properties(bits));
        });
    }
}
#[test]
fn selected_container_null_and_shifted_leaf_locator_preserve_descriptor_custody() {
    for source in [SIMPLE, MULTIPART] {
        for base in [0, 17, u64::MAX - source.len() as u64] {
            for bits in [0, 1, 128, 256, 512, 513, 1023] {
                let clock = TestClock::new();
                with_bound(source, base, &clock, |bound| {
                    let source_ptr = bound
                        .original
                        .source
                        .original
                        .source
                        .projected
                        .structure
                        .source
                        .as_ptr();
                    let mut cursor = Cursor::new(bound, 1, properties(bits), Tick(1)).unwrap();
                    let bytes = drain(&mut cursor, 7);
                    if source == MULTIPART {
                        assert_eq!(bytes, expected_container(bits));
                    }
                    if source == SIMPLE {
                        assert_eq!(bytes, expected(bits));
                    }
                    let (bound, _, actual) =
                        cursor.finish(Tick(1)).unwrap().finish(Tick(1)).unwrap();
                    assert_eq!(actual, properties(bits));
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
                    let (_, mut parent) = bound.finish(Tick(1)).unwrap();
                    assert!(parent.read_at(0, &mut [0; 2]).unwrap() > 0);
                });
            }
        }
    }
}
#[test]
fn all_preserves_each_legacy_turn_byte_and_five_cost_debit() {
    for source in [SIMPLE, MULTIPART] {
        for width in [1, 7, 64, 128] {
            let clock = TestClock::new();
            let mut legacy = Vec::new();
            with_bound(source, 0, &clock, |bound| {
                let mut cursor = super::super::Cursor::new(bound, 1, Tick(1)).unwrap();
                for _ in 0..10000 {
                    let before = costs(&cursor);
                    let mut out = [0; 128];
                    let p = cursor.poll(Tick(1), &mut out[..width]).unwrap();
                    let after = costs(&cursor);
                    legacy.push((
                        p,
                        out[..p.written].to_vec(),
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                    ));
                    if p.status == Status::Complete {
                        break;
                    }
                }
            });
            with_bound(source, 0, &clock, |bound| {
                let mut cursor = Cursor::new(bound, 1, Properties::ALL, Tick(1)).unwrap();
                for (p, bytes, debits) in legacy {
                    let before = costs(&cursor.inner);
                    let mut out = [0; 128];
                    let actual = cursor.poll(Tick(1), &mut out[..width]).unwrap();
                    let after = costs(&cursor.inner);
                    assert_eq!(actual, p);
                    assert_eq!(&out[..p.written], bytes);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| before[i] - after[i]),
                        debits
                    );
                }
                assert!(cursor.value().is_some());
            });
        }
    }
}
#[test]
fn index_ignores_commas_brackets_and_escaped_quotes_inside_generated_strings() {
    let fragment=b"\"partId\":\"1\",\"size\":5,\"type\":\"text/plain\",\"charset\":\"us-ascii\",\"name\":\"x,]\\\"y\",\"disposition\":null,\"cid\":null,\"language\":[\"en\",\"fr\"],\"location\":\"a,b\"";
    let segments: [&[u8]; 5] = [fragment, b",\"blobId\":", b"", b"null", b""];
    for width in [1, 7, 64] {
        let mut index = Index::new(16 | 128 | 256, fragment).unwrap();
        while !index.ready() {
            let n = index.remaining(fragment).unwrap().min(width);
            index.scan(segments, n).unwrap();
        }
        let bytes = (0..index.total(segments).unwrap())
            .map(|i| index.byte_at(i, segments).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            bytes,
            b"\"name\":\"x,]\\\"y\",\"language\":[\"en\",\"fr\"],\"location\":\"a,b\""
        );
    }
    let mut fields = LEAF[..9].to_vec();
    fields[4] = b"\"name\":\"x\\\\,]y\"";
    fields[7] = b"\"language\":[\"en\",\"fr\"]";
    let fragment = fields.join(&b',');
    let segments: [&[u8]; 5] = [&fragment, b",\"blobId\":", b"", b"null", b""];
    for width in [1, 7, 64] {
        let mut index = Index::new(16 | 128, &fragment).unwrap();
        while !index.ready() {
            let count = index.remaining(&fragment).unwrap().min(width);
            index.scan(segments, count).unwrap();
        }
        let bytes = (0..index.total(segments).unwrap())
            .map(|offset| index.byte_at(offset, segments).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(bytes, [fields[4], fields[7]].join(&b','));
    }
}
#[test]
fn fresh_none_and_constructor_validation_cost_nothing_in_both_deadline_domains() {
    for bits in [0, 1, 512, 1023] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let original = super::super::Cursor::new(bound, 1, Tick(1)).unwrap();
            let before = costs(&original);
            let bound = original.source;
            let mut cursor = Cursor::new(bound, 1, properties(bits), Tick(1)).unwrap();
            assert_eq!(costs(&cursor.inner), before);
            if bits == 0 {
                assert_eq!(
                    cursor.poll(Tick(100), &mut []).unwrap().status,
                    Status::Complete
                );
                assert_eq!(costs(&cursor.inner), before);
            }
        });
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let now = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                let error = if actual {
                    Error::Parent(PolicyError::Deadline)
                } else {
                    Error::Original(super::super::super::super::Error::Admission(
                        crate::nfc::Error::Work(Stop::Deadline),
                    ))
                };
                assert_eq!(
                    Cursor::new(bound, 0, properties(bits), now).err(),
                    Some(error)
                );
            });
        }
    }
}
#[test]
fn index_and_output_prefix_failures_are_sticky_and_complete_owners_stay_fresh() {
    for prefix in [0, 1, 2, 3, 4, 5, 8, 100] {
        for actual in [false, true] {
            let clock = TestClock::new();
            with_bound(SIMPLE, 0, &clock, |bound| {
                let mut cursor = Cursor::new(bound, 1, properties(513), Tick(1)).unwrap();
                for _ in 0..prefix {
                    cursor.poll(Tick(1), &mut [0; 7]).unwrap();
                }
                let now = if actual {
                    clock.expire();
                    Tick(1)
                } else {
                    Tick(100)
                };
                let error = cursor.check_deadline(now).unwrap_err();
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1), &mut [0; 7]), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            });
        }
    }
    for actual in [false, true] {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut cursor = Cursor::new(bound, 1, properties(513), Tick(1)).unwrap();
            drain(&mut cursor, 64);
            let mut member = cursor.finish(Tick(1)).unwrap();
            let now = if actual {
                clock.expire();
                Tick(1)
            } else {
                Tick(100)
            };
            let error = member.check_deadline(now).unwrap_err();
            assert!(member.value().is_none());
            assert_eq!(member.finish(Tick(1)).err(), Some(error));
        });
    }
}
#[test]
fn zero_output_and_premature_finish_do_not_skip_original_admission() {
    let clock = TestClock::new();
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut cursor = Cursor::new(bound, 1, properties(512), Tick(1)).unwrap();
        let before = costs(&cursor.inner);
        assert_eq!(
            cursor.poll(Tick(1), &mut []).unwrap().status,
            Status::NeedOutput
        );
        assert_eq!(costs(&cursor.inner), before);
        assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
    });
    with_bound(SIMPLE, 0, &clock, |bound| {
        let mut cursor = Cursor::new(bound, 1, properties(512), Tick(1)).unwrap();
        clock.expire();
        assert_eq!(
            cursor.poll(Tick(1), &mut []).err(),
            Some(Error::Parent(PolicyError::Deadline))
        );
    });
}
#[test]
fn interpretation_record_and_wire_refusals_precede_selected_copy() {
    for kind in 0..3 {
        let clock = TestClock::new();
        with_bound(SIMPLE, 0, &clock, |bound| {
            let mut cursor = Cursor::new(bound, 1, properties(512), Tick(1)).unwrap();
            if kind == 2 {
                while !cursor.inner.selection.as_ref().unwrap().ready() {
                    cursor.poll(Tick(1), &mut [0; 64]).unwrap();
                }
            }
            let structure = &mut cursor
                .inner
                .source
                .original
                .source
                .original
                .source
                .projected
                .structure;
            let expected = if kind == 0 {
                let left = structure.budget.steps_remaining();
                structure
                    .budget
                    .charge(structure.work, Tick(1), 0, left, &mut 0)
                    .unwrap();
                crate::nfc::Error::InterpretationLimit
            } else {
                let left = structure.work.remaining();
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            records: if kind == 1 { left.records } else { 0 },
                            output_bytes: if kind == 2 { left.output_bytes } else { 0 },
                            ..Charge::default()
                        },
                    )
                    .unwrap();
                crate::nfc::Error::Work(if kind == 1 {
                    Stop::Records
                } else {
                    Stop::OutputBytes
                })
            };
            let mut out = [0xa5; 64];
            let error = cursor.poll(Tick(1), &mut out).unwrap_err();
            assert_eq!(
                error,
                Error::Original(super::super::super::super::Error::Admission(expected))
            );
            assert_eq!(out, [0xa5; 64]);
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1), &mut out), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        });
    }
}

/// Original matching, generated fragments and file setup precede each interval.
pub fn probe(mut snapshot: impl FnMut()) {
    for trial in 0..8 {
        let clock = TestClock::new();
        let base = if trial == 0 {
            u64::MAX - SIMPLE.len() as u64
        } else {
            0
        };
        with_bound(SIMPLE, base, &clock, |mut bound| {
            if trial == 6 {
                let structure = &mut bound.original.source.original.source.projected.structure;
                let left = structure.work.remaining().output_bytes;
                structure
                    .work
                    .charge(
                        Tick(1),
                        Charge {
                            output_bytes: left,
                            ..Charge::default()
                        },
                    )
                    .unwrap();
            }
            if trial == 7 {
                clock.expire();
            }
            let props = properties(if trial == 1 {
                0
            } else if trial == 0 {
                513
            } else {
                512
            });
            let mut output = [0xa5; 64];
            snapshot();
            if trial == 7 {
                assert_eq!(
                    Cursor::new(bound, 1, props, Tick(1)).err(),
                    Some(Error::Parent(PolicyError::Deadline))
                );
            } else {
                let mut cursor = Cursor::new(bound, 1, props, Tick(1)).unwrap();
                if trial == 1 {
                    assert_eq!(
                        cursor.poll(Tick(100), &mut []).unwrap().status,
                        Status::Complete
                    );
                } else if trial == 6 {
                    let error = loop {
                        match cursor.poll(Tick(1), &mut output) {
                            Ok(p) => assert_eq!(p.written, 0),
                            Err(error) => break error,
                        }
                    };
                    assert_eq!(
                        error,
                        Error::Original(super::super::super::super::Error::Admission(
                            crate::nfc::Error::Work(Stop::OutputBytes)
                        ))
                    );
                    assert_eq!(output, [0xa5; 64]);
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    snapshot();
                    return;
                } else {
                    loop {
                        let progress = cursor.poll(Tick(1), &mut output).unwrap();
                        if progress.written > 0 {
                            assert_eq!(progress.status, Status::Yield);
                            break;
                        }
                    }
                    if trial == 2 {
                        clock.expire();
                        let error = Error::Parent(PolicyError::Deadline);
                        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                        snapshot();
                        return;
                    }
                    while cursor.poll(Tick(1), &mut output).unwrap().status != Status::Complete {}
                }
                if trial == 3 {
                    clock.expire();
                    assert_eq!(
                        cursor.finish(Tick(1)).err(),
                        Some(Error::Parent(PolicyError::Deadline))
                    );
                } else {
                    let member = cursor.finish(Tick(1)).unwrap();
                    assert_eq!(member.value(), Some((props, 1)));
                    if trial == 4 {
                        clock.expire();
                        assert_eq!(
                            member.finish(Tick(1)).err(),
                            Some(Error::Parent(PolicyError::Deadline))
                        );
                    } else {
                        let (mut bound, ordinal, actual) = member.finish(Tick(1)).unwrap();
                        assert_eq!(ordinal, 1);
                        assert_eq!(actual, props);
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
            snapshot();
        });
    }
}

#[test]
fn malformed_generated_metadata_refuses_canonical_order_and_scan_structure() {
    let valid = String::from_utf8(LEAF[..9].join(&b',')).unwrap();
    let mut reordered = LEAF[..9].to_vec();
    reordered.swap(0, 1);
    let cases = [
        valid.replacen("partId", "partID", 1).into_bytes(),
        reordered.join(&b','),
        format!("{valid},\"extra\":null").into_bytes(),
        valid.replace("\"name\":null", "\"name\":{}").into_bytes(),
        valid
            .replace("\"language\":null", "\"language\":[[null]]")
            .into_bytes(),
        valid
            .replace("\"location\":null", "\"location\":\"unterminated")
            .into_bytes(),
        valid
            .replace("\"location\":null", "\"location\":\"unterminated\\")
            .into_bytes(),
        valid
            .replace("\"location\":null", "\"location\":[")
            .into_bytes(),
        format!("{valid},").into_bytes(),
    ];
    for fragment in cases {
        for width in [1, 7, 64] {
            let segments: [&[u8]; 5] = [&fragment, b",\"blobId\":", b"", b"null", b""];
            let mut index = Index::new(1, &fragment).unwrap();
            let error = loop {
                let count = index.remaining(&fragment).unwrap().min(width);
                match index.scan(segments, count) {
                    Ok(()) => assert!(!index.ready()),
                    Err(error) => break error,
                }
            };
            assert_eq!(error, Error::InvalidState);
        }
    }
}
