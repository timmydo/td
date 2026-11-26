//! `td-builder check integration`: the PostgreSQL and system-level QEMU oracles. The latter
//! boot the system image in a VM with the host's qemu and firmware. They
//! run on the host after the sandboxed gates pass, never inside the gate
//! sandbox, and are not part of `check`: main runs them after a landing,
//! and a branch's `ready` never does, saying so when it defers them; a
//! boot-path change (`affected::boot_path`) is named in its notes.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// The goal word `td-builder check` takes for this tier.
pub(crate) const GOAL: &str = "integration";

pub(crate) const POSTGRES: &str = "postgres-integration";
pub(crate) const POSTGRES_PACKAGE: &str = "postgresql-x86-64-test";
const VM_LIFECYCLE: &str = "qemu-vm-lifecycle";

/// One step: its name, command arguments, and what it proves.
struct Step {
    name: &'static str,
    argv: &'static [&'static str],
    proves: &'static str,
}

/// Run PostgreSQL independently of image warming, then fetch the inputs so a missing input fails
/// fast and named rather than inside a boot; then the cheapest boot first.
const STEPS: &[Step] = &[
    Step {
        name: POSTGRES,
        argv: &["gate-crates", "crypto-cargo", POSTGRES, "--manifest-path", "td-mta/Cargo.toml"],
        proves: "the client, mail service, deployment and restore drill work against disposable PostgreSQL 17 clusters",
    },
    Step {
        name: POSTGRES_PACKAGE,
        argv: &["check-run", POSTGRES_PACKAGE, "1"],
        proves: "the recipe-built PostgreSQL package starts a SCRAM cluster, uses pg_trgm and restores its dump",
    },
    Step {
        name: "warm",
        argv: &["warm", "system-x86-64", "--flathub"],
        proves: "the system image's pinned inputs are present, and the host's feed holds the \
                 Flathub applications the guests fetch",
    },
    Step {
        name: VM_LIFECYCLE,
        argv: &["test", "--frozen", "--manifest-path", "td-vm/Cargo.toml", "qemu_integration_", "--", "--ignored"],
        proves: "td-vm creates independent overlays, supervises the guest, and preserves workspace bindings and Git enrollment",
    },
    Step {
        name: "qemu-boot-system",
        argv: &["qemu-boot-system"],
        proves: "the system image boots to its services and session, installs Firefox \
                 through root's application-cache operation, and serves td-software's \
                 install and status requests from its control socket",
    },
    Step {
        name: "qemu-boot-server",
        argv: &["qemu-boot-server"],
        proves: "the headless server image boots to its system units and td-sshd with nothing graphical, and a candidate that fails health reboots itself until it rolls back",
    },
    Step {
        name: "qemu-boot-audio",
        argv: &["qemu-boot-audio"],
        proves: "td-audio moves a playing stream from the HDA output to a USB output mid-tone, \
                 each output playing its part of the tone and the player told of no seam",
    },
    Step {
        name: "qemu-boot-audio-hotplug",
        argv: &["qemu-boot-audio-hotplug"],
        proves: "td-audio follows a USB output plugged in, out and back in: listed without \
                 taking over, the unplug falling back to HDA, and the return playing again as \
                 the human's choice at its old level",
    },
    Step {
        name: "qemu-deploy-rollback",
        argv: &["qemu-deploy-rollback"],
        proves: "an approved deploy-rollback selects the previous system, and an approved set-hostname names the system, at the next boot",
    },
    Step {
        name: "qemu-admin-update",
        argv: &["qemu-admin-update"],
        proves: "the guest acquires signed releases through restricted SSH administration, refuses expired metadata and root rollback, resumes HTTP transfers, and rolls back failed boots",
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
        "deferred to main: the PostgreSQL and system-level qemu oracles ({}), which main \
         runs after a landing; `td-builder check {GOAL}` runs them by hand",
        names.join(", ")
    )
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Outcome {
    Pass,
    /// Passed before with every input it boots unchanged; booted nothing.
    Memo,
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
            Outcome::Memo => "memo",
            Outcome::HostGap => "host-gap",
            Outcome::Fail => "fail",
        }
    }
}

/// What the oracle memo said of a step before it ran.
#[derive(Debug, PartialEq)]
enum Memo {
    /// Not asked (`warm`), or the question failed: the oracle boots.
    Unasked,
    Hit(String),
    Miss(String, String),
}

/// The evaluator's answer line, `hit KEY` or `miss KEY WHY`, or None for
/// anything else: a key that is not a sha256 hex digest is no answer, so a
/// garbled reply boots rather than passes.
fn parse_memo(line: &str) -> Option<Memo> {
    let mut words = line.splitn(3, ' ');
    let (verdict, key, rest) = (words.next()?, words.next()?, words.next());
    let hex = key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match (verdict, hex, rest) {
        ("hit", true, None) => Some(Memo::Hit(key.to_string())),
        ("miss", true, why) => Some(Memo::Miss(
            key.to_string(),
            why.unwrap_or_default().to_string(),
        )),
        _ => None,
    }
}

