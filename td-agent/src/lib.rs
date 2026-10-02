//! td-agent: td's agent harness (DESIGN.md).
//!
//! The crate is `std` and td-ui. Two modules, `json` and `toml`, are td's
//! shared std modules, copied whole from td-news and never edited here;
//! `tests/shared_modules.rs` holds them byte-identical to td-news's. What
//! td-agent does not call of them stays, allowed on its `mod` line rather
//! than trimmed. `td_fetch`, the third, joins with the model client that
//! first calls it (DESIGN.md §18, increment 5).
//!
//! The binary is two personalities of one program (DESIGN.md §2): the
//! window process (`window`, over `ui`, `control` and `supervisor`) and a
//! conversation process per open conversation (`conversation`), which
//! speak `protocol` in `frame`s over a socketpair. Only a conversation
//! process writes its conversation's directory of the `store`.
//!
//! `unsafe` is forbidden for the whole crate (DESIGN.md §2).

#![forbid(unsafe_code)]

pub mod config;
pub mod control;
pub mod conversation;
pub mod frame;
#[allow(dead_code)]
mod json;
pub mod protocol;
pub mod store;
pub mod supervisor;
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
