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
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::check_runner::{is_executable, RecipeCheckRunner};

/// The busybox /init prints this exact line on ttyS0 once the kernel has reached
/// userspace and executed the static busybox userland. Sourced from the SHARED
/// `ladder::USERLAND_MARKER` const so the /init script (linux-x86-64.rs), the cpio
/// shape check (ladder.rs), and this boot oracle can never disagree on the string.
const MARKER: &str = td_recipe::ladder::USERLAND_MARKER;

/// The line the guest `/init` prints once it has mounted the attached virtio-blk
/// disk as READ-ONLY erofs and read the sentinel back (re #549). The sole success
/// criterion of the `qemu-boot-erofs` mode — seeing it proves the source-built
/// kernel's EROFS_FS + VIRTIO_BLK config can mount a td-written erofs image
/// read-only. Shared with the /init script and the shape check via `td_recipe::ladder`.
const EROFS_MARKER: &str = td_recipe::ladder::EROFS_MARKER;

/// The sentinel file the probe erofs image carries and the guest `/init` reads back;
/// shared so the image the oracle builds and the path the /init reads never desync.
const EROFS_PROBE_SENTINEL: &str = td_recipe::ladder::EROFS_PROBE_SENTINEL;

/// The exact token the probe sentinel holds. The guest `/init` reads the file back
/// with `cat` and string-COMPARES it (not merely `test -f`), so a green result
/// proves the erofs DATA block is readable, not just that the inode exists. Shared so
/// the bytes the oracle writes and the token the /init expects can never desync.
const EROFS_PROBE_CONTENT: &str = td_recipe::ladder::EROFS_PROBE_CONTENT;

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

/// Disk ceiling on the COMBINED on-disk capture — `console.log` (ttyS0 via
/// `-serial file:`) plus `diag.log` (qemu's own stdout/stderr). The in-memory
/// capture is trimmed to CAP, but both files keep appending on disk, so a guest that
/// floods ttyS0 OR a qemu that floods stderr could fill the scratch filesystem. When
/// their sum crosses this ceiling the boot is aborted (qemu killed) and reported as
/// flooded — generous enough that a normal boot's few KiB of printk never trips it.
const MAX_CONSOLE_BYTES: u64 = 64 * 1024 * 1024;

/// How the boot loop terminated. Success is decided SOLELY by whether the marker
/// reached the console; this only labels a FAILED boot's diagnostics.
enum EndReason {
    MarkerSeen,
    QemuExited(ExitStatus),
    TimedOut(u64),
    Flooded(u64),
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
    // Locate host qemu FIRST, before the (potentially multi-minute) kernel build:
    // if qemu is absent the tool can only fail, so fail fast rather than after a
    // full source build. qemu is a control-plane test tool, never a target input.
    let qemu = find_qemu()?;

    // Build the kernel producer (its own stem, as RustToolchain builds
    // rust-toolchain) to get the bzImage + initramfs.cpio, then boot them.
    let (bzimage, initramfs) = build_kernel(runner)?;

    println!(
        "   [qemu-boot] {qemu} boots the td-source-built bzImage under TCG with the busybox initramfs\n              kernel:    {}\n              initramfs: {}",
        bzimage.display(),
        initramfs.display()
    );

