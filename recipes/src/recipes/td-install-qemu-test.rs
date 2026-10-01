use crate::types::Recipe;

/// Only the host installation oracle consumes this native PID-1 fixture.
/// Its companion inputs make one declared build closure for that oracle.
/// It speaks the installation service's two protocols through the
/// service's own codecs.
pub fn recipe() -> Recipe {
    crate::ladder::static_local_source_program(
        "td-install-qemu-test",
        &[
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
        "btrfs-progs-x86-64",
        "tzdata",
    ])
}
