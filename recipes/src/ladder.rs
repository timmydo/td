//! Shared step builders for the bootstrap-ladder rungs (#378 slices 2+3).
//!
//! Every rung recipe (recipes/src/recipes/{mes,tcc,…}.rs) composes its typed
//! `Step` list from these helpers. Conventions:
//! - `BASH` is the td-built bootstrap shell (`bash-mesboot`, bash 2.05b built
//!   entirely from source — no host tools). Every rung that needs a POSIX shell
//!   declares it as a typed RecipeOutput edge, never the leaked host bash.
//! - `MESBOOT0_TOOLS` are the td-built tcc-era userland (coreutils/sed/grep/
//!   gawk/diffutils `-mesboot0` providers) EVERY rung declares as lock inputs;
//!   `mesboot0_path()` / `mesboot0_inputs()` lay them onto a rung's PATH and
//!   input list ({tools} farm first, then the td shell, then the providers).
//! - Unpacking is ENGINE-NATIVE (`Step::Unpack` — td's own std-only
//!   tar/gzip/bzip2/xz readers), so no rung declares an unpacker package.

use crate::types::{Step, TextEdit};

/// The td-built bootstrap shell (catalog stem). `bash-mesboot` is bash 2.05b
/// built from source with no host tools (baked Makefiles + engine-native
/// patches + `oyacc`), so every rung declares it as a RecipeOutput edge.
pub const BASH: &str = "bash-mesboot";

/// The td-built tcc-era userland (catalog stems) EVERY rung declares as its
/// scripting toolset. Each is the `-mesboot0` provider recipe built from source
/// under tcc + mes libc — coreutils/sed/grep/gawk/diffutils as
/// `AuditedSeed`/`RecipeOutput` edges, never bare host names.
///
/// GNU findutils is deliberately absent as an evidenced DEAD axis for this
/// bootstrap toolset. A later source build may expose BusyBox `find`/`xargs`
/// through a ToolFarm only when it declares `busybox-x86-64`; the
/// `no_bootstrap_step_invokes_host_find_or_xargs` guard below enforces that
/// provenance instead of permitting an ambient PATH lookup.
pub const MESBOOT0_TOOLS: &[&str] = &[
    "coreutils-mesboot0",
    "sed-mesboot0",
    "grep-mesboot0",
    "gawk-mesboot0",
    "diffutils-mesboot0",
];

/// The rung PATH template: the `{tools}` farm first, then the td shell, then the
/// td-built `MESBOOT0_TOOLS` packages. Every Run step that needs the scripting
/// userland uses this.
pub fn mesboot0_path() -> String {
    let mut p = String::from("{tools}");
    p.push_str(&format!(":{{in:{BASH}}}/bin"));
    for t in MESBOOT0_TOOLS {
        p.push_str(&format!(":{{in:{t}}}/bin"));
    }
    p
}

/// A rung's full lock-input list: the rung-specific `extras` FIRST, then the td
/// shell `BASH`, then the td-built `MESBOOT0_TOOLS` — in lockstep with the order
/// `mesboot0_path()` lays down, so a rung's inputs cannot drift out of step with
/// the PATH nodes and red only at execution deep in the chain. Pair with
/// `Recipe::inputs_owned`.
pub fn mesboot0_inputs(extras: &[&str]) -> Vec<String> {
    extras
        .iter()
        .copied()
        .chain(std::iter::once(BASH))
        .chain(MESBOOT0_TOOLS.iter().copied())
        .map(|s| s.to_string())
        .collect()
}

/// The tool-farm step that symlinks a prior binutils rung's whole `bin/` into
/// `{tools}` (as/ld/ar/ranlib/nm/strip/…) with the td-built `coreutils-mesboot0`
/// `ln`, on `mesboot0_path()`. The `glob:` argv element expands sorted in the
/// engine.
pub fn link_bins(binutils_rung: &str) -> Step {
    Step::run(
        "{root}",
        &[
            "{in:coreutils-mesboot0}/bin/ln",
            "-sf",
            &format!("glob:{{in:{binutils_rung}}}/bin/*"),
            "{tools}",
        ],
    )
    .env("PATH", &mesboot0_path())
}

/// The declared shell (the sandbox has no /bin/sh): the td-built `bash-mesboot`
/// output, not a host bash.
pub const SH: &str = "{in:bash-mesboot}/bin/bash";

/// The shell and userland beyond the native self-hosting tool boundary. BusyBox
/// is a reviewed boundary output and must be a declared `native_input` of every
/// recipe using these paths.
pub const POST_BOOTSTRAP_SH: &str = "{in:busybox-x86-64}/bin/sh";

pub fn post_bootstrap_path() -> String {
    "{in:busybox-x86-64}/bin".into()
}

/// The exact line the bootable-kernel rung's busybox `/init` prints on ttyS0 once
/// the kernel has reached userspace, and that the host-side `qemu-boot` tool asserts
/// on. SINGLE SOURCE OF TRUTH shared by the `/init` script, both initramfs shape
/// checks, and the boot tool (`checks/qemu_boot.rs`, via `td_recipe::ladder`), so the
/// producer, the gated shape check, and the boot oracle can never silently desync.
pub const USERLAND_MARKER: &str = "TD-USERLAND-OK";

/// The line the bootable-kernel rung's `/init` prints on ttyS0 AFTER it mounts the
/// attached virtio-blk disk as READ-ONLY erofs and reads `EROFS_PROBE_SENTINEL`
/// back — the success signal the host-side `qemu-boot-erofs` tool asserts on
/// (re #549). Emitted only on a successful read-only mount + sentinel read, so
/// seeing it proves the source-built kernel (EROFS_FS + VIRTIO_BLK) can mount a
/// td-written erofs image. Distinct from `USERLAND_MARKER`, which the /init prints
/// first (userspace reached) unconditionally. SINGLE SOURCE OF TRUTH shared by the
/// `/init` script, the initramfs shape check, and the boot oracle.
pub const EROFS_MARKER: &str = "TD-EROFS-RO-OK";

/// The sentinel file the `qemu-boot-erofs` probe writes into the erofs image (via
/// `td-builder mkfs-erofs`) and the guest `/init` reads back after mounting the
/// disk read-only. Shared so the image producer (the boot oracle) and the consumer
/// (the /init script) name the same path.
pub const EROFS_PROBE_SENTINEL: &str = "td-erofs-probe.ok";

/// The exact CONTENT the probe writes into `EROFS_PROBE_SENTINEL`, which the guest
/// `/init` reads back with `cat` and string-compares before printing `EROFS_MARKER`.
/// Comparing the CONTENT (not just `test -f` on the name) forces the kernel to read
/// the file's DATA blocks off the erofs image — proving the flat-plain data layout
/// and block addressing, not merely that the inode/dirent parse. A single shell-safe
/// token (no spaces/quotes/newline) so the `[ "$x" = "..." ]` compare stays trivial.
pub const EROFS_PROBE_CONTENT: &str = "td-erofs-ro-readback-ok";

// ── system-x86-64 two-stage boot markers (re #550) ──────────────────────────────
// The distro's persistent boot — the initramfs selects a deployment from Btrfs,
// kexecs it, loop-mounts its read-only EROFS root, mounts persistent state below
// `/var`, and `switch_root`s into it — proves itself on ttyS0 with lines the headless
// `qemu-boot-system` oracle asserts on. They are SINGLE SOURCE OF TRUTH shared
// by the recipe (`/etc/rootcheck`, `/etc/profile`) and the oracle so they never desync.

/// Printed by `/etc/rootcheck` on the REAL root (post-`switch_root`) once it confirms
/// via `/proc/mounts` that `/` is an `erofs` mount carrying the read-only (`ro`) option
/// — i.e. the store root really is the immutable erofs image, not the initramfs.
pub const SYSTEM_ROOT_RO_MARKER: &str = "TD-ROOT-EROFS-RO-OK";

/// Printed by `/etc/rootcheck` once a root write probe confirms deployment-owned
/// `/etc` remains on the immutable EROFS root.
pub const SYSTEM_ETC_RO_MARKER: &str = "TD-ETC-EROFS-RO-OK";

/// Printed by `/etc/rootcheck` once every reviewed `MUTABLE_ETC` symlink resolves to
/// the target the image recorded, the PERSISTENT ones are populated (td-firstboot is
/// the sysinit job before it), `/etc/machine-id` reads back as 32 hex digits through
/// its symlink, and the unprivileged login user can read the SSH host key's `.pub`
/// but NOT the private key.
///
/// The companion to `SYSTEM_ETC_RO_MARKER`, and only meaningful beside it: together
/// they say `/etc` is immutable AND that the handful of per-machine files reach
/// writable state anyway — which is what td gets by naming each mutable file
/// individually instead of mounting an `/etc` overlay.
pub const SYSTEM_ETC_MUTABLE_MARKER: &str = "TD-ETC-MUTABLE-OK";

/// Printed by `/etc/rootcheck` once all immutable-root, ownership, mount, link, and
/// state write checks pass.
pub const SYSTEM_STATE_WRITABLE_MARKER: &str = "TD-STATE-WRITABLE-OK";

