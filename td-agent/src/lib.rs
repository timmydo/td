//! td-agent: td's agent harness (DESIGN.md).
//!
//! The crate is `std` and td-ui. Three modules, `json`, `toml` and
//! `td_fetch`, are td's shared std modules, copied whole from td-news and
//! never edited here; `tests/shared_modules.rs` holds them byte-identical
//! to td-news's. What td-agent does not call of them stays, allowed on its
//! `mod` line rather than trimmed.
//!
//! The binary is two personalities of one program (DESIGN.md §2): the
//! window process (`window`, over `ui`, `control` and `supervisor`) and a
//! conversation process per open or running conversation
//! (`conversation`), which speak `protocol` in `frame`s over a socketpair.
//! Only a conversation process writes its conversation's directory of the
//! `store`. The model client (DESIGN.md §5) is `client`, a streamed reply
//! read by `sse` and put back together by `assemble`, its money `cost`
//! and `accounts`, what it knows of the provider's models `models`; the
//! window reads the API key (`key`) and hands it down.
//!
//! `unsafe` is forbidden for the whole crate (DESIGN.md §2).

#![forbid(unsafe_code)]

pub mod accounts;
pub mod assemble;
pub mod client;
pub mod config;
pub mod control;
pub mod conversation;
pub mod cost;
pub mod frame;
#[allow(dead_code)]
mod json;
pub mod key;
pub mod models;
pub mod prompt;
pub mod protocol;
pub mod span;
pub mod sse;
pub mod store;
pub mod supervisor;
// The fetch service's client: td-agent posts titles and streams turns,
// and gets nothing streamed.
#[allow(dead_code)]
#[cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)
)]
pub mod td_fetch;
#[cfg(test)]
mod testing;
// td-news lints only its shipped targets; td-agent lints its tests too
// (`clippy-all-targets`), and the module's own tests unwrap and index as
// test code may. Its production code is still linted in the library.
#[allow(dead_code)]
#[cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)
)]
mod toml;
pub mod ui;
pub mod window;
