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
//! Every phase command run through `run_cmd` sits under a fail-fast watchdog
//! (#308, #339) — see `Watch`: a broken staged closure must red in minutes with
//! a named tool, never spin — whether the spin is the top-level configure or a
//! chatty sub-`./configure` nested inside a `make` phase. (`find_files`' short
//! bash probe is the one subprocess outside it.)
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

use crate::json::Json;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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

fn require_executable_file(path: &str, label: &str) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| format!("stat {label} {path}: {e}"))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{label} is not an executable file: {path}"));
    }
    Ok(())
}

/// patch-source-shebangs (in Rust) — gnu-build-system rewrites `#!/bin/sh` (and
/// friends) across the unpacked tree to a real interpreter, because the pure
/// build sandbox has no /bin/sh. td does the same: any file whose shebang names
/// an absolute `sh`/`bash` NOT already under the active store is rewritten to the seed
/// bash (sh-compatible). This is what lets a package's OWN build scripts execute
/// in the sandbox — e.g. gawk's `build-aux/install-sh`, run directly by its
/// install rule, whose `#!/bin/sh` would otherwise fail with "required file not
/// found". Deterministic (the bash path is pinned), so it stays reproducible.
fn patch_shebangs(dir: &Path, bash: &str) -> Result<(), String> {
    // The active-store prefix, computed once for the whole tree walk (not per file).
    let store_prefix = format!("{}/", crate::store::store_dir());
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
                patch_one_shebang(&entry.path(), bash, &store_prefix)?;
            }
        }
    }
    Ok(())
}

/// Rewrite one file's shebang iff it names an absolute sh/bash outside the store
/// (`store_prefix` — the slash-terminated active store dir, hoisted by the caller).
/// Peeks two bytes first, so non-scripts (incl. big binaries) are not slurped.
fn patch_one_shebang(path: &Path, bash: &str, store_prefix: &str) -> Result<(), String> {
    use std::io::Read;
    let mut head = [0u8; 2];
    match fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(2) if &head == b"#!" => {}
        _ => return Ok(()), // unreadable, empty, or not a script — leave it
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let nl = bytes.iter().position(|&b| b == b'\n').unwrap_or(bytes.len());
    let line = match std::str::from_utf8(bytes.get(..nl).unwrap_or_default()) {
        Ok(s) => s,
        Err(_) => return Ok(()), // binary first line — skip
    };
    // "#!  /bin/sh -e"  ->  interp="/bin/sh", trailing=" -e"
    let after = line.get(2..).unwrap_or_default().trim_start();
    let mut it = after.splitn(2, char::is_whitespace);
    let interp = it.next().unwrap_or("");
    let trailing = it.next().map(|s| format!(" {s}")).unwrap_or_default();
    if !interp.starts_with('/') || interp.starts_with(store_prefix) {
        return Ok(()); // relative, or already an active-store interpreter
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
    out.extend_from_slice(bytes.get(nl..).unwrap_or_default());
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
/// heavy-gate slot. `run_cmd` supervises every phase command it runs; zero
/// disables a bound:
///
///   * `repeat_limit` — a COUNT bound: while the command still RUNS, the SAME
///     line this many times in a row on one stream is a persistently-failing
///     tool in a retry loop; the group is killed and the phase reds, the
///     repeated line naming the failing tool. Only ever kills a running command:
///     a command that terminates on its own is judged by its exit status, so a
///     self-limiting spammer is never falsely killed. Set ONLY for configure —
///     healthy non-configure tools can repeat a line per work item (`tar xf`
///     prints the identical "Ignoring unknown extended header keyword" warning
///     per pax member), while a healthy configure never emits hundreds of
///     identical consecutive lines.
///   * `repeat_secs` — a DURATION bound for the same-line spin, robust to the
///     high-volume output that rules `repeat_limit` out of the `make` phase.
///     It trips only when the identical line is STILL ARRIVING after this much
///     wall-clock (the run of consecutive identical lines has lasted the window),
///     which distinguishes a chatty spin from legit high-volume output: a healthy
///     phase PROGRESSES (different lines reset the run) or COMPLETES (the burst
///     ends) long before the window — `tar xf` finishes, a verbose `make` prints
///     varied lines — whereas a broken tool keeps emitting the one line forever.
///     This closes the #339 residual: a #292-shape spin nested INSIDE a `make`
///     phase (a bundled sub-`./configure` the Makefile re-runs) that spins
///     CHATTILY resets the silence clock on every line, so only this bound — not
///     the count bound (off for `make`) nor the silence bound — catches it.
///   * `silence` — no output on either stream for this long, while the command
///     still runs, is a wedged phase: the backstop for a spin whose tool
///     stderr configure redirects away (conftest stderr usually goes to
///     /dev/null or config.log), and the bound for a SILENT `make`-phase wedge
///     (the chatty one is `repeat_secs`'s job).
///   * `drain_grace` — once the command has EXITED, how long a leftover
///     background process may keep the output pipes open before the phase's
///     process group is killed (and, `DRAIN_EXTRA` later, the drain abandoned
///     so a group-escaped holder cannot wedge the build). Wall-clock from the
///     exit, deliberately NOT activity-based: a chatty straggler must not
///     extend it. Always active (unlike silence/repeat_limit, a zero here does
///     NOT disable it — an unbounded drain is the hang we are removing). The
///     command's own exit status decides pass/fail — a green exit stays green.
///
/// The bounds are compiled in per phase, not env knobs: the sandbox clears the
/// builder's env, and a drv-env knob would vary the drv hash with a tuning
/// value. Tests pass their own tiny bounds.
struct Watch {
    silence: Duration,
    repeat_limit: u32,
    repeat_secs: Duration,
    drain_grace: Duration,
}

/// Default phase bound: make can legitimately be silent for a long time while
/// one big translation unit compiles. 4 hours is comfortably past the corpus'
/// worst single-file case: GCC 14.3.0's final codegen builds machine-generated,
/// multi-megabyte translation units (insn-recog.cc, insn-automata.cc,
/// insn-dfatab.cc, insn-latencytab.cc) that emit NOTHING for the whole compile,
/// and under the slow gcc-mesboot 4.9.4 bootstrap compiler on a loaded shared
/// box a single one of those can stay silent well past half an hour — the old
/// 30-minute bound false-killed it as "wedged" (~36 min observed). The
/// /td/store bootstrap chain does NOT run through run_cmd — bootstrap.rs /
/// toolchain_x86_64.rs have their own runners — so this bound governs the
/// generic recipe phases. This backstop is DELIBERATELY generous because it is
/// the ONLY guard that fires on a *silent* wedge; the chatty/spin wedges have
/// their own tight bounds that this change does NOT touch (WATCH_CONFIGURE's
/// 600s catches the #292 configure spin; the `repeat_secs` 300s below catches
/// the #339 nested-make chatty spin) — both still red within minutes, so only a
/// genuinely output-free hang waits out the 4 hours. No COUNT repeat bound
/// (`tar xf` repeats a warning per member); the `repeat_secs` DURATION bound
/// (5 min of the same line still arriving) is the #339 make-nested chatty-spin
/// catch — comfortably above any real burst (a tar of a huge tarball finishes
/// in a minute or two, its warning does not keep arriving for five straight
/// minutes). Kept a COMPILED-IN constant, not an env knob (the sandbox clears
/// the builder's env and a drv-env knob would poison the drv hash — see the
/// Watch doc); re-tuning it is now cache-safe because the reuse key binds the
/// builder's ABI identity, not its ELF bytes (see reuse_key_manifest_digest),
/// so an output-neutral bump like this one does not invalidate the world.
const WATCH_PHASE: Watch = Watch {
    silence: Duration::from_secs(14400),
    repeat_limit: 0,
    repeat_secs: Duration::from_secs(300),
    drain_grace: Duration::from_secs(15),
};

/// configure bound: each configure check compiles+links a conftest in seconds,
/// so ten silent minutes means wedged — this is what turns the #292 class of
/// hang into a red "within minutes" even when the loop is silent. The fast COUNT
/// bound catches the chatty top-level configure spin; a healthy configure never
/// emits 200 identical lines in a row, so no duration bound is needed here.
const WATCH_CONFIGURE: Watch = Watch {
    silence: Duration::from_secs(600),
    repeat_limit: 200,
    repeat_secs: Duration::from_secs(0),
    drain_grace: Duration::from_secs(15),
};

/// After a drain-phase group kill, how long before the drain is abandoned
/// (a holder that survived the SIGKILL left the process group or is stuck in
/// the kernel; the abandoned reader threads exit with the builder process).
const DRAIN_EXTRA: Duration = Duration::from_secs(5);

/// `1500` → `1500ms`, `1000` → `1s` (sub-second test bounds must not print 0s).
fn fmt_ms(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Clip one raw output line to a printable diagnostic fragment.
fn clip_line(line: &[u8]) -> String {
    const MAX: usize = 400;
    let head = line.get(..MAX.min(line.len())).unwrap_or(line);
    let ell = if line.len() > MAX { "…" } else { "" };
    format!("{}{ell}", String::from_utf8_lossy(head))
}

/// One stream's line accountant: repeat counting + the diagnostic tail.
struct StreamWatch {
    last_line: Vec<u8>,
    repeats: u32,
    /// ms (relative to the supervise `start`) when the CURRENT run of consecutive
    /// identical lines began — reset whenever the line changes. Feeds the
    /// `repeat_secs` duration bound: `now - run_start_ms` is how long the same
    /// line has been arriving without interruption.
    run_start_ms: u64,
    /// Last few DISTINCT lines, clipped, for the kill diagnostic (a repeat is
    /// already quoted by the trip reason; duplicating it 5x buries context).
    tail: std::collections::VecDeque<String>,
}

impl StreamWatch {
    fn new() -> Self {
        StreamWatch {
            last_line: Vec::new(),
            repeats: 0,
            run_start_ms: 0,
            tail: std::collections::VecDeque::new(),
        }
    }
}

/// State shared between the two reader threads and run_cmd's poll loop.
struct Supervise {
    start: Instant,
    /// ms since `start` of the last read from EITHER stream (silence clock).
    last_activity_ms: AtomicU64,
    /// The first trip reason wins; the poll loop kills and reds on it.
    why: Mutex<Option<String>>,
    out_done: AtomicBool,
    err_done: AtomicBool,
    out_watch: Mutex<StreamWatch>,
    err_watch: Mutex<StreamWatch>,
}

/// Record the first trip reason; later reasons lose (the poll loop kills on it).
fn record_why(why: &Mutex<Option<String>>, reason: impl FnOnce() -> String) {
    if let Ok(mut w) = why.lock() {
        if w.is_none() {
            *w = Some(reason());
        }
    }
}

/// Account one complete line: repeat counting + the distinct-line tail, then the
/// two same-line spin bounds. `now_ms` is ms since the supervise `start` (the
/// time this line arrived). A trip reason is recorded (once); the poll loop does
/// the killing, so kill and reap stay ordered in one thread and a stale pgid is
/// never signalled.
///
///   * COUNT bound (`count_limit`, configure): `count_limit` identical lines in
///     a row. Fast — a healthy configure never emits that many.
///   * DURATION bound (`repeat_ms`, `make` phase): the identical line is STILL
///     arriving `repeat_ms` after the run began. Robust to legit high-volume
///     output (`tar xf`'s per-member warning) because that COMPLETES — the line
///     stops arriving — long before the window; only a real spin keeps the one
///     line coming for the whole duration. `repeats >= 2` gates out a lone line
///     (a single line that then goes silent is the silence bound's job).
///
/// `keep_tail` records the distinct-line diagnostic tail; only the stderr
/// watcher's tail is ever read (it feeds the kill diagnostic), so stdout passes
/// `false` to skip the per-line `clip_line` allocation on a verbose build.
fn account_line(
    st: &mut StreamWatch,
    line: &[u8],
    count_limit: u32,
    repeat_ms: u64,
    now_ms: u64,
    keep_tail: bool,
    stream: &str,
    why: &Mutex<Option<String>>,
) {
    // `repeats == 0` is the initial state (no line accounted yet); force the
    // first line down the run-start path so its `run_start_ms` is seeded — an
    // empty first line must not be mistaken for a repeat of the empty sentinel.
    if st.repeats > 0 && line == st.last_line.as_slice() {
        st.repeats = st.repeats.saturating_add(1);
    } else {
        st.last_line.clear();
        st.last_line.extend_from_slice(line);
        st.repeats = 1;
        st.run_start_ms = now_ms;
        if keep_tail {
            if st.tail.len() >= 5 {
                st.tail.pop_front();
            }
            st.tail.push_back(clip_line(line));
        }
    }
    if count_limit > 0 && st.repeats >= count_limit {
        let repeats = st.repeats;
        record_why(why, || {
            format!(
                "the same {stream} line repeated {repeats}x (a persistently-failing tool in a retry loop): {}",
                clip_line(line)
            )
        });
        return;
    }
    if repeat_ms > 0 && st.repeats >= 2 && now_ms.saturating_sub(st.run_start_ms) >= repeat_ms {
        let repeats = st.repeats;
        record_why(why, || {
            format!(
                "the same {stream} line kept arriving for {} ({repeats}x — a chatty spin, likely a persistently-failing tool in a make-nested retry loop): {}",
                fmt_ms(repeat_ms),
                clip_line(line)
            )
        });
    }
}

/// Tee one child stream to `sink`, updating the shared activity clock; when
/// `watch` is set, also split into lines for the repeat accountant (stderr
/// always — its tail feeds the silence-kill diagnostic — and stdout too when
/// either repeat bound is set, so a retry spin printing to stdout cannot escape
/// the watchdog by resetting the silence clock). `watch` carries `(state,
/// count_limit, repeat_ms, keep_tail, stream)`. Chunk-based (not read_until): a
/// `\r`-progress stream with no newline still counts as activity, and an
/// unterminated line cannot grow unboundedly.
fn tee_stream(
    mut src: impl std::io::Read,
    mut sink: impl std::io::Write,
    sup: &Supervise,
    watch: Option<(&Mutex<StreamWatch>, u32, u64, bool, &str)>,
) {
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        // Activity FIRST: the read itself proves the child is alive. The sink
        // write below can block (a stalled log consumer) and must not freeze
        // the silence clock while output is in fact arriving.
        let elapsed = u64::try_from(sup.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        sup.last_activity_ms.store(elapsed, Ordering::Relaxed);
        let chunk = buf.get(..n).unwrap_or(&buf);
        let _ = sink.write_all(chunk);
        let _ = sink.flush();
        if let Some((watch, count_limit, repeat_ms, keep_tail, stream)) = watch {
            if let Ok(mut st) = watch.lock() {
                // Linear scan of the chunk; only a trailing partial line is
                // carried over (no per-line allocation, no re-scan). `elapsed`
                // (the chunk's read time) is the arrival clock for the run.
                let mut rest = chunk;
                while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
                    let line = rest.get(..nl).unwrap_or_default();
                    if pending.is_empty() {
                        account_line(&mut st, line, count_limit, repeat_ms, elapsed, keep_tail, stream, &sup.why);
                    } else {
                        pending.extend_from_slice(line);
                        account_line(&mut st, &pending, count_limit, repeat_ms, elapsed, keep_tail, stream, &sup.why);
                        pending.clear();
                    }
                    rest = rest.get(nl.saturating_add(1)..).unwrap_or_default();
                }
                pending.extend_from_slice(rest);
                // A pathological unterminated "line" counts as activity only:
                // identical 64 KiB slices of a newline-free progress stream
                // must not masquerade as a retry loop.
                if pending.len() > 65536 {
                    pending.clear();
                }
            }
        }
    }
}

/// A phase-watched run_cmd for sibling modules (mes_boot): same supervision
/// as every mesboot Run step, without exporting the Watch type.
pub(crate) fn run_cmd_phase(
    prog: &str,
    args: &[&str],
    cwd: &str,
    envs: &[(String, String)],
) -> Result<(), String> {
    run_cmd(prog, args, cwd, envs, &WATCH_PHASE)
}

/// Run a command with a CLEAN environment (`envs` only), in `cwd`, echoing it to
/// the build log. Fail-closed: a non-zero exit aborts the build. Supervised by
/// `watch` (#308): the child runs in its OWN process group with stdout/stderr
/// teed to the build log; a tripped bound SIGKILLs the whole group and reds the
/// phase with the last stderr lines — a broken tool loop in configure becomes a
/// diagnosable red in minutes, not a 30-minute spin. The supervision loop runs
/// on the calling thread with detached readers, so run_cmd's return is bounded
/// in EVERY case (trip, wedge, straggler, group-escaped pipe holder); only the
/// child's recorded exit status decides pass/fail once it has exited.
///
/// Process-group note: the new group makes the trip kill atomic (configure's
/// transient children included). The gate runner's aggregate RSS sampler walks
/// descendants across this group boundary; the per-process RLIMIT_DATA cap remains a
/// local backstop for each compiler.
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

    let sup = std::sync::Arc::new(Supervise {
        start: Instant::now(),
        last_activity_ms: AtomicU64::new(0),
        why: Mutex::new(None),
        out_done: AtomicBool::new(false),
        err_done: AtomicBool::new(false),
        out_watch: Mutex::new(StreamWatch::new()),
        err_watch: Mutex::new(StreamWatch::new()),
    });
    let count_limit = watch.repeat_limit;
    let repeat_ms = u64::try_from(watch.repeat_secs.as_millis()).unwrap_or(u64::MAX);
    let out_reader = {
        let sup = std::sync::Arc::clone(&sup);
        std::thread::spawn(move || {
            // stdout is line-watched under EITHER repeat bound (the count bound
            // for configure, the duration bound for the make phase) — a chatty
            // spin printing to stdout must not escape by resetting the silence
            // clock. No tail kept: only err_watch.tail feeds the diagnostic.
            let w = if count_limit > 0 || repeat_ms > 0 {
                Some((&sup.out_watch, count_limit, repeat_ms, false, "stdout"))
            } else {
                None
            };
            tee_stream(child_out, std::io::stdout(), &sup, w);
            sup.out_done.store(true, Ordering::Relaxed);
        })
    };
    let err_reader = {
        let sup = std::sync::Arc::clone(&sup);
        std::thread::spawn(move || {
            // stderr is always line-watched: its tail feeds every diagnostic.
            tee_stream(
                child_err,
                std::io::stderr(),
                &sup,
                Some((&sup.err_watch, count_limit, repeat_ms, true, "stderr")),
            );
            sup.err_done.store(true, Ordering::Relaxed);
        })
    };

    // The supervision loop, on this thread. It kills ONLY a command that is
    // still RUNNING: a bound trips → SIGKILL the group → red. A command that
    // TERMINATES on its own is judged by its exit status alone — the repeat/
    // silence reasons a reader may have recorded while draining its buffered
    // output are ignored (so a self-terminating spammer, or a straggler noisy
    // during the drain, never overrides a real exit). Kill ordering vs reap:
    // the pre-exit kill fires while the un-reaped leader pins the pgid, so it
    // cannot hit a recycled group; the one post-reap kill (drain phase, below)
    // fires only while a pipe holder is still alive and carries the same
    // low-probability recycled-pgid caveat the gate runner's kill already does.
    let silence_ms = u64::try_from(watch.silence.as_millis()).unwrap_or(u64::MAX);
    let tick = Duration::from_millis(25);
    let mut exit: Option<std::process::ExitStatus> = None;
    let mut exited_at = sup.start;
    let mut killed = false;
    let mut killed_at = sup.start;
    let mut drain_killed = false;
    let mut abandoned = false;
    loop {
        if exit.is_none() {
            match child.try_wait() {
                Ok(Some(st)) => {
                    exit = Some(st);
                    exited_at = Instant::now();
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = crate::sys::kill_recorded(
                        crate::sys::KillTarget::Group(pgid),
                        crate::sys::SIGKILL,
                        &format!("waiting for `{prog}` failed: {e} (td-build watchdog)"),
                    );
                    let _ = child.wait();
                    return Err(format!("wait {prog}: {e}"));
                }
            }
        }
        let drained =
            sup.out_done.load(Ordering::Relaxed) && sup.err_done.load(Ordering::Relaxed);
        match exit {
            Some(_) if drained => break,
            Some(_) => {
                // Drain phase: the command has exited but something still
                // holds its output pipes (a leftover background process).
                let dt = exited_at.elapsed();
                if !drain_killed && dt > watch.drain_grace {
                    eprintln!(
                        "td-build watchdog: `{prog}` exited but a leftover background process \
                         still holds its output pipes after {}s — killing the phase's process group",
                        watch.drain_grace.as_secs()
                    );
                    let _ = crate::sys::kill_recorded(
                        crate::sys::KillTarget::Group(pgid),
                        crate::sys::SIGKILL,
                        &format!(
                            "`{prog}` exited but a leftover background process still held its \
                             output pipes after {}s (td-build watchdog)",
                            watch.drain_grace.as_secs()
                        ),
                    );
                    drain_killed = true;
                } else if drain_killed && dt > watch.drain_grace.saturating_add(DRAIN_EXTRA) {
                    eprintln!(
                        "td-build watchdog: abandoning the output drain of `{prog}` \
                         (a pipe holder survived the group kill)"
                    );
                    abandoned = true;
                    break;
                }
            }
            None if killed => {
                // A kill was issued but the child has not reaped yet. Bound
                // this too: a child wedged in uninterruptible (D-state) sleep
                // would otherwise never reap and spin the loop forever — the
                // pre-exit analog of the drain-abandon path. The un-reaped
                // leader still pins the pgid, so nothing recycled was signalled.
                if killed_at.elapsed() > DRAIN_EXTRA {
                    abandoned = true;
                    break;
                }
            }
            None => {
                if silence_ms > 0 {
                    let elapsed =
                        u64::try_from(sup.start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let last = sup.last_activity_ms.load(Ordering::Relaxed);
                    if elapsed.saturating_sub(last) > silence_ms {
                        if let Ok(mut w) = sup.why.lock() {
                            if w.is_none() {
                                *w = Some(format!(
                                    "no output for {} (a wedged phase)",
                                    fmt_ms(silence_ms)
                                ));
                            }
                        }
                    }
                }
                let why = sup.why.lock().ok().and_then(|w| w.clone());
                if let Some(why) = why {
                    let _ = crate::sys::kill_recorded(
                        crate::sys::KillTarget::Group(pgid),
                        crate::sys::SIGKILL,
                        &format!("`{prog}`: {why} (td-build watchdog)"),
                    );
                    killed = true;
                    killed_at = Instant::now();
                }
            }
        }
        std::thread::sleep(tick);
    }
    if !abandoned {
        let _ = out_reader.join();
        let _ = err_reader.join();
    }

    // A watchdog error is reported ONLY when the loop actually killed a running
    // command. `why` recorded without a kill (buffered spam from a command that
    // then exited on its own, or a straggler noisy during the drain) is dropped
    // — the command's own exit status decides.
    if killed {
        let why = sup
            .why
            .lock()
            .ok()
            .and_then(|mut w| w.take())
            .unwrap_or_else(|| "killed by the phase watchdog".to_string());
        let tail = sup
            .err_watch
            .lock()
            .map(|st| st.tail.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let tail =
            if tail.is_empty() { String::new() } else { format!("; last stderr lines:\n{tail}") };
        return Err(format!(
            "td-build watchdog KILLED `{prog} {}` after {}s — {why}{tail}",
            args.join(" "),
            sup.start.elapsed().as_secs(),
        ));
    }
    match exit {
        Some(st) if st.success() => Ok(()),
        Some(st) => Err(format!("{prog} {} failed: {st}", args.join(" "))),
        None => Err(format!("{prog}: exit status lost (supervision bug)")),
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

use std::collections::{BTreeMap, BTreeSet};

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
        let mut args = fmtargs.get(1..).unwrap_or_default().iter();
        let mut rest = fmt;
        while let Some(pos) = rest.find("~a") {
            o.push_str(&escape_sed_repl(rest.get(..pos).unwrap_or_default()));
            let a = args.next().ok_or("format: too few arguments for ~a")?;
            o.push_str(&resolve_part(a, bindings, search_path)?);
            rest = rest.get(pos + 2..).unwrap_or_default();
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
                let clause = vec![("from".to_string(), from), ("to".to_string(), to)];
                apply_substitute(fa, &[Json::Obj(clause)], srcdir, &sed, &bindings, search_path, envs)?;
            }
        } else {
            return Err(format!("phase `{name}' has neither body nor substitutions"));
        }
    }
    Ok(())
}

/// Bounded, shallow-first search for `config.log` files under `root` (the
/// top-level configure's log first, then any AC_CONFIG_SUBDIRS sub-configures).
/// Returns at most `max` paths and stops walking once `max` are found; depth-
/// capped so a pathological tree cannot make the failure path walk forever.
/// Symlinked directories are not descended (a symlink reports `is_dir() == false`
/// via `file_type`), so a self-referential source tree cannot loop.
fn find_config_logs(root: &Path, max: usize) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 4;
    let mut found: Vec<PathBuf> = Vec::new();
    // BFS by (dir, depth): shallow dirs are visited first, so the top-level
    // config.log — the one a gnulib probe like socklen_t writes — leads.
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        if found.len() >= max {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let path = entry.path();
            if ft.is_dir() {
                if depth < MAX_DEPTH {
                    subdirs.push(path);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some("config.log") {
                found.push(path);
                if found.len() >= max {
                    return found;
                }
            }
        }
        subdirs.sort();
        for s in subdirs {
            queue.push_back((s, depth.saturating_add(1)));
        }
    }
    found
}

/// autoconf writes every conftest compile/link invocation and its stderr to
/// `config.log`, NOT to the terminal — configure prints only a summary ("cannot
/// find a type to use in place of socklen_t"), while the REAL cause (a conftest
/// the memory watchdog / kernel OOM killer took under load, a header that failed
/// to stage, a miscompiled probe) sits in config.log. So on a `./configure`
/// failure, surface the tail of every config.log under the build tree: the failing
/// probe becomes diagnosable from the gate log alone, turning a bare flaky red into
/// evidence (issue #366). Bounded — at most `MAX_LOGS` logs, shallowest first,
/// `TAIL_LINES` lines each — so a tree of sub-configures cannot flood the log.
/// Best-effort: an unreadable or absent log is skipped, never an error (this runs
/// on an already-failing path, so it must not mask the real failure with its own).
///
/// Crucially, the tail is taken from the conftest section, NOT the raw file end:
/// autoconf's EXIT trap appends a large debug dump on ANY exit (the
/// `## Cache variables ##` / `## Output variables ##` / `## confdefs.h ##`
/// sections — hundreds of lines of cache assignments and `#define`s; a real
/// config.log measured 2266 lines with that dump running from line 1937 to EOF).
/// A blind file tail would show only that `#define` noise. So the window is cut at
/// the first dump marker: the failing conftest — the actual #366 evidence — is the
/// last thing before it. A configure KILLED before its trap ran has no marker, so
/// the whole file is used (the failure is then at its very end).
fn configure_log_tails(srcdir: &Path) -> String {
    const MAX_LOGS: usize = 4;
    const TAIL_LINES: usize = 80;
    let mut out = String::new();
    for log in find_config_logs(srcdir, MAX_LOGS) {
        let body = match fs::read(&log) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&body);
        let lines: Vec<&str> = text.lines().collect();
        // Cut at the first line of autoconf's trailing debug dump; everything
        // above it is the useful conftest section. No marker ⇒ use the whole file.
        let cut = lines
            .iter()
            .position(|l| l.trim_start().starts_with("## Cache variables"))
            .unwrap_or(lines.len());
        let region = lines.get(..cut).unwrap_or(lines.as_slice());
        let shown = TAIL_LINES.min(region.len());
        let start = region.len().saturating_sub(TAIL_LINES);
        let tail = region.get(start..).unwrap_or(&[]).join("\n");
        out.push_str(&format!(
            "\n--- {} — {shown} lines of the conftest section (autoconf's trailing cache/confdefs dump trimmed); the compile failure is logged here, not on the terminal ---\n{tail}\n",
            log.display(),
        ));
    }
    out
}

fn decode_application_manifest(
    encoded: &str,
) -> Result<td_engine::application::ApplicationManifest, String> {
    let value = crate::json::parse(encoded)
        .map_err(|error| format!("TD_APPLICATION_MANIFEST JSON: {error}"))?;
    let text = value
        .as_str()
        .ok_or("TD_APPLICATION_MANIFEST is not a JSON string")?;
    let manifest = td_engine::application::ApplicationManifest::parse(text)
        .map_err(|error| format!("TD_APPLICATION_MANIFEST: {error}"))?;
    if manifest.to_keyfile() != text {
        return Err("TD_APPLICATION_MANIFEST is not canonical".into());
    }
    Ok(manifest)
}

fn decode_application_spec(
    encoded: &str,
) -> Result<td_engine::application_spec::ApplicationSpec, String> {
    let value = crate::json::parse(encoded)
        .map_err(|error| format!("TD_APPLICATION_SPEC JSON: {error}"))?;
    let text = value
        .as_str()
        .ok_or("TD_APPLICATION_SPEC is not a JSON string")?;
    let spec = td_engine::application_spec::ApplicationSpec::parse(text)
        .map_err(|error| format!("TD_APPLICATION_SPEC: {error}"))?;
    if spec.to_keyfile() != text {
        return Err("TD_APPLICATION_SPEC is not canonical".into());
    }
    Ok(spec)
}

fn decode_application_launcher(
    encoded: &str,
) -> Result<td_engine::launcher::LauncherExport, String> {
    let value = crate::json::parse(encoded)
        .map_err(|error| format!("TD_APPLICATION_LAUNCHER JSON: {error}"))?;
    let text = value
        .as_str()
        .ok_or("TD_APPLICATION_LAUNCHER is not a JSON string")?;
    let launcher = td_engine::launcher::LauncherExport::parse(text)
        .map_err(|error| format!("TD_APPLICATION_LAUNCHER: {error}"))?;
    if launcher.to_tsv() != text {
        return Err("TD_APPLICATION_LAUNCHER is not canonical".into());
    }
    Ok(launcher)
}

fn write_application_metadata_file(out: &Path, name: &str, text: &str) -> Result<(), String> {
    let path = out.join(name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "application metadata collision: {} is reserved for builder-authenticated metadata",
                    path.display()
                )
            } else {
                format!("create {}: {error}", path.display())
            }
        })?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

pub(crate) fn materialize_application_metadata_at(
    out: &Path,
    manifest: Option<&str>,
    spec: Option<&str>,
    launcher: Option<&str>,
) -> Result<(), String> {
    if manifest.is_some() != spec.is_some() || manifest.is_some() != launcher.is_some() {
        return Err(
            "application metadata requires the manifest, compiled spec and launcher export".into(),
        );
    }
    let metadata = fs::symlink_metadata(out)
        .map_err(|error| format!("stat application output {}: {error}", out.display()))?;
    if !metadata.file_type().is_dir() {
        if manifest.is_none() && metadata.file_type().is_file() {
            return Ok(());
        }
        return Err(format!(
            "application output {} is not a directory",
            out.display()
        ));
    }
    for name in ["manifest", "spec"] {
        let path = out.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(format!(
                    "undeclared application metadata: {} is reserved for builder-authenticated metadata",
                    path.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("stat {}: {error}", path.display())),
        }
    }
    let exports = out.join("exports");
    match fs::symlink_metadata(&exports) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            let path = exports.join("launcher.tsv");
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(format!(
                        "undeclared application metadata: {} is reserved for builder-authenticated metadata",
                        path.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("stat {}: {error}", path.display())),
            }
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "application metadata collision: {} is a symlink",
                exports.display()
            ));
        }
        Ok(_) if launcher.is_some() => {
            return Err(format!(
                "application metadata collision: {} is not a directory",
                exports.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("stat {}: {error}", exports.display())),
    }
    match (manifest, spec, launcher) {
        (Some(manifest), Some(spec), Some(launcher)) => {
            // Parse all three before writing any, so malformed trusted metadata
            // cannot leave a partly materialized package in scratch.
            let manifest = decode_application_manifest(manifest)?;
            let spec = decode_application_spec(spec)?;
            let launcher = decode_application_launcher(launcher)?;
            if manifest.name() != spec.name() || manifest.name() != launcher.name() {
                return Err(format!(
                    "application metadata identities disagree: manifest={:?}, spec={:?}, launcher={:?}",
                    manifest.name(),
                    spec.name(),
                    launcher.name()
                ));
            }
            write_application_metadata_file(out, "manifest", &manifest.to_keyfile())?;
            write_application_metadata_file(out, "spec", &spec.to_keyfile())?;
            match fs::symlink_metadata(&exports) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "application metadata collision: {} is not a directory",
                        exports.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&exports)
                        .map_err(|error| format!("mkdir {}: {error}", exports.display()))?;
                }
                Err(error) => return Err(format!("stat {}: {error}", exports.display())),
            }
            fs::set_permissions(&exports, fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("chmod {}: {error}", exports.display()))?;
            write_application_metadata_file(&exports, "launcher.tsv", &launcher.to_tsv())
        }
        (None, None, None) => Ok(()),
        _ => Err("application metadata is incomplete".into()),
    }
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
    // On a configure failure the real cause is in config.log, never on the
    // terminal (autoconf redirects conftest output there). Append its tail to the
    // error so the failing probe is diagnosable from the gate log alone — this is
    // what turns the #366-class flake ("cannot find a type to use in place of
    // socklen_t", a conftest killed under memory pressure) from a bare red into
    // evidence. The tail lands at the END of the error, so even a short `tail` of
    // the build log shows it.
    run_cmd(&bash, &conf, &srcdir, &envs, &WATCH_CONFIGURE)
        .map_err(|e| format!("{e}{}", configure_log_tails(Path::new(&srcdir))))?;

    // build + install. Pass SHELL=<bash> as a make OVERRIDE (not just env): make
    // launches recipe shells via the SHELL make-variable, defaulting to /bin/sh,
    // which does not exist in the sandbox (the `po/` install rules hit this). A
    // command-line assignment overrides the Makefile AND propagates to sub-makes.
    let shell = format!("SHELL={bash}");
    run_cmd(&make, &[&shell], &srcdir, &envs, &WATCH_PHASE)?;
    run_cmd(&make, &[&shell, "install"], &srcdir, &envs, &WATCH_PHASE)?;
    Ok(())
}

