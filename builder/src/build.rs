//! autotools-build — td's own minimal build "system", in Rust (DESIGN §7.1
//! corpus-independence; td's "own Rust builder").
//!
//! This is the REPLACEMENT for gnu-build-system's Guile phase runner. It is
//! invoked AS the derivation's `builder` by the daemon (system td-build
//! constructs that derivation with guix's low-level `derivation`, so the .drv
//! construction stays in guix while the build LOGIC is td's, in Rust). It runs
//! the standard
//! autotools phases directly:
//!
//!   set-paths -> unpack -> configure (--prefix=$out) -> make -> make install
//!
//! No Guile runs in the build. The environment is derived from the inputs the
//! way gnu-build-system's `set-paths` phase does, but here in Rust. The build
//! tools (tar, gcc, make, …) are the Guix toolchain — retired LAST (§5); what is
//! removed is the build-system Guile, not the toolchain.
//!
//! Every phase command runs under a fail-fast watchdog (#308) — see `Watch`: a
//! broken staged closure must red in minutes with a named tool, never spin.
//!
//! Inputs (env, set by system td-build):
//!   out                output prefix (the daemon sets this)
//!   TD_SRC             the source tarball (a fixed-output url-fetch)
//!   TD_INPUTS          ':'-joined store paths of the build inputs
//!   TD_CONFIGURE_FLAGS extra ./configure flags as a JSON array of strings (may be
//!                      empty/absent); each element is ONE argument, so a flag may
//!                      carry internal whitespace (e.g. `CFLAGS=-O2 -g -Wno-foo`)
//!   TD_PHASES          the recipe's custom build PHASES as JSON (may be empty) —
//!                      td's own interpreter (below) applies them after unpack,
//!                      the way gnu-build-system runs a recipe's `#:phases`. This
//!                      is what lets the OWN-builder path build a package with
//!                      real source-patch phases (e.g. gettext-minimal) with NO
//!                      Guile/gnu-build-system in the build.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::json::Json;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Find an executable `name` in a ':'-joined search path; return its abs path.
fn find_in_path(path: &str, name: &str) -> Option<String> {
    for dir in path.split(':').filter(|s| !s.is_empty()) {
        let cand = format!("{dir}/{name}");
        if Path::new(&cand).is_file() {
            return Some(cand);
        }
    }
    None
}

/// patch-source-shebangs (in Rust) — gnu-build-system rewrites `#!/bin/sh` (and
/// friends) across the unpacked tree to a real interpreter, because the pure
/// build sandbox has no /bin/sh. td does the same: any file whose shebang names
/// an absolute `sh`/`bash` NOT already under /gnu/store is rewritten to the seed
/// bash (sh-compatible). This is what lets a package's OWN build scripts execute
/// in the sandbox — e.g. gawk's `build-aux/install-sh`, run directly by its
/// install rule, whose `#!/bin/sh` would otherwise fail with "required file not
/// found". Deterministic (the bash path is pinned), so it stays reproducible.
fn patch_shebangs(dir: &Path, bash: &str) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd {
            let entry = entry.map_err(|e| e.to_string())?;
            let ft = entry.file_type().map_err(|e| e.to_string())?;
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                patch_one_shebang(&entry.path(), bash)?;
            }
        }
    }
    Ok(())
}

/// Rewrite one file's shebang iff it names an absolute sh/bash outside the store.
/// Peeks two bytes first, so non-scripts (incl. big binaries) are not slurped.
fn patch_one_shebang(path: &Path, bash: &str) -> Result<(), String> {
    use std::io::Read;
    let mut head = [0u8; 2];
    match fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(2) if &head == b"#!" => {}
        _ => return Ok(()), // unreadable, empty, or not a script — leave it
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let nl = bytes.iter().position(|&b| b == b'\n').unwrap_or(bytes.len());
    let line = match std::str::from_utf8(&bytes[..nl]) {
        Ok(s) => s,
        Err(_) => return Ok(()), // binary first line — skip
    };
    // "#!  /bin/sh -e"  ->  interp="/bin/sh", trailing=" -e"
    let after = line[2..].trim_start();
    let mut it = after.splitn(2, char::is_whitespace);
    let interp = it.next().unwrap_or("");
    let trailing = it.next().map(|s| format!(" {s}")).unwrap_or_default();
    if !interp.starts_with('/') || interp.starts_with("/gnu/store/") {
        return Ok(()); // relative, or already a store interpreter
    }
    match interp.rsplit('/').next() {
        Some("sh") | Some("bash") => {} // only the toolchain shell
        _ => return Ok(()),
    }
    // Preserve the file's timestamps across the rewrite: autotools' generated
    // files (configure, aclocal.m4, Makefile.in) are shipped NEWER than their
    // sources so `make` does NOT try a maintainer-mode regeneration. Bumping an
    // mtime to "now" inverts that order and make then runs aclocal/autoconf —
    // absent from the seed — failing with exit 127 (coreutils hit this). A
    // shebang fix must be invisible to make's timestamp dependency graph.
    let meta = fs::metadata(path).ok();
    // Some tarballs ship build scripts read-only (e.g. less's mkinstalldirs is
    // 0444); both fs::write and the mtime-restore reopen below would then fail
    // EACCES. Temporarily grant owner-write, rewrite, and restore the ORIGINAL
    // mode so the on-disk tree differs only in the shebang line — $out file
    // modes come from `make install`, not the source tree, so this stays
    // reproducibility-safe.
    use std::os::unix::fs::PermissionsExt;
    let orig_mode = meta.as_ref().map(|m| m.permissions().mode());
    if let Some(mode) = orig_mode {
        if mode & 0o200 == 0 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200));
        }
    }
    // File::create truncates but keeps the existing mode (exec bit survives).
    let mut out = format!("#!{bash}{trailing}").into_bytes();
    out.extend_from_slice(&bytes[nl..]);
    fs::write(path, &out).map_err(|e| format!("patch-shebang {}: {e}", path.display()))?;
    if let Some(meta) = meta.as_ref() {
        if let (Ok(accessed), Ok(modified)) = (meta.accessed(), meta.modified()) {
            if let Ok(f) = fs::File::options().write(true).open(path) {
                let _ = f.set_times(fs::FileTimes::new().set_accessed(accessed).set_modified(modified));
            }
        }
    }
    if let Some(mode) = orig_mode {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    Ok(())
}

/// The single sub-directory of `dir` (an unpacked source tree). Errors unless
/// there is exactly one — a deterministic, fail-closed "unpack" result.
fn single_subdir(dir: &str) -> Result<String, String> {
    let mut subdirs: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {dir}: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.path().is_dir() {
            subdirs.push(entry.path().to_string_lossy().into_owned());
        }
    }
    match subdirs.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(format!(
            "expected exactly one unpacked source directory, found {}",
            subdirs.len()
        )),
    }
}

/// Phase-command watchdog bounds (#308). A broken staged closure can make a GNU
/// configure spin FOREVER instead of failing: with a helper tool persistently
/// dying (issue #292 — every `expr` aborted on a missing libgmp.so.10, so the
/// `ac_count` increment in configure's "checking for grep that handles long
/// lines" loop never happened), configure retries at 100% CPU for 30+ minutes,
/// turning a clean, diagnosable red into an apparent hang that ties up a
/// heavy-gate slot. `run_cmd` supervises every phase command with two
/// independent bounds; zero disables a bound:
///
///   * `stderr_repeat_limit` — the SAME stderr line this many times in a row is
///     a persistently-failing tool in a retry loop. Trips in seconds on the
///     #292 shape, and the repeated line itself names the failing tool.
///   * `silence` — no output on either stream for this long is a wedged phase:
///     the backstop for a spin whose tool stderr configure redirects away
///     (conftest stderr usually goes to /dev/null or config.log).
///
/// The bounds are compiled in per phase, not env knobs: the sandbox clears the
/// builder's env, and a drv-env knob would vary the drv hash with a tuning
/// value. Tests pass their own tiny bounds.
struct Watch {
    silence: Duration,
    stderr_repeat_limit: u32,
}

