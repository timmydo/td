use crate::types::{Recipe, Step};

/// Only the host installation oracle consumes this native PID-1 fixture.
/// Its companion inputs make one declared build closure for that oracle.
/// It speaks the installation service's two protocols through the
/// service's own codecs.
pub fn recipe() -> Recipe {
    let mut recipe = crate::shared::static_local_source_program(
        "td-install-qemu-test",
        &[
            (
                "{src}/td-json/src/arena.rs",
                include_str!("../../../td-json/src/arena.rs"),
            ),
            (
                "{src}/td-json/src/arena_emit.rs",
                include_str!("../../../td-json/src/arena_emit.rs"),
            ),
            (
                "{src}/td-json/src/lib.rs",
                include_str!("../../../td-json/src/lib.rs"),
            ),
            (
                "{src}/td-json/src/retain.rs",
                include_str!("../../../td-json/src/retain.rs"),
            ),
            (
                "{src}/td-json/src/scan.rs",
                include_str!("../../../td-json/src/scan.rs"),
            ),
            (
                "{src}/td-json/src/string.rs",
                include_str!("../../../td-json/src/string.rs"),
            ),
            (
                "{src}/td-json/src/string_array.rs",
                include_str!("../../../td-json/src/string_array.rs"),
            ),
            (
                "{src}/td-install/src/installation_consent.rs",
                include_str!("../../../td-install/src/installation_consent.rs"),
            ),
            (
                "{src}/td-install/src/installation_plan.rs",
                include_str!("../../../td-install/src/installation_plan.rs"),
            ),
            (
                "{src}/td-install/src/installation_protocol.rs",
                include_str!("../../../td-install/src/installation_protocol.rs"),
            ),
        ],
    )
    .inputs(&[
        "linux-x86-64",
        "td-install",
        "td-firstboot",
        "td-init",
        "td-boot",
        "td-kexec",
        "td-btrfs",
        "tzdata",
    ]);
    let mut steps = Vec::new();
    for mut step in recipe.steps.take().unwrap_or_default() {
        if let Step::Run { argv, .. } = &mut step {
            if argv
                .first()
                .is_some_and(|program| program.ends_with("/rustc"))
            {
                steps.push(
                    crate::shared::target_rustc(
                        "{src}",
                        "{in:rust-toolchain}/bin/rustc",
                        &[
                            "--edition",
                            "2021",
                            "--crate-type",
                            "rlib",
                            "--crate-name",
                            "td_json",
                            "-C",
                            "opt-level=s",
                            "--target",
                            "x86_64-unknown-linux-gnu",
                            "-C",
                            "target-feature=+crt-static",
                            "-C",
                            "relocation-model=static",
                            "-C",
                            "panic=abort",
                            "-o",
                            "{root}/libtd_json.rlib",
                            "{src}/td-json/src/lib.rs",
                        ],
                    )
                    .env("SOURCE_DATE_EPOCH", "1"),
                );
                argv.extend(["--extern".into(), "td_json={root}/libtd_json.rlib".into()]);
            }
        }
        steps.push(step);
    }
    recipe.steps(steps)
}
