use crate::check_runner::RecipeCheckRunner;

pub(crate) fn run_build_only(runner: &RecipeCheckRunner, stem: &str) -> Result<(), String> {
    runner.prepare_recipe_target(stem)?;
    runner.build_plan(stem)?;
    println!("PASS: {stem} recipe check completed through build-plan --auto");
    Ok(())
}
