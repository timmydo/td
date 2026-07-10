use td_recipe::types::CheckRunner;

use crate::check_runner::RecipeCheckRunner;

mod basic;
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
