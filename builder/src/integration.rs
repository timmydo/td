//! `td-builder check integration`: the system-level qemu oracles, which
//! boot the system image in a VM with the host's qemu and firmware. They
//! run on the host after the sandboxed gates pass, never inside the gate
//! sandbox, and are not part of `check`: main runs them, and a branch runs
//! them only when it changes the boot path (`affected::boot_path`), which
//! `ready` says when it defers them.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// The goal word `td-builder check` takes for this tier.
pub(crate) const GOAL: &str = "integration";

/// One step: its name, its `td-recipe-eval` argv, and what it proves.
struct Step {
    name: &'static str,
    argv: &'static [&'static str],
    proves: &'static str,
}

/// In order: fetch what the system image needs, so a missing input fails
/// fast and named rather than inside a boot; then the cheapest boot first.
const STEPS: &[Step] = &[
    Step {
        name: "warm",
        argv: &["warm", "system-x86-64"],
        proves: "the system image's pinned inputs are present",
    },
    Step {
        name: "qemu-boot-system",
        argv: &["qemu-boot-system"],
        proves: "the system image boots to its services and session",
    },
    Step {
        name: "qemu-boot-live",
        argv: &["qemu-boot-live"],
        proves: "the live medium boots through UEFI firmware",
    },
    Step {
        name: "qemu-install-system",
        argv: &["qemu-install-system"],
        proves: "the installer installs the system from its medium and it boots",
    },
];

