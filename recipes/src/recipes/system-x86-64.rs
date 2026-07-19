use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{Recipe, Step};

// system-x86-64 (re #541): a MINIMAL, TAILORABLE Rust-first Linux distro image.
//
// This is the "system definition" recipe. It composes artifacts that already
// exist in the ladder — the source-built `linux-x86-64` kernel and the td-built
// STATIC busybox — into a bootable **initramfs** (`{out}/rootfs.cpio`) whose
// busybox `init` auto-logs-in a test user to a shell with a welcome banner. It is
// meant to be EDITED (the `SYSTEM` const below) to tailor the distro: hostname,
// users, the auto-login user, the login shell, and the applet set. Unlike the
// kernel rung, it carries no heavy test suite — a producer-rung shape check on the
// packed cpio is the only automated guard; the interactive `td-recipe-eval run
// linux-x86-64` command boots it under host qemu so you can use it.
//
// Userland strategy (v0): busybox provides init/getty/login/ash/coreutils — all
// present in its `defconfig`, all STATIC, so the initramfs is self-contained (no
// glibc closure, no host bytes). This is an explicitly TRANSITIONAL start on the
// AGENTS.md Rust-first path: each piece (coreutils -> uutils #539, the shell, the
// init) can later swap to a Rust equivalent behind this same recipe.
//
// Packing: the initramfs is built by the kernel's own `gen_init_cpio`, EXPORTED
// from the `linux-x86-64` output. gen_init_cpio writes the newc cpio from a text
// spec WITHOUT mknod privilege (the `nod /dev/console` fallback node is written
// straight into the archive) and stamps every entry uid/gid 0 regardless of the
// build user — so the rootfs is ROOT-owned, which busybox `cpio -o` could not
// achieve in the unprivileged sandbox. `-t 1` pins a fixed mtime so the cpio is
// reproducible. /dev is auto-populated at boot by the kernel's DEVTMPFS_MOUNT, so
// the spec lists /dev only as a mountpoint (plus the console fallback node).

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
    /// Supplementary groups (e.g. "wheel"); the primary group is `name`.
    groups: &'static [&'static str],
    passwordless: bool,
}

/// The distro definition. EDIT THIS to tailor the system, then rebuild and
/// `td-recipe-eval run linux-x86-64`.
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
    motd: "\n  Welcome to td — a source-built, Rust-first Linux.\n  \
           Minimal busybox userland, booted from an initramfs under qemu.\n  \
           Edit recipes/src/recipes/system-x86-64.rs (the SYSTEM const) to tailor it.\n\n",
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

/// Busybox applets symlinked into `/bin` (busybox dispatches on argv[0]). This is a
/// curated, tailorable subset — busybox `defconfig` ships far more, each reachable
/// as `busybox <applet>`; add a name here to give it a bare command in `PATH`.
///
/// `find`/`xargs` are intentionally NOT bare symlinks: the ladder's findutils
/// dead-axis lock (`no_bootstrap_step_invokes_host_find_or_xargs`) forbids those
/// tokens in any step text, and it can't tell a cpio member NAME from a host
/// invocation. They stay reachable as `busybox find` / `busybox xargs`.
const APPLETS: &[&str] = &[
    "sh", "ash", "getty", "login", "init", "mount", "umount", "reboot", "poweroff",
    "halt", "hostname", "uname", "ls", "cat", "echo", "printf", "pwd", "cp", "mv",
    "rm", "mkdir", "rmdir", "ln", "ps", "id", "env", "clear", "dmesg", "free", "df",
    "du", "chmod", "chown", "sleep", "sync", "kill", "vi", "less", "more", "grep",
    "sed", "awk", "wc", "head", "tail", "sort", "date", "whoami", "tty",
    "dd", "mktemp", "seq", "touch", "mknod", "cttyhack", "su", "which",
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
        .filter(|u| u.groups.iter().any(|g| *g == "wheel"))
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
    // busybox init: `<id>::<action>:<process>`. `id` names the tty init opens for
    // the process; empty id => the system console. DEVTMPFS_MOUNT gives us a
    // populated /dev before init runs, so we only mount /proc, /sys and a couple of
    // tmpfs; getty auto-logs-in the console.
    "::sysinit:/bin/mount -t proc proc /proc\n\
     ::sysinit:/bin/mount -t sysfs sysfs /sys\n\
     ::sysinit:/bin/mount -t tmpfs tmpfs /tmp\n\
     ::sysinit:/bin/mount -t tmpfs tmpfs /run\n\
     ::sysinit:/bin/hostname -F /etc/hostname\n\
     ttyS0::respawn:/bin/getty -L -n -l /bin/autologin 115200 ttyS0 vt100\n\
     ::ctrlaltdel:/bin/reboot\n\
     ::shutdown:/bin/umount -a -r\n"
        .into()
}

