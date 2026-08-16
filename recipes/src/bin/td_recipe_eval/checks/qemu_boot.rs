//! Host-side qemu boot validation (re #529): boot the td-source-built
//! linux-x86-64 kernel under HOST qemu and prove it reaches a real userland.
//! Reached only through the `td-recipe-eval qemu-boot` subcommand
//! (check_runner::qemu_boot_cli), NOT a gated recipe check.
//!
//! Why host-side, not a sandboxed gate check — a qemu boot needs a real
//! `qemu-system-x86_64`, and td has no such artifact. The gate wraps every
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
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::check_runner::{is_executable, RecipeCheckRunner};

use td_recipe::td_boot_protocol;

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

/// The greeter line the real-root login shell prints via `/etc/profile` once the
/// two-stage boot (#550) has switch_root'ed into the erofs root and reached an
/// interactive auto-login shell — the primary success criterion of `qemu-boot-system`.
/// Shared with the recipe (system-x86-64.rs) via `td_recipe::ladder` so the printed
/// line and the oracle key can never desync.
const GREETER_MARKER: &str = td_recipe::ladder::GREETER_MARKER;

/// Printed after unprivileged uutils behavior probes pass; shape checks cover only static closure.
const UUTILS_RUNTIME_MARKER: &str = td_recipe::ladder::UUTILS_RUNTIME_MARKER;

/// Printed after unprivileged ripgrep and fd searches return exact expected results.
const RIPGREP_FD_RUNTIME_MARKER: &str = td_recipe::ladder::RIPGREP_FD_RUNTIME_MARKER;

/// Printed by the root-owned health target after an unprivileged SSH loopback self-test.
const SSHD_MARKER: &str = td_recipe::ladder::SSHD_MARKER;

/// Printed by the root-owned health target after every td-util farm name runs unprivileged.
const TD_UTIL_RUNTIME_MARKER: &str = td_recipe::ladder::TD_UTIL_RUNTIME_MARKER;
/// Printed by the root-owned health target once `/bin/grep` and `/bin/sed` — td-txt — gave
/// the RIGHT answers over the live `/proc` and `/etc`, not merely exited 0.
const TD_TXT_RUNTIME_MARKER: &str = td_recipe::ladder::TD_TXT_RUNTIME_MARKER;
/// Printed by the greeter once every `/bin` name the static td-init boot-glue multicall
/// serves has been exercised — the reversible ones by running, the irreversible ones
/// (`reboot`/`poweroff`/`halt`/`switch_root`) by refusing a bad argument with a diagnostic.
const TD_INIT_RUNTIME_MARKER: &str = td_recipe::ladder::TD_INIT_RUNTIME_MARKER;
/// Printed by the root-owned health target after `/bin/su` — td-login — switched to the
/// unprivileged login user AND the switched process read its own credentials back out of
/// `/proc/self/status` and they matched exactly. This is the only marker that asserts the
/// RESULT of a credential change rather than that something ran.
const TD_LOGIN_RUNTIME_MARKER: &str = td_recipe::ladder::TD_LOGIN_RUNTIME_MARKER;
/// Printed after the unprivileged software compositor paints and listens.
const TD_WAYLAND_RUNTIME_MARKER: &str = td_recipe::ladder::TD_WAYLAND_RUNTIME_MARKER;

/// Printed by the compositor for a device that answered `EVIOCGABS` with a span
/// on both axes. Attached headless the tablet delivers no motion, so what this
/// proves is enumeration and the ANSWER — which is the half no unit test can
/// reach, the gate machine having no absolute device.
const TD_POINTER_ABSOLUTE_MARKER: &str = td_recipe::ladder::TD_POINTER_ABSOLUTE_MARKER;
/// Printed by the FIRST client the machine now starts: the terminal, after a frame
/// at a compositor-chosen size and a PTY the kernel agrees is that grid.
const TD_TERM_RUNTIME_MARKER: &str = td_recipe::ladder::TD_TERM_RUNTIME_MARKER;

/// The line `/etc/rootcheck` prints once it has confirmed `/` is a READ-ONLY erofs
/// mount (re #550). `qemu-boot-system` asserts it to prove the switched-into root is
/// the immutable erofs image, not a writable copy.
const SYSTEM_ROOT_RO_MARKER: &str = td_recipe::ladder::SYSTEM_ROOT_RO_MARKER;

/// Printed after root fails to create a probe below deployment-owned `/etc`.
const SYSTEM_ETC_RO_MARKER: &str = td_recipe::ladder::SYSTEM_ETC_RO_MARKER;

/// Printed after `/var` is proven to be writable Btrfs, `/run` and `/tmp` are
/// writable tmpfs mounts, and the immutable `/home` and `/root` links resolve
/// into writable `/var` state.
const SYSTEM_STATE_WRITABLE_MARKER: &str = td_recipe::ladder::SYSTEM_STATE_WRITABLE_MARKER;
const SYSTEM_STATE_OWNER_MARKER: &str = td_recipe::ladder::SYSTEM_STATE_OWNER_MARKER;
const SYSTEM_ETC_MUTABLE_MARKER: &str = td_recipe::ladder::SYSTEM_ETC_MUTABLE_MARKER;
const TD_FIRSTBOOT_NEW_MARKER: &str = td_recipe::ladder::TD_FIRSTBOOT_NEW_MARKER;
const TD_FIRSTBOOT_STABLE_MARKER: &str = td_recipe::ladder::TD_FIRSTBOOT_STABLE_MARKER;
const TD_FIRSTBOOT_HOST_KEY_PREFIX: &str = td_recipe::ladder::TD_FIRSTBOOT_HOST_KEY_PREFIX;
const SYSTEM_PERSIST_WRITE_MARKER: &str = td_recipe::ladder::SYSTEM_PERSIST_WRITE_MARKER;
const SYSTEM_PERSIST_READ_MARKER: &str = td_recipe::ladder::SYSTEM_PERSIST_READ_MARKER;
const SYSTEM_BOOT_SUCCESS_MARKER: &str = td_recipe::ladder::SYSTEM_BOOT_SUCCESS_MARKER;
const SYSTEM_DEPLOY_INSTALL_MARKER: &str = td_recipe::ladder::SYSTEM_DEPLOY_INSTALL_MARKER;
const SYSTEM_SHUTDOWN_MARKER: &str = td_recipe::ladder::SYSTEM_SHUTDOWN_MARKER;
const BOOKKEEPING_UNAVAILABLE_MARKER: &str = td_boot_protocol::BOOKKEEPING_UNAVAILABLE_MARKER;

/// The kernel-cmdline token `qemu-boot-system` appends so the greeter self-exits and
/// the VM powers off — a headless "exit powers off" proof with no terminal to type
/// into. Shared with the recipe's `/etc/profile` autotest gate via `td_recipe::ladder`.
const AUTOTEST_CMDLINE_TOKEN: &str = td_recipe::ladder::AUTOTEST_CMDLINE_TOKEN;
const PERSIST_WRITE_CMDLINE_TOKEN: &str = td_recipe::ladder::PERSIST_WRITE_CMDLINE_TOKEN;
const PERSIST_READ_CMDLINE_TOKEN: &str = td_recipe::ladder::PERSIST_READ_CMDLINE_TOKEN;
const DEPLOY_INSTALL_CMDLINE_TOKEN: &str = td_recipe::ladder::DEPLOY_INSTALL_CMDLINE_TOKEN;
const BOOT_FAIL_TARGET_CMDLINE_TOKEN: &str = td_recipe::ladder::BOOT_FAIL_TARGET_CMDLINE_TOKEN;

/// The three networking markers `/etc/netup` prints under the nettest token: the link
/// came up + DHCP applied, td-netd's own DNS client resolved the test host, and a TCP
/// connection reached it. `qemu-boot-net` asserts all three. Shared with the recipe
/// (system-x86-64.rs `build_netup`) via `td_recipe::ladder` so they can never desync.
const SYSTEM_NET_UP_MARKER: &str = td_recipe::ladder::SYSTEM_NET_UP_MARKER;
const SYSTEM_NET_RESOLVE_MARKER: &str = td_recipe::ladder::SYSTEM_NET_RESOLVE_MARKER;
const SYSTEM_NET_REACH_MARKER: &str = td_recipe::ladder::SYSTEM_NET_REACH_MARKER;

/// The kernel-cmdline token `qemu-boot-net` appends so `/etc/netup` runs the
/// resolve+reach self-test (and prints the three markers above). Shared via
/// `td_recipe::ladder` with the recipe's netup gate.
const NETTEST_CMDLINE_TOKEN: &str = td_recipe::ladder::NETTEST_CMDLINE_TOKEN;

/// The OUTER /init prints this on ttyS0 before it execs td-kexec — stage-1 reached
/// userspace. `qemu-boot-kexec` asserts it as a diagnostic that the second boot came
/// from our helper. Shared with the kexec-spike-x86-64 recipe via `td_recipe::ladder`.
const KEXEC_STAGE1_MARKER: &str = td_recipe::ladder::KEXEC_STAGE1_MARKER;

/// The kexec'd INNER /init prints this on ttyS0 once the SECOND kernel reaches
/// userspace — the `qemu-boot-kexec` success criterion, unreachable without a working
/// kexec_file_load(2) + reboot(LINUX_REBOOT_CMD_KEXEC). Shared via `td_recipe::ladder`.
const KEXEC_STAGE2_MARKER: &str = td_recipe::ladder::KEXEC_STAGE2_MARKER;

/// Default wall-clock ceiling. A tiny allnoconfig kernel boots to userspace under
/// TCG in a few seconds, but the persistent system modes hash their deployment,
/// kexec, and boot a second kernel. The poll loop returns as soon as the selected
/// mode finishes, so this ceiling only bounds a failed or unusually slow boot.
/// `TD_QEMU_BOOT_TIMEOUT_SECS` overrides it.
const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 300;
const GUEST_WAIT_MARGIN_SECS: u64 = 30;
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
const PERSISTENT_VOLUME_BYTES: u64 = 1024 * 1024 * 1024;
// Reserve fixture space for Btrfs metadata and the writable @var subvolume.
const PERSISTENT_VOLUME_HEADROOM: u64 = 256 * 1024 * 1024;
// A 64 MiB console needs 17 passes including EOF; allow seven EINTR retries.
const FINAL_DRAIN_PASSES: usize = 24;

/// How the boot loop terminated. Callers combine this with latched protocol
/// evidence; floods are rejected directly before a result is returned.
enum EndReason {
    MarkerSeen,
    QemuExited(ExitStatus),
    TimedOut(u64),
    Flooded(u64),
}

/// Outcome of a boot attempt.
#[derive(Default)]
struct ConsoleEvidence {
    target: bool,
    greeter: bool,
    current_rejected: bool,
    selected_current: bool,
    selected_previous: bool,
    selected_current_id: Option<String>,
    selected_previous_id: Option<String>,
    attempt_consumed: bool,
    attempts_exhausted: bool,
    bookkeeping_unavailable: bool,
    root_read_only: bool,
    etc_read_only: bool,
    etc_mutable: bool,
    firstboot_new: bool,
    firstboot_stable: bool,
    /// This machine's SSH host-key fingerprint, as td-firstboot printed it. An
    /// Option rather than a bool because its VALUE is the evidence: comparing it
    /// across reboots is what proves the identity persisted rather than merely
    /// that some key existed on each boot.
    host_key: Option<String>,
    state_writable: bool,
    state_owner: bool,
    uutils_runtime: bool,
    ripgrep_fd_runtime: bool,
    sshd: bool,
    td_util_runtime: bool,
    td_txt_runtime: bool,
    td_init_runtime: bool,
    td_login_runtime: bool,
    td_wayland_runtime: bool,
    td_pointer_absolute: bool,
    td_term_runtime: bool,
    persist_write: bool,
    persist_read: bool,
    boot_success: bool,
    deploy_install: bool,
    shutdown: bool,
    net_up: bool,
    net_resolve: bool,
    net_reach: bool,
    kexec_stage1: bool,
    kernel_panic: bool,
}

struct BootResult {
    /// Protocol evidence latched while reading ttyS0, before the diagnostic tail
    /// is trimmed. This keeps early boot markers authoritative under noisy printk.
    evidence: ConsoleEvidence,
    /// qemu terminated on its OWN with a success status: a clean guest-initiated
    /// power-off under `-no-reboot`. Only meaningful when the boot was allowed to run
    /// to the guest's own shutdown (`kill_on_marker = false`); the marker-killed modes
    /// reap qemu themselves and leave this false. `qemu-boot-system` asserts it — "exit
    /// powers off" means the VM terminated cleanly, not that the oracle killed it.
    exited_clean: bool,
    /// How the boot loop ended, for a FAILED boot's error message.
    reason: String,
    /// Bounded, lossily-decoded tail of ttyS0 (or qemu's own diagnostics if ttyS0
    /// was empty), for error context.
    console: String,
    /// Wall-clock time from qemu spawn through the bounded final console drain.
    elapsed: Duration,
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

    let result = boot(
        &qemu,
        &bzimage,
        &initramfs,
        BootPlan {
            disk: None,
            mem: "256",
            target_marker: MARKER,
            kill_on_marker: true,
            extra_append: "",
            user_net: false,
        },
        runner.scratch_dir(),
    )?;
    if !result.evidence.target {
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
        BootPlan {
            disk: Some(BootDisk {
                path: &disk,
                read_only: true,
            }),
            mem: "256",
            target_marker: EROFS_MARKER,
            kill_on_marker: true,
            extra_append: "",
            user_net: false,
        },
        runner.scratch_dir(),
    )?;
    if !result.evidence.target {
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

/// `qemu-boot-system`: the headless end-to-end proof of persistent deployment boot.
/// It builds a Btrfs volume containing a verified deployment, an incoming candidate,
/// and @var. One fixture proves a pending candidate is acknowledged and stays free of
/// attempt state; a selector-only fixture covers read-only bookkeeping recovery. A fresh
/// fixture then fails every configured candidate attempt before the health target and
/// must automatically restore verified previous. A final fixture proves corrupt-current
/// fallback independently. Every full boot also asserts the immutable root, writable
/// state, and clean self-exit; healthy boots assert the runtime target.
pub(crate) fn run_system(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (bzimage, selector, deployment) = build_system(runner)?;
    let (mkfs, btrfs) = build_btrfs_tools(runner)?;
    // One trust root for the run: its public half rides the selector this boots,
    // its private half signs every deployment reaching the volume below.
    let trust = RunTrust::generate()?;
    let init_cpio = provision_selector(&selector, runner.scratch_dir(), &trust)?;
    let volume = runner.scratch_dir().join("system-volume.btrfs");
    let fixture = create_persistent_volume_layout(
        &deployment,
        &mkfs,
        &btrfs,
        &volume,
        VolumeLayout::Transactional,
        &trust,
    )?;
    if fixture.initial_id == fixture.alternate_id {
        return Err("transaction fixture candidate did not change the deployment id".to_string());
    }

    println!(
        "   [qemu-boot-system] {qemu} exercises transactional install, boot-attempt rollback, and corrupt-current fallback under TCG: selector -> verified kexec -> loop-mounted root.erofs + persistent @var -> greeter\n              shim kernel:    {}\n              initramfs:      {}\n              Btrfs volume:   {}\n              initial:        {}\n              candidate:      {}",
        bzimage.display(),
        init_cpio.display(),
        volume.display(),
        fixture.initial_id,
        fixture.alternate_id
    );

    let wait_token = autotest_wait_token(boot_timeout());
    let first_tokens = format!(
        "{AUTOTEST_CMDLINE_TOKEN} {wait_token} {PERSIST_WRITE_CMDLINE_TOKEN} \
         {DEPLOY_INSTALL_CMDLINE_TOKEN}"
    );
    let first = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &first_tokens,
        "install",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &first,
        PersistencePhase::Write,
        IdentityPhase::Fresh,
        "install",
        SelectionExpectation::Current,
    )?;
    require_selected_deployment(
        &first,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &fixture.initial_id,
        "install boot",
    )?;
    require_action_marker(
        &first,
        first.evidence.deploy_install,
        SYSTEM_DEPLOY_INSTALL_MARKER,
        "install",
    )?;
    if first.evidence.attempt_consumed || first.evidence.attempts_exhausted {
        return Err(format!(
            "the install boot unexpectedly consumed or exhausted a boot-attempt budget. \
             Last serial output:\n{}",
            tail(&first.console, 80)
        ));
    }
    check_persistent_volume(&btrfs, &volume)?;

    let read_only_recovery = boot(
        &qemu,
        &bzimage,
        &init_cpio,
        BootPlan {
            disk: Some(BootDisk {
                path: &volume,
                read_only: true,
            }),
            mem: "512",
            target_marker: td_boot_protocol::SELECTED_PREVIOUS_MARKER,
            kill_on_marker: true,
            extra_append: "",
            user_net: false,
        },
        runner.scratch_dir(),
    )?;
    println!(
        "   [qemu-boot-system] read-only bookkeeping recovery elapsed: {:.2}s",
        read_only_recovery.elapsed.as_secs_f64()
    );
    if !read_only_recovery.evidence.target
        || !read_only_recovery.evidence.bookkeeping_unavailable
        || !read_only_recovery.evidence.selected_previous
        || read_only_recovery.evidence.selected_current
        || read_only_recovery.evidence.current_rejected
        || read_only_recovery.evidence.attempt_consumed
        || read_only_recovery.evidence.attempts_exhausted
        || read_only_recovery.evidence.kernel_panic
    {
        return Err(format!(
            "the read-only bookkeeping fixture did not recover the verified previous deployment \
             without mutating attempt state. Last serial output:\n{}",
            tail(&read_only_recovery.console, 80)
        ));
    }
    require_selected_deployment(
        &read_only_recovery,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER,
        &fixture.initial_id,
        "read-only bookkeeping recovery boot",
    )?;
    check_persistent_volume(&btrfs, &volume)?;

    let healthy_tokens =
        format!("{AUTOTEST_CMDLINE_TOKEN} {wait_token} {PERSIST_READ_CMDLINE_TOKEN}");
    let healthy_candidate = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &healthy_tokens,
        "healthy pending candidate",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &healthy_candidate,
        PersistencePhase::Read,
        IdentityPhase::Reused,
        "healthy pending candidate",
        SelectionExpectation::Current,
    )?;
    require_selected_deployment(
        &healthy_candidate,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &fixture.alternate_id,
        "healthy pending candidate boot",
    )?;
    require_action_marker(
        &healthy_candidate,
        healthy_candidate.evidence.attempt_consumed,
        td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
        "healthy pending candidate",
    )?;
    // The reboot-crossing proof. The marker only says td-firstboot found an identity
    // already there; this says it is the SAME one, which is what a client pinning a
    // host key (or a fleet keyed by machine-id) actually depends on.
    require_same_identity(&first, &healthy_candidate, "install", "healthy pending candidate")?;
    if healthy_candidate.evidence.attempts_exhausted {
        return Err(format!(
            "the healthy pending candidate unexpectedly exhausted its boot budget. \
             Last serial output:\n{}",
            tail(&healthy_candidate.console, 80)
        ));
    }
    check_persistent_volume(&btrfs, &volume)?;

