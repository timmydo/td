use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("sed", "4.9").source(Source::one(
        "mirror://gnu/sed/sed-4.9.tar.gz",
        "0bi808vfkg3szmpy9g5wc7jnn2yk6djiz412d30km9rky0c8liyi",
    ))
}
