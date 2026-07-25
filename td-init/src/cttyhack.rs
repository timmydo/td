//! `cttyhack PROG [ARG...]` — give PROG a controlling terminal, then exec it.
//!
//! A process spawned by init inherits init's session but does not lead it, so
//! `TIOCSCTTY` is EPERM for it and the shell it execs comes up with no
//! controlling terminal: no job control, and ^C kills nothing. This applet is
//! the standard fix — `setsid(2)`, open the console, claim it, exec — and it is
//! why init's own inittab spawns shells through it rather than directly.
//!
//! No `dup2(2)` is needed to put the console on 0/1/2: `Stdio::from(File)` makes
//! `CommandExt::exec` do that redirection in its own (safe) pre-exec setup.
//!
//! If the console cannot be claimed the program is exec'd anyway, on inherited
//! stdio. cttyhack must never be the reason a rescue shell fails to start.

use crate::sys;
use std::fs::{File, OpenOptions};
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// EPERM — `setsid(2)` returns it when we already lead a process group, which
/// is a success for our purposes, not a failure. `TIOCSCTTY` returns it for a
/// console another session still holds, which is worth reporting.
const EPERM: i32 = 1;

/// `O_NOCTTY` — the terminal is claimed explicitly below, never by side effect.
const O_NOCTTY: i32 = 0o400;

const DEV_TTY: &str = "/dev/tty";
const SYS_CONSOLE_ACTIVE: &str = "/sys/class/tty/console/active";

fn usage() -> String {
    "usage: cttyhack PROG [ARG...]".to_string()
}

/// `/dev/tty` opens only for a process that HAS a controlling terminal, so this
/// is the test itself, not an approximation of one.
fn has_controlling_tty() -> bool {
    OpenOptions::new().read(true).write(true).open(DEV_TTY).is_ok()
}

/// The console device to claim. The kernel lists every registered console in
/// `/sys/class/tty/console/active` in registration order; the LAST is the one
/// `/dev/console` speaks to, which on td's images is the serial console when one
/// is configured and the VT otherwise.
fn console_name(active: &str) -> String {
    match active.split_whitespace().last() {
        Some(name) => format!("/dev/{name}"),
        None => "/dev/console".to_string(),
    }
}

fn console_path() -> String {
    match std::fs::read_to_string(SYS_CONSOLE_ACTIVE) {
        Ok(active) => console_name(&active),
        Err(_) => "/dev/console".to_string(),
    }
}

/// Become a session leader. Without a session of our own the terminal cannot
/// become ours at all, so this comes first.
///
/// EPERM means we already lead a process group — either this session's, or one
/// inside somebody else's. Neither is worth reporting: the `TIOCSCTTY` below is
/// the thing that decides, and it reports for itself.
fn become_session_leader() {
    if let Err(e) = sys::setsid() {
        if e.raw_os_error() != Some(EPERM) {
            crate::emit_err(&format!("cttyhack: setsid: {e}\n"));
        }
    }
}

/// Claim the terminal we were handed on stdin. Same steps as the console path
/// minus the open: the fd is already the right one, so nothing needs rewiring.
fn claim_inherited() {
    become_session_leader();
    claim(std::io::stdin().as_raw_fd(), "the inherited terminal");
}

/// What a `TIOCSCTTY(0)` result means. Note what is NOT here: a steal.
///
/// The kernel clears the controlling-terminal association when a session LEADER
/// exits, so a terminal still held is one a LIVE session holds and `TIOCSCTTY(1)`
/// could only ever detach it — for `cttyhack sh` typed at a console, the
/// operator's own. busybox DOES steal unconditionally (`shell/cttyhack.c`, "try
/// to steal it from them"); this is a deliberate divergence, not its precedent.
#[derive(Debug, PartialEq, Eq)]
enum Escalation {
    /// The terminal is ours; nothing more to do.
    Held,
    /// EPERM. Report and carry on without it.
    Occupied,
    /// A failure that says nothing about who holds it — report it as-is.
    Report,
}

fn escalation(err: Option<i32>) -> Escalation {
    match err {
        None => Escalation::Held,
        Some(EPERM) => Escalation::Occupied,
        Some(_) => Escalation::Report,
    }
}

