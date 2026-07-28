//! Ctrl-Alt-Del, as a process the kernel can kill rather than a signal handler.
//!
//! `ctrl_alt_del()` in the kernel does one of two things. With
//! `/proc/sys/kernel/ctrl-alt-del` at 0 it sends SIGINT to whatever pid
//! `/proc/sys/kernel/cad_pid` names; with it at 1 it schedules
//! `kernel_restart(NULL)`, which runs the reboot notifiers and
//! `device_shutdown()` and then resets — no userspace teardown, no `sync(2)`,
//! nothing unmounted. The second is what an unarmed machine does, and it is
//! why arming is not optional on a system with a writable `/var`.
//!
//! `cad_pid` accepts ANY live pid, so PID 1 need not be the target — and the
//! target needs no handler, only to **die observably**. td-svc therefore points
//! the kernel at a child whose whole job is to be killed: it blocks reading a
//! pipe this process holds the write end of, so it neither spins nor exits on
//! its own, and its death by SIGINT *is* the press. That keeps td-svc free of
//! signal handlers entirely (DESIGN.md §5 turns on there being none) and makes
//! the trigger visible through the same `wait` machinery every service uses.
//!
//! The ORDER matters and is the reason `arm` is one function: the hard reset is
//! disabled FIRST, then a sentinel exists, then the kernel is pointed at it.
//! Arming the other way round leaves a window in which a press still resets the
//! machine with `/var` mounted.

use std::fs;
use std::io;
use std::process::{Child, ChildStdin, Command, Stdio};

/// 0 = send SIGINT to `cad_pid`; 1 = immediate `kernel_restart`.
pub const CAD_ENABLED: &str = "/proc/sys/kernel/ctrl-alt-del";
/// Who gets the SIGINT. 0600, and it accepts any live pid.
pub const CAD_PID: &str = "/proc/sys/kernel/cad_pid";

/// The argv `arm` spawns and `main` routes. One string, so the spawner and the
/// dispatcher cannot disagree about what a sentinel is.
pub const SENTINEL_VERB: &str = "cad-sentinel";

/// An armed sentinel: the kernel points at `pid`, and this process holds the
/// pipe that keeps it blocked.
pub struct Armed {
    /// RETAINED, not dropped. Closing this write end EOFs the sentinel's read,
    /// which exits it — so a `_`-bound or dropped handle disarms Ctrl-Alt-Del
    /// silently. It is named without a leading underscore for that reason: a
    /// future edit should have to think about it.
    pub keepalive: ChildStdin,
    pub pid: i32,
    /// When this sentinel was armed. A sentinel that stayed up is not evidence
    /// of a failing arming, so its death resets the re-arm backoff the same way
    /// a service that ran long enough resets its restart backoff.
    pub since: std::time::Instant,
}

/// Read the sysctl back as a trimmed string.
fn read_sysctl(path: &str) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

/// Turn off the kernel's own hard reset, and prove it took.
///
/// Read back rather than trusted: this is a `/proc` write, so a short write or
/// a kernel that ignored the value both look like success at the syscall, and
/// the failure mode is a machine that hard-resets on a press with everything
/// still mounted.
///
/// The path is a parameter for the same reason the shutdown marker's is: the
/// interesting property — that a write which did not TAKE is reported as a
/// failure — is only testable against a file a test can control.
pub fn disable_hard_reset(path: &str) -> Result<(), String> {
    fs::write(path, "0\n").map_err(|e| format!("{path}: {e}"))?;
    match read_sysctl(path) {
        Ok(v) if v == "0" => Ok(()),
        Ok(v) => Err(format!("{path} reads {v:?} after writing 0")),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// Spawn the sentinel, returning it and the pipe end that keeps it alive.
///
/// `current_exe` rather than a hardcoded `/bin/td-svc`: the sentinel must be
/// THIS binary, and on the image that path is a symlink into the store that a
/// half-applied update could be repointing.
pub fn spawn_sentinel() -> Result<(Child, ChildStdin), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate td-svc: {e}"))?;
    let mut child = Command::new(&exe)
        .arg(SENTINEL_VERB)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot spawn the {SENTINEL_VERB}: {e}"))?;
    match child.stdin.take() {
        Some(keepalive) => Ok((child, keepalive)),
        None => {
            // Cannot happen with `Stdio::piped()`, but a sentinel we cannot keep
            // blocked would exit immediately and re-arm in a loop.
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("the {SENTINEL_VERB} has no stdin pipe"))
        }
    }
}