/// Default phase bound: make can legitimately be silent for minutes while one
/// big translation unit compiles; 30 minutes is comfortably past the corpus'
/// (and the bootstrap chain's) worst single-file case, while still bounding a
/// truly-wedged phase.
const WATCH_PHASE: Watch =
    Watch { silence: Duration::from_secs(1800), stderr_repeat_limit: 200 };

/// configure bound: each configure check compiles+links a conftest in seconds,
/// so ten silent minutes means wedged — this is what turns the #292 class of
/// hang into a red "within minutes" even when the loop is silent.
const WATCH_CONFIGURE: Watch =
    Watch { silence: Duration::from_secs(600), stderr_repeat_limit: 200 };

/// Clip one raw output line to a printable diagnostic fragment.
fn clip_line(line: &[u8]) -> String {
    const MAX: usize = 400;
    let head = line.get(..MAX.min(line.len())).unwrap_or(line);
    let ell = if line.len() > MAX { "…" } else { "" };
    format!("{}{ell}", String::from_utf8_lossy(head))
}

/// The trip channel shared by the supervisor threads: the first cause wins,
/// and a trip SIGKILLs the phase's whole process group — configure's children
/// included, not just the shell.
struct TripCtl<'a> {
    why: &'a Mutex<Option<String>>,
    tripped: &'a AtomicBool,
    pgid: u32,
}

impl TripCtl<'_> {
    fn trip(&self, why: String) {
        if let Ok(mut t) = self.why.lock() {
            if t.is_none() {
                *t = Some(why);
                self.tripped.store(true, Ordering::Relaxed);
                let _ = crate::sys::kill_process_group(self.pgid, crate::sys::SIGKILL);
            }
        }
    }
}

/// The stderr accountant's state.
struct StderrWatch {
    last_line: Vec<u8>,
    repeats: u32,
    /// Last few stderr lines, clipped, for the kill diagnostic.
    tail: std::collections::VecDeque<String>,
}

/// Account one complete stderr line: repeat counting + the diagnostic tail.
/// Trips (and kills the group) when the SAME line repeats `limit` times in a
/// row — decided here at line granularity, so a command that emits fewer
/// repeats and exits green can never race a sampling thread into a false kill.
fn account_stderr_line(st: &mut StderrWatch, line: &[u8], limit: u32, ctl: &TripCtl) {
    if line == st.last_line.as_slice() {
        st.repeats = st.repeats.saturating_add(1);
    } else {
        st.last_line = line.to_vec();
        st.repeats = 1;
    }
    if st.tail.len() >= 5 {
        st.tail.pop_front();
    }
    st.tail.push_back(clip_line(line));
    if limit > 0 && st.repeats >= limit && !ctl.tripped.load(Ordering::Relaxed) {
        ctl.trip(format!(
            "the same stderr line repeated {}x (a persistently-failing tool in a retry loop): {}",
            st.repeats,
            clip_line(line)
        ));
    }
}

/// Tee one child stream to `sink`, updating the shared activity clock; for
/// stderr (`watch_lines` set) also split into lines for the repeat accountant.
/// Chunk-based (not read_until): a `\r`-progress stream with no newline still
/// counts as activity, and an unterminated line cannot grow unboundedly.
fn tee_stream(
    mut src: impl std::io::Read,
    mut sink: impl std::io::Write,
    start: Instant,
    last_activity_ms: &AtomicU64,
    watch_lines: Option<(&Mutex<StderrWatch>, u32)>,
    ctl: &TripCtl,
) {
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let chunk = buf.get(..n).unwrap_or(&buf);
        let _ = sink.write_all(chunk);
        let _ = sink.flush();
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        last_activity_ms.store(elapsed, Ordering::Relaxed);
        if let Some((watch, limit)) = watch_lines {
            pending.extend_from_slice(chunk);
            while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = pending.drain(..=nl).collect();
                line.pop(); // the \n
                if let Ok(mut st) = watch.lock() {
                    account_stderr_line(&mut st, &line, limit, ctl);
                }
            }
            // A pathological unterminated "line": account it in slices so the
            // buffer stays bounded (repeat detection needs whole lines anyway).
            if pending.len() > 65536 {
                if let Ok(mut st) = watch.lock() {
                    account_stderr_line(&mut st, &pending, limit, ctl);
                }
                pending.clear();
            }
        }
    }
}

