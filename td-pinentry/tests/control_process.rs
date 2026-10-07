//! td-pinentry under the real headless td-compositor. `ready` supplies
//! the disposable compositor through `TD_TEST_COMPOSITOR`; the cases
//! start td-pinentry as gpg-agent or ssh would, type into its window
//! through the compositor's seat, and read the answer it writes.
//!
//! The file is named `control_process` and mounts `native_compositor` so
//! the gate's native runner (`builder/src/native_tests.rs`), which runs
//! `--test control_process` filtered to `native_compositor::`, finds it.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use td_test_compositor::Directory;

const TIMEOUT: Duration = Duration::from_secs(10);

#[path = "support/native_compositor.rs"]
mod native_compositor;
