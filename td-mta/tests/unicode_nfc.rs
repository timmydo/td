#![cfg(test)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "../tools/unicode_inputs.rs"]
mod inputs;
use td_mta::{
    admission::work::{Charge, Meter},
    nfc::{Cursor, HeaderBudget, Scratch, Status},
    ports::{Deadline, Tick},
};
fn normalize(input: &str, scratch: &mut Scratch) -> String {
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: td_mta::admission::WorkLimits::default().foreground_io_bytes,
            records: td_mta::admission::WorkLimits::default().foreground_records,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(input, scratch, &mut work, &mut budget);
    let mut output = String::new();
    for _ in 0..1_000_000 {
        match cursor.poll(Tick(1)).unwrap() {
            Status::Scalar(value) => output.push(value),
            Status::Yield => {}
            Status::Complete => return output,
        }
    }
    panic!("NFC did not complete");
}
#[test]
fn complete_official_unicode_17_nfc_equations() {
    let corpus =
        inputs::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("unicode/17.0.0"))
            .unwrap();
    let vectors = corpus
        .iter()
        .find(|i| i.pin.name == "NormalizationTest.txt")
        .unwrap();
    let mut scratch = Scratch::new();
    let mut count = 0;
    let mut part_one = false;
    let mut listed = std::collections::BTreeSet::new();
    for (line_no, line) in vectors.text.lines().enumerate() {
        let content = line.split('#').next().unwrap().trim();
        if content.starts_with('@') {
            part_one = content == "@Part1";
            continue;
        }
        if content.is_empty() {
            continue;
        }
        let fields: Vec<String> = content
            .split(';')
            .take(5)
            .map(|field| {
                field
                    .split_ascii_whitespace()
                    .map(|hex| char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(fields.len(), 5);
        if part_one {
            let mut scalars = fields.first().unwrap().chars();
            assert!(listed.insert(scalars.next().unwrap()));
            assert_eq!(scalars.next(), None);
        }
        for (source, expected) in [(0, 1), (1, 1), (2, 1), (3, 3), (4, 3)] {
            assert_eq!(
                normalize(fields.get(source).unwrap(), &mut scratch),
                *fields.get(expected).unwrap(),
                "line {} column {}",
                line_no + 1,
                source + 1
            );
        }
        count += 1;
    }
    assert_eq!(count, 20034);
    assert!(!listed.is_empty());
    // The corpus also requires identity outside Part 1. Include unassigned
    // scalars too, preserving this pin independently of Rust's Unicode version.
    let mut buffer = [0; 4];
    for scalar in (0..=0x10ffff).filter_map(char::from_u32) {
        if !listed.contains(&scalar) {
            let input: &str = scalar.encode_utf8(&mut buffer);
            assert_eq!(
                normalize(input, &mut scratch),
                input,
                "U+{:04X}",
                u32::from(scalar)
            );
        }
    }
}

#[test]
fn all_official_classes_replay_in_stable_order() {
    let corpus =
        inputs::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("unicode/17.0.0"))
            .unwrap();
    let data = &corpus
        .iter()
        .find(|i| i.pin.name == "UnicodeData.txt")
        .unwrap()
        .text;
    let mut representatives = std::collections::BTreeMap::new();
    for line in data.lines() {
        let fields: Vec<_> = line.split(';').collect();
        let class: u8 = fields.get(3).unwrap().parse().unwrap();
        if class != 0 && fields.get(5).unwrap().is_empty() {
            let scalar =
                char::from_u32(u32::from_str_radix(fields.first().unwrap(), 16).unwrap()).unwrap();
            representatives.entry(class).or_insert(scalar);
        }
    }
    assert_eq!(representatives.len(), 55);
    let descending: String = representatives.values().rev().copied().collect();
    let marks = descending.repeat(6);
    let ordered: String = representatives
        .values()
        .flat_map(|value| std::iter::repeat_n(*value, 6))
        .collect();
    let mut scratch = Scratch::new();
    // NUL cannot compose; the leading case uses only one ordered traversal.
    for (input, expected) in [
        (marks.clone(), ordered.clone()),
        (format!("\0{marks}x"), format!("\0{ordered}x")),
    ] {
        assert_eq!(normalize(&input, &mut scratch), expected);
        assert_eq!(normalize(&expected, &mut scratch), expected);
    }
    let hostile = format!("\0{}", descending.repeat(1000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: td_mta::admission::WorkLimits::default().foreground_io_bytes,
            records: td_mta::admission::WorkLimits::default().foreground_records,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(&hostile, &mut scratch, &mut work, &mut budget);
    let mut limited = false;
    for _ in 0..1_000_000 {
        match cursor.poll(Tick(1)) {
            Err(td_mta::nfc::Error::InterpretationLimit) => {
                limited = true;
                break;
            }
            Ok(Status::Yield | Status::Scalar(_)) => {}
            other => panic!("hostile replay did not reach header limit: {other:?}"),
        }
    }
    assert!(limited);
    assert!(work.remaining().records > 0);
    assert_eq!(work.stopped(), None);
}
