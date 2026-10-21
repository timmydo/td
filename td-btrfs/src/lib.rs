//! td's Btrfs: a std-only writer of new single-device filesystems from a
//! directory tree (`mkfs.btrfs --rootdir`), and a reader that checks and
//! restores what it wrote. See DESIGN.md.

// The recipe compiles with rustc directly, which reads no Cargo lints.
#![forbid(unsafe_code)]

pub mod check;
pub mod crc32c;
pub mod export;
pub mod format;
pub mod fs;
pub mod fstree;
pub mod layout;
pub mod mkfs;
pub mod read;
pub mod source;
pub mod tree;