/// What `ready` prints when a branch leaves this tier to main.
pub(crate) fn deferred_note() -> String {
    let names: Vec<&str> = STEPS
        .iter()
        .filter(|s| s.name != "warm")
        .map(|s| s.name)
        .collect();
    format!(
        "deferred to main: the system-level qemu oracles ({}), run by \
         `td-builder check {GOAL}`; no boot-path file changed",
        names.join(", ")
    )
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Outcome {
    Pass,
    HostGap,
    Fail,
}

impl Outcome {
    fn of(code: Option<i32>) -> Self {
        match code {
            Some(0) => Outcome::Pass,
            Some(td_engine::exit::EXIT_UNPROVISIONED) => Outcome::HostGap,
            _ => Outcome::Fail,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::HostGap => "host-gap",
            Outcome::Fail => "fail",
        }
    }
}

/// Run the steps with the evaluator at `eval`, each with its wall time, and
/// record each in the check history. A `warm` that does not pass stops the
/// rest, which would fail or build cold on its account. The exit: 0 when
/// nothing failed and at least one oracle ran, `EXIT_UNPROVISIONED` with
/// its sentinel when no oracle could run on this host (a skip is not a
/// pass), 1 on any failure.
pub(crate) fn run(root: &Path, eval: &str) -> i32 {
    println!(">> {GOAL}: {} step(s) on the host", STEPS.len());
    let mut results: Vec<(&str, Outcome, Duration)> = Vec::new();
    for step in STEPS {
        println!(
            "================ {GOAL} {}: {} ================",
            step.name, step.proves
        );
        let started = Instant::now();
        let mut cmd = Command::new(eval);
        cmd.args(step.argv).current_dir(root);
        // An oracle runs for many minutes: it must not outlive `td-builder
        // stop` or a dead check host, holding the ladder lock.
        crate::host_bin::arm_check_child(&mut cmd);
        let status = cmd.status();
        let took = started.elapsed();
        let outcome = match &status {
            Ok(st) => Outcome::of(st.code()),
            Err(e) => {
                eprintln!("{GOAL}: could not start {eval}: {e}");
                Outcome::Fail
            }
        };
        println!(
            "================ {GOAL} {}: {} ({:.1}s) ================",
            step.name,
            outcome.word().to_uppercase(),
            took.as_secs_f64()
        );
        record(root, eval, step.name, outcome, took);
        results.push((step.name, outcome, took));
        if step.name == "warm" && outcome != Outcome::Pass {
            println!(
                ">> {GOAL}: warm {}; the oracles would fail on its account, not run",
                outcome.word()
            );
            break;
        }
    }
    let (line, code) = verdict(&results);
    println!("{line}");
    if code == td_engine::exit::EXIT_UNPROVISIONED {
        println!(
            ">> {GOAL}: UNPROVISIONED — no oracle could run on this host (qemu, or \
             OVMF for the UEFI boots); a skip is not a pass"
        );
        // A caller reads a host gap by the code and the sentinel together.
        eprintln!("{}", td_engine::exit::UNPROVISIONED_SENTINEL);
    }
    code
}

/// The summary line and exit code for `results`: the oracles counted, every
/// step timed; an oracle the run never reached counts as not run.
fn verdict(results: &[(&str, Outcome, Duration)]) -> (String, i32) {
    let oracles: Vec<&(&str, Outcome, Duration)> =
        results.iter().filter(|r| r.0 != "warm").collect();
    let count = |o: Outcome| oracles.iter().filter(|r| r.1 == o).count();
    let total = STEPS.iter().filter(|s| s.name != "warm").count();
    let timed: Vec<String> = results
        .iter()
        .map(|(n, o, t)| format!("{n} {} {:.1}s", o.word(), t.as_secs_f64()))
        .collect();
    let line = format!(
        ">> {GOAL}: of {total} oracle(s), {} passed, {} unprovisioned, {} failed, {} not run: {}",
        count(Outcome::Pass),
        count(Outcome::HostGap),
        count(Outcome::Fail),
        total.saturating_sub(oracles.len()),
        timed.join(", ")
    );
    let failed = results.iter().any(|r| r.1 == Outcome::Fail);
    let code = if failed {
        1
    } else if oracles.iter().all(|r| r.1 == Outcome::HostGap) {
        td_engine::exit::EXIT_UNPROVISIONED
    } else {
        0
    };
    (line, code)
}

/// Append the step to the check history through the evaluator, which owns
/// its format and place. Best-effort, like the history itself.
fn record(root: &Path, eval: &str, name: &str, outcome: Outcome, took: Duration) {
    let check = format!("{GOAL}:{name}");
    let secs = format!("{:.1}", took.as_secs_f64());
    let mut cmd = Command::new(eval);
    cmd.args(["check-history", "--record", &check, outcome.word(), &secs])
        .current_dir(root);
    crate::host_bin::arm_check_child(&mut cmd);
    let recorded = cmd.status();
    if !recorded.is_ok_and(|s| s.success()) {
        eprintln!("{GOAL}: {check} not recorded in the check history (non-fatal)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure fails the tier; every oracle a host gap is its own skip,
    /// not a pass; one oracle that ran is a pass beside gaps.
    #[test]
    fn the_verdict_counts_a_skip_apart_from_a_pass() {
        let s = Duration::from_secs;
        let gap = Outcome::HostGap;
        let all_gap = [
            ("warm", Outcome::Pass, s(1)),
            ("qemu-boot-system", gap, s(0)),
            ("qemu-boot-live", gap, s(0)),
        ];
        assert_eq!(verdict(&all_gap).1, td_engine::exit::EXIT_UNPROVISIONED);
        let one_ran = [
            ("warm", Outcome::Pass, s(1)),
            ("qemu-boot-system", Outcome::Pass, s(600)),
            ("qemu-boot-live", gap, s(0)),
        ];
        let (line, code) = verdict(&one_ran);
        assert_eq!(code, 0);
        assert!(
            line.contains("of 3 oracle(s), 1 passed, 1 unprovisioned, 0 failed, 1 not run"),
            "{line}"
        );
        assert!(line.contains("qemu-boot-system pass 600.0s"), "{line}");
        let failed = [("warm", Outcome::Fail, s(1))];
        assert_eq!(verdict(&failed).1, 1);
        // A warm the host could not run stops the tier as a gap, not a pass.
        let warm_gap = [("warm", gap, s(0))];
        let (line, code) = verdict(&warm_gap);
        assert_eq!(code, td_engine::exit::EXIT_UNPROVISIONED);
        assert!(
            line.contains("0 passed, 0 unprovisioned, 0 failed, 3 not run"),
            "{line}"
        );
        assert_eq!(Outcome::of(Some(0)), Outcome::Pass);
        assert_eq!(
            Outcome::of(Some(td_engine::exit::EXIT_UNPROVISIONED)),
            Outcome::HostGap
        );
        assert_eq!(Outcome::of(None), Outcome::Fail);
    }

    #[test]
    fn the_deferred_note_names_the_oracles_and_the_command() {
        let note = deferred_note();
        for name in ["qemu-boot-system", "qemu-boot-live", "qemu-install-system"] {
            assert!(note.contains(name), "{note}");
        }
        assert!(note.contains("td-builder check integration"), "{note}");
        assert!(!note.contains("warm"), "{note}");
    }
}
