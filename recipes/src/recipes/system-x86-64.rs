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
// packed cpio is the only automated guard; the interactive `td-recipe-eval run`
// command boots it under host qemu so you can use it.
//
// Userland strategy (v0): busybox provides init/getty/login/ash/coreutils — all
// present in its `defconfig`, all STATIC, so the initramfs is self-contained (no
// glibc closure, no host bytes). This is an explicitly TRANSITIONAL start on the
// AGENTS.md Rust-first path: each piece (coreutils -> uutils, the shell, the init)
// can later swap to a Rust equivalent behind this same recipe. Swapping busybox
// coreutils for the (dynamically-linked, Rust) uutils is its own atomic migration
// PR (AGENTS.md directive 5): it needs the full Rust bootstrap plus a packed glibc
// runtime closure, so it lands separately, not inline here.
//
// Layout: the image is STORE-NATIVE. The busybox binary is packed at its
// content-addressed /td/store/<hash>-busybox-x86-64/bin path, and /bin is a thin
// symlink farm whose entries (and /init) point straight into that store path. There
// is no /usr and no /sbin: every command resolves through /td/store, so the only
// real bytes live in the store rather than being copied across an FHS tree.
//
// Packing: the initramfs is built by the kernel's own `gen_init_cpio`, EXPORTED
// from the `linux-x86-64` output. gen_init_cpio writes the newc cpio from a text
// spec WITHOUT mknod privilege (the `nod /dev/console` fallback node is written
// straight into the archive) and stamps every entry uid/gid 0 regardless of the
// build user — so the rootfs is ROOT-owned, which busybox `cpio -o` could not
// achieve in the unprivileged sandbox. `-t 1` pins a fixed mtime so the cpio is
// reproducible. /dev is populated at boot by init mounting devtmpfs ITSELF (the
// inittab's first sysinit line) — CONFIG_DEVTMPFS_MOUNT does NOT auto-mount /dev on
// an initramfs boot — so the spec lists /dev only as a mountpoint, plus a packed
// /dev/console node that carries init's own stdio in the window BEFORE that mount.

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
           Minimal busybox userland, booted from an initramfs under qemu.\n  \
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
    // busybox init: `<id>::<action>:<process>`. `id` names the tty init opens for
    // the process; empty id => the system console. init must mount devtmpfs on /dev
    // ITSELF, FIRST: CONFIG_DEVTMPFS_MOUNT does NOT auto-mount /dev on an
    // initramfs/initrd boot (it fires only when the kernel mounts a real root), so
    // without this line /dev holds only the packed /dev/console node and the respawn
    // getty below cannot open /dev/ttyS0 — auto-login would never start. With
    // devtmpfs up, /dev is populated (ttyS0, null, …) before getty runs. /proc, /sys
    // and the tmpfs mounts follow; then getty auto-logs-in the serial console.
    "::sysinit:/bin/mount -t devtmpfs devtmpfs /dev\n\
     ::sysinit:/bin/mount -t proc proc /proc\n\
     ::sysinit:/bin/mount -t sysfs sysfs /sys\n\
     ::sysinit:/bin/mount -t tmpfs tmpfs /tmp\n\
     ::sysinit:/bin/mount -t tmpfs tmpfs /run\n\
     ::sysinit:/bin/hostname -F /etc/hostname\n\
     ttyS0::respawn:/bin/tty-session\n\
     ::ctrlaltdel:/bin/reboot\n\
     ::shutdown:/bin/umount -a -r\n"
        .into()
}

