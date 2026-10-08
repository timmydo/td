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
//! the process tools (`shell`). The git worker's outside half, admitted
//! remotes and the store (DESIGN.md §7, §9), is `git`; a workspace
//! repository's layout and the git a maintenance instance runs over it
//! (DESIGN.md §8, §9) are `repo`. `review` is a fourth personality, run
//! from the command line: one model's review of one commit, with no
//! window.
//!
//! `unsafe` is forbidden for the whole crate (DESIGN.md §2).

#![forbid(unsafe_code)]

pub mod accounts;
pub mod activity;
pub mod assemble;
pub mod bench;
pub mod calibrate;
pub mod card;
pub mod chooser;
pub mod classifier;
pub mod client;
pub mod compact;
pub mod config;
pub mod confirm;
pub mod control;
pub mod conversation;
pub mod cost;
pub mod diagnostics;
pub mod egress;
pub mod files;
pub mod frame;
pub mod git;
pub mod history;
pub mod host;
pub mod jail;
pub mod key;
pub mod keydialog;
pub mod menu;
pub mod models;
pub mod notes;
pub mod output;
pub mod patch;
pub mod picker;
pub mod post;
pub mod prompt;
pub mod protocol;
pub mod proxy;
pub mod removal;
pub mod repo;
pub mod review;
pub mod rules;
pub mod scan;
pub mod schedule;
#[allow(dead_code, reason = "shared dependency-free SHA-256 implementation")]
#[path = "../../engine/src/sha256.rs"]
mod sha256;
pub mod shell;
pub mod span;
pub mod sse;
pub mod store;
pub mod supervisor;
pub mod system;
pub mod templatedialog;
pub mod toolhost;
pub mod tools;
pub mod ui;
pub mod upstream;
pub mod wake;
pub mod web;
pub mod window;
pub mod wire;
pub mod workspace;
