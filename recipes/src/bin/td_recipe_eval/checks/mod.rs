use td_recipe::types::CheckRunner;

use crate::check_runner::RecipeCheckRunner;

mod basic;
// qemu_boot is NOT a CheckRunner variant: booting the kernel needs HOST qemu,
// which the daily gate's host-free sandbox hides, so it can't run as a sandboxed
// gate check. It is exposed as the host-side `td-recipe-eval qemu-boot` subcommand
// (see check_runner::qemu_boot_cli), not dispatched from a registered check.
pub(crate) mod qemu_boot;
mod rust_toolchain;

pub(crate) fn run(
    check_runner: CheckRunner,
    runner: &RecipeCheckRunner,
    stem: &str,
) -> Result<(), String> {
    match check_runner {
        CheckRunner::BuildOnly => basic::run_build_only(runner, stem),
        CheckRunner::RustToolchain => rust_toolchain::run(runner),
    }
}
