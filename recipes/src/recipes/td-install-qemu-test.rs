use crate::types::Recipe;

/// Only the host installation oracle consumes this native PID-1 fixture.
/// Its companion inputs make one declared build closure for that oracle.
pub fn recipe() -> Recipe {
    crate::ladder::static_local_source_program("td-install-qemu-test").inputs(&[
        "linux-x86-64",
        "td-install",
        "td-init",
        "td-boot",
        "td-kexec",
        "btrfs-progs-x86-64",
    ])
}
