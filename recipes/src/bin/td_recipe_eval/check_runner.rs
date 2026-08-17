use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use td_recipe::{
    catalog, source_pins,
    types::{CheckRunner, Recipe, SourcePin},
};

pub(crate) const TD_STORE_DIR: &str = "/td/store";

/// Opt-in warm-run profiling (TD_TIMING=1): time a harness phase and print
/// `[timing] <label> <ms>ms` on drop. Diagnostic only; the timer is a no-op
/// unless TD_TIMING is set to a non-empty, non-"0" value.
fn timing_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| matches!(env::var("TD_TIMING"), Ok(v) if v != "0" && !v.is_empty()))
}

struct HarnessTimer {
    label: &'static str,
    start: std::time::Instant,
}

impl Drop for HarnessTimer {
    fn drop(&mut self) {
        if timing_on() {
            eprintln!("[timing] {} {}ms", self.label, self.start.elapsed().as_millis());
        }
    }
}

fn timed_phase(label: &'static str) -> HarnessTimer {
    HarnessTimer { label, start: std::time::Instant::now() }
}

/// The stable in-crate marker for a planning-time provenance rejection.
/// `main` maps an error carrying it to `EXIT_PROVENANCE_REJECTED` (78): the
/// graph is structurally unbuildable on EVERY host until the rejected input
/// exists as a td recipe output. It shared 69 with the HOST gap until the
/// codes were split, which made the two indistinguishable to a caller holding
/// only the number — and they must be treated as opposites, since only the
/// host gap is a tolerable skip. The cross-process contract is the exit code;
/// this prose never crosses a process boundary as an interface.
pub(crate) const PROVENANCE_REJECTED: &str = "provenance rejected: ";

/// The stable in-crate marker for a HOST gap: a child reported that nothing on
/// THIS machine can do the work. `main` maps an error carrying it to
/// `EXIT_UNPROVISIONED` + the sentinel, which gate-run tolerates as a skip —
/// the opposite of [`PROVENANCE_REJECTED`], which no host can fix and which
/// must never read as one. Like that marker, the prose is in-crate only; the
/// cross-process contract is the exit code and the sentinel. It must reach
/// `die_runner` UNWRAPPED — a caller that adds context in front of it turns the
/// skip back into a plain exit 2.
pub(crate) const HOST_GAP: &str = "host gap: ";

/// A graph input with no admissible provenance (issue #469): not a recipe
/// output, not a pinned seed source. Names the recipe and the input so the
/// gap is actionable — the fix is always "build it as a rung", never "point
/// at a host path".
fn provenance_rejection(stem: &str, input: &str) -> String {
    format!(
        "{PROVENANCE_REJECTED}recipe {stem}: input `{input}' is neither a td recipe \
         output (catalog) nor a pinned seed source/patch. Host executables are not \
         admissible bootstrap inputs (re #469) — the chain must build `{input}' as a \
         recipe output before anything can declare it."
    )
}

pub fn cli(args: &[String]) -> Result<(), String> {
    let stem = args.first().ok_or_else(usage)?.as_str();
    let index = parse_index(args.get(1))?;
    if args.get(2).is_some() {
        return Err(usage());
    }
    let check_runner = selected_check_runner(stem, index)?;
    // Provenance planning FIRST — before the runner exists, so a rejected
    // graph spawns no subprocess at all (re #469).
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("check", &[stem, &index.to_string()]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    let _lock = lock_ladder_for_run(&runner)?;
    crate::checks::run(check_runner, &runner, stem)
}

/// `td-recipe-eval clear-store` — the EXPLICIT cold reset, and the ONLY path that destroys
/// persisted build state now that `setup()` never wipes. It clears BOTH machine-wide warm
/// caches so the next build genuinely cold-climbs from the compiled pins:
///  1. the ladder work dir (seed store/db, the shared build-cache, every per-invocation
///     scratch) — held under the ladder lock so it can never race a live build or boot;
///  2. the signed substitute store (`~/.td/subst`) — otherwise the next toolchain build would
///     FETCH the prior published closure instead of rebuilding from seed, and `clear-store`
///     would not actually force a cold build (the substitute is optimization-only, so a cold
///     machine simply has none). Resolved the same way the loop exposes it.
/// Resolves the SAME ladder tree `new()` builds into: the one shared ladder under
/// `~/.td/build-daemon` (HOME-derived).
/// `verify-store`: run the persistent build cache's integrity fsck (`td-builder store-verify`)
/// against THIS ladder's build cache — the on-disk check the warm reuse path skips. Opt-in and
/// separate from `run`, so a build/boot never pays for it; run it explicitly to detect rot.
pub fn verify_store_cli(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: verify-store".to_string());
    }
    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let runner = RecipeCheckRunner::new(root, &scratch_name("verify-store", &[]))?;
    // Hold the ladder lock across the fsck so it sees a stable cache: a concurrent build
    // commit or a `clear-store` would otherwise race the re-hash (spurious "corruption") or
    // swap the whole cache out from under it. Same lock `run`/`clear-store` take.
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.verify_store()
}

pub fn clear_store_cli(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: clear-store".to_string());
    }
    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let home = env::var_os("HOME").map(PathBuf::from);
    clear_ladder(&ladder_work_dir(&root, home.as_deref()))?;
    if let Some(subst) = subst_store_dir(home.as_deref()) {
        clear_subst_store(&subst)?;
    }
    Ok(())
}

/// The machine-wide signed substitute store `clear-store` also resets: an explicit
/// `TD_SUBST_STORE` wins, else the HOME-derived `~/.td/subst` — the exact resolution the loop
/// (`check_loop::subst_env`) uses to EXPOSE it, so clearing hits precisely what a later build
/// would fetch from. None when neither is available (no store to clear).
fn subst_store_dir(home: Option<&Path>) -> Option<PathBuf> {
    match env::var("TD_SUBST_STORE") {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        _ => home.map(|h| h.join(".td/subst")),
    }
}

/// Reset the substitute store under the same too-shallow/`..` guard as the ladder. No
/// commit-lock dance: nothing here holds an open fd we must swap aside, so it is a plain
/// rename-aside-then-reap — a concurrent reader (another worktree's fetch) keeps its open
/// inode and degrades to a from-seed miss rather than seeing a half-deleted tree. Factored
/// from `clear_store_cli` so the fs-level test drives it against a throwaway dir.
fn clear_subst_store(store: &Path) -> Result<(), String> {
    reject_unsafe_clear_target(store)?;
    if !store.exists() {
        println!(
            "clear-store: substitute store {} was already absent",
            store.display()
        );
        return Ok(());
    }
    let tomb = clearing_tombstone_path(store);
    remove_path_if_exists(&tomb)?;
    fs::rename(store, &tomb)
        .map_err(|e| format!("clear-store: swap {} aside: {e}", store.display()))?;
    remove_path_if_exists(&tomb)?;
    println!("clear-store: reset substitute store {}", store.display());
    Ok(())
}

/// Reset one ladder work dir under its lock. Factored from `clear_store_cli` so the fs-level
/// test drives it against a throwaway tree without mutating process-global env.
fn clear_ladder(lw: &Path) -> Result<(), String> {
    // Refuse an obviously-unsafe target: `remove_dir_all` is recursive, so a `.`, `/`, `$HOME`,
    // or a too-shallow path would delete far more than a ladder. The path is always the computed
    // shared ladder now; the guard stays as defense-in-depth for the recursive delete.
    reject_unsafe_clear_target(lw)?;
    // The ladder lock lives BESIDE lw (`<lw>.lock`), so removing lw leaves it — and its inode —
    // intact; hold it across the whole reset so no concurrent build/boot runs inside meanwhile.
    let lock_path = ladder_lock_path(lw);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _lock = lock_ladder(&lock_path, LadderLock::Exclusive)?;
    // A prior clear that crashed between the swap-aside and the reap would leave this sibling
    // tombstone; remove it first (idempotent) so it cannot accrete. Race-free under the ladder
    // lock, which serializes clears, so a fixed name needs no pid tag.
    let tomb = clearing_tombstone_path(lw);
    remove_path_if_exists(&tomb)?;
    if lw.exists() {
        // Swap lw aside atomically, THEN delete — never `remove_dir_all` the directory that holds
        // our own open commit-lock fd. That is the invariant eviction keeps by siting its lock
        // BESIDE the deleted subtree: an open-fd unlink NFS-silly-renames (rmdir then fails
        // ENOTEMPTY), and unlinking a still-live lock pathname lets a fresh committer recreate +
        // lock a NEW inode at the same path while a waiter holds the old one. The commit lock is
        // held only across the instant rename, excluding an orphaned builder child mid-commit
        // (the ladder lock does not cover a direct store-commit); once lw is renamed its pathname
        // is gone, and no committer can recreate `<lw>/build-cache.commit.lock` until a fresh
        // build — which must first take the ladder lock we still hold — recreates lw.
        {
            // BOTH commit locks: the seed store has one of its own now, so a clear that
            // took only the cache's could rename the ladder out from under an intern
            // mid-MERGE. No deadlock to reason about — nothing else takes both, and two
            // clears cannot overlap, each holding the ladder exclusively — so the order
            // here is only for the reader.
            //
            // This does NOT cover an orphaned `store-add-recursive` for its whole run —
            // it holds the seed lock only across the cold commit and the db merge, and
            // the NAR walks between them are deliberately unlocked (see main.rs). A
            // clear landing in that window renames the tree the orphan is reading; the
            // orphan then reds or writes into the tombstone, which is discarded either
            // way. Nothing SURVIVING is corrupted, which is the property this dance is
            // for; excluding the orphan outright would need a lease held across the
            // walks, which is the serialization that made concurrency pointless.
            let _cache_lock = lock_file(&lw.join(CACHE_COMMIT_LOCK_BASENAME))?;
            let _seed_lock = lock_file(&lw.join(SEED_COMMIT_LOCK_BASENAME))?;
            fs::rename(lw, &tomb)
                .map_err(|e| format!("clear-store: swap {} aside: {e}", lw.display()))?;
        }
        remove_path_if_exists(&tomb)?;
        println!("clear-store: reset ladder work dir {}", lw.display());
    } else {
        println!("clear-store: ladder work dir {} was already absent", lw.display());
    }
    Ok(())
}

/// The sibling tombstone `<lw>.clearing` that `clear_ladder` swaps lw onto before deleting it,
/// so the recursive remove never runs against the tree holding its own open lock fd.
fn clearing_tombstone_path(lw: &Path) -> PathBuf {
    let mut s = lw.as_os_str().to_os_string();
    s.push(".clearing");
    PathBuf::from(s)
}

/// The commit-lock basename inside a ladder work dir, shared with the builder's commit
/// transaction (`lock_store_commit`). ONE const so `clear_ladder` (free fn), the runner's
/// `cache_commit_lock_path`, and eviction can never take DIFFERENT locks — a divergence would
/// break the "clear/evict never races a direct committer" invariant with no compile error.
const CACHE_COMMIT_LOCK_BASENAME: &str = "build-cache.commit.lock";

/// The seed store's commit lock, the sibling `lock_store_commit` derives from the
/// seed db's parent (`<lw>/seed-db`) — kept beside that dir for the same reason the
/// cache's is kept beside `build-cache/`.
const SEED_COMMIT_LOCK_BASENAME: &str = "seed-db.commit.lock";

/// Fail closed on a `clear-store` target that would recursively delete more than a ladder.
/// A ladder work dir is always an absolute path at least THREE plain segments deep
/// (`<root>/.td-build-cache/ladder-shared-v1`, `<home>/.td/build-daemon/ladder-shared-v1`); `/`, `/x`,
/// and a bare `$HOME` like `/home/user` (depth two) are rejected, as is any `.`/`..` component
/// that could normalize the delete up out of the ladder.
fn reject_unsafe_clear_target(lw: &Path) -> Result<(), String> {
    if !lw.is_absolute() {
        return Err(format!(
            "clear-store: refusing to clear a non-absolute path {}",
            lw.display()
        ));
    }
    // Every segment after the root must be a plain name — a `.`/`..` component could traverse the
    // recursive delete out of the ladder (e.g. `/a/b/../../..`).
    let mut depth = 0usize;
    for comp in lw.components() {
        match comp {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(_) => depth += 1,
            _ => {
                return Err(format!(
                    "clear-store: refusing to clear {} — it has a `.` or `..` component; pass a \
                     plain absolute ladder path",
                    lw.display()
                ));
            }
        }
    }
    if depth < 3 {
        return Err(format!(
            "clear-store: refusing to clear the too-shallow path {} — a ladder work dir is at \
             least three components deep (a bare $HOME or repo root is not a ladder)",
            lw.display()
        ));
    }
    Ok(())
}

/// Host-side qemu boot validation (re #529). This is deliberately NOT a gated
/// recipe check: booting the kernel requires HOST qemu, and the gate wraps
/// every recipe check in a host-free `pivot_root` sandbox that exposes only
/// td-built tools by absolute /td/store path — so host qemu is unreachable there
/// (unlike the RustToolchain check, which runs the td-BUILT rustc). Registering it
/// as a sandboxed check would therefore fail on `find_qemu` on every real runner. So
/// the boot is an explicit host-side command an operator or developer runs OUTSIDE
/// the sandbox: it builds linux-x86-64 (bzImage + initramfs) and boots it under
/// host qemu, asserting the userland marker reaches ttyS0.
pub fn qemu_boot_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "linux-x86-64";
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if stem != STEM {
        return Err(format!(
            "qemu-boot only supports {STEM} (got '{stem}'); usage: qemu-boot [{STEM}]"
        ));
    }
    if args.get(1).is_some() {
        return Err(format!("usage: qemu-boot [{STEM}]"));
    }
    // Provenance planning FIRST — before the runner exists, so a rejected graph
    // spawns no subprocess at all (re #469), matching `cli`/`build_cli`.
    let targets = [stem];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    warm_operator_inputs(&runner, &targets);
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    crate::checks::qemu_boot::run(&runner)
}

/// `td-recipe-eval qemu-boot-erofs [linux-x86-64]` — the read-only-root boot proof
/// (re #549). Same host-side boot as `qemu-boot`, but it also builds a probe erofs
/// image with the control-plane `mkfs-erofs` writer (#548) and attaches it as a
/// read-only virtio-blk disk; the guest /init mounts it read-only and the tool
/// asserts the erofs marker. Host-side (never a gated check) for the same reason
/// `qemu-boot` is — the gate sandbox has no host qemu. See checks/qemu_boot.rs.
pub fn qemu_boot_erofs_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "linux-x86-64";
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if stem != STEM {
        return Err(format!(
            "qemu-boot-erofs only supports {STEM} (got '{stem}'); usage: qemu-boot-erofs [{STEM}]"
        ));
    }
    if args.get(1).is_some() {
        return Err(format!("usage: qemu-boot-erofs [{STEM}]"));
    }
    // Provenance planning FIRST — before the runner exists (re #469), matching
    // `qemu_boot_cli`: a rejected graph spawns no subprocess.
    let targets = [stem];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans
    // a killed erofs boot's per-boot directories.
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    warm_operator_inputs(&runner, &targets);
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    crate::checks::qemu_boot::run_erofs(&runner)
}

/// `td-recipe-eval qemu-boot-system [system-x86-64]` — the persistent deployment
/// boot proof. It builds the system and target Btrfs tools, creates one volume,
/// and boots it twice through selector, verified kexec, loop-mounted EROFS, and
/// persistent @var. Boot two must read boot one's synced marker; both prove the
/// immutable root, target-owned state, clean shutdown, and offline Btrfs checks.
/// This is host-side because the gate sandbox has no host qemu.
pub fn qemu_boot_system_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "system-x86-64";
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if stem != STEM {
        return Err(format!(
            "qemu-boot-system only supports {STEM} (got '{stem}'); usage: qemu-boot-system [{STEM}]"
        ));
    }
    if args.get(1).is_some() {
        return Err(format!("usage: qemu-boot-system [{STEM}]"));
    }
    // Provenance planning FIRST — before the runner exists (re #469), matching
    // `qemu_boot_cli`: a rejected graph spawns no subprocess.
    let targets = [stem, "btrfs-progs-x86-64"];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans
    // a killed system boot's per-boot directories (it can hold a multi-GiB kernel build).
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    warm_operator_inputs(&runner, &targets);
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    crate::checks::qemu_boot::run_system(&runner)
}

/// `td-recipe-eval qemu-boot-net [system-x86-64]` — the networking proof. It
/// creates the same persistent volume and selector/kexec boot as the system
/// oracle, adds a user-mode NIC, and asserts td-netd's DHCP, DNS, and TCP markers,
/// clean shutdown, and an offline Btrfs check. This additionally needs the
/// operator host's outbound DNS/TCP for SLIRP.
pub fn qemu_boot_net_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "system-x86-64";
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if stem != STEM {
        return Err(format!(
            "qemu-boot-net only supports {STEM} (got '{stem}'); usage: qemu-boot-net [{STEM}]"
        ));
    }
    if args.get(1).is_some() {
        return Err(format!("usage: qemu-boot-net [{STEM}]"));
    }
    // Provenance planning FIRST — before the runner exists (re #469), matching
    // `qemu_boot_cli`: a rejected graph spawns no subprocess.
    let targets = [stem, "btrfs-progs-x86-64"];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans
    // a killed net boot's per-boot directories (it can hold a multi-GiB kernel build).
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    warm_operator_inputs(&runner, &targets);
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    crate::checks::qemu_boot::run_net(&runner)
}

/// `td-recipe-eval qemu-boot-kexec [kexec-spike-x86-64]` — the Phase-0 kexec spike proof.
/// Builds the `kexec-spike-x86-64` two-kernel artifact (a bootable bzImage + an outer
/// initramfs embedding static busybox, td-kexec, a second-boot bzImage, and a nested inner
/// initramfs) and boots it under host qemu (TCG): the outer /init prints STAGE1 then execs
/// td-kexec to kexec_file_load(2)+reboot(KEXEC) the inner kernel, whose /init prints STAGE2.
/// It asserts STAGE2 reached (the kexec worked). Host-side (never a gated check) for the
/// same reason `qemu-boot` is: the gate sandbox has no host qemu. See checks/qemu_boot.rs.
pub fn qemu_boot_kexec_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "kexec-spike-x86-64";
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if stem != STEM {
        return Err(format!(
            "qemu-boot-kexec only supports {STEM} (got '{stem}'); usage: qemu-boot-kexec [{STEM}]"
        ));
    }
    if args.get(1).is_some() {
        return Err(format!("usage: qemu-boot-kexec [{STEM}]"));
    }
    // Provenance planning FIRST — before the runner exists (re #469), matching
    // `qemu_boot_cli`: a rejected graph spawns no subprocess.
    let targets = [stem];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans a
    // killed kexec boot's per-boot directories (it can hold a multi-GiB kernel build).
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    warm_operator_inputs(&runner, &targets);
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    crate::checks::qemu_boot::run_kexec(&runner)
}

/// `td-recipe-eval run [system-x86-64]` — the interactive distro runner (re #541).
/// Builds and verifies the complete `system-x86-64` deployment bundle, then
/// boots it under host qemu with an interactive serial console. Like `qemu-boot`,
/// this is a host-side command run OUTSIDE the gate sandbox (which has no host
/// qemu and no terminal), never a gated check. See checks/run.rs.
pub fn run_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "system-x86-64";
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if stem != STEM {
        return Err(format!(
            "run only supports {STEM} (got '{stem}'); usage: run [{STEM}]"
        ));
    }
    if args.get(1).is_some() {
        return Err(format!("usage: run [{STEM}]"));
    }
    // `run` is INTERACTIVE: it hands the guest serial console to THIS terminal so an
    // operator can use the greeter and exit the guest (`exit`/Ctrl-D at the shell powers
    // it off, or qemu's own Ctrl-A X). With stdin not a terminal (piped, redirected, or
    // backgrounded) qemu boots but cannot be driven, so it would hang uncontrollably.
    // Refuse before any planning or build (re #541, Codex review); a headless pass/fail
    // boot smoke test is the `qemu-boot` check, not this.
    if !io::stdin().is_terminal() {
        return Err(format!(
            "`run {STEM}` is interactive and needs a terminal on stdin: it wires the guest \
             serial console to this terminal so you can use the greeter and exit the guest \
             (`exit`/Ctrl-D at the shell, or qemu Ctrl-A X). Run it directly in a terminal \
             (not piped, redirected, or backgrounded). For a headless pass/fail boot check, \
             use the `qemu-boot` check instead."
        ));
    }
    // Provenance planning FIRST — before the runner exists, so a rejected graph
    // spawns no subprocess at all (re #469), matching `cli`/`build_cli`/`qemu_boot`.
    let targets = [stem, "btrfs-progs-x86-64"];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("run", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    warm_operator_inputs(&runner, &targets);
    let lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    // The interactive boot runs unbounded (until the operator quits qemu), so hand the
    // ladder lock to the runner: it releases it after the build, before the boot, so the
    // whole ladder is not blocked for the entire session (re #541, Codex review). setup()
    // above and the build inside run() still hold it.
    crate::checks::run::run(&runner, lock)
}

/// `td-recipe-eval warm [TARGET]` — fetch every declared input TARGET's closure
/// needs and the caches lack, and build nothing. The same prep the operator
/// commands now do for themselves, as a standalone step: for preparing a tree
/// ahead of a build, or from a script, where the absent terminal holds the
/// automatic prep back. Asking for it IS the consent it needs, so this is the
/// one entry point that does not condition on a terminal.
pub fn warm_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "system-x86-64";
    // Arity before the stem lookup: `warm a b` is a usage error, not a report
    // that `a' is an unknown recipe.
    if args.get(1).is_some() {
        return Err("usage: warm [TARGET]".to_string());
    }
    let stem = args.first().map(String::as_str).unwrap_or(STEM);
    if catalog::lookup(stem).is_none() {
        return Err(format!("unknown recipe stem '{stem}' (try `list`)"));
    }
    // Same order as every other host-side command: a graph with an inadmissible
    // input is rejected before anything is placed, spawned, or fetched.
    let targets = [stem];
    ensure_targets_provenance(&targets)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("warm", &targets);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    // Unlike the piggy-backed callers, a residual cold input IS this command's
    // failure — warming is the whole job, and a script gating an offline build
    // on it needs the exit code to say so.
    crate::warm::preflight(&runner, &targets)
}

pub fn build_cli(args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or_else(build_usage)?.as_str();
    if catalog::lookup(target).is_none() {
        return Err(format!("unknown recipe stem '{target}' (try `list`)"));
    }
    let outputs: Vec<&str> = if args.get(1).is_some() {
        args.iter().skip(1).map(String::as_str).collect()
    } else {
        vec![target]
    };
    // Every requested output must be a rung of TARGET's own recipe closure:
    // build-run plans ONE graph (`build-plan --auto TARGET`) and reads each
    // output's STEP line from that single build log, so a stem outside the
    // closure could only red AFTER the whole build ran. Refuse it up front.
    let members: HashSet<String> = {
        let _t = timed_phase("harness recipe_closure");
        recipe_closure(&[target])?
            .into_iter()
            .map(|n| n.stem)
            .collect()
    };
    for output in &outputs {
        if catalog::lookup(output).is_none() {
            return Err(format!(
                "unknown output recipe stem '{output}' (try `list`)"
            ));
        }
        if !members.contains(*output) {
            return Err(format!(
                "output stem '{output}' is not in the recipe closure of '{target}', \
                 so the '{target}' build plan cannot produce it"
            ));
        }
    }

    // Provenance planning FIRST — before the runner exists, so a rejected
    // graph spawns no subprocess at all (re #469).
    {
        let _t = timed_phase("harness provenance");
        ensure_targets_provenance(&[target])?;
    }

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("build", &[target]);
    let runner = {
        let _t = timed_phase("harness new/stage0-place");
        RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress()
    };
    let _lock = {
        let _t = timed_phase("harness setup");
        lock_ladder_for_run(&runner)?
    };
    runner.build_recipe_target(target, &outputs)
}

/// The full pinned-seed universe of the catalog: every seed input ANY recipe
/// declares, classified PER INPUT — a sibling input with no admissible
/// provenance (which reds the whole graph at planning) does not hide the
/// pinned seeds the same recipe declares. Shared by the `seed-digests`
/// generator and the table-coverage test so both walk the same universe.
fn catalog_seed_universe() -> Result<Vec<SeedInput>, String> {
    let mut seen = HashSet::new();
    let mut seeds = Vec::new();
    for (_, recipe) in catalog::all() {
        if let Some(key) = &recipe.source_input {
            push_seed_input(&mut seeds, &mut seen, seed_input_for_recipe_source(key, &recipe)?);
        }
        // `payload_inputs` walks with the other two: a payload declared as an
        // external pinned input needs its compiled digest row like any seed, and
        // omitting the channel here would classify it at planning time and then
        // fail it at the digest gate — passing the generator's own workflow and
        // still refusing the build.
        for input in recipe
            .inputs
            .iter()
            .chain(recipe.native_inputs.iter())
            .chain(recipe.payload_inputs.iter())
            .flatten()
        {
            if catalog::lookup(input).is_some() {
                continue;
            }
            if let Some(seed) = seed_input_for_recipe_input(input)? {
                push_seed_input(&mut seeds, &mut seen, seed);
            }
        }
    }
    Ok(seeds)
}

/// seed-digests: derive the catalog's whole pinned-seed universe
/// (`catalog_seed_universe` — every seed any recipe declares, including
/// recipes whose graphs currently red at planning on OTHER inputs) from the
/// compiled pins, through the exact `derive_seed_input` path the runner
/// enforces, and print the full seed/seed-digests.txt content — header
/// comment plus sorted `key basename` rows — on stdout. Requires the warm
/// source cache, like any ladder run.
pub fn seed_digests_cli() -> Result<(), String> {
    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let mut runner = RecipeCheckRunner::new(root, "seed-digests")?;
    // Everything this run registers is written aside — see `generator_db_path`.
    runner.db = generator_db_path(&runner.lw, &runner.scratch_id());
    let _lock = lock_ladder(&runner.lock_path(), LadderLock::Exclusive)?;
    runner.setup()?;
    let mut rows: BTreeMap<String, String> = BTreeMap::new();
    for input in catalog_seed_universe()? {
        let derived = runner.derive_seed_input(&input)?;
        rows.insert(
            input.key().to_string(),
            path_basename_str(&derived)?.to_string(),
        );
    }
    // Best-effort: the rows have served their purpose (the basenames are in `rows`),
    // and a kept file would just accumulate one per generator run.
    let _ = fs::remove_file(&runner.db);
    println!(
        "# seed/seed-digests.txt — the compiled seed-digest table (re #469).\n\
         # Every admissible seed input's expected store basename, derived from its\n\
         # compiled pin. Compiled into td-recipe-eval (enforced after every seed\n\
         # derivation) and td-builder (enforced at build-plan lock synthesis).\n\
         # Regenerate with `td-recipe-eval seed-digests > seed/seed-digests.txt`\n\
         # (warm source cache required) when a pin, seed patch, or the stage0\n\
         # source changes. Hand-editing a row is self-defeating: the runner\n\
         # re-derives from the pins every run and reds on disagreement."
    );
    for (k, b) in &rows {
        println!("{k} {b}");
    }
    Ok(())
}