/// Printed only after the unprivileged login user can write its own home but
/// cannot write the persistent state root or root's home.
pub const SYSTEM_STATE_OWNER_MARKER: &str = "TD-STATE-OWNER-OK";

/// Printed by `/bin/td-firstboot` at sysinit when it had to MINT part of this
/// machine's identity — i.e. this is the machine's first boot on this `/var`.
/// Seeing it on a LATER boot means identity did not persist, which is the failure
/// the per-file `/etc` → `/var` symlinks exist to prevent.
///
/// DUPLICATED as `NEW_MARKER` in td-firstboot/src/main.rs (a separate crate the
/// recipe builds from its own source); `td-firstboot.rs`'s unit tests read the
/// literals back out of that source and assert the two agree.
pub const TD_FIRSTBOOT_NEW_MARKER: &str = "TD-FIRSTBOOT-NEW-OK";

/// Printed by `/bin/td-firstboot` when every identity file was already present and
/// valid — the steady state of a provisioned machine. Its counterpart above must
/// NOT appear on the same boot.
///
/// DUPLICATED as `STABLE_MARKER` in td-firstboot/src/main.rs.
pub const TD_FIRSTBOOT_STABLE_MARKER: &str = "TD-FIRSTBOOT-STABLE-OK";

/// Prefix of the line `/bin/td-firstboot` prints this machine's SSH host-key
/// fingerprint on: `TD-FIRSTBOOT-HOSTKEY SHA256:<base64>`. The oracle compares the
/// fingerprint across reboots — a marker can only say a key was reused, this proves
/// it is the SAME key. Only the public fingerprint is printed; nothing derived from
/// the private key or the machine-id reaches the console.
///
/// DUPLICATED as `HOST_KEY_PREFIX` in td-firstboot/src/main.rs.
pub const TD_FIRSTBOOT_HOST_KEY_PREFIX: &str = "TD-FIRSTBOOT-HOSTKEY ";

/// Printed after the first persistence-oracle boot writes and syncs its marker below
/// `/var`. The second boot uses the same Btrfs volume and must read the exact bytes back.
pub const SYSTEM_PERSIST_WRITE_MARKER: &str = "TD-PERSIST-WRITE-OK";

/// Printed on the second persistence-oracle boot only when the marker written by the
/// first boot survives with its exact content.
pub const SYSTEM_PERSIST_READ_MARKER: &str = "TD-PERSIST-READ-OK";

/// The host's wall-clock ceiling on one `qemu-boot-system` boot, and the value
/// `TD_QEMU_BOOT_TIMEOUT_SECS` overrides. A tiny allnoconfig kernel boots to
/// userspace under TCG in a few seconds, but the persistent system modes hash their
/// deployment, kexec, and boot a second kernel. The poll loop returns as soon as the
/// selected mode finishes, so this bounds a failed or unusually slow boot alone.
///
/// It lives HERE, beside no other host constant, because it is only half of a pair:
/// the guest's boot-success loop has a patience of its own, and a host that gives up
/// first turns a diagnosable unhealthy boot into a bare timeout with no guest-side
/// reason in it. `the_host_ceiling_outlasts_the_guest_loop_it_waits_for` holds the
/// two together, and it is the reason for this number rather than any measurement:
/// the rollback pass added deployment-sized work to the install boot — the fallback's
/// payload digests verified, and the candidate hashed twice more by a reinstall that
/// copies nothing — which raised the guest's per-iteration budget and so raised this.
/// Raising it costs only how long a HUNG boot takes to be called one.
/// It does not change what the guest does: the wait token derived from it is clamped
/// in the generated scripts, so the retry budgets are the same at either value.
pub const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 480;

/// Printed after the running system installs and activates a verified candidate
/// deployment through td-boot's fsync + atomic-rename transaction.
pub const SYSTEM_DEPLOY_INSTALL_MARKER: &str = "TD-DEPLOY-INSTALL-OK";

/// Printed after the deployment that update installed is ROLLED BACK to the one that
/// is running, and then reinstalled — `td-install/DESIGN.md` §11's third oracle. The
/// `previous` slot is already exercised by the automatic-rollback and corrupt-current
/// boots; what is new here is the `rollback` VERB, driven on a running machine.
///
/// The reinstall is not decoration. The boots after this one expect the candidate to
/// be current, so the pass has to end where it began; and asserting that the second
/// install names the same deployment is what proves a rolled-back volume is one an
/// update can still proceed from — which is the real operational sequence, an update
/// that boots badly followed by another attempt.
pub const SYSTEM_DEPLOY_ROLLBACK_MARKER: &str = "TD-DEPLOY-ROLLBACK-OK";

/// Printed after the root-owned target passes immutable-state checks plus unprivileged
/// uutils and SSH runtime probes, then td-boot records or confirms the deployment successful.
pub const SYSTEM_BOOT_SUCCESS_MARKER: &str = "TD-BOOT-SUCCESS-OK";

/// Printed by BusyBox init's shutdown action after syncing and unmounting @var.
pub const SYSTEM_SHUTDOWN_MARKER: &str = "TD-SHUTDOWN-OK";

/// Printed by `/etc/profile` when the auto-login greeter shell is reached — the login
/// chain (getty → login → ash) ran on the real root. The primary "booted to the
/// greeter" success line.
pub const GREETER_MARKER: &str = "TD-GREETER-OK";

/// Printed after unprivileged uutils behavior probes pass by absolute `/bin` path.
/// Shape checks prove only static closure; the greeter can otherwise false-pass (#547).
pub const UUTILS_RUNTIME_MARKER: &str = "TD-UUTILS-RUN-OK";

/// Printed by the root-owned health target only after unprivileged `/bin/rg` finds
/// the exact hostname line and `/bin/fd` finds the exact hostname path on the EROFS
/// root. One marker covers the pair because both commands must pass before it is
/// emitted; either failure withholds boot success and names the failing command.
pub const RIPGREP_FD_RUNTIME_MARKER: &str = "TD-RG-FD-RUN-OK";

/// Printed by the root-owned health target only after unprivileged `/bin/sshd selftest`
/// exits 0 — the source-built russh daemon stood up an in-process server on an ephemeral
/// loopback port and completed a full SSH handshake+auth+channel+exec round-trip against it.
/// This proves THREE things the static scan can't: the kernel's TCP/IP loopback works
/// (CONFIG_NET+INET), the russh crypto/protocol stack runs, and sshd's dynamic runtime
/// closure (ELF interp, glibc, libgcc_s, the aws-lc crypto C lib) resolves on the erofs root.
/// The marker string is DUPLICATED as `OK_MARKER` in tests/sshd/src/main.rs (a separate
/// crate the recipe builds); the two must stay identical.
pub const SSHD_MARKER: &str = "TD-SSHD-OK";

/// Printed by the root-owned health target only after EVERY `/bin` name the static td-util
/// multicall serves exits 0 as the unprivileged login user. Absolute paths cover the shipped
/// symlinks and argv[0] dispatch plus `/proc` and `/dev/kmsg` reads skipped in the sandbox.
pub const TD_UTIL_RUNTIME_MARKER: &str = "TD-UTIL-RUN-OK";

/// Printed by the root-owned health target only after both `/bin` names the static td-txt
/// multicall serves — `grep` and `sed` — answer correctly as the unprivileged login user.
///
/// This one is deliberately NOT a bare "did it exit 0" probe like td-util's. `/bin/grep` is
/// on the boot path (`/etc/rootcheck` decides the root is healthy with it), so the interesting
/// failure is not a grep that dies, it is a grep that ANSWERS WRONGLY — which would mark a
/// broken root healthy in silence. So the probe greps the live `/proc/mounts` for the root
/// line and requires the DISCRIMINATING answer, which also re-proves on the real image that a
/// zero-`st_size` procfs file is read as a stream. `/bin/sed` has no boot-path duty at all,
/// so it is proven here or nowhere.
pub const TD_TXT_RUNTIME_MARKER: &str = "TD-TXT-RUN-OK";

/// Printed by `/etc/bootsuccess` ONLY after every `/bin` name the static td-init multicall
/// serves has been exercised by its absolute `/bin` path. Unlike the td-util farm, three of
/// those names are IRREVERSIBLE — `reboot`/`poweroff`/`halt` end the boot — so they are probed
/// through their REFUSAL: a bad option must exit non-zero without reaching `reboot(2)`, which
/// is exactly the parse-before-act contract that keeps a typo from powering the machine off.
/// `switch_root` is probed the same way (its fail-early refusal), `hostname` by reading back
/// what sysinit set, and `init` by `--dry-run` over the shipped table. The applets whose
/// SUCCESS path no probe can reach — `init` as PID 1, `switch_root` as the pivot, `reboot` as
/// the exit — are proven instead by the boot getting far enough to print this at all: nothing
/// reaches the health target unless td-init ran the inittab and pivoted the root.
pub const TD_INIT_RUNTIME_MARKER: &str = "TD-INIT-RUN-OK";

