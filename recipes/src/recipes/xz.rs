use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("xz", "5.4.5").source(Source::one(
        "http://tukaani.org/xz/xz-5.4.5.tar.gz",
        "1mmpwl4kg1vs6n653gkaldyn43dpbjh8gpk7sk0gps5f6jwr0p0k",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version xz xz "xz (XZ Utils) 5.4.5"
"#)])
}
