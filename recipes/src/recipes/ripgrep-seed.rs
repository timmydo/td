use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// The first foreign application seed is intentionally static and small. It
// exercises the package/trust boundary without choosing Firefox's runtime for
// the later compatibility work.
pub fn recipe() -> Recipe {
    let Ok(declaration) = ApplicationDeclaration::new("empty-runtime", "/app/bin/rg") else {
        return Recipe::mesboot("ripgrep-seed", "15.2.0")
            .source_input("ripgrep-seed-source")
            .steps(vec![Step::Require {
                paths: vec!["{out}/invalid-application-declaration".into()],
                exec: false,
            }]);
    };
    let steps = vec![
        Step::Unpack {
            input: "{payload:ripgrep-seed-source}".into(),
            dest: "{root}/seed".into(),
            keep_top: false,
        },
        Step::MkDir {
            path: "{out}/files/bin".into(),
        },
        Step::CopyFiles {
            files: vec!["{root}/seed/rg".into()],
            dest: "{out}/files/bin".into(),
        },
        Step::validate_static_application(&declaration),
    ];

    Recipe::mesboot("ripgrep-seed", "15.2.0")
        .source_input("ripgrep-seed-source")
        .payload_inputs(&["empty-runtime"])
        .steps(steps)
        .application(declaration)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check ripgrep-seed: package the pinned static upstream payload without executing it"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run ripgrep-seed 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