/// local-source-digests: check every catalog `local_source` tree against the row
/// the compiled table pins for it.
///
/// The cheap half of `seed-digests`. That command re-derives the WHOLE seed
/// universe and so needs a warm source cache and the ladder; a local source needs
/// neither — its bytes are already in the checkout, so the check is a tree copy
/// and a NAR hash. It exists because the key-set coverage test cannot see this
/// class of staleness at all: editing `tests/sshd` leaves the key present and the
/// row unchanged, so coverage stays green while the row now describes a tree that
/// no longer exists. Nothing but a re-hash catches that, and re-hashing a fetched
/// pin is expensive while re-hashing this is not — which is the whole reason it
/// can be a per-change gate.
///
/// Deliberately NOT a `RecipeCheckRunner`: no ladder lock, no ladder scratch, no
/// stage0 placement, no store and no db. It is wired to the recipes surface, which
/// is edited constantly, and it must never be the thing that makes
/// `affected-checks --run` sit behind another agent's multi-hour climb on the
/// SHARED ladder — every other preflight is lock-free and this one has no business
/// being the exception. td-builder is used only to NAR-hash a scratch copy, which
/// is what `--auto` staging would do to it anyway; nothing here derives a seed.
pub fn local_source_digests_cli() -> Result<(), String> {
    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let tb = find_td_builder_self(&root)?;
    let scratch = env::temp_dir().join(format!("td-local-source-digests-{}", process::id()));
    remove_path_if_exists(&scratch)?;
    fs::create_dir_all(&scratch).map_err(|e| format!("mkdir {}: {e}", scratch.display()))?;
    let mut checked = 0u32;
    let mut errors: Vec<String> = Vec::new();
    for input in catalog_seed_universe()? {
        let SeedInput::LocalSource { key, path } = &input else {
            continue;
        };
        checked += 1;
        // Every local source is reported, so one that cannot even be hashed does not
        // hide a stale row behind it — and the cleanup below stays reachable.
        let hashed = stage_local_source_at(&root, key, path, &scratch.join(key))
            .and_then(|staged| store_path_recursive_with(&tb, &root, key, &staged))
            .and_then(|candidate| {
                gate_local_source_candidate(key, &candidate).map(|()| candidate)
            });
        match hashed {
            Ok(candidate) => {
                println!("local source `{key}' ({path}) hashes to its pinned {candidate}")
            }
            Err(e) => errors.push(e),
        }
    }
    remove_path_if_exists(&scratch)?;
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    // A gate that passes over an empty class is a gate that stopped working. The
    // catalog has a local source; if this ever finds none, the classifier changed
    // under it, not the tree.
    if checked == 0 {
        return Err("no local sources found in the catalog — this check verifies nothing; \
                    the `local_source` classifier or the catalog changed under it"
            .to_string());
    }
    println!("PASS: {checked} local source(s) agree with seed/seed-digests.txt");
    Ok(())
}

fn usage() -> String {
    "usage: check-run STEM [INDEX]".to_string()
}

fn build_usage() -> String {
    "usage: build-run TARGET [OUTPUT_STEM ...]".to_string()
}

fn parse_index(arg: Option<&String>) -> Result<usize, String> {
    match arg {
        Some(s) => {
            let n = s
                .parse::<usize>()
                .map_err(|_| format!("check index '{s}' is not a positive integer"))?;
            if n == 0 {
                return Err("check index must be 1-based".to_string());
            }
            Ok(n)
        }
        None => Ok(1),
    }
}

fn scratch_name(prefix: &str, parts: &[&str]) -> String {
    let mut out = sanitize_scratch_component(prefix);
    for part in parts {
        out.push('-');
        out.push_str(&sanitize_scratch_component(part));
    }
    out.push('-');
    out.push_str(&process::id().to_string());
    out
}

fn sanitize_scratch_component(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "x".to_string()
    } else {
        out
    }
}

/// The trailing `-<pid>` a `scratch_name` appends, ignoring a `.<n>` disambiguator
/// `claim_scratch` may have added. Returns the pid iff that component is a non-empty
/// all-ASCII-digit run — so a reaper can tell one of our scratch trees apart from any
/// other directory. None for anything not so shaped (never touched by the reaper).
///
/// The pid is a DEBUGGING HINT and nothing more: it says which process made the tree
/// on the host, and is deliberately not asked whether it is alive. See `claim_scratch`.
fn trailing_pid(name: &str) -> Option<u32> {
    let stem = match name.rsplit_once('.') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => name,
    };
    let last = stem.rsplit('-').next()?;
    if last.is_empty() || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    last.parse::<u32>().ok()
}

/// Whether a directory name is one of OUR scratch trees, and so eligible for reaping —
/// `scratch_name` emits `build-…` / `check-…` / `qemu-boot-…` / `run-…` ending in a
/// numeric pid, optionally `.<n>`-disambiguated. The prefix guard means a coincidental
/// sibling such as `gcc-14` or `glibc-241` can never be reaped (belt-and-braces: this
/// dir holds only our scratch trees anyway). The `qemu-boot-` and `run-` prefixes are
/// essential: the host-side qemu-boot and interactive `run` tools create per-boot
/// scratch trees here too, and without them a crashed/killed boot's tree (which can
/// hold a multi-GiB kernel build) would leak forever. Split out so the reaper's
/// eligibility rule is unit-testable.
///
/// Eligible is not reapable: whether the tree is ABANDONED is answered by its claim
/// lock, never by this name.
fn reapable_scratch(name: &str) -> bool {
    if !name.starts_with("build-")
        && !name.starts_with("check-")
        && !name.starts_with("qemu-boot-")
        && !name.starts_with("run-")
    {
        return false;
    }
    trailing_pid(name).is_some()
}

/// The staging temp a build-run memo is published through. Split out as a pure function
/// because the property that matters is a property of the NAME — that two live runs
/// cannot pick the same one — and a concurrency test cannot pin it: whether two racing
/// renames actually collide is a matter of timing, so such a test passes just as happily
/// with the run id removed. This one fails the moment it is.
fn build_run_memo_temp_name(target: &str, fingerprint: &str, run_id: &str) -> String {
    format!(
        ".{}.{fingerprint}.{run_id}.tmp",
        sanitize_target_for_filename(target)
    )
}

/// The claim lock for a scratch NAME, a dotfile beside the tree it guards. Dotted so
/// the reaper's `read_dir` cannot mistake it for a scratch tree, and a SIBLING rather
/// than a child so it survives the tree being removed.
fn scratch_claim_lock(scratch_root: &Path, name: &str) -> PathBuf {
    scratch_root.join(format!(".{name}.claim"))
}

/// The number of `.<n>` disambiguators tried before a claim gives up. A collision needs
/// one live peer per attempt on the identical check, so this is far past any real fleet.
const SCRATCH_CLAIM_ATTEMPTS: usize = 64;

/// Claim a scratch directory EXCLUSIVELY, returning the claimed path and the lock that
/// holds it. The lock must outlive the run.
///
/// This replaces asking `/proc` whether a pid is alive, which stopped being answerable
/// the moment runs overlapped. The ladder is bind-mounted into the loop sandbox at the
/// same absolute path (`check_loop`), and that sandbox unshares `CLONE_NEWPID` over a
/// FRESH procfs — so a peer's pid is simply ABSENT from a sandboxed `/proc` (its live
/// tree reads as dead and was reaped under it), and two sandboxed runs of the same
/// check hold the SAME low pid (one `setup` then wiped the other's live tree). Both
/// were invisible while the exclusive ladder lock meant only one run existed at a time.
///
/// An flock is namespace-independent and answers exactly the question asked: the kernel
/// releases it when the owning process dies, however it dies. The lock file is NEVER
/// unlinked — reaping removes the TREE only — because a claimant that had the old inode
/// open would otherwise lock a file already replaced at that path, and two runs would
/// hold one name. It is empty, and bounded by the names a ladder ever uses.
fn claim_scratch(scratch_root: &Path, name: &str) -> Result<(PathBuf, File), String> {
    for attempt in 0..SCRATCH_CLAIM_ATTEMPTS {
        let candidate = match attempt {
            0 => name.to_string(),
            n => format!("{name}.{}", n + 1),
        };
        let lock_path = scratch_claim_lock(scratch_root, &candidate);
        let lock = open_lock_file(&lock_path)?;
        match lock.try_lock() {
            // A live peer owns this name — in this or any other pid namespace.
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("claim {}: {e}", lock_path.display()))
            }
            Ok(()) => {}
        }
        // Ours now, so a leftover tree under this name is a DEAD predecessor's and is
        // the one thing safe to clear here: nothing else may hold this name.
        let dir = scratch_root.join(&candidate);
        remove_path_if_exists(&dir)?;
        return Ok((dir, lock));
    }
    Err(format!(
        "ladder: {SCRATCH_CLAIM_ATTEMPTS} live runs already hold a scratch named {name} \
         under {} — refusing to share one",
        scratch_root.display()
    ))
}

/// The ladder work dir — the tree `check-run`/`build-run` build into and the explicit
/// `clear-store` nukes: the one shared daemon ladder under `~/.td/build-daemon` (HOME-derived),
/// falling back to a repo-local dir only when HOME is unset. Shared by `new()` (which builds
/// here) and `clear_store_cli` (which resets it), so both name the identical tree. There is no
/// cold-ladder override — a from-stage0 climb is an explicit `clear-store`, nothing else.
fn ladder_work_dir(root: &Path, home: Option<&Path>) -> PathBuf {
    // Fixed trust/layout epoch (same basename in both branches). Bump only on a trust/layout
    // change, not a pin. The repo-local fallback is HOME-unset only.
    home.map(|h| h.join(".td/build-daemon/ladder-shared-v1"))
        .unwrap_or_else(|| root.join(".td-build-cache/ladder-shared-v1"))
}

/// The single shared sources cache: `$HOME/.td/sources` (HOME unset/empty -> relative), the
/// flat pin-filename-keyed dir the ladder reads its warmed tarballs and generated
/// kernel-headers from. Shared across ALL worktrees with NO env override; keep identical to
/// the feed/builder copies (`home` comes from `var_os`, so a non-UTF-8 HOME resolves the
/// same in all three). Deliberately NOT under `~/.td/build-daemon` (bound RW into sandboxes).
fn shared_sources_dir(home: Option<&Path>) -> PathBuf {
    home.map(|h| h.join(".td/sources"))
        .unwrap_or_else(|| PathBuf::from(".td/sources"))
}

/// The seed db for THIS worktree's compiled pin table, `<lw>/seed-db/<digest>.db`.
///
/// `<lw>` is machine-wide but a pin table is a branch's, so ONE shared db is a db
/// every run must reconcile to its own table — deleting rows, and the trees under
/// them, that a peer worktree still pins. Keyed, there is nothing to reconcile: a
/// table change lands on a fresh db and re-interns, so a pin bump costs the changed
/// seed rather than a cold climb. The STORE stays shared and unkeyed — items are
/// content-addressed, so a basename two tables both name is the same bytes, and the
/// common seeds (every fetched tarball) are interned once for the machine.
/// The retained seed store, `<lw>/seed-store`.
///
/// NOT the `<lw>/store` this replaces, and the rename is the whole point: a binary
/// from before the keyed db still runs `prune_unpinned_seeds` at every setup, and
/// that prune DELETES trees whose basename ITS table does not pin — including ones a
/// keyed db here vouches, which would then red at `build-plan` with the tree simply
/// missing. Its exclusive ladder lock keeps the two apart in time and repairs
/// nothing. An old pruner cannot reach a path it does not know, so the cut-over is
/// the rename; the cost is one re-intern per machine, from the already-warm source
/// cache. `<lw>/store` is left where it is for those binaries to keep using.
fn seed_store_dir(lw: &Path) -> PathBuf {
    lw.join("seed-store")
}

/// A DISPOSABLE seed db for the table GENERATOR, beside the keyed ones.
///
/// `seed-digests` derives the whole seed universe from the pins and interns what it
/// derives — which during a pin bump is exactly a basename the CURRENT table does not
/// pin. Merged into the current table's db that is a row `authenticate_seed_db`
/// rejects the WHOLE db over, and with the unpinned-seed prune gone nothing heals it:
/// regenerating the table would brick every worktree still on the old one, including
/// the one running the generator. So the generator's rows go where nothing
/// authenticates them. Under the SAME parent as the keyed dbs deliberately —
/// `lock_store_commit` derives the shared store's commit lock from the db's parent,
/// and the generator interns into that store like any other run.
fn generator_db_path(lw: &Path, scratch_id: &str) -> PathBuf {
    lw.join("seed-db")
        .join(format!(".generator-{scratch_id}.db"))
}

fn seed_db_path(lw: &Path) -> Result<PathBuf, String> {
    let digest = crate::seed_digests::table_digest()?;
    // 16 hex chars: this names a cache directory, not a trust anchor — the table
    // itself is the authority and is compiled in.
    let short = digest.get(..16).unwrap_or(digest.as_str());
    Ok(lw.join("seed-db").join(format!("{short}.db")))
}

/// The ladder's sibling lock, `<lw>.lock`. APPENDS `.lock` to the whole path rather than
/// `with_extension` (which would REPLACE a dotted final component, e.g. a ladder path
/// ending in `.v2`, and collide two distinct ladders on one lock). Shared by the build runner
/// (`lock_path`) and `clear_ladder` so a wipe can never race a live build via a split lock.
fn ladder_lock_path(lw: &Path) -> PathBuf {
    let mut s = lw.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// The DEDICATED persistent build-output cache (store, db) under the ladder work dir.
/// Deliberately DISTINCT from the seed store/db (`<lw>/seed-store`,
/// `<lw>/seed-db/<table>.db`): those hold
/// interned seed inputs and #468 authenticates the seed db as a seed-only authority, so a
/// recipe OUTPUT committed there would be rejected as an unpinned seed. The cache lives in
/// its own subtree so reuse never pollutes the seed authority. Shared across worktrees and
/// content-addressed, so it is never wiped on a pin/patch change. Nothing reclaims it
/// implicitly; the explicit `clear-store` resets the whole ladder, and an opt-in
/// `TD_CHECK_LADDER_CACHE_CAP_BYTES` enables a coarse high-watermark eviction of the whole
/// `build-cache/` (store + db + `db.receipts` sidecars — the coherent unit the builder writes).
fn build_cache_paths(lw: &Path) -> (PathBuf, PathBuf) {
    let base = lw.join("build-cache");
    (base.join("store"), base.join("db"))
}

/// sha256 over the evaluator binary, the builder binary, every
/// `seed/patches/*.patch`, and every committed `cargo_locks` file, in a fixed
/// order — the pure-plan fingerprint that keys the build-run reuse memo. Recipes
/// and seed pins are compiled INTO the binaries (a change rebuilds them and
/// re-keys); a patch change alters a hashed file; and a rust rung's committed
/// `Cargo.lock` is the one build input read from the repo at build time rather
/// than compiled in, so a lock bump (which changes that rung's output) must
/// re-key here too. A `local_source` recipe (#469) is the same shape as a lock:
/// its bytes are an editable in-tree DIR read at build time, not a hash compiled
/// into the binary, so an edit MUST re-key here or a memo hit would serve stale
/// bytes. Its tree is hashed the same way the interner walks it (`hash_source_tree`),
/// so this fingerprint co-varies with the interned content address. Every field is
/// length-delimited so no boundary is ambiguous (concatenation cannot collide).
/// `cargo_locks` and `local_sources` must each be sorted and deduped by the caller;
/// a declared lock or source dir that cannot be read fails closed.
fn plan_fingerprint(
    eval: &Path,
    builder: &Path,
    patches_dir: &Path,
    repo_root: &Path,
    cargo_locks: &[String],
    local_sources: &[String],
) -> Result<String, String> {
    let mut h = crate::sha256::Sha256::new();
    for bin in [eval, builder] {
        let bytes = fs::read(bin).map_err(|e| format!("read {}: {e}", bin.display()))?;
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    // Only a MISSING patch dir hashes as zero patches (a tree that GAINS the dir
    // re-keys by growing the hashed list). Any other read error — and any entry
    // error — fails closed: silently dropping a patch from the key would let a
    // stale memo survive a real patch change and serve wrong bytes.
    let mut names: Vec<String> = match fs::read_dir(patches_dir) {
        Ok(rd) => {
            let mut v = Vec::new();
            for entry in rd {
                let entry = entry
                    .map_err(|e| format!("read_dir entry {}: {e}", patches_dir.display()))?;
                // Skip a directory that happens to be named `*.patch`: `fs::read`
                // on it below would fail the whole fingerprint (and thus the
                // build). A file_type() error still fails closed.
                let ft = entry
                    .file_type()
                    .map_err(|e| format!("file_type {}: {e}", entry.path().display()))?;
                if ft.is_dir() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".patch") {
                        v.push(name.to_string());
                    }
                }
            }
            v
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("read_dir {}: {e}", patches_dir.display())),
    };
    names.sort();
    for name in &names {
        let p = patches_dir.join(name);
        let bytes = fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
        h.update(&(name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    for rel in cargo_locks {
        let p = repo_root.join(rel);
        let bytes = fs::read(&p)
            .map_err(|e| format!("read committed cargoLock {}: {e}", p.display()))?;
        h.update(&(rel.len() as u64).to_le_bytes());
        h.update(rel.as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    for rel in local_sources {
        let p = repo_root.join(rel);
        h.update(&(rel.len() as u64).to_le_bytes());
        h.update(rel.as_bytes());
        // hash_source_tree emits its own self-delimiting type/length framing, and
        // this section follows cargo_locks in a fixed order, so no cross-section
        // ambiguity. A missing/unreadable declared source dir fails closed.
        hash_source_tree(&p, &mut h)?;
    }
    Ok(crate::sha256::to_base16(&h.finalize()))
}

/// Hash a local-source tree into `h` exactly the way `copy_source_tree` interns it
/// — skip `target`/`.git`, sorted entries, symlink targets verbatim, file contents
/// plus the executable bit (`mode & 0o100`, the only mode bit the NAR records) — so
/// the build-run memo fingerprint co-varies with the store content address without
/// recomputing the NAR. Fails closed on any I/O error or an unrepresentable node.
fn hash_source_tree(dir: &Path, h: &mut crate::sha256::Sha256) -> Result<(), String> {
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("stat {}: {e}", dir.display()))?;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        let target = fs::read_link(dir).map_err(|e| format!("readlink {}: {e}", dir.display()))?;
        let tb = target.as_os_str().as_bytes();
        h.update(b"L");
        h.update(&(tb.len() as u64).to_le_bytes());
        h.update(tb);
        return Ok(());
    }
    if ftype.is_dir() {
        h.update(b"D");
        let mut children = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("read_dir entry {}: {e}", dir.display()))?;
            children.push(entry.path());
        }
        children.sort();
        for child in children {
            let Some(name) = child.file_name().map(|n| n.to_owned()) else {
                continue;
            };
            if matches!(name.to_str(), Some("target") | Some(".git")) {
                continue;
            }
            let nb = name.as_bytes();
            h.update(&(nb.len() as u64).to_le_bytes());
            h.update(nb);
            hash_source_tree(&child, h)?;
        }
        return Ok(());
    }
    if ftype.is_file() {
        let bytes = fs::read(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        h.update(if meta.permissions().mode() & 0o100 != 0 {
            b"X"
        } else {
            b"F"
        });
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
        return Ok(());
    }
    Err(format!("unsupported file type at {}", dir.display()))
}

/// Parse a build-run reuse memo, returning stem -> output basename ONLY when its
/// header fingerprint matches `expected_fp` (a stale-plan file is ignored, though
/// the fingerprint is also in the filename). Every basename is validated so a
/// corrupted row can never escape the cache/tdstore dirs it is joined onto.
fn parse_build_run_memo(text: &str, expected_fp: &str) -> Option<BTreeMap<String, String>> {
    let mut lines = text.lines();
    let fp = lines.next()?.strip_prefix("fingerprint ")?.trim();
    if fp != expected_fp {
        return None;
    }
    let mut map = BTreeMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(2, ' ');
        let (stem, base) = (it.next()?, it.next()?.trim());
        if !is_plain_basename(base) {
            return None;
        }
        map.insert(stem.to_string(), base.to_string());
    }
    Some(map)
}

fn serialize_build_run_memo(fingerprint: &str, steps: &BTreeMap<String, String>) -> String {
    let mut s = format!("fingerprint {fingerprint}\n");
    for (stem, base) in steps {
        s.push_str(stem);
        s.push(' ');
        s.push_str(base);
        s.push('\n');
    }
    s
}

/// The stem -> output-basename map from a `build-plan --auto` log's STEP lines,
/// LAST line winning (matching `ladder_out_from`). Only a completely realized
/// rung emits a STEP line, so every recorded base is a fully-committed output.
fn parse_step_map(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("STEP ") {
            let mut it = rest.splitn(2, ' ');
            if let (Some(stem), Some(path)) = (it.next(), it.next()) {
                let base = path.trim().rsplit('/').next().unwrap_or("");
                if is_plain_basename(base) {
                    map.insert(stem.to_string(), base.to_string());
                }
            }
        }
    }
    map
}

/// A bare store basename — no path separator (`/` or `\`), no whitespace, and not
/// a `.`/`..` traversal — safe to `join` onto the cache store and tdstore. An
/// fsck-grade guard: the memo lives under our own ladder dir, but a corrupted row
/// must never escape it. `\` is not a separator on the Linux hosts td targets, but
/// rejecting it keeps the guard sound if this parser is ever reused elsewhere.
/// Rejecting whitespace also fails a memo closed if a space ever leaks into a step
/// stem (the `splitn(2, ' ')` would fold the stem tail into the base).
fn is_plain_basename(b: &str) -> bool {
    !b.is_empty()
        && !b.contains('/')
        && !b.contains('\\')
        && !b.contains(|c: char| c.is_whitespace())
        && b != "."
        && b != ".."
}

fn selected_check_runner(stem: &str, index: usize) -> Result<CheckRunner, String> {
    let recipe = catalog::lookup(stem)
        .ok_or_else(|| format!("unknown recipe stem '{stem}' (try `list`)"))?;
    let mut count = 0;
    if let Some(checks) = &recipe.checks {
        for check in checks {
            count += 1;
            if count == index {
                return check.runner.ok_or_else(|| {
                    format!("{stem} check index {index} has no Rust check-runner implementation")
                });
            }
        }
    }
    if count == 0 {
        return Err(format!("{stem} owns no checks"));
    }
    Err(format!(
        "{stem} owns only {count} check(s); index {index} is out of range"
    ))
}

/// How a caller holds the ladder: EXCLUSIVE for whole-ladder operations
/// (`clear-store`, the fsck, the boot harnesses), SHARED for a build or check,
/// which reads the warm cache and writes only its own pid-tagged scratch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LadderLock {
    Shared,
    Exclusive,
}

/// A build or check may SHARE the ladder — unless eviction is armed, which makes
/// `setup()` rename `build-cache/` aside, and a concurrent reader of that cache
/// cannot survive it. Nothing else in a run mutates the shared ladder: the seed db
/// is keyed per pin table (so there is no prune), the scratch is pid-private, and
/// store commits take the separate per-store commit lock. Pure, so the policy is
/// testable without the ambient env.
fn ladder_lock_mode(cache_cap: Option<u64>) -> LadderLock {
    match cache_cap {
        Some(_) => LadderLock::Exclusive,
        None => LadderLock::Shared,
    }
}

/// Take the ladder for a build or check, holding it for the WHOLE run — setup
/// included. One acquisition, so no window where the run holds nothing and a
/// `clear-store` could wipe the tree out from under it.
fn lock_ladder_for_run(runner: &RecipeCheckRunner) -> Result<File, String> {
    lock_ladder_for_run_with_cache_cap(runner, explicit_ladder_cache_cap())
}

/// The cap is read ONCE, here, and the SAME value both picks the lock mode and drives
/// setup. Reading it twice is not equivalent: an env change between the two — or any
/// future caller that passes a cap to one and not the other — takes the ladder SHARED
/// and then evicts, and eviction renames `build-cache/` aside under concurrent readers
/// that cannot survive it. Threading the value is what makes "cap armed ⇒ exclusive"
/// structural rather than a convention two call sites separately honour.
fn lock_ladder_for_run_with_cache_cap(
    runner: &RecipeCheckRunner,
    cache_cap: Option<u64>,
) -> Result<File, String> {
    let held = lock_ladder(&runner.lock_path(), ladder_lock_mode(cache_cap))?;
    runner.setup_with_cache_cap(cache_cap)?;
    Ok(held)
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open lock {}: {e}", path.display()))
}

fn lock_ladder(path: &Path, mode: LadderLock) -> Result<File, String> {
    let file = open_lock_file(path)?;
    let held = match mode {
        LadderLock::Shared => file.lock_shared(),
        // flock grants no writer preference, so a steady stream of builds can hold an
        // exclusive waiter (`clear-store`, the fsck) out for as long as they keep
        // overlapping. Nothing here can preempt them — but before this the queue always
        // drained, so an operator now needs telling WHY the command is sitting there.
        //
        // ONLY for the ladder. The commit locks go through `lock_file`, which waits
        // plainly: they are taken twice per intern and held for a db merge, so a
        // contended one is ordinary and announcing each would bury a gate log the
        // runner captures wholesale — under a message about "builds", which a
        // sub-second merge is not.
        LadderLock::Exclusive => match file.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => {
                eprintln!(
                    "ladder: waiting for concurrent builds to release {}",
                    path.display()
                );
                file.lock()
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e),
            Ok(()) => Ok(()),
        },
    };
    held.map_err(|e| format!("lock {}: {e}", path.display()))?;
    Ok(file)
}

/// A plain EXCLUSIVE flock, waiting silently — the commit locks, which have no shared
/// mode at all, and the whole-tree ladder operations that already announce themselves.
fn lock_file(path: &Path) -> Result<File, String> {
    let file = open_lock_file(path)?;
    file.lock()
        .map_err(|e| format!("lock {}: {e}", path.display()))?;
    Ok(file)
}

pub(crate) struct RecipeCheckRunner {
    root: PathBuf,
    tb: PathBuf,
    builder_path: String,
    builder_store: PathBuf,
    builder_db: PathBuf,
    lw: PathBuf,
    /// The single shared sources cache (`$HOME/.td/sources`) holding the warmed pinned
    /// tarballs + generated kernel-headers, shared across worktrees (see `shared_sources_dir`).
    sources_dir: PathBuf,
    store: PathBuf,
    db: PathBuf,
    recipes: PathBuf,
    scratch: PathBuf,
    /// The REAL daemon runtime dir (`TD_DAEMON_DIR` or the OUTER
    /// `$HOME/.td/build-daemon`), forwarded to spawned td-builders whose HOME
    /// is re-pointed at the ladder work dir — the derived blessed-seed-db
    /// lookup (re #469 round-8) keys on this dir, and without the forward it
    /// would resolve under the ladder HOME where nothing was ever blessed.
    daemon_dir: Option<String>,
    /// When set, `build_plan` TEES the builder's per-rung stderr to this process's
    /// stderr live instead of swallowing it until the build ends — so an operator
    /// watching a cold multi-minute ladder climb (host-side `run`/`build-run`/
    /// `qemu-boot`) sees each rung land. Off for gate `check-run`, whose output the
    /// gate captures wholesale.
    stream_progress: bool,
    /// Store paths this pin table's seed db vouches: `None` until first asked, then
    /// kept current as this run registers — see `db_vouches`.
    vouched: std::sync::Mutex<Option<HashSet<String>>>,
    /// The exclusive claim on `scratch`, held for this runner's whole life: it is what
    /// tells a peer's reaper that this tree is live (see `claim_scratch`). `None` only
    /// in tests, which construct the struct directly and share no ladder.
    scratch_lock: Option<File>,
}

