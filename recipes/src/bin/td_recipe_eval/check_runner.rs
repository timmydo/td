use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
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
/// `main` maps an error carrying it to exit 69 (EX_UNAVAILABLE — the same
/// "nothing can run here" signal td-builder's loop uses for
/// EXIT_UNPROVISIONED): the graph is structurally unbuildable on EVERY host
/// until the rejected input exists as a td recipe output — a bootstrap gap,
/// not a code regression. The cross-process contract is the exit code; this
/// prose never crosses a process boundary as an interface.
pub(crate) const PROVENANCE_REJECTED: &str = "provenance rejected: ";

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
    let scope = args.get(1).map(String::as_str).unwrap_or("daily");
    let index = parse_index(args.get(2))?;
    if args.get(3).is_some() {
        return Err(usage());
    }
    let check_runner = selected_check_runner(stem, scope, index)?;
    // Provenance planning FIRST — before the runner exists, so a rejected
    // graph spawns no subprocess at all (re #469).
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("check", &[stem, scope, &index.to_string()]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
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
    let _lock = lock_file(&runner.lock_path())?;
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
    let _lock = lock_file(&lock_path)?;
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
            let _commit_lock = lock_file(&lw.join(CACHE_COMMIT_LOCK_BASENAME))?;
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
/// recipe check: booting the kernel requires HOST qemu, and the daily gate wraps
/// every recipe check in a host-free `pivot_root` sandbox that exposes only
/// td-built tools by absolute /td/store path — so host qemu is unreachable there
/// (unlike the RustToolchain check, which runs the td-BUILT rustc). Registering it
/// as a daily check would therefore fail on `find_qemu` on every real runner. So
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
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    crate::checks::qemu_boot::run(&runner)
}

/// `td-recipe-eval qemu-boot-erofs [linux-x86-64]` — the read-only-root boot proof
/// (re #549). Same host-side boot as `qemu-boot`, but it also builds a probe erofs
/// image with the control-plane `mkfs-erofs` writer (#548) and attaches it as a
/// read-only virtio-blk disk; the guest /init mounts it read-only and the tool
/// asserts the erofs marker. Host-side (never a gated check) for the same reason
/// `qemu-boot` is — the daily sandbox has no host qemu. See checks/qemu_boot.rs.
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
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans
    // a killed erofs boot's per-boot directories.
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    crate::checks::qemu_boot::run_erofs(&runner)
}

/// `td-recipe-eval qemu-boot-system [system-x86-64]` — the headless two-stage boot proof
/// (re #550). Builds the `linux-x86-64` bzImage and the `system-x86-64` stage-1 init.cpio
/// and real-root tree, packs the tree into a read-only erofs image with the control-plane
/// `mkfs-erofs` writer (#548), and boots the two stages under host qemu with the autotest
/// token on the kernel cmdline: stage-1 mounts the erofs root read-only over virtio-blk and
/// `switch_root`s into it, the real-root init reaches the greeter, and the greeter self-
/// exits so the VM powers off. It asserts the greeter, a read-only erofs `/`, tmpfs-backed
/// writable dirs, and a clean power-off. UNLIKE `run`, this is a PASS/FAIL smoke test with
/// no interactive terminal — host-side (never a gated check) for the same reason `qemu-boot`
/// is: the daily sandbox has no host qemu. See checks/qemu_boot.rs.
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
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans
    // a killed system boot's per-boot directories (it can hold a multi-GiB kernel build).
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    crate::checks::qemu_boot::run_system(&runner)
}

