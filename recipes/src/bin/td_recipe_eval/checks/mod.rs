use td_recipe::types::CheckRunner;

use crate::check_runner::RecipeCheckRunner;

mod basic;
// bundle is NOT a CheckRunner variant either: it builds the distro and writes a
// redistributable demo VM, which needs host qemu-img and a writable destination.
// Exposed as `td-recipe-eval bundle` (check_runner::bundle_cli).
pub(crate) mod bundle;
mod codex;
// qemu_boot is NOT a CheckRunner variant: booting the kernel needs HOST qemu,
// which the gate's host-free sandbox hides, so it can't run as a sandboxed
// gate check. It is exposed as the host-side `td-recipe-eval qemu-boot` subcommand
// (see check_runner::qemu_boot_cli), not dispatched from a registered check.
pub(crate) mod qemu_boot;
pub(crate) mod run;
mod rust_toolchain;
pub(crate) mod vm_profile;

pub(crate) fn run(
    check_runner: CheckRunner,
    runner: &RecipeCheckRunner,
    stem: &str,
) -> Result<(), String> {
    match check_runner {
        CheckRunner::BuildOnly => basic::run_build_only(runner, stem),
        CheckRunner::Codex => codex::run(runner),
        CheckRunner::RustToolchain => rust_toolchain::run(runner),
    }
}