pub(crate) const STAGED_CARGO_LOCK: &str = ".td-Cargo.lock";

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

pub(crate) fn valid_cargo_subdir(subdir: &str) -> bool {
    !subdir.is_empty()
        && !subdir.as_bytes().contains(&0)
        && Path::new(subdir)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

pub(crate) fn valid_cargo_package_name(package: &str) -> bool {
    let mut bytes = package.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_cargo_source_patch_path(path: &str) -> bool {
    for suffix in ["Cargo.toml", "build.rs"] {
        let nested = format!("/{suffix}");
        if path == suffix
            || path
                .strip_suffix(nested.as_str())
                .is_some_and(valid_cargo_subdir)
        {
            return true;
        }
    }
    false
}

const MAX_CARGO_GIT_SOURCES: usize = 64;
const MAX_CARGO_GIT_PACKAGES: usize = 256;
const MAX_CARGO_SOURCE_PATCHES: usize = 32;
const MAX_CARGO_SOURCE_EDITS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CargoGitPackage {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CargoGitSource {
    pub source: String,
    pub input: String,
    pub packages: Vec<CargoGitPackage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CargoSourcePatch {
    file: String,
    edits: Vec<TextEdit>,
}

fn exact_object_fields(value: &Json, expected: &[&str], label: &str) -> Result<(), String> {
    let Json::Obj(fields) = value else {
        return Err(format!("{label} must be an object"));
    };
    if fields.len() != expected.len()
        || expected
            .iter()
            .any(|key| fields.iter().filter(|(held, _)| held == key).count() != 1)
    {
        return Err(format!(
            "{label} must contain exactly {}",
            expected.join(", ")
        ));
    }
    Ok(())
}

pub(crate) fn cargo_git_input_is_name(input: &str) -> bool {
    let mut bytes = input.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && input.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn cargo_git_input_is_store_path(input: &str) -> bool {
    let store_prefix = format!("{}/", crate::store::store_dir().trim_end_matches('/'));
    input
        .strip_prefix(&store_prefix)
        .is_some_and(|basename| {
            !basename.contains('/') && crate::store::hash_from_store_path(input).is_some()
        })
}

fn valid_cargo_git_input(input: &str) -> bool {
    cargo_git_input_is_name(input) || cargo_git_input_is_store_path(input)
}

fn valid_cargo_git_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_')
        })
}

fn valid_cargo_git_package_path(path: &str) -> bool {
    path == "." || valid_cargo_subdir(path)
}

/// Split an exact Cargo Git source id into the source-table key Cargo expects,
/// the transport URL, and the full commit. td deliberately admits only a
/// `rev=<40-hex>#<same-40-hex>` HTTPS source: a branch/tag-only lock entry is
/// not the commit pin AGENTS.md requires.
pub(crate) fn cargo_git_source_parts(source: &str) -> Result<(String, String, String), String> {
    if source.is_empty()
        || source.len() > 1024
        || source.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0))
    {
        return Err("Cargo Git source id is empty, oversized, or contains a line break".into());
    }
    let source_body = source
        .strip_prefix("git+")
        .ok_or_else(|| format!("Cargo Git source id must start with `git+': {source}"))?;
    let (without_fragment, fragment) = source_body
        .split_once('#')
        .ok_or_else(|| format!("Cargo Git source id has no pinned commit fragment: {source}"))?;
    if fragment.len() != 40
        || !fragment
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "Cargo Git source id commit must be exactly 40 lowercase hexadecimal digits: {source}"
        ));
    }
    let (url, rev) = without_fragment
        .rsplit_once("?rev=")
        .ok_or_else(|| format!("Cargo Git source id must carry `?rev=<commit>': {source}"))?;
    if !url.starts_with("https://")
        || url.contains('?')
        || url.contains('"')
        || url.contains('\\')
        || rev != fragment
    {
        return Err(format!(
            "Cargo Git source id must be an HTTPS URL with matching rev and fragment commits: {source}"
        ));
    }
    Ok((
        format!("git+{without_fragment}"),
        url.to_string(),
        fragment.to_string(),
    ))
}

/// Parse the typed recipe/env form shared by assembly and the Rust runner.
/// The `input` is a recipe input name before assembly and its resolved store
/// path afterwards; callers decide which of those two forms they require.
pub(crate) fn parse_cargo_git_sources(value: &Json) -> Result<Vec<CargoGitSource>, String> {
    let sources = value
        .as_arr()
        .ok_or("`cargoGitSources' must be an array")?;
    if sources.is_empty() || sources.len() > MAX_CARGO_GIT_SOURCES {
        return Err(format!(
            "`cargoGitSources' must contain 1 through {MAX_CARGO_GIT_SOURCES} sources"
        ));
    }
    let mut parsed = Vec::new();
    let mut seen_sources = std::collections::BTreeSet::new();
    let mut seen_inputs = std::collections::BTreeSet::new();
    let mut seen_packages = std::collections::BTreeSet::new();
    let mut package_count = 0usize;
    for source_value in sources {
        exact_object_fields(
            source_value,
            &["source", "input", "packages"],
            "Cargo Git source",
        )?;
        let source = source_value
            .get("source")
            .and_then(Json::as_str)
            .ok_or("Cargo Git source `source' must be a string")?;
        cargo_git_source_parts(source)?;
        if !seen_sources.insert(source.to_string()) {
            return Err(format!("duplicate Cargo Git source id: {source}"));
        }
        let input = source_value
            .get("input")
            .and_then(Json::as_str)
            .ok_or("Cargo Git source `input' must be a string")?;
        if !valid_cargo_git_input(input) {
            return Err(format!(
                "Cargo Git source input is neither a plain input name nor a canonical store path: {input}"
            ));
        }
        if !seen_inputs.insert(input.to_string()) {
            return Err(format!("duplicate Cargo Git source input: {input}"));
        }
        let packages = source_value
            .get("packages")
            .and_then(Json::as_arr)
            .ok_or("Cargo Git source `packages' must be an array")?;
        if packages.is_empty() {
            return Err(format!("Cargo Git source {source} declares no packages"));
        }
        let mut parsed_packages = Vec::new();
        for package_value in packages {
            package_count = package_count
                .checked_add(1)
                .ok_or("Cargo Git package count overflow")?;
            if package_count > MAX_CARGO_GIT_PACKAGES {
                return Err(format!(
                    "Cargo Git sources declare more than {MAX_CARGO_GIT_PACKAGES} packages"
                ));
            }
            exact_object_fields(
                package_value,
                &["name", "version", "path"],
                "Cargo Git package",
            )?;
            let name = package_value
                .get("name")
                .and_then(Json::as_str)
                .ok_or("Cargo Git package `name' must be a string")?;
            let version = package_value
                .get("version")
                .and_then(Json::as_str)
                .ok_or("Cargo Git package `version' must be a string")?;
            let path = package_value
                .get("path")
                .and_then(Json::as_str)
                .ok_or("Cargo Git package `path' must be a string")?;
            if !valid_cargo_package_name(name) {
                return Err(format!("invalid Cargo Git package name: {name}"));
            }
            if !valid_cargo_git_version(version) {
                return Err(format!("invalid Cargo Git package version: {version}"));
            }
            if !valid_cargo_git_package_path(path) {
                return Err(format!("invalid Cargo Git package path: {path}"));
            }
            if !seen_packages.insert((name.to_string(), version.to_string())) {
                return Err(format!(
                    "duplicate Cargo Git package destination: {name}-{version}"
                ));
            }
            parsed_packages.push(CargoGitPackage {
                name: name.to_string(),
                version: version.to_string(),
                path: path.to_string(),
            });
        }
        parsed.push(CargoGitSource {
            source: source.to_string(),
            input: input.to_string(),
            packages: parsed_packages,
        });
    }
    Ok(parsed)
}

pub(crate) fn parse_cargo_source_patches(
    value: &Json,
) -> Result<Vec<CargoSourcePatch>, String> {
    let patches = value
        .as_arr()
        .ok_or("`cargoSourcePatches' must be an array")?;
    if patches.is_empty() || patches.len() > MAX_CARGO_SOURCE_PATCHES {
        return Err(format!(
            "`cargoSourcePatches' must contain 1 through {MAX_CARGO_SOURCE_PATCHES} patches"
        ));
    }
    let mut parsed = Vec::new();
    let mut seen_files = std::collections::BTreeSet::new();
    let mut edit_count = 0usize;
    for patch in patches {
        exact_object_fields(patch, &["file", "edits"], "Cargo source patch")?;
        let file = patch
            .get("file")
            .and_then(Json::as_str)
            .ok_or("Cargo source patch `file' must be a string")?;
        if !valid_cargo_source_patch_path(file) {
            return Err(format!(
                "Cargo source patch path must be a plain relative Cargo.toml or build.rs path: {file}"
            ));
        }
        if !seen_files.insert(file.to_string()) {
            return Err(format!("duplicate Cargo source patch path: {file}"));
        }
        let edits = patch
            .get("edits")
            .and_then(Json::as_arr)
            .ok_or("Cargo source patch `edits' must be an array")?;
        if edits.is_empty() {
            return Err(format!("Cargo source patch {file} has no edits"));
        }
        let mut parsed_edits = Vec::new();
        for edit in edits {
            edit_count = edit_count
                .checked_add(1)
                .ok_or("Cargo source edit count overflow")?;
            if edit_count > MAX_CARGO_SOURCE_EDITS {
                return Err(format!(
                    "Cargo source patches declare more than {MAX_CARGO_SOURCE_EDITS} edits"
                ));
            }
            exact_object_fields(edit, &["from", "to", "expect"], "Cargo source edit")?;
            let from = edit
                .get("from")
                .and_then(Json::as_str)
                .ok_or("Cargo source edit `from' must be a string")?;
            let to = edit
                .get("to")
                .and_then(Json::as_str)
                .ok_or("Cargo source edit `to' must be a string")?;
            let expect = edit
                .get("expect")
                .and_then(Json::as_str)
                .and_then(|count| count.parse::<usize>().ok())
                .ok_or("Cargo source edit `expect' must be a count string")?;
            if from.is_empty() || !from.is_ascii() || !to.is_ascii() || expect == 0 || from == to {
                return Err(format!(
                    "Cargo source edit for {file} must change non-empty ASCII text with a positive expectation"
                ));
            }
            parsed_edits.push((from.to_string(), to.to_string(), expect));
        }
        parsed.push(CargoSourcePatch {
            file: file.to_string(),
            edits: parsed_edits,
        });
    }
    Ok(parsed)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CargoLockSourceCounts {
    pub registry: usize,
    pub git: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CargoLockPackageSource {
    Path,
    Registry,
    Git,
}

/// Require every external package in a committed Cargo.lock to have the
/// fixed-output representation td knows how to stage. Registry packages carry
/// their ordinary SHA-256. Git packages must match one exact declared
/// source/name/version tuple, and every declaration must occur in the lock.
pub(crate) fn validate_cargo_lock_sources(
    lock_text: &str,
    git_sources: &[CargoGitSource],
) -> Result<CargoLockSourceCounts, String> {
    let declared_git: std::collections::BTreeSet<(String, String, String)> = git_sources
        .iter()
        .flat_map(|source| {
            source.packages.iter().map(|package| {
                (
                    package.name.clone(),
                    package.version.clone(),
                    source.source.clone(),
                )
            })
        })
        .collect();
    let mut found_git = std::collections::BTreeSet::new();
    let (mut name, mut version, mut source, mut checksum) =
        (String::new(), String::new(), None, None);
    let mut in_package = false;
    let mut counts = CargoLockSourceCounts::default();
    for line in lock_text.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            if in_package {
                account_cargo_lock_package(
                    &name,
                    &version,
                    &source,
                    &checksum,
                    &declared_git,
                    &mut found_git,
                    &mut counts,
                )?;
            }
            in_package = true;
            name.clear();
            version.clear();
            source = None;
            checksum = None;
        } else if trimmed.starts_with('[') {
            if in_package {
                account_cargo_lock_package(
                    &name,
                    &version,
                    &source,
                    &checksum,
                    &declared_git,
                    &mut found_git,
                    &mut counts,
                )?;
                in_package = false;
            }
        } else if in_package {
            if let Some((key, value)) = trimmed.split_once('=') {
                let value = value.trim().trim_matches('"').to_string();
                match key.trim() {
                    "name" => name = value,
                    "version" => version = value,
                    "source" => source = Some(value),
                    "checksum" => checksum = Some(value),
                    _ => {}
                }
            }
        }
    }
    if in_package {
        account_cargo_lock_package(
            &name,
            &version,
            &source,
            &checksum,
            &declared_git,
            &mut found_git,
            &mut counts,
        )?;
    }
    if let Some(missing) = declared_git.difference(&found_git).next() {
        return Err(format!(
            "declared Cargo Git package `{}-{}' from `{}` is absent from the committed lock",
            missing.0, missing.1, missing.2
        ));
    }
    Ok(counts)
}

fn account_cargo_lock_package(
    name: &str,
    version: &str,
    source: &Option<String>,
    checksum: &Option<String>,
    declared_git: &std::collections::BTreeSet<(String, String, String)>,
    found_git: &mut std::collections::BTreeSet<(String, String, String)>,
    counts: &mut CargoLockSourceCounts,
) -> Result<(), String> {
    let kind = check_cargo_lock_package(
        name,
        version,
        source,
        checksum,
        declared_git,
        found_git,
    )?;
    let count = match kind {
        CargoLockPackageSource::Path => return Ok(()),
        CargoLockPackageSource::Registry => &mut counts.registry,
        CargoLockPackageSource::Git => &mut counts.git,
    };
    *count = (*count)
        .checked_add(1)
        .ok_or("committed Cargo.lock package count overflow")?;
    Ok(())
}

fn validate_runner_cargo_lock_sources(
    committed_lock: Option<&[u8]>,
    git_sources: &[CargoGitSource],
) -> Result<(), String> {
    let Some(lock) = committed_lock else {
        if git_sources.is_empty() {
            return Ok(());
        }
        return Err("Cargo Git sources require a staged committed Cargo.lock".into());
    };
    let lock = std::str::from_utf8(lock)
        .map_err(|error| format!("staged committed Cargo.lock is not UTF-8: {error}"))?;
    validate_cargo_lock_sources(lock, git_sources).map(|_| ())
}

fn check_cargo_lock_package(
    name: &str,
    version: &str,
    source: &Option<String>,
    checksum: &Option<String>,
    declared_git: &std::collections::BTreeSet<(String, String, String)>,
    found_git: &mut std::collections::BTreeSet<(String, String, String)>,
) -> Result<CargoLockPackageSource, String> {
    let Some(source) = source else {
        return Ok(CargoLockPackageSource::Path);
    };
    if name.is_empty() || version.is_empty() {
        return Err(format!(
            "committed lock external package from `{source}' has no name or version"
        ));
    }
    if source.starts_with("git+") {
        let key = (name.to_string(), version.to_string(), source.to_string());
        if !declared_git.contains(&key) {
            return Err(format!(
                "committed lock package `{name}-{version}' is an undeclared Git dependency (`{source}') — add an explicitly approved fixed-output cargoGitSources mapping"
            ));
        }
        found_git.insert(key);
        return Ok(CargoLockPackageSource::Git);
    }
    if !source.starts_with("registry+") {
        return Err(format!(
            "committed lock package `{name}-{version}' has unsupported source `{source}'"
        ));
    }
    match checksum {
        Some(value)
            if value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            Ok(CargoLockPackageSource::Registry)
        }
        _ => Err(format!(
            "committed lock package `{name}' has registry source `{source}' but no valid sha256 checksum"
        )),
    }
}

fn cargo_vendor_config(
    has_registry_packages: bool,
    git_sources: &[CargoGitSource],
    vendor_dir: &str,
) -> Result<String, String> {
    if vendor_dir
        .bytes()
        .any(|byte| matches!(byte, b'"' | b'\\' | b'\n' | b'\r' | 0))
    {
        return Err(format!(
            "Cargo vendor directory cannot be represented as a literal TOML path: {vendor_dir}"
        ));
    }
    let mut config = String::new();
    if has_registry_packages {
        config.push_str("[source.crates-io]\nreplace-with = \"vendored-sources\"\n");
    }
    for source in git_sources {
        let (key, url, rev) = cargo_git_source_parts(&source.source)?;
        config.push_str(&format!(
            "[source.\"{key}\"]\ngit = \"{url}\"\nrev = \"{rev}\"\nreplace-with = \"vendored-sources\"\n"
        ));
    }
    config.push_str(&format!(
        "[source.vendored-sources]\ndirectory = \"{vendor_dir}\"\n"
    ));
    Ok(config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CargoLockPolicy {
    Verify,
    Replace,
}

fn parse_cargo_lock_policy(value: &str) -> Result<CargoLockPolicy, String> {
    match value {
        "verify" => Ok(CargoLockPolicy::Verify),
        "replace" => Ok(CargoLockPolicy::Replace),
        _ => Err(format!(
            "TD_CARGO_LOCK_POLICY must be `verify' or `replace', not `{value}'"
        )),
    }
}

fn open_regular_file(path: &Path, description: &str, write: bool) -> Result<fs::File, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{description} is not a regular file: {}",
            path.display()
        ));
    }
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(write)
        .custom_flags(crate::nar::O_NOFOLLOW | crate::sys::O_NONBLOCK as i32);
    let file = options
        .open(path)
        .map_err(|error| format!("open {description} {}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| format!("inspect open {description} {}: {error}", path.display()))?
        .is_file()
    {
        return Err(format!(
            "{description} is not a regular file: {}",
            path.display()
        ));
    }
    Ok(file)
}

fn read_regular_file(path: &Path, description: &str) -> Result<Vec<u8>, String> {
    let mut file = open_regular_file(path, description, false)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {description} {}: {error}", path.display()))?;
    Ok(bytes)
}

fn write_regular_file(path: &Path, description: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = open_regular_file(path, description, true)?;
    file.set_len(0)
        .map_err(|error| format!("truncate {description} {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {description} {}: {error}", path.display()))
}

fn enforce_cargo_lock_policy(
    cargo_dir: &Path,
    staged_vendor_dir: &Path,
    policy: CargoLockPolicy,
) -> Result<Vec<u8>, String> {
    let committed_path = staged_vendor_dir.join(STAGED_CARGO_LOCK);
    let committed = read_regular_file(&committed_path, "staged committed Cargo.lock")?;
    let source_path = cargo_dir.join("Cargo.lock");
    let source = read_regular_file(&source_path, "source workspace Cargo.lock")?;
    match policy {
        CargoLockPolicy::Verify if source != committed => Err(format!(
            "source workspace Cargo.lock {} does not byte-match the committed lock; update the reviewed lock from the pinned source, or declare replaceCargoLock for an intentional normalized workspace lock",
            source_path.display()
        )),
        CargoLockPolicy::Verify => Ok(committed),
        CargoLockPolicy::Replace => {
            if source != committed {
                write_regular_file(
                    &source_path,
                    "source workspace Cargo.lock",
                    &committed,
                )?;
            }
            Ok(committed)
        }
    }
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
///   TD_INPUTS    ':'-joined input store paths (rustc, cargo, gcc, binutils,
///                libc, build userland) — their bin/ dirs build PATH, lib/ build
///                LIBRARY_PATH.
///   TD_RUST_STORE_CC / TD_RUST_STORE_CXX optional exact compilers nested in a
///                recipe output whose top-level `bin/` is not the installed
///                compiler prefix (the native GCC ladder output).
///   TD_RUST_STORE_INCLUDE optional ':'-joined native include directories.
///   TD_RUST_BINS space-separated binary names to install into $out/bin.
///   TD_CARGO_SUBDIR optional relative path from the materialized source root to
///                the Cargo workspace.
///   TD_CARGO_PACKAGE optional package selected from that workspace. Every
///                TD_RUST_BINS entry must be a binary target of this package.
///   TD_CARGO_LOCK_POLICY optional `verify` or `replace`. The exact committed
///                lock is `TD_VENDOR_DIR/.td-Cargo.lock`; verify requires the
///                materialized workspace lock to byte-match it, while replace
///                writes those exact reviewed bytes before cargo `--frozen`.
///   TD_CARGO_GIT_SOURCES optional typed JSON mapping exact Cargo Git source ids
///                to fixed-output source archive store paths and the package
///                directories copied from each archive into the vendor tree.
///   TD_CARGO_SOURCE_PATCHES optional typed JSON literal edits to Cargo.toml or
///                build.rs files below the selected workspace. Every edit
///                carries an exact count, and no path may traverse a symlink.
///   TD_RUST_PROTOC optional exact declared source-built Protocol Buffers
///                compiler exposed to Cargo build scripts as PROTOC.
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
    let recipe_name =
        env::var("TD_RECIPE_NAME").map_err(|_| "TD_RECIPE_NAME not set".to_string())?;
    require_debug_companion_policy(&recipe_name)?;
    let cargo_subdir = match env::var("TD_CARGO_SUBDIR") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_CARGO_SUBDIR is not valid UTF-8".into())
        }
    };
    let cargo_package = match env::var("TD_CARGO_PACKAGE") {
        Ok(value) if valid_cargo_package_name(&value) => Some(value),
        Ok(_) => {
            return Err(
                "TD_CARGO_PACKAGE must start with an ASCII letter or digit and use only ASCII letters, digits, `-' or `_'".into(),
            )
        }
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_CARGO_PACKAGE is not valid UTF-8".into())
        }
    };
    let cargo_lock_policy = match env::var("TD_CARGO_LOCK_POLICY") {
        Ok(value) => Some(parse_cargo_lock_policy(&value)?),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_CARGO_LOCK_POLICY is not valid UTF-8".into())
        }
    };
    let cargo_git_sources = match env::var("TD_CARGO_GIT_SOURCES") {
        Ok(value) => {
            let parsed = crate::json::parse(&value)
                .map_err(|error| format!("TD_CARGO_GIT_SOURCES JSON: {error}"))?;
            let sources = parse_cargo_git_sources(&parsed)?;
            for source in &sources {
                if !cargo_git_input_is_store_path(&source.input) {
                    return Err(format!(
                        "TD_CARGO_GIT_SOURCES input is not a canonical active-store path: {}",
                        source.input
                    ));
                }
            }
            sources
        }
        Err(env::VarError::NotPresent) => Vec::new(),
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_CARGO_GIT_SOURCES is not valid UTF-8".into())
        }
    };
    let cargo_source_patches = match env::var("TD_CARGO_SOURCE_PATCHES") {
        Ok(value) => {
            let parsed = crate::json::parse(&value)
                .map_err(|error| format!("TD_CARGO_SOURCE_PATCHES JSON: {error}"))?;
            parse_cargo_source_patches(&parsed)?
        }
        Err(env::VarError::NotPresent) => Vec::new(),
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_CARGO_SOURCE_PATCHES is not valid UTF-8".into())
        }
    };
    let protoc = match env::var("TD_RUST_PROTOC") {
        Ok(value) => {
            require_executable_file(&value, "source-built protoc")?;
            Some(value)
        }
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_RUST_PROTOC is not valid UTF-8".into())
        }
    };
    let vendor_input_dir = match env::var("TD_VENDOR_DIR") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => String::new(),
        Err(env::VarError::NotUnicode(_)) => {
            return Err("TD_VENDOR_DIR is not valid UTF-8".into())
        }
    };
    let bins: Vec<&str> = bins_spec.split_whitespace().collect();
    if bins.is_empty() {
        return Err("TD_RUST_BINS is empty (no binaries to install)".into());
    }
    if !cargo_git_sources.is_empty() && cargo_lock_policy.is_none() {
        return Err("TD_CARGO_GIT_SOURCES requires TD_CARGO_LOCK_POLICY".into());
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
    let mut path = path.join(":");
    let cargo = find_in_path(&path, "cargo").ok_or("cargo not found in TD_INPUTS")?;
    find_in_path(&path, "rustc").ok_or("rustc not found in TD_INPUTS")?;
    let objcopy = env::var("TD_RUST_OBJCOPY").map_err(|_| {
        "TD_RUST_OBJCOPY not set (exact declared target objcopy is required)".to_string()
    })?;
    require_executable_file(&objcopy, "target objcopy")?;
    let cp = find_in_path(&path, "cp").ok_or("cp not found in TD_INPUTS")?;
    let chmod = find_in_path(&path, "chmod").ok_or("chmod not found in TD_INPUTS")?;
    let gcc = env::var("TD_RUST_STORE_CC")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| find_in_path(&path, "gcc"))
        .ok_or("gcc not found in TD_INPUTS and TD_RUST_STORE_CC is unset (linker)")?;
    require_executable_file(&gcc, "native Rust linker")?;
    // Optional C/C++ compiler for crates with C build scripts (the `cc` crate honors
    // CC/CXX). Absent for pure-Rust builds — harmless, since no C is compiled then.
    let gpp = env::var("TD_RUST_STORE_CXX")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| find_in_path(&path, "g++"));
    if let Some(cxx) = &gpp {
        require_executable_file(cxx, "native Rust C++ compiler")?;
    }
    if let Ok(extra) = env::var("TD_RUST_STORE_INCLUDE") {
        for dir in extra.split(':').filter(|p| !p.is_empty()) {
            if !Path::new(dir).is_dir() {
                return Err(format!("native Rust include directory does not exist: {dir}"));
            }
            cinc.push(dir.to_string());
        }
    }

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
    if rust_workspace_needs_native_host_linker(
        &recipe_name,
        cargo_subdir.as_deref(),
        cargo_package.as_deref(),
    ) {
        if let Ok(interp) = env::var("TD_RUST_STORE_INTERP") {
            if !interp.is_empty() {
                let shell = find_in_path(&path, "sh")
                    .ok_or("sh not found in TD_INPUTS (native Rust host-link wrapper)")?;
                path = install_native_rust_host_linker(
                    &cwd,
                    &shell,
                    &gcc,
                    &interp,
                    &env::var("TD_RUST_STORE_RPATH").unwrap_or_default(),
                    &env::var("TD_RUST_STORE_BDIR").unwrap_or_default(),
                    &path,
                )?;
            }
        }
    }
    let build_abs = cwd.join(build_dir);
    let cargo_dir = cargo_workspace_dir(&build_abs, cargo_subdir.as_deref())?;
    apply_cargo_source_patches(&cargo_dir, &cargo_source_patches)?;
    let committed_lock = if let Some(policy) = cargo_lock_policy {
        if vendor_input_dir.is_empty() {
            return Err("TD_CARGO_LOCK_POLICY requires TD_VENDOR_DIR".into());
        }
        require_selected_cargo_workspace(&cargo, &cargo_dir, &path_env)?;
        Some(enforce_cargo_lock_policy(
            &cargo_dir,
            Path::new(&vendor_input_dir),
            policy,
        )?)
    } else {
        None
    };
    validate_runner_cargo_lock_sources(committed_lock.as_deref(), &cargo_git_sources)?;
    let cargo_dir_str = cargo_dir
        .to_str()
        .ok_or("non-utf8 Cargo workspace path")?
        .to_string();
    let build_abs = build_abs.to_str().ok_or("non-utf8 build path")?.to_string();
    let cargo_home = cwd.join("td-cargo-home");
    let cargo_home = cargo_home.to_str().ok_or("non-utf8 cargo-home")?.to_string();
    let vendor_dir = cwd.join("td-rust-vendor");
    let vendor_abs = vendor_dir
        .to_str()
        .ok_or("non-utf8 vendor path")?
        .to_string();
    // Reproducibility: remap the (varying) build dir + CARGO_HOME so file!()/debug
    // paths don't leak into the binary; link via gcc (ld-wrapper) so the output
    // gets a RUNPATH to the toolchain libs.
    let mut rustflags =
        td_engine::target_profile::cargo_rustflags(&build_abs, &cargo_home, &vendor_abs, &gcc);
    let cflags = td_engine::target_profile::cargo_cflags(&build_abs, &cargo_home, &vendor_abs);
    // Native /td/store toolchain (#258): the native gcc is a PLAIN gcc, NOT guix's ld-wrapper, so it
    // injects no interp/RUNPATH. When TD_RUST_STORE_INTERP is set the caller is linking against the
    // native /td/store toolchain — bake them explicitly (the #255 rustc-compile recipe): the dynamic
    // linker = the /td/store ld, a RUNPATH per TD_RUST_STORE_RPATH dir so the produced binary resolves
    // its libs (glibc, libgcc_s, libz) from /td/store at run time, and -B per TD_RUST_STORE_BDIR so the
    // native gcc finds the glibc crt/lib at link time. Unset ⇒ the guix ld-wrapper path, unchanged.
    if let Ok(interp) = env::var("TD_RUST_STORE_INTERP") {
        if !interp.is_empty() {
            rustflags.push_str(&format!(" -Clink-arg=-Wl,--dynamic-linker,{interp}"));
            // The source-built native GCC is static, and td-native Rust outputs
            // must not acquire an undeclared shared libgcc runtime edge.
            rustflags.push_str(" -Clink-arg=-static-libgcc");
            for rp in env::var("TD_RUST_STORE_RPATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
                rustflags.push_str(&format!(" -Clink-arg=-Wl,-rpath,{rp}"));
            }
            for b in env::var("TD_RUST_STORE_BDIR").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
                rustflags.push_str(&format!(" -Clink-arg=-B{b}"));
            }
        }
    }
    // A crate's C build script drives the `cc` crate, whose compile+link reads LIBRARY_PATH
    // for BOTH -l libraries and the crt startfiles (crt1.o/crti.o/crtn.o) — but only the
    // rustc link gets TD_RUST_STORE_BDIR's glibc dir via -B above, and the native glibc's
    // lib lives at a nested stage/td/store/<pkg>/lib path that is NOT any input's {p}/lib.
    // Fold those same bdir dirs into LIBRARY_PATH so a C crypto crate (aws-lc-sys/ring) links
    // instead of redding with "cannot find crt1.o". Unset ⇒ empty ⇒ no-op (the pure-Rust and
    // self-host paths are unchanged); binutils' bin dir carries no libs so adding it is inert.
    for b in env::var("TD_RUST_STORE_BDIR").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
        lib.push(b.to_string());
    }
    // A C build script may compile *and run* a probe executable (aws-lc-sys's memcmp_check
    // links a binary and asserts it exits 0). The native gcc bakes no interp/RUNPATH, so
    // that probe cannot exec in the hermetic sandbox and the crate misreports it as a
    // "compiler bug". Such probes honor LDFLAGS on their link, so mirror the same /td/store
    // dynamic-linker + RUNPATH the rustc link gets above. The `cc` crate never reads LDFLAGS
    // for its object compiles, so this only reaches build scripts that link a runnable exe.
    // Unset TD_RUST_STORE_INTERP leaves only the deterministic build-ID policy.
    let mut ldflags = String::from("-Wl,--build-id=sha1");
    if let Ok(interp) = env::var("TD_RUST_STORE_INTERP") {
        if !interp.is_empty() {
            ldflags.push_str(&format!(" -Wl,--dynamic-linker,{interp}"));
            for rp in env::var("TD_RUST_STORE_RPATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
                ldflags.push_str(&format!(" -Wl,-rpath,{rp}"));
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
        ("CFLAGS".into(), cflags.clone()),
        ("CXXFLAGS".into(), cflags),
        ("HOME".into(), "/homeless-shelter".into()),
        ("CARGO_HOME".into(), cargo_home.clone()),
        ("SOURCE_DATE_EPOCH".into(), "1".into()),
        ("RUSTFLAGS".into(), rustflags),
        (
            "CARGO_BUILD_JOBS".into(),
            crate::check_memory::build_jobs().to_string(),
        ),
    ];
    if let Some(gpp) = gpp {
        envs.push(("CXX".into(), gpp));
    }
    if let Some(protoc) = protoc {
        envs.push(("PROTOC".into(), protoc));
    }
    envs.push(("LDFLAGS".into(), ldflags));

    // Assemble one Cargo vendor directory from registry archives and reviewed
    // fixed-output Git archives. Registry packages retain their Cargo.lock
    // checksums. Git packages have no registry checksum; their archive input's
    // fixed-output hash authenticates the source tree before this runner starts.
    fs::create_dir_all(&cargo_home).map_err(|e| format!("mkdir CARGO_HOME {cargo_home}: {e}"))?;
    let crate_files = collect_vendor_crates(
        &env::var("TD_VENDOR_CRATES").unwrap_or_default(),
        &vendor_input_dir,
    )?;
    if !crate_files.is_empty() || !cargo_git_sources.is_empty() {
        let tar = find_in_path(&path, "tar").ok_or("tar not found in TD_INPUTS (vendor)")?;
        fs::create_dir_all(&vendor_dir).map_err(|e| format!("mkdir vendor: {e}"))?;
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
        let git_unpack = cwd.join("td-rust-git-sources");
        for (index, source) in cargo_git_sources.iter().enumerate() {
            let archive = Path::new(&source.input);
            let metadata = fs::symlink_metadata(archive).map_err(|error| {
                format!("inspect Cargo Git source archive {}: {error}", archive.display())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "Cargo Git source archive is not a regular file: {}",
                    archive.display()
                ));
            }
            let unpack = git_unpack.join(index.to_string());
            fs::create_dir_all(&unpack)
                .map_err(|error| format!("mkdir {}: {error}", unpack.display()))?;
            let unpack_text = unpack
                .to_str()
                .ok_or("non-utf8 Cargo Git source unpack directory")?;
            run_cmd(
                &tar,
                &["xf", source.input.as_str(), "-C", unpack_text],
                ".",
                &path_env,
                &WATCH_PHASE,
            )?;
            let archive_root = PathBuf::from(single_subdir(unpack_text)?);
            let root_metadata = fs::symlink_metadata(&archive_root).map_err(|error| {
                format!("inspect Cargo Git archive root {}: {error}", archive_root.display())
            })?;
            if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
                return Err(format!(
                    "Cargo Git archive root is not a real directory: {}",
                    archive_root.display()
                ));
            }
            for package in &source.packages {
                let package_root = cargo_git_package_root(&archive_root, &package.path)?;
                let manifest = package_root.join("Cargo.toml");
                let manifest_metadata = fs::symlink_metadata(&manifest).map_err(|error| {
                    format!("inspect Cargo Git package manifest {}: {error}", manifest.display())
                })?;
                if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
                    return Err(format!(
                        "Cargo Git package has no regular Cargo.toml: {}",
                        manifest.display()
                    ));
                }
                let destination =
                    vendor_dir.join(format!("{}-{}", package.name, package.version));
                if destination.exists() {
                    return Err(format!(
                        "duplicate Cargo vendor destination: {}",
                        destination.display()
                    ));
                }
                copy_tree_writable(&package_root, &destination)?;
                fs::write(
                    destination.join(".cargo-checksum.json"),
                    "{\"files\":{},\"package\":null}",
                )
                .map_err(|error| {
                    format!(
                        "write Git package checksum for {}: {error}",
                        destination.display()
                    )
                })?;
            }
        }

        let cargo_config =
            cargo_vendor_config(!crate_files.is_empty(), &cargo_git_sources, &vendor_abs)?;
        fs::write(
            format!("{cargo_home}/config.toml"),
            cargo_config,
        )
        .map_err(|e| format!("write cargo config: {e}"))?;
    }

    // build (offline, frozen, release) in the writable tree. Optional cargo feature
    // selection from the recipe: TD_CARGO_NO_DEFAULT=1 ⇒ --no-default-features (drop the
    // crate's defaults, e.g. a C-building jemalloc), TD_CARGO_FEATURES=a,b ⇒ --features a,b.
    // Absent ⇒ the plain default build, unchanged.
    let mut cargo_args: Vec<String> =
        ["build", "--release", "--offline", "--frozen"].iter().map(|s| s.to_string()).collect();
    let (selection_args, cargo_release_dir) = cargo_selection(
        &cargo_dir,
        &bins,
        cargo_subdir.is_some(),
        cargo_package.as_deref(),
    )?;
    cargo_args.extend(selection_args);
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
    run_cmd(&cargo, &cargo_argv, &cargo_dir_str, &envs, &WATCH_PHASE)?;

    // install the named binaries to $out/bin.
    let bindir = format!("{out}/bin");
    fs::create_dir_all(&bindir).map_err(|e| format!("mkdir {bindir}: {e}"))?;
    for b in &bins {
        let from = cargo_release_dir.join(b);
        if !from.is_file() {
            return Err(format!(
                "cargo did not produce expected binary `{b}' at {}",
                from.display()
            ));
        }
        let from = from.to_str().ok_or("non-utf8 Cargo binary path")?;
        run_cmd(&cp, &["-p", from, &format!("{bindir}/{b}")], ".", &path_env, &WATCH_PHASE)?;
    }
    split_debug_tree(Path::new(&out), Path::new(&objcopy), &recipe_name)?;
    Ok(())
}

