use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("ncurses", "6.2")
        .source(Source::one(
            "mirror://gnu/ncurses/ncurses-6.2.tar.gz",
            "17bcm2z1rdx5gmzj5fb8cp7f28aw5b4g2z4qvvqg3yg0fq66wc1h",
        ))
        .configure_flags(&["--without-cxx-binding", "--enable-overwrite", "--with-shared"])
}
