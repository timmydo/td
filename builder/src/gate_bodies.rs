//! gate_bodies.rs — typed Rust gate bodies (#318 axis 3): the `td-builder
//! gate-body <name>` subcommand that runs every gate, the build-recipes node
//! included. No gate carries shell.
//!
//! The gate runner (`gates.rs::run_gate`) execs `<current_exe> gate-body
//! <name>` in a memory-limited wrapper (the pre_exec setrlimit(RLIMIT_DATA),
//! its own process group, and TD_GATE_SPECS env). `current_exe` is the stage0
//! td-builder in the loop (the prelude execs `<stage0> … gate-run`), so a body
//! gets `tb` = its own binary for free; a body that needs the placement as the
//! old shell gates' `load_stage0` resolved it uses `PlacedStage0`.
//!
//! The registry is `is_native` + the `cli` match below (one place). `load()`
//! refuses a gate def with no registered body, so a typo is a load-time error,
//! never a silent no-op.
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

/// The native gate names. Adding a gate: add its name here + a `cli` arm.
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
    "recipe-checks",
    "store-native-profile",
    "sandbox-hardening",
    "toolchain-input-addressed",
    "toolchain-x86_64-input-addressed",
    "build-recipes",
    "stage0-cold-start",
    "cargo-test",
    "daemon-budget",
    "bootstrap-seed",
    "bootstrap-mes",
    "bootstrap-x86_64-toolchain-store-native",
    "bootstrap-x86_64-native-gcc-store-native",
    "bootstrap-x86_64-self-gcc-store-native",
];

use td_engine::exit::UNPROVISIONED_TAG;

/// A child's own exit 69, passed through as the body's exit without td's
/// provisioning sentinel: gate-run tolerates it only if the child printed that
/// sentinel itself, as it did when the shell gates `exec`'d the child.
const CHILD_EXIT_69_TAG: &str = "CHILD-EXIT-69: ";

/// `td-builder gate-body <name>` — run one native gate body.
pub fn cli(name: &str) -> ExitCode {
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
        "recipe-checks" => recipe_checks(&root),
        "store-native-profile" => store_native_profile(&root),
        "sandbox-hardening" => sandbox_hardening(&root),
        "toolchain-input-addressed" => toolchain_input_addressed(&root),
        "toolchain-x86_64-input-addressed" => toolchain_x86_64_input_addressed(&root),
        "build-recipes" => build_recipes(&root),
        "stage0-cold-start" => stage0_cold_start(&root),
        "cargo-test" => cargo_test(&root),
        "daemon-budget" => daemon_budget(&root),
        "bootstrap-seed" => bootstrap_seed(&root),
        "bootstrap-mes" => bootstrap_mes(&root),
        "bootstrap-x86_64-toolchain-store-native" => recipe_check_gate(
            &root,
            "gcc-x86-64-stage2-test",
            "build the x86_64 cross toolchain recipe graph and assert its output",
        ),
        "bootstrap-x86_64-native-gcc-store-native" => recipe_check_gate(
            &root,
            "gcc-x86-64-native-test",
            "build the native x86_64 gcc recipe graph and assert its output",
        ),
        "bootstrap-x86_64-self-gcc-store-native" => recipe_check_gate(
            &root,
            "gcc-x86-64-self-test",
            "rebuild gcc with the native recipe output and assert self-hosting",
        ),
        other => Err(format!("gate-body: unknown native gate `{other}`")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Match the bash convention: the FAIL line goes to stderr; the
            // runner captures both streams into the gate log. An UNPROVISIONED_TAG
            // error is a toolchain gap, not a regression → exit 69 so gate-run
            // classifies it Unprovisioned (tolerated), never RED.
            if let Some(rest) = e.strip_prefix(UNPROVISIONED_TAG) {
                eprintln!("gate-body {name}: unprovisioned — {rest}");
                td_engine::exit::unprovisioned_exit()
            } else if let Some(rest) = e.strip_prefix(CHILD_EXIT_69_TAG) {
                eprintln!("{rest}");
                ExitCode::from(td_engine::exit::EXIT_UNPROVISIONED as u8)
            } else {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        }
    }
}

// --- shared helpers ---------------------------------------------------------

/// The td-builder under test: this process's own binary. In the loop the runner
/// (`gate-run`) IS the stage0 td-builder, so `current_exe` is the stage0
/// placement — the same binary the old shell gates resolved via `load_stage0`.
fn tb() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve td-builder (current_exe): {e}"))
}

