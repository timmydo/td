//! Host-side interactive distro runner (re #541): build the td-source-built distro
//! — the `system-x86-64` initramfs plus the `linux-x86-64` bzImage — and boot it
//! under HOST qemu with an INTERACTIVE serial console, so an operator can watch it
//! boot, auto-log-in as the test user, and use the shell. Reached only through the
//! `td-recipe-eval run` subcommand (check_runner::run_cli).
//!
//! Sibling of checks/qemu_boot.rs. Same host-free build (`build_plan` builds the
//! kernel + initramfs inside their own nested build jail) and the same trust model
//! (host qemu is a control-plane TEST tool that only RUNS the td-built artifact and
//! never enters a target closure). The difference is the console: qemu_boot is a
//! headless PASS/FAIL oracle that scans ttyS0 for a marker and kills qemu; this
//! hands the guest a real terminal (`-nographic` wires ttyS0 <-> the operator's
//! stdio) and does NOT scan, time out, or kill. The operator quits by powering off
//! the guest (`-no-reboot` makes the guest `poweroff`/`reboot` exit qemu) or with
//! qemu's own Ctrl-A X. Because it is interactive it is a host-side command, never
//! a gated check (a gate has no terminal, and the daily sandbox has no host qemu).
use std::path::Path;
use std::process::Command;

use crate::check_runner::RecipeCheckRunner;
use crate::checks::qemu_boot::find_qemu;

/// The distro image recipe this runner boots; its recipe closure pulls in the
/// `linux-x86-64` kernel that supplies the bzImage.
const SYSTEM: &str = "system-x86-64";
const KERNEL: &str = "linux-x86-64";

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    // Locate host qemu FIRST, before the (potentially multi-minute) build: if qemu
    // is absent the tool can only fail, so fail fast rather than after a full build.
    let qemu = find_qemu()?;

    // Build the distro image; its closure includes the kernel, so a single build
    // plan yields both the bzImage and the initramfs.
    runner.prepare_recipe_target(SYSTEM)?;
    let build_out = runner.build_plan(SYSTEM)?;
    let system_tree = runner.ladder_out_from(&build_out, SYSTEM)?;
    let kernel_tree = runner.ladder_out_from(&build_out, KERNEL)?;
    let bzimage = kernel_tree.join("bzImage");
    let rootfs = system_tree.join("rootfs.cpio");
    for (label, path) in [("bzImage", &bzimage), ("rootfs.cpio", &rootfs)] {
        if !path.is_file() {
            return Err(format!(
                "distro build is missing {label} ({}) — the runner needs both the kernel and its rootfs",
                path.display()
            ));
        }
    }

    println!(
        "   [run] booting the td distro under {qemu} (TCG) — interactive serial console\n         \
         kernel: {}\n         rootfs: {}\n         (auto-login is enabled; type `poweroff` to exit, or Ctrl-A then X to quit qemu)\n",
        bzimage.display(),
        rootfs.display()
    );

    boot_interactive(&qemu, &bzimage, &rootfs)
}

/// Boot bzImage + initramfs under qemu with the guest's ttyS0 wired to THIS
/// process's stdio (`-nographic`), inherited so the operator drives the console
/// directly. No marker scan, no timeout, no kill — the guest owns the terminal
/// until it powers off or the operator quits qemu. `-nic none` + `-no-user-config`
/// keep the run offline and hermetic; `-no-reboot` makes a guest reset exit qemu.
fn boot_interactive(qemu: &str, bzimage: &Path, rootfs: &Path) -> Result<(), String> {
    let append = "console=ttyS0 rdinit=/init";
    let status = Command::new(qemu)
        .args(["-M", "pc", "-accel", "tcg", "-m", "256", "-no-reboot"])
        .args(["-no-user-config", "-nic", "none", "-nographic"])
        .arg("-kernel")
        .arg(bzimage)
        .arg("-initrd")
        .arg(rootfs)
        .args(["-append", append])
        .status()
        .map_err(|e| format!("spawn {qemu}: {e}"))?;
    // An interactive session ends many legitimate ways (guest poweroff, Ctrl-A X),
    // so a non-zero status is not by itself a tool failure — qemu prints its own
    // diagnostics to the inherited stderr. Only note it, so a genuine early failure
    // (bad image, missing accelerator) is visible without masking a normal quit.
    if !status.success() {
        println!("   [run] qemu exited with {status}");
    }
    Ok(())
}
