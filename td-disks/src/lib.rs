//! td's extra volumes: per-machine data disks the session owner names
//! through one consented operation and a root oneshot mounts at boot, and
//! removable media mounted for the owner while the session is unlocked.
//! See DESIGN.md.
#![forbid(unsafe_code)]

pub mod media;
pub mod probe;
pub mod runtime;
pub mod setting;
pub mod table;
