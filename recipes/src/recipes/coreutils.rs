use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("coreutils", "9.1").source(Source::one(
        "mirror://gnu/coreutils/coreutils-9.1.tar.xz",
        "08q4b0w7mwfxbqjs712l6wrwl2ijs7k50kssgbryg9wbsw8g98b1",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version coreutils ls "(GNU coreutils) 9.1"
"#)])
}
