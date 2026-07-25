use crate::ladder::{
    mesboot0_inputs, mesboot0_path, AUTOTEST_CMDLINE_TOKEN, GREETER_MARKER, NETTEST_CMDLINE_TOKEN,
    NETTEST_DEFAULT_HOST, NETTEST_DEFAULT_PORT, SH, SSHD_MARKER, SYSTEM_ETC_RO_MARKER,
    SYSTEM_NET_REACH_MARKER, SYSTEM_NET_RESOLVE_MARKER, SYSTEM_NET_UP_MARKER, SYSTEM_ROOT_RO_MARKER,
    SYSTEM_STATE_WRITABLE_MARKER, UUTILS_RUNTIME_MARKER,
};
use crate::types::{Recipe, Step};

// system-x86-64 (re #541, #550): a MINIMAL, TAILORABLE Rust-first Linux
// deployment, booted TWO-STAGE onto a disk-backed READ-ONLY erofs root.
//
// This is the "system definition" recipe. It composes artifacts that already exist in
// the ladder — the source-built `linux-x86-64` kernel and the td-built STATIC busybox —
// into a first-class deployment bundle:
//
//   deployment/{bzImage,initramfs.cpio,root.erofs,manifest}
//
// Stage 1 is a tiny initramfs: static busybox plus a `/init` script that mounts
// root.erofs read-only from virtio-blk, mounts writable tmpfs at `/var /run /tmp`,
// then switch_roots into stage 2. `/etc` stays deployment-owned and immutable;
// `/home` and `/root` are root-image symlinks into `/var`. Stage 2 is
// the store-native real root (busybox and uutils at their /td/store paths, a
// /bin symlink farm, and generated /etc). The typed PackErofs step invokes the
// dependency-free control-plane image writer directly; no recipe process can
// execute td-builder through PATH or argv. The versioned manifest hashes the
// three boot payloads.
//
// The busybox init auto-logs-in a test user to a shell with a welcome banner. EDIT the
// `SYSTEM` const below to tailor the distro (hostname, users, the auto-login user, the
// login shell, the applet set). A producer-rung shape check on the deployment
// bundle and its scratch root tree is the automated build guard; the interactive
// `td-recipe-eval run` boots the two-stage image under host qemu so you can use it, and
// the headless `td-recipe-eval qemu-boot-system` asserts it boots to the greeter on a
// read-only erofs root and powers off cleanly on `exit`.
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
            && name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
            })
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
           Minimal busybox userland, booted two-stage onto a read-only erofs root.\n  \
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

/// The real-root `/bin` is a symlink farm split across TWO multicall binaries, each
/// dispatching on argv[0]'s basename: the static **busybox** (the shell, boot/login/init
/// glue, and the non-coreutils tools) and the dynamically-linked Rust **uutils**
/// `coreutils` (the core file/text userland — #547's cutover). A name goes in exactly one
/// list; `shape_check` asserts the owning binary actually provides it.
///
/// BUSYBOX keeps everything the boot path needs and everything uutils does not provide.
/// The sysinit/greeter/login scripts invoke their applets as `/bin/busybox <applet>` (or
/// the busybox-served `/bin/mount`, `/bin/hostname -F`, `/bin/reboot`, `/bin/getty`,
/// `/bin/login`, `/bin/sh`), so the cutover never touches the boot-critical path — it only
/// changes what an interactive user's `PATH=/bin` resolves to.
///
/// `switch_root` is the stage-1 pivot applet: the init-initramfs execs
/// `/bin/busybox switch_root` to enter the erofs root. Listing it here both packs a
/// `/bin/switch_root` on the real root and — via `shape_check` — asserts the static
/// busybox actually implements it (a `CONFIG_SWITCH_ROOT` drift would red the build
/// rather than strand the two-stage boot).
///
/// `hostname` stays busybox: the inittab runs `/bin/hostname -F /etc/hostname` and uutils'
/// hostname has no `-F`. `find`/`xargs` are intentionally NOT bare symlinks either: the
/// ladder's findutils dead-axis lock (`no_bootstrap_step_invokes_host_find_or_xargs`)
/// forbids those tokens in any step text and can't tell a cpio member NAME from a host
/// invocation; they stay reachable as `busybox find` / `busybox xargs`.
const BUSYBOX_APPLETS: &[&str] = &[
    "sh", "ash", "getty", "login", "init", "mount", "umount", "switch_root", "reboot",
    "poweroff", "halt", "hostname", "ps", "clear", "dmesg", "free", "kill", "vi",
    "less", "more", "grep", "sed", "awk", "cttyhack", "su", "which", "readlink",
];

