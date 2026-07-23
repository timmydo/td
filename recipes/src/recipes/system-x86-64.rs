use crate::ladder::{
    mesboot0_inputs, mesboot0_path, AUTOTEST_CMDLINE_TOKEN, GREETER_MARKER, SH,
    SYSTEM_ROOT_RO_MARKER, SYSTEM_WRITABLE_MARKER, UUTILS_RUNTIME_MARKER,
};
use crate::types::{Recipe, Step};

// system-x86-64 (re #541, #550): a MINIMAL, TAILORABLE Rust-first Linux distro image,
// booted TWO-STAGE onto a disk-backed READ-ONLY erofs `/td/store` root.
//
// This is the "system definition" recipe. It composes artifacts that already exist in
// the ladder — the source-built `linux-x86-64` kernel and the td-built STATIC busybox —
// into a two-stage boot:
//
//   Stage 1 — a tiny init-initramfs (`{out}/init.cpio`): static busybox + a `/init`
//   SCRIPT that mounts the erofs root read-only over virtio-blk at `/sysroot`, overlays
//   tmpfs for the writable dirs (`/etc /var /home`) plus fresh tmpfs `/run` `/tmp`, then
//   `switch_root`s into the real root and execs its init.
//
//   Stage 2 — the REAL ROOT TREE (`{out}/root/`): the store-native rootfs (busybox at
//   its /td/store path, a /bin symlink farm, generated /etc) staged as a real directory.
//   The control-plane erofs WRITER (`td-builder mkfs-erofs`, #548) packs THIS tree into
//   the read-only erofs image the boot tools attach as `/dev/vda`. Recipes cannot invoke
//   the control-plane writer (it never sits on a recipe PATH/argv), so the recipe stages
//   the TREE and the host-side boot tools (`checks/run.rs`, `checks/qemu_boot.rs`) build
//   the image from it — the same split the #549 `qemu-boot-erofs` probe already uses.
//
// The busybox init auto-logs-in a test user to a shell with a welcome banner. EDIT the
// `SYSTEM` const below to tailor the distro (hostname, users, the auto-login user, the
// login shell, the applet set). A producer-rung shape check on both the packed init.cpio
// and the staged root tree is the automated build guard; the interactive
// `td-recipe-eval run` boots the two-stage image under host qemu so you can use it, and
// the headless `td-recipe-eval qemu-boot-system` asserts it boots to the greeter on a
// read-only erofs root and powers off cleanly on `exit`.
//
// Userland strategy (v0): busybox provides init/getty/login/ash/coreutils/switch_root —
// all present in its `defconfig`, all STATIC, so both the initramfs AND the erofs root
// are self-contained (no glibc closure, no host bytes). This is an explicitly
// TRANSITIONAL start on the AGENTS.md Rust-first path: swapping busybox coreutils for the
// (dynamically-linked, Rust) uutils is its own atomic migration PR (#547) — it needs the
// full Rust bootstrap plus a packed glibc runtime closure on the erofs root, so it lands
// separately, not inline here.
//
// Layout: the image is STORE-NATIVE. The busybox binary is packed at its
// content-addressed /td/store/<hash>-busybox-x86-64/bin path, and /bin is a PURE symlink
// farm whose every entry (and /init) points straight into that store path. There is no
// /usr and no /sbin. The only non-store files are generated system config under /etc
// (passwd/group/shadow/inittab/os-release/profile, plus the login-glue scripts autologin,
// tty-session, and the boot self-check rootcheck), referenced by absolute path.

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

// ── EDIT THIS to tailor the distro ─────────────────────────────────────────────
const SYSTEM: SystemDef = SystemDef {
    hostname: "td",
    os_name: "td",
    os_version: "0.1",
    // NOTE: keep motd (and every emitted /etc string) ASCII — td-builder's config
    // reader is Latin-1 (builder/src/json.rs), so a multi-byte UTF-8 char here (e.g.
    // an em-dash) is silently corrupted in the written file. Use '-', not the glyph.
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
    "less", "more", "grep", "sed", "awk", "cttyhack", "su", "which",
];

