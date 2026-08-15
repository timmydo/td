//! One answer to `ETXTBSY` for every program this binary execs.
//!
//! The kernel refuses to exec a file anyone holds open for WRITING, and this
//! binary races itself for that: a thread writes a program and execs it, and a
//! sibling forking anywhere in between hands its child a duplicate of the write
//! descriptor. `O_CLOEXEC` — which `std` does set — bounds that window to the
//! child's own exec rather than its lifetime, but cannot remove it, because the
//! descriptor is live for exactly as long as the child has not exec'd yet.
//! Cargo relinking a binary the gate is about to run is the same condition
//! arriving from outside, and lasts as long as a link.
//!
//! Neither is a verdict. Both end when the writer lets go, so both are waited
//! out here, in ONE place: the mechanism was discovered twice and written down
//! twice before this module existed, with two policies and two spellings of the
//! same errno.

use std::io;
use std::time::Duration;

/// The two durations as NAMED fields rather than two arguments of one type.
/// Transposed they compile, leave `step < ceiling` true of the constants below,
/// and turn every busy spawn into a single maximal sleep that then gives up —
/// which nothing inside the loop can notice.
#[derive(Clone, Copy)]
pub(crate) struct Backoff {
    pub step: Duration,
    pub ceiling: Duration,
}

/// The step is sized for a fork's window, which is microseconds; the ceiling
/// for a link, which is the slow case.
pub(crate) const BUSY: Backoff = Backoff {
    step: Duration::from_millis(5),
    ceiling: Duration::from_secs(10),
};

/// Whether a spawn failed because someone still holds the program open for
/// writing. The one place that names the condition, so a caller wanting to
/// report it — [`crate::stage0`]'s fixture prover does — does not have to spell
/// an errno or a kind of its own.
pub(crate) fn is_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ExecutableFileBusy
}

/// Run `attempt` until it stops failing with `ETXTBSY`, up to [`BUSY`].
///
/// Every other error is returned untouched and on the FIRST attempt: a retry
/// that waited out real failures would report a permission problem ten seconds
/// late, which is a worse answer, not a better one.
pub(crate) fn past_a_busy_program<T>(attempt: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    waiting_out_busy(attempt, BUSY)
}

