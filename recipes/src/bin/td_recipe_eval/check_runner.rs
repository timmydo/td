use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use td_recipe::{
    catalog, source_pins,
    types::{CheckRunner, Recipe, SourcePin},
};

pub(crate) const TD_STORE_DIR: &str = "/td/store";

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
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    crate::checks::qemu_boot::run(&runner)
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
    let members: HashSet<String> = recipe_closure(&[target])?
        .into_iter()
        .map(|n| n.stem)
        .collect();
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
    ensure_targets_provenance(&[target])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("build", &[target]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
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
/// — AND ends in a numeric pid. The prefix guard means a coincidental sibling such as
/// `gcc-14` or `glibc-241` can never be reaped (belt-and-braces: this dir holds only our
/// scratch trees anyway). The `qemu-boot-` prefix is essential: the host-side qemu-boot
/// tool creates per-boot scratch trees here too, and without it a crashed/killed boot's
/// tree (which can hold a multi-GiB kernel build) would leak forever. Split out so the
/// reaper's eligibility rule is unit-testable.
fn reapable_dead_pid(name: &str) -> Option<u32> {
    if !name.starts_with("build-")
        && !name.starts_with("check-")
        && !name.starts_with("qemu-boot-")
    {
        return None;
    }
    trailing_pid(name)
}

/// The DEDICATED persistent build-output cache (store, db) under the ladder work dir.
/// Deliberately DISTINCT from the seed store/db (`<lw>/store`, `<lw>/db`): those hold
/// interned seed inputs and #468 authenticates the seed db as a seed-only authority, so a
/// recipe OUTPUT committed there would be rejected as an unpinned seed. The cache lives in
/// its own subtree so reuse never pollutes the seed authority. Cold-wiped with the seed
/// store on a pin change (setup's cold path removes the whole `build-cache/`).
fn build_cache_paths(lw: &Path) -> (PathBuf, PathBuf) {
    let base = lw.join("build-cache");
    (base.join("store"), base.join("db"))
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
    store: PathBuf,
    db: PathBuf,
    recipes: PathBuf,
    scratch: PathBuf,
    force_cold: bool,
    /// The REAL daemon runtime dir (`TD_DAEMON_DIR` or the OUTER
    /// `$HOME/.td/build-daemon`), forwarded to spawned td-builders whose HOME
    /// is re-pointed at the ladder work dir — the derived blessed-seed-db
    /// lookup (re #469 round-8) keys on this dir, and without the forward it
    /// would resolve under the ladder HOME where nothing was ever blessed.
    daemon_dir: Option<String>,
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
        let chain_cache = match env::var("TD_CHECK_CHAIN_CACHE") {
            Ok(v) => v,
            Err(_) => home
                .as_ref()
                .map(|h| h.join(".td/build-daemon/chain").display().to_string())
                .unwrap_or_default(),
        };
        let lw = env::var_os("TD_RECIPE_CHECK_WORK")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if chain_cache.is_empty() {
                    root.join(".td-build-cache/ladder-cold")
                } else {
                    home.map(|h| h.join(".td/build-daemon/ladder"))
                        .unwrap_or_else(|| root.join(".td-build-cache/ladder-cold"))
                }
            });
        let store = lw.join("store");
        let db = lw.join("db");
        let recipes = lw.join("recipes");
        let scratch = lw.join("scratch").join(scratch_name);
        Ok(Self {
            root,
            tb,
            builder_path: cb,
            builder_store: stage0_base.join("store"),
            builder_db: stage0_base.join("builder.db"),
            lw,
            store,
            db,
            recipes,
            scratch,
            force_cold: chain_cache.is_empty()
                && env::var_os("TD_RECIPE_CHECK_PRESERVE_WORK").is_none(),
            daemon_dir,
        })
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.lw.with_extension("lock")
    }

    /// This runner's private per-invocation scratch directory, freshly created by
    /// `setup()` under the ladder work dir (NOT world-writable `/tmp`). The qemu
    /// boot tool places its console/diagnostic capture here so those files live on
    /// a private, non-shared path — no cross-user symlink pre-planting is possible.
    pub(crate) fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// This ladder's dedicated build-output cache (store, db) — see `build_cache_paths`.
    fn build_cache_paths(&self) -> (PathBuf, PathBuf) {
        build_cache_paths(&self.lw)
    }

    pub(crate) fn setup(&self) -> Result<(), String> {
        if self.force_cold {
            remove_path_if_exists(&self.lw)?;
        }
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        remove_path_if_exists(&self.scratch)?;
        fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("mkdir {}: {e}", self.scratch.display()))?;
        // Reclaim disk from ABANDONED predecessors: only THIS pid's scratch was ever
        // removed, so dead runs' trees accumulated (95 GB observed). We hold the ladder
        // lock here, so this is race-free (re #469 build speed).
        self.reap_dead_scratch();
        let pinsum = self.setup_pinsum()?;
        let setup_ok = self.lw.join("setup-ok");
        let warm = fs::read_to_string(&setup_ok)
            .map(|s| s == pinsum)
            .unwrap_or(false)
            && self.lw.join("srcs.map").is_file();
        if warm {
            return Ok(());
        }

        remove_path_if_exists(&self.store)?;
        remove_path_if_exists(&self.db)?;
        // Cold-wipe the dedicated build-output cache with the seeds: a pin change
        // invalidates prior outputs, so stale reuse never survives a pin bump.
        remove_path_if_exists(&self.lw.join("build-cache"))?;
        remove_path_if_exists(&self.lw.join("srcs.map"))?;
        // tools.map: nothing writes it anymore (the host-tool resolution it
        // carried is deleted, re #469) — scrub the stale pre-v8 artifact.
        remove_path_if_exists(&self.lw.join("tools.map"))?;
        remove_path_if_exists(&setup_ok)?;
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        File::create(self.lw.join("srcs.map")).map_err(|e| format!("create srcs.map: {e}"))?;
        fs::write(&setup_ok, pinsum).map_err(|e| format!("write {}: {e}", setup_ok.display()))?;
        Ok(())
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

    // v8: the seed-tools lock (and with it every host-tool ladder input) is
    // DELETED — the pinsum keys only what the chain may consume: the pinned
    // sources and the in-repo seed patches. The bump itself wipes every ladder
    // store built with host scaffolding, so nothing tainted is grandfathered.
    fn setup_pinsum(&self) -> Result<String, String> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ladder-setup-v8\n");
        for pin in source_pins::all() {
            bytes.extend_from_slice(pin.key.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(pin.url.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(pin.sha256.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(pin.file.as_bytes());
            bytes.push(b'\n');
        }
        for file in files_with_suffix(&self.root.join("seed/patches"), ".patch")? {
            append_file_bytes(&file, &mut bytes)?;
        }
        Ok(sha256sum(&bytes))
    }

    fn intern_source(&self, intern_name: &str, pin: &SourcePin) -> Result<String, String> {
        validate_source_file_basename(pin)?;
        let file = self.root.join(".td-build-cache/sources").join(&pin.file);
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
            .root
            .join(".td-build-cache/sources")
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
        let tarball = self.root.join(".td-build-cache/sources").join(&pin.file);
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
        let out = command_output(&mut cmd, &format!("store-add-recursive {name}"))?;
        out.lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("store-add-recursive {name} produced no path"))
    }

    fn append_src_map(&self, name: &str, path: &str) -> Result<(), String> {
        let map = self.lw.join("srcs.map");
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&map)
            .map_err(|e| format!("open {}: {e}", map.display()))?;
        writeln!(file, "{name} {path}").map_err(|e| format!("write {}: {e}", map.display()))
    }

    fn map_value_opt(&self, name: &str) -> Result<Option<String>, String> {
        let map = self.lw.join("srcs.map");
        if !map.is_file() {
            return Ok(None);
        }
        let contents =
            fs::read_to_string(&map).map_err(|e| format!("read {}: {e}", map.display()))?;
        for line in contents.lines() {
            let mut cols = line.splitn(2, ' ');
            let key = match cols.next() {
                Some(k) => k,
                None => continue,
            };
            if key == name {
                return Ok(cols.next().map(str::trim).map(str::to_string));
            }
        }
        Ok(None)
    }

    fn stage_tdstore(&self) -> Result<(), String> {
        fs::create_dir_all(self.scratch.join("tdstore"))
            .map_err(|e| format!("mkdir tdstore: {e}"))?;
        let contents = fs::read_to_string(self.lw.join("srcs.map"))
            .map_err(|e| format!("read srcs.map: {e}"))?;
        for line in contents.lines() {
            let mut cols = line.splitn(2, ' ');
            let _name = cols.next();
            if let Some(path) = cols.next() {
                self.stage_store_path(path.trim())?;
            }
        }
        Ok(())
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
        self.ensure_graph_inputs(&graph)?;
        self.emit_recipe_graph(&graph)?;
        self.stage_tdstore()
    }

    /// Classify then realize every input in the graph: `classify_graph_inputs`
    /// (the pure planning pass — see its doc for the #469 trust boundary),
    /// then intern each admitted seed into the ladder store.
    fn ensure_graph_inputs(&self, nodes: &[RecipeNode]) -> Result<(), String> {
        for input in classify_graph_inputs(nodes)? {
            self.ensure_seed_input(&input)?;
        }
        Ok(())
    }

    /// Realize one classified seed input by RE-DERIVING it from the compiled
    /// pin EVERY run — never by trusting a prior map entry. srcs.map is a
    /// mutable file, so it is a CACHE of derived paths, not an authority: the
    /// warm path used to stage whatever the map named without re-verifying
    /// the declared pin, which let a matching mutable store+db pair vouch for
    /// self-registered host bytes (re #469, PR review). Each intern_* verifies
    /// the pinned artifact and re-interns it (`store-add-recursive` is
    /// idempotent: it NAR-verifies an existing content-addressed item instead
    /// of copying over it), so the derived path is bound to the compiled pin
    /// on every run; a pre-existing map entry must AGREE with the derivation
    /// or planning reds. Cost, stated honestly: per run, each seed's bytes
    /// are read several times (the pin sha256, the NAR hash at synthesis,
    /// and store-add-recursive's NAR verify of the existing item) and stage0
    /// is re-extracted — on the order of the full seed set re-hashed every
    /// warm run, minutes not seconds on a cold cache. Deliberate: the same
    /// recorded re-hash-every-step decision as the StageManifest, trading
    /// warm-run time for a boundary with no trusted mutable state.
    fn ensure_seed_input(&self, input: &SeedInput) -> Result<(), String> {
        let derived = self.derive_seed_input(input)?;
        // The COMPILED table must vouch for the derivation (re #469): pin
        // verification proves the fetched artifact, but a GENERATED seed (the
        // kernel-headers tarball) has no upstream pin — the compiled expected
        // digest is what binds its bytes; and every seed's expected basename
        // being compiled in is what lets td-builder reject a forged map even
        // when invoked directly.
        crate::seed_digests::require(input.key(), path_basename_str(&derived)?)?;
        if reconcile_seed_map_entry(
            input.key(),
            self.map_value_opt(input.key())?.as_deref(),
            &derived,
        )? {
            self.append_src_map(input.key(), &derived)?;
        }
        self.stage_store_path(&derived)
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
        // The auto map is srcs.map verbatim: every non-owned input is an
        // interned seed source. There is no tools map — a host executable is
        // not an admissible input, so build-plan's content-scan candidate dir
        // is the ladder's OWN store of interned seeds, never a host store.
        let srcs = fs::read(self.lw.join("srcs.map")).map_err(|e| format!("read srcs.map: {e}"))?;
        let auto_map = self.scratch.join("auto-map");
        let mut map =
            File::create(&auto_map).map_err(|e| format!("create {}: {e}", auto_map.display()))?;
        map.write_all(&srcs)
            .map_err(|e| format!("write {}: {e}", auto_map.display()))?;

        let home = path_str(&self.lw)?;
        let tmp = path_str(&self.lw)?;
        let builder_store = path_str(&self.builder_store)?;
        let builder_db = path_str(&self.builder_db)?;
        let recipes = path_str(&self.recipes)?;
        let auto_map_s = path_str(&auto_map)?;
        let scratch = path_str(&self.scratch)?;
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env_clear()
            .env("HOME", home)
            .env("TMPDIR", tmp)
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", builder_store)
            .env("TD_BUILDER_DB", builder_db);
        // The derived blessed-seed-db lookup keys on the REAL daemon dir; the
        // ladder HOME override above would otherwise re-point it at a dir
        // where nothing was blessed (re #469 round-8).
        if let Some(d) = &self.daemon_dir {
            cmd.env("TD_DAEMON_DIR", d);
        }
        cmd.arg("build-plan")
            .arg("--auto")
            .arg(target)
            .arg(recipes)
            .arg(auto_map_s)
            .arg(path_str(&self.store)?)
            .arg(path_str(&self.db)?)
            .arg(scratch);
        // Opt-in cross-run reuse (re #469 build speed). Default (TD_CHECK_BUILD_REUSE
        // unset): build-plan owns its own in-run td-store and rebuilds the whole chain
        // from stage0 every run — the clean-room proof. When set, point the chain at a
        // DEDICATED build-output cache (build_cache_paths, under the ladder work dir),
        // kept SEPARATE from the seed store/db (self.store/self.db): each UNCHANGED rung
        // is reused from a prior run (a NAR-verified persistent_realization hit,
        // bit-identical to a fresh build) instead of rebuilt, and a freshly-built rung
        // commits its output back. A CHANGED rung has a different drv ⇒ different output
        // path ⇒ a miss ⇒ still rebuilds, so the rung under development always rebuilds.
        // The cache rides the same setup warmth as the seeds: a pin change cold-wipes it.
        // Safe under the global ladder lock (build-runs are serialized — no concurrent
        // writer to the cache).
        //
        // The cache MUST NOT be self.store/self.db: those are the SEED store/db (interned
        // seed inputs), and #468 authenticates self.db as a seed-only authority — a recipe
        // OUTPUT committed there would be rejected as an unpinned seed. Keeping the cache a
        // distinct store/db pair keeps the seed authority clean and makes reuse compatible
        // with #468 (which then reuses through the same persistent_realization).
        //
        // The toggle is TD_CHECK_BUILD_REUSE (not TD_BUILD_REUSE) so it rides the existing
        // TD_CHECK_ contract: the `td-builder check` sandbox forwards TD_CHECK_* by prefix
        // and check_loop's child allowlist carries it, so the opt-in survives to a gate's
        // in-sandbox `td-recipe-eval check-run` instead of being stripped at the boundary.
        if env::var_os("TD_CHECK_BUILD_REUSE").is_some_and(|v| !v.is_empty()) {
            let (cache_store, cache_db) = self.build_cache_paths();
            cmd.env("TD_PERSIST_STORE", path_str(&cache_store)?)
                .env("TD_PERSIST_DB", path_str(&cache_db)?);
        }
        let out = cmd
            .output()
            .map_err(|e| format!("spawn build-plan --auto {target}: {e}"))?;
        let out_file = self.scratch.join(format!("build-{target}.out"));
        let err_file = self.scratch.join(format!("build-{target}.err"));
        fs::write(&out_file, &out.stdout)
            .map_err(|e| format!("write {}: {e}", out_file.display()))?;
        fs::write(&err_file, &out.stderr)
            .map_err(|e| format!("write {}: {e}", err_file.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{}\nladder: build-plan --auto {target} failed",
                tail_bytes(&out.stderr, 40)
            ));
        }
        io::stdout()
            .write_all(&out.stdout)
            .map_err(|e| format!("write build-plan stdout: {e}"))?;
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
        self.prepare_recipe_target(target)?;
        let build_out = self.build_plan(target)?;
        println!("TD_RECIPE_RUN_WORK {}", self.lw.display());
        println!(
            "TD_RECIPE_RUN_TDSTORE {}",
            self.scratch.join("tdstore").display()
        );
        for output in outputs {
            let path = self.ladder_out_from(&build_out, output)?;
            println!("TD_RECIPE_RUN_OUT {output} {}", path.display());
        }
        Ok(())
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

