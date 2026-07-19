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
//! stdio) and does NOT scan, time out, or kill. The operator quits with qemu's own
//! Ctrl-A X: the auto-login user is unprivileged and busybox is not installed setuid,
//! so `su`/`poweroff` cannot elevate and there is no in-guest shutdown from the greeter
//! (`-no-reboot` still makes a guest reset exit qemu, e.g. if the image is retailored
//! with a root auto-login). Because it is interactive it is a host-side command, never
//! a gated check (a gate has no terminal, and the daily sandbox has no host qemu).
use std::fs::File;
use std::path::Path;
use std::process::Command;

use crate::check_runner::RecipeCheckRunner;
use crate::checks::qemu_boot::find_qemu;

/// The distro image recipe this runner boots; its recipe closure pulls in the
/// `linux-x86-64` kernel that supplies the bzImage.
const SYSTEM: &str = "system-x86-64";
const KERNEL: &str = "linux-x86-64";

/// `lock` is the ladder lock, acquired by the caller and held across `setup()` + the
/// build below; we RELEASE it (drop) once the images exist and before the unbounded
/// interactive boot, so other ladder builds/checks are not blocked for the whole session.
pub(crate) fn run(runner: &RecipeCheckRunner, lock: File) -> Result<(), String> {
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
                "distro build is missing {label} ({}) - the runner needs both the kernel and its rootfs",
                path.display()
            ));
        }
    }

    // The build (which mutates the ladder) is done and both images exist. Release the
    // ladder lock now, BEFORE the interactive boot: the boot only reads these two files,
    // qemu loads -kernel/-initrd into guest memory at startup, and this process stays
    // alive so the reaper never touches our scratch. Holding the lock across an unbounded
    // interactive session would block every other ladder build/check the whole time.
    drop(lock);

    println!(
        "   [run] booting the td distro under {qemu} (TCG) - interactive serial console\n         \
         kernel: {}\n         rootfs: {}\n         auto-login as the test user is enabled. To exit qemu: press Ctrl-A then X.\n         \
         (The auto-logged-in test user is unprivileged and busybox is not installed setuid,\n         \
         so `su`/`poweroff` cannot elevate; Ctrl-A X is the intended exit for this image.)\n",
        bzimage.display(),
        rootfs.display()
    );

    boot_interactive(&qemu, &bzimage, &rootfs)
}

/// Boot bzImage + initramfs under qemu with the guest's ttyS0 wired to THIS
/// process's stdio (`-nographic`), inherited so the operator drives the console
/// directly. No marker scan, no timeout, no kill — the guest owns the terminal until
/// the operator quits qemu with Ctrl-A X (the default image's auto-login user is
/// unprivileged, so it has no in-guest poweroff). `-nic none` + `-no-user-config`
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
    // The legitimate interactive exits all return 0: a guest `poweroff`/`reboot` under
    // `-no-reboot` and a `Ctrl-A X` quit both make qemu exit successfully. So a non-zero
    // status is a genuine failure (qemu could not start - bad image, missing accelerator,
    // invalid option - or the guest died abnormally), not a normal quit; surface it as an
    // error rather than swallowing it (re #541, Codex review). qemu's own diagnostics are
    // already on the inherited stderr.
    if !status.success() {
        return Err(format!(
            "qemu exited with {status} (a normal guest poweroff or Ctrl-A X quit exits 0; \
             see qemu's diagnostics on stderr above)"
        ));
    }
    Ok(())
}