/// Ask the evaluator whether `name` passed before with every input it boots
/// unchanged (`td-recipe-eval oracle-memo`, which also answers a miss under
/// `TD_CHECK_FULL`). A failure to ask is not an answer: the oracle boots.
fn ask_memo(root: &Path, eval: &str, name: &str) -> Memo {
    let mut cmd = Command::new(eval);
    cmd.args(["oracle-memo", name]).current_dir(root);
    crate::host_bin::arm_check_child(&mut cmd);
    let out = match cmd.output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "{GOAL}: oracle-memo {name} failed (it boots): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return Memo::Unasked;
        }
        Err(e) => {
            eprintln!("{GOAL}: could not ask the oracle memo for {name} (it boots): {e}");
            return Memo::Unasked;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().last().unwrap_or_default();
    parse_memo(line).unwrap_or_else(|| {
        eprintln!("{GOAL}: oracle-memo {name} said {line:?} (it boots)");
        Memo::Unasked
    })
}

/// Forget or record `name`'s pass under `key` through the evaluator:
/// `--forget` before a boot, so a doubted pass that then fails is not
/// answered from; `--record` after a pass, which the evaluator declines if
/// the inputs moved during the boot.
fn tell_memo(root: &Path, eval: &str, name: &str, flag: &str, key: &str) {
    let mut cmd = Command::new(eval);
    cmd.args(["oracle-memo", name, flag, key]).current_dir(root);
    crate::host_bin::arm_check_child(&mut cmd);
    if !cmd.status().is_ok_and(|s| s.success()) {
        eprintln!("{GOAL}: oracle-memo {name} {flag} failed (non-fatal)");
    }
}

/// Run the steps with the evaluator at `eval`, each with its wall time, and
/// record each in the check history. An oracle that passed before with
/// every input it boots unchanged answers from the memo and boots nothing
/// (`TD_CHECK_FULL` boots it anyway); when every oracle does, `warm` is
/// skipped too. Each oracle is asked again when its step is reached, since
/// the boots before it can take an hour. A `warm` that does not pass stops
/// the rest, which would fail or build cold on its account. The exit: 0
/// when nothing failed and at least one oracle ran or answered,
/// `EXIT_UNPROVISIONED` with its sentinel when no oracle could run on this
/// host (a skip is not a pass), 1 on any failure.
pub(crate) fn run(root: &Path, eval: &str) -> i32 {
    println!(">> {GOAL}: {} step(s) on the host", STEPS.len());
    if std::env::var_os("TD_CHECK_FULL").is_some() {
        println!(">> {GOAL}: TD_CHECK_FULL is set — every oracle boots, memo or not");
    }
    let every_hit = STEPS
        .iter()
        .filter(|step| {
            !matches!(
                step.name,
                "warm" | VM_LIFECYCLE | POSTGRES | POSTGRES_PACKAGE
            )
        })
        .all(|step| matches!(ask_memo(root, eval, step.name), Memo::Hit(_)));
    let mut results: Vec<(&str, Outcome, Duration)> = Vec::new();
    for step in STEPS {
        let memo = match step.name {
            "warm" if every_hit => {
                println!(">> {GOAL}: warm skipped — every image oracle answers from the memo");
                continue;
            }
            "warm" | VM_LIFECYCLE | POSTGRES | POSTGRES_PACKAGE => Memo::Unasked,
            name => ask_memo(root, eval, name),
        };
        match &memo {
            Memo::Hit(key) => {
                println!(
                    "================ {GOAL} {}: PASS from the memo (key {}): passed here \
                     before with every input it boots unchanged; TD_CHECK_FULL=1 boots it \
                     again ================",
                    step.name,
                    key.get(..12).unwrap_or(key)
                );
                record(root, eval, step.name, Outcome::Memo, Duration::ZERO);
                results.push((step.name, Outcome::Memo, Duration::ZERO));
                continue;
            }
            Memo::Miss(key, why) => {
                println!(
                    "   [memo] {GOAL} {} boots (key {}): {why}",
                    step.name,
                    key.get(..12).unwrap_or(key)
                );
                tell_memo(root, eval, step.name, "--forget", key);
            }
            Memo::Unasked => {}
        }
        println!(
            "================ {GOAL} {}: {} ================",
            step.name, step.proves
        );
        let started = Instant::now();
        let outcome = run_step(root, eval, step);
        let took = started.elapsed();
        println!(
            "================ {GOAL} {}: {} ({:.1}s) ================",
            step.name,
            outcome.word().to_uppercase(),
            took.as_secs_f64()
        );
        record(root, eval, step.name, outcome, took);
        if let (Outcome::Pass, Memo::Miss(key, _)) = (outcome, &memo) {
            tell_memo(root, eval, step.name, "--record", key);
        }
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
            ">> {GOAL}: UNPROVISIONED — missing VM lifecycle tools or no system boot oracle could run \
             (qemu, /dev/kvm, or OVMF for the UEFI boots); a skip is not a pass"
        );
        // A caller reads a host gap by the code and the sentinel together.
        eprintln!("{}", td_engine::exit::UNPROVISIONED_SENTINEL);
    }
    code
}