    let stable_candidate = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &healthy_tokens,
        "acknowledged candidate",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &stable_candidate,
        PersistencePhase::Read,
        IdentityPhase::Reused,
        "acknowledged candidate",
        SelectionExpectation::Current,
    )?;
    require_selected_deployment(
        &stable_candidate,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &fixture.alternate_id,
        "acknowledged candidate boot",
    )?;
    require_same_identity(&first, &stable_candidate, "install", "acknowledged candidate")?;
    if stable_candidate.evidence.attempt_consumed || stable_candidate.evidence.attempts_exhausted {
        return Err(format!(
            "the acknowledged candidate retained boot-attempt state after success. \
             Last serial output:\n{}",
            tail(&stable_candidate.console, 80)
        ));
    }
    check_persistent_volume(&btrfs, &volume)?;

    let failure_fixture = create_persistent_volume_layout(
        &deployment,
        &mkfs,
        &btrfs,
        &volume,
        VolumeLayout::Transactional,
        &trust,
    )?;
    if failure_fixture.initial_id != fixture.initial_id
        || failure_fixture.alternate_id != fixture.alternate_id
    {
        return Err("recreated transaction fixture changed deployment ids".to_string());
    }
    let failure_install = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &first_tokens,
        "failure-sequence install",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &failure_install,
        PersistencePhase::Write,
        IdentityPhase::Fresh,
        "failure-sequence install",
        SelectionExpectation::Current,
    )?;
    require_selected_deployment(
        &failure_install,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &failure_fixture.initial_id,
        "failure-sequence install boot",
    )?;
    require_action_marker(
        &failure_install,
        failure_install.evidence.deploy_install,
        SYSTEM_DEPLOY_INSTALL_MARKER,
        "failure-sequence install",
    )?;
    // The @var was recreated, so this is a DIFFERENT machine and must have a
    // different host key. Equality here would mean the key is coming from the image
    // rather than from this machine's entropy — i.e. every machine that boots the
    // image shares one host identity, exactly what moving the key to /var prevents.
    require_distinct_identity(&first, &failure_install, "install", "failure-sequence install")?;
    if failure_install.evidence.attempt_consumed || failure_install.evidence.attempts_exhausted {
        return Err(format!(
            "the failure-sequence install unexpectedly consumed or exhausted a boot-attempt budget. \
             Last serial output:\n{}",
            tail(&failure_install.console, 80)
        ));
    }
    check_persistent_volume(&btrfs, &volume)?;

    let candidate_tokens =
        format!("{wait_token} {PERSIST_READ_CMDLINE_TOKEN} {BOOT_FAIL_TARGET_CMDLINE_TOKEN}");
    for attempt in 1..=td_boot_protocol::DEFAULT_BOOT_ATTEMPTS {
        let ordinal = format!("candidate attempt {attempt}");
        let candidate = boot_failed_target_once(
            &qemu,
            &bzimage,
            &init_cpio,
            &volume,
            &candidate_tokens,
            &ordinal,
            runner.scratch_dir(),
        )?;
        validate_failed_target_boot(&candidate, &ordinal)?;
        require_selected_deployment(
            &candidate,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            &failure_fixture.alternate_id,
            &ordinal,
        )?;
        require_action_marker(
            &candidate,
            candidate.evidence.attempt_consumed,
            td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
            &ordinal,
        )?;
        if candidate.evidence.attempts_exhausted {
            return Err(format!(
                "the {ordinal} exhausted the candidate before all configured attempts ran. \
                 Last serial output:\n{}",
                tail(&candidate.console, 80)
            ));
        }
        check_persistent_volume(&btrfs, &volume)?;
    }

    let rollback_tokens =
        format!("{AUTOTEST_CMDLINE_TOKEN} {wait_token} {PERSIST_READ_CMDLINE_TOKEN}");
    let automatic_rollback = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &rollback_tokens,
        "automatic rollback",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &automatic_rollback,
        PersistencePhase::Read,
        IdentityPhase::Reused,
        "automatic rollback",
        SelectionExpectation::AttemptsExhausted,
    )?;
    require_same_identity(
        &failure_install,
        &automatic_rollback,
        "failure-sequence install",
        "automatic rollback",
    )?;
    require_selected_deployment(
        &automatic_rollback,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER,
        &failure_fixture.initial_id,
        "automatic rollback boot",
    )?;
    require_action_marker(
        &automatic_rollback,
        automatic_rollback.evidence.attempts_exhausted,
        td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER,
        "automatic rollback",
    )?;
    check_persistent_volume(&btrfs, &volume)?;

    let stable_rollback = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &rollback_tokens,
        "persisted automatic rollback",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &stable_rollback,
        PersistencePhase::Read,
        IdentityPhase::Reused,
        "persisted automatic rollback",
        SelectionExpectation::Current,
    )?;
    require_selected_deployment(
        &stable_rollback,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &failure_fixture.initial_id,
        "persisted automatic rollback boot",
    )?;
    require_same_identity(
        &failure_install,
        &stable_rollback,
        "failure-sequence install",
        "persisted automatic rollback",
    )?;
    if stable_rollback.evidence.attempt_consumed
        || stable_rollback.evidence.attempts_exhausted
        || stable_rollback.evidence.bookkeeping_unavailable
    {
        return Err(format!(
            "the automatic rollback selector rewrite did not persist as attempt-free current. \
             Last serial output:\n{}",
            tail(&stable_rollback.console, 80)
        ));
    }
    check_persistent_volume(&btrfs, &volume)?;

    let fallback_fixture = create_persistent_volume_layout(
        &deployment,
        &mkfs,
        &btrfs,
        &volume,
        VolumeLayout::CorruptCurrent,
        &trust,
    )?;
    if fallback_fixture.initial_id != failure_fixture.initial_id
        || fallback_fixture.alternate_id == fallback_fixture.initial_id
    {
        return Err("corrupt-current fixture ids do not identify current and previous".to_string());
    }
    let fallback_tokens = format!("{AUTOTEST_CMDLINE_TOKEN} {wait_token}");
    let fallback = boot_system_once(
        &qemu,
        &bzimage,
        &init_cpio,
        &volume,
        &fallback_tokens,
        "corrupt-current fallback",
        runner.scratch_dir(),
    )?;
    validate_system_boot(
        &fallback,
        PersistencePhase::None,
        IdentityPhase::Fresh,
        "corrupt-current fallback",
        SelectionExpectation::PreviousFallback,
    )?;
    require_selected_deployment(
        &fallback,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER,
        &fallback_fixture.initial_id,
        "corrupt-current fallback boot",
    )?;
    check_persistent_volume(&btrfs, &volume)?;

    println!(
        "PASS: system-x86-64 transactionally installed and booted a new content-addressed \
         deployment ({SYSTEM_DEPLOY_INSTALL_MARKER}), recovered verified previous when boot \
         bookkeeping was read-only ({BOOKKEEPING_UNAVAILABLE_MARKER}), acknowledged a healthy \
         pending candidate and proved its durable attempt state stayed cleared, consumed {} unacknowledged boot \
         attempts in a fresh fixture ({}) and automatically restored the retained successful \
         deployment ({}), \
         preserved exact /var state across the reused-volume boots \
         ({SYSTEM_PERSIST_WRITE_MARKER} -> {SYSTEM_PERSIST_READ_MARKER}), marked healthy boots \
         successful ({SYSTEM_BOOT_SUCCESS_MARKER}), and rejected a corrupted current payload \
         in favor of verified previous \
         ({} -> {}). Each freshly created @var minted this machine's identity exactly once \
         ({TD_FIRSTBOOT_NEW_MARKER}) and every reboot of it found that identity intact with the \
         SAME SSH host-key fingerprint ({TD_FIRSTBOOT_STABLE_MARKER}), while a recreated @var \
         minted a DIFFERENT one — so the identity is per machine, not per image. Every full boot \
         kept root and /etc immutable \
         ({SYSTEM_ROOT_RO_MARKER}, {SYSTEM_ETC_RO_MARKER}) while the reviewed per-file /etc \
         symlinks still reached that writable state ({SYSTEM_ETC_MUTABLE_MARKER}), mounted \
         target-owned writable @var \
         ({SYSTEM_STATE_WRITABLE_MARKER}, {SYSTEM_STATE_OWNER_MARKER}), ran uutils \
         ({UUTILS_RUNTIME_MARKER}), ripgrep+fd ({RIPGREP_FD_RUNTIME_MARKER}), td-util \
         ({TD_UTIL_RUNTIME_MARKER}), td-txt's grep+sed answering correctly over the live \
         /proc ({TD_TXT_RUNTIME_MARKER}), the td-init boot glue ({TD_INIT_RUNTIME_MARKER}) and a \
         td-login credential switch the switched process read back and confirmed \
         ({TD_LOGIN_RUNTIME_MARKER}), then assigned the single-user graphical seat and brought \
         the software Wayland socket up on virtio-gpu ({TD_WAYLAND_RUNTIME_MARKER}), \
         read an absolute position and its span off the virtio tablet \
         ({TD_POINTER_ABSOLUTE_MARKER}), \
         presented the td-native wl_shm TERMINAL and received its first frame callback \
         ({TD_TERM_RUNTIME_MARKER}), \
         and unmounted state \
         before exit ({SYSTEM_SHUTDOWN_MARKER})",
        td_boot_protocol::DEFAULT_BOOT_ATTEMPTS,
        td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
        td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER,
        td_boot_protocol::CURRENT_REJECTED_MARKER,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER
    );
    Ok(())
}

fn boot_system_once(
    qemu: &str,
    bzimage: &Path,
    init_cpio: &Path,
    volume: &Path,
    tokens: &str,
    label: &str,
    scratch: &Path,
) -> Result<BootResult, String> {
    let result = boot(
        qemu,
        bzimage,
        init_cpio,
        BootPlan {
            disk: Some(BootDisk {
                path: volume,
                read_only: false,
            }),
            mem: "512",
            target_marker: GREETER_MARKER,
            kill_on_marker: false,
            extra_append: tokens,
            user_net: false,
        },
        scratch,
    )?;
    println!(
        "   [qemu-boot-system] {label} elapsed: {:.2}s",
        result.elapsed.as_secs_f64()
    );
    Ok(result)
}

fn boot_failed_target_once(
    qemu: &str,
    bzimage: &Path,
    init_cpio: &Path,
    volume: &Path,
    tokens: &str,
    label: &str,
    scratch: &Path,
) -> Result<BootResult, String> {
    let result = boot(
        qemu,
        bzimage,
        init_cpio,
        BootPlan {
            disk: Some(BootDisk {
                path: volume,
                read_only: false,
            }),
            mem: "512",
            target_marker: SYSTEM_SHUTDOWN_MARKER,
            kill_on_marker: false,
            extra_append: tokens,
            user_net: false,
        },
        scratch,
    )?;
    println!(
        "   [qemu-boot-system] {label} failed-target reboot elapsed: {:.2}s",
        result.elapsed.as_secs_f64()
    );
    Ok(result)
}