/// The core file/text userland, served by the uutils `coreutils` multicall (#547). Every
/// name must be a coreutils utility the built binary implements. The recipe sandbox cannot
/// exec the dynamically-linked binary to run `coreutils --list` at build time (its interp
/// resolves an absolute `/td/store` path that only exists on the assembled root, not in the
/// build tree), so `shape_check` instead statically proves the multicall AND its full
/// runtime closure are staged on the root: a missing applet surfaces on the boot oracle, a
/// missing library reds the build. uutils is dynamically linked, so — unlike static
/// busybox — it drags a runtime closure (glibc, libgcc_s) that `real_root_steps` packs onto
/// the erofs root.
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
    // dir is a VFS overlay, no write to the erofs — then runs the boot self-check and
    // the auto-login getty. It does NOT mount /tmp or /run: stage-1 already mounted those
    // as tmpfs (they survived switch_root's mount-move), which is also what backs the
    // writable overlays. /proc must precede /etc/rootcheck (which reads /proc/mounts).
    "::sysinit:/bin/mount -t devtmpfs devtmpfs /dev\n\
     ::sysinit:/bin/mount -t proc proc /proc\n\
     ::sysinit:/bin/mount -t sysfs sysfs /sys\n\
     ::sysinit:/bin/hostname -F /etc/hostname\n\
     ::sysinit:/etc/rootcheck\n\
     ttyS0::respawn:/etc/tty-session\n\
     ::ctrlaltdel:/bin/reboot\n\
     ::shutdown:/bin/umount -a -r\n"
        .into()
}