/// Cargo's explicit `--target` keeps proc macros and build scripts on its host
/// side. That side does not inherit target RUSTFLAGS, so rustc asks PATH for the
/// conventional linker name `cc`. Native td GCC is deliberately nested inside
/// its recipe output and has no `cc` alias; install a scratch-only wrapper which
/// gives host tools the same declared interpreter, search roots and static
/// libgcc policy as target links. The wrapper is build machinery, never copied
/// into the output. Codex is the reviewed caller; pinning its full selection
/// shape keeps another selected workspace from silently acquiring the wrapper.
fn rust_workspace_needs_native_host_linker(
    recipe_name: &str,
    cargo_subdir: Option<&str>,
    cargo_package: Option<&str>,
) -> bool {
    recipe_name == "codex"
        && cargo_subdir == Some("codex-rs")
        && cargo_package == Some("codex-cli")
}

fn install_native_rust_host_linker(
    cwd: &Path,
    shell: &str,
    gcc: &str,
    interp: &str,
    rpaths: &str,
    bdirs: &str,
    path: &str,
) -> Result<String, String> {
    for (name, value) in [("shell", shell), ("gcc", gcc), ("interpreter", interp)] {
        if !safe_wrapper_word(value) {
            return Err(format!(
                "native Rust host-link {name} is not a safe absolute path"
            ));
        }
    }
    let dir = cwd.join("td-native-bin");
    fs::create_dir_all(&dir).map_err(|error| format!("mkdir {}: {error}", dir.display()))?;
    let wrapper = dir.join("cc");
    let mut script = format!(
        "#!{shell}\nexec \"{gcc}\" \"$@\" -static-libgcc -Wl,--dynamic-linker,\"{interp}\""
    );
    for rpath in rpaths.split(':').filter(|value| !value.is_empty()) {
        if !safe_wrapper_word(rpath) {
            return Err("native Rust host-link rpath is not a safe absolute path".into());
        }
        script.push_str(&format!(" -Wl,-rpath,\"{rpath}\""));
    }
    for bdir in bdirs.split(':').filter(|value| !value.is_empty()) {
        if !safe_wrapper_word(bdir) {
            return Err("native Rust host-link -B directory is not a safe absolute path".into());
        }
        script.push_str(&format!(" -B\"{bdir}\""));
    }
    script.push('\n');
    fs::write(&wrapper, script).map_err(|error| {
        format!(
            "write native Rust host linker {}: {error}",
            wrapper.display()
        )
    })?;
    let mut permissions = fs::metadata(&wrapper)
        .map_err(|error| {
            format!(
                "inspect native Rust host linker {}: {error}",
                wrapper.display()
            )
        })?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).map_err(|error| {
        format!(
            "chmod native Rust host linker {}: {error}",
            wrapper.display()
        )
    })?;
    let dir = dir
        .to_str()
        .ok_or("native Rust host-link directory is not UTF-8")?;
    Ok(format!("{dir}:{path}"))
}

fn safe_wrapper_word(value: &str) -> bool {
    value.starts_with('/')
        && !value
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r' | b'"' | b'\\' | b'$' | b'`'))
}

fn cargo_git_package_root(archive_root: &Path, path: &str) -> Result<PathBuf, String> {
    if !valid_cargo_git_package_path(path) {
        return Err(format!("invalid Cargo Git package path: {path}"));
    }
    let mut current = archive_root.to_path_buf();
    if path == "." {
        return Ok(current);
    }
    for component in Path::new(path).components() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!("invalid Cargo Git package path: {path}"));
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            format!(
                "inspect Cargo Git package path {}: {error}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "Cargo Git package path traverses a symlink or non-directory: {}",
                current.display()
            ));
        }
    }
    Ok(current)
}

fn cargo_workspace_dir(source_root: &Path, subdir: Option<&str>) -> Result<PathBuf, String> {
    let mut workspace = source_root.to_path_buf();
    if let Some(subdir) = subdir {
        if !valid_cargo_subdir(subdir) {
            return Err(format!(
                "TD_CARGO_SUBDIR must be a plain relative path below the source root: {subdir}"
            ));
        }
        let relative = Path::new(subdir);
        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                return Err(format!(
                    "TD_CARGO_SUBDIR must be a plain relative path below the source root: {subdir}"
                ));
            };
            workspace.push(component);
            let metadata = fs::symlink_metadata(&workspace).map_err(|error| {
                format!("inspect Cargo workspace component {}: {error}", workspace.display())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "Cargo workspace component is not a real directory: {}",
                    workspace.display()
                ));
            }
        }

        // Cargo walks upward from a selected package manifest to discover its
        // workspace root.  An outer manifest would therefore make Cargo use an
        // outer Cargo.lock while the lock policy below verified the selected
        // directory's lock.  The recipe deliberately selects a self-contained
        // workspace root, so reject that ambiguous layout before invoking Cargo.
        let mut ancestor = workspace.parent();
        while let Some(directory) = ancestor {
            if !directory.starts_with(source_root) {
                break;
            }
            let outer_manifest = directory.join("Cargo.toml");
            match fs::symlink_metadata(&outer_manifest) {
                Ok(_) => {
                    return Err(format!(
                        "TD_CARGO_SUBDIR `{subdir}' is below an outer Cargo.toml {}; Cargo could select that outer workspace and a different Cargo.lock",
                        outer_manifest.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "inspect outer Cargo workspace manifest {}: {error}",
                        outer_manifest.display()
                    ));
                }
            }
            if directory == source_root {
                break;
            }
            ancestor = directory.parent();
        }
    }
    let manifest = workspace.join("Cargo.toml");
    let metadata = fs::symlink_metadata(&manifest)
        .map_err(|error| format!("inspect Cargo workspace manifest {}: {error}", manifest.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "Cargo workspace has no regular Cargo.toml: {}",
            workspace.display()
        ));
    }
    Ok(workspace)
}

fn require_selected_cargo_workspace(
    cargo: &str,
    selected: &Path,
    envs: &[(String, String)],
) -> Result<(), String> {
    let output = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(selected)
        .env_clear()
        .envs(envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("run cargo locate-project in {}: {error}", selected.display()))?;
    if !output.status.success() {
        return Err(format!(
            "cargo could not resolve the selected workspace {}: {}",
            selected.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "cargo locate-project returned a non-UTF-8 workspace path".to_string())?;
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let manifest_text = lines
        .next()
        .ok_or("cargo locate-project returned no workspace manifest")?;
    if lines.next().is_some() {
        return Err("cargo locate-project returned more than one workspace manifest".into());
    }
    let reported_manifest = PathBuf::from(manifest_text.trim());
    let reported_manifest = if reported_manifest.is_absolute() {
        reported_manifest
    } else {
        selected.join(reported_manifest)
    };
    let actual = reported_manifest
        .parent()
        .ok_or_else(|| {
            format!(
                "cargo locate-project returned a manifest with no parent: {}",
                reported_manifest.display()
            )
        })?
        .canonicalize()
        .map_err(|error| {
            format!(
                "canonicalize Cargo's selected workspace {}: {error}",
                reported_manifest.display()
            )
        })?;
    let selected = selected
        .canonicalize()
        .map_err(|error| format!("canonicalize selected Cargo workspace: {error}"))?;
    if actual != selected {
        return Err(format!(
            "selected Cargo workspace {} resolves through Cargo to {}; refusing to verify a Cargo.lock Cargo will not use",
            selected.display(),
            actual.display()
        ));
    }
    Ok(())
}

const RUST_TARGET: &str = "x86_64-unknown-linux-gnu";

fn cargo_selection(
    cargo_dir: &Path,
    bins: &[&str],
    subdir_selected: bool,
    package: Option<&str>,
) -> Result<(Vec<String>, PathBuf), String> {
    let mut args = Vec::new();
    let selected = subdir_selected || package.is_some();
    let release_dir = if selected {
        let target_dir = cargo_dir.join("target");
        let target_arg = target_dir
            .to_str()
            .ok_or("non-utf8 Cargo target directory")?;
        args.push("--target-dir".into());
        args.push(target_arg.into());
        // A workspace-local `.cargo/config.toml` may set `build.target` and
        // otherwise move a successful artifact below an unanticipated triple.
        // The distribution graph is x86-64-only, and its source-built sysroot
        // carries this exact target, so pin it and make the install path exact.
        args.push("--target".into());
        args.push(RUST_TARGET.into());
        target_dir.join(RUST_TARGET).join("release")
    } else {
        cargo_dir.join("target/release")
    };
    if let Some(package) = package {
        args.push("--package".into());
        args.push(package.into());
        for bin in bins {
            args.push("--bin".into());
            args.push((*bin).to_owned());
        }
    }
    Ok((args, release_dir))
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
/// rust-stage0-build — assemble the exact upstream Rust bootstrap components and
/// retarget their ELF interpreters to td's declared runtime closure. This is the
/// explicit bootstrap trust-root transform; `rust-toolchain` is a separate recipe
/// that source-builds the shipped stage2 compiler, standard library, and Cargo.
pub fn run_rust_stage0() -> Result<(), String> {
    crate::toolchain_x86_64::run_rust_stage0_build()
}

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

/// Recursively copy a store tree to `dst`, making the copy user-writable (store
/// trees are read-only; the kaem build writes its artifacts INTO its tree). File
/// exec bits are preserved; symlinks are recreated. Pure `std` — the stage0 rung
/// has NO build inputs, so there is no coreutils `cp` in its sandbox to shell to.
pub(crate) fn copy_tree_writable(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|e| format!("read dir {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", src.display()))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("file type {}: {e}", entry.path().display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_tree_writable(&from, &to)?;
        } else if ft.is_symlink() {
            // Overlay semantics: a CopyTree may land onto an already-populated tree (the
            // kernel-header overlay after `make install`), so remove a colliding dest first —
            // otherwise symlink() reds EEXIST where the regular-file arm below would overwrite.
            let _ = fs::remove_file(&to);
            let target = fs::read_link(&from)
                .map_err(|e| format!("readlink {}: {e}", from.display()))?;
            std::os::unix::fs::symlink(&target, &to)
                .map_err(|e| format!("symlink {}: {e}", to.display()))?;
        } else {
            fs::copy(&from, &to)
                .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
            let mode = entry
                .metadata()
                .map_err(|e| format!("stat {}: {e}", from.display()))?
                .permissions()
                .mode();
            fs::set_permissions(&to, fs::Permissions::from_mode((mode & 0o777) | 0o200))
                .map_err(|e| format!("chmod {}: {e}", to.display()))?;
        }
    }
    Ok(())
}

fn validate_application_files(files: &Path) -> Result<(), String> {
    let root = fs::symlink_metadata(files)
        .map_err(|e| format!("application files root {}: {e}", files.display()))?;
    if !root.is_dir() {
        return Err(format!(
            "application files root {} is not a directory",
            files.display()
        ));
    }
    let root_mode = root.permissions().mode();
    if root_mode & 0o7000 != 0 {
        return Err(format!(
            "application files root {} retains special mode bits",
            files.display()
        ));
    }
    if root_mode & 0o001 == 0 {
        return Err(format!(
            "application files root {} is not traversable by the application uid",
            files.display()
        ));
    }
    let mut pending = vec![files.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .map_err(|e| format!("read application directory {}: {e}", directory.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read application directory {}: {e}", directory.display()))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("stat application path {}: {e}", path.display()))?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                let mode = metadata.permissions().mode();
                if mode & 0o7000 != 0 {
                    return Err(format!(
                        "application directory {} retains special mode bits",
                        path.display()
                    ));
                }
                if mode & 0o001 == 0 {
                    return Err(format!(
                        "application directory {} is not traversable by the application uid",
                        path.display()
                    ));
                }
                pending.push(path);
            } else if file_type.is_file() {
                if metadata.permissions().mode() & 0o7000 != 0 {
                    return Err(format!(
                        "application file {} retains setuid, setgid, or sticky mode bits",
                        path.display()
                    ));
                }
            } else if file_type.is_symlink() {
                return Err(format!(
                    "application path {} is a symlink; static foreign application files \
                     must contain only directories and regular files",
                    path.display()
                ));
            } else {
                return Err(format!(
                    "application path {} is not a directory, regular file, or symlink",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn validate_static_application(out: &Path, entry: &str, runtime: &Path) -> Result<(), String> {
    let runtime_files = runtime.join("files");
    let runtime_metadata = fs::symlink_metadata(&runtime_files).map_err(|e| {
        format!(
            "application runtime {} has no files directory: {e}",
            runtime.display()
        )
    })?;
    if !runtime_metadata.is_dir() {
        return Err(format!(
            "application runtime files {} is not a directory",
            runtime_files.display()
        ));
    }
    let files = out.join("files");
    validate_application_files(&files)?;
    let relative = Path::new(entry)
        .strip_prefix("/app")
        .map_err(|_| format!("application entry {entry:?} is not an absolute child of /app"))?;
    let mut entry_path = files.clone();
    let mut components = relative.components();
    let first = components.next().ok_or_else(|| {
        format!("application entry {entry:?} is not an absolute child of /app")
    })?;
    let std::path::Component::Normal(name) = first else {
        return Err(format!(
            "application entry {entry:?} escapes or aliases the /app tree"
        ));
    };
    entry_path.push(name);
    for component in components {
        let std::path::Component::Normal(name) = component else {
            return Err(format!(
                "application entry {entry:?} escapes or aliases the /app tree"
            ));
        };
        entry_path.push(name);
    }
    let metadata = fs::symlink_metadata(&entry_path)
        .map_err(|e| format!("application entry {}: {e}", entry_path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o001 == 0 {
        return Err(format!(
            "application entry {} is not a world-executable regular file",
            entry_path.display()
        ));
    }
    crate::elf::assert_x86_64_executable(&entry_path)
        .and_then(|()| crate::elf::assert_static(&entry_path))
        .map_err(|e| format!("application entry {}: {e}", entry_path.display()))
}

/// Scan a tree for `/gnu/store` in any regular file's CONTENTS or any symlink's
/// TARGET (a grep-style content walk misses a dangling guix symlink); first hit
/// errors. The stage0 seal's output half, enforced in the ENGINE per build.
fn require_no_gnu_store(dir: &Path) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("file type {}: {e}", entry.path().display()))?;
        let p = entry.path();
        let leak = if ft.is_dir() {
            require_no_gnu_store(&p)?;
            false
        } else if ft.is_symlink() {
            let target =
                fs::read_link(&p).map_err(|e| format!("readlink {}: {e}", p.display()))?;
            target.to_string_lossy().contains("/gnu/store")
        } else if ft.is_file() {
            crate::bootstrap::contains_gnu_store(&p)
                .map_err(|e| format!("read {}: {e}", p.display()))?
        } else {
            false
        };
        if leak {
            return Err(format!(
                "stage0 output {} contains /gnu/store bytes — the seed rung is sealed (north star: no guix bytes)",
                p.display()
            ));
        }
    }
    Ok(())
}

/// stage0-build — td's stage0-posix SEED build "system" (#378 slice 1; sibling of
/// `run`/`run_rust`/`run_cmake`). TD_SRC is the interned unpacked stage0 seed tree: place a
/// writable copy, mark the two binary seeds executable, exec the kaem interpreter
/// over the two pinned scripts (hex0-seed → … → M2, blood-elf-0, kaem-0 → M1,
/// hex2, kaem), install AMD64/{bin,artifact} into $out. The ONLY place a raw
/// binary seed is exec'd — an engine BuildSystem, so the recipe graph is total.
///
/// The SEAL: no build inputs (assemble_recipe_drv hard-errors on any), so the
/// copy/install are native Rust (no coreutils in this sandbox); the kaem steps
/// run with an EMPTY env (`env -i` as engine policy); and any /gnu/store byte or
/// symlink target in the output REDS the build HERE. The one /gnu/store still
/// staged is td-builder's own runtime libc (the host seed) — unreachable by the
/// env-cleared build; retires when td-builder self-hosts on the recipe toolchain.
pub fn run_stage0() -> Result<(), String> {
    let out = env::var("out").map_err(|_| "out not set".to_string())?;
    let src = env::var("TD_SRC").map_err(|_| "TD_SRC not set".to_string())?;
    if !Path::new(&src).is_dir() {
        return Err(format!("TD_SRC {src} is not a directory (want the interned unpacked stage0 seed tree)"));
    }

    // Writable working copy — the kaem build writes artifacts INTO its tree.
    let tree = Path::new("stage0-tree");
    copy_tree_writable(Path::new(&src), tree)?;
    for d in ["AMD64/artifact", "AMD64/bin"] {
        remove_path_if_exists(&tree.join(d))?;
        fs::create_dir_all(tree.join(d)).map_err(|e| format!("mkdir {d}: {e}"))?;
    }
    for seed in [
        "bootstrap-seeds/POSIX/AMD64/hex0-seed",
        "bootstrap-seeds/POSIX/AMD64/kaem-optional-seed",
    ] {
        let p = tree.join(seed);
        crate::bootstrap::make_executable(&p)
            .map_err(|e| format!("chmod +x {}: {e}", p.display()))?;
    }

    // The two kaem steps, env EMPTY (env -i): the scripts drive everything through
    // relative paths inside the tree; nothing outside it is reachable.
    let cwd = tree.to_string_lossy();
    run_cmd(
        "./bootstrap-seeds/POSIX/AMD64/kaem-optional-seed",
        &["./AMD64/mescc-tools-seed-kaem.kaem"],
        &cwd,
        &[],
        &WATCH_PHASE,
    )?;
    run_cmd(
        "./AMD64/artifact/kaem-0",
        &["./AMD64/mescc-tools-mini-kaem.kaem"],
        &cwd,
        &[],
        &WATCH_PHASE,
    )?;

    // Phases 12-23 of upstream's AMD64/kaem.run, driven directly (kaem.run
    // itself is a bash wrapper; its whole job is these two kaem invocations
    // plus the env block reproduced here). Full-kaem rebuilds M2-Planet from
    // its C sources and builds M2-Mesoplanet, blood-elf, and get_machine into
    // AMD64/bin; mescc-tools-extra then compiles the POSIX file tools (cp,
    // chmod, mkdir, rm, replace, match, catm, untar, ungz, unbz2, unxz, wrap,
    // sha256sum) with M2-Mesoplanet — the tools the mes/tcc rungs need so
    // their build scripts stop depending on host coreutils (re #469).
    let env_kv = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    };
    let full_env = env_kv(&[
        ("ARCH", "amd64"),
        ("ARCH_DIR", "AMD64"),
        ("BASE_ADDRESS", "0x00600000"),
        ("BLOOD_FLAG", "--64"),
        ("ENDIAN_FLAG", "--little-endian"),
    ]);
    run_cmd(
        "./AMD64/bin/kaem",
        &["--verbose", "--strict", "--file", "AMD64/mescc-tools-full-kaem.kaem"],
        &cwd,
        &full_env,
        &WATCH_PHASE,
    )?;
    let extra_env = env_kv(&[
        ("ARCH", "amd64"),
        ("M2LIBC", "../M2libc"),
        ("TOOLS", "../AMD64/bin"),
        ("BINDIR", "../AMD64/bin"),
        ("OPERATING_SYSTEM", "Linux"),
        ("EXE_SUFFIX", ""),
    ]);
    let extra_cwd = tree.join("mescc-tools-extra");
    run_cmd(
        "../AMD64/bin/kaem",
        &["--verbose", "--strict", "--file", "mescc-tools-extra.kaem"],
        &extra_cwd.to_string_lossy(),
        &extra_env,
        &WATCH_PHASE,
    )?;
    // Upstream's own pinned answer file: every binary the chain produced must
    // sha256-match the stage0-posix release's recorded hashes — the strongest
    // output check available, and it runs with the just-built sha256sum.
    run_cmd(
        "./AMD64/bin/sha256sum",
        &["-c", "amd64.answers"],
        &cwd,
        &[],
        &WATCH_PHASE,
    )?;

    // Install the built tool dirs — the exact paths the chain's downstream rungs
    // read ($tc/AMD64/bin/{M1,hex2,kaem}, $tc/AMD64/artifact/{M2,blood-elf-0,…}).
    for d in ["AMD64/bin", "AMD64/artifact"] {
        copy_tree_writable(&tree.join(d), &Path::new(&out).join(d))?;
    }
    // Fail HERE (a named tool), not as an opaque exec failure three rungs later.
    for b in [
        "AMD64/bin/M1",
        "AMD64/bin/hex2",
        "AMD64/bin/kaem",
        "AMD64/bin/M2-Planet",
        "AMD64/bin/M2-Mesoplanet",
        "AMD64/bin/blood-elf",
        "AMD64/bin/get_machine",
        "AMD64/bin/cp",
        "AMD64/bin/chmod",
        "AMD64/bin/mkdir",
        "AMD64/bin/rm",
        "AMD64/bin/replace",
        "AMD64/bin/match",
        "AMD64/bin/catm",
        "AMD64/bin/untar",
        "AMD64/bin/ungz",
        "AMD64/bin/unbz2",
        "AMD64/bin/unxz",
        "AMD64/bin/wrap",
        "AMD64/bin/sha256sum",
        "AMD64/artifact/M2",
        "AMD64/artifact/blood-elf-0",
        "AMD64/artifact/kaem-0",
    ] {
        let p = Path::new(&out).join(b);
        let meta = fs::metadata(&p)
            .map_err(|_| format!("seed build did not produce {b} (expected under $out)"))?;
        // A regular file with an exec bit — a directory also has 0o111, so the
        // mode alone would pass a misplaced tree here.
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("seed build product {b} is not an executable file"));
        }
    }
    // The output half of the seal: no /gnu/store byte leaves this build.
    require_no_gnu_store(Path::new(&out))
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_dir() {
                fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))
            } else {
                fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

/// The template context for mesboot steps: `{root}` `{src}` `{out}` `{tools}`
/// `{jobs}` `{in:NAME}`. Any OTHER `{…}` content passes through VERBATIM — the
/// rungs' sed expressions and configure args legitimately contain shell/brace
/// text (`${vdso_symver//./_}`), so only exactly-recognised tokens expand.
struct StepCtx {
    root: String,
    src: String,
    out: String,
    tools: String,
    jobs: String,
    inputs: Vec<(String, String)>,
    /// TD_PAYLOAD_MAP — the DATA channel (APPLICATIONS.md §B.8), disjoint from
    /// `inputs` by construction: assembly refuses a name in both and withholds
    /// every payload from TD_INPUT_MAP.
    payloads: Vec<(String, String)>,
}

impl StepCtx {
    /// The COMMAND expander: every step's templates but the six data fields. A
    /// `{payload:…}` here is an error rather than a miss, and `{in:…}` cannot name
    /// a payload because assembly withheld it from TD_INPUT_MAP.
    fn expand(&self, s: &str) -> Result<String, String> {
        self.expand_with(s, false)
    }

    /// A `glob:` pattern may only read the build's OWN trees. Splicing directory
    /// entries into argv gives a step names it never spelled, and `{in:x}/..` is
    /// the store directory — so `glob:{in:mes}/../*-firefox-140` puts a payload
    /// on a command line with no `{payload:}` (refused there) and no `{in:}`
    /// (withheld from the map): the resolution half of §B.8, defeated by a
    /// template that resolves something else. Every `glob:` in the tree already
    /// reads `{root}`.
    fn check_glob_dir(&self, pat: &str) -> Result<(), String> {
        let dir = match pat.rfind('/') {
            Some(i) => pat.get(..i).unwrap_or("."),
            None => ".",
        };
        let resolved = fs::canonicalize(dir)
            .map_err(|e| format!("glob:{pat}: resolve {dir}: {e}"))?;
        for allowed in [&self.root, &self.out] {
            let base = fs::canonicalize(allowed)
                .map_err(|e| format!("glob:{pat}: resolve {allowed}: {e}"))?;
            if resolved.starts_with(&base) {
                return Ok(());
            }
        }
        Err(format!(
            "glob:{pat} reads {} — a glob may only read the build's own tree \
             ({} or {}), never a staged input (APPLICATIONS.md section B.8)",
            resolved.display(),
            self.root,
            self.out
        ))
    }

    /// The DATA expander: the same tokens plus `{payload:NAME}`, and used ONLY by
    /// the operations the builder performs itself — `Unpack`'s input,
    /// `CopyTree`'s source, `CopyFile`'s file, `StageRuntimeClosure`'s roots,
    /// and `CompileApplicationTables`' package/runtime pairs. This is the resolution
    /// half of §B.8's channel: a step that runs a program the recipe chose has
    /// no name for a payload at all. That is a property, not a scan for one.
    fn expand_data(&self, s: &str) -> Result<String, String> {
        self.expand_with(s, true)
    }

    fn expand_with(&self, s: &str, payloads_visible: bool) -> Result<String, String> {
        let mut r = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find('{') {
            let (before, after) = rest.split_at(i);
            r.push_str(before);
            match after.find('}') {
                None => {
                    r.push_str(after);
                    return Ok(r);
                }
                Some(j) => {
                    let tok = after.get(1..j).unwrap_or("");
                    let repl = match tok {
                        "root" => Some(self.root.as_str()),
                        "src" => Some(self.src.as_str()),
                        "out" => Some(self.out.as_str()),
                        "tools" => Some(self.tools.as_str()),
                        "jobs" => Some(self.jobs.as_str()),
                        _ => {
                            if let Some(name) = tok.strip_prefix("in:") {
                                // A payload reached through `{in:}` is the channel being
                                // crossed, not a typo, so it says which rule refused it —
                                // the plain "no input" below would send a reader looking
                                // for a missing lock entry.
                                if self.payloads.iter().any(|(n, _)| n == name) {
                                    return Err(format!(
                                        "mesboot step template: `{name}' is a payloadInput \
                                         and is not reachable through {{in:}} — it is staged \
                                         as data for unpack/copyTree/copyFile/stageRuntimeClosure/compileApplicationTables only \
                                         (APPLICATIONS.md section B.8)"
                                    ));
                                }
                                Some(
                                    self.inputs
                                        .iter()
                                        .find(|(n, _)| n == name)
                                        .map(|(_, p)| p.as_str())
                                        .ok_or_else(|| {
                                            format!("mesboot step template: no input `{name}' in TD_INPUT_MAP (token {{{tok}}})")
                                        })?,
                                )
                            } else if let Some(name) = tok.strip_prefix("payload:") {
                                if !payloads_visible {
                                    return Err(format!(
                                        "mesboot step template: {{payload:{name}}} resolves \
                                         only in unpack's `input', copyTree's `from', \
                                         copyFile's `file', stageRuntimeClosure's `roots', and \
                                         compileApplicationTables' `packages' or `runtimes' — a payload is never \
                                         named by a step that \
                                         runs a command (APPLICATIONS.md section B.8)"
                                    ));
                                }
                                Some(
                                    self.payloads
                                        .iter()
                                        .find(|(n, _)| n == name)
                                        .map(|(_, p)| p.as_str())
                                        .ok_or_else(|| {
                                            format!("mesboot step template: no payload `{name}' in TD_PAYLOAD_MAP (token {{{tok}}})")
                                        })?,
                                )
                            } else {
                                None
                            }
                        }
                    };
                    match repl {
                        Some(v) => {
                            r.push_str(v);
                            rest = after.get(j + 1..).unwrap_or("");
                        }
                        None => {
                            // not a recognised token: emit the brace verbatim and continue
                            r.push('{');
                            rest = after.get(1..).unwrap_or("");
                        }
                    }
                }
            }
        }
        r.push_str(rest);
        Ok(r)
    }
    fn expand_data_all(&self, xs: &[String]) -> Result<Vec<String>, String> {
        xs.iter().map(|x| self.expand_data(x)).collect()
    }
    fn expand_all(&self, xs: &[String]) -> Result<Vec<String>, String> {
        xs.iter().map(|x| self.expand(x)).collect()
    }
}

/// One literal `SubstituteText` edit as the engine consumes it: `(from, to,
/// expect)`. The recipe-side `TextEdit` struct serializes to this tuple over the
/// build-JSON wire.
type TextEdit = (String, String, usize);

/// Apply a `Step::SubstituteText`'s literal, count-checked edits to `content`
/// (re #469's host-free `patch`/`sed`). Each `(from, to, expect)` requires
/// EXACTLY `expect` (≥ 1) occurrences of `from`, then replaces them all. Edits
/// apply in order, so a later edit sees the earlier ones' result. `file` names
/// the file only for errors. Fail-closed — any of these reds the rung:
///   * an empty `from` (would match everywhere / be meaningless);
///   * `expect == 0` (every declared edit must change something — this catches
///     a `1`→`0` typo instead of silently no-op'ing on an assert-absent);
///   * a non-ASCII byte in `from`/`to` (see below);
///   * an actual occurrence count that differs from `expect` (source drift).
///
/// `from`/`to` are restricted to ASCII: the build-JSON reader (`json.rs::string`)
/// decodes each wire byte as Latin-1 (`byte as char`), so a non-ASCII edit would
/// not survive the recipe→engine round-trip intact — a non-ASCII `to` would write
/// mangled bytes. ASCII is byte-identical through that path; anything else fails
/// closed here rather than silently corrupting the patched output.
fn apply_text_edits(
    file: &str,
    content: String,
    edits: &[TextEdit],
) -> Result<String, String> {
    apply_named_text_edits("substituteText", file, content, edits)
}

fn apply_named_text_edits(
    label: &str,
    file: &str,
    mut content: String,
    edits: &[TextEdit],
) -> Result<String, String> {
    for (j, (from, to, expect)) in edits.iter().enumerate() {
        let at = |m: String| format!("{label} {file} edit {}: {m}", j + 1);
        if from.is_empty() {
            return Err(at("empty `from' string".into()));
        }
        if !from.is_ascii() || !to.is_ascii() {
            return Err(at("`from'/`to' must be ASCII (the build-JSON reader is Latin-1)".into()));
        }
        if *expect == 0 {
            return Err(at("`expect' is 0 — every edit must change at least one occurrence".into()));
        }
        let n = content.matches(from.as_str()).count();
        if n != *expect {
            return Err(at(format!("`from' occurs {n}× (expected {expect})")));
        }
        content = content.replace(from.as_str(), to);
    }
    Ok(content)
}

/// Write `bytes` to a REGULAR file `path`, preserving its ORIGINAL permission
/// mode even when the file is read-only. Some GNU tarballs ship source files 0444
/// (GNU patch's `pch.c`, less's `mkinstalldirs`), and a plain `fs::write` would
/// then fail EACCES. Grant owner-write for the rewrite and restore the original
/// mode, so the on-disk tree differs only in content — a source file's mode never
/// reaches `$out` (install/copy sets output modes), so this stays reproducibility-
/// safe. `symlink_metadata` (no symlink follow) fixes the target BEFORE granting
/// write, so the grant/write/restore cannot land on a different file than the
/// caller validated; a non-regular target is rejected. The restore runs even when
/// the write fails (an error never leaves a 0444 source at 0644), the grant is
/// skipped when the file is already writable, and a restore failure is surfaced,
/// not swallowed (the write error takes precedence). (`patch_shebangs` open-codes
/// a similar grant/restore inline because it must also restore mtimes *between*
/// the write and the mode restore, which this helper deliberately does not.)
fn write_preserving_mode(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{}: not a regular file", path.display()),
        ));
    }
    let orig_mode = meta.permissions().mode();
    let granted = orig_mode & 0o200 == 0;
    if granted {
        fs::set_permissions(path, fs::Permissions::from_mode(orig_mode | 0o200))?;
    }
    let wrote = fs::write(path, bytes);
    let restored = if granted {
        fs::set_permissions(path, fs::Permissions::from_mode(orig_mode))
    } else {
        Ok(())
    };
    wrote.and(restored)
}