fn validate_failed_target_boot(result: &BootResult, ordinal: &str) -> Result<(), String> {
    if !result.evidence.shutdown
        || result.evidence.greeter
        || result.evidence.uutils_runtime
        || result.evidence.ripgrep_fd_runtime
        || result.evidence.sshd
        || result.evidence.boot_success
    {
        return Err(format!(
            "the {ordinal} did not prove a pre-target failure: shutdown must complete while \
             greeter, runtime-health, and success markers remain absent. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    validate_primary_selection(result, ordinal)?;
    if !result.evidence.root_read_only
        || !result.evidence.etc_read_only
        || !result.evidence.state_writable
        || !result.evidence.state_owner
        || !result.evidence.persist_read
    {
        return Err(format!(
            "the {ordinal} failed before the greeter but did not preserve the root, state, and \
             persistence preconditions. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if result.evidence.kernel_panic || !result.exited_clean {
        return Err(format!(
            "the {ordinal} did not reboot cleanly after the injected pre-target failure — {}. \
             Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

fn require_action_marker(
    result: &BootResult,
    seen: bool,
    marker: &str,
    action: &str,
) -> Result<(), String> {
    if seen {
        Ok(())
    } else {
        Err(format!(
            "the {action} boot reached userspace but did not emit {marker:?}; the verified \
             deployment transaction did not complete. Last serial output:\n{}",
            tail(&result.console, 80)
        ))
    }
}

enum PersistencePhase {
    None,
    Write,
    Read,
}

enum SelectionExpectation {
    Current,
    PreviousFallback,
    AttemptsExhausted,
}

/// What this boot's `@var` subvolume already holds, which decides whether
/// td-firstboot must MINT this machine's identity or must find it already there.
/// Stated per call site rather than derived from `PersistencePhase`, because that
/// phase describes which persistence markers the kernel cmdline asked for and says
/// nothing about whether the volume was just recreated.
enum IdentityPhase {
    /// A freshly created `@var`: a new machine, so identity is minted here.
    Fresh,
    /// The same `@var` a previous boot in this run already provisioned.
    Reused,
}

fn validate_system_boot(
    result: &BootResult,
    persistence: PersistencePhase,
    identity: IdentityPhase,
    ordinal: &str,
    selection: SelectionExpectation,
) -> Result<(), String> {
    if !result.evidence.target {
        return Err(format!(
            "the {ordinal} persistent boot did not reach the greeter {GREETER_MARKER:?} on ttyS0 — {} \
             (selection, kexec, Btrfs, loop setup, EROFS, switch_root, or login failed). \
             Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    match selection {
        SelectionExpectation::Current => {
            validate_primary_selection(result, &format!("{ordinal} persistent boot"))?
        }
        SelectionExpectation::PreviousFallback => {
            validate_fallback_selection(result, &format!("{ordinal} persistent boot"))?
        }
        SelectionExpectation::AttemptsExhausted => {
            validate_exhausted_selection(result, &format!("{ordinal} persistent boot"))?
        }
    }
    if !result.evidence.root_read_only {
        return Err(format!(
            "the greeter was reached but /etc/rootcheck did not confirm a READ-ONLY erofs root \
             ({SYSTEM_ROOT_RO_MARKER:?} absent from /proc/mounts) — `/` is not a read-only erofs mount \
             after switch_root. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.etc_read_only {
        return Err(format!(
            "the greeter was reached but /etc/rootcheck did not confirm immutable deployment config \
             ({SYSTEM_ETC_RO_MARKER:?} absent) — root could write below /etc or the check did not run. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    // The other half of the /etc contract. `SYSTEM_ETC_RO_MARKER` above says /etc
    // rejects writes; this says the handful of per-machine files reach writable state
    // anyway, through the reviewed per-file symlinks. Both, on the same boot, are what
    // "immutable /etc with per-machine identity" means — and it is checked on a
    // BOOTED machine because a staged tree cannot show that a read through a dangling
    // build-time symlink resolves once /var is mounted and provisioned.
    if !result.evidence.etc_mutable {
        return Err(format!(
            "the {ordinal} boot reached the greeter but /etc/rootcheck did not confirm the \
             mutable-/etc contract ({SYSTEM_ETC_MUTABLE_MARKER:?} absent) — a reviewed \
             MUTABLE_ETC symlink points somewhere other than the image recorded, a persistent \
             one did not resolve (so td-firstboot did not provision it), /etc/machine-id is not \
             32 hex digits through its symlink, or the unprivileged login user could read the \
             SSH host PRIVATE key (or could not read its .pub). Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    // td-firstboot's own report. A fresh @var must be provisioned exactly once, and
    // every later boot must find that identity intact: the marker that appears says
    // which happened, and the marker that does NOT is the load-bearing half —
    // `TD_FIRSTBOOT_NEW_MARKER` on a reused volume means the machine silently became
    // a DIFFERENT machine, which is the failure this whole mechanism exists to
    // prevent and which nothing else in the boot would notice.
    let (wanted, unwanted, why) = match identity {
        IdentityPhase::Fresh => (
            (TD_FIRSTBOOT_NEW_MARKER, result.evidence.firstboot_new),
            (TD_FIRSTBOOT_STABLE_MARKER, result.evidence.firstboot_stable),
            "this @var was just created, so td-firstboot must mint the identity here",
        ),
        IdentityPhase::Reused => (
            (TD_FIRSTBOOT_STABLE_MARKER, result.evidence.firstboot_stable),
            (TD_FIRSTBOOT_NEW_MARKER, result.evidence.firstboot_new),
            "this @var was already provisioned by an earlier boot in this run, so the \
             identity must survive unchanged",
        ),
    };
    if !wanted.1 {
        return Err(format!(
            "the {ordinal} boot did not emit {:?} — {why}. Either /bin/td-firstboot did not run \
             at sysinit, or it refused (the console carries its diagnostic: a volatile or \
             read-only state filesystem, a malformed machine-id it will not replace, or a \
             `/bin/sshd keygen` that failed). Last serial output:\n{}",
            wanted.0,
            tail(&result.console, 80)
        ));
    }
    if unwanted.1 {
        return Err(format!(
            "the {ordinal} boot emitted {:?}, but {why}. Last serial output:\n{}",
            unwanted.0,
            tail(&result.console, 80)
        ));
    }
    if result.evidence.host_key.is_none() {
        return Err(format!(
            "the {ordinal} boot provisioned an identity but printed no \
             {TD_FIRSTBOOT_HOST_KEY_PREFIX:?} fingerprint line, so there is nothing to compare \
             across reboots. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    // The shipped system deliberately autologins an unprivileged user; keep that
    // target-ownership guarantee part of the distribution oracle.
    if !result.evidence.state_owner {
        return Err(format!(
            "the greeter was reached but the unprivileged ownership check failed \
             ({SYSTEM_STATE_OWNER_MARKER:?} absent) — the login user could write /var or \
             /var/root, or could not write its own home. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.state_writable {
        return Err(format!(
            "the greeter was reached but /etc/rootcheck did not confirm writable state \
             ({SYSTEM_STATE_WRITABLE_MARKER:?} absent) — /var is not Btrfs, /run or /tmp is not tmpfs, \
             /run/td-volume is not read-only Btrfs, a state path rejected its write probe, \
             /home, /root, or /var/run has the wrong target, home ownership setup failed, or \
             failed-target parking could not be prepared. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.uutils_runtime {
        return Err(format!(
            "the greeter was reached and root checks passed, but the uutils runtime marker \
             ({UUTILS_RUNTIME_MARKER:?}) was absent — a named unprivileged uutils probe failed \
             its exact output, exit-status, environment, or hard-link mutation contract, or the \
             dynamically-linked coreutils runtime closure (ELF interp, glibc, libgcc_s) does not \
             resolve on the erofs root. Static shape checks cannot see that runtime DT_NEEDED \
             failure. The guest console names the failed applet. Each health leg emits its own \
             marker, so this absence localizes to uutils. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.ripgrep_fd_runtime {
        return Err(format!(
            "the greeter was reached and uutils ran, but the ripgrep/fd runtime marker \
             ({RIPGREP_FD_RUNTIME_MARKER:?}) was absent — the unprivileged health leg did not \
             get the exact configured hostname match from `/bin/rg` and the exact \
             `/etc/hostname` path from `/bin/fd`. The console names the command and unexpected \
             result; either its /bin symlink is wrong or its dynamically linked runtime closure \
             does not resolve on the EROFS root. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.sshd {
        return Err(format!(
            "the greeter was reached and root/userland checks passed, but the sshd runtime marker \
             ({SSHD_MARKER:?}) was absent — `/bin/sshd selftest` did not complete a loopback SSH \
             round-trip and exit 0. Either the kernel lacks working TCP/IP loopback (CONFIG_NET/INET \
             or the `lo` bring-up regressed), or sshd's dynamic runtime closure (loader, glibc, \
             libgcc_s, aws-lc crypto) does not resolve on the erofs root. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_util_runtime {
        return Err(format!(
            "the greeter was reached and root/userland/sshd checks passed, but the td-util runtime \
             marker ({TD_UTIL_RUNTIME_MARKER:?}) was absent — at least one /bin name the td-util \
             farm serves did not exit 0, so a shipped diagnostics command is broken on the image. \
             The console names the applet (`td-util: /bin/<name> failed`). Either the static \
             multicall does not run on the erofs root, its argv[0] dispatch regressed, or the \
             /proc or /dev/kmsg the applet reads is unavailable there. td-util-test covers ELF \
             shape and dispatch in the build sandbox but skips those legs when it has no /proc. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_txt_runtime {
        return Err(format!(
            "the root/userland/sshd/td-util health checks passed, but the td-txt runtime marker \
             ({TD_TXT_RUNTIME_MARKER:?}) was absent — `/bin/grep` or `/bin/sed` did not give the \
             expected ANSWER on this image. The console names which leg (`td-txt: ...`). This is \
             not merely a broken diagnostics command: /etc/rootcheck decides the root came up \
             read-only with the same `grep -Eq` over /proc/mounts, so a grep that answers wrongly \
             would mark a broken root healthy in silence — which is why the probe asserts a \
             non-match as well as a match. Either the static multicall does not run on the erofs \
             root, its argv[0] dispatch regressed, or it mis-reads a /proc file (they stat as \
             zero-length, so a reader that sized a buffer from st_size sees nothing). The \
             td-txt conformance corpus covers the same invocation shapes host-side. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_init_runtime {
        return Err(format!(
            "the root/userland/sshd/td-util health checks passed, but the td-init runtime marker \
             ({TD_INIT_RUNTIME_MARKER:?}) was absent — at least one /bin name the boot-glue farm \
             serves did not behave. The console names it (`td-init: /bin/<name> ...`). Note what \
             reaching the health target ALREADY proved: td-init ran the inittab as PID 1 and \
             pivoted the root as `switch_root`, since nothing else does either on this image. \
             So this marker is about the REST of the farm — `hostname` reading back what \
             sysinit set, `cttyhack` exec'ing, `init --dry-run` accepting the shipped table, and \
             `reboot`/`poweroff`/`halt`/`switch_root` REFUSING a bogus argument with a diagnostic \
             rather than acting on it. That last class is the one to read carefully: a refusal \
             probe that fails means an irreversible applet ran something it should have rejected. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_login_runtime {
        return Err(format!(
            "the root/userland/sshd/td-util/td-init health checks passed, but the td-login \
             runtime marker ({TD_LOGIN_RUNTIME_MARKER:?}) was absent — `/bin/su` reached the \
             unprivileged login user (every other health leg above runs through it, so it \
             must have), but the switched process did not read its own credentials back as \
             the ones the switch asked for. The console names the disagreement \
             (`real/effective/saved/filesystem uid is …, expected …`, or `supplementary \
             groups are …`). Read this one carefully: it is the ONLY check on this image \
             that would notice a credential switch which started a perfectly working \
             session while leaving a residual credential attached — a `setuid(2)` issued \
             before `setgroups(2)` drops the uid and silently keeps root's supplementary \
             groups, and every other marker here still prints. See \
             td-login/THREAT-MODEL.md. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_wayland_runtime {
        return Err(format!(
            "the serial boot and userland health checks passed, but the graphical runtime \
             marker ({TD_WAYLAND_RUNTIME_MARKER:?}) was absent — td-seatd did not assign \
             /dev/fb0 and the evdev seat to uid 1000, the unprivileged compositor could not \
             paint the virtio-gpu framebuffer, or its mode-0600 Wayland socket never began \
             listening. The serial greeter remains the recovery path. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_pointer_absolute {
        return Err(format!(
            "the compositor came up, but no input device reported an absolute position \
             ({TD_POINTER_ABSOLUTE_MARKER:?} was absent) — the guest kernel has no \
             VIRTIO_INPUT driver, this runner's argv no longer carries \
             -device virtio-tablet-pci, the compositor's EVIOCGABS was refused, or its \
             answer was dropped before the reader that maps with it. Those four are what \
             REACHES here: a qemu that cannot attach the device fails at startup, and a \
             seat the compositor cannot open kills it before it announces, so the \
             graphical marker above catches that one. Nothing else notices this: the \
             compositor still runs, the PS/2 mouse still moves a cursor, and it simply \
             cannot be pushed to the right or bottom edge of the screen. This is the ONLY \
             check that a real device answered — the unit gate has no absolute device to \
             ask. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.td_term_runtime {
        return Err(format!(
            "the compositor became ready, but the TERMINAL marker \
             ({TD_TERM_RUNTIME_MARKER:?}) was absent — registry binding, the XDG \
             configure/ack handshake, wl_shm descriptor transfer, buffer release, the \
             first frame callback, keymap verification, the devpts PTY, or the child \
             shell failed. The machine \
             booted to a compositor with nothing on it. The serial greeter remains the \
             recovery path. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.boot_success {
        return Err(format!(
            "the {ordinal} boot did not emit the deployment-success marker \
             {SYSTEM_BOOT_SUCCESS_MARKER:?}. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if let Some((persistence_marker, persistence_seen)) = match persistence {
        PersistencePhase::None => None,
        PersistencePhase::Write => {
            Some((SYSTEM_PERSIST_WRITE_MARKER, result.evidence.persist_write))
        }
        PersistencePhase::Read => Some((SYSTEM_PERSIST_READ_MARKER, result.evidence.persist_read)),
    } {
        if !persistence_seen {
            return Err(format!(
                "the {ordinal} boot reached the greeter but did not emit the persistence marker \
                 {persistence_marker:?}; boot one must write+sync it and later boots must read the \
                 same bytes from the reused @var subvolume. Last serial output:\n{}",
                tail(&result.console, 80)
            ));
        }
    }
    validate_persistent_shutdown(result, &format!("{ordinal} persistent boot"))?;
    // A kernel panic under `panic=-1` reboots and, with `-no-reboot`, exits qemu 0 — the
    // SAME exit code as a clean guest power-off. So `exited_clean` alone cannot tell a
    // genuine "exit powers off" from a panic AFTER the markers were printed (the root
    // checks run at sysinit, before the greeter); scan the console for a panic explicitly
    // so such a boot reds instead of false-passing as a clean shutdown (re #550, subagent
    // review). "Kernel panic" is the leading fragment of the kernel's "Kernel panic - not
    // syncing:" banner.
    if result.evidence.kernel_panic {
        return Err(format!(
            "the markers were printed but the kernel PANICKED rather than powering off cleanly — \
             under `panic=-1` a panic also exits qemu 0, so this would otherwise masquerade as a \
             clean \"exit powers off\". Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.exited_clean {
        return Err(format!(
            "the greeter was reached and the root checks passed, but the VM did not power off cleanly \
             on the autotest `exit` — {} (the `exit`-powers-off path regressed: getty/login did not \
             return 0, or init-mediated reboot did not fire). Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

/// The SSH host-key fingerprints two boots reported. `validate_system_boot` has
/// already refused a boot that printed none, so a missing one here is a caller
/// passing an unvalidated boot rather than a guest failure — say so instead of
/// silently comparing `None == None` and passing.
fn identities(
    earlier: &BootResult,
    later: &BootResult,
    earlier_label: &str,
    later_label: &str,
) -> Result<(String, String), String> {
    match (&earlier.evidence.host_key, &later.evidence.host_key) {
        (Some(a), Some(b)) => Ok((a.clone(), b.clone())),
        _ => Err(format!(
            "cannot compare the {earlier_label} and {later_label} host identities: one of them \
             reported no fingerprint, which validate_system_boot should already have rejected"
        )),
    }
}

/// Two boots of the SAME machine must present the same host key. This is the
/// assertion a marker cannot make: `TD_FIRSTBOOT_STABLE_MARKER` only says the files
/// were already there, not that they still hold the identity a client pinned.
fn require_same_identity(
    earlier: &BootResult,
    later: &BootResult,
    earlier_label: &str,
    later_label: &str,
) -> Result<(), String> {
    let (before, after) = identities(earlier, later, earlier_label, later_label)?;
    if before != after {
        return Err(format!(
            "the machine's SSH host key CHANGED across a reboot of the same @var: the \
             {earlier_label} boot presented {before} and the {later_label} boot presented \
             {after}. Every client that pinned the first key now sees a host-key mismatch, \
             which is indistinguishable from an attack — per-machine identity did not \
             persist. Last serial output:\n{}",
            tail(&later.console, 80)
        ));
    }
    Ok(())
}

/// A freshly created `@var` is a DIFFERENT machine and must get a different host
/// key. Equality would mean the key came from the image rather than from this
/// machine's entropy — one identity shared by every machine that boots the image,
/// which is precisely what keeping it out of the image prevents.
fn require_distinct_identity(
    earlier: &BootResult,
    later: &BootResult,
    earlier_label: &str,
    later_label: &str,
) -> Result<(), String> {
    let (before, after) = identities(earlier, later, earlier_label, later_label)?;
    if before == after {
        return Err(format!(
            "a freshly created @var produced the SAME SSH host key as the previous machine \
             ({before}): the {earlier_label} and {later_label} boots are different machines, so \
             an identical host identity means it is baked into the image (or derived from \
             something constant) rather than minted per machine. Last serial output:\n{}",
            tail(&later.console, 80)
        ));
    }
    Ok(())
}

fn validate_primary_selection(result: &BootResult, context: &str) -> Result<(), String> {
    if result.evidence.current_rejected {
        return Err(format!(
            "the {context} reached userspace only after td-boot rejected the current \
             deployment and fell back to previous — the primary selector path is broken. \
             Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.selected_current || result.evidence.selected_previous {
        return Err(format!(
            "the {context} did not report an unambiguous current selection \
             ({:?} absent or {:?} unexpectedly present). Last serial output:\n{}",
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            td_boot_protocol::SELECTED_PREVIOUS_MARKER,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

fn validate_fallback_selection(result: &BootResult, context: &str) -> Result<(), String> {
    if !result.evidence.current_rejected
        || !result.evidence.selected_previous
        || result.evidence.selected_current
        || result.evidence.attempts_exhausted
        || result.evidence.bookkeeping_unavailable
    {
        return Err(format!(
            "the {context} did not prove corrupt-current fallback: {:?} and {:?} must be \
             present while {:?}, {:?}, and attempt exhaustion remain absent. Last serial output:\n{}",
            td_boot_protocol::CURRENT_REJECTED_MARKER,
            td_boot_protocol::SELECTED_PREVIOUS_MARKER,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            BOOKKEEPING_UNAVAILABLE_MARKER,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

fn validate_exhausted_selection(result: &BootResult, context: &str) -> Result<(), String> {
    if result.evidence.current_rejected
        || !result.evidence.attempts_exhausted
        || !result.evidence.selected_previous
        || result.evidence.selected_current
    {
        return Err(format!(
            "the {context} did not prove automatic rollback after exhausted attempts: {:?} and \
             {:?} must be present while {:?} and {:?} remain absent. Last serial output:\n{}",
            td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER,
            td_boot_protocol::SELECTED_PREVIOUS_MARKER,
            td_boot_protocol::CURRENT_REJECTED_MARKER,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

fn require_selected_deployment(
    result: &BootResult,
    marker: &str,
    deployment_id: &str,
    context: &str,
) -> Result<(), String> {
    let selected = if marker == td_boot_protocol::SELECTED_CURRENT_MARKER {
        result.evidence.selected_current_id.as_deref()
    } else if marker == td_boot_protocol::SELECTED_PREVIOUS_MARKER {
        result.evidence.selected_previous_id.as_deref()
    } else {
        return Err(format!(
            "internal: unknown deployment selection marker {marker:?}"
        ));
    };
    if selected == Some(deployment_id) {
        return Ok(());
    }
    Err(format!(
        "the {context} did not latch the expected deployment {deployment_id} with \
         {marker:?} (latched {selected:?}). Last serial output:\n{}",
        tail(&result.console, 80)
    ))
}

fn validate_persistent_shutdown(result: &BootResult, context: &str) -> Result<(), String> {
    if !result.evidence.shutdown {
        return Err(format!(
            "the {context} reached its userspace markers but BusyBox init did not complete \
             the persistent shutdown action ({SYSTEM_SHUTDOWN_MARKER:?} absent) — @var was \
             not synced and unmounted before reboot. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

/// `qemu-boot-net`: the operator proof that the source-built kernel + the static
/// td-netd bring the network up under QEMU user-net and can resolve + reach a host.
/// It boots the SAME `system-x86-64` deployment as `qemu-boot-system`, but with a
/// user-mode NIC on virtio-net-pci and BOTH the nettest and autotest tokens on the
/// cmdline: at sysinit `/etc/netup` DHCP-configures the link (SLIRP hands out
/// 10.0.2.15), then td-netd's own DNS client resolves the test host via the DHCP
/// nameserver (10.0.2.3) and TCP-connects it — printing the three net markers — before
/// the greeter self-exits (autotest) and the VM powers off. Host-side (never a gated
/// check) like the other qemu oracles: it needs host qemu AND outbound DNS/TCP from
/// the operator host (SLIRP forwards the guest's DNS and NATs its TCP), which the
/// gate's host-free sandbox has neither of.
pub(crate) fn run_net(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (bzimage, init_cpio, disk, btrfs) = build_persistent_system(runner)?;

    println!(
        "   [qemu-boot-net] {qemu} boots the recipe-built deployment under TCG with a user-mode NIC; /etc/netup DHCP-configures the link, then td-netd resolves + reaches {}:{}\n              kernel:        {}\n              initramfs:     {}\n              Btrfs volume:  {}",
        td_recipe::ladder::NETTEST_DEFAULT_HOST,
        td_recipe::ladder::NETTEST_DEFAULT_PORT,
        bzimage.display(),
        init_cpio.display(),
        disk.display()
    );

    // Nettest drives netup's resolve+reach self-test (the three net markers); autotest
    // and its host-derived wait bound make the greeter self-exit after health completion
    // so the VM powers off cleanly. Key on the greeter (reached AFTER netup) with
    // kill_on_marker=false so the net markers, which print earlier at sysinit, are all
    // captured before the guest powers off.
    let wait_token = autotest_wait_token(boot_timeout());
    let tokens = format!("{AUTOTEST_CMDLINE_TOKEN} {wait_token} {NETTEST_CMDLINE_TOKEN}");
    let result = boot(
        &qemu,
        &bzimage,
        &init_cpio,
        BootPlan {
            disk: Some(BootDisk {
                path: &disk,
                read_only: false,
            }),
            mem: "512",
            target_marker: GREETER_MARKER,
            kill_on_marker: false,
            extra_append: &tokens,
            user_net: true,
        },
        runner.scratch_dir(),
    )?;
    println!(
        "   [qemu-boot-net] elapsed: {:.2}s",
        result.elapsed.as_secs_f64()
    );

    if !result.evidence.target {
        return Err(format!(
            "the selector/kexec boot did not reach the greeter {GREETER_MARKER:?} on ttyS0 — {} \
             (the network self-test runs at sysinit BEFORE the greeter, so a boot that never \
             reached the greeter likely failed earlier — unrelated to networking). Last serial \
             output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    validate_primary_selection(&result, "network boot")?;
    if !result.evidence.net_up {
        return Err(format!(
            "the boot reached the greeter but td-netd did not bring the link up \
             ({SYSTEM_NET_UP_MARKER:?} absent) — the VIRTIO_NET NIC did not appear, autodetect \
             found no interface, or the DHCP handshake (SLIRP's server at 10.0.2.2) did not \
             complete. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.net_resolve {
        return Err(format!(
            "the link came up but td-netd could not RESOLVE the test host \
             ({SYSTEM_NET_RESOLVE_MARKER:?} absent) — its DNS client got no A record from the \
             DHCP-provided nameserver (SLIRP's 10.0.2.3). This can also mean the OPERATOR HOST \
             has no outbound DNS for SLIRP to forward. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.net_reach {
        return Err(format!(
            "resolve succeeded but td-netd could not REACH the host \
             ({SYSTEM_NET_REACH_MARKER:?} absent) — the TCP connect to the resolved address \
             failed or timed out. This can also mean the OPERATOR HOST has no outbound TCP for \
             SLIRP to NAT. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if result.evidence.kernel_panic {
        return Err(format!(
            "the net markers were printed but the kernel PANICKED rather than powering off cleanly — \
             under `panic=-1` a panic also exits qemu 0, so this would otherwise masquerade as a \
             clean shutdown. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    if !result.exited_clean {
        return Err(format!(
            "the net self-test passed but the VM did not power off cleanly on the autotest `exit` — \
             {}. Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    validate_persistent_shutdown(&result, "network boot")?;
    check_persistent_volume(&btrfs, &disk)?;
    println!(
        "PASS: system-x86-64 brings the network up under qemu user-net — td-netd DHCP-configures the \
         virtio-net link ({SYSTEM_NET_UP_MARKER}), resolves the test host with its own DNS client \
         ({SYSTEM_NET_RESOLVE_MARKER}), TCP-reaches it ({SYSTEM_NET_REACH_MARKER}), and the VM powers \
         off cleanly"
    );
    Ok(())
}

/// `qemu-boot-kexec` (Phase-0 kexec spike): the operator proof that the source-built
/// kernel can kexec_file_load(2) a SECOND kernel start under qemu TCG — the mechanism
/// the image-based boot uses to self-boot a refreshed image. It boots the
/// `kexec-spike-x86-64` outer bzImage + outer initramfs; the outer /init prints STAGE1
/// then execs td-kexec to load and reboot(KEXEC) into the inner kernel + inner
/// initramfs, whose /init prints STAGE2. STAGE2 is the sole success criterion — it is
/// unreachable without a working kexec; STAGE1 only refines the FAILURE diagnostic
/// (and may legitimately have scrolled out of the bounded console buffer once STAGE2
/// is in). Host-side (never a gated check) for the same reason `qemu-boot` is — the
/// gate sandbox has no host qemu.
pub(crate) fn run_kexec(runner: &RecipeCheckRunner) -> Result<(), String> {
    // qemu first (fail fast if absent), then the spike artifact.
    let qemu = find_qemu()?;
    let (bzimage, initramfs) = build_spike(runner)?;

    println!(
        "   [qemu-boot-kexec] {qemu} boots the td-source-built bzImage under TCG, then the outer /init td-kexecs a SECOND kernel start\n              kernel:    {}\n              initramfs: {}",
        bzimage.display(),
        initramfs.display()
    );

    // Key on STAGE2 and kill on it. 512 MiB so the outer kernel + outer initramfs AND
    // the kexec-loaded inner kernel + inner initramfs all fit at the instant the jump
    // happens (a tiny allnoconfig kernel would fit in less, but headroom is free under
    // TCG and an OOM at the kexec would be a confusing failure).
    let result = boot(
        &qemu,
        &bzimage,
        &initramfs,
        BootPlan {
            disk: None,
            mem: "512",
            target_marker: KEXEC_STAGE2_MARKER,
            kill_on_marker: true,
            extra_append: "",
            user_net: false,
        },
        runner.scratch_dir(),
    )?;
    if !result.evidence.target {
        // STAGE2 absent — use STAGE1's presence to point at which half regressed.
        // STAGE1 present isolates the kexec itself; STAGE1 absent USUALLY means the
        // outer kernel/init never ran, but STAGE1 can also have scrolled out of the
        // bounded tail ahead of the failure, so that branch is hedged, not asserted.
        let detail = if result.evidence.kexec_stage1 {
            "stage-1 ran (STAGE1 seen) but td-kexec's kexec_file_load(2)/reboot(KEXEC) did \
             not reach stage-2 — the kexec itself failed, or the inner kernel/initramfs did not boot"
        } else {
            "STAGE1 is absent from the retained console tail — most likely the outer kernel did \
             not boot the outer initramfs or its /init did not run, though STAGE1 may instead have \
             scrolled out of the bounded tail ahead of the failure"
        };
        return Err(format!(
            "the kexec spike did not reach stage-2 {KEXEC_STAGE2_MARKER:?} on ttyS0 — {} — {detail}. \
             Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    // STAGE2 present ⇒ the kexec worked; STAGE1 necessarily preceded it (its absence from
    // the bounded buffer is not a failure). Note whether it was still visible, for context.
    let stage1 = if result.evidence.kexec_stage1 {
        KEXEC_STAGE1_MARKER
    } else {
        "STAGE1 (scrolled out of the console tail)"
    };
    println!(
        "PASS: kexec-spike-x86-64 kexecs under qemu (TCG) — the td-source-built kernel boots, td-kexec \
         kexec_file_load(2)+reboot(KEXEC)s a SECOND kernel start ({stage1} -> {KEXEC_STAGE2_MARKER} on ttyS0)"
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

/// Build the `kexec-spike-x86-64` producer and return its
/// `(bzImage, outer-initramfs.cpio)` — the two-kernel boot artifact `run_kexec` boots.
fn build_spike(runner: &RecipeCheckRunner) -> Result<(PathBuf, PathBuf), String> {
    runner.prepare_recipe_target("kexec-spike-x86-64")?;
    let build_out = runner.build_plan("kexec-spike-x86-64")?;
    let tree = runner.ladder_out_from(&build_out, "kexec-spike-x86-64")?;
    let bzimage = tree.join("bzImage");
    let initramfs = tree.join("outer-initramfs.cpio");
    for (label, path) in [("bzImage", &bzimage), ("outer-initramfs.cpio", &initramfs)] {
        if !path.is_file() {
            return Err(format!(
                "kexec-spike-x86-64 output is missing {label} ({}) — the kexec boot needs both the kernel and the two-kernel initramfs",
                path.display()
            ));
        }
    }
    Ok((bzimage, initramfs))
}

fn parse_host_manifest(
    manifest_path: &Path,
    kind: &str,
    canonical_names: &[&str],
) -> Result<Vec<String>, String> {
    let manifest = fs::read_to_string(manifest_path)
        .map_err(|e| format!("read {kind} manifest {}: {e}", manifest_path.display()))?;
    if !manifest.ends_with('\n') {
        return Err(format!(
            "{kind} manifest {} has no final newline",
            manifest_path.display()
        ));
    }
    let mut lines = manifest.lines();
    if lines.next() != Some("td-deployment-v1") {
        return Err(format!(
            "{kind} manifest {} has an unsupported or missing version header",
            manifest_path.display()
        ));
    }

    let mut digests = Vec::with_capacity(canonical_names.len());
    for name in canonical_names {
        let line = lines.next().ok_or_else(|| {
            format!(
                "{kind} manifest {} is missing the {name} entry",
                manifest_path.display()
            )
        })?;
        let (digest, label) = line.split_once("  ").ok_or_else(|| {
            format!(
                "{kind} manifest {} has a malformed {name} entry",
                manifest_path.display()
            )
        })?;
        if label != *name || !valid_manifest_digest(digest) {
            return Err(format!(
                "{kind} manifest {} has a non-canonical {name} entry",
                manifest_path.display()
            ));
        }
        digests.push(digest.to_string());
    }
    if lines.next().is_some() {
        return Err(format!(
            "{kind} manifest {} has unexpected extra entries",
            manifest_path.display()
        ));
    }
    Ok(digests)
}

fn valid_manifest_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Validate a deployment manifest and return its three payload paths. The
/// parser is deliberately strict: one version header, the canonical payload
/// order, lowercase SHA-256, exactly two separating spaces, and no extra lines.
/// Hashing again at consumption catches stale, truncated, or tampered staging
/// before qemu sees any artifact.
pub(crate) fn verify_deployment(deployment: &Path) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    // This byte-sorted order is part of the v1 wire format: the producer sorts
    // labels before writing, and the strict consumer rejects non-canonical order.
    const CANONICAL_NAMES: [&str; 3] = ["bzImage", "initramfs.cpio", "root.erofs"];
    let manifest_path = deployment.join("manifest");
    let digests = parse_host_manifest(&manifest_path, "deployment", &CANONICAL_NAMES)?;

    let mut paths = Vec::with_capacity(CANONICAL_NAMES.len());
    for (name, digest) in CANONICAL_NAMES.into_iter().zip(digests) {
        let path = deployment.join(name);
        if !path.is_file() {
            return Err(format!(
                "system-x86-64 deployment is missing {name} ({})",
                path.display()
            ));
        }
        let actual = crate::sha256::sha256_file(&path)
            .map_err(|e| format!("hash deployment payload {}: {e}", path.display()))?;
        if actual != digest {
            return Err(format!(
                "system-x86-64 deployment hash mismatch for {name}: manifest has {digest}, payload has {actual}"
            ));
        }
        paths.push(path);
    }
    let mut paths = paths.into_iter();
    let bzimage = paths
        .next()
        .ok_or_else(|| "internal: verified deployment lost bzImage".to_string())?;
    let initramfs = paths
        .next()
        .ok_or_else(|| "internal: verified deployment lost initramfs.cpio".to_string())?;
    let root = paths
        .next()
        .ok_or_else(|| "internal: verified deployment lost root.erofs".to_string())?;
    Ok((bzimage, initramfs, root))
}

/// Verify the direct-boot selector before handing its privileged td-boot/td-kexec
/// contents to qemu.
pub(crate) fn verify_selector(boot: &Path) -> Result<VerifiedSelector, String> {
    const NAME: &str = "selector-initramfs.cpio";
    let manifest_path = boot.join("manifest");
    let digest = parse_host_manifest(&manifest_path, "selector", &[NAME])?
        .into_iter()
        .next()
        .ok_or_else(|| "internal: verified selector manifest lost its digest".to_string())?;
    let selector = boot.join(NAME);
    if !selector.is_file() {
        return Err(format!(
            "system-x86-64 selector is missing {NAME} ({})",
            selector.display()
        ));
    }
    let actual = crate::sha256::sha256_file(&selector)
        .map_err(|e| format!("hash selector payload {}: {e}", selector.display()))?;
    if actual != digest {
        return Err(format!(
            "system-x86-64 selector hash mismatch: manifest has {digest}, payload has {actual}"
        ));
    }
    Ok(VerifiedSelector(selector))
}

/// A selector initramfs straight from the store, verified against its own
/// manifest — and NOT yet bootable.
///
/// The path is private on purpose. `provision_selector` is the only way to get
/// a bootable one out, and it is what appends the run's trusted key, so a
/// caller cannot boot the unprovisioned original by naming the wrong variable.
/// That mistake has no symptom: the machine comes up with no trust root and
/// nothing on either side reports it. A review found four mutations of exactly
/// that shape surviving the whole suite, which is why this is a type rather
/// than another assertion.
#[derive(Debug)]
pub(crate) struct VerifiedSelector(PathBuf);

/// Build `system-x86-64` and return the direct-boot kernel, selector initramfs,
/// and verified deployment directory.
fn build_system(
    runner: &RecipeCheckRunner,
) -> Result<(PathBuf, VerifiedSelector, PathBuf), String> {
    runner.prepare_recipe_target("system-x86-64")?;
    let build_out = runner.build_plan("system-x86-64")?;
    let system_tree = runner.ladder_out_from(&build_out, "system-x86-64")?;
    let deployment = system_tree.join("deployment");
    let (bzimage, _, _) = verify_deployment(&deployment)?;
    let selector = verify_selector(&system_tree.join("boot"))?;
    Ok((bzimage, selector, deployment))
}

fn build_persistent_system(
    runner: &RecipeCheckRunner,
) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let (bzimage, selector, deployment) = build_system(runner)?;
    let (mkfs, btrfs) = build_btrfs_tools(runner)?;
    let trust = RunTrust::generate()?;
    let initramfs = provision_selector(&selector, runner.scratch_dir(), &trust)?;
    let volume = runner.scratch_dir().join("system-volume.btrfs");
    create_persistent_volume(&deployment, &mkfs, &btrfs, &volume, &trust)?;
    Ok((bzimage, initramfs, volume, btrfs))
}

pub(crate) fn build_btrfs_tools(runner: &RecipeCheckRunner) -> Result<(PathBuf, PathBuf), String> {
    runner.prepare_recipe_target("btrfs-progs-x86-64")?;
    let build_out = runner.build_plan("btrfs-progs-x86-64")?;
    let tree = runner.ladder_out_from(&build_out, "btrfs-progs-x86-64")?;
    let mkfs = tree.join("bin/mkfs.btrfs");
    let btrfs = tree.join("bin/btrfs");
    for (label, path) in [("mkfs.btrfs", &mkfs), ("btrfs", &btrfs)] {
        if !is_executable(path) {
            return Err(format!(
                "btrfs-progs-x86-64 output is missing executable {label} ({})",
                path.display()
            ));
        }
    }
    Ok((mkfs, btrfs))
}

/// Build one writable Btrfs image containing a content-addressed deployment and
/// an empty @var subvolume. This is a disposable host-side test fixture; PID 1
/// normalizes @var ownership and modes before exposing it to userspace. The
/// deployment tree's host ownership is inert on its read-only guest mount.
pub(crate) fn create_persistent_volume(
    deployment: &Path,
    mkfs: &Path,
    btrfs: &Path,
    output: &Path,
    trust: &RunTrust,
) -> Result<(), String> {
    create_persistent_volume_layout(deployment, mkfs, btrfs, output, VolumeLayout::Basic, trust)
        .map(|_| ())
}

enum VolumeLayout {
    Basic,
    Transactional,
    CorruptCurrent,
}

struct VolumeFixture {
    initial_id: String,
    alternate_id: String,
}

fn create_persistent_volume_layout(
    deployment: &Path,
    mkfs: &Path,
    btrfs: &Path,
    output: &Path,
    layout: VolumeLayout,
    trust: &RunTrust,
) -> Result<VolumeFixture, String> {
    let manifest = deployment.join("manifest");
    let deployment_id = crate::sha256::sha256_file(&manifest)
        .map_err(|e| format!("hash deployment manifest {}: {e}", manifest.display()))?;
    let mut payload_bytes = 0u64;
    for name in ["manifest", "bzImage", "initramfs.cpio", "root.erofs"] {
        let path = deployment.join(name);
        let bytes = fs::metadata(&path)
            .map_err(|e| format!("stat persistent fixture payload {}: {e}", path.display()))?
            .len();
        payload_bytes = payload_bytes
            .checked_add(bytes)
            .ok_or_else(|| "persistent fixture payload size overflow".to_string())?;
    }
    let copies = match layout {
        VolumeLayout::Basic => 1,
        VolumeLayout::Transactional => 3,
        VolumeLayout::CorruptCurrent => 2,
    };
    let fixture_payload_bytes = payload_bytes
        .checked_mul(copies)
        .ok_or_else(|| "persistent fixture payload size overflow".to_string())?;
    let payload_limit = PERSISTENT_VOLUME_BYTES.saturating_sub(PERSISTENT_VOLUME_HEADROOM);
    if fixture_payload_bytes > payload_limit {
        return Err(format!(
            "persistent fixture deployments are {fixture_payload_bytes} bytes, exceeding the \
             {payload_limit}-byte payload limit for the {PERSISTENT_VOLUME_BYTES}-byte volume"
        ));
    }
    let parent = output.parent().ok_or_else(|| {
        format!(
            "persistent volume output has no parent: {}",
            output.display()
        )
    })?;
    let seed = parent.join("persistent-volume-seed");
    match fs::remove_dir_all(&seed) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove stale seed {}: {error}", seed.display())),
    }
    match fs::remove_file(output) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove stale volume {}: {error}", output.display())),
    }
    let _seed_cleanup = Scratch { dir: seed.clone() };

    populate_persistent_seed(deployment, &seed, &deployment_id, trust)?;
    let alternate_id = match layout {
        VolumeLayout::Basic => deployment_id.clone(),
        VolumeLayout::Transactional => {
            let candidate = seed.join("td/incoming/candidate");
            create_bootable_candidate(deployment, &candidate, trust)?
        }
        VolumeLayout::CorruptCurrent => {
            let candidate = seed.join("td/incoming/corrupt-candidate");
            let id = create_bootable_candidate(deployment, &candidate, trust)?;
            let installed = seed.join(td_boot_protocol::DEPLOYMENTS_DIR).join(&id);
            fs::rename(&candidate, &installed).map_err(|e| {
                format!(
                    "stage corrupt-current deployment {} -> {}: {e}",
                    candidate.display(),
                    installed.display()
                )
            })?;
            let root = installed.join("root.erofs");
            let mut file = OpenOptions::new()
                .append(true)
                .open(&root)
                .map_err(|e| format!("open corrupt-current payload {}: {e}", root.display()))?;
            file.write_all(b"td-corrupt-current")
                .map_err(|e| format!("corrupt current payload {}: {e}", root.display()))?;
            replace_seed_selector(&seed, "current", &id)?;
            id
        }
    };

    // mkfs.btrfs grows this regular-file target to --byte-count; creating it
    // first keeps path and permission failures in this control plane.
    File::create(output)
        .map_err(|e| format!("create persistent volume {}: {e}", output.display()))?;
    static UUID_SEQ: AtomicU64 = AtomicU64::new(0);
    let sequence = UUID_SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff;
    let fixture_id = (u64::from(std::process::id()) << 16) | sequence;
    let fixture_uuid = format!("12345678-1234-4234-8234-{fixture_id:012x}");
    let status = Command::new(mkfs)
        .args(["--rootdir"])
        .arg(&seed)
        .args(["--subvol", "rw:@var", "--byte-count"])
        .arg(PERSISTENT_VOLUME_BYTES.to_string())
        .args(["--uuid", &fixture_uuid, "--label", "td-system"])
        .arg(output)
        .status()
        .map_err(|e| format!("spawn {}: {e}", mkfs.display()))?;
    if !status.success() {
        return Err(format!(
            "{} failed ({status}) creating {}",
            mkfs.display(),
            output.display()
        ));
    }
    fs::remove_dir_all(&seed)
        .map_err(|e| format!("remove populated seed {}: {e}", seed.display()))?;
    check_persistent_volume(btrfs, output)?;
    Ok(VolumeFixture {
        initial_id: deployment_id,
        alternate_id,
    })
}

fn copy_candidate_payload(source: &Path, destination: &Path) -> Result<(), String> {
    let mut input = File::open(source)
        .map_err(|e| format!("open candidate source {}: {e}", source.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| {
            format!(
                "create candidate payload {} from {}: {e}",
                destination.display(),
                source.display()
            )
        })?;
    std::io::copy(&mut input, &mut output).map_err(|e| {
        format!(
            "copy candidate payload {} -> {}: {e}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

/// One run's trust root, generated per run and never stored.
///
/// The public half goes into the SELECTOR initramfs — the artifact firmware
/// loads, and the rootfs `td-boot boot` is running from when it selects and
/// verifies. The private half signs every deployment that reaches the volume.
///
/// WHICH initramfs is the whole of it. The deployment's own initramfs is a
/// payload the manifest hashes and it sits on the Btrfs volume, so a key there
/// would be inside the artifact being authenticated: a hostile update source
/// supplies bundle, key and signature together and self-authenticates, which
/// is the Btrfs-volume weakening §6 forbids by name. Putting it in the selector
/// also keeps D3 intact — rotating the key changes the selector rather than any
/// deployment's initramfs digest, so a re-signed deployment keeps its id.
pub(crate) struct RunTrust {
    seed: [u8; 32],
    public: [u8; 32],
}

impl RunTrust {
    /// D4: a signing key never enters a recipe or a store path, so it is made
    /// here, host-side, from `/dev/urandom` and dropped when the run ends.
    pub(crate) fn generate() -> Result<Self, String> {
        let mut seed = [0u8; 32];
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut seed))
            .map_err(|e| format!("read /dev/urandom for a throwaway signing seed: {e}"))?;
        let public = td_engine::ed25519_sign::public_key(&seed)
            .ok_or_else(|| "derive a public key from the throwaway seed".to_string())?;
        Ok(Self { seed, public })
    }

    /// The public half in the wire format td-boot reads: lowercase hex, one
    /// trailing newline.
    pub(crate) fn trusted_key_line(&self) -> Vec<u8> {
        hex_line(&self.public)
    }

    /// Write `manifest.sig` beside a staged deployment's manifest.
    ///
    /// EVERY deployment that reaches the volume gets one, not just the ones a
    /// test swaps in. The seed deployment is what current and previous point at
    /// and what every mode but the transactional ones boots, so signing only
    /// candidates would leave the ordinary boot path unsigned — and the moment
    /// verification is fail-closed, refused.
    fn sign_deployment(&self, directory: &Path) -> Result<(), String> {
        let manifest_path = directory.join(td_boot_protocol::MANIFEST_NAME);
        let manifest = fs::read(&manifest_path)
            .map_err(|e| format!("read manifest {}: {e}", manifest_path.display()))?;
        let signature = td_engine::ed25519_sign::sign(&self.seed, &manifest)
            .ok_or_else(|| format!("sign manifest {}", manifest_path.display()))?;
        let path = directory.join(td_boot_protocol::MANIFEST_SIG_NAME);
        fs::write(&path, hex_line(&signature))
            .map_err(|e| format!("write signature {}: {e}", path.display()))
    }
}

/// Copy the verified selector initramfs into scratch and append this run's
/// trusted key to the copy, returning the path to boot.
///
/// A copy because the verified original is a content-addressed store output,
/// and because `verify_selector` has already checked it against its own
/// manifest — appending before that check would invalidate it, and appending
/// after is what makes the boot artifact this run's rather than the recipe's.
pub(crate) fn provision_selector(
    selector: &VerifiedSelector,
    destination_dir: &Path,
    trust: &RunTrust,
) -> Result<PathBuf, String> {
    let provisioned = destination_dir.join("selector-initramfs-trusted.cpio");
    match fs::remove_file(&provisioned) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "remove stale provisioned selector {}: {error}",
                provisioned.display()
            ))
        }
    }
    fs::copy(&selector.0, &provisioned).map_err(|e| {
        format!(
            "copy selector {} -> {}: {e}",
            selector.0.display(),
            provisioned.display()
        )
    })?;
    append_trusted_key(&provisioned, &trust.trusted_key_line())?;
    Ok(provisioned)
}

/// Append `key` to `initramfs` as a second, concatenated cpio archive.
///
/// The alignment is the whole of this function's correctness and is NOT
/// backstopped by the kernel. A misaligned appendix makes `do_reset` error
/// `broken padding`, and because td's initramfs is an initrd built with
/// `CONFIG_BLK_DEV_RAM` off, that error is a lone `printk` and the boot
/// CONTINUES with the key absent (`init/initramfs.c:726-733`). Nothing
/// downstream reports it, so the padding here is the only thing between a run
/// and a machine that boots without its trust root.
///
/// The directory entries are emitted rather than assumed: neither phase of
/// `build_initramfs_spec` creates `/etc`, and a missing parent is silent too —
/// `filp_open` failing is `return 0` (`init/initramfs.c:385-387`).
pub(crate) fn append_trusted_key(initramfs: &Path, key: &[u8]) -> Result<(), String> {
    use td_engine::cpio::{Entry, Kind};

    // The caller's copy came from a store output, and `fs::copy` preserves the
    // source's mode — which `copy_canonical` fixed at 0444 for a non-executable
    // file (`builder/src/main.rs:806-807`). So the copy is read-only and the
    // append below is EACCES for any non-root runner. Widened here rather than
    // at each caller because both of them copy, and because a private scratch
    // file is the only thing this is ever handed.
    let mode = fs::metadata(initramfs)
        .map_err(|e| format!("stat staged initramfs {}: {e}", initramfs.display()))?
        .permissions()
        .mode();
    fs::set_permissions(initramfs, fs::Permissions::from_mode(mode | 0o200)).map_err(|e| {
        format!(
            "make the staged initramfs {} writable: {e}",
            initramfs.display()
        )
    })?;

    let length = fs::metadata(initramfs)
        .map_err(|e| format!("stat initramfs {}: {e}", initramfs.display()))?
        .len();
    let length = usize::try_from(length)
        .map_err(|_| format!("initramfs {} is too large", initramfs.display()))?;

    let mut entries = Vec::new();
    for parent in key_path_parents() {
        entries.push(Entry { name: parent, mode: 0o755, kind: Kind::Directory });
    }
    entries.push(Entry {
        name: td_boot_protocol::TRUSTED_KEY_PATH,
        mode: 0o644,
        kind: Kind::File(key),
    });

    let mut appendix = vec![0u8; td_engine::cpio::alignment_padding(length)];
    appendix.extend_from_slice(&td_engine::cpio::build(&entries)?);

    OpenOptions::new()
        .append(true)
        .open(initramfs)
        .and_then(|mut f| f.write_all(&appendix))
        .map_err(|e| format!("append key archive to {}: {e}", initramfs.display()))
}

/// Every proper directory prefix of `TRUSTED_KEY_PATH`, shallowest first.
///
/// Derived from the path rather than written beside it, so moving the key
/// cannot leave the parent list behind — which the kernel would report by
/// creating nothing at all.
fn key_path_parents() -> Vec<&'static str> {
    let path = td_boot_protocol::TRUSTED_KEY_PATH;
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(slash) = path.get(at..).and_then(|rest| rest.find('/')) {
        at = at.saturating_add(slash);
        if let Some(prefix) = path.get(..at) {
            out.push(prefix);
        }
        at = at.saturating_add(1);
    }
    out
}

fn create_bootable_candidate(
    deployment: &Path,
    destination: &Path,
    trust: &RunTrust,
) -> Result<String, String> {
    fs::create_dir_all(destination)
        .map_err(|e| format!("create candidate directory {}: {e}", destination.display()))?;
    for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
        copy_candidate_payload(&deployment.join(name), &destination.join(name))?;
    }
    let initramfs = destination.join("initramfs.cpio");
    let mut padding = OpenOptions::new()
        .append(true)
        .open(&initramfs)
        .map_err(|e| format!("open candidate initramfs {}: {e}", initramfs.display()))?;
    padding
        .write_all(&[0u8; 4])
        .map_err(|e| format!("pad candidate initramfs {}: {e}", initramfs.display()))?;

    let kernel_digest = crate::sha256::sha256_file(&destination.join("bzImage"))
        .map_err(|e| format!("hash candidate bzImage: {e}"))?;
    let initramfs_digest = crate::sha256::sha256_file(&initramfs)
        .map_err(|e| format!("hash candidate initramfs: {e}"))?;
    let root_digest = crate::sha256::sha256_file(&destination.join("root.erofs"))
        .map_err(|e| format!("hash candidate root.erofs: {e}"))?;
    let manifest = format!(
        "td-deployment-v1\n{kernel_digest}  bzImage\n{initramfs_digest}  initramfs.cpio\n\
         {root_digest}  root.erofs\n"
    );
    fs::write(destination.join("manifest"), manifest)
        .map_err(|e| format!("write candidate manifest {}: {e}", destination.display()))?;
    trust.sign_deployment(destination)?;
    verify_deployment(destination)?;
    crate::sha256::sha256_file(&destination.join("manifest"))
        .map_err(|e| format!("hash candidate manifest {}: {e}", destination.display()))
}

/// Lowercase hex plus a trailing newline — the wire format DESIGN.md §6 fixes
/// for both the key and the signature, and the one td-boot's `decode_hex` trims
/// back off. Lowercase is the documented half a permissive reader would hide.
fn hex_line(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().saturating_mul(2).saturating_add(1));
    for byte in bytes {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 0xf));
    }
    out.push(b'\n');
    out
}

fn hex_nibble(value: u8) -> u8 {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    *DIGITS.get(value as usize).unwrap_or(&b'0')
}

fn replace_seed_selector(seed: &Path, slot: &str, deployment_id: &str) -> Result<(), String> {
    let selector = seed.join(td_boot_protocol::BOOT_DIR).join(slot);
    fs::remove_file(&selector)
        .map_err(|e| format!("remove fixture {slot} selector {}: {e}", selector.display()))?;
    symlink(
        format!("{}{deployment_id}", td_boot_protocol::SELECTOR_PREFIX),
        &selector,
    )
    .map_err(|e| {
        format!(
            "replace fixture {slot} selector {}: {e}",
            selector.display()
        )
    })
}

fn populate_persistent_seed(
    deployment: &Path,
    seed: &Path,
    deployment_id: &str,
    trust: &RunTrust,
) -> Result<(), String> {
    let installed = seed
        .join(td_boot_protocol::DEPLOYMENTS_DIR)
        .join(deployment_id);
    fs::create_dir_all(&installed)
        .map_err(|e| format!("create deployment dir {}: {e}", installed.display()))?;
    fs::create_dir_all(seed.join(td_boot_protocol::BOOT_DIR))
        .map_err(|e| format!("create boot selector dir: {e}"))?;
    fs::create_dir_all(seed.join("@var")).map_err(|e| format!("create @var seed dir: {e}"))?;
    for name in ["bzImage", "initramfs.cpio", "root.erofs", "manifest"] {
        let source = deployment.join(name);
        let target = installed.join(name);
        link_or_copy(&source, &target)?;
    }
    // The staged copy is signed, not the recipe output: the source is a
    // read-only store path, and `link_or_copy` hard-links when it can, so a
    // signature written through it would land in the store.
    trust.sign_deployment(&installed)?;
    stage_volume_trust_roots(seed, trust)?;
    for slot in ["current", "previous"] {
        let link = seed.join(td_boot_protocol::BOOT_DIR).join(slot);
        symlink(
            format!("{}{deployment_id}", td_boot_protocol::SELECTOR_PREFIX),
            &link,
        )
        .map_err(|e| format!("create {} selector {}: {e}", slot, link.display()))?;
    }
    Ok(())
}

/// The two keys a booted machine's update path is given, written where
/// `td-install` would put the first (DESIGN §10 item 10a).
///
/// The wrong one is what makes the oracle's install pass mean anything: the
/// candidate is signed by the run key whether or not td-boot is told to check
/// it, so an ignored `trusted-key` argument would leave every existing
/// assertion green. It is a REAL key rather than a corrupt file, so the refusal
/// it earns is a signature that does not verify and not a key that will not
/// parse — those are different failures and only the first is the one under
/// test.
///
/// Mode 0644 explicitly: `mkfs.btrfs --rootdir` copies a mode in verbatim, so
/// an ambient umask would otherwise decide what the fixture's trust root looks
/// like — the pin `td-install` makes for the same reason.
fn stage_volume_trust_roots(seed: &Path, trust: &RunTrust) -> Result<(), String> {
    let decoy = RunTrust::generate()?;
    if decoy.trusted_key_line() == trust.trusted_key_line() {
        return Err("the decoy trust root matched the run key".to_string());
    }
    for (relative, line) in [
        (td_boot_protocol::VOLUME_TRUSTED_KEY, trust.trusted_key_line()),
        (
            td_recipe::ladder::DEPLOY_WRONG_KEY,
            decoy.trusted_key_line(),
        ),
    ] {
        let path = seed.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create fixture trust root dir {}: {e}", parent.display()))?;
        }
        fs::write(&path, &line)
            .map_err(|e| format!("write fixture trust root {}: {e}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("chmod fixture trust root {}: {e}", path.display()))?;
    }
    Ok(())
}

fn link_or_copy(source: &Path, target: &Path) -> Result<(), String> {
    if fs::hard_link(source, target).is_ok() {
        return Ok(());
    }
    fs::copy(source, target).map_err(|e| {
        format!(
            "stage deployment payload {} -> {}: {e}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

pub(crate) fn check_persistent_volume(btrfs: &Path, output: &Path) -> Result<(), String> {
    let status = Command::new(btrfs)
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(output)
        .status()
        .map_err(|e| format!("spawn {} check: {e}", btrfs.display()))?;
    if !status.success() {
        return Err(format!(
            "{} check failed ({status}) for {}",
            btrfs.display(),
            output.display()
        ));
    }
    Ok(())
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

fn guest_success_wait_secs(timeout: Duration) -> u64 {
    timeout
        .as_secs()
        .saturating_sub(GUEST_WAIT_MARGIN_SECS)
        .max(1)
}

fn autotest_wait_token(timeout: Duration) -> String {
    format!(
        "{}{}",
        td_recipe::ladder::BOOT_SUCCESS_WAIT_CMDLINE_PREFIX,
        guest_success_wait_secs(timeout)
    )
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

/// The per-mode boot parameters — everything that differs between the diskless kernel
/// boot, the erofs-probe boot, and the two-stage system boot. Grouped into one struct so
/// `boot` keeps a small, self-documenting signature: named fields at the call site
/// (`kill_on_marker: false`) beat positional bools/strings.
struct BootDisk<'a> {
    path: &'a Path,
    read_only: bool,
}

struct BootPlan<'a> {
    /// A raw image to attach over virtio-blk (/dev/vda), or none for diskless.
    /// Probe EROFS images are read-only; system volumes allow @var writes.
    disk: Option<BootDisk<'a>>,
    /// Guest RAM in MiB (qemu `-m`). Diskless/probe boots use "256"; selector and
    /// kexec boots use "512" so both kernels and initramfses fit at the handoff.
    mem: &'a str,
    /// The ttyS0 line whose appearance means the boot reached its target state; lets the
    /// boot modes key on different lines (userland vs. read-only-erofs vs. greeter).
    target_marker: &'a str,
    /// `true`: kill qemu the instant the marker appears — the marker IS the end (the
    /// diskless and erofs-probe modes). `false`: latch the marker but let the guest run to
    /// its OWN power-off, so `exited_clean` records a clean shutdown (the two-stage
    /// `qemu-boot-system` mode, whose success includes "exit powers off"). Either way the
    /// boot is bounded by the wall-clock ceiling and the flood guard.
    kill_on_marker: bool,
    /// Extra kernel cmdline appended after the base (empty for none) — `qemu-boot-system`
    /// passes the autotest token so the greeter self-exits headlessly.
    extra_append: &'a str,
    /// `true`: attach a qemu user-mode (SLIRP) NIC on virtio-net-pci instead of `-nic
    /// none`, so the guest's td-netd can DHCP, resolve, and reach a host — the
    /// `qemu-boot-net` mode. `false` (every other mode): no network, hermetic and offline.
    user_net: bool,
}

/// Boot `bzImage` + `initramfs` under qemu per `plan` (see `BootPlan`), capturing ttyS0 to
/// a FILE (never a pipe: a pipe would deadlock if the kernel log outran the buffer while we
/// poll). The console is read INCREMENTALLY into a bounded rolling buffer — decoded lossily
/// so a non-UTF-8 serial byte can't empty the capture, and trimmed to the last CAP bytes so
/// a flooding boot can't balloon memory or make the poll quadratic.
fn boot(
    qemu: &str,
    bzimage: &Path,
    initramfs: &Path,
    plan: BootPlan<'_>,
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
    let diag =
        File::create(&diag_path).map_err(|e| format!("create {}: {e}", diag_path.display()))?;
    let diag_err = diag
        .try_clone()
        .map_err(|e| format!("clone diag fd: {e}"))?;

    // -M pc + TCG: no KVM needed (the sandbox denies /dev/kvm and the host may not
    //   expose it either; TCG always works and a tiny kernel boots fast).
    // -serial file:<console>: route ttyS0 straight to a file — deterministic, no
    //   tty/stdio games (unlike -nographic, which wants a terminal on stdin).
    // -display none / -monitor none: fully headless. The attached virtio-vga still
    //   exercises fbdev and the software compositor; only its host display is hidden.
    // -device virtio-tablet-pci: an ABSOLUTE pointer. Headless it delivers no motion
    //   — there is no host cursor to follow — but it still ENUMERATES, and that is
    //   the half this proves: the guest binds it, evdev publishes the node, and the
    //   compositor's EVIOCGABS gets a span back. The oracle latches the marker that
    //   answer produces, which is the only place a real device answering is visible
    //   (the unit gate has no absolute device to ask).
    // Networking is attached conditionally below (a user-mode NIC for qemu-boot-net,
    // else `-nic none`) — qemu's default is an implicit user-mode NIC, so every mode
    // sets one explicitly.
    // -no-user-config: ignore the host's qemu config files for a hermetic run.
    // -no-reboot: BusyBox init ultimately issues reboot(2); qemu exits on the guest
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
    // Base cmdline: ttyS0 console, panic-reboots (=> qemu exits), and rdinit=/init runs
    // the stage-1 (or single-stage) init from the initramfs. `extra_append`, when set,
    // appends caller cmdline (qemu-boot-system's autotest token that makes the greeter
    // self-exit) after a single space.
    let base_append = "console=ttyS0 panic=-1 rdinit=/init";
    let append = if plan.extra_append.is_empty() {
        base_append.to_string()
    } else {
        format!("{base_append} {}", plan.extra_append)
    };
    let mut cmd = Command::new(qemu);
    cmd.args(["-M", "pc", "-accel", "tcg", "-m", plan.mem, "-no-reboot"])
        .args(["-display", "none", "-monitor", "none"])
        .args(["-no-user-config", "-vga", "none"])
        .args(["-device", "virtio-vga"])
        .args(["-device", "virtio-tablet-pci"])
        .args(["-serial", &serial])
        .arg("-kernel")
        .arg(bzimage)
        .arg("-initrd")
        .arg(initramfs)
        .args(["-append", &append]);
    // Networking: either a user-mode NIC (qemu-boot-net) or none (every other mode).
    // SLIRP's user net provides DHCP (10.0.2.15/24, gw 10.0.2.2) and a DNS forwarder
    // (10.0.2.3), so the guest's td-netd can DHCP-configure, resolve, and reach a host
    // with no host network config. virtio-net-pci is the NIC the guest's VIRTIO_NET
    // driver binds; the guest autodetects it (eth0) and brings it up.
    if plan.user_net {
        cmd.args(["-netdev", "user,id=net0"]);
        cmd.args(["-device", "virtio-net-pci,netdev=net0"]);
    } else {
        // -nic none: hermetic, offline; qemu's default is a user-mode NIC, so disable it.
        cmd.args(["-nic", "none"]);
    }
    // Optional raw disk: if=none defines the backing store and a separate
    // virtio-blk-pci device attaches it as /dev/vda.
    // drive_arg comma-doubles the image path so a scratch dir with a literal comma in
    // its path can't be misparsed as an extra -drive key=value pair.
    if let Some(disk) = plan.disk {
        cmd.arg("-drive").arg(drive_arg(disk.path, disk.read_only));
        cmd.args(["-device", "virtio-blk-pci,drive=disk0"]);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(diag))
        .stderr(Stdio::from(diag_err))
        .spawn()
        .map_err(|e| format!("spawn {qemu}: {e}"))?;

    let timeout = boot_timeout();
    let start = Instant::now();
    let marker_bytes = plan.target_marker.as_bytes();
    let mut console_file: Option<File> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut evidence = ConsoleEvidence::default();
    let mut end;
    loop {
        if let Err(error) = drain_console(
            &console_path,
            &mut console_file,
            &mut buf,
            marker_bytes,
            plan.kill_on_marker,
            &mut evidence,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            let console = String::from_utf8_lossy(&buf);
            return Err(format!(
                "{error}. Last serial output:\n{}",
                tail(&console, 80)
            ));
        }
        if evidence.target && plan.kill_on_marker {
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

    // Drain all final bytes qemu flushed before it was reaped. A marker-killed
    // mode succeeds if its target appears here; persistent modes retain the exit,
    // timeout, or flood reason because reaching the greeter is not their finish.
    let target_before_final_drain = evidence.target;
    let final_on_disk = fs::metadata(&console_path).map(|m| m.len()).unwrap_or(0)
        + fs::metadata(&diag_path).map(|m| m.len()).unwrap_or(0);
    let final_flooded = final_on_disk > MAX_CONSOLE_BYTES;
    let final_drain_error = if final_flooded {
        end = EndReason::Flooded(final_on_disk);
        None
    } else {
        drain_console_to_eof(
            &console_path,
            &mut console_file,
            &mut buf,
            marker_bytes,
            &mut evidence,
        )
        .err()
    };

    // Capture a clean self-exit after the final flood check but before a
    // marker-only mode realigns its diagnostic end reason.
    let exited_clean = matches!(&end, EndReason::QemuExited(status) if status.success());
    if plan.kill_on_marker && !target_before_final_drain && evidence.target {
        end = EndReason::MarkerSeen;
    }

    let mut console = if final_flooded {
        read_tail(&console_path, CAP).unwrap_or_else(|_| String::from_utf8_lossy(&buf).into_owned())
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    };
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

    let reason = format_end_reason(end, evidence.target);
    if final_flooded {
        return Err(format!(
            "{reason}. Last serial output:\n{}",
            tail(&console, 80)
        ));
    }
    if let Some(error) = final_drain_error {
        return Err(format!(
            "{error}. Last serial output:\n{}",
            tail(&console, 80)
        ));
    }
    Ok(BootResult {
        evidence,
        exited_clean,
        reason,
        console,
        elapsed: start.elapsed(),
    })
}

fn format_end_reason(end: EndReason, target_seen: bool) -> String {
    match end {
        EndReason::MarkerSeen => "the marker was seen".to_string(),
        EndReason::QemuExited(status) if target_seen => {
            format!("qemu exited on its own after the marker ({status})")
        }
        EndReason::QemuExited(status) => {
            format!("qemu exited on its own before the marker ({status})")
        }
        EndReason::TimedOut(secs) if target_seen => {
            format!("qemu did not finish within the {secs}s ceiling after the marker; it was killed")
        }
        EndReason::TimedOut(secs) => {
            format!("no marker within the {secs}s ceiling; qemu was killed")
        }
        EndReason::Flooded(bytes) if target_seen => format!(
            "console+diagnostic output flooded past the {MAX_CONSOLE_BYTES}-byte on-disk ceiling \
             after the marker ({bytes} bytes across console.log + diag.log); qemu was killed"
        ),
        EndReason::Flooded(bytes) => format!(
            "console+diagnostic output flooded past the {MAX_CONSOLE_BYTES}-byte on-disk ceiling \
             without reaching the marker ({bytes} bytes across console.log + diag.log); qemu was killed"
        ),
    }
}

/// Read whatever new bytes are available on the console file into `buf`, opening
/// it lazily (qemu creates it after spawn). Keeps only the last CAP bytes and
/// latches protocol evidence before trimming so early boot markers remain
/// authoritative. Bounded by DRAIN_BUDGET per call so a flooding guest can't
/// starve the outer deadline check. Marker-killed modes may return on a newly
/// seen target; persistent and final drains consume the whole available budget.
enum DrainProgress {
    Complete,
    More,
}

fn drain_console(
    path: &Path,
    file: &mut Option<File>,
    buf: &mut Vec<u8>,
    marker: &[u8],
    stop_on_new_target: bool,
    evidence: &mut ConsoleEvidence,
) -> Result<DrainProgress, String> {
    if file.is_none() {
        match File::open(path) {
            Ok(opened) => *file = Some(opened),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DrainProgress::Complete);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                return Ok(DrainProgress::More);
            }
            Err(error) => {
                return Err(format!("open qemu console {}: {error}", path.display()));
            }
        }
    }
    let target_was_seen = evidence.target;
    let overlap = evidence_marker_max_len(marker).saturating_sub(1);
    let Some(f) = file.as_mut() else {
        return Ok(DrainProgress::Complete);
    };
    let mut chunk = [0u8; 8192];
    let mut drained = 0usize;
    while drained < DRAIN_BUDGET {
        match f.read(&mut chunk) {
            Ok(0) => return Ok(DrainProgress::Complete),
            Ok(n) => {
                if let Some(slice) = chunk.get(..n) {
                    let prior_len = buf.len();
                    buf.extend_from_slice(slice);
                    drained += n;
                    let scan_from = prior_len.saturating_sub(overlap);
                    let scan = buf.get(scan_from..).unwrap_or(buf.as_slice());
                    latch_console_evidence(evidence, scan, marker);
                    if buf.len() > CAP {
                        let drop = buf.len() - CAP;
                        buf.drain(..drop);
                    }
                    if stop_on_new_target && !target_was_seen && evidence.target {
                        // Return once when the target first appears so marker-killed
                        // modes react immediately. Persistent modes resume draining
                        // normally on the next poll.
                        return Ok(DrainProgress::More);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                return Ok(DrainProgress::More);
            }
            Err(error) => {
                return Err(format!("read qemu console {}: {error}", path.display()));
            }
        }
    }
    Ok(DrainProgress::More)
}

fn drain_console_to_eof(
    path: &Path,
    file: &mut Option<File>,
    buf: &mut Vec<u8>,
    marker: &[u8],
    evidence: &mut ConsoleEvidence,
) -> Result<(), String> {
    for _ in 0..FINAL_DRAIN_PASSES {
        match drain_console(path, file, buf, marker, false, evidence)? {
            DrainProgress::Complete => return Ok(()),
            DrainProgress::More => {}
        }
    }
    Err(format!(
        "qemu console {} did not reach EOF within {FINAL_DRAIN_PASSES} bounded final-drain passes",
        path.display()
    ))
}

fn evidence_marker_max_len(target: &[u8]) -> usize {
    [
        target.len(),
        GREETER_MARKER.len(),
        td_boot_protocol::CURRENT_REJECTED_MARKER.len(),
        td_boot_protocol::ATTEMPT_CONSUMED_MARKER.len(),
        td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER.len(),
        BOOKKEEPING_UNAVAILABLE_MARKER.len(),
        td_boot_protocol::SELECTED_CURRENT_MARKER.len() + 1 + 64,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER.len() + 1 + 64,
        SYSTEM_ROOT_RO_MARKER.len(),
        SYSTEM_ETC_RO_MARKER.len(),
        SYSTEM_ETC_MUTABLE_MARKER.len(),
        TD_FIRSTBOOT_NEW_MARKER.len(),
        TD_FIRSTBOOT_STABLE_MARKER.len(),
        TD_FIRSTBOOT_HOST_KEY_PREFIX.len() + HOST_KEY_MAX,
        SYSTEM_STATE_WRITABLE_MARKER.len(),
        SYSTEM_STATE_OWNER_MARKER.len(),
        UUTILS_RUNTIME_MARKER.len(),
        RIPGREP_FD_RUNTIME_MARKER.len(),
        SSHD_MARKER.len(),
        TD_UTIL_RUNTIME_MARKER.len(),
        TD_INIT_RUNTIME_MARKER.len(),
        TD_LOGIN_RUNTIME_MARKER.len(),
        TD_WAYLAND_RUNTIME_MARKER.len(),
        TD_POINTER_ABSOLUTE_MARKER.len(),
        TD_TERM_RUNTIME_MARKER.len(),
        SYSTEM_PERSIST_WRITE_MARKER.len(),
        SYSTEM_PERSIST_READ_MARKER.len(),
        SYSTEM_BOOT_SUCCESS_MARKER.len(),
        SYSTEM_DEPLOY_INSTALL_MARKER.len(),
        SYSTEM_SHUTDOWN_MARKER.len(),
        SYSTEM_NET_UP_MARKER.len(),
        SYSTEM_NET_RESOLVE_MARKER.len(),
        SYSTEM_NET_REACH_MARKER.len(),
        KEXEC_STAGE1_MARKER.len(),
        "Kernel panic".len(),
    ]
    .into_iter()
    .fold(0, usize::max)
}

fn latch_console_evidence(evidence: &mut ConsoleEvidence, buf: &[u8], target: &[u8]) {
    latch_marker(&mut evidence.target, buf, target);
    latch_marker(&mut evidence.greeter, buf, GREETER_MARKER.as_bytes());
    latch_marker(
        &mut evidence.current_rejected,
        buf,
        td_boot_protocol::CURRENT_REJECTED_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.attempt_consumed,
        buf,
        td_boot_protocol::ATTEMPT_CONSUMED_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.attempts_exhausted,
        buf,
        td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.bookkeeping_unavailable,
        buf,
        BOOKKEEPING_UNAVAILABLE_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.selected_current,
        buf,
        td_boot_protocol::SELECTED_CURRENT_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.selected_previous,
        buf,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER.as_bytes(),
    );
    latch_selection_id(
        &mut evidence.selected_current_id,
        buf,
        td_boot_protocol::SELECTED_CURRENT_MARKER.as_bytes(),
    );
    latch_selection_id(
        &mut evidence.selected_previous_id,
        buf,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.root_read_only,
        buf,
        SYSTEM_ROOT_RO_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.etc_read_only,
        buf,
        SYSTEM_ETC_RO_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.etc_mutable,
        buf,
        SYSTEM_ETC_MUTABLE_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.firstboot_new,
        buf,
        TD_FIRSTBOOT_NEW_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.firstboot_stable,
        buf,
        TD_FIRSTBOOT_STABLE_MARKER.as_bytes(),
    );
    latch_token(
        &mut evidence.host_key,
        buf,
        TD_FIRSTBOOT_HOST_KEY_PREFIX.as_bytes(),
    );
    latch_marker(
        &mut evidence.state_writable,
        buf,
        SYSTEM_STATE_WRITABLE_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.state_owner,
        buf,
        SYSTEM_STATE_OWNER_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.uutils_runtime,
        buf,
        UUTILS_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.ripgrep_fd_runtime,
        buf,
        RIPGREP_FD_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(&mut evidence.sshd, buf, SSHD_MARKER.as_bytes());
    latch_marker(
        &mut evidence.td_util_runtime,
        buf,
        TD_UTIL_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.td_txt_runtime,
        buf,
        TD_TXT_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.td_init_runtime,
        buf,
        TD_INIT_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.td_login_runtime,
        buf,
        TD_LOGIN_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.td_wayland_runtime,
        buf,
        TD_WAYLAND_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.td_pointer_absolute,
        buf,
        TD_POINTER_ABSOLUTE_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.td_term_runtime,
        buf,
        TD_TERM_RUNTIME_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.persist_write,
        buf,
        SYSTEM_PERSIST_WRITE_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.persist_read,
        buf,
        SYSTEM_PERSIST_READ_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.boot_success,
        buf,
        SYSTEM_BOOT_SUCCESS_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.deploy_install,
        buf,
        SYSTEM_DEPLOY_INSTALL_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.shutdown,
        buf,
        SYSTEM_SHUTDOWN_MARKER.as_bytes(),
    );
    latch_marker(&mut evidence.net_up, buf, SYSTEM_NET_UP_MARKER.as_bytes());
    latch_marker(
        &mut evidence.net_resolve,
        buf,
        SYSTEM_NET_RESOLVE_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.net_reach,
        buf,
        SYSTEM_NET_REACH_MARKER.as_bytes(),
    );
    latch_marker(
        &mut evidence.kexec_stage1,
        buf,
        KEXEC_STAGE1_MARKER.as_bytes(),
    );
    latch_marker(&mut evidence.kernel_panic, buf, b"Kernel panic");
}

fn latch_marker(found: &mut bool, haystack: &[u8], marker: &[u8]) {
    if !*found {
        *found = contains(haystack, marker);
    }
}

/// Longest token `latch_token` will accept. An ed25519 SHA-256 fingerprint is
/// `SHA256:` plus 43 base64 characters; the ceiling is generous enough for another
/// hash without letting a console line of `x`s become an unbounded String.
const HOST_KEY_MAX: usize = 64;

/// Latch the graphic-character token that follows `prefix` — used for the SSH
/// host-key fingerprint, which is variable-length base64 rather than the
/// fixed-width hex deployment ids `latch_selection_id` reads.
///
/// Requires the token to be TERMINATED within the window: the console is drained
/// incrementally, so a token still being written would otherwise latch truncated
/// and then compare unequal against the same key on the next boot. The scan window
/// carries `evidence_marker_max_len` bytes of overlap (which counts this prefix plus
/// `HOST_KEY_MAX`), so a line skipped for that reason is re-examined intact.
fn latch_token(found: &mut Option<String>, haystack: &[u8], prefix: &[u8]) {
    if found.is_some() {
        return;
    }
    for start in 0..haystack.len() {
        let Some(after_prefix) = start.checked_add(prefix.len()) else {
            return;
        };
        if haystack.get(start..after_prefix) != Some(prefix) {
            continue;
        }
        let mut end = after_prefix;
        while let Some(byte) = haystack.get(end) {
            if !byte.is_ascii_graphic() || end.saturating_sub(after_prefix) >= HOST_KEY_MAX {
                break;
            }
            let Some(next) = end.checked_add(1) else {
                return;
            };
            end = next;
        }
        // Only a token followed by a real terminator counts. Anything else is
        // either still being written (its newline has not arrived), or longer than
        // the ceiling — and latching a PREFIX of a fingerprint would compare
        // unequal against the same key next boot, reporting a rotation that did
        // not happen.
        if end == after_prefix || !matches!(haystack.get(end), Some(b) if !b.is_ascii_graphic()) {
            continue;
        }
        if let Some(Ok(token)) = haystack.get(after_prefix..end).map(std::str::from_utf8) {
            *found = Some(token.to_string());
            return;
        }
    }
}

fn latch_selection_id(found: &mut Option<String>, haystack: &[u8], marker: &[u8]) {
    if found.is_some() {
        return;
    }
    for start in 0..haystack.len() {
        let Some(after_marker) = start.checked_add(marker.len()) else {
            return;
        };
        if haystack.get(start..after_marker) != Some(marker) {
            continue;
        }
        let Some(id_start) = after_marker.checked_add(1) else {
            return;
        };
        if haystack.get(after_marker) != Some(&b' ') {
            continue;
        }
        let Some(id_end) = id_start.checked_add(64) else {
            return;
        };
        let Some(id) = haystack.get(id_start..id_end) else {
            continue;
        };
        if !id
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            continue;
        }
        if let Ok(text) = std::str::from_utf8(id) {
            *found = Some(text.to_string());
            return;
        }
    }
}

/// Byte-substring search — marker detection without a UTF-8 decode, so a non-UTF-8
/// serial byte can neither hide the marker nor empty the capture.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// The `-drive` value attaching `disk` as a raw backing store with the `disk0`
/// id the `virtio-blk-pci` device references. `if=none` keeps qemu from
/// auto-attaching it to an implicit controller so only the explicit -device wires it.
///
/// qemu splits `-drive`'s key=value pairs on commas, so a literal comma in the image
/// PATH (e.g. a repo cloned under `/home/user/code,td/…`) must be doubled (`,,`) or
/// qemu would misparse the path tail as a spurious extra key. Only the path's commas
/// are doubled — the option-separator commas in the fixed prefix stay single. Built
/// byte-wise off the raw path bytes and returned as an `OsString` so a non-UTF-8 path
/// survives without a lossy round-trip. Shared with the interactive `run` tool
/// (checks/run.rs), which attaches the same persistent volume over virtio-blk.
pub(crate) fn drive_arg(disk: &Path, read_only: bool) -> OsString {
    let mut out = OsString::from("if=none,format=raw,id=disk0");
    if read_only {
        out.push(",readonly=on");
    }
    out.push(",file=");
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
        assert!(contains(
            b"boot log: TD-USERLAND-OK done",
            MARKER.as_bytes()
        ));
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
        let arg = drive_arg(Path::new("/scratch/erofs-probe.img"), true);
        let s = arg.to_string_lossy();
        assert!(s.contains("id=disk0"), "missing id: {s}");
        // Read-only + raw + the exact backing file, and if=none (no implicit controller).
        assert!(s.contains("readonly=on"), "not read-only: {s}");
        assert!(s.contains("format=raw"), "not raw: {s}");
        assert!(s.contains("if=none"), "not if=none: {s}");
        // A comma-free path is passed through verbatim after file=.
        assert!(
            s.ends_with("file=/scratch/erofs-probe.img"),
            "wrong file: {s}"
        );
    }

    #[test]
    fn drive_arg_doubles_only_the_image_path_commas() {
        // A repo/scratch path containing a literal comma must not break -drive's
        // key=value parse: qemu wants such commas doubled. Only the PATH's commas are
        // doubled; the fixed option-separator commas in the prefix stay single.
        let arg = drive_arg(Path::new("/sc,ratch/erofs,probe.img"), true);
        let s = arg.to_string_lossy();
        assert_eq!(
            s, "if=none,format=raw,id=disk0,readonly=on,file=/sc,,ratch/erofs,,probe.img",
            "path commas not doubled (or prefix separators mangled): {s}"
        );
    }

    #[test]
    fn persistent_drive_arg_allows_guest_writes() {
        let arg = drive_arg(Path::new("/scratch/system.btrfs"), false);
        let s = arg.to_string_lossy();
        assert_eq!(s, "if=none,format=raw,id=disk0,file=/scratch/system.btrfs");
        assert!(!s.contains("readonly"));
    }

    #[test]
    fn deployment_manifest_verifies_payloads_and_rejects_tampering() {
        let seq = AtomicU64::new(2000);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        for (name, bytes) in [
            ("bzImage", b"kernel".as_slice()),
            ("initramfs.cpio", b"initramfs".as_slice()),
            ("root.erofs", b"root".as_slice()),
        ] {
            fs::write(dir.join(name), bytes).unwrap();
        }
        let mut manifest = String::from("td-deployment-v1\n");
        for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
            let digest = crate::sha256::sha256_file(&dir.join(name)).unwrap();
            manifest.push_str(&format!("{digest}  {name}\n"));
        }
        fs::write(dir.join("manifest"), manifest).unwrap();

        let (kernel, initramfs, root) = verify_deployment(&dir).unwrap();
        assert_eq!(kernel, dir.join("bzImage"));
        assert_eq!(initramfs, dir.join("initramfs.cpio"));
        assert_eq!(root, dir.join("root.erofs"));

        fs::write(dir.join("root.erofs"), b"tampered").unwrap();
        let error = verify_deployment(&dir).unwrap_err();
        assert!(error.contains("hash mismatch for root.erofs"), "{error}");
    }

    #[test]
    fn candidate_changes_identity_and_remains_manifest_valid() {
        let seq = AtomicU64::new(2050);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        let deployment = dir.join("deployment");
        fs::create_dir(&deployment).unwrap();
        for (name, bytes) in [
            ("bzImage", b"kernel".as_slice()),
            ("initramfs.cpio", b"070701initramfs".as_slice()),
            ("root.erofs", b"root".as_slice()),
        ] {
            fs::write(deployment.join(name), bytes).unwrap();
        }
        let mut manifest = String::from("td-deployment-v1\n");
        for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
            let digest = crate::sha256::sha256_file(&deployment.join(name)).unwrap();
            manifest.push_str(&format!("{digest}  {name}\n"));
        }
        fs::write(deployment.join("manifest"), &manifest).unwrap();
        let initial_id = crate::sha256::sha256_file(&deployment.join("manifest")).unwrap();

        let candidate = dir.join("incoming/candidate");
        let trust = RunTrust::generate().unwrap();
        let candidate_id = create_bootable_candidate(&deployment, &candidate, &trust).unwrap();
        assert_ne!(candidate_id, initial_id);
        assert_eq!(
            crate::sha256::sha256_file(&candidate.join("manifest")).unwrap(),
            candidate_id
        );
        verify_deployment(&candidate).unwrap();
        assert!(fs::read(candidate.join("initramfs.cpio"))
            .unwrap()
            .ends_with(&[0u8; 4]));

        // The candidate's IDENTITY must not depend on the key, or `run_system`
        // — which recreates this fixture and requires the id to be unchanged —
        // fails every run. A second trust root signs differently and names the
        // same deployment.
        let other_trust = RunTrust::generate().unwrap();
        let resigned = dir.join("incoming/resigned");
        let resigned_id =
            create_bootable_candidate(&deployment, &resigned, &other_trust).unwrap();
        assert_eq!(
            resigned_id, candidate_id,
            "D3: re-signing under another key must not change the deployment id"
        );
        assert_ne!(
            fs::read(resigned.join("manifest.sig")).unwrap(),
            fs::read(candidate.join("manifest.sig")).unwrap(),
            "a different key must produce a different signature"
        );
    }

    /// The signature the harness writes must verify under the key it would put
    /// in the selector — through `engine/src/ed25519.rs`, the same file td-boot
    /// `#[path]`-includes and a DIFFERENT module from the `ed25519_sign` that
    /// signed. So this checks the mechanism rather than the signer agreeing
    /// with itself.
    #[test]
    fn every_staged_deployment_is_signed_by_the_runs_key() {
        let seq = AtomicU64::new(2060);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        let deployment = dir.join("deployment");
        fs::create_dir(&deployment).unwrap();
        for (name, bytes) in [
            ("bzImage", b"kernel".as_slice()),
            ("initramfs.cpio", b"initramfs".as_slice()),
            ("root.erofs", b"root".as_slice()),
        ] {
            fs::write(deployment.join(name), bytes).unwrap();
        }
        let mut manifest = String::from("td-deployment-v1\n");
        for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
            let digest = crate::sha256::sha256_file(&deployment.join(name)).unwrap();
            manifest.push_str(&format!("{digest}  {name}\n"));
        }
        fs::write(deployment.join("manifest"), &manifest).unwrap();
        let id = crate::sha256::sha256_file(&deployment.join("manifest")).unwrap();

        let trust = RunTrust::generate().unwrap();
        let seed = dir.join("seed");
        populate_persistent_seed(&deployment, &seed, &id, &trust).unwrap();

        // The SEED deployment, not just a candidate: this is what current and
        // previous point at and what every mode but the transactional ones
        // boots. Signing only candidates would leave the ordinary boot path
        // unsigned, and fail-closed verification would then refuse it.
        let installed = seed.join(td_boot_protocol::DEPLOYMENTS_DIR).join(&id);
        let signature =
            decode_hex_fixture::<64>(&fs::read(installed.join("manifest.sig")).unwrap());
        let staged = fs::read(installed.join("manifest")).unwrap();
        assert!(
            td_engine::ed25519::verify(&trust.public, &staged, &signature),
            "the run's key must verify the seed deployment's signature"
        );

        // Negative controls: a verifier returning true unconditionally passes
        // the line above.
        let mut tampered = staged.clone();
        tampered[0] ^= 1;
        assert!(!td_engine::ed25519::verify(&trust.public, &tampered, &signature));
        let stranger = RunTrust::generate().unwrap();
        assert!(
            !td_engine::ed25519::verify(&stranger.public, &staged, &signature),
            "another run's key must not authenticate this run's deployment"
        );

        // The signature must NOT be written back through the hard link into the
        // source deployment, which in a real run is a read-only store path.
        assert!(
            !deployment.join("manifest.sig").exists(),
            "the source deployment must be left untouched"
        );

        // Both trust roots, checked HERE because no gate boots the VM that uses
        // them: without this, deleting `stage_volume_trust_roots` outright would
        // leave the whole gate green.
        let real = fs::read(seed.join(td_boot_protocol::VOLUME_TRUSTED_KEY)).unwrap();
        let decoy = fs::read(seed.join(td_recipe::ladder::DEPLOY_WRONG_KEY)).unwrap();
        assert_eq!(
            real,
            trust.trusted_key_line(),
            "the volume's trust root must be the key that signed the deployment"
        );
        assert_ne!(
            real, decoy,
            "a decoy equal to the real key would make the refused pass succeed"
        );
        // A WHOLE VALID key, so what it earns is a signature that does not
        // verify rather than a key that will not parse.
        let decoy_key = decode_hex_fixture::<32>(&decoy);
        assert!(
            !td_engine::ed25519::verify(&decoy_key, &staged, &signature),
            "the decoy must fail to authenticate what the real key authenticates"
        );
        for relative in [
            td_boot_protocol::VOLUME_TRUSTED_KEY,
            td_recipe::ladder::DEPLOY_WRONG_KEY,
        ] {
            // `--rootdir` copies a mode in verbatim, so an ambient umask would
            // otherwise decide what the fixture's trust root looks like.
            let mode = fs::metadata(seed.join(relative)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o644, "{relative} must be staged 0644");
        }
    }

    /// The appendix's members, and that its parents are DIRECTORIES.
    ///
    /// The key path is spelled as a LITERAL here rather than as
    /// `TRUSTED_KEY_PATH`. Comparing the constant to itself passes however
    /// wrong the constant is — a review showed `var/lib/td/deployment.pub`
    /// surviving, which at boot is `filp_open` failing on a missing `var` and
    /// returning 0 with no diagnostic. The type bits are asserted for the same
    /// class of reason: parents emitted as empty FILES also survived, and give
    /// ENOTDIR and the same silent absence.
    #[test]
    fn the_appendix_places_the_key_under_real_parent_directories() {
        let seq = AtomicU64::new(2070);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        const BASE: &[u8] = b"070701selector";
        const BASE_LEN: usize = BASE.len();
        let initramfs = dir.join("selector-initramfs.cpio");
        fs::write(&initramfs, BASE).unwrap();
        // 0444, as `fs::copy` from a store output leaves it: `copy_canonical`
        // fixes a non-executable store file at that mode, so an append onto the
        // copy is EACCES. A writable 0644 fixture — which `fs::write` gives —
        // passes whatever the code does about permissions, and did.
        fs::set_permissions(&initramfs, fs::Permissions::from_mode(0o444)).unwrap();

        let trust = RunTrust::generate().unwrap();
        append_trusted_key(&initramfs, &trust.trusted_key_line()).unwrap();
        let bytes = fs::read(&initramfs).unwrap();

        let members = appendix_members(&bytes, BASE_LEN);
        let names: Vec<&str> = members.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["etc", "etc/td", "etc/td/deployment.pub"],
            "the literal path td-boot will open, and every parent of it"
        );
        assert_eq!(
            td_boot_protocol::TRUSTED_KEY_PATH,
            "etc/td/deployment.pub",
            "the shared constant must be the path asserted above"
        );

        const S_IFMT: u32 = 0o170000;
        const S_IFDIR: u32 = 0o040000;
        const S_IFREG: u32 = 0o100000;
        for (name, mode, _) in &members {
            let expected = if name == "etc/td/deployment.pub" { S_IFREG } else { S_IFDIR };
            assert_eq!(
                mode & S_IFMT,
                expected,
                "{name} has the wrong type bits — a parent that is not a directory \
                 gives ENOTDIR and the kernel skips the file in silence"
            );
        }

        // And the key bytes are this run's public half in the documented wire
        // format, not something that merely parses.
        let (_, _, key) = members
            .iter()
            .find(|(n, _, _)| n == "etc/td/deployment.pub")
            .expect("the key member");
        assert_eq!(key, &trust.trusted_key_line());
        assert_eq!(decode_hex_fixture::<32>(key), trust.public);
    }

    /// The wire format DESIGN.md §6 fixes: lowercase hex and one trailing
    /// newline. Both readers accept uppercase, so neither would notice.
    #[test]
    fn hex_line_is_lowercase_hex_with_one_trailing_newline() {
        assert_eq!(hex_line(&[0x00, 0xff, 0xa5]), b"00ffa5\n".to_vec());
        assert_eq!(hex_line(&[0xde, 0xad, 0xbe, 0xef]), b"deadbeef\n".to_vec());
        let signature = hex_line(&[0u8; 64]);
        assert_eq!(signature.len(), 129, "64 bytes as hex plus the newline");
        assert!(signature.iter().all(|b| b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || *b == b'\n'));
    }

    /// The WIRING, which none of the other tests reach: that what
    /// `provision_selector` hands back is a different file from the store
    /// output, carries the run's key, and leaves the original alone.
    ///
    /// A review mutated four ways of losing this — returning the source instead
    /// of the copy, dropping the append in either caller, and booting the
    /// unprovisioned selector — and all four survived the whole suite. Three
    /// are now type errors (`VerifiedSelector`'s path is private, and this is
    /// the only way out of it); this covers the fourth, which is a body a
    /// compiler cannot object to.
    #[test]
    fn provisioning_returns_a_keyed_copy_and_leaves_the_store_output_alone() {
        let seq = AtomicU64::new(2080);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        const BASE: &[u8] = b"070701selector";
        const BASE_LEN: usize = BASE.len();

        // A stand-in store output: 0444, as `copy_canonical` leaves a
        // non-executable file.
        let store = dir.join("store-selector-initramfs.cpio");
        fs::write(&store, BASE).unwrap();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o444)).unwrap();

        let out = dir.join("images");
        fs::create_dir(&out).unwrap();
        let trust = RunTrust::generate().unwrap();
        let booted = provision_selector(&VerifiedSelector(store.clone()), &out, &trust).unwrap();

        assert_ne!(booted, store, "the bootable path must not be the store output");
        assert!(booted.starts_with(&out), "the copy belongs in the destination dir");

        // The store output is untouched, bytes and mode: the append widens the
        // COPY, and a chmod of the original would break the store's invariant.
        assert_eq!(fs::read(&store).unwrap(), BASE);
        assert_eq!(
            fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0o444,
            "the store output's mode must survive provisioning"
        );

        // The copy is widened by exactly the owner-write bit, not flattened to
        // a literal: 0444 becomes 0644, and nothing gains group or world write.
        // Pinned because a mode wide enough to append is also a mode wide
        // enough to be rewritten by anything sharing the directory.
        assert_eq!(
            fs::metadata(&booted).unwrap().permissions().mode() & 0o777,
            0o644,
            "the provisioned copy takes owner-write and nothing else"
        );

        // And what boots really does carry this run's key.
        let bytes = fs::read(&booted).unwrap();
        assert!(bytes.starts_with(BASE), "the base archive must still be first");
        let members = appendix_members(&bytes, BASE_LEN);
        let names: Vec<&str> = members.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["etc", "etc/td", "etc/td/deployment.pub"]);
        let (_, _, key) = members
            .iter()
            .find(|(n, _, _)| n == "etc/td/deployment.pub")
            .expect("the key member");
        assert_eq!(decode_hex_fixture::<32>(key), trust.public);
    }

    #[test]
    fn key_path_parents_are_every_proper_prefix_shallowest_first() {
        assert_eq!(key_path_parents(), vec!["etc", "etc/td"]);
    }

    /// `(name, mode, data)` for each member of the appendix, walked from the
    /// offset alignment REQUIRES it to start at — computed, never searched
    /// for, since a search finds the appendix wherever it landed and so cannot
    /// fail on a misalignment. The fixture base opens with `070701` to make
    /// that concrete.
    fn appendix_members(bytes: &[u8], base_len: usize) -> Vec<(String, u32, Vec<u8>)> {
        let mut at = base_len.next_multiple_of(4);
        assert_eq!(
            &bytes[at..at + 6],
            b"070701",
            "the appendix must start at {at}, the first 4-aligned offset past the \
             {base_len}-byte base — the kernel reports nothing if it does not"
        );
        for (offset, byte) in bytes.iter().enumerate().take(at).skip(base_len) {
            assert_eq!(*byte, 0, "padding at {offset} must be NUL");
        }

        let mut members = Vec::new();
        loop {
            assert_eq!(at % 4, 0, "header at {at} is not 4-aligned");
            assert_eq!(&bytes[at..at + 6], b"070701", "magic at {at}");
            let field = |i: usize| {
                let s = at + 6 + i * 8;
                u32::from_str_radix(std::str::from_utf8(&bytes[s..s + 8]).unwrap(), 16).unwrap()
            };
            let (mode, filesize, namesize) =
                (field(1), field(6) as usize, field(11) as usize);
            let name =
                String::from_utf8(bytes[at + 110..at + 110 + namesize - 1].to_vec()).unwrap();
            let data = (at + 110 + namesize).next_multiple_of(4);
            at = (data + filesize).next_multiple_of(4);
            if name == "TRAILER!!!" {
                assert_eq!(at, bytes.len(), "the walk must consume the archive exactly");
                return members;
            }
            members.push((name, mode, bytes[data..data + filesize].to_vec()));
            assert!(members.len() < 16, "walked off the end without a trailer");
        }
    }

    fn decode_hex_fixture<const N: usize>(text: &[u8]) -> [u8; N] {
        let trimmed = text.trim_ascii();
        assert_eq!(trimmed.len(), N * 2, "expected {N} bytes of hex");
        let mut out = [0u8; N];
        for (slot, pair) in out.iter_mut().zip(trimmed.chunks_exact(2)) {
            *slot = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        out
    }

    #[test]
    fn selector_manifest_verifies_payload_and_rejects_tampering() {
        let seq = AtomicU64::new(2100);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        let name = "selector-initramfs.cpio";
        fs::write(dir.join(name), b"selector").unwrap();
        let digest = crate::sha256::sha256_file(&dir.join(name)).unwrap();
        fs::write(
            dir.join("manifest"),
            format!("td-deployment-v1\n{digest}  {name}\n"),
        )
        .unwrap();
        assert_eq!(verify_selector(&dir).unwrap().0, dir.join(name));

        fs::write(dir.join(name), b"tampered").unwrap();
        let error = verify_selector(&dir).unwrap_err();
        assert!(error.contains("selector hash mismatch"), "{error}");
    }

    #[test]
    fn persistent_seed_uses_the_td_boot_layout_contract() {
        let seq = AtomicU64::new(2200);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _guard = Scratch { dir: dir.clone() };
        let deployment = dir.join("deployment");
        fs::create_dir(&deployment).unwrap();
        for name in ["bzImage", "initramfs.cpio", "root.erofs", "manifest"] {
            fs::write(deployment.join(name), name.as_bytes()).unwrap();
        }
        let id = "a".repeat(64);
        let seed = dir.join("seed");
        populate_persistent_seed(&deployment, &seed, &id, &RunTrust::generate().unwrap()).unwrap();

        let installed = seed.join(td_boot_protocol::DEPLOYMENTS_DIR).join(&id);
        assert_eq!(fs::read(installed.join("manifest")).unwrap(), b"manifest");
        for slot in ["current", "previous"] {
            assert_eq!(
                fs::read_link(seed.join(td_boot_protocol::BOOT_DIR).join(slot)).unwrap(),
                PathBuf::from(format!("{}{id}", td_boot_protocol::SELECTOR_PREFIX))
            );
        }
        assert!(seed.join("@var").is_dir());
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
        assert!(!contains(
            b"only TD-USERLAND-OK here",
            EROFS_MARKER.as_bytes()
        ));
    }

    /// Every marker the console scanner latches, enumerated once so a new one is added in a
    /// single place. `evidence_marker_max_len` deliberately does NOT check itself against
    /// this: its result is dominated by the id-bearing markers (marker + space + 64-char
    /// hex), so a "no marker exceeds the max" assertion cannot fail and would only look like
    /// a guard. The rescan window is covered behaviourally instead, by the split tests below.
    /// The oracle's first-client evidence must be the TERMINAL's marker.
    ///
    /// Rebinding the alias to the demo's is a boot that runs to its timeout:
    /// nothing has started the demo since the cutover, so its line never
    /// appears and the failure names a marker rather than a cause. The
    /// equality is not a tautology — the alias names one of several ladder
    /// constants, and which one is exactly what can go wrong.
    #[test]
    fn the_first_client_evidence_is_the_terminals_marker() {
        assert_eq!(
            TD_TERM_RUNTIME_MARKER,
            td_recipe::ladder::TD_TERM_RUNTIME_MARKER
        );
        assert_ne!(
            TD_TERM_RUNTIME_MARKER,
            td_recipe::ladder::TD_UI_CLIENT_RUNTIME_MARKER
        );
        assert!(all_console_markers().contains(&TD_TERM_RUNTIME_MARKER));
    }

    fn all_console_markers() -> [&'static str; 34] {
        [
            MARKER,
            EROFS_MARKER,
            td_boot_protocol::CURRENT_REJECTED_MARKER,
            td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
            td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER,
            BOOKKEEPING_UNAVAILABLE_MARKER,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            td_boot_protocol::SELECTED_PREVIOUS_MARKER,
            KEXEC_STAGE1_MARKER,
            KEXEC_STAGE2_MARKER,
            "Kernel panic",
            GREETER_MARKER,
            SYSTEM_ROOT_RO_MARKER,
            SYSTEM_ETC_RO_MARKER,
            SYSTEM_STATE_WRITABLE_MARKER,
            SYSTEM_STATE_OWNER_MARKER,
            SYSTEM_PERSIST_WRITE_MARKER,
            SYSTEM_PERSIST_READ_MARKER,
            SYSTEM_BOOT_SUCCESS_MARKER,
            SYSTEM_DEPLOY_INSTALL_MARKER,
            SYSTEM_SHUTDOWN_MARKER,
            UUTILS_RUNTIME_MARKER,
            RIPGREP_FD_RUNTIME_MARKER,
            SYSTEM_NET_UP_MARKER,
            SYSTEM_NET_RESOLVE_MARKER,
            SYSTEM_NET_REACH_MARKER,
            SSHD_MARKER,
            TD_UTIL_RUNTIME_MARKER,
            TD_TXT_RUNTIME_MARKER,
            TD_INIT_RUNTIME_MARKER,
            TD_LOGIN_RUNTIME_MARKER,
            TD_WAYLAND_RUNTIME_MARKER,
            TD_POINTER_ABSOLUTE_MARKER,
            TD_TERM_RUNTIME_MARKER,
        ]
    }

    #[test]
    fn system_boot_markers_are_distinct() {
        let markers = all_console_markers();
        let unique = std::collections::BTreeSet::from(markers);
        assert_eq!(
            unique.len(),
            markers.len(),
            "each boot assertion needs its own marker"
        );
        for (index, marker) in markers.iter().enumerate() {
            for (other_index, other) in markers.iter().enumerate() {
                if index != other_index {
                    assert!(
                        !marker.contains(other),
                        "marker {marker:?} must not contain {other:?}"
                    );
                }
            }
        }
    }

    /// `drain_console` reads in fixed-size chunks and rescans the last
    /// `evidence_marker_max_len() - 1` bytes, so a marker straddling a read boundary is
    /// still latched. Drop the overlap and a marker that lands across the seam is lost —
    /// silently, and only on consoles long enough to need a second read, which is every
    /// real boot. Split the td-util marker across the seam and require it anyway.
    #[test]
    fn drain_console_latches_a_marker_split_across_a_read_boundary() {
        const CHUNK: usize = 8192;
        let seq = AtomicU64::new(0);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _g = Scratch { dir: dir.clone() };
        let path = dir.join("console.log");
        // Straddle the seam: all but the last byte of the marker lands in read 1.
        let head = CHUNK - (TD_UTIL_RUNTIME_MARKER.len() - 1);
        let mut bytes = vec![b'x'; head];
        bytes.extend_from_slice(TD_UTIL_RUNTIME_MARKER.as_bytes());
        bytes.extend_from_slice(&[b'y'; 128]);
        fs::write(&path, &bytes).unwrap();

        let mut file = None;
        let mut buffer = Vec::new();
        let mut evidence = ConsoleEvidence::default();
        drain_console_to_eof(
            &path,
            &mut file,
            &mut buffer,
            b"target-never-appears",
            &mut evidence,
        )
        .unwrap();
        assert!(
            evidence.td_util_runtime,
            "a marker split across a read boundary must still latch - the rescan overlap \
             regressed"
        );
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
    fn guest_success_wait_stays_below_the_host_timeout() {
        assert_eq!(guest_success_wait_secs(Duration::from_secs(300)), 270);
        assert_eq!(guest_success_wait_secs(Duration::from_secs(1800)), 1770);
        assert_eq!(guest_success_wait_secs(Duration::from_secs(1)), 1);
        assert_eq!(
            autotest_wait_token(Duration::from_secs(300)),
            format!("{}270", td_recipe::ladder::BOOT_SUCCESS_WAIT_CMDLINE_PREFIX)
        );
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
    fn drain_console_latches_protocol_evidence_before_tail_trim() {
        let seq = AtomicU64::new(0);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _g = Scratch { dir: dir.clone() };
        let path = dir.join("console.log");
        let selected_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let selected_previous = format!(
            "{} {selected_id}",
            td_boot_protocol::SELECTED_PREVIOUS_MARKER
        );
        let host_key_line = format!("{TD_FIRSTBOOT_HOST_KEY_PREFIX}SHA256:aGVsbG8gd29ybGQ");
        let mut bytes = [
            td_boot_protocol::CURRENT_REJECTED_MARKER,
            td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
            td_boot_protocol::ATTEMPTS_EXHAUSTED_MARKER,
            BOOKKEEPING_UNAVAILABLE_MARKER,
            selected_previous.as_str(),
            GREETER_MARKER,
            SYSTEM_ROOT_RO_MARKER,
            SYSTEM_ETC_RO_MARKER,
            SYSTEM_ETC_MUTABLE_MARKER,
            TD_FIRSTBOOT_NEW_MARKER,
            TD_FIRSTBOOT_STABLE_MARKER,
            host_key_line.as_str(),
            SYSTEM_STATE_WRITABLE_MARKER,
            SYSTEM_STATE_OWNER_MARKER,
            UUTILS_RUNTIME_MARKER,
            RIPGREP_FD_RUNTIME_MARKER,
            SSHD_MARKER,
            TD_UTIL_RUNTIME_MARKER,
            TD_TXT_RUNTIME_MARKER,
            SYSTEM_PERSIST_WRITE_MARKER,
            SYSTEM_PERSIST_READ_MARKER,
            SYSTEM_BOOT_SUCCESS_MARKER,
            SYSTEM_DEPLOY_INSTALL_MARKER,
            SYSTEM_SHUTDOWN_MARKER,
            SYSTEM_NET_UP_MARKER,
            SYSTEM_NET_RESOLVE_MARKER,
            SYSTEM_NET_REACH_MARKER,
            KEXEC_STAGE1_MARKER,
            "Kernel panic",
        ]
        .join("\n")
        .into_bytes();
        bytes.resize(CAP + 8192, b'x');
        fs::write(&path, bytes).unwrap();

        let mut file = None;
        let mut buffer = Vec::new();
        let mut evidence = ConsoleEvidence::default();
        drain_console(
            &path,
            &mut file,
            &mut buffer,
            b"target-never-appears",
            false,
            &mut evidence,
        )
        .unwrap();
        assert!(!evidence.target);
        assert!(evidence.greeter);
        assert!(evidence.current_rejected);
        assert!(evidence.attempt_consumed);
        assert!(evidence.attempts_exhausted);
        assert!(evidence.bookkeeping_unavailable);
        assert!(evidence.selected_previous);
        assert!(!evidence.selected_current);
        assert_eq!(evidence.selected_previous_id.as_deref(), Some(selected_id));
        assert_eq!(evidence.selected_current_id, None);
        assert!(evidence.root_read_only);
        assert!(evidence.etc_read_only);
        assert!(evidence.etc_mutable);
        assert!(evidence.firstboot_new);
        assert!(evidence.firstboot_stable);
        assert_eq!(evidence.host_key.as_deref(), Some("SHA256:aGVsbG8gd29ybGQ"));
        assert!(evidence.state_writable);
        assert!(evidence.state_owner);
        assert!(evidence.uutils_runtime);
        assert!(evidence.ripgrep_fd_runtime);
        assert!(evidence.sshd);
        assert!(evidence.td_util_runtime);
        assert!(evidence.td_txt_runtime);
        assert!(evidence.persist_write);
        assert!(evidence.persist_read);
        assert!(evidence.boot_success);
        assert!(evidence.deploy_install);
        assert!(evidence.shutdown);
        assert!(evidence.net_up);
        assert!(evidence.net_resolve);
        assert!(evidence.net_reach);
        assert!(evidence.kexec_stage1);
        assert!(evidence.kernel_panic);
        assert!(!contains(
            &buffer,
            td_boot_protocol::CURRENT_REJECTED_MARKER.as_bytes()
        ));
        assert!(!contains(&buffer, SSHD_MARKER.as_bytes()));
    }

    #[test]
    fn drain_console_latches_a_target_split_across_read_chunks() {
        let seq = AtomicU64::new(0);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _g = Scratch { dir: dir.clone() };
        let path = dir.join("console.log");
        let target = b"target-split-across-chunks";
        let mut bytes = vec![b'x'; 8192 - 5];
        bytes.extend_from_slice(target);
        fs::write(&path, bytes).unwrap();

        let mut file = None;
        let mut buffer = Vec::new();
        let mut evidence = ConsoleEvidence::default();
        drain_console(&path, &mut file, &mut buffer, target, true, &mut evidence).unwrap();
        assert!(evidence.target);
    }

    #[test]
    fn drain_console_latches_a_selection_id_split_across_read_chunks() {
        let seq = AtomicU64::new(0);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _g = Scratch { dir: dir.clone() };
        let path = dir.join("console.log");
        let selected_id = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let mut bytes = vec![b'x'; 8192 - 5];
        bytes.extend_from_slice(td_boot_protocol::SELECTED_CURRENT_MARKER.as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(selected_id.as_bytes());
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();

        let mut file = None;
        let mut buffer = Vec::new();
        let mut evidence = ConsoleEvidence::default();
        drain_console(
            &path,
            &mut file,
            &mut buffer,
            b"target-never-appears",
            false,
            &mut evidence,
        )
        .unwrap();

        assert_eq!(evidence.selected_current_id.as_deref(), Some(selected_id));
        assert_eq!(evidence.selected_previous_id, None);
    }

    /// The fingerprint latch is what turns "a key existed" into "the SAME key", so
    /// a truncated latch would report a host-key rotation that never happened.
    #[test]
    fn host_key_latch_takes_whole_terminated_tokens_only() {
        let prefix = TD_FIRSTBOOT_HOST_KEY_PREFIX;
        let fingerprint = "SHA256:4lqaECIkRUNj0elPdI5ADeldChXHFOGuogerW1L1iAU";

        let mut evidence = ConsoleEvidence::default();
        latch_console_evidence(
            &mut evidence,
            format!("noise\n{prefix}{fingerprint}\nmore\n").as_bytes(),
            b"target",
        );
        assert_eq!(evidence.host_key.as_deref(), Some(fingerprint));

        // Still being written: no terminator yet, so nothing latches. The scan
        // window's overlap re-examines the line once the newline arrives.
        let mut partial = ConsoleEvidence::default();
        latch_console_evidence(
            &mut partial,
            format!("{prefix}{fingerprint}").as_bytes(),
            b"target",
        );
        assert_eq!(partial.host_key, None);
        latch_console_evidence(
            &mut partial,
            format!("{prefix}{fingerprint}\n").as_bytes(),
            b"target",
        );
        assert_eq!(partial.host_key.as_deref(), Some(fingerprint));

        // Longer than the ceiling: refused outright rather than latched truncated.
        let mut oversized = ConsoleEvidence::default();
        latch_console_evidence(
            &mut oversized,
            format!("{prefix}{}\n", "x".repeat(HOST_KEY_MAX + 1)).as_bytes(),
            b"target",
        );
        assert_eq!(oversized.host_key, None);

        // An empty token is not a fingerprint.
        let mut empty = ConsoleEvidence::default();
        latch_console_evidence(&mut empty, format!("{prefix}\n").as_bytes(), b"target");
        assert_eq!(empty.host_key, None);

        // The FIRST fingerprint wins, so a later boot's line cannot overwrite the
        // one this boot is being judged on.
        let mut first = ConsoleEvidence::default();
        latch_console_evidence(
            &mut first,
            format!("{prefix}{fingerprint}\n{prefix}SHA256:different\n").as_bytes(),
            b"target",
        );
        assert_eq!(first.host_key.as_deref(), Some(fingerprint));
    }

    /// The overlap the incremental drain carries must admit the LONGEST thing
    /// latched, or a fingerprint line straddling two reads is never seen at all.
    /// The budget is `evidence_marker_max_len(marker) - 1` (see `drain_console`),
    /// and `latch_token` needs the prefix, up to HOST_KEY_MAX token bytes, AND the
    /// terminator inside the window — hence the +2.
    #[test]
    fn the_scan_overlap_covers_a_full_fingerprint_line() {
        let overlap = evidence_marker_max_len(b"t").saturating_sub(1);
        assert!(
            overlap >= TD_FIRSTBOOT_HOST_KEY_PREFIX.len() + HOST_KEY_MAX + 1,
            "the drain overlap ({overlap}) cannot hold a whole host-key line, so one \
             split across two reads would never latch"
        );
    }

    #[test]
    fn selection_id_latch_rejects_truncated_and_non_hex_ids() {
        let marker = td_boot_protocol::SELECTED_CURRENT_MARKER;
        let mut evidence = ConsoleEvidence::default();

        latch_console_evidence(
            &mut evidence,
            format!("{marker} {}", "a".repeat(63)).as_bytes(),
            b"target",
        );
        assert_eq!(evidence.selected_current_id, None);
        latch_console_evidence(
            &mut evidence,
            format!("{marker} {}", "g".repeat(64)).as_bytes(),
            b"target",
        );
        assert_eq!(evidence.selected_current_id, None);
    }

    #[test]
    fn full_drain_keeps_reading_protocol_evidence_after_the_target() {
        let seq = AtomicU64::new(0);
        let dir = create_scratch_dir(&env::temp_dir(), &seq).unwrap();
        let _g = Scratch { dir: dir.clone() };
        let path = dir.join("console.log");
        let target = b"early-target";
        let mut bytes = target.to_vec();
        bytes.resize(DRAIN_BUDGET + 100, b'x');
        bytes.extend_from_slice(b"Kernel panic");
        fs::write(&path, bytes).unwrap();

        let mut file = None;
        let mut buffer = Vec::new();
        let mut evidence = ConsoleEvidence::default();
        drain_console_to_eof(&path, &mut file, &mut buffer, target, &mut evidence).unwrap();
        assert!(evidence.target);
        assert!(evidence.kernel_panic);
    }

    #[test]
    fn end_reasons_distinguish_before_and_after_target() {
        let before = format_end_reason(EndReason::TimedOut(17), false);
        let after = format_end_reason(EndReason::TimedOut(17), true);
        assert!(before.contains("no marker"));
        assert!(after.contains("after the marker"));

        let before = format_end_reason(EndReason::Flooded(42), false);
        let after = format_end_reason(EndReason::Flooded(42), true);
        assert!(before.contains("without reaching the marker"));
        assert!(after.contains("after the marker"));
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
