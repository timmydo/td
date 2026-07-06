use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("make", "4.4.1").source(Source::one(
        "mirror://gnu/make/make-4.4.1.tar.gz",
        "1cwgcmwdn7gqn5da2ia91gkyiqs9birr10sy5ykpkaxzcwfzn5nx",
    ))
    .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version make make "GNU Make 4.4.1"
"#)])
}
