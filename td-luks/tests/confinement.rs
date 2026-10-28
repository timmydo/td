//! The only allowance belongs to the pinned device-mapper syscall module.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use std::fs;
use std::path::Path;

#[test]
fn device_mapper_is_the_only_program_syscall_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut names: Vec<_> = fs::read_dir(root.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["dm.rs", "dm_sys.rs", "lock.rs", "main.rs"]);
    let main = include_str!("../bin/main.rs");
    assert!(main.contains("#![deny(unsafe_code)]"));
    assert!(main.contains("mod dm_sys;"));
    assert!(include_str!("../src/lib.rs").contains("#![forbid(unsafe_code)]"));
    assert!(include_str!("../Cargo.toml").contains("unsafe_code = \"deny\""));
    for name in ["main.rs", "dm.rs", "lock.rs"] {
        let source = fs::read_to_string(root.join("bin").join(name)).unwrap();
        assert_eq!(
            source.matches("unsafe_code").count(),
            usize::from(name == "main.rs"),
            "{name} admits another allowance"
        );
        assert!(
            !source.contains("include!"),
            "{name} includes unchecked code"
        );
        assert!(
            !source.contains("#[path"),
            "{name} includes an unchecked module"
        );
    }
    let oracle = include_str!("../fixtures/activation.rs");
    assert!(oracle.contains("#![deny(unsafe_code)]"));
    assert_eq!(oracle.matches("unsafe_code").count(), 1);
    assert!(oracle.contains("#[path = \"../bin/dm_sys.rs\"]"));
    let source = include_bytes!("../bin/dm_sys.rs");
    let hash: String = td_fido::digest(source)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hash, "bee5b5e74795da4a0e29cb24576dad60c58f59e24df1bd1414c5289e1fc411bb",
        "review the complete syscall surface and amend UNSAFE.md before changing this pin"
    );
}