/// Tag a failed child's message as a PROVISIONING gap, so `cli` re-raises it as
/// exit 69 and gate-run tolerates the gate as a skip. Requires exit
/// `EXIT_UNPROVISIONED` AND the sentinel only td's provisioning path prints —
/// the same two-part test gate-run applies, so a bare 69 from an unrelated tool
/// stays RED and no regression can masquerade as a skip. `output` is the child's
/// streams alone, never the formatted message, so `ctx` cannot satisfy it.
fn tag_if_unprovisioned(status: &std::process::ExitStatus, output: &str, msg: String) -> String {
    if td_engine::exit::child_reported_host_gap(status.code(), b"", output.as_bytes()) {
        // Drop the FAIL: lead-in; this is reported as a skip, and a SKIP line
        // reading "unprovisioned — FAIL: ..." is the confusion being removed.
        let body = msg.strip_prefix("FAIL: ").unwrap_or(&msg);
        return format!("{UNPROVISIONED_TAG}{body}");
    }
    msg
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
    let out = crate::spawn::past_a_busy_program(|| cmd.output())
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn td-builder: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let sout = String::from_utf8_lossy(&out.stdout);
        let body = format!("{sout}{err}");
        let msg = format!(
            "FAIL: {ctx}: td-builder {args:?} exited {}\n{body}",
            out.status
        );
        return Err(tag_if_unprovisioned(&out.status, &body, msg));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True if `tb <args...>` exits zero (for the discrimination legs that expect
/// a NON-zero exit — corruption/verify-fail). stdout+stderr discarded.
///
/// The busy retry matters MORE here than where an error is returned: this
/// collapses every failure to `false`, which is the answer those legs are
/// looking for, so a program that could not be exec'd would read as one that
/// ran and refused.
fn tb_ok(tb: &Path, args: &[&str]) -> bool {
    let mut cmd = Command::new(tb);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::spawn::past_a_busy_program(|| cmd.status())
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run an arbitrary tool, returning trimmed stdout on success (the generic
/// subprocess spawn for staging helpers and one-off tool invocations).
fn run_out(program: &str, args: &[&str], ctx: &str) -> Result<String, String> {
    run_out_env(program, args, &[], ctx)
}

/// The `# ` note prefix `td-recipe-eval check-list --reaching` gives the
/// line that says why a check was reached (its `REACH_NOTE`); any other
/// note is a scope miss.
const REACH_NOTE: &str = "reach: ";

/// A `check-list` output split into the evaluator's `# reach: ` notes (why
/// a check was reached), its other `# ` notes (a scope miss: it listed
/// everything) and the check names, one per line or several, in order. The
/// notes ride stdout because `run_out_env` keeps stderr only for a failure.
fn split_check_list(raw: &str) -> CheckList<'_> {
    let mut list = CheckList::default();
    for line in raw.lines() {
        match line.strip_prefix("# ").map(str::trim) {
            Some(note) if note.starts_with(REACH_NOTE) => list.whys.push(note),
            Some(note) => list.misses.push(note),
            None => list.stems.extend(line.split_whitespace()),
        }
    }
    list
}

#[derive(Debug, Default, PartialEq)]
struct CheckList<'a> {
    whys: Vec<&'a str>,
    misses: Vec<&'a str>,
    stems: Vec<&'a str>,
}

/// `run_out`, plus extra env vars set on the child (inheriting the rest of the
/// current environment — never `env -i`, matching the old shell gates' bare
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
    let out = crate::spawn::past_a_busy_program(|| cmd.output())
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn {program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let sout = String::from_utf8_lossy(&out.stdout);
        let body = format!("{sout}{err}");
        let msg = format!(
            "FAIL: {ctx}: {program} {args:?} exited {}\n{body}",
            out.status
        );
        return Err(tag_if_unprovisioned(&out.status, &body, msg));
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
/// the memo PER GATE (as the old shell gates' `load_stage0` did) keeps the daemon's
/// builder-of-record consistent with builder.db even when a concurrent
/// re-provision replaced the placement mid-run (the #309 staleness).
struct Stage0 {
    tb: PathBuf,
}

fn stage0_from_memo(root: &Path) -> Result<Stage0, String> {
    // The old shell gates hardcoded `TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"`
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

    // A static package to run from /td/store: the loop's td-built busybox.
    let bs = busybox_pkg_dir()?;
    let bs_s = path_str(&bs)?;

    // The user's /td/store: place the static package at <store>/<base>.
    let store = work.join("td-store");
    std::fs::create_dir_all(&store).map_err(|e| format!("FAIL: mkdir {}: {e}", store.display()))?;
    let base = base_of(&bs_s);
    cp_a(&bs, &store.join(&base))?;
    chmod_r_uw(&store)?;
    println!(
        "   placed {base} into the td-owned store {}",
        store.display()
    );

    // Run inside the own-root store-ns (rootless): /td/store = store, /gnu/store
    // absent. `$(( ))` proves a LIVE interpreter ran, not a stub echoing bytes.
    let inner = format!(
        "[ -d /td/store ] && echo TDSTORE-OK\n\
         [ -d /td/store/{base}/bin ] && echo PKG-AT-TDSTORE\n\
         [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT\n\
         echo \"RAN:$(( 40 + 2 ))\"\n"
    );
    let store_s = path_str(&store)?;
    let out = tb_out(
        &tb,
        &[
            "store-ns",
            &store_s,
            "--",
            &format!("/td/store/{base}/bin/sh"),
            "-c",
            &inner,
        ],
        "store-ns run",
    )?;
    for l in out.lines() {
        println!("     {l}");
    }

    // Leg A: DURABLE behavioral — the binary ran from /td/store.
    if !out.lines().any(|l| l == "RAN:42") {
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

// --- recipe-checks (formerly tests/recipe-checks.sh) ---------------------------

fn recipe_checks(root: &Path) -> Result<(), String> {
    println!(">> recipe-checks: recipe-owned /td/store package checks");

    let eval = resolve_recipe_eval(root)?;
    let eval_s = path_str(&eval)?;
    let stage0_base = std::env::var("TD_STAGE0_BASE")
        .unwrap_or_else(|_| root.join(".td-build-cache/stage0").display().to_string());
    let envs: [(&str, &str); 2] = [
        ("TD_RECIPE_EVAL", &eval_s),
        ("TD_STAGE0_BASE", &stage0_base),
    ];

    let every = run_out_env(&eval_s, &["check-list"], &envs, "td-recipe-eval check-list")?;
    if every.trim().is_empty() {
        return Err("FAIL: no recipe checks selected".to_string());
    }
    // A confined change runs the checks it can reach. The evaluator decides
    // the reach from its own table of what each recipe embeds, and lists
    // everything when a scope is one nothing reads; the checks left out are
    // NAMED, in the report too, so a scoped run never reads as a full one.
    let scope = crate::check_loop::check_scope(
        std::env::var_os("TD_CHECK_FULL").is_some(),
        std::env::var(crate::check_loop::CHECK_SCOPE_ENV)
            .ok()
            .as_deref(),
    );
    let mut unreached: Vec<String> = Vec::new();
    let checks = match &scope {
        None => every.clone(),
        Some(dirs) => {
            let mut args: Vec<&str> = vec!["check-list", "--reaching"];
            args.extend(dirs.iter().map(String::as_str));
            let raw = run_out_env(
                &eval_s,
                &args,
                &envs,
                "td-recipe-eval check-list --reaching",
            )?;
            let CheckList {
                whys,
                misses,
                stems,
            } = split_check_list(&raw);
            let listed = stems.join(" ");
            unreached = every
                .split_whitespace()
                .filter(|c| !stems.contains(c))
                .map(str::to_string)
                .collect();
            let (reached, total) = (stems.len(), every.split_whitespace().count());
            for why in &whys {
                println!(">> recipe-checks: {why}");
            }
            for note in &misses {
                println!(">> recipe-checks: evaluator: {note}");
            }
            // A full list must not read as a narrowing, and a miss must say
            // it was one: the evaluator's note carries the reason.
            if !misses.is_empty() {
                println!(
                    ">> recipe-checks: the evaluator could not map a change under [{}] to \
                     the checks it reaches and listed every one; running all {total}",
                    dirs.join(" ")
                );
            } else if reached == total {
                println!(
                    ">> recipe-checks: a change under [{}] reaches every one of the {total} \
                     check(s); running all {total}",
                    dirs.join(" ")
                );
            } else {
                println!(
                    ">> recipe-checks: scoped to what a change under [{}] reaches: {reached} of \
                     {total} check(s)",
                    dirs.join(" ")
                );
            }
            if listed.trim().is_empty() {
                println!(
                    "PASS: recipe-checks - no recipe check reaches a change under [{}]; none \
                     of {} run: {} (td-builder check recipe-checks runs them all)",
                    dirs.join(" "),
                    unreached.len(),
                    unreached.join(" ")
                );
                return Ok(());
            }
            listed
        }
    };

    // The whole work list first: every (spec, index) this run will execute. Built
    // before anything runs so the checks can be dispatched CONCURRENTLY — one
    // check does not depend on another, and they were only ever sequential because
    // the ladder they all build against admitted one run at a time.
    let mut work: Vec<(String, usize)> = Vec::new();
    for spec in checks.split_whitespace() {
        let count_text = run_out_env(
            &eval_s,
            &["check-count", spec],
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
            work.push((spec.to_string(), index));
        }
    }

    // Longest first, by what each took when it last executed here: the gate
    // ends when its longest check does, so that check must not start last.
    // Best effort — without a history the list order stands.
    let durations = run_out_env(
        &eval_s,
        &["check-history", "--durations"],
        &envs,
        "td-recipe-eval check-history --durations",
    )
    .map(|text| parse_check_durations(&text))
    .unwrap_or_default();
    let order = longest_first(&work, &durations);
    let width = recipe_check_width(work.len());
    let borrowing = recipe_check_borrowing(width, work.len());
    if borrowing > 0 {
        println!(
            ">> recipe-checks: {} check(s), {width} at a time on the gate's grant and up to \
             {borrowing} more on host memory no check holds, longest first",
            work.len()
        );
    } else {
        println!(
            ">> recipe-checks: {} check(s), {width} at a time, longest first",
            work.len()
        );
    }
    let results = run_recipe_checks_concurrently(
        &work,
        &order,
        width,
        borrowing,
        &eval,
        &eval_s,
        &stage0_base,
    )?;

    // The blocks were printed as each check landed, in completion order. This is
    // the ordered ACCOUNT: it tallies the verdict over the work list, so the
    // skipped names below read in a stable order however the machine was loaded.
    let mut ran = 0usize;
    let mut failures = 0usize;
    let mut memoized = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut deferred: Vec<String> = Vec::new();
    let mut timed: Vec<(String, std::time::Duration)> = Vec::new();
    for ((spec, index), (outcome, took)) in work.iter().zip(results) {
        ran += 1;
        match outcome {
            CheckOutcome::Passed => timed.push((format!("{spec}#{index}"), took)),
            CheckOutcome::Memoized => memoized += 1,
            CheckOutcome::Deferred => deferred.push(format!("{spec}#{index}")),
            CheckOutcome::HostGap => skipped.push(format!("{spec}#{index}")),
            CheckOutcome::Failed => {
                failures += 1;
                timed.push((format!("{spec}#{index}"), took));
            }
        }
    }

    if let Some(line) = slowest_checks(&timed, SLOWEST_SHOWN) {
        println!("{line}");
    }
    let (report, verdict) =
        recipe_checks_verdict(ran, failures, &skipped, memoized, &deferred, &unreached);
    for line in report {
        println!("{line}");
    }
    verdict
}

/// How many of a run's slowest checks the summary names.
const SLOWEST_SHOWN: usize = 8;

/// The `shown` longest of the checks that executed, longest first with their
/// wall time, so the summary says what held the gate up without reading
/// every block, and the sum over all of them. Memo answers, deferrals and
/// host skips are left out by the caller: they say nothing about what a
/// check costs. None when nothing executed.
fn slowest_checks(timed: &[(String, std::time::Duration)], shown: usize) -> Option<String> {
    let mut by_time: Vec<&(String, std::time::Duration)> = timed.iter().collect();
    by_time.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let named: Vec<String> = by_time
        .into_iter()
        .take(shown)
        .map(|(name, took)| format!("{name} {:.1}s", took.as_secs_f64()))
        .collect();
    let total = timed.iter().fold(std::time::Duration::ZERO, |sum, (_, t)| {
        sum.saturating_add(*t)
    });
    (!named.is_empty()).then(|| {
        format!(
            ">> recipe-checks: slowest {} of {} executed: {} (all {} sum {:.1}s; \
             checks overlap, so not wall)",
            named.len(),
            timed.len(),
            named.join(", "),
            timed.len(),
            total.as_secs_f64()
        )
    })
}

/// The gate's verdict, split out so the three endings are testable without
/// running 25 package builds. Returns an optional extra report line plus the
/// verdict itself.
///
/// A skip is NOT a pass. If every check was unrunnable here the gate asserted
/// nothing, so it reports its own unprovisioned skip rather than green — a
/// green that proved nothing is the vacuous gate this contract exists to stop.
/// A PARTIAL skip stays green, because that is the same bargain gate-run
/// already makes suite-wide, but every skipped check is NAMED: a count alone
/// reads like full coverage to anyone scanning the log.
fn recipe_checks_verdict(
    ran: usize,
    failures: usize,
    skipped: &[String],
    memoized: usize,
    deferred: &[String],
    unreached: &[String],
) -> (Vec<String>, Result<(), String>) {
    // The caveat rides the FAIL arm too: "1 of 25 failed" alongside 20 silent
    // skips reads as though 24 checks vouched for the tree.
    let caveat = (!skipped.is_empty()).then(|| {
        format!(
            ">> recipe-checks: {} {} of {ran} check(s) SKIPPED as unprovisioned (nothing on \
             this host can run them), so this run is NOT full coverage: {}",
            crate::check_loop::GATES_SKIPPED_SENTINEL,
            skipped.len(),
            skipped.join(" ")
        )
    });
    // A memoized pass is a pass, but it is said out loud and counted apart:
    // "ran 44 of 44" over 41 answers from the memo would read as 44 checks
    // re-proved today, and "3 of 44 failed" beside 41 of them as 41 re-proved.
    let memo_note = (memoized != 0).then(|| {
        format!(
            ">> recipe-checks: {memoized} of {ran} check(s) answered from the verdict memo — each \
             passed here before with every input it reads unchanged since; TD_CHECK_FULL=1 runs \
             them again"
        )
    });
    // A deferral proved nothing here, so each is named, on the FAIL arm too.
    let deferred_note = (!deferred.is_empty()).then(|| {
        format!(
            ">> recipe-checks: {} of {ran} check(s) deferred to main — each passed here \
             before and only the builder engine or evaluator changed since; main-integration \
             runs them in full, TD_CHECK_FULL=1 runs them here: {}",
            deferred.len(),
            deferred.join(" ")
        )
    });
    // The checks a scope left out are named here, beside the verdict — the
    // top of a long log says only how many it runs — and on the FAIL arm too:
    // "1 of 3 failed" over 41 unreached reads as a suite of three.
    let unreached_note = (!unreached.is_empty()).then(|| {
        format!(
            ">> recipe-checks: {} check(s) not reached by the change and not run: {} \
             (td-builder check recipe-checks runs them all)",
            unreached.len(),
            unreached.join(" ")
        )
    });
    if failures != 0 {
        return (
            caveat
                .into_iter()
                .chain(memo_note)
                .chain(deferred_note)
                .chain(unreached_note)
                .collect(),
            Err(format!(
                "FAIL: recipe-checks - {failures} of {ran} recipe-owned check(s) failed"
            )),
        );
    }
    if ran == 0 {
        return (
            Vec::new(),
            Err("FAIL: recipe-checks - no checks ran".to_string()),
        );
    }
    if skipped.len() == ran {
        return (
            Vec::new(),
            Err(format!(
                "{UNPROVISIONED_TAG}recipe-checks - all {ran} recipe-owned check(s) need a \
                 toolchain no host here reaches: {}",
                skipped.join(" ")
            )),
        );
    }
    // The caveat carries the TOKEN, not just prose: this gate exits 0, so
    // without a stable machine signal automation cannot tell a partial run from
    // a full one — and prose coupling is what broke twice (#268, #315).
    let mut lines: Vec<String> = caveat.into_iter().collect();
    lines.extend(memo_note);
    lines.extend(deferred_note);
    lines.extend(unreached_note);
    let fresh = ran
        .saturating_sub(skipped.len())
        .saturating_sub(memoized)
        .saturating_sub(deferred.len());
    let answered: Vec<String> = [(memoized, "memoized"), (deferred.len(), "deferred to main")]
        .iter()
        .filter(|(n, _)| *n != 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    let memo_note = if answered.is_empty() {
        String::new()
    } else {
        format!(" ({})", answered.join(", "))
    };
    lines.push(format!(
        "PASS: recipe-checks - ran {fresh} of {ran}{memo_note} recipe-owned /td/store check(s) from the Rust recipe catalog; package behavior/repro assertions live with the package recipes."
    ));
    (lines, Ok(()))
}

fn resolve_recipe_eval(root: &Path) -> Result<PathBuf, String> {
    let path = match std::env::var_os("TD_RECIPE_EVAL") {
        Some(value) => PathBuf::from(value),
        // Through the memo, not the raw sentinel: the sentinel records only WHICH
        // binary, never which SOURCE built it, so reading it directly would
        // evaluate with a stale evaluator whenever the tree moved ahead of it.
        None => crate::stage0::recipe_eval_place(root, &root.join(".td-build-cache/recipe-eval"))
            .map(PathBuf::from)
            .map_err(|e| {
                if e.starts_with(UNPROVISIONED_TAG) {
                    e
                } else {
                    format!("FAIL: {e}")
                }
            })?,
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

/// How one recipe-owned check ended. `HostGap` is not a failure: the evaluator
/// reported that nothing on THIS machine can do the work (no toolchain in the
/// jail), which is the same class gate-run tolerates suite-wide. It is a
/// distinct code from a provenance rejection, which stays a failure — see
/// td_engine::exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckOutcome {
    Passed,
    /// Passed here before with every input it reads unchanged since, and said
    /// so with the memo sentinel on stdout instead of running. Counted as a
    /// pass and reported apart, so a run that re-ran nothing cannot read as
    /// one that re-proved everything.
    Memoized,
    /// A branch run's deferral to main: it passed here before and only the
    /// builder engine or the evaluator changed since, so it did not run.
    /// Reported apart from a pass and named, since it re-proved nothing.
    Deferred,
    Failed,
    HostGap,
}

/// One finished check: its verdict plus a bounded output tail, held so the
/// whole block can be printed at once rather than interleaved with its peers'.
///
/// Bounded tails from both streams are captured, where stderr was teed and
/// stdout inherited.
/// That tee existed so a single check's progress kept streaming; with checks
/// running concurrently, streaming is what makes the log unreadable — four
/// builds' lines arrive interleaved with nothing saying which is which. What
/// replaces the liveness is per-CHECK granularity: a block is printed the moment
/// its check lands. The one thing that genuinely regresses is the LONGEST check,
/// which is the critical path and now shows nothing until it finishes.
struct FinishedCheck {
    outcome: CheckOutcome,
    output: Vec<u8>,
    output_truncated: bool,
}

const RECIPE_CHECK_OUTPUT_BYTES: usize = 1024 * 1024;

struct CapturedCheckOutput {
    bytes: Vec<u8>,
    truncated: bool,
    /// Per sentinel asked for, in order: whether the stream carried it.
    saw: Vec<bool>,
}

impl CapturedCheckOutput {
    fn saw(&self, i: usize) -> bool {
        self.saw.get(i).copied().unwrap_or(false)
    }
}

fn capture_check_output(
    mut reader: impl std::io::Read,
    sentinels: &[&[u8]],
    limit: usize,
) -> CapturedCheckOutput {
    let mut tail = std::collections::VecDeque::with_capacity(limit);
    let mut chunk = [0u8; 8192];
    let mut scan_tail = Vec::new();
    let mut truncated = false;
    let mut saw = vec![false; sentinels.len()];
    // What a scan carries over a read boundary: one byte short of the
    // longest needle.
    let carry = sentinels
        .iter()
        .map(|needle| needle.len().saturating_sub(1))
        .max()
        .unwrap_or(0);
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let bytes = chunk.get(..count).unwrap_or(&[]);
        if !sentinels.is_empty() {
            let mut scan = Vec::with_capacity(scan_tail.len().saturating_add(bytes.len()));
            scan.extend_from_slice(&scan_tail);
            scan.extend_from_slice(bytes);
            for (needle, seen) in sentinels.iter().zip(saw.iter_mut()) {
                if !needle.is_empty() && scan.windows(needle.len()).any(|window| window == *needle)
                {
                    *seen = true;
                }
            }
            let keep = carry.min(scan.len());
            scan_tail.clear();
            scan_tail.extend_from_slice(scan.get(scan.len().saturating_sub(keep)..).unwrap_or(&[]));
        }
        let overflow = tail.len().saturating_add(bytes.len()).saturating_sub(limit);
        if overflow > 0 {
            truncated = true;
            for _ in 0..overflow.min(tail.len()) {
                let _ = tail.pop_front();
            }
        }
        let keep_from = bytes.len().saturating_sub(limit);
        tail.extend(bytes.get(keep_from..).unwrap_or(&[]));
    }
    CapturedCheckOutput {
        bytes: tail.into_iter().collect(),
        truncated,
        saw,
    }
}

/// Print one check's block: banner, its captured output verbatim, verdict
/// and wall time. The caller holds the print lock, which is what keeps the
/// three contiguous. The time is what says which checks hold a gate up, and
/// the log kept no other record of it.
fn report_finished_check(
    spec: &str,
    index: usize,
    done: Result<&FinishedCheck, &String>,
    took: std::time::Duration,
) {
    use std::io::Write;
    let took = format!("{:.1}s", took.as_secs_f64());
    println!("================ recipe-check {spec}#{index} ================");
    // stdout first, so the banner cannot land after the body it introduces when
    // the two streams are captured into one file.
    let _ = std::io::stdout().flush();
    match done {
        Ok(done) => {
            if done.output_truncated {
                let _ = std::io::stderr().write_all(
                    b"[td-builder: earlier recipe-check output truncated at the memory-safe tail limit]\n",
                );
            }
            let _ = std::io::stderr().write_all(&done.output);
            let _ = std::io::stderr().flush();
            match done.outcome {
                CheckOutcome::Passed => println!(
                    "================ recipe-check {spec}#{index}: PASS ({took}) ================"
                ),
                CheckOutcome::Memoized => println!(
                    "================ recipe-check {spec}#{index}: PASS (memoized: unchanged \
                     since it last passed here; {took}) ================"
                ),
                CheckOutcome::Deferred => println!(
                    "================ recipe-check {spec}#{index}: DEFERRED to main (only the \
                     builder engine or evaluator changed since a pass here; {took}) \
                     ================"
                ),
                CheckOutcome::HostGap => println!(
                    "================ recipe-check {spec}#{index}: SKIPPED (unprovisioned \
                     — nothing on this host can run it; {took}) ================"
                ),
                CheckOutcome::Failed => eprintln!(
                    "================ recipe-check {spec}#{index}: FAIL ({took}) ================"
                ),
            }
        }
        // A check that could not be RUN at all (spawn/wait failed). The run is
        // about to fail on it; say which one here so the log names it in place.
        Err(e) => eprintln!(
            "================ recipe-check {spec}#{index}: ERROR {e} ({took}) ================"
        ),
    }
    let _ = std::io::stdout().flush();
}

/// Each check is a full recipe build. Allocate a conservative 4 GiB share to
/// each worker from the host-issued grant held by the enclosing gate.
fn recipe_check_width(work: usize) -> usize {
    recipe_check_width_for_budget(crate::check_memory::request_job_budget(), work)
}

/// Roughly what one recipe check's process tree may peak at. Only used to derive
/// a width from the gate's memory budget — deliberately generous, since being
/// wrong low costs concurrency and being wrong high costs the whole gate.
const RECIPE_CHECK_PEAK_BYTES: u64 = 4 * crate::check_memory::GIB;

fn recipe_check_width_for_budget(budget: Option<u64>, work: usize) -> usize {
    let width = budget
        .map(|bytes| bytes / RECIPE_CHECK_PEAK_BYTES)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(1)
        .max(1);
    width.min(work.max(1))
}

/// The tokens one borrowing worker holds while its check runs: one check's
/// peak, as `RECIPE_CHECK_PEAK_BYTES` sizes it.
const RECIPE_CHECK_PEAK_TOKENS: usize = 4;

/// How many workers may run checks on borrowed memory beside the `width` the
/// gate's grant pays for (`check_memory::borrow_permit`). None outside the
/// check host, or when gate-run gave the body no place to publish what it
/// borrows, since its tree watchdog would then kill the gate for holding it.
fn recipe_check_borrowing(width: usize, work: usize) -> usize {
    if std::env::var_os(crate::check_memory::HOST_CHILD_ENV).is_none()
        || std::env::var_os(crate::check_memory::BORROWED_BYTES_FILE_ENV).is_none()
    {
        return 0;
    }
    recipe_check_borrowing_for(width, work, crate::gates::nproc()).min(
        crate::check_memory::borrowable_grants(RECIPE_CHECK_PEAK_TOKENS),
    )
}

/// The CPUs bound the whole width: each check builds with the jobs its 4 GiB
/// pays for, so more checks than that divides into would only queue for CPU.
/// Tokens bound it again as each worker borrows.
fn recipe_check_borrowing_for(width: usize, work: usize, cpus: usize) -> usize {
    let jobs = crate::check_memory::jobs_for_budget(RECIPE_CHECK_PEAK_BYTES, cpus).max(1);
    (cpus / jobs).max(width).min(work).saturating_sub(width)
}

/// `CHECK<TAB>SECS` lines from `td-recipe-eval check-history --durations`;
/// a line that does not parse is left out.
fn parse_check_durations(text: &str) -> std::collections::BTreeMap<String, f64> {
    text.lines()
        .filter_map(|line| {
            let (check, secs) = line.split_once('\t')?;
            Some((check.to_string(), secs.trim().parse::<f64>().ok()?))
        })
        .collect()
}

/// The positions of `work` in the order to start them: longest recorded
/// first, and a check with no record before every recorded one, since what
/// it costs is unknown. Ties keep the list order.
fn longest_first(
    work: &[(String, usize)],
    durations: &std::collections::BTreeMap<String, f64>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..work.len()).collect();
    let secs = |i: usize| {
        work.get(i)
            .and_then(|(spec, index)| durations.get(&format!("{spec}#{index}")))
            .copied()
            .unwrap_or(f64::INFINITY)
    };
    order.sort_by(|a, b| secs(*b).total_cmp(&secs(*a)));
    order
}

/// What this body holds beyond its grant, published to the file gate-run's
/// watchdog reads before a borrowed check starts and after it has ended, so
/// the published figure never trails the memory the checks may use.
struct BorrowLedger {
    file: Option<PathBuf>,
    held: std::sync::Mutex<u64>,
}

impl BorrowLedger {
    fn from_env() -> Self {
        BorrowLedger {
            file: std::env::var_os(crate::check_memory::BORROWED_BYTES_FILE_ENV).map(PathBuf::from),
            held: std::sync::Mutex::new(0),
        }
    }

    /// Add `delta` (or take it away) and publish the new total. Replaced
    /// whole, through a rename, so the watchdog never reads half a number.
    fn adjust(&self, delta: u64, add: bool) -> Result<(), String> {
        let file = self
            .file
            .as_ref()
            .ok_or("no borrowed-bytes file to publish to")?;
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let next = if add {
            held.saturating_add(delta)
        } else {
            held.saturating_sub(delta)
        };
        let tmp = file.with_extension("borrowed.tmp");
        std::fs::write(&tmp, next.to_string())
            .and_then(|()| std::fs::rename(&tmp, file))
            .map_err(|e| format!("publish borrowed bytes to {}: {e}", file.display()))?;
        *held = next;
        Ok(())
    }
}

/// One borrowed check's tokens, published while held: dropping it
/// unpublishes them before the permit releases them, on every way out of
/// the worker, an unwinding panic included.
struct Borrowed<'a> {
    ledger: &'a BorrowLedger,
    bytes: u64,
    _permit: crate::check_memory::MemoryPermit,
}

impl Borrowed<'_> {
    fn publish(
        ledger: &BorrowLedger,
        permit: crate::check_memory::MemoryPermit,
    ) -> Result<Borrowed<'_>, String> {
        let bytes = permit.bytes();
        ledger.adjust(bytes, true)?;
        Ok(Borrowed {
            ledger,
            bytes,
            _permit: permit,
        })
    }
}

impl Drop for Borrowed<'_> {
    fn drop(&mut self) {
        // A failed write leaves the budget looser, never tighter.
        let _ = self.ledger.adjust(self.bytes, false);
    }
}

/// Run the work list `width`-at-a-time on the gate's grant, plus up to
/// `borrowing` more on borrowed memory, starting them in `order` and
/// returning one result per item IN WORK ORDER.
///
/// Threads take from a shared cursor rather than being handed a fixed slice: the
/// checks are wildly uneven (a Rust toolchain build against a `hello` package), so
/// a static split would leave workers idle behind one long straggler.
fn run_recipe_checks_concurrently(
    work: &[(String, usize)],
    order: &[usize],
    width: usize,
    borrowing: usize,
    eval: &Path,
    eval_s: &str,
    stage0_base: &str,
) -> Result<Vec<(CheckOutcome, std::time::Duration)>, String> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut sorted = order.to_vec();
    sorted.sort_unstable();
    if !sorted.iter().copied().eq(0..work.len()) {
        return Err(format!(
            "FAIL: recipe-checks' start order is not one start per check ({} for {})",
            order.len(),
            work.len()
        ));
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    type Slot = Option<Result<(CheckOutcome, std::time::Duration), String>>;
    let slots: Vec<std::sync::Mutex<Slot>> =
        work.iter().map(|_| std::sync::Mutex::new(None)).collect();
    // Serializes the REPORTING, not the work: a block is printed whole while this
    // is held, so four builds' output cannot interleave line by line.
    let pen = std::sync::Mutex::new(());
    let worker_budget = crate::check_memory::request_job_budget()
        .unwrap_or(RECIPE_CHECK_PEAK_BYTES)
        / u64::try_from(width).unwrap_or(1).max(1);
    let ledger = BorrowLedger::from_env();
    let borrowed_runs = std::sync::atomic::AtomicUsize::new(0);

    // One check, start to slot: the step every worker repeats.
    let run_one = |slot: usize, budget: u64| {
        let Some((spec, index)) = work.get(slot) else {
            return;
        };
        let started = std::time::Instant::now();
        let done = run_recipe_check(eval, spec, *index, eval_s, stage0_base, budget);
        let took = started.elapsed();
        // Printed HERE, as each check lands, rather than after the whole
        // set: a gate that shows nothing for ten minutes is one nobody can
        // tell from a hung one, and the run this replaced streamed. The
        // ORDER is completion order and so varies with load; the ordered
        // account is the summary the caller builds from `slots`.
        // A POISONED pen is still taken (`into_inner`): poisoning means
        // some worker panicked mid-report, and dropping every later
        // check's block on account of it would turn one panic into a gate
        // whose log is silently missing most of its output. td-builder
        // UNWINDS — `panic = "abort"` is another crate's setting — so this
        // is reachable rather than theoretical.
        {
            let _held = pen.lock().unwrap_or_else(|e| e.into_inner());
            report_finished_check(spec, *index, done.as_ref(), took);
        }
        // Only the VERDICT and its wall time are kept. The output has
        // been printed, and 26 checks' captured logs held to the end of
        // the gate is exactly the memory the slot pool's admission is
        // trying to bound — a noisy failure can emit a great deal of it.
        let verdict = done.map(|d| (d.outcome, took));
        if let Some(cell) = slots.get(slot) {
            if let Ok(mut g) = cell.lock() {
                *g = Some(verdict);
            }
        }
    };

    std::thread::scope(|scope| {
        for _worker in 0..width {
            let (next, run_one) = (&next, &run_one);
            scope.spawn(move || loop {
                let Some(&slot) = order.get(next.fetch_add(1, Relaxed)) else {
                    return;
                };
                run_one(slot, worker_budget);
            });
        }
        // A borrowing worker takes a check only once it holds tokens for it,
        // and publishes them before the check starts. It gives up when the
        // list is spoken for, which also ends a wait for tokens.
        for _worker in 0..borrowing {
            let (next, run_one, ledger, borrowed_runs, pen) =
                (&next, &run_one, &ledger, &borrowed_runs, &pen);
            scope.spawn(move || loop {
                let exhausted = || next.load(Relaxed) >= order.len();
                if exhausted() {
                    return;
                }
                // A refusal other than the list running out is said once, so
                // a host that never lends reads as that and not as idleness.
                let borrowed =
                    crate::check_memory::borrow_permit(RECIPE_CHECK_PEAK_TOKENS, &exhausted)
                        .and_then(|permit| Borrowed::publish(ledger, permit));
                let borrowed = match borrowed {
                    Ok(borrowed) => borrowed,
                    Err(e) => {
                        if !exhausted() {
                            let _held = pen.lock().unwrap_or_else(|e| e.into_inner());
                            println!(">> recipe-checks: a borrowing worker stopped: {e}");
                        }
                        return;
                    }
                };
                let Some(&slot) = order.get(next.fetch_add(1, Relaxed)) else {
                    return;
                };
                borrowed_runs.fetch_add(1, Relaxed);
                // Unpublished by the drop, once the check's processes are gone.
                run_one(slot, borrowed.bytes);
                drop(borrowed);
            });
        }
    });
    let borrowed_runs = borrowed_runs.load(Relaxed);
    if borrowed_runs > 0 {
        println!(
            ">> recipe-checks: {borrowed_runs} of {} check(s) ran on borrowed host memory",
            work.len()
        );
    }

    let mut out = Vec::with_capacity(work.len());
    for (i, cell) in slots.iter().enumerate() {
        let taken = cell.lock().unwrap_or_else(|e| e.into_inner()).take();
        match taken {
            Some(Ok(done)) => out.push(done),
            Some(Err(e)) => return Err(e),
            // A hole here would be a check whose verdict silently vanished, and
            // the tally below would then report full coverage over fewer checks.
            // `thread::scope` re-raises a panicking worker's panic when it joins,
            // so this should be unreachable — it is spelled as an error anyway
            // because "should be" is what the verdict must not rest on.
            None => {
                let (spec, index) = work
                    .get(i)
                    .map(|(s, n)| (s.as_str(), *n))
                    .unwrap_or(("?", 0));
                return Err(format!(
                    "FAIL: recipe-check {spec}#{index} produced no result"
                ));
            }
        }
    }
    // The verdict is computed by zipping this against the work list, and a short
    // result list would silently shrink `ran` — "ran 20 of 20" over a suite that
    // lost six checks, green, with nothing saying so. Cheap to assert, and the
    // exact failure `recipe_checks_verdict`'s own contract exists to prevent.
    if out.len() != work.len() {
        return Err(format!(
            "FAIL: recipe-checks ran {} check(s) but collected {} result(s)",
            work.len(),
            out.len()
        ));
    }
    Ok(out)
}

fn run_recipe_check(
    eval: &Path,
    spec: &str,
    index: usize,
    eval_s: &str,
    stage0_base: &str,
    job_budget_bytes: u64,
) -> Result<FinishedCheck, String> {
    use std::io::BufReader;
    let index_s = index.to_string();
    let mut command = Command::new(eval);
    command
        .arg("check-run")
        .arg(spec)
        .arg(&index_s)
        .env("TD_RECIPE_EVAL", eval_s)
        .env("TD_RECIPE_CHECK_SPEC", spec)
        .env("TD_RECIPE_CHECK_INDEX", &index_s)
        .env("TD_STAGE0_BASE", stage0_base)
        .env(
            crate::check_memory::JOB_BUDGET_ENV,
            job_budget_bytes.to_string(),
        )
        .env(
            "CARGO_BUILD_JOBS",
            crate::check_memory::jobs_for_budget(job_budget_bytes, crate::gates::nproc())
                .to_string(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::spawn::past_a_busy_program(|| command.spawn())
        .map_err(|e| format!("FAIL: cannot spawn td-recipe-eval check-run {spec}: {e}"))?;
    // CAPTURED as bounded tails, not forwarded: checks run concurrently, so
    // writing straight to the shared streams would interleave builds' output
    // line by line and leave no block attributable to its check.
    //
    // BOTH streams. A check prints its `STEP`/`PASS` progress on stdout, which was
    // inherited here — so it went straight to the gate's stdout, past the print
    // lock, and landed inside some other check's block. Drained on a THREAD rather
    // than after the stderr loop, because a child that fills the pipe nobody is
    // reading blocks forever: reading them in sequence is a deadlock, not a delay.
    //
    // `Builder::spawn` (fallible) rather than `thread::spawn` (which PANICS when
    // the OS cannot create the thread), the reason check_runner.rs states for the
    // same call: a panic here would break the crate's no-panic rule AND unwind
    // past a live child that `Drop` neither kills nor waits for. `EAGAIN` from
    // clone is reachable at width 4 under exactly the memory pressure this change
    // creates. Failing to spawn costs this check's stdout, nothing more.
    // stdout is scanned for the MEMO and DEFERRED sentinels as stderr is for
    // the host-gap one: a check that answered from its verdict memo, or that
    // a branch run left to main, says so there.
    let stdout_reader = child.stdout.take().and_then(|out| {
        std::thread::Builder::new()
            .name("recipe-check-stdout".to_string())
            .spawn(move || {
                capture_check_output(
                    out,
                    &[
                        td_engine::exit::CHECK_MEMO_SENTINEL.as_bytes(),
                        td_engine::exit::CHECK_DEFERRED_SENTINEL.as_bytes(),
                    ],
                    RECIPE_CHECK_OUTPUT_BYTES,
                )
            })
            .ok()
    });
    let mut stderr = child.stderr.take().map_or(
        CapturedCheckOutput {
            bytes: Vec::new(),
            truncated: false,
            saw: Vec::new(),
        },
        |err| {
            capture_check_output(
                BufReader::new(err),
                &[crate::check_loop::UNPROVISIONED_SENTINEL.as_bytes()],
                RECIPE_CHECK_OUTPUT_BYTES,
            )
        },
    );
    // Joined BEFORE the wait, so the child is never reaped with its stdout pipe
    // still filling. The two bounded tails are concatenated rather than
    // interleaved in time order.
    let (mut memoized, mut deferred) = (false, false);
    if let Some(handle) = stdout_reader {
        if let Ok(mut stdout) = handle.join() {
            memoized = stdout.saw(0);
            deferred = stdout.saw(1);
            stderr.bytes.append(&mut stdout.bytes);
            stderr.truncated |= stdout.truncated;
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("FAIL: wait check-run {spec}: {e}"))?;
    // The memo and deferral sentinels count only on a SUCCESS: a check that
    // printed one and then failed, failed.
    let outcome = if status.success() {
        if memoized {
            CheckOutcome::Memoized
        } else if deferred {
            CheckOutcome::Deferred
        } else {
            CheckOutcome::Passed
        }
    } else if td_engine::exit::host_gap_from_parts(status.code(), stderr.saw(0)) {
        CheckOutcome::HostGap
    } else {
        CheckOutcome::Failed
    };
    Ok(FinishedCheck {
        outcome,
        output: stderr.bytes,
        output_truncated: stderr.truncated,
    })
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
    // An ABSENT toolchain is a PROVISIONING gap, not a recipes-crate regression:
    // `ProvisionErr::tagged` carries the UNPROVISIONED_TAG so `cli` exits 69
    // (Unprovisioned/tolerated). In the host-tool-free loop sandbox no toolchain
    // is reachable, so this leg degrades to a skip and the host `cargo-test`
    // preflight carries the recipes crate's tests + clippy. A RESOLVED-but-broken
    // toolchain stays untagged → FAILURE → RED (not silenced as a skip).
    let rustpath = crate::stage0::provision_rust(&penv).map_err(|e| e.tagged())?;
    let ccpath = crate::stage0::provision_cc(&penv).map_err(|e| e.tagged())?;
    let cargo_bin = find_in_path_frags(&rustpath, "cargo")
        .ok_or_else(|| format!("FAIL: no cargo in provision-rust output ({rustpath})"))?;
    // Pin the compiler and the HOST build-script linker to the provisioned
    // toolchain (Codex P2): the rustc the build actually uses, and the gcc that
    // links the host build script (`cc` may be absent by that name under a guix
    // profile), so no inherited RUSTC / CARGO_TARGET_<host>_LINKER substitutes a
    // different one.
    let rustc_bin = find_in_path_frags(&rustpath, "rustc")
        .ok_or_else(|| format!("FAIL: no rustc in provision-rust output ({rustpath})"))?;
    let cc_bin = find_in_path_frags(&ccpath, "cc")
        .or_else(|| find_in_path_frags(&ccpath, "gcc"))
        .ok_or_else(|| format!("FAIL: no cc/gcc in provision-cc output ({ccpath})"))?;
    let rustc_s = path_str(&rustc_bin)?;
    let cc_s = path_str(&cc_bin)?;
    // The host build scripts / proc-macros link with the provisioned cc; the
    // selected target links statically with its provisioned configuration.
    let host_triple =
        crate::stage0::rustc_host_triple(&rustc_bin).map_err(|e| format!("FAIL: {e}"))?;
    let host_linker_var = crate::stage0::target_linker_var(&host_triple);

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
    // Every control-plane helper uses this same static target configuration.
    // Encoded flags outrank Cargo wrappers; explicit --target leaves host-kind
    // build scripts dynamic. Clear inherited wrappers and pin their C linker.
    let encoded_rustflags = crate::stage0::control_plane_flags(&penv, &cc_bin)
        .map_err(|error| format!("FAIL: {error}"))?;
    let target = crate::stage0::control_plane_target(&penv);
    let envs: [(&str, &str); 8] = [
        ("PATH", &new_path),
        ("CARGO_ENCODED_RUSTFLAGS", &encoded_rustflags),
        ("CARGO_HOME", &cargo_home_s),
        ("CARGO_TARGET_DIR", &cargo_target_s),
        ("RUSTC", &rustc_s),
        ("RUSTC_WRAPPER", ""),
        ("RUSTC_WORKSPACE_WRAPPER", ""),
        (&host_linker_var, &cc_s),
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
        &[
            "test",
            "--frozen",
            "--target",
            target,
            "--manifest-path",
            "recipes/Cargo.toml",
        ],
        &envs,
        "cargo test recipes",
    )?;
    run_out_env(
        &cargo_bin_s,
        &[
            "build",
            "--release",
            "--frozen",
            "--target",
            target,
            "--manifest-path",
            "recipes/Cargo.toml",
        ],
        &envs,
        "cargo build recipes",
    )?;

    let eval = cargo_target.join(target).join("release/td-recipe-eval");
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

    let mut smoke = Command::new(&eval);
    smoke
        .args(["build-run", "not-a-recipe"])
        .stdin(Stdio::null());
    let bad_build = crate::spawn::past_a_busy_program(|| smoke.output())
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
    std::env::split_paths(&path)
        .find(|dir| {
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

/// The loop's td-built busybox package dir — the guix-free static shell already
/// on the gate's PATH (the same one `sandbox_hardening` resolves), the store-ns
/// gates' runnable static fixture now that the guix bash-static lock is retired.
/// `which_path` keeps the bound /td/store path (an applet symlink dispatches on
/// argv[0], so we must NOT canonicalize it to `busybox`); its parent's parent is
/// the package dir. `store_root_for` rejects a host `/bin/sh` — loud, not silent.
fn busybox_pkg_dir() -> Result<PathBuf, String> {
    let sh_bound = which_path("sh")
        .ok_or_else(|| String::from("FAIL: no busybox `sh` on PATH (loop userland)"))?;
    let sh_bound_s = path_str(&sh_bound)?;
    let bs = sh_bound
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("FAIL: no package dir above {sh_bound_s}"))?
        .to_path_buf();
    store_root_for(&path_str(&bs)?)?;
    if !is_executable_file(&bs.join("bin/sh")) {
        return Err(format!("FAIL: no static busybox `sh` at {}", bs.display()));
    }
    Ok(bs)
}

/// `cmdline` bytes with NULs read as SPACES, or `None` for a zombie — whose
/// cmdline is empty, so a killed-but-unreaped process is never counted, which
/// is what lets the reaping check poll for zero without racing the wait.
fn cook_cmdline(mut bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    for b in bytes.iter_mut() {
        if *b == 0 {
            *b = b' ';
        }
    }
    Some(bytes)
}

/// Every live process as (pid, cooked cmdline). One place that knows `/proc`
/// holds numeric directories; the four scans below would each carry a copy.
fn proc_cmdlines() -> Vec<(i64, Vec<u8>)> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let pid = name.to_str()?.parse::<i64>().ok()?;
            cook_cmdline(std::fs::read(e.path().join("cmdline")).ok()?).map(|c| (pid, c))
        })
        .collect()
}

/// True if `cmdline` carries `marker`. Compared as BYTES: a byte-as-`char`
/// decode is Latin-1, under which a non-ASCII path never equals the `want`
/// built from a Rust string — a silent permanent zero.
fn has_marker(cmdline: &[u8], marker: &str) -> bool {
    // `windows(0)` panics, and an empty marker would match every process.
    !marker.is_empty()
        && cmdline
            .windows(marker.len())
            .any(|w| w == marker.as_bytes())
}

/// Count the processes whose cmdline carries `marker`.
fn scan_marker_procs(marker: &str) -> usize {
    proc_cmdlines()
        .iter()
        .filter(|(_, c)| has_marker(c, marker))
        .count()
}

/// Count the LEAF `sleep <marker>` processes, matched on the WHOLE argv rather
/// than on a substring of it.
///
/// The marker is in the argv of the top `td-builder host-sandbox`, of the
/// PID-namespace parent that waits on PID 1, and of the inner `sh` — all three
/// carry the command string that names it. A readiness test counting anything
/// that merely CONTAINS the marker is therefore satisfied before `sh` has
/// forked either sleep. These two are the tree's leaves: when both exist, it
/// is up.
fn scan_sleep_procs(sleep_exec: &str, marker: &str) -> usize {
    // The trailing space is load-bearing: cmdline NUL-TERMINATES every
    // argument, so the cooked form ends with one.
    let want = format!("{sleep_exec} {marker} ");
    proc_cmdlines()
        .iter()
        .filter(|(_, c)| c.as_slice() == want.as_bytes())
        .count()
}

/// Every marker-bearing cmdline, `pid: argv`. A COUNT cannot tell an orphan
/// from a teardown still in flight, and reporting one as the other is what
/// sent this check's reader to the wrong subsystem.
fn marker_cmdlines(marker: &str) -> Vec<String> {
    let mut out: Vec<String> = proc_cmdlines()
        .into_iter()
        .filter(|(_, c)| has_marker(c, marker))
        .map(|(pid, c)| format!("{pid}: {}", String::from_utf8_lossy(&c).trim_end()))
        .collect();
    out.sort();
    out
}

/// SIGKILL every process whose cmdline still carries `marker` — the failure-path
/// sweep so a red reaping check never leaks a marker process into the shared PID
/// namespace.
fn sweep_marker_procs(marker: &str) {
    for (pid, c) in proc_cmdlines() {
        if has_marker(&c, marker) {
            let _ = crate::sys::kill_recorded(
                crate::sys::KillTarget::Pid(pid),
                crate::sys::SIGKILL,
                &format!(
                    "its command line carries reaping-check marker {marker} after a red leg \
                     (sandbox-reaping gate sweep)"
                ),
            );
        }
    }
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
        let is_glibc_input =
            line.split_whitespace().next() == Some("input") && line.ends_with(" glibc-2.41.tar.xz");
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
/// evaluator from `$TD_RECIPE_EVAL` when set, else through `recipe_eval_place`,
/// the same memo the ladder gates use. The pin PARSING + comparison is typed Rust.
fn recipe_eval_source_pins(root: &Path) -> Result<String, String> {
    let eval = match std::env::var_os("TD_RECIPE_EVAL") {
        // Set to a non-empty value: use it VERBATIM — no fallback. A non-executable
        // override then fails loudly at the `-x` check below (the shell's `[ -x ] ||
        // fail`), rather than being silently masked by a freshly built evaluator.
        // `${TD_RECIPE_EVAL:-}` treats unset and empty identically, so an empty value
        // falls through to the build path.
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            // Memoized on the recipes source: a warm tree reuses the prelude's
            // evaluator with no toolchain, so this no longer dies in the sandbox
            // on a build it cannot do. The tag survives so a genuine MISS with no
            // toolchain is still the tolerated skip, not a red.
            let base = root.join(".td-build-cache/recipe-eval");
            crate::stage0::recipe_eval_place(root, &base)
                .map(PathBuf::from)
                .map_err(|e| {
                    if e.starts_with(UNPROVISIONED_TAG) {
                        e
                    } else {
                        format!("FAIL: {e}")
                    }
                })?
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
/// the busybox fixture at the arch-keyed input-addressed /td/store path and run
/// it in the store-ns own-root with /gnu/store ABSENT. `name_stem` is the
/// input-addressed name (`busybox-static` / `busybox-static-x86_64`).
fn run_input_addressed_shell(
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
        &[
            "store-add-input-addressed",
            name_stem,
            key,
            bs,
            &store_s,
            &db_s,
        ],
        &[("TD_STORE_DIR", "/td/store")],
        &format!("store-add-input-addressed {name_stem}"),
    )?;
    let suffix = format!("-{name_stem}");
    if !(runp.starts_with("/td/store/") && runp.ends_with(&suffix)) {
        return Err(format!(
            "FAIL: {name_stem} not input-addressed at /td/store: {runp}"
        ));
    }
    if !is_executable_file(&store.join(base_of(&runp)).join("bin/sh")) {
        return Err(format!("FAIL: interned {name_stem} missing physically"));
    }
    let run_bin = format!("{runp}/bin/sh");
    let out = tb_out(
        tb,
        &[
            "store-ns",
            &store_s,
            "--",
            &run_bin,
            "-c",
            "[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT; echo \"RAN:$(( 40 + 2 ))\"",
        ],
        "store-ns run from the input-addressed path",
    )?;
    for l in out.lines() {
        println!("     {l}");
    }
    if !out.lines().any(|l| l == "RAN:42") {
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

    // A real multi-entry static package: the loop's td-built busybox.
    let bs = busybox_pkg_dir()?;
    let bs_s = path_str(&bs)?;

    // Intern it at the LOGICAL /td/store; bytes land physically under `store`.
    let store = work.join("td-store");
    mkdirp(&store)?;
    let store_s = path_str(&store)?;
    let db_s = path_str(&work.join("db.sqlite"))?;
    let pkg = tb_out_env(
        &tb,
        &[
            "store-add-recursive",
            "busybox-x86-64",
            &bs_s,
            &store_s,
            &db_s,
        ],
        &[("TD_STORE_DIR", "/td/store")],
        "store-add-recursive busybox-x86-64",
    )?;
    if !(pkg.starts_with("/td/store/") && pkg.ends_with("-busybox-x86-64")) {
        return Err(format!(
            "FAIL: busybox not content-addressed at /td/store: {pkg}"
        ));
    }
    let physpkg = store.join(base_of(&pkg));
    let physpkg_s = path_str(&physpkg)?;
    if !is_executable_file(&physpkg.join("bin/busybox")) {
        return Err(format!(
            "FAIL: interned busybox missing physically at {}",
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

    // [structural] the profile entries are LOGICAL /td/store symlinks. busybox
    // provides `sh` (an applet symlink) alongside the `busybox` multiplexer;
    // check both retarget logically.
    for t in ["sh", "busybox"] {
        let link = prof.join("bin").join(t);
        let tgt =
            std::fs::read_link(&link).map_err(|_| format!("FAIL: no profile entry for {t}"))?;
        let tgt_s = tgt.to_string_lossy();
        let want = format!("-busybox-x86-64/bin/{t}");
        if !(tgt_s.starts_with("/td/store/") && tgt_s.ends_with(&want)) {
            return Err(format!(
                "FAIL: profile/bin/{t} is not a logical /td/store link (got: {tgt_s})"
            ));
        }
    }
    println!("   [structural] profile entries (sh, busybox) are logical /td/store symlinks");

    // Run the profiled tools in the own-root via a probe FILE bound in the store
    // (no nested quoting between the outer capture and the inner script).
    // `$(( ))` proves a LIVE interpreter ran, not a stub echoing bytes.
    let probe = store.join("probe.sh");
    writef(
        &probe,
        "export PATH=/td/store/profile/bin\n\
         [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT\n\
         case \"$(command -v sh)\" in /td/store/profile/bin/sh) echo SH-VIA-PROFILE ;; esac\n\
         case \"$(command -v busybox)\" in /td/store/profile/bin/busybox) echo BUSYBOX-VIA-PROFILE ;; esac\n\
         sh -c 'echo \"SH-RAN:$(( 40 + 2 ))\"'\n",
    )?;
    let out = tb_out(
        &tb,
        &[
            "store-ns",
            &store_s,
            "--",
            "/td/store/profile/bin/sh",
            "/td/store/probe.sh",
        ],
        "store-ns profile run",
    )?;
    for l in out.lines() {
        println!("     {l}");
    }

    let has = |s: &str| out.lines().any(|l| l == s);
    if !has("SH-VIA-PROFILE") {
        return Err("FAIL: sh did not resolve via /td/store/profile/bin".into());
    }
    if !has("BUSYBOX-VIA-PROFILE") {
        return Err("FAIL: busybox did not resolve via /td/store/profile/bin".into());
    }
    if !has("SH-RAN:42") {
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
    println!(
        "   [structural] /gnu/store is ABSENT in the own-root (unmixed from the guix install)"
    );

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
fn sandbox_hardening(_root: &Path) -> Result<(), String> {
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
    let sleep_canon = which_canon("sleep").ok_or_else(|| String::from("FAIL: no sleep on PATH"))?;
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
    println!(">> (A) minimal /dev: standard nodes present, host kmsg/kvm/disks/mem/input absent");
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
    println!("   {sroot} exposes {entries} bound items; the sh package rejects writes");
    // (B) orphan reaping: killing td-builder reaps the whole inner sandbox tree.
    println!(">> (B) orphan reaping: killing td-builder reaps the whole inner sandbox tree");
    // A distinctive token carried in every inner cmdline. It doubles as the sleep
    // duration, so it must be a large integer (≈ sleeps forever); derive it from
    // this process's pid (unique in this PID namespace, no RNG needed).
    let marker = (1_000_000u64 + u64::from(std::process::id()) % 1_000_000).to_string();
    let inner = format!("{sleep_exec_s} {marker} & {sleep_exec_s} {marker} & wait");
    // Start from a clean slate. The marker is derived from a pid, and pids are
    // small and RECYCLED inside the loop sandbox, so a previous run of this
    // gate can have leaked a process carrying THIS run's marker — which makes
    // the leaf count wrong in both directions. Sweeping is the answer rather
    // than relaxing the readiness test to `>=`: taking the first two arrivals
    // is exactly the weakness that had it killing mid-construction.
    // Checked, NOT swept: the marker is matched as a substring, so an unrelated
    // process whose cmdline merely contains those digits would be SIGKILLed by
    // a sweep here. Naming it and refusing costs a red on a dirty slate; the
    // sweep costs somebody else's process, and only the failure paths — where
    // the tree is known to be ours — may pay that.
    let poll = std::time::Duration::from_millis(100);
    let stale = scan_marker_procs(&marker);
    if stale != 0 {
        for line in marker_cmdlines(&marker) {
            println!("   pre-existing: {line}");
        }
        return Err(format!(
            "FAIL: {stale} process(es) already carry marker={marker} — a reaping \
             measurement taken from a dirty slate would mean nothing"
        ));
    }
    let mut reap_args: Vec<&str> = vec!["host-sandbox"];
    reap_args.extend_from_slice(&bind_flags);
    reap_args.extend_from_slice(&["--", &sh_exec_s, "-c", &inner]);
    let mut probe = Command::new(&tb);
    probe
        .args(&reap_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = crate::spawn::past_a_busy_program(|| probe.spawn())
        .map_err(|e| format!("FAIL: cannot spawn the host-sandbox reaping probe: {e}"))?;
    let top = i64::from(child.id());

    // The tree is UP when both leaf sleeps exist. Four processes above them
    // carry the marker in their own argv — the top td-builder, the PID-ns
    // parent that waits on PID 1, PID 1 itself (a fork of that parent that
    // stays behind as init), and `sh` — so a count of marker-BEARING
    // processes reaches 2 before `sh` has forked either leaf, and killing
    // there tests the sandbox mid-construction rather than once it is up.
    const LEAVES: usize = 2;
    for _ in 0..100 {
        if scan_sleep_procs(&sleep_exec_s, &marker) == LEAVES {
            break;
        }
        std::thread::sleep(poll);
    }
    let leaves = scan_sleep_procs(&sleep_exec_s, &marker);
    let before = scan_marker_procs(&marker);
    println!("   inner procs carrying the marker before kill: {before} ({leaves} leaf sleeps)");
    if leaves != LEAVES {
        // Name what IS up and what was looked for: a leaf count stuck at zero
        // is equally "nothing started" and "the matcher does not match", and
        // the argv it wants is the only way to tell those apart.
        println!("   wanted leaf argv: {sleep_exec_s} {marker} ");
        for line in marker_cmdlines(&marker) {
            println!("   up: {line}");
        }
        let _ = crate::sys::kill_recorded(
            crate::sys::KillTarget::Pid(top),
            crate::sys::SIGTERM,
            "the inner sandbox tree never started; ending the top td-builder \
             (sandbox-reaping gate)",
        );
        sweep_marker_procs(&marker);
        let _ = child.wait();
        return Err(format!(
            "FAIL: the inner sandbox tree never started — {leaves} of {LEAVES} \
             leaf sleeps after 10s (marker={marker})"
        ));
    }

    // SIGTERM to our own live child cannot realistically fail (ESRCH/EPERM don't
    // apply to a child we just spawned), but if it somehow does, clean up the
    // marker tree and reap the child before failing loudly — symmetric with the
    // `before < 2` path above, so no path leaves a zombie or stray marker proc.
    if let Err(e) = crate::sys::kill_recorded(
        crate::sys::KillTarget::Pid(top),
        crate::sys::SIGTERM,
        "proving the PR_SET_PDEATHSIG cascade reaps the sandbox tree on a soft kill \
         (sandbox-reaping gate, leg B)",
    ) {
        sweep_marker_procs(&marker);
        let _ = child.wait();
        return Err(format!(
            "FAIL: cannot SIGTERM the top td-builder ({top}): {e}"
        ));
    }
    // Reap the TOP before judging its descendants, and judge nothing at all
    // while it is still up: a live top CARRIES the marker, so it would be
    // counted among the survivors it is the reason for.
    let mut top_exited = false;
    for _ in 0..100 {
        // Ok(Some(_)) reaps it, so no further wait is owed on this path.
        if matches!(child.try_wait(), Ok(Some(_))) {
            top_exited = true;
            break;
        }
        std::thread::sleep(poll);
    }
    if !top_exited {
        for line in marker_cmdlines(&marker) {
            println!("   still up: {line}");
        }
        // `wait` has no timeout: a td-builder that ignored SIGTERM would hang
        // here for the gate's whole budget instead of failing it in ten
        // seconds, and this diagnostic would never print.
        let _ = crate::sys::kill_child_recorded(
            &mut child,
            "the top td-builder was still alive 10s after SIGTERM (sandbox-reaping gate, \
             leg B)",
        );
        let _ = child.wait();
        sweep_marker_procs(&marker);
        return Err(format!(
            "FAIL: the top td-builder ({top}) was still alive 10s after SIGTERM — \
             descendant reaping cannot be judged from here"
        ));
    }
    for _ in 0..100 {
        if scan_marker_procs(&marker) == 0 {
            break;
        }
        std::thread::sleep(poll);
    }
    // ONE scan for both the count and the names: two would let the number and
    // the list disagree, which is the diagnosis this reports.
    let survivors = marker_cmdlines(&marker);
    let after = survivors.len();
    println!("   inner procs carrying the marker after killing td-builder ({top}): {after}");
    if after != 0 {
        for line in &survivors {
            println!("   surviving: {line}");
        }
        sweep_marker_procs(&marker);
        return Err(format!(
            "FAIL: {after} sandbox descendant(s) survived td-builder termination"
        ));
    }

    // (D) the CONSTRUCTION window, which leg B deliberately no longer reaches.
    //
    // Probabilistic in ONE direction: whether a cycle lands in the window is up
    // to the scheduler, so a miss proves nothing and cycles are repeated. A
    // healthy sandbox reaps whatever the timing, so none can red for having
    // sampled badly. Cycles are NOT independent trials — measured detection
    // clusters — so this SAMPLES the race rather than proving its absence.
    println!(">> (D) construction-window reaping: killing td-builder before its tree is up");
    const CYCLES: usize = 40;
    // Load-bearing rather than a taste: the marker packs the cycle into two
    // decimal digits, so at 100 the (pid, i) mapping stops being injective and
    // a sibling run one pid away collides outright. It is also what keeps every
    // marker the same LENGTH, which is what makes one impossible to find inside
    // another under `has_marker`'s substring match.
    const CYCLE_DIGITS: u64 = 100;
    const _: () = assert!(CYCLES as u64 <= CYCLE_DIGITS);
    // Leg B's 100ms tick is wrong here: a SIGTERM'd top becomes reapable in a
    // few ms, and at one coarse tick per cycle that is paid CYCLES times over
    // for nothing. Same 10s ceiling, finer grain.
    let fine = std::time::Duration::from_millis(1);
    const FINE_TICKS: usize = 10_000;
    for i in 0..CYCLES {
        let marker = ((2_000_000u64 + u64::from(std::process::id()) % 1_000_000) * CYCLE_DIGITS
            + i as u64)
            .to_string();
        let stale = scan_marker_procs(&marker);
        if stale != 0 {
            // Named, as leg B names its own: this is the path an UNRELATED
            // process carrying those digits trips, so the cmdline is the only
            // way to tell that from a real leak.
            for line in marker_cmdlines(&marker) {
                println!("   pre-existing: {line}");
            }
            return Err(format!(
                "FAIL: {stale} process(es) already carry marker={marker} before cycle {i}"
            ));
        }
        let inner = format!("{sleep_exec_s} {marker} & {sleep_exec_s} {marker} & wait");
        let mut args: Vec<&str> = vec!["host-sandbox"];
        args.extend_from_slice(&bind_flags);
        args.extend_from_slice(&["--", &sh_exec_s, "-c", &inner]);
        let mut probe = Command::new(&tb);
        probe
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = crate::spawn::past_a_busy_program(|| probe.spawn())
            .map_err(|e| format!("FAIL: cannot spawn construction-window probe {i}: {e}"))?;
        let top = i64::from(child.id());
        // The EARLIEST moment anything of the tree exists beyond the top
        // itself: two marker-bearing processes is the top plus the child
        // `Command::spawn` forked, which is the one that unshares and then
        // forks PID 1. Killing here is what B stopped doing.
        let mut at_kill = 0;
        for _ in 0..2_000 {
            at_kill = scan_marker_procs(&marker);
            if at_kill >= 2 {
                break;
            }
            std::thread::sleep(fine);
        }
        if at_kill < 2 {
            // NOT a pass. Killing a tree that never began finds no survivors
            // and reports success — this leg answering a question it never
            // asked, which is the one way it can go quietly useless.
            let never_started = format!(
                "cycle {i} never saw the sandbox start constructing (sandbox-reaping \
                 gate, leg D)"
            );
            let _ = crate::sys::kill_recorded(
                crate::sys::KillTarget::Pid(top),
                crate::sys::SIGTERM,
                &never_started,
            );
            let _ = crate::sys::kill_child_recorded(&mut child, &never_started);
            let _ = child.wait();
            sweep_marker_procs(&marker);
            return Err(format!(
                "FAIL: cycle {i} never saw the sandbox start constructing — {at_kill} \
                 marker-bearing process(es) after 2s (marker={marker})"
            ));
        }
        if let Err(e) = crate::sys::kill_recorded(
            crate::sys::KillTarget::Pid(top),
            crate::sys::SIGTERM,
            &format!(
                "proving the PR_SET_PDEATHSIG cascade reaps a sandbox mid-construction, \
                 cycle {i} (sandbox-reaping gate, leg D)"
            ),
        ) {
            sweep_marker_procs(&marker);
            // Timed, for the reason leg B's `!top_exited` path is: an untimed
            // wait on a child still somehow running hangs the gate for its
            // whole budget rather than failing it here.
            let _ = crate::sys::kill_child_recorded(
                &mut child,
                &format!("SIGTERM to the top td-builder failed in cycle {i}: {e}"),
            );
            let _ = child.wait();
            return Err(format!(
                "FAIL: cannot SIGTERM the top td-builder ({top}) in cycle {i}: {e}"
            ));
        }
        let mut top_exited = false;
        for _ in 0..FINE_TICKS {
            if matches!(child.try_wait(), Ok(Some(_))) {
                top_exited = true;
                break;
            }
            std::thread::sleep(fine);
        }
        if !top_exited {
            for line in marker_cmdlines(&marker) {
                println!("   still up: {line}");
            }
            let _ = crate::sys::kill_child_recorded(
                &mut child,
                &format!(
                    "the top td-builder was still alive 10s after SIGTERM in cycle {i} \
                     (sandbox-reaping gate, leg D)"
                ),
            );
            let _ = child.wait();
            sweep_marker_procs(&marker);
            return Err(format!(
                "FAIL: the top td-builder ({top}) was still alive 10s after SIGTERM in cycle {i}"
            ));
        }
        let mut drained = false;
        for _ in 0..FINE_TICKS {
            if scan_marker_procs(&marker) == 0 {
                drained = true;
                break;
            }
            std::thread::sleep(fine);
        }
        let survivors = marker_cmdlines(&marker);
        if !survivors.is_empty() {
            for line in &survivors {
                println!("   surviving: {line}");
            }
            sweep_marker_procs(&marker);
            return Err(format!(
                "FAIL: {} descendant(s) survived a kill during construction (cycle {i}, \
                 marker={marker}) — a sandbox killed before PID 1 confirmed its parent",
                survivors.len()
            ));
        }
        if !drained {
            // The drain ran out and the last scan happened to read zero. Not a
            // failure — the tree IS gone — but ten seconds to reap it is not
            // the healthy shape either, and nothing else would say so.
            println!("   note: cycle {i} took the full drain budget to reach zero");
        }
        // What was actually up when the kill landed. Without it a run where the
        // scan never catches the tree early is byte-identical to one that
        // samples the window every time — the leg going quietly useless, which
        // is the failure this whole leg exists to prevent for the sandbox.
        if at_kill > 2 {
            println!(
                "   note: cycle {i} killed at {at_kill} marker procs, past the earliest point"
            );
        }
    }
    println!("   {CYCLES} construction-window kills, no survivors");

    println!(
        "PASS: minimal /dev (no host device leak) + the inner sandbox tree is fully reaped when \
         td-builder is killed, whether it is up or still being built."
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
    let k2 = tb_out_env(
        &tb,
        &["toolchain-key", &lock_s],
        &env,
        "toolchain-key (repeat)",
    )?;
    if k1 != k2 {
        return Err(format!(
            "FAIL: [stable-key] toolchain-key not deterministic ({k1} vs {k2})"
        ));
    }
    if k1.is_empty() || !k1.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("FAIL: [stable-key] key is not a hex digest: {k1}"));
    }
    let gccp = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "gcc-14.3.0"],
        &env,
        "toolchain-path gcc",
    )?;
    let bup = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "binutils-2.44"],
        &env,
        "toolchain-path binutils",
    )?;
    let glp = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "glibc-2.41"],
        &env,
        "toolchain-path glibc",
    )?;
    for p in [&gccp, &bup, &glp] {
        if !p.starts_with("/td/store/") {
            return Err(format!("FAIL: [stable-key] not a /td/store path: {p}"));
        }
    }
    let gccp_again = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "gcc-14.3.0"],
        &env,
        "toolchain-path gcc (repeat)",
    )?;
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
        &[
            "store-add-input-addressed",
            "glibc-2.41",
            &k1,
            &v1_s,
            &iaa,
            &iaa_db,
        ],
        &env,
        "store-add-input-addressed v1",
    )?;
    let ia2 = tb_out_env(
        &tb,
        &[
            "store-add-input-addressed",
            "glibc-2.41",
            &k1,
            &v2_s,
            &iab,
            &iab_db,
        ],
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
    let (ha, hb) = match (
        registered_hash(&tb, &iaa_db, &ia1)?,
        registered_hash(&tb, &iab_db, &ia2)?,
    ) {
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
        String::from(
            "FAIL: [load-bearing] could not perturb the lock (glibc-2.41 input line not found)",
        )
    })?;
    let pert = work.join("perturbed.lock");
    writef(&pert, &pert_text)?;
    let pert_s = path_str(&pert)?;
    let glp_p = tb_out_env(
        &tb,
        &["toolchain-path", &pert_s, "glibc-2.41"],
        &env,
        "toolchain-path (perturbed)",
    )?;
    if glp_p == glp {
        return Err("FAIL: [load-bearing] perturbing an input pin did NOT change the path".into());
    }
    println!(
        "   [load-bearing] flipping one declared input pin moves glibc-2.41's path ({glp} -> \
         {glp_p})"
    );

    // [behavioral]+[structural] a real binary at an input-addressed path RUNS.
    let bs = busybox_pkg_dir()?;
    let bs_s = path_str(&bs)?;
    run_input_addressed_shell(&tb, &work, &bs_s, &k1, "busybox-static")?;

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
    let kx = tb_out_env(
        &tb,
        &["toolchain-key", &lock_s],
        &env,
        "toolchain-key x86_64",
    )?;
    let ki = tb_out_env(
        &tb,
        &["toolchain-key", &ilock_s],
        &env,
        "toolchain-key i686",
    )?;
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
    let k2 = tb_out_env(
        &tb,
        &["toolchain-key", &lock_s],
        &env,
        "toolchain-key x86_64 (repeat)",
    )?;
    if kx != k2 {
        return Err(format!(
            "FAIL: [stable-key] toolchain-key not deterministic ({kx} vs {k2})"
        ));
    }
    if kx.is_empty() || !kx.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("FAIL: [stable-key] key is not a hex digest: {kx}"));
    }
    let bup = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "binutils-2.44-x86_64"],
        &env,
        "toolchain-path binutils x86_64",
    )?;
    let gccp = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "gcc-14.3.0-x86_64"],
        &env,
        "toolchain-path gcc x86_64",
    )?;
    let glp = tb_out_env(
        &tb,
        &["toolchain-path", &lock_s, "glibc-2.41-x86_64"],
        &env,
        "toolchain-path glibc x86_64",
    )?;
    for p in [&bup, &gccp, &glp] {
        if !(p.starts_with("/td/store/") && p.ends_with("-x86_64")) {
            return Err(format!(
                "FAIL: [stable-key] not an x86_64 /td/store path: {p}"
            ));
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
    let i_gcc = tb_out_env(
        &tb,
        &["toolchain-path", &ilock_s, "gcc-14.3.0"],
        &env,
        "toolchain-path i686 gcc",
    )?;
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
    let kr = tb_out_env(
        &tb,
        &["toolchain-key", &rr_s],
        &env,
        "toolchain-key (recipe-rev bumped)",
    )?;
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
    let bs = busybox_pkg_dir()?;
    let bs_s = path_str(&bs)?;
    run_input_addressed_shell(&tb, &work, &bs_s, &kx, "busybox-static-x86_64")?;

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

// --- the former shell gates and the build-recipes prelude ----------------------

/// The stage0 td-builder placed as the shell gates' `load_stage0` placed it,
/// with the builder-of-record triple its children read.
struct PlacedStage0 {
    tb: PathBuf,
    path: String,
    store: String,
    db: String,
}

impl PlacedStage0 {
    fn place(root: &Path) -> Result<Self, String> {
        let base = root.join(".td-build-cache/stage0");
        let path = crate::stage0::stage0_place(root, &base).map_err(|e| {
            if e.starts_with(UNPROVISIONED_TAG) {
                e
            } else {
                format!("FAIL: td-builder stage0-place could not place a stage0 td-builder: {e}")
            }
        })?;
        let tb = base
            .join("store")
            .join(base_of(&path))
            .join("bin/td-builder");
        if !is_executable_file(&tb) {
            return Err(format!(
                "FAIL: stage0 td-builder not executable at {}",
                tb.display()
            ));
        }
        Ok(Self {
            tb,
            path,
            store: path_str(&base.join("store"))?,
            db: path_str(&base.join("builder.db"))?,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.tb);
        command
            .env("TD_BUILDER_PATH", &self.path)
            .env("TD_BUILDER_STORE", &self.store)
            .env("TD_BUILDER_DB", &self.db);
        command
    }
}

/// Run `command` on the gate's own streams, as the shell gates' `exec` did. A
/// child's exit 69 is the body's exit 69, which stays a tolerated skip only
/// where the child printed td's provisioning sentinel, because gate-run asks
/// for both.
fn run_streamed(mut command: Command, ctx: &str) -> Result<(), String> {
    command.stdin(Stdio::null());
    let status = crate::spawn::past_a_busy_program(|| command.status())
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn: {e}"))?;
    if status.success() {
        return Ok(());
    }
    let msg = format!("FAIL: {ctx}: exited {status}");
    if status.code() == Some(td_engine::exit::EXIT_UNPROVISIONED) {
        return Err(format!("{CHILD_EXIT_69_TAG}{msg}"));
    }
    Err(msg)
}

/// td-recipe-eval placed from the current recipes source, as the shell gates'
/// `recipe-eval-place` did.
fn placed_recipe_eval(root: &Path) -> Result<String, String> {
    crate::stage0::recipe_eval_place(root, &root.join(".td-build-cache/recipe-eval")).map_err(|e| {
        if e.starts_with(UNPROVISIONED_TAG) {
            e
        } else {
            format!("ERROR: could not build td's Rust recipe evaluator (recipes/ crate): {e}")
        }
    })
}

/// The build phase's prelude: place the stage0 td-builder and td-recipe-eval,
/// which the build_gate store primitives reuse. There is no corpus to pre-build,
/// so a spec reaching it is an error.
fn build_recipes(root: &Path) -> Result<(), String> {
    println!(
        ">> build-recipes: the build_gate PRELUDE — stage0 td-builder (env rust) + \
         td-recipe-eval, GUIX-FREE"
    );
    let specs = std::env::var("TD_BUILD_SPECS").map_err(|_| {
        String::from("FAIL: the gate runner passes TD_BUILD_SPECS (empty = prelude only)")
    })?;
    let s0 = PlacedStage0::place(root)?;
    println!(
        ">> builds run on the td-bootstrapped stage0 td-builder ({}) — compiled from source \
         with the environment's rust",
        s0.path
    );
    let eval = placed_recipe_eval(root)?;
    println!(">> recipes EVALUATE with td's OWN Rust td-recipe-eval ({eval})");
    if specs.split_whitespace().next().is_some() {
        return Err(format!(
            "ERROR: build-recipes got specs ({specs}) but the guix-seeded corpus retired — no \
             spec-carrying gate should remain"
        ));
    }
    println!(
        "PASS: build-recipes — guix-free prelude: stage0 td-builder placed (env rust) + \
         td-recipe-eval built; the store primitives build their subjects in-gate."
    );
    Ok(())
}

/// `td-builder bootstrap-recipe <which>` on the placed stage0, with an
/// evaluator placed from the current source whatever TD_RECIPE_EVAL names.
fn bootstrap_recipe(root: &Path, which: &str) -> Result<(), String> {
    let s0 = PlacedStage0::place(root)?;
    let eval = placed_recipe_eval(root)?;
    let mut command = s0.command();
    command
        .args(["bootstrap-recipe", which])
        .env("TD_RECIPE_EVAL", &eval);
    run_streamed(command, &format!("td-builder bootstrap-recipe {which}"))
}

fn bootstrap_seed(root: &Path) -> Result<(), String> {
    println!(
        ">> bootstrap-seed: the structured Rust seed recipe builds the first stage0 artifacts \
         with guix off env — self-reproducing, working, reproducible (source-bootstrap brick 0)"
    );
    bootstrap_recipe(root, "seed")
}

fn bootstrap_mes(root: &Path) -> Result<(), String> {
    println!(
        ">> bootstrap-mes: the structured Rust mes recipe builds GNU Mes (mes-m2) and proves it \
         evaluates Scheme, guix-free + reproducible (source-bootstrap brick 2)"
    );
    bootstrap_recipe(root, "mes")
}

/// A gate that is only the runner's entry point for one recipe check, on an
/// executable TD_RECIPE_EVAL or else an evaluator placed from the source.
fn recipe_check_gate(root: &Path, spec: &str, what: &str) -> Result<(), String> {
    println!(">> recipe-check {spec}: {what}");
    let eval = match std::env::var("TD_RECIPE_EVAL") {
        Ok(named) if !named.is_empty() && is_executable_file(Path::new(&named)) => named,
        _ => placed_recipe_eval(root)?,
    };
    let mut command = Command::new(&eval);
    command
        .args(["check-run", spec, "1"])
        .env("TD_RECIPE_EVAL", &eval);
    run_streamed(command, &format!("td-recipe-eval check-run {spec} 1"))
}

/// The daemon under test and its scratch, stopped and removed however the
/// gate ends.
struct BudgetDaemon {
    child: std::process::Child,
    scratch: PathBuf,
}

impl Drop for BudgetDaemon {
    fn drop(&mut self) {
        // A SHUTDOWN request ends a healthy daemon; give it a moment so the kill
        // (and its audit record) is only for one that did not stop.
        for _ in 0..25 {
            if !matches!(self.child.try_wait(), Ok(None)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        if !matches!(self.child.try_wait(), Ok(None)) {
            let _ = std::fs::remove_dir_all(&self.scratch);
            return;
        }
        let _ = crate::sys::kill_child_recorded(
            &mut self.child,
            "daemon-budget: stop the daemon under test",
        );
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

fn daemon_budget(root: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    println!(
        ">> daemon-budget: the shared build daemon caps concurrent workers across independent \
         submitters"
    );
    let s0 = PlacedStage0::place(root)?;
    let scratch = fresh_scratch(root, ".daemon-budget-scratch")?;
    let d = scratch.join("d");
    mkdirp(&d)?;
    let sock = scratch.join("sock");
    let sock_s = path_str(&sock)?;
    let log = scratch.join("daemon.log");
    let log_s = path_str(&log)?;
    let budget = "2";
    let out = std::fs::File::create(&log).map_err(|e| format!("FAIL: create {log_s}: {e}"))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("FAIL: clone {log_s}: {e}"))?;
    let mut command = s0.command();
    command
        .arg("daemon")
        .arg(&sock)
        .arg(scratch.join("unused-store-db"))
        .arg(&d)
        .env("TD_DAEMON_TEST_BUDGET", budget)
        .env("TD_DAEMON_TEST_SLEEP_MS", "400")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    let child = crate::spawn::past_a_busy_program(|| command.spawn())
        .map_err(|e| format!("FAIL: cannot spawn the daemon: {e}"))?;
    let daemon = BudgetDaemon {
        child,
        scratch: scratch.clone(),
    };
    let daemon_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let is_socket = || {
        std::fs::symlink_metadata(&sock)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
    };
    for _ in 0..50 {
        if is_socket() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !is_socket() {
        return Err(format!(
            "FAIL: daemon socket never appeared\n{}",
            daemon_log()
        ));
    }
    let mut probes = Vec::new();
    for i in 1..=6 {
        let mut probe = s0.command();
        probe
            .args(["daemon-budget-probe", &sock_s, &i.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(child) = crate::spawn::past_a_busy_program(|| probe.spawn()) {
            probes.push(child);
        }
    }
    for mut probe in probes {
        let _ = probe.wait();
    }
    let stats = tb_out(
        &s0.tb,
        &["daemon-budget-check", &log_s, budget],
        "daemon-budget-check",
    )
    .map_err(|_| {
        format!(
            "FAIL: daemon did not honor its test worker budget {budget}\n{}",
            daemon_log()
        )
    })?;
    println!("  [DURABLE behavioral] {stats} — the cap holds across submitters");
    let _ = tb_ok(&s0.tb, &["daemon-request", &sock_s, "SHUTDOWN"]);
    drop(daemon);
    println!(
        "PASS: daemon-budget — the shared build daemon caps concurrent workers at {budget} across \
         independent submitters."
    );
    Ok(())
}

/// Split one `gate-crates cargo-cmds` line into argv. The lines hold bare words
/// and single-quoted words only (`shell_quote` refuses a path with a quote);
/// anything else a shell would interpret is refused rather than guessed at.
fn gate_command_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    for c in line.chars() {
        if quoted {
            if c == '\'' {
                quoted = false;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' => {
                quoted = true;
                started = true;
            }
            ' ' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '"' | '\\' | '$' | '`' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '*' | '?' | '{'
            | '}' | '!' | '[' | '#' | '~' | '\t' | '\n' => {
                return Err(format!(
                    "FAIL: cargo command `{line}` needs a shell (`{c}`)"
                ));
            }
            other => {
                word.push(other);
                started = true;
            }
        }
    }
    if quoted {
        return Err(format!("FAIL: cargo command `{line}` has an open quote"));
    }
    if started {
        words.push(word);
    }
    if words.is_empty() {
        return Err(String::from("FAIL: an empty cargo command"));
    }
    Ok(words)
}

/// Run `words` with stdout and stderr merged into one pipe, copied both to the
/// gate log and to `log`, as `2>&1 | tee` did.
fn run_teed(
    words: &[String],
    envs: &[(&str, &str)],
    log: &mut std::fs::File,
) -> Result<(), String> {
    use std::io::{Read, Write};
    let (program, args) = words
        .split_first()
        .ok_or_else(|| String::from("FAIL: an empty cargo command"))?;
    let (mut reader, writer) =
        std::io::pipe().map_err(|e| format!("FAIL: pipe for {program}: {e}"))?;
    let writer_err = writer
        .try_clone()
        .map_err(|e| format!("FAIL: pipe for {program}: {e}"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer_err);
    for (k, v) in envs {
        command.env(k, v);
    }
    let mut child = crate::spawn::past_a_busy_program(|| command.spawn())
        .map_err(|e| format!("FAIL: cannot spawn {program}: {e}"))?;
    // The parent's copies of the write end go with `command`, so the read
    // below ends when the child and its descendants close theirs.
    drop(command);
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdout = std::io::stdout();
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("FAIL: reading {program}'s output: {e}")),
        };
        let chunk = buf.get(..n).unwrap_or_default();
        let _ = stdout.write_all(chunk);
        log.write_all(chunk)
            .map_err(|e| format!("FAIL: writing the cargo-test log: {e}"))?;
    }
    let status = child
        .wait()
        .map_err(|e| format!("FAIL: waiting on {program}: {e}"))?;
    if !status.success() {
        return Err(format!("FAIL: `{}` exited {status}", words.join(" ")));
    }
    Ok(())
}

fn cargo_test(root: &Path) -> Result<(), String> {
    println!(
        ">> cargo-test: engine crates lint clean (cargo clippy: no panic surface, .get over \
         indexing, unsafe confined) + td-builder unit tests (cargo test) — offline, guix-free \
         toolchain (td-builder provision-{{rust,cc}})"
    );
    let td = tb()?;
    let mut locks = Command::new(&td);
    locks.args(["gate-crates", "locks"]);
    run_streamed(locks, "td-builder gate-crates locks")?;
    let rustpath = tb_out(&td, &["provision-rust"], "provision-rust")?;
    let ccpath = tb_out(&td, &["provision-cc"], "provision-cc")?;
    let scratch = fresh_scratch(root, ".cargo-test-scratch")?;
    let home = scratch.join("home");
    let target = scratch.join("target");
    mkdirp(&home)?;
    mkdirp(&target)?;
    let log_path = scratch.join("out.log");
    let mut log = std::fs::File::create(&log_path)
        .map_err(|e| format!("FAIL: create {}: {e}", log_path.display()))?;
    let cmds = crate::affected::gate_cargo_cmds(root).map_err(|e| format!("FAIL: {e}"))?;
    let names = crate::affected::gate_crate_names(root)
        .map_err(|e| format!("FAIL: {e}"))?
        .join(", ");
    let path = match std::env::var("PATH") {
        Ok(p) if !p.is_empty() => format!("{rustpath}:{ccpath}:{p}"),
        _ => format!("{rustpath}:{ccpath}"),
    };
    let home_s = path_str(&home)?;
    let target_s = path_str(&target)?;
    let envs = [
        ("PATH", path.as_str()),
        ("CARGO_HOME", home_s.as_str()),
        ("CARGO_TARGET_DIR", target_s.as_str()),
    ];
    for line in &cmds {
        run_teed(&gate_command_words(line)?, &envs, &mut log)?;
    }
    drop(log);
    let bytes =
        std::fs::read(&log_path).map_err(|e| format!("FAIL: read {}: {e}", log_path.display()))?;
    if !crate::cargo_test_reported_nonzero_tests(&String::from_utf8_lossy(&bytes)) {
        return Err("ERROR: cargo test reported no passing tests (vacuous run?)".into());
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: cargo-test — the engine workspace (builder + recipes + engine) and {names} \
         satisfy their named dependency policies and lint clean; their unit tests pass \
         (guix-free toolchain)."
    );
    Ok(())
}

/// `td-builder stage0-place-without-guix EMPTY DEST`: in a private user and
/// mount namespace of its own, hide /var/guix behind the empty EMPTY, then place
/// a stage0 into DEST. The mount dies with the namespace, so the verb cannot
/// hide a host's /var/guix. A setup failure is exit 9, which the gate reports
/// as such.
pub fn stage0_place_without_guix(args: &[String]) -> ExitCode {
    let (Some(empty), Some(dest), None) = (args.get(2), args.get(3), args.get(4)) else {
        eprintln!("usage: td-builder stage0-place-without-guix EMPTY DEST");
        return ExitCode::from(2);
    };
    let empty = empty.as_str();
    let hide = || -> Result<(), String> {
        crate::enter_private_userns()?;
        std::fs::create_dir_all("/var/guix").map_err(|e| format!("mkdir /var/guix: {e}"))?;
        let src = std::ffi::CString::new(empty).map_err(|e| format!("{empty}: {e}"))?;
        let dest_c = std::ffi::CString::new("/var/guix").map_err(|e| e.to_string())?;
        crate::sys::mount(Some(&src), &dest_c, None, crate::sys::MS_BIND, None)
            .map_err(|e| format!("bind-mount {empty} on /var/guix: {e}"))?;
        let mut entries =
            std::fs::read_dir("/var/guix").map_err(|e| format!("read /var/guix: {e}"))?;
        if entries.next().is_some() {
            return Err("cold leg: /var/guix not hidden".into());
        }
        Ok(())
    };
    if let Err(e) = hide() {
        eprintln!("td-builder: stage0-place-without-guix: {e}");
        return ExitCode::from(9);
    }
    let placed = std::env::current_dir()
        .map_err(|e| format!("getcwd: {e}"))
        .and_then(|root| crate::stage0::stage0_place(&root, Path::new(dest)));
    match placed {
        Ok(cb) => {
            println!("{cb}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            if let Some(rest) = e.strip_prefix(UNPROVISIONED_TAG) {
                eprintln!("td-builder: stage0-place-without-guix: unprovisioned — {rest}");
                td_engine::exit::unprovisioned_exit()
            } else {
                eprintln!("td-builder: stage0-place-without-guix: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// `tb store-closure DB PATH`, its nonempty lines sorted.
fn closure_lines(tb: &Path, db: &str, path: &str) -> Result<Vec<String>, String> {
    let out = tb_out(tb, &["store-closure", db, path], "store-closure")?;
    Ok(sorted_lines(&out)
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect())
}

fn stage0_cold_start(root: &Path) -> Result<(), String> {
    println!(
        ">> stage0-cold-start: a COLD stage0 placement works with guix state HIDDEN — same \
         path, same SELF-ONLY closure as the warm guix-host placement; an absent seed dir \
         places with no refs, a broken seed dir errors loudly (#313)"
    );
    // A leftover cold placement would let the cold leg reuse its memo, so the
    // scratch must really be gone, not merely attempted.
    let scratch = root.join(".td-build-cache/stage0-cold-start");
    if scratch.exists() {
        let _ = chmod_r_uw(&scratch);
        std::fs::remove_dir_all(&scratch)
            .map_err(|e| format!("FAIL: cannot clear {}: {e}", scratch.display()))?;
    }
    mkdirp(&scratch)?;
    let at = |name: &str| path_str(&scratch.join(name));
    mkdirp(&scratch.join("empty"))?;

    println!(">> warm leg (baseline): place the shared stage0 as every stage0 consumer does");
    let warm = PlacedStage0::place(root).map_err(|e| {
        if e.starts_with(UNPROVISIONED_TAG) {
            e
        } else {
            format!("warm stage0 provisioning did not complete: {e}")
        }
    })?;
    let tbw = &warm.tb;

    println!(
        ">> cold leg (the feature): fresh cache, /var/guix bind-mounted EMPTY in a private \
         mount ns — the placement must need no guix db"
    );
    let cold = scratch.join("cold");
    let cold_s = path_str(&cold)?;
    let empty_s = at("empty")?;
    let mut command = Command::new(tbw);
    command
        .args(["stage0-place-without-guix", &empty_s, &cold_s])
        .stdin(Stdio::null());
    let out = crate::spawn::past_a_busy_program(|| command.output())
        .map_err(|e| format!("FAIL: cannot spawn the cold leg: {e}"))?;
    let cold_err = String::from_utf8_lossy(&out.stderr).into_owned();
    eprint!("{cold_err}");
    if !out.status.success() {
        let msg = format!(
            "cold stage0 placement with /var/guix hidden did not complete ({}): 69 = no \
             toolchain reachable in the jail (skipped); other = the guix-less cold start is \
             broken (#313)",
            out.status
        );
        return Err(tag_if_unprovisioned(&out.status, &cold_err, msg));
    }
    let cbc = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if warm.path != cbc {
        return Err(format!(
            "FAIL: cold placement {cbc} != warm placement {} — provenance drift",
            warm.path
        ));
    }
    let tbc = cold
        .join("store")
        .join(base_of(&cbc))
        .join("bin/td-builder");
    let sent = run_out(&path_str(&tbc)?, &[], "the cold-placed stage0's sentinel")?;
    if sent != "td-builder 0.1.0 ok" {
        return Err(format!("FAIL: cold-placed stage0 sentinel was '{sent}'"));
    }
    println!("  [DURABLE behavioral] the cold-placed stage0 runs its sentinel ({cbc})");
    let cl_cold = closure_lines(&tbc, &path_str(&cold.join("builder.db"))?, &cbc)?;
    let cl_warm = closure_lines(tbw, &warm.db, &warm.path)?;
    if cl_cold != cl_warm {
        let only_warm: Vec<&String> = cl_warm.iter().filter(|l| !cl_cold.contains(l)).collect();
        let only_cold: Vec<&String> = cl_cold.iter().filter(|l| !cl_warm.contains(l)).collect();
        return Err(format!(
            "FAIL: cold closure differs from warm closure (provenance drift): only warm \
             {only_warm:?}, only cold {only_cold:?}"
        ));
    }
    if cl_cold.len() != 1 {
        return Err(format!(
            "FAIL: cold closure is not self-only ({} paths) — the musl-static stage0 builder \
             must record ONLY itself; an external ref means it linked dynamically and would \
             leak a host runtime lib dir into the sandbox (re #469)\n{}",
            cl_cold.len(),
            cl_cold.join("\n")
        ));
    }
    if cl_cold.first() != Some(&cbc) {
        return Err(format!(
            "FAIL: the single cold-closure path is not the canonical builder {cbc}\n{}",
            cl_cold.join("\n")
        ));
    }
    println!(
        "  [DURABLE no-drift] cold (guix state hidden) and warm closures are IDENTICAL and \
         SELF-ONLY (exactly the canonical builder path, no external ref) — the musl-static \
         link keeps every host runtime lib dir out of the sandbox (re #469)"
    );

    println!(">> guix-less arm: an ABSENT seed dir (no /gnu/store at all) must still place, with a self-only closure");
    mkdirp(&scratch.join("probe/bin"))?;
    writef(
        &scratch.join("probe/bin/tool"),
        "no store refs in this tree\n",
    )?;
    let add =
        |name: &str, tree: &str, store: &str, db: &str, seed: &str| -> Result<String, String> {
            tb_out(
                tbw,
                &[
                    "store-add-builder",
                    name,
                    &at(tree)?,
                    &at(store)?,
                    &at(db)?,
                    &at(seed)?,
                ],
                "store-add-builder",
            )
        };
    let pa = add("probe-0.1.0", "probe", "pstore-a", "pa.db", "ABSENT").map_err(|e| {
        format!("FAIL: store-add-builder with an absent seed dir failed — the truly-guix-less arm is broken (#313): {e}")
    })?;
    let pan = closure_lines(tbw, &at("pa.db")?, &pa)?.len();
    if pan != 1 {
        return Err(format!(
            "FAIL: absent-seed-dir placement recorded {pan} closure paths, expected 1 (self only)"
        ));
    }
    println!("  [DURABLE guix-less arm] absent seed dir: placement succeeds, closure is self-only");

    println!(
        ">> idempotent re-placement: re-placing into a store that ALREADY holds the \
         content-addressed path must succeed at the same path, not EEXIST"
    );
    let pa2 = add("probe-0.1.0", "probe", "pstore-a", "pa2.db", "ABSENT").map_err(|e| {
        format!("FAIL: re-placing an already-present content-addressed builder failed — store-add-builder is not idempotent, so a warm re-run cannot place the stage0: {e}")
    })?;
    if pa2 != pa {
        return Err(format!(
            "FAIL: re-placement returned {pa2}, expected the same content-addressed path {pa}"
        ));
    }
    println!("  [DURABLE idempotent re-intern] re-placing the same tree into the same store succeeds at the same path");

    println!(
        ">> fail-loud arm: a PRESENT-but-unreadable seed dir (a regular file, not a directory) \
         must ERROR — a broken seed must not silently place a refless builder (#313 fail-open guard)"
    );
    let notadir = at("notadir")?;
    writef(Path::new(&notadir), "not a store directory\n")?;
    let mut refused = Command::new(tbw);
    refused
        .args(["store-add-builder", "probe-0.1.0", &at("probe")?])
        .args([&at("pstore-f")?, &at("pf.db")?, &notadir])
        .stdin(Stdio::null());
    let out = crate::spawn::past_a_busy_program(|| refused.output())
        .map_err(|e| format!("FAIL: cannot spawn store-add-builder: {e}"))?;
    if out.status.success() {
        return Err("FAIL: store-add-builder ACCEPTED a non-directory seed store — a broken seed silently placed a refless builder (fail-open)".into());
    }
    let pf_err = String::from_utf8_lossy(&out.stderr);
    if !pf_err.contains(&notadir) {
        return Err(format!(
            "FAIL: store-add-builder errored but did not name the bad seed store:\n{pf_err}"
        ));
    }
    println!("  [DURABLE fail-loud] a non-directory seed store errors loudly, naming the bad path (not a silent refless placement)");

    println!(
        ">> self-discrimination: the readdir candidate source is load-bearing — a probe \
         embedding a SYNTHETIC store path records NO ref with the seed dir absent, and DOES \
         record it when a controlled seed dir holding the matching entry is passed"
    );
    // A hash part in the scanner's alphabet (scan.rs BASE32_CHARS), which has
    // no e, o, u or t: one outside it can never be found.
    let g = at("seed/0123456789abcdfghijklmnpqrsvwxyz-fakeref-1.0")?;
    mkdirp(Path::new(&g))?;
    mkdirp(&scratch.join("probe2/bin"))?;
    writef(&scratch.join("probe2/bin/tool"), &g)?;
    let pb0 = add("probe2-0.1.0", "probe2", "pstore-b0", "pb0.db", "ABSENT")?;
    let pbn = closure_lines(tbw, &at("pb0.db")?, &pb0)?.len();
    if pbn != 1 {
        return Err(format!(
            "FAIL: absent-seed-dir scan found {pbn} paths for the embedded-ref probe, expected 1 (no candidates, no refs)"
        ));
    }
    let pb = add("probe2-0.1.0", "probe2", "pstore-b", "pb.db", "seed")?;
    if !closure_lines(tbw, &at("pb.db")?, &pb)?.contains(&g) {
        return Err(format!(
            "FAIL: the embedded ref {g} was NOT found by the controlled seed-dir readdir scan — the candidate source is broken"
        ));
    }
    println!("  [DURABLE self-discrimination] same probe bytes: absent dir → self-only; controlled seed dir → the embedded synthetic ref recorded");
    let _ = chmod_r_uw(&scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: the stage0 placement no longer needs ANY guix state: with /var/guix bind-mounted \
         empty, a cold stage0 placement runs at the SAME canonical path with the SAME \
         SELF-ONLY builder.db closure as the warm placement (re #469). The reference scan stays \
         load-bearing (a synthetic store path is recorded ONLY when a controlled seed dir \
         holding the matching entry is passed); an absent seed dir still places self-only; \
         re-placing an already-present tree is idempotent; and a non-directory seed dir errors \
         loudly. The guix-less cold start (#313) is unblocked."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The readiness test turns on matching a WHOLE argv, and on one byte of
    /// it: `/proc/<pid>/cmdline` NUL-TERMINATES every argument, so cooked it
    /// ends with a trailing space. A `want` built without one never equals
    /// anything, the leaf count stays at zero, and the gate reports that the
    /// tree never started rather than that its matcher is broken.
    #[test]
    fn no_construction_cycle_marker_can_be_found_inside_another() {
        // Leg D scans by SUBSTRING, so distinctness is not enough: one cycle's
        // marker must not appear inside another's cmdline, or a cycle counts a
        // sibling's leaf as its own survivor. What buys that is every marker
        // being the same LENGTH, which holds only while the cycle index fits
        // the two decimal digits the derivation reserves — the invariant a
        // future CYCLES bump would otherwise break in silence.
        const CYCLE_DIGITS: u64 = 100;
        let markers = |pid: u64, n: u64| -> Vec<String> {
            (0..n)
                .map(|i| ((2_000_000 + pid % 1_000_000) * CYCLE_DIGITS + i).to_string())
                .collect()
        };
        let ms = markers(4242, 40);
        for (a, x) in ms.iter().enumerate() {
            assert_eq!(x.len(), 9, "every marker is nine digits: {x}");
            for (b, y) in ms.iter().enumerate() {
                if a != b {
                    assert_ne!(x, y, "markers must be distinct");
                    assert!(!x.contains(y.as_str()), "{y} must not be inside {x}");
                }
            }
        }
        // The bound is real, not decorative: at 100 the cycle digits overflow
        // into the pid and a run one pid away collides outright.
        let over = markers(4242, 101);
        let next = markers(4243, 1);
        assert_eq!(
            over.get(100),
            next.first(),
            "at CYCLES=101 a sibling pid's cycle 0 IS this run's cycle 100 — \
             which is what the const assert in the leg refuses"
        );
    }

    #[test]
    fn a_cmdline_matches_whole_argv_rather_than_a_substring_of_it() {
        let leaf = cook_cmdline(b"/bin/sleep\x001000050\x00".to_vec()).unwrap();
        assert_eq!(leaf, b"/bin/sleep 1000050 ");
        assert_ne!(leaf, b"/bin/sleep 1000050".to_vec());

        // A whole-argv match is what separates a leaf from the `sh` and the
        // two td-builder levels above it, all of which CARRY the marker.
        let sh = cook_cmdline(b"/bin/sh\x00-c\x00/bin/sleep 1000050 & wait\x00".to_vec()).unwrap();
        assert!(has_marker(&sh, "1000050"));
        assert_ne!(sh, b"/bin/sleep 1000050 ".to_vec());

        // Bytes, not a byte-as-char decode: that is Latin-1, so a non-ASCII
        // path would never equal a `want` built from a Rust string.
        let utf8 = cook_cmdline("/bin/slëep\u{0}1000050\u{0}".as_bytes().to_vec()).unwrap();
        assert_eq!(utf8, "/bin/slëep 1000050 ".as_bytes());
        assert!(has_marker(&utf8, "1000050"));

        // An empty marker would match every process on the box; `windows(0)`
        // would panic before it got the chance.
        assert!(!has_marker(&leaf, ""));

        // A zombie's cmdline is empty: not a live process, so not counted.
        assert_eq!(cook_cmdline(Vec::new()), None);
    }

    #[test]
    fn recipe_check_width_is_bounded_by_the_work_and_never_zero() {
        assert_eq!(
            recipe_check_width_for_budget(Some(16 * crate::check_memory::GIB), 1),
            1
        );
        assert_eq!(
            recipe_check_width_for_budget(Some(16 * crate::check_memory::GIB), 2),
            2
        );
        assert_eq!(
            recipe_check_width_for_budget(Some(16 * crate::check_memory::GIB), 26),
            4
        );
        assert_eq!(recipe_check_width_for_budget(None, 26), 1);
        assert_eq!(recipe_check_width_for_budget(Some(1), 0), 1);
    }

    #[test]
    fn recipe_check_width_is_capped_by_the_gates_memory_budget() {
        assert_eq!(
            recipe_check_width_for_budget(Some(8 * crate::check_memory::GIB), 26),
            2
        );
        assert_eq!(
            recipe_check_width_for_budget(Some(4 * crate::check_memory::GIB), 26),
            1
        );
        assert_eq!(
            recipe_check_width_for_budget(Some(512 * 1024 * 1024), 26),
            1
        );
    }

    /// Borrowing fills the CPUs at the jobs one check's 4 GiB pays for, never
    /// past the work, and adds nothing when the grant already fills them.
    #[test]
    fn borrowing_is_bounded_by_the_cpus_and_the_work() {
        assert_eq!(recipe_check_borrowing_for(2, 61, 16), 6);
        assert_eq!(recipe_check_borrowing_for(2, 3, 16), 1);
        assert_eq!(recipe_check_borrowing_for(2, 61, 4), 0);
        assert_eq!(recipe_check_borrowing_for(8, 61, 16), 0);
        assert_eq!(recipe_check_borrowing_for(1, 61, 1), 0);
    }

    /// The longest recorded check starts first, an unrecorded one before it,
    /// and equal times keep the list order.
    #[test]
    fn checks_start_longest_first() {
        let work: Vec<(String, usize)> = ["a", "b", "rust", "new", "c"]
            .iter()
            .map(|s| (s.to_string(), 1))
            .collect();
        let durations = parse_check_durations(
            "a#1\t10.0\nb#1\t5.0\nrust#1\t600.0\nc#1\t10.0\nbad line\nx#1\tnope\n",
        );
        assert_eq!(durations.len(), 4);
        assert_eq!(longest_first(&work, &durations), vec![3, 2, 0, 4, 1]);
        let none = std::collections::BTreeMap::new();
        assert_eq!(longest_first(&work, &none), vec![0, 1, 2, 3, 4]);
    }

    /// The published figure is what the watchdog adds: a running total,
    /// replaced whole, and never below zero.
    #[test]
    fn the_borrow_ledger_publishes_a_running_total() {
        let dir = std::env::temp_dir().join(format!("td-rc-borrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("recipe-checks.borrowed");
        let ledger = BorrowLedger {
            file: Some(file.clone()),
            held: std::sync::Mutex::new(0),
        };
        ledger.adjust(4, true).unwrap();
        ledger.adjust(4, true).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "8");
        ledger.adjust(4, false).unwrap();
        ledger.adjust(9, false).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "0");
        let unpublished = BorrowLedger {
            file: None,
            held: std::sync::Mutex::new(0),
        };
        assert!(unpublished.adjust(4, true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The summary names the longest checks first, breaks ties by name, stops
    /// at the cap, sums every executed check, and says nothing when none ran.
    #[test]
    fn the_slowest_checks_are_named_longest_first() {
        let t = |name: &str, ms: u64| (name.to_string(), std::time::Duration::from_millis(ms));
        let timed = vec![
            t("a#1", 1_000),
            t("c#1", 30_000),
            t("b#1", 30_000),
            t("d#1", 500),
        ];
        assert_eq!(
            slowest_checks(&timed, 3).as_deref(),
            Some(
                ">> recipe-checks: slowest 3 of 4 executed: b#1 30.0s, c#1 30.0s, \
                 a#1 1.0s (all 4 sum 61.5s; checks overlap, so not wall)"
            )
        );
        assert_eq!(slowest_checks(&[], 3), None);
    }

    #[test]
    fn recipe_check_output_keeps_a_bounded_tail_and_finds_split_sentinels() {
        let mut input = vec![b'x'; 8190];
        input.extend_from_slice(crate::check_loop::UNPROVISIONED_SENTINEL.as_bytes());
        input.extend_from_slice(&vec![b'y'; 64]);
        let captured = capture_check_output(
            input.as_slice(),
            &[crate::check_loop::UNPROVISIONED_SENTINEL.as_bytes()],
            128,
        );
        assert!(captured.truncated);
        assert!(captured.saw(0));
        assert_eq!(captured.bytes.len(), 128);
        assert!(captured.bytes.ends_with(&vec![b'y'; 64]));

        // Several needles, each told apart, a short one split as well as a
        // long one; one absent stays unseen.
        let memo = td_engine::exit::CHECK_MEMO_SENTINEL.as_bytes();
        let deferred = td_engine::exit::CHECK_DEFERRED_SENTINEL.as_bytes();
        let mut input = vec![b'x'; 8190];
        input.extend_from_slice(deferred);
        input.extend_from_slice(&vec![b'y'; 64]);
        let captured = capture_check_output(input.as_slice(), &[memo, deferred, b"ab"], 128);
        assert_eq!(captured.saw, vec![false, true, false]);
        assert!(!captured.saw(7), "an index past the needles is unseen");
        let mut input = vec![b'x'; 8191];
        input.extend_from_slice(b"ab");
        let captured = capture_check_output(input.as_slice(), &[memo, b"ab"], 16);
        assert_eq!(captured.saw, vec![false, true]);
        assert!(capture_check_output(&b"zz"[..], &[], 8).saw.is_empty());
    }

    // The pool's contract, which the verdict rests on: every work item yields
    // EXACTLY ONE result, and the results come back in WORK order rather than
    // completion order. Ordering is what makes the skipped-name list stable
    // whatever the machine's load did, and it is not observable from the width
    // policy. Driven over a script that decides its verdict from the index it is
    // given, at a width that guarantees real overlap.
    #[test]
    fn the_pool_returns_one_result_per_item_in_work_order() {
        let dir = std::env::temp_dir().join(format!("td-rc-pool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let code = crate::check_loop::EXIT_UNPROVISIONED;
        let sentinel = crate::check_loop::UNPROVISIONED_SENTINEL;
        // index 1 -> pass, 2 -> fail, 3 -> host gap, repeating.
        let script = dir.join("check");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$TD_RECIPE_CHECK_INDEX\" in\n\
                 *[147]) exit 0 ;;\n\
                 *[258]) echo boom >&2; exit 1 ;;\n\
                 *) echo '{sentinel}' >&2; exit {code} ;;\nesac\n"
            ),
        )
        .unwrap();
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let work: Vec<(String, usize)> = (1..=9).map(|i| ("spec".to_string(), i)).collect();
        // Started in REVERSE, so the results being in work order is the
        // pool's doing and not the start order's. No borrowing: gate 325
        // runs this inside a hosted gate, whose real tokens a test must not
        // take.
        let reversed: Vec<usize> = (0..work.len()).rev().collect();
        let got =
            run_recipe_checks_concurrently(&work, &reversed, 4, 0, &script, "e", "s").unwrap();

        assert_eq!(got.len(), work.len(), "one result per work item");
        for (i, (outcome, _took)) in got.iter().enumerate() {
            let index = i + 1;
            let want = match index % 3 {
                1 => CheckOutcome::Passed,
                2 => CheckOutcome::Failed,
                _ => CheckOutcome::HostGap,
            };
            assert_eq!(
                *outcome, want,
                "result {index} is out of work order or misjudged"
            );
        }

        // And the verdict is width-INDEPENDENT: the same work at width 1 gives
        // the same answers in the same places.
        let in_order: Vec<usize> = (0..work.len()).collect();
        let serial =
            run_recipe_checks_concurrently(&work, &in_order, 1, 0, &script, "e", "s").unwrap();
        assert!(
            run_recipe_checks_concurrently(&work, &in_order[1..], 1, 0, &script, "e", "s").is_err(),
            "an order that leaves a check out must not run as a full list"
        );
        let mut doubled = in_order.clone();
        doubled[1] = 0;
        assert!(
            run_recipe_checks_concurrently(&work, &doubled, 1, 0, &script, "e", "s").is_err(),
            "an order that starts one check twice must not run"
        );
        let verdicts = |r: &[(CheckOutcome, std::time::Duration)]| {
            r.iter().map(|(o, _)| *o).collect::<Vec<_>>()
        };
        assert_eq!(
            verdicts(&got),
            verdicts(&serial),
            "width must not change any check's verdict or its position"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A check that could not run HERE is not a check that passed. All-skipped
    /// must report the gate's own unprovisioned skip (green would be a gate
    /// asserting nothing), and a partial skip must NAME what was missed —
    /// "ran 25" over 5 silent skips is how a shrinking suite goes unnoticed.
    #[test]
    fn recipe_checks_never_reports_a_skip_as_coverage() {
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();

        // A real failure outranks any number of skips.
        let (_, v) = recipe_checks_verdict(25, 1, &s(&["a#1"]), 0, &[], &[]);
        assert!(v.unwrap_err().starts_with("FAIL: "));

        // Nothing ran anywhere here → the gate's own tolerated skip, tagged so
        // `cli` maps it to 69 + sentinel rather than a bare failure.
        let (_, v) = recipe_checks_verdict(3, 0, &s(&["a#1", "b#1", "c#1"]), 0, &[], &[]);
        let err = v.unwrap_err();
        assert!(err.starts_with(UNPROVISIONED_TAG), "{err:?}");
        assert!(err.contains("a#1 b#1 c#1"), "names them: {err:?}");
        assert!(!err.starts_with("FAIL: "));

        // A failure does not excuse dropping the caveat: "1 of 25 failed" over
        // 20 silent skips reads as though 24 checks vouched for the tree.
        let (lines, v) = recipe_checks_verdict(25, 1, &s(&["a#1", "b#1"]), 0, &[], &[]);
        assert!(v.is_err() && lines.len() == 1, "{lines:?}");
        assert!(lines[0].contains(crate::check_loop::GATES_SKIPPED_SENTINEL));
        assert!(lines[0].contains("a#1 b#1"));

        // Nothing ran at all is never a pass.
        let (_, v) = recipe_checks_verdict(0, 0, &[], 0, &[], &[]);
        assert!(v.is_err(), "0 checks must not report green");

        // Partial → green, but the caveat comes FIRST and names every skip.
        let (lines, v) = recipe_checks_verdict(3, 0, &s(&["b#1"]), 0, &[], &[]);
        assert!(v.is_ok());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("NOT full coverage") && lines[0].contains("b#1"));
        // Prose is not a signal: automation greps the token, and this gate
        // exits 0, so without it a partial reads as a full green.
        assert!(
            lines[0].contains(crate::check_loop::GATES_SKIPPED_SENTINEL),
            "a partial skip must carry the incomplete-coverage token: {:?}",
            lines[0]
        );
        assert!(lines[1].starts_with("PASS: ") && lines[1].contains("ran 2 of 3"));

        // Clean run → one PASS line, no caveat.
        let (lines, v) = recipe_checks_verdict(3, 0, &[], 0, &[], &[]);
        assert!(v.is_ok() && lines.len() == 1 && lines[0].starts_with("PASS: "));
        assert!(
            !lines[0].contains(crate::check_loop::GATES_SKIPPED_SENTINEL),
            "a full run must NOT claim incomplete coverage"
        );
    }

    /// A scoped run names what it left out beside its verdict.
    #[test]
    fn a_scoped_run_names_the_checks_it_did_not_reach() {
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        let (lines, v) = recipe_checks_verdict(9, 0, &[], 0, &[], &s(&["x#1", "y#2"]));
        assert!(v.is_ok());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2 check(s) not reached") && l.contains("x#1 y#2")),
            "{lines:?}"
        );
        assert!(lines
            .last()
            .is_some_and(|l| l.starts_with("PASS: recipe-checks - ran 9 of 9")));
        let (lines, _) = recipe_checks_verdict(9, 0, &[], 0, &[], &[]);
        assert!(
            !lines.iter().any(|l| l.contains("not reached")),
            "{lines:?}"
        );
        let (lines, v) = recipe_checks_verdict(9, 1, &[], 0, &[], &s(&["x#1"]));
        assert!(v.is_err());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("1 check(s) not reached") && l.contains("x#1")),
            "a failure still names what the scope left out: {lines:?}"
        );
        let list = split_check_list("# scope miss: no recipe reads x\na\nb c\n");
        assert_eq!(list.misses, vec!["scope miss: no recipe reads x"]);
        assert_eq!(list.stems, vec!["a", "b", "c"]);
        assert!(list.whys.is_empty());
        // A reason a check was reached is no miss.
        let list = split_check_list("# reach: a: uutils reads recipes/x.rs\na\n");
        assert_eq!(list.whys, vec!["reach: a: uutils reads recipes/x.rs"]);
        assert!(list.misses.is_empty());
        assert_eq!(split_check_list(""), CheckList::default());
        // The evaluator's copy of the prefix is this one: a drift would take
        // every reason for a miss and run every check.
        let evaluator = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../recipes/src/bin/td_recipe_eval/check_runner.rs");
        match std::fs::read_to_string(&evaluator) {
            Ok(text) => assert!(
                text.contains(&format!(
                    "pub(crate) const REACH_NOTE: &str = {REACH_NOTE:?};"
                )),
                "{} holds another REACH_NOTE",
                evaluator.display()
            ),
            Err(e) => eprintln!("SKIP: {} ({e}): builder-only tree", evaluator.display()),
        }
    }

    /// A memoized pass is a pass that is counted apart and said out loud, on
    /// its own line and in the PASS line's count, so a run that answered
    /// everything from the memo cannot read as one that re-proved it all.
    #[test]
    fn recipe_checks_reports_memoized_passes_apart() {
        let (lines, v) = recipe_checks_verdict(44, 0, &[], 41, &[], &[]);
        assert!(v.is_ok());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("41 of 44") && lines[0].contains("TD_CHECK_FULL=1"));
        assert!(lines[1].starts_with("PASS: ") && lines[1].contains("ran 3 of 44 (41 memoized)"));
        // All memoized is still green: every check has a pass on record for
        // exactly these inputs.
        let (lines, v) = recipe_checks_verdict(3, 0, &[], 3, &[], &[]);
        assert!(
            v.is_ok() && lines[1].contains("ran 0 of 3 (3 memoized)"),
            "{lines:?}"
        );
        // A failure still outranks it, and the count of what ran is honest
        // beside a skip.
        let (lines, v) = recipe_checks_verdict(3, 1, &[], 2, &[], &[]);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2 of 3 check(s) answered from the verdict memo")),
            "a failure beside memoized passes still counts them: {lines:?}"
        );
        assert!(v.is_err());
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        let (lines, v) = recipe_checks_verdict(4, 0, &s(&["a#1"]), 2, &[], &[]);
        assert!(
            v.is_ok()
                && lines
                    .last()
                    .is_some_and(|l| l.contains("ran 1 of 4 (2 memoized)")),
            "{lines:?}"
        );
    }

    /// The memo sentinel on stdout marks a memoized pass, and only on a pass:
    /// with a non-zero exit it is a failure like any other, and on stderr it
    /// is not the memo's line.
    #[test]
    fn a_recipe_check_is_memoized_only_on_a_pass_that_says_so_on_stdout() {
        let dir = std::env::temp_dir().join(format!("td-rcm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let memo = td_engine::exit::CHECK_MEMO_SENTINEL;
        let write = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perm = std::fs::metadata(&p).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
            std::fs::set_permissions(&p, perm).unwrap();
            p
        };
        let run = |p: &Path| {
            run_recipe_check(p, "spec", 1, "e", "s", RECIPE_CHECK_PEAK_BYTES)
                .unwrap()
                .outcome
        };
        assert!(matches!(
            run(&write(
                "memo",
                &format!("echo '{memo} spec#1: passed before'; exit 0")
            )),
            CheckOutcome::Memoized
        ));
        assert!(matches!(
            run(&write("memofail", &format!("echo '{memo}'; exit 1"))),
            CheckOutcome::Failed
        ));
        assert!(matches!(
            run(&write("memoerr", &format!("echo '{memo}' >&2; exit 0"))),
            CheckOutcome::Passed
        ));
        assert!(matches!(
            run(&write("plain", "echo PASS; exit 0")),
            CheckOutcome::Passed
        ));
        let deferred = td_engine::exit::CHECK_DEFERRED_SENTINEL;
        assert!(matches!(
            run(&write(
                "deferred",
                &format!("echo '{deferred} spec#1: deferred to main'; exit 0")
            )),
            CheckOutcome::Deferred
        ));
        assert!(matches!(
            run(&write(
                "deferredfail",
                &format!("echo '{deferred}'; exit 1")
            )),
            CheckOutcome::Failed
        ));
        assert!(matches!(
            run(&write(
                "deferrederr",
                &format!("echo '{deferred}' >&2; exit 0")
            )),
            CheckOutcome::Passed
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deferral is green, like a memo answer, but named: on its own line,
    /// in the PASS line's count, and beside a failure.
    #[test]
    fn recipe_checks_names_deferred_checks_apart() {
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        let (lines, v) = recipe_checks_verdict(5, 0, &[], 1, &s(&["a#1", "b#2"]), &[]);
        assert!(v.is_ok(), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2 of 5 check(s) deferred to main") && l.ends_with("a#1 b#2")),
            "{lines:?}"
        );
        assert!(
            lines
                .last()
                .is_some_and(|l| l.contains("ran 2 of 5 (1 memoized, 2 deferred to main)")),
            "{lines:?}"
        );
        let (lines, v) = recipe_checks_verdict(2, 0, &[], 0, &s(&["a#1", "b#2"]), &[]);
        assert!(
            v.is_ok()
                && lines
                    .last()
                    .is_some_and(|l| l.contains("ran 0 of 2 (2 deferred to main)")),
            "{lines:?}"
        );
        let (lines, v) = recipe_checks_verdict(3, 1, &[], 0, &s(&["a#1"]), &[]);
        assert!(v.is_err());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("deferred to main") && l.ends_with("a#1")),
            "a failure beside a deferral still names it: {lines:?}"
        );
    }

    /// The skip verdict is EVIDENCE, not the exit code alone: a 69 without the
    /// sentinel is some other tool's EX_UNAVAILABLE and must stay a failure, or
    /// a regression hides as "nothing to run here". Runs real children, because
    /// the tee-and-scan is the part that can break.
    #[test]
    fn a_recipe_check_skips_only_on_a_69_that_carries_the_sentinel() {
        let dir = std::env::temp_dir().join(format!("td-rcv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = crate::check_loop::UNPROVISIONED_SENTINEL;
        let code = crate::check_loop::EXIT_UNPROVISIONED;

        let write = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perm = std::fs::metadata(&p).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
            std::fs::set_permissions(&p, perm).unwrap();
            p
        };
        let run = |p: &Path| {
            run_recipe_check(p, "spec", 1, "e", "s", RECIPE_CHECK_PEAK_BYTES)
                .unwrap()
                .outcome
        };

        assert!(matches!(run(&write("ok", "exit 0")), CheckOutcome::Passed));
        assert!(matches!(
            run(&write(
                "gap",
                &format!("echo '{sentinel}' >&2; exit {code}")
            )),
            CheckOutcome::HostGap
        ));
        assert!(
            matches!(
                run(&write("bare", &format!("exit {code}"))),
                CheckOutcome::Failed
            ),
            "a bare 69 is not proof of a host gap"
        );
        assert!(
            matches!(
                run(&write("wrong", &format!("echo '{sentinel}' >&2; exit 1"))),
                CheckOutcome::Failed
            ),
            "the sentinel alone does not license a skip"
        );
        // Invalid UTF-8 on stderr must not fail a check that passed: the tee
        // forwards bytes, so one stray byte from a compiler cannot red a green.
        assert!(
            matches!(
                run(&write("binary", "printf '\\377\\376 noise\\n' >&2; exit 0")),
                CheckOutcome::Passed
            ),
            "non-UTF-8 stderr must not turn a passing check into an error"
        );
        assert!(
            matches!(
                run(&write(
                    "bingap",
                    &format!("printf '\\377\\376\\n' >&2; echo '{sentinel}' >&2; exit {code}")
                )),
                CheckOutcome::HostGap
            ),
            "the sentinel is still found alongside undecodable bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A native body that shells out to a tool which could not provision a
    /// toolchain must re-raise that as 69, not 1: `cli` keys off the tag, and
    /// gate-run keys off the 69, so an untagged message is a hard RED where the
    /// contract says tolerated skip. Tightness matters as much as the mapping —
    /// a 69 WITHOUT the sentinel is some other tool's EX_UNAVAILABLE and must
    /// stay red, or a real regression could hide as a skip.
    #[test]
    fn an_unprovisioned_child_is_tagged_but_only_with_the_sentinel() {
        use std::os::unix::process::ExitStatusExt;
        let unprov = std::process::ExitStatus::from_raw(crate::check_loop::EXIT_UNPROVISIONED << 8);
        let other = std::process::ExitStatus::from_raw(1 << 8);
        let sentinel = crate::check_loop::UNPROVISIONED_SENTINEL;

        let child = format!("no rustc\n{sentinel}\n");
        let tagged = tag_if_unprovisioned(&unprov, &child, "FAIL: m".into());
        // Tagged, and the FAIL: lead-in dropped — it is reported as a skip.
        assert_eq!(
            tagged.strip_prefix(UNPROVISIONED_TAG),
            Some("m"),
            "{tagged:?}"
        );
        // A bare 69 with no sentinel is not td's provisioning path.
        assert_eq!(
            tag_if_unprovisioned(&unprov, "boom\n", "FAIL: m".into()),
            "FAIL: m"
        );
        // The sentinel alone does not license a skip either.
        assert_eq!(
            tag_if_unprovisioned(&other, &child, "FAIL: m".into()),
            "FAIL: m"
        );
    }

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

    /// A child's exit 69 is passed through untagged: the body must not print
    /// td's provisioning sentinel for it, or a bare 69 from an evaluator would
    /// satisfy gate-run's two-part skip test.
    #[test]
    fn a_childs_exit_69_is_passed_through_without_the_sentinel_tag() {
        let exit = |code: &str| {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("exit {code}")]);
            run_streamed(command, "probe")
        };
        let skipped = exit("69").unwrap_err();
        assert!(skipped.starts_with(CHILD_EXIT_69_TAG), "{skipped}");
        assert!(!skipped.contains(UNPROVISIONED_TAG), "{skipped}");
        let failed = exit("3").unwrap_err();
        assert!(failed.starts_with("FAIL: probe"), "{failed}");
        assert!(exit("0").is_ok());
    }

    #[test]
    fn gate_command_words_split_the_derived_cargo_commands() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let cmds = crate::affected::gate_cargo_cmds(root).unwrap();
        assert!(!cmds.is_empty());
        for line in &cmds {
            let words = gate_command_words(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(words.len() >= 2, "{line}: {words:?}");
            assert!(words.iter().all(|w| !w.contains('\'')), "{line}: {words:?}");
        }
        assert_eq!(
            gate_command_words("cargo test --config 'env.X.value=\"1\"'").unwrap(),
            ["cargo", "test", "--config", "env.X.value=\"1\""]
        );
        assert_eq!(
            gate_command_words("'/a b/td-builder'  gate-crates names").unwrap(),
            ["/a b/td-builder", "gate-crates", "names"]
        );
        assert_eq!(gate_command_words("x ''").unwrap(), ["x", ""]);
        for refused in [
            "cargo test; rm -rf x",
            "cargo $HOME",
            "cargo 'open",
            "cargo test | tee log",
            "cargo \"x\"",
            "cargo *",
            "cargo a{b,c}",
            "cargo !x",
            "",
            "   ",
        ] {
            assert!(gate_command_words(refused).is_err(), "{refused:?}");
        }
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
        assert_eq!(
            lock.inputs,
            vec!["dddd glibc-2.41.tar.xz # trailing note".to_string()]
        );
    }

    #[test]
    fn filter_pin_lines_keeps_raw_pin_lines_for_set_compare() {
        let raw = filter_pin_lines(LOCK_FIXTURE);
        assert_eq!(raw.len(), 3);
        assert!(raw
            .iter()
            .all(|l| l.starts_with("input ") || l.starts_with("patch ")));
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
        assert_eq!(
            source_pin_sha(pins, "glibc-2.41.tar.xz").as_deref(),
            Some("feedface")
        );
        assert_eq!(
            source_pin_sha(pins, "gcc-14.3.0.tar.xz").as_deref(),
            Some("deadbeef")
        );
        assert_eq!(source_pin_sha(pins, "not-there.tar.xz"), None);
    }

    #[test]
    fn store_root_for_takes_the_first_component_store() {
        assert_eq!(
            store_root_for("/td/store/abc-bash/bin/bash").unwrap(),
            "/td/store"
        );
        // First-component agnostic: any /<x>/store root derives, none is hardcoded.
        assert_eq!(
            store_root_for("/seed/store/abc-sleep/bin/sleep").unwrap(),
            "/seed/store"
        );
        assert!(store_root_for("/not-a-store-path").is_err());
        assert!(store_root_for("relative/store/x").is_err());
    }
}