fn cargo_source_patch_path(workspace: &Path, relative: &str) -> Result<PathBuf, String> {
    if !valid_cargo_source_patch_path(relative) {
        return Err(format!("invalid Cargo source patch path: {relative}"));
    }
    let mut current = workspace.to_path_buf();
    let components: Vec<_> = Path::new(relative).components().collect();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!("invalid Cargo source patch path: {relative}"));
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            format!(
                "inspect Cargo source patch path {}: {error}",
                current.display()
            )
        })?;
        let final_component = index.saturating_add(1) == components.len();
        let valid_kind = if final_component {
            metadata.is_file()
        } else {
            metadata.is_dir()
        };
        if metadata.file_type().is_symlink() || !valid_kind {
            return Err(format!(
                "Cargo source patch path traverses a symlink or wrong file type: {}",
                current.display()
            ));
        }
    }
    Ok(current)
}

fn apply_cargo_source_patches(
    workspace: &Path,
    patches: &[CargoSourcePatch],
) -> Result<(), String> {
    for patch in patches {
        let path = cargo_source_patch_path(workspace, &patch.file)?;
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("read Cargo source patch {}: {error}", path.display()))?;
        let edited = apply_named_text_edits(
            "cargoSourcePatches",
            &patch.file,
            content,
            &patch.edits,
        )?;
        write_preserving_mode(&path, edited.as_bytes())
            .map_err(|error| format!("write Cargo source patch {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Parse a `substituteText` step: expand ONLY `file` (the target path) with
/// `ctx`; read every edit's `from`/`to`/`expect` LITERALLY. `from`/`to` are
/// source text — a `{in:…}`/`{src}` inside a patched hunk must survive verbatim,
/// so they are NEVER template-expanded (only `file` is). Returns the expanded
/// path and the literal edits (validated for content by `apply_text_edits`).
fn parse_substitute_edits(
    ctx: &StepCtx,
    o: &Json,
) -> Result<(String, Vec<TextEdit>), String> {
    let file = ctx.expand(
        o.get("file")
            .and_then(Json::as_str)
            .ok_or("substituteText: missing/non-string `file'")?,
    )?;
    let edits_json = o
        .get("edits")
        .and_then(Json::as_arr)
        .ok_or("substituteText: `edits' not an array")?;
    let mut edits: Vec<TextEdit> = Vec::with_capacity(edits_json.len());
    for (j, e) in edits_json.iter().enumerate() {
        let where_ = |m: String| format!("substituteText edit {}: {m}", j + 1);
        let from = e
            .get("from")
            .and_then(Json::as_str)
            .ok_or_else(|| where_("`from' missing or not a string".into()))?;
        let to = e
            .get("to")
            .and_then(Json::as_str)
            .ok_or_else(|| where_("`to' missing or not a string".into()))?;
        let expect: usize = e
            .get("expect")
            .and_then(Json::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| where_("`expect' missing or not a count".into()))?;
        edits.push((from.to_string(), to.to_string(), expect));
    }
    Ok((file, edits))
}

/// Minimal glob for mesboot `glob:` argv elements: exactly one `*`, in the LAST
/// path component (`DIR/PRE*SUF`). Returns full paths of matching entries.
fn glob_one_star(pat: &str) -> Result<Vec<String>, String> {
    let (dir, base) = match pat.rfind('/') {
        Some(i) => (pat.get(..i).unwrap_or("."), pat.get(i + 1..).unwrap_or("")),
        None => (".", pat),
    };
    let (pre, suf) = base
        .split_once('*')
        .ok_or_else(|| format!("glob pattern has no `*': {pat}"))?;
    if suf.contains('*') || dir.contains('*') {
        return Err(format!("glob supports exactly one `*' in the basename: {pat}"));
    }
    let mut hits = Vec::new();
    let rd = fs::read_dir(dir).map_err(|e| format!("glob {pat}: read {dir}: {e}"))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("glob {pat}: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(pre) && name.ends_with(suf) && name.len() >= pre.len() + suf.len() {
            hits.push(format!("{dir}/{name}"));
        }
    }
    Ok(hits)
}

fn regular_file_without_symlink_components(
    operation: &str,
    path: &Path,
) -> Result<fs::Metadata, String> {
    let mut current = PathBuf::new();
    let mut final_metadata = None;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => current.push("/"),
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(name) => current.push(name),
            std::path::Component::ParentDir => {
                return Err(format!(
                    "{operation}: {} contains a parent-directory component",
                    path.display()
                ));
            }
            std::path::Component::Prefix(_) => {
                return Err(format!(
                    "{operation}: {} contains an unsupported path prefix",
                    path.display()
                ));
            }
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|e| format!("lstat {}: {e}", current.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{operation}: {} traverses symlink {}; symlinks are refused",
                path.display(),
                current.display()
            ));
        }
        final_metadata = Some(metadata);
    }
    let metadata = final_metadata.ok_or_else(|| {
        format!(
            "{operation}: {} has no regular file component",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{operation}: {} is not a regular file; special files are refused",
            path.display()
        ));
    }
    Ok(metadata)
}

/// Copy one regular file to dest dir (keeping its basename), exec bit preserved,
/// owner-write added. Refuse a symlink anywhere in the source path.
fn copy_file_writable(from: &Path, dest_dir: &Path) -> Result<(), String> {
    let base = from
        .file_name()
        .ok_or_else(|| format!("copyFiles: {} has no basename", from.display()))?;
    let metadata = regular_file_without_symlink_components("copyFiles", from)?;
    fs::create_dir_all(dest_dir).map_err(|e| format!("mkdir {}: {e}", dest_dir.display()))?;
    let to = dest_dir.join(base);
    fs::copy(from, &to)
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
    let mode = metadata.permissions().mode();
    fs::set_permissions(&to, fs::Permissions::from_mode((mode & 0o777) | 0o200))
        .map_err(|e| format!("chmod {}: {e}", to.display()))
}

/// `copyFile`'s destination must be an exact absolute path strictly inside the
/// build's own output, with no `..` component, whose existing components below
/// that root are directories rather than links. The command expander already
/// keeps a payload template out of `to`; this keeps the DESTINATION out of
/// `{root}`, `{tools}` and every other tree, because a data step that could
/// place an executable copy of a payload where a later `run` names it would
/// hand the recipe exactly the tool §B.8's `ro,noexec` bind withholds.
fn confined_destination(out: &Path, to: &Path) -> Result<(), String> {
    let plain = to.components().all(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )
    });
    let Some(below) = to.strip_prefix(out).ok().filter(|_| plain && !to.is_relative()) else {
        return Err(format!(
            "copyFile: destination {} is not a plain path inside the build output {} \
             (APPLICATIONS.md section B.8)",
            to.display(),
            out.display()
        ));
    };
    if below.as_os_str().is_empty() {
        return Err(format!(
            "copyFile: destination {} is the build output itself",
            to.display()
        ));
    }
    let mut current = out.to_path_buf();
    for component in below.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "copyFile: destination {} traverses symlink {}; symlinks are refused",
                    to.display(),
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("lstat {}: {error}", current.display())),
        }
    }
    Ok(())
}

/// `copyFile`: one regular file to an exact destination path. The source may be
/// a payload (APPLICATIONS.md §B.8) — a marked payload that upstream publishes
/// as a bare executable rather than an archive has no other way to reach its
/// `/app` entry name — so the copy is at least as strict as the tree copy: no
/// symlink or parent component in the source, a destination confined to the
/// build output and never overwritten, and a mode taken from the recipe's
/// `exec` rather than from the fetched bytes. That last point is a widening
/// the tree copy does not have — it can only preserve a mode — and is why the
/// destination is confined: an execute bit may be minted only under `{out}`,
/// which is writable and executable for a build's own products already.
fn copy_single_file(out: &Path, file: &Path, to: &Path, exec: bool) -> Result<(), String> {
    regular_file_without_symlink_components("copyFile", file)?;
    confined_destination(out, to)?;
    let parent = to
        .parent()
        .ok_or_else(|| format!("copyFile: {} has no parent directory", to.display()))?;
    // Every directory this step creates is normalized to 0755 the way the tree
    // copy normalizes its own: `create_dir` obeys the caller's umask, and a
    // 0700 `files/bin` would fail the validator's world-traversable rule. The
    // walk stops at `{out}`, which the build created before its first step and
    // the confinement above put `to` strictly inside; a missing `{out}` is an
    // error at the first `mkdir`, not something this step creates.
    let mut missing = Vec::new();
    let mut ancestor = Some(parent);
    while let Some(dir) = ancestor.filter(|dir| *dir != out) {
        match fs::symlink_metadata(dir) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(dir.to_path_buf())
            }
            Err(e) => return Err(format!("stat {}: {e}", dir.display())),
        }
        ancestor = dir.parent();
    }
    for dir in missing.iter().rev() {
        fs::create_dir(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    }
    // `create_new` makes "never overwritten" the kernel's promise rather than a
    // preceding stat's: an existing file, or a link of any kind, fails the open.
    let mut source = fs::File::open(file)
        .map_err(|e| format!("copyFile: open {}: {e}", file.display()))?;
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .map_err(|e| format!("copyFile: create {}: {e}", to.display()))?;
    std::io::copy(&mut source, &mut destination)
        .map_err(|e| format!("copy {} -> {}: {e}", file.display(), to.display()))?;
    let mode = if exec { 0o755 } else { 0o644 };
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", to.display()))
}

fn runtime_candidate_index(
    inputs: &[(String, String)],
    target_store: &str,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut store_dirs = BTreeSet::new();
    store_dirs.insert(PathBuf::from(target_store));
    for (_, path) in inputs {
        if crate::store::hash_from_store_path(path).is_none() {
            continue;
        }
        if let Some(parent) = Path::new(path).parent() {
            store_dirs.insert(parent.to_path_buf());
        }
    }
    let mut by_hash: BTreeMap<String, (String, String)> = BTreeMap::new();
    for dir in store_dirs {
        let entries = fs::read_dir(&dir).map_err(|e| {
            format!(
                "stageRuntimeClosure: read store {}: {e}",
                dir.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                format!(
                    "stageRuntimeClosure: read store entry {}: {e}",
                    dir.display()
                )
            })?;
            let path = entry.path();
            let path_str = path.to_str().ok_or_else(|| {
                format!(
                    "stageRuntimeClosure: non-UTF-8 store path under {}",
                    dir.display()
                )
            })?;
            let Some(hash) = crate::store::hash_from_store_path(path_str) else {
                continue;
            };
            let name = entry.file_name().into_string().map_err(|_| {
                format!(
                    "stageRuntimeClosure: non-UTF-8 store entry under {}",
                    dir.display()
                )
            })?;
            // The canonical entry is shorter than its `.check`/`.chroot` siblings.
            match by_hash.get(hash) {
                Some((current_name, _)) if current_name.len() < name.len() => {}
                Some((current_name, current_path)) if current_name.len() == name.len() => {
                    if current_path != path_str {
                        return Err(format!(
                            "stageRuntimeClosure: store hash {hash} names both \
                             {current_path} and {path_str}"
                        ));
                    }
                }
                _ => {
                    by_hash.insert(hash.to_string(), (name, path_str.to_string()));
                }
            }
        }
    }

    let mut candidates = BTreeMap::new();
    for (_, (_, path)) in by_hash {
        candidates.insert(path.clone(), PathBuf::from(path));
    }
    Ok(candidates)
}

/// Walk the loader-visible graph only. `runtime_link_search` projects each
/// ELF's PT_INTERP, RUNPATH/RPATH, and DT_NEEDED plus symlink targets into the
/// daemon-compatible hash scanner, excluding unrelated build-provenance strings.
/// `roots_declared` gates what a step may NAME as a root; `refs_declared` gates
/// what the walk may FOLLOW out of the bytes it stages. They differ by exactly the
/// payloads (APPLICATIONS.md §B.8), and keeping them apart is the point: a payload
/// is a legitimate root because a step named it, and a payload REACHED from an
/// ordinary input is a td-built output embedding a foreign path, which is the case
/// §B.8 exists to red rather than to stage silently.
fn runtime_store_closure(
    candidates: &BTreeMap<String, PathBuf>,
    roots_declared: &BTreeSet<String>,
    refs_declared: &BTreeSet<String>,
    roots: &[String],
    target_store: &str,
) -> Result<BTreeSet<String>, String> {
    if roots.is_empty() {
        return Err("stageRuntimeClosure: roots is empty".into());
    }
    let target_store = Path::new(target_store);
    for root in roots {
        if !roots_declared.contains(root) {
            return Err(format!(
                "stageRuntimeClosure: root {root} is not a declared recipe input"
            ));
        }
        if Path::new(root).parent() != Some(target_store)
            || crate::store::hash_from_store_path(root).is_none()
        {
            return Err(format!(
                "stageRuntimeClosure: root {root} is not a top-level item in {}",
                target_store.display()
            ));
        }
        if !candidates.contains_key(root) {
            return Err(format!(
                "stageRuntimeClosure: declared root {root} is absent from the staged store"
            ));
        }
    }

    let candidate_paths: Vec<String> = candidates.keys().cloned().collect();
    let mut scanner = crate::scan::Scanner::new(&candidate_paths)
        .map_err(|e| format!("stageRuntimeClosure: build reference index: {e}"))?;
    let mut closure = BTreeSet::new();
    let mut pending: std::collections::VecDeque<String> = roots.iter().cloned().collect();
    while let Some(path) = pending.pop_front() {
        if !closure.insert(path.clone()) {
            continue;
        }
        let physical = candidates.get(&path).ok_or_else(|| {
            format!("stageRuntimeClosure: reachable store item {path} is not staged")
        })?;
        scanner.reset();
        let absolute_refs = scan_runtime_store_refs(&mut scanner, physical, target_store)?;
        let mut references: BTreeSet<String> = scanner.refs().into_iter().collect();
        references.extend(absolute_refs);
        // An item declared ONLY as a root — nameable, but not an ordinary input —
        // may reach the whole declared set; an ordinary input may reach only
        // ordinary inputs. Today the two sets differ by exactly the payloads, so
        // this is §B.8's rule stated without naming it: an application may name
        // its runtime and its own store path (an absolute RUNPATH into itself is
        // the ordinary shape), while an ordinary input dragging a payload into
        // the staged tree still reds. For a build with no payload the sets are
        // equal, the difference is empty, and nothing changes.
        let reachable = if roots_declared.contains(&path) && !refs_declared.contains(&path) {
            roots_declared
        } else {
            refs_declared
        };
        for reference in references {
            if !reachable.contains(&reference) {
                return Err(format!(
                    "stageRuntimeClosure: {path} references undeclared recipe input {reference}"
                ));
            }
            if Path::new(&reference).parent() != Some(target_store) {
                return Err(format!(
                    "stageRuntimeClosure: {path} references {reference} outside active store {}",
                    target_store.display()
                ));
            }
            if !closure.contains(&reference) {
                pending.push_back(reference);
            }
        }
    }
    Ok(closure)
}

fn scan_runtime_store_refs(
    scanner: &mut crate::scan::Scanner,
    root: &Path,
    target_store: &Path,
) -> Result<BTreeSet<String>, String> {
    let mut absolute_refs = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("stageRuntimeClosure: stat {}: {e}", path.display()))?;
        if metadata.file_type().is_dir() {
            if is_debug_companion_dir(&path) {
                continue;
            }
            let entries = fs::read_dir(&path)
                .map_err(|e| format!("stageRuntimeClosure: read dir {}: {e}", path.display()))?;
            for entry in entries {
                let entry = entry.map_err(|e| {
                    format!("stageRuntimeClosure: read dir {}: {e}", path.display())
                })?;
                pending.push(entry.path());
            }
            continue;
        }
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .map_err(|e| format!("stageRuntimeClosure: readlink {}: {e}", path.display()))?;
            scan_runtime_fragment(
                scanner,
                target.as_os_str().as_encoded_bytes(),
                &path,
                "symlink",
                target_store,
                &mut absolute_refs,
            )?;
            continue;
        }
        if !metadata.file_type().is_file() {
            continue;
        }
        let (interp, run_paths, needed) = crate::elf::runtime_link_search(&path)
            .map_err(|e| format!("stageRuntimeClosure: inspect {}: {e}", path.display()))?;
        if let Some(interp) = interp {
            scan_runtime_fragment(
                scanner,
                interp.as_bytes(),
                &path,
                "interpreter",
                target_store,
                &mut absolute_refs,
            )?;
        }
        for run_path in run_paths {
            scan_runtime_fragment(
                scanner,
                run_path.as_bytes(),
                &path,
                "run-path",
                target_store,
                &mut absolute_refs,
            )?;
        }
        for needed in needed {
            scan_runtime_fragment(
                scanner,
                needed.as_bytes(),
                &path,
                "needed entry",
                target_store,
                &mut absolute_refs,
            )?;
        }
    }
    Ok(absolute_refs)
}

fn is_debug_companion_dir(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("debug")
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("lib")
}

fn scan_runtime_fragment(
    scanner: &mut crate::scan::Scanner,
    fragment: &[u8],
    path: &Path,
    kind: &str,
    target_store: &Path,
    absolute_refs: &mut BTreeSet<String>,
) -> Result<(), String> {
    for store_dir in [target_store, Path::new("/gnu/store")] {
        if let Some(reference) = absolute_store_reference(fragment, store_dir) {
            absolute_refs.insert(reference);
        }
    }
    std::io::Write::write_all(scanner, fragment)
        .and_then(|()| std::io::Write::write_all(scanner, b"\0"))
        .map_err(|e| {
            format!(
                "stageRuntimeClosure: scan {kind} {}: {e}",
                path.display()
            )
        })
}

fn absolute_store_reference(fragment: &[u8], store_dir: &Path) -> Option<String> {
    let prefix = store_dir.as_os_str().as_encoded_bytes();
    let rest = fragment.strip_prefix(prefix)?.strip_prefix(b"/")?;
    let component_len = rest
        .iter()
        .take_while(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
        })
        .count();
    let component = std::str::from_utf8(rest.get(..component_len)?).ok()?;
    let reference = format!("{}/{}", store_dir.display(), component);
    crate::store::name_from_store_path(&reference)?;
    Some(reference)
}

fn copy_store_item_writable(from: &Path, to: &Path) -> Result<(), String> {
    match fs::symlink_metadata(to) {
        Ok(_) => {
            return Err(format!(
                "stageRuntimeClosure: destination already exists: {}",
                to.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "stageRuntimeClosure: stat {}: {e}",
                to.display()
            ));
        }
    }
    let metadata = fs::symlink_metadata(from)
        .map_err(|e| format!("stageRuntimeClosure: stat {}: {e}", from.display()))?;
    if metadata.file_type().is_dir() {
        return copy_tree_writable(from, to);
    }
    let parent = to.parent().ok_or_else(|| {
        format!(
            "stageRuntimeClosure: destination {} has no parent",
            to.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("stageRuntimeClosure: mkdir {}: {e}", parent.display()))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(from)
            .map_err(|e| format!("stageRuntimeClosure: readlink {}: {e}", from.display()))?;
        return std::os::unix::fs::symlink(target, to)
            .map_err(|e| format!("stageRuntimeClosure: symlink {}: {e}", to.display()));
    }
    fs::copy(from, to)
        .map_err(|e| format!("stageRuntimeClosure: copy {} -> {}: {e}", from.display(), to.display()))?;
    let mode = metadata.permissions().mode();
    fs::set_permissions(to, fs::Permissions::from_mode((mode & 0o777) | 0o200))
        .map_err(|e| format!("stageRuntimeClosure: chmod {}: {e}", to.display()))
}

/// The two declared sets `stageRuntimeClosure` gates on: what a step may NAME as
/// a root, and what an ORDINARY item may reach.
///
/// §B.8 names this step a permitted payload consumer, so payloads join the ROOT
/// set. They stay out of the REFERENCE set, or an ordinary input embedding a
/// payload's store path would drag it into the staged tree with no step naming
/// it — `copy_store_item_writable` writes it out with its exec bits on a
/// writable mount, `noexec` undone by a different door.
///
/// Split out because `stage_runtime_closure` reads the real store dir and so
/// cannot be unit-tested; this is where the two sets are decided.
fn declared_sets(
    inputs: &[(String, String)],
    payloads: &[(String, String)],
) -> (BTreeSet<String>, BTreeSet<String>) {
    (
        inputs
            .iter()
            .chain(payloads)
            .map(|(_, path)| path.clone())
            .collect(),
        inputs.iter().map(|(_, path)| path.clone()).collect(),
    )
}

fn stage_runtime_closure(
    inputs: &[(String, String)],
    payloads: &[(String, String)],
    roots: &[String],
    dest: &Path,
) -> Result<BTreeSet<String>, String> {
    let target_store = crate::store::store_dir();
    let all: Vec<(String, String)> = inputs.iter().chain(payloads).cloned().collect();
    let candidates = runtime_candidate_index(&all, &target_store)?;
    let (roots_declared, refs_declared) = declared_sets(inputs, payloads);
    stage_runtime_closure_from_index(
        &candidates,
        &roots_declared,
        &refs_declared,
        roots,
        &target_store,
        dest,
    )
}

fn stage_runtime_closure_from_index(
    candidates: &BTreeMap<String, PathBuf>,
    roots_declared: &BTreeSet<String>,
    refs_declared: &BTreeSet<String>,
    roots: &[String],
    target_store: &str,
    dest: &Path,
) -> Result<BTreeSet<String>, String> {
    let closure = runtime_store_closure(
        candidates,
        roots_declared,
        refs_declared,
        roots,
        target_store,
    )?;
    for path in &closure {
        let relative = Path::new(path)
            .strip_prefix("/")
            .map_err(|_| format!("stageRuntimeClosure: store path is not absolute: {path}"))?;
        let physical = candidates.get(path).ok_or_else(|| {
            format!("stageRuntimeClosure: reachable store item {path} is not staged")
        })?;
        copy_store_item_writable(physical, &dest.join(relative))?;
    }
    Ok(closure)
}

fn read_application_metadata(
    package: &Path,
    relative: &str,
    limit: usize,
) -> Result<String, String> {
    let package_metadata = fs::symlink_metadata(package).map_err(|error| {
        format!(
            "compileApplicationTables: stat {}: {error}",
            package.display()
        )
    })?;
    if !package_metadata.file_type().is_dir() {
        return Err(format!(
            "compileApplicationTables: package {} is not a directory",
            package.display()
        ));
    }
    let mut path = package.to_path_buf();
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!(
                "compileApplicationTables: metadata path {relative:?} is not relative"
            ));
        };
        path.push(name);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!("compileApplicationTables: stat {}: {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "compileApplicationTables: {} traverses a symlink",
                path.display()
            ));
        }
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("compileApplicationTables: stat {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "compileApplicationTables: {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(format!(
            "compileApplicationTables: {} is {} bytes; the limit is {limit}",
            path.display(),
            metadata.len()
        ));
    }
    let file = fs::File::open(&path)
        .map_err(|error| format!("compileApplicationTables: open {}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(limit as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("compileApplicationTables: read {}: {error}", path.display()))?;
    if text.len() > limit {
        return Err(format!(
            "compileApplicationTables: {} grew beyond the {limit}-byte limit while being read",
            path.display()
        ));
    }
    Ok(text)
}

fn write_application_table(path: &Path, text: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "compileApplicationTables: output {} has no parent",
            path.display()
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "compileApplicationTables: mkdir {}: {error}",
            parent.display()
        )
    })?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "compileApplicationTables: create {} without replacing it: {error}",
                path.display()
            )
        })?;
    file.write_all(text.as_bytes()).map_err(|error| {
        format!(
            "compileApplicationTables: write {}: {error}",
            path.display()
        )
    })?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|error| {
            format!(
                "compileApplicationTables: chmod {}: {error}",
                path.display()
            )
        })
}

fn compile_application_tables(
    names: &[String],
    packages: &[String],
    runtimes: &[String],
    payloads: &[(String, String)],
    registry_path: &Path,
    launcher_path: &Path,
) -> Result<(), String> {
    compile_application_tables_in(
        names,
        packages,
        runtimes,
        payloads,
        registry_path,
        launcher_path,
        &crate::store::store_dir(),
    )
}

fn compile_application_tables_in(
    names: &[String],
    packages: &[String],
    runtimes: &[String],
    payloads: &[(String, String)],
    registry_path: &Path,
    launcher_path: &Path,
    store: &str,
) -> Result<(), String> {
    if registry_path == launcher_path {
        return Err("compileApplicationTables: registry and launcher outputs are the same".into());
    }
    if names.len() != packages.len() || packages.len() != runtimes.len() {
        return Err(format!(
            "compileApplicationTables: {} names, {} packages and {} runtimes",
            names.len(),
            packages.len(),
            runtimes.len()
        ));
    }
    if packages.len() > td_engine::launcher::MAX_APPLICATIONS {
        return Err(format!(
            "compileApplicationTables: {} packages; the limit is {}",
            packages.len(),
            td_engine::launcher::MAX_APPLICATIONS
        ));
    }
    let prefix = format!("{}/", store.trim_end_matches('/'));
    let payload_paths: BTreeSet<&str> = payloads.iter().map(|(_, path)| path.as_str()).collect();
    let mut seen_paths = BTreeSet::new();
    let mut registry_entries = Vec::with_capacity(packages.len());
    let mut launcher_entries = Vec::with_capacity(packages.len());
    for ((expected_name, package), expected_runtime) in names.iter().zip(packages).zip(runtimes) {
        td_engine::application::validate_application_identity(expected_name)
            .map_err(|error| format!("compileApplicationTables: selected name: {error}"))?;
        let Some(basename) = package.strip_prefix(&prefix) else {
            return Err(format!(
                "compileApplicationTables: package {package} is outside the active store {store}"
            ));
        };
        if basename.contains('/')
            || crate::store::hash_from_store_path(package).is_none()
            || crate::store::name_from_store_path(package).is_none()
        {
            return Err(format!(
                "compileApplicationTables: package {package} is not one canonical store child"
            ));
        }
        if !seen_paths.insert(package.as_str()) {
            return Err(format!(
                "compileApplicationTables: duplicate package path {package}"
            ));
        }
        if !payload_paths.contains(package.as_str()) {
            return Err(format!(
                "compileApplicationTables: package {package} is not a declared payload"
            ));
        }
        let root = Path::new(package);
        let manifest_text = read_application_metadata(
            root,
            "manifest",
            td_engine::application::MAX_MANIFEST_BYTES,
        )?;
        let spec_text = read_application_metadata(
            root,
            "spec",
            td_engine::application_spec::MAX_APPLICATION_SPEC_BYTES,
        )?;
        let launcher_text = read_application_metadata(
            root,
            "exports/launcher.tsv",
            td_engine::launcher::MAX_LAUNCHER_EXPORT_BYTES,
        )?;
        let manifest = td_engine::application::ApplicationManifest::parse(&manifest_text)
            .map_err(|error| format!("compileApplicationTables: {package}/manifest: {error}"))?;
        let spec = td_engine::application_spec::ApplicationSpec::parse(&spec_text)
            .map_err(|error| format!("compileApplicationTables: {package}/spec: {error}"))?;
        let launcher =
            td_engine::launcher::LauncherExport::parse(&launcher_text).map_err(|error| {
                format!("compileApplicationTables: {package}/exports/launcher.tsv: {error}")
            })?;
        if manifest.to_keyfile() != manifest_text || spec.to_keyfile() != spec_text {
            return Err(format!(
                "compileApplicationTables: package {package} metadata is not canonical"
            ));
        }
        if !payload_paths.contains(expected_runtime.as_str()) {
            return Err(format!(
                "compileApplicationTables: runtime {expected_runtime} is not a declared payload"
            ));
        }
        if spec.runtime() != expected_runtime {
            return Err(format!(
                "compileApplicationTables: package {package} names runtime {}, not selected runtime {expected_runtime}",
                spec.runtime()
            ));
        }
        if manifest.name() != expected_name
            || manifest.name() != spec.name()
            || manifest.name() != launcher.name()
        {
            return Err(format!(
                "compileApplicationTables: package {package} identities disagree: selected={expected_name:?}, manifest={:?}, spec={:?}, launcher={:?}",
                manifest.name(),
                spec.name(),
                launcher.name()
            ));
        }
        registry_entries.push((manifest.name().to_string(), package.clone()));
        launcher_entries.push(launcher);
    }
    let registry = td_engine::launcher::ApplicationRegistry::new(registry_entries)
        .map_err(|error| format!("compileApplicationTables: {error}"))?;
    let launcher = td_engine::launcher::LauncherTable::new(launcher_entries)
        .map_err(|error| format!("compileApplicationTables: {error}"))?;
    write_application_table(registry_path, &registry.to_tsv())?;
    write_application_table(launcher_path, &launcher.to_tsv())
}

fn string_array(object: &Json, key: &str) -> Result<Vec<String>, String> {
    let values = object
        .get(key)
        .and_then(Json::as_arr)
        .ok_or_else(|| format!("`{key}' not an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("`{key}' contains a non-string"))
        })
        .collect()
}

fn bytes_contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn bytes_replace_all(hay: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut rest = hay;
    while let Some(pos) = rest.windows(needle.len()).position(|w| w == needle) {
        let Some(before) = rest.get(..pos) else {
            break;
        };
        out.extend_from_slice(before);
        out.extend_from_slice(replacement);
        let next = pos.saturating_add(needle.len());
        rest = rest.get(next..).unwrap_or(&[]);
    }
    out.extend_from_slice(rest);
    out
}

/// Strip the absolute configure prefix (`{prefix}/lib/`) from GNU ld scripts in
/// `dir` so their GROUP/AS_NEEDED members resolve via `-L`/`-B` instead of the
/// build-time store path. Both `*.so` and `*.a` are considered: glibc ships ld
/// scripts under both extensions (e.g. `libc.so`, and a `libm.a` that is a GNU
/// ld script, not a real archive). The `head -c 80` "GNU ld script" content
/// guard is what actually gates a rewrite, so a genuine `ar` archive named
/// `*.a` is skipped untouched — the extension is only a cheap prefilter. This
/// matches the x86_64 toolchain relocator (`toolchain_x86_64.rs`), which has
/// always handled both extensions.
fn relocate_ld_scripts(dir: &Path, prefix: &str) -> Result<(), String> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("relocate ld scripts: read {}: {e}", dir.display())),
    };
    let needle = format!("{prefix}/lib/");
    for entry in rd {
        let entry = entry.map_err(|e| format!("relocate ld scripts: read {}: {e}", dir.display()))?;
        let path = entry.path();
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        if ext != "so" && ext != "a" {
            continue;
        }
        let bytes = fs::read(&path)
            .map_err(|e| format!("relocate ld scripts: read {}: {e}", path.display()))?;
        let head_len = bytes.len().min(80);
        let head = bytes.get(..head_len).unwrap_or(&[]);
        if !bytes_contains(head, b"GNU ld script") {
            continue;
        }
        let fixed = bytes_replace_all(&bytes, needle.as_bytes(), b"");
        if fixed != bytes {
            fs::write(&path, fixed)
                .map_err(|e| format!("relocate ld scripts: write {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn pack_erofs(root: &Path, output: &Path) -> Result<(), String> {
    let image = crate::erofs::build_image(root)
        .map_err(|e| format!("pack erofs {}: {e}", root.display()))?;
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| format!("pack erofs: mkdir {}: {e}", parent.display()))?;
    }
    fs::write(output, image)
        .map_err(|e| format!("pack erofs: write {}: {e}", output.display()))
}

fn valid_artifact_label(label: &str) -> bool {
    !label.is_empty()
        && label != "."
        && label != ".."
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn write_sha256_manifest(
    output: &Path,
    mut entries: Vec<(String, String)>,
) -> Result<(), String> {
    if entries.is_empty() {
        return Err("sha256 manifest: no artifacts".into());
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut manifest = String::from("td-deployment-v1\n");
    let mut previous: Option<&str> = None;
    for (label, path) in &entries {
        if !valid_artifact_label(label) {
            return Err(format!("sha256 manifest: invalid artifact label `{label}'"));
        }
        if previous == Some(label.as_str()) {
            return Err(format!("sha256 manifest: duplicate artifact label `{label}'"));
        }
        let digest = crate::sha256::sha256_file(Path::new(path))
            .map_err(|e| format!("sha256 manifest: hash {path}: {e}"))?;
        manifest.push_str(&digest);
        manifest.push_str("  ");
        manifest.push_str(label);
        manifest.push('\n');
        previous = Some(label);
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| format!("sha256 manifest: mkdir {}: {e}", parent.display()))?;
    }
    fs::write(output, manifest)
        .map_err(|e| format!("sha256 manifest: write {}: {e}", output.display()))
}

