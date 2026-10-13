#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::time::Deadline;
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100000,
            records: 100000,
            output_bytes: 100000,
            unlinks: 0,
        },
    )
}
fn encode(value: &str, width: usize) -> Vec<u8> {
    let mut meter = meter();
    let mut cursor = Cursor::new(value, Tick(1), &mut meter).unwrap();
    let mut result = Vec::new();
    for _ in 0..10000 {
        let mut output = vec![0; width];
        let step = cursor.poll(&mut output, Tick(1), &mut meter).unwrap();
        assert!(step.written <= 75);
        result.extend_from_slice(&output[..step.written]);
        if step.status == Status::Complete {
            cursor.finish(Tick(1), &mut meter).unwrap();
            return result;
        }
    }
    panic!("header failed to complete");
}
#[test]
fn literals_and_backpressure() {
    for (input, expected) in [
        ("", ""),
        ("hello", "\r\n =?UTF-8?B?aGVsbG8=?="),
        ("hé ☃", "\r\n =?UTF-8?B?aMOpIOKYgw==?="),
    ] {
        for width in [1, 2, 3, 74, 75, 4096] {
            assert_eq!(encode(input, width), expected.as_bytes());
        }
    }
}
#[test]
fn word_limits_preserve_scalar_boundaries_and_whitespace() {
    for value in [
        "a".repeat(46),
        "☃".repeat(31),
        "🦀".repeat(30),
        format!("{}🦀 then  space ", "a".repeat(44)),
    ] {
        for width in [1, 17, 4096] {
            let wire = encode(&value, width);
            let mut decoded = Vec::new();
            for line in wire.split(|b| *b == b'\n').skip(1) {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                assert!(line.len() <= 73);
                let payload = line
                    .strip_prefix(b" =?UTF-8?B?")
                    .unwrap()
                    .strip_suffix(b"?=")
                    .unwrap();
                let mut decoder = crate::base64::Decoder::default();
                let mut output = [0; 45];
                let mut work = meter();
                let step = decoder
                    .poll(payload, &mut output, true, Tick(1), &mut work)
                    .unwrap();
                assert_eq!(step.status, crate::base64::Status::Complete);
                assert!(!decoder.is_encoding_problem());
                std::str::from_utf8(&output[..step.written]).unwrap();
                decoded.extend_from_slice(&output[..step.written]);
            }
            assert_eq!(decoded, value.as_bytes());
        }
    }
}
#[test]
fn invalid_text_and_original_budget_refuse_stickily() {
    for value in [
        "\t",
        "a\tb",
        "a\r\nb",
        "a\0b",
        "\u{85}",
        "\u{fdd0}",
        "\u{10ffff}",
    ] {
        let mut work = meter();
        let mut cursor = Cursor::new(value, Tick(1), &mut work).unwrap();
        assert_eq!(
            cursor.poll(&mut [0; 100], Tick(1), &mut work),
            Err(Error::InvalidText)
        );
        let mut replacement = meter();
        let before = replacement.remaining();
        assert_eq!(
            cursor.poll(&mut [], Tick(1), &mut replacement),
            Err(Error::InvalidText)
        );
        assert_eq!(replacement.remaining(), before);
        assert_eq!(cursor.finish(Tick(1), &mut work), Err(Error::InvalidText));
    }
    for stop in [Stop::IoBytes, Stop::OutputBytes, Stop::Records] {
        for cap in 0..4 {
            let mut capacity = meter().remaining();
            match stop {
                Stop::IoBytes => capacity.io_bytes = cap,
                Stop::OutputBytes => capacity.output_bytes = cap,
                _ => capacity.records = cap.min(1),
            }
            let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            let mut cursor = Cursor::new("hello", Tick(1), &mut work).unwrap();
            assert_eq!(
                cursor.poll(&mut [0; 100], Tick(1), &mut work),
                Err(Error::Work(stop))
            );
            let mut replacement = meter();
            let before = replacement.remaining();
            assert_eq!(
                cursor.poll(&mut [], Tick(1), &mut replacement),
                Err(Error::Work(stop))
            );
            assert_eq!(replacement.remaining(), before);
        }
    }
}
#[test]
fn cached_completion_finish_and_owner_checks_stay_fresh() {
    let mut work = meter();
    let mut cursor = Cursor::new("hello", Tick(1), &mut work).unwrap();
    assert_eq!(
        cursor.poll(&mut [], Tick(1), &mut work).unwrap().status,
        Status::NeedOutput
    );
    assert_eq!(cursor.finish(Tick(1), &mut work), Err(Error::Incomplete));
    for replacement in [false, true] {
        let mut work = meter();
        let mut cursor = Cursor::new("", Tick(1), &mut work).unwrap();
        assert_eq!(
            cursor.poll(&mut [], Tick(1), &mut work).unwrap().status,
            Status::Complete
        );
        let mut other = meter();
        let before = other.remaining();
        if replacement {
            assert_eq!(cursor.finish(Tick(1), &mut other), Err(Error::InvalidOwner));
            assert_eq!(other.remaining(), before);
        } else {
            assert_eq!(
                cursor.finish(Tick(100), &mut work),
                Err(Error::Work(Stop::Deadline))
            );
        }
    }
    let mut work = meter();
    let mut cursor = Cursor::new("", Tick(1), &mut work).unwrap();
    cursor.poll(&mut [], Tick(1), &mut work).unwrap();
    let before = work.remaining();
    cursor.poll(&mut [], Tick(1), &mut work).unwrap();
    assert_eq!(
        work.remaining(),
        Charge {
            records: before.records - 1,
            ..before
        }
    );
    let mut other = meter();
    let before = other.remaining();
    assert_eq!(
        cursor.poll(&mut [], Tick(1), &mut other),
        Err(Error::InvalidOwner)
    );
    assert_eq!(other.remaining(), before);
    assert_eq!(cursor.finish(Tick(1), &mut work), Err(Error::InvalidOwner));
}

