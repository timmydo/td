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
//! from its File menu (`menu`) through the key dialog (`keydialog`), and
//! keeps its notes to the human for the Messages window (`notes`). The
//! conversation tools (DESIGN.md §3, §12) are `tools`, their reads of a
//! log `history` and their wake budget `wake`; the window routes the
//! messages they send between conversations through `post`. The tool
//! host (DESIGN.md §2, §12) is a third personality, `toolhost`, which
//! serves `host`'s protocol and performs the file tools (`files`) and
//! the process tools (`shell`).
//!
//! `unsafe` is forbidden for the whole crate (DESIGN.md §2).

#![forbid(unsafe_code)]

pub mod accounts;
pub mod assemble;
pub mod bench;
pub mod chooser;
pub mod client;
pub mod config;
pub mod confirm;
pub mod control;
pub mod conversation;
pub mod cost;
pub mod diagnostics;
pub mod files;
pub mod frame;
pub mod history;
pub mod host;
pub mod jail;
pub mod key;
pub mod keydialog;
pub mod menu;
pub mod models;
pub mod notes;
pub mod picker;
pub mod post;
pub mod prompt;
pub mod protocol;
#[allow(dead_code, reason = "shared dependency-free SHA-256 implementation")]
#[path = "../../engine/src/sha256.rs"]
mod sha256;
pub mod shell;
pub mod span;
pub mod sse;
pub mod store;
pub mod supervisor;
pub mod system;
pub mod toolhost;
pub mod tools;
pub mod ui;
pub mod wake;
pub mod window;
pub mod workspace;
