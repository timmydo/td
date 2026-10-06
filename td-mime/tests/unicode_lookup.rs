#![cfg(test)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "../tools/unicode_inputs.rs"]
mod inputs;
use td_mime::unicode;

#[test]
fn every_official_hangul_vector_matches_and_composes_back() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("unicode/17.0.0");
    let corpus = inputs::load(&root).unwrap();
    let vectors = corpus
        .iter()
        .find(|i| i.pin.name == "NormalizationTest.txt")
        .unwrap();
    let mut tested = std::collections::BTreeSet::new();
    for line in vectors.text.lines() {
        let content = line.split('#').next().unwrap().trim();
        if content.is_empty() || content.starts_with('@') {
            continue;
        }
        let fields: Vec<_> = content.split(';').collect();
        let first = *fields.first().unwrap();
        let mut first = first.split_ascii_whitespace();
        let code = u32::from_str_radix(first.next().unwrap(), 16).unwrap();
        if first.next().is_some() || !(0xac00..=0xd7a3).contains(&code) {
            continue;
        }
        let expected: Vec<_> = fields
            .get(2)
            .unwrap()
            .split_ascii_whitespace()
            .map(|v| char::from_u32(u32::from_str_radix(v, 16).unwrap()).unwrap())
            .collect();
        let scalar = char::from_u32(code).unwrap();
        let decomposition = unicode::decompose(scalar).unwrap();
        assert!(
            decomposition.iter().eq(expected.iter().copied()),
            "U+{code:04X}"
        );
        let mut actual = *expected.first().unwrap();
        for &next in expected.iter().skip(1) {
            actual = unicode::compose(actual, next).unwrap().unwrap();
        }
        assert_eq!(actual, scalar);
        tested.insert(code);
    }
    assert_eq!(tested.len(), 11172);
}