fn run_step(root: &Path, eval: &str, step: &Step) -> Outcome {
    let mut cmd = if step.name == POSTGRES {
        match std::env::current_exe() {
            Ok(builder) => Command::new(builder),
            Err(error) => {
                eprintln!("{GOAL}: cannot locate td-builder: {error}");
                return Outcome::Fail;
            }
        }
    } else if step.name == VM_LIFECYCLE {
        let path = std::env::var("PATH").unwrap_or_default();
        if let Some(missing) = ["qemu-img", "qemu-io", "qemu-system-x86_64"]
            .into_iter()
            .find(|tool| crate::stage0::find_in_path(&path, tool).is_none())
        {
            println!(">> {GOAL} {}: unprovisioned (missing {missing})", step.name);
            return Outcome::HostGap;
        }
        Command::new("cargo")
    } else {
        Command::new(eval)
    };
    if step.name == VM_LIFECYCLE {
        // Cargo runs each selected harness under a short, request-backed /tmp.
        // The registrar harness is already inside its trusted-root runner.
        cmd.env(crate::test_root::ENV, "1")
            .env("TD_VM_GIT_TEST_INNER", "1");
    }
    if step.name == POSTGRES_PACKAGE {
        let mut warm = Command::new(eval);
        warm.args(["warm", POSTGRES_PACKAGE]);
        let warmed = run_command(root, POSTGRES_PACKAGE, warm);
        if warmed != Outcome::Pass {
            return warmed;
        }
        // This host stage records its own observed result, never a recipe memo.
        cmd.env("TD_CHECK_FULL", "1")
            .env_remove(td_engine::exit::CHECK_DEFER_ENV);
    }
    cmd.args(step.argv);
    run_command(root, step.name, cmd)
}

