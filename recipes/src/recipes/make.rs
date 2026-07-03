use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("make", "4.4.1").source(Source::one(
        "mirror://gnu/make/make-4.4.1.tar.gz",
        "1cwgcmwdn7gqn5da2ia91gkyiqs9birr10sy5ykpkaxzcwfzn5nx",
    ))
}
