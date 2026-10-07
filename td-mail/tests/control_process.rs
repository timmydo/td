//! The td-mail window under the real headless td-compositor. `ready`
//! supplies the disposable compositor through `TD_TEST_COMPOSITOR`; the
//! native case launches td-mail offline as an ordinary client, types into
//! it through the compositor's seat, and reads what it did back from the
//! files it retains, since td-mail has no control socket.
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
