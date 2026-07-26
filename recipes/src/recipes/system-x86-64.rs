use crate::ladder::{
    mesboot0_inputs, mesboot0_path, AUTOTEST_CMDLINE_TOKEN, BOOT_FAIL_TARGET_CMDLINE_TOKEN,
    BOOT_SUCCESS_WAIT_CMDLINE_PREFIX, DEPLOY_INSTALL_CMDLINE_TOKEN, GREETER_MARKER,
    NETTEST_CMDLINE_TOKEN, NETTEST_DEFAULT_HOST, NETTEST_DEFAULT_PORT, PERSIST_READ_CMDLINE_TOKEN,
    PERSIST_WRITE_CMDLINE_TOKEN, RIPGREP_FD_RUNTIME_MARKER, SH, SSHD_MARKER,
    SYSTEM_BOOT_SUCCESS_MARKER, SYSTEM_DEPLOY_INSTALL_MARKER, SYSTEM_ETC_RO_MARKER,
    SYSTEM_NET_REACH_MARKER, SYSTEM_NET_RESOLVE_MARKER, SYSTEM_NET_UP_MARKER,
    SYSTEM_PERSIST_READ_MARKER, SYSTEM_PERSIST_WRITE_MARKER, SYSTEM_ROOT_RO_MARKER,
    SYSTEM_SHUTDOWN_MARKER, SYSTEM_STATE_OWNER_MARKER, SYSTEM_STATE_WRITABLE_MARKER,
    TD_INIT_RUNTIME_MARKER, TD_UTIL_RUNTIME_MARKER, UUTILS_RUNTIME_MARKER,
};
use crate::types::{Recipe, Step};

#[cfg(test)]
#[path = "../../../td-boot/src/protocol.rs"]
#[allow(dead_code)]
mod td_boot_protocol;

const BOOT_SUCCESS_RETRY_SECS: u8 = 3;
const BOOT_SUCCESS_RETRY_MAX_SECS: u8 = 10;
const BOOT_FAIL_PARK_WAIT_SECS: u8 = 30;
const BOOT_FAIL_PARKED: &str = "td-boot-parked-v1";

// system-x86-64 (re #541, #550): a MINIMAL, TAILORABLE Rust-first Linux
// deployment, selected from persistent Btrfs and entered through kexec onto a
// disk-backed READ-ONLY EROFS root.
//
// This is the "system definition" recipe. It composes artifacts that already exist in
// the ladder — the source-built `linux-x86-64` kernel and the td-built STATIC busybox —
// into a first-class deployment bundle:
//
//   boot/{selector-initramfs.cpio,manifest}
//   deployment/{bzImage,initramfs.cpio,root.erofs,manifest}
//
// The direct-boot selector initramfs carries static busybox, td-boot, and
// td-kexec; it has no branch that can enter a deployment directly. It verifies
// current/previous from the Btrfs volume and kexecs the selected deployment.
// That deployment's distinct initramfs requires the td.deployment handoff,
// re-verifies root.erofs, binds it to a read-only loop device, mounts @var from
// Btrfs, and switch_roots. `/etc` stays deployment-owned and immutable; `/home`
// and `/root` are root-image symlinks into `/var`. The real root is
// the store-native real root (busybox, uutils, ripgrep, and fd at their /td/store
// paths, a /bin symlink farm, and generated /etc). The typed PackErofs step invokes
// the dependency-free control-plane image writer directly; no recipe process can
// execute td-builder through PATH or argv. Strict manifests separately hash the
// selector and the three deployment payloads.
//
// The busybox init auto-logs-in a test user to a shell with a welcome banner. EDIT the
// `SYSTEM` const below to tailor the distro (hostname, users, the auto-login user, the
// login shell, the applet set). A producer-rung shape check on the deployment
// bundle and its scratch root tree is the automated build guard; the interactive
// `td-recipe-eval run` boots the selector, kexecs the verified deployment under
// host qemu, and gives you a shell. The headless `td-recipe-eval
// qemu-boot-system` asserts the deployment state machine across repeated boots.
//
// Userland strategy (v0): static busybox provides the boot/login/shell path;
// source-built Rust uutils provides the interactive core file/text userland
// with its declared glibc runtime closure.
//
// Layout: the image is STORE-NATIVE. The busybox binary is packed at its
// content-addressed /td/store/<hash>-busybox-x86-64/bin path, and /bin is a PURE symlink
// farm whose every entry (and /init) points straight into that store path. There is no
// /usr and no /sbin. Generated system config lives under immutable /etc; the other
// non-store root entries are mountpoints plus /home and /root links into /var.

/// One account materialised into `/etc/passwd`, `/etc/group`, `/etc/shadow`, and a
/// home directory. `passwordless` writes an EMPTY shadow password — convenient for
/// a throwaway VM (the auto-login path bypasses auth anyway); set it false for a
/// locked account.
struct User {
    name: &'static str,
    uid: u32,
    gid: u32,
    gecos: &'static str,
    home: &'static str,
    shell: &'static str,
    /// Supplementary groups; the primary group is `name`. NOTE: `build_group` only
    /// materialises `"wheel"` (gid 10) today; declaring any other supplementary group
    /// would be silently dropped from `/etc/group`, so `system_def_is_self_consistent`
    /// rejects it at `cargo test`. To support a new group, give it a gid in
    /// `build_group` first, then it may be named here.
    groups: &'static [&'static str],
    passwordless: bool,
}

/// The distro definition. EDIT THIS to tailor the system, then rebuild and
/// `td-recipe-eval run`.
struct SystemDef {
    hostname: &'static str,
    os_name: &'static str,
    os_version: &'static str,
    /// Welcome banner printed by the login shell (via `/etc/profile`).
    motd: &'static str,
    /// The user busybox getty auto-logs-in on ttyS0 (no password prompt).
    autologin: &'static str,
    users: &'static [User],
}

#[cfg(test)]
fn valid_home(uid: u32, home: &str) -> bool {
    // Home strings are embedded unquoted in generated PID-1 shell.
    if uid == 0 {
        return home == "/root";
    }
    home.strip_prefix("/home/").is_some_and(|name| {
        name != "."
            && name != ".."
            && !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    })
}

// ── EDIT THIS to tailor the distro ─────────────────────────────────────────────
const SYSTEM: SystemDef = SystemDef {
    hostname: "td",
    os_name: "td",
    os_version: "0.1",
    // NOTE: td-builder's config reader now decodes JSON as UTF-8 (the shared
    // engine/src/json.rs, same codec td-recipe-eval emits with), so a multi-byte
    // char here round-trips intact. Keeping /etc strings ASCII is still the safe
    // default for the minimal boot console — use '-' rather than an em-dash unless
    // you've confirmed the console renders the glyph.
    motd: "\n  Welcome to td - a source-built, Rust-first Linux.\n  \
           Selected from persistent Btrfs onto a read-only erofs root.\n  \
           Edit recipes/src/recipes/system-x86-64.rs (the SYSTEM const) to tailor it.\n  \
           Type 'exit' (or Ctrl-D) to power off the VM; Ctrl-A X quits qemu.\n\n",
    autologin: "tester",
    users: &[
        User {
            name: "root",
            uid: 0,
            gid: 0,
            gecos: "root",
            home: "/root",
            shell: "/bin/sh",
            groups: &[],
            passwordless: true,
        },
        User {
            name: "tester",
            uid: 1000,
            gid: 1000,
            gecos: "Test User",
            home: "/home/tester",
            shell: "/bin/sh",
            groups: &["wheel"],
            passwordless: true,
        },
    ],
};
// ────────────────────────────────────────────────────────────────────────────────

/// The real-root `/bin` is a symlink farm split across FOUR multicall binaries, each
/// dispatching on argv[0]'s basename: the static **busybox** (the shell and the login/tty
/// glue), the static Rust **td-init** (the boot glue — see `TD_INIT_FARM`), the static
/// Rust **td-util** (diagnostics), and the dynamically-linked Rust **uutils** `coreutils`
/// (the core file/text userland — #547's cutover). A name goes in exactly one list;
/// `shape_check` asserts the owning binary actually provides it.
///
/// BUSYBOX now keeps only what no Rust multicall serves: the shell (`sh`/`ash`), the login
/// chain (`getty`/`login`/`su`), the mount pair the pre-pivot initramfs needs before any
/// dynamic closure is reachable, the text tools uutils does not provide (`grep`/`sed`/`awk`),
/// and the pagers/editor. `more` stays because its uutils feature compiles a crossterm pager
/// stack into the shipped multicall, which is a dependency call, not a farm move.
/// `find`/`xargs` are intentionally NOT bare symlinks either: the ladder's findutils
/// dead-axis lock (`no_bootstrap_step_invokes_host_find_or_xargs`) forbids those tokens in
/// any step text and can't tell a cpio member NAME from a host invocation; they stay
/// reachable as `busybox find` / `busybox xargs`.
const BUSYBOX_APPLETS: &[&str] = &[
    "sh", "ash", "getty", "login", "mount", "umount", "vi", "less", "more", "grep", "sed", "awk",
    "su",
];

/// The boot glue, served by the static td-init multicall — the busybox applets that need a
/// RAW SYSCALL, which is why they are a separate binary from td-util's
/// `#![forbid(unsafe_code)]` farm. Unlike every other farm here these names are LOAD-BEARING:
/// `/init` is PID 1, `switch_root` is the stage-1 pivot, `hostname -F` runs at sysinit (the
/// `-F` flag is the gap uutils has — it ships no `hostname` at all), and `reboot` is how a
/// boot ends. So this farm is not merely symlinked and probed; the image's actual boot path
/// runs it, and `shape_check` additionally dry-runs the shipped `/etc/inittab` through the
/// packed binary so a table PID 1 would reject reds the BUILD rather than the boot.
///
/// td-init is an ET_EXEC with an EMPTY runtime closure, which is what lets the same binary
/// serve the pre-pivot initramfs (where no loader exists) and the real root's `/bin`.
///
/// ONE table, name paired with the way the greeter proves that name on the booted image, so
/// a farm entry cannot exist without a probe or a probe without a farm entry — the shape the
/// td-util cutover had to add a separate test to get.
const TD_INIT_FARM: &[(&str, Probe)] = &[
    // Probed by REFUSAL even though it is reversible, because the success path is not free:
    // cttyhack's whole job is `setsid(2)` + `TIOCSCTTY`, and the health target inherits
    // init's /dev/console on stdin, so a run-it probe claims the LIVE console once per boot.
    // That normally EPERMs (getty holds ttyS0 by then), but `::once:` jobs start before the
    // respawned tty session, so there is a window where the claim succeeds and the probe —
    // exiting immediately as a session leader — vhangups it. No script on this image uses
    // cttyhack, so a probe that mutates global terminal state to prove an exec nothing
    // performs is pure cost. The usage refusal still pins the packed name and its dispatch.
    ("cttyhack", Probe::Refuses("", "usage: cttyhack")),
    ("halt", Probe::Refuses("--not-an-option", "unrecognised argument")),
    ("hostname", Probe::ReadsBackHostname),
    // The shipped table, parsed by the binary that will be PID 1 next boot.
    ("init", Probe::Runs("--dry-run -f /etc/inittab")),
    (
        "poweroff",
        Probe::Refuses("--not-an-option", "unrecognised argument"),
    ),
    (
        "reboot",
        Probe::Refuses("--not-an-option", "unrecognised argument"),
    ),
    // /etc holds no init, so this is refused by the INIT-RESOLUTION check — the first of
    // the two fail-early guards, and the only one a read-only root can drive without
    // building a candidate NEWROOT. The mount-point guard behind it is exercised at build
    // time, where shape_check can construct a directory holding a real init. Nothing is
    // moved either way, and the greeter is unprivileged, so mount(2)/chroot(2) would EPERM
    // even if both regressed — which is why this matches the DIAGNOSTIC, not a bare
    // non-zero exit that an EPERM would also produce.
    (
        "switch_root",
        Probe::Refuses("/etc /init", "refusing to switch"),
    ),
];

/// How the greeter proves one td-init farm name on the booted image. Three of these applets
/// are IRREVERSIBLE — running them successfully ENDS the boot — so they are proven by their
/// REFUSAL instead. That is not a weaker proof of the shipped symlink and argv[0] dispatch
/// (the binary still had to run in order to refuse), and it is a sharper proof of the
/// contract that matters most for a name a typo can fire: options are parsed before anything
/// irreversible happens. Their SUCCESS path is proven by the boot itself — `reboot` is how
/// the oracle's VM powers off.
enum Probe {
    /// Must exit 0 with these arguments.
    Runs(&'static str),
    /// Must exit NON-ZERO with these arguments AND say so with this diagnostic. The
    /// diagnostic is half the assertion: an unprivileged EPERM from the syscall would also
    /// exit non-zero, and that would prove the applet TRIED — the opposite of the contract.
    Refuses(&'static str, &'static str),
    /// Must print the hostname `sysinit` set from /etc/hostname — the only way to see that
    /// `hostname -F` actually took, since it runs long before the greeter exists.
    ReadsBackHostname,
}

fn td_init_applets() -> Vec<&'static str> {
    TD_INIT_FARM.iter().map(|(name, _)| *name).collect()
}

/// The greeter's proof for one farm name, generated from the table so the probe and the
/// shipped symlink can never cover different sets. Each segment clears the marker gate
/// (`i=0`) on failure and names the applet, so the oracle's console tail says WHICH one
/// broke instead of only that the marker was absent.
/// Double quotes only, never single: these segments are pasted inside the health target's
/// single-quoted `su -c '…'` argument, where one `'` would end it and hand the rest to the
/// wrong shell.
fn td_init_probe(applet: &str, probe: &Probe, sys: &SystemDef) -> String {
    match probe {
        // Captured, not discarded: the one Runs probe is `init --dry-run` over the shipped
        // table, and its per-line parse diagnostics ARE the answer to why it refused.
        // shape_check prints them at build time; a boot-time rejection deserves the same.
        Probe::Runs(args) => format!(
            "if e=$(/bin/{applet} {args} 2>&1); then :; \
             else echo \"td-init: /bin/{applet} failed: $e\"; i=0; fi; "
        ),
        // if/else, not `&&` then an unconditional `case`: the two failures are mutually
        // exclusive, and running both would report an applet that RAN a bogus argument as
        // also having refused it wrongly. The wrong-diagnostic arm echoes what it actually
        // got, since that arm fires precisely when the expected text is not what to look for.
        Probe::Refuses(args, says) => format!(
            "if e=$(/bin/{applet} {args} 2>&1); then \
             echo \"td-init: /bin/{applet} ran a bogus argument instead of refusing it\"; i=0; \
             else case \"$e\" in *\"{says}\"*) ;; \
             *) echo \"td-init: /bin/{applet} refused without saying {says}: $e\"; i=0 ;; esac; fi; "
        ),
        Probe::ReadsBackHostname => format!(
            "[ \"$(/bin/{applet})\" = \"{}\" ] || \
             {{ echo \"td-init: /bin/{applet} did not read back the configured name\"; i=0; }}; ",
            sys.hostname
        ),
    }
}

/// Applets reached through the packed BusyBox multicall as `/bin/busybox <applet>`, whether
/// or not they also get a `/bin` symlink — `rootcheck` reaches `chown`, `mkdir`, `readlink`,
/// `rm` and `test` this way, all names uutils serves in `/bin`. They must still EXIST in
/// busybox, so `shape_check` probes them against `busybox --list` like the farm names;
/// otherwise a config drift breaks sysinit with no build-time signal. Unlike the three /bin farms
/// this is deliberately NOT disjoint from BUSYBOX_APPLETS. `script_applets_are_covered`
/// derives it from the script text both ways, so neither an uncovered call nor a stale
/// entry survives; td-boot's own invocations are justified by its protocol constant.
const INITRAMFS_APPLETS: &[&str] = &[
    "cat",
    "chmod",
    "chown",
    "ln",
    "losetup",
    "mkdir",
    "mknod",
    "mount",
    "printf",
    "readlink",
    "rm",
    "sh",
    "sleep",
    "sync",
    "test",
    "umount",
];

/// The diagnostics userland, served by the static td-util multicall — the busybox names
/// uutils does not provide. Like busybox and uutils it dispatches on argv[0]'s basename, so
/// a `/bin/<applet>` -> td-util symlink runs that applet. Unlike uutils it is an ET_EXEC
/// with an EMPTY runtime closure, so these entries keep working when no dynamic loader
/// would: a diagnostics tool that dies with the closure is useless exactly when it is
/// needed. `shape_check` probes each name against the packed binary's own `--list`, so an
/// entry td-util does not serve reds the build rather than shipping a `/bin` name that
/// dispatches to nothing.
const TD_UTIL_APPLETS: &[&str] = &["clear", "which", "free", "ps", "dmesg"];

/// The core file/text userland, served by the uutils `coreutils` multicall (#547). Every
/// name must be a coreutils utility the built binary implements. The recipe sandbox cannot
/// exec the dynamically-linked binary to run `coreutils --list` at build time (its interp
/// resolves an absolute `/td/store` path that only exists on the assembled root, not in the
/// build tree). A missing applet surfaces on the boot oracle; the engine-native runtime
/// closure step proves every referenced store item is declared and stages it at its
/// canonical path. uutils is dynamically linked, so — unlike static busybox — it pulls its
/// reachable runtime store closure onto the erofs root.
const UUTILS_APPLETS: &[&str] = &[
    "uname", "ls", "cat", "echo", "printf", "pwd", "cp", "mv", "rm", "mkdir", "rmdir", "ln", "id",
    "env", "df", "du", "chmod", "chown", "sleep", "sync", "wc", "head", "tail", "sort", "date",
    "whoami", "tty", "dd", "mktemp", "seq", "touch", "mknod", "kill", "readlink",
];

