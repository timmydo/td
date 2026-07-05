use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("gawk", "5.3.0")
        .source(Source::one(
            "mirror://gnu/gawk/gawk-5.3.0.tar.xz",
            "02x97iyl9v84as4rkdrrkfk2j4vy4r3hpp3rkp3gh3qxs79id76a",
        ))
        .configure_flags(&["CFLAGS=-O2 -g -Wno-incompatible-pointer-types"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_gnu_version gawk gawk "GNU Awk 5.3.0"
"#)])
}