/// The core file/text userland, served by the uutils `coreutils` multicall (#547). Every
/// name must be a coreutils utility the built binary implements. The recipe sandbox cannot
/// exec the dynamically-linked binary to run `coreutils --list` at build time (its interp
/// resolves an absolute `/td/store` path that only exists on the assembled root, not in the
/// build tree). A missing applet surfaces on the boot oracle; the engine-native runtime
/// closure step proves every referenced store item is declared and stages it at its
/// canonical path. uutils is dynamically linked, so — unlike static busybox — it pulls its
/// reachable runtime store closure onto the erofs root.
const UUTILS_APPLETS: &[&str] = &[
    "uname", "ls", "cat", "echo", "printf", "pwd", "cp", "mv", "rm", "mkdir", "rmdir",
    "ln", "id", "env", "df", "du", "chmod", "chown", "sleep", "sync", "wc", "head",
    "tail", "sort", "date", "whoami", "tty", "dd", "mktemp", "seq", "touch", "mknod",
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
    // busybox init: `<id>::<action>:<process>`. `id` names the tty init opens for the
    // process; empty id => the system console. This inittab runs on the REAL root AFTER
    // stage-1 `switch_root`ed into it: init re-mounts the pseudo-filesystems (devtmpfs,
    // proc, sysfs) on the erofs root's empty mountpoint dirs — mounting over a read-only
    // dir is a VFS overlay, no write to the erofs — then runs the boot self-check, brings
    // the network up, starts the sshd service, and the auto-login getty. It does NOT mount
    // /var, /tmp, or /run: stage-1 already mounted those as tmpfs, and switch_root preserves
    // the mounts. /proc must precede /etc/rootcheck (which reads /proc/mounts).
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
    "::sysinit:/bin/mount -t devtmpfs devtmpfs /dev\n\
     ::sysinit:/bin/mount -t proc proc /proc\n\
     ::sysinit:/bin/mount -t sysfs sysfs /sys\n\
     ::sysinit:/bin/hostname -F /etc/hostname\n\
     ::sysinit:/etc/rootcheck\n\
     ::sysinit:/etc/netup\n\
     ::respawn:/bin/sshd serve --listen 0.0.0.0:22 --authorized-keys /etc/ssh/authorized_keys\n\
     ttyS0::respawn:/etc/tty-session\n\
     ::ctrlaltdel:/bin/reboot\n\
     ::shutdown:/bin/umount -a -r\n"
        .into()
}