pub(crate) struct RecipeNode {
    pub(crate) stem: String,
    pub(crate) recipe: Recipe,
}

/// Gate one local source's freshly computed content address against the compiled
/// table. Separate from `seed_digests::require` only for its recovery line: the
/// generic wording blames "a pin bump without regenerating", which for a local
/// source is never what happened — nobody bumped anything, the tree was edited.
/// It also has to say what NOT to do: the failure surfaces near enough to the
/// stale-seed-store reds that `clear-store` looks like the fix, and it is not one.
/// A cold ladder derives the same address from the same tree and reds identically,
/// having thrown away every rung to get there.
/// Resolve and validate a `local_source` path against a repo root: it must be a
/// plain repo-relative path (no `..`/`.`/absolute component) that, once symlinks
/// are resolved, stays under the root, naming a directory that is a Cargo crate
/// (Cargo.toml + committed Cargo.lock). Returns the CANONICAL path, so whoever
/// copies it copies the validated bytes.
fn resolve_local_source_dir_at(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("local source path is empty".into());
    }
    let relp = Path::new(rel);
    for comp in relp.components() {
        if !matches!(comp, std::path::Component::Normal(_)) {
            return Err(format!(
                "local source `{rel}' must be a plain repo-relative path \
                 (no `..', `.', or absolute root)"
            ));
        }
    }
    let dir = root.join(relp);
    if !dir.is_dir() {
        return Err(format!(
            "local source `{rel}' is not a directory ({})",
            dir.display()
        ));
    }
    // Defense beyond the lexical `..` check: a symlinked path COMPONENT could
    // still resolve outside the checkout and smuggle ambient (non-committed)
    // bytes into the interned seed, breaking the in-tree provenance boundary.
    // Canonicalize both and require the source stays under the repo root; the
    // caller then copies this resolved path.
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize repo root {}: {e}", root.display()))?;
    let canon_dir = dir
        .canonicalize()
        .map_err(|e| format!("canonicalize local source {}: {e}", dir.display()))?;
    if !canon_dir.starts_with(&canon_root) {
        return Err(format!(
            "local source `{rel}' resolves outside the checkout ({}) — a symlinked \
             component must not escape the repo (#469 in-tree provenance)",
            canon_dir.display()
        ));
    }
    if !canon_dir.join("Cargo.toml").is_file() {
        return Err(format!("local source `{rel}' has no Cargo.toml"));
    }
    if !canon_dir.join("Cargo.lock").is_file() {
        return Err(format!("local source `{rel}' has no committed Cargo.lock"));
    }
    Ok(canon_dir)
}

/// Copy a validated `local_source` tree (minus `target`/`.git`) to `dest`. The
/// staged tree is what BOTH the content-address computation and the intern read,
/// so the address that is gated and the bytes that are interned cannot diverge.
fn stage_local_source_at(root: &Path, key: &str, rel: &str, dest: &Path) -> Result<PathBuf, String> {
    let dir = resolve_local_source_dir_at(root, rel)?;
    remove_path_if_exists(dest)?;
    copy_source_tree(&dir, dest)
        .map_err(|e| format!("copy local source {} for `{key}': {e}", dir.display()))?;
    Ok(dest.to_path_buf())
}

/// `td-builder store-path-recursive`: the content address `src` WOULD intern at,
/// computed with no store and no db written.
fn store_path_recursive_with(
    tb: &Path,
    root: &Path,
    name: &str,
    src: &Path,
) -> Result<String, String> {
    let mut cmd = Command::new(tb);
    cmd.current_dir(root)
        .env("TD_STORE_DIR", TD_STORE_DIR)
        .arg("store-path-recursive")
        .arg(name)
        .arg(path_str(src)?);
    let out = command_output(&mut cmd, &format!("store-path-recursive {name}"))?;
    out.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("store-path-recursive {name} produced no path"))
}

fn gate_local_source_candidate(key: &str, candidate_path: &str) -> Result<(), String> {
    let candidate = path_basename_str(candidate_path)?;
    // The DECISION stays `require`'s, so a local source is admitted on exactly the
    // terms every other seed is; only the explanation below is local.
    if crate::seed_digests::require(key, candidate).is_ok() {
        return Ok(());
    }
    // PROVENANCE_REJECTED, not the literal: `die_runner` keys the 78 exit on this
    // prefix, so a drifted copy would silently reclassify the failure.
    Err(match crate::seed_digests::expected(key)? {
        Some(exp) => format!(
            "{PROVENANCE_REJECTED}local source `{key}' hashes to {candidate} but the compiled \
             table pins {exp} — the in-tree source was edited without regenerating \
             seed/seed-digests.txt. Fix the TABLE (`td-recipe-eval seed-digests > \
             seed/seed-digests.txt', or edit this one row to {candidate}) and commit it. \
             `clear-store' does NOT help: a cold ladder hashes the same tree to the same \
             {candidate} and reds here again, minus the build cache (re #469)"
        ),
        None => format!(
            "{PROVENANCE_REJECTED}local source `{key}' has no compiled expected digest in \
             seed/seed-digests.txt — an unpinned seed is not admissible; regenerate the table \
             with `td-recipe-eval seed-digests' and commit it (re #469)"
        ),
    })
}

