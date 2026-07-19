//! Host-side qemu boot validation (re #529): boot the td-source-built
//! linux-x86-64 kernel under HOST qemu and prove it reaches a real userland.
//! Reached only through the `td-recipe-eval qemu-boot` subcommand
//! (check_runner::qemu_boot_cli), NOT a gated recipe check.
//!
//! Why host-side, not a daily gate check — a qemu boot needs a real
//! `qemu-system-x86_64`, and td has no such artifact. The daily gate wraps every
//! recipe check in a host-free `pivot_root` sandbox that exposes only td-built
//! tools, each reachable by absolute /td/store path (that is how the RustToolchain
//! check runs the td-BUILT rustc). A HOST binary like qemu is simply not present
//! in that sandbox, so a gate-registered boot check would fail on `find_qemu` on
//! every real runner — a permanently-red, green-washed check. Booting therefore
//! only makes sense OUTSIDE the sandbox, run on the host by an operator or
//! developer; `build_plan()` still builds the kernel host-free inside its own
//! nested build jail, and only the resulting bzImage + initramfs are handed to
//! host qemu.
//!
//! Trust model — host qemu is a control-plane TEST tool, not a target input.
//! Every byte of the ARTIFACT under test is td-built and host-free: the bzImage
//! is compiled by td's native GCC/binutils/glibc ladder, and the initramfs is a
//! statically-linked td-built busybox plus a shell /init. `qemu-system-x86_64`
//! only supplies the virtual machine that RUNS that artifact — exactly as the
//! host Rust toolchain is a control-plane SEED that compiles td's control-plane
//! programs yet never enters a target closure. qemu is never on a recipe's PATH
//! or argv and contributes nothing to any /td/store output. Adding host qemu as a
//! test oracle is a new host dependency (AGENTS.md directive 3): it is the
//! explicitly requested mechanism for booting the kernel, confined to this
//! host-side TEST tool — it never enters the target artifact graph. If host qemu
//! is absent the tool FAILS loudly rather than silently passing, so a green result
//! always means a real boot happened.
use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::check_runner::{is_executable, RecipeCheckRunner};

/// The busybox /init prints this exact line on ttyS0 once the kernel has reached
/// userspace and executed the static busybox userland. It must match the /init
/// script's `echo` in recipes/src/recipes/linux-x86-64.rs.
const MARKER: &str = "TD-USERLAND-OK";

/// Default wall-clock ceiling. A tiny allnoconfig kernel boots to userspace under
/// TCG in a few seconds, but TCG on a loaded builder can be slow; 180s is
/// generous. The poll loop kills qemu the instant the marker appears, so a healthy
/// boot returns in seconds — the ceiling only bounds a FAILED boot (panic without
/// reboot, a wedged userland) so the check reds instead of hanging forever. The
/// `TD_QEMU_BOOT_TIMEOUT_SECS` env var overrides it (for a slower CI host or a
/// faster local smoke test).
const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 180;
const POLL: Duration = Duration::from_millis(200);

/// Cap on retained console/diagnostic bytes. The console is scanned incrementally
/// and only the last CAP bytes are kept, so a kernel that floods ttyS0 without
/// panicking cannot balloon memory or turn the poll loop quadratic. The marker is
/// latched the moment it is seen, so trimming older bytes never loses it.
const CAP: usize = 256 * 1024;

/// Per-poll read budget. Bounds the inner drain loop so the outer deadline check
/// runs regularly even if qemu writes ttyS0 as fast as we read it.
const DRAIN_BUDGET: usize = 4 * 1024 * 1024;

/// How the boot loop terminated. Success is decided SOLELY by whether the marker
/// reached the console; this only labels a FAILED boot's diagnostics.
enum EndReason {
    MarkerSeen,
    QemuExited(ExitStatus),
    TimedOut(u64),
}

/// Outcome of a boot attempt.
struct BootResult {
    /// The userland marker reached ttyS0 — the sole success criterion.
    marker: bool,
    /// How the boot loop ended, for a FAILED boot's error message.
    reason: String,
    /// Bounded, lossily-decoded tail of ttyS0 (or qemu's own diagnostics if ttyS0
    /// was empty), for error context.
    console: String,
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    // Build the kernel producer (its own stem, as RustToolchain builds
    // rust-toolchain) to get the bzImage + initramfs.cpio, then boot them.
    runner.prepare_recipe_target("linux-x86-64")?;
    let build_out = runner.build_plan("linux-x86-64")?;
    let tree = runner.ladder_out_from(&build_out, "linux-x86-64")?;
    let bzimage = tree.join("bzImage");
    let initramfs = tree.join("initramfs.cpio");
    for (label, path) in [("bzImage", &bzimage), ("initramfs.cpio", &initramfs)] {
        if !path.is_file() {
            return Err(format!(
                "linux-x86-64 output is missing {label} ({}) — the boot check needs both the kernel and its userland",
                path.display()
            ));
        }
    }