#[test]
fn folded_words_round_trip_through_subject_reader() {
    use crate::{header_property, header_select, header_value, nfc};
    for value in ["", "hé ☃", "two  spaces", &"🦀 then spaces ".repeat(20)] {
        let mut source = b"Subject:".to_vec();
        source.extend_from_slice(&encode(value, 1));
        source.extend_from_slice(b"\r\n\r\n");
        let mut work = meter();
        let mut property = header_property::Cursor::new("subject", header_property::Context::Email);
        let property = loop {
            if let header_property::Status::Complete(property) =
                property.poll(Tick(1), &mut work).unwrap()
            {
                break property.unwrap();
            }
        };
        let mut scratch = nfc::Scratch::new();
        let mut budget = nfc::HeaderBudget::new();
        let mut cursor = header_value::Text::new(
            header_value::Input {
                bytes: &source,
                base: 0,
                header_limit: source.len() as u64,
                property,
                source_end: header_select::SourceEnd::Eof,
            },
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut result = Vec::new();
        loop {
            let mut output = [0; 8];
            let step = cursor.poll(Tick(1), &mut output).unwrap();
            result.extend_from_slice(&output[..step.written]);
            if matches!(step.status, header_value::Status::Complete(_)) {
                break;
            }
        }
        assert!(!cursor.is_encoding_problem());
        assert_eq!(result, format!("\"{value}\"").as_bytes());
    }
}

#[test]
fn validation_funds_scalar_lookahead_before_a_word() {
    let mut work = meter();
    let before = work.remaining();
    let input = "a".repeat(49);
    let mut cursor = Cursor::new(&input, Tick(1), &mut work).unwrap();
    assert_eq!(
        cursor.poll(&mut [0; 75], Tick(1), &mut work).unwrap(),
        Progress {
            written: 75,
            status: Status::Yield
        }
    );
    assert_eq!(before.io_bytes - work.remaining().io_bytes, 49 + 45);
    assert_eq!(before.output_bytes - work.remaining().output_bytes, 62 + 75);
    assert_eq!(before.records - work.remaining().records, 2);
}
