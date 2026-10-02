//! td-agent: td's agent harness (DESIGN.md).
//!
//! The crate is `std` and td-ui. Two modules, `json` and `toml`, are td's
//! shared std modules, copied whole from td-news and never edited here;
//! `tests/shared_modules.rs` holds them byte-identical to td-news's. What
//! td-agent does not call of them stays, allowed on its `mod` line rather
//! than trimmed: a binary crate exports nothing, so `dead_code` fires here
//! and not in the module's own crate. `td_fetch`, the third, joins with the
//! model client that first calls it (DESIGN.md §18, increment 5).
//!
//! `unsafe` is forbidden for the whole crate (DESIGN.md §2).

#![forbid(unsafe_code)]

#[allow(dead_code)]
mod json;
// td-news lints only its shipped targets; td-agent lints its tests too
// (`clippy-all-targets`), and the module's own tests unwrap and index as
// test code may. Its production code is still linted in the binary.
#[allow(dead_code)]
#[cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)
)]
mod toml;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: td-agent [--help]\n\
\n\
td-agent is td's agent harness. This build carries its crate and gate\n\
only: the window, the conversation processes and the store land next\n\
(td-agent/DESIGN.md §18, increment 4).\n";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h") if args.len() == 1 => {
            let _ = std::io::stdout().lock().write_all(USAGE.as_bytes());
            ExitCode::SUCCESS
        }
        _ => {
            let _ = std::io::stderr().lock().write_all(USAGE.as_bytes());
            ExitCode::from(2)
        }
    }
}