fn build_passwd(sys: &SystemDef) -> String {
    let mut s = String::new();
    for u in sys.users {
        s.push_str(&format!(
            "{}:x:{}:{}:{}:{}:{}\n",
            u.name, u.uid, u.gid, u.gecos, u.home, u.shell
        ));
    }
    s
}

fn build_group(sys: &SystemDef) -> String {
    let mut s = String::new();
    // Primary group per user (group name == user name).
    for u in sys.users {
        s.push_str(&format!("{}:x:{}:\n", u.name, u.gid));
    }
    // A `wheel` group (gid 10) whose members are the users that declare it.
    let wheel: Vec<&str> = sys
        .users
        .iter()
        .filter(|u| u.groups.contains(&"wheel"))
        .map(|u| u.name)
        .collect();
    s.push_str(&format!("wheel:x:10:{}\n", wheel.join(",")));
    s.push_str("tty:x:5:\n");
    s
}

fn build_shadow(sys: &SystemDef) -> String {
    let mut s = String::new();
    for u in sys.users {
        // Empty password field => no password (login -f bypasses auth regardless;
        // an empty field also lets `su` reach the account on a throwaway VM). A
        // non-passwordless account is locked (`!`). A fixed last-change day (19000)
        // keeps the file reproducible (no wall-clock date).
        let pw = if u.passwordless { "" } else { "!" };
        s.push_str(&format!("{}:{}:19000:0:99999:7:::\n", u.name, pw));
    }
    s
}

fn build_inittab() -> String {
    // td-init: `<id>::<action>:<process>`. `id` names the tty init opens for the
    // process; empty id => the system console. This inittab runs on the REAL root AFTER
    // stage-1 `switch_root`ed into it: init re-mounts the pseudo-filesystems (devtmpfs,
    // proc, sysfs) on the erofs root's empty mountpoint dirs — mounting over a read-only
    // dir is a VFS overlay, no write to the erofs — then runs the boot self-check, brings
    // the network up, starts the sshd service, and the auto-login getty. It does NOT mount
    // /var, /tmp, or /run: stage-1 already mounted persistent @var and the volatile
    // tmpfs filesystems, and switch_root preserves the mounts. /proc must precede
    // /etc/rootcheck (which reads /proc/mounts).
    // /etc/netup runs AFTER rootcheck (networking after the read-only-root self-check)
    // and AFTER /run is a tmpfs (mounted by stage-1, preserved through switch_root): it
    // brings the link up every boot — loopback with 127.0.0.1/8 (so sshd's own loopback
    // bind/connect and the boot self-test route) plus any NIC — and, under the nettest
    // token, self-tests resolve + reach. td-netd writes resolv.conf/hosts through /etc
    // symlinks into that /run.
    //
    // sshd runs as an init-managed `respawn` service AFTER netup (so loopback and any
    // external link are already up): it binds all interfaces on port 22 (privileged, so it
    // runs as root) and authorizes only /etc/ssh/authorized_keys (shipped empty => deny-all
    // until keys are provisioned). A correctly-binding daemon never exits, so respawn does
    // not loop; if it ever did, init restarts it rather than leaving the box without sshd.
    //
    // There is no `ctrlaltdel` or `shutdown` line: td-init supervises with NO signals (a
    // blocking wait4 IS its event loop), so both actions are signal contracts it cannot
    // honour and would only be rejected at boot as unsupported. The teardown they used to
    // reach lives in /etc/tty-session instead — see build_tty_session.
    "::sysinit:/bin/mount -t devtmpfs devtmpfs /dev\n\
     ::sysinit:/bin/mount -t proc proc /proc\n\
     ::sysinit:/bin/mount -t sysfs sysfs /sys\n\
     ::sysinit:/bin/hostname -F /etc/hostname\n\
     ::sysinit:/etc/rootcheck\n\
     ::sysinit:/etc/netup\n\
     ::once:/etc/bootsuccess\n\
     ::once:/etc/bootfail\n\
     ::respawn:/bin/sshd serve --listen 0.0.0.0:22 --authorized-keys /etc/ssh/authorized_keys\n\
     ttyS0::respawn:/etc/tty-session\n"
        .into()
}

/// The firmware/direct-boot initramfs always selects through td-boot and kexecs
/// the verified deployment. It has no selected-deployment branch, so an external
/// kernel command line cannot bypass current/previous selection.
fn build_selector_init() -> String {
    "#!/bin/sh\n\
     set -e\n\
     set -f\n\
     /bin/busybox mount -t devtmpfs dev /dev\n\
     /bin/busybox mount -t proc proc /proc\n\
     n=0\n\
     while /bin/busybox test \"$n\" -lt 5 && ! /bin/busybox test -b /dev/vda; do /bin/busybox sleep 1; n=$((n+1)); done\n\
     exec /bin/td-boot boot /dev/vda /volume \"$(/bin/busybox cat /proc/cmdline)\"\n"
        .into()
}

/// The selected deployment initramfs requires exactly one td.deployment handoff,
/// validates that manifest and root payload, and enters the immutable root.
fn build_deployment_init(sys: &SystemDef) -> String {
    // /dev/vda is one Btrfs filesystem. The top-level vfsmount stays read-only,
    // while the shared Btrfs superblock becomes writable for the @var mount. The
    // mount flag prevents accidental writes, not a privileged remount by root.
    // The verified loop keeps root.erofs open, so the top-level mount cannot be
    // unmounted; move it below the new root's volatile /run instead.
    let mut init = "#!/bin/sh\n\
     set -e\n\
     set -f\n\
     /bin/busybox mount -t devtmpfs dev /dev\n\
     /bin/busybox mount -t proc proc /proc\n\
     n=0\n\
     while /bin/busybox test \"$n\" -lt 5 && ! /bin/busybox test -b /dev/vda; do /bin/busybox sleep 1; n=$((n+1)); done\n\
     deployment=\n\
     deployment_seen=\n\
     for word in $(/bin/busybox cat /proc/cmdline); do\n\
       case \"$word\" in\n\
         td.deployment=*) \
           /bin/busybox test -z \"$deployment_seen\" || { echo 'td-init: duplicate td.deployment handoff' >&2; exit 1; }; \
           deployment_seen=1; deployment=${word#td.deployment=} ;;\n\
       esac\n\
     done\n\
     /bin/busybox test -n \"$deployment\" || { echo 'td-init: missing td.deployment handoff' >&2; exit 1; }\n\
     /bin/busybox mount -t btrfs -o ro,nodev,nosuid,noexec /dev/vda /volume\n\
     if ! /bin/busybox test -b /dev/loop0; then /bin/busybox mknod /dev/loop0 b 7 0; fi\n\
     /bin/td-boot root-loop /volume \"$deployment\" /dev/loop0\n\
     /bin/busybox mount -t erofs -o ro /dev/loop0 /sysroot\n\
     /bin/busybox mount -t btrfs -o rw,nodev,nosuid,subvol=@var /dev/vda /sysroot/var\n\
     /bin/busybox umount /proc\n\
     /bin/busybox umount /dev\n\
     /bin/busybox mount -t tmpfs -o mode=0755 tmpfs /sysroot/run\n\
     /bin/busybox printf '%s\\n' \"$deployment\" > /sysroot/run/td-deployment\n\
     /bin/busybox chmod 0600 /sysroot/run/td-deployment\n\
     /bin/busybox mkdir -p /sysroot/run/td-volume\n\
     /bin/busybox mount -o move /volume /sysroot/run/td-volume\n\
     /bin/busybox mount -t tmpfs -o mode=1777 tmpfs /sysroot/tmp\n\
     /bin/busybox mkdir -p /sysroot/var/log /sysroot/var/home"
        .to_string();
    for user in sys.users {
        if user.home != "/root" {
            init.push_str(&format!(" /sysroot/var{}", user.home));
        }
    }
    init.push_str(
        "\n/bin/busybox sh -c 'umask 077; /bin/busybox mkdir -p /sysroot/var/root'\n\
         /bin/busybox rm -rf /sysroot/var/run\n\
         /bin/busybox ln -s /run /sysroot/var/run\n\
         /bin/busybox chown 0:0 /sysroot/var /sysroot/var/log /sysroot/var/home /sysroot/var/root\n\
         /bin/busybox chmod 0755 /sysroot/var /sysroot/var/log /sysroot/var/home\n\
         /bin/busybox chmod 0700 /sysroot/var/root\n\
         exec /bin/switch_root /sysroot /init\n",
    );
    init
}

/// The ttyS0 session wrapper, run by init AS ROOT (inittab `respawn`). It runs the
/// normal getty -> autologin -> `login -f <user>` flow, then, when that session
/// ENDS — the greeter user types `exit` / Ctrl-D — tears the system down and resets
/// the machine so the VM stops. The auto-login user is UNPRIVILEGED and cannot shut
/// the system down itself; this wrapper runs as root (init's child), so it does it on
/// the user's behalf, making `exit` a clean way out of the VM. Under qemu's
/// `-no-reboot`, the resulting reset makes qemu exit 0.
///
/// It runs `/etc/shutdown` ITSELF rather than leaving it to PID 1. td-init supervises
/// with no signals, so it has no shutdown sequence to hook: on td the orderly teardown
/// belongs to whoever DECIDES to shut down, and this wrapper is that decision point.
/// The teardown's own failure never blocks the reset (`;`, not `&&`) — a machine that
/// refuses to reboot because it could not unmount is worse than one that reboots after
/// a sync — but /etc/shutdown withholds its marker, so the oracle reds rather than
/// passing a boot whose state was never released.
///
/// The teardown writes to `/dev/console`, NOT to the tty it inherited. By the time it
/// runs, the greeter shell — the SESSION LEADER of the ttyS0 session getty created —
/// has exited, so the kernel has vhangup'd that terminal and every write through the
/// inherited descriptor returns EIO. The teardown would run correctly and silently: the
/// machine reboots, and the marker proving state was released is lost, which reads
/// exactly like a teardown that never ran. This is only a hazard because the teardown
/// moved OFF PID 1 (whose stdio is /dev/console and is never part of a login session);
/// under busybox init the same script needed no redirect. Verified by observing exactly
/// that failure — reboot with no output at all — before adding it.
///
/// The teardown and reboot are both gated on `getty` SUCCEEDING (`&&`): getty sets up the
/// tty and execs the login chain, returning the user shell's exit status, so a normal
/// `exit`/Ctrl-D returns 0 -> power off. But if getty/login FAILS to start a session at all
/// (e.g. it cannot open ttyS0), getty returns non-zero, the `&&` short-circuits, and the
/// wrapper exits non-zero so init RESPAWNS it — a visible retry loop — rather than firing
/// `reboot` and letting `-no-reboot` mask a broken greeter as a clean exit-0 shutdown
/// (re #541, Codex review).
fn build_tty_session() -> String {
    "#!/bin/sh\n\
     /bin/getty -L -n -l /etc/autologin 115200 ttyS0 vt100 && { /etc/shutdown; exec /bin/reboot; } >/dev/console 2>&1\n"
        .into()
}

fn build_shutdown() -> String {
    // Run by /etc/tty-session before it reboots, with the greeter session already gone.
    // Keep this a strict tripwire, but attempt every safety step after any failure.
    format!(
        "#!/bin/sh\n\
         ok=1\n\
         /bin/busybox sync || {{ echo 'td-shutdown: sync failed' >&2; ok=0; }}\n\
         /bin/busybox umount /var || {{ echo 'td-shutdown: umount /var failed' >&2; ok=0; }}\n\
         /bin/busybox umount -a -r || {{ echo 'td-shutdown: final unmount failed' >&2; ok=0; }}\n\
         /bin/busybox test \"$ok\" = 1 && echo {SYSTEM_SHUTDOWN_MARKER}\n"
    )
}

fn build_autologin(sys: &SystemDef) -> String {
    // getty (-n -l) execs this with the tty already set up; force-login the
    // configured user with no authentication.
    format!("#!/bin/sh\nexec /bin/login -f {}\n", sys.autologin)
}

/// The boot self-check run once at sysinit AS ROOT on the REAL (post-switch_root) root
/// (re #550). It gives each non-root user ownership of its `/var`-backed home and
/// prints the diagnostic markers the headless oracle asserts: `/` and `/etc` remain
/// immutable, while the direct state and volatile mounts accept writes.
fn build_rootcheck(sys: &SystemDef) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\nok=1\n");
    // Home ownership below the writable /var mount (skip root, which already owns it).
    for u in sys.users {
        if u.uid != 0 {
            s.push_str(&format!(
                "/bin/busybox chown {}:{} {} 2>/dev/null || ok=0\n",
                u.uid, u.gid, u.home
            ));
        }
    }
    if let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) {
        if user.uid != 0 {
            s.push_str(&format!(
                "if /bin/busybox su -s /bin/sh {} -c \
                 '/bin/busybox test -d /var/root \
                 && /bin/busybox test ! -w /var \
                 && /bin/busybox test ! -w /var/root \
                 && /bin/busybox test -w {}'; then \
                 echo {SYSTEM_STATE_OWNER_MARKER}; else ok=0; fi\n",
                user.name, user.home
            ));
        }
    }
    // `/` is a read-only erofs mount (fields: <src> <mnt> <fstype> <opts> …; erofs is
    //     always mounted `ro`, so the options field begins `ro`).
    s.push_str(&format!(
        "if /bin/busybox grep -Eq '^[^ ]+ / erofs ro[, ]' /proc/mounts; then echo {SYSTEM_ROOT_RO_MARKER}; else ok=0; fi\n"
    ));
    // Root runs this check, so a failed /etc write proves the filesystem rejects writes,
    // not merely that file modes deny an unprivileged process. Run the redirection in a
    // child shell: ash exits a non-interactive shell when a special builtin redirection
    // fails, instead of returning control to the parent `if`.
    s.push_str(&format!(
        "if /bin/busybox sh -c ': > /etc/.tdwr' 2>/dev/null; then /bin/busybox rm -f /etc/.tdwr; ok=0; else echo {SYSTEM_ETC_RO_MARKER}; fi\n"
    ));
    // State is the persistent Btrfs @var subvolume; only run/tmp are volatile.
    // Homes remain stable paths through immutable symlinks into /var.
    s.push_str(
        "/bin/busybox grep -Eq '^[^ ]+ /var btrfs ' /proc/mounts || ok=0\n\
         /bin/busybox awk '$2 == \"/run/td-volume\" && $3 == \"btrfs\" && \
         $4 ~ /(^|,)ro(,|$)/ { found=1 } END { exit !found }' /proc/mounts || ok=0\n\
         for d in /run /tmp; do \
         /bin/busybox grep -Eq \"^[^ ]+ $d tmpfs \" /proc/mounts || ok=0; \
         done\n",
    );
    s.push_str(
        "[ \"$(/bin/busybox readlink /home)\" = var/home ] || ok=0\n\
         [ \"$(/bin/busybox readlink /root)\" = var/root ] || ok=0\n\
         [ \"$(/bin/busybox readlink /var/run)\" = /run ] || ok=0\n",
    );
    let mut probe_paths = "/var /run /tmp /home /root".to_string();
    for user in sys.users {
        if user.home != "/root" {
            probe_paths.push(' ');
            probe_paths.push_str(user.home);
        }
    }
    s.push_str(&format!(
        "for d in {probe_paths}; do \
         if /bin/busybox sh -c ': > \"$1/.tdwr\"' td-probe \"$d\" 2>/dev/null; then /bin/busybox rm -f \"$d/.tdwr\"; else ok=0; fi; \
         done\n"
    ));
    if let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) {
        s.push_str(&format!(
            "if /bin/busybox grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline; then \
             /bin/busybox printf '%s\\n' waiting > /run/td-boot-parked \
             && /bin/busybox chown {}:{} /run/td-boot-parked \
             && /bin/busybox chmod 0600 /run/td-boot-parked || ok=0; \
             fi\n",
            user.uid, user.gid
        ));
    }
    s.push_str(&format!(
        "if [ \"$ok\" = 1 ]; then \
         echo {SYSTEM_STATE_WRITABLE_MARKER}; \
         /bin/busybox printf '%s\\n' td-rootcheck-v1 > /run/td-rootcheck-ok \
         && /bin/busybox chmod 0600 /run/td-rootcheck-ok; \
         fi\n"
    ));
    s.push_str(&format!(
        "if /bin/busybox grep -q -F '{PERSIST_WRITE_CMDLINE_TOKEN}' /proc/cmdline; then \
         if /bin/busybox test ! -e /var/lib/td/boot-marker \
         && /bin/busybox mkdir -p /var/lib/td \
         && /bin/busybox printf '%s\\n' td-persistent-v1 > /var/lib/td/boot-marker \
         && /bin/busybox sync; then \
         echo {SYSTEM_PERSIST_WRITE_MARKER}; fi; \
         fi\n\
         if /bin/busybox grep -q -F '{PERSIST_READ_CMDLINE_TOKEN}' /proc/cmdline \
         && /bin/busybox test \"$(/bin/busybox cat /var/lib/td/boot-marker 2>/dev/null)\" = td-persistent-v1; then \
         echo {SYSTEM_PERSIST_READ_MARKER}; \
         fi\n"
    ));
    s
}

