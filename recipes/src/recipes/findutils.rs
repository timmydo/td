use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("findutils", "4.10.0").source(Source::one(
        "mirror://gnu/findutils/findutils-4.10.0.tar.xz",
        "1xd4y24qfsdfp3ndz7d5j49lkhbhpzgr13wrvsmx4izjgyvf11qk",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version findutils find "(GNU findutils) 4.10.0"
"#)])
}
