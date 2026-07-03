use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("bash", "5.2.37")
        .source(Source::one(
            "mirror://gnu/bash/bash-5.2.tar.gz",
            "1yrjmf0mqg2q8pqphjlark0mcmgf88b0acq7bqf4gx3zvxkc2fd1",
        ))
        .inputs(&["readline", "ncurses"])
        .configure_flags(&["CFLAGS=-O2 -g -std=gnu17"])
}
