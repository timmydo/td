//! `run-capped` — the cargo `runner` that bounds td's test binaries.
//!
//! A test that allocates without bound used to take the machine down: on
//! 2026-08-23 a `td-sh` test binary reached 47.7 GiB three times in 49 minutes
//! and was killed by earlyoom. td already had the primitive to stop that
//! (`sys::set_rlimit`, applied by `sandbox::cap_child_data_rlimit`) but wired
//! it to GATE BODIES only, so a bare `cargo test` in a worktree — which is how
//! both runaways ran — had no ceiling at all.
//!
//! `.cargo/config.toml` points cargo's `target.<triple>.runner` here, which
//! reaches invocations td does not launch. We set `RLIMIT_DATA` on ourselves
//! and `exec` the target: rlimits survive `execve`, so the ceiling binds the
//! test binary and everything it forks, and `exec` leaves no wrapper in the
//! tree to get the exit status wrong.
//!
//! What this bounds is narrower than "memory", deliberately. `RLIMIT_DATA`
//! makes covered `brk`/`mmap` requests fail with `ENOMEM`; the kernel does not
//! signal the process. Ordinary infallible Rust heap growth then reaches
//! `handle_alloc_error`, which currently aborts — toolchain behaviour, not a
//! guarantee, and `try_reserve` returns `Err` instead. It is also not an RSS
//! ceiling: private writable file mappings count toward it and shared mappings
//! escape it. It bounds the failure that actually happened — unbounded
//! infallible heap growth — and is a defence, not a total guarantee.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

use crate::sys;

const MIB: u64 = 1024 * 1024;

/// Default per-process ceiling. Every td crate but the control plane is
/// pure-`std` logic over bounded inputs; `td-sh`'s conformance suite already
/// proves 256 MiB suffices for its streaming shapes.
const DEFAULT_MIB: u64 = 1024;

/// The control plane does NAR and store work over real trees and needs more.
/// Kept as a three-name exception rather than a full per-crate roster: the
/// default is the TIGHTER direction, so a crate added later is capped, not
/// exempted, and the list stays auditable at a glance.
const CONTROL_PLANE_MIB: u64 = 2048;
const CONTROL_PLANE: [&str; 3] = ["td-builder", "td-recipe", "td-engine"];

/// Raise or lower the ceiling for a deliberate one-off. It may not REMOVE one:
/// a safety that can be switched off is one that eventually is.
const OVERRIDE_ENV: &str = "TD_RUN_CAPPED_MIB";

