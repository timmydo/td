use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("grep", "3.11")
        .source(Source::one(
            "mirror://gnu/grep/grep-3.11.tar.xz",
            "1avf4x8skxbqrjp5j2qr9sp5vlf8jkw2i5bdn51fl3cxx3fsxchx",
        ))
        .inputs(&["pcre2"])
}