/// The body above with its backoff as a parameter, so the give-up path is
/// reachable from a test without waiting the shipped ceiling out.
pub(crate) fn waiting_out_busy<T>(
    mut attempt: impl FnMut() -> io::Result<T>,
    backoff: Backoff,
) -> io::Result<T> {
    let mut waited = Duration::ZERO;
    loop {
        match attempt() {
            Err(error) if is_busy(&error) && waited < backoff.ceiling => {
                std::thread::sleep(backoff.step);
                waited = waited.saturating_add(backoff.step);
            }
            // The refusal at the ceiling is returned as itself: a writer that
            // never let go is worth a failure, and says so in its own words
            // rather than as a timeout of this module's invention.
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn quick(step_ms: u64, ceiling_ms: u64) -> Backoff {
        Backoff {
            step: Duration::from_millis(step_ms),
            ceiling: Duration::from_millis(ceiling_ms),
        }
    }

    /// The mapping the whole module turns on, asked of the kernel rather than
    /// assumed. If Linux's `ETXTBSY` did not reach Rust as
    /// `ExecutableFileBusy`, every logic test below would still pass while the
    /// retry arm never fired once.
    ///
    /// The recovery half then drives the SHIPPED entry point against a real
    /// busy program. The writer is released INSIDE the attempt, on the second
    /// call, so recovery is a fact about this loop rather than about a timer.
    /// Releasing it beforehand and asserting the next spawn works would
    /// re-create the very race this module removes — a sibling test's fork can
    /// hold the descriptor past our `drop` until its own exec.
    #[test]
    fn a_program_held_open_for_writing_is_waited_out() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("td-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("held");
        let mut writer = std::fs::File::create(&program).unwrap();
        writer.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perm = std::fs::metadata(&program).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&program, perm).unwrap();

        // Still open — that IS the fixture.
        let held = Command::new(&program).spawn();
        assert_eq!(
            held.map(|_| ()).map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::ExecutableFileBusy)
        );

        let mut held_open = Some(writer);
        let mut attempts = 0u32;
        let released = past_a_busy_program(|| {
            attempts = attempts.saturating_add(1);
            if attempts >= 2 {
                held_open = None;
            }
            Command::new(&program).spawn()
        });
        assert!(attempts >= 2, "the shipped helper never retried");
        // "No longer BUSY" rather than success: the exec clears the busy check
        // before it resolves `#!/bin/sh`, so a machine without that shell
        // would fail this spawn for an unrelated reason.
        assert_ne!(
            released.as_ref().err().map(|e| e.kind()),
            Some(io::ErrorKind::ExecutableFileBusy),
            "the helper did not recover once the writer let go"
        );
        if let Ok(mut child) = released {
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three arms, with canned errors and their own backoff so no test
    /// sleeps for the shipped ceiling.
    #[test]
    fn a_busy_program_is_waited_out_and_nothing_else_is() {
        use std::io::Error;
        let backoff = quick(1, 5);

        // Busy, then ready. The elapsed LOWER bound is what pins that the loop
        // actually sleeps: delete the sleep and keep the accumulator and every
        // other assertion here still holds, while the retry becomes a hot spin
        // that burns a core for ten seconds against a link.
        let started = std::time::Instant::now();
        let mut left = 3u32;
        let mut tries = 0u32;
        let got = waiting_out_busy(
            || {
                tries = tries.saturating_add(1);
                if left > 0 {
                    left = left.saturating_sub(1);
                    return Err(Error::from(io::ErrorKind::ExecutableFileBusy));
                }
                Ok(7u8)
            },
            backoff,
        );
        assert_eq!(got.ok(), Some(7));
        assert_eq!(tries, 4, "a busy program should be retried until it is not");
        assert!(
            started.elapsed() >= backoff.step.saturating_mul(3),
            "three retries should have slept three steps"
        );

        // Anything else is the verdict, on the FIRST try.
        let mut other = 0u32;
        let denied = waiting_out_busy(
            || {
                other = other.saturating_add(1);
                Err::<u8, _>(Error::from(io::ErrorKind::PermissionDenied))
            },
            backoff,
        );
        assert_eq!(other, 1);
        assert_eq!(
            denied.map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::PermissionDenied)
        );

        // A writer that never lets go still fails, as itself.
        let mut forever = 0u32;
        let stuck = waiting_out_busy(
            || {
                forever = forever.saturating_add(1);
                Err::<u8, _>(Error::from(io::ErrorKind::ExecutableFileBusy))
            },
            backoff,
        );
        assert_eq!(forever, 6, "five 1ms sleeps fit under a 5ms ceiling");
        assert_eq!(
            stuck.map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::ExecutableFileBusy)
        );
    }

    /// The shipped pair, which the arms above do not use: a zero step spins,
    /// and a ceiling below the step buys one maximal sleep and then gives up.
    #[test]
    fn the_shipped_backoff_retries_more_than_once() {
        assert!(!BUSY.step.is_zero(), "a zero step would spin");
        assert!(
            BUSY.step.saturating_mul(2) <= BUSY.ceiling,
            "a ceiling this close to the step is a single sleep, not a retry"
        );
    }

    /// One policy, enforced rather than intended. This mechanism was discovered
    /// twice and written down twice before this module existed, with two
    /// spellings of the same errno and two sets of constants — and a third copy
    /// would be invisible to every other test here, so the roster is the
    /// absence of one. Read off the directory rather than a list of modules,
    /// because a list is exactly what a new module is missing from.
    #[test]
    fn nothing_outside_this_module_carries_its_own_busy_retry() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let Ok(entries) = std::fs::read_dir(&src) else {
            return;
        };
        let mut offenders = Vec::new();
        for path in entries.flatten().map(|e| e.path()) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if !name.ends_with(".rs") || name == "spawn.rs" {
                continue;
            }
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            // The errno by NUMBER is what the copy this module replaced used,
            // and what a new one would reach for; the named kind belongs here.
            if body.contains("Some(26)") || body.contains("ExecutableFileBusy") {
                offenders.push(name.to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "a second busy retry outside spawn.rs: {offenders:?}"
        );
    }

    /// The retry re-`spawn`s the SAME `Command`, which is only sound if the
    /// stdio configuration survives an attempt. `run_recipe_check` reads its
    /// child's stderr for the host-gap sentinel, so a second attempt that fell
    /// back to an inherited stderr would take that sentinel away and turn a
    /// skip into a failure — a review raised exactly that.
    ///
    /// It does not happen, because `piped()` is a MARKER rather than a
    /// descriptor and each spawn makes its own pipe. Pinned because it is a
    /// property of `std` this module DEPENDS on rather than one it owns: were
    /// it to change, the retry would go on working while quietly reporting the
    /// wrong verdict for a host gap.
    #[test]
    fn a_respawned_command_keeps_its_piped_stdio() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut command = Command::new(exe);
        // `--list` exits promptly, and `wait_with_output` DRAINS both pipes,
        // which a bare `wait` would not — a listing bigger than the pipe
        // buffer would then block forever.
        command
            .arg("--list")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for attempt in 1..=2 {
            let Ok(child) = command.spawn() else {
                continue;
            };
            assert!(child.stdout.is_some(), "attempt {attempt} lost its stdout");
            assert!(child.stderr.is_some(), "attempt {attempt} lost its stderr");
            let _ = child.wait_with_output();
        }
    }
}
