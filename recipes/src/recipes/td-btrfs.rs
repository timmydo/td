use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// Target-built static td-btrfs (td-btrfs/DESIGN.md): `mkfs.btrfs`, which the
// system image ships for td-install and the live profile, and the check, list
// and restore verbs the recipe and host oracles read volumes back with. One
// ELF serves both names by its argv[0]; `bin/mkfs.btrfs` links it. The crate's
// library is compiled first as an rlib under the binary's profile and passed
// by `--extern`, as td-install passes td-fat.
const SOURCES: &[(&str, &str)] = &[
    (
        "{src}/td-btrfs/src/lib.rs",
        include_str!("../../../td-btrfs/src/lib.rs"),
    ),
    (
        "{src}/td-btrfs/src/main.rs",
        include_str!("../../../td-btrfs/src/main.rs"),
    ),
    (
        "{src}/td-btrfs/src/check.rs",
        include_str!("../../../td-btrfs/src/check.rs"),
    ),
    (
        "{src}/td-btrfs/src/crc32c.rs",
        include_str!("../../../td-btrfs/src/crc32c.rs"),
    ),
    (
        "{src}/td-btrfs/src/export.rs",
        include_str!("../../../td-btrfs/src/export.rs"),
    ),
    (
        "{src}/td-btrfs/src/format.rs",
        include_str!("../../../td-btrfs/src/format.rs"),
    ),
    (
        "{src}/td-btrfs/src/fs.rs",
        include_str!("../../../td-btrfs/src/fs.rs"),
    ),
    (
        "{src}/td-btrfs/src/fstree.rs",
        include_str!("../../../td-btrfs/src/fstree.rs"),
    ),
    (
        "{src}/td-btrfs/src/layout.rs",
        include_str!("../../../td-btrfs/src/layout.rs"),
    ),
    (
        "{src}/td-btrfs/src/mkfs.rs",
        include_str!("../../../td-btrfs/src/mkfs.rs"),
    ),
    (
        "{src}/td-btrfs/src/read.rs",
        include_str!("../../../td-btrfs/src/read.rs"),
    ),
    (
        "{src}/td-btrfs/src/source.rs",
        include_str!("../../../td-btrfs/src/source.rs"),
    ),
    (
        "{src}/td-btrfs/src/tree.rs",
        include_str!("../../../td-btrfs/src/tree.rs"),
    ),
];

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::MkDir {
            path: "{src}/td-btrfs/src".into(),
        },
    ];
    for (path, content) in SOURCES {
        steps.push(Step::WriteFile {
            path: (*path).into(),
            content: (*content).into(),
            exec: false,
        });
    }
    // Mirror the crate's [profile.release]: opt-level 2, abort on panic.
    let profile = [
        "--edition",
        "2021",
        "-C",
        "opt-level=2",
        "--target",
        "x86_64-unknown-linux-gnu",
        "-C",
        "target-feature=+crt-static",
        "-C",
        "relocation-model=static",
        "-C",
        "panic=abort",
    ];
    let mut lib = profile.to_vec();
    lib.extend_from_slice(&[
        "--crate-type",
        "rlib",
        "--crate-name",
        "td_btrfs",
        "-o",
        "{root}/libtd_btrfs.rlib",
        "{src}/td-btrfs/src/lib.rs",
    ]);
    steps.push(target_rustc("{src}", rustc, &lib).env("SOURCE_DATE_EPOCH", "1"));
    let mut bin = profile.to_vec();
    bin.extend_from_slice(&[
        "--crate-name",
        "td_btrfs_bin",
        "--extern",
        "td_btrfs={root}/libtd_btrfs.rlib",
        "-o",
        "{out}/bin/td-btrfs",
        "{src}/td-btrfs/src/main.rs",
    ]);
    steps.push(target_rustc("{src}", rustc, &bin).env("SOURCE_DATE_EPOCH", "1"));
    steps.push(Step::Symlink {
        target: "td-btrfs".into(),
        link: "{out}/bin/mkfs.btrfs".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-btrfs".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-btrfs"]));

    Recipe::mesboot("td-btrfs", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_crate_source_is_staged() {
        crate::ladder::assert_sibling_sources_staged(super::recipe(), &["td-btrfs"]);
    }
}
