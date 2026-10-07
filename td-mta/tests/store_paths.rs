#![allow(clippy::unwrap_used)]
use td_mta::store_paths::{Name, RootEntry, CAPACITY};

fn check(name: &Name, expected: &str) {
    assert_eq!(name.as_str().unwrap(), expected);
    assert_eq!(name.as_bytes().unwrap(), expected.as_bytes());
    assert_eq!(name.as_path().unwrap(), std::path::Path::new(expected));
    assert_eq!(name.as_c_str().unwrap().to_bytes(), expected.as_bytes());
    assert_eq!(
        name.as_c_str().unwrap().to_bytes_with_nul().last(),
        Some(&0)
    );
    assert!(expected.len() <= CAPACITY);
    assert!(!name.as_path().unwrap().is_absolute());
    assert!(name
        .as_path()
        .unwrap()
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_))));
}

#[test]
fn generated_database_layout_has_only_fixed_root_names() {
    for (entry, text) in [
        (RootEntry::Database, "metadata.sqlite3"),
        (RootEntry::Wal, "metadata.sqlite3-wal"),
        (RootEntry::SharedMemory, "metadata.sqlite3-shm"),
        (RootEntry::Lock, "LOCK"),
    ] {
        check(&Name::root(entry).unwrap(), text);
    }
}