/// Each component marker is emitted by its OWN leg, not from one block gated on every leg
/// passing. Emitting them together meant any single failure withheld every marker, so the oracle
/// reported whichever it checked first — uutils — and the diagnostics the other farms went to
/// trouble to produce named a component nobody would look at. The `m*` flags keep a retry from
/// reprinting a marker it already earned. `SYSTEM_BOOT_SUCCESS_MARKER` stays gated on the whole
/// transaction, which is what it means.
fn build_bootsuccess(sys: &SystemDef) -> String {
    // These run under `su` as the unprivileged user, so the dmesg leg needs an unprivileged
    // /dev/kmsg read — which linux-x86-64 guarantees by pinning CONFIG_SECURITY_DMESG_RESTRICT
    // off. Drop dmesg from the farm and that pin is orphaned.
    let mut td_util_probes = String::new();
    for applet in TD_UTIL_APPLETS {
        let args = if *applet == "which" { " sh" } else { "" };
        td_util_probes.push_str(&format!(
            "/bin/{applet}{args} >/dev/null 2>&1 || \
             {{ echo \"td-util: /bin/{applet} failed\"; u=0; }}; "
        ));
    }
    // The boot-glue farm. Unlike td-util's, this one cannot be plain invocations: running
    // `reboot` for real ends the boot, and `init`'s and `switch_root`'s success paths already
    // RAN — as PID 1 and as the pivot — before this script existed. So the reversible names
    // are invoked and the irreversible ones are driven into their refusal, which is the only
    // probe that can precede the marker it gates. See `Probe`.
    let mut td_init_probes = String::new();
    for (applet, probe) in TD_INIT_FARM {
        td_init_probes.push_str(&td_init_probe(applet, probe, sys));
    }
    format!(
        "#!/bin/sh\n\
         set -f\n\
         finish() {{ /bin/busybox printf '%s\\n' \"$1\" > /run/td-boot-success-ok; \
         /bin/busybox chmod 0644 /run/td-boot-success-ok; }}\n\
         fail() {{ finish td-boot-failure-v1; exit 1; }}\n\
         /bin/busybox grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline && exit 0\n\
         /bin/busybox test \"$(/bin/busybox cat /run/td-rootcheck-ok 2>/dev/null)\" = td-rootcheck-v1 || fail\n\
         deployment=$(/bin/busybox cat /run/td-deployment 2>/dev/null)\n\
         /bin/busybox test -n \"$deployment\" || fail\n\
         wait={BOOT_SUCCESS_RETRY_SECS}\n\
         for token in $(/bin/busybox cat /proc/cmdline); do \
         case \"$token\" in {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done\n\
         case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_SUCCESS_RETRY_SECS};; esac\n\
         [ \"$wait\" -gt {BOOT_SUCCESS_RETRY_MAX_SECS} ] && wait={BOOT_SUCCESS_RETRY_MAX_SECS}\n\
         n=0\n\
         mu=0; mrf=0; ms=0; mtu=0; mti=0\n\
         while [ \"$n\" -lt \"$wait\" ]; do \
         healthy=1; \
         if /bin/busybox su -s /bin/sh {} -c \
         '/bin/cat /etc/os-release >/dev/null 2>&1'; then \
         [ \"$mu\" = 1 ] || {{ echo {UUTILS_RUNTIME_MARKER}; mu=1; }}; else healthy=0; fi; \
         if /bin/busybox su -s /bin/sh {} -c \
         'r=$(/bin/rg --color never --no-filename --fixed-strings --line-regexp -- \
         {hostname} /etc/hostname) || \
         {{ echo \"ripgrep: /bin/rg failed\"; exit 1; }}; \
         [ \"$r\" = {hostname} ] || \
         {{ echo \"ripgrep: unexpected hostname result: $r\"; exit 1; }}; \
         f=$(/bin/fd --color never --absolute-path --max-depth 1 ^hostname$ /etc) || \
         {{ echo \"fd: /bin/fd failed\"; exit 1; }}; \
         [ \"$f\" = /etc/hostname ] || {{ echo \"fd: unexpected hostname path: $f\"; exit 1; }}'; then \
         [ \"$mrf\" = 1 ] || {{ echo {RIPGREP_FD_RUNTIME_MARKER}; mrf=1; }}; else healthy=0; fi; \
         if /bin/busybox su -s /bin/sh {} -c \
         '/bin/sshd selftest >/dev/null 2>&1'; then \
         [ \"$ms\" = 1 ] || {{ echo {SSHD_MARKER}; ms=1; }}; else healthy=0; fi; \
         if /bin/busybox su -s /bin/sh {} -c \
         'u=1; /bin/td-util --list >/dev/null 2>&1 || \
         {{ echo \"td-util: --list failed\"; u=0; }}; \
         {td_util_probes}[ \"$u\" = 1 ]'; then \
         [ \"$mtu\" = 1 ] || {{ echo {TD_UTIL_RUNTIME_MARKER}; mtu=1; }}; else healthy=0; fi; \
         if /bin/busybox su -s /bin/sh {} -c \
         'i=1; /bin/td-init --list >/dev/null 2>&1 || \
         {{ echo \"td-init: --list failed\"; i=0; }}; \
         {td_init_probes}[ \"$i\" = 1 ]'; then \
         [ \"$mti\" = 1 ] || {{ echo {TD_INIT_RUNTIME_MARKER}; mti=1; }}; else healthy=0; fi; \
         if [ \"$healthy\" = 1 ] \
         && /bin/td-boot success /dev/vda /run/td-update \"$deployment\" >/run/td-success-id; then \
         if /bin/busybox grep -q -F '{DEPLOY_INSTALL_CMDLINE_TOKEN}' /proc/cmdline; then \
         if /bin/td-boot install /dev/vda /run/td-update \
         /run/td-volume/td/incoming/candidate >/run/td-installed-id; then \
         echo {SYSTEM_DEPLOY_INSTALL_MARKER}; else healthy=0; fi; \
         fi; \
         if [ \"$healthy\" = 1 ]; then \
         echo {SYSTEM_BOOT_SUCCESS_MARKER}; \
         finish td-boot-success-v1; exit 0; fi; \
         fi; \
         n=$((n+1)); /bin/busybox sleep 1; \
         done\n\
         fail\n",
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        hostname = sys.hostname
    )
}

/// The failed-candidate watchdog, and the SECOND thing on this image that decides to reboot.
/// Since td-init supervises with no signals there is no `::shutdown:` action for `/bin/reboot`
/// to trigger, so every initiator runs `/etc/shutdown` itself; `reboots_run_the_teardown_first`
/// holds that invariant over all generated scripts.
fn build_bootfail() -> String {
    format!(
        "#!/bin/sh\n\
         set -f\n\
         /bin/busybox grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline || exit 0\n\
         /bin/busybox test \"$(/bin/busybox cat /run/td-rootcheck-ok 2>/dev/null)\" = td-rootcheck-v1 || exit 1\n\
         wait={BOOT_FAIL_PARK_WAIT_SECS}\n\
         for token in $(/bin/busybox cat /proc/cmdline); do \
         case \"$token\" in {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done\n\
         case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_FAIL_PARK_WAIT_SECS};; esac\n\
         [ \"$wait\" -gt {BOOT_FAIL_PARK_WAIT_SECS} ] && wait={BOOT_FAIL_PARK_WAIT_SECS}\n\
         n=0\n\
         while [ \"$n\" -lt \"$wait\" ]; do \
         /bin/busybox grep -q -x '{BOOT_FAIL_PARKED}' /run/td-boot-parked 2>/dev/null && \
         {{ /etc/shutdown; exec /bin/reboot; }} >/dev/console 2>&1; \
         n=$((n+1)); /bin/busybox sleep 1; \
         done\n\
         echo 'td-boot: greeter park handshake timed out' >&2\n\
         exit 1\n"
    )
}

fn build_profile(sys: &SystemDef) -> String {
    // The login shell (busybox ash, invoked as `-sh`) sources this. We print the banner
    // HERE via a literal here-doc so it shows exactly once regardless of busybox login's
    // own motd feature, and set a sane PATH/PS1.
    let mut s = String::new();
    // Just /bin — the store-native symlink farm. There is no /usr or /sbin in this image
    // (every /bin entry resolves into /td/store), so keep PATH honest and minimal.
    s.push_str("export PATH=/bin\n");
    s.push_str("export PS1='\\u@\\h:\\w\\$ '\n");
    s.push_str(&format!(
        "if /bin/busybox grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline; then \
         exec /bin/busybox sh -c 'cd / \
         || exit 1; \
         if ! /bin/busybox printf \"%s\\n\" {BOOT_FAIL_PARKED} > /run/td-boot-parked; then \
         echo \"td-boot: could not park greeter\" >&2; fi; \
         while :; do /bin/busybox sleep 300; done'; fi\n"
    ));
    s.push_str("cat <<'__TD_MOTD__'\n");
    s.push_str(sys.motd);
    if !sys.motd.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("__TD_MOTD__\n");
    // The greeter has been reached (login chain ran, shell live) — the primary success
    // line the qemu-boot-system oracle keys on.
    s.push_str(&format!("echo {GREETER_MARKER}\n"));
    // Autotest waits for the independent root-owned health transaction.
    s.push_str(&format!(
        "if /bin/busybox grep -q -F '{AUTOTEST_CMDLINE_TOKEN}' /proc/cmdline 2>/dev/null; then \
         set -f; wait=0; for token in $(/bin/busybox cat /proc/cmdline); do \
         case \"$token\" in {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done; \
         case \"$wait\" in ''|*[!0-9]*|0) wait=1;; esac; \
         n=0; while [ \"$n\" -lt \"$wait\" ]; do \
         status=$(/bin/busybox cat /run/td-boot-success-ok 2>/dev/null); \
         [ \"$status\" = td-boot-success-v1 ] && break; \
         [ \"$status\" = td-boot-failure-v1 ] && break; \
         n=$((n+1)); /bin/busybox sleep 1; done; \
         exit 0; fi\n"
    ));
    s
}

/// The sysinit network bring-up glue, run AS ROOT once at boot. `td-netd up`
/// autodetects the link, DHCP-configures it, and writes resolv.conf + hosts (a
/// NIC-less boot is a clean no-op). Under the `NETTEST_CMDLINE_TOKEN` the headless
/// `qemu-boot-net` oracle appends, it additionally self-tests the stack — resolve
/// the default host via the DHCP-provided nameserver, then TCP-reach it — printing
/// the three net markers on ttyS0. Off the token (normal boot, or the `-nic none`
/// `qemu-boot-system` oracle) the link still comes up but no marker is printed.
///
/// One `td-netd up`: `$up` records whether it configured the link so NET_UP is
/// asserted only on real success (a DHCP timeout drops the marker and reds the
/// oracle rather than false-passing). `-F`: the token is a fixed string, matched
/// literally (the `.` must not act as a regex wildcard), mirroring build_profile.
fn build_netup() -> String {
    format!(
        "#!/bin/sh\n\
         if /bin/td-netd up; then up=1; else up=0; fi\n\
         if /bin/busybox grep -q -F '{NETTEST_CMDLINE_TOKEN}' /proc/cmdline 2>/dev/null; then \
         [ \"$up\" = 1 ] && echo {SYSTEM_NET_UP_MARKER}; \
         /bin/td-netd resolve {NETTEST_DEFAULT_HOST} && echo {SYSTEM_NET_RESOLVE_MARKER}; \
         /bin/td-netd reach {NETTEST_DEFAULT_HOST} {NETTEST_DEFAULT_PORT} && echo {SYSTEM_NET_REACH_MARKER}; \
         fi\n"
    )
}

fn build_os_release(sys: &SystemDef) -> String {
    format!(
        "NAME=\"{name}\"\nID={id}\nVERSION=\"{ver}\"\nVERSION_ID={ver}\n\
         PRETTY_NAME=\"{name} {ver}\"\n",
        name = sys.os_name,
        id = sys.os_name,
        ver = sys.os_version
    )
}

/// The generated /etc files (config + the login-glue and boot-check scripts). `exec`
/// marks the ones getty/init reference as executables. Shared by the real-root staging
/// (written under `{root}/real-root/etc`) and the shape check (which asserts they landed).
fn etc_files(sys: &SystemDef) -> Vec<(&'static str, String, bool)> {
    vec![
        ("passwd", build_passwd(sys), false),
        ("group", build_group(sys), false),
        ("shadow", build_shadow(sys), false),
        ("hostname", format!("{}\n", sys.hostname), false),
        ("os-release", build_os_release(sys), false),
        ("inittab", build_inittab(), false),
        ("profile", build_profile(sys), false),
        // Executable glue (mode 0755): getty execs autologin; init respawns tty-session
        // and runs rootcheck at sysinit. They live in /etc so /bin stays a pure
        // store-symlink farm.
        ("autologin", build_autologin(sys), true),
        ("tty-session", build_tty_session(), true),
        ("shutdown", build_shutdown(), true),
        ("rootcheck", build_rootcheck(sys), true),
        ("netup", build_netup(), true),
        ("bootsuccess", build_bootsuccess(sys), true),
        ("bootfail", build_bootfail(), true),
    ]
}

/// Which of the two structurally distinct boot phases a cpio is packed for. They carry
/// DISJOINT helpers, and each helper's absence from the other phase is asserted: the
/// selector kexecs (td-kexec) and never pivots, the deployment phase pivots (td-init's
/// `switch_root`) and never kexecs. A phase that carried both would let an initramfs do
/// something its /init has no branch for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Selector,
    Deployment,
}

/// A gen_init_cpio spec for one of the two structurally distinct boot phases.
fn build_initramfs_spec(init: &str, phase: Phase) -> String {
    let mut s = String::new();
    for d in ["/dev", "/proc", "/run", "/sysroot", "/td", "/td/store"] {
        s.push_str(&format!("dir {d} 0755 0 0\n"));
    }
    s.push_str("dir /volume 0700 0 0\n");
    // The static busybox at its content-addressed /td/store path; the cpio's /bin/busybox
    // and /bin/sh symlinks (all the stage-1 script needs) point straight at it.
    s.push_str("dir {in:busybox-x86-64} 0755 0 0\n");
    s.push_str("dir {in:busybox-x86-64}/bin 0755 0 0\n");
    s.push_str("file {in:busybox-x86-64}/bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n");
    s.push_str("dir /bin 0755 0 0\n");
    s.push_str("slink /bin/busybox {in:busybox-x86-64}/bin/busybox 0777 0 0\n");
    s.push_str("slink /bin/sh {in:busybox-x86-64}/bin/busybox 0777 0 0\n");
    s.push_str("dir {in:td-boot} 0755 0 0\n");
    s.push_str("dir {in:td-boot}/bin 0755 0 0\n");
    s.push_str("file {in:td-boot}/bin/td-boot {in:td-boot}/bin/td-boot 0755 0 0\n");
    s.push_str("slink /bin/td-boot {in:td-boot}/bin/td-boot 0777 0 0\n");
    match phase {
        Phase::Selector => {
            s.push_str("dir {in:td-kexec} 0755 0 0\n");
            s.push_str("dir {in:td-kexec}/bin 0755 0 0\n");
            s.push_str("file {in:td-kexec}/bin/td-kexec {in:td-kexec}/bin/td-kexec 0755 0 0\n");
            s.push_str("slink /bin/td-kexec {in:td-kexec}/bin/td-kexec 0777 0 0\n");
        }
        // The pivot applet, and ONLY it: /init execs `/bin/switch_root`, so that is the one
        // name this phase needs. td-init is a static ET_EXEC with an empty runtime closure,
        // which is what lets it run here at all — nothing has mounted the real root yet, so
        // a dynamically-linked pivot would be a kernel panic rather than a degraded boot.
        Phase::Deployment => {
            s.push_str("dir {in:td-init} 0755 0 0\n");
            s.push_str("dir {in:td-init}/bin 0755 0 0\n");
            s.push_str("file {in:td-init}/bin/td-init {in:td-init}/bin/td-init 0755 0 0\n");
            s.push_str("slink /bin/switch_root {in:td-init}/bin/td-init 0777 0 0\n");
        }
    }
    s.push_str("nod /dev/console 0600 0 0 c 5 1\n");
    s.push_str(&format!("file /init {{root}}/{init} 0755 0 0\n"));
    s
}