fn claim(fd: std::os::fd::RawFd, what: &str) {
    let first = sys::set_controlling_tty(fd);
    let err = match &first {
        Ok(()) => None,
        Err(e) => Some(e.raw_os_error().unwrap_or(0)),
    };
    match escalation(err) {
        Escalation::Held => {}
        // EPERM covers three kernel causes — not a session leader, already have a
        // controlling terminal, or this one is taken — so the message names the
        // outcome rather than guessing which. Only the third is reachable from an
        // inittab job, whose children are not process-group leaders.
        Escalation::Occupied => crate::emit_err(&format!(
            "cttyhack: cannot claim {what} (EPERM); continuing without a controlling terminal\n"
        )),
        Escalation::Report => {
            if let Err(e) = first {
                crate::emit_err(&format!("cttyhack: TIOCSCTTY on {what}: {e}\n"));
            }
        }
    }
}

/// Become a session leader and claim the console. The path is resolved BEFORE
/// `setsid(2)`: that call drops whatever controlling terminal we had, so doing
/// any fallible work after it risks leaving the process worse off than it was.
fn claim_console() -> Result<File, String> {
    let path = console_path();
    become_session_leader();
    // `O_NOCTTY` so the ioctl below is the SOLE acquisition mechanism: a session
    // leader opening a free tty would otherwise acquire it implicitly, and the
    // ioctl's result would then say nothing about whether we hold the console.
    let console = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(&path)
        .map_err(|e| format!("{path}: {e}"))?;
    // A failed claim is reported, never fatal: a rescue shell without job
    // control still beats no shell at all.
    claim(console.as_raw_fd(), &path);
    Ok(console)
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let prog = args.first().ok_or_else(usage)?;
    let mut cmd = Command::new(prog);
    cmd.args(args.get(1..).unwrap_or(&[]));

    if !has_controlling_tty() {
        // If stdin is ALREADY a terminal, that is the one to claim: init opened
        // the tty its inittab entry named and wired it onto 0/1/2, so opening
        // the global console here would move a `tty1:` job onto ttyS0. Claim
        // what we were given and leave the stdio alone.
        if std::io::stdin().is_terminal() {
            claim_inherited();
        } else {
            match claim_console() {
                Ok(console) => match (console.try_clone(), console.try_clone()) {
                    (Ok(out), Ok(err)) => {
                        cmd.stdin(Stdio::from(console))
                            .stdout(Stdio::from(out))
                            .stderr(Stdio::from(err));
                    }
                    _ => crate::emit_err("cttyhack: could not duplicate the console handle\n"),
                },
                Err(e) => crate::emit_err(&format!("cttyhack: {e}\n")),
            }
        }
    }

    // `exec` replaces this process image and does not return on success.
    Err(format!("exec {prog}: {}", cmd.exec()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// The LAST console listed is the one `/dev/console` maps to — taking the
    /// first would hand a serial-console boot the VT instead.
    #[test]
    fn the_last_active_console_is_the_one_claimed() {
        assert_eq!(console_name("tty0 ttyS0\n"), "/dev/ttyS0");
        assert_eq!(console_name("ttyS0\n"), "/dev/ttyS0");
        assert_eq!(console_name("  tty0   ttyS0  "), "/dev/ttyS0");
    }

    /// An empty or absent listing falls back to `/dev/console`, which the kernel
    /// always provides.
    #[test]
    fn an_empty_listing_falls_back_to_dev_console() {
        assert_eq!(console_name(""), "/dev/console");
        assert_eq!(console_name("   \n"), "/dev/console");
    }

    /// No program means nothing to exec — diagnosed before any session or
    /// terminal state is touched.
    #[test]
    fn a_missing_program_is_a_usage_error() {
        assert!(run(&[]).unwrap_err().contains("usage: cttyhack"));
    }

    /// There is no outcome that takes a terminal from whoever holds it. EPERM
    /// means a live session has it — the kernel releases the association when a
    /// session leader exits, so "still held" and "still alive" are the same
    /// fact — and `cttyhack sh` typed at an operator's own console is exactly
    /// that case. A rescue shell without job control beats taking their
    /// terminal away, so the applet continues without one.
    #[test]
    fn a_terminal_held_by_a_live_session_is_left_alone() {
        assert_eq!(escalation(Some(EPERM)), Escalation::Occupied);
        assert_eq!(escalation(None), Escalation::Held);
        // Any other errno says nothing about ownership; report it as-is.
        assert_eq!(escalation(Some(25)), Escalation::Report); // ENOTTY
        assert_eq!(escalation(Some(9)), Escalation::Report); // EBADF
    }
}