/// The ttyS0 session wrapper, run by init AS ROOT (inittab `respawn`). It runs the
/// normal getty -> autologin -> `login -f <user>` flow, then, when that session
/// ENDS — the greeter user types `exit` / Ctrl-D — resets the machine so the VM
/// stops. The auto-login user is UNPRIVILEGED and cannot shut the system down
/// itself; this wrapper runs as root (init's child), so it does it on the user's
/// behalf, making `exit` a clean way out of the VM. `reboot -f` calls `reboot(2)`
/// directly and, under qemu's `-no-reboot`, makes qemu exit 0 — the exact proven
/// exit path the kernel-boot test uses (`linux-x86-64-test`). An initramfs RAM
/// rootfs has no disk to flush, so a direct reset is clean here (the orderly
/// `::shutdown:` umount is skipped, which does not matter for tmpfs/devtmpfs).
fn build_tty_session() -> String {
    "#!/bin/sh\n\
     /bin/getty -L -n -l /bin/autologin 115200 ttyS0 vt100\n\
     exec /bin/reboot -f\n"
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
    // Base dirs, parents before children. This image is STORE-NATIVE: there is no /usr
    // and no /sbin — /bin is a thin symlink farm and every command resolves into
    // /td/store/<hash>-.../bin, so the only real bytes live under /td/store rather than
    // scattered across /usr + /bin (re #541: "why is there /bin and /usr, not /td/store
    // symlinks?"). PATH is just /bin (build_profile).
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
        ("/td", "0755"),
        ("/td/store", "0755"),
    ];
    for (d, m) in dirs {
        s.push_str(&format!("dir {d} {m} 0 0\n"));
    }
    // The busybox store tree: pack the static busybox binary AT its content-addressed
    // /td/store path. The {in:busybox-x86-64} token expands to /td/store/<hash>-busybox-x86-64
    // in BOTH the member name and the source position, so gen_init_cpio reads the staged
    // binary and writes it to the in-image store path its /bin symlinks point at. Only the
    // binary is packed (busybox is static and self-contained); the output's own applet
    // symlinks are not needed — the image supplies its own /bin farm below.
    s.push_str("dir {in:busybox-x86-64} 0755 0 0\n");
    s.push_str("dir {in:busybox-x86-64}/bin 0755 0 0\n");
    s.push_str("file {in:busybox-x86-64}/bin/busybox {in:busybox-x86-64}/bin/busybox 0755 0 0\n");
    // Per-user home dirs, owned by the user (skip /root, already added above).
    for u in sys.users {
        if u.home != "/root" {
            s.push_str(&format!("dir {} 0755 {} {}\n", u.home, u.uid, u.gid));
        }
    }
    // Packed /dev/console node: init needs a console for its OWN stdio before it can
    // run the inittab's first sysinit line (which mounts devtmpfs on /dev, and devtmpfs
    // then provides the real nodes). This static node carries that pre-mount window.
    s.push_str("nod /dev/console 0600 0 0 c 5 1\n");
    // /bin symlink farm: /bin/busybox and every applet resolve DIRECTLY into the store
    // busybox, and so does /init. busybox dispatches on argv[0]'s basename, so the target
    // need only BE the busybox binary; pointing each link straight at the store path makes
    // `ls -l /bin` show every command coming from /td/store (the requested layout).
    s.push_str("slink /bin/busybox {in:busybox-x86-64}/bin/busybox 0777 0 0\n");
    for app in APPLETS {
        s.push_str(&format!(
            "slink /bin/{app} {{in:busybox-x86-64}}/bin/busybox 0777 0 0\n"
        ));
    }
    s.push_str("slink /init {in:busybox-x86-64}/bin/busybox 0777 0 0\n");
    s.push_str("file /bin/autologin {root}/bin/autologin 0755 0 0\n");
    // The ttyS0 session wrapper (getty -> login, then reboot on exit); see
    // build_tty_session. inittab respawns it in place of a bare getty line.
    s.push_str("file /bin/tty-session {root}/bin/tty-session 0755 0 0\n");
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
/// truncated/corrupt stream), the presence of the key members that make it
/// bootable, AND that busybox actually implements EVERY packed APPLETS entry (a
/// config drift or a tailoring typo that dropped/misnamed an applet would leave a
/// dead /bin symlink the cpio member check alone can't catch). All strings are ASCII: td-builder's
/// config reader is Latin-1, so a UTF-8 glyph here would be corrupted in the
/// executed step. This is a build sanity assert, not a behavioural test — the boot
/// is exercised interactively by `td-recipe-eval run`.
fn shape_check() -> String {
    "sz=$(wc -c < '{out}/rootfs.cpio'); \
     [ \"$sz\" -ge 65536 ] || { echo \"rootfs.cpio: implausibly small ($sz bytes) - the static busybox alone is ~1 MiB\" >&2; exit 1; }; \
     set -- $(od -An -tx1 -N 6 '{out}/rootfs.cpio'); \
     [ \"$1$2$3$4$5$6\" = 303730373031 ] || { echo 'rootfs.cpio: missing the newc cpio magic 070701' >&2; exit 1; }; \
     list=$('{in:busybox-x86-64}/bin/busybox' cpio -t < '{out}/rootfs.cpio' 2>/dev/null) || { echo 'rootfs.cpio: busybox cpio -t could not parse the archive (truncated/corrupt newc stream)' >&2; exit 1; }; \
     for m in init bin/busybox bin/sh bin/login bin/getty bin/autologin bin/tty-session etc/inittab etc/passwd; do \
         printf '%s\\n' \"$list\" | grep -q -x -F \"$m\" || { echo \"rootfs.cpio: cpio member '$m' missing - the bootable userland is incomplete\" >&2; exit 1; }; \
     done; \
     printf '%s\\n' \"$list\" | grep -qE '^td/store/[^/]+/bin/busybox$' || { echo 'rootfs.cpio: the busybox binary is not packed under td/store/<hash>-busybox-x86-64/bin - the store-native /bin symlinks would all dangle' >&2; exit 1; }; \
     applets=$('{in:busybox-x86-64}/bin/busybox' --list 2>/dev/null) || { echo 'rootfs.cpio: busybox --list failed - cannot verify applet coverage' >&2; exit 1; }; \
     for a in @APPLETS@; do \
         printf '%s\\n' \"$applets\" | grep -q -x -F \"$a\" || { echo \"rootfs.cpio: busybox does not implement applet '$a' (config drift) - its packed /bin/$a symlink would be a dead link\" >&2; exit 1; }; \
     done"
        // Validate EVERY packed applet, not just the greeter-critical few: build_spec
        // links every APPLETS entry into /bin, so a typo or a disabled applet must red
        // here rather than ship a dead symlink (re #541, Codex review). Names are all
        // shell-safe identifiers, so a space-joined `for` list is safe unquoted.
        .replace("@APPLETS@", &APPLETS.join(" "))
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
    steps.push(Step::WriteFile {
        path: "{root}/bin/tty-session".into(),
        content: build_tty_session(),
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
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/rootfs.spec' > '{out}/rootfs.cpio'",
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
        // busybox: the packed userland + the `cpio -t` shape check (static binary).
        // linux-x86-64: the EXPORTED gen_init_cpio packer (verified STATICALLY linked:
        //   `ELF 64-bit ... statically linked`).
        // glibc-x86-64: no step in THIS (busybox) image links against the native glibc —
        //   gen_init_cpio and busybox are both static and the shape check runs the mesboot0
        //   userland — but it is already in the closure (busybox links libc.a from it) and
        //   is the runtime closure the uutils follow-up will pack, so it is retained here
        //   ahead of that migration rather than dropped and re-added (re #541).
        .native_inputs(&["busybox-x86-64", "linux-x86-64", "glibc-x86-64"])
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
            // with <applet> in APPLETS. A bare "sh" would pass a naive basename check yet
            // fail at runtime (execv("sh") -> ENOENT -> login respawn-loops); reject it.
            let packed_applet = u.shell.strip_prefix("/bin/");
            assert!(
                packed_applet.is_some_and(|a| APPLETS.contains(&a)),
                "user '{}' login shell '{}' must be \"/bin/<applet>\" with <applet> in APPLETS \
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

    /// getty auto-logs-in via `-l /bin/autologin`, and login needs both applets; the
    /// respawn line is inert without them. `reboot` is what `tty-session` execs when the
    /// greeter session ends (the in-guest power-off path), so it is greeter-critical too.
    /// Belt-and-braces against an APPLETS edit that drops one (the cpio shape check
    /// catches it at build time, this catches it at `cargo test` time).
    #[test]
    fn greeter_applets_are_present() {
        for a in ["sh", "getty", "login", "init", "mount", "umount", "reboot"] {
            assert!(APPLETS.contains(&a), "greeter applet '{a}' missing from APPLETS");
        }
    }

    /// The inittab must respawn `tty-session` (not a bare getty), and `tty-session` must
    /// exec `reboot -f` after the login flow — that is the "exit / Ctrl-D powers off the
    /// VM" path. A refactor that reverts the inittab to a bare getty, or drops the reboot
    /// from tty-session, would silently strip the in-guest shutdown; red it here.
    #[test]
    fn exit_powers_off_the_vm() {
        let inittab = build_inittab();
        assert!(
            inittab.contains("ttyS0::respawn:/bin/tty-session"),
            "inittab must respawn /bin/tty-session on ttyS0 (the getty -> reboot wrapper)"
        );
        let session = build_tty_session();
        assert!(
            session.contains("/bin/getty ") && session.contains("/bin/reboot -f"),
            "tty-session must run getty then `reboot -f` so the greeter's exit stops the VM"
        );
    }
}
