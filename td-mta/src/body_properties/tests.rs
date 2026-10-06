#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::Deadline;
const NAMES: &[&str] = &[
    "partId",
    "blobId",
    "size",
    "headers",
    "name",
    "type",
    "charset",
    "disposition",
    "cid",
    "language",
    "location",
    "subParts",
];
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    )
}
fn complete(cursor: &mut Cursor<'_, '_, '_>, meter: &mut Meter) -> Result<(), Error> {
    for _ in 0..20000 {
        let before = meter.remaining();
        let result = cursor.poll(Tick(1), meter);
        let after = meter.remaining();
        assert!(before.io_bytes - after.io_bytes <= 184);
        assert!(before.records - after.records <= 32);
        assert_eq!(
            (before.output_bytes, before.unlinks),
            (after.output_bytes, after.unlinks)
        );
        match result? {
            Status::Yield => assert!(cursor.value().is_none()),
            Status::Complete => return Ok(()),
        }
    }
    panic!("body properties stalled")
}
#[test]
fn literal_fields_preserve_twelve_independent_choices_and_metadata_defaults() {
    for (key, expected) in [
        (
            "partId",
            Fields {
                metadata: Metadata {
                    part_id: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "blobId",
            Fields {
                metadata: Metadata {
                    blob_id: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "size",
            Fields {
                metadata: Metadata {
                    size: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "headers",
            Fields {
                headers: true,
                ..Fields::NONE
            },
        ),
        (
            "name",
            Fields {
                metadata: Metadata {
                    name: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "type",
            Fields {
                metadata: Metadata {
                    media_type: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "charset",
            Fields {
                metadata: Metadata {
                    charset: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "disposition",
            Fields {
                metadata: Metadata {
                    disposition: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "cid",
            Fields {
                metadata: Metadata {
                    cid: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "language",
            Fields {
                metadata: Metadata {
                    language: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "location",
            Fields {
                metadata: Metadata {
                    location: true,
                    ..Metadata::NONE
                },
                ..Fields::NONE
            },
        ),
        (
            "subParts",
            Fields {
                sub_parts: true,
                ..Fields::NONE
            },
        ),
    ] {
        let keys = [key];
        let mut cells = [];
        let mut cursor = Cursor::new(&keys, &mut cells);
        assert!(cursor.value().is_none());
        complete(&mut cursor, &mut work()).unwrap();
        let value = cursor.finish().unwrap();
        assert_eq!(value.fields(), expected);
        assert!(value.headers().is_empty());
    }
    let mut cells = [];
    let mut cursor = Cursor::new(NAMES, &mut cells);
    complete(&mut cursor, &mut work()).unwrap();
    assert_eq!(
        cursor.finish().unwrap().fields(),
        Fields {
            metadata: Metadata::ALL,
            headers: true,
            sub_parts: true
        }
    );
    assert_eq!(
        Fields::DEFAULT,
        Fields {
            metadata: Metadata::ALL,
            headers: false,
            sub_parts: false
        }
    );
}
#[test]
fn explicit_empty_is_fresh_complete_without_work_and_cached_selection_is_passive() {
    let mut cells = [];
    let mut cursor = Cursor::new(&[], &mut cells);
    let mut meter = work();
    let before = meter.remaining();
    assert_eq!(cursor.value().unwrap().fields(), Fields::NONE);
    assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
    assert_eq!(meter.remaining(), before);
    assert_eq!(cursor.finish().unwrap().fields(), Fields::NONE);
    let keys = ["size", "size", "partId"];
    let mut cells = [];
    let mut cursor = Cursor::new(&keys, &mut cells);
    complete(&mut cursor, &mut meter).unwrap();
    let before = meter.remaining();
    assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
    assert_eq!(meter.remaining(), before);
    assert_eq!(
        cursor.finish().unwrap().fields().metadata,
        Metadata {
            size: true,
            part_id: true,
            ..Metadata::NONE
        }
    );
}
#[test]
fn headers_deduplicate_exact_requested_keys_and_keep_first_loan_and_spelling() {
    let first = String::from("header:X");
    let duplicate = String::from("header:X");
    let keys = [
        "size",
        first.as_str(),
        duplicate.as_str(),
        "header:x",
        "header:X:asRaw",
        "header:X:asText:all",
        "size",
    ];
    let mut cells = std::array::from_fn::<_, 5, _>(|_| Cell::new());
    let mut cursor = Cursor::new(&keys, &mut cells);
    complete(&mut cursor, &mut work()).unwrap();
    let value = cursor.finish().unwrap();
    assert_eq!(
        value.fields().metadata,
        Metadata {
            size: true,
            ..Metadata::NONE
        }
    );
    assert!(!value.fields().headers);
    assert_eq!(value.headers().len(), 4);
    for (cell, key) in value.headers().iter().zip([
        first.as_str(),
        "header:x",
        "header:X:asRaw",
        "header:X:asText:all",
    ]) {
        assert_eq!(cell.property().unwrap().requested(), key);
        assert_eq!(cell.property().unwrap().requested().as_ptr(), key.as_ptr());
    }
    assert_eq!(
        value.headers()[0].property().unwrap().requested().as_ptr(),
        first.as_ptr()
    );
    assert_eq!(
        value.headers()[3].property().unwrap().form(),
        header_property::Form::Text
    );
    assert_eq!(
        value.headers()[3].property().unwrap().occurrence(),
        header_property::Occurrence::All
    );
}
#[test]
fn standard_selection_debits_equal_each_original_recognizer_even_for_duplicates() {
    let keys = ["size", "partId", "size", "subParts", "headers"];
    let mut expected = work();
    for key in keys {
        let mut cursor = body_property::Cursor::new(key);
        loop {
            if matches!(
                cursor.poll(Tick(1), &mut expected).unwrap(),
                body_property::Status::Complete(_)
            ) {
                break;
            }
        }
    }
    let mut cells = [];
    let mut actual = work();
    let mut cursor = Cursor::new(&keys, &mut cells);
    complete(&mut cursor, &mut actual).unwrap();
    assert_eq!(actual.remaining(), expected.remaining());
    assert_eq!(
        cursor.finish().unwrap().fields(),
        Fields {
            metadata: Metadata {
                size: true,
                part_id: true,
                ..Metadata::NONE
            },
            headers: true,
            sub_parts: true
        }
    );
}
#[test]
fn unknown_alias_and_forbidden_keys_prevent_whole_value_and_remain_sticky() {
    for (key, error) in [
        ("unknown", Error::InvalidProperty),
        ("subject", Error::InvalidProperty),
        ("SIZE", Error::InvalidProperty),
        (
            "header:X:asBroken",
            Error::Recognition(body_property::Error::InvalidProperty),
        ),
        (
            "header:From:asDate",
            Error::Recognition(body_property::Error::ForbiddenForm),
        ),
    ] {
        let keys = ["size", key];
        let mut cells = [];
        let mut cursor = Cursor::new(&keys, &mut cells);
        let mut meter = work();
        assert_eq!(complete(&mut cursor, &mut meter), Err(error));
        assert!(cursor.value().is_none());
        let before = meter.remaining();
        assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
        assert_eq!(meter.remaining(), before);
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        assert_eq!(cursor.finish().err(), Some(error));
    }
}
#[test]
fn capacity_reuse_and_spare_cells_preserve_prefix_and_tail_without_whole_value() {
    let keys = ["header:A", "header:B"];
    let mut empty = [];
    let mut cursor = Cursor::new(&keys, &mut empty);
    assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
    assert!(cursor.value().is_none());
    assert_eq!(cursor.finish().err(), Some(Error::Capacity));
    let mut cells = [Cell::new()];
    let mut cursor = Cursor::new(&keys, &mut cells);
    assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
    let mut meter = work();
    let before = meter.remaining();
    assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::Capacity));
    assert_eq!(meter.remaining(), before);
    assert_eq!(cells[0].property().unwrap().requested(), "header:A");
    let mut parsed = body_property::Cursor::new("header:Sentinel");
    let sentinel = loop {
        if let body_property::Status::Complete(Some(body_property::Property::Header(p))) =
            parsed.poll(Tick(1), &mut work()).unwrap()
        {
            break p;
        }
    };
    let mut cells = std::array::from_fn::<_, 3, _>(|_| Cell {
        property: Some(sentinel),
    });
    let ptr = cells.as_ptr();
    let mut cursor = Cursor::new(&keys, &mut cells);
    complete(&mut cursor, &mut work()).unwrap();
    let view = cursor.finish().unwrap();
    assert_eq!(view.headers().as_ptr(), ptr);
    assert_eq!(view.headers().len(), 2);
    assert_eq!(
        view.headers()[0].property().unwrap().requested(),
        "header:A"
    );
    assert_eq!(
        view.headers()[1].property().unwrap().requested(),
        "header:B"
    );
    assert_eq!(cells[2].property(), Some(sentinel));
}
#[test]
fn every_work_cut_is_sticky_and_premature_completion_never_exposes_selection() {
    for keys in [
        ["size", "header:X", "header:X"],
        ["header:X", "header:XY", "header:XY"],
    ] {
        let mut cells = [Cell::new(), Cell::new()];
        let mut cursor = Cursor::new(&keys, &mut cells);
        let mut baseline = work();
        let before = baseline.remaining();
        complete(&mut cursor, &mut baseline).unwrap();
        let after = baseline.remaining();
        let visits = before.io_bytes - after.io_bytes;
        let records = before.records - after.records;
        let mut exact_cells = [Cell::new(), Cell::new()];
        let mut exact_cursor = Cursor::new(&keys, &mut exact_cells);
        let mut exact = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: visits,
                records,
                ..Charge::default()
            },
        );
        complete(&mut exact_cursor, &mut exact).unwrap();
        assert_eq!(exact.remaining().io_bytes, 0);
        assert_eq!(exact.remaining().records, 0);
        assert!(exact_cursor.value().is_some());
        for (bytes, steps, error) in [
            (visits - 1, records, Stop::IoBytes),
            (visits, records - 1, Stop::Records),
        ] {
            let mut cells = [Cell::new(), Cell::new()];
            let mut cursor = Cursor::new(&keys, &mut cells);
            let mut meter = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: bytes,
                    records: steps,
                    ..Charge::default()
                },
            );
            let error = Error::from(error);
            assert_eq!(complete(&mut cursor, &mut meter), Err(error));
            assert!(cursor.value().is_none());
            let before = meter.remaining();
            assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
            assert_eq!(meter.remaining(), before);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
        }
        let mut cells = [Cell::new(), Cell::new()];
        let mut cursor = Cursor::new(&keys, &mut cells);
        let mut meter = work();
        let mut turns = 0;
        loop {
            turns += 1;
            if cursor.poll(Tick(1), &mut meter).unwrap() == Status::Complete {
                break;
            }
        }
        for cut in 0..turns {
            let mut cells = [Cell::new(), Cell::new()];
            let mut cursor = Cursor::new(&keys, &mut cells);
            let mut meter = work();
            for _ in 0..cut {
                assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
            }
            let before = meter.remaining();
            let error = Error::from(Stop::Deadline);
            assert_eq!(cursor.poll(Tick(100), &mut meter), Err(error));
            assert!(cursor.value().is_none());
            assert_eq!(meter.remaining(), before);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
            assert_eq!(cursor.finish().err(), Some(error));
            let mut cells = [Cell::new(), Cell::new()];
            let mut cursor = Cursor::new(&keys, &mut cells);
            let mut meter = work();
            for _ in 0..cut {
                cursor.poll(Tick(1), &mut meter).unwrap();
            }
            assert_eq!(cursor.finish().err(), Some(Error::InvalidState));
        }
    }
}
#[test]
fn long_duplicate_headers_prepay_bounded_two_sided_comparisons_without_copying() {
    let header = format!("header:{}:asText:all", "X".repeat(65536));
    let keys = [header.as_str(), header.as_str()];
    let mut cells = [Cell::new()];
    let mut cursor = Cursor::new(&keys, &mut cells);
    let mut meter = work();
    let mut comparisons = 0;
    loop {
        let visits = match cursor.phase {
            Phase::Header {
                slot: 0, offset, ..
            } if cursor.used == 1 => Some((header.len() - offset).min(64) * 2),
            _ => None,
        };
        let before = meter.remaining();
        let status = cursor.poll(Tick(1), &mut meter).unwrap();
        let after = meter.remaining();
        if let Some(visits) = visits {
            comparisons += 1;
            assert_eq!(before.io_bytes - after.io_bytes, visits as u64);
            assert_eq!(before.records - after.records, 1);
        }
        if status == Status::Complete {
            break;
        }
    }
    assert_eq!(comparisons, header.len().div_ceil(64));
    let view = cursor.finish().unwrap();
    assert_eq!(view.headers().len(), 1);
    assert_eq!(
        view.headers()[0].property().unwrap().requested().as_ptr(),
        header.as_ptr()
    );
}
pub fn probe(mut snapshot: impl FnMut()) {
    let long = format!("header:{}:asText:all", "X".repeat(65536));
    for trial in 0..8 {
        let long_keys = [long.as_str(), long.as_str()];
        let usual = ["size", "header:X", "header:X"];
        let keys: &[&str] = match trial {
            0 => NAMES,
            1 => &long_keys,
            2 => &[],
            3 => &["size", "unknown"],
            4 => &["header:From:asDate"],
            5 => &["header:X"],
            _ => &usual,
        };
        let mut cells = std::array::from_fn::<_, 2, _>(|_| Cell::new());
        let cells = if trial == 5 {
            &mut cells[..0]
        } else {
            &mut cells[..]
        };
        let mut cursor = Cursor::new(keys, cells);
        let mut meter = work();
        if trial >= 5 {
            loop {
                if matches!(cursor.phase, Phase::Header { .. }) && (trial == 5 || cursor.used == 1)
                {
                    break;
                }
                assert_eq!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield));
            }
            if trial == 6 {
                meter = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes: 1000,
                        records: 0,
                        ..Charge::default()
                    },
                );
            }
        }
        snapshot();
        match trial {
            0 => {
                complete(&mut cursor, &mut meter).unwrap();
                assert_eq!(
                    cursor.finish().unwrap().fields(),
                    Fields {
                        metadata: Metadata::ALL,
                        headers: true,
                        sub_parts: true
                    }
                );
            }
            1 => {
                complete(&mut cursor, &mut meter).unwrap();
                assert_eq!(cursor.finish().unwrap().headers().len(), 1);
            }
            2 => {
                let before = meter.remaining();
                assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
                assert_eq!(meter.remaining(), before);
                assert_eq!(cursor.finish().unwrap().fields(), Fields::NONE);
            }
            3 => assert_eq!(
                complete(&mut cursor, &mut meter),
                Err(Error::InvalidProperty)
            ),
            4 => assert_eq!(
                complete(&mut cursor, &mut meter),
                Err(Error::Recognition(body_property::Error::ForbiddenForm))
            ),
            5 => assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::Capacity)),
            6 => assert_eq!(
                cursor.poll(Tick(1), &mut meter),
                Err(Error::from(Stop::Records))
            ),
            7 => assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::from(Stop::Deadline))
            ),
            _ => panic!("unknown selection trial"),
        }
        snapshot();
    }
}

#[test]
fn different_lengths_and_insertions_pay_exact_records_without_byte_visits() {
    let keys = ["header:X", "header:XY", "header:XY"];
    let mut cells = [Cell::new(), Cell::new()];
    let mut cursor = Cursor::new(&keys, &mut cells);
    let mut meter = work();
    let mut header_turns = 0;
    loop {
        let expected = match &cursor.phase {
            Phase::Header { slot, .. } if *slot >= cursor.used => Some(0),
            Phase::Header {
                candidate, slot, ..
            } => {
                let existing = cursor.cells[*slot].property().unwrap().requested();
                Some(if existing.len() == candidate.requested().len() {
                    18
                } else {
                    0
                })
            }
            _ => None,
        };
        let before = meter.remaining();
        let status = cursor.poll(Tick(1), &mut meter).unwrap();
        let after = meter.remaining();
        if let Some(visits) = expected {
            header_turns += 1;
            assert_eq!(before.io_bytes - after.io_bytes, visits);
            assert_eq!(before.records - after.records, 1);
        }
        if status == Status::Complete {
            break;
        }
    }
    assert_eq!(header_turns, 5);
    let view = cursor.finish().unwrap();
    assert_eq!(view.headers().len(), 2);
}
