use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("file", "5.46").source(Source::one(
        "http://ftp.astron.com/pub/file/file-5.46.tar.gz",
        "1230v1sks2p4ijc7x68iy2z9sqfm17v5lmfwbq9l7ib0qp3pgk69",
    ))
}
