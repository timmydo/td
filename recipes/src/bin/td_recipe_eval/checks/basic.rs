use crate::check_runner::RecipeCheckRunner;
use td_recipe::catalog;

pub(crate) fn run_build_only(runner: &RecipeCheckRunner, stem: &str) -> Result<(), String> {
    runner.prepare_recipe_target(stem)?;
    let build_out = runner.build_plan(stem)?;
    if catalog::lookup(stem).is_some_and(|recipe| recipe.application.is_some()) {
        let report = runner.application_store_closure(stem, &build_out)?;
        if stem == "ripgrep-seed" {
            verify_ripgrep_seed_closure(&report)?;
        }
        print!("{report}");
    }
    println!("PASS: {stem} recipe check completed through build-plan --auto");
    Ok(())
}

fn verify_ripgrep_seed_closure(report: &str) -> Result<(), String> {
    for exact in [
        "members\t2",
        "unmarked\t1",
        "recipe-members\t2",
        "audited-seeds\t1",
    ] {
        if !report.lines().any(|line| line == exact) {
            return Err(format!(
                "ripgrep-seed application closure does not contain expected row {exact:?}"
            ));
        }
    }
    if report.lines().filter(|line| line.starts_with("store\t")).count() != 2 {
        return Err("ripgrep-seed application closure must contain exactly two store paths".into());
    }
    let source_disposition = report.lines().find_map(|line| {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["payload", "pin", "ripgrep-seed-source", _, disposition] => Some(*disposition),
            _ => None,
        }
    });
    if source_disposition != Some("build-only") {
        return Err(
            "ripgrep-seed application closure must keep ripgrep-seed-source build-only".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_ripgrep_seed_closure;

    fn report(disposition: &str) -> String {
        format!(
            "members\t2\nunmarked\t1\nrecipe-members\t2\naudited-seeds\t1\n\
             store\t/td/store/app\nstore\t/td/store/runtime\n\
             payload\tpin\tripgrep-seed-source\t/td/store/source\t{disposition}\n"
        )
    }

    #[test]
    fn the_first_seed_requires_its_exact_two_member_build_only_source_closure() {
        verify_ripgrep_seed_closure(&report("build-only")).unwrap();
        let error = verify_ripgrep_seed_closure(&report("retained")).unwrap_err();
        assert!(error.contains("build-only"), "{error}");
        let error = verify_ripgrep_seed_closure(
            &report("build-only").replace("members\t2", "members\t3"),
        )
        .unwrap_err();
        assert!(error.contains("members\\t2") || error.contains("members\t2"), "{error}");
    }
}