/// Run a command with a CLEAN environment (`envs` only), in `cwd`, echoing it to
/// the build log. Fail-closed: a non-zero exit aborts the build. Supervised by
/// `watch` (#308): the child runs in its OWN process group with stdout/stderr
/// teed to the build log, and a tripped bound SIGKILLs the whole group and reds
/// the phase with the last stderr lines — a broken tool loop in configure
/// becomes a diagnosable red in minutes, not a 30-minute spin.
fn run_cmd(
    prog: &str,
    args: &[&str],
    cwd: &str,
    envs: &[(String, String)],
    watch: &Watch,
) -> Result<(), String> {
    println!(">> td-build: (cd {cwd} && {prog} {})", args.join(" "));
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(envs.iter().map(|(k, v)| (k.clone(), v.clone())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn {prog}: {e}"))?;
    let pgid = child.id();
    let child_out = child.stdout.take().ok_or_else(|| format!("{prog}: no stdout pipe"))?;
    let child_err = child.stderr.take().ok_or_else(|| format!("{prog}: no stderr pipe"))?;

    let start = Instant::now();
    let last_activity_ms = AtomicU64::new(0);
    let why: Mutex<Option<String>> = Mutex::new(None);
    let tripped = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let ctl = TripCtl { why: &why, tripped: &tripped, pgid };
    let stderr_watch = Mutex::new(StderrWatch {
        last_line: Vec::new(),
        repeats: 0,
        tail: std::collections::VecDeque::new(),
    });

    let silence_ms = u64::try_from(watch.silence.as_millis()).unwrap_or(u64::MAX);
    let status = std::thread::scope(|s| {
        let out_reader = s.spawn(|| {
            tee_stream(child_out, std::io::stdout(), start, &last_activity_ms, None, &ctl);
        });
        let err_reader = s.spawn(|| {
            tee_stream(
                child_err,
                std::io::stderr(),
                start,
                &last_activity_ms,
                Some((&stderr_watch, watch.stderr_repeat_limit)),
                &ctl,
            );
        });
        // Silence watchdog: runs until the readers have drained (NOT merely
        // until the child exits) — so a build that leaves a background child
        // holding the pipes open is also bounded, instead of hanging the join.
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) && !tripped.load(Ordering::Relaxed) {
                if silence_ms > 0 {
                    let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    if elapsed.saturating_sub(last_activity_ms.load(Ordering::Relaxed)) > silence_ms {
                        ctl.trip(format!("no output for {}s (a wedged phase)", silence_ms / 1000));
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let st = child.wait();
        let _ = out_reader.join();
        let _ = err_reader.join();
        stop.store(true, Ordering::Relaxed);
        st
    });

    let why = why.lock().ok().and_then(|mut t| t.take());
    if let Some(why) = why {
        let tail = stderr_watch
            .lock()
            .map(|st| st.tail.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let tail = if tail.is_empty() { String::new() } else { format!("; last stderr lines:\n{tail}") };
        return Err(format!(
            "td-build watchdog KILLED `{prog} {}` after {}s — {why}{tail}",
            args.join(" "),
            start.elapsed().as_secs(),
        ));
    }
    match status {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(format!("{prog} {} failed: {st}", args.join(" "))),
        Err(e) => Err(format!("wait {prog}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Phase interpreter — td's own runner for a recipe's custom build phases (the
// move-off-Guile §5 step toward td owning .drv creation: td's builder runs the
// phases, not gnu-build-system's Guile). The recipe's phase DATA arrives as JSON
// in TD_PHASES; we apply each `substitute*` with the toolchain's `sed`/`find`.
// Scope: this is the OWN-builder (behavioral) path — the output has a distinct
// store path, so the substitutions need to produce the right EFFECT, not a
// byte-identical edit. `let`-`which` bindings + `with-fluids` wrappers are
// descended; their `{var}` references resolve to the bound program path.

use std::collections::BTreeMap;

/// Escape a LITERAL string for the replacement side of `sed s|…|…|`: `\` and `&`
/// are special there, and a newline would terminate the `s` command (so it
/// becomes the `\n` sed understands as "insert a newline").
fn escape_sed_repl(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '&' => o.push_str("\\&"),
            '\n' => o.push_str("\\n"),
            _ => o.push(c),
        }
    }
    o
}

/// One `RefPart`/replacement atom → its sed-replacement text. `bindings` maps a
/// `let`-`which` name to the resolved program path; a `{var}` not in it is a
/// match variable (the whole match → `&`).
fn resolve_part(p: &Json, bindings: &BTreeMap<String, String>, search_path: &str) -> Result<String, String> {
    if let Some(s) = p.as_str() {
        return Ok(escape_sed_repl(s));
    }
    if let Some(n) = p.get("var").and_then(Json::as_str) {
        return Ok(match bindings.get(n) {
            Some(v) => escape_sed_repl(v),
            None => "&".to_string(), // match variable: the whole match
        });
    }
    if let Some(n) = p.get("output").and_then(Json::as_str) {
        let v = env::var(n).map_err(|_| format!("phase references output `{n}' which is not set"))?;
        return Ok(escape_sed_repl(&v));
    }
    if let Some(n) = p.get("input").and_then(Json::as_str) {
        return Ok(escape_sed_repl(bindings.get(n).map(String::as_str).unwrap_or(n)));
    }
    if let Some(prog) = p.get("which").and_then(Json::as_str) {
        let abs = find_in_path(search_path, prog)
            .ok_or_else(|| format!("phase `which {prog}': not found in TD_INPUTS"))?;
        return Ok(escape_sed_repl(&abs));
    }
    Err(format!("unsupported replacement/part: {p:?}"))
}

/// A substitution's `to` → its sed-replacement text.
fn resolve_to(to: &Json, bindings: &BTreeMap<String, String>, search_path: &str) -> Result<String, String> {
    if let Some(parts) = to.get("stringAppend").and_then(Json::as_arr) {
        let mut o = String::new();
        for p in parts {
            o.push_str(&resolve_part(p, bindings, search_path)?);
        }
        return Ok(o);
    }
    if let Some(fmtargs) = to.get("format").and_then(Json::as_arr) {
        // (format #f FMT ARG…): substitute each `~a` in FMT with the next ARG.
        let fmt = fmtargs.first().and_then(Json::as_str).ok_or("format: missing format string")?;
        let mut o = String::new();
        let mut args = fmtargs[1..].iter();
        let mut rest = fmt;
        while let Some(pos) = rest.find("~a") {
            o.push_str(&escape_sed_repl(&rest[..pos]));
            let a = args.next().ok_or("format: too few arguments for ~a")?;
            o.push_str(&resolve_part(a, bindings, search_path)?);
            rest = &rest[pos + 2..];
        }
        o.push_str(&escape_sed_repl(rest));
        return Ok(o);
    }
    // string | {var} | {which} | {output} | {input}
    resolve_part(to, bindings, search_path)
}

/// Resolve a `substitute*` FILE argument to the concrete file paths to edit,
/// relative to the unpacked `srcdir`.
fn resolve_files(fa: &Json, srcdir: &str, search_path: &str) -> Result<Vec<PathBuf>, String> {
    if let Some(s) = fa.as_str() {
        return Ok(vec![Path::new(srcdir).join(s)]);
    }
    if let Some(list) = fa.get("list").and_then(Json::as_arr) {
        return list.iter()
            .map(|f| f.as_str().map(|s| Path::new(srcdir).join(s)).ok_or("file list entry is not a string".to_string()))
            .collect();
    }
    if let Some(ff) = fa.get("findFiles").and_then(Json::as_arr) {
        let dir = ff.first().and_then(Json::as_str).ok_or("findFiles: missing dir")?;
        let re = ff.get(1).and_then(Json::as_str).ok_or("findFiles: missing regex")?;
        return find_files(srcdir, dir, re, search_path);
    }
    if let Some(c) = fa.get("cons").and_then(Json::as_arr) {
        let mut v = resolve_files(c.first().ok_or("cons: missing head")?, srcdir, search_path)?;
        v.extend(resolve_files(c.get(1).ok_or("cons: missing tail")?, srcdir, search_path)?);
        return Ok(v);
    }
    Err(format!("unsupported substitute* file argument: {fa:?}"))
}

/// `(find-files DIR REGEX)` — files under `srcdir/DIR` whose BASENAME matches the
/// POSIX-ERE `regex` (`find` + `grep -E`, the toolchain's regex). Missing dir →
/// empty (these phases patch test files, absent in some trees — a no-op).
fn find_files(srcdir: &str, dir: &str, regex: &str, search_path: &str) -> Result<Vec<PathBuf>, String> {
    let full = Path::new(srcdir).join(dir);
    if !full.is_dir() {
        return Ok(Vec::new());
    }
    let bash = find_in_path(search_path, "bash").ok_or("bash not found for find-files")?;
    // List files; keep those whose basename matches the regex. Single-quote the
    // regex (the corpus find-files regexes contain none); PATH carries find/grep.
    // The match test is an `if` (not `grep && printf`): a NON-matching last file
    // would otherwise leave the `while` loop — and thus the pipeline — with grep's
    // exit 1, which `set -e` turns into a spurious "find-files failed" (gettext's
    // gettext-tools/tests dir, where most files don't match, hit exactly this).
    // `pipefail` keeps a genuine `find` failure fatal.
    let script = format!(
        "set -eo pipefail; export PATH={path}; find {full} -type f | while IFS= read -r p; do \
         if printf '%s\\n' \"${{p##*/}}\" | grep -qE -- '{regex}'; then printf '%s\\n' \"$p\"; fi; done",
        path = search_path,
        full = full.display(),
        regex = regex,
    );
    let outp = Command::new(&bash)
        .args(["-c", &script])
        .output()
        .map_err(|e| format!("find-files spawn: {e}"))?;
    if !outp.status.success() {
        return Err(format!("find-files in {} failed", full.display()));
    }
    Ok(String::from_utf8_lossy(&outp.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Apply one `substitute*` (file argument + clauses) via `sed -E -i`.
fn apply_substitute(
    fa: &Json,
    clauses: &[Json],
    srcdir: &str,
    sed: &str,
    bindings: &BTreeMap<String, String>,
    search_path: &str,
    envs: &[(String, String)],
) -> Result<(), String> {
    let files = resolve_files(fa, srcdir, search_path)?;
    // Build a sed `s` script per clause, with a control-char delimiter (\x01) the
    // corpus patterns never contain, so `/` in paths needs no escaping.
    let mut exprs: Vec<String> = Vec::new();
    for c in clauses {
        let from = c.get("from").and_then(Json::as_str).ok_or("clause: missing from")?;
        let to = resolve_to(c.get("to").ok_or("clause: missing to")?, bindings, search_path)?;
        exprs.push(format!("s\u{1}{from}\u{1}{to}\u{1}g"));
    }
    for f in &files {
        if !f.exists() {
            return Err(format!("substitute* target does not exist: {}", f.display()));
        }
        let mut args: Vec<String> = vec!["-E".into(), "-i".into()];
        for e in &exprs {
            args.push("-e".into());
            args.push(e.clone());
        }
        args.push(f.to_string_lossy().into_owned());
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        // Run from the build root: resolve_files / find_files yield paths already
        // joined to `srcdir` (which is relative to the build root, e.g.
        // `./gzip-1.14/gunzip.in`), so the cwd must be `.`, not `srcdir`.
        run_cmd(sed, &argrefs, ".", envs, &WATCH_PHASE)?;
    }
    Ok(())
}

/// Recurse a phase body, applying each statement. `let`-`which` extends the
/// bindings; `with-fluids` (byte-encoding) is transparent to `sed`.
fn apply_body(
    stmts: &[Json],
    srcdir: &str,
    sed: &str,
    bindings: &BTreeMap<String, String>,
    search_path: &str,
    envs: &[(String, String)],
) -> Result<(), String> {
    for s in stmts {
        if let Some(fa) = s.get("substitute") {
            let clauses = s.get("clauses").and_then(Json::as_arr).ok_or("substitute: no clauses")?;
            apply_substitute(fa, clauses, srcdir, sed, bindings, search_path, envs)?;
        } else if let Some(binds) = s.get("letWhich").and_then(Json::as_arr) {
            let mut extended = bindings.clone();
            for b in binds {
                let name = b.get("name").and_then(Json::as_str).ok_or("letWhich: no name")?;
                let prog = b.get("prog").and_then(Json::as_str).ok_or("letWhich: no prog")?;
                let abs = find_in_path(search_path, prog)
                    .ok_or_else(|| format!("letWhich `{prog}': not found in TD_INPUTS"))?;
                extended.insert(name.to_string(), abs);
            }
            let body = s.get("body").and_then(Json::as_arr).ok_or("letWhich: no body")?;
            apply_body(body, srcdir, sed, &extended, search_path, envs)?;
        } else if s.get("withDefaultPortEncodingFalse").map(Json::is_true).unwrap_or(false) {
            let body = s.get("body").and_then(Json::as_arr).ok_or("withFluids: no body")?;
            apply_body(body, srcdir, sed, bindings, search_path, envs)?;
        } else {
            return Err(format!("unsupported phase-body statement: {s:?}"));
        }
    }
    Ok(())
}

/// Apply the recipe's TD_PHASES (a JSON array of phases) in `srcdir`, after unpack.
fn apply_phases(srcdir: &str, search_path: &str, envs: &[(String, String)]) -> Result<(), String> {
    let spec = env::var("TD_PHASES").unwrap_or_default();
    if spec.trim().is_empty() {
        return Ok(());
    }
    let sed = find_in_path(search_path, "sed").ok_or("sed not found in TD_INPUTS")?;
    let j = crate::json::parse(&spec).map_err(|e| format!("TD_PHASES JSON: {e}"))?;
    let phases = j.as_arr().ok_or("TD_PHASES is not a JSON array")?;
    let bindings: BTreeMap<String, String> = BTreeMap::new();
    for phase in phases {
        let name = phase.get("name").and_then(Json::as_str).unwrap_or("<phase>");
        println!(">> td-build: phase `{name}' (td's own runner)");
        if let Some(body) = phase.get("body").and_then(Json::as_arr) {
            // Rich nested body (gettext-minimal et al.).
            apply_body(body, srcdir, &sed, &bindings, search_path, envs)?;
        } else if let Some(subs) = phase.get("substitutions").and_then(Json::as_arr) {
            // Flat form: each entry is a single-clause substitute* {file, from, to}.
            for sub in subs {
                let fa = sub.get("file").ok_or("substitution: missing file")?;
                let from = sub.get("from").cloned().ok_or("substitution: missing from")?;
                let to = sub.get("to").cloned().ok_or("substitution: missing to")?;
                let mut clause = std::collections::BTreeMap::new();
                clause.insert("from".to_string(), from);
                clause.insert("to".to_string(), to);
                apply_substitute(fa, &[Json::Obj(clause)], srcdir, &sed, &bindings, search_path, envs)?;
            }
        } else {
            return Err(format!("phase `{name}' has neither body nor substitutions"));
        }
    }
    Ok(())
}

pub fn run() -> Result<(), String> {
    let out = env::var("out").map_err(|_| "out not set".to_string())?;
    let src = env::var("TD_SRC").map_err(|_| "TD_SRC not set".to_string())?;
    let inputs = env::var("TD_INPUTS").unwrap_or_default();
    // TD_CONFIGURE_FLAGS is a JSON array of strings (may be empty/absent); each
    // element is ONE ./configure argument so flags with internal whitespace (e.g.
    // `CFLAGS=-O2 -g -Wno-incompatible-pointer-types`) survive intact.
    let configure_flags_json = env::var("TD_CONFIGURE_FLAGS").unwrap_or_default();
    let configure_flags: Vec<String> = if configure_flags_json.trim().is_empty() {
        Vec::new()
    } else {
        crate::json::parse(&configure_flags_json)
            .map_err(|e| format!("TD_CONFIGURE_FLAGS JSON: {e}"))?
            .as_arr()
            .ok_or("TD_CONFIGURE_FLAGS is not a JSON array")?
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    };

    // set-paths phase (in Rust): derive PATH / C_INCLUDE_PATH /
    // CPLUS_INCLUDE_PATH / LIBRARY_PATH from the inputs' bin/include/lib dirs.
    let (mut path, mut cinc, mut cxxinc, mut lib): (
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for p in inputs.split(':').filter(|s| !s.is_empty()) {
        let push_if_dir = |sub: &str, dst: &mut Vec<String>| {
            let d = format!("{p}/{sub}");
            if Path::new(&d).is_dir() {
                dst.push(d);
            }
        };
        push_if_dir("bin", &mut path);
        push_if_dir("include", &mut cinc);
        push_if_dir("lib", &mut lib);
        push_if_dir("lib64", &mut lib);
        // C++ search path: include/c++ then include.
        push_if_dir("include/c++", &mut cxxinc);
        push_if_dir("include", &mut cxxinc);
    }
    let path = path.join(":");

    let bash = find_in_path(&path, "bash").ok_or("bash not found in TD_INPUTS")?;
    let tar = find_in_path(&path, "tar").ok_or("tar not found in TD_INPUTS")?;
    let make = find_in_path(&path, "make").ok_or("make not found in TD_INPUTS")?;

    // The build environment, the same shape gnu-build-system sets up.
    let envs: Vec<(String, String)> = vec![
        ("out".into(), out.clone()),
        ("PATH".into(), path.clone()),
        ("C_INCLUDE_PATH".into(), cinc.join(":")),
        ("CPLUS_INCLUDE_PATH".into(), cxxinc.join(":")),
        ("LIBRARY_PATH".into(), lib.join(":")),
        // configure / make sub-shells use bash (no /bin/sh in the sandbox).
        ("CONFIG_SHELL".into(), bash.clone()),
        ("SHELL".into(), bash.clone()),
        ("SOURCE_DATE_EPOCH".into(), "1".into()),
        ("HOME".into(), "/homeless-shelter".into()),
    ];

    // unpack -> the single source tree.
    run_cmd(&tar, &["xf", &src], ".", &envs, &WATCH_PHASE)?;
    let srcdir = single_subdir(".")?;

    // patch-source-shebangs — rewrite `#!/bin/sh` build scripts to the seed bash
    // (no /bin/sh in the sandbox), the way gnu-build-system does, before anything
    // runs them.
    patch_shebangs(Path::new(&srcdir), &bash)?;

    // The recipe's custom PHASES (td's own runner) — gnu-build-system applies
    // these via Guile `#:phases`; here td applies them in Rust, after unpack.
    apply_phases(&srcdir, &path, &envs)?;

    // configure --prefix=$out [extra flags].
    let prefix = format!("--prefix={out}");
    let mut conf: Vec<&str> = vec!["./configure", &prefix];
    conf.extend(configure_flags.iter().map(String::as_str));
    run_cmd(&bash, &conf, &srcdir, &envs, &WATCH_CONFIGURE)?;

    // build + install. Pass SHELL=<bash> as a make OVERRIDE (not just env): make
    // launches recipe shells via the SHELL make-variable, defaulting to /bin/sh,
    // which does not exist in the sandbox (the `po/` install rules hit this). A
    // command-line assignment overrides the Makefile AND propagates to sub-makes.
    let shell = format!("SHELL={bash}");
    run_cmd(&make, &[&shell], &srcdir, &envs, &WATCH_PHASE)?;
    run_cmd(&make, &[&shell, "install"], &srcdir, &envs, &WATCH_PHASE)?;
    Ok(())
}

/// Collect the `(crate-file-path, name-version)` pairs to vendor, from TD_VENDOR_CRATES
/// (':'-joined `.crate` STORE paths — nv via the store-path basename) and/or TD_VENDOR_DIR
/// (an interned DIRECTORY of `*.crate` files — nv = the crate filename, so NO `/gnu/store`
/// path is needed; this is td's OWN guix-free crate set). Pure given the env strings + a
/// directory listing, so it is unit-testable.
fn collect_vendor_crates(
    vendor_crates: &str,
    vendor_dir: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for c in vendor_crates.split(':').filter(|s| !s.is_empty()) {
        let nv_crate = crate::store::name_from_store_path(c)
            .ok_or_else(|| format!("vendor crate not a store path: {c}"))?;
        let nv = nv_crate.strip_suffix(".crate").unwrap_or(&nv_crate).to_string();
        out.push((c.to_string(), nv));
    }
    if !vendor_dir.is_empty() {
        let mut entries: Vec<PathBuf> = fs::read_dir(vendor_dir)
            .map_err(|e| format!("read TD_VENDOR_DIR {vendor_dir}: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "crate").unwrap_or(false))
            .collect();
        entries.sort();
        for p in entries {
            let path = p
                .to_str()
                .ok_or_else(|| format!("non-utf8 crate path in {vendor_dir}"))?
                .to_string();
            let base = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("non-utf8 crate name in {vendor_dir}"))?;
            let nv = base.strip_suffix(".crate").unwrap_or(base).to_string();
            out.push((path, nv));
        }
    }
    Ok(out)
}

/// rust-build — td's OWN Rust/cargo build "system" (sibling of `run`, the
/// autotools runner). The REPLACEMENT for Guix's `cargo-build-system`: here the
/// build LOGIC is td's Rust; only the rustc/cargo/gcc seed is the external
/// toolchain (§5, retired last). Phases: set-paths -> materialize a WRITABLE
/// source tree (TD_SRC is a store DIRECTORY — e.g. self-hosting the builder — or
/// a source tarball) -> `cargo build --release --offline --frozen` (no network,
/// Cargo.lock honored) -> install the named bins to $out/bin.
///
/// Determinism (the durable repro oracle is `td-builder check`'s double-build):
/// SOURCE_DATE_EPOCH=1 plus `--remap-path-prefix` strip the (varying) build-dir
/// and CARGO_HOME absolute paths so the binary does not embed them; linking
/// through gcc-toolchain's gcc (Guix's ld-wrapper) injects the RUNPATH to the
/// toolchain libs, so the output runs on a guix system and both double-build runs
/// share the same RUNPATH.
///
/// Inputs (env, set by `system td-build`):
///   out          the output store path.
///   TD_SRC       the crate source (a store directory or a source tarball).
///   TD_INPUTS    ':'-joined input store paths (rustc, cargo, gcc-toolchain,
///                coreutils, bash) — their bin/ dirs build PATH, lib/ build
///                LIBRARY_PATH.
///   TD_RUST_BINS space-separated binary names to install into $out/bin.
///   TD_VENDOR_CRATES optional ':'-joined `.crate` STORE paths (the dependency closure
///                pinned by Cargo.lock; nv from the store-path basename). The guix-realized
///                FOD inputs.
///   TD_VENDOR_DIR optional path to a single interned DIRECTORY of `*.crate` files (nv =
///                the crate filename) — td's OWN guix-free crate set (td-feed-warmed +
///                interned by store-add-recursive; NO `/gnu/store` crate path). When either
///                is set, a cargo `vendored-sources` dir is assembled so `cargo build
///                --offline` resolves deps from it instead of the network; neither ⇒ a
///                dependency-free build (the self-host path).
pub fn run_rust() -> Result<(), String> {
    let out = env::var("out").map_err(|_| "out not set".to_string())?;
    let src = env::var("TD_SRC").map_err(|_| "TD_SRC not set".to_string())?;
    let inputs = env::var("TD_INPUTS").unwrap_or_default();
    let bins_spec = env::var("TD_RUST_BINS").map_err(|_| "TD_RUST_BINS not set".to_string())?;
    let bins: Vec<&str> = bins_spec.split_whitespace().collect();
    if bins.is_empty() {
        return Err("TD_RUST_BINS is empty (no binaries to install)".into());
    }

    // set-paths: PATH from inputs' bin/ dirs; LIBRARY_PATH from lib/lib64 so the
    // ld-wrapper finds (and RUNPATHs) the toolchain libs at link time; C_INCLUDE_PATH
    // from include/ so a crate's C build script (e.g. the crypto backend's `cc`
    // build) finds the seed's headers — incl. the kernel headers (linux/*.h).
    let mut path: Vec<String> = Vec::new();
    let mut lib: Vec<String> = Vec::new();
    let mut cinc: Vec<String> = Vec::new();
    for p in inputs.split(':').filter(|s| !s.is_empty()) {
        let bin = format!("{p}/bin");
        if Path::new(&bin).is_dir() {
            path.push(bin);
        }
        for sub in ["lib", "lib64"] {
            let d = format!("{p}/{sub}");
            if Path::new(&d).is_dir() {
                lib.push(d);
            }
        }
        let inc = format!("{p}/include");
        if Path::new(&inc).is_dir() {
            cinc.push(inc);
        }
    }
    let path = path.join(":");
    let cargo = find_in_path(&path, "cargo").ok_or("cargo not found in TD_INPUTS")?;
    find_in_path(&path, "rustc").ok_or("rustc not found in TD_INPUTS")?;
    let cp = find_in_path(&path, "cp").ok_or("cp not found in TD_INPUTS")?;
    let chmod = find_in_path(&path, "chmod").ok_or("chmod not found in TD_INPUTS")?;
    let gcc = find_in_path(&path, "gcc").ok_or("gcc not found in TD_INPUTS (linker)")?;
    // Optional C/C++ compiler for crates with C build scripts (the `cc` crate honors
    // CC/CXX). Absent for pure-Rust builds — harmless, since no C is compiled then.
    let gpp = find_in_path(&path, "g++");

    // Materialize a WRITABLE source tree (cargo writes target/). A store directory
    // (self-host) is copied; a tarball is unpacked, then its single subdir copied.
    let path_env = vec![("PATH".to_string(), path.clone())];
    let build_dir = "td-rust-build";
    if Path::new(&src).is_dir() {
        run_cmd(&cp, &["-aT", &src, build_dir], ".", &path_env, &WATCH_PHASE)?;
    } else {
        let tar = find_in_path(&path, "tar").ok_or("tar not found in TD_INPUTS")?;
        run_cmd(&tar, &["xf", &src], ".", &path_env, &WATCH_PHASE)?;
        let sub = single_subdir(".")?;
        run_cmd(&cp, &["-aT", &sub, build_dir], ".", &path_env, &WATCH_PHASE)?;
    }
    // store copies are read-only; make the tree writable for cargo's target/.
    run_cmd(&chmod, &["-R", "u+w", build_dir], ".", &path_env, &WATCH_PHASE)?;

    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    let build_abs = cwd.join(build_dir);
    let build_abs = build_abs.to_str().ok_or("non-utf8 build path")?.to_string();
    let cargo_home = cwd.join("td-cargo-home");
    let cargo_home = cargo_home.to_str().ok_or("non-utf8 cargo-home")?.to_string();
    // Reproducibility: remap the (varying) build dir + CARGO_HOME so file!()/debug
    // paths don't leak into the binary; link via gcc (ld-wrapper) so the output
    // gets a RUNPATH to the toolchain libs.
    let mut rustflags = format!(
        "--remap-path-prefix={build_abs}=/td-build --remap-path-prefix={cargo_home}=/td-cargo -Clinker={gcc}"
    );
    // Native /td/store toolchain (#258): the native gcc is a PLAIN gcc, NOT guix's ld-wrapper, so it
    // injects no interp/RUNPATH. When TD_RUST_STORE_INTERP is set the caller is linking against the
    // native /td/store toolchain — bake them explicitly (the #255 rustc-compile recipe): the dynamic
    // linker = the /td/store ld, a RUNPATH per TD_RUST_STORE_RPATH dir so the produced binary resolves
    // its libs (glibc, libgcc_s, libz) from /td/store at run time, and -B per TD_RUST_STORE_BDIR so the
    // native gcc finds the glibc crt/lib at link time. Unset ⇒ the guix ld-wrapper path, unchanged.
    if let Ok(interp) = env::var("TD_RUST_STORE_INTERP") {
        if !interp.is_empty() {
            rustflags.push_str(&format!(" -Clink-arg=-Wl,--dynamic-linker,{interp}"));
            for rp in env::var("TD_RUST_STORE_RPATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
                rustflags.push_str(&format!(" -Clink-arg=-Wl,-rpath,{rp}"));
            }
            for b in env::var("TD_RUST_STORE_BDIR").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
                rustflags.push_str(&format!(" -Clink-arg=-B{b}"));
            }
        }
    }
    let mut envs: Vec<(String, String)> = vec![
        ("out".into(), out.clone()),
        ("PATH".into(), path.clone()),
        ("LIBRARY_PATH".into(), lib.join(":")),
        ("C_INCLUDE_PATH".into(), cinc.join(":")),
        ("CPLUS_INCLUDE_PATH".into(), cinc.join(":")),
        ("CC".into(), gcc.clone()),
        ("HOME".into(), "/homeless-shelter".into()),
        ("CARGO_HOME".into(), cargo_home.clone()),
        ("SOURCE_DATE_EPOCH".into(), "1".into()),
        ("RUSTFLAGS".into(), rustflags),
    ];
    if let Some(gpp) = gpp {
        envs.push(("CXX".into(), gpp));
    }

    // vendored deps: if TD_VENDOR_CRATES is set, assemble a cargo `vendored-sources`
    // directory from each `.crate` (untar -> `<name>-<version>/`, plus a minimal
    // `.cargo-checksum.json` whose `package` is the crate's sha256 — cargo verifies
    // only that against Cargo.lock, not the per-file map) and point CARGO_HOME's
    // config at it, so `cargo build --offline` resolves deps from the vendor dir
    // instead of the network. Unset ⇒ the dependency-free self-host path, unchanged.
    fs::create_dir_all(&cargo_home).map_err(|e| format!("mkdir CARGO_HOME {cargo_home}: {e}"))?;
    let crate_files = collect_vendor_crates(
        &env::var("TD_VENDOR_CRATES").unwrap_or_default(),
        &env::var("TD_VENDOR_DIR").unwrap_or_default(),
    )?;
    if !crate_files.is_empty() {
        let tar = find_in_path(&path, "tar").ok_or("tar not found in TD_INPUTS (vendor)")?;
        let vendor_dir = cwd.join("td-rust-vendor");
        fs::create_dir_all(&vendor_dir).map_err(|e| format!("mkdir vendor: {e}"))?;
        let vendor_abs = vendor_dir.to_str().ok_or("non-utf8 vendor path")?.to_string();
        for (c, nv) in &crate_files {
            // a cargo `.crate` tarball unpacks to exactly the single `<name>-<version>/` dir.
            run_cmd(&tar, &["xf", c.as_str(), "-C", &vendor_abs], ".", &path_env, &WATCH_PHASE)?;
            let cdir = vendor_dir.join(nv);
            if !cdir.is_dir() {
                return Err(format!("crate {c} did not unpack to {}/", cdir.display()));
            }
            // cargo keys the vendored checksum on the crate's sha256 (= its
            // Cargo.lock checksum, = the fixed-output content hash).
            let bytes = fs::read(c).map_err(|e| format!("read crate {c}: {e}"))?;
            let mut h = crate::sha256::Sha256::new();
            h.update(&bytes);
            let sha = crate::sha256::to_base16(&h.finalize());
            fs::write(cdir.join(".cargo-checksum.json"), format!("{{\"files\":{{}},\"package\":\"{sha}\"}}"))
                .map_err(|e| format!("write checksum for {nv}: {e}"))?;
        }
        // CARGO_HOME config: replace crates-io with the assembled vendor dir.
        fs::write(
            format!("{cargo_home}/config.toml"),
            format!("[source.crates-io]\nreplace-with = \"vendored-sources\"\n[source.vendored-sources]\ndirectory = \"{vendor_abs}\"\n"),
        )
        .map_err(|e| format!("write cargo config: {e}"))?;
    }

    // build (offline, frozen, release) in the writable tree. Optional cargo feature
    // selection from the recipe: TD_CARGO_NO_DEFAULT=1 ⇒ --no-default-features (drop the
    // crate's defaults, e.g. a C-building jemalloc), TD_CARGO_FEATURES=a,b ⇒ --features a,b.
    // Absent ⇒ the plain default build, unchanged.
    let mut cargo_args: Vec<String> =
        ["build", "--release", "--offline", "--frozen"].iter().map(|s| s.to_string()).collect();
    if env::var("TD_CARGO_NO_DEFAULT").is_ok() {
        cargo_args.push("--no-default-features".into());
    }
    if let Ok(feats) = env::var("TD_CARGO_FEATURES") {
        if !feats.is_empty() {
            cargo_args.push("--features".into());
            cargo_args.push(feats);
        }
    }
    let cargo_argv: Vec<&str> = cargo_args.iter().map(String::as_str).collect();
    run_cmd(&cargo, &cargo_argv, build_dir, &envs, &WATCH_PHASE)?;

    // install the named binaries to $out/bin.
    let bindir = format!("{out}/bin");
    fs::create_dir_all(&bindir).map_err(|e| format!("mkdir {bindir}: {e}"))?;
    for b in &bins {
        let from = format!("{build_dir}/target/release/{b}");
        if !Path::new(&from).is_file() {
            return Err(format!("cargo did not produce expected binary `{b}' at {from}"));
        }
        run_cmd(&cp, &["-p", &from, &format!("{bindir}/{b}")], ".", &path_env, &WATCH_PHASE)?;
    }
    Ok(())
}

/// cmake-build — td's OWN minimal cmake build "system", in Rust (sibling of `run`,
/// the autotools runner; move-off-Guile §5). The REPLACEMENT for Guix's
/// `cmake-build-system`'s Guile phase runner: here the build LOGIC is td's Rust;
/// only cmake/gcc/make are the external Guix toolchain SEED (retired LAST, §5),
/// exactly as the autotools path uses make/gcc. It runs the standard cmake phases
/// directly, OUT OF SOURCE (cmake's idiom):
///
///   set-paths -> unpack -> configure (cmake <src> -DCMAKE_INSTALL_PREFIX=$out) ->
///   make -> make install
///
/// No Guile runs in the build. The environment is derived from the inputs the same
/// way `run`'s set-paths phase does (PATH / C_INCLUDE_PATH / CPLUS_INCLUDE_PATH /
/// LIBRARY_PATH from the inputs' bin/include/lib dirs).
///
/// Inputs (env, set by `build-recipe` via system td-build's derivation):
///   out                output prefix (the daemon sets this).
///   TD_SRC             the source (a source tarball, or a store DIRECTORY).
///   TD_INPUTS          ':'-joined store paths of the build inputs (cmake,
///                      gcc-toolchain, make, coreutils, bash, tar, gzip).
///   TD_CONFIGURE_FLAGS extra `cmake` flags as a JSON array of strings (may be
///                      empty/absent); each element is ONE argument, so a flag may
///                      carry internal whitespace, the same drv-safe encoding the
///                      autotools path uses.
///
/// Determinism: the configure pins CMAKE_BUILD_TYPE=Release and the build dir is a
/// fixed relative path, and SOURCE_DATE_EPOCH=1 / HOME=/homeless-shelter mirror the
/// autotools path — so `td-builder check`'s double-build (the durable repro oracle)
/// gets the same output both times.
pub fn run_cmake() -> Result<(), String> {
    let out = env::var("out").map_err(|_| "out not set".to_string())?;
    let src = env::var("TD_SRC").map_err(|_| "TD_SRC not set".to_string())?;
    let inputs = env::var("TD_INPUTS").unwrap_or_default();
    // Extra `cmake` flags as a JSON array of strings (may be empty/absent); each
    // element stays ONE cmake argument so a flag with internal whitespace survives.
    let configure_flags_json = env::var("TD_CONFIGURE_FLAGS").unwrap_or_default();
    let configure_flags: Vec<String> = if configure_flags_json.trim().is_empty() {
        Vec::new()
    } else {
        crate::json::parse(&configure_flags_json)
            .map_err(|e| format!("TD_CONFIGURE_FLAGS JSON: {e}"))?
            .as_arr()
            .ok_or("TD_CONFIGURE_FLAGS is not a JSON array")?
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    };

    // set-paths phase (in Rust): derive PATH / C_INCLUDE_PATH / CPLUS_INCLUDE_PATH /
    // LIBRARY_PATH from the inputs' bin/include/lib dirs (same as the autotools path).
    let (mut path, mut cinc, mut cxxinc, mut lib): (
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for p in inputs.split(':').filter(|s| !s.is_empty()) {
        let push_if_dir = |sub: &str, dst: &mut Vec<String>| {
            let d = format!("{p}/{sub}");
            if Path::new(&d).is_dir() {
                dst.push(d);
            }
        };
        push_if_dir("bin", &mut path);
        push_if_dir("include", &mut cinc);
        push_if_dir("lib", &mut lib);
        push_if_dir("lib64", &mut lib);
        push_if_dir("include/c++", &mut cxxinc);
        push_if_dir("include", &mut cxxinc);
    }
    let path = path.join(":");

    let bash = find_in_path(&path, "bash").ok_or("bash not found in TD_INPUTS")?;
    let cmake = find_in_path(&path, "cmake").ok_or("cmake not found in TD_INPUTS")?;
    let make = find_in_path(&path, "make").ok_or("make not found in TD_INPUTS")?;

    // The build environment, the same shape `run` (autotools) sets up.
    let envs: Vec<(String, String)> = vec![
        ("out".into(), out.clone()),
        ("PATH".into(), path.clone()),
        ("C_INCLUDE_PATH".into(), cinc.join(":")),
        ("CPLUS_INCLUDE_PATH".into(), cxxinc.join(":")),
        ("LIBRARY_PATH".into(), lib.join(":")),
        // cmake / make sub-shells use bash (no /bin/sh in the sandbox).
        ("CONFIG_SHELL".into(), bash.clone()),
        ("SHELL".into(), bash.clone()),
        ("SOURCE_DATE_EPOCH".into(), "1".into()),
        ("HOME".into(), "/homeless-shelter".into()),
    ];

    // unpack -> the single source tree. TD_SRC may be a store DIRECTORY (interned
    // tree) or a source tarball; resolve to an absolute srcdir either way.
    let srcdir = if Path::new(&src).is_dir() {
        // an absolute store path already; cmake reads it read-only (out-of-source).
        src.clone()
    } else {
        let tar = find_in_path(&path, "tar").ok_or("tar not found in TD_INPUTS")?;
        run_cmd(&tar, &["xf", &src], ".", &envs, &WATCH_PHASE)?;
        let rel = single_subdir(".")?;
        // make it absolute so the cmake invocation (run from the build dir) resolves it.
        let cwd = env::current_dir().map_err(|e| e.to_string())?;
        cwd.join(rel).to_string_lossy().into_owned()
    };

    // patch-source-shebangs — rewrite `#!/bin/sh` build scripts to the seed bash
    // (no /bin/sh in the sandbox), as `run` does. Skipped for a read-only store
    // source dir (an interned tree's store path is immutable; cmake reads it
    // out-of-source so there is nothing to patch in place).
    if !Path::new(&src).is_dir() {
        patch_shebangs(Path::new(&srcdir), &bash)?;
    }

    // configure: out-of-source. cmake <srcdir> -DCMAKE_INSTALL_PREFIX=$out from a
    // fresh build dir (cmake's idiom; keeps the source tree pristine).
    let build_dir = "td-cmake-build";
    fs::create_dir_all(build_dir).map_err(|e| format!("mkdir {build_dir}: {e}"))?;
    let prefix = format!("-DCMAKE_INSTALL_PREFIX={out}");
    let mut conf: Vec<&str> = vec![&srcdir, &prefix, "-DCMAKE_BUILD_TYPE=Release"];
    conf.extend(configure_flags.iter().map(String::as_str));
    run_cmd(&cmake, &conf, build_dir, &envs, &WATCH_CONFIGURE)?;

    // build + install. Pass SHELL=<bash> as a make OVERRIDE (not just env), as `run`
    // does: make launches recipe shells via the SHELL make-variable, defaulting to
    // /bin/sh, which does not exist in the sandbox.
    let shell = format!("SHELL={bash}");
    run_cmd(&make, &[&shell], build_dir, &envs, &WATCH_PHASE)?;
    run_cmd(&make, &[&shell, "install"], build_dir, &envs, &WATCH_PHASE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The test-process bash + a PATH env for the child (scripts use sleep etc.).
    fn bash_and_env() -> (String, Vec<(String, String)>) {
        let path = env::var("PATH").unwrap();
        let bash = find_in_path(&path, "bash").expect("bash on PATH");
        (bash, vec![("PATH".to_string(), path)])
    }

    #[test]
    fn watchdog_reds_a_configure_stuck_in_a_failing_tool_loop() {
        // The #292 shape (issue #308): a staged closure missing libgmp makes
        // every `expr` die with the SAME loader error, and configure's
        // "checking for grep that handles long lines" counter loop retries
        // forever at 100% CPU. Without the guard this run_cmd call never
        // returns (verified-red: with the trip neutered, this test hangs past
        // any bound); with it, the phase reds in seconds and the diagnostic
        // quotes the failing tool's stderr line.
        let (bash, envs) = bash_and_env();
        let loop_forever = "while :; do echo 'expr: error while loading shared libraries: \
                            libgmp.so.10: cannot open shared object file' >&2; done";
        let t0 = Instant::now();
        let err = run_cmd(
            &bash,
            &["-c", loop_forever],
            ".",
            &envs,
            &Watch { silence: Duration::ZERO, stderr_repeat_limit: 25 },
        )
        .expect_err("a persistently-failing tool loop must red");
        assert!(t0.elapsed() < Duration::from_secs(30), "must red promptly, not spin: {err}");
        assert!(err.contains("td-build watchdog KILLED"), "names the watchdog: {err}");
        assert!(err.contains("repeated 25x"), "names the repeat bound: {err}");
        assert!(
            err.contains("expr: error while loading shared libraries"),
            "quotes the failing tool's stderr: {err}"
        );
        assert!(err.contains("last stderr lines:"), "carries the stderr tail: {err}");
    }

    #[test]
    fn watchdog_reds_a_silently_wedged_phase() {
        // The silent variant: configure often sends a helper's stderr to
        // /dev/null or config.log, so the spin produces NO output. The silence
        // bound is the backstop; the whole process group dies (the exec'd
        // sleep included), so the test returns instead of waiting 300s.
        let (bash, envs) = bash_and_env();
        let t0 = Instant::now();
        let err = run_cmd(
            &bash,
            &["-c", "exec sleep 300"],
            ".",
            &envs,
            &Watch { silence: Duration::from_secs(1), stderr_repeat_limit: 0 },
        )
        .expect_err("a silent wedged phase must red");
        assert!(t0.elapsed() < Duration::from_secs(30), "must red at the bound: {err}");
        assert!(err.contains("no output for 1s"), "names the silence bound: {err}");
    }

    #[test]
    fn watchdog_passes_a_healthy_command_and_sub_limit_repeats() {
        // Green control: repeats BELOW the bound (24 identical stderr lines,
        // limit 25) exit green — the guard trips on the pathological loop, not
        // on a noisy-but-terminating tool. And a changing stderr stream resets
        // the counter, so far more total lines than the limit stay green.
        let (bash, envs) = bash_and_env();
        let noisy = "for i in $(seq 24); do echo 'same warning' >&2; done; echo done-ok";
        run_cmd(
            &bash,
            &["-c", noisy],
            ".",
            &envs,
            &Watch { silence: Duration::from_secs(600), stderr_repeat_limit: 25 },
        )
        .expect("sub-limit repeats must stay green");
        let alternating =
            "for i in $(seq 40); do echo \"warn $((i % 2))\" >&2; done; echo done-ok";
        run_cmd(
            &bash,
            &["-c", alternating],
            ".",
            &envs,
            &Watch { silence: Duration::from_secs(600), stderr_repeat_limit: 25 },
        )
        .expect("alternating stderr lines must reset the repeat counter");
    }

    #[test]
    fn watchdog_keeps_the_plain_failure_contract() {
        // A normal non-zero exit still reds with the pre-#308 message shape —
        // the supervisor changes HOW output is carried, not the pass/fail
        // contract.
        let (bash, envs) = bash_and_env();
        let err = run_cmd(
            &bash,
            &["-c", "echo out; echo err >&2; exit 3"],
            ".",
            &envs,
            &WATCH_PHASE,
        )
        .expect_err("exit 3 must red");
        assert!(err.contains("failed"), "plain failure message kept: {err}");
        run_cmd(&bash, &["-c", "true"], ".", &envs, &WATCH_PHASE).expect("true is green");
    }

    #[test]
    fn vendor_dir_collects_crates_by_filename_guix_free() {
        // TD_VENDOR_DIR: every *.crate is collected, nv = the filename (no /gnu/store path),
        // sorted, non-.crate files ignored — this is td's guix-free crate set.
        let tmp = std::env::temp_dir().join(format!("td-vendor-dir-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("aho-corasick-1.1.2.crate"), b"y").unwrap();
        fs::write(tmp.join("adler2-2.0.0.crate"), b"x").unwrap();
        fs::write(tmp.join("README.txt"), b"ignored").unwrap();
        let got = collect_vendor_crates("", tmp.to_str().unwrap()).unwrap();
        let nvs: Vec<&str> = got.iter().map(|(_, nv)| nv.as_str()).collect();
        assert_eq!(nvs, vec!["adler2-2.0.0", "aho-corasick-1.1.2"]);
        // the collected path is the real crate file (so vendoring can untar + sha it).
        assert!(got.iter().all(|(p, _)| p.ends_with(".crate") && Path::new(p).exists()));
        // neither source set ⇒ empty (the dependency-free self-host path).
        assert!(collect_vendor_crates("", "").unwrap().is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn patch_shebangs_rewrites_only_bin_sh_bash_keeping_exec_and_args() {
        let base = std::env::temp_dir().join(format!("td-shebang-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        let bash = "/gnu/store/zzz-bash-5.2.37/bin/bash";

        // `#!/bin/sh -e` with the exec bit: rewritten to the seed bash, keeping
        // the trailing args AND the exec bit (install-sh is run as a program).
        let sh = base.join("install-sh");
        fs::write(&sh, b"#!/bin/sh -e\necho install\n").unwrap();
        fs::set_permissions(&sh, fs::Permissions::from_mode(0o755)).unwrap();
        // Pin an OLD mtime: the rewrite must preserve it (else autotools sees
        // generated files as stale and runs aclocal — absent — failing 127).
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&sh)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(old).set_modified(old))
            .unwrap();
        // `#! /bin/bash` (leading space) -> rewritten.
        let bsh = base.join("sub/cfg");
        fs::write(&bsh, b"#! /bin/bash\nexit 0\n").unwrap();
        // already a store interpreter -> untouched.
        let store = base.join("already");
        let store_orig = format!("#!{bash}\nx\n");
        fs::write(&store, store_orig.as_bytes()).unwrap();
        // a non-shell interpreter -> untouched.
        let perl = base.join("p.pl");
        fs::write(&perl, b"#!/usr/bin/perl\nprint 1;\n").unwrap();
        // not a script -> untouched (and not slurped as text).
        let data = base.join("data");
        fs::write(&data, b"\x7fELF\x00bytes").unwrap();

        patch_shebangs(&base, bash).unwrap();

        assert_eq!(fs::read_to_string(&sh).unwrap(), format!("#!{bash} -e\necho install\n"));
        assert_eq!(fs::metadata(&sh).unwrap().permissions().mode() & 0o111, 0o111);
        assert_eq!(fs::metadata(&sh).unwrap().modified().unwrap(), old, "mtime preserved");
        assert_eq!(fs::read_to_string(&bsh).unwrap(), format!("#!{bash}\nexit 0\n"));
        assert_eq!(fs::read_to_string(&store).unwrap(), store_orig);
        assert_eq!(fs::read_to_string(&perl).unwrap(), "#!/usr/bin/perl\nprint 1;\n");
        assert_eq!(fs::read(&data).unwrap(), b"\x7fELF\x00bytes");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn patch_shebangs_rewrites_a_read_only_script_and_restores_its_mode() {
        // less's mkinstalldirs ships 0444 — fs::write would EACCES. The rewrite
        // must succeed (grant write temporarily) and leave the original mode.
        let base = std::env::temp_dir().join(format!("td-shebang-ro-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let bash = "/gnu/store/zzz-bash-5.2.37/bin/bash";

        let ro = base.join("mkinstalldirs");
        fs::write(&ro, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o444)).unwrap();

        patch_shebangs(&base, bash).unwrap();

        assert_eq!(fs::read_to_string(&ro).unwrap(), format!("#!{bash}\nexit 0\n"));
        assert_eq!(
            fs::metadata(&ro).unwrap().permissions().mode() & 0o777,
            0o444,
            "original read-only mode restored"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
