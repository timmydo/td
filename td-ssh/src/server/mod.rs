//! `td-sshd`'s stages (DESIGN.md section 4) and their policy.

pub mod account;
pub mod authkeys;
pub mod config;
pub mod credentials;
pub mod ipc;
pub mod jail;
pub mod keys;
pub mod listener;
pub mod log;
pub mod monitor;
pub mod preauth;
pub mod session;
pub mod sftp;
pub mod signer;

// td-login's account tables and status reader, so both programs parse these
// files identically. Mounted whole, used in part.
#[allow(dead_code)]
#[path = "../../../td-login/src/db.rs"]
mod login_db;
#[allow(dead_code)]
#[path = "../../../td-login/src/status.rs"]
mod login_status;
