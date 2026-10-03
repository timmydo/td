#![forbid(unsafe_code)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "../tools/unicode_generate.rs"]
mod generator;
#[path = "../src/unicode_tables.rs"]
mod tables;
use std::{collections::BTreeSet, mem::size_of_val, path::PathBuf};

#[test]
fn exact_offline_regeneration_and_compiled_table_bounds() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let generated = generator::generate(&root.join("unicode/17.0.0")).unwrap();
    assert_eq!(generated, include_str!("../src/unicode_tables.rs"));
    assert_eq!(
        generator::generate(&root.join("unicode/17.0.0")).unwrap(),
        generated
    );
    assert!(tables::DECOMPOSITION
        .windows(2)
        .all(|w| w.first().unwrap().0 < w.get(1).unwrap().0));
    let mut offset = 0;
    for &(code, start, len) in tables::DECOMPOSITION {
        assert!(char::from_u32(code).is_some());
        assert_eq!(start as usize, offset);
        assert!((1..=4).contains(&len));
        offset += len as usize;
        assert!(tables::DECOMPOSED.get(start as usize..offset).is_some());
    }
    assert_eq!(offset, tables::DECOMPOSED.len());
    assert!(tables::DECOMPOSED
        .iter()
        .all(|c| char::from_u32(*c).is_some()));
    assert!(tables::CLASSES
        .windows(2)
        .all(|w| w.first().unwrap().1 < w.get(1).unwrap().0));
    assert!(tables::CLASSES
        .iter()
        .all(|&(start, end, class)| start <= end
            && class != 0
            && char::from_u32(start).is_some()
            && char::from_u32(end).is_some()));
    assert_eq!(
        tables::CLASSES
            .iter()
            .map(|r| r.2)
            .collect::<BTreeSet<_>>()
            .len(),
        55
    );
    assert!(tables::COMPOSITION.windows(2).all(|w| {
        let a = w.first().unwrap();
        let b = w.get(1).unwrap();
        (a.0, a.1) < (b.0, b.1)
    }));
    assert!(tables::COMPOSITION
        .iter()
        .all(|&(a, b, c)| [a, b, c].iter().all(|c| char::from_u32(*c).is_some())));
    assert!(tables::LOWERCASE
        .windows(2)
        .all(|w| w.first().unwrap().0 < w.get(1).unwrap().0));
    assert!(tables::LOWERCASE
        .iter()
        .all(|&(a, b)| char::from_u32(a).is_some() && char::from_u32(b).is_some()));
    let bytes = size_of_val(tables::DECOMPOSITION)
        + size_of_val(tables::DECOMPOSED)
        + size_of_val(tables::CLASSES)
        + size_of_val(tables::COMPOSITION)
        + size_of_val(tables::LOWERCASE);
    println!(
        "table rows: {} {} {} {} {}; payload bytes: {bytes}",
        tables::DECOMPOSITION.len(),
        tables::DECOMPOSED.len(),
        tables::CLASSES.len(),
        tables::COMPOSITION.len(),
        tables::LOWERCASE.len()
    );
    assert_eq!(bytes, 58_720);
    assert_eq!(
        (
            tables::DECOMPOSITION.len(),
            tables::DECOMPOSED.len(),
            tables::CLASSES.len(),
            tables::COMPOSITION.len(),
            tables::LOWERCASE.len()
        ),
        (2081, 3450, 403, 961, 1488)
    );
}