/// Stage the REAL ROOT tree under `{root}/real-root` build scratch. The typed
/// PackErofs step later packs it into the deployment output. Uses typed steps (no shell): the busybox
/// package is copied to its /td/store path, /bin is a symlink farm into it, /init is a
/// symlink to busybox, /etc holds the generated config, and the pseudo-fs + writable
/// mountpoint dirs are created empty (stage-1/init mount over them). `/home` and `/root`
/// are immutable symlinks into the writable `/var` mount; per-user ownership is fixed at
/// boot by `/etc/rootcheck`.
fn real_root_steps(sys: &SystemDef) -> Vec<Step> {
    let mut steps = Vec::new();
    // Empty mountpoints and the immutable root-image skeleton. State directories are
    // created after /var is mounted, rather than hidden content packed into EROFS.
    for d in [
        "/dev",
        "/proc",
        "/sys",
        "/tmp",
        "/run",
        "/etc",
        "/bin",
        "/mnt",
        "/var",
        "/td",
        "/td/store",
    ] {
        steps.push(Step::MkDir {
            path: format!("{{root}}/real-root{d}"),
        });
    }
    for (link, target) in [("/home", "var/home"), ("/root", "var/root")] {
        steps.push(Step::Symlink {
            target: target.into(),
            link: format!("{{root}}/real-root{link}"),
        });
    }
    // Static busybox has no runtime store closure; copy its package directly. Scanning the
    // whole output as a runtime root would mistake build-provenance strings for runtime
    // edges and pull its compiler into the image.
    steps.push(Step::CopyTree {
        from: "{in:busybox-x86-64}".into(),
        dest: "{root}/real-root{in:busybox-x86-64}".into(),
    });
    // td-netd is STATIC (empty runtime closure, like busybox), so copy its package
    // directly rather than through StageRuntimeClosure — scanning it as a runtime
    // root would mistake build-provenance strings for edges and pull its toolchain in.
    steps.push(Step::CopyTree {
        from: "{in:td-netd}".into(),
        dest: "{root}/real-root{in:td-netd}".into(),
    });
    // td-boot is static and serves both initramfs selection and root-side
    // deployment transactions.
    steps.push(Step::CopyTree {
        from: "{in:td-boot}".into(),
        dest: "{root}/real-root{in:td-boot}".into(),
    });
    // td-util is static too (ET_EXEC, empty closure — assert_static fail-closes on it),
    // so it copies directly rather than through StageRuntimeClosure.
    steps.push(Step::CopyTree {
        from: "{in:td-util}".into(),
        dest: "{root}/real-root{in:td-util}".into(),
    });
    // td-init likewise — and here the empty closure is load-bearing rather than merely
    // convenient: this same binary is /init (PID 1) and the initramfs pivot, both of which
    // run where no dynamic loader is reachable.
    steps.push(Step::CopyTree {
        from: "{in:td-init}".into(),
        dest: "{root}/real-root{in:td-init}".into(),
    });
    // Stage the dynamically linked userland and every transitively referenced store item
    // at its canonical absolute path. uutils, ripgrep, and fd pull their td glibc closure;
    // sshd additionally pulls the aws-lc crypto C lib. The engine admits only direct recipe
    // inputs, so a Rust bootstrap or other build-only reference fails closed rather than
    // entering the EROFS image.
    steps.push(Step::StageRuntimeClosure {
        roots: vec![
            "{in:uutils}".into(),
            "{in:ripgrep}".into(),
            "{in:fd}".into(),
            "{in:sshd}".into(),
        ],
        dest: "{root}/real-root".into(),
    });
    // /bin symlink farm: /bin/busybox, every applet, and /init resolve DIRECTLY into the
    // store busybox (busybox dispatches on argv[0]'s basename).
    steps.push(Step::Symlink {
        target: "{in:busybox-x86-64}/bin/busybox".into(),
        link: "{root}/real-root/bin/busybox".into(),
    });
    for app in BUSYBOX_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:busybox-x86-64}/bin/busybox".into(),
            link: format!("{{root}}/real-root/bin/{app}"),
        });
    }
    // The core file/text userland resolves into the uutils `coreutils` multicall instead of
    // busybox (#547). uutils dispatches on argv[0]'s basename exactly like busybox, so a
    // /bin/<applet> -> coreutils symlink runs that applet.
    for app in UUTILS_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:uutils}/bin/coreutils".into(),
            link: format!("{{root}}/real-root/bin/{app}"),
        });
    }
    for (name, target) in [("rg", "{in:ripgrep}/bin/rg"), ("fd", "{in:fd}/bin/fd")] {
        steps.push(Step::Symlink {
            target: target.into(),
            link: format!("{{root}}/real-root/bin/{name}"),
        });
    }
    // /init is PID 1 on the real root, and it is td-init. switch_root execs it under the
    // OPERAND's name (`/init`), so argv[0]'s basename is `init` and the multicall dispatches
    // to the init applet exactly as a /bin/<applet> symlink would — the symlink's TARGET
    // name never enters that decision.
    steps.push(Step::Symlink {
        target: "{in:td-init}/bin/td-init".into(),
        link: "{root}/real-root/init".into(),
    });
    // /bin/td-netd resolves into the store td-netd package (a single static binary,
    // NOT a multicall — it is its own /bin entry, unlike the busybox/uutils farms).
    steps.push(Step::Symlink {
        target: "{in:td-netd}/bin/td-netd".into(),
        link: "{root}/real-root/bin/td-netd".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-boot}/bin/td-boot".into(),
        link: "{root}/real-root/bin/td-boot".into(),
    });
    // /bin/td-util is the multicall's own entry (`td-util <applet>`, and `--list`); the loop
    // below is the argv[0] farm the diagnostics names resolve through.
    steps.push(Step::Symlink {
        target: "{in:td-util}/bin/td-util".into(),
        link: "{root}/real-root/bin/td-util".into(),
    });
    for app in TD_UTIL_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:td-util}/bin/td-util".into(),
            link: format!("{{root}}/real-root/bin/{app}"),
        });
    }
    // /bin/td-init is the multicall's own entry (`td-init <applet>`, and `--list`); the loop
    // below is the argv[0] farm the boot-glue names resolve through.
    steps.push(Step::Symlink {
        target: "{in:td-init}/bin/td-init".into(),
        link: "{root}/real-root/bin/td-init".into(),
    });
    for app in td_init_applets() {
        steps.push(Step::Symlink {
            target: "{in:td-init}/bin/td-init".into(),
            link: format!("{{root}}/real-root/bin/{app}"),
        });
    }
    // resolv.conf and hosts live at /etc but are SYMLINKS into the writable /run
    // tmpfs, so td-netd can (re)write them under the read-only erofs /etc. They are
    // deliberately dangling at build time; td-netd creates the /run targets at boot.
    for name in ["resolv.conf", "hosts"] {
        steps.push(Step::Symlink {
            target: format!("/run/{name}"),
            link: format!("{{root}}/real-root/etc/{name}"),
        });
    }
    // The sshd daemon: a single (non-multicall) dynamically-linked binary. /bin/sshd
    // resolves into its staged store path; its runtime closure is staged by
    // StageRuntimeClosure above.
    steps.push(Step::Symlink {
        target: "{in:sshd}/bin/sshd".into(),
        link: "{root}/real-root/bin/sshd".into(),
    });
    // Generated /etc.
    for (name, content, exec) in etc_files(sys) {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/real-root/etc/{name}"),
            content,
            exec,
        });
    }
    // /etc/ssh/authorized_keys — the daemon's ONLY authorization source. Shipped EMPTY
    // (comment only) so a fresh image denies every login until an operator provisions keys
    // into this immutable-/etc file; the daemon fails closed on a missing/empty file.
    steps.push(Step::MkDir {
        path: "{root}/real-root/etc/ssh".into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/real-root/etc/ssh/authorized_keys".into(),
        content: "# td-sshd authorized_keys — one OpenSSH public key per line.\n\
                  # Empty => deny all. /etc is immutable; rebuild the image to change this.\n"
            .into(),
        exec: false,
    });
    steps
}

/// A producer-rung shape check on the deployment bundle and staged real-root
/// scratch tree. For the cpio: real newc magic, a size floor (static busybox alone is ~1 MiB), a
/// `busybox cpio -t` parse, the members that make it bootable (incl. the /init pivot
/// script), and the busybox binary under /td/store. For the root tree: /init and /bin/sh
/// are symlinks into /td/store, the key /etc files exist, and the busybox binary is
/// packed under /td/store. AND that busybox actually implements EVERY BUSYBOX_APPLETS
/// entry (incl. `switch_root`) — a config drift or tailoring typo that dropped/misnamed
/// an applet would leave a dead /bin symlink the member checks alone can't catch. For the
/// uutils farm: the `coreutils` multicall is staged and every UUTILS_APPLETS /bin symlink
/// exists. Its transitive store closure is enforced and staged by `StageRuntimeClosure`.
/// All strings are ASCII (td-builder's config reader is Latin-1).
///
/// The td-init legs are the exception to "shape assert, not behavioural test", and
/// deliberately so. Every other farm here fails visibly at runtime — a dead /bin name
/// prints an error to somebody's terminal — but td-init's names ARE the boot: a table PID 1
/// rejects, or a pivot that refuses, is an image that does not come up, which the oracle can
/// only report as a timeout. So this step EXECUTES the packed static binary (it can: ET_EXEC,
/// empty closure) and drives the two failures that would otherwise be silent — the shipped
/// `/etc/inittab` through `init --dry-run`, and `switch_root`'s fail-early refusal — turning
/// each into a named build error on a per-change check instead of an unbootable artifact on
/// a nightly one. The boot ITSELF is still exercised by `td-recipe-eval run` and the headless
/// `qemu-boot-system` oracle; this only moves the cheaply-decidable half earlier.
fn shape_check() -> String {
    "selector='{out}/boot/selector-initramfs.cpio'; selector_manifest='{out}/boot/manifest'; init='{out}/deployment/initramfs.cpio'; root='{root}/real-root'; disk='{out}/deployment/root.erofs'; manifest='{out}/deployment/manifest'; bb='{in:busybox-x86-64}/bin/busybox'; \
     for archive in \"$selector\" \"$init\"; do \
         sz=$(wc -c < \"$archive\"); \
         [ \"$sz\" -ge 65536 ] || { echo \"initramfs $archive: implausibly small ($sz bytes) - the static busybox alone is ~1 MiB\" >&2; exit 1; }; \
         set -- $(od -An -tx1 -N 6 \"$archive\"); \
         [ \"$1$2$3$4$5$6\" = 303730373031 ] || { echo \"initramfs $archive: missing the newc cpio magic 070701\" >&2; exit 1; }; \
         \"$bb\" cpio -t < \"$archive\" >/dev/null 2>&1 || { echo \"initramfs $archive: busybox cpio -t could not parse the archive\" >&2; exit 1; }; \
     done; \
     selector_list=$(\"$bb\" cpio -t < \"$selector\" 2>/dev/null); \
     init_list=$(\"$bb\" cpio -t < \"$init\" 2>/dev/null); \
     for m in init bin/busybox bin/sh bin/td-boot dev/console proc run volume sysroot; do \
         printf '%s\\n' \"$selector_list\" | grep -q -x -F \"$m\" || { echo \"selector initramfs: cpio member '$m' missing\" >&2; exit 1; }; \
         printf '%s\\n' \"$init_list\" | grep -q -x -F \"$m\" || { echo \"deployment initramfs: cpio member '$m' missing\" >&2; exit 1; }; \
     done; \
     printf '%s\\n' \"$selector_list\" | grep -q -x -F bin/td-kexec || { echo 'selector initramfs: td-kexec missing' >&2; exit 1; }; \
     if printf '%s\\n' \"$init_list\" | grep -q -x -F bin/td-kexec; then echo 'deployment initramfs: td-kexec must be selector-only' >&2; exit 1; fi; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/td-boot$' || { echo 'selector initramfs: td-boot store member missing' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-boot$' || { echo 'deployment initramfs: td-boot store member missing' >&2; exit 1; }; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/td-kexec$' || { echo 'selector initramfs: td-kexec store member missing' >&2; exit 1; }; \
     if printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-kexec$'; then echo 'deployment initramfs: td-kexec store member must be selector-only' >&2; exit 1; fi; \
     printf '%s\\n' \"$init_list\" | grep -q -x -F bin/switch_root || { echo 'deployment initramfs: bin/switch_root missing - its /init would exec nothing and the boot would end in a 300s timeout with no cause' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-init$' || { echo 'deployment initramfs: td-init store member missing - the switch_root symlink would dangle' >&2; exit 1; }; \
     if printf '%s\\n' \"$selector_list\" | grep -q -x -F bin/switch_root; then echo 'selector initramfs: switch_root must be deployment-only - the selector kexecs, it never pivots' >&2; exit 1; fi; \
     [ \"$(wc -l < \"$selector_manifest\")\" -eq 2 ] || { echo 'selector manifest: expected header plus one payload entry' >&2; exit 1; }; \
     [ \"$(head -n 1 \"$selector_manifest\")\" = td-deployment-v1 ] || { echo 'selector manifest: unsupported header' >&2; exit 1; }; \
     grep -q -E '^[0-9a-f]{64}  selector-initramfs\\.cpio$' \"$selector_manifest\" || { echo 'selector manifest: missing strict SHA-256 entry' >&2; exit 1; }; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/busybox$' || { echo 'selector initramfs: busybox store member missing' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/busybox$' || { echo 'deployment initramfs: busybox store member missing' >&2; exit 1; }; \
     [ -f \"$root/init\" ] || [ -L \"$root/init\" ] || { echo 'root tree: /init missing' >&2; exit 1; }; \
     case $(readlink \"$root/init\") in /td/store/*) : ;; *) echo 'root tree: /init is not a symlink into /td/store' >&2; exit 1;; esac; \
     case $(readlink \"$root/bin/sh\") in /td/store/*) : ;; *) echo 'root tree: /bin/sh is not a symlink into /td/store - the store-native /bin farm regressed' >&2; exit 1;; esac; \
     for f in passwd group shadow hostname os-release inittab profile autologin tty-session shutdown rootcheck netup bootsuccess bootfail; do \
         [ -f \"$root/etc/$f\" ] || { echo \"root tree: /etc/$f missing\" >&2; exit 1; }; \
     done; \
     for l in resolv.conf hosts; do \
         [ \"$(readlink \"$root/etc/$l\")\" = \"/run/$l\" ] || { echo \"root tree: /etc/$l must be a symlink into writable /run (td-netd writes it under the read-only erofs /etc)\" >&2; exit 1; }; \
     done; \
     case $(readlink \"$root/bin/td-netd\") in /td/store/*/bin/td-netd) : ;; *) echo 'root tree: /bin/td-netd is not a symlink into /td/store - the network daemon /bin entry regressed' >&2; exit 1;; esac; \
     tnd=\"{root}/real-root{in:td-netd}/bin/td-netd\"; { [ -f \"$tnd\" ] && [ -x \"$tnd\" ]; } || { echo 'root tree: the td-netd binary is not packed/executable at real-root{in:td-netd}/bin/td-netd - the /bin/td-netd symlink would dangle' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-boot\" 2>/dev/null)\" = \"{in:td-boot}/bin/td-boot\" ] || { echo 'root tree: /bin/td-boot is not a symlink to the staged deployment helper' >&2; exit 1; }; \
     tdb=\"{root}/real-root{in:td-boot}/bin/td-boot\"; { [ -f \"$tdb\" ] && [ -x \"$tdb\" ]; } || { echo 'root tree: td-boot is not packed/executable for root-side deployment transactions' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-util\" 2>/dev/null)\" = \"{in:td-util}/bin/td-util\" ] || { echo 'root tree: /bin/td-util is not a symlink to the staged diagnostics multicall' >&2; exit 1; }; \
     tdu=\"{root}/real-root{in:td-util}/bin/td-util\"; tdutgt=\"{in:td-util}/bin/td-util\"; { [ -f \"$tdu\" ] && [ -x \"$tdu\" ]; } || { echo 'root tree: the td-util binary is not packed/executable at real-root{in:td-util}/bin/td-util - the /bin/td-util symlink would dangle' >&2; exit 1; }; \
     tdulist=$(\"$tdu\" --list 2>/dev/null) || { echo 'td-util --list failed - cannot verify the diagnostics farm' >&2; exit 1; }; \
     for a in @TD_UTIL_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tdutgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged td-util multicall ($tdutgt) - the diagnostics /bin farm regressed\" >&2; exit 1; }; \
         printf '%s\\n' \"$tdulist\" | grep -q -x -F \"$a\" || { echo \"td-util does not serve applet '$a' - its packed /bin/$a symlink would dispatch to nothing (usage, exit 2)\" >&2; exit 1; }; \
     done; \
     [ \"$(readlink \"$root/bin/td-init\" 2>/dev/null)\" = \"{in:td-init}/bin/td-init\" ] || { echo 'root tree: /bin/td-init is not a symlink to the staged boot-glue multicall' >&2; exit 1; }; \
     tdi=\"{root}/real-root{in:td-init}/bin/td-init\"; tditgt=\"{in:td-init}/bin/td-init\"; { [ -f \"$tdi\" ] && [ -x \"$tdi\" ]; } || { echo 'root tree: the td-init binary is not packed/executable at real-root{in:td-init}/bin/td-init - /init would not exec and the machine would not boot' >&2; exit 1; }; \
     [ \"$(readlink \"$root/init\")\" = \"$tditgt\" ] || { echo 'root tree: /init must be a symlink to the staged td-init multicall - it is PID 1' >&2; exit 1; }; \
     tdilist=$(\"$tdi\" --list 2>/dev/null) || { echo 'td-init --list failed - cannot verify the boot-glue farm' >&2; exit 1; }; \
     for a in @TD_INIT_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tditgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged td-init multicall ($tditgt) - the boot-glue /bin farm regressed\" >&2; exit 1; }; \
         printf '%s\\n' \"$tdilist\" | grep -q -x -F \"$a\" || { echo \"td-init does not serve applet '$a' - its packed /bin/$a symlink would dispatch to nothing (usage, exit 2)\" >&2; exit 1; }; \
     done; \
     tditab=$(\"$tdi\" init --dry-run -f \"$root/etc/inittab\" 2>&1) || { echo 'td-init init --dry-run REJECTED the inittab this image ships - PID 1 would come up having understood only part of its table. Its per-line diagnostics:' >&2; printf '%s\\n' \"$tditab\" >&2; exit 1; }; \
     mkdir -p '{root}/pivot-probe' && cp \"$tdi\" '{root}/pivot-probe/init' || { echo 'root tree: could not build the switch_root probe NEWROOT' >&2; exit 1; }; \
     tdipiv=$(\"$tdi\" switch_root '{root}/pivot-probe' /init 2>&1) && { echo 'td-init switch_root ACCEPTED a NEWROOT that is not a mount point - the last refusal standing between a bad pivot and a panicked kernel is gone' >&2; exit 1; }; \
     case \"$tdipiv\" in *'not a mount point'*) : ;; *) echo \"td-init switch_root refused a non-mount NEWROOT for the WRONG reason, so the mount-point guard is untested: $tdipiv\" >&2; exit 1;; esac; \
     [ \"$(readlink \"$root/home\")\" = var/home ] || { echo 'root tree: /home must point to var/home' >&2; exit 1; }; \
     [ \"$(readlink \"$root/root\")\" = var/root ] || { echo 'root tree: /root must point to var/root' >&2; exit 1; }; \
     rbb=\"{root}/real-root{in:busybox-x86-64}/bin/busybox\"; { [ -f \"$rbb\" ] && [ -x \"$rbb\" ]; } || { echo 'root tree: the busybox binary is not packed/executable at real-root{in:busybox-x86-64}/bin/busybox - the store-native /bin symlinks would all dangle' >&2; exit 1; }; \
     applets=$(\"$bb\" --list 2>/dev/null) || { echo 'busybox --list failed - cannot verify applet coverage' >&2; exit 1; }; \
     for a in @BUSYBOX_APPLETS@; do \
         printf '%s\\n' \"$applets\" | grep -q -x -F \"$a\" || { echo \"busybox does not implement applet '$a' (config drift) - its packed /bin/$a symlink would be a dead link\" >&2; exit 1; }; \
     done; \
     for a in @INITRAMFS_APPLETS@; do \
         printf '%s\\n' \"$applets\" | grep -q -x -F \"$a\" || { echo \"busybox does not implement multiplexed applet '$a' (config drift) - a generated script or td-boot invokes it as 'busybox $a'\" >&2; exit 1; }; \
     done; \
     uu=\"{root}/real-root{in:uutils}/bin/coreutils\"; uutgt=\"{in:uutils}/bin/coreutils\"; \
     { [ -f \"$uu\" ] && [ -x \"$uu\" ]; } || { echo 'root tree: the uutils coreutils multicall is not packed at real-root{in:uutils}/bin/coreutils - the /bin coreutils symlinks would all dangle (#547)' >&2; exit 1; }; \
     for a in @UUTILS_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$uutgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged uutils multicall ($uutgt) - the uutils /bin farm regressed (#547)\" >&2; exit 1; }; \
     done; \
     rg=\"{root}/real-root{in:ripgrep}/bin/rg\"; rgtgt=\"{in:ripgrep}/bin/rg\"; \
     { [ -f \"$rg\" ] && [ -x \"$rg\" ]; } || { echo 'root tree: ripgrep is not packed/executable at real-root{in:ripgrep}/bin/rg - /bin/rg would dangle and StageRuntimeClosure did not stage it' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/rg\" 2>/dev/null)\" = \"$rgtgt\" ] || { echo 'root tree: /bin/rg is not a symlink to staged ripgrep' >&2; exit 1; }; \
     fd=\"{root}/real-root{in:fd}/bin/fd\"; fdtgt=\"{in:fd}/bin/fd\"; \
     { [ -f \"$fd\" ] && [ -x \"$fd\" ]; } || { echo 'root tree: fd is not packed/executable at real-root{in:fd}/bin/fd - /bin/fd would dangle and StageRuntimeClosure did not stage it' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/fd\" 2>/dev/null)\" = \"$fdtgt\" ] || { echo 'root tree: /bin/fd is not a symlink to staged fd' >&2; exit 1; }; \
     sshd=\"{root}/real-root{in:sshd}/bin/sshd\"; sshdtgt=\"{in:sshd}/bin/sshd\"; \
     { [ -f \"$sshd\" ] && [ -x \"$sshd\" ]; } || { echo 'root tree: the sshd daemon is not packed/executable at real-root{in:sshd}/bin/sshd - /bin/sshd would dangle and StageRuntimeClosure did not stage it' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/sshd\" 2>/dev/null)\" = \"$sshdtgt\" ] || { echo 'root tree: /bin/sshd is not a symlink to the staged sshd daemon' >&2; exit 1; }; \
     [ -f \"$root/etc/ssh/authorized_keys\" ] || { echo 'root tree: /etc/ssh/authorized_keys missing - the sshd daemon has no authorization source' >&2; exit 1; }; \
     dsz=$(wc -c < \"$disk\"); \
     [ \"$dsz\" -ge 4096 ] || { echo \"root.erofs: implausibly small ($dsz bytes)\" >&2; exit 1; }; \
     set -- $(od -An -tx1 -j 1024 -N 4 \"$disk\"); \
     [ \"$1$2$3$4\" = e2e1f5e0 ] || { echo 'root.erofs: missing EROFS superblock magic at byte 1024' >&2; exit 1; }; \
     [ \"$(wc -l < \"$manifest\")\" -eq 4 ] || { echo 'manifest: expected header plus exactly three payload entries' >&2; exit 1; }; \
     [ \"$(head -n 1 \"$manifest\")\" = td-deployment-v1 ] || { echo 'manifest: unsupported or missing td-deployment-v1 header' >&2; exit 1; }; \
     for a in bzImage initramfs.cpio root.erofs; do \
         grep -q -E \"^[0-9a-f]{64}  $a$\" \"$manifest\" || { echo \"manifest: missing strict SHA-256 entry for $a\" >&2; exit 1; }; \
     done"
        // The busybox check names the concrete `{in:busybox-x86-64}` path, not a
        // `td/store/*/bin/busybox` glob: bash-mesboot 2.05b (this step's shell) can't expand
        // a wildcard in a non-terminal path component.
        //
        // Validate EVERY packed applet, not just the greeter-critical few. Names are all
        // shell-safe identifiers, so a space-joined `for` list is safe unquoted. uutils
        // cannot execute in the build sandbox because its absolute interpreter exists only
        // inside the assembled root; compare symlink text without resolving it. The headless
        // boot oracle executes uutils after pivoting and remains the behavioral runtime check.
        .replace("@BUSYBOX_APPLETS@", &BUSYBOX_APPLETS.join(" "))
        .replace("@INITRAMFS_APPLETS@", &INITRAMFS_APPLETS.join(" "))
        .replace("@UUTILS_APPLETS@", &UUTILS_APPLETS.join(" "))
        .replace("@TD_UTIL_APPLETS@", &TD_UTIL_APPLETS.join(" "))
        .replace("@TD_INIT_APPLETS@", &td_init_applets().join(" "))
}

