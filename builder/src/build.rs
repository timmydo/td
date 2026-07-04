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
    let line = match std::str::from_utf8(bytes.get(..nl).unwrap_or_default()) {
        Ok(s) => s,
        Err(_) => return Ok(()), // binary first line — skip
    };
    // "#!  /bin/sh -e"  ->  interp="/bin/sh", trailing=" -e"
    let after = line.get(2..).unwrap_or_default().trim_start();
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

/// Default phase bound: make can legitimately be silent for minutes while one
/// big translation unit compiles; 30 minutes is comfortably past the corpus'
/// worst single-file case (the /td/store bootstrap chain does NOT run through
/// run_cmd — bootstrap.rs/toolchain_x86_64.rs have their own runners), while
/// still bounding a truly-wedged phase. No COUNT repeat bound (`tar xf` repeats
/// a warning per member); the `repeat_secs` DURATION bound (5 min of the same
/// line still arriving) is the #339 make-nested chatty-spin catch — comfortably
/// above any real burst (a tar of a huge tarball finishes in a minute or two,
/// its warning does not keep arriving for five straight minutes) yet well under
/// the 30-min silence backstop, so a broken sub-configure reds in minutes.
const WATCH_PHASE: Watch = Watch {
    silence: Duration::from_secs(1800),
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
/// transient children included) and is invisible to the post-#328 cgroup
/// memory enforcement (descendants inherit the gate cgroup regardless of
/// pgroup). The gate runner's FALLBACK pgroup-RSS sampler, used only where no
/// delegated cgroup exists, does lose sight of phase children for a build run
/// in-gate — the per-process RLIMIT_DATA cap still binds each of them.
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
                    let _ = crate::sys::kill_process_group(pgid, crate::sys::SIGKILL);
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
                    let _ = crate::sys::kill_process_group(pgid, crate::sys::SIGKILL);
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
                if sup.why.lock().map(|w| w.is_some()).unwrap_or(false) {
                    let _ = crate::sys::kill_process_group(pgid, crate::sys::SIGKILL);
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
        let err = run_cmd(&bash, &["-c", loop_forever], ".", &envs, &w(0, 25, 0))
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
        let (bash, envs) = bash_and_env();
        let t0 = Instant::now();
        let err = run_cmd(
            &bash,
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
        let (bash, envs) = bash_and_env();
        let t0 = Instant::now();
        let err = run_cmd(&bash, &["-c", "exec sleep 300"], ".", &envs, &w(1, 0, 0))
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
        let (bash, envs) = bash_and_env();
        let make_nested_spin = "echo 'make: Entering directory subdir'; \
            while :; do echo 'configure: error: cannot run C compiled programs'; done";
        let t0 = Instant::now();
        let err = run_cmd(&bash, &["-c", make_nested_spin], ".", &envs, &w(0, 0, 500))
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
        let (bash, envs) = bash_and_env();
        let tar_like =
            "yes 'tar: Ignoring unknown extended header keyword' | head -n 50000; echo done-ok";
        let t0 = Instant::now();
        run_cmd(&bash, &["-c", tar_like], ".", &envs, &w(0, 0, 5000))
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
        // same stderr line while holding the pipes open. The repeat accountant
        // sees the spam and records a trip reason — but the main command
        // already exited, so the loop is in the drain phase and never enters
        // the kill path: `killed` stays false, the recorded reason is dropped,
        // and the exit status (0) decides. The drain grace kills the straggler
        // group so run_cmd still returns promptly. This is the guarantee that a
        // repeat reason recorded WITHOUT a kill never overrides a real exit.
        let (bash, envs) = bash_and_env();
        let t0 = Instant::now();
        run_cmd(
            &bash,
            &["-c", "while :; do echo 'expr: died again' >&2; done & exit 0"],
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
        let (bash, envs) = bash_and_env();
        let t0 = Instant::now();
        run_cmd(&bash, &["-c", "sleep 30 & exit 0"], ".", &envs, &w(600, 0, 0))
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
        let (bash, envs) = bash_and_env();
        let noisy = "for i in $(seq 24); do echo 'same warning' >&2; done; echo done-ok";
        run_cmd(&bash, &["-c", noisy], ".", &envs, &w(600, 25, 0))
            .expect("sub-limit repeats must stay green");
        let alternating =
            "for i in $(seq 40); do echo \"warn $((i % 2))\" >&2; done; echo done-ok";
        run_cmd(&bash, &["-c", alternating], ".", &envs, &w(600, 25, 0))
            .expect("alternating stderr lines must reset the repeat counter");
    }

    #[test]
    fn watchdog_keeps_the_plain_failure_contract() {
        // A normal non-zero exit still reds with the pre-#308 message shape —
        // the supervisor changes HOW output is carried, not the pass/fail
        // contract.
        let (bash, envs) = bash_and_env();
        let err = run_cmd(&bash, &["-c", "echo out; echo err >&2; exit 3"], ".", &envs, &WATCH_PHASE)
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
