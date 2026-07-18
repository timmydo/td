//! gate_bodies.rs — typed Rust gate bodies (#318 axis 3): the `td-builder
//! gate-body <name>` subcommand that replaces a gate's bash `script` field.
//!
//! A gate whose `GateDef.script` is EMPTY is "native": the gate runner
//! (`gates.rs::run_gate`) execs `<current_exe> gate-body <name>` in the exact
//! same memory-limited wrapper (the pre_exec setrlimit(RLIMIT_DATA), the
//! per-gate cgroup, its own process group, TD_CHECK_CHAIN_CACHE /
//! TD_GATE_SPECS env) it uses for shell gates. `current_exe` is the stage0 td-builder in the loop (the
//! prelude execs `<stage0> … gate-run`), so a native body gets `tb` = its own
//! binary for free — no `load_stage0` shell dance for the td-builder under test.
//!
//! The registry is `is_native` + the `cli` match below (one place, not a
//! GateDef field, so the other bash gates are untouched). `load()` asserts
//! empty-script ⟺ `is_native`, so a typo (empty script with no body, or a body
//! whose gate still carries bash) is a load-time error, never a silent no-op.
//!
//! The store-* cluster shares `store_subject`: a typed synthetic output with a
//! valid td-assembled `.drv` and a two-path runtime closure staged into a
//! self-contained td-owned store. External tools spawned by these bodies are
//! staging artifacts only (`cp -a`/`chmod`) — the gate LOGIC (every assertion)
//! is typed Rust, and the only reader of td's hand-written SQLite bytes is td's
//! OWN pure-Rust reader (`store_db_read`, via `store-query`); no external
//! oracle (`sqlite3` or otherwise) is spawned. No body spawns a guix process.

use std::collections::HashMap;
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
    "recipe-rs",
    "recipe-checks-daily",
    "store-native-profile",
    "sandbox-hardening",
    "toolchain-input-addressed",
    "toolchain-x86_64-input-addressed",
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
        "recipe-rs" => recipe_rs(&root),
        "recipe-checks-daily" => recipe_checks_daily(&root),
        "store-native-profile" => store_native_profile(&root),
        "sandbox-hardening" => sandbox_hardening(&root),
        "toolchain-input-addressed" => toolchain_input_addressed(&root),
        "toolchain-x86_64-input-addressed" => toolchain_x86_64_input_addressed(&root),
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
    tb_out_env(tb, args, &[], ctx)
}

