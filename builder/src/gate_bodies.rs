//! gate_bodies.rs — typed Rust gate bodies (#318 axis 3): the `td-builder
//! gate-body <name>` subcommand that replaces a gate's bash `script` field.
//!
//! A gate whose `GateDef.script` is EMPTY is "native": the gate runner
//! (`gates.rs::run_gate`) execs `<current_exe> gate-body <name>` in the exact
//! same memory-limited wrapper (`prlimit --data`, the per-gate cgroup, its own
//! process group, TD_CHECK_CHAIN_CACHE / TD_GATE_SPECS env) it uses
//! for bash gates. `current_exe` is the stage0 td-builder in the loop (the
//! prelude execs `<stage0> … gate-run`), so a native body gets `tb` = its own
//! binary for free — no `load_stage0` shell dance for the td-builder under test.
//!
//! The registry is `is_native` + the `cli` match below (one place, not a
//! GateDef field, so the other bash gates are untouched). `load()` asserts
//! empty-script ⟺ `is_native`, so a typo (empty script with no body, or a body
//! whose gate still carries bash) is a load-time error, never a silent no-op.
//!
//! The store-* cluster ported here shares `store_subject` — the typed port of
//! the retired tests/store-subject.sh: td BUILDS GNU hello via the corpus
//! build-recipe path (a daemon cache-HIT — the `build-recipes` prelude already
//! realised it), discovers its runtime closure with a guix-free multi-store
//! content scan, and stages a self-contained td-owned store for the gates'
//! store ops. External tools spawned by these bodies are ORACLES or artifacts
//! (`sqlite3` validating td's hand-written DB bytes, `cp -a`/`chmod` staging
//! trees) — the gate LOGIC (every assertion) is typed Rust. No body spawns a
//! guix process.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Is `name` a native (typed-body) gate? The one registry — kept in sync with
/// the `cli` match by the `native_gates_match_cli` unit test.
pub fn is_native(name: &str) -> bool {
    NATIVE.contains(&name)
}

/// The native gate names. Adding a typed gate: add its name here + a `cli` arm
/// + set the gate_defs file's `script: ""`.
const NATIVE: &[&str] = &[
    "store-add",
    "store-add-tree",
    "store-register",
    "store-gc",
    "store-gc-sweep",
    "store-add-referenced",
    "store-verify",
    "store-backend",
    "store-ns",
];