pub(crate) const MESBOOT_STEPS_FILE: &str = ".td-steps.json";
pub(crate) const MESBOOT_STEPS_FILE_ENV: &str = "TD_STEPS_FILE";

pub(crate) fn consume_mesboot_steps_file(path: &Path) -> Result<String, String> {
    let steps = fs::read_to_string(path)
        .map_err(|e| format!("read mesboot steps input {}: {e}", path.display()))?;
    fs::remove_file(path)
        .map_err(|e| format!("remove mesboot steps input {}: {e}", path.display()))?;
    Ok(steps)
}

fn create_tool_farm_link(tools: &Path, name: &str, target: &Path) -> Result<(), String> {
    fs::metadata(target)
        .map_err(|e| format!("toolFarm target {}: {e}", target.display()))?;
    let link = tools.join(name);
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(target, &link)
        .map_err(|e| format!("symlink {name} -> {}: {e}", target.display()))
}

/// Read `TD_PAYLOAD_MAP` (payload name → store path) STRICTLY, where
/// `TD_INPUT_MAP`'s read filters non-strings out.
///
/// The map carries a RESTRICTION, so a dropped entry is a payload with no name
/// in the data expander — silently fail-closed, and one refactor from failing
/// open. `sandbox::payload_paths` refuses the identical malformation, and two
/// readers of one variable must not disagree about what it may contain.
fn parse_payload_map(text: &str) -> Result<Vec<(String, String)>, String> {
    let parsed = crate::json::parse(text).map_err(|e| format!("TD_PAYLOAD_MAP JSON: {e}"))?;
    let Json::Obj(kvs) = &parsed else {
        return Err("TD_PAYLOAD_MAP is not a JSON object".into());
    };
    let mut out = Vec::with_capacity(kvs.len());
    for (name, value) in kvs {
        let path = value
            .as_str()
            .ok_or_else(|| format!("TD_PAYLOAD_MAP entry `{name}' is not a string"))?;
        out.push((name.clone(), path.to_string()));
    }
    Ok(out)
}

/// Assembly partitions the two maps by PATH; this is that same refusal made
/// where they are READ, because a drv could reach the builder some other way.
/// A payload aliased into `TD_INPUT_MAP` resolves through `{in:ALIAS}` for a
/// command-bearing step, and the resolver below compares only NAMES — `noexec`
/// still refuses an exec, but not `gcc -I<payload>/include`.
fn refuse_aliased_payloads(
    inputs: &[(String, String)],
    payloads: &[(String, String)],
) -> Result<(), String> {
    for (pname, ppath) in payloads {
        if let Some((iname, _)) = inputs.iter().find(|(_, ipath)| ipath == ppath) {
            return Err(format!(
                "payload `{pname}' is also reachable as input `{iname}' ({ppath}): the payload \
                 channel is disjoint from the input channel (APPLICATIONS.md section B.8)"
            ));
        }
    }
    Ok(())
}

fn collect_runtime_elfs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|e| format!("read debug-tree directory {}: {e}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read debug-tree directory {}: {e}", dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("stat debug-tree entry {}: {e}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_runtime_elfs(&path, out)?;
        } else if metadata.is_file() && crate::elf::is_runtime_elf(&path)? {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_debug_bytes(
    dir: &Path,
    inside_debug: bool,
    seen: &mut std::collections::HashSet<(u64, u64)>,
    total: &mut u64,
) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|e| format!("read debug-size directory {}: {e}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read debug-size directory {}: {e}", dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("stat debug-size entry {}: {e}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let enters_debug = path.file_name() == Some(OsStr::new("debug"))
                && path.parent().and_then(Path::file_name) == Some(OsStr::new("lib"));
            collect_debug_bytes(&path, inside_debug || enters_debug, seen, total)?;
        } else if inside_debug
            && metadata.is_file()
            && seen.insert((metadata.dev(), metadata.ino()))
        {
            *total = total
                .checked_add(metadata.len())
                .ok_or_else(|| format!("debug-size byte total overflow below {}", dir.display()))?;
        }
    }
    Ok(())
}

fn assert_debug_size(root: &Path, report: &Path, scope: &str, ceiling: u64) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!(
            "debug-size root is not a directory: {}",
            root.display()
        ));
    }
    if scope.is_empty()
        || !scope
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("debug-size scope is not a lowercase label: {scope:?}"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0u64;
    collect_debug_bytes(root, false, &mut seen, &mut bytes)?;
    if bytes == 0 {
        return Err(format!(
            "debug-size scope {scope} found no companion bytes below {}",
            root.display()
        ));
    }
    if bytes > ceiling {
        return Err(format!(
            "debug-size scope {scope} uses {bytes} bytes, exceeding compiled ceiling {ceiling}"
        ));
    }
    if let Some(parent) = report
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir debug-size report {}: {e}", parent.display()))?;
    }
    let content = format!(
        "format=1\nscope={scope}\ndebug_bytes={bytes}\nceiling_bytes={ceiling}\n"
    );
    fs::write(report, content)
        .map_err(|e| format!("write debug-size report {}: {e}", report.display()))?;
    fs::set_permissions(report, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("chmod debug-size report {}: {e}", report.display()))?;
    eprintln!("debug-size: scope={scope} bytes={bytes} ceiling={ceiling}");
    Ok(())
}

fn compare_files(left: &Path, right: &Path) -> Result<(), String> {
    let left_meta = fs::metadata(left)
        .map_err(|e| format!("compare stat {}: {e}", left.display()))?;
    let right_meta = fs::metadata(right)
        .map_err(|e| format!("compare stat {}: {e}", right.display()))?;
    if !left_meta.is_file() || !right_meta.is_file() {
        return Err(format!(
            "compare requires two regular files: {} {}",
            left.display(),
            right.display()
        ));
    }
    if left_meta.len() != right_meta.len() {
        return Err(format!(
            "files differ in length: {}={} {}={}",
            left.display(),
            left_meta.len(),
            right.display(),
            right_meta.len()
        ));
    }
    let mut left_file = fs::File::open(left)
        .map_err(|e| format!("compare open {}: {e}", left.display()))?;
    let mut right_file = fs::File::open(right)
        .map_err(|e| format!("compare open {}: {e}", right.display()))?;
    let mut left_buf = vec![0u8; 64 * 1024];
    let mut right_buf = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    while offset < left_meta.len() {
        let width = usize::try_from((left_meta.len() - offset).min(left_buf.len() as u64))
            .map_err(|_| "compare chunk width does not fit usize")?;
        left_file
            .read_exact(
                left_buf
                    .get_mut(..width)
                    .ok_or("compare left chunk width exceeds buffer")?,
            )
            .map_err(|e| format!("compare read {}: {e}", left.display()))?;
        right_file
            .read_exact(
                right_buf
                    .get_mut(..width)
                    .ok_or("compare right chunk width exceeds buffer")?,
            )
            .map_err(|e| format!("compare read {}: {e}", right.display()))?;
        let left_chunk = left_buf
            .get(..width)
            .ok_or("compare left chunk width exceeds buffer")?;
        let right_chunk = right_buf
            .get(..width)
            .ok_or("compare right chunk width exceeds buffer")?;
        if left_chunk != right_chunk {
            let within = left_chunk
                .iter()
                .zip(right_chunk)
                .position(|(a, b)| a != b)
                .ok_or("compare mismatch had no differing byte")?;
            return Err(format!(
                "files differ at byte {}: {} {}",
                offset + within as u64,
                left.display(),
                right.display()
            ));
        }
        offset += width as u64;
    }
    Ok(())
}

fn validate_line_exception_runtime(
    root: &Path,
    runtimes: &[PathBuf],
    exception: td_engine::target_profile::LineAttributionException,
) -> Result<(), String> {
    let target = root.join(exception.runtime_relative_path);
    if !runtimes.iter().any(|runtime| runtime == &target) {
        return Err(format!(
            "named line-attribution exception runtime is absent: {}",
            target.display()
        ));
    }
    let target_metadata = fs::metadata(&target)
        .map_err(|e| format!("stat line-attribution exception {}: {e}", target.display()))?;
    let target_inode = (target_metadata.dev(), target_metadata.ino());
    for runtime in runtimes.iter().filter(|runtime| *runtime != &target) {
        let metadata = fs::metadata(runtime)
            .map_err(|e| format!("stat installed ELF {}: {e}", runtime.display()))?;
        if (metadata.dev(), metadata.ino()) == target_inode {
            return Err(format!(
                "named line-attribution exception {} aliases ordinary runtime {}",
                target.display(),
                runtime.display(),
            ));
        }
    }
    Ok(())
}

fn require_debug_companion_policy(recipe_name: &str) -> Result<(), String> {
    let actual = env::var("TD_DEBUG_COMPANION_POLICY")
        .map_err(|_| "TD_DEBUG_COMPANION_POLICY is not set for debug splitting".to_string())?;
    let expected = td_engine::target_profile::debug_companion_policy(recipe_name);
    if actual != expected {
        return Err(format!(
            "debug companion policy {actual:?} does not match builder policy {expected:?}"
        ));
    }
    Ok(())
}

/// Split every installed executable/shared object below `root`, then verify
/// the runtime and companion as one build-ID-addressed pair. The walker and
/// validation are engine-native; only the declared target `objcopy` executes.
fn split_debug_tree(root: &Path, objcopy: &Path, recipe_name: &str) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!(
            "debug-tree root is not a directory: {}",
            root.display()
        ));
    }
    require_executable_file(
        objcopy
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 objcopy path: {}", objcopy.display()))?,
        "target objcopy",
    )?;
    let debug_root = root.join("lib/debug");
    if debug_root.exists() {
        return Err(format!(
            "debug companion root already exists before split: {}",
            debug_root.display()
        ));
    }
    let mut runtimes = Vec::new();
    collect_runtime_elfs(root, &mut runtimes)?;
    runtimes.sort();
    if runtimes.is_empty() {
        return Err(format!(
            "debug-tree root contains no ET_EXEC/ET_DYN: {}",
            root.display()
        ));
    }
    let line_exception = td_engine::target_profile::line_attribution_exception(recipe_name);
    if let Some(exception) = line_exception {
        validate_line_exception_runtime(root, &runtimes, exception)?;
    }
    let objcopy = objcopy
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 objcopy path: {}", objcopy.display()))?;
    let cwd = root
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 debug-tree root: {}", root.display()))?;
    // Create every companion before stripping any runtime. Installed packages
    // may expose one ELF through several hard links (glibc getconf does); a
    // later in-place objcopy of one name must not become the source for another
    // name's companion.
    let mut companion_by_inode: std::collections::HashMap<(u64, u64), (PathBuf, PathBuf, bool)> =
        std::collections::HashMap::new();
    let mut pairs = Vec::with_capacity(runtimes.len());
    for runtime in &runtimes {
        // Validate the linked identity before changing bytes. A missing or
        // duplicate note therefore cannot leave an apparently paired output.
        crate::elf::read_build_id(runtime)?;
        let metadata = fs::metadata(runtime)
            .map_err(|e| format!("stat installed ELF {}: {e}", runtime.display()))?;
        let inode = (metadata.dev(), metadata.ino());
        let relative = runtime.strip_prefix(root).map_err(|_| {
            format!(
                "runtime {} escaped debug-tree root {}",
                runtime.display(),
                root.display()
            )
        })?;
        let relative_text = relative
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 installed ELF path: {}", relative.display()))?;
        let runtime_line_exception = line_exception
            .filter(|exception| exception.runtime_relative_path == relative_text);
        let debug = debug_root.join(format!("{relative_text}.debug"));
        let parent = debug
            .parent()
            .ok_or_else(|| format!("debug companion has no parent: {}", debug.display()))?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir debug companion parent {}: {e}", parent.display()))?;
        let runtime_text = runtime
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 installed ELF path: {}", runtime.display()))?;
        let debug_text = debug
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 debug companion path: {}", debug.display()))?;
        let canonical_runtime = if let Some((first_debug, first_runtime, first_has_exception)) =
            companion_by_inode.get(&inode)
        {
            if *first_has_exception != runtime_line_exception.is_some() {
                return Err(format!(
                    "named line-attribution exception {} aliases ordinary runtime {}",
                    runtime.display(),
                    first_runtime.display(),
                ));
            }
            fs::hard_link(first_debug, &debug).map_err(|e| {
                format!(
                    "hard-link debug companion {} from {}: {e}",
                    debug.display(),
                    first_debug.display()
                )
            })?;
            first_runtime.clone()
        } else {
            run_cmd(
                objcopy,
                &["--only-keep-debug", runtime_text, debug_text],
                cwd,
                &[],
                &WATCH_PHASE,
            )?;
            let remove_debug_strings = runtime_line_exception.is_some()
                || !crate::elf::debug_line_requires_debug_str(&debug)?;
            let mut prune_args: Vec<String> =
                td_engine::target_profile::ALWAYS_PRUNED_DEBUG_SECTIONS
                    .iter()
                    .map(|section| format!("--remove-section={section}"))
                    .collect();
            // DWARF 2 through 4 carry line-table paths inline. DWARF 5 can
            // reference `.debug_str`, so keep it only when a structurally
            // checked directory/file format declares DW_FORM_strp. The named
            // oversized-line exception deliberately makes no reader claim.
            if remove_debug_strings {
                prune_args.push("--remove-section=.debug_str".into());
            }
            prune_args.push(debug_text.into());
            let prune_arg_refs: Vec<&str> = prune_args.iter().map(String::as_str).collect();
            run_cmd(objcopy, &prune_arg_refs, cwd, &[], &WATCH_PHASE)?;
            fs::set_permissions(&debug, fs::Permissions::from_mode(0o644))
                .map_err(|e| format!("chmod debug companion {}: {e}", debug.display()))?;
            if let Some(exception) = runtime_line_exception {
                let bytes = fs::metadata(&debug)
                    .map_err(|e| format!("stat debug companion {}: {e}", debug.display()))?
                    .len();
                if bytes > exception.max_companion_bytes {
                    return Err(format!(
                        "debug companion {} uses {bytes} bytes, exceeding named line-attribution exception ceiling {}",
                        debug.display(),
                        exception.max_companion_bytes,
                    ));
                }
            }
            companion_by_inode.insert(
                inode,
                (
                    debug.clone(),
                    runtime.clone(),
                    runtime_line_exception.is_some(),
                ),
            );
            runtime.clone()
        };
        pairs.push((
            runtime.clone(),
            debug,
            canonical_runtime,
            inode,
            runtime_line_exception,
        ));
    }
    let mut stripped_inodes = std::collections::HashSet::new();
    for (_, _, canonical_runtime, original_inode, _) in &pairs {
        if !stripped_inodes.insert(*original_inode) {
            continue;
        }
        let runtime_text = canonical_runtime
            .to_str()
            .ok_or_else(|| {
                format!(
                    "non-UTF-8 installed ELF path: {}",
                    canonical_runtime.display()
                )
            })?;
        run_cmd(
            objcopy,
            &["--strip-all", runtime_text],
            cwd,
            &[],
            &WATCH_PHASE,
        )?;
    }
    // GNU objcopy currently preserves a multiply-linked inode, but enforce the
    // installed relation rather than depending on that implementation detail.
    // If objcopy replaced the canonical name, every alias is reconnected to the
    // stripped bytes before pair validation.
    for (runtime, _, canonical_runtime, _, _) in &pairs {
        if runtime == canonical_runtime {
            continue;
        }
        fs::remove_file(runtime)
            .map_err(|e| format!("remove pre-strip hard-link alias {}: {e}", runtime.display()))?;
        fs::hard_link(canonical_runtime, runtime).map_err(|e| {
            format!(
                "restore runtime hard link {} from {}: {e}",
                runtime.display(),
                canonical_runtime.display()
            )
        })?;
    }
    for (runtime, debug, _, _, runtime_line_exception) in &pairs {
        if let Some(exception) = runtime_line_exception {
            crate::elf::assert_debug_pair_with_line_limit(
                runtime,
                debug,
                exception.max_line_section_bytes,
                exception.require_complete_line_strings,
            )?;
        } else {
            crate::elf::assert_debug_pair(runtime, debug)?;
        }
    }
    let exceptions = td_engine::target_profile::output_assembly_exceptions(recipe_name);
    if !exceptions.is_empty() {
        let marker = debug_root.join(".td-assembly-exception");
        let mut content = format!("format=1\noutput={recipe_name}\n");
        for (index, (source, reason)) in exceptions.iter().enumerate() {
            content.push_str(&format!("exception.{index}.source={source}\n"));
            content.push_str(&format!("exception.{index}.reason={reason}\n"));
        }
        fs::write(&marker, content)
            .map_err(|e| format!("write assembly-exception marker {}: {e}", marker.display()))?;
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("chmod assembly-exception marker {}: {e}", marker.display()))?;
    }
    if let Some(exception) = line_exception {
        let marker = debug_root.join(".td-line-attribution-exception");
        let content = format!(
            "format=1\noutput={recipe_name}\nruntime={}\nreader_ceiling_bytes={}\nadmitted_ceiling_bytes={}\ncompanion_ceiling_bytes={}\nreason={}\n",
            exception.runtime_relative_path,
            td_engine::target_profile::DEFAULT_PROFILE_LINE_SECTION_BYTES,
            exception.max_line_section_bytes,
            exception.max_companion_bytes,
            exception.reason,
        );
        fs::write(&marker, content).map_err(|e| {
            format!(
                "write line-attribution-exception marker {}: {e}",
                marker.display()
            )
        })?;
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).map_err(|e| {
            format!(
                "chmod line-attribution-exception marker {}: {e}",
                marker.display()
            )
        })?;
    }
    Ok(())
}

