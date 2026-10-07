#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::Path;

fn scan(root: &Path, directory: &Path) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_type().unwrap().is_symlink());
        let path = entry.path();
        if path.is_dir() {
            scan(root, &path);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            if path.strip_prefix(root).unwrap() == Path::new("tests/sqlite_confinement.rs") {
                continue;
            }
            // The dependency-policy fixture names the admitted crate as data.
            // Remove only that exact literal; scan all other tokens in the file.
            let source = if path.strip_prefix(root).unwrap() == Path::new("src/ports.rs") {
                let literal = r##"r#"rusqlite = { version = "=0.40.2", default-features = false, features = ["bundled", "hooks", "limits"] }"#"##;
                assert_eq!(source.matches(literal).count(), 1);
                source.replace(literal, "")
            } else {
                source
            };
            if source
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == "rusqlite")
            {
                let relative = path.strip_prefix(root).unwrap();
                assert!(
                    [
                        "src/store_fs/index.rs",
                        "src/store_fs/index/relational.rs",
                        "src/store_fs/index/relational/read.rs",
                    ]
                    .iter()
                    .any(|owner| relative == Path::new(owner)),
                    "native SQL outside its private owner: {}",
                    relative.display()
                );
            }
        }
    }
}

#[test]
fn native_sql_is_private_and_has_no_external_query_or_extension_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    scan(root, &root.join("src"));
    scan(root, &root.join("tests"));
    let source = [
        include_str!("../src/store_fs/index.rs"),
        include_str!("../src/store_fs/index/relational.rs"),
        include_str!("../src/store_fs/index/relational/read.rs"),
        include_str!("../src/store_fs/index/relational/schema.rs"),
    ]
    .join("\n");
    let module = include_str!("../src/store_fs.rs");
    assert!(module.contains("mod index;"));
    assert!(!module.contains("pub mod index"));
    for forbidden in [
        "pub struct Native",
        "pub connection",
        "pub fn execute",
        "pub fn query",
        "load_extension",
        "enable_load_extension",
        "SQLITE_OPEN_URI",
        "ATTACH DATABASE",
    ] {
        assert!(!source.contains(forbidden), "{forbidden}");
    }
    for required in [
        "SQLITE_OPEN_NOFOLLOW",
        "SQLITE_DBCONFIG_DEFENSIVE",
        "PRAGMA foreign_keys=ON",
        "PRAGMA trusted_schema=OFF",
        "PRAGMA synchronous=FULL",
        "PRAGMA wal_autocheckpoint=0",
        "PRAGMA hard_heap_limit",
        "VM_STEPS",
        "progress_handler",
    ] {
        assert!(source.contains(required), "{required}");
    }
}
