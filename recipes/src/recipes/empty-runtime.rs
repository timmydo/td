use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// A deliberately empty /usr for static application fixtures. Keeping it as a
// real package makes runtime resolution and the containment edge observable
// without pretending a td userland tree is an application ABI.
pub fn recipe() -> Recipe {
    Recipe::mesboot("empty-runtime", "1")
        .steps(vec![Step::MkDir {
            path: "{out}/files".into(),
        }])
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check empty-runtime: materialize the declared empty application runtime"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run empty-runtime 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
