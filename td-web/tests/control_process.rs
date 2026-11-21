//! The td-web window as a process: its command line before any display,
//! and, under the real headless td-compositor, the window at the outputs
//! its pages were measured at (DESIGN.md §10, increment 7), fetching from
//! a mock td-fetch service and driven over its control socket and the
//! seat.
//!
//! The file is named `control_process` and mounts `native_compositor` so
//! the gate's native runner, which runs `--test control_process` filtered
//! to `native_compositor::`, finds the ignored cases; `ready` supplies the
//! compositor through `TD_TEST_COMPOSITOR`.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::process::Command;

#[path = "support/mock_fetch.rs"]
mod mock_fetch;

#[path = "support/native_compositor.rs"]
mod native_compositor;

/// A control socket's path is judged, and refused, before a display is
/// asked for: the binary run with no display says what is wrong with the
/// path, not that there is no compositor.
#[test]
fn a_relative_control_socket_is_refused_before_a_display() {
    let output = Command::new(env!("CARGO_BIN_EXE_td-web"))
        .args(["--control-socket", "relative-td-web-control"])
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("control socket needs an absolute"),
        "{stderr}"
    );
}

/// `--help actions` prints the seam's table, each action's name on it.
#[test]
fn the_action_table_is_printed() {
    let output = Command::new(env!("CARGO_BIN_EXE_td-web"))
        .args(["--help", "actions"])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    let table = String::from_utf8(output.stdout).unwrap();
    for name in ["open", "back", "follow", "find", "close"] {
        assert!(
            table.lines().any(|l| l.starts_with(name)),
            "{name}: {table}"
        );
    }
}