/// Reconcile a freshly PIN-DERIVED seed path against the srcs.map cache entry
/// (None = key not yet mapped). Returns Ok(true) when the caller should append
/// the new entry, Ok(false) when the cache already agrees, and REDS when the
/// map names a different path: the map is mutable state, so an entry the
/// compiled pin cannot re-derive is exactly the self-registered-host-bytes
/// ingress #469 forbids — never silently prefer either side (re #469).
///
/// Deliberately NO self-heal: rewriting the mismatched entry to the derived
/// value would be safe for the honest causes (a pin bump against a stale
/// work dir; a torn append from an interrupted run) but would also silently
/// absorb tampering, and the map feeds build-plan --auto's lock synthesis
/// downstream. Tampering must be LOUD; the honest causes cost one
/// work-dir delete (the dir is disposable derived state, and the error says
/// exactly that).
fn reconcile_seed_map_entry(
    key: &str,
    prior: Option<&str>,
    derived: &str,
) -> Result<bool, String> {
    match prior {
        None => Ok(true),
        Some(p) if p == derived => Ok(false),
        Some(p) => Err(format!(
            "provenance rejected: srcs.map maps `{key}' to {p}, but the compiled pin derives \
             {derived} — a map entry the pinned seed cannot reproduce is not admissible \
             (self-registered or stale bytes; delete the ladder work dir to re-derive, re #469)"
        )),
    }
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

fn files_with_suffix(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>, String> {
    let mut files = read_dir_sorted(dir)?;
    files.retain(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(suffix))
            .unwrap_or(false)
            && p.is_file()
    });
    Ok(files)
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

    // The srcs.map is a cache, never an authority (re #469): a fresh
    // pin-derivation must agree with a prior entry or planning reds — the
    // mismatch arm is exactly a self-registered/stale item the compiled pin
    // cannot reproduce. Scope, precisely: this exercises the pure helper
    // only. It is the SOLE decision point — ensure_seed_input calls it
    // unconditionally after every intern (the pre-fix warm short-circuit
    // that staged the mapped path without deriving anything is DELETED, not
    // gated), so there is no warm path left to integration-test; the
    // structural guarantee is the absence of any other map read before
    // staging (grep `map_value_opt`).
    #[test]
    fn seed_map_entries_must_agree_with_the_pin_derivation() {
        // Unmapped key: derive and append.
        assert!(reconcile_seed_map_entry("mes-source", None, "/td/store/aaa-mes").unwrap());
        // Cache agrees: no append, no error.
        assert!(!reconcile_seed_map_entry(
            "mes-source",
            Some("/td/store/aaa-mes"),
            "/td/store/aaa-mes"
        )
        .unwrap());
        // Cache names bytes the pin cannot re-derive: provenance rejected.
        let err = reconcile_seed_map_entry(
            "mes-source",
            Some("/td/store/zzz-host-bash"),
            "/td/store/aaa-mes",
        )
        .unwrap_err();
        assert!(err.contains("provenance rejected"), "{err}");
        assert!(err.contains("zzz-host-bash"), "{err}");
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
            store: PathBuf::new(),
            db: PathBuf::new(),
            recipes: PathBuf::new(),
            scratch: tmp.join("scratch"),
            force_cold: false,
            daemon_dir: None,
        };

        let got = runner.ladder_out_from(&current, "rust-toolchain").unwrap();

        assert_eq!(got, tmp.join("scratch/tdstore/current-rust"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
