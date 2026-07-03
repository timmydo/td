use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("libunistring", "1.3").source(Source::one(
        "mirror://gnu/libunistring/libunistring-1.3.tar.xz",
        "09wmas38i9fw7l3sv92xkbvy7idcl76ifhzv7l7ia98xhdn7higj",
    ))
}
