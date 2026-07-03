use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("less", "608")
        .source(Source::one(
            "mirror://gnu/less/less-608.tar.gz",
            "02f2d9d6hyf03va28ip620gjc6rf4aikmdyk47h7frqj18pbx6m6",
        ))
        .inputs(&["ncurses"])
        .configure_flags(&["CFLAGS=-O2 -std=gnu17"])
}