#[derive(Debug)]
pub(crate) enum SeedInput {
    Stage0 { key: String },
    Source { key: String, pin: SourcePin },
    LinuxHeaders { key: String, arch: &'static str },
    Patch { key: String, patch: String },
    /// An IN-TREE source directory (#469 local-source provenance): `path` is the
    /// repo-relative dir the recipe's `local_source` names. Interned by copying
    /// the committed tree (minus build/VCS artifacts) into the seed store, then
    /// gated against the compiled table like every other seed.
    LocalSource { key: String, path: String },
}

impl SeedInput {
    fn key(&self) -> &str {
        match self {
            SeedInput::Stage0 { key }
            | SeedInput::Source { key, .. }
            | SeedInput::LinuxHeaders { key, .. }
            | SeedInput::Patch { key, .. }
            | SeedInput::LocalSource { key, .. } => key,
        }
    }
}

impl RecipeCheckRunner {
    fn new(root: PathBuf, scratch_name: &str) -> Result<Self, String> {
        let stage0_base = env::var_os("TD_STAGE0_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".td-build-cache/stage0"));
        let td_builder_self = find_td_builder_self(&root)?;
        let cb = place_stage0_builder(&root, &stage0_base, &td_builder_self)?;
        let cb_base = path_basename_str(&cb)?;
        let tb = stage0_base
            .join("store")
            .join(cb_base)
            .join("bin")
            .join("td-builder");
        if !is_executable(&tb) {
            return Err(format!(
                "stage0 td-builder not executable at {}",
                tb.display()
            ));
        }

        let home = env::var_os("HOME").map(PathBuf::from);
        let daemon_dir = match env::var("TD_DAEMON_DIR") {
            Ok(v) if !v.trim().is_empty() => Some(v),
            _ => home
                .as_ref()
                .map(|h| h.join(".td/build-daemon").display().to_string()),
        };
        let lw = ladder_work_dir(&root, home.as_deref());
        let sources_dir = shared_sources_dir(home.as_deref());
        let store = seed_store_dir(&lw);
        let db = seed_db_path(&lw)?;
        // Claimed here rather than in setup(): the claim is what makes this name OURS,
        // and every path below is derived from it.
        let (scratch, scratch_lock) = claim_scratch(&lw.join("scratch"), scratch_name)?;
        // Emitted recipe JSON is current-graph-only, so it lives under the
        // per-invocation scratch, not a shared/persistent dir.
        let recipes = scratch.join("recipes");
        Ok(Self {
            root,
            tb,
            builder_path: cb,
            builder_store: stage0_base.join("store"),
            builder_db: stage0_base.join("builder.db"),
            lw,
            sources_dir,
            store,
            db,
            recipes,
            scratch,
            daemon_dir,
            stream_progress: false,
            vouched: std::sync::Mutex::new(None),
            scratch_lock: Some(scratch_lock),
        })
    }

    /// Opt into live per-rung build progress: `build_plan` tees the build's stdout and
    /// stderr to this process's stdout/stderr as the ladder climbs, rather than buffering
    /// until the build finishes. Set by the host-side, human-invoked commands (`run`,
    /// `build-run`, `qemu-boot*`) so a cold multi-minute climb is not a silent wait.
    /// `TD_RECIPE_QUIET=1` overrides this back to the buffered path (see `quiet_requested`).
    pub(crate) fn with_streamed_progress(mut self) -> Self {
        self.stream_progress = true;
        self
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        ladder_lock_path(&self.lw)
    }

    /// This runner's private per-invocation scratch directory, freshly created by
    /// `setup()` under the ladder work dir (NOT world-writable `/tmp`). The qemu
    /// boot tool places its console/diagnostic capture here so those files live on
    /// a private, non-shared path — no cross-user symlink pre-planting is possible.
    pub(crate) fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// The ladder work dir — the tree an explicit `clear-store` nukes. The interactive
    /// runner uses this to refuse staging boot images anywhere inside it (a `TMPDIR`
    /// pointed into the ladder), which a concurrent post-lock `clear-store` could delete
    /// mid-boot.
    pub(crate) fn ladder_work_dir(&self) -> &Path {
        &self.lw
    }

    /// This ladder's dedicated build-output cache (store, db) — see `build_cache_paths`.
    fn build_cache_paths(&self) -> (PathBuf, PathBuf) {
        build_cache_paths(&self.lw)
    }

    /// Re-verify every registered path against its recorded NAR hash (the builder's
    /// `store-verify` fsck) for BOTH on-disk stores the warm hot path leans on: the persistent
    /// build-OUTPUT cache (the reuse path trusts the receipt + db row and skips a re-hash) AND the
    /// retained SEED store (`ensure_seed_input`/`auto_seed_provenance` trust an already-interned
    /// seed at its pinned basename per rung). This is an explicit, out-of-band integrity pass a
    /// build/run never pays for — run it to detect store rot/corruption. (The seed store is also
    /// CA-authenticated once per `build-plan` inline; this is the additional operator-driven
    /// fsck.) Verifies whichever of the two stores EXIST, independently — the seed store and the
    /// cache are populated on different schedules (a fresh checkout, or a post-`clear-store` cache
    /// drop, can leave one present and the other absent) — and errs only if NEITHER exists.
    pub(crate) fn verify_store(&self) -> Result<(), String> {
        let (cache_store, cache_db) = self.build_cache_paths();
        let mut checked = 0u32;
        // EVERY keyed seed db, not just this branch's. The store they register into is
        // shared and unkeyed, so an item only a PEER table vouches is still an item in
        // this ladder — fscking one db would leave it unreachable by the only thing that
        // re-hashes it, and the fsck is what `clear-store`'s advice rests on.
        for db in self.seed_dbs()? {
            self.store_verify_pair(&db, &self.store)?;
            checked += 1;
        }
        if cache_db.exists() {
            self.store_verify_pair(&cache_db, &cache_store)?;
            checked += 1;
        }
        if checked == 0 {
            return Err(format!(
                "nothing to verify: neither a seed db under {} nor the build cache db {} \
                 exists — build the ladder first (e.g. `td-recipe-eval run system-x86-64`)",
                self.db.parent().unwrap_or(&self.lw).display(),
                cache_db.display()
            ));
        }
        Ok(())
    }

    /// Every pin table's seed db on this ladder, sorted so a failure names the same one
    /// run to run. Dotfiles are skipped: the table GENERATOR's disposable db lives here
    /// too (see `generator_db_path`) and is deliberately not an authority.
    fn seed_dbs(&self) -> Result<Vec<PathBuf>, String> {
        let Some(dir) = self.db.parent() else {
            return Ok(Vec::new());
        };
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("read {}: {e}", dir.display())),
        };
        let mut dbs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "db"))
            .filter(|p| {
                !p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
            })
            .collect();
        dbs.sort();
        Ok(dbs)
    }

    /// One `td-builder store-verify DB STORE` fsck: re-hash every registered path against its
    /// recorded hash. The builder arm holds the store commit lock; the caller holds the ladder
    /// lock (see `verify_store_cli`) so a concurrent build/clear can't race the scan.
    fn store_verify_pair(&self, db: &Path, store: &Path) -> Result<(), String> {
        let mut cmd = self.builder_command();
        cmd.arg("store-verify").arg(path_str(db)?).arg(path_str(store)?);
        let status = cmd
            .status()
            .map_err(|e| format!("spawn td-builder store-verify: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("td-builder store-verify {} failed ({status})", db.display()))
        }
    }

    /// The stable per-cache commit lock, shared with the builder's commit transaction
    /// (builder `lock_store_commit`, which derives the same `<build-cache>.commit.lock`). Sited
    /// BESIDE `build-cache/`, never inside it, so eviction — which renames `build-cache/` aside
    /// — cannot split the lock across an evict/recreate. Eviction and the builder take this same
    /// lock, so GC never renames the cache out from under an uncovered committer.
    fn cache_commit_lock_path(&self) -> PathBuf {
        self.lw.join(CACHE_COMMIT_LOCK_BASENAME)
    }

    /// Prepare this invocation's private workspace WITHOUT destroying any persisted ladder
    /// state. setup() ensures the seed-store dir exists, creates a fresh per-invocation
    /// scratch, and reaps dead runs' abandoned scratch trees — it NEVER wipes the seed
    /// store/db or the shared build-cache. Resetting the ladder is the explicit `clear-store`
    /// command's sole job; a stale or torn seed now reds (with a clear-store hint) instead of
    /// being silently re-derived. The seeds re-intern idempotently every run regardless
    /// (`ensure_seed_input`), so a retained, intact seed store is reused, not clobbered.
    pub(crate) fn setup(&self) -> Result<(), String> {
        self.setup_with_cache_cap(explicit_ladder_cache_cap())
    }

    /// setup() with the eviction cap injected — the env-reading `setup()` is the production
    /// entrypoint; tests pass an explicit cap so they stay hermetic against the ambient
    /// `TD_CHECK_LADDER_CACHE_CAP_BYTES` knob. `None` ⇒ no eviction at all (the default): an
    /// implicit default-cap eviction would itself be a surprise cold-climb, exactly what
    /// dropping the auto-wipe avoids, so build-cache reclaim is opt-in via that env or the
    /// explicit `clear-store`.
    fn setup_with_cache_cap(&self, cache_cap: Option<u64>) -> Result<(), String> {
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        // The seed db lives one level down now (keyed per pin table), and the atomic
        // write that publishes it stages a sibling temp — neither creates the dir.
        if let Some(dir) = self.db.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        // Only THIS invocation's private, CLAIMED scratch is created — `claim_scratch`
        // already cleared any dead predecessor's tree under the name it won, and no
        // live run can hold that name, so nothing here can reach persisted store state.
        fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("mkdir {}: {e}", self.scratch.display()))?;
        // Reclaim disk from abandoned predecessors' scratch trees. Safe under a SHARED
        // ladder because it reaps by CLAIM rather than by name: a live peer's tree is
        // one whose lock it cannot take, in this or any other pid namespace.
        self.reap_dead_scratch();
        match cache_cap {
            Some(cap) => self.evict_build_cache_if_over_watermark(cap),
            None => Ok(()),
        }
    }

    /// Coarse disk reclaim for the SHARED build-output cache: over the high-watermark cap,
    /// evict the whole `build-cache/` subtree atomically — rename to a tombstone, then reap
    /// it — so a crash mid-reclaim leaves only a stale tombstone (reaped next setup), never a
    /// torn store/db/receipts triple. Content-addressing makes eviction safe: an evicted rung
    /// cold-climbs on next need, never mis-reuses. All-or-nothing, so a steady-state union
    /// over the cap re-evicts every setup; a low-watermark retention GC is the follow-up.
    fn evict_build_cache_if_over_watermark(&self, cap: u64) -> Result<(), String> {
        // Take the SAME stable commit lock the builder holds during a commit, held across reap +
        // size + rename + reap, so eviction never renames the cache out from under an uncovered
        // committer (an orphaned builder child, or a direct store-commit) the outer ladder lock
        // does not cover. Lock ordering is always ladder -> commit, so no inversion / deadlock.
        let _cache_lock = lock_file(&self.cache_commit_lock_path())?;
        self.reap_cache_tombstones()?;
        let build_cache = self.lw.join("build-cache");
        let size = dir_size_capped(&build_cache, cap);
        if size > cap {
            eprintln!(
                "ladder: shared build-cache is {size} bytes (> cap {cap}); evicting {} — \
                 the next build re-derives seeds and cold-climbs the affected closure",
                build_cache.display()
            );
            let tomb = self
                .lw
                .join(format!("build-cache.evicting.{}", process::id()));
            remove_path_if_exists(&tomb)?;
            // Atomic swap-aside then reap. Only a NotFound rename is benign (build_cache
            // vanished under us — nothing to evict); a real error (EBUSY/EACCES/EIO) must
            // surface, not be mistaken for "already gone" and silently skip the reclaim.
            match fs::rename(&build_cache, &tomb) {
                Ok(()) => remove_path_if_exists(&tomb).map_err(|e| {
                    format!(
                        "ladder: evicted the over-cap build-cache to {} but could not reclaim \
                         it: {e} — the cache name is free but the disk is NOT; refusing to \
                         proceed (a fresh cache would grow atop unreclaimed bytes). Remove {} \
                         to recover.",
                        tomb.display(),
                        tomb.display()
                    )
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "ladder: evict rename {} -> {}: {e}",
                        build_cache.display(),
                        tomb.display()
                    ))
                }
            }
        }
        Ok(())
    }

    /// Reap `build-cache.evicting.*` tombstones an interrupted eviction left behind. A
    /// tombstone holds unreclaimed disk the cap does not count, so a reap failure is NOT
    /// best-effort: it fails setup rather than let a fresh cache grow atop it. Runs under
    /// the ladder lock.
    fn reap_cache_tombstones(&self) -> Result<(), String> {
        let entries = match fs::read_dir(&self.lw) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        let mut first_err: Option<String> = None;
        for entry in entries.flatten() {
            let is_tomb = entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("build-cache.evicting."));
            if is_tomb {
                if let Err(e) = remove_path_if_exists(&entry.path()) {
                    first_err.get_or_insert_with(|| {
                        format!(
                            "ladder: could not reap stale build-cache tombstone {}: {e} — it \
                             holds unreclaimed disk the cap does not count; remove it to recover",
                            entry.path().display()
                        )
                    });
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Best-effort removal of ABANDONED per-pid scratch trees under `scratch/`. Each
    /// build-/check-run works in `scratch/<name>-<pid>` and never removes it on exit, so
    /// dead runs' trees pile up. Removes only trees whose CLAIM it can take, which is
    /// what makes it safe with peers holding the ladder SHARED: our own in-progress
    /// scratch and every live peer's are locked, so neither is ever a candidate, and
    /// that holds across pid namespaces where `/proc` did not. Never fails setup — any
    /// error leaves the tree for a later pass.
    fn reap_dead_scratch(&self) {
        let dir = match self.scratch.parent() {
            Some(d) => d,
            None => return,
        };
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if !reapable_scratch(&name) {
                continue;
            }
            // Liveness is the CLAIM, not the name: take the tree's lock or leave the
            // tree alone. A claim we can take is one whose owner is gone (the kernel
            // drops an flock however a process dies), including our own dead
            // predecessors. Our OWN tree is never a candidate — we are holding its
            // claim. The lock file itself is deliberately left behind; see
            // `claim_scratch`.
            let lock = match open_lock_file(&scratch_claim_lock(dir, &name)) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if matches!(lock.try_lock(), Ok(())) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    fn intern_source(&self, intern_name: &str, pin: &SourcePin) -> Result<String, String> {
        validate_source_file_basename(pin)?;
        let file = self.sources_dir.join(&pin.file);
        if !file.is_file() {
            return Err(format!(
                "ladder: pinned tarball not warm ({}) - run 'td-feed warm sources'",
                file.display()
            ));
        }
        verify_source_pin(&file, pin)?;
        self.store_add_recursive(intern_name, &file)
    }

    fn intern_linux_headers(&self, intern_name: &str, arch: &str) -> Result<String, String> {
        let pin = source_pin_for_key("linux-source")?;
        validate_source_file_basename(&pin)?;
        let version = linux_version_from_file(&pin.file)?;
        let file = self
            .sources_dir
            .join(format!("linux-headers-{version}-{arch}.tar"));
        if !file.is_file() {
            return Err(format!(
                "ladder: kernel-headers tarball not warm ({})",
                file.display()
            ));
        }
        self.store_add_recursive(intern_name, &file)
    }

    fn intern_patch(&self, intern_name: &str, patch: &str) -> Result<String, String> {
        let file = self
            .root
            .join("seed")
            .join("patches")
            .join(format!("{patch}.patch"));
        if !file.is_file() {
            return Err(format!("ladder: missing {}", file.display()));
        }
        self.store_add_recursive(intern_name, &file)
    }

    fn intern_stage0_source(&self, intern_name: &str) -> Result<String, String> {
        let tarball = self.stage0_source_tarball()?;
        let extract = self.scratch.join("stage0-source-extract");
        remove_path_if_exists(&extract)?;
        fs::create_dir_all(&extract).map_err(|e| format!("mkdir {}: {e}", extract.display()))?;
        let tar_s = path_str(&tarball)?;
        let extract_s = path_str(&extract)?;
        let mut cmd = self.builder_command();
        cmd.arg("tar-gz-extract").arg(tar_s).arg(extract_s);
        command_output(&mut cmd, "td-builder tar-gz-extract stage0 source")?;
        let stage0 = single_subdir_path(&extract)?;
        clean_stage0_build_dirs(&stage0)?;
        if !stage0
            .join("bootstrap-seeds/POSIX/AMD64/hex0-seed")
            .is_file()
            || !stage0.join("AMD64/mescc-tools-seed-kaem.kaem").is_file()
        {
            return Err(format!(
                "{} did not unpack to the expected stage0 source tree",
                tarball.display()
            ));
        }
        self.store_add_recursive(intern_name, &stage0)
    }

    /// Stage an IN-TREE source directory for hashing (#469 local-source
    /// provenance): resolve the repo-relative path and copy the committed source
    /// (minus `target`/`.git`) into scratch. The staged tree is what both the
    /// content-address computation and the intern read, so the address that is
    /// gated and the bytes that are interned cannot diverge.
    fn stage_local_source(&self, intern_name: &str, rel: &str) -> Result<PathBuf, String> {
        let dest = self.scratch.join(format!("local-source-{intern_name}"));
        stage_local_source_at(&self.root, intern_name, rel, &dest)
    }

    /// Intern an IN-TREE source directory: stage it, then content-address it into
    /// the seed store under `intern_name`. No fetch/verify: the bytes ARE the
    /// committed tree, and the compiled seed-digest table is what pins them. This
    /// is the GENERATOR's path (`seed-digests`, which is producing the table and so
    /// cannot be gated by it); the enforcing path is `ensure_local_source`.
    fn intern_local_source(&self, intern_name: &str, rel: &str) -> Result<String, String> {
        let staged = self.stage_local_source(intern_name, rel)?;
        self.store_add_recursive(intern_name, &staged)
    }

    /// Realize a local source under the compiled table's authority, in the ONE order
    /// that keeps a stale digest recoverable: hash, GATE, only then intern.
    ///
    /// Interning first would be self-defeating. The retained seed db is authenticated
    /// WHOLESALE (`authenticate_seed_db`) — one row on a basename the table does not
    /// pin makes every later `build-plan` red, for every target, including ones that
    /// never look at this source. So an intern that happens before the gate converts
    /// "this tree's digest is stale" into "the ladder is unusable", and the developer
    /// pays a full cold climb for a one-line table fix.
    fn ensure_local_source(&self, key: &str, rel: &str) -> Result<String, String> {
        let staged = self.stage_local_source(key, rel)?;
        let candidate = self.store_path_recursive(key, &staged)?;
        gate_local_source_candidate(key, &candidate)?;
        let derived = self.store_add_recursive(key, &staged)?;
        // The intern re-hashes the same staged tree, so this cannot disagree with the
        // gate above — assert it rather than assume it, since everything downstream
        // trusts that the interned basename is the one the table vouched for.
        if derived != candidate {
            return Err(format!(
                "local source `{key}': hashed {candidate} before interning but interned \
                 {derived} — the staged tree changed under the gate"
            ));
        }
        self.stage_store_path(&derived)?;
        Ok(derived)
    }

    fn stage0_source_tarball(&self) -> Result<PathBuf, String> {
        let pin = source_pin_for_key("stage0-source")?;
        validate_source_file_basename(&pin)?;
        let tarball = self.sources_dir.join(&pin.file);
        if !tarball.is_file() {
            return Err(format!(
                "ladder: pinned stage0 source not warm ({}) - run 'td-feed warm sources'",
                tarball.display()
            ));
        }
        verify_source_pin(&tarball, &pin)?;
        Ok(tarball)
    }

    /// The content-addressed store path `src` WOULD intern at — computed, nothing
    /// written. The seed store and db are untouched, so a caller may compare the
    /// address against the compiled table and walk away.
    fn store_path_recursive(&self, name: &str, src: &Path) -> Result<String, String> {
        store_path_recursive_with(&self.tb, &self.root, name, src)
    }

    /// This run's claimed scratch basename — unique among LIVE runs on this ladder,
    /// whatever pid namespace each is in. The identity anything shared must be keyed
    /// by. Falls back to the pid only for a directly-constructed test runner.
    fn scratch_id(&self) -> String {
        self.scratch
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| process::id().to_string())
    }

    /// Whether THIS pin table's seed db already vouches `derived`.
    ///
    /// The db is read at most once per run and the answer then kept UP TO DATE as this
    /// run registers. A read-only cache would be correct but slow where it matters: a
    /// run prepares several targets, `classify_graph_inputs` dedupes only within one
    /// graph, and on a table's first run every seed starts unvouched — so a seed shared
    /// by K targets would be re-hashed and re-merged K times, over trees the size of the
    /// linux, gcc and rust sources. That is the very latency this change is about.
    ///
    /// An unreadable or absent db vouches NOTHING, which registers rather than skips —
    /// the fail-closed direction — and so does a poisoned mutex, which is why neither is
    /// an error path.
    fn db_vouches(&self, derived: &str) -> bool {
        let Ok(mut guard) = self.vouched.lock() else {
            return false;
        };
        let set = guard.get_or_insert_with(|| {
            if !self.db.is_file() {
                return HashSet::new();
            }
            let Ok(db_s) = path_str(&self.db) else {
                return HashSet::new();
            };
            let mut cmd = self.builder_command();
            cmd.arg("store-query").arg(db_s).arg("info");
            match command_output(&mut cmd, "td-builder store-query") {
                Ok(out) => out
                    .lines()
                    .filter_map(|l| l.split('|').next())
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect(),
                Err(_) => HashSet::new(),
            }
        });
        set.contains(derived)
    }

    /// Record a path this run has just registered, so a later target sharing the seed
    /// does not re-hash it. Silent on a poisoned mutex: the cost is redundant work.
    fn record_vouched(&self, derived: &str) {
        if let Ok(mut guard) = self.vouched.lock() {
            if let Some(set) = guard.as_mut() {
                set.insert(derived.to_string());
            }
        }
    }

    fn store_add_recursive(&self, name: &str, src: &Path) -> Result<String, String> {
        let src_s = path_str(src)?;
        let store_s = path_str(&self.store)?;
        let db_s = path_str(&self.db)?;
        let mut cmd = self.builder_command();
        cmd.arg("store-add-recursive")
            .arg(name)
            .arg(src_s)
            .arg(store_s)
            .arg(db_s);
        let out = command_output(&mut cmd, &format!("store-add-recursive {name}"))
            .map_err(|e| with_seed_reset_hint(e, &self.lw))?;
        out.lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("store-add-recursive {name} produced no path"))
    }

    /// This TARGET's `--auto` seed map, written fresh each run under the PRIVATE scratch
    /// dir — never a shared or persistent file. One `NAME PATH` line per seed the target's
    /// graph declares, each a pin-verified content-addressed store path: the exact format
    /// `build-plan --auto` parses. Scoped by target so a `prepare_recipe_target(A)` +
    /// `build_plan(B)` mismatch reds on the missing map rather than silently planning B
    /// against A's seeds (build_plan's `is_file` guard).
    fn auto_map_path(&self, target: &str) -> PathBuf {
        self.scratch
            .join(format!("auto-map-{}", sanitize_target_for_filename(target)))
    }

    fn write_auto_map(&self, target: &str, entries: &[(String, String)]) -> Result<(), String> {
        let path = self.auto_map_path(target);
        fs::write(&path, serialize_auto_map(entries))
            .map_err(|e| format!("write {}: {e}", path.display()))
    }

    fn stage_store_path(&self, store_path: &str) -> Result<(), String> {
        let base = path_basename_str(store_path)?;
        let src = self.store.join(base);
        let dst = self.scratch.join("tdstore").join(base);
        if dst.exists() {
            return Ok(());
        }
        copy_tree(&src, &dst).map_err(|e| {
            format!(
                "ladder: stage {} into tdstore failed ({} -> {}): {e}",
                base,
                src.display(),
                dst.display()
            )
        })
    }

    fn emit_recipe_graph(&self, nodes: &[RecipeNode]) -> Result<(), String> {
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        for node in nodes {
            fs::write(
                self.recipes.join(format!("{}.json", node.stem)),
                node.recipe.to_json().to_canonical(),
            )
            .map_err(|e| format!("ladder: emit {}: {e}", node.stem))?;
        }
        Ok(())
    }

    pub(crate) fn prepare_recipe_target(&self, target: &str) -> Result<(), String> {
        let graph = recipe_closure(&[target])?;
        // ensure_graph_inputs re-derives, pin-verifies, interns, and STAGES every
        // seed in the current graph, and writes the fresh per-run auto-map from
        // exactly those verified paths — no persistent map is read or trusted.
        self.ensure_graph_inputs(target, &graph)?;
        self.emit_recipe_graph(&graph)
    }

    /// Classify then realize every input in the graph: `classify_graph_inputs`
    /// (the pure planning pass — see its doc for the #469 trust boundary), then
    /// intern and stage each admitted seed. The `--auto` seed map is written
    /// FRESH here from this run's re-derived, pin-verified paths — the map is
    /// per-invocation derived state, never a persisted authority.
    fn ensure_graph_inputs(&self, target: &str, nodes: &[RecipeNode]) -> Result<(), String> {
        let tdstore = self.scratch.join("tdstore");
        fs::create_dir_all(&tdstore)
            .map_err(|e| format!("mkdir {}: {e}", tdstore.display()))?;
        let mut entries: Vec<(String, String)> = Vec::new();
        for input in classify_graph_inputs(nodes)? {
            let derived = self.ensure_seed_input(&input)?;
            entries.push((input.key().to_string(), derived));
        }
        self.write_auto_map(target, &entries)
    }

    /// Realize one classified seed input to its content-addressed store path.
    ///
    /// WARM path: if the COMPILED seed-digest table pins this key to a basename ALREADY interned
    /// in the retained seed store, return it and skip the cold re-derive (re-fetch/re-extract/
    /// re-intern). The compiled table binds key->basename (a forged/renamed map is still rejected
    /// with no byte I/O), the intern committed ATOMICALLY (a present basename is a whole tree,
    /// never a half-written one — see store_add_recursive), and the builder CA-authenticates every
    /// interned seed once per `build-plan` (authenticate_seed_db) — so skipping the recipe-side
    /// re-derive costs no inline integrity. `verify-store` fscks the seed store as an additional
    /// out-of-band pass.
    ///
    /// KNOWN LIMITATION (#469 "the compiled table is the authority"): the warm short-circuit keys
    /// on the table's CURRENT basename, so bumping a seed's pin WITHOUT regenerating
    /// seed-digests.txt, on a store where the OLD basename is still interned, silently reuses the
    /// old bytes. The intended workflow bumps the pin AND regenerates the table in one commit —
    /// then the new basename is not yet interned and this falls through to the cold path (which
    /// re-derives from the pin and gates the fresh basename via `require`); `clear-store` also
    /// forces cold.
    ///
    /// A LOCAL source is exempt from the warm path entirely (the early return at the top of
    /// this function, before the table is consulted at all), because
    /// for it that limitation is not a mis-workflow but the ORDINARY case: its bytes are the
    /// working tree, so any edit — no pin to bump, no table row to notice — leaves the store
    /// holding a tree the table's basename no longer describes, and the warm hit would then
    /// build the old bytes on every warm machine while a cold one reds. Re-deriving is a tree
    /// copy plus a NAR hash of a small in-tree crate: no fetch, no extract, cheap enough to pay
    /// every run so the table is checked against what the checkout says NOW.
    ///
    /// COLD path (basename not yet interned): RE-DERIVE from the compiled pin — each
    /// intern_* verifies the pinned artifact and interns it into the seed store — then gate
    /// the derived basename against the compiled table before use.
    fn ensure_seed_input(&self, input: &SeedInput) -> Result<String, String> {
        if let SeedInput::LocalSource { key, path } = input {
            return self.ensure_local_source(key, path);
        }
        if let Some(base) = crate::seed_digests::expected(input.key())? {
            let tree = self.store.join(base);
            if tree.exists() {
                let derived = format!("{TD_STORE_DIR}/{base}");
                // A warm STORE is no longer evidence that this table's db has the row:
                // the store is shared across pin tables and the db is not, so on a
                // table's first run here every seed is already interned and none is
                // registered. Register from the interned tree — content-addressed, so
                // this re-hashes and merges the row without re-fetching or re-verifying
                // the pin, which the address already settled.
                if !self.db_vouches(&derived) {
                    // GATE FIRST, then intern — `ensure_local_source`'s rule, and for its
                    // reason. `store_add_recursive` merges the row as part of interning,
                    // so a tree that no longer content-addresses to its own name would
                    // land an unpinned row in this table's db; `authenticate_seed_db`
                    // judges that db WHOLESALE, so ONE such row reds every later
                    // build-plan for every target — and the prune that used to drop it
                    // is gone. Computing the address first keeps the failure to this
                    // seed, where it belongs.
                    let got = self.store_path_recursive(input.key(), &tree)?;
                    if got != derived {
                        return Err(format!(
                            "seed {} is interned at {base} but re-hashes to {got} — the \
                             store item does not content-address to its own name; remove \
                             it to re-intern (re #469)",
                            input.key()
                        ));
                    }
                    let added = self.store_add_recursive(input.key(), &tree)?;
                    if added != derived {
                        return Err(format!(
                            "seed {} registered as {added}, not the {derived} its own \
                             bytes address (re #469)",
                            input.key()
                        ));
                    }
                    self.record_vouched(&derived);
                }
                self.stage_store_path(&derived)?;
                return Ok(derived);
            }
        }
        let derived = self.derive_seed_input(input)?;
        // The COMPILED table must vouch for the derivation (re #469): pin
        // verification proves the fetched artifact, but a GENERATED seed (the
        // kernel-headers tarball) has no upstream pin — the compiled expected
        // digest is what binds its bytes; and every seed's expected basename
        // being compiled in is what lets td-builder reject a forged map even
        // when invoked directly.
        crate::seed_digests::require(input.key(), path_basename_str(&derived)?)?;
        self.stage_store_path(&derived)?;
        Ok(derived)
    }

    /// Derive ONE classified seed from its compiled pin — verify, intern, and
    /// return the content-addressed store path. Shared by the enforcement
    /// path (`ensure_seed_input`) and the table generator (`seed-digests`),
    /// so the printed table is produced by the exact derivation the runner
    /// later enforces.
    ///
    /// GENERATOR-ONLY for a local source: this arm interns UNGATED, which is
    /// correct for the command that is producing the table and wrong for anything
    /// else. `ensure_seed_input` therefore never reaches it — it dispatches local
    /// sources to `ensure_local_source` before this is called.
    fn derive_seed_input(&self, input: &SeedInput) -> Result<String, String> {
        match input {
            SeedInput::Stage0 { key } => self.intern_stage0_source(key),
            SeedInput::Source { key, pin } => self.intern_source(key, pin),
            SeedInput::LinuxHeaders { key, arch } => self.intern_linux_headers(key, arch),
            SeedInput::Patch { key, patch } => self.intern_patch(key, patch),
            SeedInput::LocalSource { key, path } => self.intern_local_source(key, path),
        }
    }

    pub(crate) fn build_plan(&self, target: &str) -> Result<PathBuf, String> {
        // The auto map is the FRESH per-run map prepare_recipe_target wrote from this
        // graph's re-derived, pin-verified seeds (every non-owned input is an interned
        // seed source). There is no tools map — a host executable is not an admissible
        // input, so build-plan's content-scan candidate dir is the ladder's OWN store of
        // interned seeds, never a host store.
        let auto_map = self.auto_map_path(target);
        if !auto_map.is_file() {
            return Err(format!(
                "ladder: {} missing — prepare_recipe_target({target}) must run before build_plan({target})",
                auto_map.display()
            ));
        }

        let home = path_str(&self.lw)?;
        let tmp = path_str(&self.lw)?;
        let builder_store = path_str(&self.builder_store)?;
        let builder_db = path_str(&self.builder_db)?;
        let recipes = path_str(&self.recipes)?;
        let auto_map_s = path_str(&auto_map)?;
        let scratch = path_str(&self.scratch)?;
        let root_s = path_str(&self.root)?;
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env_clear()
            .env("HOME", home)
            .env("TMPDIR", tmp)
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", builder_store)
            .env("TD_BUILDER_DB", builder_db)
            // Repo anchor for `--auto` rust-step crate vendoring: build_plan resolves a
            // rust recipe's committed Cargo.lock and its warm `.td-build-cache/crate-vendor`
            // tree under this root (re #547). Absent ⇒ no committed-lock vendoring.
            .env("TD_AUTO_REPO_ROOT", root_s);
        // The derived blessed-seed-db lookup keys on the REAL daemon dir; the
        // ladder HOME override above would otherwise re-point it at a dir
        // where nothing was blessed (re #469 round-8).
        if let Some(d) = &self.daemon_dir {
            cmd.env("TD_DAEMON_DIR", d);
        }
        // Pass TD_TIMING through the env_clear() barrier so a warm-run climb emits its
        // per-rung phase timings from the build-plan subprocess (diagnostic, opt-in).
        if let Ok(v) = std::env::var("TD_TIMING") {
            cmd.env("TD_TIMING", v);
        }
        cmd.arg("build-plan")
            .arg("--auto")
            .arg(target)
            .arg(recipes)
            .arg(auto_map_s)
            .arg(path_str(&self.store)?)
            .arg(path_str(&self.db)?)
            .arg(scratch);
        // Cross-run reuse is ALWAYS on (re #469 build speed): point the chain at the
        // DEDICATED build-output cache (build_cache_paths, under the ladder work dir), kept
        // SEPARATE from the seed store/db (self.store/self.db). Each UNCHANGED rung is reused
        // from a prior run (a NAR-verified persistent_realization hit, bit-identical to a
        // fresh build) instead of rebuilt, and a freshly-built rung commits its output back.
        // A CHANGED rung has a different drv ⇒ different output path ⇒ a miss ⇒ still
        // rebuilds, so the rung under development always rebuilds. The cache is SHARED across
        // worktrees and content-addressed, so a pin change is just a different-drv miss
        // (rebuild), never a wipe — divergent branches reuse each other's unchanged rungs.
        // The ONLY way to force a from-stage0 cold climb is the explicit `clear-store`, which
        // resets the whole ladder; nothing reclaims the cache implicitly except an opt-in
        // TD_CHECK_LADDER_CACHE_CAP_BYTES high-watermark eviction in setup().
        // Concurrent writers to the cache are the ordinary case now that build-runs hold
        // the ladder SHARED; what keeps them apart is the builder's own per-store commit
        // lock, not this caller. The builder commits each rung ATOMICALLY (stage into a
        // sibling temp, then rename — commit_canonical_atomic / commit_tree_checked), so a
        // kill mid-commit leaves only a swept temp, never a torn tree at the destination;
        // an unregistered mismatching orphan is removed and re-committed, never served. The
        // build-run reuse memo relies on this: a committed base (one that emitted a STEP
        // line) is complete and immutable, so its later presence is a sound reuse gate.
        //
        // The cache MUST NOT be self.store/self.db: those are the SEED store/db (interned
        // seed inputs), and #468 authenticates self.db as a seed-only authority — a recipe
        // OUTPUT committed there would be rejected as an unpinned seed. Keeping the cache a
        // distinct store/db pair keeps the seed authority clean and makes reuse compatible
        // with #468 (which then reuses through the same persistent_realization).
        let (cache_store, cache_db) = self.build_cache_paths();
        cmd.env("TD_PERSIST_STORE", path_str(&cache_store)?)
            .env("TD_PERSIST_DB", path_str(&cache_db)?);
        // Host-side human commands stream the build's stdout AND stderr live so a cold
        // ladder climb is not a silent multi-minute wait; `TD_RECIPE_QUIET` reverts to the
        // buffered path (output captured and shown on completion, not tee'd live), and gate
        // `check-run` never streams so its captured log stays byte-identical. All paths
        // return the same (status, stdout, stderr) triple, so the file/tail/scan below is shared.
        let stream = self.stream_progress && !quiet_requested();
        let (status, stdout_bytes, stderr_bytes) = if stream {
            spawn_capture_tee(&mut cmd)
                .map_err(|e| format!("build-plan --auto {target}: {e}"))?
        } else {
            let out = cmd
                .output()
                .map_err(|e| format!("spawn build-plan --auto {target}: {e}"))?;
            (out.status, out.stdout, out.stderr)
        };
        let out_file = self.scratch.join(format!("build-{target}.out"));
        let err_file = self.scratch.join(format!("build-{target}.err"));
        fs::write(&out_file, &stdout_bytes)
            .map_err(|e| format!("write {}: {e}", out_file.display()))?;
        fs::write(&err_file, &stderr_bytes)
            .map_err(|e| format!("write {}: {e}", err_file.display()))?;
        if !status.success() {
            let base = format!(
                "{}\nladder: build-plan --auto {target} failed",
                tail_bytes(&stderr_bytes, 40)
            );
            // Scan the FULL stderr bytes, not just the 40-line tail, for the retained-seed
            // markers — a long build log could scroll the auth red out of the tail. Byte-level
            // so a huge or non-UTF-8 log costs no lossy full-buffer allocation on the error path.
            let msg = if stale_seed_in(&stderr_bytes) {
                // Which hint is chosen reads the FULL stderr too: the unpinned-basename
                // marker is what distinguishes a prune from a ladder-destroying reset, and
                // it can sit well above the tail.
                let hint = seed_reset_hint(&self.lw, &String::from_utf8_lossy(&stderr_bytes));
                format!("{base}\n{hint}")
            } else {
                base
            };
            // Same rule the other child-runners apply: a host gap the child
            // reported must survive into the exit code, not flatten into prose.
            return Err(
                if td_engine::exit::child_reported_host_gap(
                    status.code(),
                    &stdout_bytes,
                    &stderr_bytes,
                ) {
                    format!("{HOST_GAP}{msg}")
                } else {
                    msg
                },
            );
        }
        // The streaming path already tee'd stdout live; only the buffered path flushes it here.
        if !stream {
            io::stdout()
                .write_all(&stdout_bytes)
                .map_err(|e| format!("write build-plan stdout: {e}"))?;
        }
        Ok(out_file)
    }

    pub(crate) fn ladder_out_from(&self, build_out: &Path, rung: &str) -> Result<PathBuf, String> {
        let prefix = format!("STEP {rung} ");
        let mut got = None;
        let contents = fs::read_to_string(build_out)
            .map_err(|e| format!("read {}: {e}", build_out.display()))?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix(&prefix) {
                got = Some(rest.trim().to_string());
            }
        }
        let path = got.ok_or_else(|| format!("ladder: no STEP output recorded for {rung}"))?;
        let base = path_basename_str(&path)?;
        Ok(self.scratch.join("tdstore").join(base))
    }

    /// Typed provenance databases written by every recipe step in BUILD_OUT.
    /// A product-level follow-up build (the `td shell` Rust-userland proof)
    /// consumes the already-built platform through these exact databases rather
    /// than reclassifying its store trees as seeds.
    pub(crate) fn recipe_output_dbs(&self, build_out: &Path) -> Result<Vec<PathBuf>, String> {
        let contents = fs::read_to_string(build_out)
            .map_err(|e| format!("read {}: {e}", build_out.display()))?;
        let mut dbs = Vec::new();
        let mut seen = HashSet::new();
        for line in contents.lines() {
            let Some(rest) = line.strip_prefix("STEP ") else {
                continue;
            };
            let name = rest
                .split_whitespace()
                .next()
                .ok_or_else(|| format!("malformed STEP line in {}: {line}", build_out.display()))?;
            let db = self.scratch.join(name).join("td.db");
            if !db.is_file() {
                return Err(format!(
                    "recipe step `{name}' has no output database at {}",
                    db.display()
                ));
            }
            if seen.insert(name.to_string()) {
                dbs.push(db);
            }
        }
        if dbs.is_empty() {
            return Err(format!(
                "build log {} recorded no recipe STEP outputs",
                build_out.display()
            ));
        }
        Ok(dbs)
    }

    pub(crate) fn tdstore_path(&self) -> PathBuf {
        self.scratch.join("tdstore")
    }

    pub(crate) fn product_scratch(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// Physical control-plane builder used to enter `store-ns`. It executes
    /// outside the namespace and is never copied into the target `/td/store`.
    pub(crate) fn control_builder_path(&self) -> &Path {
        &self.tb
    }

    /// The repo checkout this run reads recipes, patches, and committed locks from.
    pub(crate) fn repo_root(&self) -> &Path {
        &self.root
    }

    /// The shared warmed-source cache the pinned tarballs and generated
    /// kernel-header seeds are interned from.
    pub(crate) fn sources_dir(&self) -> &Path {
        &self.sources_dir
    }

    fn build_recipe_target(&self, target: &str, outputs: &[&str]) -> Result<(), String> {
        let staged = self.build_and_stage(target, outputs)?;
        println!("TD_RECIPE_RUN_WORK {}", self.lw.display());
        println!(
            "TD_RECIPE_RUN_TDSTORE {}",
            self.scratch.join("tdstore").display()
        );
        for (output, path) in outputs.iter().zip(staged.iter()) {
            println!("TD_RECIPE_RUN_OUT {output} {}", path.display());
        }
        Ok(())
    }

    /// Build TARGET's graph and stage the requested output rungs into this run's
    /// tdstore, returning each output's staged path in `outputs` order. Shared by
    /// `build-run` and the interactive `run`.
    ///
    /// CONTRACT: exactly the REQUESTED `outputs` are staged into tdstore — not the
    /// whole closure. A cold climb incidentally leaves every rung's output in
    /// tdstore as a byproduct of building; a warm hit stages only the requested
    /// roots. Every consumer depends only on the requested roots: `run` reads the
    /// self-contained `<system>/deployment` bundle, loop-userland reads the static
    /// busybox/make trees, and the CLI prints the paths. Recipe binaries reference
    /// absolute `/td/store`, never the per-invocation scratch tdstore, so the
    /// incidental cold closure is not a consumer contract; a caller that needs
    /// more outputs staged names them explicitly (`build-run TARGET A B ...`).
    ///
    /// TOP-LEVEL REUSE: the whole ladder is a pure function of the compiled
    /// evaluator + builder + seed patches + the closure's committed cargoLocks
    /// (`evaluator_fingerprint`), so a memo keyed on that fingerprint records each
    /// rung's content-addressed output basename. When the fingerprint is unchanged AND every requested output's
    /// durable build-cache tree is still present, this SKIPS seed-inputs,
    /// planning, and the whole climb — it only copies the recorded outputs into
    /// tdstore. Any recipe/pin/patch change rebuilds the evaluator or builder,
    /// re-keys the fingerprint, and misses (full rebuild); an evicted or
    /// `clear-store`d output also misses. Trust-store presence is the reuse gate
    /// (the same boundary the persistent-realization warm hit uses):
    /// `verify-store` fscks the bytes out of band, never inline on this hot path.
    pub(crate) fn build_and_stage(
        &self,
        target: &str,
        outputs: &[&str],
    ) -> Result<Vec<PathBuf>, String> {
        let fingerprint = self.evaluator_fingerprint(target)?;
        if let Some(staged) = self.reuse_build_run(target, &fingerprint, outputs)? {
            return Ok(staged);
        }
        {
            let _t = timed_phase("harness prepare/seed-inputs");
            self.prepare_recipe_target(target)?;
        }
        let build_out = self.build_plan(target)?;
        let steps = parse_step_map(
            &fs::read_to_string(&build_out)
                .map_err(|e| format!("read {}: {e}", build_out.display()))?,
        );
        // Record only AFTER a successful climb: every recorded base has a STEP
        // line, i.e. a completely committed output, so a later reuse can trust its
        // presence. RE-FINGERPRINT first and publish only if it is byte-identical
        // to the pre-build fingerprint: the plan inputs (evaluator/builder binaries,
        // patches, closure locks) are hashed before a minutes-long climb but read
        // DURING it, and the ladder lock does not serialize working-tree edits or a
        // concurrent `cargo build` replacing the binary. If an input changed under
        // us the output no longer matches `fingerprint`, so skip the memo rather
        // than map a fingerprint to bytes built from different inputs; a later run
        // recomputes and rebuilds. (A single edit-then-revert WITHIN one build is
        // out of scope — the base build's own output would be equally mixed.)
        // All of this is best-effort — nothing here fails the completed build.
        match self.evaluator_fingerprint(target) {
            Ok(after) if after == fingerprint => {
                if let Err(e) = self.write_build_run_memo(target, &fingerprint, &steps) {
                    eprintln!("ladder: build-run reuse memo not recorded (non-fatal): {e}");
                }
            }
            Ok(_) => eprintln!(
                "ladder: plan inputs changed during the build; reuse memo not recorded for {target}"
            ),
            Err(e) => {
                eprintln!("ladder: reuse memo not recorded (re-fingerprint failed, non-fatal): {e}")
            }
        }
        outputs
            .iter()
            .map(|o| self.ladder_out_from(&build_out, o))
            .collect()
    }

    /// The compiled-plan fingerprint for TARGET: sha256 over the running evaluator
    /// binary, the staged builder binary, every `seed/patches/*.patch`, and every
    /// committed `Cargo.lock` a rung in TARGET's closure vendors from. Mirrors the
    /// loop-userland fingerprint (check_loop.rs), extended with the builder binary
    /// (it equally determines the output bytes a reuse skips) and the closure's
    /// cargoLocks (the one build input read from the repo, not compiled in — a lock
    /// bump changes a rust rung's output but no binary, so it MUST re-key here), and
    /// every `local_source` dir a rung in the closure interns (#469: an editable
    /// in-tree source, likewise repo-read and not compiled in, so an edit MUST
    /// re-key). Closure-scoped so an unrelated recipe's lock or source never
    /// invalidates this target.
    fn evaluator_fingerprint(&self, target: &str) -> Result<String, String> {
        let eval = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let closure = recipe_closure(&[target])?;
        let mut locks: Vec<String> = closure
            .iter()
            .filter_map(|n| n.recipe.cargo_lock.clone())
            .collect();
        locks.sort();
        locks.dedup();
        let mut local_sources: Vec<String> = closure
            .iter()
            .filter_map(|n| n.recipe.local_source.clone())
            .collect();
        local_sources.sort();
        local_sources.dedup();
        plan_fingerprint(
            &eval,
            &self.tb,
            &self.root.join("seed/patches"),
            &self.root,
            &locks,
            &local_sources,
        )
    }

    /// A memo HIT: the recorded plan for `fingerprint` is present and every
    /// requested output's durable build-cache tree still exists. COPY each into
    /// this run's tdstore (an INDEPENDENT inode — the operator-exposed tdstore
    /// output must never alias the durable cache, so a modified output cannot
    /// poison it) and return the staged paths. Any miss (no memo, an output not
    /// in it, or an evicted tree) returns None so the caller does a full build.
    ///
    /// Only the REQUESTED roots are staged, not the whole planned closure the
    /// full-build path stages — sound because the callers read only those roots
    /// (`build-run` prints their paths; `run` verifies and consumes the
    /// `<system>/deployment` bundle), and each root output is a self-contained
    /// tree. Two passes: validate ALL outputs first (so a partial eviction is a
    /// clean miss with no half-staged tdstore and no wasted copy), then stage.
    ///
    /// The stage is `copy_tree` (`fs::copy` per file), reflink-cheap on a CoW
    /// filesystem (btrfs/xfs) but a full byte copy on ext4 — the builder's
    /// FICLONE fast path lives behind its `unsafe` syscall layer, unreachable from
    /// this `unsafe`-forbidden crate — so a warm hit still skips the entire ladder
    /// climb, but the final copy of the requested roots is image-sized off CoW.
    fn reuse_build_run(
        &self,
        target: &str,
        fingerprint: &str,
        outputs: &[&str],
    ) -> Result<Option<Vec<PathBuf>>, String> {
        let map = match self.read_build_run_memo(target, fingerprint) {
            Some(m) => m,
            None => return Ok(None),
        };
        let (cache_store, _cache_db) = self.build_cache_paths();
        // Pass 1: resolve every requested output to a present durable cache tree.
        // Any gap (not memoized, or its tree evicted) is a MISS decided with NO
        // staging side effect.
        let mut sources: Vec<(String, PathBuf)> = Vec::with_capacity(outputs.len());
        for output in outputs {
            let base = match map.get(*output) {
                Some(b) => b,
                None => return Ok(None),
            };
            let src = cache_store.join(base);
            // Presence of a REAL DIRECTORY is a sound "complete tree" proxy because
            // the builder prints a rung's `STEP` line only AFTER its durable
            // persist-cache commit finishes, and a committed base is
            // content-addressed and hence immutable — so a memoized base was fully
            // committed and its bytes can only be removed wholesale (eviction /
            // clear-store under the ladder lock), never torn in place. Use
            // `symlink_metadata` (lstat, does NOT follow) not `Path::is_dir` (which
            // follows): a symlink squatting the basename must be REJECTED, not
            // followed — else `copy_tree` would recreate the link and stage an
            // alias to an out-of-cache tree, breaking confinement and copy-not-
            // alias. A stray file is rejected too. `verify-store` fscks the bytes
            // out of band, so this hot path never re-hashes.
            let is_real_dir = fs::symlink_metadata(&src)
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false);
            if !is_real_dir {
                return Ok(None);
            }
            sources.push((base.clone(), src));
        }
        // Pass 2: stage each as an INDEPENDENT copy (never a hardlink/symlink).
        // Copy into a temp then rename, so a kill mid-copy never leaves a torn tree
        // a later read would trust as complete. The temp name is static (not
        // pid-tagged): staging serializes under the ladder lock, so there is no
        // concurrent writer, and a static name lets `remove_path_if_exists` reap a
        // crashed run's orphan on the next run instead of leaking one per dead pid.
        let tdstore = self.scratch.join("tdstore");
        fs::create_dir_all(&tdstore).map_err(|e| format!("mkdir {}: {e}", tdstore.display()))?;
        let mut staged = Vec::with_capacity(sources.len());
        for (base, src) in &sources {
            let dst = tdstore.join(base);
            if !dst.exists() {
                let tmp = tdstore.join(format!(".{base}.tmp"));
                remove_path_if_exists(&tmp)?;
                copy_tree(src, &tmp).map_err(|e| {
                    format!(
                        "ladder: reuse-stage {} ({} -> {}): {e}",
                        base,
                        src.display(),
                        tmp.display()
                    )
                })?;
                fs::rename(&tmp, &dst).map_err(|e| {
                    format!(
                        "ladder: reuse-stage rename {} -> {}: {e}",
                        tmp.display(),
                        dst.display()
                    )
                })?;
            }
            staged.push(dst);
        }
        eprintln!(
            "   [reuse] {target} unchanged (plan {}): skipped seed-inputs, planning, and the ladder climb",
            fingerprint.get(..12).unwrap_or(fingerprint)
        );
        Ok(Some(staged))
    }

    fn build_run_memo_path(&self, target: &str, fingerprint: &str) -> PathBuf {
        self.lw.join("build-run-memo").join(format!(
            "{}.{fingerprint}.map",
            sanitize_target_for_filename(target)
        ))
    }

    fn read_build_run_memo(
        &self,
        target: &str,
        fingerprint: &str,
    ) -> Option<BTreeMap<String, String>> {
        let text = fs::read_to_string(self.build_run_memo_path(target, fingerprint)).ok()?;
        parse_build_run_memo(&text, fingerprint)
    }

    fn write_build_run_memo(
        &self,
        target: &str,
        fingerprint: &str,
        steps: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let dir = self.lw.join("build-run-memo");
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let path = self.build_run_memo_path(target, fingerprint);
        // Atomic publish: temp then rename, so a kill mid-write never leaves a torn
        // memo a later run would half-read. The temp is qualified by fingerprint AND
        // pid because build-runs no longer serialize under the ladder lock — they
        // hold it SHARED. A temp named per target alone is one name two live runs
        // both unlink, write and rename: the loser's rename hits ENOENT, and the
        // winner can publish the OTHER's bytes under its own fingerprint, which
        // `parse_build_run_memo` then rejects on read and turns into a full climb.
        // The orphan a crash leaves is inert exactly as a stale `.map` is (its name
        // is never looked up), and is cleaned by the same wholesale `clear-store`.
        //
        // The published `.map` is per fingerprint and is NOT reaped: `lw` (hence
        // this dir) is shared across all worktrees, whose distinct evaluator
        // binaries fingerprint differently, so deleting other-fingerprint maps
        // would clobber a concurrent worktree's live memo. Per-fingerprint maps sit
        // side by side (mirroring the loop-userland map, check_loop.rs); a stale one
        // is inert (its fingerprint never matches, so it is never read) and is
        // cleaned only wholesale by `clear-store`.
        // Qualified by this run's CLAIMED SCRATCH NAME, not by its pid: the pid is
        // namespace-local and two sandboxed runs share one (see `claim_scratch`),
        // which would put them back on a single temp name. The claim makes the
        // scratch name unique among live runs by construction, so it is the only
        // name here that distinguishes them.
        let tmp = dir.join(build_run_memo_temp_name(
            target,
            fingerprint,
            &self.scratch_id(),
        ));
        remove_path_if_exists(&tmp)?;
        fs::write(&tmp, serialize_build_run_memo(fingerprint, steps))
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))
    }

    pub(crate) fn store_ns_output(
        &self,
        argv: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, String> {
        let store_path = self.scratch.join("tdstore");
        let store = path_str(&store_path)?;
        let mut cmd = self.builder_command();
        cmd.arg("store-ns").arg(store).arg("--");
        for arg in argv {
            cmd.arg(arg);
        }
        match stdin {
            Some(input) => command_output_with_stdin(&mut cmd, "store-ns", input),
            None => command_output(&mut cmd, "store-ns"),
        }
    }

    pub(crate) fn builder_command(&self) -> Command {
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", &self.builder_store)
            .env("TD_BUILDER_DB", &self.builder_db);
        cmd
    }

    /// A host-environment-free control-plane command for product proofs. The
    /// explicit builder provenance and daemon directory are the only inherited
    /// authorities; package builds add their complete environment themselves.
    pub(crate) fn clean_builder_command(&self) -> Command {
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env_clear()
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", &self.builder_store)
            .env("TD_BUILDER_DB", &self.builder_db);
        if let Some(daemon_dir) = &self.daemon_dir {
            cmd.env("TD_DAEMON_DIR", daemon_dir);
        }
        cmd
    }
}

