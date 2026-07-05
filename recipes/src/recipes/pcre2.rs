use crate::types::{Recipe, RecipeCheck, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("pcre2", "10.42").source(Source::one(
        "https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.42/pcre2-10.42.tar.bz2",
        "0h78np8h3dxlmvqvpnj558x67267n08n9zsqncmlqapans6csdld",
    ))
    .checks(vec![RecipeCheck::daily(r##"
recipe_c_link_check pcre2 pcre2.h pcre2-8 "#define PCRE2_CODE_UNIT_WIDTH 8"
"##)])
}