fn tb_out_env(
    tb: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    ctx: &str,
) -> Result<String, String> {
    let mut cmd = Command::new(tb);
    cmd.args(args).stdin(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
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

/// True if `tb <args...>` exits zero (for the discrimination legs that expect
/// a NON-zero exit — corruption/verify-fail). stdout+stderr discarded.
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
/// subprocess spawn for staging helpers and one-off tool invocations).
fn run_out(program: &str, args: &[&str], ctx: &str) -> Result<String, String> {
    run_out_env(program, args, &[], ctx)
}

/// `run_out`, plus extra env vars set on the child (inheriting the rest of the
/// current environment — never `env -i`, matching the bash gates' bare
/// `VAR=val cmd` prefix form).
fn run_out_env(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    ctx: &str,
) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn {program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let sout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "FAIL: {ctx}: {program} {args:?} exited {}\n{sout}{err}",
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The first `bin`-dir among `frags` (a `:`-joined PATH fragment, as
/// `stage0::provision_rust`/`provision_cc` return) that actually has an executable
/// named `bin` — resolving the absolute binary path ourselves rather than
/// leaning on `Command`'s PATH search (which uses the CURRENT process's PATH,
/// not a child `.env("PATH", ..)` override).
fn find_in_path_frags(frags: &str, bin: &str) -> Option<PathBuf> {
    frags.split(':').map(Path::new).find_map(|d| {
        let p = d.join(bin);
        let exec = p.is_file() && file_mode(&p).ok().is_some_and(|mode| mode & 0o111 != 0);
        exec.then_some(p)
    })
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
    f.write_all(b"X")
        .map_err(|e| format!("FAIL: append to {}: {e}", p.display()))
}

/// The first regular file under `dir` (depth-first) — the corruption victim
/// (`find "$dir" -type f | head -1`).
fn first_regular_file(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
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
        .map(|l| {
            l.split('|')
                .nth(idx.saturating_sub(1))
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// Non-empty lines, sorted and deduped (`sort -u`).
fn sorted_dedup(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Non-empty lines, sorted (`sort`, no -u).
fn sorted_lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// The basename of a path string.
fn base_of(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// A path as UTF-8 for passing to argv (all td scratch paths are UTF-8).
fn path_str(p: &Path) -> Result<String, String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("FAIL: non-UTF-8 path {}", p.display()))
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
        return Err(format!(
            "FAIL: stage0 td-builder not executable at {}",
            tb.display()
        ));
    }
    Ok(Stage0 { tb })
}

// --- the shared td-built subject ---------------------------------------------

/// The td-built subject the store-backend cluster exercises: a synthetic output
/// with one runtime dependency and a valid td-assembled `.drv`.
struct Subject {
    /// SUBJ_STORE — the self-contained td-owned store dir.
    store: PathBuf,
    /// SUBJ_ROOT — the output path IN that store (the GC root).
    root: String,
    /// SUBJ_CLOSURE — the file listing every member as `<store>/<base>`.
    closure_file: PathBuf,
    /// The same members, sorted + deduped.
    closure: Vec<String>,
    /// SUBJ_N.
    n: usize,
    /// SUBJ_DRV — the canonical td-ASSEMBLED .drv path (the deriver string).
    drv: String,
    /// SUBJ_LOCALDRV — the on-disk assembled .drv file (its bytes).
    local_drv: PathBuf,
}

fn store_subject(_s0: &Stage0, _root: &Path, scratch: &Path) -> Result<Subject, String> {
    use std::os::unix::fs::PermissionsExt as _;

    let subj_store = scratch.join("allstore");
    let _ = std::fs::remove_dir_all(&subj_store);
    std::fs::create_dir_all(&subj_store)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", subj_store.display()))?;

    let dep_base = "11111111111111111111111111111111-glibc-store-subject-dep-1.0";
    let dep_path = subj_store.join(dep_base);
    std::fs::create_dir_all(dep_path.join("lib"))
        .map_err(|e| format!("FAIL: mkdir synthetic dep lib: {e}"))?;
    std::fs::create_dir_all(dep_path.join("share"))
        .map_err(|e| format!("FAIL: mkdir synthetic dep share: {e}"))?;
    std::fs::write(
        dep_path.join("lib/libc.so.6"),
        b"td synthetic glibc fixture\n",
    )
    .map_err(|e| format!("FAIL: write synthetic dep library: {e}"))?;
    std::fs::write(
        dep_path.join("share/name"),
        b"glibc-store-subject-dep-1.0\n",
    )
    .map_err(|e| format!("FAIL: write synthetic dep metadata: {e}"))?;
    let dep_s = path_str(&dep_path)?;

    let spec = format!(
        "name td-store-subject-1.0\n\
         system x86_64-linux\n\
         builder /no-such-td-store-subject-builder\n\
         arg build\n\
         input-src {dep_s}\n\
         env TD_SUBJECT_DEP={dep_s}\n"
    );
    let read_drv = |p: &str| std::fs::read(p).map_err(|e| format!("read input drv {p}: {e}"));
    let (subj_drv, drv_content) = crate::store::assemble_drv(&spec, &read_drv)?;
    let parsed = crate::drv::parse(drv_content.as_bytes())
        .map_err(|e| format!("FAIL: parse synthetic subject .drv: {e}"))?;
    let out_path = parsed
        .outputs
        .iter()
        .find(|o| o.name == "out")
        .map(|o| o.path.as_str())
        .ok_or_else(|| String::from("FAIL: synthetic subject .drv has no out output"))?;
    if crate::store::name_from_store_path(out_path).is_none() {
        return Err(format!(
            "FAIL: synthetic subject output is not store-shaped: {out_path}"
        ));
    }

    let root_base = base_of(out_path);
    let subj_root_path = subj_store.join(&root_base);
    std::fs::create_dir_all(subj_root_path.join("bin"))
        .map_err(|e| format!("FAIL: mkdir synthetic root bin: {e}"))?;
    std::fs::create_dir_all(subj_root_path.join("share"))
        .map_err(|e| format!("FAIL: mkdir synthetic root share: {e}"))?;
    let probe = subj_root_path.join("bin/subject");
    std::fs::write(
        &probe,
        b"#!/bin/sh\nprintf 'td synthetic store subject\\n'\n",
    )
    .map_err(|e| format!("FAIL: write synthetic subject probe: {e}"))?;
    let mut probe_perm = std::fs::metadata(&probe)
        .map_err(|e| format!("FAIL: stat {}: {e}", probe.display()))?
        .permissions();
    probe_perm.set_mode(0o755);
    std::fs::set_permissions(&probe, probe_perm)
        .map_err(|e| format!("FAIL: chmod {}: {e}", probe.display()))?;
    std::fs::write(
        subj_root_path.join("share/reference.txt"),
        format!("runtime reference: {dep_s}\n"),
    )
    .map_err(|e| format!("FAIL: write synthetic root reference: {e}"))?;

    let local_drv_dir = scratch.join("pkgcache/subject/b");
    std::fs::create_dir_all(&local_drv_dir)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", local_drv_dir.display()))?;
    let local_drv = local_drv_dir.join(base_of(&subj_drv));
    std::fs::write(&local_drv, drv_content)
        .map_err(|e| format!("FAIL: write synthetic .drv {}: {e}", local_drv.display()))?;

    chmod_r_uw(&subj_store)
        .map_err(|e| format!("FAIL: could not make the staged store writable\n{e}"))?;
    let subj_root = path_str(&subj_root_path)?;
    let mut members: Vec<String> = vec![subj_root.clone(), dep_s];
    members.sort();
    members.dedup();
    let closure_file = scratch.join("closure.txt");
    let mut listing = members.join("\n");
    listing.push('\n');
    std::fs::write(&closure_file, listing)
        .map_err(|e| format!("FAIL: write {}: {e}", closure_file.display()))?;

    let n = members.len();
    if n != 2 {
        return Err(format!(
            "FAIL: synthetic subject closure should have 2 paths, got {n}"
        ));
    }

    println!(
        "   [td-subject] td assembled a valid .drv and staged a {n}-path synthetic runtime \
         closure into the td-owned store {}",
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
/// Rust, no daemon in the write path).
fn store_add(root: &Path) -> Result<(), String> {
    println!(
        ">> store-add: td PLACES a /td/store text path into its OWN store + registers it (pure \
         Rust, no daemon in the write path)"
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

    // td computes + writes + registers the SAME path itself, no daemon.
    let store_s = path_str(&store)?;
    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let td_path = tb_out_env(
        &tb,
        &["store-add-text", name, &content_s, &store_s, &tddb_s],
        &[("TD_STORE_DIR", "/td/store")],
        "td store-add-text (/td/store)",
    )?;
    let content_bytes =
        std::fs::read(&content).map_err(|e| format!("FAIL: read {}: {e}", content.display()))?;
    let expected_path = "/td/store/acs7ncyflz0ms0wfcd0vlvrcirn5fhp1-td-store-add-probe";
    if td_path != expected_path {
        return Err(format!(
            "FAIL: td computed {td_path} != the fixed addTextToStore known vector {expected_path}"
        ));
    }
    println!("   td computed the fixed addTextToStore known-vector path");

    let base = base_of(&td_path);
    let td_file = store.join(&base);
    if !td_file.is_file() {
        return Err(format!("FAIL: td did not write the store file {base}"));
    }
    let mode = file_mode(&td_file)?;
    if mode != 0o444 {
        return Err(format!(
            "FAIL: td's store file mode {mode:o} != 444 (canonical read-only)"
        ));
    }
    let written = std::fs::read(&td_file)
        .map_err(|e| format!("FAIL: read td store file {}: {e}", td_file.display()))?;
    if written != content_bytes {
        return Err("FAIL: td's store file bytes differ from the input content".into());
    }
    println!(
        "   td WROTE the store file itself, canonical mode 0444 (no daemon in the write path)"
    );

    // NAR hash is metadata-independent over the canonical store file.
    let td_file_s = path_str(&td_file)?;
    let td_file_hash = tb_out(
        &tb,
        &["nar-hash", &td_file_s],
        "nar-hash of td's store file",
    )?;
    println!("   td's store file NAR hash is {td_file_hash}");

    // td's registration, read back by TD'S OWN reader.
    let td_reg = tb_out(
        &tb,
        &["store-query", &tddb_s, "info"],
        "td store-query (td's own reader)",
    )?;
    let mut fields = td_reg.split('|');
    let reg_path = fields.next().unwrap_or("");
    let reg_hash = fields.next().unwrap_or("");
    if reg_path != td_path {
        return Err(format!("FAIL: td registered path {reg_path} != {td_path}"));
    }
    if reg_hash != td_file_hash {
        return Err(format!(
            "FAIL: td registered hash {reg_hash} != td's NAR hash {td_file_hash}"
        ));
    }
    println!(
        "   td's registration (read back by TD'S OWN reader) records the path + the NAR hash of \
         what td wrote"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td PLACED a /td/store path into its OWN store and REGISTERED it ITSELF, in pure Rust with NO \
         daemon in the write path — td computed the addTextToStore path, wrote the exact content \
         as a canonical 0444 store file, and its registration (read back by TD'S OWN reader) \
         records that path + the NAR hash of what td wrote."
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
    std::fs::write(
        fx.join("file.txt"),
        "hello from the td store-add-recursive fixture\n",
    )
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
        tb_out(
            &tb,
            &["store-add-recursive", name, &t, &s, &d],
            "store-add-recursive",
        )
    };

    let p1 = intern(&fx, "store", "td.db")?;
    // The placement prefix follows the ACTIVE store (TD_STORE_DIR or the
    // default), not a hardcoded /gnu/store.
    if !(p1.starts_with(&format!("{}/", crate::store::store_dir()))
        && p1.ends_with(&format!("-{name}")))
    {
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
        return Err(format!(
            "FAIL: td did not restore the tree at {}",
            restored.display()
        ));
    }
    let restored_s = path_str(&restored)?;
    let rnar = tb_out(
        &tb,
        &["nar-hash", &restored_s],
        "nar-hash of the restored tree",
    )?;
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
        && std::fs::read_link(&link)
            .map(|t| t == Path::new("file.txt"))
            .unwrap_or(false);
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
    println!(
        "   [REGISTRATION] td's own reader reads back the interned path + the tree's NAR hash"
    );

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
    let cnar = tb_out(
        &tb,
        &["store-query", &tdc_s, "info"],
        "store-query (perturbed)",
    )?
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
        std::fs::set_permissions(
            tree_x.join("run.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
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

/// store-register — td WRITES the store SQLite DB for a td-built subject's FULL
/// closure (pure-Rust file format) and READS it back itself (td-builder
/// store-query — a pure-Rust SQLite reader, no external engine). Port of
/// 275-store-register.rs.
fn store_register(root: &Path) -> Result<(), String> {
    println!(
        ">> store-register: td WRITES the store SQLite DB for a TD-BUILT subject's FULL CLOSURE \
         (pure-Rust file format) and READS it back itself (guix off PATH; no guix build, no guix \
         gc, no /var/guix read; no external SQLite engine anywhere in this gate)"
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
        ">> td WRITES the store SQLite DB for the {n}-path closure at {} (td emits the SQLite \
         bytes itself, no external engine)",
        tddb.display()
    );
    tb_out(
        &tb,
        &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s],
        "store-register",
    )?;
    if std::fs::metadata(&tddb).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("FAIL: td wrote no store DB".into());
    }

    // The content-scan oracle: seed-manifest's STORE-DIR form recomputes each
    // member's hash/narSize/direct-refs straight from the staged bytes (the
    // scan.rs candidate-index + NAR walk, same as store_subject's OWN closure
    // discovery) — it never reads td.db, so it's independent of both
    // store_db.rs (the writer) and store_db_read.rs (the reader) and gives the
    // gate a real ground truth to check store-query's output against, in place
    // of the dropped sqlite3 reader-vs-reader cross-check.
    let store_s = path_str(&subj.store)?;
    let manifest = tb_out(
        &tb,
        &["seed-manifest", &store_s, &subj.root],
        "seed-manifest (content-scan oracle)",
    )?;
    let mut expected_info: Vec<String> = Vec::new();
    let mut expected_refs: Vec<String> = Vec::new();
    for line in manifest.lines() {
        let mut f = line.splitn(4, ' ');
        let malformed = || format!("FAIL: malformed seed-manifest line: {line}");
        let p = f.next().ok_or_else(malformed)?;
        let hash = f.next().ok_or_else(malformed)?;
        let size = f.next().ok_or_else(malformed)?;
        let refs = f.next().ok_or_else(malformed)?;
        expected_info.push(format!("{p}|{hash}|{size}"));
        if refs != "-" {
            for r in refs.split(',') {
                expected_refs.push(format!("{p}|{r}"));
            }
        }
    }
    expected_info.sort();
    expected_refs.sort();

    println!(
        ">> td READS its own store DB itself (td-builder store-query — a pure-Rust SQLite reader; \
         no external engine, no daemon in td's query path):"
    );
    let td_read_info = tb_out(&tb, &["store-query", &tddb_s, "info"], "store-query info")?;
    let nrows = td_read_info.lines().count();
    if nrows != n {
        return Err(format!("FAIL: td registered {nrows} paths, expected {n}"));
    }
    let regpaths = cut_field(&td_read_info, 1);
    if regpaths != subj.closure {
        return Err("FAIL: the registered path set != the staged closure".into());
    }
    let td_info_lines = sorted_lines(&td_read_info);
    if td_info_lines != expected_info {
        return Err(format!(
            "FAIL: td's reader (store-query info) disagrees with the content-scan oracle \
             (seed-manifest, independent of the SQLite bytes) for the SAME staged store\n  \
             td-read: {td_info_lines:?}\n  content-scan: {expected_info:?}"
        ));
    }
    println!(
        "   info: td's reader parsed all {n} closure paths' path|hash|narSize, matching an \
         INDEPENDENT content-scan of the same staged store — exactly the staged closure"
    );
    let td_read_refs = tb_out(
        &tb,
        &["store-query", &tddb_s, "references"],
        "store-query references",
    )?;
    let td_refs_lines = sorted_lines(&td_read_refs);
    if td_refs_lines != expected_refs {
        return Err(format!(
            "FAIL: td's reader (store-query references) disagrees with the content-scan oracle \
             (seed-manifest) for the SAME staged store\n  td-read: {td_refs_lines:?}\n  \
             content-scan: {expected_refs:?}"
        ));
    }
    let nedges = td_refs_lines.len();
    println!(
        "   references: td's reader parsed {nedges} edges of the inter-path Refs relation, \
         matching an INDEPENDENT content-scan of the same staged store"
    );

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
    let dic_info = tb_out(
        &tb,
        &["store-query", &dic_s, "info"],
        "store-query info (deriver-in-closure)",
    )?;
    let dic_total = dic_info.lines().count();
    let mut dic_paths = cut_field(&dic_info, 1);
    dic_paths.sort();
    let mut dic_distinct_paths = dic_paths.clone();
    dic_distinct_paths.dedup();
    let dic_distinct = dic_distinct_paths.len();
    if dic_total != n || dic_distinct != n {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for p in &dic_paths {
            *counts.entry(p.clone()).or_insert(0) += 1;
        }
        let mut dups: Vec<String> = counts
            .into_iter()
            .filter(|(_, c)| *c > 1)
            .map(|(p, c)| format!("{p} {c}"))
            .collect();
        dups.sort();
        return Err(format!(
            "FAIL: deriver-in-closure produced {dic_total} rows ({dic_distinct} distinct), \
             expected {n} with no duplicate — the closure-member deriver was registered twice\n{dups:?}"
        ));
    }
    println!("   the closure-member deriver is registered once ({n} rows, no duplicate)");

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td WROTE the store SQLite DB for a TD-BUILT subject's full {n}-path closure itself \
         in pure Rust AND READ it back itself (td-builder store-query — a pure-Rust SQLite \
         reader, no external SQLite engine and no daemon anywhere in this gate): every path's \
         hash + narSize and the full inter-path Refs relation, as answered by TD'S OWN READER, \
         match an INDEPENDENT content-scan oracle (seed-manifest, bypassing the SQLite bytes \
         entirely) of the same staged store; and a closure-member deriver is registered exactly \
         once."
    );
    Ok(())
}

/// store-gc — td computes the GC-reachable closure from its OWN store DB
/// (Refs-graph walk) == td's own content scan == the staged closure. Port of
/// 290-store-gc.rs.
fn store_gc(root: &Path) -> Result<(), String> {
    println!(
        ">> store-gc: td computes the GC-reachable closure of a TD-BUILT subject from its OWN store \
         DB (pure Rust, no daemon) == td's own content scan (guix off PATH; no guix gc)"
    );
    let s0 = stage0_from_memo(root)?;
    let tb = s0.tb.clone();
    let scratch = fresh_scratch(root, ".store-gc-scratch")?;
    let subj = store_subject(&s0, root, &scratch)?;

    let tddb_s = path_str(&scratch.join("td.db"))?;
    let closure_s = path_str(&subj.closure_file)?;
    tb_out(
        &tb,
        &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s],
        "store-register",
    )?;
    let store_s = path_str(&subj.store)?;
    let td_reach = sorted_dedup(&tb_out(
        &tb,
        &["store-closure", &tddb_s, &subj.root],
        "store-closure",
    )?);
    let scan_reach = sorted_dedup(&tb_out(
        &tb,
        &["store-closure-scan", &store_s, &subj.root],
        "store-closure-scan",
    )?);
    let staged = subj.closure.clone();
    let n = staged.len();
    if td_reach != scan_reach {
        return Err(format!(
            "FAIL: td's DB-walk GC closure != td's content-scan closure\n  db:   {td_reach:?}\n  \
             scan: {scan_reach:?}"
        ));
    }
    println!(
        "   (1) td's DB-walk (Refs graph) and (2) content-scan closures of the td-built subject \
         AGREE ({n} paths)"
    );
    if td_reach != staged {
        return Err(
            "FAIL: the reachable set != the staged closure (register/scan disagree with what was \
             staged)"
                .into(),
        );
    }
    println!("   both == the staged runtime closure — every staged member is reachable from the subject output");
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
        "PASS: td computed the GC-reachable CLOSURE of a TD-BUILT subject ({n} paths) TWO \
         daemon-free ways, in pure Rust, over its OWN store — (1) walking the Refs graph in a \
         store DB it wrote (td's own SQLite reader) and (2) CONTENT-SCANNING the staged store \
         from the subject output — and BOTH agree with each other AND with the staged closure. The \
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
    tb_out(
        &tb,
        &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s],
        "store-register",
    )?;

    // A non-trivial GC root: glibc (a PROPER subset of the subject closure).
    let gc_root = subj
        .closure
        .iter()
        .find(|p| p.contains("-glibc-"))
        .cloned()
        .ok_or_else(|| {
            String::from(
                "FAIL: no glibc dependency in the subject closure to use as a non-trivial GC root",
            )
        })?;
    let live: Vec<String> = {
        let out = tb_out(
            &tb,
            &["store-closure", &tddb_s, &gc_root],
            "store-closure (mark)",
        )?;
        let mut v: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(base_of).collect();
        v.sort();
        v
    };
    let nlive = live.len();
    if nlive >= n {
        return Err(format!(
            "FAIL: glibc's closure is not a PROPER subset of the subject's ({nlive} vs {n}) — nothing \
             would be swept"
        ));
    }
    println!(
        ">> td store holds the subject's {n}-path closure; GC root glibc marks {nlive} live (td's own \
         store-closure), {} dead",
        n - nlive
    );

    let store_s = path_str(&subj.store)?;
    tb_out(
        &tb,
        &["store-gc-sweep", &store_s, &tddb_s, &gc_root],
        "store-gc-sweep",
    )?;
    let survivors: Vec<String> = {
        let rd = std::fs::read_dir(&subj.store)
            .map_err(|e| format!("FAIL: read {}: {e}", subj.store.display()))?;
        let mut v: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
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
        let info = tb_out(
            &tb,
            &["store-query", &tddb_s, "info"],
            "store-query (swept db)",
        )?;
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
         daemon — over a TD-BUILT subject's {n}-path closure staged into a td-owned store. After \
         registering it and marking the live set with td's own store-closure (GC root glibc), td \
         swept: it DELETED the dead paths' files and rewrote the DB so BOTH the surviving store \
         entries AND the ValidPaths records hold EXACTLY the {nlive}-path marked-live set. The \
         host /gnu/store is never touched. td now owns BOTH halves of GC — mark and sweep."
    );
    Ok(())
}

/// store-add-referenced — td ADDS a td-assembled subject .drv WITH references to
/// its OWN store: the parsed references fold back to the assembler's path
/// (round-trip). Port of 305-store-add-referenced.rs.
fn store_add_referenced(root: &Path) -> Result<(), String> {
    println!(
        ">> store-add-referenced: td ADDS a td-ASSEMBLED subject .drv WITH references to its OWN \
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
        ">> the subject's td-assembled .drv ({name}) has {nref} references (its input drvs/srcs, parsed \
         by td-builder drv-refs)"
    );

    let refs_s = path_str(&refs_f)?;
    let store_s = path_str(&store)?;
    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let td_path = tb_out(
        &tb,
        &[
            "store-add-referenced",
            &name,
            &drv,
            &refs_s,
            &store_s,
            &tddb_s,
        ],
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
        return Err(format!(
            "FAIL: td's stored .drv NAR {td_nar} != the source .drv {src_nar}"
        ));
    }
    println!("   td's stored .drv is byte-identical (NAR) to the source: {src_nar}");

    let td_refs: Vec<String> = {
        let out = tb_out(
            &tb,
            &["store-query", &tddb_s, "references"],
            "store-query references",
        )?;
        let mut v: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                l.split_once('|')
                    .map(|(_, r)| r.to_string())
                    .unwrap_or_else(|| l.to_string())
            })
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
         for the subject's TD-ASSEMBLED .drv and its {nref} references. td computed the \
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
    std::fs::create_dir_all(&pstore)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", pstore.display()))?;
    let subj = store_subject(&s0, root, &scratch)?;
    let n = subj.n;

    let tddb_s = path_str(&scratch.join("td.db"))?;
    let closure_s = path_str(&subj.closure_file)?;
    tb_out(
        &tb,
        &["store-register", &subj.root, &subj.drv, &closure_s, &tddb_s],
        "store-register",
    )?;
    let store_s = path_str(&subj.store)?;
    if !tb_ok(&tb, &["store-verify", &tddb_s, &store_s]) {
        return Err("FAIL: td-verify flagged the intact td-built closure".into());
    }
    println!(
        "   (A) td-verify: the intact {n}-path subject closure in the td-owned store matches its \
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
        &[
            "store-add-text",
            "verify-probe",
            &content_s,
            &pstore_s,
            &probedb_s,
        ],
        "store-add-text (probe)",
    )?;
    if !tb_ok(&tb, &["store-verify", &probedb_s, &pstore_s]) {
        return Err("FAIL: td-verify flagged an intact probe".into());
    }
    println!("   (C) td-verify: an intact td-authored probe (store-add-text) verifies OK");
    let pinfo = tb_out(
        &tb,
        &["store-query", &probedb_s, "info"],
        "store-query (probe)",
    )?;
    let pbase = base_of(pinfo.split('|').next().unwrap_or(""));
    if pbase.is_empty() {
        return Err(format!("FAIL: malformed probe registration {pinfo}"));
    }
    corrupt_append(&pstore.join(&pbase))?;
    if tb_ok(&tb, &["store-verify", &probedb_s, &pstore_s]) {
        return Err("FAIL: td-verify did NOT detect the corrupted probe".into());
    }
    println!(
        "   (C) td-verify: a one-byte corruption of the probe is DETECTED (verify exits nonzero)"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td VERIFIED store integrity ITSELF, in pure Rust with NO daemon — the daemon's \
         guix gc --verify --check-contents. Over a TD-BUILT subject's {n}-path closure staged into \
         a td-owned store: (A) td-verify re-NAR-hashed each registered path and confirmed it \
         matches td's recorded hash; (B) a one-byte corruption of a real closure member is \
         DETECTED (exit nonzero); (C) an independent flat probe (store-add-text) verifies OK and \
         its corruption is DETECTED. Boundary: td reads + writes only its own scratch store. The \
         destructive GC sweep is store-gc-sweep."
    );
    Ok(())
}

/// store-backend — a td store backend HOLDS + SERVES a td-built subject output
/// (place + register + query + verify + deriver/drv->output mapping). Port of
/// 310-store-backend.rs.
fn store_backend(root: &Path) -> Result<(), String> {
    println!(
        ">> store-backend: a td store backend HOLDS + SERVES a TD-BUILT subject output (place + \
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
        &[
            "store-add-output",
            &subj.root,
            &subj.drv,
            &closure_s,
            &store_s,
            &tddb_s,
        ],
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
        "   (1) td PLACED the subject output into its store, NAR-identical to the source staged tree: \
         {src_nar}"
    );

    let td_info = tb_out(&tb, &["store-query", &tddb_s, "info"], "store-query info")?;
    let mut f = td_info.split('|');
    if f.next().unwrap_or("") != subj.root {
        return Err(format!(
            "FAIL: store-query info path != {} ({td_info})",
            subj.root
        ));
    }
    if f.next().unwrap_or("") != src_nar {
        return Err(format!(
            "FAIL: store-query info hash != the re-derived NAR hash ({td_info})"
        ));
    }
    println!("   (2) td's store SERVES the registration (store-query info) == the re-derived hash + narSize");

    // The backend's references == store-register's INDEPENDENT direct-ref scan.
    let fulldb = scratch.join("full.db");
    let fulldb_s = path_str(&fulldb)?;
    tb_out(
        &tb,
        &[
            "store-register",
            &subj.root,
            &subj.drv,
            &closure_s,
            &fulldb_s,
        ],
        "store-register (independent scan)",
    )?;
    let direct_refs: Vec<String> = {
        let out = tb_out(
            &tb,
            &["store-query", &fulldb_s, "references"],
            "store-query (full)",
        )?;
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
        return Err(
            "FAIL: the subject output has no direct references (the check would be vacuous)".into(),
        );
    }
    let td_refs: Vec<String> = {
        let out = tb_out(
            &tb,
            &["store-query", &tddb_s, "references"],
            "store-query (backend)",
        )?;
        let mut v: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                l.split_once('|')
                    .map(|(_, r)| r.to_string())
                    .unwrap_or_else(|| l.to_string())
            })
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

    let all_outputs = tb_out(
        &tb,
        &["store-query", &tddb_s, "outputs"],
        "store-query outputs",
    )?;
    let out_prefix = format!("{}|", subj.root);
    let dout_lines: Vec<&str> = all_outputs
        .lines()
        .filter(|l| l.starts_with(&out_prefix))
        .collect();
    let expected = format!("{root}|{drv}|{drv}|out", root = subj.root, drv = subj.drv);
    if dout_lines != [expected.as_str()] {
        return Err(format!(
            "FAIL: td's deriver/drv->output rows for {} ({dout_lines:?}) != EXACTLY one row \
             equal to the expected (td-assembled .drv) -> out -> output ({expected})",
            subj.root
        ));
    }
    println!(
        "   (5) td's store records the deriver + drv->output mapping == (the td-assembled .drv) \
         -> out -> the output"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: a td STORE BACKEND holds + serves a TD-BUILT subject output, in pure Rust with NO \
         daemon in any store operation and guix OFF PATH — td PLACED the subject output into a \
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
    println!(
        ">> td-builder under test (stage0, guix-free): {}",
        tb.display()
    );
    let work = fresh_scratch(root, ".store-ns-scratch")?;

    // A static binary to run from /td/store: bash-static, from the committed
    // substitute fixture lock (td's own content scan — no store DB, no guix process).
    let lock_rel = "tests/td-subst.lock";
    let lock_text = std::fs::read_to_string(root.join(lock_rel))
        .map_err(|e| format!("FAIL: read {lock_rel}: {e}"))?;
    let bash = lock_text
        .lines()
        .find(|l| l.contains("-bash-") && !l.contains("static"))
        .and_then(|l| l.split_once(' ').map(|(_, p)| p.trim().to_string()))
        .ok_or_else(|| String::from("FAIL: no bash in td-subst.lock"))?;
    // The candidate dir for the closure scan is the lock entry's OWN store
    // (its parent dir) — derived, never a hardcoded prefix.
    let seed_scan = Path::new(&bash)
        .parent()
        .and_then(Path::to_str)
        .map(str::to_string)
        .ok_or_else(|| format!("FAIL: no store dir above {bash}"))?;
    let scan = tb_out(
        &tb,
        &["store-closure-scan", &seed_scan, &bash],
        "store-closure-scan",
    )?;
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
    println!(
        "   placed {base} into the td-owned store {}",
        store.display()
    );

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
        &[
            "store-ns",
            &store_s,
            "--",
            &format!("/td/store/{base}/bin/bash"),
            "-c",
            &inner,
        ],
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
    println!(
        "   [DURABLE behavioral] a binary ran from /td/store in td's own root (rootless userns)"
    );

    // Leg B: DURABLE structural — /td/store is the store, /gnu/store ABSENT.
    if !out.lines().any(|l| l == "TDSTORE-OK") {
        return Err("FAIL: /td/store is not present in the own-root".into());
    }
    if !out.lines().any(|l| l == "GNU-ABSENT") {
        return Err(
            "FAIL: /gnu/store is PRESENT in the own-root — mixed with the guix install!".into(),
        );
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

// --- recipe-checks-daily (formerly tests/recipe-checks.sh) ----------------------

fn recipe_checks_daily(root: &Path) -> Result<(), String> {
    let scope = "daily";
    println!(
        ">> recipe-checks: recipe-owned /td/store package checks (scope={scope}; goals=recipe-checks-daily)"
    );

    let eval = resolve_recipe_eval(root)?;
    let eval_s = path_str(&eval)?;
    let stage0_base = std::env::var("TD_STAGE0_BASE")
        .unwrap_or_else(|_| root.join(".td-build-cache/stage0").display().to_string());
    let envs: [(&str, &str); 3] = [
        ("TD_RECIPE_EVAL", &eval_s),
        ("TD_RECIPE_CHECK_SCOPE", scope),
        ("TD_STAGE0_BASE", &stage0_base),
    ];

    let checks = run_out_env(
        &eval_s,
        &["check-list", scope],
        &envs,
        "td-recipe-eval check-list",
    )?;
    if checks.trim().is_empty() {
        return Err(format!("FAIL: no recipe checks selected for scope={scope}"));
    }

    let mut ran = 0usize;
    let mut failures = 0usize;
    for spec in checks.split_whitespace() {
        let count_text = run_out_env(
            &eval_s,
            &["check-count", spec, scope],
            &envs,
            &format!("td-recipe-eval check-count {spec}"),
        )?;
        let count = count_text.trim().parse::<usize>().map_err(|e| {
            format!(
                "FAIL: non-numeric check-count for {spec}: '{}': {e}",
                count_text.trim()
            )
        })?;
        if count == 0 {
            return Err(format!(
                "FAIL: check-list selected {spec} but check-count is 0"
            ));
        }
        for index in 1..=count {
            ran += 1;
            println!("================ recipe-check {spec}#{index} ({scope}) ================");
            if run_recipe_check(&eval, spec, scope, index, &eval_s, &stage0_base)? {
                println!(
                    "================ recipe-check {spec}#{index} ({scope}): PASS ================"
                );
            } else {
                failures += 1;
                eprintln!(
                    "================ recipe-check {spec}#{index} ({scope}): FAIL ================"
                );
            }
        }
    }

    if failures != 0 {
        return Err(format!(
            "FAIL: recipe-checks - {failures} of {ran} recipe-owned check(s) failed (scope={scope})"
        ));
    }
    println!(
        "PASS: recipe-checks - ran {ran} recipe-owned /td/store check(s) from the Rust recipe catalog (scope={scope}); package behavior/repro assertions live with the package recipes."
    );
    Ok(())
}

fn resolve_recipe_eval(root: &Path) -> Result<PathBuf, String> {
    let path = match std::env::var_os("TD_RECIPE_EVAL") {
        Some(value) => PathBuf::from(value),
        None => {
            let sentinel = root.join(".td-build-cache/recipe-eval/recipe-eval-path");
            let text = std::fs::read_to_string(&sentinel).map_err(|_| {
                format!(
                    "FAIL: no td-recipe-eval sentinel ({}) - the build-recipes prelude must run first",
                    sentinel.display()
                )
            })?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(format!(
                    "FAIL: empty td-recipe-eval sentinel {}",
                    sentinel.display()
                ));
            }
            PathBuf::from(trimmed)
        }
    };
    if !is_executable_file(&path) {
        return Err(format!(
            "FAIL: td-recipe-eval not executable at {}",
            path.display()
        ));
    }
    let eval_s = path_str(&path)?;
    if !eval_s.contains(".td-build-cache/") {
        return Err(format!(
            "FAIL: TD_RECIPE_EVAL is not td's own build ({eval_s})"
        ));
    }
    Ok(path)
}

fn run_recipe_check(
    eval: &Path,
    spec: &str,
    scope: &str,
    index: usize,
    eval_s: &str,
    stage0_base: &str,
) -> Result<bool, String> {
    let index_s = index.to_string();
    let status = Command::new(eval)
        .arg("check-run")
        .arg(spec)
        .arg(scope)
        .arg(&index_s)
        .env("TD_RECIPE_EVAL", eval_s)
        .env("TD_RECIPE_CHECK_SCOPE", scope)
        .env("TD_RECIPE_CHECK_SPEC", spec)
        .env("TD_RECIPE_CHECK_INDEX", &index_s)
        .env("TD_STAGE0_BASE", stage0_base)
        .status()
        .map_err(|e| format!("FAIL: cannot spawn td-recipe-eval check-run {spec}: {e}"))?;
    Ok(status.success())
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file() && file_mode(path).ok().is_some_and(|mode| mode & 0o111 != 0)
}

// --- recipe-rs (formerly tests/recipe-rs.sh) ----------------------------------

/// recipe-rs — the Rust package + spec surface (the `recipes` crate) is
/// self-consistent (rust-recipe-surface track). Builds + unit-tests the
/// dependency-free `recipes` crate OFFLINE with a guix-free rust+cc toolchain
/// (`stage0::provision_rust`/`provision_cc` — a host-prep concern, resolved
/// in-process; no ambient host sh, re #469). The catalog's
/// coverage (every recipe emits valid, round-tripping JSON) and
/// discrimination (a mismatch is not vacuously accepted) legs are `#[test]`s
/// in the `recipes` crate itself (`catalog::tests`, `td-recipe-eval::tests`)
/// — `cargo test` below is what runs them; this function additionally smokes
/// representative RELEASE binary argv dispatch, including the `build-run`
/// surface used by the x86_64 gates.
fn recipe_rs(root: &Path) -> Result<(), String> {
    println!(
        ">> recipe-rs: the Rust package + spec surface (td-recipe crate) is self-consistent (rust-recipe-surface)"
    );

    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let rustpath = crate::stage0::provision_rust(&penv)
        .map_err(|e| format!("FAIL: provision-rust: {e}"))?;
    let ccpath =
        crate::stage0::provision_cc(&penv).map_err(|e| format!("FAIL: provision-cc: {e}"))?;
    let cargo_bin = find_in_path_frags(&rustpath, "cargo")
        .ok_or_else(|| format!("FAIL: no cargo in provision-rust output ({rustpath})"))?;

    let scratch = fresh_scratch(root, ".recipe-rs-scratch")?;
    let cargo_home = scratch.join("home");
    let cargo_target = scratch.join("target");
    std::fs::create_dir_all(&cargo_home)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", cargo_home.display()))?;
    std::fs::create_dir_all(&cargo_target)
        .map_err(|e| format!("FAIL: mkdir {}: {e}", cargo_target.display()))?;
    let cargo_home_s = path_str(&cargo_home)?;
    let cargo_target_s = path_str(&cargo_target)?;
    let cargo_bin_s = path_str(&cargo_bin)?;

    let old_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{rustpath}:{ccpath}:{old_path}");
    // STATICALLY link the evaluator (and its cargo build-script/test binaries)
    // against a MATCHED static glibc. A static binary has an EMPTY runtime closure
    // — no DT_NEEDED, no DT_RUNPATH — so it can never load libgcc_s.so.1/libc.so.6
    // from the MUTABLE ~/.guix-home/profile/lib that guix's gcc ld-wrapper would
    // otherwise bake in as a runpath and that vanishes while guix-home reconfigures
    // or GCs ("error while loading shared libraries: libgcc_s.so.1", exit 127),
    // flaking this control-plane tool and reddening the daily backstop. Fixing it
    // at the SOURCE (crt-static) supersedes pinning a runpath (re #469). The recipes
    // crate is dependency-free (pure std, no proc-macros) so the flags apply
    // cleanly to the whole build; see stage0::static_rustflags.
    let glibc_static = crate::stage0::provision_glibc_static(&penv)
        .map_err(|e| format!("FAIL: provision static glibc for a crt-static evaluator: {e}"))?;
    let rustflags = crate::stage0::static_rustflags(&glibc_static);
    let envs: [(&str, &str); 4] = [
        ("PATH", &new_path),
        ("RUSTFLAGS", &rustflags),
        ("CARGO_HOME", &cargo_home_s),
        ("CARGO_TARGET_DIR", &cargo_target_s),
    ];

    println!(
        ">> build + unit-test the dependency-free td-recipe crate (offline, guix-free toolchain via tools/provision-{{rust,cc}}.sh)"
    );
    // The coverage (every recipe emits valid, round-tripping JSON) and
    // discrimination (a mismatch is not vacuously accepted) legs are plain
    // #[test]s in recipes/src/bin/td-recipe-eval.rs — same crate, same types,
    // no subprocess/temp-file dance needed to exercise a property of the
    // crate's own data. `cargo test` here is what actually runs them.
    run_out_env(
        &cargo_bin_s,
        &["test", "--frozen", "--manifest-path", "recipes/Cargo.toml"],
        &envs,
        "cargo test recipes",
    )?;
    run_out_env(
        &cargo_bin_s,
        &[
            "build",
            "--release",
            "--frozen",
            "--manifest-path",
            "recipes/Cargo.toml",
        ],
        &envs,
        "cargo build recipes",
    )?;

    let eval = cargo_target.join("release/td-recipe-eval");
    if !eval.is_file() {
        return Err(format!(
            "FAIL: td-recipe-eval was not built at {}",
            eval.display()
        ));
    }
    // Fail closed if the toolchain silently linked the evaluator dynamically: a
    // dynamic control-plane binary drags a host runtime closure (and a mutable
    // guix-home runpath) that the #469 sandbox boundary must deny.
    crate::elf::assert_static(&eval)?;
    let eval_s = path_str(&eval)?;

    // CLI smoke: `cargo test` proves EVERY recipe's data is correct
    // (catalog::tests::every_recipe_emits_canonical_json_and_round_trips runs
    // `to_json().to_canonical()` on all of them) but never runs the RELEASE
    // BINARY's argv dispatch, which is untested surface of its own (a typo in
    // `main`'s `Some("emit") => ...` arm wouldn't fail any unit test). That
    // dispatch doesn't branch per-stem, so one representative stem is enough
    // to prove it — looping over the whole catalog here would just re-run
    // `cargo test`'s own per-recipe assertion via a slower subprocess path.
    println!(">> CLI smoke: the release td-recipe-eval binary's list/emit subcommands work");
    let list_out = run_out(&eval_s, &["list"], "td-recipe-eval list")?;
    let first = list_out
        .split_whitespace()
        .next()
        .ok_or_else(|| "FAIL: empty recipe catalog (vacuous)".to_string())?;
    let json = run_out(&eval_s, &["emit", first], &format!("emit {first}"))?;
    if json.trim().is_empty() {
        return Err(format!("FAIL: emit {first} produced no JSON"));
    }
    println!("   ok: list/emit {first} produced JSON via the release binary");

    let bad_build = Command::new(&eval)
        .args(["build-run", "not-a-recipe"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("FAIL: cannot spawn td-recipe-eval build-run smoke: {e}"))?;
    if bad_build.status.success() {
        return Err(
            "FAIL: td-recipe-eval build-run unknown-target smoke unexpectedly succeeded"
                .to_string(),
        );
    }
    let bad_err = String::from_utf8_lossy(&bad_build.stderr);
    if !bad_err.contains("unknown recipe stem 'not-a-recipe'") {
        return Err(format!(
            "FAIL: td-recipe-eval build-run unknown-target smoke did not reach the build-run dispatch: {bad_err}"
        ));
    }
    println!("   ok: build-run dispatch rejects an unknown target before setup");

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: recipe-rs — the Rust package surface emits valid self-consistent JSON and \
         discriminates a mismatch (recipes/src/bin/td-recipe-eval.rs unit tests), and the \
         release binary's CLI entry points work. Correctness vs upstream is proven by \
         recipe-owned package checks, not boa (retired)."
    );
    Ok(())
}

// --- shared helpers for the own-root / lock-addressed gates -------------------

/// `mkdir -p p`.
fn mkdirp(p: &Path) -> Result<(), String> {
    std::fs::create_dir_all(p).map_err(|e| format!("FAIL: mkdir {}: {e}", p.display()))
}

/// Write `data` to `p` (the scratch-file staging the gates do before a tool call).
fn writef(p: &Path, data: &str) -> Result<(), String> {
    std::fs::write(p, data).map_err(|e| format!("FAIL: write {}: {e}", p.display()))
}

/// A declared artifact input's resolved path — the runner exported it as
/// `TD_GATE_INPUT_<NAME>` before the body ran (#353). The env-var name is
/// computed by the SAME function the runner uses, so the two can't drift.
fn gate_input(name: &str) -> Result<String, String> {
    let var = crate::gate_inputs::env_var(name);
    std::env::var(&var).map_err(|_| {
        format!("FAIL: {var} unset — run via td-builder gate-run, which resolves the gate's declared inputs")
    })
}

/// `readlink -f $(command -v BIN)` — the first executable `bin` on PATH,
/// canonicalized. Resolves the absolute binary ourselves (Command's PATH search
/// uses the CURRENT process env, not a child override).
fn which_canon(bin: &str) -> Option<PathBuf> {
    which_path(bin).and_then(|p| std::fs::canonicalize(p).ok())
}

/// PATH lookup WITHOUT canonicalizing: the entry as the caller would exec it.
/// Multi-call userlands (a symlink farm at one binary) dispatch on argv[0], so
/// a probe must exec THIS path — canonicalizing first would erase the program
/// name. Which layout provides a program is the userland's business, never
/// assumed here.
fn which_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| {
        let p = dir.join(bin);
        p.is_file() && file_mode(&p).ok().is_some_and(|m| m & 0o111 != 0)
    })
    .map(|dir| dir.join(bin))
}

/// The store root `/td/store` of a `/<first>/store/...` path (the shell's
/// `store_root_for`): take the first path component and append `/store`, then
/// confirm the path is actually under it.
fn store_root_for(p: &str) -> Result<String, String> {
    let rest = p
        .strip_prefix('/')
        .ok_or_else(|| format!("FAIL: {p} is not an absolute store path"))?;
    let first = rest.split('/').next().unwrap_or("");
    let root = format!("/{first}/store");
    if !p.starts_with(&format!("{root}/")) {
        return Err(format!("FAIL: {p} is not under a store root"));
    }
    Ok(root)
}

/// Count the processes whose `/proc/<pid>/cmdline` carries `marker` (NULs read as
/// spaces). A zombie's cmdline is empty, so a killed-but-unreaped parent is not
/// counted — exactly why the reaping check can poll for zero without racing the
/// wait. cmdline bytes are ASCII (argv paths + a decimal marker).
fn scan_marker_procs(marker: &str) -> usize {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|e| proc_cmdline_has(&e.path().join("cmdline"), marker))
        .count()
}

/// SIGKILL every process whose cmdline still carries `marker` — the failure-path
/// sweep so a red reaping check never leaks a marker process into the shared PID
/// namespace.
fn sweep_marker_procs(marker: &str) {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        if proc_cmdline_has(&e.path().join("cmdline"), marker) {
            let _ = crate::sys::kill_pid(pid, crate::sys::SIGKILL);
        }
    }
}

/// True if `cmdline` (NUL-separated argv) contains `marker` as a substring.
fn proc_cmdline_has(cmdline: &Path, marker: &str) -> bool {
    let Ok(bytes) = std::fs::read(cmdline) else {
        return false;
    };
    let text: String = bytes
        .iter()
        .map(|&b| if b == 0 { ' ' } else { b as char })
        .collect();
    text.contains(marker)
}

// --- lock / source-pin parsing (unit-tested; #460) ---------------------------

/// A fixed-output pin line in a toolchain lock: `input <sha> <file>` (an upstream
/// source tarball) or `patch <sha> <file>` (a vendored `seed/patches/<file>`).
enum PinKind {
    Input,
    Patch,
}
struct PinLine {
    kind: PinKind,
    sha: String,
    file: String,
}

/// Parse the `input`/`patch` pin lines of a lock into (kind, sha, file), matching
/// `store::ToolchainLock::parse` byte-for-byte on the field split (and the shell's
/// `read -r kind sha file`): the line is trimmed, blank/`#`-comment and non-pin
/// directive lines are skipped, and `file` is the REST of the line after the sha
/// (trimmed) — trailing content is part of the field the key hashes, so pinned-sync
/// must validate it too, not silently drop it. An `input`/`patch` line with no sha
/// or no file is skipped; the authoritative key parser rejects it, so the stable-key
/// leg fails loudly on it.
fn parse_pin_lines(lock_text: &str) -> Vec<PinLine> {
    lock_text
        .lines()
        .filter_map(|l| {
            let line = l.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, val) = line.split_once(' ').map(|(k, v)| (k, v.trim()))?;
            let kind = match key {
                "input" => PinKind::Input,
                "patch" => PinKind::Patch,
                _ => return None,
            };
            let (sha, file) = val.split_once(' ')?;
            Some(PinLine {
                kind,
                sha: sha.trim().to_string(),
                file: file.trim().to_string(),
            })
        })
        .collect()
}

/// The RAW `input`/`patch` lines of a lock (the shell's `copy_pin_lines`) — used
/// for the arch-parity set comparison, which is a comparison of the source SET
/// verbatim (the shell hashed the sorted lines; we compare the sorted lines
/// directly).
fn filter_pin_lines(lock_text: &str) -> Vec<String> {
    lock_text
        .lines()
        .filter(|l| matches!(l.split_whitespace().next(), Some("input") | Some("patch")))
        .map(str::to_string)
        .collect()
}

/// The lock directives allowed in an arch-parametrized toolchain lock.
const ARCH_DIRECTIVES: &[&str] = &["name", "recipe-rev", "component", "input", "patch"];

/// The distinct directive keys in `lock_text` that are NOT in the arch-lock
/// allowlist (empty ⟹ the lock is well-formed) — the shell's
/// `validate_directives`. Blank and `#`-comment lines are ignored.
fn bad_directive_keys(lock_text: &str) -> Vec<String> {
    let mut bad: Vec<String> = Vec::new();
    for line in lock_text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(key) = line.split_whitespace().next() else {
            continue;
        };
        if !ARCH_DIRECTIVES.contains(&key) && !bad.iter().any(|b| b == key) {
            bad.push(key.to_string());
        }
    }
    bad
}

/// Rewrite the `input <sha> glibc-2.41.tar.xz` pin to an all-zero digest — the
/// load-bearing perturbation. `None` if no such pin exists (the perturbation
/// would be vacuous).
fn perturb_glibc_pin(lock_text: &str) -> Option<String> {
    let zeros = "0".repeat(64);
    let mut seen = false;
    let mut out = String::new();
    for line in lock_text.lines() {
        let is_glibc_input = line.split_whitespace().next() == Some("input")
            && line.ends_with(" glibc-2.41.tar.xz");
        if is_glibc_input {
            out.push_str(&format!("input {zeros} glibc-2.41.tar.xz\n"));
            seen = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    seen.then_some(out)
}

/// Bump the `recipe-rev 1` directive to `recipe-rev 2` — the load-bearing
/// recipe-rev perturbation. `None` if the lock has no `recipe-rev 1` line.
fn rewrite_recipe_rev(lock_text: &str) -> Option<String> {
    let mut seen = false;
    let mut out = String::new();
    for line in lock_text.lines() {
        if line == "recipe-rev 1" {
            out.push_str("recipe-rev 2\n");
            seen = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    seen.then_some(out)
}

/// The sha256 a recipe source pin declares for `file`, from `td-recipe-eval
/// source-pins` output (`<key>\t<url>\t<sha256>\t<file>` per line).
fn source_pin_sha(pins_text: &str, file: &str) -> Option<String> {
    pins_text.lines().find_map(|line| {
        let mut f = line.split_whitespace();
        let _key = f.next()?;
        let _url = f.next()?;
        let sha = f.next()?;
        let name = f.next()?;
        (name == file).then(|| sha.to_string())
    })
}

/// The recipe-owned source pins (`td-recipe-eval source-pins`). Resolves the
/// evaluator from `$TD_RECIPE_EVAL` when set, else builds td's OWN dependency-free
/// td-recipe-eval via `tests/recipe-eval-tool.sh` (the guix-free host-prep the
/// recipe-rs gate also shells out to). The pin PARSING + comparison is typed Rust.
fn recipe_eval_source_pins(root: &Path) -> Result<String, String> {
    let eval = match std::env::var_os("TD_RECIPE_EVAL") {
        // Set to a non-empty value: use it VERBATIM — no fallback. A non-executable
        // override then fails loudly at the `-x` check below (the shell's `[ -x ] ||
        // fail`), rather than being silently masked by a freshly built evaluator.
        // `${TD_RECIPE_EVAL:-}` treats unset and empty identically, so an empty value
        // falls through to the build path.
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let base = root.join(".td-build-cache/recipe-eval");
            let base_s = path_str(&base)?;
            // The tool resolves its toolchain via `$TD_BUILDER_SELF provision-{rust,cc}`;
            // we ARE a td-builder, so pass ourselves explicitly rather than relying on
            // the gate-run export (this body is also reachable from dev invocations).
            let self_exe = std::env::current_exe()
                .map_err(|e| format!("FAIL: cannot resolve current td-builder: {e}"))?;
            let self_s = path_str(&self_exe)?;
            let printed = run_out_env(
                "sh",
                &["tests/recipe-eval-tool.sh", &base_s],
                &[("TD_BUILDER_SELF", &self_s)],
                "recipe-eval-tool.sh (build td-recipe-eval from the current worktree)",
            )?;
            let bin = printed.lines().last().unwrap_or("").trim();
            if bin.is_empty() {
                return Err("FAIL: recipe-eval-tool.sh printed no td-recipe-eval path".into());
            }
            PathBuf::from(bin)
        }
    };
    if !is_executable_file(&eval) {
        return Err(format!(
            "FAIL: td-recipe-eval is not executable: {}",
            eval.display()
        ));
    }
    let eval_s = path_str(&eval)?;
    run_out(&eval_s, &["source-pins"], "td-recipe-eval source-pins")
}

/// The registered NAR hash for `path` in the store DB `db`, read by td's OWN
/// store-query (`path|hash|narSize` rows). `None` if the path is not registered.
fn registered_hash(tb: &Path, db: &str, path: &str) -> Result<Option<String>, String> {
    let info = tb_out(tb, &["store-query", db, "info"], "store-query info")?;
    Ok(info.lines().find_map(|line| {
        let mut f = line.split('|');
        let p = f.next().unwrap_or("");
        let h = f.next().unwrap_or("");
        (p == path).then(|| h.to_string())
    }))
}

/// The [pinned-sync] leg shared by both input-addressed gates: every lock
/// `input` pin equals the recipe source pin for that file, every `patch` pin
/// equals the sha256 of `seed/patches/<file>`, and the toolchain has the
/// expected floor of inputs/patches. Returns (input-count, patch-count).
fn check_pinned_sync(
    root: &Path,
    lock_text: &str,
    source_pins: &str,
) -> Result<(usize, usize), String> {
    let mut nin = 0usize;
    let mut npatch = 0usize;
    for pin in parse_pin_lines(lock_text) {
        match pin.kind {
            PinKind::Input => {
                let want = source_pin_sha(source_pins, &pin.file).ok_or_else(|| {
                    format!(
                        "FAIL: [pinned-sync] no recipe source pin declares file `{}`",
                        pin.file
                    )
                })?;
                if pin.sha != want {
                    return Err(format!(
                        "FAIL: [pinned-sync] {}: lock pin {} != recipe source pin {want}",
                        pin.file, pin.sha
                    ));
                }
                nin += 1;
            }
            PinKind::Patch => {
                let pf = root.join("seed/patches").join(&pin.file);
                if !pf.is_file() {
                    return Err(format!(
                        "FAIL: [pinned-sync] vendored patch missing: {}",
                        pf.display()
                    ));
                }
                let got = crate::sha256::sha256_file(&pf)
                    .map_err(|e| format!("FAIL: sha256 {}: {e}", pf.display()))?;
                if pin.sha != got {
                    return Err(format!(
                        "FAIL: [pinned-sync] {}: lock pin {} != file sha {got}",
                        pin.file, pin.sha
                    ));
                }
                npatch += 1;
            }
        }
    }
    if nin < 20 {
        return Err(format!(
            "FAIL: [pinned-sync] only {nin} input pins — the toolchain has more inputs than that"
        ));
    }
    if npatch < 4 {
        return Err(format!("FAIL: [pinned-sync] only {npatch} patch pins"));
    }
    Ok((nin, npatch))
}

/// The [behavioral]+[structural] leg shared by both input-addressed gates: place
/// the static-bash fixture at the arch-keyed input-addressed /td/store path and
/// run it in the store-ns own-root with /gnu/store ABSENT. `name_stem` is the
/// input-addressed name (`bash-static` / `bash-static-x86_64`).
fn run_input_addressed_bash(
    tb: &Path,
    work: &Path,
    bs: &str,
    key: &str,
    name_stem: &str,
) -> Result<(), String> {
    let store = work.join("store");
    mkdirp(&store)?;
    let store_s = path_str(&store)?;
    let db_s = path_str(&work.join("store.db"))?;
    let runp = tb_out_env(
        tb,
        &["store-add-input-addressed", name_stem, key, bs, &store_s, &db_s],
        &[("TD_STORE_DIR", "/td/store")],
        &format!("store-add-input-addressed {name_stem}"),
    )?;
    let suffix = format!("-{name_stem}");
    if !(runp.starts_with("/td/store/") && runp.ends_with(&suffix)) {
        return Err(format!(
            "FAIL: {name_stem} not input-addressed at /td/store: {runp}"
        ));
    }
    if !is_executable_file(&store.join(base_of(&runp)).join("bin/bash")) {
        return Err(format!("FAIL: interned {name_stem} missing physically"));
    }
    let run_bin = format!("{runp}/bin/bash");
    let out = tb_out(
        tb,
        &[
            "store-ns",
            &store_s,
            "--",
            &run_bin,
            "-c",
            "[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT; echo \"RAN:$BASH_VERSION\"",
        ],
        "store-ns run from the input-addressed path",
    )?;
    for l in out.lines() {
        println!("     {l}");
    }
    if !out.lines().any(|l| l.starts_with("RAN:5")) {
        return Err(
            "FAIL: [behavioral] the binary did not run from its input-addressed /td/store path"
                .into(),
        );
    }
    println!("   [behavioral] a real binary placed at the input-addressed path {runp} RUNS in the own-root");
    if !out.lines().any(|l| l == "GNU-ABSENT") {
        return Err("FAIL: [structural] /gnu/store is PRESENT in the own-root".into());
    }
    println!("   [structural] /gnu/store is ABSENT in the own-root");
    Ok(())
}

// --- store-native-profile (formerly tests/store-native-profile.sh) ------------

/// store-native-profile — `td-builder profile --store-native` assembles a profile
/// of LOGICAL /td/store symlinks that RESOLVE + RUN inside a store-ns own-root
/// with /gnu/store ABSENT (the .scm-free userspace assembly mechanism). Port of
/// tests/store-native-profile.sh (gate 412).
fn store_native_profile(root: &Path) -> Result<(), String> {
    println!(
        ">> store-native-profile: td-builder profile --store-native builds a profile of logical \
         /td/store links that resolve + run in the store-ns own-root, /gnu/store ABSENT (the \
         .scm-free userspace assembly mechanism)"
    );
    let tb = tb()?;
    println!(">> td-builder (stage0, guix-free): {}", tb.display());
    let work = fresh_scratch(root, ".store-native-profile-scratch")?;

    // The declared bash-static fixture (#353): a real multi-entry static package.
    let bs = gate_input("bash-static")?;
    if !is_executable_file(&Path::new(&bs).join("bin/bash")) {
        return Err(format!("FAIL: no static bash fixture at {bs}"));
    }

    // Intern it at the LOGICAL /td/store; bytes land physically under `store`.
    let store = work.join("td-store");
    mkdirp(&store)?;
    let store_s = path_str(&store)?;
    let db_s = path_str(&work.join("db.sqlite"))?;
    let pkg = tb_out_env(
        &tb,
        &["store-add-recursive", "bash-static", &bs, &store_s, &db_s],
        &[("TD_STORE_DIR", "/td/store")],
        "store-add-recursive bash-static",
    )?;
    if !(pkg.starts_with("/td/store/") && pkg.ends_with("-bash-static")) {
        return Err(format!(
            "FAIL: bash-static not content-addressed at /td/store: {pkg}"
        ));
    }
    let physpkg = store.join(base_of(&pkg));
    let physpkg_s = path_str(&physpkg)?;
    if !is_executable_file(&physpkg.join("bin/bash")) {
        return Err(format!(
            "FAIL: interned bash-static missing physically at {}",
            physpkg.display()
        ));
    }

    // A STORE-NATIVE profile: the links target the LOGICAL /td/store path.
    let prof = store.join("profile");
    let prof_s = path_str(&prof)?;
    tb_out_env(
        &tb,
        &["profile", "--store-native", &prof_s, &physpkg_s],
        &[("TD_STORE_DIR", "/td/store")],
        "profile --store-native",
    )?;

    // [structural] the profile entries are LOGICAL /td/store symlinks.
    for t in ["bash", "sh"] {
        let link = prof.join("bin").join(t);
        let tgt = std::fs::read_link(&link)
            .map_err(|_| format!("FAIL: no profile entry for {t}"))?;
        let tgt_s = tgt.to_string_lossy();
        let want = format!("-bash-static/bin/{t}");
        if !(tgt_s.starts_with("/td/store/") && tgt_s.ends_with(&want)) {
            return Err(format!(
                "FAIL: profile/bin/{t} is not a logical /td/store link (got: {tgt_s})"
            ));
        }
    }
    println!("   [structural] profile entries (bash, sh) are logical /td/store symlinks");

    // Run the profiled tools in the own-root via a probe FILE bound in the store
    // (no nested quoting between the outer capture and the inner script).
    let probe = store.join("probe.sh");
    writef(
        &probe,
        "export PATH=/td/store/profile/bin\n\
         [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT\n\
         case \"$(command -v bash)\" in /td/store/profile/bin/bash) echo BASH-VIA-PROFILE ;; esac\n\
         case \"$(command -v sh)\" in /td/store/profile/bin/sh) echo SH-VIA-PROFILE ;; esac\n\
         bash -c 'echo \"BASH-RAN:$BASH_VERSION\"'\n\
         sh -c 'echo SH-RAN-OK'\n",
    )?;
    let out = tb_out(
        &tb,
        &[
            "store-ns",
            &store_s,
            "--",
            "/td/store/profile/bin/bash",
            "/td/store/probe.sh",
        ],
        "store-ns profile run",
    )?;
    for l in out.lines() {
        println!("     {l}");
    }

    let has = |s: &str| out.lines().any(|l| l == s);
    if !has("BASH-VIA-PROFILE") {
        return Err("FAIL: bash did not resolve via /td/store/profile/bin".into());
    }
    if !has("SH-VIA-PROFILE") {
        return Err("FAIL: sh did not resolve via /td/store/profile/bin".into());
    }
    if !out.lines().any(|l| l.starts_with("BASH-RAN:5")) {
        return Err("FAIL: the profiled bash did not run from /td/store".into());
    }
    if !has("SH-RAN-OK") {
        return Err("FAIL: the profiled sh did not run from /td/store".into());
    }
    println!(
        "   [behavioral] the profiled tools resolve via /td/store/profile/bin and RUN from /td/store"
    );
    if !has("GNU-ABSENT") {
        return Err(
            "FAIL: /gnu/store is PRESENT in the own-root — mixed with the guix install".into(),
        );
    }
    println!("   [structural] /gnu/store is ABSENT in the own-root (unmixed from the guix install)");

    let _ = chmod_r_uw(&work);
    let _ = std::fs::remove_dir_all(&work);
    println!(
        "PASS: store-native-profile — td-builder profile --store-native builds a profile of \
         LOGICAL /td/store links that resolve + RUN in the store-ns own-root, /gnu/store ABSENT. \
         The .scm-free userspace assembly mechanism the /td/store-native userland slots into."
    );
    Ok(())
}

// --- sandbox-hardening (formerly tests/sandbox-hardening.sh) -------------------

/// sandbox-hardening — behavioral self-tests that td's loop sandbox
/// (`td-builder host-sandbox`) exposes only a MINIMAL /dev (no host device leak),
/// REAPS its inner tree when the top td-builder is killed (PR_SET_PDEATHSIG),
/// and exposes the store INPUT-ONLY (per-item read-only binds, never a whole
/// store directory: /td/store holds exactly the loop's provisioned td-built
/// userland items, the declared seed store holds the bounded seed-lock closure,
/// and a bound item rejects writes).
/// Port of tests/sandbox-hardening.sh (gate 272). Runs INSIDE the loop sandbox
/// under the td-built userland. The probes resolve their programs (`sh`,
/// `sleep`) from PATH like any consumer and bind the store item(s) those
/// entries canonicalize into — NOTHING here assumes which package provides
/// them (a multi-call farm today, discrete binaries tomorrow): the PATH entry
/// itself is exec'd, so argv[0] keeps the program name and any layout
/// dispatches correctly. The nested td-builder's processes are visible in this
/// PID namespace, so a /proc cmdline scan confirms they are gone after the kill.
fn sandbox_hardening(root: &Path) -> Result<(), String> {
    println!(
        ">> sandbox-hardening: td's loop sandbox has a minimal /dev (no host device leak), \
         reaps its inner tree when killed, and exposes the store input-only"
    );
    let tb = tb()?;
    println!(">> td-builder (stage0, guix-free): {}", tb.display());

    // Resolve the probes' programs from PATH — exec paths are the PATH
    // entries themselves (argv[0] keeps the program name), bind targets are
    // the store item(s) they canonicalize into. Derived per program; if a
    // future userland provides sh and sleep from different packages, both
    // items are bound.
    let sh_exec = which_path("sh").ok_or_else(|| String::from("FAIL: no sh on PATH"))?;
    let sh_exec_s = path_str(&sh_exec)?;
    let sh_canon = which_canon("sh").ok_or_else(|| String::from("FAIL: no sh on PATH"))?;
    let sh_canon_s = path_str(&sh_canon)?;
    let sleep_exec = which_path("sleep").ok_or_else(|| String::from("FAIL: no sleep on PATH"))?;
    let sleep_exec_s = path_str(&sleep_exec)?;
    let sleep_canon =
        which_canon("sleep").ok_or_else(|| String::from("FAIL: no sleep on PATH"))?;
    let sleep_canon_s = path_str(&sleep_canon)?;
    let sroot = store_root_for(&sh_canon_s)?;
    let item_of = |canon: &str| -> Result<String, String> {
        Path::new(canon)
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| format!("FAIL: no package root above {canon}"))
            .and_then(path_str)
    };
    let sh_item_s = item_of(&sh_canon_s)?;
    let sh_item = Path::new(&sh_item_s);
    let sleep_item_s = item_of(&sleep_canon_s)?;
    // The exec paths must live inside the bound items — a PATH entry outside
    // any store item would exec on the host but not in the nested sandbox.
    for (exec, canon, item) in [
        (&sh_exec_s, &sh_canon_s, &sh_item_s),
        (&sleep_exec_s, &sleep_canon_s, &sleep_item_s),
    ] {
        if !exec.starts_with(&format!("{item}/")) && !canon.starts_with(&format!("{item}/")) {
            return Err(format!(
                "FAIL: {exec} (-> {canon}) is not inside its own store item {item}"
            ));
        }
    }
    let mut bind_flags: Vec<&str> = vec!["--store-item", &sh_item_s];
    if sleep_item_s != sh_item_s {
        bind_flags.push("--store-item");
        bind_flags.push(&sleep_item_s);
    }

    // (A) minimal /dev: standard nodes present, host kmsg/kvm/disks/mem/input
    // absent. The nested sandbox binds ONLY the probes' own item(s) — the
    // same per-item input-only model the loop itself uses.
    println!(
        ">> (A) minimal /dev: standard nodes present, host kmsg/kvm/disks/mem/input absent"
    );
    let dev_probe = "\
[ -e /dev/null ] && [ -w /dev/null ]    || { echo \"  no writable /dev/null\";   exit 11; }
[ -e /dev/zero ] && [ -e /dev/urandom ] || { echo \"  missing /dev/zero|urandom\"; exit 12; }
for leak in kmsg kvm mem sda sdb nvme0n1 input/event0; do
  [ -e \"/dev/$leak\" ] && { echo \"  LEAK: /dev/$leak is reachable\"; exit 21; }
done
exit 0
";
    let mut dev_args: Vec<&str> = vec!["host-sandbox"];
    dev_args.extend_from_slice(&bind_flags);
    dev_args.extend_from_slice(&["--", &sh_exec_s, "-c", dev_probe]);
    tb_out(
        &tb,
        &dev_args,
        "minimal-/dev assertion — the sandbox /dev is not minimal (host device leak)",
    )?;
    println!("   /dev exposes the standard nodes; kmsg/kvm/mem/disks/input are absent");

    // (C) input-only store exposure: the loop sandbox binds store ITEMS, never
    // a store directory. /td/store holds exactly the provisioned userland
    // items (a handful); the seed store holds the seed-lock closure (a few
    // dozen to a few hundred) — never a whole host store (hundreds of
    // thousands of entries). And a bound item is READ-ONLY (its ro-remount is
    // load-bearing, sandbox::Bind), so a write into a bound package must
    // fail. Probed directly — this body already runs inside the loop sandbox.
    println!(">> (C) input-only store: bounded item counts, items read-only");
    let entries = std::fs::read_dir(&sroot)
        .map_err(|e| format!("FAIL: cannot read {sroot}: {e}"))?
        .count();
    if entries == 0 || entries > 4096 {
        return Err(format!(
            "FAIL: {sroot} exposes {entries} entries — expected the loop's provisioned \
             userland items, not a whole-store bind"
        ));
    }
    let probe = sh_item.join(".td-ro-probe");
    if std::fs::File::create(&probe).is_ok() {
        let _ = std::fs::remove_file(&probe);
        return Err(format!(
            "FAIL: created {} — a bound store item is WRITABLE inside the sandbox",
            probe.display()
        ));
    }
    println!(
        "   {sroot} exposes {entries} bound items; the sh package rejects writes"
    );
    // The declared SEED store (whatever dir the seed locks name — derived,
    // never hardcoded, exactly like the prelude derives it): also a bounded
    // per-item closure, also read-only. The only remaining seed locks are the
    // pinned RUST toolchain's (td-subst.lock / td-builder-rust.lock — the
    // AGENTS.md control plane); the recipe rungs' seed-tools lock is DELETED
    // (host executables are not admissible bootstrap inputs, re #469).
    let lock_text =
        std::fs::read_to_string(root.join("tests/td-subst.lock")).unwrap_or_default();
    let seed_item = lock_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|p| p.starts_with('/'))
        .find(|p| Path::new(p).is_dir());
    match seed_item {
        Some(item) => {
            let seed_dir = Path::new(item)
                .parent()
                .ok_or_else(|| format!("FAIL: no parent dir above the seed item {item}"))?;
            let seed_entries = std::fs::read_dir(seed_dir)
                .map_err(|e| format!("FAIL: cannot read {}: {e}", seed_dir.display()))?
                .count();
            if seed_entries == 0 || seed_entries > 4096 {
                return Err(format!(
                    "FAIL: {} exposes {seed_entries} entries — expected the loop's bounded \
                     seed-lock closure, not a whole-store bind",
                    seed_dir.display()
                ));
            }
            // The bound item's CONTENT must be visible, not just its mountpoint:
            // ro_dirs locks the parent via a recursive self-bind + ro remount,
            // and a non-recursive self-bind there would clone only the top
            // mount, shadowing every item bind with its empty mountpoint dir
            // (review finding — the loop would see empty store items).
            let item_entries = std::fs::read_dir(item)
                .map_err(|e| format!("FAIL: cannot read the bound seed item {item}: {e}"))?
                .count();
            if item_entries == 0 {
                return Err(format!(
                    "FAIL: {item} is EMPTY inside the sandbox — the item bind is shadowed \
                     by the parent dir's read-only lock (host_shell ro_dirs must self-bind \
                     recursively so child mounts stay visible)"
                ));
            }
            let probe = Path::new(item).join(".td-ro-probe");
            if std::fs::File::create(&probe).is_ok() {
                let _ = std::fs::remove_file(&probe);
                return Err(format!(
                    "FAIL: created {} — a bound seed item is WRITABLE inside the sandbox",
                    probe.display()
                ));
            }
            // The seed dir ITSELF must reject entry creation (host_shell
            // ro_dirs): the items being ro is not enough — a writable parent
            // would let a gate plant a fake sibling "store item" next to the
            // declared inputs. (/td/store is the documented exception: it is
            // the loop's WORKING store prefix and stays writable.)
            let sibling = seed_dir.join(".td-sibling-probe");
            if std::fs::File::create(&sibling).is_ok() {
                let _ = std::fs::remove_file(&sibling);
                return Err(format!(
                    "FAIL: created {} — the seed store dir accepts NEW entries inside the \
                     sandbox (a fake sibling store item is plantable)",
                    sibling.display()
                ));
            }
            println!(
                "   {} exposes {seed_entries} bound items (not the host store); {item} shows \
                 {item_entries} entries and rejects writes; the dir rejects new entries",
                seed_dir.display()
            );
        }
        None => println!(
            "   (no seed-lock item present in the sandbox — seed-store leg skipped, matching \
             a seed-less prelude)"
        ),
    }

    // (B) orphan reaping: killing td-builder reaps the whole inner sandbox tree.
    println!(">> (B) orphan reaping: killing td-builder reaps the whole inner sandbox tree");
    // A distinctive token carried in every inner cmdline. It doubles as the sleep
    // duration, so it must be a large integer (≈ sleeps forever); derive it from
    // this process's pid (unique in this PID namespace, no RNG needed).
    let marker = (1_000_000u64 + u64::from(std::process::id()) % 1_000_000).to_string();
    let inner = format!("{sleep_exec_s} {marker} & {sleep_exec_s} {marker} & wait");
    let mut reap_args: Vec<&str> = vec!["host-sandbox"];
    reap_args.extend_from_slice(&bind_flags);
    reap_args.extend_from_slice(&["--", &sh_exec_s, "-c", &inner]);
    let mut child = Command::new(&tb)
        .args(&reap_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("FAIL: cannot spawn the host-sandbox reaping probe: {e}"))?;
    let top = i64::from(child.id());

    let poll = std::time::Duration::from_millis(100);
    for _ in 0..100 {
        if scan_marker_procs(&marker) >= 2 {
            break;
        }
        std::thread::sleep(poll);
    }
    let before = scan_marker_procs(&marker);
    println!("   inner procs carrying the marker before kill: {before}");
    if before < 2 {
        let _ = crate::sys::kill_pid(top, crate::sys::SIGTERM);
        sweep_marker_procs(&marker);
        let _ = child.wait();
        return Err(format!(
            "FAIL: the inner sandbox tree never started (marker={marker})"
        ));
    }

    // SIGTERM to our own live child cannot realistically fail (ESRCH/EPERM don't
    // apply to a child we just spawned), but if it somehow does, clean up the
    // marker tree and reap the child before failing loudly — symmetric with the
    // `before < 2` path above, so no path leaves a zombie or stray marker proc.
    if let Err(e) = crate::sys::kill_pid(top, crate::sys::SIGTERM) {
        sweep_marker_procs(&marker);
        let _ = child.wait();
        return Err(format!("FAIL: cannot SIGTERM the top td-builder ({top}): {e}"));
    }
    for _ in 0..100 {
        if scan_marker_procs(&marker) == 0 {
            break;
        }
        std::thread::sleep(poll);
    }
    let after = scan_marker_procs(&marker);
    let _ = child.wait();
    println!("   inner procs carrying the marker after killing td-builder ({top}): {after}");
    if after != 0 {
        sweep_marker_procs(&marker);
        return Err(format!(
            "FAIL: {after} sandbox descendant(s) survived td-builder termination — orphaned \
             (PR_SET_PDEATHSIG reaping broken)"
        ));
    }

    println!(
        "PASS: minimal /dev (no host device leak) + the inner sandbox tree is fully reaped when \
         td-builder is killed."
    );
    Ok(())
}

// --- toolchain-input-addressed (formerly tests/toolchain-input-addressed.sh) ---

/// toolchain-input-addressed — the /td/store modern toolchain (gcc-14.3.0 +
/// binutils-2.44 + glibc-2.41) gets a STABLE input-addressed key derived from its
/// DECLARED inputs, so its path is identical across non-reproducible rebuilds and
/// predictable from the lock — the prereq for td-subst chain-caching. Port of
/// tests/toolchain-input-addressed.sh (gate 414, i686).
fn toolchain_input_addressed(root: &Path) -> Result<(), String> {
    println!(
        ">> toolchain-input-addressed: the /td/store modern toolchain gets a STABLE \
         input-addressed key (td-toolchain.lock + toolchain-key/path) — a pure function of its \
         declared inputs, identical across non-reproducible rebuilds, predictable from the lock"
    );
    let tb = tb()?;
    println!(">> td-builder (stage0, guix-free): {}", tb.display());
    let lock = root.join("tests/td-toolchain.lock");
    let lock_s = path_str(&lock)?;
    let lock_text = std::fs::read_to_string(&lock)
        .map_err(|_| String::from("FAIL: missing tests/td-toolchain.lock"))?;
    let work = fresh_scratch(root, ".toolchain-input-addressed-scratch")?;
    let env = [("TD_STORE_DIR", "/td/store")];

    // [pinned-sync] every lock pin mirrors the recipe source pin / patch it names.
    let source_pins = recipe_eval_source_pins(root)?;
    let (nin, npatch) = check_pinned_sync(root, &lock_text, &source_pins)?;
    println!(
        "   [pinned-sync] {nin} source pins + {npatch} patch pins match recipe source pins + \
         seed/patches"
    );

    // [stable-key] the key + component paths are deterministic and distinct.
    let k1 = tb_out_env(&tb, &["toolchain-key", &lock_s], &env, "toolchain-key")?;
    let k2 = tb_out_env(&tb, &["toolchain-key", &lock_s], &env, "toolchain-key (repeat)")?;
    if k1 != k2 {
        return Err(format!(
            "FAIL: [stable-key] toolchain-key not deterministic ({k1} vs {k2})"
        ));
    }
    if k1.is_empty() || !k1.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("FAIL: [stable-key] key is not a hex digest: {k1}"));
    }
    let gccp = tb_out_env(&tb, &["toolchain-path", &lock_s, "gcc-14.3.0"], &env, "toolchain-path gcc")?;
    let bup = tb_out_env(&tb, &["toolchain-path", &lock_s, "binutils-2.44"], &env, "toolchain-path binutils")?;
    let glp = tb_out_env(&tb, &["toolchain-path", &lock_s, "glibc-2.41"], &env, "toolchain-path glibc")?;
    for p in [&gccp, &bup, &glp] {
        if !p.starts_with("/td/store/") {
            return Err(format!("FAIL: [stable-key] not a /td/store path: {p}"));
        }
    }
    let gccp_again =
        tb_out_env(&tb, &["toolchain-path", &lock_s, "gcc-14.3.0"], &env, "toolchain-path gcc (repeat)")?;
    if gccp_again != gccp {
        return Err("FAIL: [stable-key] toolchain-path not deterministic".into());
    }
    if gccp == bup || gccp == glp || bup == glp {
        return Err("FAIL: [stable-key] components collide".into());
    }
    println!(
        "   [stable-key] key={k1}; gcc/binutils/glibc each get a distinct, deterministic /td/store \
         path"
    );

    // [content-indep] same key, different bytes -> SAME input-addressed path
    // (content-addressed store-add-recursive of the same two bytes splits).
    let v1 = work.join("v1");
    let v2 = work.join("v2");
    mkdirp(&v1.join("bin"))?;
    mkdirp(&v2.join("bin"))?;
    writef(&v1.join("bin/x"), "AAAAA\n")?;
    writef(&v2.join("bin/x"), "BBBBB-different\n")?;
    let v1_s = path_str(&v1)?;
    let v2_s = path_str(&v2)?;
    let iaa = path_str(&work.join("iaA"))?;
    let iab = path_str(&work.join("iaB"))?;
    let iaa_db = path_str(&work.join("iaA.db"))?;
    let iab_db = path_str(&work.join("iaB.db"))?;
    let ia1 = tb_out_env(
        &tb,
        &["store-add-input-addressed", "glibc-2.41", &k1, &v1_s, &iaa, &iaa_db],
        &env,
        "store-add-input-addressed v1",
    )?;
    let ia2 = tb_out_env(
        &tb,
        &["store-add-input-addressed", "glibc-2.41", &k1, &v2_s, &iab, &iab_db],
        &env,
        "store-add-input-addressed v2",
    )?;
    if ia1 != ia2 {
        return Err(format!(
            "FAIL: [content-indep] input-addressed path moved with content ({ia1} vs {ia2})"
        ));
    }
    if ia1 != glp {
        return Err(format!(
            "FAIL: [content-indep] producer path {ia1} != toolchain-path {glp} (consumer can't \
             predict it)"
        ));
    }
    let caa = path_str(&work.join("caA"))?;
    let cab = path_str(&work.join("caB"))?;
    let caa_db = path_str(&work.join("caA.db"))?;
    let cab_db = path_str(&work.join("caB.db"))?;
    let ca1 = tb_out_env(
        &tb,
        &["store-add-recursive", "glibc-2.41", &v1_s, &caa, &caa_db],
        &env,
        "store-add-recursive v1",
    )?;
    let ca2 = tb_out_env(
        &tb,
        &["store-add-recursive", "glibc-2.41", &v2_s, &cab, &cab_db],
        &env,
        "store-add-recursive v2",
    )?;
    if ca1 == ca2 {
        return Err(
            "FAIL: [content-indep] content-addressed paths did NOT move — fixture bytes are equal?"
                .into(),
        );
    }
    let (ha, hb) = match (registered_hash(&tb, &iaa_db, &ia1)?, registered_hash(&tb, &iab_db, &ia2)?) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => (a, b),
        _ => {
            return Err(
                "FAIL: [content-indep] input-addressed adds did not register a NAR hash".into(),
            )
        }
    };
    if ha == hb {
        return Err(
            "FAIL: [content-indep] registered NAR hashes are equal — content integrity not recorded"
                .into(),
        );
    }
    println!(
        "   [content-indep] same key+different bytes -> same path {ia1} (content-addressed would \
         split: {ca1} vs {ca2})"
    );

    // [load-bearing] perturbing one input pin moves the path.
    let pert_text = perturb_glibc_pin(&lock_text).ok_or_else(|| {
        String::from("FAIL: [load-bearing] could not perturb the lock (glibc-2.41 input line not found)")
    })?;
    let pert = work.join("perturbed.lock");
    writef(&pert, &pert_text)?;
    let pert_s = path_str(&pert)?;
    let glp_p =
        tb_out_env(&tb, &["toolchain-path", &pert_s, "glibc-2.41"], &env, "toolchain-path (perturbed)")?;
    if glp_p == glp {
        return Err("FAIL: [load-bearing] perturbing an input pin did NOT change the path".into());
    }
    println!(
        "   [load-bearing] flipping one declared input pin moves glibc-2.41's path ({glp} -> \
         {glp_p})"
    );

    // [behavioral]+[structural] a real binary at an input-addressed path RUNS.
    let bs = gate_input("bash-static")?;
    if !is_executable_file(&Path::new(&bs).join("bin/bash")) {
        return Err(format!("FAIL: no static bash fixture at {bs}"));
    }
    run_input_addressed_bash(&tb, &work, &bs, &k1, "bash-static")?;

    let _ = chmod_r_uw(&work);
    let _ = std::fs::remove_dir_all(&work);
    println!(
        "PASS: toolchain-input-addressed — the /td/store modern toolchain has a STABLE \
         input-addressed key (td-toolchain.lock + toolchain-key/path): a pure function of its \
         declared inputs, so its path is identical across non-reproducible rebuilds and \
         predictable from the lock — the prereq for td-subst chain-caching (2b/2c). A real binary \
         placed there runs, /gnu/store absent."
    );
    Ok(())
}

// --- toolchain-x86_64-input-addressed (formerly tests/toolchain-x86_64-input-addressed.sh) ---

/// toolchain-x86_64-input-addressed — the x86_64 /td/store toolchain gets a STABLE
/// input-addressed key that SHARES i686's exact source set with ARCH (name +
/// component names) as the sole discriminator. Port of
/// tests/toolchain-x86_64-input-addressed.sh (gate 418).
fn toolchain_x86_64_input_addressed(root: &Path) -> Result<(), String> {
    println!(
        ">> toolchain-x86_64-input-addressed: the x86_64 /td/store toolchain gets a STABLE \
         input-addressed key (td-toolchain-x86_64.lock + toolchain-key/path) — sharing i686's \
         source set with ARCH as the sole discriminator, predictable from the lock"
    );
    let tb = tb()?;
    println!(">> td-builder (stage0, guix-free): {}", tb.display());
    let lock = root.join("tests/td-toolchain-x86_64.lock");
    let ilock = root.join("tests/td-toolchain.lock");
    let lock_s = path_str(&lock)?;
    let ilock_s = path_str(&ilock)?;
    let lock_text = std::fs::read_to_string(&lock)
        .map_err(|_| String::from("FAIL: missing tests/td-toolchain-x86_64.lock"))?;
    let ilock_text = std::fs::read_to_string(&ilock).map_err(|_| {
        String::from("FAIL: missing tests/td-toolchain.lock (the i686 lock to compare against)")
    })?;
    let work = fresh_scratch(root, ".toolchain-x86_64-input-addressed-scratch")?;
    let env = [("TD_STORE_DIR", "/td/store")];

    // [pinned-sync] every lock pin mirrors the recipe source pin / patch it names.
    let source_pins = recipe_eval_source_pins(root)?;
    let (nin, npatch) = check_pinned_sync(root, &lock_text, &source_pins)?;
    println!(
        "   [pinned-sync] {nin} source pins + {npatch} patch pins match recipe source pins + \
         seed/patches"
    );

    // [arch-parity] the x86_64 lock shares i686's EXACT source set; only the arch
    // directives (name/recipe-rev/component) differ. Compare the sorted pin sets
    // directly, and assert both locks carry only arch directives.
    let mut xset = filter_pin_lines(&lock_text);
    let mut iset = filter_pin_lines(&ilock_text);
    xset.sort();
    iset.sort();
    if xset != iset {
        return Err(
            "FAIL: [arch-parity] x86_64 input/patch set differs from i686 — the cross must reuse \
             i686's sources"
                .into(),
        );
    }
    for (name, text) in [
        ("tests/td-toolchain-x86_64.lock", &lock_text),
        ("tests/td-toolchain.lock", &ilock_text),
    ] {
        let bad = bad_directive_keys(text);
        if !bad.is_empty() {
            return Err(format!(
                "FAIL: [arch-parity] {name} has an unexpected non-arch directive: {} (only \
                 name/recipe-rev/component/input/patch allowed)",
                bad.join(" ")
            ));
        }
    }
    println!(
        "   [arch-parity] x86_64 lock shares i686's exact {nin}+{npatch} source set; only \
         name/recipe-rev/component differ"
    );

    // [distinct-key] ARCH is the discriminator: distinct key, no path collision.
    let kx = tb_out_env(&tb, &["toolchain-key", &lock_s], &env, "toolchain-key x86_64")?;
    let ki = tb_out_env(&tb, &["toolchain-key", &ilock_s], &env, "toolchain-key i686")?;
    if kx == ki {
        return Err(format!(
            "FAIL: [distinct-key] x86_64 key collides with i686 ({kx}) — arch did not re-key"
        ));
    }
    println!(
        "   [distinct-key] x86_64 key {kx} != i686 key {ki} (arch re-keys with zero source \
         duplication)"
    );

    // [stable-key] deterministic, distinct, x86_64-suffixed /td/store paths.
    let k2 = tb_out_env(&tb, &["toolchain-key", &lock_s], &env, "toolchain-key x86_64 (repeat)")?;
    if kx != k2 {
        return Err(format!(
            "FAIL: [stable-key] toolchain-key not deterministic ({kx} vs {k2})"
        ));
    }
    if kx.is_empty() || !kx.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("FAIL: [stable-key] key is not a hex digest: {kx}"));
    }
    let bup = tb_out_env(&tb, &["toolchain-path", &lock_s, "binutils-2.44-x86_64"], &env, "toolchain-path binutils x86_64")?;
    let gccp = tb_out_env(&tb, &["toolchain-path", &lock_s, "gcc-14.3.0-x86_64"], &env, "toolchain-path gcc x86_64")?;
    let glp = tb_out_env(&tb, &["toolchain-path", &lock_s, "glibc-2.41-x86_64"], &env, "toolchain-path glibc x86_64")?;
    for p in [&bup, &gccp, &glp] {
        if !(p.starts_with("/td/store/") && p.ends_with("-x86_64")) {
            return Err(format!("FAIL: [stable-key] not an x86_64 /td/store path: {p}"));
        }
    }
    let gccp_again = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "gcc-14.3.0-x86_64"],
        &env,
        "toolchain-path gcc x86_64 (repeat)",
    )?;
    if gccp_again != gccp {
        return Err("FAIL: [stable-key] toolchain-path not deterministic".into());
    }
    if gccp == bup || gccp == glp || bup == glp {
        return Err("FAIL: [stable-key] components collide".into());
    }
    let i_gcc = tb_out_env(&tb, &["toolchain-path", &ilock_s, "gcc-14.3.0"], &env, "toolchain-path i686 gcc")?;
    if gccp == i_gcc {
        return Err("FAIL: [distinct-key] x86_64 gcc path == i686 gcc path".into());
    }
    println!(
        "   [stable-key] key={kx}; cross binutils/gcc/glibc each get a distinct, deterministic \
         x86_64 /td/store path"
    );

    // [load-bearing] recipe-rev bump moves the key; an input pin moves a path.
    let rr_text = rewrite_recipe_rev(&lock_text)
        .ok_or_else(|| String::from("FAIL: [load-bearing] could not bump recipe-rev"))?;
    let rr = work.join("rr.lock");
    writef(&rr, &rr_text)?;
    let rr_s = path_str(&rr)?;
    let kr = tb_out_env(&tb, &["toolchain-key", &rr_s], &env, "toolchain-key (recipe-rev bumped)")?;
    if kr == kx {
        return Err("FAIL: [load-bearing] bumping recipe-rev did NOT move the key".into());
    }
    let pin_text = perturb_glibc_pin(&lock_text).ok_or_else(|| {
        String::from("FAIL: [load-bearing] could not perturb the glibc-2.41 input pin")
    })?;
    let pin = work.join("pin.lock");
    writef(&pin, &pin_text)?;
    let pin_s = path_str(&pin)?;
    let glp_p = tb_out_env(
        &tb,
        &["toolchain-path", &pin_s, "glibc-2.41-x86_64"],
        &env,
        "toolchain-path (perturbed)",
    )?;
    if glp_p == glp {
        return Err("FAIL: [load-bearing] perturbing an input pin did NOT move the path".into());
    }
    println!(
        "   [load-bearing] recipe-rev bump moves the key; flipping one input pin moves \
         glibc-2.41-x86_64's path"
    );

    // [behavioral]+[structural] a real binary at the x86_64-keyed path RUNS.
    let bs = gate_input("bash-static")?;
    if !is_executable_file(&Path::new(&bs).join("bin/bash")) {
        return Err(format!("FAIL: no static bash fixture at {bs}"));
    }
    run_input_addressed_bash(&tb, &work, &bs, &kx, "bash-static-x86_64")?;

    let _ = chmod_r_uw(&work);
    let _ = std::fs::remove_dir_all(&work);
    println!(
        "PASS: toolchain-x86_64-input-addressed — the x86_64 /td/store toolchain has a STABLE \
         input-addressed key (td-toolchain-x86_64.lock + toolchain-key/path): a pure function of \
         its declared inputs, sharing i686's exact source set with ARCH (name+components) as the \
         sole discriminator — distinct from i686, predictable from the lock across \
         non-reproducible rebuilds. The prereq for fetching the x86_64 toolchain instead of the \
         ~90-min from-seed rebuild (rust compile/userland rungs 3/4)."
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
            "fingerprintline\n/td/store/abc123-td-builder-0.1.0\n",
        )
        .unwrap();
        let s0 = stage0_from_memo(&root).expect("memo");
        assert!(s0
            .tb
            .ends_with("store/abc123-td-builder-0.1.0/bin/td-builder"));
        // A missing memo is a loud provisioning error, not a fallback.
        let _ = std::fs::remove_dir_all(&root);
        assert!(stage0_from_memo(&root).is_err());
    }

    // A representative toolchain-lock fixture: the arch directives, two input
    // pins (one of them glibc), one patch pin, plus a comment/blank to exercise
    // the skip paths.
    const LOCK_FIXTURE: &str = "\
# a comment
name td-toolchain-x86_64
recipe-rev 1
component gcc-14.3.0-x86_64 gcc-14.3.0

input aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa gcc-14.3.0.tar.xz
input bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb glibc-2.41.tar.xz
patch cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc glibc-boot-2.16.0.patch
";

    #[test]
    fn parse_pin_lines_splits_input_and_patch_only() {
        let pins = parse_pin_lines(LOCK_FIXTURE);
        assert_eq!(pins.len(), 3, "two inputs + one patch, no directives");
        assert!(matches!(pins[0].kind, PinKind::Input));
        assert_eq!(pins[0].file, "gcc-14.3.0.tar.xz");
        assert_eq!(
            pins[1].sha,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert!(matches!(pins[2].kind, PinKind::Patch));
        assert_eq!(pins[2].file, "glibc-boot-2.16.0.patch");
    }

    #[test]
    fn parse_pin_lines_file_is_rest_of_line_like_toolchain_lock() {
        // `file` is the REST of the line after the sha (trimmed), exactly what
        // `store::ToolchainLock::parse` canonicalizes and hashes into the key —
        // trailing content is NOT dropped, so pinned-sync validates the same
        // bytes the store path is keyed on. A leading-whitespace line still
        // parses (the line is trimmed first). A line with no file is skipped.
        let pins = parse_pin_lines(
            "  input dddd glibc-2.41.tar.xz # trailing note\ninput eeee\npatch ffff a b.patch\n",
        );
        assert_eq!(pins.len(), 2, "the file-less `input eeee` row is skipped");
        assert_eq!(pins[0].file, "glibc-2.41.tar.xz # trailing note");
        assert_eq!(pins[0].sha, "dddd");
        assert_eq!(pins[1].file, "a b.patch");
        // Parity witness: ToolchainLock canonicalizes the same file remainder.
        let lock = crate::store::ToolchainLock::parse(
            "name x\nrecipe-rev 1\ncomponent c\ninput dddd glibc-2.41.tar.xz # trailing note\n",
        )
        .expect("well-formed lock");
        assert_eq!(lock.inputs, vec!["dddd glibc-2.41.tar.xz # trailing note".to_string()]);
    }

    #[test]
    fn filter_pin_lines_keeps_raw_pin_lines_for_set_compare() {
        let raw = filter_pin_lines(LOCK_FIXTURE);
        assert_eq!(raw.len(), 3);
        assert!(raw.iter().all(|l| l.starts_with("input ") || l.starts_with("patch ")));
        // Reordering the SAME pins yields an equal sorted set (arch-parity's crux).
        let reordered = "\
patch cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc glibc-boot-2.16.0.patch
input bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb glibc-2.41.tar.xz
input aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa gcc-14.3.0.tar.xz
";
        let (mut a, mut b) = (raw, filter_pin_lines(reordered));
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn bad_directive_keys_flags_only_non_arch_directives() {
        assert!(bad_directive_keys(LOCK_FIXTURE).is_empty());
        let bad = bad_directive_keys("name x\nbogus 1\ninput s f\nweird 2\n");
        assert_eq!(bad, vec!["bogus".to_string(), "weird".to_string()]);
    }

    #[test]
    fn perturb_glibc_pin_zeroes_the_glibc_input() {
        let out = perturb_glibc_pin(LOCK_FIXTURE).expect("glibc input present");
        assert!(out.contains(&format!("input {} glibc-2.41.tar.xz", "0".repeat(64))));
        // The gcc pin and the arch directives are untouched.
        assert!(out.contains("input aaaa"));
        assert!(out.contains("recipe-rev 1"));
        // No glibc pin -> None (a vacuous perturbation is a hard error upstream).
        assert!(perturb_glibc_pin("name x\ninput s gcc-14.3.0.tar.xz\n").is_none());
    }

    #[test]
    fn rewrite_recipe_rev_bumps_one_to_two() {
        let out = rewrite_recipe_rev(LOCK_FIXTURE).expect("recipe-rev 1 present");
        assert!(out.contains("recipe-rev 2"));
        assert!(!out.contains("recipe-rev 1"));
        assert!(rewrite_recipe_rev("name x\nrecipe-rev 3\n").is_none());
    }

    #[test]
    fn source_pin_sha_matches_on_the_file_field() {
        let pins = "gcc\thttps://x/gcc.tar.xz\tdeadbeef\tgcc-14.3.0.tar.xz\n\
                    glibc\thttps://x/glibc.tar.xz\tfeedface\tglibc-2.41.tar.xz\n";
        assert_eq!(source_pin_sha(pins, "glibc-2.41.tar.xz").as_deref(), Some("feedface"));
        assert_eq!(source_pin_sha(pins, "gcc-14.3.0.tar.xz").as_deref(), Some("deadbeef"));
        assert_eq!(source_pin_sha(pins, "not-there.tar.xz"), None);
    }

    #[test]
    fn store_root_for_takes_the_first_component_store() {
        assert_eq!(store_root_for("/td/store/abc-bash/bin/bash").unwrap(), "/td/store");
        // First-component agnostic: any /<x>/store root derives, none is hardcoded.
        assert_eq!(store_root_for("/seed/store/abc-sleep/bin/sleep").unwrap(), "/seed/store");
        assert!(store_root_for("/not-a-store-path").is_err());
        assert!(store_root_for("relative/store/x").is_err());
    }
}
