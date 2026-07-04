use crate::types::{Recipe, Source};

// The pinned channel's sqlite (3.51.0) — the version tests/sqlite-no-guix.lock's
// `sqlite-source` entry realizes, like hello/sed. A LEAF recipe: the ladder's
// sqlite3 is the store-DB parser oracle (store-register) and needs no line
// editing, so guix's readline input is deliberately omitted (#312 — the
// /td/store harness userland stays minimal). 3.49+ tarballs configure via
// autosetup, which accepts the gnu lowering's `./configure --prefix=$out`.
pub fn recipe() -> Recipe {
    Recipe::gnu("sqlite", "3.51.0").source(Source::one(
        "https://sqlite.org/2025/sqlite-autoconf-3510000.tar.gz",
        "19bc2inw7f9fn0y6j3b57w4mk6bzi2q8hp5yn6qyd8kav7ynvqj2",
    ))
}