/// The stage-1 init-initramfs `/init` (re #550): the FIRST userspace, run by the kernel
/// as PID 1 from the `init.cpio` initramfs. It mounts the read-only erofs store root over
/// virtio-blk, mounts the writable tmpfs filesystems, then `switch_root`s into the real
/// root. Static busybox with NO /bin PATH yet, so every applet is reached explicitly as
/// `/bin/busybox <applet>` (only `/bin/sh` and `/bin/busybox` are symlinked in the cpio);
/// `echo`-free by design. The final line MUST be `exec` so switch_root inherits PID 1.
fn build_stage1_init(sys: &SystemDef) -> String {
    // /var is the one state mount. /home and /root are immutable symlinks into it, so
    // replacing this tmpfs with a Btrfs @var mount does not change the real-root layout.
    // /run and /tmp remain separate volatile tmpfs mounts. EROFS is inherently read-only;
    // `-o ro` is belt-and-suspenders. The /dev/vda probe tolerates async virtio-blk attach.
    //
    // `set -e` makes the FIRST failing setup command abort the script — its error already
    // on the console (no 2>/dev/null) — rather than press on into a doomed or PARTIAL
    // `switch_root`: a failed mount then panics loudly (init exits) instead
    // of booting a half-read-only system where some dirs silently aren't writable (re #550,
    // Codex review). The `while` probe condition is exempt from `set -e`, so a missing
    // /dev/vda still falls through to the erofs mount, which fails and aborts cleanly.
    let mut init = "#!/bin/sh\n\
     set -e\n\
     /bin/busybox mount -t devtmpfs dev /dev\n\
     n=0\n\
     while /bin/busybox test \"$n\" -lt 5 && ! /bin/busybox test -b /dev/vda; do /bin/busybox sleep 1; n=$((n+1)); done\n\
     /bin/busybox mount -t erofs -o ro /dev/vda /sysroot\n\
     /bin/busybox mount -t tmpfs -o mode=0755 tmpfs /sysroot/var\n\
     /bin/busybox mount -t tmpfs -o mode=0755 tmpfs /sysroot/run\n\
     /bin/busybox mount -t tmpfs -o mode=1777 tmpfs /sysroot/tmp\n\
     /bin/busybox mkdir -p /sysroot/var/log /sysroot/var/run /sysroot/var/home"
        .to_string();
    for user in sys.users {
        if user.home != "/root" {
            init.push_str(&format!(" /sysroot/var{}", user.home));
        }
    }
    init.push_str(
        "\n/bin/busybox sh -c 'umask 077; /bin/busybox mkdir -p /sysroot/var/root'\n\
         exec /bin/busybox switch_root /sysroot /init\n",
    );
    init
}