    let qemu = find_qemu()?;
    println!(
        "   [qemu-boot] {qemu} boots the td-source-built bzImage under TCG with the busybox initramfs\n              kernel:    {}\n              initramfs: {}",
        bzimage.display(),
        initramfs.display()
    );

    let result = boot(&qemu, &bzimage, &initramfs)?;
    if !result.marker {
        return Err(format!(
            "kernel did not reach the userland marker {MARKER:?} on ttyS0 — {} \
             (no console output, a kernel panic before userspace, or the busybox /init did not run). \
             Last serial output:\n{}",
            result.reason,
            tail(&result.console, 60)
        ));
    }
    println!(
        "PASS: linux-x86-64 boots under qemu (TCG) — the td-source-built kernel reaches userspace and \
         runs the static busybox userland ({MARKER} on ttyS0)"
    );
    Ok(())
}

/// Locate host `qemu-system-x86_64` (a control-plane test tool; see module doc).
/// Search PATH first, then the standard host locations. Fail loudly if absent so
/// the daily runner is known to require it rather than green-washing the boot.
fn find_qemu() -> Result<String, String> {
    const NAME: &str = "qemu-system-x86_64";
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let cand = Path::new(dir).join(NAME);
            if is_executable(&cand) {
                return Ok(cand.to_string_lossy().into_owned());
            }
        }
    }
    for dir in [
        "/run/current-system/profile/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/bin",
    ] {
        let cand = Path::new(dir).join(NAME);
        if is_executable(&cand) {
            return Ok(cand.to_string_lossy().into_owned());
        }
    }
    Err(format!(
        "{NAME} not found on PATH or the standard host locations — the linux-x86-64 qemu boot check \
         (daily tier) requires host qemu as a control-plane test tool"
    ))
}

