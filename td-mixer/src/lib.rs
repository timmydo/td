//! td-mixer: the sound panel, a fixed desktop program td-authd starts as
//! the human by request `30` (APPLICATIONS.md §K.5).
//! An ordinary controller of td-audio's
//! socket through td-pulse: what it reads and writes (`model`), the two
//! threads that talk to the daemon (`worker`), and the panel (`app`).
#![forbid(unsafe_code)]

pub mod app;
pub mod model;
pub mod worker;

/// td-audio's socket (`td_engine::permissions::TD_AUDIO_SOCKET_PATH`).
pub const SOCKET: &str = "/run/td-audio/native";
