use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("patch", "2.8").source(Source::one(
        "mirror://gnu/patch/patch-2.8.tar.xz",
        "1qssgwgy3mfahkpgg99a35gl38vamlqb15m3c2zzrd62xrlywz7q",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version patch patch "GNU patch 2.8"
"#)])
}