/// Printed by `/etc/bootsuccess` only after `/bin/su` — td-login — has switched to
/// the unprivileged login user AND the kernel's own view of the switched process
/// matched what the switch asked for, read back out of `/proc/self/status` by
/// `td-login verify-credentials`.
///
/// Unlike the td-util and td-init farms, td-login's success path needs no synthetic
/// probe: `login -f` is how this image reaches its greeter and `su` is how every
/// other unprivileged health leg runs, so a td-login that fails to start a session
/// fails the boot outright. What those legs CANNOT see is the failure that matters
/// most — a switch that started a working session while leaving a residual
/// credential attached. A `setuid(2)` issued before `setgroups(2)` drops the uid and
/// silently keeps root's supplementary groups; every marker on this image still
/// prints. So this one asserts the RESULT: all four uid columns, all four gid
/// columns, and the supplementary set exactly. See td-login/THREAT-MODEL.md.
pub const TD_LOGIN_RUNTIME_MARKER: &str = "TD-LOGIN-RUN-OK";

/// Printed by the unprivileged compositor only after its first framebuffer
/// paint succeeded and its mode-0600 Wayland socket is listening.
///
/// DUPLICATED as the ready line in td-compositor/src/server.rs. The compositor
/// recipe pins the source literal to this value.
pub const TD_WAYLAND_RUNTIME_MARKER: &str = "TD-WAYLAND-READY";

/// Printed by the unprivileged td-native client only after wl_shm buffer release
/// and the first wl_surface frame callback have both arrived.
///
/// DUPLICATED in td-compositor/src/client.rs and pinned by its recipe.
pub const TD_UI_CLIENT_RUNTIME_MARKER: &str = "TD-UI-CLIENT-READY";

/// Printed by the terminal once `present` has returned — a frame drawn at a size
/// the compositor CHOSE, with both the wl_shm buffer release and the first frame
/// callback arrived — and once the PTY the kernel agrees is that grid has a child
/// on it — more than [`TD_UI_CLIENT_RUNTIME_MARKER`] proves, in every dimension
/// but ONE. The demo required a seat advertising POINTER and KEYBOARD and asked
/// for both; the terminal needs no pointer and requires only KEYBOARD, so a
/// compositor whose `wl_seat.get_pointer` path broke used to fail the boot and
/// now does not. That is a real loss of coverage, kept because a terminal
/// demanding a device it never uses would be a client lying about its needs to
/// hold a test property up.
///
/// DUPLICATED as `MARKER` in td-compositor/src/ready.rs and pinned by its recipe.
pub const TD_TERM_RUNTIME_MARKER: &str = "TD-TERM-READY";

/// Printed by the compositor for each input device that ANSWERED `EVIOCGABS`
/// with a span on both axes — QEMU's virtio tablet, on the image the oracle
/// boots.
///
/// It is the one property in that path a unit test cannot hold up: the gate
/// machine has no absolute device, so a compositor that never asked, or that
/// asked and discarded the answer, passes every test in `input.rs`. This is
/// where a real device answering becomes observable, and it carries the SPAN
/// because the span is what the mapping divides by and a wrong one is
/// invisible everywhere else: only a span of ZERO is refused, so `0..1` is
/// admitted and maps every report to one of two positions. Nothing parses the
/// numbers — this is latched as a substring — so they are for a person reading
/// a console, the only thing that can tell a plausible range from the device's
/// real one.
///
/// It is emitted from the reader, off the value the mapping itself uses, and
/// not beside the `EVIOCGABS` that produced it: an answer dropped between the
/// ask and the use would otherwise leave this line printed over a device read
/// as relative.
///
/// DUPLICATED as the literal in td-compositor/src/input.rs. The compositor
/// recipe pins the source emit to this value.
pub const TD_POINTER_ABSOLUTE_MARKER: &str = "TD-POINTER-ABSOLUTE";

/// Printed by `/etc/bootsuccess`, as the unprivileged login user, once the RUNNING
/// kernel has been observed to carry the sandbox features
/// `recipes/src/recipes/linux-x86-64.rs` pins for APPLICATIONS.md §0 — user, pid,
/// uts and net namespaces, each with a non-zero ucount ceiling; seccomp with BPF
/// filtering; inotify; and cgroup v2 with the pids controller enabled.
///
/// Not `CONFIG_MEMCG`, which is pinned and guarded in the recipe but has no runtime
/// witness until something mounts cgroup2: memcg registers its v1 interface only
/// under `CONFIG_MEMCG_V1`, so `proc_cgroupstats_show` filters `memory` out of
/// `/proc/cgroups` entirely. `cgroup.controllers` answers it, and that arrives with
/// td-svc's delegation. This marker therefore means "every symbol with a witness",
/// which is not the same as "every symbol pinned" — stated because the difference is
/// exactly one controller and reading it as the stronger claim is the mistake.
///
/// The build already greps the resolved `.config`, so this is not that check
/// repeated. What it adds is that the kernel the image BOOTS is the kernel that
/// config described: a pin only constrains the producer, and the image's kernel
/// could be replaced, rebuilt from a stale tree, or selected from a deployment
/// nobody re-checked. §0 asks for a regression to red the IMAGE rather than the
/// first application, and only a runtime observation can do that.
///
/// It reads `/proc` rather than issuing `unshare(2)` and `seccomp(2)`, and that is
/// a real limit rather than a preference. Those two calls are surface #9's, which
/// arrives with td-jail; nothing on the image today may issue them, and inventing
/// a prober for this rung would mean an `unsafe` surface added outside the crate
/// that owns it. So this asserts the kernel is CAPABLE and the functional half —
/// that an unprivileged `unshare(CLONE_NEWUSER|CLONE_NEWNS)` actually returns 0
/// and a trivial allow-all filter installs — lands with td-jail, where the
/// syscalls are in the roster. The gap is narrow: `/proc/self/ns/user` exists if
/// and only if `CONFIG_USER_NS`, and `Seccomp_filters:` appears in
/// `/proc/self/status` if and only if `CONFIG_SECCOMP_FILTER`, so what is
/// unproven here is the sysctl and LSM policy around those calls, not the
/// features themselves. `/proc/sys/user/max_user_namespaces` covers the one
/// sysctl that can turn a compiled-in USER_NS into an EPERM.
pub const TD_SANDBOX_KERNEL_MARKER: &str = "TD-SANDBOX-KERNEL-OK";

/// Kernel-cmdline token the headless `qemu-boot-system` oracle appends so the greeter
/// waits for the root-owned health/update transaction and then exits. `tty-session`
/// turns that exit into a clean VM poweroff. Without it, the greeter is interactive.
pub const AUTOTEST_CMDLINE_TOKEN: &str = "td.autotest=1";
/// Caps greeter completion and failed-boot parking below the host QEMU timeout.
/// Boot time consumes the same host budget, whose deadline remains the final backstop.
pub const BOOT_SUCCESS_WAIT_CMDLINE_PREFIX: &str = "td.boot-success-wait=";

/// Kernel-cmdline token for boot one of the persistence oracle. `/etc/rootcheck`
/// writes and syncs the fixed marker below `/var` before the greeter self-exits.
pub const PERSIST_WRITE_CMDLINE_TOKEN: &str = "td.persist=write";

/// Kernel-cmdline token for boot two of the persistence oracle. `/etc/rootcheck`
/// emits `SYSTEM_PERSIST_READ_MARKER` only after reading boot one's exact bytes.
pub const PERSIST_READ_CMDLINE_TOKEN: &str = "td.persist=read";

/// Kernel-cmdline token for the transactional-update oracle. The root-owned health
/// target installs the fixture candidate from the read-only top-volume view.
pub const DEPLOY_INSTALL_CMDLINE_TOKEN: &str = "td.deploy=install";

/// A valid ed25519 public key that signed NOTHING, staged on the fixture volume
/// beside the real trust root. The oracle's update pass runs three times: under
/// this key, which must be REFUSED; over an empty channel, which must be quiet;
/// and then under the real one, which must INSTALL. Without the first, the whole
/// trusted-key argument could be ignored by td-boot and every assertion would
/// still pass — the candidate is signed either way.
///
/// Volume-relative, because the two sides need it differently: the harness joins
/// it to the seed tree, and the boot script joins it to `/run/td-volume`.
pub const DEPLOY_WRONG_KEY: &str = "td/oracle-wrong.pub";

/// An EMPTY channel, staged beside the real one so the oracle can exercise the
/// state an up-to-date machine is in almost all of the time: `update` must exit
/// 0 and print nothing. That is the path a timer takes on every tick that has
/// no work, so a verb that errored there would fail a machine continuously —
/// and no gate can boot a VM to notice.
///
/// A second directory rather than the real channel emptied, because the real one
/// has to keep its candidate for the pass that follows.
pub const DEPLOY_IDLE_CHANNEL: &str = "td/incoming-idle";

/// Kernel-cmdline token used only by the boot-attempt oracle. The login profile blocks
/// before its greeter milestone and an isolated root-owned watchdog reboots the target.
pub const BOOT_FAIL_TARGET_CMDLINE_TOKEN: &str = "td.boot-fail-target=1";

// ── system-x86-64 networking markers (link-up + DHCP, re td-netd) ─────────────────
// The static td-netd daemon brings the link up and DHCP-configures it at sysinit on
// every boot (a NIC-less boot is a clean no-op). Under the `td.nettest=1` token the
// headless `qemu-boot-net` oracle appends, `/etc/netup` additionally SELF-TESTS the
// stack — resolve a name via the DHCP-provided nameserver, then TCP-reach it — and
// prints these markers on ttyS0. SINGLE SOURCE OF TRUTH shared by `/etc/netup` (baked
// by the recipe) and the oracle so they never desync.