pub(crate) fn recipe_closure(targets: &[&str]) -> Result<Vec<RecipeNode>, String> {
    let mut visiting = HashSet::new();
    let mut emitted = HashSet::new();
    let mut out = Vec::new();
    for target in targets {
        visit_recipe(target, &mut visiting, &mut emitted, &mut out)?;
    }
    Ok(out)
}

fn visit_recipe(
    stem: &str,
    visiting: &mut HashSet<String>,
    emitted: &mut HashSet<String>,
    out: &mut Vec<RecipeNode>,
) -> Result<(), String> {
    if emitted.contains(stem) {
        return Ok(());
    }
    if !visiting.insert(stem.to_string()) {
        return Err(format!("ladder: cycle in recipe nativeInputs at `{stem}'"));
    }
    let recipe =
        catalog::lookup(stem).ok_or_else(|| format!("ladder: no td recipe for `{stem}'"))?;
    if let Some(native_inputs) = &recipe.native_inputs {
        for dep in native_inputs {
            if catalog::lookup(dep).is_some() {
                visit_recipe(dep, visiting, emitted, out)?;
            }
        }
    }
    if let Some(inputs) = &recipe.inputs {
        for dep in inputs {
            if catalog::lookup(dep).is_some() {
                visit_recipe(dep, visiting, emitted, out)?;
            }
        }
    }
    // The DATA channel is followed too. A payload is not a tool, but it is still a
    // recipe that has to be BUILT before the one that stages it — omitting it here
    // would leave an image declaring a payload emitting no recipe for it, and
    // `build-plan --auto` failing to resolve a name the catalog plainly has.
    if let Some(payload_inputs) = &recipe.payload_inputs {
        for dep in payload_inputs {
            if catalog::lookup(dep).is_some() {
                visit_recipe(dep, visiting, emitted, out)?;
            }
        }
    }
    visiting.remove(stem);
    emitted.insert(stem.to_string());
    out.push(RecipeNode {
        stem: stem.to_string(),
        recipe,
    });
    Ok(())
}

fn push_seed_input(inputs: &mut Vec<SeedInput>, seen: &mut HashSet<String>, input: SeedInput) {
    if seen.insert(input.key().to_string()) {
        inputs.push(input);
    }
}

/// The PURE planning pass (issue #469's trust boundary): classify every input
/// of every node into exactly TWO admissible provenances —
///
///   - **RecipeOutput** — the input names another td recipe in the catalog,
///     realized by an earlier plan step;
///   - **AuditedSeed** — the input names a pinned, hash-verified source /
///     seed patch / stage0 artifact, interned into the ladder store by td's
///     own addToStore.
///
/// ANYTHING else is rejected HERE, during planning — there is no host-tool
/// class, no lock of store paths, no PATH lookup, and no store discovery.
/// Every declaration channel is classified: `sourceInput`, `inputs`,
/// `nativeInputs` AND `payload_inputs`. The last is a DATA channel
/// (APPLICATIONS.md §B.8) and what travels it is restricted elsewhere — but it
/// is still a declared dependency, so it is classified here like any other. A
/// channel that skipped this would be one whose paths reach a build with no
/// provenance class at all, which is precisely the ingress #469 exists to stop.
/// A rung that declares scaffolding the chain has not built
/// (bash, coreutils, make, …) fails closed with `PROVENANCE_REJECTED` until
/// that tool exists as a recipe output. Deliberately pure — no subprocess, no
/// filesystem — so the entry points run it BEFORE any ambient execution
/// (stage0 placement, interning): a rejected graph executes NOTHING.
pub(crate) fn classify_graph_inputs(nodes: &[RecipeNode]) -> Result<Vec<SeedInput>, String> {
    let mut seen = HashSet::new();
    let mut seed_inputs = Vec::new();
    for node in nodes {
        if let Some(key) = &node.recipe.source_input {
            let input = seed_input_for_recipe_source(key, &node.recipe)?;
            push_seed_input(&mut seed_inputs, &mut seen, input);
        }
        for input in node
            .recipe
            .inputs
            .iter()
            .chain(node.recipe.native_inputs.iter())
            .chain(node.recipe.payload_inputs.iter())
            .flatten()
        {
            if catalog::lookup(input).is_some() {
                continue;
            }
            match seed_input_for_recipe_input(input)? {
                Some(seed_input) => push_seed_input(&mut seed_inputs, &mut seen, seed_input),
                None => return Err(provenance_rejection(&node.stem, input)),
            }
        }
    }
    Ok(seed_inputs)
}

fn seed_input_for_recipe_source(key: &str, recipe: &Recipe) -> Result<SeedInput, String> {
    // A local-source recipe supplies its OWN bytes (an in-tree dir), so it is
    // classified from `recipe.local_source` — never a fetch pin or a special
    // seed. This must win first: its `<name>-source` key has no pin to resolve.
    if let Some(path) = &recipe.local_source {
        return Ok(SeedInput::LocalSource {
            key: key.to_string(),
            path: path.clone(),
        });
    }
    match special_seed_input(key)? {
        Some(input) => Ok(input),
        None => {
            let pin = source_pin_for_key(key).map_err(|e| {
                format!(
                    "ladder: cannot resolve sourceInput `{key}' for {}-{} to a recipe source \
                     pin: {e}",
                    recipe.name, recipe.version
                )
            })?;
            Ok(SeedInput::Source {
                key: key.to_string(),
                pin,
            })
        }
    }
}

fn seed_input_for_recipe_input(key: &str) -> Result<Option<SeedInput>, String> {
    if let Some(input) = special_seed_input(key)? {
        return Ok(Some(input));
    }
    Ok(source_pins::by_key(key).map(|pin| SeedInput::Source {
        key: key.to_string(),
        pin,
    }))
}

/// Planning-only provenance gate over TARGETS' full recipe closures — the
/// FIRST act of `check-run` and `build-run`, before the runner exists and
/// before ANY subprocess (stage0 placement, source interning, builds): a
/// graph with a forbidden input reds here and nothing ambient ever executes
/// for it (re #469).
fn ensure_targets_provenance(targets: &[&str]) -> Result<(), String> {
    let graph = recipe_closure(targets)?;
    classify_graph_inputs(&graph).map(|_| ())
}

/// Fetch what TARGETS' closures declare and no cache holds, before the ladder
/// lock — the prep an operator would otherwise be told to run by hand.
///
/// What keeps this off every gate is that no gate invokes `run`/`qemu-boot*` at
/// all: they need a host qemu and a terminal the sandbox does not have, which is
/// why they are commands and not checks. The stdin condition is a second belt —
/// nothing nulls stdin for a gate subprocess, so a future gate shelling out to
/// one of these from an operator's terminal would still warm; it would also be a
/// gate reaching for a host-side command, which is the thing to notice.
///
/// A warm that could not happen is reported and then ignored: these callers came
/// to BUILD, the cold-input errors downstream are precise, and a memo hit needs
/// no inputs at all.
fn warm_operator_inputs(runner: &RecipeCheckRunner, targets: &[&str]) {
    if io::stdin().is_terminal() {
        if let Err(e) = crate::warm::preflight(runner, targets) {
            eprintln!("   [warm] {e} — continuing; the build reports what it cannot resolve");
        }
    }
}

fn special_seed_input(key: &str) -> Result<Option<SeedInput>, String> {
    if key == "stage0-source" {
        return Ok(Some(SeedInput::Stage0 {
            key: key.to_string(),
        }));
    }
    if key == "linux-headers" {
        return Ok(Some(SeedInput::LinuxHeaders {
            key: key.to_string(),
            arch: "i386",
        }));
    }
    if key == "linux-headers-x86-64" {
        return Ok(Some(SeedInput::LinuxHeaders {
            key: key.to_string(),
            arch: "x86_64",
        }));
    }
    if let Some(patch) = key.strip_prefix("patch-") {
        if patch.is_empty() {
            return Err(format!("ladder: malformed patch input `{key}'"));
        }
        // A pinned source whose key happens to start with `patch-` (the GNU
        // patch program's own `patch-mesboot-source`) is a Source, not a
        // seed/patches/*.patch file — the pin table wins over the prefix
        // convention. Every run hits this (seeds re-derive from their pins
        // each run; there is no map short-circuit): the misclassification
        // fails the whole chain on intern_patch's missing-file check.
        if source_pins::by_key(key).is_none() {
            return Ok(Some(SeedInput::Patch {
                key: key.to_string(),
                patch: patch.to_string(),
            }));
        }
    }
    Ok(None)
}

pub(crate) fn source_pin_for_key(key: &str) -> Result<SourcePin, String> {
    source_pins::by_key(key).ok_or_else(|| format!("no recipe source pin for `{key}'"))
}

fn validate_source_file_basename(pin: &SourcePin) -> Result<(), String> {
    if pin.file.is_empty() || pin.file.contains('/') {
        return Err(format!(
            "recipe source pin `{}` has non-basename file `{}`",
            pin.key, pin.file
        ));
    }
    Ok(())
}

fn verify_source_pin(path: &Path, pin: &SourcePin) -> Result<(), String> {
    let mut bytes = Vec::new();
    append_file_bytes(path, &mut bytes)?;
    let got = sha256sum(&bytes);
    if got != pin.sha256 {
        return Err(format!(
            "{} sha256 {got} != recipe source pin {}",
            path.display(),
            pin.sha256
        ));
    }
    Ok(())
}

/// Serialize the `--auto` seed map: one `NAME PATH` line per entry, in the order
/// the graph classified them. The keys are compiled seed constants and the paths
/// are content-addressed store paths, so neither carries the space separator that
/// `build-plan --auto` splits each line on.
fn serialize_auto_map(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, store_path) in entries {
        out.push_str(key);
        out.push(' ');
        out.push_str(store_path);
        out.push('\n');
    }
    out
}

/// A filesystem-safe rendering of a recipe target for the per-invocation auto-map
/// filename. Recipe stems are already simple (`[a-z0-9-]`), but map any other byte to `_`
/// so the target can never traverse out of the scratch dir or inject a path separator.
fn sanitize_target_for_filename(target: &str) -> String {
    target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Recursive byte size of `path`, short-circuiting as soon as it exceeds `cap`
/// (so the common under-cap walk is the only full traversal, and an over-cap tree
/// stops early). Uses `symlink_metadata`, so a symlink counts as its own small
/// entry rather than being followed — bounded and cycle-free. Unreadable entries
/// are skipped (best-effort disk accounting, never an error).
fn dir_size_capped(path: &Path, cap: u64) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.path().symlink_metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
                if total > cap {
                    return total;
                }
            }
        }
    }
    total
}

/// High-watermark byte cap for the shared build-output cache, ONLY when the operator sets a
/// positive `TD_CHECK_LADDER_CACHE_CAP_BYTES`. Unset/zero/garbage ⇒ `None` ⇒ setup() reclaims
/// nothing: the ladder is reset only by the explicit `clear-store`, and a rare over-cap
/// eviction is opt-in for operators who want bounded auto-reclaim (an implicit default-cap
/// eviction would itself be a surprise cold-climb). The `TD_CHECK_` prefix is load-bearing —
/// the `td-builder check` sandbox forwards only `TD_CHECK_*` / `TD_SUBST_*` / `TD_DAEMON_*`,
/// so a bare `TD_LADDER_…` name would be stripped before it reached the in-sandbox runner.
fn explicit_ladder_cache_cap() -> Option<u64> {
    parse_cache_cap(env::var("TD_CHECK_LADDER_CACHE_CAP_BYTES").ok().as_deref())
}

fn parse_cache_cap(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

fn find_td_builder_self(root: &Path) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("TD_BUILDER_SELF").map(PathBuf::from) {
        if is_executable(&path) {
            return Ok(path);
        }
        return Err(format!(
            "TD_BUILDER_SELF is not executable: {}",
            path.display()
        ));
    }
    // Workspace member: the build output lands in the workspace-root target dir.
    let release = root.join("target/release/td-builder");
    if is_executable(&release) {
        return Ok(release);
    }
    // Nothing built yet — do the host `cargo build` rather than print it as an
    // instruction: this is the same host-brings-cargo seed the stage0 provision
    // uses, and the same fallback check_loop::resolve_recipe_eval_bin already does
    // for this binary's sibling. A host WITHOUT cargo still gets the manual line,
    // and the probe comes FIRST so it is not preceded by a "building it" notice
    // for a build that never starts.
    let Some(cargo) = cargo_on_path(env::var_os("PATH").as_deref()) else {
        return Err(format!(
            "TD_BUILDER_SELF is unset, {} is not executable, and this host has no cargo to build \
             it with; run `cargo build --release --manifest-path builder/Cargo.toml`",
            release.display()
        ));
    };
    eprintln!(
        "td-recipe-eval: td-builder is not built yet; building it \
         (cargo build --release --manifest-path builder/Cargo.toml)"
    );
    build_td_builder(&cargo, root, &release)
}

/// The first executable `cargo` on `path`, house style rather than letting
/// `Command::new("cargo")` search implicitly — the caller needs to distinguish "no
/// cargo here" from "cargo failed" BEFORE it announces a build. The PATH is an
/// argument so the absent-cargo branch is testable without mutating process env
/// the same shape `stage0::rustup_add_musl_target` uses.
fn cargo_on_path(path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path?)
        // An EMPTY component means the cwd, and `PathBuf::new().join("cargo")` is the
        // relative `cargo` — which `Command::…current_dir(root)` would then resolve
        // against `root`, i.e. a different file than the one probed here. Dropped, as
        // `stage0::find_in_path` drops them.
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("cargo"))
        .find(|c| is_executable(c))
}

/// Prove the built binary runs on THIS host before handing it back: the caller execs
/// it immediately, and the executable BIT says nothing about architecture or ABI. A
/// `build.target` or `CARGO_BUILD_TARGET` naming another triple cross-builds happily,
/// and without this the failure surfaces two calls later as `Exec format error`
/// against a path nobody suspects. td-builder's bare invocation is its own sentinel
/// and exits 0.
fn runs_here(exe: &Path) -> Result<(), String> {
    match Command::new(exe).stdin(Stdio::null()).output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "{} was built but does not run here ({}) — a cross-target `build.target` or \
             CARGO_BUILD_TARGET would produce a binary for another host",
            exe.display(),
            out.status
                .code()
                .map_or_else(|| "killed by a signal".to_string(), |c| format!("exit {c}"))
        )),
        Err(e) => Err(format!("{} was built but does not run here: {e}", exe.display())),
    }
}

/// The binary cargo says it produced, read out of its own JSON artifact stream.
/// Asking is the only reliable way: `--target-dir` fixes the target ROOT, but an
/// ambient `CARGO_BUILD_TARGET` or a `build.target` in `.cargo/config.toml` moves the
/// artifact to `target/<triple>/release/`, and a path guess would report a successful
/// build as missing.
fn cargo_reported_executable(stdout: &str, bin: &str) -> Option<PathBuf> {
    stdout.lines().rev().find_map(|line| {
        let msg = td_engine::json::parse(line).ok()?;
        if msg.get("reason")?.as_str()? != "compiler-artifact" {
            return None;
        }
        if msg.get("target")?.get("name")?.as_str()? != bin {
            return None;
        }
        // `executable` is null for a non-binary artifact; `as_str` filters those.
        msg.get("executable")?.as_str().map(PathBuf::from)
    })
}

/// `cargo build --release` of builder/. Split out of [`find_td_builder_self`] so the
/// outcomes are testable against a fake cargo: built, exited 0
/// without producing a binary, and failed.
///
/// The nested build assumes the `cargo run` entry point, which `exec`-replaces itself
/// on Unix and so has RELEASED the build-directory lock before td-recipe-eval starts.
/// Reaching here from under `cargo test` — which holds that lock for the whole run —
/// would block on the parent that is waiting for this child; no
/// current caller does, and the fake-cargo tests below drive this function directly
/// rather than through a real cargo. Two concurrent check legs both finding
/// `target/release/td-builder` absent is the residual case: the second waits on
/// cargo's build lock, which is bounded and announces itself on the inherited stderr.
fn build_td_builder(cargo: &Path, root: &Path, release: &Path) -> Result<PathBuf, String> {
    // `--target-dir` is PINNED at the dir the caller looks in: an ambient
    // CARGO_TARGET_DIR would otherwise land the binary elsewhere.
    // `--locked` because gate 325 pins this workspace's Cargo.lock at exactly three
    // packages, and nothing reached from a check may rewrite it.
    // `json-render-diagnostics` keeps cargo's human-readable errors on the inherited
    // stderr while the machine-readable artifact list comes back on stdout.
    let out = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--locked",
            "--message-format",
            "json-render-diagnostics",
            "--manifest-path",
        ])
        .arg(root.join("builder/Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target"))
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn {} build (builder): {e}", cargo.display()))?;
    if !out.status.success() {
        // Deliberately NOT asserting a compile error: a read-only tree, a wrong
        // root, a cargo without a matching rustc and a full disk all land here too
        // Report what happened and let the operator read cargo's
        // own diagnostics, which went straight to the inherited stderr.
        return Err(format!(
            "`cargo build --release --locked --manifest-path builder/Cargo.toml` exited with {} \
             (its diagnostics are on stderr above)",
            out.status
                .code()
                .map_or_else(|| "a signal".to_string(), |c| format!("status {c}"))
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let named = cargo_reported_executable(&stdout, "td-builder");
    if let Some(exe) = &named {
        if is_executable(exe) {
            return runs_here(exe).map(|()| exe.clone());
        }
    }
    // Fall back to the conventional path: a cargo too old to report artifacts still
    // built something, and the caller's location is where it would be.
    if is_executable(release) {
        return runs_here(release).map(|()| release.to_path_buf());
    }
    // Say which of the two it was; "cargo named none" is a different diagnosis from
    // "cargo named a path that is not executable" and sends the reader elsewhere.
    let named = match &named {
        Some(exe) => format!("cargo named {}, which is not executable", exe.display()),
        None => "cargo named no executable".to_string(),
    };
    Err(format!(
        "`cargo build --release --locked --manifest-path builder/Cargo.toml` succeeded but no \
         td-builder executable resulted ({named}, and {} is not executable)",
        release.display()
    ))
}

fn place_stage0_builder(
    root: &Path,
    base: &Path,
    td_builder_self: &Path,
) -> Result<String, String> {
    fs::create_dir_all(base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
    // `td-builder stage0-place` — the one stage0 entry point (the placement
    // logic lives in builder/src/stage0.rs; no ambient host sh, re #469).
    let mut cmd = Command::new(td_builder_self);
    cmd.current_dir(root).arg("stage0-place").arg(base);
    let out = command_output(&mut cmd, "td-builder stage0-place")?;
    out.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "stage0-builder produced no output".to_string())
}

/// The recovery line for a stale/torn retained seed. setup() no longer wipes the seed
/// store/db, so these red here instead of self-healing. There is one shared ladder, so a
/// bare `clear-store` always targets it; the path is shown for the operator's confirmation.
///
/// Two very different failures used to share one line, and the shared line said
/// `clear-store'. For a TORN seed that is right — the bytes are unusable and must be
/// re-derived. For an UNPINNED one it is actively destructive: `clear-store' discards the
/// whole shared ladder — build cache included, which is hours of other people's rungs —
/// and lands on the identical red, because throwing bytes away cannot correct a table.
fn seed_reset_hint(lw: &Path, err: &str) -> String {
    if unpinned_seed_in(err.as_bytes()) {
        return format!(
            "hint: the seed db for this pin table vouches an item the table does not pin. \
             The db is keyed by the table's row set, so a peer branch's seeds cannot land in \
             it and a pin change starts a fresh one — which leaves a stale TABLE as the \
             remaining cause: regenerate it (`td-recipe-eval seed-digests', or \
             `local-source-digests' for an in-tree source). Do NOT `clear-store' the ladder \
             ({}): it discards the whole shared build cache and cannot correct a table.",
            lw.display()
        );
    }
    format!(
        "hint: the ladder's retained seed store/db is torn or corrupt (an interrupted intern, \
         or bytes changed under a registration). Run `td-recipe-eval clear-store` to reset the \
         ladder ({}) and re-derive seeds from the compiled pins.",
        lw.display()
    )
}

/// The one stale-seed marker `clear-store` does not fix: a db row on a basename the
/// compiled table has no row for. Keying the db per table makes it unreachable from a
/// peer branch; it survives as a detector because a stale TABLE still reaches it.
fn unpinned_seed_in(bytes: &[u8]) -> bool {
    contains_subslice(bytes, b"is not a basename the compiled seed-digest table pins")
}

/// A retained-seed failure marker — a plan-seed-db authentication red
/// (`authenticate_seed_db`/`authenticate_ca_db`: a pinned-seed change), a corrupt
/// content-addressed seed item
/// (`store-add-recursive`'s idempotent re-intern rejecting a torn tree), or an `--auto`
/// provenance red (`auto_seed_provenance`: a retained seed gone missing or content-address
/// mismatched). All three clear with the same `clear-store` re-derive-from-pins reset.
fn looks_like_stale_seed(text: &str) -> bool {
    stale_seed_in(text.as_bytes())
}

/// Byte-level marker scan — used directly on a (possibly large, possibly non-UTF-8) build-plan
/// stderr so the error path never allocates a lossy copy of the whole log.
fn stale_seed_in(bytes: &[u8]) -> bool {
    contains_subslice(bytes, b"plan seed db")
        || contains_subslice(bytes, b"corrupt content-addressed item")
        || contains_subslice(bytes, b"is not interned in the seed store")
        || contains_subslice(bytes, b"tampered post-intern")
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

fn with_seed_reset_hint(err: String, lw: &Path) -> String {
    if looks_like_stale_seed(&err) {
        let hint = seed_reset_hint(lw, &err);
        format!("{err}\n{hint}")
    } else {
        err
    }
}

/// Whether the human-facing live progress stream is suppressed. `TD_RECIPE_QUIET=1`
/// reverts `run`/`build-run`/`qemu-boot*` to the buffered path: stdout is captured and
/// flushed on completion, stderr is captured to the err-file and surfaced on failure (the
/// error tail) — neither is tee'd live as the ladder climbs.
fn quiet_requested() -> bool {
    env::var_os("TD_RECIPE_QUIET").is_some_and(|v| !v.is_empty())
}

/// Run `cmd`, teeing its stdout AND stderr live to this process's stdout/stderr while
/// accumulating both, and return the same `(status, stdout, stderr)` triple
/// `Command::output` would — so a caller can still write the out/err files, take an error
/// tail, and scan for stale-seed markers — but the operator sees the build's own output as
/// it happens. The sandboxed build inherits the builder's fds, so its make/configure
/// chatter (stdout) and compiler diagnostics (stderr) both surface live.
///
/// Each stream is drained CONCURRENTLY (stdout on a thread, stderr on the main loop):
/// build-plan interleaves per-rung `STEP` lines on stdout with progress on stderr, and
/// draining one to EOF before touching the other could deadlock once a long build fills
/// the unread pipe's buffer.
fn spawn_capture_tee(
    cmd: &mut Command,
) -> Result<(process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut child = cmd
        // Null stdin, matching `Command::output`: the build-plan child is non-interactive,
        // and inheriting the parent's stdin (a terminal on the interactive `run` path) would
        // both risk a hang and hand the sandboxed build an undeclared host input.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "stdout pipe unavailable".to_string())?;
    // `Builder::spawn` (fallible) not `thread::spawn` (panics if the OS cannot create the
    // thread): a panic here would both violate the crate's no-panic rule and unwind past the
    // already-spawned child, which `Drop` neither kills nor waits — orphaning a builder that
    // keeps mutating the cache after the caller releases the ladder lock. Reap it instead.
    // The reader tees stdout live (chunked, not read_to_end) so it surfaces as it is produced.
    let stdout_thread = std::thread::Builder::new()
        .name("build-plan-stdout".to_string())
        .spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let stdout = io::stdout();
            loop {
                match stdout_pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let slice = chunk.get(..n).unwrap_or(&[]);
                        // Tee live; a broken/closed terminal must not abort a valid build, so a
                        // failed write is ignored — the bytes are still captured for the out-file.
                        {
                            let mut handle = stdout.lock();
                            let _ = handle.write_all(slice);
                            let _ = handle.flush();
                        }
                        buf.extend_from_slice(slice);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(buf)
        })
        .map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            format!("spawn build-plan stdout reader: {e}")
        })?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "stderr pipe unavailable".to_string())?;
    let mut stderr_buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let stderr = io::stderr();
    loop {
        match stderr_pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let slice = chunk.get(..n).unwrap_or(&[]);
                // Tee live. A broken/closed terminal must not abort a valid build, so a
                // failed terminal write is ignored — the bytes are still captured below
                // for the err-file, error tail, and stale-seed scan.
                {
                    let mut handle = stderr.lock();
                    let _ = handle.write_all(slice);
                    let _ = handle.flush();
                }
                stderr_buf.extend_from_slice(slice);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Hard stderr read error: reap the child before surfacing it. `Child::drop`
                // neither kills nor waits, so a bare return would orphan a builder that keeps
                // mutating scratch/cache after the caller releases the ladder lock. Killing
                // closes the child's stdout too, so the stdout reader unblocks and joins.
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                return Err(format!("read stderr: {e}"));
            }
        }
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    let stdout_buf = match stdout_thread.join() {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => return Err(format!("read stdout: {e}")),
        Err(_) => return Err("stdout reader thread panicked".to_string()),
    };
    Ok((status, stdout_buf, stderr_buf))
}

