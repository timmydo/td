//! The td-setup installer client under the real headless td-compositor: a
//! render oracle, not a wire fixture. `ready` supplies the disposable
//! compositor through `TD_TEST_COMPOSITOR`; the sole native case launches the
//! client, waits for its toplevel to present a frame, captures the output,
//! presses Enter and Escape through the compositor's input control,
//! and compares the welcome and unavailable destination pixels.
//!
//! The file is named `control_process` and mounts `native_compositor` so the
//! gate's native runner (`builder/src/native_tests.rs`), which hardcodes
//! `--test control_process` filtered to `native_compositor::`, discovers it.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use td_test_compositor::Directory;

const TIMEOUT: Duration = Duration::from_secs(10);

#[path = "support/native_compositor.rs"]
mod native_compositor;