/// Printed by `/etc/netup` once `td-netd up` has brought the link up and applied a
/// DHCP lease (address + netmask + default route, resolv.conf written). Emitted only
/// under `NETTEST_CMDLINE_TOKEN` so a normal or NIC-less boot never false-asserts it.
pub const SYSTEM_NET_UP_MARKER: &str = "TD-NET-UP-OK";

/// Printed by `/etc/netup` once `td-netd resolve` returns an address for the test
/// host via the DHCP-provided nameserver — proves td-netd's own (NSS-free) DNS client
/// works end to end against qemu user-net's resolver.
pub const SYSTEM_NET_RESOLVE_MARKER: &str = "TD-NET-RESOLVE-OK";

/// Printed by `/etc/netup` once `td-netd reach` opens a TCP connection to the test
/// host — the "reach a host" half of the QEMU user-net test.
pub const SYSTEM_NET_REACH_MARKER: &str = "TD-NET-REACH-OK";

/// Kernel-cmdline token the headless `qemu-boot-net` oracle appends so `/etc/netup`
/// runs the resolve+reach self-test (and prints the three markers above). Absent it
/// (normal boot, or the `-nic none` `qemu-boot-system` oracle), td-netd still brings
/// the link up but the self-test and its markers are skipped.
pub const NETTEST_CMDLINE_TOKEN: &str = "td.nettest=1";

/// Fixed resolve/reach target for the self-test. qemu user-net forwards DNS (via
/// 10.0.2.3) and NATs outbound TCP, so a stable public anycast host answers both a
/// DNS A-query and a TCP connect; `NETTEST_DEFAULT_PORT` is DNS-over-TCP (53), which
/// that host serves reliably. `/etc/netup` compiles these in — there is no runtime
/// cmdline override (that would need argument parsing in the boot shell).
pub const NETTEST_DEFAULT_HOST: &str = "one.one.one.one";
pub const NETTEST_DEFAULT_PORT: &str = "53";

// ── kexec-spike-x86-64 two-kernel boot markers (Phase-0 kexec spike) ─────────────
// The spike proves the source-built kernel can kexec_file_load(2) + reboot(KEXEC) a
// SECOND kernel start under qemu TCG. ONE qemu run boots the outer kernel + outer
// initramfs; the outer /init runs td-kexec to jump into an inner kernel + inner
// initramfs (a kexec is NOT a machine reset, so `-no-reboot` does not fire on it),
// and the inner /init prints STAGE2 before a real `reboot -f` exits qemu. Both markers
// are SINGLE SOURCE OF TRUTH shared by the spike recipe's two /init scripts and the
// host-side `qemu-boot-kexec` oracle so they can never silently desync.

/// Printed by the OUTER /init on ttyS0 once the first kernel reaches userspace, just
/// before it execs td-kexec. Proves stage-1 ran; the oracle asserts it as a diagnostic
/// that the second boot was initiated by our helper, not a stray direct boot.
pub const KEXEC_STAGE1_MARKER: &str = "TD-KEXEC-BOOT1";

/// Printed by the INNER /init on ttyS0 once the kexec'd SECOND kernel reaches userspace.
/// The spike's success criterion: it cannot appear unless kexec_file_load(2) +
/// reboot(LINUX_REBOOT_CMD_KEXEC) actually loaded and jumped into the second kernel.
/// The `qemu-boot-kexec` oracle keys on it (and additionally asserts STAGE1).
pub const KEXEC_STAGE2_MARKER: &str = "TD-KEXEC-BOOT2";

/// Shell (for `sh -c`) asserting that `initramfs` is a COMPLETE, well-formed newc cpio
/// carrying the bootable busybox userland. Shared by the `linux-x86-64` producer rung
/// and the `linux-x86-64-test` rung so the two checks cannot drift.
///
/// Uses `busybox cpio -t` for a REAL newc parse whose listing is exact MEMBER NAMES —
/// unlike the previous payload greps (`grep -a TRAILER` / `grep -a busybox`), which are
/// satisfied by strings EMBEDDED IN THE BUSYBOX BINARY itself (it contains both
/// "TRAILER!!!" and "busybox"), so an archive truncated after the marker but before its
/// real trailer passed every assertion. What actually guarantees COMPLETENESS is
/// requiring EVERY expected member name in the listing: any truncation that drops a
/// member (busybox, /init, …) reds on the missing name. `cpio -t`'s exit code is a
/// secondary signal — it reds on a mid-record `short read`, but can still exit 0 on an
/// archive truncated cleanly at a header boundary (no TRAILER), which is exactly why the
/// member-name assertions, not the exit code, carry the load. The `{marker}` and
/// `{erofs_marker}` payload greps additionally prove the /init script's CONTENT (not
/// just its name) is packed — cpio -t validates structure, not bytes — covering both
/// the userland marker and the read-only-erofs probe marker the boot oracles assert on.
///
/// `busybox` is the absolute path to the busybox multi-call binary; `grep`/`od`/`wc`
/// come from the mesboot0 userland, so callers keep `PATH = mesboot0_path()`.
pub fn initramfs_cpio_shape_check(initramfs: &str, busybox: &str) -> String {
    let marker = USERLAND_MARKER;
    let erofs_marker = EROFS_MARKER;
    format!(
        "sz=$(wc -c < '{initramfs}'); \
         [ \"$sz\" -ge 65536 ] || {{ echo \"initramfs.cpio: implausibly small ($sz bytes) — the static busybox alone is ~1 MiB\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -N 6 '{initramfs}'); \
         [ \"$1$2$3$4$5$6\" = 303730373031 ] || {{ echo 'initramfs.cpio: missing the newc cpio magic 070701' >&2; exit 1; }}; \
         list=$('{busybox}' cpio -t < '{initramfs}' 2>/dev/null) || {{ echo 'initramfs.cpio: busybox cpio -t could not parse the archive (truncated/corrupt newc stream — no valid TRAILER)' >&2; exit 1; }}; \
         for m in init bin/busybox bin/sh dev/console; do \
             printf '%s\\n' \"$list\" | grep -q -x -F \"$m\" || {{ echo \"initramfs.cpio: cpio member '$m' missing — the bootable userland is incomplete\" >&2; exit 1; }}; \
         done; \
         grep -q -a {marker} '{initramfs}' || {{ echo 'initramfs.cpio: /init marker not packed — the boot script the qemu tool asserts on is missing' >&2; exit 1; }}; \
         grep -q -a {erofs_marker} '{initramfs}' || {{ echo 'initramfs.cpio: /init erofs marker not packed — the read-only-root probe the qemu-boot-erofs tool asserts on is missing' >&2; exit 1; }}"
    )
}

/// Unpack tarball input NAME into DEST (top-level dir stripped) with the
/// ENGINE's own readers — no unpacker packages in the sandbox.
pub fn unpack_into(input: &str, dest: &str) -> Vec<Step> {
    vec![Step::Unpack {
        input: format!("{{in:{input}}}"),
        dest: dest.into(),
        keep_top: false,
    }]
}

/// Unpack tarball input NAME into DEST with the top-level dir KEPT (the gcc
/// prereqs land as gmp-X.Y.Z/ subdirs that then get version-free symlinks).
pub fn unpack_keep_top(input: &str, dest: &str) -> Vec<Step> {
    vec![Step::Unpack {
        input: format!("{{in:{input}}}"),
        dest: dest.into(),
        keep_top: true,
    }]
}

/// Apply a patch input with the td-built patch rung: `patch --force -p1 -i X`
/// in {src}, env-cleared (exactly the ladder's `env -i patch …`).
pub fn apply_patch(patch_rung: &str, patch_input: &str) -> Step {
    Step::run(
        "{src}",
        &[
            &format!("{{in:{patch_rung}}}/bin/patch"),
            "--force",
            "-p1",
            "-i",
            &format!("{{in:{patch_input}}}"),
        ],
    )
}

/// `sed -i EXPR FILE…` via the td-built `sed-mesboot0` on `mesboot0_path()` (dir
/// {src} unless absolute). `sed -i` writes a temp file and renames, so it never
/// touches stdin or a non-syncable fd — the mes-libc bugs sed-mesboot0 patches
/// don't apply here.
pub fn sed_i(expr: &str, files: &[&str]) -> Step {
    let mut argv: Vec<&str> = vec!["{in:sed-mesboot0}/bin/sed", "-i", expr];
    argv.extend_from_slice(files);
    Step::run("{src}", &argv).env("PATH", &mesboot0_path())
}

/// Relocate every staged glibc GNU ld script under `lib/*.so` to bare member
/// names by stripping the configured store prefix. Real ELF shared objects are
/// left untouched.
pub fn relocate_ld_scripts(stage: &str, store_prefix: &str) -> Step {
    Step::RelocateLdScripts {
        dir: format!("{stage}/lib"),
        prefix: store_prefix.into(),
    }
}

