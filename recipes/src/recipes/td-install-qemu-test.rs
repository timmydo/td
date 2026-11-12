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
    let mut steps = crate::shared::allocation_sources();
    for mut step in recipe.steps.take().unwrap_or_default() {
        if let Step::Run { argv, .. } = &mut step {
            if argv
                .first()
                .is_some_and(|program| program.ends_with("/rustc"))
            {
                for (name, source, output, externs) in [
                    (
                        "td_alloc",
                        "{src}/td-alloc/src/owned.rs",
                        "{root}/libtd_alloc.rlib",
                        &[][..],
                    ),
                    (
                        "td_json",
                        "{src}/td-json/src/lib.rs",
                        "{root}/libtd_json.rlib",
                        &["--extern", "td_alloc={root}/libtd_alloc.rlib"],
                    ),
                ] {
                    let mut args = vec![
                        "--edition",
                        "2021",
                        "--crate-type",
                        "rlib",
                        "--crate-name",
                        name,
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
                        "-L",
                        "dependency={root}",
                        "-o",
                        output,
                        source,
                    ];
                    args.extend_from_slice(externs);
                    steps.push(
                        crate::shared::target_rustc(
                            "{src}",
                            "{in:rust-toolchain}/bin/rustc",
                            &args,
                        )
                        .env("SOURCE_DATE_EPOCH", "1"),
                    );
                }
                argv.extend([
                    "--extern".into(),
                    "td_json={root}/libtd_json.rlib".into(),
                    "-L".into(),
                    "dependency={root}".into(),
                ]);
            }
        }
        steps.push(step);
    }
    recipe.steps(steps)
}