/// Wall-clock ceiling, overridable via `TD_QEMU_BOOT_TIMEOUT_SECS` (a positive
/// integer; anything unparsable or zero falls back to the default).
fn boot_timeout() -> Duration {
    let secs = env::var("TD_QEMU_BOOT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_BOOT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Boot bzImage + initramfs under qemu, capturing ttyS0 to a FILE (never a pipe:
/// a pipe would deadlock if the kernel log outran the buffer while we poll). The
/// console is read INCREMENTALLY into a bounded rolling buffer — decoded lossily
/// so a non-UTF-8 serial byte can't empty the capture, and trimmed to the last
/// CAP bytes so a flooding boot can't balloon memory or make the poll quadratic.
/// Kill qemu the instant the marker appears; otherwise bound it by the wall-clock
/// ceiling or the guest's own `reboot -f`.
fn boot(qemu: &str, bzimage: &Path, initramfs: &Path) -> Result<BootResult, String> {
    // Per-invocation scratch dir, unique even under concurrent boots in one
    // process (pid + a process-wide counter — pid alone collides between two
    // simultaneous boots in the same process), and removed on EVERY exit path
    // (success or an early `?`) by the Scratch drop guard. A fresh unpredictable
    // directory also means the console file never pre-exists, so there is no stale
    // marker to misread and no shared-/tmp symlink to clobber.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("td-qemu-{}-{seq}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let _scratch = Scratch { dir: dir.clone() };
    let console_path = dir.join("console.log");
    let diag_path = dir.join("diag.log");
    let diag = File::create(&diag_path).map_err(|e| format!("create {}: {e}", diag_path.display()))?;
    let diag_err = diag.try_clone().map_err(|e| format!("clone diag fd: {e}"))?;

    // -M pc + TCG: no KVM needed (the sandbox denies /dev/kvm and the host may not
    //   expose it either; TCG always works and a tiny kernel boots fast).
    // -serial file:<console>: route ttyS0 straight to a file — deterministic, no
    //   tty/stdio games (unlike -nographic, which wants a terminal on stdin).
    // -display none / -monitor none: fully headless.
    // -nic none: the guest needs no network; qemu's default is a user-mode NIC, so
    //   disable it to keep the boot offline and free of inherited host net state.
    // -no-user-config: ignore the host's qemu config files for a hermetic run.
    // -no-reboot: the busybox /init issues `reboot -f`; qemu exits on the guest
    //   reset instead of looping, so a healthy boot terminates on its own.
    // console=ttyS0: kernel printk + the /init echo land on the 8250 UART.
    // panic=-1: on a kernel panic, reboot immediately (=> qemu exits) rather than
    //   wedge, so a failed boot reds promptly instead of riding out the ceiling.
    let serial = format!("file:{}", console_path.display());
    let append = "console=ttyS0 panic=-1 rdinit=/init";
    let mut child = Command::new(qemu)
        .args(["-M", "pc", "-accel", "tcg", "-m", "256", "-no-reboot"])
        .args(["-display", "none", "-monitor", "none"])
        .args(["-nic", "none", "-no-user-config"])
        .args(["-serial", &serial])
        .arg("-kernel")
        .arg(bzimage)
        .arg("-initrd")
        .arg(initramfs)
        .args(["-append", append])
        .stdin(Stdio::null())
        .stdout(Stdio::from(diag))
        .stderr(Stdio::from(diag_err))
        .spawn()
        .map_err(|e| format!("spawn {qemu}: {e}"))?;

    let timeout = boot_timeout();
    let start = Instant::now();
    let mut console_file: Option<File> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut marker = false;
    let end;
    loop {
        marker |= drain_console(&console_path, &mut console_file, &mut buf);
        if marker {
            let _ = child.kill();
            let _ = child.wait();
            end = EndReason::MarkerSeen;
            break;
        }
        match child.try_wait() {
            // qemu exited on its own (guest reboot, panic-reboot, or a failure).
            Ok(Some(status)) => {
                end = EndReason::QemuExited(status);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait on qemu: {e}"));
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            end = EndReason::TimedOut(timeout.as_secs());
            break;
        }
        thread::sleep(POLL);
    }

    // Drain any final bytes qemu flushed before it was reaped (e.g. the marker
    // line printed just before `reboot -f` made qemu exit).
    marker |= drain_console(&console_path, &mut console_file, &mut buf);

    let mut console = String::from_utf8_lossy(&buf).into_owned();
    if console.trim().is_empty() {
        // ttyS0 produced nothing — qemu likely failed before the guest ran; surface
        // its own diagnostics (bad args, missing accelerator, unreadable image),
        // bounded to the last CAP bytes.
        if let Ok(d) = read_tail(&diag_path, CAP) {
            if !d.trim().is_empty() {
                console = format!("(no ttyS0 output; qemu diagnostics)\n{d}");
            }
        }
    }

    let reason = match end {
        EndReason::MarkerSeen => "the marker was seen".to_string(),
        EndReason::QemuExited(status) => {
            format!("qemu exited on its own before the marker ({status})")
        }
        EndReason::TimedOut(secs) => {
            format!("no marker within the {secs}s ceiling; qemu was killed")
        }
    };
    Ok(BootResult {
        marker,
        reason,
        console,
    })
}

/// Read whatever new bytes are available on the console file into `buf`, opening
/// it lazily (qemu creates it after spawn). Keeps only the last CAP bytes and
/// returns whether the marker is now present. Bounded by DRAIN_BUDGET per call so
/// a flooding guest can't starve the outer deadline check.
fn drain_console(path: &Path, file: &mut Option<File>, buf: &mut Vec<u8>) -> bool {
    if file.is_none() {
        *file = File::open(path).ok();
    }
    let mut found = false;
    if let Some(f) = file.as_mut() {
        let mut chunk = [0u8; 8192];
        let mut drained = 0usize;
        while drained < DRAIN_BUDGET {
            match f.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(slice) = chunk.get(..n) {
                        buf.extend_from_slice(slice);
                        drained += n;
                        if contains(buf, MARKER.as_bytes()) {
                            found = true;
                        }
                        if buf.len() > CAP {
                            let drop = buf.len() - CAP;
                            buf.drain(..drop);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }
    found
}

/// Byte-substring search — marker detection without a UTF-8 decode, so a non-UTF-8
/// serial byte can neither hide the marker nor empty the capture.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Read at most the last `cap` bytes of `path`, decoded lossily — bounds memory
/// if qemu floods its diagnostics.
fn read_tail(path: &Path, cap: usize) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let cap64 = cap as u64;
    if len > cap64 {
        let _ = f.seek(SeekFrom::Start(len - cap64));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Removes its scratch directory on drop, so `boot` leaves no temp files on ANY
/// return path — the happy path, an early `?` (e.g. a failed `spawn`), or a
/// mid-loop error return.
struct Scratch {
    dir: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Last `n` lines, for error context.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines.get(start..).map(|s| s.join("\n")).unwrap_or_default()
}
