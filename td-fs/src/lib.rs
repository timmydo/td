//! td's shared file helpers, std-only, one file per helper.
//!
//! A crate cargo builds depends on td-fs by path and calls these through
//! the crate. A crate a recipe compiles with a direct rustc includes by
//! `#[path]` only the file of the helper it uses, so a helper it does not
//! use, and that helper's file-system calls, never reach it: td-boot,
//! td-install and the recipe library include `real_file.rs` alone, td-net
//! `real_file.rs` and `replace.rs`, and td-update and td-vm-guest
//! `private_dir.rs` alone. The control-plane workspace includes
//! `temp_dir.rs` only under `cfg(test)` to own disposable fixtures.

mod bounded;
mod on_path;
mod private_dir;
mod real_file;
mod replace;

pub use bounded::read_bounded;
pub use on_path::{executable_in, executable_on_path};
pub use private_dir::{check_private_dir, private_dir};
pub use real_file::{open_real_file, read_bounded_real_file};
pub use replace::replace;

mod temp_dir;
pub use temp_dir::TempDir;