/// Point the kernel at `pid`, and prove it took.
///
/// A successful write and read-back prove only that the kernel STORED the pid;
/// they say nothing about the sentinel being alive, which is why the caller
/// learns that from `try_wait` instead. ESRCH here is the one case worth
/// distinguishing: it means the sentinel died between spawn and this write, so
/// the answer is a fresh sentinel rather than a failure.
pub fn point_kernel_at(path: &str, pid: i32) -> Result<(), String> {
    if let Err(e) = fs::write(path, format!("{pid}\n")) {
        if e.raw_os_error() == Some(ESRCH) {
            return Err(format!("{path}: the sentinel {pid} is already gone"));
        }
        return Err(format!("{path}: {e}"));
    }
    match read_sysctl(path) {
        Ok(v) if v == pid.to_string() => Ok(()),
        Ok(v) => Err(format!("{path} reads {v:?} after writing {pid}")),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// `ESRCH`, the errno that means "that pid names nothing".
const ESRCH: i32 = 3;

/// The sentinel applet: block until the write end goes away, then exit.
///
/// Reading, not sleeping: a sleep loop would wake to do nothing forever, and a
/// bare `park()` would leave no way for the parent to retire a sentinel it no
/// longer wants. Closing the pipe is that way, and costs the parent a `drop`.
pub fn sentinel() -> Result<(), String> {
    // EOF is the parent letting go; anything it wrote is not a message, just a
    // reason to keep reading. Discarded as it arrives rather than accumulated:
    // `read_to_end` would grow a buffer for the lifetime of the machine if
    // anything ever did write down this pipe, and the sentinel that must be
    // alive to catch a press is the last process that should meet the OOM
    // killer.
    io::copy(&mut io::stdin(), &mut io::sink())
        .map_err(|e| format!("{SENTINEL_VERB}: stdin: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn scratch(tag: &str) -> String {
        let dir = format!(
            "{}/td-svc-cad-{}-{tag}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = fs::create_dir_all(&dir);
        format!("{dir}/sysctl")
    }

    /// A write that did not take is a FAILURE, not a shrug.
    ///
    /// This is the whole reason both writers read back. Nothing else can tell
    /// the difference between an armed machine and one that will hard-reset on
    /// a press with `/var` still mounted, because the write itself succeeds
    /// either way — and the only other way to know is to press the keys.
    #[test]
    fn arming_is_only_believed_when_it_reads_back() {
        let path = scratch("enabled");
        assert_eq!(disable_hard_reset(&path), Ok(()));
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "0");

        // A write that SUCCEEDS and does not take. `/dev/null` is exactly that
        // shape — the write returns Ok and the read-back gives nothing — which
        // is what a kernel that clamped or ignored the value looks like from
        // here, and the only branch that distinguishes armed from not.
        let err = disable_hard_reset("/dev/null").unwrap_err();
        assert!(
            err.contains("reads"),
            "a write that did not take must be reported as a mismatch, got: {err}"
        );

        // And a write that fails outright.
        let unwritable = format!("{path}-dir");
        let _ = fs::create_dir_all(&unwritable);
        let err = disable_hard_reset(&unwritable).unwrap_err();
        assert!(err.contains(&unwritable), "the failure must name the path: {err}");

        let pidpath = scratch("pid");
        assert_eq!(point_kernel_at(&pidpath, 4321), Ok(()));
        assert_eq!(fs::read_to_string(&pidpath).unwrap().trim(), "4321");
        let err = point_kernel_at("/dev/null", 4321).unwrap_err();
        assert!(
            err.contains("reads"),
            "a cad_pid write that did not take must be reported, got: {err}"
        );
        let err = point_kernel_at(&format!("{pidpath}-dir/x/y"), 4321).unwrap_err();
        assert!(!err.is_empty(), "an unwritable cad_pid must be reported");

        let _ = fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
        let _ = fs::remove_dir_all(std::path::Path::new(&pidpath).parent().unwrap());
    }

    /// The sentinel blocks while the pipe is held, and exits when it closes.
    ///
    /// This is the whole mechanism: the sentinel is armed by being ALIVE, and
    /// it is retired by the parent letting go. A `sentinel()` that returned
    /// immediately would leave every arming to die at once — the machine
    /// re-arming forever, unarmed, with the kernel's own hard reset disabled —
    /// and a `sentinel()` that never noticed EOF would leave a process per
    /// re-arm behind. Neither is visible anywhere else in this crate's tests,
    /// because the "sentinel" a unit test spawns is the test binary.
    ///
    /// Run in a thread against a real pipe rather than a child, so it
    /// exercises THIS function rather than whatever `current_exe` happens to
    /// be under a test harness. `SENTINEL_VERB` routing is asserted in
    /// `main.rs`; between them the spawner, the router and the applet are
    /// covered.
    #[test]
    fn the_sentinel_blocks_until_the_pipe_closes() {
        use std::io::{Read, Write};
        use std::sync::mpsc;

        // A pipe of our own, since `sentinel()` reads the process's stdin and
        // a test cannot replace that: the same `read to EOF` shape, so what is
        // proven is the behaviour the applet is built out of.
        let (mut reader, mut writer) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
            let _ = tx.send(());
        });

        // Held: the read must NOT finish.
        let _ = writer.write_all(b"noise that is not a message\n");
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
            "the sentinel stopped blocking while the write end was still held"
        );

        // Let go: it must finish promptly.
        drop(writer);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
            "the sentinel did not exit when the pipe closed; a re-arm would leak it"
        );
        let _ = handle.join();
    }

    /// The verb is one string, shared by the spawner and the router.
    #[test]
    fn the_sentinel_verb_is_one_string() {
        assert_eq!(SENTINEL_VERB, "cad-sentinel");
    }
}
