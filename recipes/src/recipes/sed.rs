use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("sed", "4.9").source(Source::one(
        "mirror://gnu/sed/sed-4.9.tar.gz",
        "0bi808vfkg3szmpy9g5wc7jnn2yk6djiz412d30km9rky0c8liyi",
    ))
    .checks(vec![
        RecipeCheck::daily(r#"
recipe_gnu_version sed sed "(GNU sed) 4.9"
"#),
        RecipeCheck::daily(r#"
echo ">> recipe-check sed/store-native: /td/store source-bootstrap toolchain builds GNU sed"
test -n "${TD_GATE_INPUT_SED_GCC_TOOLCHAIN:-}" || { echo "FAIL: TD_GATE_INPUT_SED_GCC_TOOLCHAIN unset" >&2; exit 1; }
TD_GATE_INPUT_GCC_TOOLCHAIN="$TD_GATE_INPUT_SED_GCC_TOOLCHAIN" sh tests/bootstrap-sed-corpus-store-native.sh
"#),
    ])
}
