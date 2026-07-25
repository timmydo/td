//! Host-side interactive distro runner (re #541, #550): build the td-source-built
//! `system-x86-64` deployment bundle and boot it under HOST qemu with an
//! INTERACTIVE serial console, so an operator can
//! watch it boot, mount the read-only erofs root, `switch_root` into it, auto-log-in as
//! the test user, and use the shell. Reached only through the `td-recipe-eval run`
//! subcommand (check_runner::run_cli).
//!
//! Persistent deployment boot: the guest boots the verified selector initramfs,
//! selects and kexecs `current` from the Btrfs volume, then
//! loop-mounts the selected EROFS root read-only, mounts persistent `@var` plus
//! volatile `/run` and `/tmp`, and `switch_root`s into the real root. The recipe
//! emits hashed deployment and selector artifacts; this consumer verifies both
//! manifests before building the boot volume.
//!
//! Sibling of checks/qemu_boot.rs. Same host-free build (`build_plan` builds the kernel
//! and system images inside their own nested build jail) and the same trust model (host
//! qemu is a control-plane TEST tool that only RUNS the td-built artifact and never
//! enters a target closure). The difference is the console: qemu_boot is a headless
//! PASS/FAIL oracle that scans ttyS0 for a marker and kills qemu; this hands the guest a
//! real terminal (`-nographic` wires ttyS0 <-> the operator's stdio) and does NOT scan,
//! time out, or kill. The operator exits the guest by typing `exit` / Ctrl-D at the
//! greeter shell: the ttyS0 session is wrapped by `/etc/tty-session`, which runs the
//! login flow AS ROOT (init's child) and asks init to reboot when the session ends, so
//! under `-no-reboot` qemu exits 0. (qemu's own Ctrl-A X still force-quits at any time.)
//! Because it is interactive it is a host-side command, never a gated check (a gate has
//! no terminal, and the daily sandbox has no host qemu).
use std::fs::File;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check_runner::RecipeCheckRunner;
use crate::checks::qemu_boot::{
    build_btrfs_tools, create_persistent_volume, drive_arg, find_qemu, verify_deployment,
    verify_selector,
};

/// The distro image recipe this runner boots; its recipe closure pulls in the
/// `linux-x86-64` kernel that supplies the bzImage.
const SYSTEM: &str = "system-x86-64";
const KERNEL: &str = "linux-x86-64";

