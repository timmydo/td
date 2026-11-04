//! td's transport to a program of its own on another machine (DESIGN.md).
//! A window runs here and what it works on is there; this
//! crate carries bounded messages between them over the standard streams
//! of an `ssh` child, so ssh remains the only network code, the only login
//! and the only listener. What the messages mean is each program's own.

#![forbid(unsafe_code)]

mod client;
pub mod codec;
mod error;
pub mod frame;
mod link;
mod ssh;

pub use client::{Client, Connector, RequestError};
pub use error::Error;
pub use link::{printable, Hello, Inbox, Link, Options, Sender, CHUNK_BYTES};
pub use ssh::{Host, RemoteProgram};
