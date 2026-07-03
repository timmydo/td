use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("patch", "2.8").source(Source::one(
        "mirror://gnu/patch/patch-2.8.tar.xz",
        "1qssgwgy3mfahkpgg99a35gl38vamlqb15m3c2zzrd62xrlywz7q",
    ))
}
