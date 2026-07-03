use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("readline", "8.2.13")
        .source(Source::one(
            "mirror://gnu/readline/readline-8.2.tar.gz",
            "0dbw02ai0z8x6d9s14pl0hnaa2g1kdxnv8qqra1fx13ay5qp3srz",
        ))
        .inputs(&["ncurses"])
}
