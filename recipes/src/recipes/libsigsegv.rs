use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("libsigsegv", "2.14").source(Source::one(
        "mirror://gnu/libsigsegv/libsigsegv-2.14.tar.gz",
        "15d2r831xz94s7540nvb1gbfl062g7mrnj88m60wyr1kh10kkb6d",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_c_link_check libsigsegv sigsegv.h sigsegv
"#)])
}