fn command_output(cmd: &mut Command, label: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("spawn {label}: {e}"))?;
    if !out.status.success() {
        return Err(child_failure(label, &out));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{label} output not UTF-8: {e}"))
}

fn command_output_with_stdin(
    cmd: &mut Command,
    label: &str,
    stdin: &str,
) -> Result<String, String> {
    command_output_with_stdin_bytes(cmd, label, stdin.as_bytes())
}

fn command_output_with_stdin_bytes(
    cmd: &mut Command,
    label: &str,
    stdin: &[u8],
) -> Result<String, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {label}: {e}"))?;
    match child.stdin.as_mut() {
        Some(input) => input
            .write_all(stdin)
            .map_err(|e| format!("write {label} stdin: {e}"))?,
        None => return Err(format!("{label}: stdin pipe unavailable")),
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait {label}: {e}"))?;
    if !out.status.success() {
        return Err(child_failure(label, &out));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{label} output not UTF-8: {e}"))
}

/// The failed-child message for every runner here that CAPTURES both streams,
/// so the HOST GAP a child reported cannot be flattened away by whichever one
/// happened to run it. `td-builder` exits 69 + sentinel when no toolchain is
/// reachable in the jail; without the marker that becomes an ordinary error and
/// `die_runner` reds on something no host could have avoided. `build_plan`
/// applies the same rule inline, on its own captured bytes. `store_verify_pair`
/// cannot: it runs on `.status()` with no stream to scan, and the verb it drives
/// does not exit 69.
fn child_failure(label: &str, out: &std::process::Output) -> String {
    let msg = format!(
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if td_engine::exit::child_reported_host_gap(out.status.code(), &out.stdout, &out.stderr) {
        return format!("{HOST_GAP}{msg}");
    }
    msg
}

/// Hex SHA-256 of a byte string. In-process (`crate::sha256`) — pin
/// verification must not depend on an ambient host `sha256sum` (re #469).
fn sha256sum(bytes: &[u8]) -> String {
    crate::sha256::hex_digest(bytes)
}

fn append_file_bytes(path: &Path, out: &mut Vec<u8>) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.read_to_end(out)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(())
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read dir {}: {e}", dir.display()))? {
        out.push(
            entry
                .map_err(|e| format!("read dir {} entry: {e}", dir.display()))?
                .path(),
        );
    }
    out.sort();
    Ok(out)
}

pub(crate) fn linux_version_from_file(file_name: &str) -> Result<String, String> {
    let rest = file_name
        .strip_prefix("linux-")
        .ok_or_else(|| format!("linux source file name is malformed: {file_name}"))?;
    rest.split(".tar")
        .next()
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("linux source file name is malformed: {file_name}"))
}

fn path_basename_str(path: &str) -> Result<&str, String> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("path has no UTF-8 basename: {path}"))
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub(crate) fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_dir() {
                make_user_writable(path)?;
                fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))
            } else {
                make_file_user_writable(path, &meta)?;
                fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

fn make_user_writable(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o700);
        fs::set_permissions(path, perms)
            .map_err(|e| format!("chmod u+rwx {}: {e}", path.display()))?;
        for child in read_dir_sorted(path)? {
            make_user_writable(&child)?;
        }
    } else {
        make_file_user_writable(path, &meta)?;
    }
    Ok(())
}

fn make_file_user_writable(path: &Path, meta: &fs::Metadata) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() | 0o600);
    fs::set_permissions(path, perms).map_err(|e| format!("chmod u+rw {}: {e}", path.display()))
}

fn single_subdir_path(dir: &Path) -> Result<PathBuf, String> {
    let mut subdirs = Vec::new();
    for path in read_dir_sorted(dir)? {
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    match subdirs.len() {
        1 => subdirs
            .pop()
            .ok_or_else(|| format!("expected one top-level dir under {}", dir.display())),
        n => Err(format!(
            "expected one top-level dir under {}, found {n}",
            dir.display()
        )),
    }
}

fn clean_stage0_build_dirs(root: &Path) -> Result<(), String> {
    for dir in ["AMD64/artifact", "AMD64/bin"] {
        let path = root.join(dir);
        remove_path_if_exists(&path)?;
        fs::create_dir_all(&path).map_err(|e| format!("mkdir {}: {e}", path.display()))?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        symlink(target, dst)?;
        return Ok(());
    }
    if ftype.is_dir() {
        fs::create_dir_all(dst)?;
        let mut children = Vec::new();
        for entry in fs::read_dir(src)? {
            children.push(entry?.path());
        }
        children.sort();
        for child in children {
            if let Some(name) = child.file_name() {
                copy_tree(&child, &dst.join(name))?;
            }
        }
        fs::set_permissions(dst, meta.permissions())?;
        return Ok(());
    }
    if ftype.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        fs::set_permissions(dst, meta.permissions())?;
    }
    Ok(())
}

/// Copy a source tree, skipping build/VCS artifacts (`target`, `.git`) at every
/// level so the interned bytes are the committed source only — a content address
/// that does not depend on a prior host `cargo build` or the local git dir.
// Recursively copy a source tree (skipping `target`/`.git`), preserving symlinks,
// executable bits, and a deterministic (sorted) directory order for a stable
// content address. This interns the WORKING tree, not a git snapshot: any other
// untracked or locally-modified file under the dir folds into the address and, if
// it does not match the compiled seed-digest pin, reds the gate. That is
// fail-closed (a dirty checkout can never silently swap the interned source), so
// the pin builds from a clean tree; git-tracked filtering would need a git
// subprocess, which the zero-shell recipe runner deliberately avoids.
fn copy_source_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        symlink(target, dst)?;
        return Ok(());
    }
    if ftype.is_dir() {
        fs::create_dir_all(dst)?;
        let mut children = Vec::new();
        for entry in fs::read_dir(src)? {
            children.push(entry?.path());
        }
        children.sort();
        for child in children {
            let Some(name) = child.file_name() else {
                continue;
            };
            if matches!(name.to_str(), Some("target") | Some(".git")) {
                continue;
            }
            copy_source_tree(&child, &dst.join(name))?;
        }
        fs::set_permissions(dst, meta.permissions())?;
        return Ok(());
    }
    if ftype.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        fs::set_permissions(dst, meta.permissions())?;
    }
    Ok(())
}

