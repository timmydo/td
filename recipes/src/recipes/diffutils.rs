use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("diffutils", "3.12").source(Source::one(
        "mirror://gnu/diffutils/diffutils-3.12.tar.xz",
        "1zbxf8vv7z18ypddwqgzj51n426k959fiv4wxbyl34b0r2gpz2vw",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version diffutils diff "(GNU diffutils) 3.12"
"#)])
}
