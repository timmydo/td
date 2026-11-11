#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "../tools/generate.rs"]
mod generator;

#[test]
fn checked_in_tables_reproduce_from_pinned_upstream_json() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for (path, bytes) in generator::generate(&root.join("data")).unwrap() {
        let actual = std::fs::read(root.join(&path)).unwrap();
        if path.ends_with(".rs") {
            // The generator emits only fixed Rust syntax, safe ASCII names and
            // labels; formatting may change whitespace, never label contents.
            let compact = |b: Vec<u8>| {
                b.into_iter()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect::<Vec<_>>()
            };
            assert_eq!(compact(bytes), compact(actual), "{path}");
        } else {
            assert_eq!(bytes, actual, "{path}");
        }
    }
}