/// A private host-side scratch dir holding the boot images, copied out of the ladder
/// before the lock is released. Removed on `Drop`, so every return path (Ok or Err)
/// cleans it up. See `run()` for why the copy is necessary.
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
    fn new(ladder_work_dir: &Path) -> Result<Self, String> {
        let base = std::env::temp_dir();
        // Fail closed if the system temp dir is INSIDE the ladder work tree. The whole
        // point of staging the boot images here is to survive a concurrent `clear-store`
        // ladder wipe after the lock is released (see `run()`); if `TMPDIR` points into
        // the ladder, these "private" copies would be wiped WITH it and qemu could read
        // them out from under itself — the very race the copy-out closes. Refuse rather
        // than boot from a wipe-exposed location (re #541, Codex review). Best-effort
        // canonical comparison: if either path cannot be canonicalised we proceed (no
        // worse than before this guard existed).
        if let (Ok(cbase), Ok(clw)) = (base.canonicalize(), ladder_work_dir.canonicalize()) {
            if cbase == clw || cbase.starts_with(&clw) {
                return Err(format!(
                    "the system temp dir ({}) is inside the ladder work tree ({}); a concurrent \
                     ladder wipe could delete the staged boot images mid-boot. Set TMPDIR to a \
                     directory outside the ladder and retry.",
                    base.display(),
                    ladder_work_dir.display()
                ));
            }
        }
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
                Err(e) => return Err(format!("create boot-image temp dir {}: {e}", dir.display())),
            }
        }
        Err(
            "could not create a private boot-image temp dir under the system temp \
             directory after 1024 attempts"
                .to_string(),
        )
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
/// build below; we RELEASE it (drop) once the images are copied/built out and before the
/// unbounded interactive boot, so other ladder builds/checks are not blocked for the
/// whole session.
pub(crate) fn run(runner: &RecipeCheckRunner, lock: File) -> Result<(), String> {
    // Locate host qemu FIRST, before the (potentially multi-minute) build: if qemu
    // is absent the tool can only fail, so fail fast rather than after a full build.
    let qemu = find_qemu()?;

    // Build the distro image; its closure includes the kernel, so a single build
    // plan yields the complete deployment bundle.
    // Announce the build up front — an otherwise-silent wait — and the runner streams
    // the builder's per-rung stderr live from here on (each `td-builder: build-plan
    // step ...` line is one rung landing).
    println!(
        "   [run] building the td distro ({SYSTEM}); its closure pulls in the {KERNEL} kernel.\n         \
         An unchanged tree is reused whole (a `[reuse]` line below) and returns at once; otherwise\n         \
         only changed rungs rebuild, so a warm tree finishes in minutes. The first build (or the\n         \
         first after a `td-recipe-eval clear-store`) cold-climbs the whole ladder from stage0 and\n         \
         can take many minutes. Per-rung progress streams below.\n"
    );
    // Build (or reuse whole) the distro and stage its single deployment output
    // into tdstore; a warm memo hit skips the climb entirely.
    let trees = runner.build_and_stage(SYSTEM, &[SYSTEM])?;
    let system_tree = trees
        .first()
        .cloned()
        .ok_or_else(|| format!("distro build did not stage the {SYSTEM} output"))?;
    let deployment = system_tree.join("deployment");
    // Verify every selected payload before copying the direct-boot artifacts.
    let (bzimage, _, _) = verify_deployment(&deployment)?;
    let selector = verify_selector(&system_tree.join("boot"))?;
    let (mkfs, btrfs) = build_btrfs_tools(runner)?;

    // Stage the boot images OUT of the ladder scratch to a private host temp dir BEFORE we
    // release the lock. Once the lock is free, a concurrent `td-recipe-eval clear-store` can
    // acquire it and wipe the entire ladder work dir (check_runner::clear_ladder),
    // which would delete these images out from under a boot that has not yet loaded
    // -kernel/-initrd/-drive into guest memory. Booting from private copies closes that
    // race entirely (re #541, Codex/subagent review). Everything under `images`
    // is removed when it drops — on every return path.
    let images = TempImages::new(runner.ladder_work_dir())?;
    let boot_bzimage = images.stage(&bzimage, "bzImage")?;
    let boot_init = images.stage(&selector, "selector-initramfs.cpio")?;
    let boot_disk = images.dir.join("system.btrfs");
    create_persistent_volume(&deployment, &mkfs, &btrfs, &boot_disk)?;

    // The build is done and every verified payload is staged out.
    // Release the ladder lock now, BEFORE the unbounded interactive boot: this process
    // stays alive so the reaper never touches our scratch, and the boot reads only the
    // private copies. Holding the lock across an unbounded interactive session would
    // block every other ladder build/check the whole time.
    drop(lock);

    println!(
        "   [run] booting the td distro through its persistent selector under {qemu} (TCG) - interactive serial console\n         \
         shim kernel:   {}\n         initramfs:     {}\n         Btrfs volume:  {}\n         \
         The initramfs verifies + kexecs current, loop-mounts its EROFS root, mounts @var,\n         \
         and switch_roots into the deployment. This private test volume lasts for the\n         \
         interactive session and is discarded when qemu exits;\n         \
         auto-login as the test user is enabled.\n         \
         To power off: type `exit` (or Ctrl-D) at the shell - the session wrapper reboots\n         \
         as root and qemu (-no-reboot) exits. To force-quit qemu at any time: Ctrl-A then X.\n",
        boot_bzimage.display(),
        boot_init.display(),
        boot_disk.display()
    );

    boot_interactive(&qemu, &boot_bzimage, &boot_init, &boot_disk)
}

/// Boot the selector plus deployment under qemu with the guest's ttyS0 wired to THIS process's
/// stdio (`-nographic`), inherited so the operator drives the console directly. The
/// selector initramfs boots with a writable Btrfs volume attached as `/dev/vda`.
/// It selects and kexecs current; the deployment initramfs mounts root.erofs and @var. No
/// marker scan, no timeout, no kill — the guest owns the terminal until the operator
/// types `exit`/Ctrl-D at the greeter (the `tty-session` wrapper then asks init to reboot as
/// root) or force-quits with Ctrl-A X. `-nic none` + `-no-user-config` keep the run
/// offline and hermetic; `-no-reboot` makes the guest reset exit qemu.
fn boot_interactive(
    qemu: &str,
    bzimage: &Path,
    init_cpio: &Path,
    disk: &Path,
) -> Result<(), String> {
    // No `panic=-1` here (unlike the headless qemu_boot oracle, which uses it to
    // auto-exit on panic): an interactive operator wants a kernel panic left ON SCREEN
    // to read, then quits with Ctrl-A X — an auto-reboot would scroll it away. No autotest
    // token either, so the greeter is a normal interactive shell (it powers off on `exit`,
    // not immediately).
    let append = "console=ttyS0 rdinit=/init";
    let status = Command::new(qemu)
        .args(["-M", "pc", "-accel", "tcg", "-m", "512", "-no-reboot"])
        .args(["-no-user-config", "-nic", "none", "-nographic"])
        .arg("-kernel")
        .arg(bzimage)
        .arg("-initrd")
        .arg(init_cpio)
        .args(["-append", append])
        // The writable persistent Btrfs volume over virtio-blk (/dev/vda).
        .arg("-drive")
        .arg(drive_arg(disk, false))
        .args(["-device", "virtio-blk-pci,drive=disk0"])
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
