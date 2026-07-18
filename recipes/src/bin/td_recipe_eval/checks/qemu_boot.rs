//! QemuBoot check runner (re #529): boot the td-source-built linux-x86-64 kernel
//! under HOST qemu and prove it reaches a real userland.
//!
//! Trust model — host qemu is a control-plane TEST tool, not a target input.
//! Every byte of the ARTIFACT under test is td-built and host-free: the bzImage
//! is compiled by td's native GCC/binutils/glibc ladder, and the initramfs is a
//! statically-linked td-built busybox plus a shell /init. `qemu-system-x86_64`
//! only supplies the virtual machine that RUNS that artifact — exactly as the
//! host Rust toolchain is a control-plane SEED that compiles td's control-plane
//! programs yet never enters a target closure. qemu is invoked OUTSIDE the
//! host-free sandbox (parity with how the RustToolchain check runs the built
//! rustc), never from a recipe's PATH or argv, and it contributes nothing to any
//! /td/store output. If host qemu is absent the check FAILS loudly rather than
//! silently passing — the daily runner is required to provide it.
use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::check_runner::{is_executable, RecipeCheckRunner};

/// The busybox /init prints this exact line on ttyS0 once the kernel has reached
/// userspace and executed the static busybox userland. It must match the /init
/// script's `echo` in recipes/src/recipes/linux-x86-64.rs.
const MARKER: &str = "TD-USERLAND-OK";

/// Wall-clock ceiling. A tiny allnoconfig kernel boots to userspace under TCG in
/// a few seconds, but TCG on a loaded builder can be slow; 180s is generous. The
/// poll loop kills qemu the instant the marker appears, so a healthy boot returns
/// in seconds — the ceiling only bounds a FAILED boot (panic without reboot, a
/// wedged userland) so the check reds instead of hanging forever.
const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(200);

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

    let console = boot(&qemu, &bzimage, &initramfs)?;
    if !console.lines().any(|line| line.contains(MARKER)) {
        return Err(format!(
            "kernel booted but the userland marker {MARKER:?} never reached the serial console \
             (no console output, a kernel panic before userspace, or the busybox /init did not run). \
             Last serial output:\n{}",
            tail(&console, 60)
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

/// Boot bzImage + initramfs under qemu, capturing ttyS0 to a FILE (never a pipe:
/// a pipe would deadlock if the kernel log outran the buffer while we poll). Kill
/// qemu the instant the marker appears; otherwise bound it by a wall-clock
/// ceiling or the guest's own `reboot -f`. Returns the captured console (or, if
/// the console is empty, qemu's own diagnostics so a spawn/args failure surfaces).
fn boot(qemu: &str, bzimage: &Path, initramfs: &Path) -> Result<String, String> {
    let pid = std::process::id();
    let console_path = env::temp_dir().join(format!("td-qemu-console-{pid}.log"));
    let diag_path = env::temp_dir().join(format!("td-qemu-diag-{pid}.log"));
    let diag = File::create(&diag_path).map_err(|e| format!("create {}: {e}", diag_path.display()))?;
    let diag_err = diag.try_clone().map_err(|e| format!("clone diag fd: {e}"))?;

    // -M pc + TCG: no KVM needed (the sandbox denies /dev/kvm and the host may not
    //   expose it either; TCG always works and a tiny kernel boots fast).
    // -serial file:<console>: route ttyS0 straight to a file — deterministic, no
    //   tty/stdio games (unlike -nographic, which wants a terminal on stdin).
    // -display none / -monitor none: fully headless.
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

    let start = Instant::now();
    let mut console = String::new();
    loop {
        if let Ok(text) = fs::read_to_string(&console_path) {
            console = text;
            if console.lines().any(|line| line.contains(MARKER)) {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
        match child.try_wait() {
            // qemu exited on its own (guest reboot, panic-reboot, or a failure).
            Ok(Some(_)) => {
                console = fs::read_to_string(&console_path).unwrap_or(console);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait on qemu: {e}"));
            }
        }
        if start.elapsed() >= BOOT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            console = fs::read_to_string(&console_path).unwrap_or(console);
            break;
        }
        thread::sleep(POLL);
    }

    // If ttyS0 produced nothing, qemu likely failed before the guest ran; surface
    // its own diagnostics (bad args, missing accelerator, unreadable image).
    if console.trim().is_empty() {
        if let Ok(d) = fs::read_to_string(&diag_path) {
            if !d.trim().is_empty() {
                console = format!("(no ttyS0 output; qemu diagnostics)\n{d}");
            }
        }
    }
    let _ = fs::remove_file(&console_path);
    let _ = fs::remove_file(&diag_path);
    Ok(console)
}

/// Last `n` lines, for error context.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines.get(start..).map(|s| s.join("\n")).unwrap_or_default()
}