/// mesboot-build — td's bootstrap-RUNG build "system" (#378 slices 2+3; sibling
/// of `run`/`run_rust`/`run_cmake`/`run_stage0`). Executes the recipe's typed
/// steps (materialized from the drv's `TD_STEPS` data; see recipes/src/types.rs
/// `Step`) over the staged inputs (TD_INPUT_MAP: lock name → store path,
/// `{in:NAME}` templates). The engine interprets NO shell: a `run` step spawns
/// its argv directly with the EXACT env given (cleared otherwise — the ladder's
/// `env -i` + `MAKEFLAGS=` scrubbing as engine policy); configure scripts run
/// because their argv names the declared bash input. `{jobs}` is the available
/// parallelism (execution-time, not baked into the drv — the double-build repro
/// oracle guards it).
pub fn run_mesboot() -> Result<(), String> {
    let out = env::var("out").map_err(|_| "out not set".to_string())?;
    let recipe_name =
        env::var("TD_RECIPE_NAME").map_err(|_| "TD_RECIPE_NAME not set".to_string())?;
    let steps_file = env::var_os(MESBOOT_STEPS_FILE_ENV)
        .ok_or_else(|| format!("{MESBOOT_STEPS_FILE_ENV} not set"))?;
    let steps_path = Path::new(&steps_file);
    let steps_json = consume_mesboot_steps_file(steps_path)?;
    let map_json = env::var("TD_INPUT_MAP").map_err(|_| "TD_INPUT_MAP not set".to_string())?;
    let steps = crate::json::parse(&steps_json).map_err(|e| {
        format!(
            "TD_STEPS file {} ({} bytes) JSON: {e}",
            steps_path.display(),
            steps_json.len()
        )
    })?;
    let steps = steps.as_arr().ok_or("TD_STEPS file is not a JSON array")?;
    let map = crate::json::parse(&map_json).map_err(|e| format!("TD_INPUT_MAP JSON: {e}"))?;
    let inputs: Vec<(String, String)> = match &map {
        Json::Obj(kvs) => kvs
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|p| (k.clone(), p.to_string())))
            .collect(),
        _ => return Err("TD_INPUT_MAP is not a JSON object".into()),
    };
    // ABSENT is the ordinary case and means no payload: assembly emits this only for
    // a recipe that declares one, which is what keeps every existing derivation's
    // spec byte-identical.
    let payloads = match env::var("TD_PAYLOAD_MAP") {
        Err(_) => Vec::new(),
        Ok(text) => parse_payload_map(&text)?,
    };
    refuse_aliased_payloads(&inputs, &payloads)?;
    let root = env::current_dir()
        .map_err(|e| format!("cwd: {e}"))?
        .to_string_lossy()
        .into_owned();
    let ctx = StepCtx {
        src: format!("{root}/src"),
        tools: format!("{root}/tools"),
        jobs: crate::check_memory::build_jobs().to_string(),
        out: out.clone(),
        root,
        inputs,
        payloads,
    };
    for d in [&ctx.src, &ctx.tools, &ctx.out] {
        fs::create_dir_all(d).map_err(|e| format!("mkdir {d}: {e}"))?;
    }

    let field = |o: &Json, k: &str| -> Result<String, String> {
        o.get(k)
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("mesboot step: missing/non-string `{k}'"))
    };
    let pairs = |o: &Json, k: &str| -> Result<Vec<(String, String)>, String> {
        let a = o.get(k).and_then(Json::as_arr).ok_or_else(|| format!("mesboot step: `{k}' not an array"))?;
        a.iter()
            .map(|p| {
                let pa = p.as_arr().filter(|pa| pa.len() == 2).ok_or("mesboot step: pair is not a 2-array")?;
                match (pa.first().and_then(Json::as_str), pa.get(1).and_then(Json::as_str)) {
                    (Some(a), Some(b)) => Ok((a.to_string(), b.to_string())),
                    _ => Err("mesboot step: non-string pair".to_string()),
                }
            })
            .collect()
    };
    for (i, step) in steps.iter().enumerate() {
        let err = |m: String| format!("mesboot step {}: {m}", i + 1);
        if let Some(o) = step.get("run") {
            // `glob:PAT` argv elements expand to the SORTED matches (one `*`,
            // basename component only) — `ar r out.a tg/*.o`-shaped rung steps
            // without any shell. Zero matches is a hard error (a silent empty
            // splice would turn an install step into a no-op).
            let mut argv: Vec<String> = Vec::new();
            for a in ctx
                .expand_all(&string_array(o, "argv").map_err(err)?)
                .map_err(err)?
            {
                match a.strip_prefix("glob:") {
                    None => argv.push(a),
                    Some(pat) => {
                        ctx.check_glob_dir(pat).map_err(err)?;
                        let mut hits = glob_one_star(pat).map_err(err)?;
                        if hits.is_empty() {
                            return Err(err(format!("glob:{pat} matched nothing")));
                        }
                        hits.sort();
                        argv.extend(hits);
                    }
                }
            }
            let dir = ctx.expand(&field(o, "dir")?).map_err(err)?;
            let mut envs: Vec<(String, String)> = Vec::new();
            for (k, v) in pairs(o, "env")? {
                envs.push((k, ctx.expand(&v).map_err(err)?));
            }
            let (prog, rest) = argv
                .split_first()
                .ok_or_else(|| err("run: empty argv".into()))?;
            let restv: Vec<&str> = rest.iter().map(String::as_str).collect();
            run_cmd(prog, &restv, &dir, &envs, &WATCH_PHASE)?;
        } else if let Some(o) = step.get("toolFarm") {
            let links = o
                .as_arr()
                .ok_or_else(|| err("toolFarm: not an array".into()))?;
            for p in links {
                let pa = p.as_arr().filter(|pa| pa.len() == 2).ok_or_else(|| err("toolFarm: pair".into()))?;
                let (name, target) = match (pa.first().and_then(Json::as_str), pa.get(1).and_then(Json::as_str)) {
                    (Some(a), Some(b)) => (a.to_string(), ctx.expand(b).map_err(err)?),
                    _ => return Err(err("toolFarm: non-string pair".into())),
                };
                create_tool_farm_link(Path::new(&ctx.tools), &name, Path::new(&target))
                    .map_err(err)?;
            }
        } else if let Some(o) = step.get("writeFile") {
            let path = ctx.expand(&field(o, "path")?).map_err(err)?;
            let content = ctx.expand(&field(o, "content")?).map_err(err)?;
            let exec = o.get("exec").is_some_and(Json::is_true);
            if let Some(parent) = Path::new(&path).parent() {
                fs::create_dir_all(parent).map_err(|e| err(format!("mkdir {}: {e}", parent.display())))?;
            }
            fs::write(&path, content).map_err(|e| err(format!("write {path}: {e}")))?;
            let mode = if exec { 0o755 } else { 0o644 };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                .map_err(|e| err(format!("chmod {path}: {e}")))?;
        } else if let Some(o) = step.get("unpack") {
            // Engine-native source unpack (re #469): td's own std-only
            // tar/gzip/bzip2/xz readers — the rungs declare no unpacker
            // packages, so `{in:tar}`-shaped host edges are gone from the
            // graph. keepTop=false strips the unique top-level dir
            // (`--strip-components=1`); anything else is a hard error.
            let input = ctx.expand_data(&field(o, "input")?).map_err(err)?;
            let dest = ctx.expand(&field(o, "dest")?).map_err(err)?;
            let keep_top = o.get("keepTop").is_some_and(Json::is_true);
            crate::tar::unpack_archive(Path::new(&input), Path::new(&dest), keep_top)
                .map_err(err)?;
        } else if let Some(o) = step.get("mesBoot") {
            // The engine-native mes rung (re #469): configure + bootstrap +
            // install of the pinned mes tarball, spawning only stage0 recipe
            // outputs and the just-built mes — no host shell or coreutils.
            let source = ctx.expand(&field(o, "source")?).map_err(err)?;
            let nyacc = ctx.expand(&field(o, "nyacc")?).map_err(err)?;
            let stage0 = ctx.expand(&field(o, "stage0")?).map_err(err)?;
            crate::mes_boot::run(&source, &nyacc, &stage0, &ctx.out).map_err(err)?;
        } else if let Some(o) = step.get("copyFiles") {
            let dest = ctx.expand(&field(o, "dest")?).map_err(err)?;
            for f in ctx
                .expand_all(&string_array(o, "files").map_err(err)?)
                .map_err(err)?
            {
                copy_file_writable(Path::new(&f), Path::new(&dest)).map_err(err)?;
            }
        } else if let Some(o) = step.get("copyTree") {
            // `from` is the DATA side and may name a payload; `dest` is inside this
            // build's own output and may not.
            let from = ctx.expand_data(&field(o, "from")?).map_err(err)?;
            let dest = ctx.expand(&field(o, "dest")?).map_err(err)?;
            copy_tree_writable(Path::new(&from), Path::new(&dest)).map_err(err)?;
        } else if let Some(o) = step.get("copyFile") {
            // `file` is the DATA side and may name a payload; `to` is an exact
            // path inside this build's own output and may not.
            let file = ctx.expand_data(&field(o, "file")?).map_err(err)?;
            let to = ctx.expand(&field(o, "to")?).map_err(err)?;
            let exec = o.get("exec").is_some_and(Json::is_true);
            copy_single_file(Path::new(&ctx.out), Path::new(&file), Path::new(&to), exec)
                .map_err(err)?;
        } else if let Some(o) = step.get("splitDebugTree") {
            let root = ctx.expand(&field(o, "root")?).map_err(err)?;
            let objcopy = ctx.expand(&field(o, "objcopy")?).map_err(err)?;
            require_debug_companion_policy(&recipe_name).map_err(err)?;
            split_debug_tree(Path::new(&root), Path::new(&objcopy), &recipe_name).map_err(err)?;
        } else if let Some(o) = step.get("assertDebugSize") {
            let root = ctx.expand(&field(o, "root")?).map_err(err)?;
            let report = ctx.expand(&field(o, "report")?).map_err(err)?;
            let scope = field(o, "scope")?;
            let ceiling = match o.get("ceiling") {
                Some(Json::Num(number)) => number
                    .parse::<u64>()
                    .map_err(|e| err(format!("assertDebugSize.ceiling: {e}")))?,
                _ => return Err(err("assertDebugSize.ceiling: missing/non-number".into())),
            };
            assert_debug_size(Path::new(&root), Path::new(&report), &scope, ceiling)
                .map_err(err)?;
        } else if let Some(o) = step.get("compareFiles") {
            let left = ctx.expand(&field(o, "left")?).map_err(err)?;
            let right = ctx.expand(&field(o, "right")?).map_err(err)?;
            compare_files(Path::new(&left), Path::new(&right)).map_err(err)?;
        } else if let Some(o) = step.get("stageRuntimeClosure") {
            let roots = ctx
                .expand_data_all(&string_array(o, "roots").map_err(err)?)
                .map_err(err)?;
            let dest = ctx.expand(&field(o, "dest")?).map_err(err)?;
            stage_runtime_closure(&ctx.inputs, &ctx.payloads, &roots, Path::new(&dest))
                .map_err(err)?;
        } else if let Some(o) = step.get("compileApplicationTables") {
            let names = string_array(o, "names").map_err(err)?;
            let packages = ctx
                .expand_data_all(&string_array(o, "packages").map_err(err)?)
                .map_err(err)?;
            let runtimes = ctx
                .expand_data_all(&string_array(o, "runtimes").map_err(err)?)
                .map_err(err)?;
            let registry = ctx.expand(&field(o, "registry")?).map_err(err)?;
            let launcher = ctx.expand(&field(o, "launcher")?).map_err(err)?;
            compile_application_tables(
                &names,
                &packages,
                &runtimes,
                &ctx.payloads,
                Path::new(&registry),
                Path::new(&launcher),
            )
                .map_err(err)?;
        } else if let Some(o) = step.get("packErofs") {
            let root = ctx.expand(&field(o, "root")?).map_err(err)?;
            let output = ctx.expand(&field(o, "output")?).map_err(err)?;
            pack_erofs(Path::new(&root), Path::new(&output)).map_err(err)?;
        } else if let Some(o) = step.get("sha256Manifest") {
            let output = ctx.expand(&field(o, "output")?).map_err(err)?;
            let mut entries = Vec::new();
            for (label, path) in pairs(o, "entries")? {
                entries.push((label, ctx.expand(&path).map_err(err)?));
            }
            write_sha256_manifest(Path::new(&output), entries).map_err(err)?;
        } else if let Some(o) = step.get("symlink") {
            let target = ctx.expand(&field(o, "target")?).map_err(err)?;
            let link = ctx.expand(&field(o, "link")?).map_err(err)?;
            // Create the link's parent (every sibling step does), so a symlink into a
            // not-yet-made dir does not red ENOENT; then replace any existing entry.
            if let Some(parent) = Path::new(&link).parent() {
                fs::create_dir_all(parent).map_err(|e| err(format!("mkdir {}: {e}", parent.display())))?;
            }
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| err(format!("symlink {link} -> {target}: {e}")))?;
        } else if let Some(p) = step.get("mkDir") {
            let path = ctx
                .expand(p.as_str().ok_or_else(|| err("mkDir: not a string".into()))?)
                .map_err(err)?;
            fs::create_dir_all(&path).map_err(|e| err(format!("mkdir {path}: {e}")))?;
        } else if let Some(o) = step.get("truncate") {
            let path = ctx.expand(&field(o, "path")?).map_err(err)?;
            let raw = field(o, "bytes")?;
            let bytes: u64 = raw
                .parse()
                .map_err(|_| err(format!("truncate {path}: {raw:?} is not a byte count")))?;
            if let Some(parent) = Path::new(&path).parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| err(format!("mkdir {}: {e}", parent.display())))?;
            }
            // Create-or-resize, and the length is READ BACK: `set_len` past a
            // filesystem's limit is the failure a disk image cares about, and a
            // destination shorter than asked for is a partition table written
            // over the end of it.
            let f = fs::File::create(&path)
                .map_err(|e| err(format!("truncate {path}: create: {e}")))?;
            f.set_len(bytes)
                .map_err(|e| err(format!("truncate {path} to {bytes}: {e}")))?;
            let got = f
                .metadata()
                .map_err(|e| err(format!("truncate {path}: stat: {e}")))?
                .len();
            if got != bytes {
                return Err(err(format!("truncate {path}: asked {bytes}, got {got}")));
            }
        } else if let Some(o) = step.get("patchShebangs") {
            let dir = ctx.expand(&field(o, "dir")?).map_err(err)?;
            let shell = ctx.expand(&field(o, "shell")?).map_err(err)?;
            patch_shebangs(Path::new(&dir), &shell).map_err(err)?;
        } else if let Some(o) = step.get("relocateLdScripts") {
            let dir = ctx.expand(&field(o, "dir")?).map_err(err)?;
            let prefix = ctx.expand(&field(o, "prefix")?).map_err(err)?;
            relocate_ld_scripts(Path::new(&dir), &prefix).map_err(err)?;
        } else if let Some(o) = step.get("require") {
            let exec = o.get("exec").is_some_and(Json::is_true);
            for p in ctx
                .expand_all(&string_array(o, "paths").map_err(err)?)
                .map_err(err)?
            {
                let meta = fs::metadata(&p)
                    .map_err(|_| err(format!("required product missing: {p}")))?;
                if exec && (!meta.is_file() || meta.permissions().mode() & 0o111 == 0) {
                    return Err(err(format!("required product not an executable file: {p}")));
                }
            }
        } else if let Some(o) = step.get("assertStatic") {
            // Runtime-provenance gate (re #469): each product must be a fully
            // static ELF -- no host loader (PT_INTERP), no host libc (DT_NEEDED),
            // no run-path. A dynamically linked tcc/make/yacc would pull a host
            // loader + glibc in at run time; fail closed here naming the leak.
            for p in ctx
                .expand_all(&string_array(o, "paths").map_err(err)?)
                .map_err(err)?
            {
                crate::elf::assert_static(Path::new(&p)).map_err(err)?;
            }
        } else if let Some(o) = step.get("validateStaticApplication") {
            let entry = field(o, "entry")?;
            let runtime_name = field(o, "runtime")?;
            let runtime = ctx
                .payloads
                .iter()
                .find(|(name, _)| name == &runtime_name)
                .map(|(_, path)| path)
                .ok_or_else(|| {
                    err(format!(
                        "application runtime {runtime_name:?} is not a declared payload"
                    ))
                })?;
            validate_static_application(Path::new(&ctx.out), &entry, Path::new(runtime))
                .map_err(err)?;
        } else if let Some(o) = step.get("validateDynamicApplication") {
            let entry = field(o, "entry")?;
            let runtime_name = field(o, "runtime")?;
            let library_paths = string_array(o, "libraryPaths").map_err(err)?;
            let optional_targets = string_array(o, "optionalTargets").map_err(err)?;
            let optional_links = match o.get("optionalLinks") {
                Some(Json::Num(number)) => number
                    .parse::<usize>()
                    .map_err(|error| err(format!("optionalLinks: {error}")))?,
                _ => return Err(err("optionalLinks: missing/non-number".into())),
            };
            let runtime = ctx
                .payloads
                .iter()
                .find(|(name, _)| name == &runtime_name)
                .map(|(_, path)| path)
                .ok_or_else(|| {
                    err(format!(
                        "application runtime {runtime_name:?} is not a declared payload"
                    ))
                })?;
            crate::application::validate_dynamic_application(
                Path::new(&ctx.out),
                &entry,
                Path::new(runtime),
                &library_paths,
                &optional_targets,
                optional_links,
            )
            .map_err(err)?;
        } else if let Some(o) = step.get("substituteText") {
            // Host-free `patch`/`sed` (re #469): literal, fail-closed text edits
            // in pure Rust. `parse_substitute_edits` expands ONLY `file`; the
            // `from`/`to` source text stays literal (C braces / `{…}` pass
            // through untouched). `apply_text_edits` requires each edit's exact
            // occurrence count, so a drift in the pinned source reds the rung
            // instead of silently no-op'ing; edits apply in order.
            let (file, edits) = parse_substitute_edits(&ctx, o).map_err(err)?;
            let content = fs::read_to_string(&file)
                .map_err(|e| err(format!("substituteText: read {file}: {e}")))?;
            let out = apply_text_edits(&file, content, &edits).map_err(err)?;
            // Some tarballs ship source files READ-ONLY (GNU patch's pch.c is
            // 0444); write_preserving_mode grants owner-write for the rewrite and
            // restores the original mode, so the tree differs only in content.
            write_preserving_mode(Path::new(&file), out.as_bytes())
                .map_err(|e| err(format!("substituteText: write {file}: {e}")))?;
        } else {
            return Err(err("unknown step kind".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_git_fixture() -> (String, Vec<CargoGitSource>) {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let source = format!(
            "git+https://example.invalid/example?rev={commit}#{commit}"
        );
        let declaration = crate::json::parse(&format!(
            r#"[{{"source":"{source}","input":"example-git-source","packages":[{{"name":"gitdep","version":"1.2.3","path":"crate"}}]}}]"#
        ))
        .unwrap();
        (source, parse_cargo_git_sources(&declaration).unwrap())
    }

    #[test]
    fn cargo_git_sources_require_exact_commits_and_typed_packages() {
        let (source, parsed) = cargo_git_fixture();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].source, source);
        assert_eq!(parsed[0].input, "example-git-source");
        assert_eq!(parsed[0].packages[0].path, "crate");

        for invalid in [
            source.replace("?rev=", "?branch="),
            source.replace(
                "0123456789abcdef0123456789abcdef01234567",
                "0123456789ABCDEF0123456789ABCDEF01234567",
            ),
            source.replace(
                "#0123456789abcdef0123456789abcdef01234567",
                "#fedcba9876543210fedcba9876543210fedcba98",
            ),
            "git+http://example.invalid/example?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef0123456789abcdef01234567".into(),
        ] {
            let declaration = crate::json::parse(&format!(
                r#"[{{"source":"{invalid}","input":"example-git-source","packages":[{{"name":"gitdep","version":"1.2.3","path":"crate"}}]}}]"#
            ))
            .unwrap();
            assert!(parse_cargo_git_sources(&declaration).is_err(), "{invalid}");
        }
    }

    #[test]
    fn rust_runner_rejects_git_lock_entries_without_declarations() {
        let (source, declared) = cargo_git_fixture();
        let lock = format!(
            "version = 4\n\n[[package]]\nname = \"gitdep\"\nversion = \"1.2.3\"\nsource = \"{source}\"\n"
        );
        let error = validate_runner_cargo_lock_sources(Some(lock.as_bytes()), &[])
            .unwrap_err();
        assert!(error.contains("undeclared Git dependency"), "{error}");
        validate_runner_cargo_lock_sources(Some(lock.as_bytes()), &declared).unwrap();
        assert!(validate_runner_cargo_lock_sources(None, &declared).is_err());
    }

    #[test]
    fn cargo_git_package_paths_refuse_symlink_traversal() {
        let base = std::env::temp_dir().join(format!(
            "td-cargo-git-path-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("real/nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        assert_eq!(
            cargo_git_package_root(&root, "real/nested").unwrap(),
            root.join("real/nested")
        );
        let error = cargo_git_package_root(&root, "linked").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(cargo_git_package_root(&root, "../outside").is_err());
        let _ = fs::remove_dir_all(&base);
    }

    /// `copyFile` places one payload byte-for-byte under a reviewed name with a
    /// recipe-chosen mode, and refuses what a data copy must: a symlink source,
    /// an occupied destination, and any destination that is not a plain
    /// absolute path strictly inside the build output — outside it, `..`
    /// through it, the output itself, a relative spelling, or a symlinked
    /// component below it.
    #[test]
    fn copy_file_places_one_regular_file_strictly() {
        let base = std::env::temp_dir().join(format!("td-copy-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("store")).unwrap();
        fs::create_dir_all(base.join("tools")).unwrap();
        // `{out}` exists before a build's first step; the step never creates it.
        let out = base.join("out");
        fs::create_dir(&out).unwrap();
        let payload = base.join("store/abc-claude-code-source");
        fs::write(&payload, b"\x7fELF-bytes").unwrap();
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o444)).unwrap();

        let absent = base.join("absent");
        let unbounded =
            copy_single_file(&absent, &payload, &absent.join("files/x"), true).unwrap_err();
        assert!(unbounded.contains("mkdir"), "{unbounded}");
        assert!(!absent.exists());

        let entry = out.join("files/bin/claude");
        copy_single_file(&out, &payload, &entry, true).unwrap();
        assert_eq!(fs::read(&entry).unwrap(), b"\x7fELF-bytes");
        assert_eq!(fs::metadata(&entry).unwrap().permissions().mode() & 0o7777, 0o755);
        // Every directory the step created is world-traversable whatever the
        // umask, which the application validators require of a package tree.
        for dir in ["out/files", "out/files/bin"] {
            assert_eq!(
                fs::metadata(base.join(dir)).unwrap().permissions().mode() & 0o7777,
                0o755,
                "{dir}"
            );
        }

        let data = out.join("files/share/notes");
        copy_single_file(&out, &payload, &data, false).unwrap();
        assert_eq!(fs::metadata(&data).unwrap().permissions().mode() & 0o7777, 0o644);

        let occupied = copy_single_file(&out, &payload, &entry, true).unwrap_err();
        assert!(occupied.contains("create") && occupied.contains("exists"), "{occupied}");
        // A regular file where a parent should be is refused by the confinement
        // walk's own stat of the path beneath it (ENOTDIR is not NotFound), so
        // nothing is created beside it and no `mkdir` is attempted.
        fs::write(out.join("files/plain"), b"").unwrap();
        let blocked =
            copy_single_file(&out, &payload, &out.join("files/plain/x"), true).unwrap_err();
        assert!(blocked.contains("lstat"), "{blocked}");

        let link = base.join("store/link");
        std::os::unix::fs::symlink(&payload, &link).unwrap();
        let linked =
            copy_single_file(&out, &link, &out.join("files/bin/other"), true).unwrap_err();
        assert!(linked.contains("symlink"), "{linked}");

        // The destination side: the three spellings that would let a data step
        // mint an executable outside the build's own output. A `.` component
        // is not among them: `Path::components` drops it, and it cannot escape.
        for outside in [
            base.join("tools/cc"),
            out.join("../tools/cc"),
            PathBuf::from("files/bin/cc"),
        ] {
            let refused = copy_single_file(&out, &payload, &outside, true).unwrap_err();
            assert!(
                refused.contains("not a plain path inside the build output"),
                "{}: {refused}",
                outside.display()
            );
            assert!(!base.join("tools/cc").exists());
        }
        let itself = copy_single_file(&out, &payload, &out, true).unwrap_err();
        assert!(itself.contains("build output itself"), "{itself}");
        std::os::unix::fs::symlink(base.join("tools"), out.join("escape")).unwrap();
        let through = copy_single_file(&out, &payload, &out.join("escape/cc"), true).unwrap_err();
        assert!(through.contains("traverses symlink"), "{through}");
        assert!(!base.join("tools/cc").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cargo_source_patches_are_scoped_literal_and_count_checked() {
        let declaration = crate::json::parse(
            r#"[{"file":"nested/Cargo.toml","edits":[{"from":"native-tls","to":"rustls","expect":"1"}]}]"#,
        )
        .unwrap();
        let patches = parse_cargo_source_patches(&declaration).unwrap();
        assert_eq!(patches[0].file, "nested/Cargo.toml");
        let build_script = crate::json::parse(
            r#"[{"file":"nested/build.rs","edits":[{"from":"old","to":"new","expect":"1"}]}]"#,
        )
        .unwrap();
        assert_eq!(
            parse_cargo_source_patches(&build_script).unwrap()[0].file,
            "nested/build.rs"
        );

        for invalid in [
            r#"[{"file":"../Cargo.toml","edits":[{"from":"a","to":"b","expect":"1"}]}]"#,
            r#"[{"file":"nested/config.toml","edits":[{"from":"a","to":"b","expect":"1"}]}]"#,
            r#"[{"file":"Cargo.toml","edits":[{"from":"a","to":"a","expect":"1"}]}]"#,
            r#"[{"file":"Cargo.toml","edits":[{"from":"a","to":"b","expect":"0"}]}]"#,
        ] {
            let value = crate::json::parse(invalid).unwrap();
            assert!(parse_cargo_source_patches(&value).is_err(), "{invalid}");
        }

        let base =
            std::env::temp_dir().join(format!("td-cargo-source-patch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("nested")).unwrap();
        fs::write(
            base.join("nested/Cargo.toml"),
            "transport = \"native-tls\"\n",
        )
        .unwrap();
        apply_cargo_source_patches(&base, &patches).unwrap();
        assert_eq!(
            fs::read_to_string(base.join("nested/Cargo.toml")).unwrap(),
            "transport = \"rustls\"\n"
        );
        let error = apply_cargo_source_patches(&base, &patches).unwrap_err();
        assert!(error.contains("occurs 0×"), "{error}");
        assert!(error.starts_with("cargoSourcePatches "), "{error}");

        fs::remove_file(base.join("nested/Cargo.toml")).unwrap();
        std::os::unix::fs::symlink(base.join("outside.toml"), base.join("nested/Cargo.toml"))
            .unwrap();
        let error = apply_cargo_source_patches(&base, &patches).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn native_host_link_wrapper_is_scoped_to_selected_workspaces() {
        assert!(!rust_workspace_needs_native_host_linker(
            "ripgrep", None, None
        ));
        assert!(!rust_workspace_needs_native_host_linker(
            "other-workspace",
            Some("workspace"),
            Some("tool")
        ));
        assert!(rust_workspace_needs_native_host_linker(
            "codex",
            Some("codex-rs"),
            Some("codex-cli")
        ));
    }

    #[test]
    fn line_exception_runtime_is_present_and_not_an_ordinary_alias() {
        let base = std::env::temp_dir().join(format!(
            "td-line-exception-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("bin")).unwrap();
        let codex = base.join("bin/codex");
        fs::write(&codex, b"codex").unwrap();
        let exception = td_engine::target_profile::line_attribution_exception("codex").unwrap();

        validate_line_exception_runtime(&base, std::slice::from_ref(&codex), exception).unwrap();
        let missing = validate_line_exception_runtime(&base, &[], exception).unwrap_err();
        assert!(missing.contains("runtime is absent"), "{missing}");

        let alias = base.join("bin/codex-alias");
        fs::hard_link(&codex, &alias).unwrap();
        let aliased = validate_line_exception_runtime(&base, &[codex, alias], exception)
            .unwrap_err();
        assert!(aliased.contains("aliases ordinary runtime"), "{aliased}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cargo_git_vendor_config_resolves_offline_without_a_git_checkout() {
        let base = std::env::temp_dir().join(format!(
            "td-cargo-git-vendor-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("consumer");
        let cargo_home = base.join("cargo-home");
        let vendor = base.join("vendor");
        let package = vendor.join("gitdep-1.2.3");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(cargo_home.as_path()).unwrap();
        fs::create_dir_all(package.join("src")).unwrap();
        let (source, sources) = cargo_git_fixture();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ngitdep = { git = \"https://example.invalid/example\", rev = \"0123456789abcdef0123456789abcdef01234567\" }\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { gitdep::called(); }\n").unwrap();
        fs::write(
            root.join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"consumer\"\nversion = \"0.1.0\"\ndependencies = [\n \"gitdep\",\n]\n\n[[package]]\nname = \"gitdep\"\nversion = \"1.2.3\"\nsource = \"{source}\"\n"
            ),
        )
        .unwrap();
        fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = \"gitdep\"\nversion = \"1.2.3\"\nedition = \"2021\"\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        fs::write(package.join("src/lib.rs"), "pub fn called() {}\n").unwrap();
        fs::write(
            package.join(".cargo-checksum.json"),
            "{\"files\":{},\"package\":null}",
        )
        .unwrap();
        let vendor_text = vendor.to_str().unwrap();
        fs::write(
            cargo_home.join("config.toml"),
            cargo_vendor_config(false, &sources, vendor_text).unwrap(),
        )
        .unwrap();

        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let output = Command::new(cargo)
            .args(["check", "--offline", "--frozen"])
            .current_dir(&root)
            .env("CARGO_HOME", &cargo_home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo did not accept the exact Git source replacement:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cargo_workspace_selection_stays_below_the_source_root() {
        let base = std::env::temp_dir().join(format!(
            "td-cargo-workspace-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("codex-rs")).unwrap();
        fs::create_dir_all(base.join("nested")).unwrap();
        fs::write(base.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(base.join("codex-rs/Cargo.toml"), "[workspace]\n").unwrap();

        assert_eq!(cargo_workspace_dir(&base, None).unwrap(), base);
        let error = cargo_workspace_dir(&base, Some("codex-rs")).unwrap_err();
        assert!(error.contains("outer Cargo.toml"), "{error}");
        fs::remove_file(base.join("Cargo.toml")).unwrap();
        assert_eq!(
            cargo_workspace_dir(&base, Some("codex-rs")).unwrap(),
            base.join("codex-rs")
        );
        for subdir in ["", ".", "../escape", "/absolute", "nested/../escape"] {
            assert!(cargo_workspace_dir(&base, Some(subdir)).is_err(), "{subdir}");
        }

        let real = base.join("real-workspace");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::os::unix::fs::symlink(&real, base.join("linked-workspace")).unwrap();
        assert!(cargo_workspace_dir(&base, Some("linked-workspace")).is_err());

        let linked_manifest = base.join("linked-manifest");
        fs::create_dir_all(&linked_manifest).unwrap();
        std::os::unix::fs::symlink(base.join("Cargo.toml"), linked_manifest.join("Cargo.toml"))
            .unwrap();
        assert!(cargo_workspace_dir(&base, Some("linked-manifest")).is_err());

        let directory_manifest = base.join("directory-manifest/Cargo.toml");
        fs::create_dir_all(&directory_manifest).unwrap();
        assert!(cargo_workspace_dir(&base, Some("directory-manifest")).is_err());
        fs::create_dir_all(base.join("missing-manifest")).unwrap();
        assert!(cargo_workspace_dir(&base, Some("missing-manifest")).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cargo_workspace_selection_refuses_explicit_workspace_redirection() {
        let base = std::env::temp_dir().join(format!(
            "td-cargo-workspace-redirection-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        let selected = source.join("codex-rs");
        let external = base.join("external-workspace");
        fs::create_dir_all(selected.join("src")).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(selected.join("src/lib.rs"), "").unwrap();
        fs::write(
            selected.join("Cargo.toml"),
            "[package]\nname = \"redirected\"\nversion = \"0.1.0\"\nedition = \"2021\"\nworkspace = \"../../external-workspace\"\n",
        )
        .unwrap();
        fs::write(
            external.join("Cargo.toml"),
            "[workspace]\nmembers = [\"../source/codex-rs\"]\nresolver = \"2\"\n",
        )
        .unwrap();

        let cargo_dir = cargo_workspace_dir(&source, Some("codex-rs")).unwrap();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let envs = vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )];
        let error = require_selected_cargo_workspace(&cargo, &cargo_dir, &envs).unwrap_err();
        assert!(error.contains("Cargo.lock Cargo will not use"), "{error}");

        fs::write(selected.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\n").unwrap();
        require_selected_cargo_workspace(&cargo, &cargo_dir, &envs).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn committed_cargo_lock_is_verified_or_explicitly_replaced() {
        let base = std::env::temp_dir().join(format!(
            "td-cargo-lock-enforcement-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let cargo_dir = base.join("workspace");
        let vendor_dir = base.join("vendor");
        fs::create_dir_all(&cargo_dir).unwrap();
        fs::create_dir_all(&vendor_dir).unwrap();
        let source_lock = cargo_dir.join("Cargo.lock");
        let committed_lock = vendor_dir.join(STAGED_CARGO_LOCK);
        fs::write(&source_lock, b"version = 4\n# source\n").unwrap();
        fs::write(&committed_lock, b"version = 4\n# source\n").unwrap();

        enforce_cargo_lock_policy(&cargo_dir, &vendor_dir, CargoLockPolicy::Verify).unwrap();
        fs::write(&committed_lock, b"version = 4\n# normalized\n").unwrap();
        let error = enforce_cargo_lock_policy(&cargo_dir, &vendor_dir, CargoLockPolicy::Verify)
            .unwrap_err();
        assert!(error.contains("does not byte-match"), "{error}");
        assert_eq!(
            fs::read(&source_lock).unwrap(),
            b"version = 4\n# source\n",
            "verify mode must not modify a mismatched source lock"
        );
        enforce_cargo_lock_policy(&cargo_dir, &vendor_dir, CargoLockPolicy::Replace).unwrap();
        assert_eq!(
            fs::read(&source_lock).unwrap(),
            b"version = 4\n# normalized\n",
            "replace mode writes the exact staged committed bytes"
        );
        assert_eq!(
            parse_cargo_lock_policy("verify").unwrap(),
            CargoLockPolicy::Verify
        );
        assert_eq!(
            parse_cargo_lock_policy("replace").unwrap(),
            CargoLockPolicy::Replace
        );
        assert!(parse_cargo_lock_policy("ignore").is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn committed_cargo_lock_refuses_symlinks_and_non_files() {
        let base = std::env::temp_dir().join(format!(
            "td-cargo-lock-node-types-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let cargo_dir = base.join("workspace");
        let vendor_dir = base.join("vendor");
        fs::create_dir_all(&cargo_dir).unwrap();
        fs::create_dir_all(&vendor_dir).unwrap();
        let real_lock = base.join("real.lock");
        fs::write(&real_lock, b"version = 4\n").unwrap();
        std::os::unix::fs::symlink(&real_lock, vendor_dir.join(STAGED_CARGO_LOCK)).unwrap();
        fs::write(cargo_dir.join("Cargo.lock"), b"version = 4\n").unwrap();
        let error = enforce_cargo_lock_policy(&cargo_dir, &vendor_dir, CargoLockPolicy::Verify)
            .unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");

        fs::remove_file(vendor_dir.join(STAGED_CARGO_LOCK)).unwrap();
        fs::write(vendor_dir.join(STAGED_CARGO_LOCK), b"version = 4\n").unwrap();
        fs::remove_file(cargo_dir.join("Cargo.lock")).unwrap();
        fs::create_dir(cargo_dir.join("Cargo.lock")).unwrap();
        let error = enforce_cargo_lock_policy(&cargo_dir, &vendor_dir, CargoLockPolicy::Replace)
            .unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cargo_selection_pins_the_target_and_selected_bins() {
        let (args, release_dir) = cargo_selection(
            Path::new("/build/codex-rs"),
            &["codex", "apply_patch"],
            true,
            Some("codex-cli"),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "--target-dir",
                "/build/codex-rs/target",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--package",
                "codex-cli",
                "--bin",
                "codex",
                "--bin",
                "apply_patch",
            ]
        );
        assert_eq!(
            release_dir,
            Path::new("/build/codex-rs/target/x86_64-unknown-linux-gnu/release")
        );
        let (plain_args, plain_release_dir) =
            cargo_selection(Path::new("/build"), &["tool"], false, None).unwrap();
        assert!(plain_args.is_empty());
        assert_eq!(plain_release_dir, Path::new("/build/target/release"));
    }

    #[test]
    fn native_rust_host_linker_is_a_scratch_only_declared_toolchain_wrapper() {
        let base =
            std::env::temp_dir().join(format!("td-native-rust-host-linker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let path = install_native_rust_host_linker(
            &base,
            "/td/store/busybox/bin/sh",
            "/td/store/gcc/bin/gcc",
            "/td/store/glibc/lib/ld-linux-x86-64.so.2",
            "/td/store/glibc/lib:/td/store/zlib/lib",
            "/td/store/binutils/bin:/td/store/glibc/lib",
            "/td/store/rust/bin:/td/store/busybox/bin",
        )
        .unwrap();
        assert!(path.starts_with(base.join("td-native-bin").to_str().unwrap()));
        let wrapper = fs::read_to_string(base.join("td-native-bin/cc")).unwrap();
        for required in [
            "#!/td/store/busybox/bin/sh",
            "exec \"/td/store/gcc/bin/gcc\" \"$@\"",
            "-static-libgcc",
            "-Wl,--dynamic-linker,\"/td/store/glibc/lib/ld-linux-x86-64.so.2\"",
            "-Wl,-rpath,\"/td/store/glibc/lib\"",
            "-B\"/td/store/binutils/bin\"",
        ] {
            assert!(wrapper.contains(required), "wrapper omits {required}");
        }
        assert!(!safe_wrapper_word("/td/store/path\nexec bad"));
        assert!(!safe_wrapper_word("relative/path"));
        assert_eq!(
            fs::metadata(base.join("td-native-bin/cc"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_closure_walk_skips_only_debug_companion_directories() {
        assert!(is_debug_companion_dir(Path::new("/out/lib/debug")));
        assert!(is_debug_companion_dir(Path::new(
            "/out/stage/td/store/pkg/lib/debug"
        )));
        assert!(!is_debug_companion_dir(Path::new("/out/debug")));
        assert!(!is_debug_companion_dir(Path::new("/out/lib/debugger")));
    }

    #[test]
    fn debug_size_deduplicates_hard_links_and_enforces_the_external_ceiling() {
        let base = std::env::temp_dir().join(format!("td-debug-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let debug = base.join("root/pkg/lib/debug/bin");
        fs::create_dir_all(&debug).unwrap();
        fs::write(debug.join("one.debug"), b"1234").unwrap();
        fs::hard_link(debug.join("one.debug"), debug.join("two.debug")).unwrap();
        fs::write(base.join("root/not-debug"), b"not counted").unwrap();
        let report = base.join("report");

        assert_debug_size(&base.join("root"), &report, "fixture", 4).unwrap();
        assert_eq!(
            fs::read_to_string(&report).unwrap(),
            "format=1\nscope=fixture\ndebug_bytes=4\nceiling_bytes=4\n"
        );
        let error = assert_debug_size(&base.join("root"), &report, "fixture", 3).unwrap_err();
        assert!(error.contains("exceeding compiled ceiling 3"), "{error}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn compare_files_streams_equal_bytes_and_names_the_first_difference() {
        let base = std::env::temp_dir().join(format!("td-compare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let left = base.join("left");
        let right = base.join("right");
        fs::write(&left, b"same").unwrap();
        fs::write(&right, b"same").unwrap();
        compare_files(&left, &right).unwrap();
        fs::write(&right, b"samp").unwrap();
        let error = compare_files(&left, &right).unwrap_err();
        assert!(error.contains("differ at byte 3"), "{error}");
        let _ = fs::remove_dir_all(&base);
    }

    fn compact_mesboot_dispatch() -> String {
        let source = include_str!("build.rs");
        let (_, dispatch) = source
            .split_once("    for (i, step) in steps.iter().enumerate() {")
            .expect("mesboot dispatch must remain identifiable");
        let (dispatch, _) = dispatch
            .split_once("\n    Ok(())\n}")
            .expect("mesboot dispatch must remain bounded");
        crate::affected::strip_line_comments(dispatch)
            .split_whitespace()
            .collect()
    }
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn test_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("td-build-{tag}-{}", std::process::id()))
    }

    fn minimal_static_elf() -> Vec<u8> {
        let total = 64 + 56;
        let mut bytes = vec![0u8; total];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[0x10..0x12].copy_from_slice(&2u16.to_le_bytes());
        bytes[0x12..0x14].copy_from_slice(&62u16.to_le_bytes());
        bytes[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
        bytes[0x18..0x20].copy_from_slice(&64u64.to_le_bytes());
        bytes[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        bytes[0x34..0x36].copy_from_slice(&64u16.to_le_bytes());
        bytes[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        bytes[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
        bytes[68..72].copy_from_slice(&1u32.to_le_bytes());
        bytes[80..88].copy_from_slice(&0u64.to_le_bytes());
        bytes[96..104].copy_from_slice(&(total as u64).to_le_bytes());
        bytes[104..112].copy_from_slice(&(total as u64).to_le_bytes());
        bytes
    }

    fn static_application_fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let directory = test_dir(tag);
        let _ = fs::remove_dir_all(&directory);
        let out = directory.join("out");
        let entry = out.join("files/bin/app");
        let runtime = directory.join("runtime");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::create_dir_all(runtime.join("files")).unwrap();
        fs::write(&entry, minimal_static_elf()).unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        (directory, out, runtime)
    }

    #[test]
    fn copy_files_refuses_an_archive_symlink_to_an_outside_executable() {
        let directory = test_dir("copy-files-symlink");
        let _ = fs::remove_dir_all(&directory);
        let unpacked = directory.join("unpacked");
        let outside = directory.join("outside");
        let dest = directory.join("out/files/bin");
        fs::create_dir_all(&unpacked).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let executable = outside.join("outside-app");
        fs::write(&executable, minimal_static_elf()).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let archive_entry = unpacked.join("rg");
        std::os::unix::fs::symlink(&executable, &archive_entry).unwrap();

        let error = copy_file_writable(&archive_entry, &dest)
            .expect_err("copyFiles must not dereference a foreign archive symlink");
        assert!(error.contains("symlinks") && error.contains("refused"), "{error}");
        assert!(!dest.join("rg").exists());

        let nested = unpacked.join("nested");
        std::os::unix::fs::symlink(&outside, &nested).unwrap();
        let nested_entry = nested.join("outside-app");
        let error = copy_file_writable(&nested_entry, &dest)
            .expect_err("copyFiles must not traverse an intermediate archive symlink");
        assert!(
            error.contains("traverses symlink") && error.contains("refused"),
            "{error}"
        );
        assert!(!dest.join("outside-app").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn static_application_validation_binds_entry_runtime_and_tree() {
        let (directory, out, runtime) = static_application_fixture("static-app-good");
        assert!(validate_static_application(&out, "/app/bin/app", &runtime).is_ok());
        for entry in ["/app", "/app/../../bin/app", "/usr/bin/app"] {
            let error = validate_static_application(&out, entry, &runtime).unwrap_err();
            assert!(error.contains("application entry"), "{entry}: {error}");
        }

        let entry = out.join("files/bin/app");
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o644)).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("not a world-executable regular file"), "{error}");
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o100)).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("not a world-executable regular file"), "{error}");
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();

        let missing_runtime = directory.join("missing-runtime");
        let error = validate_static_application(&out, "/app/bin/app", &missing_runtime).unwrap_err();
        assert!(error.contains("has no files directory"), "{error}");

        fs::write(&entry, b"not an ELF").unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("not an ELF file"), "{error}");

        let mut wrong_machine = minimal_static_elf();
        wrong_machine[0x12..0x14].copy_from_slice(&3u16.to_le_bytes());
        fs::write(&entry, wrong_machine).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("expected EM_X86_64"), "{error}");

        let mut relocatable = minimal_static_elf();
        relocatable[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        fs::write(&entry, relocatable).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("ET_EXEC or ET_DYN"), "{error}");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn application_tree_refuses_special_bits_nodes_and_all_symlinks() {
        let (directory, out, runtime) = static_application_fixture("static-app-metadata");
        let files = out.join("files");
        fs::set_permissions(&files, fs::Permissions::from_mode(0o2755)).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("files root") && error.contains("mode bits"), "{error}");
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();

        fs::set_permissions(&files, fs::Permissions::from_mode(0o700)).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("files root") && error.contains("not traversable"), "{error}");
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();

        let bin = files.join("bin");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("application directory") && error.contains("not traversable"), "{error}");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let data = files.join("data");
        fs::write(&data, b"data").unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o4644)).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("mode bits"), "{error}");

        fs::set_permissions(&data, fs::Permissions::from_mode(0o644)).unwrap();
        let socket = files.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(
            error.contains("is not a directory, regular file, or symlink"),
            "{error}"
        );
        drop(listener);
        fs::remove_file(&socket).unwrap();

        let escaping = files.join("bin/link");
        std::os::unix::fs::symlink("../data", &escaping).unwrap();
        let error = validate_static_application(&out, "/app/bin/app", &runtime).unwrap_err();
        assert!(error.contains("is a symlink") && error.contains("regular files"), "{error}");
        let _ = fs::remove_dir_all(directory);
    }

    fn edit(from: &str, to: &str, expect: usize) -> (String, String, usize) {
        (from.to_string(), to.to_string(), expect)
    }

    #[test]
    fn mesboot_steps_file_is_consumed_after_read() {
        let dir = test_dir("steps-file");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MESBOOT_STEPS_FILE);
        let steps = format!("[{{\"payload\":\"{}\"}}]", "x".repeat(300 * 1024));
        fs::write(&path, &steps).unwrap();
        assert_eq!(consume_mesboot_steps_file(&path).unwrap(), steps);
        assert!(
            !path.exists(),
            "the transport file must not be observable to recipe steps"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn text_edits_apply_in_order_and_are_literal() {
        // A patch-shaped edit set: single-line swap, a multi-line hunk, and a
        // replacement whose `from` contains C braces / `{…}` that must NOT be
        // treated as templates.
        let src = "extern char *__progname;\nif (x) { g(); }\n#include <paths.h>\nvolatile sig_atomic_t s;\n";
        let out = apply_text_edits(
            "defs.h",
            src.to_string(),
            &[
                edit("extern char *__progname;", "char *__progname;", 1),
                edit("if (x) { g(); }", "if (x) { h(); }", 1),
                edit("#include <paths.h>", "#include <getopt.h>", 1),
                edit("volatile sig_atomic_t s;", "volatile int s;", 1),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "char *__progname;\nif (x) { h(); }\n#include <getopt.h>\nvolatile int s;\n"
        );
    }

    #[test]
    fn text_edits_replace_every_occurrence_when_count_matches() {
        let out = apply_text_edits("f", "a a a".to_string(), &[edit("a", "b", 3)]).unwrap();
        assert_eq!(out, "b b b");
    }

    #[test]
    fn text_edits_sequential_edits_see_prior_result() {
        // Edit 2's `from` only exists after edit 1 has run.
        let out = apply_text_edits(
            "f",
            "one".to_string(),
            &[edit("one", "two", 1), edit("two", "three", 1)],
        )
        .unwrap();
        assert_eq!(out, "three");
    }

    #[test]
    fn text_edits_fail_closed_on_count_mismatch() {
        // Pinned source drifted (0 matches) — must red, not silently no-op.
        let e = apply_text_edits("main.c", "nothing here".to_string(), &[edit("mkstemp", "mktemp", 1)])
            .unwrap_err();
        assert!(e.contains("occurs 0×") && e.contains("expected 1"), "{e}");
        // Under-counted expectation (2 present, said 1) also reds.
        let e2 = apply_text_edits("main.c", "fd fd".to_string(), &[edit("fd", "fname", 1)]).unwrap_err();
        assert!(e2.contains("occurs 2×"), "{e2}");
    }

    #[test]
    fn text_edits_reject_empty_from() {
        let e = apply_text_edits("f", "x".to_string(), &[edit("", "y", 1)]).unwrap_err();
        assert!(e.contains("empty `from'"), "{e}");
    }

    #[test]
    fn text_edits_reject_expect_zero() {
        // `expect: 0` is a `1`→`0` typo trap (silently no-ops when absent), not a
        // supported assert-absent — every declared edit must change something.
        let e = apply_text_edits("f", "no match here".to_string(), &[edit("gone", "x", 0)])
            .unwrap_err();
        assert!(e.contains("`expect' is 0"), "{e}");
    }

    #[test]
    fn text_edits_reject_non_ascii() {
        // The build-JSON reader is Latin-1, so a non-ASCII edit can't round-trip:
        // a non-ASCII `to` would write mangled bytes. Fail closed, don't corrupt.
        let e_to = apply_text_edits("f", "cafe".to_string(), &[edit("cafe", "café", 1)])
            .unwrap_err();
        assert!(e_to.contains("must be ASCII"), "{e_to}");
        let e_from = apply_text_edits("f", "x".to_string(), &[edit("café", "cafe", 1)])
            .unwrap_err();
        assert!(e_from.contains("must be ASCII"), "{e_from}");
    }

    #[test]
    fn deployment_image_and_manifest_are_engine_native_and_deterministic() {
        let d = test_dir("deployment");
        let _ = fs::remove_dir_all(&d);
        let root = d.join("root");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/issue"), b"td\n").unwrap();
        let deployment = d.join("deployment");
        let image = deployment.join("root.erofs");
        pack_erofs(&root, &image).unwrap();
        let bytes = fs::read(&image).unwrap();
        assert_eq!(
            bytes.get(1024..1028),
            Some([0xe2, 0xe1, 0xf5, 0xe0].as_slice()),
            "EROFS superblock magic"
        );

        let kernel = deployment.join("bzImage");
        let initramfs = deployment.join("initramfs.cpio");
        fs::write(&kernel, b"kernel").unwrap();
        fs::write(&initramfs, b"initramfs").unwrap();
        let manifest = deployment.join("manifest");
        write_sha256_manifest(
            &manifest,
            vec![
                ("root.erofs".into(), image.to_string_lossy().into_owned()),
                (
                    "initramfs.cpio".into(),
                    initramfs.to_string_lossy().into_owned(),
                ),
                ("bzImage".into(), kernel.to_string_lossy().into_owned()),
            ],
        )
        .unwrap();
        let first = fs::read_to_string(&manifest).unwrap();
        write_sha256_manifest(
            &manifest,
            vec![
                ("bzImage".into(), kernel.to_string_lossy().into_owned()),
                ("root.erofs".into(), image.to_string_lossy().into_owned()),
                (
                    "initramfs.cpio".into(),
                    initramfs.to_string_lossy().into_owned(),
                ),
            ],
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&manifest).unwrap(), first);
        assert_eq!(
            first,
            format!(
                "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
                crate::sha256::hex_digest(b"kernel"),
                crate::sha256::hex_digest(b"initramfs"),
                crate::sha256::hex_digest(&bytes),
            ),
            "the engine producer matches the target boot contract"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deployment_manifest_rejects_ambiguous_labels() {
        let d = test_dir("manifest-labels");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let artifact = d.join("artifact");
        fs::write(&artifact, b"x").unwrap();
        let path = artifact.to_string_lossy().into_owned();

        for label in ["../escape", ".", ".."] {
            let bad = write_sha256_manifest(
                &d.join("bad"),
                vec![(label.into(), path.clone())],
            )
            .unwrap_err();
            assert!(bad.contains("invalid artifact label"), "{bad}");
        }
        let duplicate = write_sha256_manifest(
            &d.join("duplicate"),
            vec![("same".into(), path.clone()), ("same".into(), path)],
        )
        .unwrap_err();
        assert!(duplicate.contains("duplicate artifact label"), "{duplicate}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn substitute_edits_expand_only_the_file_path_not_from_to() {
        // The executor-level guarantee: `parse_substitute_edits` expands `{src}`
        // in the FILE path, but `from`/`to` are literal source text — a `{in:…}`
        // / `{src}` inside a hunk must NOT be template-expanded (it is real C
        // that happens to contain braces). Pins the arm the 5 helper tests can't.
        let ctx = StepCtx {
            root: "/r".into(),
            src: "/r/src".into(),
            out: "/o".into(),
            tools: "/r/tools".into(),
            jobs: "4".into(),
            inputs: vec![("mes".into(), "/td/store/abc-mes".into())],
            payloads: Vec::new(),
        };
        let o = crate::json::parse(
            r#"{"file":"{src}/x.c","edits":[{"from":"a{in:mes}b","to":"c{src}d","expect":"1"}]}"#,
        )
        .unwrap();
        let (file, edits) = parse_substitute_edits(&ctx, &o).unwrap();
        assert_eq!(file, "/r/src/x.c", "the file path IS expanded");
        assert_eq!(
            edits,
            vec![("a{in:mes}b".to_string(), "c{src}d".to_string(), 1)],
            "from/to stay literal — no template expansion"
        );
    }

    #[test]
    fn write_preserving_mode_rewrites_readonly_and_restores_mode() {
        // GNU patch ships pch.c 0444; substituteText must still rewrite it and
        // leave the mode untouched — a plain fs::write would EACCES (the bug this
        // guards). PR-tier coverage: patch-mesboot's full build is UNPROVISIONED,
        // so this is the only per-PR gate on the read-only rewrite path.
        let d = std::env::temp_dir().join(format!("td-writero-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d); // clear any 0444 leftover from a crashed prior run
        fs::create_dir_all(&d).unwrap();
        let f = d.join("ro.c");
        fs::write(&f, b"before").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();

        write_preserving_mode(&f, b"after").unwrap();

        assert_eq!(fs::read_to_string(&f).unwrap(), "after", "read-only file rewritten");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o444,
            "original 0444 mode restored"
        );

        // A writable target is rewritten too, mode left 0644 (grant skipped).
        let w = d.join("rw.c");
        fs::write(&w, b"x").unwrap();
        fs::set_permissions(&w, fs::Permissions::from_mode(0o644)).unwrap();
        write_preserving_mode(&w, b"y").unwrap();
        assert_eq!(fs::read_to_string(&w).unwrap(), "y");
        assert_eq!(fs::metadata(&w).unwrap().permissions().mode() & 0o777, 0o644);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn write_preserving_mode_rejects_symlink_target() {
        // A symlinked target must be rejected (not followed) so the grant/write
        // cannot land on a different file than substituteText validated.
        let d = std::env::temp_dir().join(format!("td-writesym-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let real = d.join("real.c");
        fs::write(&real, b"keep").unwrap();
        let link = d.join("link.c");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let e = write_preserving_mode(&link, b"clobber").unwrap_err();

        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
        assert_eq!(fs::read_to_string(&real).unwrap(), "keep", "real file untouched");
        fs::remove_dir_all(&d).unwrap();
    }

    /// A POSIX `sh` + a PATH env for the child. The watchdog scripts use only
    /// shell builtins plus `kill` and INTEGER `sleep` (busybox `sleep` has no
    /// float applet in defconfig), so they run under any POSIX shell. On a dev
    /// host that is the system `/bin/sh`; in the loop host-sandbox it is busybox
    /// `sh` (ash), already on PATH — no seed bash, so these tests RUN there
    /// without a guix lock.
    fn sh_and_env() -> (String, Vec<(String, String)>) {
        let path = env::var("PATH").unwrap();
        let sh = find_in_path(&path, "sh").expect("sh on PATH");
        (sh, vec![("PATH".to_string(), path)])
    }

    /// A test-sized Watch. `silence`/`limit`/`repeat_ms` 0 = off; drain grace 1s.
    /// `repeat_ms` is the sustained-duration bound in ms (a `Duration` field, so
    /// tests can use a sub-second window and stay fast).
    fn w(silence_secs: u64, limit: u32, repeat_ms: u64) -> Watch {
        Watch {
            silence: Duration::from_secs(silence_secs),
            repeat_limit: limit,
            repeat_secs: Duration::from_millis(repeat_ms),
            drain_grace: Duration::from_secs(1),
        }
    }

    #[test]
    fn application_metadata_finalizer_is_literal_fixed_and_non_overwriting() {
        let directory =
            std::env::temp_dir().join(format!("td-application-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let text = "name=firefox\nversion=1\nruntime=empty-runtime\nentry=/app/bin/firefox\nprovenance=foreign\n\n[Environment]\nTOKEN={root}\n";
        let encoded = Json::Str(text.into()).to_canonical();
        let manifest = td_engine::application::ApplicationManifest::parse(text).unwrap();
        let spec = td_engine::application_spec::ApplicationSpec::compile(
            &manifest,
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
            td_engine::permissions::PermissionPolicy::new(),
        )
        .unwrap()
        .to_keyfile();
        let spec_encoded = Json::Str(spec.clone()).to_canonical();
        let launcher = "firefox\tFirefox\tbrowser web\n";
        let launcher_encoded = Json::Str(launcher.into()).to_canonical();
        materialize_application_metadata_at(
            &directory,
            Some(&encoded),
            Some(&spec_encoded),
            Some(&launcher_encoded),
        )
        .unwrap();
        let path = directory.join("manifest");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let spec_path = directory.join("spec");
        assert_eq!(fs::read_to_string(&spec_path).unwrap(), spec);
        assert_eq!(
            fs::metadata(&spec_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let launcher_path = directory.join("exports/launcher.tsv");
        assert_eq!(fs::read_to_string(&launcher_path).unwrap(), launcher);
        assert_eq!(
            fs::metadata(&launcher_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let error = write_application_metadata_file(&directory, "manifest", text).unwrap_err();
        assert!(
            error.contains("reserved for builder-authenticated metadata"),
            "an existing manifest must not be replaced: {error}"
        );

        let plain = directory.join("plain");
        fs::create_dir_all(&plain).unwrap();
        materialize_application_metadata_at(&plain, None, None, None).unwrap();
        fs::write(plain.join("manifest"), "provenance=source\n").unwrap();
        let error = materialize_application_metadata_at(&plain, None, None, None).unwrap_err();
        assert!(error.contains("undeclared application metadata"), "{error}");
        let plain_spec = directory.join("plain-spec");
        fs::create_dir_all(&plain_spec).unwrap();
        fs::write(plain_spec.join("spec"), "format=1\n").unwrap();
        let error = materialize_application_metadata_at(&plain_spec, None, None, None).unwrap_err();
        assert!(error.contains("undeclared application metadata"), "{error}");
        let plain_launcher = directory.join("plain-launcher");
        fs::create_dir_all(plain_launcher.join("exports")).unwrap();
        fs::write(plain_launcher.join("exports/launcher.tsv"), launcher).unwrap();
        let error =
            materialize_application_metadata_at(&plain_launcher, None, None, None).unwrap_err();
        assert!(error.contains("undeclared application metadata"), "{error}");
        let linked_exports = directory.join("linked-exports");
        let linked_exports_target = directory.join("linked-exports-target");
        fs::create_dir_all(&linked_exports).unwrap();
        fs::create_dir_all(&linked_exports_target).unwrap();
        std::os::unix::fs::symlink(&linked_exports_target, linked_exports.join("exports")).unwrap();
        let error =
            materialize_application_metadata_at(&linked_exports, None, None, None).unwrap_err();
        assert!(error.contains("exports is a symlink"), "{error}");

        let missing = directory.join("missing");
        let error = materialize_application_metadata_at(
            &missing,
            Some(&encoded),
            Some(&spec_encoded),
            Some(&launcher_encoded),
        )
        .unwrap_err();
        assert!(error.contains("stat application output"), "{error}");
        let file = directory.join("file-output");
        fs::write(&file, "not a directory").unwrap();
        materialize_application_metadata_at(&file, None, None, None).unwrap();
        let error = materialize_application_metadata_at(
            &file,
            Some(&encoded),
            Some(&spec_encoded),
            Some(&launcher_encoded),
        )
        .unwrap_err();
        assert!(error.contains("is not a directory"), "{error}");

        let linked_target = directory.join("linked-target");
        fs::create_dir_all(&linked_target).unwrap();
        let linked_output = directory.join("linked-output");
        std::os::unix::fs::symlink(&linked_target, &linked_output).unwrap();
        let error = materialize_application_metadata_at(
            &linked_output,
            Some(&encoded),
            Some(&spec_encoded),
            Some(&launcher_encoded),
        )
        .unwrap_err();
        assert!(error.contains("is not a directory"), "{error}");
        assert!(!linked_target.join("manifest").exists());
        assert!(!linked_target.join("spec").exists());

        let noncanonical = Json::Str(format!("# comment\n{text}")).to_canonical();
        let other = directory.join("other");
        let error = decode_application_manifest(&noncanonical).unwrap_err();
        assert!(error.contains("not canonical"), "{error}");
        let error =
            decode_application_spec(&Json::Str(format!("#\n{spec}")).to_canonical()).unwrap_err();
        assert!(error.contains("TD_APPLICATION_SPEC"), "{error}");
        let error =
            materialize_application_metadata_at(&other, Some(&encoded), None, None).unwrap_err();
        assert!(error.contains("requires the manifest"), "{error}");
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn application_tables_bind_exports_to_exact_package_paths() {
        let directory =
            std::env::temp_dir().join(format!("td-application-tables-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let store = directory.join("store");
        let package = store.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ripgrep-seed-15.2.0");
        fs::create_dir_all(package.join("exports")).unwrap();
        let manifest =
            td_engine::application::ApplicationDeclaration::new("empty-runtime", "/app/bin/rg")
                .unwrap()
                .manifest(
                    "ripgrep-seed",
                    "15.2.0",
                    td_engine::application::ApplicationProvenance::Foreign,
                )
                .unwrap();
        let spec = td_engine::application_spec::ApplicationSpec::compile(
            &manifest,
            "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1",
            td_engine::permissions::PermissionPolicy::new(),
        )
        .unwrap();
        let launcher =
            td_engine::launcher::LauncherDeclaration::new("Ripgrep", &["ripgrep", "rg", "search"])
                .unwrap()
                .bind("ripgrep-seed")
                .unwrap();
        fs::write(package.join("manifest"), manifest.to_keyfile()).unwrap();
        fs::write(package.join("spec"), spec.to_keyfile()).unwrap();
        fs::write(package.join("exports/launcher.tsv"), launcher.to_tsv()).unwrap();

        let registry = directory.join("image/etc/td-applications.tsv");
        let table = directory.join("image/etc/td-launcher.tsv");
        let payloads = vec![
            (
                "ripgrep-seed".into(),
                package.to_string_lossy().into_owned(),
            ),
            (
                "empty-runtime".into(),
                "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into(),
            ),
        ];
        compile_application_tables_in(
            &["ripgrep-seed".into()],
            &[package.to_string_lossy().into_owned()],
            &["/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into()],
            &payloads,
            &registry,
            &table,
            &store.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&registry).unwrap(),
            format!("ripgrep-seed\t{}\n", package.display())
        );
        assert_eq!(
            fs::read_to_string(&table).unwrap(),
            "ripgrep-seed\tRipgrep\tripgrep rg search\n"
        );
        assert_eq!(
            fs::metadata(&registry).unwrap().permissions().mode() & 0o777,
            0o644
        );

        let missing_runtime_registry = directory.join("missing-runtime/registry.tsv");
        let missing_runtime_table = directory.join("missing-runtime/launcher.tsv");
        let error = compile_application_tables_in(
            &["ripgrep-seed".into()],
            &[package.to_string_lossy().into_owned()],
            &["/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into()],
            std::slice::from_ref(payloads.first().unwrap()),
            &missing_runtime_registry,
            &missing_runtime_table,
            &store.to_string_lossy(),
        )
        .unwrap_err();
        assert!(error.contains("is not a declared payload"), "{error}");

        let mismatched_runtime =
            "/td/store/cccccccccccccccccccccccccccccccc-other-runtime-1".to_string();
        let mut mismatched_payloads = payloads.clone();
        mismatched_payloads.push(("other-runtime".into(), mismatched_runtime.clone()));
        let error = compile_application_tables_in(
            &["ripgrep-seed".into()],
            &[package.to_string_lossy().into_owned()],
            &[mismatched_runtime],
            &mismatched_payloads,
            &directory.join("mismatched-runtime/registry.tsv"),
            &directory.join("mismatched-runtime/launcher.tsv"),
            &store.to_string_lossy(),
        )
        .unwrap_err();
        assert!(error.contains("not selected runtime"), "{error}");

        let error = compile_application_tables_in(
            &["catalog-stem".into()],
            &[package.to_string_lossy().into_owned()],
            &["/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into()],
            &payloads,
            &directory.join("mismatched-name/registry.tsv"),
            &directory.join("mismatched-name/launcher.tsv"),
            &store.to_string_lossy(),
        )
        .unwrap_err();
        assert!(error.contains("selected=\"catalog-stem\""), "{error}");

        let error = compile_application_tables_in(
            &["td-jail".into()],
            &[package.to_string_lossy().into_owned()],
            &["/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into()],
            &payloads,
            &directory.join("reserved-name/registry.tsv"),
            &directory.join("reserved-name/launcher.tsv"),
            &store.to_string_lossy(),
        )
        .unwrap_err();
        assert!(error.contains("reserved by td-jail"), "{error}");

        let noncanonical_registry = directory.join("noncanonical/registry.tsv");
        let noncanonical_table = directory.join("noncanonical/launcher.tsv");
        fs::write(
            package.join("manifest"),
            format!("# comment\n{}", manifest.to_keyfile()),
        )
        .unwrap();
        let error = compile_application_tables_in(
            &["ripgrep-seed".into()],
            &[package.to_string_lossy().into_owned()],
            &["/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into()],
            &payloads,
            &noncanonical_registry,
            &noncanonical_table,
            &store.to_string_lossy(),
        )
        .unwrap_err();
        assert!(error.contains("metadata is not canonical"), "{error}");
        fs::write(package.join("manifest"), manifest.to_keyfile()).unwrap();

        let second_registry = directory.join("second/registry.tsv");
        let second_table = directory.join("second/launcher.tsv");
        fs::remove_file(package.join("exports/launcher.tsv")).unwrap();
        std::os::unix::fs::symlink(
            directory.join("image/etc/td-launcher.tsv"),
            package.join("exports/launcher.tsv"),
        )
        .unwrap();
        let error = compile_application_tables_in(
            &["ripgrep-seed".into()],
            &[package.to_string_lossy().into_owned()],
            &["/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-empty-runtime-1".into()],
            &payloads,
            &second_registry,
            &second_table,
            &store.to_string_lossy(),
        )
        .unwrap_err();
        assert!(error.contains("traverses a symlink"), "{error}");
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn mesboot_string_arrays_refuse_non_strings() {
        let object = Json::Obj(vec![(
            "packages".into(),
            Json::Arr(vec![Json::Str("package".into()), Json::Bool(false)]),
        )]);
        let error = string_array(&object, "packages").unwrap_err();
        assert_eq!(error, "`packages' contains a non-string");
    }

    #[test]
    fn every_mesboot_string_array_error_gets_its_step_number() {
        let compact = compact_mesboot_dispatch();
        assert_eq!(
            compact.match_indices("string_array(o,").count(),
            10,
            "every production array call must remain pinned"
        );
        for (field, count) in [
            ("argv", 1),
            ("files", 1),
            ("roots", 1),
            ("names", 1),
            ("packages", 1),
            ("runtimes", 1),
            ("libraryPaths", 1),
            ("optionalTargets", 1),
            ("paths", 2),
        ] {
            let site = format!("string_array(o,\"{field}\").map_err(err)?");
            assert_eq!(
                compact.match_indices(&site).count(),
                count,
                "array field {field:?} bypasses or duplicates the numbered wrapper"
            );
        }
    }

    #[test]
    fn source_site_audits_ignore_line_comments_and_layout() {
        let source = "// ctx.expand_data string_array(o,\"fake\")\n\
                      string_array( o,\n \"roots\" )\n .map_err(err)?;";
        let compact: String = crate::affected::strip_line_comments(source)
            .split_whitespace()
            .collect();
        assert!(!compact.contains("ctx.expand_data"), "{compact}");
        assert_eq!(
            compact,
            "string_array(o,\"roots\").map_err(err)?;"
        );
    }

    #[test]
    fn mesboot_template_expands_tokens_and_passes_foreign_braces_verbatim() {
        // The rung seds carry brace text (`${vdso_symver//./_}`) that must NOT
        // expand; recognised tokens must; an unknown {in:X} is a hard error.
        let ctx = StepCtx {
            root: "/r".into(),
            src: "/r/src".into(),
            out: "/o".into(),
            tools: "/r/tools".into(),
            jobs: "4".into(),
            inputs: vec![("mes".into(), "/td/store/abc-mes".into())],
            payloads: Vec::new(),
        };
        assert_eq!(
            ctx.expand("{in:mes}/bin -j{jobs} {src}").unwrap(),
            "/td/store/abc-mes/bin -j4 /r/src"
        );
        assert_eq!(
            ctx.expand("s,${vdso_symver//./_},x,").unwrap(),
            "s,${vdso_symver//./_},x,"
        );
        let err = ctx.expand("{in:nope}/bin").expect_err("unknown input reds");
        assert!(err.contains("nope"), "{err}");
    }

    /// The two readers of `TD_PAYLOAD_MAP` must agree about what it may contain.
    /// `sandbox::payload_paths` refuses a non-string entry before the build runs;
    /// this one is what the STEPS read, and filtering here would drop a payload's
    /// name silently instead.
    #[test]
    fn the_payload_map_is_read_strictly() {
        assert_eq!(
            parse_payload_map(r#"{"firefox":"/td/store/def-firefox"}"#).unwrap(),
            vec![("firefox".to_string(), "/td/store/def-firefox".to_string())]
        );
        assert!(parse_payload_map("{}").unwrap().is_empty());
        for bad in ["not json", "[\"a\"]", r#"{"firefox":7}"#, r#"{"a":null}"#] {
            let e = parse_payload_map(bad).expect_err("a malformed map must refuse");
            assert!(e.contains("TD_PAYLOAD_MAP"), "must name the variable: {e}");
        }
    }

    /// The escape a review found in the RESOLUTION half: `glob:` splices
    /// directory entries straight into argv, and `{in:<any input>}/..` is the
    /// store directory — so a step could name a payload with no `{payload:}`
    /// (an error there) and no `{in:PAYLOAD}` (withheld from the map), which
    /// `noexec` does not cover for `-I`/`-L`.
    #[test]
    fn a_glob_may_not_read_its_way_out_of_the_build_tree() {
        let d = std::env::temp_dir().join(format!("td-globdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let root = d.join("build");
        let out = d.join("out");
        let store = d.join("store");
        fs::create_dir_all(root.join("tg")).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::create_dir_all(store.join("aaa-mes")).unwrap();
        fs::create_dir_all(store.join("bbb-firefox-140")).unwrap();
        fs::write(root.join("tg/a.o"), b"o").unwrap();
        let ctx = StepCtx {
            root: root.to_string_lossy().into_owned(),
            src: root.join("src").to_string_lossy().into_owned(),
            out: out.to_string_lossy().into_owned(),
            tools: root.join("tools").to_string_lossy().into_owned(),
            jobs: "1".into(),
            inputs: vec![(
                "mes".to_string(),
                store.join("aaa-mes").to_string_lossy().into_owned(),
            )],
            payloads: vec![(
                "firefox".to_string(),
                store.join("bbb-firefox-140").to_string_lossy().into_owned(),
            )],
        };
        // The shape every rung in the tree uses.
        assert!(ctx.check_glob_dir(&format!("{}/tg/*.o", ctx.root)).is_ok());
        assert!(ctx.check_glob_dir(&format!("{}/*.a", ctx.out)).is_ok());
        // The escape: `{in:mes}/..` is the store dir, reached without naming a
        // payload at all. `..` is resolved, so the check cannot be walked past.
        let escape = format!("{}/aaa-mes/../*-firefox-140", store.to_string_lossy());
        let e = ctx
            .check_glob_dir(&escape)
            .expect_err("a glob out of the store must refuse");
        assert!(e.contains("build's own tree"), "{e}");
        // ...and it really would have matched, or this proves nothing.
        assert_eq!(glob_one_star(&escape).unwrap().len(), 1);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_tool_farm_refuses_a_missing_target_before_linking_it() {
        let d = std::env::temp_dir().join(format!("td-tool-farm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let tools = d.join("tools");
        let target = d.join("input/bin/as");
        fs::create_dir_all(&tools).unwrap();
        let error = create_tool_farm_link(&tools, "as", &target)
            .expect_err("a missing tool target must refuse");
        assert!(error.contains("toolFarm target"), "{error}");
        assert!(!tools.join("as").exists());

        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"tool").unwrap();
        create_tool_farm_link(&tools, "as", &target).unwrap();
        assert_eq!(fs::read_link(tools.join("as")).unwrap(), target);
        fs::remove_dir_all(&d).unwrap();
    }

    /// Assembly partitions the two channels; this is the same partition checked
    /// where the maps are READ, since a derivation that reached the builder with
    /// a payload aliased under an input name would resolve it for a step that
    /// runs a command. The runtime resolver compares names, so it would not
    /// notice, and the alias is the whole exploit.
    #[test]
    fn a_payload_aliased_as_an_input_is_refused_at_the_map() {
        let payloads = vec![("firefox".to_string(), "/td/store/def-firefox".to_string())];
        let ok = vec![("bash".to_string(), "/td/store/abc-bash".to_string())];
        assert!(refuse_aliased_payloads(&ok, &payloads).is_ok());
        assert!(refuse_aliased_payloads(&ok, &[]).is_ok());
        let aliased = vec![
            ("bash".to_string(), "/td/store/abc-bash".to_string()),
            ("alias".to_string(), "/td/store/def-firefox".to_string()),
        ];
        let e = refuse_aliased_payloads(&aliased, &payloads)
            .expect_err("the same path under two names must refuse");
        assert!(e.contains("firefox"), "names the payload: {e}");
        assert!(e.contains("alias"), "names the input it was reachable as: {e}");
    }

    /// Both map guards are functions a test can call directly, which is what
    /// makes them testable and also what would let either be left UNWIRED with
    /// every assertion above still green. `run_mesboot` is reached only through
    /// the `mesboot-build` argv, so this scans the shipped source for the two
    /// calls and for their ORDER: a check that ran after the steps did would be
    /// a check of nothing.
    #[test]
    fn the_map_guards_are_wired_into_the_runner_before_any_step() {
        let src = include_str!("build.rs");
        let shipped = match src.find("\n#[cfg(test)]\nmod tests {") {
            Some(at) => src.get(..at).unwrap_or(src),
            None => src,
        };
        let body = shipped
            .find("pub fn run_mesboot()")
            .and_then(|at| shipped.get(at..))
            .expect("run_mesboot is in this file");
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} is not called by run_mesboot"));
        let parse = at("parse_payload_map(&text)");
        let alias = at("refuse_aliased_payloads(&inputs, &payloads)");
        let steps = at("for (i, step) in steps.iter().enumerate()");
        assert!(parse < alias, "the map is parsed before it is checked");
        assert!(alias < steps, "both guards run before the first step does");
        // The glob guard is inside the step loop and per-pattern, so its position
        // is the other way round — but it must still run BEFORE the expansion it
        // gates, or it reports on matches already spliced into argv.
        let check = at("ctx.check_glob_dir(pat).map_err(err)?;");
        let expand = at("glob_one_star(pat).map_err(err)?;");
        assert!(steps < check && check < expand, "the glob is gated before it reads");
    }

    /// The data expander's CALLERS are pinned, because the channel is only as
    /// narrow as its call sites.
    ///
    /// `expand_data` resolving `{payload:}` is the whole permission §B.8 grants,
    /// and the compiler has nothing to say about which expander a step picked — a
    /// new step that reached for it, or an existing one changed to, would widen
    /// the channel with every other test in this file still green. Six sites, and
    /// which FIELD each serves, since `copyTree`'s `dest` or `copyFile`'s `to`
    /// taking it would put a payload path on the writable side of a copy.
    #[test]
    fn only_the_six_typed_data_fields_use_the_data_expander() {
        let compact = compact_mesboot_dispatch();
        let sites = [
            "ctx.expand_data(&field(o,\"input\")?)",
            "ctx.expand_data(&field(o,\"from\")?)",
            "ctx.expand_data(&field(o,\"file\")?)",
            "ctx.expand_data_all(&string_array(o,\"roots\").map_err(err)?)",
            "ctx.expand_data_all(&string_array(o,\"packages\").map_err(err)?)",
            "ctx.expand_data_all(&string_array(o,\"runtimes\").map_err(err)?)",
        ];
        assert_eq!(
            compact.match_indices("ctx.expand_data").count(),
            6,
            "only unpack's `input', copyTree's `from', copyFile's `file', \
             stageRuntimeClosure's `roots', and compileApplicationTables' `packages' and \
             `runtimes' may resolve a payload (APPLICATIONS.md section B.8)"
        );
        let positions: Vec<usize> = sites
            .iter()
            .map(|site| {
                compact
                    .find(site)
                    .unwrap_or_else(|| panic!("missing exact data-expander site {site:?}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair.first() < pair.get(1)),
            "the six data-expander fields changed order: {positions:?}"
        );
    }

    /// The payload channel's resolution boundary (APPLICATIONS.md §B.8): a payload
    /// has a name in the DATA expander and none in the command one.
    ///
    /// All four arms matter and each fails differently if it is wrong. A
    /// `{payload:}` that merely MISSED in a command context would fall through to
    /// the unknown-token branch and be emitted as a literal brace — silently, which
    /// is how a recipe would ship pointing a compiler at a string. And `{in:}`
    /// naming a payload has to say WHICH rule refused it, or the ordinary "no input"
    /// sends a reader looking for a lock entry that was withheld on purpose.
    #[test]
    fn a_payload_resolves_for_data_steps_and_is_nameless_to_a_command() {
        let ctx = StepCtx {
            root: "/r".into(),
            src: "/r/src".into(),
            out: "/o".into(),
            tools: "/r/tools".into(),
            jobs: "4".into(),
            inputs: vec![("mes".into(), "/td/store/abc-mes".into())],
            payloads: vec![("firefox".into(), "/td/store/def-firefox".into())],
        };
        assert_eq!(
            ctx.expand_data("{payload:firefox}/lib").unwrap(),
            "/td/store/def-firefox/lib"
        );
        let err = ctx
            .expand("{payload:firefox}/lib")
            .expect_err("a command step must not name a payload");
        assert!(err.contains("runs a command"), "{err}");
        let err = ctx
            .expand("{in:firefox}/lib")
            .expect_err("the tool channel must not reach a payload");
        assert!(err.contains("is a payloadInput"), "{err}");
        // ...and the same refusal from the data expander, which may resolve
        // `{payload:}` but must not launder a payload through `{in:}` either.
        let err = ctx.expand_data("{in:firefox}/lib").expect_err("still refused");
        assert!(err.contains("is a payloadInput"), "{err}");
        // An ordinary input is unaffected in both, or the assertions above would
        // pass for an expander that refused everything.
        for got in [ctx.expand("{in:mes}"), ctx.expand_data("{in:mes}")] {
            assert_eq!(got.unwrap(), "/td/store/abc-mes");
        }
        // A payload NAME that is not declared reds rather than resolving to nothing.
        let err = ctx.expand_data("{payload:nope}").expect_err("unknown payload");
        assert!(err.contains("TD_PAYLOAD_MAP"), "{err}");
    }

    #[test]
    fn mesboot_glob_one_star_matches_sorted_and_rejects_bad_patterns() {
        let d = std::env::temp_dir().join(format!("td-glob-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        for f in ["b.o", "a.o", "c.txt"] {
            fs::write(d.join(f), b"x").unwrap();
        }
        let pat = format!("{}/*.o", d.display());
        let mut hits = glob_one_star(&pat).unwrap();
        hits.sort();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits[0].ends_with("/a.o") && hits[1].ends_with("/b.o"), "{hits:?}");
        assert!(glob_one_star("no-star-here").is_err());
        assert!(glob_one_star("two/*st*ars").is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn mesboot_ld_script_relocation_rewrites_marked_so_and_a_scripts() {
        let d = std::env::temp_dir().join(format!("td-ldscripts-{}", std::process::id()));
        let lib = d.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let script = "/* GNU ld script */\nGROUP ( /td/store/glibc-test/lib/libc.so.6 /td/store/glibc-test/lib/libc_nonshared.a )\n";
        fs::write(lib.join("libc.so"), script).unwrap();
        fs::write(lib.join("libextra.so"), b"not a linker script /td/store/glibc-test/lib/keep").unwrap();
        // A `.a` that IS a GNU ld script (glibc's libm.a) — must be relocated too,
        // matching the busybox static-link fixup that this typed step replaces.
        let marchive = "/* GNU ld script */\nGROUP ( /td/store/glibc-test/lib/libm.so.6 /td/store/glibc-test/lib/libmvec.a )\n";
        fs::write(lib.join("libm.a"), marchive).unwrap();
        // A `.a` that is a genuine `ar` archive — the "GNU ld script" content guard
        // must leave it byte-for-byte untouched even though the extension matches.
        fs::write(lib.join("libreal.a"), b"!<arch>\n/td/store/glibc-test/lib/keep").unwrap();
        fs::write(lib.join("libc.so.6"), b"\x7fELF /td/store/glibc-test/lib/keep").unwrap();

        relocate_ld_scripts(&lib, "/td/store/glibc-test").unwrap();

        let got = fs::read_to_string(lib.join("libc.so")).unwrap();
        assert!(got.contains("GROUP ( libc.so.6 libc_nonshared.a )"), "got: {got}");
        assert!(!got.contains("/td/store/glibc-test/lib/"), "prefix not stripped: {got}");
        let mgot = fs::read_to_string(lib.join("libm.a")).unwrap();
        assert!(mgot.contains("GROUP ( libm.so.6 libmvec.a )"), "got: {mgot}");
        assert!(!mgot.contains("/td/store/glibc-test/lib/"), "prefix not stripped: {mgot}");
        let unmarked = fs::read(lib.join("libextra.so")).unwrap();
        assert!(bytes_contains(&unmarked, b"/td/store/glibc-test/lib/keep"));
        let real = fs::read(lib.join("libreal.a")).unwrap();
        assert!(bytes_contains(&real, b"/td/store/glibc-test/lib/keep"), "real ar archive was rewritten");
        let versioned = fs::read(lib.join("libc.so.6")).unwrap();
        assert!(bytes_contains(&versioned, b"/td/store/glibc-test/lib/keep"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stage0_output_seal_reds_a_gnu_store_byte_and_passes_a_clean_tree() {
        // The stage0 SEAL's output half (#378): any /gnu/store byte in the seed
        // rung's output must red the BUILD in the engine. Verified-red by
        // construction: the poisoned tree errors, the clean twin passes.
        let d = std::env::temp_dir().join(format!("td-stage0-seal-{}", std::process::id()));
        let sub = d.join("AMD64/bin");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("M1"), b"clean bytes").unwrap();
        std::os::unix::fs::symlink("../artifact/M2", sub.join("clean-link")).unwrap();
        assert!(require_no_gnu_store(&d).is_ok(), "a clean tree must pass");
        // Red 1: a /gnu/store byte in file CONTENTS.
        fs::write(sub.join("kaem"), b"oops /gnu/store/abc-glibc leak").unwrap();
        let err = require_no_gnu_store(&d).expect_err("a /gnu/store byte must red");
        assert!(err.contains("/gnu/store"), "diagnostic names the leak: {err}");
        fs::remove_file(sub.join("kaem")).unwrap();
        // Red 2: a symlink whose TARGET points into /gnu/store — invisible to a
        // grep -r content walk, so the engine scan must catch it itself.
        std::os::unix::fs::symlink("/gnu/store/abc-glibc/lib/ld.so", sub.join("leak-link"))
            .unwrap();
        let err = require_no_gnu_store(&d).expect_err("a /gnu/store symlink target must red");
        assert!(err.contains("leak-link"), "diagnostic names the symlink: {err}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stage0_copy_tree_writable_preserves_exec_and_adds_write() {
        // run_stage0's working copy: store trees are read-only (0444/0555) and the
        // kaem build must write into its tree; exec bits must survive the copy.
        let d = std::env::temp_dir().join(format!("td-stage0-copy-{}", std::process::id()));
        let src = d.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("tool"), b"#!x").unwrap();
        fs::set_permissions(&src.join("tool"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::write(src.join("data"), b"d").unwrap();
        fs::set_permissions(&src.join("data"), fs::Permissions::from_mode(0o444)).unwrap();
        let dst = d.join("dst");
        copy_tree_writable(&src, &dst).unwrap();
        let tool = fs::metadata(dst.join("tool")).unwrap().permissions().mode();
        let data = fs::metadata(dst.join("data")).unwrap().permissions().mode();
        assert_eq!(tool & 0o111, 0o111, "exec bit preserved: {tool:o}");
        assert_eq!(tool & 0o200, 0o200, "owner write added: {tool:o}");
        assert_eq!(data & 0o111, 0, "plain file stays non-exec: {data:o}");
        assert_eq!(data & 0o200, 0o200, "owner write added: {data:o}");
        fs::remove_dir_all(&d).unwrap();
    }

    fn runtime_test_item(
        dir: &Path,
        hash: &str,
        name: &str,
        runtime_ref: Option<&str>,
    ) -> (String, PathBuf) {
        let canonical = format!("/td/store/{hash}-{name}");
        let physical = dir.join(format!("{hash}-{name}"));
        fs::create_dir_all(&physical).unwrap();
        fs::write(physical.join("payload"), b"runtime payload").unwrap();
        if let Some(reference) = runtime_ref {
            std::os::unix::fs::symlink(reference, physical.join("runtime-ref")).unwrap();
        }
        (canonical, physical)
    }

    fn runtime_needed_elf(needed: &str) -> Vec<u8> {
        const EHDR: usize = 64;
        const PHENT: usize = 56;
        const DYNENT: usize = 16;
        let dyn_off = EHDR + 2 * PHENT;
        let dyn_size = 3 * DYNENT;
        let strtab_off = dyn_off + dyn_size;
        let total = strtab_off + 1 + needed.len() + 1;
        let mut bytes = vec![0u8; total];
        bytes.get_mut(0..4).unwrap().copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        let put_u64 = |bytes: &mut [u8], offset: usize, value: u64| {
            bytes
                .get_mut(offset..offset + 8)
                .unwrap()
                .copy_from_slice(&value.to_le_bytes());
        };
        put_u64(&mut bytes, 0x20, EHDR as u64);
        bytes
            .get_mut(0x36..0x38)
            .unwrap()
            .copy_from_slice(&(PHENT as u16).to_le_bytes());
        bytes
            .get_mut(0x38..0x3a)
            .unwrap()
            .copy_from_slice(&2u16.to_le_bytes());

        bytes
            .get_mut(EHDR..EHDR + 4)
            .unwrap()
            .copy_from_slice(&1u32.to_le_bytes());
        put_u64(&mut bytes, EHDR + 0x20, total as u64);
        let dynamic = EHDR + PHENT;
        bytes
            .get_mut(dynamic..dynamic + 4)
            .unwrap()
            .copy_from_slice(&2u32.to_le_bytes());
        put_u64(&mut bytes, dynamic + 0x08, dyn_off as u64);
        put_u64(&mut bytes, dynamic + 0x10, dyn_off as u64);
        put_u64(&mut bytes, dynamic + 0x20, dyn_size as u64);

        put_u64(&mut bytes, dyn_off, 5);
        put_u64(&mut bytes, dyn_off + 8, strtab_off as u64);
        put_u64(&mut bytes, dyn_off + DYNENT, 1);
        put_u64(&mut bytes, dyn_off + DYNENT + 8, 1);
        bytes
            .get_mut(strtab_off + 1..strtab_off + 1 + needed.len())
            .unwrap()
            .copy_from_slice(needed.as_bytes());
        bytes
    }

    #[test]
    fn runtime_closure_stages_transitive_declared_refs_once() {
        let d = test_dir("runtime-closure");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let hash_c = "cccccccccccccccccccccccccccccccc";
        let path_a = format!("/td/store/{hash_a}-app");
        let path_b = format!("/td/store/{hash_b}-lib");
        let path_c = format!("/td/store/{hash_c}-loader");
        let (a, a_disk) = runtime_test_item(&d, hash_a, "app", None);
        let (b, b_disk) = runtime_test_item(&d, hash_b, "lib", Some(&path_c));
        let (c, c_disk) = runtime_test_item(&d, hash_c, "loader", Some(&path_a));
        fs::write(a_disk.join("app"), runtime_needed_elf(&path_b)).unwrap();
        let candidates = BTreeMap::from([
            (a.clone(), a_disk),
            (b.clone(), b_disk),
            (c.clone(), c_disk),
        ]);
        let declared = BTreeSet::from([a.clone(), b.clone(), c.clone()]);
        let dest = d.join("root");

        let closure = stage_runtime_closure_from_index(
            &candidates,
            &declared,
            &declared,
            &[a.clone(), c],
            "/td/store",
            &dest,
        )
        .unwrap();

        assert_eq!(closure, BTreeSet::from([a, path_b, path_c]));
        for path in &closure {
            let relative = Path::new(path).strip_prefix("/").unwrap();
            assert!(
                dest.join(relative).join("payload").is_file(),
                "closure member was not staged at its canonical path: {path}"
            );
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn runtime_closure_rejects_undeclared_and_foreign_refs() {
        let d = test_dir("runtime-closure-reject");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let undeclared = format!("/td/store/{hash_b}-ambient");
        let (app_path, app_disk) = runtime_test_item(&d, hash_a, "app", None);
        let candidates = BTreeMap::from([(app_path.clone(), app_disk.clone())]);
        let declared = BTreeSet::from([app_path.clone()]);

        fs::write(app_disk.join("payload"), &undeclared).unwrap();
        assert_eq!(
            runtime_store_closure(
                &candidates,
                &declared,
                &declared,
                &[app_path.clone()],
                "/td/store",
            )
            .unwrap(),
            BTreeSet::from([app_path.clone()]),
            "a provenance string outside loader metadata is not a runtime edge"
        );
        std::os::unix::fs::symlink(&undeclared, app_disk.join("runtime-ref")).unwrap();
        let error = runtime_store_closure(
            &candidates,
            &declared,
            &declared,
            &[app_path.clone()],
            "/td/store",
        )
        .unwrap_err();
        assert!(
            error.contains("references undeclared recipe input") && error.contains(&undeclared),
            "{error}"
        );

        let foreign = format!("/gnu/store/{hash_b}-foreign");
        let (_foreign_path, foreign_disk) = runtime_test_item(&d, hash_b, "foreign", None);
        fs::remove_file(app_disk.join("runtime-ref")).unwrap();
        std::os::unix::fs::symlink(&foreign, app_disk.join("runtime-ref")).unwrap();
        let candidates = BTreeMap::from([
            (app_path.clone(), app_disk),
            (foreign.clone(), foreign_disk),
        ]);
        let declared = BTreeSet::from([app_path.clone(), foreign.clone()]);
        let error =
            runtime_store_closure(&candidates, &declared, &declared, &[app_path], "/td/store")
                .unwrap_err();
        assert!(
            error.contains("outside active store") && error.contains(&foreign),
            "{error}"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn runtime_closure_rejects_a_root_not_declared_directly() {
        let path = "/td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app".to_string();
        let error = runtime_store_closure(
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &[path],
            "/td/store",
        )
        .unwrap_err();
        assert!(error.contains("root") && error.contains("not a declared recipe input"), "{error}");
    }

    /// The two gates differ by exactly the payloads, and the difference is the
    /// finding: adding payloads to `declared` for the ROOT check silently widened
    /// the REFERENCE walk too, so an ordinary input embedding a payload's store
    /// path would drag it into the staged tree with no step naming it — and
    /// `copy_store_item_writable` writes it out with its exec bits, on a writable
    /// mount, which is the `noexec` bind undone by another door.
    #[test]
    fn a_payload_may_be_a_root_but_may_not_be_reached_from_an_ordinary_input() {
        let d = std::env::temp_dir().join(format!("td-payload-refs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let hash_app = "a".repeat(32);
        let hash_pay = "b".repeat(32);
        let (app_path, app_disk) = runtime_test_item(&d, &hash_app, "app", None);
        let (pay_path, pay_disk) = runtime_test_item(&d, &hash_pay, "payload", None);
        let candidates = BTreeMap::from([
            (app_path.clone(), app_disk.clone()),
            (pay_path.clone(), pay_disk),
        ]);
        let inputs_only = BTreeSet::from([app_path.clone()]);
        let with_payload = BTreeSet::from([app_path.clone(), pay_path.clone()]);

        // As a ROOT, a payload is fine — a step named it.
        assert!(
            runtime_store_closure(
                &candidates,
                &with_payload,
                &inputs_only,
                &[pay_path.clone()],
                "/td/store",
            )
            .is_ok(),
            "a step may name a payload as a runtime root"
        );
        // REACHED from an ordinary input, it must red rather than be staged.
        std::os::unix::fs::symlink(&pay_path, app_disk.join("runtime-ref")).unwrap();
        let error = runtime_store_closure(
            &candidates,
            &with_payload,
            &inputs_only,
            &[app_path.clone()],
            "/td/store",
        )
        .unwrap_err();
        assert!(
            error.contains("references undeclared recipe input") && error.contains(&pay_path),
            "{error}"
        );
        // ...and it WOULD have been staged silently with one shared gate, which is
        // what this test exists to keep from coming back.
        assert!(
            runtime_store_closure(
                &candidates,
                &with_payload,
                &with_payload,
                &[app_path],
                "/td/store",
            )
            .is_ok_and(|c| c.contains(&pay_path)),
            "the one-gate form must be the thing that stages it, or this proves nothing"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    /// The other direction, which the inputs-only reference gate got WRONG: it
    /// applied to every walked item, so a payload could not reach its own store
    /// path or a second declared payload. AGENTS.md has an application NAME its
    /// runtime through this channel, and an absolute RUNPATH into its own path
    /// is the ordinary shape of a self-contained payload — so both errored on
    /// the exact arrangement §B.8 describes, and rung 5 would have hit it first.
    #[test]
    fn a_payload_may_reach_itself_and_a_second_payload() {
        let d = std::env::temp_dir().join(format!("td-payload-self-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let hash_app = "a".repeat(32);
        let hash_pay = "b".repeat(32);
        let hash_rt = "c".repeat(32);
        let (app_path, app_disk) = runtime_test_item(&d, &hash_app, "app", None);
        let (rt_path, rt_disk) = runtime_test_item(&d, &hash_rt, "runtime", None);
        let (pay_path, pay_disk) = runtime_test_item(&d, &hash_pay, "payload", None);
        // A payload naming its own path (RUNPATH into itself) and its runtime.
        std::os::unix::fs::symlink(&pay_path, pay_disk.join("self-ref")).unwrap();
        std::os::unix::fs::symlink(&rt_path, pay_disk.join("runtime-ref")).unwrap();
        let candidates = BTreeMap::from([
            (app_path.clone(), app_disk),
            (rt_path.clone(), rt_disk),
            (pay_path.clone(), pay_disk),
        ]);
        // The sets the CALLER builds, not sets composed for this assertion.
        let (roots, refs) = declared_sets(
            &[("app".to_string(), app_path)],
            &[
                ("firefox".to_string(), pay_path.clone()),
                ("runtime".to_string(), rt_path.clone()),
            ],
        );
        let closure =
            runtime_store_closure(&candidates, &roots, &refs, &[pay_path.clone()], "/td/store")
                .expect("a payload may name its runtime and itself");
        assert!(closure.contains(&pay_path) && closure.contains(&rt_path), "{closure:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    /// The split asserted at the point it is MADE. Re-widening the reference set
    /// to the union inside the caller left the parameter-level test above green,
    /// because that one passes an inputs-only set in explicitly.
    #[test]
    fn the_declared_sets_differ_by_exactly_the_payloads() {
        let inputs = [("gcc".to_string(), "/td/store/aaa-gcc".to_string())];
        let payloads = [("firefox".to_string(), "/td/store/bbb-firefox".to_string())];
        let (roots, refs) = declared_sets(&inputs, &payloads);
        assert_eq!(
            roots,
            BTreeSet::from([
                "/td/store/aaa-gcc".to_string(),
                "/td/store/bbb-firefox".to_string()
            ])
        );
        assert_eq!(refs, BTreeSet::from(["/td/store/aaa-gcc".to_string()]));
        // With no payload the two are equal, which is why landing the split
        // changed nothing for any recipe in the tree.
        let (roots, refs) = declared_sets(&inputs, &[]);
        assert_eq!(roots, refs);
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
        let (sh, envs) = sh_and_env();
        let loop_forever = "while :; do echo 'expr: error while loading shared libraries: \
                            libgmp.so.10: cannot open shared object file' >&2; done";
        let t0 = Instant::now();
        let err = run_cmd(&sh, &["-c", loop_forever], ".", &envs, &w(0, 25, 0))
            .expect_err("a persistently-failing tool loop must red");
        assert!(t0.elapsed() < Duration::from_secs(30), "must red promptly, not spin: {err}");
        assert!(err.contains("td-build watchdog KILLED"), "names the watchdog: {err}");
        assert!(err.contains("repeated 25x"), "names the repeat bound: {err}");
        assert!(
            err.contains("expr: error while loading shared libraries"),
            "quotes the failing tool's stderr: {err}"
        );
    }

    #[test]
    fn watchdog_reds_a_stdout_spinning_loop_too() {
        // A retry spin that prints to STDOUT would reset the silence clock on
        // every line and escape a stderr-only repeat bound — under a repeat
        // bound both streams are line-watched, so it reds all the same.
        let (sh, envs) = sh_and_env();
        let t0 = Instant::now();
        let err = run_cmd(
            &sh,
            &["-c", "while :; do echo 'configure: retrying tool probe'; done"],
            ".",
            &envs,
            &w(0, 25, 0),
        )
        .expect_err("a stdout-spinning loop must red");
        assert!(t0.elapsed() < Duration::from_secs(30), "must red promptly: {err}");
        assert!(err.contains("stdout line repeated 25x"), "names the stream: {err}");
    }

    #[test]
    fn watchdog_reds_a_silently_wedged_phase() {
        // The silent variant: configure often sends a helper's stderr to
        // /dev/null or config.log, so the spin produces NO output. The silence
        // bound is the backstop; the whole process group dies (the exec'd
        // sleep included), so the test returns instead of waiting 300s.
        let (sh, envs) = sh_and_env();
        let t0 = Instant::now();
        let err = run_cmd(&sh, &["-c", "exec sleep 300"], ".", &envs, &w(1, 0, 0))
            .expect_err("a silent wedged phase must red");
        assert!(t0.elapsed() < Duration::from_secs(30), "must red at the bound: {err}");
        assert!(err.contains("no output for 1s"), "names the silence bound: {err}");
        assert!(err.contains("td-build watchdog KILLED"), "names the watchdog: {err}");
    }

    #[test]
    fn watchdog_reds_a_make_nested_chatty_sub_configure_spin() {
        // #339: a #292-shape broken-tool loop nested INSIDE a `make` phase — a
        // bundled sub-`./configure` the Makefile re-runs — that spins CHATTILY
        // (constant identical output at 100% CPU) resets the silence clock on
        // every line, so the silence bound never trips; and WATCH_PHASE carries
        // NO count bound (`tar xf` repeats a warning per member). Only the
        // sustained-DURATION bound catches it. Modeled with a phase-shaped Watch
        // — silence OFF, count OFF, a tiny repeat window — so the duration bound
        // is the sole thing that can red it, and the spin prints to STDOUT to
        // prove stdout is line-watched under the phase's duration bound. Verified
        // red: with the duration bound neutered this run_cmd never returns
        // (silence + count both off), the test hangs past any bound.
        let (sh, envs) = sh_and_env();
        let make_nested_spin = "echo 'make: Entering directory subdir'; \
            while :; do echo 'configure: error: cannot run C compiled programs'; done";
        let t0 = Instant::now();
        let err = run_cmd(&sh, &["-c", make_nested_spin], ".", &envs, &w(0, 0, 500))
            .expect_err("a chatty make-nested spin must red on the duration bound");
        assert!(t0.elapsed() < Duration::from_secs(30), "must red at the window, not spin: {err}");
        assert!(err.contains("td-build watchdog KILLED"), "names the watchdog: {err}");
        assert!(err.contains("kept arriving for 500ms"), "names the duration bound: {err}");
        assert!(err.contains("stdout"), "names the spinning stream: {err}");
        assert!(
            err.contains("configure: error: cannot run C compiled programs"),
            "quotes the spinning sub-configure line: {err}"
        );
    }

    #[test]
    fn watchdog_spares_a_healthy_high_volume_repeating_phase() {
        // The false-kill guard (#339): a healthy phase may print the SAME line at
        // high volume — `tar xf` of a many-member pax tarball emits an identical
        // "Ignoring unknown extended header keyword" warning per member — but it
        // COMPLETES; the line stops arriving long before the window. Under a
        // phase-shaped Watch (count OFF, a repeat window far above the burst's
        // runtime) the duration bound must NOT trip: 50k identical lines — vastly
        // more than any count bound would tolerate — exit 0 and stay GREEN,
        // because it is the DURATION (not the count) the phase bound keys on.
        // Verified red: dropping the `now - run_start >= repeat_ms` gate (trip on
        // volume alone) reds this while the spin test above still passes.
        let (sh, envs) = sh_and_env();
        // A POSIX `while` counter emits the burst and then EXITS, completing
        // before the window — no bash `{1..N}` brace expansion (busybox ash lacks
        // it) and no `yes`/`seq` (absent from the loop's busybox userland). 50000
        // builtin-`echo` iterations finish in well under the 5s window, so it is
        // the DURATION, not the count, that a phase bound could key on.
        let tar_like = "i=0; while [ \"$i\" -lt 50000 ]; do \
            echo 'tar: Ignoring unknown extended header keyword'; i=$((i+1)); done; echo done-ok";
        let t0 = Instant::now();
        run_cmd(&sh, &["-c", tar_like], ".", &envs, &w(0, 0, 5000))
            .expect("a healthy high-volume identical burst that COMPLETES must stay green");
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "the burst must finish well within the window, not wedge"
        );
    }

    #[test]
    fn account_line_seeds_run_start_on_first_line_and_honors_keep_tail() {
        // Two #339-review invariants, tested at the accountant directly.
        //
        // (1) run_start_ms is seeded on the FIRST accounted line even when it
        //     equals the empty-sentinel last_line — otherwise the duration
        //     window would measure from process start (t=0), and an empty
        //     first line could false-trip. Verified red: without the
        //     `repeats > 0` guard the empty first line is counted as a repeat
        //     of the sentinel with run_start_ms stuck at 0, so the second empty
        //     line at t=1100 (1100 - 0 >= 300) trips.
        let why = Mutex::new(None);
        let mut st = StreamWatch::new();
        account_line(&mut st, b"", 0, 300, 1000, true, "stderr", &why);
        assert_eq!(
            (st.repeats, st.run_start_ms),
            (1, 1000),
            "empty first line starts a run seeded at its arrival, not t=0"
        );
        account_line(&mut st, b"", 0, 300, 1100, true, "stderr", &why);
        assert_eq!(st.repeats, 2);
        assert!(why.lock().unwrap().is_none(), "100ms < 300ms window must not trip");

        // (2) keep_tail gates the distinct-line tail: the stdout watcher passes
        //     false, so a verbose build allocates no clip_line String per line;
        //     the stderr watcher passes true (its tail feeds the diagnostic).
        //     Verified red: without the gate stdout would keep a 2-entry tail.
        let why2 = Mutex::new(None);
        let mut sout = StreamWatch::new();
        account_line(&mut sout, b"line-a", 0, 300, 0, false, "stdout", &why2);
        account_line(&mut sout, b"line-b", 0, 300, 0, false, "stdout", &why2);
        assert!(sout.tail.is_empty(), "keep_tail=false keeps no diagnostic tail");
        let mut serr = StreamWatch::new();
        account_line(&mut serr, b"e-a", 0, 300, 0, true, "stderr", &why2);
        account_line(&mut serr, b"e-b", 0, 300, 0, true, "stderr", &why2);
        assert_eq!(serr.tail.len(), 2, "keep_tail=true records the distinct-line tail");
    }

    #[test]
    fn watchdog_keeps_a_green_exit_despite_a_repeating_straggler_during_drain() {
        // A command exits 0 but leaves a background straggler that SPAMS the
        // same stderr line while holding the pipes open. The straggler waits
        // until the parent shell PID has disappeared; because a zombie still
        // answers kill -0, that means run_cmd has reaped the shell and entered
        // the drain phase. It then emits enough repeated lines to record a trip
        // reason and sleeps while holding the pipe open, but the kill path is no
        // longer reachable: `killed` stays false, the recorded reason is dropped,
        // and the exit status (0) decides. The drain grace kills the straggler
        // group so run_cmd still returns promptly.
        let (sh, envs) = sh_and_env();
        let t0 = Instant::now();
        run_cmd(
            &sh,
            &[
                "-c",
                "parent=$$; (while kill -0 \"$parent\" 2>/dev/null; do :; done; \
                 i=0; while [ \"$i\" -lt 30 ]; do echo 'expr: died again' >&2; i=$((i+1)); done; \
                 sleep 30) & exit 0",
            ],
            ".",
            &envs,
            &w(600, 25, 0),
        )
        .expect("a green exit must win over a straggler's repeat spam during drain");
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the drain grace must bound the repeating straggler"
        );
    }

    #[test]
    fn watchdog_drain_bounds_a_pipe_holding_straggler_and_keeps_the_green_exit() {
        // A phase that exits 0 but leaves a background child holding the
        // output pipes must NOT hang run_cmd (the old .status() semantics
        // returned at exit; the readers must not wait 30s for the sleep) and
        // must NOT red: the drain grace kills the straggler group, and the
        // command's own exit status decides.
        let (sh, envs) = sh_and_env();
        let t0 = Instant::now();
        run_cmd(&sh, &["-c", "sleep 30 & exit 0"], ".", &envs, &w(600, 0, 0))
            .expect("a green exit with a straggler must stay green");
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the drain grace must bound the straggler well before its natural 30s exit"
        );
    }

    #[test]
    fn watchdog_passes_a_healthy_command_and_sub_limit_repeats() {
        // Green control: repeats BELOW the bound (24 identical stderr lines,
        // limit 25) exit green — the guard trips on the pathological loop, not
        // on a noisy-but-terminating tool. And a changing stderr stream resets
        // the counter, so far more total lines than the limit stay green.
        let (sh, envs) = sh_and_env();
        // POSIX `while` counters (no `seq`/`yes`, absent from the busybox
        // userland; no bash C-style `for (( ))`, which busybox ash lacks).
        let noisy = "i=0; while [ \"$i\" -lt 24 ]; do echo 'same warning' >&2; i=$((i+1)); done; echo done-ok";
        run_cmd(&sh, &["-c", noisy], ".", &envs, &w(600, 25, 0))
            .expect("sub-limit repeats must stay green");
        let alternating =
            "i=0; while [ \"$i\" -lt 40 ]; do echo \"warn $((i % 2))\" >&2; i=$((i+1)); done; echo done-ok";
        run_cmd(&sh, &["-c", alternating], ".", &envs, &w(600, 25, 0))
            .expect("alternating stderr lines must reset the repeat counter");
    }

    #[test]
    fn watchdog_keeps_the_plain_failure_contract() {
        // A normal non-zero exit still reds with the pre-#308 message shape —
        // the supervisor changes HOW output is carried, not the pass/fail
        // contract.
        let (sh, envs) = sh_and_env();
        let err = run_cmd(&sh, &["-c", "echo out; echo err >&2; exit 3"], ".", &envs, &WATCH_PHASE)
            .expect_err("exit 3 must red");
        assert!(err.contains("failed"), "plain failure message kept: {err}");
        run_cmd(&sh, &["-c", "true"], ".", &envs, &WATCH_PHASE).expect("true is green");
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
    fn configure_log_tails_surfaces_the_failing_probe_bounded_and_shallow_first() {
        // #366: on a configure failure the real cause is in config.log, never on
        // the terminal (autoconf redirects conftest output there). The engine
        // must surface the TAIL of every config.log under the build tree — the
        // failing conftest — bounded (a real tail, not a whole-file dump) and
        // top-level first (a gnulib probe like socklen_t writes the top-level log).
        let base = std::env::temp_dir().join(format!("td-configlog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let srcdir = base.join("sed-4.9");
        fs::create_dir_all(srcdir.join("lib")).unwrap();

        // A realistic top-level config.log with the SHAPE a real one has: an
        // ancient sentinel the tail must drop, a long filler stretch, the exact
        // conftest failure a compiler killed under memory pressure produces (the
        // #366 symptom), then — critically — autoconf's EXIT-trap debug dump
        // (`## Cache variables ##` … `## confdefs.h ##` + hundreds of `#define`s).
        // That dump is what a naive file-tail would surface INSTEAD of the
        // failure, so the tail must be cut at the dump marker (the fix the code
        // review caught: a blind `tail 40` shows only the `#define` spam below).
        let mut top = String::from("ANCIENT-PREAMBLE-SENTINEL must be dropped by the tail\n");
        for i in 0..200 {
            top.push_str(&format!("configure:{i}: checking a harmless earlier probe\n"));
        }
        top.push_str("configure:4033: checking for socklen_t\n");
        top.push_str("configure:4041: gcc -c -O2 conftest.c >&5\n");
        top.push_str("gcc: fatal error: Killed signal terminated program cc1\n");
        top.push_str("configure:4041: $? = 1\n");
        top.push_str("configure: error: Cannot find a type to use in place of socklen_t\n");
        // autoconf's trailing debug dump — 300 lines of cache/confdefs noise that
        // must NOT drown the failure above it.
        top.push_str("## ---------------- ##\n## Cache variables. ##\n## ---------------- ##\n");
        for i in 0..120 {
            top.push_str(&format!("ac_cv_probe_{i}=yes\n"));
        }
        top.push_str("## ----------------- ##\n## Output variables. ##\n## ----------------- ##\n");
        top.push_str("## ----------- ##\n## confdefs.h. ##\n## ----------- ##\n");
        for i in 0..180 {
            top.push_str(&format!("#define CONFDEFS_DUMP_SPAM_{i} 1\n"));
        }
        top.push_str("configure: exit 1\n");
        fs::write(srcdir.join("config.log"), &top).unwrap();

        // A sub-configure (AC_CONFIG_SUBDIRS) log, one level deeper, must ALSO be
        // surfaced — the failure may be in a bundled sub-package's configure.
        fs::write(srcdir.join("lib").join("config.log"), "sub-configure: a different failure\n")
            .unwrap();

        let out = configure_log_tails(&srcdir);

        // The failing probe AND its real cause (the killed compiler) are surfaced.
        assert!(
            out.contains("Cannot find a type to use in place of socklen_t"),
            "the socklen_t failure line must be surfaced: {out}"
        );
        assert!(
            out.contains("Killed signal terminated program cc1"),
            "the killed-compiler evidence (the real cause) must be surfaced: {out}"
        );
        // It is a TAIL, not a dump: the ancient sentinel (200+ lines back) is gone.
        assert!(
            !out.contains("ANCIENT-PREAMBLE-SENTINEL"),
            "lines older than the tail window must be dropped (a tail, not a dump): {out}"
        );
        // THE FIX (code-review Finding 1): autoconf's trailing cache/confdefs dump
        // must be trimmed, NOT surfaced — a blind file tail would show only these
        // 300 lines of `#define` noise and miss the failure above. This is what
        // reds against the naive tail-the-whole-file implementation.
        assert!(
            !out.contains("CONFDEFS_DUMP_SPAM"),
            "autoconf's trailing confdefs dump must be trimmed, not surfaced: {out}"
        );
        assert!(
            !out.contains("ac_cv_probe_"),
            "autoconf's trailing cache-variables dump must be trimmed, not surfaced: {out}"
        );
        // Both logs surface, and the top-level leads the deeper sub-configure's.
        let top_at = out.find("sed-4.9/config.log").expect("top-level log named");
        let sub_at = out.find("lib/config.log").expect("sub-configure log named");
        assert!(top_at < sub_at, "the top-level config.log must lead the sub-configure's: {out}");
        assert!(out.contains("sub-configure: a different failure"), "sub log surfaced: {out}");

        // No config.log anywhere ⇒ empty addendum (the configure error is left
        // exactly as it was — the diagnostic only ADDS, never rewrites).
        let empty = base.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(configure_log_tails(&empty).is_empty(), "no logs ⇒ no addendum");

        // The count bound is load-bearing (a tree of sub-configures cannot flood
        // the log): five logs, asked for at most two, yields two.
        for d in ["a", "b", "c", "d", "e"] {
            let sub = base.join("many").join(d);
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("config.log"), "x\n").unwrap();
        }
        assert_eq!(find_config_logs(&base.join("many"), 2).len(), 2, "MAX_LOGS bound holds");

        let _ = fs::remove_dir_all(&base);
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
