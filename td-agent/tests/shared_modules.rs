//! td-agent carries td-news's `json` and `toml` modules byte for byte
//! (DESIGN.md §1). The recipe test that holds td-news's and td-mail's
//! copies identical gains td-agent only with packaging (DESIGN.md §17), so
//! until then this test is the one that holds td-agent's: an edit to either
//! side without the other reds here.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

use std::path::Path;

/// Every module td-agent shares with td-news.
const SHARED: [&str; 2] = ["json.rs", "toml.rs"];

#[test]
fn the_shared_modules_are_td_news_copies_byte_for_byte() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    for module in SHARED {
        let ours = std::fs::read(here.join("src").join(module)).unwrap();
        let theirs = std::fs::read(here.join("../td-news/src").join(module)).unwrap();
        assert!(
            ours == theirs,
            "td-agent/src/{module} differs from td-news's copy; the shared \
             modules are edited in every carrier at once"
        );
    }
}
