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
//! stdio) and does NOT scan, time out, or kill. The operator exits the guest by
//! typing `exit` / Ctrl-D at the greeter shell: the ttyS0 session is wrapped by
//! `/bin/tty-session`, which runs the login flow AS ROOT (init's child) and then
//! `reboot -f`s when the session ends, so under `-no-reboot` qemu exits 0. (qemu's
//! own Ctrl-A X still force-quits at any time.) Because it is interactive it is a
//! host-side command, never a gated check (a gate has no terminal, and the daily
//! sandbox has no host qemu).
use std::fs::File;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check_runner::RecipeCheckRunner;
use crate::checks::qemu_boot::find_qemu;

/// The distro image recipe this runner boots; its recipe closure pulls in the
/// `linux-x86-64` kernel that supplies the bzImage.
const SYSTEM: &str = "system-x86-64";
const KERNEL: &str = "linux-x86-64";

/// A private host-side scratch dir holding the two boot images, copied out of the
/// ladder before the lock is released. Removed on `Drop`, so every return path (Ok or
/// Err) cleans it up. See `run()` for why the copy is necessary.
struct TempImages {
    dir: PathBuf,
}

impl TempImages {
    /// An EXCLUSIVELY-owned dir with an unpredictable name under the host temp dir
    /// (outside the ladder tree, so a ladder wipe cannot touch it). Created with
    /// `DirBuilder::new().mode(0o700).create` — the same idiom as
    /// `qemu_boot.rs::create_scratch_dir` — so owner-only permissions are established
    /// ATOMICALLY by the `mkdir` syscall itself, never a create-then-chmod window. Mode
    /// `0o700` has no group/other bits, so the umask can only leave it more restrictive,
    /// never world- or group-writable; a permissive umask cannot open a plant-a-symlink
    /// window. `create` (unlike `create_dir_all`) also fails if the path already exists,
    /// so a local attacker cannot pre-plant a dir or symlink at our path and have us
    /// reuse it or copy image bytes through it (CWE-377 insecure temp). The name mixes
    /// pid + a nanosecond seed + a counter; a collision just retries, so the first
    /// success is atomically ours, empty, and owner-only. `std::time` is fine here — this
    /// is host-side runtime code, not a resume-sensitive workflow script.
    fn new() -> Result<Self, String> {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for attempt in 0..1024u32 {
            let dir = base.join(format!("td-run-{pid}-{seed}-{attempt}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(format!("create boot-image temp dir {}: {e}", dir.display()))
                }
            }
        }
        Err("could not create a private boot-image temp dir under the system temp \
             directory after 1024 attempts"
            .to_string())
    }

    /// Copy `src` to `<dir>/<name>`, returning the destination path.
    fn stage(&self, src: &Path, name: &str) -> Result<PathBuf, String> {
        let dst = self.dir.join(name);
        std::fs::copy(src, &dst)
            .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
        Ok(dst)
    }
}

impl Drop for TempImages {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `lock` is the ladder lock, acquired by the caller and held across `setup()` + the
/// build below; we RELEASE it (drop) once the images are copied out and before the
/// unbounded interactive boot, so other ladder builds/checks are not blocked for the
/// whole session.
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

    // Copy both images OUT of the ladder scratch to a private host temp dir BEFORE we
    // release the lock. Once the lock is free, a concurrent force-cold runner can acquire
    // it and wipe the entire ladder work dir (check_runner::setup -> remove_path_if_exists),
    // which would delete these images out from under a boot that has not yet loaded
    // -kernel/-initrd into guest memory. Booting from private copies closes that race
    // entirely (re #541, Codex/subagent review). The copies are removed when `images`
    // drops — on every return path below.
    let images = TempImages::new()?;
    let boot_bzimage = images.stage(&bzimage, "bzImage")?;
    let boot_rootfs = images.stage(&rootfs, "rootfs.cpio")?;

    // The build (which mutates the ladder) is done and both images are copied out.
    // Release the ladder lock now, BEFORE the unbounded interactive boot: this process
    // stays alive so the reaper never touches our scratch, and the boot reads only the
    // private copies. Holding the lock across an unbounded interactive session would
    // block every other ladder build/check the whole time.
    drop(lock);

    println!(
        "   [run] booting the td distro under {qemu} (TCG) - interactive serial console\n         \
         kernel: {}\n         rootfs: {}\n         auto-login as the test user is enabled.\n         \
         To power off: type `exit` (or Ctrl-D) at the shell - the session wrapper reboots\n         \
         as root and qemu (-no-reboot) exits. To force-quit qemu at any time: Ctrl-A then X.\n",
        bzimage.display(),
        rootfs.display()
    );

    boot_interactive(&qemu, &boot_bzimage, &boot_rootfs)
}

/// Boot bzImage + initramfs under qemu with the guest's ttyS0 wired to THIS
/// process's stdio (`-nographic`), inherited so the operator drives the console
/// directly. No marker scan, no timeout, no kill — the guest owns the terminal until
/// the operator types `exit`/Ctrl-D at the greeter (the `tty-session` wrapper then
/// `reboot -f`s as root) or force-quits with Ctrl-A X. `-nic none` + `-no-user-config`
/// keep the run offline and hermetic; `-no-reboot` makes the guest reset exit qemu.
fn boot_interactive(qemu: &str, bzimage: &Path, rootfs: &Path) -> Result<(), String> {
    // No `panic=-1` here (unlike the headless qemu_boot oracle, which uses it to
    // auto-exit on panic): an interactive operator wants a kernel panic left ON SCREEN
    // to read, then quits with Ctrl-A X — an auto-reboot would scroll it away.
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