/// `td-recipe-eval qemu-boot-kexec [kexec-spike-x86-64]` — the Phase-0 kexec spike proof.
/// Builds the `kexec-spike-x86-64` two-kernel artifact (a bootable bzImage + an outer
/// initramfs embedding static busybox, td-kexec, a second-boot bzImage, and a nested inner
/// initramfs) and boots it under host qemu (TCG): the outer /init prints STAGE1 then execs
/// td-kexec to kexec_file_load(2)+reboot(KEXEC) the inner kernel, whose /init prints STAGE2.
/// It asserts STAGE2 reached (the kexec worked). Host-side (never a gated check) for the
/// same reason `qemu-boot` is: the daily sandbox has no host qemu. See checks/qemu_boot.rs.
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
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    // Reuse the `qemu-boot-` scratch prefix so the stale-scratch reaper still cleans a
    // killed kexec boot's per-boot directories (it can hold a multi-GiB kernel build).
    let scratch_name = scratch_name("qemu-boot", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    crate::checks::qemu_boot::run_kexec(&runner)
}

/// `td-recipe-eval run [system-x86-64]` — the interactive distro runner (re #541).
/// Builds the `system-x86-64` initramfs (its closure pulls in the `linux-x86-64`
/// bzImage) and boots it under host qemu with an interactive serial console. Like
/// `qemu-boot`, this is a host-side command run OUTSIDE the daily sandbox (which
/// has no host qemu and no terminal), never a gated check. See checks/run.rs.
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
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("run", &[stem]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?.with_streamed_progress();
    let lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    // The interactive boot runs unbounded (until the operator quits qemu), so hand the
    // ladder lock to the runner: it releases it after the build, before the boot, so the
    // whole ladder is not blocked for the entire session (re #541, Codex review). setup()
    // above and the build inside run() still hold it.
    crate::checks::run::run(&runner, lock)
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
    let _lock = lock_file(&runner.lock_path())?;
    {
        let _t = timed_phase("harness setup");
        runner.setup()?;
    }
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
        for input in recipe
            .inputs
            .iter()
            .chain(recipe.native_inputs.iter())
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
    let runner = RecipeCheckRunner::new(root, "seed-digests")?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    let mut rows: BTreeMap<String, String> = BTreeMap::new();
    for input in catalog_seed_universe()? {
        let derived = runner.derive_seed_input(&input)?;
        rows.insert(
            input.key().to_string(),
            path_basename_str(&derived)?.to_string(),
        );
    }
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

fn usage() -> String {
    "usage: check-run STEM [pr|daily|all] [INDEX]".to_string()
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

fn parse_tier(arg: &str) -> Result<Option<td_recipe::types::CheckTier>, String> {
    match arg {
        "all" => Ok(None),
        "pr" => Ok(Some(td_recipe::types::CheckTier::Pr)),
        "daily" => Ok(Some(td_recipe::types::CheckTier::Daily)),
        other => Err(format!(
            "unknown check tier '{other}' (expected pr|daily|all)"
        )),
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

/// The trailing `-<pid>` a `scratch_name` appends. Returns the pid iff the last
/// `-`-separated component is a non-empty all-ASCII-digit run — so a reaper can tell
/// an abandoned scratch tree apart from any other directory. None for anything that
/// does not end in a numeric pid (never touched by the reaper).
fn trailing_pid(name: &str) -> Option<u32> {
    let last = name.rsplit('-').next()?;
    if last.is_empty() || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    last.parse::<u32>().ok()
}

/// A pid is live iff `/proc/<pid>` exists. The reaper runs under the ladder lock, so no
/// other same-ladder run is mid-build when it fires; this check's load-bearing job is to
/// never reap OUR OWN just-created scratch, with defense-in-depth for a leftover tree of
/// some still-alive process. Pid reuse can only make a dead scratch LOOK live (skip →
/// under-reap, harmless); it can never make our live scratch look dead, so an in-progress
/// build is never reaped.
fn pid_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// The pid of a reapable scratch tree, or None. A tree is reapable only if it is one of
/// OUR trees — `scratch_name` emits `build-…-<pid>` / `check-…-<pid>` / `qemu-boot-…-<pid>`
/// / `run-…-<pid>` — AND ends in a numeric pid. The prefix guard means a coincidental
/// sibling such as `gcc-14` or `glibc-241` can never be reaped (belt-and-braces: this dir
/// holds only our scratch trees anyway). The `qemu-boot-` and `run-` prefixes are
/// essential: the host-side qemu-boot and interactive `run` tools create per-boot scratch
/// trees here too, and without them a crashed/killed boot's tree (which can hold a
/// multi-GiB kernel build) would leak forever. Split out so the reaper's eligibility rule
/// is unit-testable.
fn reapable_dead_pid(name: &str) -> Option<u32> {
    if !name.starts_with("build-")
        && !name.starts_with("check-")
        && !name.starts_with("qemu-boot-")
        && !name.starts_with("run-")
    {
        return None;
    }
    trailing_pid(name)
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
/// Deliberately DISTINCT from the seed store/db (`<lw>/store`, `<lw>/db`): those hold
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
/// re-key here too. Every field is length-delimited so no boundary is ambiguous
/// (concatenation cannot collide). `cargo_locks` must be sorted and deduped by
/// the caller; a declared lock that cannot be read fails closed.
fn plan_fingerprint(
    eval: &Path,
    builder: &Path,
    patches_dir: &Path,
    repo_root: &Path,
    cargo_locks: &[String],
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
    Ok(crate::sha256::to_base16(&h.finalize()))
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

fn selected_check_runner(stem: &str, scope: &str, index: usize) -> Result<CheckRunner, String> {
    let tier = parse_tier(scope)?;
    let recipe = catalog::lookup(stem)
        .ok_or_else(|| format!("unknown recipe stem '{stem}' (try `list`)"))?;
    let mut count = 0;
    if let Some(checks) = &recipe.checks {
        for check in checks {
            if tier.map(|t| check.tier == t).unwrap_or(true) {
                count += 1;
                if count == index {
                    return check.runner.ok_or_else(|| {
                        format!(
                            "{stem} check index {index} has no Rust check-runner implementation"
                        )
                    });
                }
            }
        }
    }
    if count == 0 {
        return Err(format!("{stem} has no checks in the requested tier"));
    }
    Err(format!(
        "{stem} has only {count} check(s) in the requested tier; index {index} is out of range"
    ))
}

fn lock_file(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open lock {}: {e}", path.display()))?;
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
}

struct RecipeNode {
    stem: String,
    recipe: Recipe,
}

#[derive(Debug)]
enum SeedInput {
    Stage0 { key: String },
    Source { key: String, pin: SourcePin },
    LinuxHeaders { key: String, arch: &'static str },
    Patch { key: String, patch: String },
}

impl SeedInput {
    fn key(&self) -> &str {
        match self {
            SeedInput::Stage0 { key }
            | SeedInput::Source { key, .. }
            | SeedInput::LinuxHeaders { key, .. }
            | SeedInput::Patch { key, .. } => key,
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
        let store = lw.join("store");
        let db = lw.join("db");
        let scratch = lw.join("scratch").join(scratch_name);
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
        if self.db.exists() {
            self.store_verify_pair(&self.db, &self.store)?;
            checked += 1;
        }
        if cache_db.exists() {
            self.store_verify_pair(&cache_db, &cache_store)?;
            checked += 1;
        }
        if checked == 0 {
            return Err(format!(
                "nothing to verify: neither the seed store db {} nor the build cache db {} \
                 exists — build the ladder first (e.g. `td-recipe-eval run system-x86-64`)",
                self.db.display(),
                cache_db.display()
            ));
        }
        Ok(())
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
        // Only THIS invocation's private, pid-tagged scratch is (re)created fresh — a stale
        // same-pid tree is a dead predecessor's leftover, never persisted store state.
        remove_path_if_exists(&self.scratch)?;
        fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("mkdir {}: {e}", self.scratch.display()))?;
        // Reclaim disk from abandoned predecessors' scratch trees; under the ladder lock,
        // so reaping a dead pid's tree never races a live build.
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
    /// dead runs' trees pile up. Runs under the ladder lock (setup holds it), so no other
    /// same-ladder run is mid-build; remove only trees whose trailing `-<pid>` names a DEAD
    /// process. A LIVE pid is ours (never reap our own in-progress scratch) or some other
    /// still-alive process whose tree we defer — so a running build is never reaped. Never
    /// fails setup — any error leaves the tree for a later pass.
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
            match reapable_dead_pid(&name) {
                Some(pid) if !pid_is_alive(pid) => {
                    let _ = fs::remove_dir_all(entry.path());
                }
                _ => {}
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
    /// COLD path (basename not yet interned): RE-DERIVE from the compiled pin — each
    /// intern_* verifies the pinned artifact and interns it into the seed store — then gate
    /// the derived basename against the compiled table before use.
    fn ensure_seed_input(&self, input: &SeedInput) -> Result<String, String> {
        if let Some(base) = crate::seed_digests::expected(input.key())? {
            if self.store.join(base).exists() {
                let derived = format!("{TD_STORE_DIR}/{base}");
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
    fn derive_seed_input(&self, input: &SeedInput) -> Result<String, String> {
        match input {
            SeedInput::Stage0 { key } => self.intern_stage0_source(key),
            SeedInput::Source { key, pin } => self.intern_source(key, pin),
            SeedInput::LinuxHeaders { key, arch } => self.intern_linux_headers(key, arch),
            SeedInput::Patch { key, patch } => self.intern_patch(key, patch),
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
        // Safe under the global ladder lock (build-runs are serialized — no concurrent
        // writer to the cache). The builder commits each rung ATOMICALLY (stage into a
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
            return Err(if stale_seed_in(&stderr_bytes) {
                format!("{base}\n{}", seed_reset_hint(&self.lw))
            } else {
                base
            });
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
    /// self-contained `<system>/root` + `<system>/init.cpio` + `<kernel>/bzImage`
    /// (exactly what it read before this change), loop-userland reads the static
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
    /// bump changes a rust rung's output but no binary, so it MUST re-key here).
    /// Closure-scoped so an unrelated recipe's lock never invalidates this target.
    fn evaluator_fingerprint(&self, target: &str) -> Result<String, String> {
        let eval = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut locks: Vec<String> = recipe_closure(&[target])?
            .iter()
            .filter_map(|n| n.recipe.cargo_lock.clone())
            .collect();
        locks.sort();
        locks.dedup();
        plan_fingerprint(
            &eval,
            &self.tb,
            &self.root.join("seed/patches"),
            &self.root,
            &locks,
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
    /// (`build-run` prints their paths; `run` packs the erofs from
    /// `<system>/root` and reads `<kernel>/bzImage`), and each root output is a
    /// self-contained tree. Two passes: validate ALL outputs first (so a partial
    /// eviction is a clean miss with no half-staged tdstore and no wasted copy),
    /// then stage.
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
        // memo a later run would half-read. Build-runs serialize under the ladder
        // lock, so there is no concurrent writer to race; the temp name is static
        // per target (not pid-tagged) so a crashed run's orphan is reaped here on
        // the next write of this target rather than leaking one per dead pid. `lw`
        // is shared across runs (unlike the per-run scratch tdstore), so this
        // matters more here than for the staging temp.
        //
        // The published `.map` is per fingerprint and is NOT reaped: `lw` (hence
        // this dir) is shared across all worktrees, whose distinct evaluator
        // binaries fingerprint differently, so deleting other-fingerprint maps
        // would clobber a concurrent worktree's live memo. Per-fingerprint maps sit
        // side by side (mirroring the loop-userland map, check_loop.rs); a stale one
        // is inert (its fingerprint never matches, so it is never read) and is
        // cleaned only wholesale by `clear-store`.
        let tmp = dir.join(format!(".{}.tmp", sanitize_target_for_filename(target)));
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

fn recipe_closure(targets: &[&str]) -> Result<Vec<RecipeNode>, String> {
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
/// Every declaration channel is classified: `sourceInput`, `inputs`, AND
/// `nativeInputs`. A rung that declares scaffolding the chain has not built
/// (bash, coreutils, make, …) fails closed with `PROVENANCE_REJECTED` until
/// that tool exists as a recipe output. Deliberately pure — no subprocess, no
/// filesystem — so the entry points run it BEFORE any ambient execution
/// (stage0 placement, interning): a rejected graph executes NOTHING.
fn classify_graph_inputs(nodes: &[RecipeNode]) -> Result<Vec<SeedInput>, String> {
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

fn source_pin_for_key(key: &str) -> Result<SourcePin, String> {
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
    let release = root.join("builder/target/release/td-builder");
    if is_executable(&release) {
        return Ok(release);
    }
    Err(format!(
        "TD_BUILDER_SELF is unset and {} is not executable; run `cargo build --release --manifest-path builder/Cargo.toml`",
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

/// The explicit-reset recovery line, appended when a failure looks like a stale/torn retained
/// seed. setup() no longer wipes the seed store/db, so these red here instead of self-healing.
/// There is one shared ladder, so a bare `clear-store` always targets it; the path is shown
/// for the operator's confirmation.
fn seed_reset_hint(lw: &Path) -> String {
    format!(
        "hint: the ladder's retained seed store/db is stale or torn (a pinned-seed change or an \
         interrupted intern). Run `td-recipe-eval clear-store` to reset the ladder ({}) and \
         re-derive seeds from the compiled pins.",
        lw.display()
    )
}

/// A retained-seed failure marker — a plan-seed-db authentication red
/// (`authenticate_seed_db`/`authenticate_ca_db`: a pinned-seed change, or rows an accumulated
/// cross-branch db can no longer vouch for), a corrupt content-addressed seed item
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
        format!("{err}\n{}", seed_reset_hint(lw))
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
        return Err(format!(
            "{label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
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
        return Err(format!(
            "{label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{label} output not UTF-8: {e}"))
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

fn tail_bytes(bytes: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut selected: Vec<&str> = text.lines().rev().take(lines).collect();
    selected.reverse();
    selected.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(trailing_pid("check-make-test-daily-1-12345"), Some(12345));
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
        // Whatever scratch_name emits, the reaper must recover this pid from it
        // (so our OWN live scratch is always identified as live, never reaped).
        let n = scratch_name("build", &["oyacc"]);
        assert_eq!(trailing_pid(&n), Some(process::id()));
        assert!(pid_is_alive(process::id()));
    }

    #[test]
    fn reapable_dead_pid_requires_our_scratch_prefix() {
        // Our own trees are reapable...
        assert_eq!(reapable_dead_pid("build-oyacc-4059"), Some(4059));
        assert_eq!(reapable_dead_pid("check-make-test-daily-1-12345"), Some(12345));
        // ...including the host-side qemu-boot tool's per-boot scratch (a killed boot's
        // multi-GiB kernel-build tree would otherwise leak forever).
        assert_eq!(reapable_dead_pid("qemu-boot-linux-x86-64-22760"), Some(22760));
        // ...and the interactive `run` tool's per-boot scratch (same multi-GiB leak risk).
        assert_eq!(reapable_dead_pid("run-system-x86-64-31820"), Some(31820));
        // ...but a coincidental numeric-suffixed sibling is NEVER reaped.
        assert_eq!(reapable_dead_pid("gcc-14"), None);
        assert_eq!(reapable_dead_pid("glibc-241"), None);
        assert_eq!(reapable_dead_pid("binutils-244"), None);
        assert_eq!(reapable_dead_pid("build-cache"), None); // the cache dir, no pid
        assert_eq!(reapable_dead_pid("store"), None);
        // And a real scratch name always round-trips (our live tree stays identified).
        assert_eq!(
            reapable_dead_pid(&scratch_name("build", &["oyacc"])),
            Some(process::id())
        );
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
            store: lw.join("store"),
            db: lw.join("db"),
            recipes: scratch.join("recipes"),
            scratch,
            daemon_dir: None,
            stream_progress: false,
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
        fs::create_dir_all(lw.join("store")).unwrap();
        fs::write(lw.join("store").join("seed-item"), b"interned-seed").unwrap();
        fs::write(lw.join("db"), b"this ladder's registered seed rows").unwrap();

        let runner = shared_test_runner(&lw);
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
        assert!(lw.join("store").join("seed-item").is_file());
        assert!(lw.join("db").is_file());
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
        fs::create_dir_all(lw.join("store")).unwrap();
        fs::write(lw.join("store").join("seed-item"), b"y").unwrap();
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
        // A populated store: the stashed td-subst binary + a signed narinfo, as the daily publishes.
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

    // setup() never wipes the seed store/db: it retains it across runs. A from-stage0
    // clean-room run is an explicit `clear-store` first, never a side effect of setup().
    #[test]
    fn setup_retains_the_seed_store_across_runs() {
        let lw = env::temp_dir().join(format!("td-ladder-{}", process::id()));
        let _ = fs::remove_dir_all(&lw);
        fs::create_dir_all(lw.join("store")).unwrap();
        fs::write(lw.join("store").join("prior-seed"), b"x").unwrap();

        let runner = shared_test_runner(&lw);
        runner.setup().unwrap();

        // The prior run's seed survives — setup() no longer wipes it.
        assert!(lw.join("store").join("prior-seed").is_file());
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
            assert!(hinted.contains("clear-store"));
            // The ladder path is shown for the operator's confirmation; no env-var override.
            assert!(hinted.contains("/home/u/.td/build-daemon/ladder-shared-v1"));
            assert!(!hinted.contains("TD_RECIPE_CHECK_WORK"));
        }

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
        let seed_store = lw.join("store");
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
        let fp = |locks: &[String]| plan_fingerprint(&eval, &builder, &patches, &tmp, locks).unwrap();

        let base = fp(&locks);
        // Deterministic: same inputs, same fingerprint.
        assert_eq!(base, fp(&locks));
        // Evaluator change re-keys.
        fs::write(&eval, b"EVAL-v2").unwrap();
        let after_eval = fp(&locks);
        assert_ne!(base, after_eval);
        // Builder change re-keys.
        fs::write(&builder, b"BUILDER-v2").unwrap();
        let after_builder = fp(&locks);
        assert_ne!(after_eval, after_builder);
        // Patch change re-keys.
        fs::write(patches.join("a.patch"), b"patch-a2").unwrap();
        let after_patch = fp(&locks);
        assert_ne!(after_builder, after_patch);
        // A committed cargoLock bump re-keys (the repo-read build input).
        fs::write(tmp.join("recipes/locks/x/Cargo.lock"), b"lock-v2").unwrap();
        let after_lock = fp(&locks);
        assert_ne!(after_patch, after_lock);
        // Dropping the lock from the closure (no rust rung) re-keys and is stable.
        let nolock = fp(&[]);
        assert_ne!(after_lock, nolock);
        assert_eq!(nolock, fp(&[]));

        // A missing patch dir is fine (hashes as zero patches) and stable.
        let nopatch = tmp.join("gone");
        let f1 = plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[]).unwrap();
        assert_eq!(f1, plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[]).unwrap());

        // Length-delimiting: splitting a byte across the eval/builder boundary must
        // NOT collide (naive concatenation would).
        fs::write(&eval, b"ab").unwrap();
        fs::write(&builder, b"c").unwrap();
        let split_a = plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[]).unwrap();
        fs::write(&eval, b"a").unwrap();
        fs::write(&builder, b"bc").unwrap();
        let split_b = plan_fingerprint(&eval, &builder, &nopatch, &tmp, &[]).unwrap();
        assert_ne!(split_a, split_b);

        // A declared-but-missing lock fails closed (never silently ignored).
        assert!(plan_fingerprint(&eval, &builder, &nopatch, &tmp, &["recipes/locks/gone/Cargo.lock".to_string()]).is_err());
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
}