/// The stage-1 init-initramfs `/init` (re #550): the FIRST userspace, run by the kernel
/// as PID 1 from the `init.cpio` initramfs. It mounts the read-only erofs store root over
/// virtio-blk, sets up the writable tmpfs overlays, then `switch_root`s into the real
/// root. Static busybox with NO /bin PATH yet, so every applet is reached explicitly as
/// `/bin/busybox <applet>` (only `/bin/sh` and `/bin/busybox` are symlinked in the cpio);
/// `echo`-free by design. The final line MUST be `exec` so switch_root inherits PID 1.
fn build_stage1_init() -> String {
    // Overlay backing lives on a tmpfs mounted at /sysroot/run — INSIDE the future root —
    // so switch_root's mount-move carries the upper/work dirs cleanly (no orphaned mount
    // dangling off the discarded initramfs). /run and /tmp become fresh tmpfs; /etc, /var
    // and /home become overlays (lower = the read-only erofs dir, upper = tmpfs) so the
    // packed base content (passwd, inittab, the user home) stays visible AND writable.
    // erofs is inherently read-only; `-o ro` is belt-and-suspenders. The /dev/vda probe
    // loop tolerates an async virtio-blk attach.
    //
    // `set -e` makes the FIRST failing setup command abort the script — its error already
    // on the console (no 2>/dev/null) — rather than press on into a doomed or PARTIAL
    // `switch_root`: a failed erofs/overlay mount then panics loudly (init exits) instead
    // of booting a half-read-only system where some dirs silently aren't writable (re #550,
    // Codex review). The `while` probe condition is exempt from `set -e`, so a missing
    // /dev/vda still falls through to the erofs mount, which fails and aborts cleanly.
    //
    // /run is mounted 0755 (not tmpfs's default 1777) — it holds the overlay backing and
    // matches a standard /run; /tmp keeps the default sticky 1777.
    "#!/bin/sh\n\
     set -e\n\
     /bin/busybox mount -t devtmpfs dev /dev\n\
     n=0\n\
     while /bin/busybox test \"$n\" -lt 5 && ! /bin/busybox test -b /dev/vda; do /bin/busybox sleep 1; n=$((n+1)); done\n\
     /bin/busybox mount -t erofs -o ro /dev/vda /sysroot\n\
     /bin/busybox mount -t tmpfs -o mode=0755 tmpfs /sysroot/run\n\
     /bin/busybox mount -t tmpfs tmpfs /sysroot/tmp\n\
     for d in etc var home; do \
     /bin/busybox mkdir -p /sysroot/run/.rw/$d /sysroot/run/.work/$d; \
     /bin/busybox mount -t overlay overlay -o lowerdir=/sysroot/$d,upperdir=/sysroot/run/.rw/$d,workdir=/sysroot/run/.work/$d /sysroot/$d; \
     done\n\
     exec /bin/busybox switch_root /sysroot /init\n"
        .into()
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
/// (re #550). It (1) gives each non-root user an owned, writable home on the /home
/// overlay — the erofs base is root-owned (the writer stamps uid/gid 0), so a `chown`
/// copies the home up in the overlay so the auto-login user can write to `~` — and
/// (2) prints the two diagnostic markers the headless `qemu-boot-system` oracle asserts
/// on: the root really is a read-only erofs mount, and the writable dirs are tmpfs-backed
/// and actually accept writes. All applets are called as `/bin/busybox <applet>` (init
/// runs sysinit with no PATH); the write probes use a plain `> file` redirection so a
/// read-only target fails the `if` without needing an external tool.
fn build_rootcheck(sys: &SystemDef) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    // (1) Home ownership on the writable /home overlay (skip root, which owns /).
    for u in sys.users {
        if u.uid != 0 {
            s.push_str(&format!(
                "/bin/busybox chown {}:{} {} 2>/dev/null\n",
                u.uid, u.gid, u.home
            ));
        }
    }
    // (2) `/` is a read-only erofs mount (fields: <src> <mnt> <fstype> <opts> …; erofs is
    //     always mounted `ro`, so the options field begins `ro`).
    s.push_str(&format!(
        "if /bin/busybox grep -Eq '^[^ ]+ / erofs ro[, ]' /proc/mounts; then echo {SYSTEM_ROOT_RO_MARKER}; fi\n"
    ));
    // (3) The writable dirs are tmpfs-backed AND accept a write. /run must be a tmpfs
    //     mount, and each overlaid/tmpfs dir must take a probe file (created, then removed).
    s.push_str("ok=1\n");
    s.push_str("/bin/busybox grep -Eq '^[^ ]+ /run tmpfs ' /proc/mounts || ok=0\n");
    s.push_str(
        "for d in /etc /var /run /tmp /home; do \
         if : 2>/dev/null > \"$d/.tdwr\"; then /bin/busybox rm -f \"$d/.tdwr\"; else ok=0; fi; \
         done\n",
    );
    s.push_str(&format!(
        "[ \"$ok\" = 1 ] && echo {SYSTEM_WRITABLE_MARKER}\n"
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
    s.push_str(&format!(
        "if /bin/busybox grep -q -F '{AUTOTEST_CMDLINE_TOKEN}' /proc/cmdline 2>/dev/null; then \
         /bin/cat /etc/os-release >/dev/null 2>&1 && echo {UUTILS_RUNTIME_MARKER}; exit; fi\n"
    ));
    s
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
/// (written under `{out}/root/etc`) and the shape check (which asserts they landed).
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

/// Stage the REAL ROOT tree under `{out}/root` (packed to a read-only erofs by the
/// control-plane writer in the boot tools). Uses typed steps (no shell): the busybox
/// package is copied to its /td/store path, /bin is a symlink farm into it, /init is a
/// symlink to busybox, /etc holds the generated config, and the pseudo-fs + writable
/// mountpoint dirs are created empty (stage-1/init mount over them). The erofs writer
/// stamps uid/gid 0, so the whole tree is root-owned; per-user home ownership is fixed at
/// boot by /etc/rootcheck on the writable /home overlay.
fn real_root_steps(sys: &SystemDef) -> Vec<Step> {
    let mut steps = Vec::new();
    // Empty dirs: the pseudo-fs mountpoints (/dev /proc /sys), the writable-overlay bases
    // (/etc /var /home + per-user homes) and fresh-tmpfs mountpoints (/tmp /run), plus
    // /root, /bin, /mnt and the /td/store spine. /var/{log,run} exist so login's
    // utmp/wtmp writes land on the overlay rather than ENOENT.
    let mut dirs: Vec<String> = [
        "/dev", "/proc", "/sys", "/tmp", "/run", "/root", "/home", "/etc", "/bin", "/mnt",
        "/var", "/var/log", "/var/run", "/td", "/td/store",
    ]
    .iter()
    .map(|d| (*d).to_string())
    .collect();
    for u in sys.users {
        if u.home != "/root" {
            dirs.push(u.home.to_string());
        }
    }
    for d in &dirs {
        steps.push(Step::MkDir {
            path: format!("{{out}}/root{d}"),
        });
    }
    // The busybox store package copied to its content-addressed /td/store path inside the
    // root, so /bin's symlinks (and /init) resolve on the mounted erofs.
    steps.push(Step::CopyTree {
        from: "{in:busybox-x86-64}".into(),
        dest: "{out}/root{in:busybox-x86-64}".into(),
    });
    // uutils' `coreutils` multicall copied to its content-addressed /td/store path (a
    // direct, hash-prefixed store dir like busybox), so the /bin coreutils symlinks resolve
    // on the mounted erofs (#547).
    steps.push(Step::CopyTree {
        from: "{in:uutils}".into(),
        dest: "{out}/root{in:uutils}".into(),
    });
    // uutils is dynamically linked, so its runtime closure must ALSO live on the read-only
    // root at the absolute /td/store path its interpreter + RUNPATH resolve. Both build paths
    // (`td shell` via TD_SHELL_NATIVE_INTERP and `build-plan --auto` via the builder's derived
    // TD_RUST_STORE_INTERP) bake the HASH-PREFIXED, nested path
    // /td/store/<hash>-glibc-x86-64/stage/td/store/glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2 —
    // NOT a fixed /td/store/glibc-2.41-x86_64 — so stage glibc's runtime subtree UNDER its own
    // content-addressed output dir (like busybox/uutils), matching the exact path the ELF names.
    // `shape_check` fails closed if uutils references any /td/store package this does not stage
    // (e.g. a separate libgcc_s dir), naming the exact miss.
    steps.push(Step::CopyTree {
        from: "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64".into(),
        dest: "{out}/root{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64".into(),
    });
    // /bin symlink farm: /bin/busybox, every applet, and /init resolve DIRECTLY into the
    // store busybox (busybox dispatches on argv[0]'s basename).
    steps.push(Step::Symlink {
        target: "{in:busybox-x86-64}/bin/busybox".into(),
        link: "{out}/root/bin/busybox".into(),
    });
    for app in BUSYBOX_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:busybox-x86-64}/bin/busybox".into(),
            link: format!("{{out}}/root/bin/{app}"),
        });
    }
    // The core file/text userland resolves into the uutils `coreutils` multicall instead of
    // busybox (#547). uutils dispatches on argv[0]'s basename exactly like busybox, so a
    // /bin/<applet> -> coreutils symlink runs that applet.
    for app in UUTILS_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:uutils}/bin/coreutils".into(),
            link: format!("{{out}}/root/bin/{app}"),
        });
    }
    steps.push(Step::Symlink {
        target: "{in:busybox-x86-64}/bin/busybox".into(),
        link: "{out}/root/init".into(),
    });
    // Generated /etc.
    for (name, content, exec) in etc_files(sys) {
        steps.push(Step::WriteFile {
            path: format!("{{out}}/root/etc/{name}"),
            content,
            exec,
        });
    }
    steps
}