/// True when `path` names a cargo TEST artifact.
///
/// Cargo gives every test and bench binary a `-C metadata` suffix (`-` plus 16
/// lowercase hex) and gives a directly-run binary none:
///
/// ```text
/// cargo test              <target>/debug/deps/td_sh-6c723055c1a2b3d4   capped
/// cargo test --example x  <target>/debug/examples/x-7de23f2819f45846   capped
/// cargo run               <target>/debug/td-sh                         not
/// cargo run --example x   <target>/debug/examples/x                    not
/// ```
///
/// That distinction is load-bearing, not cosmetic. td's single full check is
/// `cargo run --release --manifest-path builder/Cargo.toml -- check`, and
/// `cap_child_data_rlimit` clamps each gate's request to the ambient hard
/// limit — so capping `cargo run` would silently reduce every 4 GiB compiler
/// gate body to this ceiling. Reading the file NAME rather than the directory
/// also survives `--target-dir`, `CARGO_TARGET_DIR` (gate 325 puts every crate
/// under one shared scratch dir), and a profile that happens to be named
/// `deps`.
///
/// The suffix is a convention cargo follows, not a namespace it reserves: a
/// target named `tool-0123456789abcdef` would match. No target in the tree
/// does, and `no_runnable_target_name_looks_like_a_test_artifact` keeps that
/// true for every crate at the top level (it does not descend to nested
/// fixture crates such as `tests/sshd`).
pub fn is_test_artifact(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let Some((stem, hash)) = name.rsplit_once('-') else {
        return false;
    };
    !stem.is_empty()
        && hash.len() == 16
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The ceiling this invocation should apply, in MiB.
///
/// Selection is by `CARGO_PKG_NAME`, which cargo sets on the runner process,
/// rather than by the artifact path — under gate 325's shared
/// `CARGO_TARGET_DIR` the path holds no crate directory at all. An unset name
/// takes the default: a missing name caps rather than skips.
pub fn ceiling_mib(package: Option<&str>, override_value: Option<&str>) -> Result<u64, String> {
    // An EMPTY value reads as unset, not as an error: a caller writing
    // `TD_RUN_CAPPED_MIB="${SOMETHING:-}"` would otherwise brick every test
    // binary in the tree with a parse failure.
    if let Some(raw) = override_value.filter(|v| !v.trim().is_empty()) {
        let text = raw.trim();
        let mib: u64 = text
            .parse()
            .map_err(|_| format!("{OVERRIDE_ENV}={text:?} is not a number of MiB"))?;
        if mib == 0 {
            return Err(format!(
                "{OVERRIDE_ENV}=0 would remove the ceiling, which is not allowed; \
                 set a positive MiB value to raise or lower it"
            ));
        }
        if mib.checked_mul(MIB).is_none() {
            return Err(format!("{OVERRIDE_ENV}={text} overflows a byte count"));
        }
        return Ok(mib);
    }
    Ok(match package {
        Some(name) if CONTROL_PLANE.contains(&name) => CONTROL_PLANE_MIB,
        _ => DEFAULT_MIB,
    })
}

/// The (soft, hard) pair to set, given what we want and what we inherited.
///
/// Setting `(want, want)` blindly is wrong in both directions: it is `EPERM`
/// when the inherited hard limit is lower, and it RAISES an inherited soft
/// limit that was deliberately tighter. Taking minima never loosens either.
pub fn effective_limits(want_bytes: u64, ambient: (u64, u64)) -> (u64, u64) {
    let (soft0, hard0) = ambient;
    let hard = want_bytes.min(hard0);
    let soft = want_bytes.min(soft0).min(hard);
    (soft, hard)
}

pub fn main(args: &[String]) -> ExitCode {
    let Some((program, rest)) = args.split_first() else {
        eprintln!("td-builder run-capped: usage: run-capped <binary> [args...]");
        return ExitCode::FAILURE;
    };

    if is_test_artifact(program) {
        let package = std::env::var("CARGO_PKG_NAME").ok();
        let override_value = std::env::var(OVERRIDE_ENV).ok();
        let mib = match ceiling_mib(package.as_deref(), override_value.as_deref()) {
            Ok(mib) => mib,
            Err(e) => {
                eprintln!("td-builder run-capped: {e}");
                return ExitCode::FAILURE;
            }
        };
        // Fail closed: a ceiling we cannot read or set is not a reason to run
        // the test uncapped, which is the state this whole mechanism exists to
        // leave behind.
        let ambient = match sys::get_rlimit(sys::RLIMIT_DATA) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("td-builder run-capped: read RLIMIT_DATA: {e}");
                return ExitCode::FAILURE;
            }
        };
        let (soft, hard) = effective_limits(mib.saturating_mul(MIB), ambient);
        // Both numbers: the kernel rejects the PAIR, so an EPERM is only
        // legible next to the hard limit that refused it.
        if let Err(e) = sys::set_rlimit(sys::RLIMIT_DATA, soft, hard) {
            eprintln!("td-builder run-capped: set RLIMIT_DATA to ({soft}, {hard}) bytes: {e}");
            return ExitCode::FAILURE;
        }
        // An exported environment variable is not visible in the command line
        // that used it, so an override says so where the reader is looking.
        if override_value.is_some() {
            eprintln!("td-builder run-capped: {OVERRIDE_ENV} in effect — ceiling {mib} MiB");
        }
    }

    // exec, not spawn: no wrapper left in the process tree, and the exit status
    // is the test binary's own rather than something we forward and get wrong.
    let e = Command::new(program).args(rest).exec();
    eprintln!("td-builder run-capped: exec {program}: {e}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_artifacts_are_capped_and_run_binaries_are_not() {
        // Capped: a `-C metadata` suffix, wherever cargo put it.
        for path in [
            "td-sh/target/debug/deps/td_sh-6c723055c1a2b3d4",
            "target/debug/deps/td_builder-0123456789abcdef",
            ".cargo-test-scratch/target/debug/deps/td_busd-fedcba9876543210",
            "/abs/target/debug/examples/demo-7de23f2819f45846",
        ] {
            assert!(is_test_artifact(path), "{path} is a test artifact");
        }
        // Not capped: no suffix. The `cargo run` shapes — including the
        // canonical full check, whose gates need their own larger allowance.
        for path in [
            "target/release/td-builder",
            "td-sh/target/debug/td-sh",
            "target/debug/examples/demo",
            "target/deps/tool",
        ] {
            assert!(!is_test_artifact(path), "{path} is not a test artifact");
        }
    }

    #[test]
    fn a_hash_shaped_name_needs_the_exact_shape() {
        // Too short, too long, uppercase, non-hex, and a bare suffix.
        for name in [
            "x-0123456789abcde",
            "x-0123456789abcdef0",
            "x-0123456789ABCDEF",
            "x-0123456789abcdeg",
            "-0123456789abcdef",
        ] {
            assert!(!is_test_artifact(name), "{name} must not look like a test");
        }
    }

    #[test]
    fn the_control_plane_gets_more_and_everything_else_takes_the_default() {
        for name in CONTROL_PLANE {
            assert_eq!(ceiling_mib(Some(name), None), Ok(CONTROL_PLANE_MIB));
        }
        assert_eq!(ceiling_mib(Some("td-sh"), None), Ok(DEFAULT_MIB));
        assert_eq!(ceiling_mib(Some("td-busd"), None), Ok(DEFAULT_MIB));
        // A crate nobody has written yet, and a missing name: both CAP.
        assert_eq!(ceiling_mib(Some("td-not-yet"), None), Ok(DEFAULT_MIB));
        assert_eq!(ceiling_mib(None, None), Ok(DEFAULT_MIB));
    }

    #[test]
    fn the_override_may_move_the_ceiling_but_not_remove_it() {
        assert_eq!(ceiling_mib(Some("td-sh"), Some("4096")), Ok(4096));
        assert_eq!(ceiling_mib(Some("td-sh"), Some(" 64 ")), Ok(64));
        // Zero is the one value that would leave a binary uncapped.
        assert!(
            matches!(ceiling_mib(Some("td-sh"), Some("0")), Err(e) if e.contains("remove the ceiling")),
            "0 must be refused for removing the ceiling"
        );
        for bad in ["lots", "-1", "12MiB"] {
            assert!(
                ceiling_mib(Some("td-sh"), Some(bad)).is_err(),
                "{bad:?} must not parse"
            );
        }
        // An EMPTY value reads as unset, so `TD_RUN_CAPPED_MIB="${X:-}"` takes
        // the crate's ceiling rather than bricking every test binary.
        for empty in ["", "   "] {
            assert_eq!(
                ceiling_mib(Some("td-sh"), Some(empty)),
                Ok(DEFAULT_MIB),
                "{empty:?} must read as unset"
            );
        }
        assert!(
            matches!(
                ceiling_mib(Some("td-sh"), Some("18446744073709551615")),
                Err(e) if e.contains("overflows")
            ),
            "a MiB value that overflows a byte count must be refused"
        );
    }

    /// This process's HARD `RLIMIT_DATA`, from `/proc/self/limits` — a FILE
    /// read, so it needs no syscall and no `unsafe`, and the same helper would
    /// compile in the crates that `#![forbid(unsafe_code)]` if this is ever
    /// rolled out per-crate.
    ///
    /// The HARD limit specifically, not the soft one. `RLIMIT_DATA` is
    /// per-PROCESS and the harness runs every `#[test]` in one process, so
    /// `sys::tests::set_rlimit_data_round_trips` transiently moves the SOFT
    /// limit out from under a concurrent reader. It never moves the hard limit
    /// — an unprivileged process cannot raise one — so reading hard is the
    /// race-free way to ask what ceiling this process is actually under.
    fn own_hard_data_ceiling() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/limits").ok()?;
        let row = text.lines().find(|l| l.starts_with("Max data size"))?;
        // "Max data size   <soft>   <hard>   bytes"
        let hard = row
            .get("Max data size".len()..)?
            .split_whitespace()
            .nth(1)?;
        if hard == "unlimited" {
            return None;
        }
        hard.parse().ok()
    }

    /// Everything else here proves the runner WORKS. This proves it was USED —
    /// a ceiling nobody can confirm is applied is one that quietly stops
    /// existing, which is the state that let a test binary reach 47.7 GiB.
    ///
    /// It asserts the ceiling is finite AND no larger than what this crate
    /// should have been given. Finite alone is not enough: a binary run
    /// directly under some inherited 64 GiB limit is finite and would still
    /// recreate the incident, and `affected.rs` routes `.cargo/config.toml`
    /// edits here, so a loosened runner must not pass.
    ///
    /// One attestation, here, rather than one per crate: this reds whenever the
    /// mechanism is broken globally — `.cargo/config.toml` missing or shadowed,
    /// the runner replaced through `CARGO_TARGET_*_RUNNER`, or a `--target`
    /// triple with no runner entry — which is the realistic failure. A bypass
    /// confined to one other crate is not covered; see the design note.
    #[test]
    fn this_test_binary_runs_under_a_ceiling() {
        // Absent means unlimited, which fails the same assertion — u64::MAX is
        // above any ceiling — so one check covers "no runner ran at all" and
        // "a runner ran but left us too much room".
        let hard = own_hard_data_ceiling();
        let seen = hard.map_or_else(|| "unlimited".to_string(), |b| format!("{b} bytes"));
        // What THIS crate should have been handed, honouring an override so a
        // deliberately raised ceiling does not red the run that asked for it.
        let want = ceiling_mib(
            Some(env!("CARGO_PKG_NAME")),
            std::env::var(OVERRIDE_ENV).ok().as_deref(),
        )
        .unwrap_or(CONTROL_PLANE_MIB)
        .saturating_mul(MIB);
        assert!(
            hard.unwrap_or(u64::MAX) <= want,
            "this test binary's RLIMIT_DATA hard limit is {seen}, above the {want} \
             bytes it should have been capped to. If it is unlimited the cargo \
             runner did not apply at all: check that .cargo/config.toml is present \
             and not shadowed, that target/release/td-builder is built, and that \
             CARGO_TARGET_*_RUNNER is unset \
             (cargo build --release --manifest-path builder/Cargo.toml)."
        );
    }

    /// The classifier reads a cargo CONVENTION, not a namespace cargo reserves:
    /// a target legitimately named `tool-0123456789abcdef` would match, and
    /// `cargo run`ning it would hand it the TEST ceiling — silently shrinking
    /// the full check's gate allowances, the exact failure the classifier
    /// exists to prevent. Nothing in the tree collides; this keeps it that way.
    #[test]
    fn no_runnable_target_name_looks_like_a_test_artifact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        // Two counters, not one. Manifest `name =` lines outnumber target-dir
        // stems roughly seven to one, so a single total stays comfortably above
        // any threshold while the stem half — the one covering the
        // `cargo test --example` shape — silently stops finding anything.
        let mut manifest_names = 0usize;
        let mut target_stems = 0usize;
        let entries = std::fs::read_dir(&root);
        assert!(entries.is_ok(), "cannot walk {} to sweep target names", root.display());
        let Ok(entries) = entries else { return };
        for crate_dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            // Explicit [[bin]]/[[example]] names, and the auto-discovered
            // targets cargo takes from these two directories.
            if let Ok(manifest) = std::fs::read_to_string(crate_dir.join("Cargo.toml")) {
                for name in manifest.lines().filter_map(|l| {
                    l.trim()
                        .strip_prefix("name")?
                        .trim_start()
                        .strip_prefix('=')?
                        .trim()
                        .trim_matches('"')
                        .into()
                }) {
                    manifest_names = manifest_names.saturating_add(1);
                    assert!(
                        !is_test_artifact(name),
                        "{}: target {name:?} ends in a cargo metadata hash, so \
                         `cargo run` would cap it like a test binary",
                        crate_dir.display()
                    );
                }
            }
            for sub in ["src/bin", "examples"] {
                let Ok(files) = std::fs::read_dir(crate_dir.join(sub)) else {
                    continue;
                };
                for stem in files
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                    .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
                {
                    target_stems = target_stems.saturating_add(1);
                    assert!(
                        !is_test_artifact(&stem),
                        "{}/{sub}/{stem}.rs ends in a cargo metadata hash, so \
                         `cargo run --example`/`--bin` would cap it like a test",
                        crate_dir.display()
                    );
                }
            }
        }
        // The sweep is the point; a walk that silently found nothing would pass
        // while guarding nothing. Each half is asserted separately.
        assert!(
            manifest_names > 20,
            "only {manifest_names} manifest target names swept — the walk broke"
        );
        assert!(
            target_stems > 0,
            "no src/bin or examples stems swept — the half that covers \
             `cargo test --example` guarded nothing"
        );
    }

    #[test]
    fn the_effective_limit_never_loosens_what_it_inherited() {
        const G: u64 = 1024 * MIB;
        // The ordinary case: nothing inherited, we get what we asked for.
        assert_eq!(effective_limits(G, (u64::MAX, u64::MAX)), (G, G));
        // A lower inherited HARD limit clamps us — raising it would be EPERM.
        assert_eq!(effective_limits(G, (u64::MAX, G / 2)), (G / 2, G / 2));
        // A lower inherited SOFT limit is someone's deliberate tightening and
        // survives; we must not raise it just because the hard limit allows.
        assert_eq!(effective_limits(G, (G / 4, u64::MAX)), (G / 4, G));
        // Asking for less than either keeps our own tighter number.
        assert_eq!(effective_limits(G / 8, (G, G)), (G / 8, G / 8));
    }
}
