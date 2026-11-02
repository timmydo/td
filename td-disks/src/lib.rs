//! td's extra volumes: per-machine data disks the session owner names
//! through one consented operation and a root oneshot mounts at boot.
//! See DESIGN.md.
#![forbid(unsafe_code)]

pub mod probe;
pub mod runtime;
pub mod setting;
pub mod table;
