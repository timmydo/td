//! The td-agent window under the real headless compositor. `ready`
//! supplies the disposable compositor through `TD_TEST_COMPOSITOR`; the
//! native case launches td-agent as an ordinary client, types into it
//! through the compositor's seat, and reads what it did back from the
//! conversation store it writes.
//!
//! The file is named `control_process` and mounts `native_compositor` so
//! the gate's native runner, which runs `--test control_process` filtered
//! to `native_compositor::`, discovers it. This is td-agent's one process
//! test file (DESIGN.md §17); the rest of the window is tested against
//! its widget state in `src/ui.rs` and `src/control.rs`.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use td_test_compositor::Directory;

// Every wait polls and returns once its condition holds, so the bound costs
// a passing run nothing; it is wide for a host loaded by parallel checks.
const TIMEOUT: Duration = Duration::from_secs(30);

#[path = "support/native_compositor.rs"]
mod native_compositor;
