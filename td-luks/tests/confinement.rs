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
    assert_eq!(
        names,
        [
            "dm.rs",
            "dm_sys.rs",
            "lock.rs",
            "main.rs",
            "reencrypt.rs",
            "reencrypt_io.rs",
            "reencrypt_stack.rs"
        ]
    );
    let main = include_str!("../bin/main.rs");
    assert!(main.contains("#![deny(unsafe_code)]"));
    assert!(main.contains("mod dm_sys;"));
    assert!(include_str!("../src/lib.rs").contains("#![forbid(unsafe_code)]"));
    assert!(include_str!("../Cargo.toml").contains("unsafe_code = \"deny\""));
    for name in [
        "main.rs",
        "dm.rs",
        "lock.rs",
        "reencrypt.rs",
        "reencrypt_io.rs",
        "reencrypt_stack.rs",
    ] {
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
    let runtime_oracle = include_str!("../fixtures/reencryption.rs");
    assert!(runtime_oracle.contains("#![deny(unsafe_code)]"));
    assert_eq!(runtime_oracle.matches("unsafe_code").count(), 1);
    assert!(runtime_oracle.contains("#[path = \"../bin/dm_sys.rs\"]"));
    let source = include_bytes!("../bin/dm_sys.rs");
    let hash: String = td_fido::digest(source)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hash, "c16ec8147d985a611795c36c2783d0b1bdf16e46e21c80616733158f4e57c32e",
        "review the complete syscall surface and amend UNSAFE.md before changing this pin"
    );
}
