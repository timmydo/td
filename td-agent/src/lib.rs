//! td-agent: td's agent harness (DESIGN.md).
//!
//! The crate is `std` and td's own crates: td-ui, td-json, td-toml and
//! td-fetch-client, the fetch service's client.
//!
//! The binary is two personalities of one program (DESIGN.md §2): the
//! window process (`window`, over `ui`, `control` and `supervisor`) and a
//! conversation process per open or running conversation
//! (`conversation`), which speak `protocol` in `frame`s over a socketpair.
//! Only a conversation process writes its conversation's directory of the
//! `store`. The model client (DESIGN.md §5) is `client`, a streamed reply
//! read by `sse` and put back together by `assemble`, its money `cost`
//! and `accounts`, what it knows of the provider's models `models`; the
//! window reads the API key (`key`) and hands it down, and stores one
//! from its File menu (`menu`) through the key dialog (`keydialog`). The
//! conversation tools (DESIGN.md §3, §12) are `tools`, their reads of a
//! log `history` and their wake budget `wake`; the window routes the
//! messages they send between conversations through `post`.
//!
//! `unsafe` is forbidden for the whole crate (DESIGN.md §2).

#![forbid(unsafe_code)]

pub mod accounts;
pub mod assemble;
pub mod client;
pub mod config;
pub mod confirm;
pub mod control;
pub mod conversation;
pub mod cost;
pub mod diagnostics;
pub mod frame;
pub mod history;
pub mod key;
pub mod keydialog;
pub mod menu;
pub mod models;
pub mod picker;
pub mod post;
pub mod prompt;
pub mod protocol;
pub mod span;
pub mod sse;
pub mod store;
pub mod supervisor;
pub mod system;
pub mod tools;
pub mod ui;
pub mod wake;
pub mod window;