fn run_command(root: &Path, name: &str, mut cmd: Command) -> Outcome {
    cmd.current_dir(root);
    // An oracle runs for many minutes: it must not outlive `td-builder
    // stop` or a dead check host, holding the ladder lock.
    crate::host_bin::arm_check_child(&mut cmd);
    let status = cmd.status();
    match &status {
        Ok(st) => Outcome::of(st.code()),
        Err(e) => {
            eprintln!("{GOAL}: could not start {name}: {e}");
            Outcome::Fail
        }
    }
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
        ">> {GOAL}: of {total} oracle(s), {} passed ({} from the memo), {} unprovisioned, \
         {} failed, {} not run: {}",
        count(Outcome::Pass) + count(Outcome::Memo),
        count(Outcome::Memo),
        count(Outcome::HostGap),
        count(Outcome::Fail),
        total.saturating_sub(oracles.len()),
        timed.join(", ")
    );
    let failed = results.iter().any(|r| r.1 == Outcome::Fail);
    let code = if failed {
        1
    } else if oracles
        .iter()
        .filter(|r| !matches!(r.0, VM_LIFECYCLE | POSTGRES | POSTGRES_PACKAGE))
        .all(|r| r.1 == Outcome::HostGap)
        || results
            .iter()
            .any(|r| r.0 == VM_LIFECYCLE && r.1 == Outcome::HostGap)
    {
        td_engine::exit::EXIT_UNPROVISIONED
    } else if [POSTGRES, POSTGRES_PACKAGE]
        .iter()
        .all(|name| results.iter().any(|r| r.0 == *name && r.1 == Outcome::Pass))
    {
        0
    } else {
        1
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
        let manager_only = [
            (VM_LIFECYCLE, Outcome::Pass, s(1)),
            ("qemu-boot-system", gap, s(0)),
        ];
        assert_eq!(
            verdict(&manager_only).1,
            td_engine::exit::EXIT_UNPROVISIONED
        );
        let missing_manager_tools = [
            (VM_LIFECYCLE, gap, s(0)),
            ("qemu-boot-system", Outcome::Pass, s(1)),
        ];
        assert_eq!(
            verdict(&missing_manager_tools).1,
            td_engine::exit::EXIT_UNPROVISIONED
        );
        let one_ran = [
            (POSTGRES, Outcome::Pass, s(3)),
            (POSTGRES_PACKAGE, Outcome::Pass, s(2)),
            ("warm", Outcome::Pass, s(1)),
            ("qemu-boot-system", Outcome::Pass, s(600)),
            ("qemu-boot-live", gap, s(0)),
        ];
        let (line, code) = verdict(&one_ran);
        assert_eq!(code, 0);
        assert!(
            line.contains(
                "of 11 oracle(s), 3 passed (0 from the memo), 1 unprovisioned, 0 failed, 7 not run"
            ),
            "{line}"
        );
        assert!(line.contains("qemu-boot-system pass 600.0s"), "{line}");
        assert_ne!(verdict(&[(POSTGRES, Outcome::Pass, s(1))]).1, 0);
        assert_ne!(verdict(&[("qemu-boot-system", Outcome::Pass, s(1))]).1, 0);
        for missing in [POSTGRES, POSTGRES_PACKAGE] {
            let available: Vec<_> = one_ran.iter().copied().filter(|r| r.0 != missing).collect();
            assert_ne!(verdict(&available).1, 0);
        }
        let failed = [("warm", Outcome::Fail, s(1))];
        assert_eq!(verdict(&failed).1, 1);
        // A warm the host could not run stops the tier as a gap, not a pass.
        let warm_gap = [("warm", gap, s(0))];
        let (line, code) = verdict(&warm_gap);
        assert_eq!(code, td_engine::exit::EXIT_UNPROVISIONED);
        assert!(
            line.contains("0 passed (0 from the memo), 0 unprovisioned, 0 failed, 11 not run"),
            "{line}"
        );
        // Every oracle from the memo, warm skipped: a pass that booted nothing,
        // counted as passed and said to be the memo's.
        let all_memo = [
            (POSTGRES, Outcome::Pass, s(3)),
            (POSTGRES_PACKAGE, Outcome::Pass, s(2)),
            ("qemu-boot-system", Outcome::Memo, s(0)),
            ("qemu-boot-server", Outcome::Memo, s(0)),
            ("qemu-boot-audio", Outcome::Memo, s(0)),
            ("qemu-boot-audio-hotplug", Outcome::Memo, s(0)),
            ("qemu-deploy-rollback", Outcome::Memo, s(0)),
            ("qemu-admin-update", Outcome::Memo, s(0)),
            ("qemu-boot-live", Outcome::Memo, s(0)),
            ("qemu-install-system", Outcome::Memo, s(0)),
        ];
        let (line, code) = verdict(&all_memo);
        assert_eq!(code, 0);
        assert!(
            line.contains("10 passed (8 from the memo), 0 unprovisioned"),
            "{line}"
        );
        assert_eq!(Outcome::Memo.word(), "memo");
        assert_eq!(Outcome::of(Some(0)), Outcome::Pass);
        assert_eq!(
            Outcome::of(Some(td_engine::exit::EXIT_UNPROVISIONED)),
            Outcome::HostGap
        );
        assert_eq!(Outcome::of(None), Outcome::Fail);
    }

    #[test]
    fn only_a_well_formed_memo_answer_counts() {
        let key = "0123456789abcdef".repeat(4);
        assert_eq!(
            parse_memo(&format!("hit {key}")),
            Some(Memo::Hit(key.clone()))
        );
        assert_eq!(
            parse_memo(&format!("miss {key} recipe system-x86-64 changed")),
            Some(Memo::Miss(
                key.clone(),
                "recipe system-x86-64 changed".into()
            ))
        );
        assert_eq!(
            parse_memo(&format!("miss {key}")),
            Some(Memo::Miss(key.clone(), String::new()))
        );
        // A garbled reply boots: no key, a short or non-hex key, extra words
        // after a hit, an unknown verdict.
        for line in [
            "",
            "hit",
            "hit ",
            "hit bogus",
            &format!("hit {}", key.to_uppercase()),
            &format!("hit {key} extra"),
            &format!("pass {key}"),
        ] {
            assert_eq!(parse_memo(line), None, "{line:?}");
        }
    }

    #[test]
    fn the_deferred_note_names_the_oracles_and_the_command() {
        let note = deferred_note();
        for name in [
            POSTGRES,
            POSTGRES_PACKAGE,
            VM_LIFECYCLE,
            "qemu-boot-system",
            "qemu-boot-audio",
            "qemu-boot-audio-hotplug",
            "qemu-deploy-rollback",
            "qemu-boot-live",
            "qemu-install-system",
        ] {
            assert!(note.contains(name), "{note}");
        }
        assert!(note.contains("td-builder check integration"), "{note}");
        assert!(!note.contains("warm"), "{note}");
    }
}