    let result = boot(&qemu, &bzimage, &initramfs, None, MARKER, runner.scratch_dir())?;
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

/// `qemu-boot-erofs` (re #549): the same host-side boot, but with a READ-ONLY erofs
/// disk attached over virtio-blk. Proves the source-built kernel's EROFS_FS +
/// VIRTIO_BLK + VIRTIO_PCI config can mount a td-written erofs image read-only — the
/// filesystem the two-stage boot (#550) pivots into. The probe image is built by the
/// in-house `td-builder mkfs-erofs` writer (#548) from a one-file rootfs; the guest
/// `/init` mounts `/dev/vda` read-only and prints `EROFS_MARKER` only after it also
/// reads the sentinel back, so a green result is a true read-only-mount proof.
pub(crate) fn run_erofs(runner: &RecipeCheckRunner) -> Result<(), String> {
    // qemu first (fail fast if absent), then the kernel, then the probe image.
    let qemu = find_qemu()?;
    let (bzimage, initramfs) = build_kernel(runner)?;
    let disk = build_probe_image(runner)?;

    println!(
        "   [qemu-boot-erofs] {qemu} boots the td-source-built bzImage under TCG with a read-only erofs virtio-blk disk\n              kernel:    {}\n              initramfs: {}\n              erofs img: {}",
        bzimage.display(),
        initramfs.display(),
        disk.display()
    );

    let result = boot(
        &qemu,
        &bzimage,
        &initramfs,
        Some(&disk),
        EROFS_MARKER,
        runner.scratch_dir(),
    )?;
    if !result.marker {
        return Err(format!(
            "kernel did not reach the read-only-erofs marker {EROFS_MARKER:?} on ttyS0 — {} \
             (the initramfs could not mount /dev/vda as read-only erofs, the virtio-blk node did not \
             appear, or the sentinel {EROFS_PROBE_SENTINEL:?} was unreadable). Last serial output:\n{}",
            result.reason,
            tail(&result.console, 60)
        ));
    }
    println!(
        "PASS: linux-x86-64 mounts a td-written erofs image READ-ONLY over virtio-blk under qemu (TCG) — \
         the source-built EROFS_FS + VIRTIO_BLK kernel reads the store-shaped root back ({EROFS_MARKER} on ttyS0)"
    );
    Ok(())
}

/// Build the `linux-x86-64` producer and return its `(bzImage, initramfs.cpio)` —
/// shared by both boot modes so they build the kernel identically.
fn build_kernel(runner: &RecipeCheckRunner) -> Result<(PathBuf, PathBuf), String> {
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
    Ok((bzimage, initramfs))
}

/// Build a tiny probe erofs image with the control-plane `td-builder mkfs-erofs`
/// writer (#548): a one-file rootfs holding the sentinel the guest `/init` reads
/// back. Returns the image path (raw erofs bytes) to attach as a virtio-blk disk.
/// The rootfs and image live in the runner's per-invocation scratch and are rebuilt
/// fresh (any stale copy removed first) so a prior run's bytes can never be reused.
fn build_probe_image(runner: &RecipeCheckRunner) -> Result<PathBuf, String> {
    let scratch = runner.scratch_dir();
    let root = scratch.join("erofs-probe-root");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    let sentinel = root.join(EROFS_PROBE_SENTINEL);
    // Write the shared content token (trailing newline is fine — the guest reads it
    // via `$(cat …)`, which strips it before the string compare). Reading this exact
    // token back in the guest is what makes EROFS_MARKER a DATA-read proof, not just
    // an inode-exists check.
    fs::write(&sentinel, format!("{EROFS_PROBE_CONTENT}\n"))
        .map_err(|e| format!("write {}: {e}", sentinel.display()))?;

    let img = scratch.join("erofs-probe.img");
    let _ = fs::remove_file(&img);
    // `td-builder mkfs-erofs ROOTFS-DIR OUT.img` — a control-plane capability (never
    // on a recipe PATH). builder_command() carries the runner's builder provenance.
    let status = runner
        .builder_command()
        .arg("mkfs-erofs")
        .arg(&root)
        .arg(&img)
        .status()
        .map_err(|e| format!("spawn td-builder mkfs-erofs: {e}"))?;
    if !status.success() {
        return Err(format!(
            "td-builder mkfs-erofs failed ({status}) building the probe erofs image from {}",
            root.display()
        ));
    }
    if !img.is_file() {
        return Err(format!(
            "td-builder mkfs-erofs reported success but did not produce {}",
            img.display()
        ));
    }
    Ok(img)
}

/// Locate host `qemu-system-x86_64` (a control-plane test tool; see module doc).
/// Search PATH first, then the standard host locations. Fail loudly if absent so
/// the tool is known to require it rather than green-washing the boot.
pub(crate) fn find_qemu() -> Result<String, String> {
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
        "{NAME} not found on PATH or the standard host locations — the linux-x86-64 qemu boot \
         tool requires host qemu as a control-plane test tool (run outside the sandbox)"
    ))
}

/// Wall-clock ceiling, overridable via `TD_QEMU_BOOT_TIMEOUT_SECS` (a positive
/// integer; anything unparsable or zero falls back to the default).
fn boot_timeout() -> Duration {
    parse_timeout(env::var("TD_QEMU_BOOT_TIMEOUT_SECS").ok())
}

