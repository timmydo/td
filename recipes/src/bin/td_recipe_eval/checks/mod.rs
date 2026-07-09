use td_recipe::types::CheckRunner;

use crate::check_runner::RecipeCheckRunner;

mod basic;
mod rust_toolchain;
mod x86_64;

pub(crate) fn run(
    check_runner: CheckRunner,
    runner: &RecipeCheckRunner,
    stem: &str,
) -> Result<(), String> {
    match check_runner {
        CheckRunner::BuildOnly => basic::run_build_only(runner, stem),
        CheckRunner::RustToolchain => rust_toolchain::run(runner),
        CheckRunner::X8664CrossToolchain => x86_64::run_cross_toolchain(runner),
        CheckRunner::X8664NativeGcc => x86_64::run_native_gcc(runner),
        CheckRunner::X8664SelfGcc => x86_64::run_self_gcc(runner),
    }
}