/// Make libtool assemble a static library (e.g. libstdc++.a) from its
/// convenience archives WITHOUT `find` (re #469, #477's retired-axis guard).
///
/// `ltmain.sh`'s `func_extract_archives` merges each per-language convenience
/// archive (libc++11convenience.a &c.) into the final `.a` by `cd`-ing into a
/// scratch dir, `ar x`-ing the members flat into it, then enumerating them with
/// `find $my_xdir -name \*.o -print`. The mesboot userland ships no `find`
/// (retired in #477), so that enumeration returns nothing, `ar rc` appends
/// nothing, and the archive silently ends up with only its directly-compiled
/// objects — a partial libstdc++.a missing std::string/std::vector/iostream.
/// GCC's own C++ generators (gensupport, genattrtab under GCC 14) then fail to
/// link against it.
///
/// `ar x` extracts object members flat, one level deep (libtool's own `ar t`
/// pass aborts on duplicate member names within an archive), so a *terminal*
/// glob over `$my_xdir` captures exactly what the recursive `find` would — and
/// unlike a non-terminal glob it expands correctly under bash-mesboot (bash
/// 2.05b on mes libc). `test -f` drops the no-match literal; `printf '%s\n'`
/// prints one path per line, exactly like `find … -print`.
///
/// We replace only the `find` COMMAND, leaving libtool's surrounding backticks
/// and its `| [sort |] $NL2SP` post-pipe intact: that command is byte-identical
/// across the two libtool versions td builds (GCC 4.9.4 pipes `find … | $NL2SP`;
/// GCC 14.3.0 pipes `find … | sort | $NL2SP` for a deterministic archive), so
/// one edit serves both and 14.3.0 keeps its sort. The `count: 1` fail-closes if
/// a future source bump drifts the line. This ELIMINATES the find need rather
/// than satisfying it with a host/find provider.
pub fn libtool_extract_without_find(ltmain: &str) -> Step {
    Step::substitute_text(
        ltmain,
        vec![TextEdit::new(
            "find $my_xdir -name \\*.$objext -print -o -name \\*.lo -print",
            "for f in $my_xdir/*.$objext $my_xdir/*.lo; do test -f \"$f\" && printf '%s\\n' \"$f\"; done",
            1,
        )],
    )
}

/// Make GCC 14's libstdc++ stamp rules independent of the absent `date` tool.
///
/// The stamp contents are never read; make uses only each file's existence and
/// mtime. A shell-builtin no-op plus redirection therefore preserves the rule's
/// semantics without adding another bootstrap-userland executable. Patch both
/// the ordinary convenience archive and the optional debug-tree stamp so every
/// C++-enabled GCC 14 rung has the same host-free source shape.
pub fn gcc14_libstdcxx_stamp_fixups() -> Step {
    Step::substitute_text(
        "{src}/libstdc++-v3/src/Makefile.in",
        vec![
            TextEdit::new(
                "\tdate > stamp-libstdc++convenience;",
                "\t: > stamp-libstdc++convenience;",
                1,
            ),
            TextEdit::new("\tdate > stamp-debug;", "\t: > stamp-debug;", 1),
        ],
    )
}

/// Select GCC's cp-based include-tree installer when bootstrap `tar` is absent.
///
/// Modern GCC configure otherwise chooses `install-headers-tar` for the native
/// i686/x86_64 hosts used by this ladder. The source ships an equivalent
/// `install-headers-cp` target, backed by the already-declared mesboot coreutils.
pub fn gcc_install_headers_without_tar() -> Step {
    Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "INSTALL_HEADERS_DIR = @build_install_headers_dir@",
            "INSTALL_HEADERS_DIR = install-headers-cp",
            1,
        )],
    )
}

/// The bash-mesboot `configure` fixups every modern GCC rung needs before its
/// `configure` runs (re #469). bash 2.05b (mes libc) cannot expand the
/// non-terminal `*/config-lang.in` globs configure uses to discover language
/// front-ends, and its automake dependency-style probe runs each depmode as
/// `env $depcmd` but the mesboot userland ships no `env` (so every depmode exits
/// 127 and the probe aborts with "no usable dependency style found"). `LANGS`
/// is the exact, sorted set of language fragments shipped by the selected GCC
/// tarball. Pre-expand both globs to that set (a working shell's expansion
/// verbatim) and rewrite the probe to the POSIX builtin `eval "$depcmd"`.
/// `--enable-languages` still selects only what each rung asks for. The edit
/// counts fail-closed if a future source bump drifts.
pub fn gcc_configure_fixups(langs: &[&str]) -> Vec<Step> {
    let top = langs
        .iter()
        .map(|l| format!("${{srcdir}}/gcc/{l}/config-lang.in"))
        .collect::<Vec<_>>()
        .join(" ");
    let gcc = langs
        .iter()
        .map(|l| format!("${{srcdir}}/{l}/config-lang.in"))
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        Step::substitute_text(
            "{src}/configure",
            vec![TextEdit::new("${srcdir}/gcc/*/config-lang.in", &top, 2)],
        ),
        Step::substitute_text(
            "{src}/gcc/configure",
            vec![TextEdit::new("${srcdir}/*/config-lang.in", &gcc, 2)],
        ),
        Step::substitute_text(
            "{src}/gcc/configure",
            vec![TextEdit::new("env $depcmd", "eval \"$depcmd\"", 1)],
        ),
        Step::substitute_text(
            "{src}/libcpp/configure",
            vec![TextEdit::new("env $depcmd", "eval \"$depcmd\"", 1)],
        ),
    ]
}

/// GCC 14.3.0 ships twelve language fragments. Every GCC 14 rung uses the same
/// release tarball, so this wrapper keeps their call sites declarative while the
/// shared implementation also serves the GCC 10.5.0 bridge.
pub fn gcc14_configure_fixups() -> Vec<Step> {
    gcc_configure_fixups(&[
        "ada", "c", "cp", "d", "fortran", "go", "jit", "lto", "m2", "objc", "objcp", "rust",
    ])
}

/// Disable GCC's build-host signal-name self-test. The bootstrap libc's
/// `sys_siglist` is deliberately a stub, so executing this development-only
/// diagnostic crashes even when the compiler itself is sound. Installed
/// compiler behavior is covered by rung-specific checks and downstream builds.
pub fn gcc_disable_selftest() -> Step {
    Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "all.internal: start.encap rest.encap doc selftest",
            "all.internal: start.encap rest.encap doc",
            1,
        )],
    )
}