/// The ttyS0 session wrapper, run by init AS ROOT (inittab `respawn`). It runs the
/// normal getty -> autologin -> `login -f <user>` flow, then, when that session
/// ENDS — the greeter user types `exit` / Ctrl-D — resets the machine so the VM
/// stops. The auto-login user is UNPRIVILEGED and cannot shut the system down
/// itself; this wrapper runs as root (init's child), so it does it on the user's
/// behalf, making `exit` a clean way out of the VM. `reboot -f` calls `reboot(2)`
/// directly and, under qemu's `-no-reboot`, makes qemu exit 0 — the exact proven
/// exit path the kernel-boot test uses (`linux-x86-64-test`).
///
/// The reboot is gated on `getty` SUCCEEDING (`&&`): getty sets up the tty and execs
/// the login chain, returning the user shell's exit status, so a normal `exit`/Ctrl-D
/// returns 0 -> power off. But if getty/login FAILS to start a session at all (e.g. it
/// cannot open ttyS0), getty returns non-zero, the `&&` short-circuits, and the wrapper
/// exits non-zero so init RESPAWNS it — a visible retry loop — rather than firing
/// `reboot -f` and letting `-no-reboot` mask a broken greeter as a clean exit-0 shutdown
/// (re #541, Codex review).
fn build_tty_session() -> String {
    "#!/bin/sh\n\
     /bin/getty -L -n -l /etc/autologin 115200 ttyS0 vt100 && exec /bin/reboot -f\n"
        .into()
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
    // `/` is a read-only erofs mount (fields: <src> <mnt> <fstype> <opts> …; erofs is
    //     always mounted `ro`, so the options field begins `ro`).
    s.push_str(&format!(
        "if /bin/busybox grep -Eq '^[^ ]+ / erofs ro[, ]' /proc/mounts; then echo {SYSTEM_ROOT_RO_MARKER}; fi\n"
    ));
    // Root runs this check, so a failed /etc write proves the filesystem rejects writes,
    // not merely that file modes deny an unprivileged process. Run the redirection in a
    // child shell: ash exits a non-interactive shell when a special builtin redirection
    // fails, instead of returning control to the parent `if`.
    s.push_str(&format!(
        "if /bin/busybox sh -c ': > /etc/.tdwr' 2>/dev/null; then /bin/busybox rm -f /etc/.tdwr; else echo {SYSTEM_ETC_RO_MARKER}; fi\n"
    ));
    // State and volatile paths must be direct tmpfs mounts. Homes remain stable paths
    // through immutable symlinks, ready for /var to become the persistent @var mount.
    s.push_str(
        "for d in /var /run /tmp; do \
         /bin/busybox grep -Eq \"^[^ ]+ $d tmpfs \" /proc/mounts || ok=0; \
         done\n",
    );
    s.push_str(
        "[ \"$(/bin/busybox readlink /home)\" = var/home ] || ok=0\n\
         [ \"$(/bin/busybox readlink /root)\" = var/root ] || ok=0\n",
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
    s.push_str(&format!(
        "[ \"$ok\" = 1 ] && echo {SYSTEM_STATE_WRITABLE_MARKER}\n"
    ));
    s
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
    s.push_str("cat <<'__TD_MOTD__'\n");
    s.push_str(sys.motd);
    if !sys.motd.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("__TD_MOTD__\n");
    // The greeter has been reached (login chain ran, shell live) — the primary success
    // line the qemu-boot-system oracle keys on.
    s.push_str(&format!("echo {GREETER_MARKER}\n"));
    // Headless self-test: when the oracle appends the autotest token to the kernel
    // cmdline, the greeter (a) RUNS a uutils applet by absolute `/bin` path and, only if it
    // exits 0, prints UUTILS_RUNTIME_MARKER — a live proof that the dynamically-linked
    // coreutils multicall's runtime closure resolves on the erofs root (the greeter line
    // above is a shell builtin `echo`, so it says nothing about uutils health; the MOTD
    // `cat` ignores failure). Then (b) `exit`s so `tty-session`'s `reboot -f` powers the VM
    // off — proving "exit powers off" from a clean qemu exit 0 with no terminal to type
    // into. `/bin/cat` (a uutils applet) on `/etc/os-release` (guaranteed staged) exercises
    // exec → loader → glibc; a broken closure fails the `&&`, drops the marker, and reds the
    // oracle. Interactively (no token) none of this runs — the greeter is a normal shell.
    // `-F`: the token is a FIXED string (`td.autotest=1`), so match it literally — the `.`
    // must not act as a regex wildcard (re #550, Agy review).
    // Then (c) `/bin/sshd selftest`, only if it exits 0, prints SSHD_MARKER — the loopback
    // boot proof. selftest stands up an in-process russh server on an ephemeral 127.0.0.1
    // port and drives a full SSH handshake+auth+channel+exec round-trip against it,
    // exercising the kernel's TCP/IP stack (CONFIG_NET+INET), the russh protocol stack, and
    // sshd's dynamic runtime closure (loader, glibc, libgcc_s, the aws-lc crypto C lib) on
    // the erofs root. It runs as the UNPRIVILEGED greeter user on an ephemeral port — no
    // root, no shipped credential — needing only the loopback `lo` that sysinit brought up.
    // A broken net stack or closure fails the `&&`, drops the marker, and reds the oracle;
    // selftest's own stdout/stderr are suppressed so the marker string appears exactly once.
    // Runs before `exit` so `tty-session`'s `reboot -f` still powers the VM off.
    s.push_str(&format!(
        "if /bin/busybox grep -q -F '{AUTOTEST_CMDLINE_TOKEN}' /proc/cmdline 2>/dev/null; then \
         /bin/cat /etc/os-release >/dev/null 2>&1 && echo {UUTILS_RUNTIME_MARKER}; \
         /bin/sshd selftest >/dev/null 2>&1 && echo {SSHD_MARKER}; \
         exit; fi\n"
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
        ("rootcheck", build_rootcheck(sys), true),
        ("netup", build_netup(), true),
    ]
}

/// The gen_init_cpio spec for the STAGE-1 init-initramfs (`init.cpio`): a self-contained
/// static busybox plus the `/init` pivot script. `{in:...}`/`{root}` tokens are expanded
/// by the engine when it writes this file, so gen_init_cpio reads real paths. Every entry
/// is uid/gid 0. The packed `/dev/console` node carries PID-1 stdio in the window before
/// stage-1 mounts devtmpfs; /sysroot is the erofs mountpoint.
fn build_stage1_spec() -> String {
    let mut s = String::new();
    for d in ["/dev", "/sysroot", "/td", "/td/store"] {
        s.push_str(&format!("dir {d} 0755 0 0\n"));
    }
    // The static busybox at its content-addressed /td/store path; the cpio's /bin/busybox
    // and /bin/sh symlinks (all the stage-1 script needs) point straight at it.
    s.push_str("dir {in:busybox-x86-64} 0755 0 0\n");
    s.push_str("dir {in:busybox-x86-64}/bin 0755 0 0\n");
    s.push_str("file {in:busybox-x86-64}/bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n");
    s.push_str("dir /bin 0755 0 0\n");
    s.push_str("slink /bin/busybox {in:busybox-x86-64}/bin/busybox 0777 0 0\n");
    s.push_str("slink /bin/sh {in:busybox-x86-64}/bin/busybox 0777 0 0\n");
    s.push_str("nod /dev/console 0600 0 0 c 5 1\n");
    s.push_str("file /init {root}/stage1-init 0755 0 0\n");
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
        "/dev", "/proc", "/sys", "/tmp", "/run", "/etc", "/bin", "/mnt", "/var", "/td",
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
    // Stage uutils and sshd plus every transitively referenced store item at its canonical
    // absolute path. Both are dynamically linked, so each pulls its reachable runtime store
    // closure (glibc, libgcc_s, and for sshd the aws-lc crypto C lib) onto the erofs root.
    // The engine admits only direct recipe inputs, so a new runtime dependency fails closed
    // until it is reviewed and declared here.
    steps.push(Step::StageRuntimeClosure {
        roots: vec!["{in:uutils}".into(), "{in:sshd}".into()],
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
    steps.push(Step::Symlink {
        target: "{in:busybox-x86-64}/bin/busybox".into(),
        link: "{root}/real-root/init".into(),
    });
    // /bin/td-netd resolves into the store td-netd package (a single static binary,
    // NOT a multicall — it is its own /bin entry, unlike the busybox/uutils farms).
    steps.push(Step::Symlink {
        target: "{in:td-netd}/bin/td-netd".into(),
        link: "{root}/real-root/bin/td-netd".into(),
    });
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
/// All strings are ASCII (td-builder's config reader is Latin-1). This is a build sanity
/// assert, not a behavioural test — the boot is exercised by `td-recipe-eval run` and the
/// headless `qemu-boot-system` oracle.
fn shape_check() -> String {
    "init='{out}/deployment/initramfs.cpio'; root='{root}/real-root'; disk='{out}/deployment/root.erofs'; manifest='{out}/deployment/manifest'; bb='{in:busybox-x86-64}/bin/busybox'; \
     sz=$(wc -c < \"$init\"); \
     [ \"$sz\" -ge 65536 ] || { echo \"init.cpio: implausibly small ($sz bytes) - the static busybox alone is ~1 MiB\" >&2; exit 1; }; \
     set -- $(od -An -tx1 -N 6 \"$init\"); \
     [ \"$1$2$3$4$5$6\" = 303730373031 ] || { echo 'init.cpio: missing the newc cpio magic 070701' >&2; exit 1; }; \
     list=$(\"$bb\" cpio -t < \"$init\" 2>/dev/null) || { echo 'init.cpio: busybox cpio -t could not parse the archive (truncated/corrupt newc stream)' >&2; exit 1; }; \
     for m in init bin/busybox bin/sh dev/console; do \
         printf '%s\\n' \"$list\" | grep -q -x -F \"$m\" || { echo \"init.cpio: cpio member '$m' missing - the stage-1 initramfs is incomplete\" >&2; exit 1; }; \
     done; \
     printf '%s\\n' \"$list\" | grep -qE '^td/store/[^/]+/bin/busybox$' || { echo 'init.cpio: the busybox binary is not packed under td/store/<hash>/bin' >&2; exit 1; }; \
     [ -f \"$root/init\" ] || [ -L \"$root/init\" ] || { echo 'root tree: /init missing' >&2; exit 1; }; \
     case $(readlink \"$root/init\") in /td/store/*) : ;; *) echo 'root tree: /init is not a symlink into /td/store' >&2; exit 1;; esac; \
     case $(readlink \"$root/bin/sh\") in /td/store/*) : ;; *) echo 'root tree: /bin/sh is not a symlink into /td/store - the store-native /bin farm regressed' >&2; exit 1;; esac; \
     for f in passwd group shadow hostname os-release inittab profile autologin tty-session rootcheck netup; do \
         [ -f \"$root/etc/$f\" ] || { echo \"root tree: /etc/$f missing\" >&2; exit 1; }; \
     done; \
     for l in resolv.conf hosts; do \
         [ \"$(readlink \"$root/etc/$l\")\" = \"/run/$l\" ] || { echo \"root tree: /etc/$l must be a symlink into writable /run (td-netd writes it under the read-only erofs /etc)\" >&2; exit 1; }; \
     done; \
     case $(readlink \"$root/bin/td-netd\") in /td/store/*/bin/td-netd) : ;; *) echo 'root tree: /bin/td-netd is not a symlink into /td/store - the network daemon /bin entry regressed' >&2; exit 1;; esac; \
     tnd=\"{root}/real-root{in:td-netd}/bin/td-netd\"; { [ -f \"$tnd\" ] && [ -x \"$tnd\" ]; } || { echo 'root tree: the td-netd binary is not packed/executable at real-root{in:td-netd}/bin/td-netd - the /bin/td-netd symlink would dangle' >&2; exit 1; }; \
     [ \"$(readlink \"$root/home\")\" = var/home ] || { echo 'root tree: /home must point to var/home' >&2; exit 1; }; \
     [ \"$(readlink \"$root/root\")\" = var/root ] || { echo 'root tree: /root must point to var/root' >&2; exit 1; }; \
     rbb=\"{root}/real-root{in:busybox-x86-64}/bin/busybox\"; { [ -f \"$rbb\" ] && [ -x \"$rbb\" ]; } || { echo 'root tree: the busybox binary is not packed/executable at real-root{in:busybox-x86-64}/bin/busybox - the store-native /bin symlinks would all dangle' >&2; exit 1; }; \
     applets=$(\"$bb\" --list 2>/dev/null) || { echo 'busybox --list failed - cannot verify applet coverage' >&2; exit 1; }; \
     for a in @BUSYBOX_APPLETS@; do \
         printf '%s\\n' \"$applets\" | grep -q -x -F \"$a\" || { echo \"busybox does not implement applet '$a' (config drift) - its packed /bin/$a symlink would be a dead link\" >&2; exit 1; }; \
     done; \
     uu=\"{root}/real-root{in:uutils}/bin/coreutils\"; uutgt=\"{in:uutils}/bin/coreutils\"; \
     { [ -f \"$uu\" ] && [ -x \"$uu\" ]; } || { echo 'root tree: the uutils coreutils multicall is not packed at real-root{in:uutils}/bin/coreutils - the /bin coreutils symlinks would all dangle (#547)' >&2; exit 1; }; \
     for a in @UUTILS_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$uutgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged uutils multicall ($uutgt) - the uutils /bin farm regressed (#547)\" >&2; exit 1; }; \
     done; \
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
        .replace("@UUTILS_APPLETS@", &UUTILS_APPLETS.join(" "))
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

    // 2) Stage the STAGE-1 init-initramfs: write the pivot /init script and the
    //    gen_init_cpio spec, then pack init.cpio with the exported (td-built)
    //    gen_init_cpio — root-owned entries, the /dev/console fallback node, `-t 1` for a
    //    reproducible mtime.
    steps.push(Step::WriteFile {
        path: "{root}/stage1-init".into(),
        content: build_stage1_init(&SYSTEM),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/init.spec".into(),
        content: build_stage1_spec(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/init.spec' > '{root}/initramfs.cpio'",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 3) Materialise the first-class deployment bundle. PackErofs is executed
    //    by the derivation engine itself, never exposed to recipe argv/PATH.
    steps.push(Step::MkDir {
        path: "{out}/deployment".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{in:linux-x86-64}/bzImage".into(),
            "{root}/initramfs.cpio".into(),
        ],
        dest: "{out}/deployment".into(),
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
            (
                "root.erofs".into(),
                "{out}/deployment/root.erofs".into(),
            ),
        ],
    });

    // 4) Require the complete contract and shape-check every payload.
    steps.push(Step::Require {
        paths: vec![
            "{out}/deployment/bzImage".into(),
            "{out}/deployment/initramfs.cpio".into(),
            "{out}/deployment/root.erofs".into(),
            "{out}/deployment/manifest".into(),
        ],
        exec: false,
    });
    steps.push(Step::run("{out}", &[SH, "-c", &shape_check()]).env("PATH", &mesboot0_path()));

    Recipe::mesboot("system-x86-64", "0.1")
        // busybox: the static boot/greeter userland + the `cpio -t`/applet shape check.
        // linux-x86-64: the EXPORTED gen_init_cpio packer (verified STATICALLY linked).
        // uutils: the dynamically-linked `coreutils` multicall packed as the /bin file/text
        //   userland (#547).
        // sshd: the source-built russh SSH daemon, packed at /bin/sshd; its runtime closure
        //   (glibc, libgcc_s, aws-lc crypto C lib) is reached by StageRuntimeClosure.
        // glibc-x86-64: uutils' and sshd's declared runtime input. StageRuntimeClosure reaches
        //   it from their embedded store references and copies the whole content-addressed item.
        // td-netd: the static network bring-up daemon (empty runtime closure, CopyTree'd).
        .native_inputs(&["busybox-x86-64", "linux-x86-64", "uutils", "glibc-x86-64", "sshd", "td-netd"])
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
        assert_eq!(closures.len(), 1, "the image needs one runtime-closure step");
        let (roots, dest) = closures.first().expect("one runtime closure");
        assert_eq!(
            roots.as_slice(),
            ["{in:uutils}", "{in:sshd}"],
            "the dynamically linked uutils multicall and sshd daemon are the explicit runtime roots"
        );
        assert_eq!(dest.as_str(), "{root}/real-root");
        assert!(
            steps.iter().all(|step| !matches!(
                step,
                Step::CopyTree { from, .. }
                    if from.contains("uutils")
                        || from.contains("glibc-x86-64")
            )),
            "runtime store items must not bypass StageRuntimeClosure"
        );

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
    }

    /// The tailorable `SYSTEM` const is hand-edited to shape the distro; guard the
    /// invariants a bad edit would otherwise surface only as a silent boot failure —
    /// a getty respawn-looping on `login -f <missing-user>`, or a login shell that was
    /// never packed into /bin.
    #[test]
    fn system_def_is_self_consistent() {
        assert!(
            SYSTEM.users.iter().any(|u| u.name == SYSTEM.autologin),
            "autologin user '{}' is not defined in SYSTEM.users",
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
    /// all boot-critical and MUST stay busybox (static, no runtime closure): belt-and-
    /// braces against a farm edit that drops one or reroutes it to dynamically-linked
    /// uutils (the shape check catches it at build time, this catches it at test time).
    #[test]
    fn greeter_and_pivot_applets_are_present() {
        for a in ["sh", "getty", "login", "init", "mount", "umount", "reboot", "switch_root"] {
            assert!(
                BUSYBOX_APPLETS.contains(&a),
                "boot-critical applet '{a}' missing from BUSYBOX_APPLETS"
            );
        }
    }

    /// The two /bin farms must be DISJOINT — a name in both would pack two conflicting
    /// symlinks for one applet (last-writer-wins, non-deterministic) and blur the
    /// static-vs-dynamic boot-safety boundary. Also pin the boot-critical names that MUST
    /// stay busybox: `hostname` (inittab runs `hostname -F`, a flag uutils lacks) and
    /// `mount`/`umount` (the stage-1 pivot runs before uutils' glibc closure is reachable).
    #[test]
    fn applet_farms_are_disjoint_and_boot_names_stay_busybox() {
        for a in UUTILS_APPLETS {
            assert!(
                !BUSYBOX_APPLETS.contains(a),
                "applet '{a}' is in BOTH farms - a name belongs to exactly one /bin farm"
            );
        }
        for a in ["hostname", "mount", "umount", "sh", "init"] {
            assert!(
                BUSYBOX_APPLETS.contains(&a),
                "boot-critical applet '{a}' must stay busybox, not route to uutils"
            );
            assert!(
                !UUTILS_APPLETS.contains(&a),
                "boot-critical applet '{a}' must NOT be served by dynamically-linked uutils"
            );
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
    /// sysinit (the read-only-root self-check), and `tty-session` must exec `reboot -f`
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
        let session = build_tty_session();
        // getty must gate the reboot (`&&`), so a FAILED session respawns rather than
        // firing reboot -f and masking a broken greeter as a clean exit-0 shutdown.
        assert!(
            session.contains("/bin/getty ")
                && session.contains("-l /etc/autologin ")
                && session.contains("&& exec /bin/reboot -f"),
            "tty-session must run getty (autologin at /etc/autologin) then, only on success, \
             `reboot -f` so the greeter's exit stops the VM but a failure retries"
        );
    }

    /// Stage 1 must mount the EROFS root read-only, mount writable state directly,
    /// leave `/etc` untouched, and `exec switch_root` so the pivot inherits PID 1.
    #[test]
    fn stage1_init_mounts_ro_and_pivots() {
        let init = build_stage1_init(&SYSTEM);
        // Fail-safe: `set -e` aborts on the first mount failure rather than pressing on
        // into a partial switch_root (re #550, Codex review).
        assert!(
            init.contains("\nset -e\n") || init.contains("#!/bin/sh\nset -e"),
            "stage-1 init must `set -e` so a failed mount aborts loudly, not into a partial pivot"
        );
        assert!(
            init.contains("mount -t erofs -o ro /dev/vda /sysroot"),
            "stage-1 init must mount /dev/vda as read-only erofs at /sysroot"
        );
        assert!(
            init.contains("tmpfs /sysroot/var")
                && init.contains("tmpfs /sysroot/run")
                && init.contains("tmpfs /sysroot/tmp"),
            "stage-1 init must mount the writable state and volatile directories directly"
        );
        assert!(
            !init.contains("-t overlay") && !init.contains(" /sysroot/etc"),
            "stage-1 init must leave deployment-owned /etc on the immutable erofs root"
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
            init.trim_end().ends_with("exec /bin/busybox switch_root /sysroot /init"),
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
        assert!(rootcheck.contains(SYSTEM_ROOT_RO_MARKER), "rootcheck must emit the ro-root marker");
        assert!(rootcheck.contains(SYSTEM_ETC_RO_MARKER), "rootcheck must emit the immutable-/etc marker");
        assert!(
            rootcheck.contains(SYSTEM_STATE_WRITABLE_MARKER),
            "rootcheck must emit the writable-state marker"
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
        assert!(profile.contains(GREETER_MARKER), "profile must emit the greeter marker");
        assert!(
            profile.contains(AUTOTEST_CMDLINE_TOKEN) && profile.contains("exit"),
            "profile must exit on the autotest cmdline token so the headless boot powers off"
        );
        // The headless self-test must PROVE uutils runs: a uutils applet invoked by absolute
        // /bin path, gated with `&&` on the marker echo, so a broken runtime closure drops the
        // marker and reds the oracle (#547, review finding #2).
        assert!(
            profile.contains(UUTILS_RUNTIME_MARKER),
            "profile must emit the uutils runtime marker"
        );
        assert!(
            profile.contains("/bin/cat /etc/os-release") && profile.contains(&format!("&& echo {UUTILS_RUNTIME_MARKER}")),
            "the uutils runtime marker must be gated on a successful absolute-path uutils invocation"
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