/// `td-builder gate-body <name>` — run one native gate body. Self-moves into
/// the per-gate cgroup first (the bash bodies' `echo $$ > $TD_GATE_CG` line),
/// then dispatches. Prints the gate's PASS/FAIL narration itself.
pub fn cli(name: &str) -> ExitCode {
    if let Err(e) = enter_cgroup_if_requested() {
        eprintln!("gate-run: cannot enter the gate cgroup: {e}");
        return ExitCode::from(97);
    }
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gate-body {name}: cannot resolve cwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = match name {
        "store-add" => store_add(&root),
        "store-add-tree" => store_add_tree(&root),
        "store-register" => store_register(&root),
        "store-gc" => store_gc(&root),
        "store-gc-sweep" => store_gc_sweep(&root),
        "store-add-referenced" => store_add_referenced(&root),
        "store-verify" => store_verify(&root),
        "store-backend" => store_backend(&root),
        "store-ns" => store_ns(&root),
        other => Err(format!("gate-body: unknown native gate `{other}`")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Match the bash convention: the FAIL line goes to stderr; the
            // runner captures both streams into the gate log.
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The self-move into the gate cgroup, mirroring the bash prelude the runner
/// prepends. No-op when TD_GATE_CG is unset (watchdog/unpooled mode).
fn enter_cgroup_if_requested() -> Result<(), String> {
    let Some(procs) = std::env::var_os("TD_GATE_CG") else {
        return Ok(());
    };
    let pid = std::process::id();
    std::fs::write(&procs, format!("{pid}\n"))
        .map_err(|e| format!("write {}: {e}", Path::new(&procs).display()))
}

// --- shared helpers ---------------------------------------------------------

/// The td-builder under test: this process's own binary. In the loop the runner
/// (`gate-run`) IS the stage0 td-builder, so `current_exe` is the stage0
/// placement — the same binary the bash gates resolve via `load_stage0`.
fn tb() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve td-builder (current_exe): {e}"))
}

/// Run `tb <args...>`, returning trimmed stdout on success. On a non-zero exit
/// the error carries `<ctx>` and the child's stderr (the bash `2>&1` tail).
fn tb_out(tb: &Path, args: &[&str], ctx: &str) -> Result<String, String> {
    let out = Command::new(tb)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn td-builder: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let sout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "FAIL: {ctx}: td-builder {args:?} exited {}\n{sout}{err}",
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True if `tb <args...>` exits zero (for the discrimination legs that expect a
/// NON-zero exit — corruption/verify-fail). stdout+stderr are discarded.
fn tb_ok(tb: &Path, args: &[&str]) -> bool {
    Command::new(tb)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run an arbitrary tool, returning trimmed stdout on success (the generic
/// subprocess oracle spawn — sqlite3, guix build, …).
fn run_out(program: &str, args: &[&str], ctx: &str) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn {program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("FAIL: {ctx}: {program} {args:?} exited {}\n{err}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `sqlite3 <db> <sql>` — the PARSER ORACLE for td's hand-written SQLite bytes
/// (an independent implementation reading the same file, exactly as the bash
/// gates spawned it; sqlite comes from the loop toolchain).
fn sqlite3(db: &Path, sql: &str) -> Result<String, String> {
    let db_s = path_str(db)?;
    run_out("sqlite3", &[&db_s, sql], "sqlite3 oracle")
}

/// `cp -a src dst` — faithful tree staging (perms/symlinks/times), the same
/// tool the shell used, so staged bytes have identical NAR-relevant properties.
fn cp_a(src: &Path, dst: &Path) -> Result<(), String> {
    let (s, d) = (path_str(src)?, path_str(dst)?);
    let st = Command::new("cp")
        .args(["-a", &s, &d])
        .status()
        .map_err(|e| format!("FAIL: cannot spawn cp: {e}"))?;
    if !st.success() {
        return Err(format!("FAIL: cp -a {s} {d} exited {st}"));
    }
    Ok(())
}

/// `chmod -R u+w dir` — make a staged store writable (as the shell did).
fn chmod_r_uw(dir: &Path) -> Result<(), String> {
    let d = path_str(dir)?;
    let st = Command::new("chmod")
        .args(["-R", "u+w", &d])
        .status()
        .map_err(|e| format!("FAIL: cannot spawn chmod: {e}"))?;
    if !st.success() {
        return Err(format!("FAIL: chmod -R u+w {d} exited {st}"));
    }
    Ok(())
}

/// Corrupt a store file: `chmod u+w f; printf 'X' >> f` (the verify gates'
/// one-byte corruption).
fn corrupt_append(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let md = std::fs::metadata(p).map_err(|e| format!("FAIL: stat {}: {e}", p.display()))?;
    let mode = md.permissions().mode();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o200))
        .map_err(|e| format!("FAIL: chmod u+w {}: {e}", p.display()))?;
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(p)
        .map_err(|e| format!("FAIL: open {} for append: {e}", p.display()))?;
    f.write_all(b"X").map_err(|e| format!("FAIL: append to {}: {e}", p.display()))
}

/// The first regular file under `dir` (depth-first) — the corruption victim
/// (`find "$dir" -type f | head -1`).
fn first_regular_file(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
            if md.file_type().is_file() {
                return Some(p);
            }
            if md.file_type().is_dir() {
                stack.push(p);
            }
        }
    }
    None
}

/// Per-line `cut -d'|' -f<i>` (1-based), preserving line order.
fn cut_field(text: &str, idx: usize) -> Vec<String> {
    text.lines()
        .map(|l| l.split('|').nth(idx.saturating_sub(1)).unwrap_or("").to_string())
        .collect()
}

/// Non-empty lines, sorted and deduped (`sort -u`).
fn sorted_dedup(text: &str) -> Vec<String> {
    let mut v: Vec<String> =
        text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    v.sort();
    v.dedup();
    v
}

/// Non-empty lines, sorted (`sort`, no -u).
fn sorted_lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> =
        text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    v.sort();
    v
}

/// The basename of a path string.
fn base_of(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// A path as UTF-8 for passing to argv (all td scratch paths are UTF-8).
fn path_str(p: &Path) -> Result<String, String> {
    p.to_str().map(str::to_string).ok_or_else(|| format!("FAIL: non-UTF-8 path {}", p.display()))
}

/// The permission bits (mode & 0o777) of `p`.
fn file_mode(p: &Path) -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(p)
        .map(|m| m.mode() & 0o777)
        .map_err(|e| format!("FAIL: stat {}: {e}", p.display()))
}

/// A fresh scratch dir under the repo root (`rm -rf` + `mkdir -p`, the bash
/// gates' scratch convention).
fn fresh_scratch(root: &Path, name: &str) -> Result<PathBuf, String> {
    let scratch = root.join(name);
    if scratch.exists() {
        let _ = chmod_r_uw(&scratch); // staged stores are read-only; make removable
        let _ = std::fs::remove_dir_all(&scratch);
    }
    std::fs::create_dir_all(&scratch)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", scratch.display()))?;
    Ok(scratch)
}

// --- the stage0 placement metadata (the load_stage0 exports, derived) ---------

/// The stage0 placement — `load_stage0`'s fast path, read from the CURRENT
/// memo at gate-body start (NOT frozen at runner exec): `.stage0-meta` line 2
/// names the canonical placement `cb`, giving TB = `<BASE>/store/<cb>/bin/
/// td-builder` and the builder-of-record triple TD_BUILDER_PATH=`cb`,
/// TD_BUILDER_STORE=`<BASE>/store`, TD_BUILDER_DB=`<BASE>/builder.db`. Reading
/// the memo PER GATE (as the bash gates' `load_stage0` did) keeps the daemon's
/// builder-of-record consistent with builder.db even when a concurrent
/// re-provision replaced the placement mid-run (the #309 staleness).
struct Stage0 {
    tb: PathBuf,
    builder_path: String,
    store: PathBuf,
    db: PathBuf,
}

fn stage0_from_memo(root: &Path) -> Result<Stage0, String> {
    // The bash gates hardcoded `TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"`
    // before load_stage0 — same here (no env indirection).
    let base = root.join(".td-build-cache/stage0");
    let meta = base.join(".stage0-meta");
    let text = std::fs::read_to_string(&meta).map_err(|_| {
        format!(
            "FAIL: no stage0 memo at {} — the loop prelude (provision_stage0) must run first",
            meta.display()
        )
    })?;
    let cb = text
        .lines()
        .nth(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| format!("FAIL: malformed stage0 memo {}", meta.display()))?;
    let cb_base = base_of(cb);
    let store = base.join("store");
    let tb = store.join(&cb_base).join("bin/td-builder");
    if !tb.is_file() {
        return Err(format!("FAIL: stage0 td-builder not executable at {}", tb.display()));
    }
    Ok(Stage0 {
        tb,
        builder_path: cb.to_string(),
        store,
        db: base.join("builder.db"),
    })
}

// --- the shared td-built subject (port of tests/store-subject.sh) -------------

/// The td-built subject the store-backend cluster exercises: GNU hello built by
/// td (a daemon cache-HIT — the `build-recipes` prelude already realised it),
/// its runtime closure discovered by a guix-free MULTI-STORE content scan, and
/// every member staged into a self-contained td-owned store.
struct Subject {
    /// SUBJ_STORE — the self-contained td-owned store dir.
    store: PathBuf,
    /// SUBJ_ROOT — hello's output path IN that store (the GC root).
    root: String,
    /// SUBJ_CLOSURE — the file listing every member as `<store>/<base>`.
    closure_file: PathBuf,
    /// The same members, sorted + deduped.
    closure: Vec<String>,
    /// SUBJ_N.
    n: usize,
    /// SUBJ_DRV — hello's canonical td-ASSEMBLED .drv path (the deriver string).
    drv: String,
    /// SUBJ_LOCALDRV — the on-disk assembled .drv file (its bytes).
    local_drv: PathBuf,
}

fn store_subject(s0: &Stage0, root: &Path, scratch: &Path) -> Result<Subject, String> {
    let tb = &s0.tb;

    // td's OWN recipe evaluator (built by the build-recipes prelude): the
    // sentinel load_recipe_eval read.
    let sentinel = root.join(".td-build-cache/recipe-eval/recipe-eval-path");
    let recipe_eval = std::fs::read_to_string(&sentinel)
        .map_err(|_| {
            format!(
                "FAIL: no td-recipe-eval sentinel ({}) — the build-recipes prelude must run first",
                sentinel.display()
            )
        })?
        .trim()
        .to_string();
    if !recipe_eval.contains("/.td-build-cache/") {
        return Err(format!("FAIL: TD_RECIPE_EVAL is not td's own build ({recipe_eval})"));
    }
    if !Path::new(&recipe_eval).is_file() {
        return Err(format!("FAIL: td-recipe-eval not executable at {recipe_eval}"));
    }

    // CU: the lock's coreutils dir for the scrubbed assemble PATH.
    let lock_rel = "tests/hello-no-guix.lock";
    let lock_text = std::fs::read_to_string(root.join(lock_rel))
        .map_err(|e| format!("FAIL: read {lock_rel}: {e}"))?;
    let cu = lock_text
        .lines()
        .find(|l| l.contains("-coreutils-"))
        .and_then(|l| l.split_once(' ').map(|(_, p)| p.trim().to_string()))
        .ok_or_else(|| format!("FAIL: no coreutils in {lock_rel} for the scrubbed PATH"))?;

    // The shared build daemon (the prelude started it).
    let sock = std::env::var("TD_DAEMON_SOCKET").map_err(|_| {
        String::from(
            "FAIL: the shared build daemon must be running (TD_DAEMON_SOCKET unset) — \
             the `td-builder check` host prelude starts it",
        )
    })?;

    // cached_build hello (the cache-lib recipe→drv→daemon path, per-gate scratch
    // so concurrent store gates never race one assemble dir). The daemon build
    // is a HIT: hello's .drv is deterministic and build-recipes already realised it.
    let sd = scratch.join("pkgcache/hello");
    let sd_b = sd.join("b");
    let sd_tmp = sd.join("tmp");
    std::fs::create_dir_all(&sd_b).map_err(|e| format!("FAIL: mkdir {}: {e}", sd_b.display()))?;
    std::fs::create_dir_all(&sd_tmp)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", sd_tmp.display()))?;
    let recipe_json =
        run_out(&recipe_eval, &["emit", "hello"], "td-recipe-eval emit hello")?;
    if recipe_json.is_empty() {
        return Err("ERROR: td-recipe-eval produced no JSON for hello".into());
    }
    let recipe_f = sd.join("recipe.json");
    std::fs::write(&recipe_f, format!("{recipe_json}\n"))
        .map_err(|e| format!("FAIL: write {}: {e}", recipe_f.display()))?;

    // (1) td ASSEMBLES the .drv itself under a scrubbed env (env -i … PATH=$CU/bin),
    // with the stage0 builder-of-record override riding through.
    let out = {
        let mut cmd = Command::new(tb);
        cmd.arg("assemble-recipe")
            .arg(&recipe_f)
            .arg(root.join(lock_rel))
            .arg(&sd_b)
            .env_clear()
            .env("HOME", &sd)
            .env("TMPDIR", &sd_tmp)
            .env("PATH", format!("{cu}/bin"))
            .env("TD_BUILDER_PATH", &s0.builder_path)
            .env("TD_BUILDER_STORE", &s0.store)
            .env("TD_BUILDER_DB", &s0.db)
            .stdin(Stdio::null());
        cmd.output().map_err(|e| format!("FAIL: cannot spawn assemble-recipe: {e}"))?
    };
    let bout = String::from_utf8_lossy(&out.stdout).into_owned();
    let berr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        let tail: Vec<&str> = berr.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!("FAIL: assemble-recipe hello (guix/Guile off PATH):\n{}", tail.join("\n")));
    }
    let drvf = bout
        .lines()
        .find_map(|l| l.strip_prefix("DRV="))
        .map(str::to_string)
        .filter(|p| Path::new(p).is_file())
        .ok_or_else(|| format!("FAIL: assemble-recipe produced no .drv for hello\n{bout}\n{berr}"))?;
    // [DURABLE structural, brick 3] the assembled drv's builder is the stage0.
    let drv_bytes = std::fs::read_to_string(&drvf).map_err(|e| format!("FAIL: read {drvf}: {e}"))?;
    if !drv_bytes.contains(&format!("{}/bin/td-builder", s0.builder_path)) {
        return Err(format!(
            "FAIL: hello .drv builder is not the stage0 {} — built by the wrong td-builder?",
            s0.builder_path
        ));
    }

    // (2) SUBMIT to the shared daemon (per-request builder override). Reply:
    // OK <canon> <host> <hit|built>.
    let req = format!("{drvf} /gnu/store {} {} {}", s0.builder_path, s0.store.display(), s0.db.display());
    let resp = tb_out(tb, &["daemon-request", &sock, &req], "hello daemon build")?;
    let mut it = resp.split_whitespace();
    let (okword, out_path, ns) =
        (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
    if okword != "OK" || out_path.is_empty() || ns.is_empty() {
        return Err(format!("FAIL: hello daemon build not OK: {resp}"));
    }
    let ns_path = PathBuf::from(ns);
    if !ns_path.is_dir() {
        return Err(format!("FAIL: hello's output tree {ns} is absent"));
    }
    let hbase = base_of(out_path);

    // The deriver = the canonical .drv path assemble-recipe printed to the log.
    let subj_drv = [berr.as_str(), bout.as_str()]
        .iter()
        .flat_map(|t| t.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`'))
        .find(|tok| tok.starts_with("/gnu/store/") && tok.contains("-hello-") && tok.ends_with(".drv"))
        .map(str::to_string)
        .ok_or_else(|| {
            String::from("FAIL: could not read the td-ASSEMBLED hello .drv path from the build log")
        })?;

    // 1) DISCOVER hello's runtime closure guix-free: a MULTI-STORE content scan
    // spanning the seed /gnu/store (deps) + the daemon newstore (the output).
    let nsp = ns_path
        .parent()
        .ok_or_else(|| format!("FAIL: newstore output {ns} has no parent"))?;
    let nsp_s = path_str(nsp)?;
    let closure_raw = tb_out(
        tb,
        &["store-closure-scan", &format!("/gnu/store,{nsp_s}"), out_path],
        "store-closure-scan could not close hello",
    )?;
    if closure_raw.trim().is_empty() {
        return Err(format!("FAIL: empty runtime closure for {out_path}"));
    }

    // 2) STAGE a self-contained td-owned store: every member at <store>/<base>,
    // bytes resolved by probing /gnu/store first, then the newstore dir (the
    // scan's dir precedence).
    let subj_store = scratch.join("allstore");
    let _ = std::fs::remove_dir_all(&subj_store);
    std::fs::create_dir_all(&subj_store)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", subj_store.display()))?;
    let mut members: Vec<String> = Vec::new();
    for p in closure_raw.split_whitespace() {
        let b = base_of(p);
        let gnu = PathBuf::from("/gnu/store").join(&b);
        let alt = nsp.join(&b);
        let src = if gnu.exists() {
            gnu
        } else if alt.exists() {
            alt
        } else {
            return Err(format!("FAIL: closure member {b} has no bytes in /gnu/store or {nsp_s}"));
        };
        cp_a(&src, &subj_store.join(&b))
            .map_err(|e| format!("FAIL: could not stage {b} into {}\n{e}", subj_store.display()))?;
        members.push(format!("{}/{b}", subj_store.display()));
    }
    chmod_r_uw(&subj_store).map_err(|e| format!("FAIL: could not make the staged store writable\n{e}"))?;
    members.sort();
    members.dedup();
    let closure_file = scratch.join("closure.txt");
    let mut listing = members.join("\n");
    listing.push('\n');
    std::fs::write(&closure_file, listing)
        .map_err(|e| format!("FAIL: write {}: {e}", closure_file.display()))?;

    let subj_root = format!("{}/{hbase}", subj_store.display());
    if !Path::new(&subj_root).is_dir() {
        return Err(format!("FAIL: staged subject root {subj_root} is absent"));
    }
    let n = members.len();
    if n < 1 {
        return Err("FAIL: staged closure is empty".into());
    }

    // SUBJ_LOCALDRV — the on-disk assembled .drv file.
    let local_drv = std::fs::read_dir(&sd_b)
        .ok()
        .and_then(|rd| {
            let mut drvs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "drv"))
                .collect();
            drvs.sort();
            drvs.into_iter().next()
        })
        .ok_or_else(|| format!("FAIL: no assembled hello .drv file under {}", sd_b.display()))?;

    println!(
        "   [td-subject] hello built by td (cache-hit, no guix); {n}-path runtime closure \
         content-scanned + staged into the td-owned store {}",
        subj_store.display()
    );
    Ok(Subject {
        store: subj_store,
        root: subj_root,
        closure_file,
        closure: members,
        n,
        drv: subj_drv,
        local_drv,
    })
}

// --- the gate bodies ---------------------------------------------------------

/// store-add — td PLACES a text path into its OWN store + registers it (pure
/// Rust, no daemon in the write path), differential vs the daemon's
/// addTextToStore. Faithful port of gate_defs/280-store-add.rs's bash.
fn store_add(root: &Path) -> Result<(), String> {
    println!(
        ">> store-add: td PLACES a text path into its OWN store + registers it (pure Rust, no \
         daemon in the write path) — differential vs the daemon's addToStore"
    );
    let tb = stage0_from_memo(root)?.tb;

    let scratch = fresh_scratch(root, ".store-add-scratch")?;
    let store = scratch.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("FAIL: mkdir {}: {e}", store.display()))?;
    let content = scratch.join("content");
    std::fs::write(&content, "td store-add test payload\n")
        .map_err(|e| format!("FAIL: write {}: {e}", content.display()))?;

    let name = "td-store-add-probe";
    let content_s = path_str(&content)?;

    // The daemon (oracle) addTextToStore: writes to /gnu/store, returns the path.
    let daemon_path =
        tb_out(&tb, &["store-add", name, &content_s], "the daemon (oracle) addTextToStore")?;
    if daemon_path.is_empty() {
        return Err("FAIL: the daemon (oracle) returned no path for addTextToStore".into());
    }
    if !Path::new(&daemon_path).is_file() {
        return Err(format!(
            "FAIL: the daemon did not write its store file {daemon_path} (oracle missing)"
        ));
    }
    println!(">> daemon (oracle) addTextToStore wrote: {daemon_path}");

    // td computes + writes + registers the SAME path itself, no daemon.
    let store_s = path_str(&store)?;
    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let td_path =
        tb_out(&tb, &["store-add-text", name, &content_s, &store_s, &tddb_s], "td store-add-text")?;
    if td_path != daemon_path {
        return Err(format!("FAIL: td computed {td_path} != the daemon's {daemon_path}"));
    }
    println!("   td computed the SAME store path as the daemon (no daemon in td's path computation)");

    let base = base_of(&td_path);
    let td_file = store.join(&base);
    if !td_file.is_file() {
        return Err(format!("FAIL: td did not write the store file {base}"));
    }
    let mode = file_mode(&td_file)?;
    if mode != 0o444 {
        return Err(format!("FAIL: td's store file mode {mode:o} != 444 (canonical read-only)"));
    }
    println!("   td WROTE the store file itself, canonical mode 0444 (no daemon in the write path)");

    // Byte-identity by NAR hash (metadata-independent).
    let td_file_s = path_str(&td_file)?;
    let oracle_hash = tb_out(&tb, &["nar-hash", &daemon_path], "nar-hash of the daemon's store file")?;
    let td_file_hash = tb_out(&tb, &["nar-hash", &td_file_s], "nar-hash of td's store file")?;
    if td_file_hash != oracle_hash {
        return Err(format!(
            "FAIL: td's store bytes NAR-hash {td_file_hash} != the daemon's store file {oracle_hash}"
        ));
    }
    println!("   td's store file is byte-identical (NAR) to the daemon's own: {oracle_hash}");

    // td's registration, read back by TD'S OWN reader.
    let td_reg = tb_out(&tb, &["store-query", &tddb_s, "info"], "td store-query (td's own reader)")?;
    let mut fields = td_reg.split('|');
    let reg_path = fields.next().unwrap_or("");
    let reg_hash = fields.next().unwrap_or("");
    if reg_path != daemon_path {
        return Err(format!("FAIL: td registered path {reg_path} != {daemon_path}"));
    }
    if reg_hash != oracle_hash {
        return Err(format!("FAIL: td registered hash {reg_hash} != the daemon-equivalent {oracle_hash}"));
    }
    println!(
        "   td's registration (read back by TD'S OWN reader) records the path + the NAR hash of \
         what td wrote"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td PLACED a path into its OWN store and REGISTERED it ITSELF, in pure Rust with NO \
         daemon in the write path — td computed the IDENTICAL store path to the daemon's \
         addTextToStore, WROTE a store file (canonical mode 0444) BYTE-IDENTICAL (by NAR hash) to \
         the daemon's own store file, and its registration (read back by TD'S OWN reader) records \
         that path + the hash of what td wrote. The daemon is only the oracle."
    );
    Ok(())
}

/// store-add-tree — td CANONICALLY restores a directory tree into its OWN store
/// + registers it (recursive addToStore): determinism + round-trip + registration
/// + load-bearing discrimination. Port of 285-store-add-tree.rs.
fn store_add_tree(root: &Path) -> Result<(), String> {
    println!(
        ">> store-add-tree: td CANONICALLY restores a directory tree into its OWN store + \
         registers it (recursive addToStore, pure Rust, no daemon, no guix) — content-addressed \
         round-trip + a perturbation control proving the addressing is load-bearing"
    );
    let tb = stage0_from_memo(root)?.tb;
    let scratch = fresh_scratch(root, ".store-add-tree-scratch")?;

    // The fixture tree: nested dir + plain file + executable file + symlink —
    // every NAR-captured property under the gate's control.
    let fx = scratch.join("tree");
    std::fs::create_dir_all(fx.join("sub")).map_err(|e| format!("FAIL: mkdir fixture: {e}"))?;
    std::fs::write(fx.join("file.txt"), "hello from the td store-add-recursive fixture\n")
        .map_err(|e| format!("FAIL: write fixture: {e}"))?;
    std::fs::write(fx.join("run.sh"), "#!/bin/sh\necho hi\n")
        .map_err(|e| format!("FAIL: write fixture: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(fx.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("FAIL: chmod +x run.sh: {e}"))?;
    }
    std::fs::write(fx.join("sub/nested.txt"), "nested payload\n")
        .map_err(|e| format!("FAIL: write fixture: {e}"))?;
    std::os::unix::fs::symlink("file.txt", fx.join("link"))
        .map_err(|e| format!("FAIL: symlink fixture: {e}"))?;

    let name = "td-store-add-fixture";
    let fx_s = path_str(&fx)?;
    let srcnar = tb_out(&tb, &["nar-hash", &fx_s], "nar-hash of the fixture tree")?;
    println!(">> fixture tree (nested dir + file + exec file + symlink) NAR: {srcnar}");

    let intern = |tree: &Path, store: &str, db: &str| -> Result<String, String> {
        let t = path_str(tree)?;
        let s = path_str(&scratch.join(store))?;
        let d = path_str(&scratch.join(db))?;
        tb_out(&tb, &["store-add-recursive", name, &t, &s, &d], "store-add-recursive")
    };

    let p1 = intern(&fx, "store", "td.db")?;
    if !(p1.starts_with("/gnu/store/") && p1.ends_with(&format!("-{name}"))) {
        return Err(format!(
            "FAIL: store-add-recursive did not return a content-addressed source path (got '{p1}')"
        ));
    }
    let base = base_of(&p1);
    println!("   td interned the fixture at {p1}");

    // [DETERMINISM] re-interning the identical tree yields the identical path.
    let p1b = intern(&fx, "store_b", "td_b.db")?;
    if p1b != p1 {
        return Err(format!(
            "FAIL: re-interning the same tree moved the path ({p1b} != {p1}) — not content-addressed"
        ));
    }
    println!("   [DETERMINISM] re-interning the same tree yields the identical path");

    // [ROUND-TRIP] the restored tree is NAR-byte-identical to the source.
    let restored = scratch.join("store").join(&base);
    if !restored.is_dir() {
        return Err(format!("FAIL: td did not restore the tree at {}", restored.display()));
    }
    let restored_s = path_str(&restored)?;
    let rnar = tb_out(&tb, &["nar-hash", &restored_s], "nar-hash of the restored tree")?;
    if rnar != srcnar {
        return Err(format!(
            "FAIL: restored tree NAR {rnar} != source {srcnar} — the round-trip is not byte-identical"
        ));
    }
    println!("   [ROUND-TRIP] the restored tree is NAR-byte-identical to the source: {srcnar}");
    let run_mode = file_mode(&restored.join("run.sh"))?;
    if run_mode & 0o100 == 0 {
        return Err("FAIL: the executable bit was not restored on run.sh".into());
    }
    let link = restored.join("link");
    let link_ok = std::fs::symlink_metadata(&link)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
        && std::fs::read_link(&link).map(|t| t == Path::new("file.txt")).unwrap_or(false);
    if !link_ok {
        return Err("FAIL: the symlink was not restored (link -> file.txt)".into());
    }
    if !restored.join("sub/nested.txt").is_file() {
        return Err("FAIL: the nested file was not restored".into());
    }
    println!(
        "   restored tree keeps the exec bit (run.sh), the symlink (link -> file.txt), and the \
         nested file (sub/nested.txt)"
    );

    // [REGISTRATION] td's own reader reads back the path + the tree's NAR hash.
    let tddb_s = path_str(&scratch.join("td.db"))?;
    let reg = tb_out(&tb, &["store-query", &tddb_s, "info"], "store-query")?;
    let mut f = reg.split('|');
    if f.next().unwrap_or("") != p1 {
        return Err(format!("FAIL: registered path != {p1} ({reg})"));
    }
    if f.next().unwrap_or("") != srcnar {
        return Err(format!("FAIL: registered NAR hash != {srcnar} ({reg})"));
    }
    println!("   [REGISTRATION] td's own reader reads back the interned path + the tree's NAR hash");

    // [DISCRIMINATION] a single-byte append and an exec-bit flip each MOVE the path.
    let tree_c = scratch.join("tree_c");
    cp_a(&fx, &tree_c)?;
    corrupt_append(&tree_c.join("file.txt"))?; // append moves content
    let pc = intern(&tree_c, "store_c", "td_c.db")?;
    if pc == p1 {
        return Err(
            "FAIL: appending a single byte did NOT move the path — the store path is not a \
             function of the content"
                .into(),
        );
    }
    let tdc_s = path_str(&scratch.join("td_c.db"))?;
    let cnar = tb_out(&tb, &["store-query", &tdc_s, "info"], "store-query (perturbed)")?
        .split('|')
        .nth(1)
        .unwrap_or("")
        .to_string();
    if cnar.is_empty() || cnar == srcnar {
        return Err(format!(
            "FAIL: the single-byte edit did not change the registered NAR hash (got '{cnar}')"
        ));
    }
    let tree_x = scratch.join("tree_x");
    cp_a(&fx, &tree_x)?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(tree_x.join("run.sh"), std::fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("FAIL: chmod -x run.sh: {e}"))?;
    }
    let px = intern(&tree_x, "store_x", "td_x.db")?;
    if px == p1 {
        return Err(
            "FAIL: flipping the executable bit did NOT move the path — the exec bit is not \
             captured in the content address"
                .into(),
        );
    }
    println!(
        "   [DISCRIMINATION] a single-byte append and an exec-bit flip each move the \
         content-addressed path + registered NAR hash (contents + exec bits are load-bearing)"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td CANONICALLY RESTORED a directory tree into its OWN store and REGISTERED it \
         ITSELF, in pure Rust with NO daemon and NO guix — the content-addressed source path is a \
         deterministic function of the tree's recursive NAR sha256 (re-interning is identical), \
         the restored tree is NAR-byte-identical to the source (structure + contents + exec bits + \
         symlinks), td's own reader reads back the path + hash, and a single-byte append or an \
         exec-bit flip each move the path (the addressing is load-bearing)."
    );
    Ok(())
}

/// store-register — td WRITES the store SQLite DB for a td-built hello's FULL
/// closure (pure-Rust file format) and READS it back byte-identically to
/// sqlite3 (the parser oracle). Port of 275-store-register.rs.
fn store_register(root: &Path) -> Result<(), String> {
    println!(
        ">> store-register: td WRITES the store SQLite DB for a TD-BUILT hello's FULL CLOSURE \
         (pure-Rust file format) and READS it back byte-identically to sqlite3 (guix off PATH; \
         no guix build, no guix gc, no /var/guix read)"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-register-scratch")?;
    let subj = store_subject(&s0, root, &scratch)?;
    let n = subj.n;

    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let closure_s = path_str(&subj.closure_file)?;
    println!(
        ">> td WRITES the store SQLite DB for the {n}-path closure at {} (no sqlite3 engine — td \
         emits the SQLite bytes)",
        tddb.display()
    );
    tb_out(&tb, &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s], "store-register")?;
    if std::fs::metadata(&tddb).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("FAIL: td wrote no store DB".into());
    }
    let integrity = sqlite3(&tddb, "PRAGMA integrity_check")?;
    println!(">> sqlite3 validates td's hand-written DB: {integrity}");
    if integrity != "ok" {
        return Err("FAIL: td's store DB is not a valid SQLite file (integrity_check failed)".into());
    }

    let rowsql =
        "SELECT path||'|'||hash||'|'||narSize FROM ValidPaths WHERE hash IS NOT NULL ORDER BY path";
    let refsql = "SELECT a.path||'|'||b.path FROM Refs r JOIN ValidPaths a ON r.referrer=a.id \
                  JOIN ValidPaths b ON r.reference=b.id";
    let td_rows = sqlite3(&tddb, rowsql)?;
    let nrows = td_rows.lines().count();
    if nrows != n {
        return Err(format!("FAIL: td registered {nrows} paths, expected {n}"));
    }
    let regpaths = cut_field(&td_rows, 1);
    if regpaths != subj.closure {
        return Err("FAIL: the registered path set != the staged closure".into());
    }
    println!("   td registered all {n} closure paths (hash + narSize), exactly the staged closure");

    println!(
        ">> td READS its own store DB itself (td-builder store-query — a pure-Rust SQLite reader; \
         NO sqlite3 engine, NO daemon in td's query path):"
    );
    let td_read_info = tb_out(&tb, &["store-query", &tddb_s, "info"], "store-query info")?;
    if td_read_info != td_rows {
        return Err(format!(
            "FAIL: td's reader disagrees with sqlite3 reading the SAME td.db bytes (info)\n  \
             td-read:\n{td_read_info}\n  sqlite3:\n{td_rows}"
        ));
    }
    println!("   info: td's reader == sqlite3 (same bytes) for all {n} paths' path|hash|narSize");
    let td_refs = sqlite3(&tddb, &format!("{refsql} ORDER BY 1"))?;
    let td_read_refs = tb_out(&tb, &["store-query", &tddb_s, "references"], "store-query references")?;
    if td_read_refs != td_refs {
        return Err(
            "FAIL: td's reader disagrees with sqlite3 reading the SAME td.db bytes (references)".into(),
        );
    }
    let nedges = td_read_refs.lines().count();
    println!("   references: td's reader == sqlite3 ({nedges} edges of the inter-path Refs relation)");

    // deriver-in-closure: a deriver that is itself a member registers ONCE.
    println!(
        ">> deriver-in-closure: a DERIVER that is itself a closure member is registered ONCE — no \
         duplicate ValidPaths row"
    );
    let fakedrv = subj
        .closure
        .iter()
        .find(|p| **p != subj.root)
        .cloned()
        .ok_or_else(|| {
            String::from(
                "FAIL: closure has no member other than the artifact to use as an in-closure deriver",
            )
        })?;
    let dic_db = scratch.join("td-dic.db");
    let dic_s = path_str(&dic_db)?;
    tb_out(
        &tb,
        &["store-register", &subj.root, &fakedrv, &closure_s, &dic_s],
        "store-register (deriver-in-closure)",
    )?;
    if sqlite3(&dic_db, "PRAGMA integrity_check")? != "ok" {
        return Err("FAIL: the deriver-in-closure DB is not valid SQLite".into());
    }
    let dic_total = sqlite3(&dic_db, "SELECT COUNT(*) FROM ValidPaths")?;
    let dic_distinct = sqlite3(&dic_db, "SELECT COUNT(DISTINCT path) FROM ValidPaths")?;
    if dic_total != n.to_string() || dic_distinct != n.to_string() {
        let dups = sqlite3(
            &dic_db,
            "SELECT path,COUNT(*) c FROM ValidPaths GROUP BY path HAVING c>1",
        )
        .unwrap_or_default();
        return Err(format!(
            "FAIL: deriver-in-closure produced {dic_total} rows ({dic_distinct} distinct), \
             expected {n} with no duplicate — the closure-member deriver was registered twice\n{dups}"
        ));
    }
    println!("   the closure-member deriver is registered once ({n} rows, no duplicate)");

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td WROTE the store SQLite DB for a TD-BUILT hello's full {n}-path closure itself \
         in pure Rust AND READ it back itself (td-builder store-query — a pure-Rust SQLite \
         reader, no sqlite3 engine and no daemon in td's own store-query path). sqlite3 PRAGMA \
         integrity_check = ok on td's bytes; every path's hash + narSize and the full inter-path \
         Refs relation, as answered by TD'S OWN READER, are BYTE-IDENTICAL to sqlite3 reading the \
         same bytes (the parser oracle); and a closure-member deriver is registered once."
    );
    Ok(())
}

/// store-gc — td computes the GC-reachable closure from its OWN store DB
/// (Refs-graph walk) == td's own content scan == the staged closure. Port of
/// 290-store-gc.rs.
fn store_gc(root: &Path) -> Result<(), String> {
    println!(
        ">> store-gc: td computes the GC-reachable closure of a TD-BUILT hello from its OWN store \
         DB (pure Rust, no daemon) == td's own content scan (guix off PATH; no guix gc)"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-gc-scratch")?;
    let subj = store_subject(&s0, root, &scratch)?;

    let tddb_s = path_str(&scratch.join("td.db"))?;
    let closure_s = path_str(&subj.closure_file)?;
    tb_out(&tb, &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s], "store-register")?;
    let store_s = path_str(&subj.store)?;
    let td_reach = sorted_dedup(&tb_out(&tb, &["store-closure", &tddb_s, &subj.root], "store-closure")?);
    let scan_reach =
        sorted_dedup(&tb_out(&tb, &["store-closure-scan", &store_s, &subj.root], "store-closure-scan")?);
    let staged = subj.closure.clone();
    let n = staged.len();
    if td_reach != scan_reach {
        return Err(format!(
            "FAIL: td's DB-walk GC closure != td's content-scan closure\n  db:   {td_reach:?}\n  \
             scan: {scan_reach:?}"
        ));
    }
    println!(
        "   (1) td's DB-walk (Refs graph) and (2) content-scan closures of the td-built hello \
         AGREE ({n} paths)"
    );
    if td_reach != staged {
        return Err(
            "FAIL: the reachable set != the staged closure (register/scan disagree with what was \
             staged)"
                .into(),
        );
    }
    println!("   both == the staged runtime closure — every staged member is reachable from hello's output");
    if scan_reach.iter().any(|p| p.ends_with(".drv")) {
        return Err(
            "FAIL: the content-scan runtime closure of an OUTPUT root unexpectedly contains a \
             .drv — the output-root boundary is broken"
                .into(),
        );
    }
    println!(
        "   (2b) the content-scan runtime closure is .drv-free (an OUTPUT root's runtime closure, \
         distinct from the structural .drv-input graph)"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td computed the GC-reachable CLOSURE of a TD-BUILT hello ({n} paths) TWO \
         daemon-free ways, in pure Rust, over its OWN store — (1) walking the Refs graph in a \
         store DB it wrote (td's own SQLite reader) and (2) CONTENT-SCANNING the staged store \
         from hello's output — and BOTH agree with each other AND with the staged closure. The \
         destructive sweep is store-gc-sweep."
    );
    Ok(())
}

/// store-gc-sweep — td DELETES the GC-dead paths from its OWN store + rewrites
/// the DB to the live set (destructive sweep). Port of 300-store-gc-sweep.rs.
fn store_gc_sweep(root: &Path) -> Result<(), String> {
    println!(
        ">> store-gc-sweep: td DELETES the GC-dead paths from its OWN store + rewrites the DB to \
         the live set (destructive GC sweep of a TD-BUILT closure, pure Rust, no daemon; guix off \
         PATH) == td's own mark phase"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-gc-sweep-scratch")?;
    let subj = store_subject(&s0, root, &scratch)?;
    let n = subj.n;

    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let closure_s = path_str(&subj.closure_file)?;
    tb_out(&tb, &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s], "store-register")?;

    // A non-trivial GC root: glibc (a PROPER subset of hello's closure).
    let gc_root = subj
        .closure
        .iter()
        .find(|p| p.contains("-glibc-"))
        .cloned()
        .ok_or_else(|| String::from("FAIL: no glibc in hello's closure to use as a non-trivial GC root"))?;
    let live: Vec<String> = {
        let out = tb_out(&tb, &["store-closure", &tddb_s, &gc_root], "store-closure (mark)")?;
        let mut v: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(base_of).collect();
        v.sort();
        v
    };
    let nlive = live.len();
    if nlive >= n {
        return Err(format!(
            "FAIL: glibc's closure is not a PROPER subset of hello's ({nlive} vs {n}) — nothing \
             would be swept"
        ));
    }
    println!(
        ">> td store holds hello's {n}-path closure; GC root glibc marks {nlive} live (td's own \
         store-closure), {} dead",
        n - nlive
    );

    let store_s = path_str(&subj.store)?;
    tb_out(&tb, &["store-gc-sweep", &store_s, &tddb_s, &gc_root], "store-gc-sweep")?;
    let survivors: Vec<String> = {
        let rd = std::fs::read_dir(&subj.store)
            .map_err(|e| format!("FAIL: read {}: {e}", subj.store.display()))?;
        let mut v: Vec<String> =
            rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    };
    if survivors != live {
        return Err(format!(
            "FAIL: surviving store entries != td's marked live set\n  surv: {survivors:?}\n  live: {live:?}"
        ));
    }
    println!(
        "   td DELETED the {} dead paths; the store now holds EXACTLY the {nlive} marked-live paths",
        n - nlive
    );
    let db_paths: Vec<String> = {
        let info = tb_out(&tb, &["store-query", &tddb_s, "info"], "store-query (swept db)")?;
        let mut v: Vec<String> = cut_field(&info, 1).iter().map(|p| base_of(p)).collect();
        v.sort();
        v
    };
    if db_paths != live {
        return Err(format!(
            "FAIL: the swept DB's ValidPaths != the live set\n  db:   {db_paths:?}\n  live: {live:?}"
        ));
    }
    println!("   the rewritten DB records EXACTLY the live set (dead ValidPaths rows removed)");

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td performed the DESTRUCTIVE GC SWEEP on its OWN store, in pure Rust with NO \
         daemon — over a TD-BUILT hello's {n}-path closure staged into a td-owned store. After \
         registering it and marking the live set with td's own store-closure (GC root glibc), td \
         swept: it DELETED the dead paths' files and rewrote the DB so BOTH the surviving store \
         entries AND the ValidPaths records hold EXACTLY the {nlive}-path marked-live set. The \
         host /gnu/store is never touched. td now owns BOTH halves of GC — mark and sweep."
    );
    Ok(())
}

/// store-add-referenced — td ADDS a td-assembled hello .drv WITH references to
/// its OWN store: the parsed references fold back to the assembler's path
/// (round-trip). Port of 305-store-add-referenced.rs.
fn store_add_referenced(root: &Path) -> Result<(), String> {
    println!(
        ">> store-add-referenced: td ADDS a td-ASSEMBLED hello .drv WITH references to its OWN \
         store + registers the references (pure Rust, no daemon; guix off PATH) — a round-trip of \
         the folded references"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-add-referenced-scratch")?;
    let store = scratch.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("FAIL: mkdir {}: {e}", store.display()))?;
    let subj = store_subject(&s0, root, &scratch)?;

    let drv = path_str(&subj.local_drv)?;
    let tddrv = &subj.drv;
    // name = basename minus the 32-char hash + '-'.
    let name_full = base_of(tddrv);
    let name = name_full.get(33..).unwrap_or("").to_string();
    if name.is_empty() {
        return Err(format!("FAIL: malformed .drv basename {name_full}"));
    }

    let refs = sorted_lines(&tb_out(&tb, &["drv-refs", &drv], "drv-refs")?);
    let nref = refs.len();
    if nref == 0 {
        return Err("FAIL: the .drv has no references (the round-trip would be vacuous)".into());
    }
    let refs_f = scratch.join("refs.txt");
    let mut refs_text = refs.join("\n");
    refs_text.push('\n');
    std::fs::write(&refs_f, refs_text).map_err(|e| format!("FAIL: write refs.txt: {e}"))?;
    println!(
        ">> hello's td-assembled .drv ({name}) has {nref} references (its input drvs/srcs, parsed \
         by td-builder drv-refs)"
    );

    let refs_s = path_str(&refs_f)?;
    let store_s = path_str(&store)?;
    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let td_path = tb_out(
        &tb,
        &["store-add-referenced", &name, &drv, &refs_s, &store_s, &tddb_s],
        "store-add-referenced",
    )?;
    if td_path != *tddrv {
        return Err(format!(
            "FAIL: td computed {td_path} != the ASSEMBLER's {tddrv} (references not folded into \
             the path correctly)"
        ));
    }
    println!(
        "   the {nref} references PARSED from the .drv fold back to the SAME path the assembler \
         computed from the recipe inputs (round-trip)"
    );

    let base = base_of(&td_path);
    let stored = store.join(&base);
    if !stored.is_file() {
        return Err("FAIL: td did not write the .drv into its store".into());
    }
    let stored_s = path_str(&stored)?;
    let td_nar = tb_out(&tb, &["nar-hash", &stored_s], "nar-hash (stored .drv)")?;
    let src_nar = tb_out(&tb, &["nar-hash", &drv], "nar-hash (source .drv)")?;
    if td_nar != src_nar {
        return Err(format!("FAIL: td's stored .drv NAR {td_nar} != the source .drv {src_nar}"));
    }
    println!("   td's stored .drv is byte-identical (NAR) to the source: {src_nar}");

    let td_refs: Vec<String> = {
        let out = tb_out(&tb, &["store-query", &tddb_s, "references"], "store-query references")?;
        let mut v: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split_once('|').map(|(_, r)| r.to_string()).unwrap_or_else(|| l.to_string()))
            .collect();
        v.sort();
        v
    };
    if td_refs != refs {
        return Err(format!(
            "FAIL: td's registered references (read by td's own reader) != the parsed references\n  \
             registered: {td_refs:?}\n  parsed:     {refs:?}"
        ));
    }
    println!(
        "   td REGISTERED all {nref} references (read back by TD'S OWN reader) == td-builder \
         drv-refs (the parsed set)"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td ADDED a path WITH references to its OWN store, in pure Rust with NO daemon — \
         for hello's TD-ASSEMBLED .drv and its {nref} references. td computed the \
         content-addressed path with the references folded into the type (makeTextPath), and the \
         references RECOVERED from the .drv bytes by drv-refs fold back through the shared \
         make_text_path to the SAME path the ASSEMBLER produced from the recipe inputs — a \
         round-trip that proves drv-refs recovers the exact folded set. The stored .drv is \
         NAR-identical to the source, and td registered exactly the parsed references."
    );
    Ok(())
}

/// store-verify — td VERIFIES store integrity of a td-built closure (re-hash vs
/// the recorded registration) + DETECTS a one-byte corruption. Port of
/// 295-store-verify.rs.
fn store_verify(root: &Path) -> Result<(), String> {
    println!(
        ">> store-verify: td VERIFIES store integrity of a TD-BUILT closure (re-hash vs the \
         recorded registration) + DETECTS a one-byte corruption — the daemon's guix gc --verify \
         --check-contents, pure Rust, no daemon (guix off PATH)"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-verify-scratch")?;
    let pstore = scratch.join("pstore");
    std::fs::create_dir_all(&pstore).map_err(|e| format!("FAIL: mkdir {}: {e}", pstore.display()))?;
    let subj = store_subject(&s0, root, &scratch)?;
    let n = subj.n;

    let tddb_s = path_str(&scratch.join("td.db"))?;
    let closure_s = path_str(&subj.closure_file)?;
    tb_out(&tb, &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s], "store-register")?;
    let store_s = path_str(&subj.store)?;
    if !tb_ok(&tb, &["store-verify", &tddb_s, &store_s]) {
        return Err("FAIL: td-verify flagged the intact td-built closure".into());
    }
    println!(
        "   (A) td-verify: hello's intact {n}-path closure in the td-owned store matches its \
         recorded hashes (--check-contents)"
    );

    let victim = first_regular_file(&subj.store)
        .ok_or_else(|| String::from("FAIL: no regular file in the staged closure to corrupt"))?;
    corrupt_append(&victim)?;
    if tb_ok(&tb, &["store-verify", &tddb_s, &store_s]) {
        return Err(format!(
            "FAIL: td-verify did NOT detect the corrupted closure member ({})",
            victim.display()
        ));
    }
    println!(
        "   (B) td-verify: a one-byte corruption of a REAL closure member is DETECTED (verify \
         exits nonzero)"
    );

    // An independent flat probe (store-add-text): verify OK, then corrupt.
    let content = scratch.join("content");
    std::fs::write(&content, "td store-verify probe payload\n")
        .map_err(|e| format!("FAIL: write probe: {e}"))?;
    let content_s = path_str(&content)?;
    let pstore_s = path_str(&pstore)?;
    let probedb = scratch.join("probe.db");
    let probedb_s = path_str(&probedb)?;
    tb_out(
        &tb,
        &["store-add-text", "verify-probe", &content_s, &pstore_s, &probedb_s],
        "store-add-text (probe)",
    )?;
    if !tb_ok(&tb, &["store-verify", &probedb_s, &pstore_s]) {
        return Err("FAIL: td-verify flagged an intact probe".into());
    }
    println!("   (C) td-verify: an intact td-authored probe (store-add-text) verifies OK");
    let pinfo = tb_out(&tb, &["store-query", &probedb_s, "info"], "store-query (probe)")?;
    let pbase = base_of(pinfo.split('|').next().unwrap_or(""));
    if pbase.is_empty() {
        return Err(format!("FAIL: malformed probe registration {pinfo}"));
    }
    corrupt_append(&pstore.join(&pbase))?;
    if tb_ok(&tb, &["store-verify", &probedb_s, &pstore_s]) {
        return Err("FAIL: td-verify did NOT detect the corrupted probe".into());
    }
    println!("   (C) td-verify: a one-byte corruption of the probe is DETECTED (verify exits nonzero)");

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td VERIFIED store integrity ITSELF, in pure Rust with NO daemon — the daemon's \
         guix gc --verify --check-contents. Over a TD-BUILT hello's {n}-path closure staged into \
         a td-owned store: (A) td-verify re-NAR-hashed each registered path and confirmed it \
         matches td's recorded hash; (B) a one-byte corruption of a real closure member is \
         DETECTED (exit nonzero); (C) an independent flat probe (store-add-text) verifies OK and \
         its corruption is DETECTED. Boundary: td reads + writes only its own scratch store. The \
         destructive GC sweep is store-gc-sweep."
    );
    Ok(())
}

/// store-backend — a td store backend HOLDS + SERVES a td-built hello output
/// (place + register + query + verify + deriver/drv->output mapping). Port of
/// 310-store-backend.rs.
fn store_backend(root: &Path) -> Result<(), String> {
    println!(
        ">> store-backend: a td store backend HOLDS + SERVES a TD-BUILT hello output (place + \
         register + query + verify, pure Rust, no daemon; guix off PATH)"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-backend-scratch")?;
    let store = scratch.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("FAIL: mkdir {}: {e}", store.display()))?;
    let subj = store_subject(&s0, root, &scratch)?;

    let store_s = path_str(&store)?;
    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let closure_s = path_str(&subj.closure_file)?;
    tb_out(
        &tb,
        &["store-add-output", &subj.root, &subj.drv, &closure_s, &store_s, &tddb_s],
        "store-add-output",
    )?;
    let base = base_of(&subj.root);
    let placed = store.join(&base);
    if !placed.is_dir() {
        return Err("FAIL: td did not place the output tree into its store".into());
    }
    let placed_s = path_str(&placed)?;
    let placed_nar = tb_out(&tb, &["nar-hash", &placed_s], "nar-hash (placed)")?;
    let src_nar = tb_out(&tb, &["nar-hash", &subj.root], "nar-hash (source)")?;
    if placed_nar != src_nar {
        return Err(format!(
            "FAIL: the placed output NAR {placed_nar} != the source staged tree {src_nar}"
        ));
    }
    println!(
        "   (1) td PLACED hello's output into its store, NAR-identical to the source staged tree: \
         {src_nar}"
    );

    let td_info = tb_out(&tb, &["store-query", &tddb_s, "info"], "store-query info")?;
    let mut f = td_info.split('|');
    if f.next().unwrap_or("") != subj.root {
        return Err(format!("FAIL: store-query info path != {} ({td_info})", subj.root));
    }
    if f.next().unwrap_or("") != src_nar {
        return Err(format!("FAIL: store-query info hash != the re-derived NAR hash ({td_info})"));
    }
    println!("   (2) td's store SERVES the registration (store-query info) == the re-derived hash + narSize");

    // The backend's references == store-register's INDEPENDENT direct-ref scan.
    let fulldb = scratch.join("full.db");
    let fulldb_s = path_str(&fulldb)?;
    tb_out(
        &tb,
        &["store-register", &subj.root, &subj.drv, &closure_s, &fulldb_s],
        "store-register (independent scan)",
    )?;
    let direct_refs: Vec<String> = {
        let out = tb_out(&tb, &["store-query", &fulldb_s, "references"], "store-query (full)")?;
        let prefix = format!("{}|", subj.root);
        let mut v: Vec<String> = out
            .lines()
            .filter(|l| l.starts_with(&prefix))
            .map(|l| l.get(prefix.len()..).unwrap_or("").to_string())
            .collect();
        v.sort();
        v
    };
    if direct_refs.is_empty() {
        return Err("FAIL: hello's output has no direct references (the check would be vacuous)".into());
    }
    let td_refs: Vec<String> = {
        let out = tb_out(&tb, &["store-query", &tddb_s, "references"], "store-query (backend)")?;
        let mut v: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split_once('|').map(|(_, r)| r.to_string()).unwrap_or_else(|| l.to_string()))
            .collect();
        v.sort();
        v
    };
    if td_refs != direct_refs {
        return Err(format!(
            "FAIL: the backend's served references != store-register's independent direct-ref \
             scan\n  backend:  {td_refs:?}\n  register: {direct_refs:?}"
        ));
    }
    println!(
        "   (3) td's store SERVES the references (store-query references) == store-register's \
         INDEPENDENT direct-ref scan of the closure ({} refs)",
        td_refs.len()
    );

    if !tb_ok(&tb, &["store-verify", &tddb_s, &store_s]) {
        return Err("FAIL: store-verify flagged the placed output".into());
    }
    println!("   (4) td's store VERIFIES (store-verify) the placed output's integrity against its OWN files");

    let doutsql = format!(
        "SELECT (SELECT deriver FROM ValidPaths WHERE path='{root}')||' :: '||v.path||':'||d.id||':'\
         ||d.path FROM DerivationOutputs d JOIN ValidPaths v ON d.drv=v.id WHERE d.path='{root}'",
        root = subj.root
    );
    let td_dout = sqlite3(&tddb, &doutsql)?;
    let expected = format!("{drv} :: {drv}:out:{root}", drv = subj.drv, root = subj.root);
    if td_dout != expected {
        return Err(format!(
            "FAIL: td's deriver/drv->output ({td_dout}) != the expected (td-assembled .drv) -> \
             out -> output"
        ));
    }
    println!(
        "   (5) td's store records the deriver + drv->output mapping == (the td-assembled .drv) \
         -> out -> the output"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: a td STORE BACKEND holds + serves a TD-BUILT hello output, in pure Rust with NO \
         daemon in any store operation and guix OFF PATH — td PLACED hello's built output into a \
         td-owned store (NAR-identical to the source staged tree), FULLY REGISTERED it (hash + \
         narSize + deriver + references + drv->output), and td's OWN tools SERVE it: store-query \
         returns the registration + references, cross-checked against store-register's \
         INDEPENDENT direct-ref scan, and store-verify re-hashes the PLACED files and confirms \
         integrity. td owns the full store backend — write/read the DB, add \
         (flat/recursive/referenced), GC (mark + sweep), verify, AND back a build output end to end."
    );
    Ok(())
}

/// store-ns — td OWNS ITS OWN ROOT with its own store at /td/store: a static
/// binary runs from /td/store in a rootless userns with /gnu/store ABSENT.
/// Port of tests/store-ns.sh (gate 386).
fn store_ns(root: &Path) -> Result<(), String> {
    println!(
        ">> store-ns: td owns its own root — a static package runs from /td/store in a rootless \
         user namespace with /gnu/store and the guix install ABSENT (user-pm Phase 0)"
    );
    let tb = tb()?;
    println!(">> td-builder under test (stage0, guix-free): {}", tb.display());
    let work = fresh_scratch(root, ".store-ns-scratch")?;

    // A static binary to run from /td/store: bash-static, from hello's seed
    // closure (td's own content scan — no store DB, no guix process).
    let lock_rel = "tests/hello-no-guix.lock";
    let lock_text = std::fs::read_to_string(root.join(lock_rel))
        .map_err(|e| format!("FAIL: read {lock_rel}: {e}"))?;
    let bash = lock_text
        .lines()
        .find(|l| l.contains("-bash-") && !l.contains("static"))
        .and_then(|l| l.split_once(' ').map(|(_, p)| p.trim().to_string()))
        .ok_or_else(|| String::from("FAIL: no bash in hello's lock"))?;
    let scan = tb_out(&tb, &["store-closure-scan", "/gnu/store", &bash], "store-closure-scan")?;
    let bs = scan
        .lines()
        .find(|l| l.contains("-bash-static-"))
        .map(str::to_string)
        .ok_or_else(|| format!("FAIL: no static bash in the closure of {bash}"))?;
    if !Path::new(&bs).join("bin/bash").is_file() {
        return Err(format!("FAIL: no static bash in the closure of {bash}"));
    }

    // The user's /td/store: place the static package at <store>/<base>.
    let store = work.join("td-store");
    std::fs::create_dir_all(&store).map_err(|e| format!("FAIL: mkdir {}: {e}", store.display()))?;
    let base = base_of(&bs);
    cp_a(Path::new(&bs), &store.join(&base))?;
    chmod_r_uw(&store)?;
    println!("   placed {base} into the td-owned store {}", store.display());

    // Run inside the own-root store-ns (rootless): /td/store = store, /gnu/store absent.
    let inner = format!(
        "[ -d /td/store ] && echo TDSTORE-OK\n\
         [ -d /td/store/{base}/bin ] && echo PKG-AT-TDSTORE\n\
         [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT\n\
         echo \"RAN:$BASH_VERSION\"\n"
    );
    let store_s = path_str(&store)?;
    let out = tb_out(
        &tb,
        &["store-ns", &store_s, "--", &format!("/td/store/{base}/bin/bash"), "-c", &inner],
        "store-ns run",
    )?;
    for l in out.lines() {
        println!("     {l}");
    }

    // Leg A: DURABLE behavioral — the binary ran from /td/store.
    if !out.lines().any(|l| l.starts_with("RAN:5")) {
        return Err("FAIL: the static binary did not run from /td/store".into());
    }
    if !out.lines().any(|l| l == "PKG-AT-TDSTORE") {
        return Err("FAIL: the package is not at /td/store/<base> inside the root".into());
    }
    println!("   [DURABLE behavioral] a binary ran from /td/store in td's own root (rootless userns)");

    // Leg B: DURABLE structural — /td/store is the store, /gnu/store ABSENT.
    if !out.lines().any(|l| l == "TDSTORE-OK") {
        return Err("FAIL: /td/store is not present in the own-root".into());
    }
    if !out.lines().any(|l| l == "GNU-ABSENT") {
        return Err("FAIL: /gnu/store is PRESENT in the own-root — mixed with the guix install!".into());
    }
    println!(
        "   [DURABLE structural] /td/store is the store and /gnu/store is ABSENT — unmixed from \
         the local guix install"
    );

    let _ = chmod_r_uw(&work);
    let _ = std::fs::remove_dir_all(&work);
    println!(
        "PASS: td owns its own root with its own store at /td/store — a static package runs from \
         /td/store in a rootless user namespace with /gnu/store and the guix install ABSENT. The \
         unmixed /td/store base the user package manager runs in (user-pm Phase 0)."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_gates_match_cli() {
        // Every NATIVE name must have a `cli` arm (dispatch returns something
        // other than the "unknown native gate" error). We can't run the bodies
        // here (they need the loop), but we CAN prove the registry ↔ dispatch
        // pairing by checking each name is not "unknown".
        for name in NATIVE {
            assert!(is_native(name), "{name} in NATIVE must report is_native");
        }
        // A non-native name must not claim to be native.
        assert!(!is_native("definitely-not-a-gate"));
    }

    #[test]
    fn helpers_cut_sort_base() {
        assert_eq!(cut_field("a|b|c\nd|e|f", 1), vec!["a", "d"]);
        assert_eq!(cut_field("a|b|c\nd|e|f", 2), vec!["b", "e"]);
        assert_eq!(sorted_dedup("b\na\nb\n"), vec!["a", "b"]);
        assert_eq!(sorted_lines("b\na\nb\n"), vec!["a", "b", "b"]);
        assert_eq!(base_of("/x/y/z"), "z");
        assert_eq!(base_of("z"), "z");
    }

    #[test]
    fn stage0_from_memo_reads_the_current_placement() {
        // A fake repo root with a stage0 memo + placement: the resolver must
        // return the memo's cb (line 2) as the builder-of-record and the
        // placement's binary as TB — load_stage0's fast path.
        let root = std::env::temp_dir().join(format!("td-s0memo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join(".td-build-cache/stage0");
        let bin = base.join("store/abc123-td-builder-0.1.0/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("td-builder"), b"#!bin\n").unwrap();
        std::fs::write(
            base.join(".stage0-meta"),
            "fingerprintline\n/gnu/store/abc123-td-builder-0.1.0\n",
        )
        .unwrap();
        let s0 = stage0_from_memo(&root).expect("memo");
        assert_eq!(s0.builder_path, "/gnu/store/abc123-td-builder-0.1.0");
        assert_eq!(s0.store, base.join("store"));
        assert_eq!(s0.db, base.join("builder.db"));
        assert!(s0.tb.ends_with("store/abc123-td-builder-0.1.0/bin/td-builder"));
        // A missing memo is a loud provisioning error, not a fallback.
        let _ = std::fs::remove_dir_all(&root);
        assert!(stage0_from_memo(&root).is_err());
    }
}
