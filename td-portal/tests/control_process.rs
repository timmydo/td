//! The file chooser's render under the real headless td-compositor: a render
//! oracle, not a wire fixture. `ready` supplies the disposable compositor
//! through `TD_TEST_COMPOSITOR`; the sole native case maps a minimal client
//! that presents `Chooser::render_sized`, captures the composited output, and
//! asserts it equals the crate's own render of the same surface. The
//! manager-protocol path the portal alone speaks is covered by the socket
//! regression in `tests/dialog.rs`.
//!
//! The file is named `control_process` and mounts `native_compositor` so the
//! gate's native runner (`builder/src/native_tests.rs`), which hardcodes
//! `--test control_process` filtered to `native_compositor::`, discovers it.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use td_test_compositor::Directory;

type Result<T> = std::result::Result<T, String>;
const TIMEOUT: Duration = Duration::from_secs(10);

#[path = "support/native_compositor.rs"]
mod native_compositor;