pub fn recipe() -> Recipe {
    let mut steps = Vec::new();
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });

    // 1) Stage the real-root tree in build scratch. shadow gets a follow-up chmod 0600 (WriteFile can
    //    only set 0644/0755, and a world-readable shadow — even with empty/locked
    //    passwords — should not regress from the old gen_init_cpio 0600).
    steps.extend(real_root_steps(&SYSTEM));
    steps.push(
        Step::run(
            "{out}",
            &[SH, "-c", "chmod 0600 '{root}/real-root/etc/shadow'"],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 2) Pack distinct direct-boot selector and selected-deployment initramfs
    //    artifacts. Only the selector contains td-kexec.
    steps.push(Step::WriteFile {
        path: "{root}/selector-init".into(),
        content: build_selector_init(),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/selector.spec".into(),
        content: build_initramfs_spec("selector-init", Phase::Selector),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{root}/deployment-init".into(),
        content: build_deployment_init(&SYSTEM),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/deployment.spec".into(),
        content: build_initramfs_spec("deployment-init", Phase::Deployment),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/selector.spec' > '{root}/selector-initramfs.cpio'; \
                 '{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/deployment.spec' > '{root}/initramfs.cpio'",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 3) Materialise the first-class deployment bundle. PackErofs is executed
    //    by the derivation engine itself, never exposed to recipe argv/PATH.
    steps.push(Step::MkDir {
        path: "{out}/deployment".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/boot".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{in:linux-x86-64}/bzImage".into(),
            "{root}/initramfs.cpio".into(),
        ],
        dest: "{out}/deployment".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{root}/selector-initramfs.cpio".into()],
        dest: "{out}/boot".into(),
    });
    steps.push(Step::Sha256Manifest {
        output: "{out}/boot/manifest".into(),
        entries: vec![(
            "selector-initramfs.cpio".into(),
            "{out}/boot/selector-initramfs.cpio".into(),
        )],
    });
    steps.push(Step::PackErofs {
        root: "{root}/real-root".into(),
        output: "{out}/deployment/root.erofs".into(),
    });
    steps.push(Step::Sha256Manifest {
        output: "{out}/deployment/manifest".into(),
        entries: vec![
            ("bzImage".into(), "{out}/deployment/bzImage".into()),
            (
                "initramfs.cpio".into(),
                "{out}/deployment/initramfs.cpio".into(),
            ),
            ("root.erofs".into(), "{out}/deployment/root.erofs".into()),
        ],
    });

    // 4) Require the complete contract and shape-check every payload.
    steps.push(Step::Require {
        paths: vec![
            "{out}/deployment/bzImage".into(),
            "{out}/deployment/initramfs.cpio".into(),
            "{out}/deployment/root.erofs".into(),
            "{out}/deployment/manifest".into(),
            "{out}/boot/selector-initramfs.cpio".into(),
            "{out}/boot/manifest".into(),
        ],
        exec: false,
    });
    steps.push(Step::run("{out}", &[SH, "-c", &shape_check()]).env("PATH", &mesboot0_path()));

    Recipe::mesboot("system-x86-64", "0.2")
        // busybox: the static boot/greeter userland + the `cpio -t`/applet shape check.
        // linux-x86-64: the EXPORTED gen_init_cpio packer (verified STATICALLY linked).
        // uutils: the dynamically-linked `coreutils` multicall packed as the /bin file/text
        //   userland (#547).
        // ripgrep/fd: dynamically linked Rust search tools exposed as /bin/rg and /bin/fd.
        // sshd: the source-built russh SSH daemon, packed at /bin/sshd; its runtime closure
        //   (glibc, libgcc_s, aws-lc crypto C lib) is reached by StageRuntimeClosure.
        // glibc-x86-64: the dynamic Rust userland's declared runtime input.
        //   StageRuntimeClosure reaches it from embedded store references and copies the whole
        //   content-addressed item.
        // td-netd: the static network bring-up daemon (empty runtime closure, CopyTree'd).
        // td-boot: static initramfs selector and root-side deployment helper (CopyTree'd).
        // td-kexec: confined selector-only kexec helper.
        // td-util: the static diagnostics multicall (empty runtime closure, CopyTree'd),
        //   serving the /bin farm those five names resolve through.
        // td-init: the static boot-glue multicall (empty runtime closure, CopyTree'd). Not a
        //   farm like the others: it is /init (PID 1), the deployment initramfs' pivot, and
        //   the sysinit `hostname -F`, so the image's boot path runs it on every boot.
        .native_inputs(&[
            "busybox-x86-64",
            "linux-x86-64",
            "uutils",
            "ripgrep",
            "fd",
            "glibc-x86-64",
            "sshd",
            "td-netd",
            "td-boot",
            "td-kexec",
            "td-util",
            "td-init",
        ])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_contract_is_recipe_owned() {
        let steps = recipe().steps.expect("system recipe steps");
        let closures: Vec<(&Vec<String>, &String)> = steps
            .iter()
            .filter_map(|step| match step {
                Step::StageRuntimeClosure { roots, dest } => Some((roots, dest)),
                _ => None,
            })
            .collect();
        assert_eq!(
            closures.len(),
            1,
            "the image needs one runtime-closure step"
        );
        let (roots, dest) = closures.first().expect("one runtime closure");
        assert_eq!(
            roots.as_slice(),
            ["{in:uutils}", "{in:ripgrep}", "{in:fd}", "{in:sshd}"],
            "the dynamically linked shipped programs are the explicit runtime roots"
        );
        assert_eq!(dest.as_str(), "{root}/real-root");
        assert!(
            steps.iter().all(|step| !matches!(
                step,
                Step::CopyTree { from, .. }
                    if from.contains("uutils")
                        || from.contains("ripgrep")
                        || from.starts_with("{in:fd}")
                        || from.contains("glibc-x86-64")
            )),
            "runtime store items must not bypass StageRuntimeClosure"
        );
        for (name, target) in [("rg", "{in:ripgrep}/bin/rg"), ("fd", "{in:fd}/bin/fd")] {
            let link = format!("{{root}}/real-root/bin/{name}");
            let targets: Vec<&str> = steps
                .iter()
                .filter_map(|step| match step {
                    Step::Symlink {
                        target,
                        link: candidate,
                    } if candidate == &link => Some(target.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                targets,
                [target],
                "/bin/{name} must have one claimant resolving to its staged package"
            );
        }

        let native_inputs = recipe().native_inputs.expect("system native inputs");
        for forbidden in [
            "rust-stage0",
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
        ] {
            assert!(
                !native_inputs.iter().any(|input| input == forbidden),
                "build-only input {forbidden} must not be a direct system input"
            );
        }

        let pack = steps.iter().filter_map(|step| match step {
            Step::PackErofs { root, output } => Some((root.as_str(), output.as_str())),
            _ => None,
        });
        assert_eq!(
            pack.collect::<Vec<_>>(),
            vec![("{root}/real-root", "{out}/deployment/root.erofs")],
            "the recipe must pack its scratch root into the deployment output exactly once"
        );

        let manifests: Vec<&Vec<(String, String)>> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Sha256Manifest { output, entries }
                    if output == "{out}/deployment/manifest" =>
                {
                    Some(entries)
                }
                _ => None,
            })
            .collect();
        assert_eq!(manifests.len(), 1, "the deployment needs one manifest");
        let labels: Vec<&str> = manifests
            .first()
            .expect("one deployment manifest")
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(labels, ["bzImage", "initramfs.cpio", "root.erofs"]);
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Sha256Manifest { output, entries }
                if output == "{out}/boot/manifest"
                    && entries.as_slice()
                        == [(
                            "selector-initramfs.cpio".into(),
                            "{out}/boot/selector-initramfs.cpio".into(),
                        )]
        )));
    }

    /// The tailorable `SYSTEM` const is hand-edited to shape the distro; guard the
    /// invariants a bad edit would otherwise surface only as a silent boot failure —
    /// a getty respawn-looping on `login -f <missing-user>`, or a login shell that was
    /// never packed into /bin.
    #[test]
    fn system_def_is_self_consistent() {
        for user in SYSTEM.users {
            assert_eq!(
                SYSTEM
                    .users
                    .iter()
                    .filter(|candidate| candidate.name == user.name)
                    .count(),
                1,
                "user name '{}' must be unique",
                user.name
            );
        }
        assert!(
            SYSTEM
                .users
                .iter()
                .find(|user| user.name == SYSTEM.autologin)
                .is_some_and(|user| user.uid != 0),
            "autologin user '{}' must resolve uniquely to an unprivileged user",
            SYSTEM.autologin
        );
        for u in SYSTEM.users {
            assert!(
                valid_home(u.uid, u.home),
                "user '{}' home '{}' must be /root for uid 0 or one shell-safe direct \
                 child of /home for an unprivileged uid",
                u.name,
                u.home
            );
            // busybox `login` execs the shell by ABSOLUTE path (execv, no PATH search),
            // and we only pack applets under /bin, so the shell MUST be "/bin/<applet>"
            // packed by either farm. A bare "sh" would pass a naive basename check yet
            // fail at runtime (execv("sh") -> ENOENT -> login respawn-loops); reject it.
            let packed_applet = u.shell.strip_prefix("/bin/");
            assert!(
                packed_applet
                    .is_some_and(|a| BUSYBOX_APPLETS.contains(&a) || UUTILS_APPLETS.contains(&a)),
                "user '{}' login shell '{}' must be \"/bin/<applet>\" packed by a /bin farm \
                 (busybox login execs it by absolute path)",
                u.name,
                u.shell
            );
            // build_group only materialises the `wheel` supplementary group today; any
            // other declared group would be silently dropped from /etc/group (its
            // membership lost), so reject it until build_group learns to emit it.
            for g in u.groups {
                assert!(
                    *g == "wheel",
                    "user '{}' declares supplementary group '{}', but build_group only \
                     materialises \"wheel\"; give it a gid in build_group before naming it here",
                    u.name,
                    g
                );
            }
        }
    }

    /// getty auto-logs-in via `-l /etc/autologin`, and login needs both applets; the
    /// respawn line is inert without them. `reboot` is what `tty-session` execs when the
    /// greeter session ends (the in-guest power-off path). `switch_root` is the stage-1
    /// pivot applet — without it the two-stage boot cannot enter the erofs root. These are
    /// all boot-critical and must stay on a STATIC multicall (no runtime closure) — busybox
    /// for the login pair, td-init for `reboot`/`switch_root` since the cutover: belt-and-
    /// braces against a farm edit that drops one or reroutes it to dynamically-linked
    /// uutils (the shape check catches it at build time, this catches it at test time).
    #[test]
    fn greeter_and_pivot_applets_are_present() {
        // Split across two static multicalls now, so each name is pinned to the ONE that
        // serves it. Asserting only "some farm has it" would let a boot name drift between
        // binaries unnoticed — and for these names that drift IS the boot.
        for a in ["sh", "getty", "login", "mount", "umount"] {
            assert!(
                BUSYBOX_APPLETS.contains(&a),
                "boot-critical applet '{a}' missing from BUSYBOX_APPLETS"
            );
        }
        for a in ["init", "reboot", "switch_root", "hostname"] {
            assert!(
                td_init_applets().contains(&a),
                "boot-glue applet '{a}' missing from the td-init farm"
            );
        }
    }

    /// The /bin farms must be DISJOINT — a name in both would pack two conflicting symlinks
    /// for one applet (last-writer-wins, non-deterministic) and blur the static-vs-dynamic
    /// boot-safety boundary. Also pin the boot-critical names to a STATIC multicall: every
    /// one of them runs somewhere no dynamic loader is reachable (the pre-pivot initramfs)
    /// or where a failure has nowhere to be reported (PID 1's own sysinit), so what matters
    /// is not which static binary serves them but that uutils never does.
    #[test]
    fn applet_farms_are_disjoint_and_boot_names_stay_static() {
        // Four farms now, so check every pair: a name served twice emits two Symlink steps
        // for one link and the LAST one silently wins.
        let td_init = td_init_applets();
        let farms: [(&str, &[&str]); 4] = [
            ("busybox", BUSYBOX_APPLETS),
            ("uutils", UUTILS_APPLETS),
            ("td-util", TD_UTIL_APPLETS),
            ("td-init", &td_init),
        ];
        for (i, (a_name, a_set)) in farms.iter().enumerate() {
            for (b_name, b_set) in farms.iter().skip(i + 1) {
                for a in a_set.iter() {
                    assert!(
                        !b_set.contains(a),
                        "applet '{a}' is in BOTH the {a_name} and {b_name} farms - a name \
                         belongs to exactly one /bin farm"
                    );
                }
            }
        }
        for a in ["hostname", "mount", "umount", "sh", "init", "switch_root"] {
            assert!(
                BUSYBOX_APPLETS.contains(&a) || td_init.contains(&a),
                "boot-critical applet '{a}' must be served by a STATIC multicall (busybox or \
                 td-init) - it runs where no dynamic loader is reachable"
            );
            assert!(
                !UUTILS_APPLETS.contains(&a),
                "boot-critical applet '{a}' must NOT be served by dynamically-linked uutils"
            );
        }
    }

    /// The `/bin` names one initramfs actually carries, DERIVED from its gen_init_cpio spec.
    /// The two cpios differ (only the selector carries td-kexec), so each `/init` is checked
    /// against its own `/bin`, not a shared guess. Both `slink` and `file` count: everything
    /// is a symlink to the multicall today, but a directly packed binary is still a name the
    /// script may call.
    fn initramfs_bin_names(phase: Phase) -> Vec<String> {
        const BIN: &str = "/bin/";
        let spec = build_initramfs_spec("unused-init", phase);
        let mut names = Vec::new();
        for line in spec.lines() {
            let mut fields = line.split_whitespace();
            let (Some(kind), Some(path)) = (fields.next(), fields.next()) else {
                continue;
            };
            if matches!(kind, "slink" | "file") {
                if let Some(name) = path.strip_prefix(BIN) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    /// Every generated script TEXT, paired with the name each carries in the image (so a
    /// failure names the file to edit) and the `/bin` universe it resolves against: the two
    /// `/init` scripts run before the pivot, off a cpio whose `/bin` is far smaller than the
    /// real root's, so `None` means "the real root's packed farm".
    fn script_sources() -> Vec<(String, String, Option<Vec<String>>)> {
        let mut sources = vec![
            (
                "/init (selector)".to_string(),
                build_selector_init(),
                Some(initramfs_bin_names(Phase::Selector)),
            ),
            (
                "/init (deployment)".to_string(),
                build_deployment_init(&SYSTEM),
                Some(initramfs_bin_names(Phase::Deployment)),
            ),
        ];
        for (name, content, _) in etc_files(&SYSTEM) {
            sources.push((format!("/etc/{name}"), content, None));
        }
        sources
    }

    /// Every `busybox <applet>` the generated scripts invoke through the multiplexer must be
    /// one `shape_check` verifies against `busybox --list` — a packed farm name or an
    /// INITRAMFS_APPLETS entry. Derived from the script TEXT, so adding a `busybox
    /// <applet>` call without covering it reds here rather than at sysinit. This is what
    /// keeps applets like `chown`/`readlink` (served in /bin by uutils, but still invoked as
    /// `busybox chown` by rootcheck) from losing their busybox-side guarantee.
    ///
    /// Only the absolute `/bin/busybox <applet>` spelling is scannable, so the bare token is
    /// rejected outright rather than parsed for command position: deciding that from
    /// surrounding shell text needs a shell grammar, and every approximation of one leaves a
    /// form (`if`, a `case` arm, an assignment prefix) that reads as prose and escapes.
    #[test]
    fn script_applets_are_covered() {
        const TOKEN: &str = "busybox";
        let mut seen: Vec<String> = Vec::new();
        let mut sources_with_calls = 0usize;
        for (name, text, _) in script_sources() {
            let before_count = seen.len();
            for (idx, _) in text.match_indices(TOKEN) {
                let before = text.get(..idx).unwrap_or("");
                assert!(
                    before.ends_with("/bin/"),
                    "{name} spells the busybox multiplexer as a bare `{TOKEN}` token rather \
                     than `/bin/{TOKEN} <applet>`. In command position that runs it through \
                     $PATH, invisibly to this scan, so its applet would never be verified \
                     against `busybox --list`; write the absolute form (even in prose)"
                );
                let Some(rest) = text.get(idx + TOKEN.len()..) else {
                    continue;
                };
                // The charset spans every name busybox can expose: truncating `mkfs.ext2` to
                // `mkfs` would red naming an applet that does not exist.
                let rest = rest.trim_start();
                let applet: String = rest
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '[')
                    })
                    .collect();
                // Fail CLOSED on a form this cannot resolve (`/bin/busybox "$cmd"`): skipping
                // it would let the one invocation that most needs review escape the check.
                assert!(
                    !applet.is_empty(),
                    "{name} invokes the busybox multiplexer with a form this scan cannot \
                     resolve statically ({:?}) - it would silently escape the coverage check; \
                     invoke the applet under a literal name",
                    rest.chars().take(24).collect::<String>()
                );
                assert!(
                    BUSYBOX_APPLETS.contains(&applet.as_str())
                        || INITRAMFS_APPLETS.contains(&applet.as_str()),
                    "{name} invokes `busybox {applet}`, but neither the /bin farm nor \
                     INITRAMFS_APPLETS covers it - shape_check would never verify \
                     busybox implements it"
                );
                seen.push(applet);
            }
            if seen.len() > before_count {
                sources_with_calls += 1;
            }
        }
        // Guard the guard. A bare total would not bind: one `/init` alone contributes more
        // than any plausible floor, so dropping the whole `/etc` half of the scan would keep
        // a count green. Requiring several sources to contribute is what pins the inputs.
        assert!(
            sources_with_calls >= 3,
            "only {sources_with_calls} generated script(s) matched a `/bin/busybox <applet>` \
             call - the scan has gone stale or stopped being fed its sources, and this test \
             is now vacuous"
        );
        // ...and no dead entries: a multiplexed applet nothing invokes is a stale shape-check
        // probe that outlived the call it was protecting, and would red the build for a
        // legitimate busybox config trim. Script text is not the only source of the
        // requirement — td-boot invokes its own set from Rust, where this scan cannot see it,
        // so those are justified by the protocol constant instead.
        for a in INITRAMFS_APPLETS {
            assert!(
                seen.iter().any(|s| s == a)
                    || td_boot_protocol::REQUIRED_BUSYBOX_APPLETS.contains(a),
                "INITRAMFS_APPLETS lists '{a}', but no generated script invokes `busybox {a}` \
                 and td-boot does not require it - drop it, or move it to the /bin farm if it \
                 needs a symlink"
            );
        }
    }

    /// Every name `real_root_steps` actually links into the real root's /bin, DERIVED from
    /// the steps rather than restated: both farms plus each binary packed by hand. A list
    /// spelled out here would silently rot behind a newly packed daemon.
    fn packed_bin_names() -> Vec<String> {
        const LINK_PREFIX: &str = "{root}/real-root/bin/";
        let mut names = Vec::new();
        for step in real_root_steps(&SYSTEM) {
            if let Step::Symlink { link, .. } = step {
                if let Some(name) = link.strip_prefix(LINK_PREFIX) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    /// The mirror of `script_applets_are_covered`, for the form this commit actually
    /// re-points: a direct `/bin/<name>`. Every one must be a name its own image actually
    /// packs, or the script calls a dangling symlink — and `shape_check` only validates
    /// farm → symlink, never script → farm, so nothing else would catch it.
    ///
    /// The two `/init` scripts are stricter: each runs from its cpio, whose `/bin` holds
    /// only the handful of `slink`s `build_initramfs_spec` emits, and runs BEFORE the pivot
    /// that makes uutils' glibc closure reachable. A `/bin/<anything-else>` there is an
    /// unbootable image, even for a name the real root packs.
    #[test]
    fn direct_bin_calls_resolve_to_a_packed_name() {
        const PREFIX: &str = "/bin/";
        let packed = packed_bin_names();
        // Guard the derivation: if it stops seeing real_root_steps' symlinks it would
        // accept nothing (and red on everything) or, worse, be edited into accepting all.
        let td_init = td_init_applets();
        for a in BUSYBOX_APPLETS
            .iter()
            .chain(UUTILS_APPLETS)
            .chain(TD_UTIL_APPLETS)
            .chain(&td_init)
            .chain(&["busybox", "td-util", "td-init"])
        {
            assert!(
                packed.iter().any(|p| p == a),
                "'{a}' is a /bin name this file packs, but packed_bin_names() did not derive \
                 it from real_root_steps - the derivation has gone stale"
            );
        }
        // The pre-pivot half is the strict one, and it lives entirely in the sources that
        // carry a cpio /bin. Pin their count here: a script_sources() that stopped returning
        // the two inits would make that half vacuous with every remaining assertion green.
        let cpio_backed = script_sources().iter().filter(|(_, _, c)| c.is_some()).count();
        assert_eq!(
            cpio_backed, 2,
            "expected the selector and deployment inits to be checked against their own cpio \
             /bin, found {cpio_backed} such sources - the pre-pivot half has gone vacuous"
        );
        for (name, text, cpio_bin) in script_sources() {
            // Same guard for the initramfs side: an empty derived /bin would accept nothing.
            if let Some(names) = &cpio_bin {
                assert!(
                    names.iter().any(|n| n == "busybox"),
                    "{name} runs from a cpio whose derived /bin does not contain busybox - \
                     initramfs_bin_names() has gone stale"
                );
            }
            let available = cpio_bin.as_ref().unwrap_or(&packed);
            for (idx, _) in text.match_indices(PREFIX) {
                let before = text.get(..idx).unwrap_or("");
                // Only the ROOT /bin. `/td/store/<hash>/bin/coreutils` is an absolute store
                // path, not a call into the farm this checks, and would resolve against the
                // wrong universe.
                if before.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '.') {
                    continue;
                }
                let Some(rest) = text.get(idx + PREFIX.len()..) else {
                    continue;
                };
                let called: String = rest
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '[')
                    })
                    .collect();
                // Fail CLOSED, as the multiplexer scan does: `/bin/"$cmd"` and `/bin//ls`
                // both leave this empty, and skipping them would let the calls that most
                // need review escape unchecked.
                assert!(
                    !called.is_empty(),
                    "{name} calls /bin/ with a form this scan cannot resolve statically \
                     ({:?}) - it would silently escape the check; call it under a literal name",
                    rest.chars().take(24).collect::<String>()
                );
                assert!(
                    available.iter().any(|p| *p == called),
                    "{name} calls /bin/{called}, which its image does not pack - it would be \
                     a dangling symlink at boot; add it to a farm or call it through the \
                     busybox multiplexer"
                );
            }
        }
    }

    /// The uutils recipe must build exactly the applets we symlink into /bin.
    /// coreutils 0.9.0 names each applet's cargo feature after the applet, so an
    /// applet in UUTILS_APPLETS with no matching feature would dispatch to nothing
    /// (a dead /bin symlink), and a feature we don't symlink is dead weight in the
    /// COMPILED graph. Selecting only these applets (vs the `feat_Tier1`/`unix`
    /// aggregate) trims what cargo COMPILES and links — NOT the derivation's INPUT
    /// closure: the committed Cargo.lock still pins the full resolved set (507 crates)
    /// and stage_verified_vendor interns every pinned `.crate` — build-time-unused cc /
    /// bindgen / clang-sys sources included — as authenticated input. So only the
    /// compiled/linked graph is the smaller, cc-free one; shrinking the interned input
    /// set too would need a committed selected-closure sub-lock. Guard the
    /// feature↔applet coupling here.
    #[test]
    fn uutils_recipe_builds_exactly_the_shipped_farm() {
        let uutils = crate::catalog::lookup("uutils")
            .expect("uutils recipe must be registered in the catalog");
        let feats: std::collections::BTreeSet<&str> = uutils
            .features
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect();
        let applets: std::collections::BTreeSet<&str> = UUTILS_APPLETS.iter().copied().collect();
        assert_eq!(
            feats, applets,
            "uutils recipe features must equal UUTILS_APPLETS; drift means a dead \
             /bin symlink or a wasted crate subtree"
        );
        assert_eq!(
            uutils.no_default_features,
            Some(true),
            "uutils must set no_default_features so only the shipped applets build \
             (the default `feat_common_core` pulls ~76 utilities)"
        );
    }

    /// The inittab must respawn `tty-session` (not a bare getty), run `rootcheck` at
    /// sysinit (the read-only-root self-check), and `tty-session` must ask init to reboot
    /// after the login flow — the "exit / Ctrl-D powers off the VM" path. A refactor that
    /// reverts the inittab to a bare getty, drops rootcheck, or drops the reboot would
    /// silently strip a guarantee; red it here.
    #[test]
    fn exit_powers_off_and_rootcheck_runs() {
        let inittab = build_inittab();
        assert!(
            inittab.contains("ttyS0::respawn:/etc/tty-session"),
            "inittab must respawn /etc/tty-session on ttyS0 (the getty -> reboot wrapper)"
        );
        assert!(
            inittab.contains("::sysinit:/etc/rootcheck"),
            "inittab must run /etc/rootcheck at sysinit (the read-only-root self-check)"
        );
        assert!(
            inittab.contains("::sysinit:/etc/netup"),
            "inittab must run /etc/netup at sysinit (network bring-up + resolve/reach self-test)"
        );
        assert!(
            inittab.contains("::once:/etc/bootsuccess"),
            "inittab must start the root-owned health target"
        );
        assert!(
            inittab.contains("::once:/etc/bootfail"),
            "inittab must start the isolated failed-target watchdog"
        );
        // td-init supervises with no signals, so an inittab action it does not implement is
        // not "inert" — it is a line PID 1 reports as unsupported on every boot, and (for
        // `shutdown`) a teardown silently never run. shape_check dry-runs this table through
        // the real parser; this catches the same thing without a target build.
        const SUPPORTED_ACTIONS: [&str; 4] = ["sysinit", "wait", "once", "respawn"];
        for line in inittab
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        {
            let mut fields = line.splitn(4, ':');
            let action = fields.nth(2).unwrap_or("");
            assert!(
                SUPPORTED_ACTIONS.contains(&action),
                "inittab line {line:?} uses action '{action}', which td-init does not \
                 implement - it supervises with NO signals, so `ctrlaltdel`/`shutdown`/\
                 `restart` have nothing to trigger them. Teardown belongs in /etc/tty-session"
            );
        }
        let session = build_tty_session();
        // getty must gate BOTH the teardown and the reboot (`&&`), so a FAILED session
        // respawns rather than tearing a live system down and masking a broken greeter as a
        // clean exit-0 shutdown.
        assert!(
            session.contains("/bin/getty ")
                && session.contains("-l /etc/autologin ")
                && session.contains("&& { /etc/shutdown; exec /bin/reboot; }")
                && !session.contains("reboot -f"),
            "tty-session must run getty (autologin at /etc/autologin) then, only on success, \
             run the teardown and reboot, while a failed login retries"
        );
        // The teardown runs AFTER the greeter shell — the ttyS0 session leader — exited, so
        // the kernel has already vhangup'd that terminal and writes through the inherited
        // descriptor return EIO. Without this the machine still reboots and the marker is
        // simply lost, which is indistinguishable from a teardown that never ran. Observed,
        // not theorised.
        assert!(
            session.contains("{ /etc/shutdown; exec /bin/reboot; } >/dev/console 2>&1"),
            "the teardown must write to /dev/console, not the hung-up tty it inherits from \
             the ended login session - otherwise TD-SHUTDOWN-OK is written to a descriptor \
             returning EIO and the oracle reds on a teardown that actually worked"
        );
        // `;` between them, not `&&`: a machine that refuses to reboot because it could not
        // unmount is worse than one that reboots after a sync. The marker, not the exit
        // status, is what carries the failure to the oracle.
        // Assert the separator POSITIVELY: forbidding `&&` alone still admits `|| exit 1`
        // and every other operator that makes the reset conditional on the teardown.
        assert!(
            session.contains("/etc/shutdown; exec /bin/reboot"),
            "a failed teardown must not block the reset - `;` between them, so the reboot is \
             unconditional and a failure is carried by the withheld marker, not the exit status"
        );
        let shutdown = build_shutdown();
        assert!(
            shutdown.contains("/bin/busybox sync || {")
                && shutdown.contains("/bin/busybox umount /var || {")
                && shutdown.contains("/bin/busybox umount -a -r || {")
                && shutdown.contains("/bin/busybox test \"$ok\" = 1")
                && shutdown.contains(SYSTEM_SHUTDOWN_MARKER),
            "the teardown must attempt every safety step and emit its marker only when all pass"
        );
    }

    /// The first pass must select through td-boot. The selected pass must bind
    /// root.erofs through td-boot, mount persistent @var, leave `/etc`
    /// untouched, and `exec switch_root` so the pivot inherits PID 1.
    #[test]
    fn distinct_initramfses_select_then_mount_persistent_state() {
        let selector = build_selector_init();
        let init = build_deployment_init(&SYSTEM);
        assert!(
            selector.contains("exec /bin/td-boot boot /dev/vda /volume")
                && !selector.contains("root-loop")
                && init.contains("root-loop")
                && !init.contains("td-boot boot"),
            "the direct-boot selector and selected-deployment initramfs must be structurally distinct"
        );
        // Fail-safe: abort on the first mount failure instead of switch_rooting
        // into a partial system.
        assert!(
            selector.contains("\nset -e\n") && init.contains("\nset -e\n"),
            "both initramfs phases must fail closed"
        );
        assert!(
            selector.contains("\nset -f\n")
                && init.contains("\nset -f\n")
                && init.contains("duplicate td.deployment handoff")
                && init.contains("missing td.deployment handoff"),
            "both initramfses must disable globbing and the selected phase must require one handoff"
        );
        assert!(
            init.contains("/bin/td-boot root-loop /volume")
                && init.contains("mount -t erofs -o ro /dev/loop0 /sysroot"),
            "the selected pass must bind the reverified root to a read-only loop device"
        );
        assert!(
            init.contains("subvol=@var /dev/vda /sysroot/var")
                && init.contains("tmpfs /sysroot/run")
                && init.contains("tmpfs /sysroot/tmp")
                && init.contains("rm -rf /sysroot/var/run")
                && init.contains("ln -s /run /sysroot/var/run"),
            "stage-1 init must mount persistent @var, keep runtime state volatile, and link /var/run into /run"
        );
        assert!(
            !init.contains("tmpfs /sysroot/var")
                && !init.contains("-t overlay")
                && !init.contains(" /sysroot/etc"),
            "stage-1 init must not restore tmpfs state or an overlay over immutable /etc"
        );
        assert!(
            init.contains("mount -o move /volume /sysroot/run/td-volume")
                && init.contains("printf '%s\\n' \"$deployment\" > /sysroot/run/td-deployment")
                && init.contains("chmod 0600 /sysroot/run/td-deployment")
                && init.contains("/bin/busybox umount /proc")
                && init.contains("/bin/busybox umount /dev"),
            "the selected id must be handed to the real root before the verified backing volume moves below it"
        );
        assert!(
            init.contains("/sysroot/var/home"),
            "stage-1 init must create the shared home-state directory"
        );
        for user in SYSTEM.users {
            let path = format!("/sysroot/var{}", user.home);
            assert!(
                init.contains(&path),
                "stage-1 init must create state directory {path} before switch_root"
            );
        }
        assert!(
            init.contains("umask 077") && init.contains("mkdir -p /sysroot/var/root"),
            "stage-1 init must create the root home with mode 0700"
        );
        assert!(
            init.contains("chown 0:0 /sysroot/var")
                && init.contains("chmod 0755 /sysroot/var")
                && init.contains("chmod 0700 /sysroot/var/root"),
            "selected init must normalize persistent state ownership and modes"
        );
        assert!(
            init.trim_end().ends_with("exec /bin/switch_root /sysroot /init"),
            "stage-1 init must END by exec-ing switch_root so the pivot inherits PID 1"
        );
    }

    #[test]
    fn homes_are_immutable_links_into_var() {
        let steps = real_root_steps(&SYSTEM);
        for (link, target) in [("/home", "var/home"), ("/root", "var/root")] {
            let path = format!("{{root}}/real-root{link}");
            assert!(
                steps.iter().any(|step| matches!(
                    step,
                    Step::Symlink {
                        target: actual_target,
                        link: actual_link,
                    } if actual_target == target && actual_link == &path
                )),
                "{link} must be an immutable root-image symlink to {target}"
            );
            assert!(
                steps.iter().all(|step| !matches!(
                    step,
                    Step::MkDir { path: actual } if actual == &path
                )),
                "{link} must not also be materialized as a root-image directory"
            );
        }
    }

    #[test]
    fn home_validation_rejects_shell_and_path_syntax() {
        for home in [
            "/home/..",
            "/home/.",
            "/home/a b",
            "/home/a;b",
            "/home/a/b",
            "/srv/user",
            "/root",
        ] {
            assert!(!valid_home(1000, home), "unsafe user home passed: {home}");
        }
        assert!(valid_home(0, "/root"));
        assert!(valid_home(1000, "/home/test-user_1.0"));
    }

    /// The read-only-root self-check must emit both diagnostic markers the headless
    /// oracle asserts on, and the greeter must emit its marker and honour the autotest
    /// exit — the seam between the recipe and `qemu-boot-system`.
    #[test]
    fn boot_markers_are_wired() {
        let rootcheck = build_rootcheck(&SYSTEM);
        let steps = real_root_steps(&SYSTEM);
        assert!(
            rootcheck.contains(SYSTEM_ROOT_RO_MARKER),
            "rootcheck must emit the ro-root marker"
        );
        assert!(
            rootcheck.contains(SYSTEM_ETC_RO_MARKER),
            "rootcheck must emit the immutable-/etc marker"
        );
        assert!(
            rootcheck.contains(SYSTEM_STATE_WRITABLE_MARKER),
            "rootcheck must emit the writable-state marker"
        );
        assert!(
            rootcheck.contains("readlink /var/run)\" = /run"),
            "rootcheck must prove /var/run resolves into volatile /run"
        );
        assert!(
            rootcheck.contains(SYSTEM_STATE_OWNER_MARKER)
                && rootcheck.contains("test ! -w /var")
                && rootcheck.contains("test ! -w /var/root"),
            "rootcheck must prove the login user cannot own system state"
        );
        assert!(
            rootcheck.contains(PERSIST_WRITE_CMDLINE_TOKEN)
                && rootcheck.contains(SYSTEM_PERSIST_WRITE_MARKER)
                && rootcheck.contains(PERSIST_READ_CMDLINE_TOKEN)
                && rootcheck.contains(SYSTEM_PERSIST_READ_MARKER),
            "rootcheck must wire both halves of the two-boot persistence oracle"
        );
        assert!(
            rootcheck.contains("test ! -e /var/lib/td/boot-marker")
                && rootcheck.contains("&& /bin/busybox sync"),
            "the write marker must require a fresh path and a successful sync"
        );
        let bootsuccess = build_bootsuccess(&SYSTEM);
        let bootfail = build_bootfail();
        let profile = build_profile(&SYSTEM);
        assert!(
            rootcheck.contains("td-rootcheck-v1 > /run/td-rootcheck-ok")
                && bootsuccess.contains("set -f")
                && bootsuccess.contains("su -s /bin/sh tester -c")
                && bootsuccess.contains("/bin/cat /etc/os-release")
                && bootsuccess.contains(&format!(
                    "/bin/rg --color never --no-filename --fixed-strings --line-regexp -- {} \
                     /etc/hostname",
                    SYSTEM.hostname
                ))
                && bootsuccess.contains(
                    "/bin/fd --color never --absolute-path --max-depth 1 ^hostname$ /etc"
                )
                && bootsuccess.contains(RIPGREP_FD_RUNTIME_MARKER)
                && bootsuccess.contains("/bin/sshd selftest")
                && bootsuccess.contains("/bin/td-util --list")
                && bootsuccess.contains(TD_UTIL_RUNTIME_MARKER)
                && bootsuccess
                    .contains("td-boot success /dev/vda /run/td-update \"$deployment\"")
                && bootsuccess.contains(SYSTEM_BOOT_SUCCESS_MARKER)
                && bootsuccess.contains("test -n \"$deployment\" || fail")
                && bootsuccess.contains(&format!("wait={BOOT_SUCCESS_RETRY_SECS}"))
                && bootsuccess.contains(BOOT_SUCCESS_WAIT_CMDLINE_PREFIX)
                && bootsuccess.contains(&format!(
                    "case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_SUCCESS_RETRY_SECS}"
                ))
                && bootsuccess.contains(&format!(
                    "[ \"$wait\" -gt {BOOT_SUCCESS_RETRY_MAX_SECS} ] \
                     && wait={BOOT_SUCCESS_RETRY_MAX_SECS}"
                ))
                && bootsuccess.contains("while [ \"$n\" -lt \"$wait\" ]")
                && profile.contains("/run/td-boot-success-ok"),
            "the root-owned target must probe unprivileged runtime health, retry, and acknowledge the exact deployment"
        );
        assert!(
            bootsuccess.find("/bin/cat /etc/os-release").unwrap()
                < bootsuccess
                    .find("&& /bin/td-boot success /dev/vda")
                    .unwrap()
                && bootsuccess.find("/bin/rg --color never").unwrap()
                    < bootsuccess
                        .find("&& /bin/td-boot success /dev/vda")
                        .unwrap()
                && bootsuccess.find("/bin/fd --color never").unwrap()
                    < bootsuccess
                        .find("&& /bin/td-boot success /dev/vda")
                        .unwrap()
                && bootsuccess.find("/bin/sshd selftest").unwrap()
                    < bootsuccess
                        .find("&& /bin/td-boot success /dev/vda")
                        .unwrap()
                && bootsuccess.find("/bin/td-util --list").unwrap()
                    < bootsuccess
                        .find("if [ \"$healthy\" = 1 ]")
                        .unwrap(),
            "deployment success must follow every unprivileged runtime probe"
        );
        assert!(
            bootsuccess.contains(
                &format!(
                    "r=$(/bin/rg --color never --no-filename --fixed-strings --line-regexp -- {} \
                     /etc/hostname) || {{ echo \"ripgrep: /bin/rg failed\"; exit 1; }}",
                    SYSTEM.hostname
                )
            ) && bootsuccess.contains(
                "f=$(/bin/fd --color never --absolute-path --max-depth 1 ^hostname$ /etc) || \
                 { echo \"fd: /bin/fd failed\"; exit 1; }"
            ) && bootsuccess.contains(&format!(
                "[ \"$f\" = /etc/hostname ] || \
                 {{ echo \"fd: unexpected hostname path: $f\"; exit 1; }}'; then \
                 [ \"$mrf\" = 1 ] || {{ echo {RIPGREP_FD_RUNTIME_MARKER}; mrf=1; }}; \
                 else healthy=0; fi"
            )),
            "ripgrep and fd must both return exact results before their shared marker is emitted"
        );
        let configured = SystemDef {
            hostname: "configured.host",
            ..SYSTEM
        };
        let configured_bootsuccess = build_bootsuccess(&configured);
        assert!(
            configured_bootsuccess.contains(
                "r=$(/bin/rg --color never --no-filename --fixed-strings --line-regexp -- \
                 configured.host /etc/hostname)"
            ) && configured_bootsuccess.contains("[ \"$r\" = configured.host ]"),
            "the ripgrep health probe must follow the configured hostname without treating \
             hostname punctuation as a regular expression"
        );
        assert!(
            bootsuccess.contains(DEPLOY_INSTALL_CMDLINE_TOKEN)
                && bootsuccess.contains("td-boot install /dev/vda /run/td-update")
                && bootsuccess.contains(SYSTEM_DEPLOY_INSTALL_MARKER),
            "the root-owned health target must wire transactional install through td-boot"
        );
        assert!(
            bootfail.contains(BOOT_FAIL_TARGET_CMDLINE_TOKEN)
                && bootfail.contains("set -f")
                && bootfail.contains("exec /bin/reboot")
                && bootfail.contains(BOOT_FAIL_PARKED)
                && bootfail.contains("greeter park handshake timed out")
                && bootfail.contains(&format!("wait={BOOT_FAIL_PARK_WAIT_SECS}"))
                && bootfail.contains(BOOT_SUCCESS_WAIT_CMDLINE_PREFIX)
                && bootfail.contains(&format!(
                    "case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_FAIL_PARK_WAIT_SECS}"
                ))
                && bootfail.contains(&format!(
                    "[ \"$wait\" -gt {BOOT_FAIL_PARK_WAIT_SECS} ] \
                     && wait={BOOT_FAIL_PARK_WAIT_SECS}"
                ))
                && bootfail.contains("while [ \"$n\" -lt \"$wait\" ]")
                && profile.contains("cd /")
                && profile.contains("> /run/td-boot-parked")
                && profile.contains("while :; do /bin/busybox sleep 300; done"),
            "the failed-target injection must park outside /var before its watchdog reboots"
        );
        assert!(
            steps.iter().any(|step| matches!(
                step,
                Step::CopyTree { from, dest }
                    if from == "{in:td-boot}" && dest == "{root}/real-root{in:td-boot}"
            )) && steps.iter().any(|step| matches!(
                step,
                Step::Symlink { target, link }
                    if target == "{in:td-boot}/bin/td-boot"
                        && link == "{root}/real-root/bin/td-boot"
            )),
            "the immutable root must pack td-boot and expose /bin/td-boot for transactions"
        );
        // Home ownership is fixed for every non-root user below /var.
        for u in SYSTEM.users {
            if u.uid != 0 {
                assert!(
                    rootcheck.contains(&format!("chown {}:{} {}", u.uid, u.gid, u.home)),
                    "rootcheck must chown {}'s /var-backed home",
                    u.name
                );
            }
        }
        let profile = build_profile(&SYSTEM);
        assert!(
            profile.contains(GREETER_MARKER),
            "profile must emit the greeter marker"
        );
        assert!(
            profile.contains(AUTOTEST_CMDLINE_TOKEN)
                && profile.contains("set -f; wait=0")
                && profile.contains("exit"),
            "profile must exit on the autotest cmdline token so the headless boot powers off"
        );
        // The root-owned target must prove uutils runs as the unprivileged user before
        // it emits the marker, so a broken runtime closure reds the oracle.
        assert!(
            bootsuccess.contains(UUTILS_RUNTIME_MARKER),
            "the root-owned target must emit the uutils runtime marker"
        );
        assert!(
            bootsuccess.contains("/bin/cat /etc/os-release")
                && bootsuccess.find("/bin/cat /etc/os-release").unwrap()
                    < bootsuccess.find(&format!("echo {UUTILS_RUNTIME_MARKER}")).unwrap(),
            "the uutils marker must follow a successful unprivileged absolute-path invocation"
        );
        assert!(
            bootsuccess.contains("finish td-boot-failure-v1")
                && bootsuccess.contains("chmod 0644 /run/td-boot-success-ok")
                && profile.contains("td-boot-failure-v1")
                && profile.contains("cat /run/td-boot-success-ok")
                && profile.contains(BOOT_SUCCESS_WAIT_CMDLINE_PREFIX)
                && profile.contains("while [ \"$n\" -lt \"$wait\" ]"),
            "the root-owned completion status must be readable by the headless greeter"
        );

        // Networking: netup brings the link up unconditionally, and under the nettest
        // token self-tests resolve + reach, printing the three net markers. Each marker
        // must be `&&`-gated on its td-netd subcommand so a failure drops the marker and
        // reds the qemu-boot-net oracle rather than false-passing.
        let netup = build_netup();
        assert!(
            netup.contains("/bin/td-netd up"),
            "netup must bring the link up via td-netd on every boot"
        );
        assert!(
            netup.contains(NETTEST_CMDLINE_TOKEN),
            "netup must gate its self-test on the nettest cmdline token"
        );
        assert!(
            netup.contains(SYSTEM_NET_UP_MARKER),
            "netup must emit the link-up marker"
        );
        assert!(
            netup.contains(&format!(
                "/bin/td-netd resolve {NETTEST_DEFAULT_HOST} && echo {SYSTEM_NET_RESOLVE_MARKER}"
            )),
            "the resolve marker must be gated on a successful td-netd resolve"
        );
        assert!(
            netup.contains(&format!(
                "/bin/td-netd reach {NETTEST_DEFAULT_HOST} {NETTEST_DEFAULT_PORT} && echo {SYSTEM_NET_REACH_MARKER}"
            )),
            "the reach marker must be gated on a successful td-netd reach"
        );
    }

    #[test]
    fn initramfs_packs_the_verified_boot_chain() {
        let selector = build_initramfs_spec("selector-init", Phase::Selector);
        let deployment = build_initramfs_spec("deployment-init", Phase::Deployment);
        for entry in [
            "file {in:td-boot}/bin/td-boot {in:td-boot}/bin/td-boot 0755 0 0",
            "slink /bin/td-boot {in:td-boot}/bin/td-boot 0777 0 0",
            "dir /volume 0700 0 0",
            "dir /proc 0755 0 0",
            "dir /run 0755 0 0",
        ] {
            assert!(
                selector.contains(entry) && deployment.contains(entry),
                "both initramfs specs need boot-chain entry {entry}"
            );
        }
        assert!(
            selector
                .contains("file {in:td-kexec}/bin/td-kexec {in:td-kexec}/bin/td-kexec 0755 0 0")
                && selector.contains("slink /bin/td-kexec {in:td-kexec}/bin/td-kexec 0777 0 0")
                && !deployment.contains("{in:td-kexec}"),
            "only the selector initramfs may carry td-kexec"
        );
        // The mirror image: the pivot applet belongs to the phase that pivots. The selector
        // kexecs and has no branch that enters a root, so a td-init there would be a
        // capability its /init cannot reach — and one more payload in the cpio the firmware
        // path loads.
        assert!(
            deployment.contains("file {in:td-init}/bin/td-init {in:td-init}/bin/td-init 0755 0 0")
                && deployment.contains("slink /bin/switch_root {in:td-init}/bin/td-init 0777 0 0")
                && !selector.contains("{in:td-init}"),
            "only the deployment initramfs may carry td-init, and it must expose the pivot as \
             /bin/switch_root"
        );
        for applet in td_boot_protocol::REQUIRED_BUSYBOX_APPLETS {
            assert!(
                INITRAMFS_APPLETS.contains(applet),
                "td-boot invokes uncovered busybox applet {applet}"
            );
        }
    }

    /// td-util is packed, serves its whole /bin farm, and every one of those names is
    /// exercised by the greeter on the image. All three must hold together: a farm whose
    /// binary is not packed dangles, and a farm no probe runs is a cutover asserted only by
    /// symlink text — `shape_check` compares link targets and cannot execute an applet.
    #[test]
    fn td_util_serves_its_farm_and_every_name_is_probed() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-util}" && dest == "{root}/real-root{in:td-util}"
            )),
            "td-util must be CopyTree'd into the real root (static, empty closure)"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-util}/bin/td-util"
                        && link == "{root}/real-root/bin/td-util"
            )),
            "/bin/td-util must symlink into the store td-util package"
        );
        // Collect every step claiming each link, not the first: Step::Symlink is
        // last-writer-wins (build.rs unlinks before creating), so a name left in two farms
        // would ship whichever loop ran last while a first-match probe still found the
        // other and passed. Requiring exactly one claimant closes that and the general
        // duplicate-name hole; applet_farms_are_disjoint_... checks the same thing from the
        // list side.
        for applet in TD_UTIL_APPLETS {
            let link = format!("{{root}}/real-root/bin/{applet}");
            let targets: Vec<&str> = steps
                .iter()
                .filter_map(|s| match s {
                    Step::Symlink { target, link: l } if *l == link => Some(target.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                targets.len(),
                1,
                "exactly one Symlink step may claim /bin/{applet}, found {}: a later step \
                 silently overwrites an earlier one, so a second claimant re-points the name \
                 with nothing else noticing",
                targets.len()
            );
            assert_eq!(
                targets.first().copied(),
                Some("{in:td-util}/bin/td-util"),
                "/bin/{applet} must resolve to the staged td-util multicall"
            );
        }
        let bootsuccess = build_bootsuccess(&SYSTEM);
        // Every farm name must be probed BY ITS /bin PATH: that is what exercises the
        // shipped symlink and argv[0] dispatch. A probe over a subset would green-light
        // names it never ran.
        assert!(
            !TD_UTIL_APPLETS.is_empty(),
            "an empty farm would make every per-applet assertion below, and shape_check's \
             own farm loop, silently vacuous"
        );
        for applet in TD_UTIL_APPLETS {
            // Match the WHOLE generated segment, failure branch included. Matching the name
            // alone is satisfied by the diagnostic `echo`, which also contains it — the
            // command could be deleted and this would pass. And matching everything except
            // `u=0` leaves the gate defeatable: drop that one assignment and the marker
            // prints unconditionally, so the oracle greens with a broken applet.
            let args = if *applet == "which" { " sh" } else { "" };
            assert!(
                bootsuccess.contains(&format!(
                    "/bin/{applet}{args} >/dev/null 2>&1 || {{ echo \"td-util: /bin/{applet} \
                     failed\"; u=0; }}"
                )),
                "the health target must RUN /bin/{applet} by its literal /bin path AND clear the \
                 marker gate on failure - without the u=0 the marker is unconditional and \
                 the oracle passes a broken applet"
            );
        }
        // The marker is emitted by THIS leg, so match the whole gate: the `else healthy=0`
        // keeps a failure out of the deployment transaction, and the marker echo sits inside
        // the success branch, which is what makes an absent TD-UTIL-RUN-OK mean td-util
        // rather than "some component upstream of it failed".
        assert!(
            bootsuccess.contains(&format!(
                "[ \"$u\" = 1 ]'; then [ \"$mtu\" = 1 ] || {{ echo {TD_UTIL_RUNTIME_MARKER}; \
                 mtu=1; }}; else healthy=0; fi"
            )),
            "the health target must gate td-util health on every probed applet exiting 0, and \
             emit the td-util marker from that leg alone"
        );
    }

    /// td-init is packed, is /init and the pivot, serves its whole /bin farm, and every one
    /// of those names is exercised by the health target on the image.
    ///
    /// This farm differs from td-util's in what a mistake COSTS. A dead diagnostics symlink
    /// prints an error to somebody's terminal; a dead `/init` or `switch_root` is an image
    /// that does not boot, which the oracle reports as a 300s timeout with no cause attached.
    /// So the assertions here are about keeping every failure NAMED: one Symlink claimant per
    /// link, a probe per farm name, and — in shape_check — the shipped inittab driven through
    /// the real parser at build time.
    #[test]
    fn td_init_serves_its_farm_and_every_name_is_probed() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-init}" && dest == "{root}/real-root{in:td-init}"
            )),
            "td-init must be CopyTree'd into the real root (static, empty closure)"
        );
        // /init is the one that cannot merely dangle: the kernel's exec of it IS the boot.
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-init}/bin/td-init"
                        && link == "{root}/real-root/init"
            )),
            "/init must symlink into the store td-init package - it is PID 1"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-init}/bin/td-init"
                        && link == "{root}/real-root/bin/td-init"
            )),
            "/bin/td-init must symlink into the store td-init package"
        );
        // Collect every step claiming each link, not the first: Step::Symlink is
        // last-writer-wins, so a name left in two farms would ship whichever loop ran last
        // while a first-match probe still found the other and passed.
        for applet in td_init_applets() {
            let link = format!("{{root}}/real-root/bin/{applet}");
            let targets: Vec<&str> = steps
                .iter()
                .filter_map(|s| match s {
                    Step::Symlink { target, link: l } if *l == link => Some(target.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                targets.len(),
                1,
                "exactly one Symlink step may claim /bin/{applet}, found {}: a later step \
                 silently overwrites an earlier one, so a second claimant re-points the name \
                 with nothing else noticing",
                targets.len()
            );
            assert_eq!(
                targets.first().copied(),
                Some("{in:td-init}/bin/td-init"),
                "/bin/{applet} must resolve to the staged td-init multicall"
            );
        }
        let bootsuccess = build_bootsuccess(&SYSTEM);
        assert!(
            !TD_INIT_FARM.is_empty(),
            "an empty farm would make every per-applet assertion below, and shape_check's \
             own farm loop, silently vacuous"
        );
        // The segments are pasted inside the health target's single-quoted `su -c '…'`
        // argument, so ONE `'` anywhere in them ends it and hands the rest to the wrong
        // shell. The rule is stated at `td_init_probe`; this is what holds it — including
        // for `sys.hostname`, the one caller-supplied value that reaches generated shell here.
        assert!(
            !SYSTEM.hostname.is_empty()
                && SYSTEM
                    .hostname
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'),
            "hostname {:?} is interpolated into generated shell; keep it to characters that \
             need no quoting",
            SYSTEM.hostname
        );
        for (applet, probe) in TD_INIT_FARM {
            let segment = td_init_probe(applet, probe, &SYSTEM);
            assert!(
                !segment.contains('\''),
                "the probe segment for /bin/{applet} contains a single quote, which would \
                 terminate the health target's `su -c '...'` argument: {segment}"
            );
        }
        // Match the WHOLE generated segment, failure branch included: matching the applet
        // name alone is satisfied by the diagnostic `echo`, which also contains it, so the
        // invocation itself could be deleted and this would still pass. And matching
        // everything except the `i=0` leaves the gate defeatable — drop that one assignment
        // and the marker prints unconditionally.
        for (applet, probe) in TD_INIT_FARM {
            let segment = td_init_probe(applet, probe, &SYSTEM);
            assert!(
                bootsuccess.contains(&segment),
                "the health target must probe /bin/{applet} by its literal /bin path AND clear the \
                 marker gate on failure; expected segment:\n{segment}"
            );
            assert!(
                segment.contains("i=0"),
                "the probe for /bin/{applet} does not clear the marker gate, so the marker \
                 would print even when the applet misbehaved"
            );
        }
        // The irreversible three are probed by REFUSAL, never by invocation: a probe that
        // simply ran `/bin/reboot` would end the boot before the marker it gates.
        for applet in ["reboot", "poweroff", "halt"] {
            let probe = TD_INIT_FARM
                .iter()
                .find(|(name, _)| *name == applet)
                .map(|(_, probe)| probe);
            assert!(
                matches!(probe, Some(Probe::Refuses(_, _))),
                "/bin/{applet} must be probed through its REFUSAL - running it successfully \
                 ends the boot, so an invocation probe could never reach the marker it gates"
            );
        }
        // Same shape as td-util's: the marker belongs to THIS leg, so an absent
        // TD-INIT-RUN-OK localizes to the boot glue instead of being one of several markers
        // withheld together by whichever component happened to fail.
        assert!(
            bootsuccess.contains(&format!(
                "[ \"$i\" = 1 ]'; then [ \"$mti\" = 1 ] || {{ echo {TD_INIT_RUNTIME_MARKER}; \
                 mti=1; }}; else healthy=0; fi"
            )),
            "the health target must gate td-init health on every probe passing AND emit the \
             td-init marker from that leg alone, so an absent marker names the boot glue"
        );
        // The build-time half. These two legs are the only thing that turns "PID 1 rejects a
        // line of the shipped inittab" and "the pivot's fail-early refusal is gone" into a
        // named per-change failure instead of an image that does not come up; they run in the
        // daily tier, so nothing else here would notice their deletion.
        let shape = shape_check();
        assert!(
            shape.contains("init --dry-run -f \"$root/etc/inittab\""),
            "shape_check must dry-run the image's OWN /etc/inittab through the packed td-init \
             - without it an inittab line PID 1 rejects ships as an unbootable image whose \
             only symptom is the boot oracle timing out"
        );
        // Assert the two DISCRIMINATING pieces, not the phrase "not a mount point": that
        // phrase also occurs in the leg's own failure `echo`, so an assertion on it survives
        // deleting either the NEWROOT construction or the diagnostic test — the exact trap
        // this test warns about for the probe segments above.
        assert!(
            shape.contains("cp \"$tdi\" '{root}/pivot-probe/init'"),
            "shape_check must build a NEWROOT that HOLDS an init, so switch_root's earlier \
             init-resolution check cannot be what refuses - without it the mount-point guard \
             is never reached and could be deleted with this check still green"
        );
        assert!(
            shape.contains("case \"$tdipiv\" in *'not a mount point'*"),
            "shape_check must assert switch_root refused for the MOUNT-POINT reason, not merely \
             that it exited non-zero - that guard is what stands between a bad pivot and a \
             panicked kernel, and every other failure also exits non-zero"
        );
        // …and the farm loop is fed the whole farm, expanded — a placeholder left unreplaced
        // would iterate over the literal token and verify nothing.
        assert!(
            shape.contains(&format!("for a in {}; do", td_init_applets().join(" "))),
            "shape_check must loop over the EXPANDED td-init farm, verifying each /bin symlink \
             against the packed binary's own --list"
        );
    }

    /// Every diagnostic a refusal probe waits for must exist in the applet that emits it.
    ///
    /// `Probe::Refuses` asserts on td-init's own words — "unrecognised argument", "refusing to
    /// switch", "usage: cttyhack" — copied by hand across a crate boundary. Americanise one
    /// spelling or reword one refusal and the probe stops matching: the marker is withheld on
    /// a HEALTHY image, and the only thing that would have caught it is a manual
    /// qemu-boot-system run, which is not in check-pr or the daily tier. The recipe already
    /// embeds every applet source via `include_str!`, so the link costs nothing.
    #[test]
    fn every_expected_refusal_is_a_string_the_applet_actually_emits() {
        // applet -> the module that implements it (reboot/poweroff/halt are one module).
        fn module(applet: &str) -> &str {
            match applet {
                "reboot" | "poweroff" | "halt" => "halt",
                "switch_root" => "switchroot",
                other => other,
            }
        }
        let mut checked = 0;
        for (applet, probe) in TD_INIT_FARM {
            let Probe::Refuses(_, says) = probe else {
                continue;
            };
            let name = module(applet);
            let source = super::super::td_init::module_source(name)
                .unwrap_or_else(|| panic!("td-init embeds no module `{name}` for /bin/{applet}"));
            assert!(
                source.contains(says),
                "the probe for /bin/{applet} waits for {says:?}, which does not appear in \
                 td-init/src/{name}.rs - the applet was reworded and the probe now withholds \
                 TD-INIT-RUN-OK on a healthy image"
            );
            checked += 1;
        }
        assert!(
            checked >= 5,
            "expected every refusal probe to be pinned, checked only {checked}"
        );
    }

    /// Every script that decides to reboot must run the teardown itself.
    ///
    /// Under busybox `/bin/reboot` signalled PID 1 into `::shutdown:/etc/shutdown`. td-init
    /// supervises with NO signals, so that action does not exist and nothing catches a bare
    /// `exec /bin/reboot`: /var stays mounted (unclean Btrfs) and the shutdown marker never
    /// prints. This scans EVERY generated /etc file rather than the one initiator that
    /// existed when the rule was written — `/etc/bootfail` became a second one while this
    /// branch was in review, and a per-file assertion would not have noticed.
    #[test]
    fn reboots_run_the_teardown_first() {
        // Match `/bin/reboot`, not `exec /bin/reboot`: a caller that drops the `exec` still
        // reboots, and still skips the teardown, so keying on the `exec` would let exactly
        // that edit through (Agy review). The refusal PROBES also name /bin/reboot — on
        // purpose, with a bogus argument they must refuse — so strip the generated probe
        // segments first; they are the one invocation that does not reboot.
        // All three power applets, not just reboot: poweroff and halt end a boot identically,
        // so a script switched to either would escape a reboot-only scan while the count below
        // still held.
        let mut initiators = 0;
        for (name, body, _) in etc_files(&SYSTEM) {
            let mut body = body;
            for (applet, probe) in TD_INIT_FARM {
                body = body.replace(&td_init_probe(applet, probe, &SYSTEM), "");
            }
            for applet in ["reboot", "poweroff", "halt"] {
                for (at, _) in body.match_indices(&format!("/bin/{applet}")) {
                    initiators += 1;
                    assert!(
                        body.get(..at).unwrap_or_default().contains("/etc/shutdown"),
                        "/etc/{name} runs /bin/{applet} without running /etc/shutdown first - \
                         with no ::shutdown: action to catch it, /var is left mounted and the \
                         shutdown marker is never printed"
                    );
                }
            }
        }
        assert_eq!(
            initiators, 2,
            "expected exactly the two known reboot initiators (tty-session, bootfail); a new \
             one must run the teardown too, and a vanished one means this stopped covering it"
        );
    }

    /// td-netd must be packed and symlinked into /bin, and resolv.conf/hosts must be
    /// /etc symlinks into writable /run so the daemon can (re)write them under the
    /// read-only erofs root. A refactor that drops the CopyTree, the /bin symlink, or
    /// reverts the /etc files to plain writes would silently break network bring-up.
    #[test]
    fn td_netd_is_packed_and_etc_is_run_backed() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-netd}" && dest == "{root}/real-root{in:td-netd}"
            )),
            "td-netd package must be CopyTree'd into the real root (static, empty closure)"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-netd}/bin/td-netd"
                        && link == "{root}/real-root/bin/td-netd"
            )),
            "/bin/td-netd must symlink into the store td-netd package"
        );
        for name in ["resolv.conf", "hosts"] {
            let link = format!("{{root}}/real-root/etc/{name}");
            let target = format!("/run/{name}");
            assert!(
                steps.iter().any(|s| matches!(
                    s,
                    Step::Symlink { target: t, link: l } if *t == target && *l == link
                )),
                "/etc/{name} must be a symlink into /run (writable under the read-only /etc)"
            );
            // And NOT also written as a plain file (a WriteFile would shadow the symlink).
            assert!(
                steps.iter().all(|s| !matches!(
                    s,
                    Step::WriteFile { path, .. } if *path == link
                )),
                "/etc/{name} must be a /run symlink only, never a plain WriteFile"
            );
        }
    }
}