fn tail_bytes(bytes: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut selected: Vec<&str> = text.lines().rev().take(lines).collect();
    selected.reverse();
    selected.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `cargo` at `path`: `body` runs with the real cargo's argv, so a test
    /// can make it produce the binary, exit 0 producing nothing, or fail.
    fn write_fake_cargo(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        // Same ETXTBSY race `stage0::tests::write_exec` documents: a fork in another
        // test thread holds our write fd until it execs, and this script is exec'd by
        // the code under test. Prove it runs before handing it over.
        for _ in 0..400 {
            match Command::new(path).arg("--td-fixture-probe").stdin(Stdio::null()).output() {
                Err(e) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                _ => return,
            }
        }
        panic!("fixture {} never stopped being Text-file-busy", path.display());
    }

    /// The auto-build's three outcomes. `find_td_builder_self` runs a real host
    /// cargo, so the outcome handling is what a test can pin — and the middle case
    /// (exit 0, no binary) is the one an ambient CARGO_TARGET_DIR used to produce.
    #[test]
    fn the_auto_build_reports_its_three_outcomes_apart() {
        let d = env::temp_dir().join(format!("td-autobuild-{}", process::id()));
        let _ = fs::remove_dir_all(&d);
        let release = d.join("target/release/td-builder");
        let cargo = d.join("bin/cargo");

        // Built: the fake cargo materializes the binary where the caller looks.
        write_fake_cargo(
            &cargo,
            &format!(
                "mkdir -p '{}' && printf '#!/bin/sh\\n' > '{}' && chmod 755 '{}'\n",
                release.parent().unwrap().display(),
                release.display(),
                release.display()
            ),
        );
        assert_eq!(build_td_builder(&cargo, &d, &release).unwrap(), release);

        // Exited 0 and produced nothing: NOT reported as a build failure, because
        // the build did not fail — the binary is simply not where we look.
        fs::remove_file(&release).unwrap();
        write_fake_cargo(&cargo, "exit 0\n");
        let quiet = build_td_builder(&cargo, &d, &release).unwrap_err();
        assert!(quiet.contains("succeeded but"), "{quiet}");

        // Failed: report the status, and do NOT assert a compile error — a
        // read-only tree or a wrong root lands here too.
        write_fake_cargo(&cargo, "exit 101\n");
        let failed = build_td_builder(&cargo, &d, &release).unwrap_err();
        assert!(failed.contains("status 101"), "{failed}");
        assert!(!failed.contains("regression"), "{failed}");
        let _ = fs::remove_dir_all(&d);
    }

    /// A `build.target` in `.cargo/config.toml` (or an ambient CARGO_BUILD_TARGET)
    /// moves the artifact to `target/<triple>/release/`, which `--target-dir` does
    /// NOT prevent — it pins the target ROOT, not the triple subdir under it. A path
    /// guess would report a successful build as missing, so cargo is asked.
    #[test]
    fn the_artifact_is_taken_from_cargo_not_guessed() {
        let d = env::temp_dir().join(format!("td-autobuild-triple-{}", process::id()));
        let _ = fs::remove_dir_all(&d);
        let guessed = d.join("target/release/td-builder");
        let actual = d.join("target/x86_64-unknown-linux-musl/release/td-builder");
        let cargo = d.join("bin/cargo");
        write_fake_cargo(
            &cargo,
            &format!(
                "mkdir -p '{}' && printf '#!/bin/sh\\n' > '{}' && chmod 755 '{}'\n\
                 printf '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\
                 \"td-builder\"}},\"executable\":\"{}\"}}\\n'\n",
                actual.parent().unwrap().display(),
                actual.display(),
                actual.display(),
                actual.display()
            ),
        );
        assert_eq!(build_td_builder(&cargo, &d, &guessed).unwrap(), actual);
        assert!(!guessed.exists(), "the guessed path must not be what answered");
        let _ = fs::remove_dir_all(&d);
    }

    /// Honouring `build.target` means a cross-target build lands an artifact that is
    /// executable and still cannot run here, and the caller execs what it gets back.
    /// A binary that does not run must be named at the build, not two calls later as
    /// `Exec format error`.
    #[test]
    fn a_binary_that_does_not_run_here_is_refused() {
        let d = env::temp_dir().join(format!("td-autobuild-foreign-{}", process::id()));
        let _ = fs::remove_dir_all(&d);
        let release = d.join("target/release/td-builder");
        let cargo = d.join("bin/cargo");
        // Stands in for a foreign-architecture binary: present, +x, and unrunnable.
        write_fake_cargo(
            &cargo,
            &format!(
                "mkdir -p '{}'
printf '#!/bin/sh\nexit 1\n' > '{}'
chmod 755 '{}'
",
                release.parent().unwrap().display(),
                release.display(),
                release.display()
            ),
        );
        let err = build_td_builder(&cargo, &d, &release).unwrap_err();
        assert!(err.contains("does not run here"), "{err}");
        let _ = fs::remove_dir_all(&d);
    }

    /// The absent-cargo branch decides whether the operator is told to build by hand,
    /// and it used to read the real PATH, so nothing could cover it. The PATH is an
    /// argument now.
    #[test]
    fn an_empty_path_finds_no_cargo() {
        assert_eq!(cargo_on_path(None), None);
        assert_eq!(cargo_on_path(Some(OsStr::new(""))), None);
        let d = env::temp_dir().join(format!("td-cargo-probe-{}", process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        assert_eq!(cargo_on_path(Some(d.as_os_str())), None);
        write_fake_cargo(&d.join("cargo"), "exit 0\n");
        assert_eq!(cargo_on_path(Some(d.as_os_str())), Some(d.join("cargo")));
        let _ = fs::remove_dir_all(&d);
    }

    // build-run reads every requested output's STEP line from the ONE plan it
    // builds (`build-plan --auto TARGET`), so a stem outside TARGET's recipe
    // closure must refuse at argv validation — not red after the whole build.
    #[test]
    fn build_cli_refuses_output_outside_target_closure() {
        let err = build_cli(&["stage0".to_string(), "mes".to_string()])
            .expect_err("mes is not in stage0's closure");
        assert!(
            err.contains("not in the recipe closure of 'stage0'"),
            "got: {err}"
        );
    }

    // The compiled seed-digest table and the catalog must agree EXACTLY
    // (re #469): a seed key any recipe declares without a compiled digest
    // would red at derivation, and an orphan row pins nothing. Cold-safe:
    // walks the compiled catalog + pins only, no warm sources. On mismatch,
    // regenerate with `td-recipe-eval seed-digests > seed/seed-digests.txt`.
    #[test]
    fn seed_digest_table_covers_the_catalog_seed_universe() {
        let universe: std::collections::BTreeSet<String> = catalog_seed_universe()
            .unwrap()
            .iter()
            .map(|s| s.key().to_string())
            .collect();
        let table: std::collections::BTreeSet<String> = crate::seed_digests::rows()
            .unwrap()
            .iter()
            .map(|(k, _)| (*k).to_string())
            .collect();
        assert_eq!(
            universe, table,
            "seed/seed-digests.txt must pin exactly the catalog's pinned-seed universe — \
             regenerate with `td-recipe-eval seed-digests > seed/seed-digests.txt`"
        );
    }

    // The stale basename this repo actually shipped: `tests/sshd` was edited in the
    // /etc-symlinks landing while its table row kept the digest of the tree before
    // that edit. Every warm machine kept building the old bytes; a cold one red.
    const STALE_SSHD_BASENAME: &str = "gyw2rg42bcbx74znn7kr0hlcdhds13r2-sshd-source";

    fn pinned_basename(key: &str) -> &'static str {
        crate::seed_digests::expected(key).unwrap().unwrap()
    }

    // The gate an edited local source hits, in both directions. An address that is
    // NOT the pinned one is rejected however warm the ladder is (the store still
    // holding the old basename is not consulted — this is a pure comparison against
    // the compiled table), and the matching address is accepted.
    #[test]
    fn a_local_source_is_gated_on_its_current_hash_not_on_what_the_store_holds() {
        let key = "sshd-source";
        let err = gate_local_source_candidate(key, &format!("{TD_STORE_DIR}/{STALE_SSHD_BASENAME}"))
            .expect_err("the pre-edit basename must no longer be admissible");
        assert!(err.contains("provenance rejected"), "got: {err}");
        assert!(err.contains(pinned_basename(key)), "got: {err}");
        // The recovery line must send the developer to the TABLE. `clear-store` is the
        // reflex the neighbouring stale-seed reds teach, and it is wrong here: it costs
        // the whole ladder and lands on the identical red.
        assert!(err.contains("seed/seed-digests.txt"), "got: {err}");
        assert!(
            err.contains("`clear-store' does NOT help"),
            "the error must say clear-store is not the fix: {err}"
        );

        gate_local_source_candidate(key, &format!("{TD_STORE_DIR}/{}", pinned_basename(key)))
            .expect("the tree's current hash is exactly what the committed table pins");
    }

    // A warm seed store answers for a PINNED seed and NEVER for a local source. A pin
    // names bytes no checkout edit can change, so a present basename IS those bytes; a
    // local source's bytes are the working tree, so the same presence proves nothing.
    // Driven through `ensure_seed_input` with BOTH basenames interned: the pinned seed
    // short-circuits (this runner's `tb` is empty, so it cannot have derived anything),
    // and the local source declines to, reaching the staging step — which reds here
    // because the test root holds no such directory.
    //
    // The vouch set is planted rather than read, because the store being warm is no
    // longer sufficient — the db must also carry the row (see `db_vouches`) —
    // and reading it for real would need the builder this runner deliberately lacks.
    // `a_warm_tree_this_tables_db_does_not_vouch_is_re_registered` covers the gate itself.
    #[test]
    fn warm_seed_store_answers_for_a_pin_but_never_for_a_local_source() {
        let lw = env::temp_dir().join(format!("td-warm-seed-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let mut runner = shared_test_runner(&lw);
        runner.root = lw.join("root");
        fs::create_dir_all(&runner.root).unwrap();
        fs::create_dir_all(&runner.store).unwrap();
        fs::create_dir_all(&runner.scratch).unwrap();
        for key in ["stage0-source", "sshd-source"] {
            fs::create_dir_all(runner.store.join(pinned_basename(key))).unwrap();
        }
        *runner.vouched.lock().unwrap() = Some(
            ["stage0-source", "sshd-source"]
                .iter()
                .map(|k| format!("{TD_STORE_DIR}/{}", pinned_basename(k)))
                .collect(),
        );

        let warm = runner
            .ensure_seed_input(&SeedInput::Stage0 {
                key: "stage0-source".into(),
            })
            .expect("a pinned seed interned at its table basename is a warm hit");
        assert_eq!(
            warm,
            format!("{TD_STORE_DIR}/{}", pinned_basename("stage0-source"))
        );

        let err = runner
            .ensure_seed_input(&SeedInput::LocalSource {
                key: "sshd-source".into(),
                path: "tests/sshd".into(),
            })
            .expect_err("a local source must re-hash even with its basename interned");
        assert!(err.contains("local source"), "got: {err}");
        let _ = fs::remove_dir_all(&lw);
    }

    // The store is shared across pin tables and the db is not, so a warm TREE is not
    // evidence this table's db has the row: on a table's first run every seed is
    // already interned and none is registered. Left short-circuiting on presence
    // alone, the db stays empty and `build-plan --auto` reds on a seed db that is not
    // even a file — which is exactly what a live run did before this gate existed.
    //
    // Observed through the re-registration ATTEMPT: with an empty vouch set the warm
    // path must reach the re-hash, which needs the builder this runner has none of.
    // An error naming it is the gate firing; `Ok` would be the short-circuit that
    // regressed. It is `store-path-recursive` rather than `store-add-recursive`
    // because the address is COMPUTED before anything is interned — see the gate
    // there, which is what keeps a bad tree out of this table's db.
    #[test]
    fn a_warm_tree_this_tables_db_does_not_vouch_is_re_registered() {
        let lw = env::temp_dir().join(format!("td-unvouched-seed-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let mut runner = shared_test_runner(&lw);
        runner.root = lw.join("root");
        fs::create_dir_all(&runner.root).unwrap();
        fs::create_dir_all(&runner.store).unwrap();
        fs::create_dir_all(&runner.scratch).unwrap();
        fs::create_dir_all(runner.store.join(pinned_basename("stage0-source"))).unwrap();
        // No db at all — a pin table that has never run on this ladder.
        assert!(!runner.db.exists());
        assert!(
            !runner.db_vouches(&format!("{TD_STORE_DIR}/{}", pinned_basename("stage0-source"))),
            "an absent db vouches nothing, which is the fail-closed direction"
        );

        let err = runner
            .ensure_seed_input(&SeedInput::Stage0 {
                key: "stage0-source".into(),
            })
            .expect_err("a warm tree the db does not vouch must re-register, not short-circuit");
        assert!(
            err.contains("store-path-recursive"),
            "the gate must re-hash BEFORE interning, so this is where it stops: {err}"
        );
        let _ = fs::remove_dir_all(&lw);
    }

    // A local source that does not make it through the check leaves the retained seed
    // store and db byte-identical. The tree here is a real, resolvable crate, so the
    // run gets as far as hashing it (which fails: this runner has no builder) — far
    // enough to prove nothing along the way writes to the ladder.
    //
    // What this does NOT prove is the ORDER, since it never reaches the gate; a
    // rejection that had already interned would need a working hasher to observe.
    // `ensure_local_source_gates_before_it_interns` is the guard for that.
    #[test]
    fn a_failed_local_source_check_leaves_the_retained_store_and_db_untouched() {
        let lw = env::temp_dir().join(format!("td-local-src-notouch-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let mut runner = shared_test_runner(&lw);
        runner.root = lw.join("root");
        let src = runner.root.join("tests/demo-src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Cargo.toml"), b"[package]\nname = \"demo\"\n").unwrap();
        fs::write(src.join("Cargo.lock"), b"version = 4\n").unwrap();
        fs::create_dir_all(&runner.store).unwrap();
        fs::create_dir_all(&runner.scratch).unwrap();
        // A retained ladder: one interned seed plus its db.
        fs::create_dir_all(runner.store.join(pinned_basename("stage0-source"))).unwrap();
        fs::create_dir_all(runner.db.parent().unwrap()).unwrap();
        fs::write(&runner.db, b"the retained seed registrations").unwrap();
        let store_before = dir_listing(&runner.store);
        let db_before = fs::read(&runner.db).unwrap();

        let err = runner
            .ensure_seed_input(&SeedInput::LocalSource {
                key: "sshd-source".into(),
                path: "tests/demo-src".into(),
            })
            .expect_err("this runner has no td-builder to hash with");
        assert!(err.contains("store-path-recursive"), "got: {err}");

        assert_eq!(store_before, dir_listing(&runner.store));
        assert_eq!(db_before, fs::read(&runner.db).unwrap());
        let _ = fs::remove_dir_all(&lw);
    }

    // The ORDER inside `ensure_local_source` — hash, gate, only then intern — is the
    // whole defence, and no type or test above can observe it without a real builder
    // to hash with. Assert it against the source, as td-init does for its syscall
    // confinement: the gate call must PRECEDE the interning call.
    #[test]
    fn ensure_local_source_gates_before_it_interns() {
        let src = include_str!("check_runner.rs");
        let body = src
            .split_once("fn ensure_local_source(")
            .and_then(|(_, rest)| rest.split_once("\n    }\n"))
            .map(|(body, _)| body)
            .expect("ensure_local_source must be findable in this file");
        let gate = body
            .find("gate_local_source_candidate(")
            .expect("ensure_local_source must gate the candidate address");
        let intern = body
            .find("store_add_recursive(")
            .expect("ensure_local_source must intern the staged tree");
        assert!(
            gate < intern,
            "ensure_local_source must gate the address BEFORE interning it — interning \
             first poisons the retained seed db for every later plan"
        );
    }

    fn dir_listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // The seed map is fresh per-run derived state, never a persisted authority
    // (re #469): every run re-derives, pin-verifies, and stages each seed, then
    // writes the `--auto` map from exactly those verified paths. `serialize_auto_map`
    // is the pure format helper; there is no prior map read anywhere (the persistent
    // srcs.map, its reconcile guard, and the warm short-circuit are all DELETED).
    #[test]
    fn auto_map_serializes_the_current_graph_seeds_as_name_space_path_lines() {
        let entries = vec![
            ("mes-source".to_string(), "/td/store/aaa-mes".to_string()),
            (
                "stage0-source".to_string(),
                "/td/store/bbb-stage0".to_string(),
            ),
        ];
        assert_eq!(
            serialize_auto_map(&entries),
            "mes-source /td/store/aaa-mes\nstage0-source /td/store/bbb-stage0\n"
        );
        // No entries ⇒ an empty map (not a stray newline).
        assert_eq!(serialize_auto_map(&[]), "");
    }

    #[test]
    fn sanitize_target_keeps_recipe_stems_and_neutralizes_path_bytes() {
        // A normal recipe stem passes through unchanged (dots kept for versions).
        assert_eq!(sanitize_target_for_filename("system-x86-64"), "system-x86-64");
        assert_eq!(sanitize_target_for_filename("gcc.14_2"), "gcc.14_2");
        // Every separator becomes `_`, so no `/` survives to form a traversal — the result
        // is always a single flat filename component (kept dots can't traverse alone).
        assert_eq!(sanitize_target_for_filename("../../etc/x"), ".._.._etc_x");
        assert_eq!(sanitize_target_for_filename("a/b"), "a_b");
        assert!(!sanitize_target_for_filename("../../etc/x").contains('/'));
    }

    #[test]
    fn pinned_patch_prefixed_source_is_a_source_not_a_seed_patch() {
        // `patch-mesboot-source` pins the GNU patch PROGRAM's tarball; the
        // `patch-` prefix convention must not shadow it into a (nonexistent)
        // seed/patches/mesboot-source.patch — that broke every cold-host
        // chain build at the first mesboot rung.
        assert!(special_seed_input("patch-mesboot-source")
            .unwrap()
            .is_none());
        match special_seed_input("patch-binutils-boot-2.20.1a").unwrap() {
            Some(SeedInput::Patch { patch, .. }) => {
                assert_eq!(patch, "binutils-boot-2.20.1a")
            }
            _ => panic!("expected the binutils-boot seed patch input"),
        }
    }

    #[test]
    fn trailing_pid_parses_only_a_numeric_suffix() {
        // scratch_name appends `-<pid>` — the reaper keys on exactly that.
        assert_eq!(trailing_pid("build-oyacc-4059"), Some(4059));
        assert_eq!(trailing_pid("check-make-test-1-12345"), Some(12345));
        assert_eq!(trailing_pid("seed-digests-7"), Some(7));
        // No numeric suffix ⇒ not a reapable scratch dir (never touched).
        assert_eq!(trailing_pid("build-oyacc"), None);
        assert_eq!(trailing_pid("build-oyacc-"), None);
        assert_eq!(trailing_pid("build-oyacc-4059abc"), None);
        assert_eq!(trailing_pid("recipes"), None);
        assert_eq!(trailing_pid(""), None);
    }

    #[test]
    fn scratch_name_round_trips_through_trailing_pid() {
        // Whatever scratch_name emits stays recognizable as one of our trees — both
        // as emitted and after a claim has disambiguated it.
        let n = scratch_name("build", &["oyacc"]);
        assert_eq!(trailing_pid(&n), Some(process::id()));
        assert!(reapable_scratch(&n));
        assert!(reapable_scratch(&format!("{n}.2")));
    }

    #[test]
    fn reapable_scratch_requires_our_scratch_prefix() {
        // Our own trees are eligible...
        assert!(reapable_scratch("build-oyacc-4059"));
        assert!(reapable_scratch("check-make-test-1-12345"));
        // ...including the host-side qemu-boot tool's per-boot scratch (a killed boot's
        // multi-GiB kernel-build tree would otherwise leak forever).
        assert!(reapable_scratch("qemu-boot-linux-x86-64-22760"));
        // ...and the interactive `run` tool's per-boot scratch (same multi-GiB leak risk).
        assert!(reapable_scratch("run-system-x86-64-31820"));
        // ...and a claim-disambiguated one, which two same-pid runs in different pid
        // namespaces now produce.
        assert!(reapable_scratch("check-make-test-1-12345.3"));
        // ...but a coincidental numeric-suffixed sibling is NEVER reaped.
        assert!(!reapable_scratch("gcc-14"));
        assert!(!reapable_scratch("glibc-241"));
        assert!(!reapable_scratch("binutils-244"));
        assert!(!reapable_scratch("build-cache")); // the cache dir, no pid
        assert!(!reapable_scratch("store"));
        assert!(!reapable_scratch("seed-store"));
        // A claim lock is a dotfile, so read_dir never offers it as a tree anyway —
        // but it must not read as one either.
        assert!(!reapable_scratch(".check-make-test-1-9.claim"));
    }

    // The reaper's whole safety rule, and the one the pid could not express: a tree
    // whose claim is HELD is live and must survive, whoever holds it and in whatever
    // pid namespace; a tree whose claim is free is abandoned and goes. Driven with a
    // real lock rather than a real peer, since a peer in another pid namespace is
    // exactly what the gate cannot spawn.
    #[test]
    fn the_reaper_spares_a_claimed_tree_and_takes_an_unclaimed_one() {
        let lw = env::temp_dir().join(format!("td-reap-claim-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let runner = shared_test_runner(&lw);
        let scratch_root = lw.join("scratch");

        // A live peer: a HELD claim, then its tree — the order production uses, since
        // claiming CLEARS whatever the name held. The name carries a pid that is not
        // ours and need not exist here; under the old rule that alone condemned it.
        let live = "check-peer-test-1-999999";
        let held = claim_scratch(&scratch_root, live).unwrap().1;
        fs::create_dir_all(scratch_root.join(live)).unwrap();

        // An abandoned tree: same shape, claim free.
        let dead = "check-dead-test-1-999998";
        fs::create_dir_all(scratch_root.join(dead)).unwrap();

        runner.reap_dead_scratch();

        assert!(
            scratch_root.join(live).is_dir(),
            "a claimed tree is a LIVE peer's and must survive the reaper"
        );
        assert!(
            !scratch_root.join(dead).exists(),
            "an unclaimed tree is abandoned and must be reclaimed"
        );
        // The claim FILE survives either way — reaping it would let two runs hold one
        // name through the replaced inode.
        assert!(scratch_claim_lock(&scratch_root, dead).is_file());
        drop(held);
        let _ = fs::remove_dir_all(&lw);
    }

    // Two runs asking for the SAME scratch name get DIFFERENT directories while both
    // are live. This is what two sandboxed peers do: a fresh pid namespace hands each
    // the same low pid, so `check-<spec>-<index>-<pid>` collided and `setup` wiped a
    // live tree. Asserted on the second claim rather than on a pid, because the claim
    // is what now decides.
    #[test]
    fn two_live_claims_on_one_name_get_separate_scratch_dirs() {
        let lw = env::temp_dir().join(format!("td-claim-collide-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let root = lw.join("scratch");
        let name = "check-td-boot-test-1-1";

        let (first, hold_first) = claim_scratch(&root, name).unwrap();
        let (second, _hold_second) = claim_scratch(&root, name).unwrap();
        assert_ne!(first, second, "a live claim must not be handed out twice");
        assert_eq!(first.file_name().and_then(|n| n.to_str()), Some(name));
        // Still one of ours, so it is still reapable once abandoned.
        let base = second.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(reapable_scratch(base));

        // Released, the first name is reusable — a dead predecessor does not cost a
        // name forever.
        drop(hold_first);
        let (again, _hold) = claim_scratch(&root, name).unwrap();
        assert_eq!(again, first);
        let _ = fs::remove_dir_all(&lw);
    }

    /// A minimal runner pointed at a throwaway ladder tree, for the fs-level
    /// setup() tests. Only the path fields matter; the rest are inert.
    fn shared_test_runner(lw: &Path) -> RecipeCheckRunner {
        let scratch = lw.join("scratch").join("test");
        RecipeCheckRunner {
            root: PathBuf::new(),
            tb: PathBuf::new(),
            builder_path: String::new(),
            builder_store: PathBuf::new(),
            builder_db: PathBuf::new(),
            lw: lw.to_path_buf(),
            sources_dir: lw.join("sources"),
            // The real derivations, not `lw/store` and `lw/db`: these tests assert what
            // setup() and clear-store do to the seed store and db, and a helper that
            // invented its own paths would assert it about files production never writes.
            store: seed_store_dir(lw),
            db: seed_db_path(lw).expect("compiled pin table parses"),
            recipes: scratch.join("recipes"),
            scratch,
            daemon_dir: None,
            stream_progress: false,
            vouched: std::sync::Mutex::new(None),
            // Not claimed: this runner shares no ladder with anything.
            scratch_lock: None,
        }
    }

    // The heart of the change: setup() NEVER destroys persisted ladder state. The shared
    // build-cache AND the seed store/db all survive a normal run — clearing is the explicit
    // `clear-store`'s job. (The seeds are re-interned idempotently each run by
    // `ensure_seed_input`; a retained, intact seed store is verified-and-reused, and a torn
    // one reds with the clear-store hint instead of being silently papered over.) Only THIS
    // invocation's private, pid-tagged scratch is (re)created fresh.
    #[test]
    fn setup_preserves_all_persisted_ladder_state() {
        let lw = env::temp_dir().join(format!("td-ladder-shared-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        // A neighbor's warm build-cache (the shared layer) and this ladder's retained seed
        // store/db — none of it may be touched by setup().
        fs::create_dir_all(lw.join("build-cache").join("store")).unwrap();
        fs::write(
            lw.join("build-cache").join("store").join("rung-sentinel"),
            b"toolchain",
        )
        .unwrap();
        fs::create_dir_all(seed_store_dir(&lw)).unwrap();
        fs::write(seed_store_dir(&lw).join("seed-item"), b"interned-seed").unwrap();
        let runner = shared_test_runner(&lw);
        fs::create_dir_all(runner.db.parent().unwrap()).unwrap();
        fs::write(&runner.db, b"this ladder's registered seed rows").unwrap();

        // No cap ⇒ no eviction, so even the tiny sentinel build-cache survives; this stays
        // hermetic against the ambient TD_CHECK_LADDER_CACHE_CAP_BYTES knob.
        runner.setup_with_cache_cap(None).unwrap();

        // Nothing persisted is wiped: the build-cache, the seed store, and the seed db all
        // survive intact.
        assert!(lw
            .join("build-cache")
            .join("store")
            .join("rung-sentinel")
            .is_file());
        assert!(seed_store_dir(&lw).join("seed-item").is_file());
        assert!(runner.db.is_file(), "the seed db for this pin table survives");
        // The per-invocation scratch is freshly created.
        assert!(runner.scratch.is_dir());
        let _ = fs::remove_dir_all(&lw);
    }

    // `clear-store` is the ONLY path that resets persisted ladder state: it removes the whole
    // ladder work dir (seed store/db AND the shared build-cache), leaving the sibling lock
    // untouched. Driven through `clear_ladder` (the env-free core of `clear_store_cli`) so the
    // test stays hermetic against process-global env.
    #[test]
    fn clear_store_nukes_the_whole_ladder_and_keeps_the_lock() {
        // A deep-enough dir so reject_unsafe_clear_target admits it (a real ladder is >=3 deep).
        let lw = env::temp_dir()
            .join(format!("td-clear-{}", process::id()))
            .join("build-daemon")
            .join("ladder-shared-v1");
        let lock = ladder_lock_path(&lw);
        let tomb = clearing_tombstone_path(&lw);
        let _ = fs::remove_dir_all(&lw);
        let _ = fs::remove_file(&lock);
        let _ = fs::remove_dir_all(&tomb);
        fs::create_dir_all(lw.join("build-cache").join("store")).unwrap();
        fs::write(lw.join("build-cache").join("store").join("rung"), b"x").unwrap();
        fs::create_dir_all(seed_store_dir(&lw)).unwrap();
        fs::write(seed_store_dir(&lw).join("seed-item"), b"y").unwrap();
        fs::write(lw.join("db"), b"rows").unwrap();
        // Materialize the sibling lock as a build would, so we can assert it survives.
        drop(lock_file(&lock).unwrap());

        clear_ladder(&lw).unwrap();

        // The whole ladder tree is gone, the swap-aside tombstone did not leak, and the sibling
        // lock (BESIDE lw) is not touched.
        assert!(!lw.exists());
        assert!(!tomb.exists());
        assert!(lock.is_file());
        // Idempotent: clearing an already-absent ladder is a no-op Ok (creates only the lock).
        clear_ladder(&lw).unwrap();
        assert!(!lw.exists());
        // A stray argument is a usage error (checked before any fs work).
        let err = clear_store_cli(&["extra".to_string()]).unwrap_err();
        assert!(err.contains("usage: clear-store"));
        let _ = fs::remove_dir_all(env::temp_dir().join(format!("td-clear-{}", process::id())));
    }

    // `clear-store` also resets the signed substitute store, so a post-clear toolchain build
    // cold-climbs from seed instead of fetching the prior publish. Driven through
    // `clear_subst_store` (the env-free core) against a throwaway tree.
    #[test]
    fn clear_store_nukes_the_substitute_store() {
        let store = env::temp_dir()
            .join(format!("td-subst-clear-{}", process::id()))
            .join(".td")
            .join("subst");
        let tomb = clearing_tombstone_path(&store);
        let _ = fs::remove_dir_all(&store);
        let _ = fs::remove_dir_all(&tomb);
        // A populated store: the stashed td-subst binary + a signed narinfo, as a publisher leaves it.
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("td-subst"), b"bin").unwrap();
        fs::write(store.join("abc.narinfo"), b"StorePath: /td/store/abc").unwrap();

        clear_subst_store(&store).unwrap();

        // The whole store is gone and the swap-aside tombstone did not leak.
        assert!(!store.exists());
        assert!(!tomb.exists());
        // Idempotent: clearing an already-absent store is a no-op Ok.
        clear_subst_store(&store).unwrap();
        assert!(!store.exists());
        // The too-shallow guard still fires (a bare `$HOME` is not a substitute store).
        assert!(clear_subst_store(Path::new("/a")).is_err());
        let _ = fs::remove_dir_all(env::temp_dir().join(format!("td-subst-clear-{}", process::id())));
    }

    // The coarse GC evicts the whole build-cache when it exceeds the cap, and does so
    // atomically (rename to a `build-cache.evicting.*` tombstone, then reap) so a crash
    // can never leave a torn store/db/receipts triple — and a stale tombstone from a
    // previous interrupted eviction is reaped too. Cap is injected (not read from env) so
    // this stays deterministic under the parallel test runner. The under-cap survival case
    // is covered by setup_shares_only_the_build_cache_*; the env knob by cache_cap_prefers_*.
    #[test]
    fn evict_over_cap_removes_the_build_cache_and_reaps_tombstones() {
        let lw = env::temp_dir().join(format!("td-ladder-evict-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        fs::create_dir_all(lw.join("build-cache").join("store")).unwrap();
        fs::write(
            lw.join("build-cache").join("store").join("big-rung"),
            vec![0u8; 4096],
        )
        .unwrap();
        // A tombstone a prior interrupted eviction abandoned — reaped regardless of cap.
        fs::create_dir_all(lw.join("build-cache.evicting.999999")).unwrap();

        let runner = shared_test_runner(&lw);
        runner.evict_build_cache_if_over_watermark(512).unwrap();

        assert!(!lw.join("build-cache").exists());
        assert!(!lw.join("build-cache.evicting.999999").exists());

        // Under-cap: the reap still runs, but the build-cache is left intact.
        fs::create_dir_all(lw.join("build-cache").join("store")).unwrap();
        fs::create_dir_all(lw.join("build-cache.evicting.111111")).unwrap();
        runner
            .evict_build_cache_if_over_watermark(64 * 1024 * 1024)
            .unwrap();
        assert!(lw.join("build-cache").join("store").is_dir());
        assert!(!lw.join("build-cache.evicting.111111").exists());
        let _ = fs::remove_dir_all(&lw);
    }

    // The commit lock is sited BESIDE build-cache/, so eviction (which renames build-cache/
    // aside and recreates it) leaves the lock file — and its inode — untouched. The builder's
    // commit transaction and GC therefore always contend on ONE stable inode; that stable
    // exclusion is what lets GC block an uncovered committer (and vice versa). Without the
    // sibling placement, an evict/recreate would mint a new lock inode and split the lock.
    #[test]
    fn commit_lock_survives_eviction_and_stays_beside_the_cache() {
        use std::os::unix::fs::MetadataExt;
        let lw = env::temp_dir().join(format!("td-ladder-locklife-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        fs::create_dir_all(lw.join("build-cache").join("store")).unwrap();
        fs::write(
            lw.join("build-cache").join("store").join("big-rung"),
            vec![0u8; 4096],
        )
        .unwrap();

        let runner = shared_test_runner(&lw);
        let lock_path = runner.cache_commit_lock_path();
        // Sibling of build-cache/, not inside it.
        assert_eq!(lock_path.parent(), Some(lw.as_path()));
        assert!(!lock_path.starts_with(lw.join("build-cache")));

        // Materialize the lock file as the builder's first commit would (acquire + release),
        // then record its identity. Not held across evict — evict takes the SAME lock, and one
        // process holding it via two descriptions would self-deadlock.
        drop(lock_file(&lock_path).unwrap());
        let ino_before = fs::metadata(&lock_path).unwrap().ino();

        runner.evict_build_cache_if_over_watermark(512).unwrap();
        assert!(!lw.join("build-cache").exists(), "over-cap cache evicted");
        assert!(lock_path.exists(), "the commit lock survives eviction");
        assert_eq!(
            fs::metadata(&lock_path).unwrap().ino(),
            ino_before,
            "same lock inode across eviction — the lock is not split"
        );

        // The stable path mutually excludes — the exclusion both the builder and GC rely on.
        let held = lock_file(&lock_path).unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(contender.try_lock().is_err(), "commit lock is exclusive while held");
        drop(held);
        assert!(contender.try_lock().is_ok(), "released once the holder drops");
        let _ = fs::remove_dir_all(&lw);
    }

    // The point of the shared mode: two builds hold the ladder at once. Under the
    // exclusive lock this pair serialized, which is what made every recipe check on
    // the machine — across all worktrees — run one at a time.
    #[test]
    fn a_shared_ladder_admits_a_second_holder_and_still_excludes_a_writer() {
        // Only the sibling `<lw>.lock` is materialized here — `lw` itself is never
        // created, so there is no tree to reap. The name must still be unique:
        // `setup_preserves_all_persisted_ladder_state` owns `td-ladder-shared-<pid>`
        // and these run concurrently in one test binary.
        let lw = env::temp_dir().join(format!("td-ladder-sharemode-{}", process::id()));
        let lock = ladder_lock_path(&lw);
        let _ = fs::remove_file(&lock);

        // The fix itself: two builds hold it at once. Under the exclusive lock the
        // second acquisition blocks forever instead.
        let first = lock_ladder(&lock, LadderLock::Shared).unwrap();
        let second = lock_ladder(&lock, LadderLock::Shared).unwrap();
        drop(second);
        drop(first);

        // And the direction that keeps a wipe off a live build: while `clear-store`
        // or the fsck holds the ladder EXCLUSIVE, a build cannot take it shared.
        // Matched as WouldBlock rather than any Err — an ENOLCK/EOPNOTSUPP filesystem
        // would satisfy `is_err()` without anything about sharing being observed.
        let wiper = lock_ladder(&lock, LadderLock::Exclusive).unwrap();
        let build = open_lock_file(&lock).unwrap();
        assert!(
            matches!(build.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock)),
            "a build must not join a ladder held exclusively"
        );
        drop(wiper);
        // BLOCKING, as every production caller is. A `try_lock_shared` here fails
        // intermittently with WouldBlock while `/proc/locks` shows the inode already
        // free: releasing on close is not ordered against a lock request on another
        // descriptor. The claim is that a build is admitted, not how fast — so this
        // waits, and a regression that never admits hangs rather than passing.
        drop(build);
        drop(lock_ladder(&lock, LadderLock::Shared).unwrap());

        let _ = fs::remove_file(&lock);
    }

    // A run takes the ladder ONCE and holds it across setup and the build, so there is
    // no window in which it holds nothing and `clear-store` could wipe the tree under
    // it. What it holds is shared, so a peer build joins.
    //
    // The cap is passed EXPLICITLY, not read from the env: with
    // `TD_CHECK_LADDER_CACHE_CAP_BYTES` set the run would take the ladder exclusively
    // and the peer acquire below — a blocking `lock_shared` on a second descriptor of
    // a file this process already holds exclusively — would deadlock. A gate that
    // hangs with no output is worse than one that fails, and the sibling
    // `setup_preserves_all_persisted_ladder_state` guards the same knob the same way.
    #[test]
    fn a_run_holds_one_shared_ladder_across_setup_and_build() {
        let lw = env::temp_dir().join(format!("td-run-lock-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let _ = fs::remove_file(ladder_lock_path(&lw));
        let runner = shared_test_runner(&lw);

        let held = lock_ladder_for_run_with_cache_cap(&runner, None).unwrap();

        // setup() ran under that same hold: this invocation's private scratch exists.
        assert!(runner.scratch.is_dir(), "setup() ran while the ladder was held");
        // A peer build joins it.
        let peer = lock_ladder(&runner.lock_path(), LadderLock::Shared).unwrap();
        drop(peer);
        drop(held);

        let _ = fs::remove_file(ladder_lock_path(&lw));
        let _ = fs::remove_dir_all(&lw);
    }

    // Eviction renames build-cache/ aside, which a concurrent reader of that cache
    // cannot survive — so an armed cap takes the ladder exclusively. The default is no
    // cap, which is the shared hot path. Tied to the parser rather than restating it:
    // `0` disarms eviction and so must land on the shared side.
    #[test]
    fn ladder_lock_mode_is_shared_unless_eviction_is_armed() {
        assert_eq!(ladder_lock_mode(None), LadderLock::Shared);
        assert_eq!(ladder_lock_mode(Some(1)), LadderLock::Exclusive);
        assert_eq!(ladder_lock_mode(parse_cache_cap(Some("0"))), LadderLock::Shared);
        assert_eq!(ladder_lock_mode(parse_cache_cap(Some("4096"))), LadderLock::Exclusive);
    }

    // `clear-store` must take the SEED store's commit lock as well as the cache's, and it
    // names that lock by a constant while the committer derives it from the db path. A
    // drift between the two is a clear racing an orphaned intern with nothing to say so,
    // so pin the constant against the derivation `lock_store_commit` performs.
    #[test]
    fn the_seed_commit_lock_constant_is_what_the_committer_derives() {
        let lw = Path::new("/tmp/some-ladder");
        let db = seed_db_path(lw).unwrap();
        // lock_store_commit anchors on the db's PARENT and appends `.commit.lock`.
        let parent = db.parent().unwrap();
        let mut derived = parent.as_os_str().to_os_string();
        derived.push(".commit.lock");
        assert_eq!(PathBuf::from(derived), lw.join(SEED_COMMIT_LOCK_BASENAME));
        // And it is a sibling of seed-db/, never inside it — the eviction/rename argument.
        assert_eq!(lw.join(SEED_COMMIT_LOCK_BASENAME).parent(), Some(lw));
    }

    // `verify-store` fscks EVERY pin table's seed db, not just the running branch's:
    // the store is shared and unkeyed, so an item only a peer table registers is still
    // this ladder's and must still be re-hashable. The generator's disposable db is
    // excluded — it is a dotfile and holds no authority.
    #[test]
    fn the_fsck_covers_every_keyed_seed_db_and_not_the_generators() {
        let lw = env::temp_dir().join(format!("td-fsck-scope-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let runner = shared_test_runner(&lw);
        let dir = runner.db.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        // No seed-db dir yet ⇒ nothing to verify, not an error.
        assert!(runner.seed_dbs().unwrap().is_empty());

        fs::write(&runner.db, b"ours").unwrap();
        let peer = dir.join("00112233aabbccdd.db");
        fs::write(&peer, b"a peer branch's table").unwrap();
        fs::write(dir.join(".generator-seed-digests-7.db"), b"disposable").unwrap();
        fs::write(dir.join("notes.txt"), b"not a db").unwrap();

        let found = runner.seed_dbs().unwrap();
        assert!(found.contains(&runner.db), "our own db must be fscked: {found:?}");
        assert!(found.contains(&peer), "a peer table's db must be fscked too: {found:?}");
        assert_eq!(found.len(), 2, "only the authoritative dbs: {found:?}");
        // Sorted, so a failure names the same db run to run.
        let mut sorted = found.clone();
        sorted.sort();
        assert_eq!(found, sorted);
        let _ = fs::remove_dir_all(&lw);
    }

    // The TABLE GENERATOR must not write into the db any run authenticates. It interns
    // what it derives, and during a pin bump that is precisely a basename the current
    // table does not pin — one row `authenticate_seed_db` rejects a whole db over, with
    // no prune left to heal it. So its db is a separate, disposable file; and it stays
    // under the keyed dbs' parent, because `lock_store_commit` derives the shared seed
    // store's commit lock from that parent and the generator interns into that store.
    #[test]
    fn the_table_generator_writes_beside_the_keyed_dbs_but_never_into_one() {
        let lw = Path::new("/tmp/some-ladder");
        let keyed = seed_db_path(lw).unwrap();
        let generator = generator_db_path(lw, "seed-digests-4242");
        assert_ne!(generator, keyed, "the generator must not write the keyed db");
        assert_eq!(generator.parent(), keyed.parent(), "same commit-lock parent");
        // Distinct per run, so two generators cannot merge into one file.
        assert_ne!(generator, generator_db_path(lw, "seed-digests-4243"));
    }

    // The seed store moved with the keyed db, and that rename IS the rollout barrier:
    // an old binary still runs `seed-prune` over `<lw>/store` and deletes trees whose
    // basename ITS table does not pin — which a keyed db here may vouch. It cannot
    // reach a path it does not know.
    #[test]
    fn the_seed_store_is_not_the_path_an_old_pruner_deletes_from() {
        let lw = Path::new("/tmp/some-ladder");
        assert_eq!(seed_store_dir(lw), lw.join("seed-store"));
        assert_ne!(seed_store_dir(lw), lw.join("store"));
        // Under the ladder, so `clear-store` still reclaims it wholesale.
        assert_eq!(seed_store_dir(lw).parent(), Some(lw));
    }

    // The seed db is keyed by the compiled table's ROW SET, which is what lets branches
    // with different pins share one machine-wide ladder without pruning each other's
    // seeds. Keyed on the rows, not the file: a comment edit must not fork a db.
    #[test]
    fn the_seed_db_is_keyed_by_the_pin_table_and_sits_under_the_ladder() {
        let lw = Path::new("/tmp/some-ladder");
        let db = seed_db_path(lw).unwrap();
        assert_eq!(db.parent(), Some(lw.join("seed-db").as_path()));
        assert!(db.extension().is_some_and(|e| e == "db"));
        // Stable across calls, and the digest is over the parsed rows.
        assert_eq!(db, seed_db_path(lw).unwrap());
        let digest = crate::seed_digests::table_digest().unwrap();
        assert_eq!(digest.len(), 64, "a sha256 hex digest");
        assert!(db.to_string_lossy().contains(digest.get(..16).unwrap_or("")));
        // The STORE is deliberately NOT keyed — content-addressed items are shared.
        assert_eq!(seed_store_dir(lw), shared_test_runner(lw).store);
    }

    // setup() never wipes the seed store/db: it retains it across runs. A from-stage0
    // clean-room run is an explicit `clear-store` first, never a side effect of setup().
    #[test]
    fn setup_retains_the_seed_store_across_runs() {
        let lw = env::temp_dir().join(format!("td-ladder-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        fs::create_dir_all(seed_store_dir(&lw)).unwrap();
        fs::write(seed_store_dir(&lw).join("prior-seed"), b"x").unwrap();

        let runner = shared_test_runner(&lw);
        runner.setup().unwrap();

        // The prior run's seed survives — setup() no longer wipes it.
        assert!(seed_store_dir(&lw).join("prior-seed").is_file());
        assert!(runner.scratch.is_dir());
        let _ = fs::remove_dir_all(&lw);
    }

    // The coarse GC's size probe: an exact recursive sum under a generous cap, an
    // early-exit over a tiny cap (so eviction trips), and 0 for a missing tree.
    #[test]
    fn dir_size_capped_sums_files_and_short_circuits_over_cap() {
        let tmp = env::temp_dir().join(format!("td-dirsize-{}", process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a").join("b")).unwrap();
        fs::write(tmp.join("a").join("f1"), vec![0u8; 100]).unwrap();
        fs::write(tmp.join("a").join("b").join("f2"), vec![0u8; 200]).unwrap();
        assert_eq!(dir_size_capped(&tmp, 10_000), 300);
        assert!(dir_size_capped(&tmp, 50) > 50);
        assert_eq!(dir_size_capped(&tmp.join("nope"), 10_000), 0);
        let _ = fs::remove_dir_all(&tmp);
    }

    // The eviction cap is now opt-in: a positive value enables eviction at that cap; zero,
    // garbage, and absent all yield None ⇒ setup() reclaims nothing (no implicit eviction).
    #[test]
    fn cache_cap_is_opt_in_on_a_positive_value_else_none() {
        assert_eq!(parse_cache_cap(Some("4096")), Some(4096));
        assert_eq!(parse_cache_cap(Some("  4096  ")), Some(4096));
        assert_eq!(parse_cache_cap(Some("0")), None);
        assert_eq!(parse_cache_cap(Some("not-a-number")), None);
        assert_eq!(parse_cache_cap(None), None);
    }

    // The retained-seed failure markers get the clear-store recovery line appended (it shows
    // the one shared ladder's path for confirmation); an unrelated error passes through
    // untouched (no spurious hint). Byte-level scan matches raw stderr; the subslice search
    // handles empty/oversized needles.
    #[test]
    fn seed_reset_hint_fires_only_on_retained_seed_failures() {
        let lw = Path::new("/home/u/.td/build-daemon/ladder-shared-v1");
        let db_red = "plan seed db /x/db: provenance rejected: `/td/store/foo' is not a basename \
                      the compiled seed-digest table pins";
        let torn = "store-add-recursive foo failed\nstderr:\nstore item /x exists but hashes \
                    sha256:aa, expected sha256:bb — corrupt content-addressed item; refusing to \
                    re-register it (re #469)";
        // `auto_seed_provenance` reds surface only in build-plan stderr; both wordings clear
        // with the same reset, so the byte scan matches them too.
        let auto_missing = "--auto: provenance rejected: recipe `foo' input `bar' resolves to \
                            `/td/store/x' but `x' is not interned in the seed store /x/store (re #469)";
        let auto_tampered = "--auto: provenance rejected: the interned bytes content-address to \
                             `/td/store/y' — renamed, self-registered under the wrong address, or \
                             tampered post-intern; origin authority is the calling runner's pins";
        assert!(looks_like_stale_seed(db_red));
        assert!(stale_seed_in(torn.as_bytes()));
        assert!(stale_seed_in(auto_missing.as_bytes()));
        assert!(stale_seed_in(auto_tampered.as_bytes()));
        for hinted in [
            with_seed_reset_hint(db_red.to_string(), lw),
            with_seed_reset_hint(torn.to_string(), lw),
            with_seed_reset_hint(auto_missing.to_string(), lw),
            with_seed_reset_hint(auto_tampered.to_string(), lw),
        ] {
            // The ladder path is shown for the operator's confirmation; no env-var override.
            assert!(hinted.contains("/home/u/.td/build-daemon/ladder-shared-v1"));
            assert!(!hinted.contains("TD_RECIPE_CHECK_WORK"));
        }
        // A TORN seed must be re-derived, so it still says clear-store.
        for torn_case in [torn, auto_missing, auto_tampered] {
            assert!(with_seed_reset_hint(torn_case.to_string(), lw).contains("clear-store"));
        }
        // An UNPINNED one must NOT: clear-store throws away the shared ladder — every
        // other branch's cached rungs with it — and lands on the same red, because
        // discarding bytes cannot correct a table. This is the advice that turns one
        // stale row into hours of everybody's rebuilt rungs.
        let unpinned = with_seed_reset_hint(db_red.to_string(), lw);
        assert!(
            !unpinned.contains("Run `td-recipe-eval clear-store`"),
            "an unpinned seed must not be sent to clear-store: {unpinned}"
        );
        assert!(unpinned.contains("Do NOT `clear-store'"), "{unpinned}");
        assert!(unpinned.contains("seed-digests"), "{unpinned}");

        let unrelated = "ladder: pinned tarball not warm (/x/foo.tar) - run 'td-feed warm sources'";
        assert!(!looks_like_stale_seed(unrelated));
        assert_eq!(with_seed_reset_hint(unrelated.to_string(), lw), unrelated);

        // Subslice search edges: present, absent, empty needle, needle longer than haystack.
        assert!(contains_subslice(b"abcXYZdef", b"XYZ"));
        assert!(!contains_subslice(b"abcdef", b"XYZ"));
        assert!(!contains_subslice(b"abc", b""));
        assert!(!contains_subslice(b"ab", b"abc"));
    }

    // clear-store fails closed on a too-shallow, relative, or `..`-bearing target so the
    // recursive delete can never hit `.`, `/`, `$HOME`, or traverse out of the ladder.
    // A real ladder is >=3 plain segments deep.
    #[test]
    fn clear_store_rejects_unsafe_targets() {
        // Real ladders (>=3 deep) pass.
        assert!(reject_unsafe_clear_target(Path::new("/home/u/.td/build-daemon/ladder-shared-v1")).is_ok());
        assert!(reject_unsafe_clear_target(Path::new("/a/b/c")).is_ok());
        // Too shallow: root, a system dir, and a bare $HOME (`/home/user`, depth two) are refused.
        assert!(reject_unsafe_clear_target(Path::new("/")).is_err());
        assert!(reject_unsafe_clear_target(Path::new("/home")).is_err());
        assert!(reject_unsafe_clear_target(Path::new("/home/user")).is_err());
        assert!(reject_unsafe_clear_target(Path::new("/tmp/ladder")).is_err());
        // Relative and `.`/`..`-bearing targets are refused (traversal can escape the ladder).
        assert!(reject_unsafe_clear_target(Path::new(".")).is_err());
        assert!(reject_unsafe_clear_target(Path::new("relative/path/here")).is_err());
        assert!(reject_unsafe_clear_target(Path::new("/home/user/../../etc")).is_err());
        assert!(reject_unsafe_clear_target(Path::new("/a/b/c/..")).is_err());
    }

    // The sibling lock path APPENDS `.lock` (never `with_extension`, which would truncate a
    // dotted final component and collide two ladders on one lock).
    #[test]
    fn ladder_lock_path_appends_and_never_truncates() {
        assert_eq!(
            ladder_lock_path(Path::new("/x/ladder")),
            Path::new("/x/ladder.lock")
        );
        assert_eq!(
            ladder_lock_path(Path::new("/x/ladder.v2")),
            Path::new("/x/ladder.v2.lock")
        );
    }

    #[test]
    fn build_cache_is_a_distinct_authority_from_the_seed_store() {
        // The opt-in reuse cache MUST live apart from the seed store/db: recipe OUTPUTS
        // committed to the cache must never land in the seed authority (#468
        // authenticates the seed db as seed-only). Assert the cache pair is under
        // build-cache/ and shares no path with the seed store (<lw>/store) or db.
        let lw = Path::new("/example/ladder");
        let (cache_store, cache_db) = build_cache_paths(lw);
        assert_eq!(cache_store, lw.join("build-cache").join("store"));
        assert_eq!(cache_db, lw.join("build-cache").join("db"));
        let seed_store = seed_store_dir(lw);
        let seed_db = lw.join("db");
        assert_ne!(cache_store, seed_store);
        assert_ne!(cache_db, seed_db);
        // Not even nested under the seed store — a fully separate subtree.
        assert!(!cache_store.starts_with(&seed_store));
        assert!(!cache_db.starts_with(&seed_db));
    }

    #[test]
    fn recipe_closure_is_derived_from_catalog_edges() {
        let graph = recipe_closure(&["busybox-test"]).unwrap();
        let stems: Vec<&str> = graph.iter().map(|node| node.stem.as_str()).collect();

        for expected in [
            "stage0",
            "mes",
            "gcc-x86-64-stage2",
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "make-x86-64",
            "busybox-x86-64",
            "busybox-test",
        ] {
            assert!(
                stems.iter().any(|stem| stem == &expected),
                "missing {expected} from busybox-test closure: {stems:?}"
            );
        }

        let busybox_pos = stems
            .iter()
            .position(|stem| stem == &"busybox-x86-64")
            .unwrap();
        let test_pos = stems
            .iter()
            .position(|stem| stem == &"busybox-test")
            .unwrap();
        assert!(
            busybox_pos < test_pos,
            "dependency should be emitted before dependent: {stems:?}"
        );
    }

    /// The real bootstrap graph is host-free: planning provenance ACCEPTS every
    /// real target, because each rung in every target's closure resolves each
    /// input to a catalog recipe output or a pinned seed. This is a regression
    /// guard — a reintroduced host input would red here, before any build. The
    /// `synthetic_recipes_with_forbidden_inputs_are_rejected_at_planning` test
    /// below keeps the negative direction covered.
    #[test]
    fn real_bootstrap_graph_is_host_free_at_planning() {
        for target in [
            "make-test",
            "busybox-test",
            "gcc-x86-64-stage2-test",
            "gcc-x86-64-native-test",
            "gcc-x86-64-self-test",
            // #529 modern-kernel rung + its two new host-tool dependency recipes;
            // each -test pulls its producer's whole closure, so this also covers
            // flex-x86-64, elfutils-x86-64, and linux-x86-64 transitively.
            "flex-x86-64-test",
            "elfutils-x86-64-test",
            "linux-x86-64-test",
            "hello-test",
        ] {
            if let Err(err) = ensure_targets_provenance(&[target]) {
                panic!("{target}: expected host-free provenance to pass, got: {err}");
            }
        }
    }

    /// #469 structural test: a synthetic recipe declaring a host tool, an
    /// absolute host path, or a host-store path is rejected during planning —
    /// on the `inputs` channel AND the `nativeInputs` channel (review finding:
    /// the native channel must not sail through planning and surface later at
    /// lock synthesis). The classifier admits exactly catalog outputs and
    /// pinned seeds; no name, path string, or store prefix is provenance.
    #[test]
    fn synthetic_recipes_with_forbidden_inputs_are_rejected_at_planning() {
        for forbidden in [
            "bash",
            "make",
            "python",
            "/usr/bin/env",
            "/gnu/store/abc123-gcc-toolchain-15.2.0",
        ] {
            for native in [false, true] {
                let recipe = Recipe::mesboot("synthetic-red", "0");
                let recipe = if native {
                    recipe.native_inputs(&[forbidden])
                } else {
                    recipe.inputs_owned(vec![forbidden.to_string()])
                };
                let nodes = vec![RecipeNode {
                    stem: "synthetic-red".to_string(),
                    recipe,
                }];
                let err = classify_graph_inputs(&nodes).unwrap_err();
                assert!(
                    err.starts_with(PROVENANCE_REJECTED) && err.contains(forbidden),
                    "input `{forbidden}' (native={native}): expected a provenance \
                     rejection, got: {err}"
                );
            }
        }
    }

    /// The classifier itself: a non-special, non-pinned input has NO seed
    /// interpretation (the caller rejects it); pinned sources and the special
    /// seed keys still classify as AuditedSeed.
    #[test]
    fn only_pinned_seeds_classify_as_seed_inputs() {
        for tool in ["bash", "coreutils", "sed", "make", "python", "flex"] {
            assert!(
                seed_input_for_recipe_input(tool).unwrap().is_none(),
                "`{tool}' must not classify as a seed input"
            );
        }
        assert!(seed_input_for_recipe_input("stage0-source")
            .unwrap()
            .is_some());
        assert!(seed_input_for_recipe_input("linux-headers-x86-64")
            .unwrap()
            .is_some());
    }

    #[test]
    fn output_lookup_uses_the_current_build_log_only() {
        let tmp = env::temp_dir().join(format!("td-recipe-runner-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let old = tmp.join("build-old.out");
        let current = tmp.join("build-current.out");
        fs::write(&old, "STEP rust-toolchain /td/store/stale-rust\n").unwrap();
        fs::write(&current, "STEP rust-toolchain /td/store/current-rust\n").unwrap();
        let runner = RecipeCheckRunner {
            root: PathBuf::new(),
            tb: PathBuf::new(),
            builder_path: String::new(),
            builder_store: PathBuf::new(),
            builder_db: PathBuf::new(),
            lw: tmp.clone(),
            sources_dir: PathBuf::new(),
            store: PathBuf::new(),
            db: PathBuf::new(),
            recipes: PathBuf::new(),
            scratch: tmp.join("scratch"),
            daemon_dir: None,
            stream_progress: false,
            vouched: std::sync::Mutex::new(None),
            scratch_lock: None,
        };

        let got = runner.ladder_out_from(&current, "rust-toolchain").unwrap();

        assert_eq!(got, tmp.join("scratch/tdstore/current-rust"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_plain_basename_rejects_traversal_and_separators() {
        assert!(is_plain_basename("aaa-name"));
        assert!(!is_plain_basename(""));
        assert!(!is_plain_basename("."));
        assert!(!is_plain_basename(".."));
        assert!(!is_plain_basename("a/b"));
        assert!(!is_plain_basename("../etc"));
        assert!(!is_plain_basename("a b"));
        assert!(!is_plain_basename("a\tb"));
        assert!(!is_plain_basename("a\\b"));
        assert!(!is_plain_basename("..\\etc"));
    }

    // The reuse memo round-trips, and a stale-plan file (header fingerprint != the
    // caller's) is ignored even if it somehow shares the filename — the header is a
    // second, in-band check on top of the fingerprint-in-filename.
    #[test]
    fn build_run_memo_round_trips_and_rejects_a_wrong_fingerprint() {
        let mut steps = BTreeMap::new();
        steps.insert("system-x86-64".to_string(), "aaa-system".to_string());
        steps.insert("linux-x86-64".to_string(), "bbb-linux".to_string());
        let text = serialize_build_run_memo("deadbeef", &steps);
        assert_eq!(parse_build_run_memo(&text, "deadbeef"), Some(steps));
        assert_eq!(parse_build_run_memo(&text, "cafef00d"), None);
        // No header line at all is a miss, never a panic.
        assert_eq!(parse_build_run_memo("", "deadbeef"), None);
    }

    // A corrupted memo row whose basename would traverse out of the cache/tdstore
    // dirs is rejected wholesale (fsck-grade): the whole file is treated as a miss.
    #[test]
    fn parse_build_run_memo_rejects_a_traversal_basename() {
        let evil = "fingerprint deadbeef\nsystem-x86-64 ../../etc/passwd\n";
        assert_eq!(parse_build_run_memo(evil, "deadbeef"), None);
    }

    // The STEP map takes the LAST line for a stem (matching ladder_out_from) and
    // keeps only the basename; a malformed/traversal STEP path is dropped.
    #[test]
    fn parse_step_map_takes_the_last_step_and_the_basename() {
        let log = "STEP gcc /td/store/stale-gcc\n\
                   STEP gcc /td/store/final-gcc\n\
                   STEP glibc /td/store/xyz-glibc\n\
                   STEP trailing /td/store/\n\
                   noise line\n";
        let map = parse_step_map(log);
        assert_eq!(map.get("gcc").map(String::as_str), Some("final-gcc"));
        assert_eq!(map.get("glibc").map(String::as_str), Some("xyz-glibc"));
        // A path whose last component is empty yields no plain basename, so the row
        // is dropped rather than recorded.
        assert_eq!(map.get("trailing"), None);
    }

    // The fingerprint changes if ANY of the evaluator bytes, the builder bytes, or a
    // patch changes; it is stable when nothing changes; a missing patch dir is fine;
    // and the length-delimiting defeats a concatenation collision.
    #[test]
    fn plan_fingerprint_changes_with_any_input_and_tolerates_no_patch_dir() {
        let tmp = env::temp_dir().join(format!("td-fp-test-{}", process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let patches = tmp.join("seed/patches");
        fs::create_dir_all(&patches).unwrap();
        let eval = tmp.join("td-recipe-eval");
        let builder = tmp.join("td-builder");
        fs::write(&eval, b"EVAL-v1").unwrap();
        fs::write(&builder, b"BUILDER-v1").unwrap();
        fs::write(patches.join("a.patch"), b"patch-a").unwrap();
        // A committed cargoLock read relative to the repo root (here `tmp`).
        fs::create_dir_all(tmp.join("recipes/locks/x")).unwrap();
        fs::write(tmp.join("recipes/locks/x/Cargo.lock"), b"lock-v1").unwrap();
        let locks = vec!["recipes/locks/x/Cargo.lock".to_string()];
        let fp = |locks: &[String], srcs: &[String]| {
            plan_fingerprint(&eval, &builder, &patches, &tmp, locks, srcs).unwrap()
        };

        let base = fp(&locks, &[]);
        // Deterministic: same inputs, same fingerprint.
        assert_eq!(base, fp(&locks, &[]));
        // Evaluator change re-keys.
        fs::write(&eval, b"EVAL-v2").unwrap();
        let after_eval = fp(&locks, &[]);
        assert_ne!(base, after_eval);
        // Builder change re-keys.
        fs::write(&builder, b"BUILDER-v2").unwrap();
        let after_builder = fp(&locks, &[]);
        assert_ne!(after_eval, after_builder);
        // Patch change re-keys.
        fs::write(patches.join("a.patch"), b"patch-a2").unwrap();
        let after_patch = fp(&locks, &[]);
        assert_ne!(after_builder, after_patch);
        // A committed cargoLock bump re-keys (the repo-read build input).
        fs::write(tmp.join("recipes/locks/x/Cargo.lock"), b"lock-v2").unwrap();
        let after_lock = fp(&locks, &[]);
        assert_ne!(after_patch, after_lock);
        // Dropping the lock from the closure (no rust rung) re-keys and is stable.
        let nolock = fp(&[], &[]);
        assert_ne!(after_lock, nolock);
        assert_eq!(nolock, fp(&[], &[]));

        // A local_source dir (#469): its in-tree content is read at build time, so
        // editing it MUST re-key. Declaring one re-keys; a byte edit under it
        // re-keys; and it is deterministic when unchanged.
        let srcdir = tmp.join("tests/demo-src");
        fs::create_dir_all(&srcdir).unwrap();
        fs::write(srcdir.join("main.rs"), b"fn main() {}").unwrap();
        let srcs = vec!["tests/demo-src".to_string()];
        let with_src = fp(&[], &srcs);
        assert_ne!(nolock, with_src);
        assert_eq!(with_src, fp(&[], &srcs));
        fs::write(srcdir.join("main.rs"), b"fn main() { /* v2 */ }").unwrap();
        let after_src_edit = fp(&[], &srcs);
        assert_ne!(with_src, after_src_edit);
        // The skipped `target`/`.git` dirs do NOT perturb the fingerprint (they are
        // excluded from the interned content address too).
        fs::create_dir_all(srcdir.join("target")).unwrap();
        fs::write(srcdir.join("target/junk"), b"artifact").unwrap();
        assert_eq!(after_src_edit, fp(&[], &srcs));

        // A missing patch dir is fine (hashes as zero patches) and stable.
        let nopatch = tmp.join("gone");
        let f1 = plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[], &[]).unwrap();
        assert_eq!(
            f1,
            plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[], &[]).unwrap()
        );

        // Length-delimiting: splitting a byte across the eval/builder boundary must
        // NOT collide (naive concatenation would).
        fs::write(&eval, b"ab").unwrap();
        fs::write(&builder, b"c").unwrap();
        let split_a = plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[], &[]).unwrap();
        fs::write(&eval, b"a").unwrap();
        fs::write(&builder, b"bc").unwrap();
        let split_b = plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[], &[]).unwrap();
        assert_ne!(split_a, split_b);

        // A declared-but-missing lock fails closed (never silently ignored).
        assert!(plan_fingerprint(
            &eval,
            &builder,
            &nopatch,
            &tmp,
            &["recipes/locks/gone/Cargo.lock".to_string()],
            &[]
        )
        .is_err());
        // A declared-but-missing local_source dir also fails closed.
        assert!(plan_fingerprint(
            &eval,
            &builder,
            &nopatch,
            &tmp,
            &[],
            &["tests/gone-src".to_string()]
        )
        .is_err());
        let _ = fs::remove_dir_all(&tmp);
    }

    // End-to-end reuse: with a recorded memo AND the durable cache trees present,
    // reuse_build_run COPIES each requested output into tdstore (an independent
    // inode) and returns its staged path. It misses (None → full build) when the
    // fingerprint differs, when a requested output is not in the memo, or when its
    // durable cache tree is absent (evicted / clear-store'd).
    #[test]
    fn build_run_reuse_stages_present_outputs_and_misses_on_absent_or_stale() {
        let lw = env::temp_dir().join(format!("td-reuse-test-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let runner = shared_test_runner(&lw);
        let (cache_store, _db) = runner.build_cache_paths();
        // Two committed outputs live in the durable build cache.
        for (base, body) in [("aaa-system", b"SYS" as &[u8]), ("bbb-linux", b"KRN")] {
            fs::create_dir_all(cache_store.join(base)).unwrap();
            fs::write(cache_store.join(base).join("file"), body).unwrap();
        }
        let mut steps = BTreeMap::new();
        steps.insert("system-x86-64".to_string(), "aaa-system".to_string());
        steps.insert("linux-x86-64".to_string(), "bbb-linux".to_string());
        runner
            .write_build_run_memo("system-x86-64", "fp1", &steps)
            .unwrap();

        // HIT: both outputs staged into tdstore, as independent copies (mutating the
        // staged copy does NOT touch the durable cache tree).
        let staged = runner
            .reuse_build_run("system-x86-64", "fp1", &["system-x86-64", "linux-x86-64"])
            .unwrap()
            .expect("memo hit");
        let tdstore = runner.scratch.join("tdstore");
        assert_eq!(
            staged,
            vec![tdstore.join("aaa-system"), tdstore.join("bbb-linux")]
        );
        assert_eq!(fs::read(tdstore.join("aaa-system").join("file")).unwrap(), b"SYS");
        fs::write(tdstore.join("aaa-system").join("file"), b"TAMPERED").unwrap();
        assert_eq!(
            fs::read(cache_store.join("aaa-system").join("file")).unwrap(),
            b"SYS",
            "the durable cache tree must not share an inode with the staged copy"
        );

        // MISS: a different fingerprint has no memo file.
        assert!(runner
            .reuse_build_run("system-x86-64", "fp2", &["system-x86-64"])
            .unwrap()
            .is_none());
        // MISS: a requested output not recorded in the memo.
        assert!(runner
            .reuse_build_run("system-x86-64", "fp1", &["busybox-x86-64"])
            .unwrap()
            .is_none());
        // MISS: the durable cache tree was evicted since it was recorded.
        fs::remove_dir_all(cache_store.join("bbb-linux")).unwrap();
        assert!(runner
            .reuse_build_run("system-x86-64", "fp1", &["linux-x86-64"])
            .unwrap()
            .is_none());
        // MISS: a non-directory squats the recorded basename (corruption) — a bare
        // `exists()` would accept it; the real-dir gate rejects it and rebuilds.
        fs::write(cache_store.join("bbb-linux"), b"not-a-dir").unwrap();
        assert!(runner
            .reuse_build_run("system-x86-64", "fp1", &["linux-x86-64"])
            .unwrap()
            .is_none());
        // MISS: a SYMLINK-to-directory squats the basename. `Path::is_dir` follows
        // and would accept it (then copy_tree would recreate the link and stage an
        // alias); the `symlink_metadata` gate rejects it and rebuilds.
        fs::remove_file(cache_store.join("bbb-linux")).unwrap();
        let external = lw.join("external-dir");
        fs::create_dir_all(external.join("payload")).unwrap();
        std::os::unix::fs::symlink(&external, cache_store.join("bbb-linux")).unwrap();
        assert!(runner
            .reuse_build_run("system-x86-64", "fp1", &["linux-x86-64"])
            .unwrap()
            .is_none());
        let _ = fs::remove_dir_all(&lw);
    }

    // Per-fingerprint memo maps coexist and are NOT reaped: `lw` is shared across
    // worktrees whose distinct binaries fingerprint differently, so a write for one
    // fingerprint must never delete another's live map (a concurrent worktree's).
    #[test]
    fn write_build_run_memo_keeps_other_fingerprint_maps() {
        let lw = env::temp_dir().join(format!("td-keepmap-test-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let runner = shared_test_runner(&lw);
        let dir = lw.join("build-run-memo");
        let mut steps = BTreeMap::new();
        steps.insert("system-x86-64".to_string(), "aaa-system".to_string());

        runner.write_build_run_memo("system-x86-64", "aaa", &steps).unwrap();
        runner.write_build_run_memo("system-x86-64", "bbb", &steps).unwrap();
        runner.write_build_run_memo("busybox-x86-64", "ccc", &steps).unwrap();

        assert!(
            dir.join("system-x86-64.aaa.map").exists(),
            "a prior-fingerprint map for the same target must survive (could be a live worktree's)"
        );
        assert!(dir.join("system-x86-64.bbb.map").exists());
        assert!(dir.join("busybox-x86-64.ccc.map").exists());
        let _ = fs::remove_dir_all(&lw);
    }

    // Build-runs hold the ladder SHARED, so two of them publish memos for the same
    // target at once. With one temp name per target they unlink and rename each
    // other's file: the loser gets ENOENT, or a map is published carrying the other's
    // fingerprint, which reads back as a miss and costs a full climb. Both must land,
    // and each must carry its OWN steps.
    #[test]
    fn concurrent_memo_writes_for_one_target_do_not_clobber_each_other() {
        let lw = env::temp_dir().join(format!("td-memo-race-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        let runner = shared_test_runner(&lw);
        let dir = lw.join("build-run-memo");

        let fps = ["aaa", "bbb", "ccc", "ddd"];
        std::thread::scope(|scope| {
            for fp in fps {
                let runner = &runner;
                scope.spawn(move || {
                    let mut steps = BTreeMap::new();
                    steps.insert("system-x86-64".to_string(), format!("out-{fp}"));
                    runner.write_build_run_memo("system-x86-64", fp, &steps).unwrap();
                });
            }
        });

        for fp in fps {
            let map = dir.join(format!("system-x86-64.{fp}.map"));
            assert!(map.is_file(), "{fp}: memo lost to a concurrent writer");
            // Published under its own fingerprint AND carrying its own bytes — a
            // cross-published map would parse back as a miss.
            let text = fs::read_to_string(&map).unwrap();
            let parsed = parse_build_run_memo(&text, fp)
                .unwrap_or_else(|| panic!("{fp}: memo does not read back for its fingerprint"));
            assert_eq!(parsed.get("system-x86-64").map(String::as_str), Some(&*format!("out-{fp}")));
        }
        let _ = fs::remove_dir_all(&lw);
    }

    // The case the RUN ID exists for, which varying the fingerprint above cannot show:
    // same target, SAME fingerprint, two runs. That is two sandboxed peers of one gate,
    // and it is why the qualifier cannot be the pid — a fresh pid namespace hands both
    // the same low one.
    //
    // Asserted on the NAME rather than by racing two writers, deliberately. A race is
    // only a probability: driving two threads through `write_build_run_memo` passes
    // with the run id deleted, because whether the two renames actually interleave is
    // timing. This distinguishes the fix from its absence every time.
    #[test]
    fn two_runs_on_one_target_and_fingerprint_get_different_memo_temps() {
        let a = build_run_memo_temp_name("system-x86-64", "same-fp", "check-system-1-1");
        let b = build_run_memo_temp_name("system-x86-64", "same-fp", "check-system-1-1.2");
        assert_ne!(a, b, "two live runs must not stage through one temp name");
        // Dotted, so it is never mistaken for a published `.map`, and it does carry the
        // fingerprint — a temp per target alone was the original collision.
        for name in [&a, &b] {
            assert!(name.starts_with('.') && name.ends_with(".tmp"), "{name}");
            assert!(name.contains("same-fp"), "{name}");
        }
        // And the same run asking twice is stable — the temp is unlinked and rewritten,
        // not accumulated per call.
        assert_eq!(
            a,
            build_run_memo_temp_name("system-x86-64", "same-fp", "check-system-1-1")
        );
    }
}