/// Pure parser behind `boot_timeout` (unit-tested without mutating process env):
/// a positive integer wins; anything unparsable, zero, or absent → the default.
fn parse_timeout(raw: Option<String>) -> Duration {
    let secs = raw
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
/// Kill qemu the instant `target_marker` appears; otherwise bound it by the
/// wall-clock ceiling or the guest's own `reboot -f`. `disk`, when set, attaches an
/// erofs image as a read-only virtio-blk disk (/dev/vda in the guest); `None` boots
/// diskless. `target_marker` lets the two boot modes key on different lines (the
/// userland marker vs. the read-only-erofs marker printed only after a good mount).
fn boot(
    qemu: &str,
    bzimage: &Path,
    initramfs: &Path,
    disk: Option<&Path>,
    target_marker: &str,
    scratch_base: &Path,
) -> Result<BootResult, String> {
    // Per-invocation console/diag dir created EXCLUSIVELY (mkdir, not mkdir -p)
    // with 0700 under the runner's private scratch base — NOT world-writable
    // `/tmp`. Exclusive creation means this process is the sole creator (a stale or
    // attacker-planted dir at the same path fails the create and is rejected, so
    // the console file can never pre-exist with a stale marker); the private base
    // means no cross-user symlink can target our path in the first place. The dir
    // is removed on EVERY exit path (success or an early `?`) by the Scratch drop
    // guard, which is safe to remove precisely because we exclusively created it.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = create_scratch_dir(scratch_base, &SEQ)?;
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
    // The path is passed VERBATIM. Comma-doubling is WRONG here: `-serial file:PATH`
    // is qemu's legacy compat form (qemu_chr_parse_compat), which takes everything
    // after `file:` as the path directly (`qemu_opt_set(opts, "path", p)`) with NO
    // comma processing — commas are literal. Comma-splitting applies only to the
    // QemuOpts/`-chardev file,path=…` form. So doubling a comma would make qemu open
    // a different (doubled-comma) path than drain_console watches; verbatim opens the
    // exact path, correct even if the base dir contains a comma. (Do not "escape"
    // this.)
    let serial = format!("file:{}", console_path.display());
    let append = "console=ttyS0 panic=-1 rdinit=/init";
    let mut cmd = Command::new(qemu);
    cmd.args(["-M", "pc", "-accel", "tcg", "-m", "256", "-no-reboot"])
        .args(["-display", "none", "-monitor", "none"])
        .args(["-nic", "none", "-no-user-config"])
        .args(["-serial", &serial])
        .arg("-kernel")
        .arg(bzimage)
        .arg("-initrd")
        .arg(initramfs)
        .args(["-append", append]);
    // Optional read-only erofs disk (re #549): if=none defines the backing store and
    // a separate virtio-blk-pci -device attaches it as /dev/vda in the guest.
    // readonly=on matches the immutable erofs root and lets qemu refuse any write.
    // drive_arg comma-doubles the image path so a scratch dir with a literal comma in
    // its path can't be misparsed as an extra -drive key=value pair.
    if let Some(disk) = disk {
        cmd.arg("-drive").arg(drive_arg(disk));
        cmd.args(["-device", "virtio-blk-pci,drive=erofs0"]);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(diag))
        .stderr(Stdio::from(diag_err))
        .spawn()
        .map_err(|e| format!("spawn {qemu}: {e}"))?;

    let timeout = boot_timeout();
    let start = Instant::now();
    let marker_bytes = target_marker.as_bytes();
    let mut console_file: Option<File> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut marker = false;
    let mut end;
    loop {
        marker |= drain_console(&console_path, &mut console_file, &mut buf, marker_bytes);
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
        // Abort a guest that floods without panicking: the in-memory capture is
        // trimmed to CAP, but BOTH on-disk sinks keep growing — `-serial file:`
        // appends ttyS0 to console.log, and qemu's own stdout/stderr append to
        // diag.log. Bound their COMBINED size so neither path can fill the scratch
        // fs (a chatty-but-not-panicking guest floods ttyS0; a misconfigured qemu
        // floods stderr).
        let on_disk = fs::metadata(&console_path).map(|m| m.len()).unwrap_or(0)
            + fs::metadata(&diag_path).map(|m| m.len()).unwrap_or(0);
        if on_disk > MAX_CONSOLE_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            end = EndReason::Flooded(on_disk);
            break;
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
    // line printed just before `reboot -f` made qemu exit). If the marker only
    // shows up in this final flush, realign `end` so it agrees with `marker`.
    if drain_console(&console_path, &mut console_file, &mut buf, marker_bytes) {
        marker = true;
        end = EndReason::MarkerSeen;
    }

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
        EndReason::Flooded(bytes) => format!(
            "console+diagnostic output flooded past the {MAX_CONSOLE_BYTES}-byte on-disk ceiling \
             ({bytes} bytes across console.log + diag.log) without reaching the marker; qemu was killed"
        ),
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
fn drain_console(path: &Path, file: &mut Option<File>, buf: &mut Vec<u8>, marker: &[u8]) -> bool {
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
                        if buf.len() > CAP {
                            let drop = buf.len() - CAP;
                            buf.drain(..drop);
                        }
                        if contains(buf, marker) {
                            // Latch and stop: the caller kills qemu the moment this
                            // returns true, so draining further bytes only wastes work
                            // (and on a flooding-then-marker boot, could spin to
                            // DRAIN_BUDGET before the outer loop reacts).
                            found = true;
                            break;
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

/// The `-drive` value attaching `disk` as a read-only raw backing store with the
/// `erofs0` id the `virtio-blk-pci` -device references. `readonly=on` matches the
/// immutable erofs root (qemu refuses guest writes); `if=none` keeps qemu from
/// auto-attaching it to an implicit controller so only the explicit -device wires it.
///
/// qemu splits `-drive`'s key=value pairs on commas, so a literal comma in the image
/// PATH (e.g. a repo cloned under `/home/user/code,td/…`) must be doubled (`,,`) or
/// qemu would misparse the path tail as a spurious extra key. Only the path's commas
/// are doubled — the option-separator commas in the fixed prefix stay single. Built
/// byte-wise off the raw path bytes and returned as an `OsString` so a non-UTF-8 path
/// survives without a lossy round-trip.
fn drive_arg(disk: &Path) -> OsString {
    let mut out = OsString::from("if=none,format=raw,readonly=on,id=erofs0,file=");
    let path_bytes = disk.as_os_str().as_bytes();
    let mut escaped: Vec<u8> = Vec::with_capacity(path_bytes.len());
    for &b in path_bytes {
        if b == b',' {
            escaped.push(b',');
        }
        escaped.push(b);
    }
    out.push(OsString::from_vec(escaped));
    out
}

/// Read at most the last `cap` bytes of `path`, decoded lossily — bounds memory
/// if qemu floods its diagnostics. A failed seek is propagated (not swallowed), and
/// the read is itself capped at `cap`, so this can never fall through to an
/// unbounded whole-file read.
fn read_tail(path: &Path, cap: usize) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let cap64 = cap as u64;
    if len > cap64 {
        f.seek(SeekFrom::Start(len - cap64))
            .map_err(|e| format!("seek {}: {e}", path.display()))?;
    }
    let mut bytes = Vec::new();
    // Cap the read at `cap` bytes even if the file grew since the stat, so a seek
    // that succeeded but landed short of EOF still can't read unboundedly.
    Read::take(&mut f, cap64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Create a fresh per-boot directory under `base` with 0700 permissions using
/// EXCLUSIVE creation (`mkdir`, which fails if the path already exists) so this
/// process is provably the sole creator. `AlreadyExists` — a leftover from a
/// crashed run, or a racing concurrent boot in the same process — is rejected and
/// retried under a fresh sequence number; any other error is fatal. `base` is the
/// runner's private scratch, already created by `setup()`.
fn create_scratch_dir(base: &Path, seq: &AtomicU64) -> Result<PathBuf, String> {
    for _ in 0..64 {
        let n = seq.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("qemu-boot-{n}"));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create {}: {e}", dir.display())),
        }
    }
    Err(format!(
        "could not create a fresh qemu-boot scratch dir under {} after 64 attempts",
        base.display()
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_matches_substrings_and_boundaries() {
        assert!(contains(b"boot log: TD-USERLAND-OK done", MARKER.as_bytes()));
        assert!(contains(b"abc", b"a")); // at the very start
        assert!(contains(b"abc", b"c")); // at the very end
        assert!(contains(b"abc", b"abc")); // full length
        assert!(!contains(b"abc", b"d")); // absent
        assert!(!contains(b"ab", b"abc")); // needle longer than haystack
        assert!(!contains(b"anything", b"")); // empty needle never matches
    }

    #[test]
    fn contains_finds_marker_split_across_chunks() {
        // Mirrors drain_console appending in chunks: the marker only becomes
        // present once BOTH halves are in the rolling buffer.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"...TD-USER");
        assert!(!contains(&buf, MARKER.as_bytes()));
        buf.extend_from_slice(b"LAND-OK...");
        assert!(contains(&buf, MARKER.as_bytes()));
    }

    #[test]
    fn drive_arg_is_readonly_raw_with_the_device_id() {
        let arg = drive_arg(Path::new("/scratch/erofs-probe.img"));
        let s = arg.to_string_lossy();
        // The virtio-blk-pci -device references drive=erofs0, so the drive's id must match.
        assert!(s.contains("id=erofs0"), "missing id: {s}");
        // Read-only + raw + the exact backing file, and if=none (no implicit controller).
        assert!(s.contains("readonly=on"), "not read-only: {s}");
        assert!(s.contains("format=raw"), "not raw: {s}");
        assert!(s.contains("if=none"), "not if=none: {s}");
        // A comma-free path is passed through verbatim after file=.
        assert!(s.ends_with("file=/scratch/erofs-probe.img"), "wrong file: {s}");
    }

    #[test]
    fn drive_arg_doubles_only_the_image_path_commas() {
        // A repo/scratch path containing a literal comma must not break -drive's
        // key=value parse: qemu wants such commas doubled. Only the PATH's commas are
        // doubled; the fixed option-separator commas in the prefix stay single.
        let arg = drive_arg(Path::new("/sc,ratch/erofs,probe.img"));
        let s = arg.to_string_lossy();
        assert_eq!(
            s,
            "if=none,format=raw,readonly=on,id=erofs0,file=/sc,,ratch/erofs,,probe.img",
            "path commas not doubled (or prefix separators mangled): {s}"
        );
    }

    #[test]
    fn erofs_marker_is_distinct_from_the_userland_marker() {
        // Both boot modes must key on different lines: the plain check kills qemu on
        // MARKER (printed first), the erofs check waits for EROFS_MARKER (printed only
        // after a successful read-only mount). Identical strings would let a diskless
        // boot satisfy the erofs check.
        assert!(!MARKER.is_empty() && !EROFS_MARKER.is_empty());
        assert_ne!(MARKER, EROFS_MARKER);
        // drain_console keys on the passed marker, so EROFS_MARKER must be matchable
        // the same substring way the userland marker is.
        assert!(contains(
            b"...booted... TD-EROFS-RO-OK ...done",
            EROFS_MARKER.as_bytes()
        ));
        assert!(!contains(b"only TD-USERLAND-OK here", EROFS_MARKER.as_bytes()));
    }

    #[test]
    fn tail_keeps_last_n_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a\nb", 5), "a\nb"); // fewer lines than requested
        assert_eq!(tail("solo", 1), "solo");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn parse_timeout_prefers_valid_positive_else_default() {
        let dflt = Duration::from_secs(DEFAULT_BOOT_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some(" 42 ".into())), Duration::from_secs(42));
        assert_eq!(parse_timeout(Some("0".into())), dflt); // zero → default
        assert_eq!(parse_timeout(Some("nope".into())), dflt); // unparsable → default
        assert_eq!(parse_timeout(Some("".into())), dflt); // empty → default
        assert_eq!(parse_timeout(None), dflt); // unset → default
    }

    #[test]
    fn read_tail_bounds_to_last_cap_bytes() {
        // Isolate the test file in its own exclusively-created scratch dir.
        let seq = AtomicU64::new(0);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _g = Scratch { dir: dir.clone() };
        let path = dir.join("diag.log");
        fs::write(&path, b"0123456789").unwrap();
        assert_eq!(read_tail(&path, 4).unwrap(), "6789"); // only the last cap bytes
        assert_eq!(read_tail(&path, 100).unwrap(), "0123456789"); // cap >= len → whole file
    }

    #[test]
    fn create_scratch_dir_is_exclusive_and_fresh() {
        let seq = AtomicU64::new(0);
        let base = {
            let s = AtomicU64::new(1000);
            create_scratch_dir(&env::temp_dir(), &s).unwrap()
        };
        let _g = Scratch { dir: base.clone() };
        let a = create_scratch_dir(&base, &seq).unwrap();
        let b = create_scratch_dir(&base, &seq).unwrap();
        assert_ne!(a, b); // distinct dirs from the shared counter
        assert!(a.is_dir() && b.is_dir());
    }
}