/// Make glibc 2.41's architecture selection and syscall generation work with
/// the mesboot shell/userland. Its configure asks bash-mesboot to expand the
/// non-terminal `sysdeps/*/preconfigure` glob; that shell leaves it literal, so
/// x86_64 never becomes x86_64/64 and the matching arch-syscall.h is omitted.
/// Pre-expand the exact sorted fragment set shipped by the pinned release.
///
/// make-syscalls.sh also uses GNU grep's newer `-o` option to enumerate the
/// byte offsets of `U` argument markers, while the declared grep-mesboot0 2.4
/// predates that option. The awk loop emits the identical zero-based `N:U`
/// records from the same colon-prefixed signature using the already-declared
/// gawk provider. Finally, elf/Makefile repeats the non-terminal
/// `build/*/stamp.os` glob while generating librtld.mk; GNU make's wildcard
/// function supplies the same existing-file set without relying on the shell.
pub fn glibc241_host_free_fixups() -> Vec<Step> {
    let preconfigure = [
        "aarch64",
        "alpha",
        "arc",
        "arm",
        "csky",
        "hppa",
        "i386",
        "loongarch",
        "m68k",
        "microblaze",
        "mips",
        "or1k",
        "powerpc",
        "riscv",
        "s390",
        "sh",
        "sparc",
        "x86_64",
    ]
    .iter()
    .map(|arch| format!("${{srcdir}}/sysdeps/{arch}/preconfigure"))
    .collect::<Vec<_>>()
    .join(" ");
    vec![
        Step::substitute_text(
            "{src}/configure",
            vec![TextEdit::new(
                "$srcdir/sysdeps/*/preconfigure",
                &preconfigure,
                1,
            )],
        ),
        Step::substitute_text(
            "{src}/sysdeps/unix/make-syscalls.sh",
            vec![TextEdit::new(
                "grep -ob U",
                r#"awk '{ for (i = 1; i <= length($0); ++i) if (substr($0, i, 1) == "U") print i - 1 ":U" }'"#,
                1,
            )],
        ),
        Step::substitute_text(
            "{src}/elf/Makefile",
            vec![TextEdit::new(
                "$(common-objpfx)*/stamp.os",
                "$(wildcard $(common-objpfx)*/stamp.os)",
                1,
            )],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use crate::catalog;
    use crate::types::{Recipe, Step};
    use std::collections::HashSet;

    const POST_BOOTSTRAP_BOUNDARY_OUTPUTS: [&str; 5] = [
        "rust-toolchain",
        "gcc-x86-64-self",
        "binutils-x86-64-self",
        "glibc-x86-64",
        "busybox-x86-64",
    ];
    // These independent target artifacts and checks deliberately run before
    // self-hosting but are not ancestors of rust-toolchain. New recipes default
    // to the far side of the boundary and must not grow this list silently.
    const BOOTSTRAP_SIDE_CONSUMERS: [&str; 18] = [
        "btrfs-progs-x86-64",
        "btrfs-progs-x86-64-test",
        "busybox-test",
        "elfutils-x86-64",
        "elfutils-x86-64-test",
        "flex-x86-64",
        "flex-x86-64-test",
        "gcc-10-bridge-test",
        "gcc-x86-64-native-test",
        "gcc-x86-64-stage2-test",
        "glibc-241",
        "hello",
        "hello-test",
        "linux-x86-64",
        "linux-x86-64-test",
        "make-test",
        "sed-mesboot",
        "util-linux-libs-x86-64",
    ];
    const SELF_HOSTED_PHASE_MARKERS: [&str; 3] =
        ["rust-toolchain", "gcc-x86-64-self", "binutils-x86-64-self"];
    const POST_BOOTSTRAP_PROTECTED_INPUT_EXCEPTIONS: [(&str, &str); 6] = [
        // Identity/codegen audits deliberately look back across the boundary.
        ("rust-userland-auto-test", "rust-stage0"),
        ("gcc-x86-64-self-test", "gcc-x86-64-native"),
        ("gcc-x86-64-self-test", "binutils-x86-64-native"),
        // Later boot artifacts consume the pre-self kernel and its cpio packer.
        ("kexec-spike-x86-64", "linux-x86-64"),
        ("system-x86-64", "linux-x86-64"),
        // ...and the installer consumes the pre-self FILESYSTEM tool, for the
        // same reason: `td-install/DESIGN.md`'s D7 approves `mkfs.btrfs` as the
        // one third-party program on the install path, because writing a Btrfs
        // formatter in Rust would produce a volume that mounts and then loses
        // data. btrfs-progs is a C program built by the GNU toolchain and
        // belongs on the bootstrap side; nothing about it moves post-boundary,
        // so the consumer declares the edge instead.
        ("td-install-test", "btrfs-progs-x86-64"),
    ];
    const RECIPE_SHEBANG_INTERPRETERS: [&str; 2] = [super::SH, super::POST_BOOTSTRAP_SH];
    const GUEST_LITERAL_SHEBANGS: [(&str, &str); 12] = [
        ("linux-x86-64", "{root}/initramfs/init"),
        ("kexec-spike-x86-64", "{root}/inner-init"),
        ("kexec-spike-x86-64", "{root}/outer-init"),
        ("system-x86-64", "{root}/selector-init"),
        ("system-x86-64", "{root}/deployment-init"),
        ("system-x86-64", "{root}/real-root/etc/autologin"),
        ("system-x86-64", "{root}/real-root/etc/tty-session"),
        ("system-x86-64", "{root}/real-root/etc/shutdown"),
        ("system-x86-64", "{root}/real-root/etc/rootcheck"),
        ("system-x86-64", "{root}/real-root/etc/netup"),
        ("system-x86-64", "{root}/real-root/etc/bootsuccess"),
        ("system-x86-64", "{root}/real-root/etc/bootfail"),
    ];
    type RunStep<'a> = (&'a [String], &'a [(String, String)], &'a str);

    /// True if `cmd` appears in `s` as a whole command word. Every
    /// non-alphanumeric character is a boundary EXCEPT `_`, so `/usr/bin/find`,
    /// `find`, and `find;` all surface the word `find`, while `findutils`,
    /// `found`, `x86-64` and `find_map` do not.
    ///
    /// `_` is the exception because no shell command is named `find_map`: a body
    /// that says so is naming an identifier, not spawning findutils. That
    /// matters because the scanned surface is not only scripts — eight td
    /// recipes write their Rust MODULES out with `WriteFile` (and four more
    /// embed a single source), so every identifier in a shipped source is read
    /// by this, and without the exception `outcomes.iter().find_map(…)` is a
    /// recipe invoking `find`.
    ///
    /// It frees the IDENTIFIER and nothing else. A bare `find` in a comment is
    /// still an invocation, deliberately: the token is what a quoted
    /// `Command::new("find")` leaves behind too, and this cannot tell the two
    /// apart. So a shipped module may say `find_map` but still not the bare
    /// English word.
    fn invokes(s: &str, cmd: &str) -> bool {
        s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|t| t == cmd)
    }

    /// Every catalog-authored text of a step that becomes a command or an
    /// interpreted script/Makefile: Run argv, ANY WriteFile body (baked
    /// Makefiles/kaem scripts are written `exec: false` and then run over by a
    /// Run step), ToolFarm links, and the `to` side of the literal SubstituteText
    /// edits (the host-free `patch`/`sed` stand-in). Engine-native steps that
    /// carry only paths (Unpack/CopyTree/Symlink/PatchShebangs/…) cannot invoke a
    /// tool, so they contribute nothing. Shared by the catalog-walk guard and its
    /// coverage test so both exercise exactly the same extraction.
    ///
    /// Only a SubstituteText's `to` is a command surface: `from` is the text being
    /// REMOVED from a source file, so a `find`/`xargs` there is being deleted, not
    /// invoked (e.g. the gcc-mesboot ltmain.sh edit that replaces libtool's
    /// convenience-archive `find` with a bash-mesboot glob loop). Scanning `from`
    /// would misfire on exactly the patches that eliminate a host-tool call.
    fn command_texts(step: &Step) -> Vec<&str> {
        match step {
            Step::Run { argv, .. } => argv.iter().map(String::as_str).collect(),
            Step::WriteFile { content, .. } => vec![content.as_str()],
            Step::ToolFarm { links } => links
                .iter()
                .flat_map(|(a, b)| [a.as_str(), b.as_str()])
                .collect(),
            Step::SubstituteText { edits, .. } => edits.iter().map(|e| e.to.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    fn direct_recipe_inputs(recipe: &Recipe) -> Vec<&str> {
        recipe
            .inputs
            .iter()
            .flatten()
            .chain(recipe.native_inputs.iter().flatten())
            .map(String::as_str)
            .collect()
    }

    fn collect_recipe_closure(
        recipes: &[(&'static str, Recipe)],
        stem: &str,
        closure: &mut HashSet<String>,
    ) {
        let Some((_, recipe)) = recipes.iter().find(|(candidate, _)| *candidate == stem) else {
            return;
        };
        if !closure.insert(stem.to_string()) {
            return;
        }
        for input in direct_recipe_inputs(recipe) {
            collect_recipe_closure(recipes, input, closure);
        }
    }

    fn bootstrap_partition(
        recipes: &[(&'static str, Recipe)],
    ) -> (HashSet<String>, HashSet<String>) {
        let mut bootstrap_recipes = HashSet::new();
        collect_recipe_closure(recipes, "rust-toolchain", &mut bootstrap_recipes);
        let mut bootstrap_interior = bootstrap_recipes.clone();
        for boundary_output in POST_BOOTSTRAP_BOUNDARY_OUTPUTS {
            assert!(
                bootstrap_interior.remove(boundary_output),
                "post-bootstrap boundary output is absent from rust-toolchain closure: \
                 {boundary_output}"
            );
        }
        (bootstrap_recipes, bootstrap_interior)
    }

    #[test]
    fn bootstrap_side_consumers_remain_pre_self_hosting() {
        let recipes = catalog::all();
        let (bootstrap_recipes, bootstrap_interior) = bootstrap_partition(&recipes);
        for allowed_stem in BOOTSTRAP_SIDE_CONSUMERS {
            let recipe = recipes
                .iter()
                .find(|(stem, _)| *stem == allowed_stem)
                .map(|(_, recipe)| recipe);
            assert!(
                recipe.is_some(),
                "bootstrap-side consumer must remain in the catalog: {allowed_stem}"
            );
            let Some(recipe) = recipe else {
                continue;
            };
            assert!(
                !bootstrap_recipes.contains(allowed_stem),
                "bootstrap-side consumer moved into the rust-toolchain closure: {allowed_stem}"
            );
            assert!(
                direct_recipe_inputs(recipe)
                    .iter()
                    .any(|input| bootstrap_interior.contains(*input)),
                "bootstrap-side consumer no longer uses a bootstrap input: {allowed_stem}"
            );
            let mut closure = HashSet::new();
            collect_recipe_closure(&recipes, allowed_stem, &mut closure);
            assert!(
                !SELF_HOSTED_PHASE_MARKERS
                    .iter()
                    .any(|marker| closure.contains(*marker)),
                "bootstrap-side consumer crossed the self-hosted boundary: {allowed_stem}"
            );
        }
    }

    fn post_bootstrap_back_edges(recipes: &[(&'static str, Recipe)]) -> Vec<(String, String)> {
        let (bootstrap_recipes, bootstrap_interior) = bootstrap_partition(recipes);
        let mut protected_inputs = bootstrap_interior;
        protected_inputs.extend(BOOTSTRAP_SIDE_CONSUMERS.iter().map(|stem| stem.to_string()));
        let mut back_edges = Vec::new();
        for (stem, recipe) in recipes {
            if bootstrap_recipes.contains(*stem) || BOOTSTRAP_SIDE_CONSUMERS.contains(stem) {
                continue;
            }
            for input in direct_recipe_inputs(recipe) {
                let boundary_probe = POST_BOOTSTRAP_PROTECTED_INPUT_EXCEPTIONS.iter().any(
                    |(allowed_stem, allowed_input)| stem == allowed_stem && input == *allowed_input,
                );
                if protected_inputs.contains(input) && !boundary_probe {
                    back_edges.push((stem.to_string(), input.to_string()));
                }
            }
        }
        back_edges.sort();
        back_edges
    }

    /// Only the Rust-toolchain closure and explicitly reviewed bootstrap-side
    /// consumers may declare an internal tool rung. Every other catalog recipe
    /// defaults to the far side of the boundary. The exact exceptions are
    /// separately reviewed audit or boot-artifact edges.
    #[test]
    fn post_bootstrap_recipes_use_only_reviewed_boundary_inputs() {
        let back_edges = post_bootstrap_back_edges(&catalog::all());
        assert!(
            back_edges.is_empty(),
            "post-bootstrap recipes directly use protected bootstrap inputs: {back_edges:?}"
        );
    }

    #[test]
    fn post_bootstrap_boundary_guard_rejects_a_new_back_edge() {
        let mut recipes = catalog::all();
        recipes.push((
            "synthetic-post-bootstrap",
            Recipe::mesboot("synthetic-post-bootstrap", "0")
                .native_inputs(&["busybox-x86-64"])
                .inputs_owned(vec!["bash-mesboot".into(), "binutils-x86-64-native".into()]),
        ));
        let mut synthetic_closure = HashSet::new();
        collect_recipe_closure(&recipes, "synthetic-post-bootstrap", &mut synthetic_closure);
        for marker in SELF_HOSTED_PHASE_MARKERS {
            assert!(
                !synthetic_closure.contains(marker),
                "the negative control must prove the marker-free boundary"
            );
        }
        assert_eq!(
            post_bootstrap_back_edges(&recipes),
            vec![
                ("synthetic-post-bootstrap".into(), "bash-mesboot".into()),
                (
                    "synthetic-post-bootstrap".into(),
                    "binutils-x86-64-native".into(),
                ),
            ]
        );
    }

    #[test]
    fn executable_write_files_use_declared_shebangs() {
        let mut seen_guest_shebangs = HashSet::new();
        let expected_guest_shebangs: HashSet<(String, String)> = GUEST_LITERAL_SHEBANGS
            .iter()
            .map(|(stem, path)| (stem.to_string(), path.to_string()))
            .collect();
        let mut bad = Vec::new();
        for (stem, recipe) in catalog::all() {
            for step in recipe.steps.iter().flatten() {
                let Step::WriteFile {
                    path,
                    content,
                    exec: true,
                } = step
                else {
                    continue;
                };
                let shebang = content.lines().next().unwrap_or_default();
                if let Some(interpreter) = shebang.strip_prefix("#!{in:") {
                    let declared = interpreter
                        .find('}')
                        .and_then(|end| interpreter.get(..end))
                        .filter(|input| direct_recipe_inputs(&recipe).contains(input));
                    let approved = RECIPE_SHEBANG_INTERPRETERS
                        .iter()
                        .any(|approved| shebang == format!("#!{approved}"));
                    if declared.is_some() && approved {
                        continue;
                    }
                }
                let guest = (stem.to_string(), path.clone());
                if shebang == "#!/bin/sh" && expected_guest_shebangs.contains(&guest) {
                    seen_guest_shebangs.insert(guest);
                    continue;
                }
                bad.push((stem, path.clone(), shebang.to_string()));
            }
        }
        assert!(
            bad.is_empty(),
            "sandbox-executable WriteFile shebangs must name declared inputs: {bad:?}"
        );
        assert_eq!(
            seen_guest_shebangs, expected_guest_shebangs,
            "the literal /bin/sh exceptions must remain exact packed-guest scripts"
        );
    }

    fn linux_boundary_references_are_boot_artifacts_only(
        canonical: &str,
        cpio_references: usize,
        packed_kernel_references: usize,
        copied_kernel_references: usize,
    ) -> bool {
        let linux_token = "{in:linux-x86-64}";
        let cpio_use = format!("'{linux_token}/gen_init_cpio' -t 1 ");
        let packed_kernel = format!("file /kernel/bzImage {linux_token}/bzImage 0644 0 0");
        let copied_kernel = format!("\"{linux_token}/bzImage\"");
        canonical.matches(linux_token).count()
            == cpio_references + packed_kernel_references + copied_kernel_references
            && canonical.matches(&cpio_use).count() == cpio_references
            && canonical.matches(&packed_kernel).count() == packed_kernel_references
            && canonical.matches(&copied_kernel).count() == copied_kernel_references
    }

    #[test]
    fn linux_boundary_exceptions_are_boot_artifacts_only() {
        let recipes = catalog::all();
        for (stem, cpio_references, packed_kernel_references, copied_kernel_references) in [
            ("kexec-spike-x86-64", 2, 1, 1),
            ("system-x86-64", 2, 0, 1),
        ]
        {
            let recipe = recipes
                .iter()
                .find(|(candidate, _)| *candidate == stem)
                .map(|(_, recipe)| recipe);
            assert!(
                recipe.is_some(),
                "boot recipe must remain in the catalog: {stem}"
            );
            let Some(recipe) = recipe else {
                continue;
            };
            let canonical = recipe.to_json().to_canonical();
            assert!(
                linux_boundary_references_are_boot_artifacts_only(
                    &canonical,
                    cpio_references,
                    packed_kernel_references,
                    copied_kernel_references,
                ),
                "{stem} may use linux-x86-64 only for gen_init_cpio and bzImage"
            );
            let bypass = format!("{canonical}{{in:linux-x86-64}}/scripts/host-tool");
            assert!(
                !linux_boundary_references_are_boot_artifacts_only(
                    &bypass,
                    cpio_references,
                    packed_kernel_references,
                    copied_kernel_references,
                ),
                "another linux-x86-64 path must not fit the boot-artifact exception"
            );
            for artifact in ["gen_init_cpio", "bzImage"] {
                for suffix in [".unexpected", "-wrapper", "$suffix"] {
                    let prefix_bypass = canonical.replacen(
                        &format!("{{in:linux-x86-64}}/{artifact}"),
                        &format!("{{in:linux-x86-64}}/{artifact}{suffix}"),
                        1,
                    );
                    assert!(
                        !linux_boundary_references_are_boot_artifacts_only(
                            &prefix_bypass,
                            cpio_references,
                            packed_kernel_references,
                            copied_kernel_references,
                        ),
                        "a same-prefix linux-x86-64 path must not fit the exception"
                    );
                }
            }
            let quote_concat_bypass = canonical.replacen(
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 ",
                "'{in:linux-x86-64}/gen_init_cpio'.unexpected -t 1 ",
                1,
            );
            assert!(
                !linux_boundary_references_are_boot_artifacts_only(
                    &quote_concat_bypass,
                    cpio_references,
                    packed_kernel_references,
                    copied_kernel_references,
                ),
                "shell quote concatenation must not extend the approved executable"
            );
        }
    }

    fn stage0_command_is_identity_only(command: &str) -> bool {
        let stage0_token = "{in:rust-stage0}";
        let identity_read = "stage0='{in:rust-stage0}'; stage0_base=${stage0##*/};";
        let identity_scan = "'{in:td-txt}/bin/td-txt' grep -a -Fq -- \"$stage0_base\" ";
        if !command.contains(identity_read) || !command.contains(identity_scan) {
            return false;
        }
        let residue = command
            .replacen(identity_read, "", 1)
            .replacen(identity_scan, "", 1);
        !residue.contains(stage0_token)
            && !residue.contains("$stage0")
            && !residue.contains("${stage0")
    }

    #[test]
    fn rust_stage0_boundary_exception_is_identity_only() {
        let recipes = catalog::all();
        let recipe = recipes
            .iter()
            .find(|(stem, _)| *stem == "rust-userland-auto-test")
            .map(|(_, recipe)| recipe);
        assert!(
            recipe.is_some(),
            "rust-userland-auto-test must remain in the catalog"
        );
        let Some(recipe) = recipe else {
            return;
        };
        let stage0_token = "{in:rust-stage0}";
        assert_eq!(
            recipe
                .to_json()
                .to_canonical()
                .matches(stage0_token)
                .count(),
            2,
            "the boundary probe may name rust-stage0 once per tested binary"
        );

        let commands: Vec<&str> = recipe
            .steps
            .iter()
            .flatten()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => argv
                    .iter()
                    .find(|arg| arg.contains(stage0_token))
                    .map(String::as_str),
                _ => None,
            })
            .collect();
        assert_eq!(
            commands.len(),
            2,
            "each tested binary must keep one rust-stage0 identity read"
        );
        for command in commands {
            assert!(
                stage0_command_is_identity_only(command),
                "the boundary probe may scan for the basename but must not use rust-stage0"
            );
            let bypass = format!("{command}; \"$stage0_base/bin/rustc\" --version");
            assert!(
                !stage0_command_is_identity_only(&bypass),
                "executing a path reconstructed from stage0_base must be rejected"
            );
        }
    }

    fn self_hosted_audit_value(value: &str) -> String {
        value
            .replace("{in:gcc-x86-64-native}", "{in:gcc-x86-64-self}")
            .replace("-x86_64-native", "-x86_64-self")
            .replace("{in:binutils-x86-64-native}", "{in:binutils-x86-64-self}")
            .replace("native-c.s", "self-c.s")
            .replace("native-cxx.s", "self-cxx.s")
    }

    #[test]
    fn gcc_native_boundary_exception_is_same_codegen_only() {
        let recipes = catalog::all();
        let recipe = recipes
            .iter()
            .find(|(stem, _)| *stem == "gcc-x86-64-self-test")
            .map(|(_, recipe)| recipe);
        assert!(
            recipe.is_some(),
            "gcc-x86-64-self-test must remain in the catalog"
        );
        let Some(recipe) = recipe else {
            return;
        };
        let native_gcc_token = "{in:gcc-x86-64-native}";
        let native_binutils_token = "{in:binutils-x86-64-native}";
        let native_tokens = [native_gcc_token, native_binutils_token];
        let canonical = recipe.to_json().to_canonical();
        assert_eq!(
            canonical.matches(native_gcc_token).count(),
            2,
            "only the C and C++ native compiler probes may name gcc-native"
        );
        assert_eq!(
            canonical.matches(native_binutils_token).count(),
            4,
            "only the C and C++ native compiler probes may name binutils-native"
        );
        let run_steps: Vec<RunStep<'_>> = recipe
            .steps
            .iter()
            .flatten()
            .filter_map(|step| match step {
                Step::Run { argv, env, dir } => {
                    Some((argv.as_slice(), env.as_slice(), dir.as_str()))
                }
                _ => None,
            })
            .collect();
        let native_steps: Vec<RunStep<'_>> = run_steps
            .iter()
            .copied()
            .filter(|(argv, env, dir)| {
                argv.iter()
                    .chain(env.iter().flat_map(|(key, value)| [key, value]))
                    .any(|value| native_tokens.iter().any(|token| value.contains(token)))
                    || native_tokens.iter().any(|token| dir.contains(token))
            })
            .collect();
        assert_eq!(
            native_steps.len(),
            2,
            "the native exception is exactly the C and C++ same-codegen probes"
        );

        for (argv, env, dir) in native_steps {
            assert!(
                argv.iter().any(|arg| arg == "-S")
                    && argv
                        .iter()
                        .any(|arg| arg.ends_with("/codegen.c") || arg.ends_with("/codegen.cc"))
                    && argv
                        .iter()
                        .any(|arg| arg.ends_with("/native-c.s") || arg.ends_with("/native-cxx.s")),
                "the native compiler may only emit assembly for the codegen fixture"
            );
            let paired_argv: Vec<String> = argv
                .iter()
                .map(|value| self_hosted_audit_value(value))
                .collect();
            let paired_env: Vec<(String, String)> = env
                .iter()
                .map(|(key, value)| (self_hosted_audit_value(key), self_hosted_audit_value(value)))
                .collect();
            let paired_dir = self_hosted_audit_value(dir);
            assert!(
                run_steps
                    .iter()
                    .any(|(candidate_argv, candidate_env, candidate_dir)| {
                        *candidate_argv == paired_argv.as_slice()
                            && *candidate_env == paired_env.as_slice()
                            && *candidate_dir == paired_dir.as_str()
                    }),
                "each native codegen probe must retain an identical self-hosted counterpart"
            );
        }
    }

    /// Dead-axis lock: GNU findutils is absent from the tool tier after an
    /// exhaustive sweep found no rung invokes ambient `find`/`xargs` (not in any Run
    /// argv, WriteFile body, ToolFarm link, or SubstituteText edit — and neither
    /// is in the autoconf `configure`/`make` vocabulary these tarballs drive).
    /// This walks the WHOLE catalog and fails if any rung reintroduces a host
    /// `find`/`xargs` invocation, which would silently need the removed PATH node
    /// back. A rung may expose one only through a ToolFarm link to an explicitly
    /// declared td-built BusyBox input; the Rust source build needs those tools.
    ///
    /// Coverage note: it scans every catalog-authored surface that becomes a
    /// command or an interpreted script/Makefile — Run argv, ANY WriteFile body
    /// (baked Makefiles/kaem scripts are written `exec: false` and then run over
    /// by a Run step), ToolFarm links, and the literal SubstituteText edits (the
    /// host-free `patch`/`sed` stand-in). Engine-native steps that carry only
    /// paths (Unpack/CopyTree/Symlink/PatchShebangs/…) cannot invoke a tool.
    #[test]
    fn no_bootstrap_step_invokes_host_find_or_xargs() {
        for (stem, recipe) in catalog::all() {
            let Some(steps) = &recipe.steps else {
                continue;
            };
            for step in steps {
                for text in command_texts(step) {
                    for cmd in ["find", "xargs"] {
                        let declared_busybox_tool =
                            recipe.native_inputs.as_ref().is_some_and(|inputs| {
                                inputs.iter().any(|input| input == "busybox-x86-64")
                            }) && matches!(
                                step,
                                Step::ToolFarm { links }
                                    if links.iter().any(|(name, target)| {
                                        name == cmd
                                            && target
                                                == "{in:busybox-x86-64}/bin/busybox"
                                    })
                            );
                        assert!(
                            !invokes(text, cmd) || declared_busybox_tool,
                            "recipe `{stem}' invokes `{cmd}' in `{text}' — \
                             GNU findutils was retired from the tool tier; a rung \
                             must expose this command through a ToolFarm link to \
                             its declared td-built busybox-x86-64 input"
                        );
                    }
                }
            }
        }
    }

    /// Proof that `command_texts` — the extraction the guard above runs — covers
    /// the interpreted-text surfaces that are NOT a `Run` argv: a baked
    /// Makefile/kaem script (`WriteFile`, `exec: false`) and the `to` side of a
    /// literal patch/sed edit (`SubstituteText`). Without this, a `find`/`xargs`
    /// reintroduced in one of those would slip past the guard.
    #[test]
    fn guard_scans_nonexec_writefile_and_substitutetext() {
        use crate::types::TextEdit;

        let baked_makefile = Step::WriteFile {
            path: "Makefile".into(),
            content: "clean:\n\tfind . -name '*.o' -delete\n".into(),
            exec: false,
        };
        let literal_edit = Step::SubstituteText {
            file: "configure".into(),
            edits: vec![TextEdit::new("rm -f x", "xargs rm -f", 1)],
        };
        for (step, cmd) in [(&baked_makefile, "find"), (&literal_edit, "xargs")] {
            assert!(
                command_texts(step).iter().any(|t| invokes(t, cmd)),
                "command_texts must scan this surface for `{cmd}'"
            );
        }
    }

    /// An identifier that CONTAINS a tool's name is not a call to it.
    ///
    /// The scanned surface includes `WriteFile` bodies, and td's own Rust
    /// modules are written out that way, so this is the difference between a
    /// gate that reads shipped source and one that forbids `find_map` or
    /// `xargs_len`. A real invocation is separated by shell metacharacters or
    /// whitespace, which `_` is not.
    ///
    /// The positive half pins the boundaries this does NOT relax — a path, a
    /// pipe, a separator — because `/` or `.` joining a word to `find` is the
    /// same argument as `_` and must keep the opposite answer. Without them a
    /// later relaxation could make `/usr/bin/find` invisible with every test
    /// still green.
    #[test]
    fn an_identifier_is_not_an_invocation() {
        for (text, cmd) in [
            ("let x = outcomes.iter().find_map(|o| o.ok());", "find"),
            ("// the word find_map appears here", "find"),
            ("fn xargs_limit() -> usize { 0 }", "xargs"),
            // Word-shaped neighbours, which the old rule already excluded and
            // this must not start admitting.
            ("findutils is retired from the tool tier", "find"),
            ("nothing found here", "find"),
            ("target x86-64 needs no xargsy tool", "xargs"),
        ] {
            assert!(!invokes(text, cmd), "`{text}' is not an invocation of `{cmd}'");
        }
        // ...and every spelling that IS one still is: a bare word, an absolute
        // PATH, after a pipe, after a separator, as the head of a line, and in
        // a substitution. The bare English word in a comment is one of them,
        // which is the limit this fix deliberately keeps.
        for (text, cmd) in [
            ("find . -name '*.o' -delete", "find"),
            ("/usr/bin/find . -type f", "find"),
            ("ls | xargs rm -f", "xargs"),
            ("cd x && find y", "find"),
            ("\tfind . -type f\n", "find"),
            ("$(find .)", "find"),
            ("// we cannot use find here", "find"),
        ] {
            assert!(invokes(text, cmd), "`{text}' IS an invocation of `{cmd}'");
        }
    }

    /// A SubstituteText's `from` is REMOVED text, not a command: a patch that
    /// deletes a `find`/`xargs` call (like the real `libtool_extract_without_find`
    /// ltmain.sh glob-loop swap) must not be flagged as reintroducing the tool.
    /// The guard scans only `to`, so a `find` in `from` with a tool-free `to` is
    /// allowed. Exercised against the actual helper so the two cannot drift.
    #[test]
    fn guard_ignores_find_on_the_removed_from_side() {
        let removes_find = super::libtool_extract_without_find("{src}/ltmain.sh");
        // The helper's `from` names `find`; its `to` (the glob loop) does not.
        assert!(
            !command_texts(&removes_find)
                .iter()
                .any(|t| invokes(t, "find")),
            "a find on the removed `from' side must not be flagged as an invocation"
        );
    }
}
