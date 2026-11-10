//! td-ssh: a small SSH client, server and key tool. DESIGN.md is normative.
#![deny(unsafe_code)]

pub mod client;
pub mod client_config;
pub mod conn;
pub mod error;
pub mod kex;
pub mod keyfile;
pub mod keygen;
pub mod known_hosts;
pub mod msg;
pub mod packet;
pub mod pattern;
pub mod prompt;
pub mod server;
pub mod sys;
pub mod wire;
// The workload account ledger reader td-login's account tables include: they
// refuse a passwd or group entry that speaks for a workload account.
#[allow(
    dead_code,
    reason = "td-login's tables name it; sshd uses their passwd checks alone"
)]
#[forbid(unsafe_code)]
#[path = "../../engine/src/workload_accounts.rs"]
mod workload_accounts;