/// A producer-rung shape check on BOTH the stage-1 `init.cpio` and the staged real-root
/// tree. For the cpio: real newc magic, a size floor (static busybox alone is ~1 MiB), a
/// `busybox cpio -t` parse, the members that make it bootable (incl. the /init pivot
/// script), and the busybox binary under /td/store. For the root tree: /init and /bin/sh
/// are symlinks into /td/store, the key /etc files exist, and the busybox binary is
/// packed under /td/store. AND that busybox actually implements EVERY BUSYBOX_APPLETS
/// entry (incl. `switch_root`) — a config drift or tailoring typo that dropped/misnamed
/// an applet would leave a dead /bin symlink the member checks alone can't catch. For the
/// uutils farm: the `coreutils` multicall is staged, every UUTILS_APPLETS /bin symlink
/// exists, and — since uutils is dynamically linked — its whole /td/store closure resolves
/// on the root (fail-closed). All strings are ASCII (td-builder's config reader is
/// Latin-1). This is a build sanity assert, not a behavioural test — the boot is exercised
/// by `td-recipe-eval run` and the headless `qemu-boot-system` oracle.
fn shape_check() -> String {
    "init='{out}/init.cpio'; root='{out}/root'; bb='{in:busybox-x86-64}/bin/busybox'; \
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
     for f in passwd group shadow hostname os-release inittab profile autologin tty-session rootcheck; do \
         [ -f \"$root/etc/$f\" ] || { echo \"root tree: /etc/$f missing\" >&2; exit 1; }; \
     done; \
     rbb=\"{out}/root{in:busybox-x86-64}/bin/busybox\"; { [ -f \"$rbb\" ] && [ -x \"$rbb\" ]; } || { echo 'root tree: the busybox binary is not packed/executable at root{in:busybox-x86-64}/bin/busybox - the store-native /bin symlinks would all dangle' >&2; exit 1; }; \
     applets=$(\"$bb\" --list 2>/dev/null) || { echo 'busybox --list failed - cannot verify applet coverage' >&2; exit 1; }; \
     for a in @BUSYBOX_APPLETS@; do \
         printf '%s\\n' \"$applets\" | grep -q -x -F \"$a\" || { echo \"busybox does not implement applet '$a' (config drift) - its packed /bin/$a symlink would be a dead link\" >&2; exit 1; }; \
     done; \
     uu=\"{out}/root{in:uutils}/bin/coreutils\"; uutgt=\"{in:uutils}/bin/coreutils\"; \
     { [ -f \"$uu\" ] && [ -x \"$uu\" ]; } || { echo 'root tree: the uutils coreutils multicall is not packed at root{in:uutils}/bin/coreutils - the /bin coreutils symlinks would all dangle (#547)' >&2; exit 1; }; \
     for a in @UUTILS_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$uutgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged uutils multicall ($uutgt) - the uutils /bin farm regressed (#547)\" >&2; exit 1; }; \
     done; \
     pkgs=$(tr -c 'A-Za-z0-9._+/-' '\\n' < \"$uu\" | sed -n 's#^[^/]*/td/store/\\([A-Za-z0-9._+-][A-Za-z0-9._+-]*\\).*#/td/store/\\1#p' | sort -u); \
     [ -n \"$pkgs\" ] || { echo 'root tree: extracted NO /td/store path from the uutils binary - the static closure scan (tr | sed) is broken or the binary is not dynamically linked; refusing to pass vacuously (#547)' >&2; exit 1; }; \
     miss=0; \
     for p in $pkgs; do \
         [ -d \"$root$p\" ] || { echo \"root tree: uutils references store package '$p' (its interp/RUNPATH) which is NOT staged on the erofs root - the dynamic closure is incomplete (#547); add the package that provides it to native_inputs and CopyTree its store subtree onto the root\" >&2; miss=1; }; \
     done; \
     [ \"$miss\" = 0 ] || exit 1"
        // The busybox check names the concrete `{in:busybox-x86-64}` path, not a
        // `td/store/*/bin/busybox` glob: bash-mesboot 2.05b (this step's shell) can't expand
        // a wildcard in a non-terminal path component.
        //
        // Validate EVERY packed applet, not just the greeter-critical few (re #541, Codex
        // review). Names are all shell-safe identifiers, so a space-joined `for` list is
        // safe unquoted. uutils is dynamically linked and CANNOT be exec'd in the build
        // sandbox (its interp is an absolute /td/store path present only on the assembled
        // root, not in the build tree), so verify its closure STATICALLY: the multicall is
        // staged+executable, every uutils /bin link's TEXT points at that multicall (compare
        // link text, never resolve — the target is an absolute /td/store path that dangles
        // on the build host), and every TOP-LEVEL /td/store package dir the binary references
        // (interp + RUNPATH) is a staged directory. We extract the FIRST `/td/store/<pkg>` per
        // reference string, not every occurrence, and NOT via `grep -o`, for two reasons:
        //   (1) this step runs on the mesboot0 PATH, whose grep is GNU grep 2.4 — it predates
        //       `-o` (--only-matching), which would exit 2 and (via the emptiness guard above)
        //       leave the recipe permanently unbuildable; and
        //   (2) `grep -o`'s all-occurrences semantics is WRONG here. glibc is staged under its
        //       own content-addressed output, so uutils' interp/RUNPATH nest /td/store TWICE:
        //       `/td/store/<hash>-glibc-x86-64/stage/td/store/glibc-2.41-x86_64/lib/ld-…`. The
        //       TOP-LEVEL package is the OUTER `<hash>-glibc-x86-64` (a real root dir); the
        //       inner `glibc-2.41-x86_64` is a SUBPATH inside it (comes along via CopyTree),
        //       never a top-level dir — extracting it would fail `-d "$root$p"` on every
        //       correctly-linked image. So `tr -c` bursts the binary into tokens on any byte
        //       outside the store-path charset (NUL, the `:` in a RUNPATH, …), and one `sed`
        //       takes the FIRST `/td/store/<pkg>` per token: `^[^/]*` skips a non-slash prefix
        //       (an interp abutting prior bytes) to reach the first `/td/store/`, the class
        //       excludes `/` so it stops at the package root, and any later `/td/store/` in the
        //       same token is that package's own subpath and is ignored. `[class]` requires >=1
        //       char so a bare `/td/store/` cannot match vacuously; an empty scan is itself a
        //       failure (a dynamic binary must name its interp). Assumes each store path
        //       appears as its own absolute string (the toolchain never embeds one behind a
        //       slash-bearing prefix) — verified against real ELFs.
        // LIMIT: this catches PATH-referenced closure (interp + RUNPATH dirs), not a
        // soname-only DT_NEEDED with no RUNPATH entry (e.g. libgcc_s.so.1 if the linker baked
        // no rpath for it). That residual is caught by the qemu-boot-system oracle, which now
        // EXECS a uutils applet by absolute path and asserts UUTILS_RUNTIME_MARKER — a broken
        // runtime closure reds it (a static-only claim would not). That oracle is operator-run
        // (`td-recipe-eval qemu-boot-system` needs host qemu, absent in the gated/daily
        // sandbox), so it is a pre-landing backstop, not a per-change gate; this static scan is
        // the per-change guard and fails closed naming any unstaged package (#547).
        .replace("@BUSYBOX_APPLETS@", &BUSYBOX_APPLETS.join(" "))
        .replace("@UUTILS_APPLETS@", &UUTILS_APPLETS.join(" "))
}

pub fn recipe() -> Recipe {
    let mut steps = Vec::new();
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });

    // 1) Stage the real-root TREE at {out}/root (packed to a read-only erofs by the boot
    //    tools' control-plane writer). shadow gets a follow-up chmod 0600 (WriteFile can
    //    only set 0644/0755, and a world-readable shadow — even with empty/locked
    //    passwords — should not regress from the old gen_init_cpio 0600).
    steps.extend(real_root_steps(&SYSTEM));
    steps.push(
        Step::run(
            "{out}",
            &[SH, "-c", "chmod 0600 '{out}/root/etc/shadow'"],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 2) Stage the STAGE-1 init-initramfs: write the pivot /init script and the
    //    gen_init_cpio spec, then pack init.cpio with the exported (td-built)
    //    gen_init_cpio — root-owned entries, the /dev/console fallback node, `-t 1` for a
    //    reproducible mtime.
    steps.push(Step::WriteFile {
        path: "{root}/stage1-init".into(),
        content: build_stage1_init(),
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
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/init.spec' > '{out}/init.cpio'",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 3) Require the artifacts and shape-check them.
    steps.push(Step::Require {
        paths: vec!["{out}/init.cpio".into(), "{out}/root/init".into()],
        exec: false,
    });
    steps.push(Step::run("{out}", &[SH, "-c", &shape_check()]).env("PATH", &mesboot0_path()));

    Recipe::mesboot("system-x86-64", "0.1")
        // busybox: the static boot/greeter userland + the `cpio -t`/applet shape check.
        // linux-x86-64: the EXPORTED gen_init_cpio packer (verified STATICALLY linked).
        // uutils: the dynamically-linked `coreutils` multicall packed as the /bin file/text
        //   userland (#547).
        // glibc-x86-64: uutils' runtime closure (ld-linux + libc.so.6). Staged NESTED under
        //   its own content-addressed output — /td/store/<hash>-glibc-x86-64/stage/td/store/
        //   glibc-2.41-x86_64/… — which is the exact absolute interp/RUNPATH uutils bakes, so
        //   it resolves on the erofs root. shape_check verifies the OUTER <hash>-glibc-x86-64
        //   dir is staged; the inner glibc-2.41-x86_64 is a subpath inside it (via CopyTree).
        .native_inputs(&["busybox-x86-64", "linux-x86-64", "uutils", "glibc-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The stage-1 init is the load-bearing new piece: it must mount the erofs root
    /// read-only, set up the tmpfs-backed writable overlays, and `exec switch_root` (so
    /// the pivot inherits PID 1). Guard those against a careless edit.
    #[test]
    fn stage1_init_mounts_ro_and_pivots() {
        let init = build_stage1_init();
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
        // The /sysroot/run tmpfs backs the overlays; it carries `-o mode=0755` now, so match
        // on the source+mountpoint (`tmpfs /sysroot/run`) not the exact flag string.
        assert!(
            init.contains("-t overlay overlay") && init.contains("tmpfs /sysroot/run"),
            "stage-1 init must set up the tmpfs-backed writable overlays"
        );
        assert!(
            init.trim_end().ends_with("exec /bin/busybox switch_root /sysroot /init"),
            "stage-1 init must END by exec-ing switch_root so the pivot inherits PID 1"
        );
    }

    /// The read-only-root self-check must emit both diagnostic markers the headless
    /// oracle asserts on, and the greeter must emit its marker and honour the autotest
    /// exit — the seam between the recipe and `qemu-boot-system`.
    #[test]
    fn boot_markers_are_wired() {
        let rootcheck = build_rootcheck(&SYSTEM);
        assert!(rootcheck.contains(SYSTEM_ROOT_RO_MARKER), "rootcheck must emit the ro-root marker");
        assert!(rootcheck.contains(SYSTEM_WRITABLE_MARKER), "rootcheck must emit the writable marker");
        // Home ownership is fixed for every non-root user on the /home overlay.
        for u in SYSTEM.users {
            if u.uid != 0 {
                assert!(
                    rootcheck.contains(&format!("chown {}:{} {}", u.uid, u.gid, u.home)),
                    "rootcheck must chown {}'s home on the overlay",
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
    }
}
