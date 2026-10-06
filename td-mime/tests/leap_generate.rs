#![forbid(unsafe_code)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "../tools/leap_generate.rs"]
mod generator;
#[path = "../src/header_date/leap_dates.rs"]
mod table;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("leap-seconds")
}
#[test]
fn exact_pinned_regeneration_and_independent_date_oracle() {
    let generated = generator::generate(&root()).unwrap();
    assert_eq!(generated, include_str!("../src/header_date/leap_dates.rs"));
    assert_eq!(generator::generate(&root()).unwrap(), generated);
    assert_eq!(std::mem::size_of_val(&table::POSITIVE_DATES), 108);
    assert_eq!(
        table::POSITIVE_DATES,
        [
            19720630, 19721231, 19731231, 19741231, 19751231, 19761231, 19771231, 19781231,
            19791231, 19810630, 19820630, 19830630, 19850630, 19871231, 19891231, 19901231,
            19920630, 19930630, 19940630, 19951231, 19970630, 19981231, 20051231, 20081231,
            20120630, 20150630, 20161231,
        ]
    );
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-mime-leap-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn missing_modified_wrong_length_and_wrong_type_sources_refuse() {
    let scratch = Scratch::new();
    let path = scratch.0.join("leap-seconds.list");
    assert!(generator::generate(&scratch.0).is_err());
    let mut bytes = std::fs::read(root().join("leap-seconds.list")).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(generator::generate(&scratch.0).is_ok());
    *bytes.first_mut().unwrap() ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    assert!(generator::generate(&scratch.0)
        .unwrap_err()
        .to_string()
        .contains("SHA-256"));
    *bytes.first_mut().unwrap() ^= 1;
    let last = bytes.pop().unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(generator::generate(&scratch.0)
        .unwrap_err()
        .to_string()
        .contains("pinned length"));
    bytes.push(last);
    bytes.extend_from_slice(b"xx");
    std::fs::write(&path, &bytes).unwrap();
    assert!(generator::generate(&scratch.0)
        .unwrap_err()
        .to_string()
        .contains("pinned length"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(generator::generate(&scratch.0).is_err());
    std::fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink(root().join("leap-seconds.list"), &path).unwrap();
    assert!(generator::generate(&scratch.0).is_err());
}