fn build_autologin(sys: &SystemDef) -> String {
    // getty (-n -l) execs this with the tty already set up; force-login the
    // configured user with no authentication.
    format!("#!/bin/sh\nexec /bin/login -f {}\n", sys.autologin)
}

fn build_profile(sys: &SystemDef) -> String {
    // The login shell (busybox ash, invoked as `-sh`) sources this. We print the
    // banner HERE via a literal here-doc so it shows exactly once regardless of
    // busybox login's own motd feature, and set a sane PATH/PS1.
    let mut s = String::new();
    s.push_str("export PATH=/bin:/sbin:/usr/bin:/usr/sbin\n");
    s.push_str("export PS1='\\u@\\h:\\w\\$ '\n");
    s.push_str("cat <<'__TD_MOTD__'\n");
    s.push_str(sys.motd);
    if !sys.motd.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("__TD_MOTD__\n");
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

/// The gen_init_cpio spec: one line per archive member. `{in:...}`/`{root}` tokens
/// are expanded by the engine when it writes this file, so gen_init_cpio reads real
/// paths. Every entry is uid/gid 0 (root-owned) except the per-user home dirs.
fn build_spec(sys: &SystemDef) -> String {
    let mut s = String::new();
    // Directories first, parents before children.
    let dirs: &[(&str, &str)] = &[
        ("/dev", "0755"),
        ("/proc", "0755"),
        ("/sys", "0755"),
        ("/tmp", "1777"),
        ("/run", "0755"),
        ("/root", "0700"),
        ("/home", "0755"),
        ("/etc", "0755"),
        ("/bin", "0755"),
        ("/sbin", "0755"),
        ("/usr", "0755"),
        ("/usr/bin", "0755"),
        ("/usr/sbin", "0755"),
    ];
    for (d, m) in dirs {
        s.push_str(&format!("dir {d} {m} 0 0\n"));
    }
    // Per-user home dirs, owned by the user (skip /root, already added above).
    for u in sys.users {
        if u.home != "/root" {
            s.push_str(&format!("dir {} 0755 {} {}\n", u.home, u.uid, u.gid));
        }
    }
    // Console fallback device node (DEVTMPFS_MOUNT provides the real one; this
    // guarantees init has a console even if that mount is ever delayed).
    s.push_str("nod /dev/console 0600 0 0 c 5 1\n");
    // The static busybox + its applet symlinks, and /init = busybox init.
    s.push_str("file /bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n");
    for app in APPLETS {
        s.push_str(&format!("slink /bin/{app} /bin/busybox 0777 0 0\n"));
    }
    s.push_str("slink /init /bin/busybox 0777 0 0\n");
    s.push_str("file /bin/autologin {root}/bin/autologin 0755 0 0\n");
    // Generated /etc, packed from the files written into {root}/etc below.
    let etc: &[(&str, &str)] = &[
        ("passwd", "0644"),
        ("group", "0644"),
        ("shadow", "0600"),
        ("hostname", "0644"),
        ("os-release", "0644"),
        ("inittab", "0644"),
        ("profile", "0644"),
    ];
    for (f, m) in etc {
        s.push_str(&format!("file /etc/{f} {{root}}/etc/{f} {m} 0 0\n"));
    }
    s
}

/// A producer-rung shape check on the packed initramfs: real newc magic, a size
/// floor (static busybox alone is ~1 MiB), a `busybox cpio -t` parse (reds on a
/// truncated/corrupt stream), and the presence of the key members that make it
/// bootable. This is a build sanity assert, not a behavioural test — the boot is
/// exercised interactively by `td-recipe-eval run linux-x86-64`.
fn shape_check() -> String {
    "sz=$(wc -c < '{out}/rootfs.cpio'); \
     [ \"$sz\" -ge 65536 ] || { echo \"rootfs.cpio: implausibly small ($sz bytes) — the static busybox alone is ~1 MiB\" >&2; exit 1; }; \
     set -- $(od -An -tx1 -N 6 '{out}/rootfs.cpio'); \
     [ \"$1$2$3$4$5$6\" = 303730373031 ] || { echo 'rootfs.cpio: missing the newc cpio magic 070701' >&2; exit 1; }; \
     list=$('{in:busybox-x86-64}/bin/busybox' cpio -t < '{out}/rootfs.cpio' 2>/dev/null) || { echo 'rootfs.cpio: busybox cpio -t could not parse the archive (truncated/corrupt newc stream)' >&2; exit 1; }; \
     for m in init bin/busybox bin/sh bin/login bin/getty bin/autologin etc/inittab etc/passwd; do \
         printf '%s\\n' \"$list\" | grep -q -x -F \"$m\" || { echo \"rootfs.cpio: cpio member '$m' missing — the bootable userland is incomplete\" >&2; exit 1; }; \
     done"
        .into()
}

pub fn recipe() -> Recipe {
    let mut steps = Vec::new();

    // 1) Materialise the generated /etc + the autologin helper under {root}.
    steps.push(Step::MkDir {
        path: "{root}/etc".into(),
    });
    steps.push(Step::MkDir {
        path: "{root}/bin".into(),
    });
    let etc_files: [(&str, String, bool); 7] = [
        ("etc/passwd", build_passwd(&SYSTEM), false),
        ("etc/group", build_group(&SYSTEM), false),
        ("etc/shadow", build_shadow(&SYSTEM), false),
        ("etc/hostname", format!("{}\n", SYSTEM.hostname), false),
        ("etc/os-release", build_os_release(&SYSTEM), false),
        ("etc/inittab", build_inittab(), false),
        ("etc/profile", build_profile(&SYSTEM), false),
    ];
    for (rel, content, exec) in etc_files {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/{rel}"),
            content,
            exec,
        });
    }
    steps.push(Step::WriteFile {
        path: "{root}/bin/autologin".into(),
        content: build_autologin(&SYSTEM),
        exec: true,
    });

    // 2) Write the gen_init_cpio spec.
    steps.push(Step::WriteFile {
        path: "{root}/rootfs.spec".into(),
        content: build_spec(&SYSTEM),
        exec: false,
    });

    // 3) Pack the initramfs with the exported (td-built) gen_init_cpio: root-owned
    //    entries, the /dev/console fallback node, `-t 1` for a reproducible mtime.
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 {root}/rootfs.spec > {out}/rootfs.cpio",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    // 4) Require the artifact and shape-check it.
    steps.push(Step::Require {
        paths: vec!["{out}/rootfs.cpio".into()],
        exec: false,
    });
    steps.push(Step::run("{out}", &[SH, "-c", &shape_check()]).env("PATH", &mesboot0_path()));

    Recipe::mesboot("system-x86-64", "0.1")
        // busybox: the packed userland + the `cpio -t` shape check.
        // linux-x86-64: the EXPORTED gen_init_cpio packer.
        // glibc-x86-64: gen_init_cpio's runtime closure (it is a dynamically-linked
        //   HOSTCC hostprog; the kernel build ran it with the same glibc staged).
        .native_inputs(&["busybox-x86-64", "linux-x86-64", "glibc-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}
