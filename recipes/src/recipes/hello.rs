use crate::types::{Recipe, Source};

// The `hello-perturbed` twin derives from this recipe (`super::hello::recipe()`),
// so base and twin stay field-identical except the twin's deliberate delta.
pub fn recipe() -> Recipe {
    Recipe::gnu("hello", "2.12.2").source(Source::one(
        "mirror://gnu/hello/hello-2.12.2.tar.gz",
        "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js",
    ))
}
