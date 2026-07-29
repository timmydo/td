use crate::ladder::{
    post_bootstrap_path, AUTOTEST_CMDLINE_TOKEN, BOOT_FAIL_TARGET_CMDLINE_TOKEN,
    BOOT_SUCCESS_WAIT_CMDLINE_PREFIX, DEPLOY_INSTALL_CMDLINE_TOKEN, GREETER_MARKER,
    NETTEST_CMDLINE_TOKEN, NETTEST_DEFAULT_HOST, NETTEST_DEFAULT_PORT, PERSIST_READ_CMDLINE_TOKEN,
    PERSIST_WRITE_CMDLINE_TOKEN, POST_BOOTSTRAP_SH, RIPGREP_FD_RUNTIME_MARKER, SSHD_MARKER,
    SYSTEM_BOOT_SUCCESS_MARKER, SYSTEM_DEPLOY_INSTALL_MARKER, SYSTEM_ETC_MUTABLE_MARKER,
    SYSTEM_ETC_RO_MARKER, SYSTEM_NET_REACH_MARKER, SYSTEM_NET_RESOLVE_MARKER, SYSTEM_NET_UP_MARKER,
    SYSTEM_PERSIST_READ_MARKER, SYSTEM_PERSIST_WRITE_MARKER, SYSTEM_ROOT_RO_MARKER,
    SYSTEM_SHUTDOWN_MARKER, SYSTEM_STATE_OWNER_MARKER, SYSTEM_STATE_WRITABLE_MARKER,
    TD_INIT_RUNTIME_MARKER, TD_LOGIN_RUNTIME_MARKER, TD_TXT_RUNTIME_MARKER, TD_UTIL_RUNTIME_MARKER,
    UUTILS_RUNTIME_MARKER,
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
// The direct-boot selector initramfs carries static busybox, td-init (for the
// mount pair), td-boot, and td-kexec; it has no branch that can enter a
// deployment directly, because it links no `/bin/switch_root`. It verifies
// current/previous from the Btrfs volume and kexecs the selected deployment.
// That deployment's distinct initramfs requires the td.deployment handoff,
// re-verifies root.erofs, binds it to a read-only loop device, mounts @var from
// Btrfs, and switch_roots. `/etc` stays deployment-owned and immutable, with ONE
// reviewed symlink per mutable file out to writable state (the `MUTABLE_ETC` table
// below) rather than an overlay — so the read-only-`/etc` assertion survives while
// per-machine identity still persists; `/home` and `/root` are root-image symlinks
// into `/var`. The real root is
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
// Userland strategy (v0): the static Rust td-init multicall provides the boot
// glue — PID 1, the pivot, and every mount/umount on the machine — the static
// Rust td-login serves the credential switch (/bin/{login,su}) and td-util the
// diagnostics, while static busybox provides the shell, the tty setup and the
// text tools uutils lacks; source-built Rust uutils provides the interactive
// core file/text userland with its declared glibc runtime closure.

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
    /// The user getty auto-logs-in on ttyS0, through `/etc/autologin` running
    /// `login -f`. td-login refuses `-f` for a LOCKED account, so this user must be
    /// `passwordless` — `system_def_is_self_consistent` holds that.
    autologin: &'static str,
    users: &'static [User],
}

/// Account names are embedded UNQUOTED in generated root shell — `/bin/su -s /bin/sh
/// <name> -c …` in rootcheck and every health leg, and `/bin/login -f <name>` in
/// /etc/autologin — and unquoted in the colon-separated /etc/{passwd,group,shadow}
/// this recipe writes. A name carrying `$(…)` would run as ROOT at sysinit; one
/// carrying `:` would silently restructure the account database. This is the same
/// hazard `valid_home` below already guards, applied to the other string that
/// reaches those scripts.
///
/// The grammar is td-login's own `plausible_name`, so a name this accepts is one the
/// image's `login` will look up (`the_account_grammar_matches_the_one_td_login_uses`
/// pins the two together).
#[cfg(test)]
fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
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

const UI_USER: &str = "tester";
const UI_UID: u32 = 1000;
const UI_GID: u32 = 1000;
// ────────────────────────────────────────────────────────────────────────────────

/// The real-root `/bin` is a symlink farm split across SIX multicall binaries, each
/// dispatching on argv[0]'s basename: the static **busybox** (the shell and the tty glue),
/// the static Rust **td-init** (the boot glue — see `TD_INIT_FARM`), the static Rust
/// **td-login** (the credential switch — see `TD_LOGIN_APPLETS`), the static Rust
/// **td-txt** (grep/sed — see `TD_TXT_APPLETS`), the static Rust **td-util**
/// (diagnostics), and the dynamically-linked Rust **uutils** `coreutils`
/// (the core file/text userland — #547's cutover). A name goes in exactly one list;
/// `shape_check` asserts the owning binary actually provides it.
///
/// BUSYBOX now keeps only what no Rust multicall serves: the shell (`sh`/`ash`) and
/// `getty` (the tty setup half of the login chain — `login`/`su` moved to td-login).
/// Those are the last two, and only `sh` and `getty` have a call site at all.
///
/// `grep`/`sed` LEFT this list for td-txt (see `TD_TXT_APPLETS`) once the conformance corpus
/// covered the shapes the image's own scripts use — which is also why those scripts stopped
/// spelling them `/bin/busybox grep` and now call `/bin/grep`: with the name off this list,
/// the multiplexer spelling would name an applet `shape_check` no longer verifies.
///
/// `awk` and `more` LEFT this list, and neither was replaced in kind. No generated script
/// invoked `awk` once `rootcheck`'s one field test became an ERE, so it was a name a user
/// might reach for and nothing else; `no_generated_script_invokes_awk` still bans it, and
/// the ban matters MORE now that `/bin/awk` is not a symlink — a script naming it would
/// fail at run time instead of quietly running a farm no conformance corpus scores.
/// `more` is served by td-util's `less` (see `TD_UTIL_APPLETS`): the uutils pager is
/// behind a feature that compiles a crossterm stack into the shipped multicall, which is a
/// dependency call rather than a farm move, and busybox was being carried for that one
/// name. What td's own pager gives up to stay dependency-free — single-keystroke commands
/// and backward scrolling — is recorded in `td-util/src/less.rs`.
///
/// `find`/`xargs` are intentionally NOT bare symlinks either: the ladder's findutils
/// dead-axis lock (`no_bootstrap_step_invokes_host_find_or_xargs`) forbids those tokens in
/// any step text and can't tell a cpio member NAME from a host invocation; they stay
/// reachable as `busybox find` / `busybox xargs`.
///
/// `mount`/`umount` LEFT this list with td-init's `mount(2)`/`umount2(2)` amendment, and
/// `losetup` with the `LOOP_SET_FD` one. Those three were the boot-path jobs needing a
/// syscall no safe `std` reaches, which is what a 1 MiB C multicall was being carried for;
/// what busybox still serves here is ordinary userland.
const BUSYBOX_APPLETS: &[&str] = &["sh", "ash", "getty"];

/// The text userland, served by the static td-txt multicall — the `grep` and `sed` busybox
/// used to own, because uutils ships neither. Like every other farm here it dispatches on
/// argv[0]'s basename, so a `/bin/<applet>` -> td-txt symlink runs that applet; like td-util
/// and td-init (and unlike uutils) it is an ET_EXEC with an EMPTY runtime closure.
///
/// These names are LOAD-BEARING, which is what separates this farm from td-util's: the real
/// root's `/etc/rootcheck` decides whether the boot is healthy with five `grep -Eq` runs over
/// `/proc/mounts` and `/etc/machine-id`, and `/etc/profile`, `/etc/netup`, `/etc/bootsuccess`
/// and `/etc/bootfail` each read `/proc/cmdline` with one. So every boot runs this binary
/// before it can report success, and a grep that answered wrongly would not fail loudly — it
/// would quietly mark a broken root healthy. That is why the corpus (td-txt/spec) had to
/// cover those exact invocation shapes before this list existed.
///
/// The fifth was busybox's last text call on the image — an `awk` field test over
/// `/proc/mounts`. It decided boot health from a farm the conformance corpus does not
/// cover, so moving it here put every text predicate in that decision under the corpus.
///
/// `/proc` files stat as zero-length, so the applets must read them as streams rather than
/// sizing a buffer from `st_size`; td-txt reads whole inputs by `read_to_end`, and the
/// greeter probe below re-proves it on the booted image against the real `/proc/mounts`.
const TD_TXT_APPLETS: &[&str] = &["grep", "sed"];

/// Names the busybox retirement DROPS rather than reimplementing as a td app: nothing on
/// the image calls any of them. Listed rather than merely absent so the drop is checkable —
/// `shape_check` asserts the staged root packs no such `/bin` entry, and
/// `dropped_applets_stay_dropped` makes putting one back a deliberate deletion here.
///
/// `less` LEFT this list: it was dropped only because `more` was the pager, and `more` is
/// what the busybox binary was being carried for once the shell and getty are discounted.
/// td-util serves `less` now, so the pager name that survives is the one people type, and
/// `more`/`awk` join the drops — `awk` because no generated script may name it anyway
/// (`no_generated_script_invokes_awk`), `more` because there is no reason to ship two
/// pagers. Neither has a call site to break.
const DROPPED_APPLETS: &[&str] = &["vi", "more", "awk"];
/// The credential switch, served by the static td-login multicall — the two busybox applets
/// that change WHO A PROCESS IS. They are their own binary, and their own AGENTS.md unsafe
/// exception, because a credential-ordering bug in them is privilege escalation rather than
/// a malfunction: `setuid(2)` before `setgroups(2)` drops the uid and silently keeps the
/// previous holder's supplementary groups. td-login/THREAT-MODEL.md is the specification.
///
/// Both names are LOAD-BEARING on this image, more so than any other farm here. `/bin/login`
/// is what getty execs through `/etc/autologin`, so it is how the machine reaches its greeter
/// at all; `/bin/su` is how `/etc/rootcheck` and `/etc/bootsuccess` run every unprivileged
/// health leg. Neither can regress without the boot failing, which is why — unlike td-util
/// and td-init — this farm needs no synthetic per-name probe. What the boot cannot see is a
/// switch that started a working session while leaving a residual credential behind, and
/// that is what `TD_LOGIN_RUNTIME_MARKER` gates on.
///
/// td-login is an ET_EXEC with an EMPTY runtime closure: a `login` that dies with the
/// dynamic closure locks an operator out of the console exactly when the closure is what
/// broke.
const TD_LOGIN_APPLETS: &[&str] = &["login", "su"];

/// The boot glue, served by the static td-init multicall — the busybox applets that need a
/// RAW SYSCALL, which is why they are a separate binary from td-util's
/// `#![forbid(unsafe_code)]` farm. Unlike every other farm here these names are LOAD-BEARING:
/// `/init` is PID 1, `switch_root` is the stage-1 pivot, `mount`/`umount` bring up and
/// release every filesystem the machine has (both `/init` scripts, sysinit, td-boot and
/// `/etc/shutdown`), `hostname -F` runs at sysinit (the `-F` flag is the gap uutils has — it
/// ships no `hostname` at all), and `reboot` is how a boot ends. So this farm is not merely
/// symlinked and probed; the image's actual boot path runs it, and `shape_check` additionally
/// dry-runs the shipped `/etc/inittab` through the packed binary so a table PID 1 would
/// reject reds the BUILD rather than the boot.
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
    // Probed by REFUSAL for the same reason `mount` is: attaching a loop device SUCCEEDS
    // destructively, and the greeter is unprivileged so a real attach would EPERM anyway.
    // The refusal proves the packed name and the argv[0] dispatch, and that arguments are
    // parsed before any ioctl — which is what stops a mistyped device from being bound.
    // The BOOT use is initramfs-only (the deployment phase binds the root loop), so this
    // probe on the real root is the only automated exercise the applet's dispatch gets:
    // the ioctl itself needs privilege no test here has.
    (
        "losetup",
        Probe::Refuses("--not-an-option", "usage: losetup"),
    ),
    // Probed by REFUSAL, though for the opposite reason to the three below: mount SUCCEEDS
    // destructively. A probe that mounted something would change the running system's mount
    // table to prove a symlink, and the greeter is unprivileged so the interesting paths
    // would EPERM anyway. The refusal proves the packed name, the argv[0] dispatch, and the
    // property every fixed mount line on this image rests on — arguments are parsed before
    // any syscall, so a typo cannot mount over a directory the boot needs.
    (
        "mount",
        Probe::Refuses("--not-an-option", "unrecognised argument"),
    ),
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
    // The one applet on this image whose SUCCESS path can end the boot as thoroughly as
    // `reboot`: `umount -a` releases the root. So it is proven by its refusal too — and the
    // bad option is the sharpest case, since a parser that fell through to `-a` would take
    // the greeter's own filesystems away.
    (
        "umount",
        Probe::Refuses("--not-an-option", "unrecognised argument"),
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

/// Whether the text a token follows leaves it in shell COMMAND position.
///
/// A heuristic, and stated as one: deciding this exactly needs a shell grammar.
/// It errs toward CALLING it a command (an unrecognised introducer is treated as
/// one only when the token starts a word after nothing else), so the guard that
/// uses it fails closed. Enough for generated scripts, whose shapes this file
/// writes itself.
#[cfg(test)]
fn in_command_position(before: &str) -> bool {
    let head = before.trim_end_matches([' ', '\t']);
    // A trailing backslash is a shell line-continuation, so what decides position
    // is whatever precedes IT. Stripped before the separator test but never
    // together with the newline: doing both at once removes the very `\n` that
    // says "start of line", which is the commonest command position there is.
    let head = if head.ends_with('\\') {
        head.trim_end_matches('\\').trim_end_matches([' ', '\t'])
    } else {
        head
    };
    if head.is_empty() {
        return true;
    }
    // `)` closes a `case` pattern, and this file writes that shape.
    for sep in ["\n", "\r", ";", "&", "|", "(", ")", "`", "{", "&&", "||", "!"] {
        if head.ends_with(sep) {
            return true;
        }
    }
    // An opening quote is command position ONLY as the body of `-c`. Treating every
    // quote that way would flag `echo "td-util: ..."`, which is prose, not a call —
    // and a guard that cries wolf on its own diagnostics gets deleted.
    if let Some(rest) = head.strip_suffix(['\'', '"']) {
        if rest.trim_end_matches([' ', '\t']).ends_with("-c") {
            return true;
        }
    }
    // An assignment prefix (`VAR=value cmd`) also leaves a command next.
    if head.rsplit([' ', '\t', '\n']).next().is_some_and(|w| {
        let mut parts = w.splitn(2, '=');
        parts.next().is_some_and(|n| {
            !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_')
        }) && parts.next().is_some()
    }) {
        return true;
    }
    // Keywords that introduce a command WITHOUT being punctuation. Matched on a
    // word boundary, so a mention ending in one (`motif`, `ado`) is not read as
    // an introducer — the punctuation above cannot collide that way, these can.
    for word in [
        "if", "elif", "then", "else", "while", "until", "do", "done", "exec", "command", "env",
    ] {
        if let Some(rest) = head.strip_suffix(word) {
            if rest.is_empty() || !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                return true;
            }
        }
    }
    false
}

/// The applet names a multicall's own `APPLETS` table lists, read from its source.
///
/// The shipped roster is that table; restating it here would be a second list to drift.
#[cfg(test)]
fn applet_table(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let Some(start) = source.match_indices("const APPLETS").next().map(|(i, _)| i) else {
        return names;
    };
    let body = source.get(start..).unwrap_or("");
    let end = body.match_indices("];").next().map(|(i, _)| i).unwrap_or(body.len());
    for (idx, _) in body.get(..end).unwrap_or("").match_indices("(\"") {
        let rest = body.get(idx.saturating_add(2)..).unwrap_or("");
        let name: String = rest.chars().take_while(|c| *c != '"').collect();
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
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

/// Applets still reached through the packed BusyBox multicall as `/bin/busybox <applet>`.
/// They must EXIST in busybox, so `shape_check` probes them against `busybox --list` like
/// the farm names; otherwise a config drift breaks sysinit with no build-time signal.
/// Unlike the three /bin farms this is deliberately NOT disjoint from BUSYBOX_APPLETS.
/// `script_applets_are_covered` derives it from the script text both ways, so neither an
/// uncovered call nor a stale entry survives; td-boot's own invocations are justified by
/// its protocol constant.
///
/// The ten that LEFT — cat, chmod, chown, ln, mkdir, printf, readlink, rm, sleep, test —
/// are td-util applets now, and `sync` is td-init's. All eleven were reached here rather
/// than through uutils' `/bin/<name>` because they run where a dynamic closure is not safe
/// to assume; td-util and td-init are static with empty closures, so the property is kept
/// while the third-party multicall goes.
///
/// `test` was the one that looked like it needed a syscall amendment: rootcheck asked
/// `test ! -w /var` under `su ... <user>`, and `-w` is access(2). It moved instead by
/// deleting the question — those probes WRITE now (see `build_rootcheck`), which is
/// ground truth rather than a prediction of it, so the applet never serves `-w`.
///
/// `mknod` left with td-init's tenth syscall — the amendment that also gave the
/// crate one `dev_t` packing instead of two, and a readback that unlinks a node
/// the kernel disagrees about. What remains is `sh`: the /init and /etc script
/// interpreter, until td-sh ships.
const INITRAMFS_APPLETS: &[&str] = &["sh"];

/// The diagnostics userland, served by the static td-util multicall — the busybox names
/// uutils does not provide. Like busybox and uutils it dispatches on argv[0]'s basename, so
/// a `/bin/<applet>` -> td-util symlink runs that applet. Unlike uutils it is an ET_EXEC
/// with an EMPTY runtime closure, so these entries keep working when no dynamic loader
/// would: a diagnostics tool that dies with the closure is useless exactly when it is
/// needed. `shape_check` probes each name against the packed binary's own `--list`, so an
/// entry td-util does not serve reds the build rather than shipping a `/bin` name that
/// dispatches to nothing.
const TD_UTIL_APPLETS: &[&str] = &["clear", "which", "free", "ps", "dmesg", "less"];

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
    "whoami", "tty", "dd", "mktemp", "seq", "touch", "mknod", "kill", "readlink", "basename",
    "dirname", "true", "false", "printenv", "link", "unlink",
];

enum UutilsProbe {
    Output {
        applet: &'static str,
        args: &'static str,
        expected: &'static str,
    },
    Succeeds(&'static str),
    Fails(&'static str),
    Printenv,
    Link,
    Unlink,
}

const UUTILS_BEHAVIOR_PROBES: &[UutilsProbe] = &[
    UutilsProbe::Output {
        applet: "basename",
        args: "/tmp/td-uutils-probe/source",
        expected: "source",
    },
    UutilsProbe::Output {
        applet: "dirname",
        args: "/tmp/td-uutils-probe/source",
        expected: "/tmp/td-uutils-probe",
    },
    UutilsProbe::Succeeds("true"),
    UutilsProbe::Fails("false"),
    UutilsProbe::Printenv,
    UutilsProbe::Link,
    UutilsProbe::Unlink,
];

impl UutilsProbe {
    fn applet(&self) -> &'static str {
        match self {
            Self::Output { applet, .. } | Self::Succeeds(applet) | Self::Fails(applet) => applet,
            Self::Printenv => "printenv",
            Self::Link => "link",
            Self::Unlink => "unlink",
        }
    }
}

/// Render one unprivileged `/bin` behavior check. The result is embedded in a
/// single-quoted `su -c` script, so it must contain no single quotes.
fn uutils_behavior_probe(probe: &UutilsProbe) -> String {
    let applet = probe.applet();
    match probe {
        UutilsProbe::Output { args, expected, .. } => format!(
            "if o=$(/bin/{applet} {args} 2>&1); then \
             [ \"$o\" = \"{expected}\" ] || \
             {{ echo \"uutils: /bin/{applet} returned unexpected output: $o\"; u=0; }}; \
             else echo \"uutils: /bin/{applet} failed: $o\"; u=0; fi; "
        ),
        UutilsProbe::Succeeds(_) => format!(
            "/bin/{applet} || \
             {{ echo \"uutils: /bin/{applet} did not exit zero\"; u=0; }}; "
        ),
        UutilsProbe::Fails(_) => format!(
            "o=$(/bin/{applet} 2>&1); s=$?; \
             if [ \"$s\" != 1 ] || [ -n \"$o\" ]; then \
             echo \"uutils: /bin/{applet} exited $s: $o\"; u=0; fi; "
        ),
        UutilsProbe::Printenv => format!(
            "TD_UUTILS_PROBE=td-uutils-v1; export TD_UUTILS_PROBE; \
             if o=$(/bin/{applet} TD_UUTILS_PROBE 2>&1); then \
             [ \"$o\" = td-uutils-v1 ] || \
             {{ echo \"uutils: /bin/{applet} returned unexpected output: $o\"; u=0; }}; \
             else echo \"uutils: /bin/{applet} failed: $o\"; u=0; fi; "
        ),
        UutilsProbe::Link => format!(
            "if /bin/printf \"%s\\n\" td-uutils-before > /tmp/td-uutils-probe/source; then \
             if /bin/{applet} /tmp/td-uutils-probe/source /tmp/td-uutils-probe/hard; then \
             if [ -h /tmp/td-uutils-probe/hard ]; then \
             echo \"uutils: /bin/{applet} created a symbolic link\"; u=0; \
             elif /bin/printf \"%s\\n\" td-uutils-after > /tmp/td-uutils-probe/source; then \
             if o=$(/bin/cat /tmp/td-uutils-probe/hard 2>&1); then \
             if [ \"$o\" = td-uutils-after ]; then h=1; \
             else echo \"uutils: /bin/{applet} hard-link contents mismatch: $o\"; u=0; fi; \
             else echo \"uutils: /bin/cat could not read hard link: $o\"; u=0; fi; \
             else echo \"uutils: /bin/printf could not rewrite hard-link source\"; u=0; fi; \
             else echo \"uutils: /bin/{applet} failed\"; u=0; fi; \
             else echo \"uutils: /bin/printf could not seed hard-link source\"; u=0; fi; "
        ),
        UutilsProbe::Unlink => format!(
            "if [ \"$h\" = 1 ]; then \
             if /bin/{applet} /tmp/td-uutils-probe/hard; then \
             if [ -e /tmp/td-uutils-probe/hard ] || \
             [ -h /tmp/td-uutils-probe/hard ]; then \
             echo \"uutils: /bin/{applet} left the directory entry present\"; u=0; fi; \
             if o=$(/bin/cat /tmp/td-uutils-probe/source 2>&1); then \
             [ \"$o\" = td-uutils-after ] || \
             {{ echo \"uutils: /bin/{applet} source contents mismatch: $o\"; u=0; }}; \
             else echo \"uutils: /bin/cat could not read source after unlink: $o\"; u=0; fi; \
             else echo \"uutils: /bin/{applet} failed\"; u=0; fi; fi; "
        ),
    }
}

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
    // stage-1 `switch_root`ed into it.
    //
    // PID 1 now keeps only what it must OWN: the pseudo-filesystem mounts, then
    // respawning td-svc. Every service, its ordering, and its restart policy moved to
    // /etc/td-svc.conf — see build_td_svc_conf for why, and td-svc/DESIGN.md §1 for the
    // three things a signal-free PID 1 cannot express.
    //
    // The mounts stay HERE, not in the table, because td-svc reads /proc: its own
    // process group and session come from /proc/self/stat, and every containment query
    // and liveness check is a /proc read. A td-svc started before /proc exists comes up
    // degraded and cannot signal a group at all (it fails closed). sysinit jobs run to
    // completion before any respawn line starts, so this ordering IS that guarantee.
    // Mounting over a read-only dir is a VFS overlay, no write to the erofs. It does NOT
    // mount /var, /tmp, or /run: stage-1 already mounted persistent @var and the
    // volatile tmpfs filesystems, and switch_root preserves the mounts.
    //
    // td-svc is a `respawn` job: if the supervisor dies, PID 1 brings it back. That is
    // the whole reason PID 1 keeps reaping — td-svc's children reparent onto PID 1 when
    // it dies, and only wait4(-1) can collect them.
    //
    // There is still no `ctrlaltdel` or `shutdown` line: td-init supervises with NO
    // signals (a blocking wait4 IS its event loop), so both actions are signal contracts
    // it cannot honour. Ctrl-Alt-Del is td-svc's to own precisely because PID 1 cannot.
    format!(
        "::sysinit:/bin/mount -t devtmpfs devtmpfs /dev\n\
         ::sysinit:/bin/mount -t proc proc /proc\n\
         ::sysinit:/bin/mount -t sysfs sysfs /sys\n\
         ::respawn:/bin/td-svc run -f {TD_SVC_CONF}\n"
    )
}

/// Where the unit table lives. ONE const, and every consumer derives from it: the
/// inittab's `-f`, the `etc_files` entry that generates it, and the shape check that
/// runs `td-svc check` against it. They cannot disagree, which matters because a
/// disagreement is silent and boot-fatal — td-svc given a path that does not exist
/// prints one line and then idles forever with zero units (it has no exit path, so PID
/// 1 never respawns it either), which is a machine with no console, no sshd and no
/// network. The recipe would still generate a perfectly good table at the old path and
/// the shape check would still validate it, both exiting 0.
const TD_SVC_CONF: &str = "/etc/td-svc.conf";

/// `TD_SVC_CONF` as `etc_files` names it: relative to /etc, since that is the directory
/// it writes into. Derived rather than repeated — see TD_SVC_CONF for what a divergence
/// costs. `the_unit_table_path_has_exactly_one_source_of_truth` pins the relationship.
fn td_svc_conf_etc_name() -> &'static str {
    match TD_SVC_CONF.strip_prefix("/etc/") {
        Some(name) => name,
        // Unreachable while TD_SVC_CONF is under /etc; the test above reds if it moves,
        // rather than this silently generating a file the inittab does not name.
        None => TD_SVC_CONF,
    }
}

/// Every unit `/etc/td-svc.conf` must resolve into a start order.
///
/// The shape check greps `td-svc check`'s printed plan for each. `check` already reds
/// on a table it cannot parse, but a unit SILENTLY dropped from the plan — skipped for
/// an unsatisfiable dependency — is a clean exit with a shorter list, and that is the
/// regression this catches: the boot comes up missing a service and says nothing.
const TD_SVC_UNITS: [&str; 11] = [
    "hostname",
    "td-firstboot",
    "rootcheck",
    "seat",
    "netup",
    "wayland",
    "ui-demo",
    "bootsuccess",
    "bootfail",
    "sshd",
    "greeter",
];

/// Each generated oneshot's `timeout=`.
///
/// td-svc defaults one when a table omits it — a oneshot that hangs never settles, and
/// everything ordered after it waits forever — but that default is sized for a small
/// job and these are not. So each is explicit, and each is a BACKSTOP against a hang,
/// not a service-level objective: the values sit far above what the job can actually
/// take. Under the old inittab these jobs had no bound at all, and one that hung
/// wedged the boot with no console and no sshd, so a finite bound is strictly better —
/// a oneshot that times out is marked failed, and since `after=` is ordering, every
/// dependent still runs.
mod svc_timeouts {
    /// `sethostname(2)` plus one file read.
    pub const HOSTNAME: u32 = 30;
    /// Mints the per-machine identity: ed25519 keygen, writes to /var, then sync.
    pub const FIRSTBOOT: u32 = 300;
    /// Dozens of greps over /proc/mounts, several `su` runs, and write probes.
    pub const ROOTCHECK: u32 = 120;
    /// Validates and assigns one framebuffer plus the built-in evdev nodes.
    pub const SEAT: u32 = 30;
    /// `td-netd up` is DHCP with bounded retries (2s reads); under the nettest token it
    /// also resolves (3s x 3) and reaches (5s connect) a real host.
    pub const NETUP: u32 = 300;
    /// The script's own retry loop is clamped to BOOT_SUCCESS_RETRY_MAX_SECS iterations,
    /// but each runs a large probe farm (seven `su` blocks) and can run a transactional
    /// `td-boot install`, so an iteration is worth seconds on a slow disk, not one.
    pub const BOOTSUCCESS: u32 = 600;
    /// The park handshake: a grep and a 1s sleep, clamped to BOOT_FAIL_PARK_WAIT_SECS.
    pub const BOOTFAIL: u32 = 300;
}

/// `/etc/td-svc.conf` — the boot's ordering contract, as a graph rather than as line
/// order in the inittab.
///
/// This is the cutover's substance. The same jobs run in the same order for the same
/// reasons; what changes is that the reasons are now DECLARED and machine-checkable.
/// `td-svc check` runs over this table at image-build time (see shape_check), so an
/// ordering regression reds the build instead of the boot — which the inittab could
/// never do, because "sysinit runs in table order" makes every line's position
/// load-bearing and nothing can tell a deliberate order from an accidental one.
///
/// Ordering, and why each edge exists:
///   hostname precedes td-firstboot only to keep the sysinit chain SERIAL. init ran
///     sysinit lines one at a time; td-svc starts every unit whose edges are settled in
///     the same pass, so an absent edge here is not "no constraint", it is CONCURRENCY
///     the inittab never had. Nothing actually reads across the two.
///   td-firstboot mints the per-machine identity, so it precedes everything that reads
///     or checks it — rootcheck (asserts it is READABLE through the MUTABLE_ETC
///     symlinks on a still-read-only /etc), sshd (--host-key, --authorized-keys).
///   rootcheck precedes netup: the read-only-root self-check before networking.
///   netup brings the link up — loopback 127.0.0.1/8, so sshd's own loopback bind and
///     the boot self-test route work, plus any NIC.
///
/// Everything that was `::once:` or `::respawn:` names the WHOLE sysinit set, because
/// that is exactly what init gave it: those jobs started only once every sysinit job
/// had run to completion. Naming the set rather than just its last member keeps that
/// true if the chain is ever reordered.
///
/// Existing boot jobs keep their ordering-only `after=` edges. The graphical daemon
/// additionally requires the seat assignment: starting an unprivileged compositor
/// without its device capability can only crash-loop. Deployment success strictly
/// requires graphical readiness, so a broken UI cannot mark an update healthy. The
/// serial console has no such dependency and remains available when graphics fail.
fn build_td_svc_conf() -> String {
    // The sysinit set, in one place: every post-sysinit unit names all of it.
    let sysinit = "hostname,td-firstboot,rootcheck,netup";
    format!(
        "# Generated by the system-x86-64 recipe. Edit build_td_svc_conf, not this file.\n\
         #\n\
         # PID 1 (/etc/inittab) mounts /dev, /proc and /sys, then respawns td-svc with\n\
         # this table. Ordering lives here as a graph; `td-svc check` validates it at\n\
         # image-build time.\n\
         \n\
         [hostname]\n\
         type=oneshot\n\
         exec=/bin/hostname -F /etc/hostname\n\
         timeout={hostname}\n\
         \n\
         # Mints the per-machine identity everything below reads or checks.\n\
         # after=hostname only to keep sysinit SERIAL, as init ran it: nothing here\n\
         # reads the hostname (this writes /var/lib/td alone), but a cutover that\n\
         # silently made two jobs concurrent would be a behaviour change wearing a\n\
         # migration's clothes. Relaxing this edge is a separate, argued landing.\n\
         [td-firstboot]\n\
         type=oneshot\n\
         exec=/bin/td-firstboot provision\n\
         after=hostname\n\
         timeout={firstboot}\n\
         \n\
         # Asserts the identity is readable through the MUTABLE_ETC symlinks.\n\
         [rootcheck]\n\
         type=oneshot\n\
         exec=/etc/rootcheck\n\
         after=td-firstboot\n\
         timeout={rootcheck}\n\
         \n\
         # The one graphical user owns the fixed framebuffer and evdev seat.\n\
         [seat]\n\
         type=oneshot\n\
         exec=/bin/td-seatd assign --uid {ui_uid} --gid {ui_gid}\n\
         after=rootcheck\n\
         timeout={seat}\n\
         \n\
         # Networking after the read-only-root self-check.\n\
         [netup]\n\
         type=oneshot\n\
         exec=/etc/netup\n\
         after=rootcheck\n\
         timeout={netup}\n\
         \n\
         # No shell-owned device setup: td-seatd assigned the nodes, td-login drops\n\
         # credentials, and the compositor opens only those fixed paths.\n\
         [wayland]\n\
         type=daemon\n\
         exec=/bin/su -s /bin/sh {ui_user} -c '/bin/td-compositor run --framebuffer /dev/fb0 --input /dev/input --socket /run/user/{ui_uid}/wayland-0'\n\
         after=seat\n\
         requires=seat\n\
         ready=/bin/su -s /bin/sh {ui_user} -c '/bin/td-compositor probe /run/user/{ui_uid}/wayland-0'\n\
         ready-timeout=30\n\
         restart=always\n\
         \n\
         # The first td-native client stays mapped. Its readiness probe is exposed\n\
         # only after the configure/ack, wl_shm commit, release, and frame callback.\n\
         [ui-demo]\n\
         type=daemon\n\
         exec=/bin/su -s /bin/sh {ui_user} -c '/bin/td-ui-demo run --socket /run/user/{ui_uid}/wayland-0 --ready-socket /run/user/{ui_uid}/td-ui-demo-ready'\n\
         after=wayland\n\
         requires=wayland\n\
         ready=/bin/su -s /bin/sh {ui_user} -c '/bin/td-ui-demo probe /run/user/{ui_uid}/td-ui-demo-ready'\n\
         ready-timeout=30\n\
         restart=always\n\
         \n\
         [bootsuccess]\n\
         type=oneshot\n\
         exec=/etc/bootsuccess\n\
         after={sysinit},wayland,ui-demo\n\
         requires=ui-demo\n\
         timeout={bootsuccess}\n\
         \n\
         [bootfail]\n\
         type=oneshot\n\
         exec=/etc/bootfail\n\
         after={sysinit}\n\
         timeout={bootfail}\n\
         \n\
         # Binds all interfaces on port 22 (privileged, so root). Naming --host-key is\n\
         # FAIL-CLOSED: with no identity sshd refuses to start rather than fall back to\n\
         # the public committed builtin key, and td-svc's backoff holds the job rather\n\
         # than scrolling the console.\n\
         [sshd]\n\
         type=daemon\n\
         exec=/bin/sshd serve --listen 0.0.0.0:22 --host-key {host_key} --authorized-keys {authorized_keys}\n\
         after={sysinit}\n\
         restart=always\n\
         # sshd is the one shipped service that talks to the network, so its\n\
         # output is what a failed login has to be reconstructed from. Rotated\n\
         # by td-svc, because /var is a persistent volume this could fill.\n\
         log=/var/log/svc/sshd.log\n\
         # ...and COPIED to the console, because before capture existed sshd\n\
         # inherited td-svc's stderr and its failures reached the serial line.\n\
         # The boot oracle prints \"Last serial output\" when sshd's marker is\n\
         # missing; capturing alone would take sshd's reason out of exactly the\n\
         # text that gets printed when sshd is why the boot failed.\n\
         console=yes\n\
         \n\
         # The auto-login greeter. tty= hands it /dev/ttyS0 and, per td-svc/DESIGN.md,\n\
         # exempts it from process_group(0) so getty's setsid() succeeds — grouping it\n\
         # would yield a console with no controlling terminal on every boot.\n\
         [greeter]\n\
         type=daemon\n\
         exec=/etc/tty-session\n\
         after={sysinit}\n\
         tty=ttyS0\n\
         restart=always\n",
        hostname = svc_timeouts::HOSTNAME,
        firstboot = svc_timeouts::FIRSTBOOT,
        rootcheck = svc_timeouts::ROOTCHECK,
        seat = svc_timeouts::SEAT,
        netup = svc_timeouts::NETUP,
        bootsuccess = svc_timeouts::BOOTSUCCESS,
        bootfail = svc_timeouts::BOOTFAIL,
        ui_user = UI_USER,
        ui_uid = UI_UID,
        ui_gid = UI_GID,
        host_key = SSHD_HOST_KEY,
        authorized_keys = SSHD_AUTHORIZED_KEYS,
    )
}

/// The firmware/direct-boot initramfs always selects through td-boot and kexecs
/// the verified deployment. It has no selected-deployment branch, so an external
/// kernel command line cannot bypass current/previous selection.
fn build_selector_init() -> String {
    "#!/bin/sh\n\
     set -e\n\
     set -f\n\
     /bin/mount -t devtmpfs dev /dev\n\
     /bin/mount -t proc proc /proc\n\
     n=0\n\
     while /bin/td-util test \"$n\" -lt 5 && ! /bin/td-util test -b /dev/vda; do /bin/td-util sleep 1; n=$((n+1)); done\n\
     exec /bin/td-boot boot /dev/vda /volume \"$(/bin/td-util cat /proc/cmdline)\"\n"
        .into()
}

/// The selected deployment initramfs requires exactly one td.deployment handoff,
/// validates that manifest and root payload, and enters the immutable root.
fn build_deployment_init(sys: &SystemDef) -> String {
    // The loop node is created at RUNTIME, not by a `nod` line in the cpio: /dev is
    // devtmpfs from the first line below, which shadows whatever the cpio put at
    // /dev/loop0. So it has to be made in the devtmpfs, after the mount — and it
    // has to be made at all, because the kernel only populates loop0 there when the
    // loop driver registered it, which is a config away from not happening.
    //
    // /dev/vda is one Btrfs filesystem. The top-level vfsmount stays read-only,
    // while the shared Btrfs superblock becomes writable for the @var mount. The
    // mount flag prevents accidental writes, not a privileged remount by root.
    // The verified loop keeps root.erofs open, so the top-level mount cannot be
    // unmounted; move it below the new root's volatile /run instead.
    let mut init = "#!/bin/sh\n\
     set -e\n\
     set -f\n\
     /bin/mount -t devtmpfs dev /dev\n\
     /bin/mount -t proc proc /proc\n\
     /bin/mount -t sysfs sysfs /sys\n\
     n=0\n\
     while /bin/td-util test \"$n\" -lt 5 && ! /bin/td-util test -b /dev/vda; do /bin/td-util sleep 1; n=$((n+1)); done\n\
     deployment=\n\
     deployment_seen=\n\
     for word in $(/bin/td-util cat /proc/cmdline); do\n\
       case \"$word\" in\n\
         td.deployment=*) \
           /bin/td-util test -z \"$deployment_seen\" || { echo 'td-init: duplicate td.deployment handoff' >&2; exit 1; }; \
           deployment_seen=1; deployment=${word#td.deployment=} ;;\n\
       esac\n\
     done\n\
     /bin/td-util test -n \"$deployment\" || { echo 'td-init: missing td.deployment handoff' >&2; exit 1; }\n\
     /bin/mount -t btrfs -o ro,nodev,nosuid,noexec /dev/vda /volume\n\
     if ! /bin/td-util test -b /dev/loop0; then /bin/mknod /dev/loop0 b 7 0; fi\n\
     /bin/td-boot root-loop /volume \"$deployment\" /dev/loop0\n\
     /bin/mount -t erofs -o ro /dev/loop0 /sysroot\n\
     /bin/mount -t btrfs -o rw,nodev,nosuid,subvol=@var /dev/vda /sysroot/var\n\
     /bin/umount /proc\n\
     /bin/umount /dev\n\
     /bin/umount /sys\n\
     /bin/mount -t tmpfs -o mode=0755 tmpfs /sysroot/run\n\
     /bin/td-util printf '%s\\n' \"$deployment\" > /sysroot/run/td-deployment\n\
     /bin/td-util chmod 0600 /sysroot/run/td-deployment\n\
     /bin/td-util mkdir -p /sysroot/run/td-volume\n\
     /bin/mount -o move /volume /sysroot/run/td-volume\n\
     /bin/mount -t tmpfs -o mode=1777 tmpfs /sysroot/tmp\n\
     /bin/td-util mkdir -p /sysroot/var/log /sysroot/var/home"
        .to_string();
    for user in sys.users {
        if user.home != "/root" {
            init.push_str(&format!(" /sysroot/var{}", user.home));
        }
    }
    init.push_str(
        "\n/bin/busybox sh -c 'umask 077; /bin/td-util mkdir -p /sysroot/var/root'\n\
         /bin/td-util rm -rf /sysroot/var/run\n\
         /bin/td-util ln -s /run /sysroot/var/run\n\
         /bin/td-util chown 0:0 /sysroot/var /sysroot/var/log /sysroot/var/home /sysroot/var/root\n\
         /bin/td-util chmod 0755 /sysroot/var /sysroot/var/log /sysroot/var/home\n\
         /bin/td-util chmod 0700 /sysroot/var/root\n\
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
/// It ASKS td-svc rather than tearing the system down itself. The supervisor owns the
/// ordered teardown (stop every unit in reverse dependency order, run `/etc/shutdown`,
/// then exec the power applet); this wrapper is only the decision point, and one word
/// on td-svc's control socket is the whole of that decision. It used to inline
/// `{ /etc/shutdown; exec /bin/reboot; }`, which reset the machine with services still
/// running — fine when nothing was supervised, wrong now that something is.
///
/// The reboot is gated on `getty` SUCCEEDING (`&&`): getty sets up the tty and execs the
/// login chain, returning the user shell's exit status, so a normal `exit`/Ctrl-D returns
/// 0 -> power off. But if getty/login FAILS to start a session at all (e.g. it cannot open
/// ttyS0), getty returns non-zero, the `&&` short-circuits, and the wrapper exits non-zero
/// so init RESPAWNS it — a visible retry loop — rather than firing `reboot` and letting
/// `-no-reboot` mask a broken greeter as a clean exit-0 shutdown (re #541, Codex review).
/// Keeping the `&&` keeps "the greeter never started" and "the user logged out"
/// distinguishable, which is the only thing that makes a retry loop possible.
///
/// `>/dev/console` is for td-svc's OWN diagnostics — a refused or unreachable socket must
/// be visible. By the time this runs the greeter shell (SESSION LEADER of getty's ttyS0
/// session) has exited, so the kernel has vhangup'd that terminal and writes through the
/// inherited descriptor return EIO. Under busybox init the same script needed no redirect;
/// verified by observing exactly that failure — reboot with no output at all — before
/// adding it. The teardown's own output no longer rides this descriptor: td-svc opens the
/// console itself for `/etc/shutdown` (supervise.rs `run_teardown`), so the marker the boot
/// oracle latches survives even if this client is gone by then.
fn build_tty_session() -> String {
    "#!/bin/sh\n\
     /bin/getty -L -n -l /etc/autologin 115200 ttyS0 vt100 && exec /bin/td-svc reboot >/dev/console 2>&1\n"
        .into()
}

fn build_shutdown() -> String {
    // Run by td-svc once every service is down, immediately before it hands off to the
    // power applet. Keep this a strict tripwire, but attempt every safety step after
    // any failure.
    //
    // `--exclude /run` is load-bearing, not tidiness. td-svc records a shutdown in
    // flight at /run/td-svc/shutdown BEFORE it stops anything (DESIGN.md I6), and PID 1
    // respawns td-svc unconditionally — so a supervisor that dies while this script is
    // running has to find that marker and resume to the power applet. `umount -a` would
    // take the /run tmpfs with it, and the replacement would instead see a clean start
    // and bring services up against filesystems this script has already released. The
    // exclusion is EXACT, so the moved btrfs at /run/td-volume is still released; /run
    // itself is tmpfs holding nothing that needs releasing.
    format!(
        "#!/bin/sh\n\
         ok=1\n\
         /bin/td-init sync || {{ echo 'td-shutdown: sync failed' >&2; ok=0; }}\n\
         /bin/umount /var || {{ echo 'td-shutdown: umount /var failed' >&2; ok=0; }}\n\
         /bin/umount -a -r --exclude /run || {{ echo 'td-shutdown: final unmount failed' >&2; ok=0; }}\n\
         /bin/td-util test \"$ok\" = 1 && echo {SYSTEM_SHUTDOWN_MARKER}\n"
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
                "/bin/td-util chown {}:{} {} 2>/dev/null || ok=0\n",
                u.uid, u.gid, u.home
            ));
        }
    }
    if let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) {
        if user.uid != 0 {
            let (name, home) = (user.name, user.home);
            // These probes WRITE rather than ask `test -w`: POSIX `-w` is access(2),
            // and a mode-bits answer would read root's own bit on 0755 `/var` and
            // report success for a system that had failed. Each attempt runs in a
            // CHILD shell because ash exits a non-interactive shell when a special
            // builtin's redirection fails (the /etc probe below documents the same
            // constraint; it spells the shell `/bin/busybox sh` where this spells it
            // `/bin/sh` — the same static binary today, and the spelling that follows
            // `sh` wherever it goes next).
            //
            // Root clears the probe files on BOTH sides, and the pre-clear is
            // load-bearing: a stale root-owned `/var/.tdwr-su` makes the unprivileged
            // write fail with EACCES even where `/var` is world-writable, and that
            // failure would read as a pass. `home` sits inside DOUBLE quotes, so `$`
            // and `"` would be live; `valid_home` admits only `/home/<alnum . _ ->`,
            // which is what makes that safe.
            s.push_str(&format!(
                "/bin/td-util rm -f /var/.tdwr-su /var/root/.tdwr-su {home}/.tdwr-su || ok=0\n\
                 if /bin/su -s /bin/sh {name} -c \
                 '/bin/td-util test -d /var/root || exit 1; \
                 /bin/sh -c \": > /var/.tdwr-su\" 2>/dev/null && exit 1; \
                 /bin/sh -c \": > /var/root/.tdwr-su\" 2>/dev/null && exit 1; \
                 /bin/sh -c \": > {home}/.tdwr-su\" 2>/dev/null || exit 1'; then \
                 echo {SYSTEM_STATE_OWNER_MARKER}; else ok=0; fi\n\
                 /bin/td-util rm -f /var/.tdwr-su /var/root/.tdwr-su {home}/.tdwr-su || ok=0\n"
            ));
        }
    }
    // `/` is a read-only erofs mount (fields: <src> <mnt> <fstype> <opts> …; erofs is
    //     always mounted `ro`, so the options field begins `ro`).
    s.push_str(&format!(
        "if /bin/grep -Eq '^[^ ]+ / erofs ro[, ]' /proc/mounts; then echo {SYSTEM_ROOT_RO_MARKER}; else ok=0; fi\n"
    ));
    // Root runs this check, so a failed /etc write proves the filesystem rejects writes,
    // not merely that file modes deny an unprivileged process. Run the redirection in a
    // child shell: ash exits a non-interactive shell when a special builtin redirection
    // fails, instead of returning control to the parent `if`.
    s.push_str(&format!(
        "if /bin/busybox sh -c ': > /etc/.tdwr' 2>/dev/null; then /bin/td-util rm -f /etc/.tdwr; ok=0; else echo {SYSTEM_ETC_RO_MARKER}; fi\n"
    ));
    // State is the persistent Btrfs @var subvolume; only run/tmp are volatile.
    // Homes remain stable paths through immutable symlinks into /var.
    // The td-volume line needs `ro` as a whole comma-delimited option, not a substring:
    // `errors=remount-ro` and `rootcontext=…` both carry the letters. An awk field test
    // used to spell that; an ERE anchored on the space-delimited fields is equivalent
    // over procfs, which emits exactly six single-space-separated fields per line and
    // escapes any literal blank as `\040` (see `rootcheck_pins_the_td_volume_ere`).
    // ANY matching line satisfies it, exactly as the awk's `found=1` did — so a volume
    // mounted ro and later over-mounted rw still reads healthy here.
    s.push_str(
        "/bin/grep -Eq '^[^ ]+ /var btrfs ' /proc/mounts || ok=0\n\
         /bin/grep -Eq '^[^ ]+ /run/td-volume btrfs ([^ ]*,)?ro(,[^ ]*)?( |$)' \
         /proc/mounts || ok=0\n\
         for d in /run /tmp; do \
         /bin/grep -Eq \"^[^ ]+ $d tmpfs \" /proc/mounts || ok=0; \
         done\n",
    );
    s.push_str(
        "[ \"$(/bin/td-util readlink /home)\" = var/home ] || ok=0\n\
         [ \"$(/bin/td-util readlink /root)\" = var/root ] || ok=0\n\
         [ \"$(/bin/td-util readlink /var/run)\" = /run ] || ok=0\n",
    );
    s.push_str(&build_mutable_etc_check(sys));
    let mut probe_paths = "/var /run /tmp /home /root".to_string();
    for user in sys.users {
        if user.home != "/root" {
            probe_paths.push(' ');
            probe_paths.push_str(user.home);
        }
    }
    s.push_str(&format!(
        "for d in {probe_paths}; do \
         if /bin/busybox sh -c ': > \"$1/.tdwr\"' td-probe \"$d\" 2>/dev/null; then /bin/td-util rm -f \"$d/.tdwr\"; else ok=0; fi; \
         done\n"
    ));
    if let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) {
        s.push_str(&format!(
            "if /bin/grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline; then \
             /bin/td-util printf '%s\\n' waiting > /run/td-boot-parked \
             && /bin/td-util chown {}:{} /run/td-boot-parked \
             && /bin/td-util chmod 0600 /run/td-boot-parked || ok=0; \
             fi\n",
            user.uid, user.gid
        ));
    }
    s.push_str(&format!(
        "if [ \"$ok\" = 1 ]; then \
         echo {SYSTEM_STATE_WRITABLE_MARKER}; \
         /bin/td-util printf '%s\\n' td-rootcheck-v1 > /run/td-rootcheck-ok \
         && /bin/td-util chmod 0600 /run/td-rootcheck-ok; \
         fi\n"
    ));
    s.push_str(&format!(
        "if /bin/grep -q -F '{PERSIST_WRITE_CMDLINE_TOKEN}' /proc/cmdline; then \
         if /bin/td-util test ! -e /var/lib/td/boot-marker \
         && /bin/td-util mkdir -p /var/lib/td \
         && /bin/td-util printf '%s\\n' td-persistent-v1 > /var/lib/td/boot-marker \
         && /bin/td-init sync; then \
         echo {SYSTEM_PERSIST_WRITE_MARKER}; fi; \
         fi\n\
         if /bin/grep -q -F '{PERSIST_READ_CMDLINE_TOKEN}' /proc/cmdline \
         && /bin/td-util test \"$(/bin/td-util cat /var/lib/td/boot-marker 2>/dev/null)\" = td-persistent-v1; then \
         echo {SYSTEM_PERSIST_READ_MARKER}; \
         fi\n"
    ));
    s
}

/// The mutable-`/etc` contract, checked on the RUNNING system rather than only in the
/// staged image — the half of this design that a build check cannot reach.
///
/// Emitted from `/etc/rootcheck`, which runs at sysinit right AFTER `td-firstboot`, so
/// the persistent targets exist by now while the volatile ones do not (td-netd writes
/// those later in the same sequence). Its own flag, not `rootcheck`'s `ok`: folding it
/// in would make one firstboot failure withhold the unrelated state-writable marker and
/// send whoever reads the console after the wrong component (the lesson
/// `build_bootsuccess` records).
///
/// The private-key leg is a BEHAVIOURAL mode check rather than a `stat` comparison: the
/// unprivileged login user must be unable to read the host key and able to read its
/// `.pub`. That is the property that actually matters, and busybox ships no `stat`.
fn build_mutable_etc_check(sys: &SystemDef) -> String {
    let mut s = String::from("me=1\n");
    for entry in MUTABLE_ETC {
        // Every entry: the symlink must say exactly what the table records. A
        // symlink that moved is a file whose writes land somewhere unreviewed.
        s.push_str(&format!(
            "[ \"$(/bin/td-util readlink /etc/{})\" = {} ] || me=0\n",
            entry.etc, entry.target
        ));
        // Persistent entries only: td-firstboot has already run, so these must
        // RESOLVE — which is the proof that a read through the read-only /etc
        // reaches writable /var.
        if entry.state == State::Persistent {
            s.push_str(&format!(
                "/bin/td-util test -f /etc/{} || me=0\n",
                entry.etc
            ));
        }
    }
    // The id must be the shape every reader expects, read back THROUGH /etc.
    s.push_str(
        "/bin/grep -Eq '^[0-9a-f]{32}$' /etc/machine-id || me=0\n",
    );
    if let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) {
        if user.uid != 0 {
            // ONE su, and the marker requires it to SUCCEED. Splitting this into a
            // negative probe (`if su …; then me=0; fi`) would fail OPEN: `su` itself
            // failing for any unrelated reason would look exactly like a private key
            // that is correctly unreadable, and pass. Here a broken `su` is a
            // non-zero exit and withholds the marker, while a passing run has proved
            // both halves — the private key is NOT readable and the `.pub` IS.
            s.push_str(&format!(
                "if /bin/su -s /bin/sh {name} -c \
                 'if /bin/td-util cat {key} >/dev/null 2>&1; then exit 1; fi; \
                 /bin/td-util cat {key}.pub >/dev/null 2>&1'; then :; else me=0; fi\n",
                name = user.name,
                key = SSHD_HOST_KEY,
            ));
        }
    }
    s.push_str(&format!(
        "if [ \"$me\" = 1 ]; then echo {SYSTEM_ETC_MUTABLE_MARKER}; fi\n"
    ));
    s
}

/// The supplementary gids the SHIPPED `/etc/group` grants `user`, derived by reading the
/// generated file the way td-login will read it at boot rather than by restating the gids
/// here. The health probe below compares against this, so a `wheel` that changed gid — or a
/// membership the generator stopped emitting — moves both sides together instead of turning
/// the credential assertion into a false failure (or, worse, a vacuous pass).
///
/// A user's PRIMARY group is deliberately not in this list: `build_group` writes those lines
/// with an empty member field, so td-login's `/etc/group` walk does not see them either, and
/// `Credentials::new` is the one place that folds the primary gid into the set.
fn supplementary_gids(sys: &SystemDef, user: &str) -> Vec<u32> {
    let mut gids = Vec::new();
    for line in build_group(sys).lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let (Some(gid), Some(members)) = (fields.get(2), fields.get(3)) else {
            continue;
        };
        let Ok(gid) = gid.parse::<u32>() else {
            continue;
        };
        if members.split(',').any(|m| !m.is_empty() && m == user) {
            gids.push(gid);
        }
    }
    gids.sort_unstable();
    gids.dedup();
    gids
}

/// The td-login leg of the health target: run THROUGH `/bin/su` (which is td-login) and have
/// the switched process read its own credentials back out of `/proc/self/status`.
///
/// This is the one failure the rest of the image cannot see. Every other unprivileged leg
/// already goes through `su`, so a td-login that fails to start a session reds them all — but
/// a `setuid(2)` issued before `setgroups(2)` starts a perfectly working session that has
/// silently kept root's supplementary groups, and every marker still prints. So this asserts
/// the RESULT: all four uid columns, all four gid columns, and the exact supplementary set.
///
/// Double quotes only, never single: this is pasted inside the health target's single-quoted
/// `su -c '…'` argument, where one `'` would end it and hand the rest to the wrong shell.
fn td_login_probe(sys: &SystemDef) -> String {
    let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) else {
        // Fail CLOSED. `system_def_is_self_consistent` makes this unreachable, but an
        // EMPTY probe body is a `su -c ''` that exits 0, so the marker would print
        // unconditionally and the oracle would green a switch nothing checked. A
        // vacuous pass is worse than the build error it replaces.
        return "echo \"td-login: no autologin user to verify credentials for\"; false".into();
    };
    let groups: Vec<String> = supplementary_gids(sys, user.name)
        .iter()
        .map(|gid| gid.to_string())
        .collect();
    format!(
        "l=1; /bin/td-login --list >/dev/null 2>&1 || \
         {{ echo \"td-login: --list failed\"; l=0; }}; \
         /bin/td-login verify-credentials --uid {uid} --gid {gid} --groups \"{groups}\" || \
         {{ echo \"td-login: the su credential switch did not produce uid {uid} gid {gid} \
         groups [{groups}]\"; l=0; }}; [ \"$l\" = 1 ]",
        uid = user.uid,
        gid = user.gid,
        groups = groups.join(",")
    )
}

/// Each component marker is emitted by its OWN leg, not from one block gated on every leg
/// passing. Emitting them together meant any single failure withheld every marker, so the oracle
/// reported whichever it checked first — uutils — and the diagnostics the other farms went to
/// trouble to produce named a component nobody would look at. The `m*` flags keep a retry from
/// reprinting a marker it already earned. `SYSTEM_BOOT_SUCCESS_MARKER` stays gated on the whole
/// transaction, which is what it means.
/// The td-txt farm's greeter probes. Unlike td-util's `/bin/<applet> >/dev/null` loop these
/// assert an ANSWER, not an exit status, because the failure that matters for a text tool is
/// a wrong answer rather than a crash — a `grep -Eq` that selected the wrong line would let
/// `/etc/rootcheck` call a writable root read-only, in silence.
///
/// - `grep` runs rootcheck's own `-Eq` over the LIVE `/proc/mounts`, both ways: it must select
///   the read-only erofs root and must NOT select the `rw` spelling of the same line. Two
///   runs, so a grep that matched everything (or nothing) fails one of them. Reading
///   `/proc/mounts` at all is the other half — procfs files stat as zero-length, so a reader
///   that trusted `st_size` would see an empty file and quietly agree with whichever test
///   asked for "no match".
/// - `sed` has no boot-path duty, so it is exercised on its own, over `/etc/os-release`:
///   an `s///p` that must print `td`, and a `2p` that must print `ID=td`. Both compare
///   against a known value rather than just checking the exit status, for the same reason.
///
/// Every probe is unprivileged and read-only — no state to restore, unlike td-init's farm
/// where the reversible/irreversible split forced probing by refusal.
fn build_td_txt_probes() -> String {
    // NO SINGLE QUOTE may appear below: this whole string is interpolated INSIDE the
    // greeter's `su -s /bin/sh USER -c '…'` argument, so one would close that argument
    // and scatter the rest of the probe into the outer shell as stray words — which
    // still PARSES, so `sh -n` would not catch it. Double quotes are literal there, and
    // none of these patterns contains `$`, a backtick or a backslash.
    // `td_txt_probes_survive_the_greeters_quoting` asserts the rule.
    let mut p = String::from("t=1; ");
    p.push_str(
        "/bin/grep -Eq \"^[^ ]+ / erofs ro[, ]\" /proc/mounts || \
         { echo \"td-txt: /bin/grep did not select the read-only root in /proc/mounts\"; \
         t=0; }; \
         if /bin/grep -Eq \"^[^ ]+ / erofs rw[, ]\" /proc/mounts; then \
         echo \"td-txt: /bin/grep selected a rw root line that is not there\"; t=0; fi; ",
    );
    p.push_str(
        "s=$(/bin/sed -n \"s/^ID=//p\" /etc/os-release) || \
         { echo \"td-txt: /bin/sed substitution failed\"; t=0; }; \
         [ \"$s\" = td ] || { echo \"td-txt: /bin/sed printed $s, want td\"; t=0; }; \
         n=$(/bin/sed -n \"2p\" /etc/os-release) || \
         { echo \"td-txt: /bin/sed line select failed\"; t=0; }; \
         [ \"$n\" = \"ID=td\" ] || \
         { echo \"td-txt: /bin/sed printed $n for line 2, want ID=td\"; t=0; }; ",
    );
    p
}

/// What each td-util health probe is given to work on, if anything.
///
/// Shared with the test that asserts the generated script, which would otherwise
/// restate it — and a restated mapping is one that stays green while the script it
/// describes changes underneath it.
///
/// `which` needs a name to resolve. `less`'s operand is not cosmetic: with none it
/// reads STDIN, and the only reason that does not hang the whole boot today is that
/// td-svc hands a unit with no `tty=` a null stdin — a fact stated in supervise.rs,
/// nowhere near here, and true of `[bootsuccess]` only for as long as nobody gives
/// it a tty. An operand ties the probe to nothing but itself.
fn td_util_probe_args(applet: &str) -> &'static str {
    match applet {
        "which" => " sh",
        "less" => " /etc/os-release",
        _ => "",
    }
}

fn build_bootsuccess(sys: &SystemDef) -> String {
    let mut uutils_behavior_probes = String::new();
    for probe in UUTILS_BEHAVIOR_PROBES {
        uutils_behavior_probes.push_str(&uutils_behavior_probe(probe));
    }
    // These run under `su` as the unprivileged user, so the dmesg leg needs an unprivileged
    // /dev/kmsg read — which linux-x86-64 guarantees by pinning CONFIG_SECURITY_DMESG_RESTRICT
    // off. Drop dmesg from the farm and that pin is orphaned.
    let mut td_util_probes = String::new();
    for applet in TD_UTIL_APPLETS {
        let args = td_util_probe_args(applet);
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
    let td_login_probe = td_login_probe(sys);
    let td_txt_probes = build_td_txt_probes();
    format!(
        "#!/bin/sh\n\
         set -f\n\
         finish() {{ /bin/td-util printf '%s\\n' \"$1\" > /run/td-boot-success-ok; \
         /bin/td-util chmod 0644 /run/td-boot-success-ok; }}\n\
         fail() {{ finish td-boot-failure-v1; exit 1; }}\n\
         /bin/grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline && exit 0\n\
         /bin/td-util test \"$(/bin/td-util cat /run/td-rootcheck-ok 2>/dev/null)\" = td-rootcheck-v1 || fail\n\
         deployment=$(/bin/td-util cat /run/td-deployment 2>/dev/null)\n\
         /bin/td-util test -n \"$deployment\" || fail\n\
         wait={BOOT_SUCCESS_RETRY_SECS}\n\
         for token in $(/bin/td-util cat /proc/cmdline); do \
         case \"$token\" in {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done\n\
         case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_SUCCESS_RETRY_SECS};; esac\n\
         [ \"$wait\" -gt {BOOT_SUCCESS_RETRY_MAX_SECS} ] && wait={BOOT_SUCCESS_RETRY_MAX_SECS}\n\
         n=0\n\
         mu=0; mrf=0; ms=0; mtu=0; mti=0; mtl=0; mtt=0\n\
         while [ \"$n\" -lt \"$wait\" ]; do \
         healthy=1; \
         if /bin/su -s /bin/sh {} -c \
         'u=1; h=0; /bin/cat /etc/os-release >/dev/null 2>&1 || \
         {{ echo \"uutils: /bin/cat failed\"; u=0; }}; \
         /bin/rm -rf /tmp/td-uutils-probe; \
         /bin/mkdir /tmp/td-uutils-probe || \
         {{ echo \"uutils: /bin/mkdir could not create probe directory\"; u=0; }}; \
         {uutils_behavior_probes}/bin/rm -rf /tmp/td-uutils-probe || \
         {{ echo \"uutils: /bin/rm could not remove probe directory\"; u=0; }}; \
         [ \"$u\" = 1 ]'; then \
         [ \"$mu\" = 1 ] || {{ echo {UUTILS_RUNTIME_MARKER}; mu=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         'r=$(/bin/rg --color never --no-filename --fixed-strings --line-regexp -- \
         {hostname} /etc/hostname) || \
         {{ echo \"ripgrep: /bin/rg failed\"; exit 1; }}; \
         [ \"$r\" = {hostname} ] || \
         {{ echo \"ripgrep: unexpected hostname result: $r\"; exit 1; }}; \
         f=$(/bin/fd --color never --absolute-path --max-depth 1 ^hostname$ /etc) || \
         {{ echo \"fd: /bin/fd failed\"; exit 1; }}; \
         [ \"$f\" = /etc/hostname ] || {{ echo \"fd: unexpected hostname path: $f\"; exit 1; }}'; then \
         [ \"$mrf\" = 1 ] || {{ echo {RIPGREP_FD_RUNTIME_MARKER}; mrf=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         '/bin/sshd selftest >/dev/null 2>&1'; then \
         [ \"$ms\" = 1 ] || {{ echo {SSHD_MARKER}; ms=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         'u=1; /bin/td-util --list >/dev/null 2>&1 || \
         {{ echo \"td-util: --list failed\"; u=0; }}; \
         {td_util_probes}[ \"$u\" = 1 ]'; then \
         [ \"$mtu\" = 1 ] || {{ echo {TD_UTIL_RUNTIME_MARKER}; mtu=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         'i=1; /bin/td-init --list >/dev/null 2>&1 || \
         {{ echo \"td-init: --list failed\"; i=0; }}; \
         {td_init_probes}[ \"$i\" = 1 ]'; then \
         [ \"$mti\" = 1 ] || {{ echo {TD_INIT_RUNTIME_MARKER}; mti=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         '{td_login_probe}'; then \
         [ \"$mtl\" = 1 ] || {{ echo {TD_LOGIN_RUNTIME_MARKER}; mtl=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         '{td_txt_probes}[ \"$t\" = 1 ]'; then \
         [ \"$mtt\" = 1 ] || {{ echo {TD_TXT_RUNTIME_MARKER}; mtt=1; }}; else healthy=0; fi; \
         if [ \"$healthy\" = 1 ] \
         && /bin/td-boot success /dev/vda /run/td-update \"$deployment\" >/run/td-success-id; then \
         if /bin/grep -q -F '{DEPLOY_INSTALL_CMDLINE_TOKEN}' /proc/cmdline; then \
         if /bin/td-boot install /dev/vda /run/td-update \
         /run/td-volume/td/incoming/candidate >/run/td-installed-id; then \
         echo {SYSTEM_DEPLOY_INSTALL_MARKER}; else healthy=0; fi; \
         fi; \
         if [ \"$healthy\" = 1 ]; then \
         echo {SYSTEM_BOOT_SUCCESS_MARKER}; \
         finish td-boot-success-v1; exit 0; fi; \
         fi; \
         n=$((n+1)); /bin/td-util sleep 1; \
         done\n\
         fail\n",
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        hostname = sys.hostname
    )
}

/// The failed-candidate watchdog, and the SECOND thing on this image that decides to reboot.
/// Like the greeter wrapper it asks td-svc rather than resetting the machine itself, so the
/// ordered teardown happens once, in one place; `reboots_run_the_teardown_first` holds that
/// invariant over all generated scripts.
fn build_bootfail() -> String {
    format!(
        "#!/bin/sh\n\
         set -f\n\
         /bin/grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline || exit 0\n\
         /bin/td-util test \"$(/bin/td-util cat /run/td-rootcheck-ok 2>/dev/null)\" = td-rootcheck-v1 || exit 1\n\
         wait={BOOT_FAIL_PARK_WAIT_SECS}\n\
         for token in $(/bin/td-util cat /proc/cmdline); do \
         case \"$token\" in {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done\n\
         case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_FAIL_PARK_WAIT_SECS};; esac\n\
         [ \"$wait\" -gt {BOOT_FAIL_PARK_WAIT_SECS} ] && wait={BOOT_FAIL_PARK_WAIT_SECS}\n\
         n=0\n\
         while [ \"$n\" -lt \"$wait\" ]; do \
         /bin/grep -q -x '{BOOT_FAIL_PARKED}' /run/td-boot-parked 2>/dev/null && \
         exec /bin/td-svc reboot >/dev/console 2>&1; \
         n=$((n+1)); /bin/td-util sleep 1; \
         done\n\
         echo 'td-boot: greeter park handshake timed out' >&2\n\
         exit 1\n"
    )
}

fn build_profile(sys: &SystemDef) -> String {
    // The login shell (busybox ash, invoked as `-sh` by td-login) sources this. We print
    // the banner HERE via a literal here-doc rather than leaning on a `login` motd feature
    // — td-login has none, by design: printing files at a console is not the job of the
    // program that hands out credentials — and set a sane PATH/PS1.
    let mut s = String::new();
    // Just /bin — the store-native symlink farm. There is no /usr or /sbin in this image
    // (every /bin entry resolves into /td/store), so keep PATH honest and minimal.
    s.push_str("export PATH=/bin\n");
    s.push_str(&format!("export XDG_RUNTIME_DIR=/run/user/{UI_UID}\n"));
    s.push_str("export WAYLAND_DISPLAY=wayland-0\n");
    s.push_str("export PS1='\\u@\\h:\\w\\$ '\n");
    s.push_str(&format!(
        "if /bin/grep -q -F '{BOOT_FAIL_TARGET_CMDLINE_TOKEN}' /proc/cmdline; then \
         exec /bin/busybox sh -c 'cd / \
         || exit 1; \
         if ! /bin/td-util printf \"%s\\n\" {BOOT_FAIL_PARKED} > /run/td-boot-parked; then \
         echo \"td-boot: could not park greeter\" >&2; fi; \
         while :; do /bin/td-util sleep 300; done'; fi\n"
    ));
    // The terminal hand-over, checked where it actually happened. `login` chowns the
    // session's terminal to the user and chmods it 0600, and a FAILED hand-over is
    // deliberately not fatal (see td-login/THREAT-MODEL.md section 6) — the shell
    // already holds the descriptor, so the session works and every marker still
    // prints. That is exactly why it needs saying out loud somewhere.
    //
    // It lives HERE, in the login session, rather than in /etc/bootsuccess: the health
    // target runs as root through `su`, whose terminal is not this one, and making it
    // wait for a greeter that may not have started yet would couple the deployment
    // transaction to a session with its own timing. A console diagnostic in the one
    // process that can see the answer is the honest trade; `greeter_checks_the_login_
    // terminal_was_handed_over` asserts this is here.
    //
    // A subshell, because `set -f` in a sourced profile would persist into the
    // operator's interactive shell. `ls -ln` fields: mode, links, uid, gid.
    if let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) {
        s.push_str(&format!(
            "(t=$(/bin/tty 2>/dev/null); [ -n \"$t\" ] || exit 0; set -f; \
             set -- $(/bin/ls -ln \"$t\" 2>/dev/null); \
             [ \"$1\" = crw------- ] && [ \"$2\" = 1 ] && [ \"$3\" = {uid} ] && [ \"$4\" = {gid} ] || \
             echo \"td-login: login terminal $t is not 0600 {uid}:{gid} (got $1 $3:$4)\")\n",
            uid = user.uid,
            gid = user.gid
        ));
    }
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
        "if /bin/grep -q -F '{AUTOTEST_CMDLINE_TOKEN}' /proc/cmdline 2>/dev/null; then \
         set -f; wait=0; for token in $(/bin/td-util cat /proc/cmdline); do \
         case \"$token\" in {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done; \
         case \"$wait\" in ''|*[!0-9]*|0) wait=1;; esac; \
         n=0; while [ \"$n\" -lt \"$wait\" ]; do \
         status=$(/bin/td-util cat /run/td-boot-success-ok 2>/dev/null); \
         [ \"$status\" = td-boot-success-v1 ] && break; \
         [ \"$status\" = td-boot-failure-v1 ] && break; \
         n=$((n+1)); /bin/td-util sleep 1; done; \
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
         if /bin/grep -q -F '{NETTEST_CMDLINE_TOKEN}' /proc/cmdline 2>/dev/null; then \
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

/// Whether a mutable `/etc` file's real bytes have to survive a reboot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Rebuilt from nothing every boot by its owner, so the target lives on the
    /// `/run` tmpfs and is absent at the start of each boot.
    Volatile,
    /// Per-machine identity on the persistent Btrfs `@var` subvolume. Minted once
    /// by `/bin/td-firstboot` and thereafter never rewritten.
    Persistent,
}

/// One reviewed hole in the immutable `/etc`.
///
/// `/etc` is deployment-owned content inside the read-only erofs image, and
/// `SYSTEM_ETC_RO_MARKER` is the boot-time assertion that it rejects writes even
/// from root. A writable `/etc` overlay would retire that assertion for the WHOLE
/// directory in exchange for mutability in a handful of files. So each mutable file
/// is instead one symlink out of `/etc` into writable state, named here with its
/// owner and the reason it cannot be image content — and this table is the only way
/// to add another: the staging steps and `shape_check` are both generated from it,
/// and the shape check additionally proves that no OTHER entry under `/etc` is a
/// symlink, because the invariant is only as strong as the list of holes in it.
///
/// `/home` and `/root` are NOT here: those are whole directories rather than
/// config, and they are staged separately (see `real_root_steps`).
struct MutableEtc {
    /// Path under `/etc` — the stable name every reader uses.
    etc: &'static str,
    /// Absolute symlink target: under `/run` for `Volatile`, under `/var/lib/td`
    /// (td-firstboot's state dir) for `Persistent`.
    target: &'static str,
    state: State,
    /// Who writes the target, and why this cannot be image content. The reason has
    /// to be that the content differs PER MACHINE or PER BOOT; anything that can be
    /// identical everywhere belongs in the image, where it is immutable.
    why: &'static str,
}

/// td-firstboot's state directory, which every `Persistent` target above is under.
/// Test-only because the targets are `const` literals (there is no const `format!`)
/// — its whole job is to be the thing
/// `firstboot_and_the_mutable_etc_table_agree_on_every_path` checks the table and
/// that crate's own source against.
#[cfg(test)]
const STATE_DIR: &str = "/var/lib/td";

const MUTABLE_ETC: &[MutableEtc] = &[
    MutableEtc {
        etc: "resolv.conf",
        target: "/run/resolv.conf",
        state: State::Volatile,
        why: "td-netd writes this boot's DHCP-supplied nameservers",
    },
    MutableEtc {
        etc: "hosts",
        target: "/run/hosts",
        state: State::Volatile,
        why: "td-netd writes this boot's own address mapping",
    },
    MutableEtc {
        etc: "machine-id",
        target: "/var/lib/td/machine-id",
        state: State::Persistent,
        why: "one image boots many machines; a baked id would make them all the same machine",
    },
    MutableEtc {
        etc: "ssh/ssh_host_ed25519_key",
        target: "/var/lib/td/ssh/ssh_host_ed25519_key",
        state: State::Persistent,
        why: "a baked host key is a host identity every holder of the image can impersonate",
    },
    MutableEtc {
        etc: "ssh/ssh_host_ed25519_key.pub",
        target: "/var/lib/td/ssh/ssh_host_ed25519_key.pub",
        state: State::Persistent,
        why: "the fingerprint an operator pins, re-derived from the per-machine private key",
    },
    MutableEtc {
        etc: "ssh/authorized_keys",
        target: "/var/lib/td/ssh/authorized_keys",
        state: State::Persistent,
        why: "granting admin access is a per-machine act; an image-baked file would grant it \
              on every machine that boots the image, and only a rebuild could revoke it",
    },
];

/// The two `/etc` paths the sshd service line names. Both are `MUTABLE_ETC`
/// entries — `the_sshd_service_reads_only_mutable_etc_paths` proves it — so the
/// daemon presents a per-machine host identity and authorizes from per-machine
/// state.
const SSHD_HOST_KEY: &str = "/etc/ssh/ssh_host_ed25519_key";
const SSHD_AUTHORIZED_KEYS: &str = "/etc/ssh/authorized_keys";

/// Directories that must exist under `/etc` to hold the table's symlinks, and the
/// globs `shape_check` sweeps for symlinks that are not in the table. Derived from
/// the table so a new entry in a new subdirectory extends both.
fn mutable_etc_dirs() -> Vec<&'static str> {
    let mut dirs = Vec::new();
    for entry in MUTABLE_ETC {
        if let Some((dir, _)) = entry.etc.rsplit_once('/') {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// `/etc/mutable-state` — the reviewed list of every `/etc` path that is NOT
/// immutable image content, written into the image as an ordinary (immutable) file.
///
/// The table above already decides the symlinks, so shipping it as text costs one
/// small file and answers, ON the machine, the question the design provokes: why is
/// this one file not immutable like the rest of `/etc`, and where do its writes go?
/// `shape_check` asserts every table entry appears here, so the answer cannot go
/// stale.
fn build_mutable_state() -> String {
    let mut s = String::from(
        "# Every /etc path that is NOT immutable image content.\n\
         #\n\
         # /etc is a read-only erofs directory (proved on each boot by /etc/rootcheck)\n\
         # and there is deliberately NO /etc overlay: each line below is one reviewed\n\
         # symlink out of it, so the set of mutable files is a fixed, auditable list\n\
         # rather than a whole writable directory.\n\
         #\n\
         # volatile  = /run tmpfs, rebuilt every boot by td-netd\n\
         # persistent = /var Btrfs subvolume, minted once per machine by td-firstboot\n\
         #\n\
         # <path>  <state>  <target>\n\
         #     why it cannot be image content\n",
    );
    for entry in MUTABLE_ETC {
        let state = match entry.state {
            State::Volatile => "volatile",
            State::Persistent => "persistent",
        };
        s.push_str(&format!(
            "{}  {state}  {}\n    {}\n",
            entry.etc, entry.target, entry.why
        ));
    }
    s
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
        ("mutable-state", build_mutable_state(), false),
        ("inittab", build_inittab(), false),
        (td_svc_conf_etc_name(), build_td_svc_conf(), false),
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
/// DISJOINT capabilities, and each one's absence from the other phase is asserted: the
/// selector kexecs (`/bin/td-kexec`) and never pivots, the deployment phase pivots
/// (`/bin/switch_root`) and never kexecs. A phase that carried both would let an initramfs
/// do something its /init has no branch for.
///
/// The line is drawn at the /bin NAME, not at the binary: since the `mount(2)` amendment
/// BOTH phases pack td-init, because both mount devtmpfs and proc before doing anything
/// else. Only the deployment phase links `switch_root` and `losetup` to it — the pivot
/// and the loop bind are the capabilities the selector must not have — and the same test
/// that pins those pins the selector's `bin/switch_root` and `bin/losetup` absent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Selector,
    Deployment,
}

/// A gen_init_cpio spec for one of the two structurally distinct boot phases.
fn build_initramfs_spec(init: &str, phase: Phase) -> String {
    let mut s = String::new();
    // /sys is here for one reason: td-init's `losetup` reads
    // /sys/dev/block/<major>:<minor>/ro back to confirm the kernel really made
    // the root loop read-only. Without it the attach cannot be checked, and an
    // unchecked attach is a writable loop over a verified root. Only the
    // deployment /init mounts anything on it — the selector binds no loop — but
    // the directory list is shared, as it already is for /sysroot.
    for d in ["/dev", "/proc", "/run", "/sys", "/sysroot", "/td", "/td/store"] {
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
    // td-init, in BOTH phases: each /init mounts devtmpfs and proc as its first act, and
    // td-boot mounts the Btrfs volume through the same `/bin/mount`. td-init is a static
    // ET_EXEC with an empty runtime closure, which is what lets it run here at all — nothing
    // has mounted the real root yet, so a dynamically-linked mount would be a kernel panic
    // rather than a degraded boot.
    s.push_str("dir {in:td-init} 0755 0 0\n");
    s.push_str("dir {in:td-init}/bin 0755 0 0\n");
    s.push_str("file {in:td-init}/bin/td-init {in:td-init}/bin/td-init 0755 0 0\n");
    s.push_str("slink /bin/mount {in:td-init}/bin/td-init 0777 0 0\n");
    s.push_str("slink /bin/umount {in:td-init}/bin/td-init 0777 0 0\n");
    // td-util, for the same reason td-init is here: /init needs cat, printf, mkdir,
    // chmod, chown, ln, rm and sleep BEFORE the pivot, and uutils serves every one
    // of those names dynamically — against a closure no loader has been mounted for
    // yet. It is reached as `td-util <applet>`, not through /bin/<applet>, because
    // uutils owns those names on the real root and the farms are disjoint.
    s.push_str("dir {in:td-util} 0755 0 0\n");
    s.push_str("dir {in:td-util}/bin 0755 0 0\n");
    s.push_str("file {in:td-util}/bin/td-util {in:td-util}/bin/td-util 0755 0 0\n");
    s.push_str("slink /bin/td-util {in:td-util}/bin/td-util 0777 0 0\n");
    match phase {
        Phase::Selector => {
            s.push_str("dir {in:td-kexec} 0755 0 0\n");
            s.push_str("dir {in:td-kexec}/bin 0755 0 0\n");
            s.push_str("file {in:td-kexec}/bin/td-kexec {in:td-kexec}/bin/td-kexec 0755 0 0\n");
            s.push_str("slink /bin/td-kexec {in:td-kexec}/bin/td-kexec 0777 0 0\n");
        }
        // The pivot applet and the loop applet, and ONLY here. /init execs
        // `/bin/switch_root`, and the selector has no branch that enters a root.
        // `losetup` is the same shape of decision: only `td-boot root-loop` binds
        // the verified image, and only the deployment /init runs it — the selector
        // runs `td-boot boot`, which kexecs. Carrying either name there would give
        // an initramfs a capability its /init has no branch for.
        Phase::Deployment => {
            s.push_str("slink /bin/switch_root {in:td-init}/bin/td-init 0777 0 0\n");
            s.push_str("slink /bin/losetup {in:td-init}/bin/td-init 0777 0 0\n");
            // `mknod` joins them for the same reason: only this /init creates
            // /dev/loop0, because only this one has a loop to bind.
            s.push_str("slink /bin/mknod {in:td-init}/bin/td-init 0777 0 0\n");
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
    // td-txt is static too, and its empty closure matters for the same reason td-util's
    // does: /etc/rootcheck greps /proc/mounts to decide whether the root came up correctly,
    // so a text tool that needs a working runtime closure would be unusable exactly when the
    // machine is being asked to prove it has one.
    steps.push(Step::CopyTree {
        from: "{in:td-txt}".into(),
        dest: "{root}/real-root{in:td-txt}".into(),
    });
    // td-init likewise — and here the empty closure is load-bearing rather than merely
    // convenient: this same binary is /init (PID 1) and the initramfs pivot, both of which
    // run where no dynamic loader is reachable.
    steps.push(Step::CopyTree {
        from: "{in:td-init}".into(),
        dest: "{root}/real-root{in:td-init}".into(),
    });
    // td-firstboot is static too, and here the empty closure matters for the same
    // reason it does for td-util: this runs at sysinit on a machine with no identity
    // yet, and a provisioning tool that needs a working closure cannot report why
    // the machine has none.
    steps.push(Step::CopyTree {
        from: "{in:td-firstboot}".into(),
        dest: "{root}/real-root{in:td-firstboot}".into(),
    });
    // td-login the same way, and for the same reason as td-util plus one of its own: a
    // `login` that cannot run without the dynamic closure locks an operator out of the
    // console exactly when the closure is what broke.
    steps.push(Step::CopyTree {
        from: "{in:td-login}".into(),
        dest: "{root}/real-root{in:td-login}".into(),
    });
    // td-svc the same way. Its empty closure is load-bearing like td-init's: PID 1 execs
    // this before anything has established that the dynamic loader works, and it is the
    // process that would otherwise be reporting that it does not.
    steps.push(Step::CopyTree {
        from: "{in:td-svc}".into(),
        dest: "{root}/real-root{in:td-svc}".into(),
    });
    // The software UI is static and owns no dynamic runtime closure.
    steps.push(Step::CopyTree {
        from: "{in:td-seatd}".into(),
        dest: "{root}/real-root{in:td-seatd}".into(),
    });
    steps.push(Step::CopyTree {
        from: "{in:td-compositor}".into(),
        dest: "{root}/real-root{in:td-compositor}".into(),
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
    // /bin/td-firstboot — a single static binary, not a multicall, so it is its own
    // /bin entry. The inittab runs it at sysinit; nothing else invokes it.
    steps.push(Step::Symlink {
        target: "{in:td-firstboot}/bin/td-firstboot".into(),
        link: "{root}/real-root/bin/td-firstboot".into(),
    });
    // /bin/td-svc — the service supervisor, a single static binary. PID 1's ONLY
    // respawn line, so its absence is not a degraded boot but no userland at all: no
    // identity, no network, no sshd, no console. Static with an empty runtime closure
    // for exactly that reason.
    steps.push(Step::Symlink {
        target: "{in:td-svc}/bin/td-svc".into(),
        link: "{root}/real-root/bin/td-svc".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-seatd}/bin/td-seatd".into(),
        link: "{root}/real-root/bin/td-seatd".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-compositor}/bin/td-compositor".into(),
        link: "{root}/real-root/bin/td-compositor".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-compositor}/bin/td-ui-demo".into(),
        link: "{root}/real-root/bin/td-ui-demo".into(),
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
    // /bin/td-txt is the multicall's own entry (`td-txt <applet>`, and `--list`); the loop
    // below is the argv[0] farm `grep` and `sed` resolve through — the two names that left
    // busybox once the conformance corpus covered the image's own invocation shapes.
    steps.push(Step::Symlink {
        target: "{in:td-txt}/bin/td-txt".into(),
        link: "{root}/real-root/bin/td-txt".into(),
    });
    for app in TD_TXT_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:td-txt}/bin/td-txt".into(),
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
    // /bin/td-login is the multicall's own entry (`td-login <applet>`, `--list`, and the
    // `verify-credentials` readback the health target runs); the loop below is the argv[0]
    // farm `login` and `su` resolve through. The probe deliberately gets NO symlink — it is
    // not an applet, and a /bin name no farm list accounts for is a name nothing checks.
    steps.push(Step::Symlink {
        target: "{in:td-login}/bin/td-login".into(),
        link: "{root}/real-root/bin/td-login".into(),
    });
    for app in TD_LOGIN_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:td-login}/bin/td-login".into(),
            link: format!("{{root}}/real-root/bin/{app}"),
        });
    }
    // The mutable /etc: one symlink per reviewed MUTABLE_ETC entry, out of the
    // read-only erofs /etc into writable state. Every one is deliberately DANGLING
    // at build time — the volatile targets are written each boot by td-netd, the
    // persistent ones once per machine by td-firstboot. A dangling symlink is the
    // correct shipped state: it is what makes "this file is per-machine" a property
    // of the image rather than a convention.
    for dir in mutable_etc_dirs() {
        steps.push(Step::MkDir {
            path: format!("{{root}}/real-root/etc/{dir}"),
        });
    }
    for entry in MUTABLE_ETC {
        steps.push(Step::Symlink {
            target: entry.target.into(),
            link: format!("{{root}}/real-root/etc/{}", entry.etc),
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
     for m in init bin/busybox bin/sh bin/td-boot bin/mount bin/umount bin/td-util dev/console proc run volume sysroot; do \
         printf '%s\\n' \"$selector_list\" | grep -q -x -F \"$m\" || { echo \"selector initramfs: cpio member '$m' missing\" >&2; exit 1; }; \
         printf '%s\\n' \"$init_list\" | grep -q -x -F \"$m\" || { echo \"deployment initramfs: cpio member '$m' missing\" >&2; exit 1; }; \
     done; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/td-init$' || { echo 'selector initramfs: td-init store member missing - its /bin/mount and /bin/umount symlinks would dangle and the boot would stop before its first mount' >&2; exit 1; }; \
     printf '%s\\n' \"$selector_list\" | grep -q -x -F bin/td-kexec || { echo 'selector initramfs: td-kexec missing' >&2; exit 1; }; \
     if printf '%s\\n' \"$init_list\" | grep -q -x -F bin/td-kexec; then echo 'deployment initramfs: td-kexec must be selector-only' >&2; exit 1; fi; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/td-boot$' || { echo 'selector initramfs: td-boot store member missing' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-boot$' || { echo 'deployment initramfs: td-boot store member missing' >&2; exit 1; }; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/td-kexec$' || { echo 'selector initramfs: td-kexec store member missing' >&2; exit 1; }; \
     if printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-kexec$'; then echo 'deployment initramfs: td-kexec store member must be selector-only' >&2; exit 1; fi; \
     printf '%s\\n' \"$init_list\" | grep -q -x -F bin/switch_root || { echo 'deployment initramfs: bin/switch_root missing - its /init would exec nothing and the boot would end in a 300s timeout with no cause' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-init$' || { echo 'deployment initramfs: td-init store member missing - the switch_root and mount/umount symlinks would dangle' >&2; exit 1; }; \
     for l in \"$selector_list\" \"$init_list\"; do printf '%s\\n' \"$l\" | grep -qE '^td/store/[^/]+/bin/td-util$' || { echo 'initramfs: td-util store member missing - /bin/td-util would dangle and the /init would stop at its first cat/sleep under set -e, with no cause on the console' >&2; exit 1; }; done; \
     if printf '%s\\n' \"$selector_list\" | grep -q -x -F bin/switch_root; then echo 'selector initramfs: switch_root must be deployment-only - the selector kexecs, it never pivots' >&2; exit 1; fi; \
     printf '%s\\n' \"$init_list\" | grep -q -x -F bin/losetup || { echo 'deployment initramfs: bin/losetup missing - td-boot root-loop could not bind the verified root and the boot would stop there' >&2; exit 1; }; \
     if printf '%s\\n' \"$selector_list\" | grep -q -x -F bin/losetup; then echo 'selector initramfs: losetup must be deployment-only - the selector kexecs, it never binds a root loop' >&2; exit 1; fi; \
     [ \"$(wc -l < \"$selector_manifest\")\" -eq 2 ] || { echo 'selector manifest: expected header plus one payload entry' >&2; exit 1; }; \
     [ \"$(head -n 1 \"$selector_manifest\")\" = td-deployment-v1 ] || { echo 'selector manifest: unsupported header' >&2; exit 1; }; \
     grep -q -E '^[0-9a-f]{64}  selector-initramfs\\.cpio$' \"$selector_manifest\" || { echo 'selector manifest: missing strict SHA-256 entry' >&2; exit 1; }; \
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/busybox$' || { echo 'selector initramfs: busybox store member missing' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/busybox$' || { echo 'deployment initramfs: busybox store member missing' >&2; exit 1; }; \
     [ -f \"$root/init\" ] || [ -L \"$root/init\" ] || { echo 'root tree: /init missing' >&2; exit 1; }; \
     case $(readlink \"$root/init\") in /td/store/*) : ;; *) echo 'root tree: /init is not a symlink into /td/store' >&2; exit 1;; esac; \
     case $(readlink \"$root/bin/sh\") in /td/store/*) : ;; *) echo 'root tree: /bin/sh is not a symlink into /td/store - the store-native /bin farm regressed' >&2; exit 1;; esac; \
     for f in passwd group shadow hostname os-release mutable-state inittab @TD_SVC_CONF_NAME@ profile autologin tty-session shutdown rootcheck netup bootsuccess bootfail; do \
         [ -f \"$root/etc/$f\" ] || { echo \"root tree: /etc/$f missing\" >&2; exit 1; }; \
         if [ -L \"$root/etc/$f\" ]; then echo \"root tree: /etc/$f is a symlink - immutable image config must be a regular file in the erofs, not a hole in the read-only /etc\" >&2; exit 1; fi; \
     done; \
     for pair in @MUTABLE_ETC@; do \
         l=${pair%%=*}; t=${pair#*=}; \
         [ \"$(readlink \"$root/etc/$l\")\" = \"$t\" ] || { echo \"root tree: /etc/$l must be a symlink to $t - it is a reviewed MUTABLE_ETC entry, so its writes must land on the state it names and nowhere else\" >&2; exit 1; }; \
         grep -q -F \"$l  \" \"$root/etc/mutable-state\" || { echo \"root tree: /etc/mutable-state does not document /etc/$l - the shipped list of holes in the read-only /etc must name every one of them\" >&2; exit 1; }; \
     done; \
     ( cd \"$root/etc\" || exit 1; \
       for p in * .*; do \
           { [ -d \"$p\" ] && [ ! -L \"$p\" ]; } || continue; \
           case $p in .|..) continue;; esac; \
           seen=0; for d in @MUTABLE_ETC_DIRS@; do if [ \"$d\" = \"$p\" ]; then seen=1; fi; done; \
           [ \"$seen\" = 1 ] || { echo \"root tree: /etc/$p is a directory no MUTABLE_ETC entry declares, so the symlink sweep below cannot look inside it - add the entry that needs it (or the sweep stops being a proof)\" >&2; exit 1; }; \
       done; \
       n=0; \
       for p in @ETC_GLOBS@; do \
           [ -L \"$p\" ] || continue; \
           case $p in .|..) continue;; esac; \
           n=$((n+1)); \
           seen=0; for a in @MUTABLE_ETC_NAMES@; do if [ \"$a\" = \"$p\" ]; then seen=1; fi; done; \
           [ \"$seen\" = 1 ] || { echo \"root tree: /etc/$p is a symlink out of the immutable /etc but is not a reviewed MUTABLE_ETC entry - the read-only-/etc invariant is only as strong as the list of holes in it\" >&2; exit 1; }; \
       done; \
       [ \"$n\" = @MUTABLE_ETC_COUNT@ ] || { echo \"root tree: found $n symlinks under /etc but MUTABLE_ETC declares @MUTABLE_ETC_COUNT@ - the counts must agree or a hole is unaccounted for in either direction\" >&2; exit 1; }; \
     ) || exit 1; \
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
     [ \"$(readlink \"$root/bin/td-txt\" 2>/dev/null)\" = \"{in:td-txt}/bin/td-txt\" ] || { echo 'root tree: /bin/td-txt is not a symlink to the staged text multicall' >&2; exit 1; }; \
     tdt=\"{root}/real-root{in:td-txt}/bin/td-txt\"; tdttgt=\"{in:td-txt}/bin/td-txt\"; { [ -f \"$tdt\" ] && [ -x \"$tdt\" ]; } || { echo 'root tree: the td-txt binary is not packed/executable at real-root{in:td-txt}/bin/td-txt - the /bin/grep symlink would dangle and /etc/rootcheck would fail every boot' >&2; exit 1; }; \
     tdtlist=$(\"$tdt\" --list 2>/dev/null) || { echo 'td-txt --list failed - cannot verify the text farm' >&2; exit 1; }; \
     for a in @TD_TXT_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tdttgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged td-txt multicall ($tdttgt) - the text /bin farm regressed\" >&2; exit 1; }; \
         printf '%s\\n' \"$tdtlist\" | grep -q -x -F \"$a\" || { echo \"td-txt does not serve applet '$a' - its packed /bin/$a symlink would dispatch to nothing (usage, exit 2)\" >&2; exit 1; }; \
     done; \
     tdtro=$(printf '/dev/root / erofs ro,relatime 0 0\\n/dev/vda2 /var btrfs rw 0 0\\n' | \"$tdt\" grep -E '^[^ ]+ / erofs ro[, ]') || { echo 'td-txt: the packed grep did not select the read-only root line /etc/rootcheck depends on' >&2; exit 1; }; \
     case \"$tdtro\" in '/dev/root / erofs ro,relatime 0 0') : ;; *) echo \"td-txt: the packed grep selected the wrong /proc/mounts line: $tdtro\" >&2; exit 1;; esac; \
     printf '/dev/root / erofs rw,relatime 0 0\\n' | \"$tdt\" grep -qE '^[^ ]+ / erofs ro[, ]' && { echo 'td-txt: the packed grep selected a rw root as read-only - /etc/rootcheck would call a writable root healthy' >&2; exit 1; }; \
     [ \"$(printf 'a\\nb\\n' | \"$tdt\" sed -n '$=')\" = 2 ] || { echo 'td-txt: the packed sed miscounted a two-line stream' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-init\" 2>/dev/null)\" = \"{in:td-init}/bin/td-init\" ] || { echo 'root tree: /bin/td-init is not a symlink to the staged boot-glue multicall' >&2; exit 1; }; \
     tdi=\"{root}/real-root{in:td-init}/bin/td-init\"; tditgt=\"{in:td-init}/bin/td-init\"; { [ -f \"$tdi\" ] && [ -x \"$tdi\" ]; } || { echo 'root tree: the td-init binary is not packed/executable at real-root{in:td-init}/bin/td-init - /init would not exec and the machine would not boot' >&2; exit 1; }; \
     [ \"$(readlink \"$root/init\")\" = \"$tditgt\" ] || { echo 'root tree: /init must be a symlink to the staged td-init multicall - it is PID 1' >&2; exit 1; }; \
     tdilist=$(\"$tdi\" --list 2>/dev/null) || { echo 'td-init --list failed - cannot verify the boot-glue farm' >&2; exit 1; }; \
     for a in @TD_INIT_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tditgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged td-init multicall ($tditgt) - the boot-glue /bin farm regressed\" >&2; exit 1; }; \
         printf '%s\\n' \"$tdilist\" | grep -q -x -F \"$a\" || { echo \"td-init does not serve applet '$a' - its packed /bin/$a symlink would dispatch to nothing (usage, exit 2)\" >&2; exit 1; }; \
     done; \
     [ \"$(readlink \"$root/bin/td-login\" 2>/dev/null)\" = \"{in:td-login}/bin/td-login\" ] || { echo 'root tree: /bin/td-login is not a symlink to the staged credential multicall' >&2; exit 1; }; \
     tdl=\"{root}/real-root{in:td-login}/bin/td-login\"; tdltgt=\"{in:td-login}/bin/td-login\"; { [ -f \"$tdl\" ] && [ -x \"$tdl\" ]; } || { echo 'root tree: the td-login binary is not packed/executable at real-root{in:td-login}/bin/td-login - getty would exec a dangling /bin/login and no session could start' >&2; exit 1; }; \
     tdllist=$(\"$tdl\" --list 2>/dev/null) || { echo 'td-login --list failed - cannot verify the credential farm' >&2; exit 1; }; \
     for a in @TD_LOGIN_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tdltgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged td-login multicall ($tdltgt) - the credential /bin farm regressed\" >&2; exit 1; }; \
         printf '%s\\n' \"$tdllist\" | grep -q -x -F \"$a\" || { echo \"td-login does not serve applet '$a' - its packed /bin/$a symlink would dispatch to nothing (usage, exit 2)\" >&2; exit 1; }; \
     done; \
     [ -e \"$root/bin/verify-credentials\" ] && { echo 'root tree: verify-credentials is a readback PROBE, not an applet; a /bin symlink for it is a name no farm list accounts for' >&2; exit 1; }; \
     \"$tdl\" verify-credentials --uid 4294967294 --gid 4294967294 >/dev/null 2>&1 && { echo 'td-login verify-credentials ACCEPTED credentials this build process cannot have - the readback the TD-LOGIN-RUN-OK marker gates on proves nothing' >&2; exit 1; }; \
     set -- $(ls -l \"$tdl\"); case \"$1\" in *[sS]*) echo \"root tree: the packed td-login carries a setuid/setgid bit (mode $1). td-login is NEVER installed setuid-root (td-login/THREAT-MODEL.md section 4): with one, an unprivileged caller starts with euid 0 and 'su root' becomes root without authenticating\" >&2; exit 1;; esac; \
     tditab=$(\"$tdi\" init --dry-run -f \"$root/etc/inittab\" 2>&1) || { echo 'td-init init --dry-run REJECTED the inittab this image ships - PID 1 would come up having understood only part of its table. Its per-line diagnostics:' >&2; printf '%s\\n' \"$tditab\" >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-svc\" 2>/dev/null)\" = \"{in:td-svc}/bin/td-svc\" ] || { echo 'root tree: /bin/td-svc is not a symlink to the staged service supervisor - PID 1s only respawn line would exec nothing and the machine would have no userland at all' >&2; exit 1; }; \
     tds=\"{root}/real-root{in:td-svc}/bin/td-svc\"; { [ -f \"$tds\" ] && [ -x \"$tds\" ]; } || { echo 'root tree: the td-svc binary is not packed/executable at real-root{in:td-svc}/bin/td-svc - no identity, no network, no sshd and no console' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-seatd\" 2>/dev/null)\" = \"{in:td-seatd}/bin/td-seatd\" ] || { echo 'root tree: /bin/td-seatd is not a symlink to the staged single-user seat assigner' >&2; exit 1; }; \
     seat=\"{root}/real-root{in:td-seatd}/bin/td-seatd\"; { [ -f \"$seat\" ] && [ -x \"$seat\" ]; } || { echo 'root tree: td-seatd is not packed and executable' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-compositor\" 2>/dev/null)\" = \"{in:td-compositor}/bin/td-compositor\" ] || { echo 'root tree: /bin/td-compositor is not a symlink to the staged software Wayland compositor' >&2; exit 1; }; \
     compositor=\"{root}/real-root{in:td-compositor}/bin/td-compositor\"; { [ -f \"$compositor\" ] && [ -x \"$compositor\" ]; } || { echo 'root tree: td-compositor is not packed and executable' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-ui-demo\" 2>/dev/null)\" = \"{in:td-compositor}/bin/td-ui-demo\" ] || { echo 'root tree: /bin/td-ui-demo is not a symlink to the staged td-native Wayland client' >&2; exit 1; }; \
     uidemo=\"{root}/real-root{in:td-compositor}/bin/td-ui-demo\"; { [ -f \"$uidemo\" ] && [ -x \"$uidemo\" ]; } || { echo 'root tree: td-ui-demo is not packed and executable' >&2; exit 1; }; \
     tdsplan=$(\"$tds\" check -f \"$root@TD_SVC_CONF@\" 2>&1) || { echo 'td-svc check REJECTED the unit table this image ships - the boot would run a table the supervisor only partly understood. Its diagnostics:' >&2; printf '%s\\n' \"$tdsplan\" >&2; exit 1; }; \
     for u in @TD_SVC_UNITS@; do \
         printf '%s\\n' \"$tdsplan\" | grep -q -E \"^[0-9]+\\. $u\\$\" || { echo \"td-svc check resolved a start order without '$u' - a unit the inittab used to run is missing from the plan\" >&2; exit 1; }; \
     done; \
     : 'Order as the SHIPPED binary resolves the SHIPPED table. This cannot see a'; \
     : 'DELETED after= edge - ties break in declaration order, so dropping one leaves'; \
     : 'the plan identical. the_declared_edges_are_exactly_these pins the edge set on'; \
     : 'the host; this pins that td-svc itself still resolves them this way.'; \
     svcpos() { printf '%s\\n' \"$tdsplan\" | grep -n -E \"^[0-9]+\\. $1\\$\" | cut -d: -f1; }; \
     hn=$(svcpos hostname); fb=$(svcpos td-firstboot); rc=$(svcpos rootcheck); st=$(svcpos seat); nu=$(svcpos netup); wl=$(svcpos wayland); ui=$(svcpos ui-demo); bs=$(svcpos bootsuccess); sd=$(svcpos sshd); gr=$(svcpos greeter); \
     [ \"$hn\" -lt \"$fb\" ] || { echo 'td-svc would not serialize hostname before td-firstboot - init ran every sysinit line to completion before the next, and td-svc starts settled units in the same pass' >&2; exit 1; }; \
     [ \"$fb\" -lt \"$rc\" ] || { echo 'td-svc would start rootcheck before td-firstboot - rootcheck asserts the identity td-firstboot mints is readable' >&2; exit 1; }; \
     [ \"$rc\" -lt \"$nu\" ] || { echo 'td-svc would start netup before rootcheck - networking must follow the read-only-root self-check' >&2; exit 1; }; \
     [ \"$nu\" -lt \"$sd\" ] || { echo 'td-svc would start sshd before netup - sshd binds loopback, which netup brings up' >&2; exit 1; }; \
     [ \"$fb\" -lt \"$sd\" ] || { echo 'td-svc would start sshd before td-firstboot - sshd is fail-closed on the host key td-firstboot mints, so it would refuse to start on every boot' >&2; exit 1; }; \
     [ \"$nu\" -lt \"$gr\" ] || { echo 'td-svc would start the greeter before netup' >&2; exit 1; }; \
     [ \"$rc\" -lt \"$st\" ] && [ \"$st\" -lt \"$wl\" ] && [ \"$wl\" -lt \"$ui\" ] && [ \"$ui\" -lt \"$bs\" ] || { echo 'td-svc would not serialize rootcheck -> seat -> wayland -> ui-demo -> bootsuccess' >&2; exit 1; }; \
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
     for a in @DROPPED_APPLETS@; do \
         if [ -e \"$root/bin/$a\" ] || [ -L \"$root/bin/$a\" ]; then echo \"root tree: /bin/$a is packed, but '$a' is in DROPPED_APPLETS - the busybox retirement dropped this name rather than reimplementing it\" >&2; exit 1; fi; \
         if printf '%s\\n' \"$selector_list\" | grep -q -x -F \"bin/$a\"; then echo \"selector initramfs: bin/$a is packed, but '$a' is in DROPPED_APPLETS\" >&2; exit 1; fi; \
         if printf '%s\\n' \"$init_list\" | grep -q -x -F \"bin/$a\"; then echo \"deployment initramfs: bin/$a is packed, but '$a' is in DROPPED_APPLETS\" >&2; exit 1; fi; \
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
     tdf=\"{root}/real-root{in:td-firstboot}/bin/td-firstboot\"; tdftgt=\"{in:td-firstboot}/bin/td-firstboot\"; \
     { [ -f \"$tdf\" ] && [ -x \"$tdf\" ]; } || { echo 'root tree: the td-firstboot binary is not packed/executable at real-root{in:td-firstboot}/bin/td-firstboot - the sysinit job would fail and the machine would have no identity, so sshd (--host-key) would refuse to start' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-firstboot\" 2>/dev/null)\" = \"$tdftgt\" ] || { echo 'root tree: /bin/td-firstboot is not a symlink to the staged identity provisioner' >&2; exit 1; }; \
     \"$tdf\" --help >/dev/null 2>&1 || { echo 'the packed td-firstboot does not run (it is static with an empty closure, so it must)' >&2; exit 1; }; \
     \"$tdf\" --nonesuch >/dev/null 2>&1; [ $? -eq 2 ] || { echo 'td-firstboot must exit 2 on an unknown argument (usage error) rather than provisioning something unasked' >&2; exit 1; }; \
     dsz=$(wc -c < \"$disk\"); \
     [ \"$dsz\" -ge 4096 ] || { echo \"root.erofs: implausibly small ($dsz bytes)\" >&2; exit 1; }; \
     set -- $(od -An -tx1 -j 1024 -N 4 \"$disk\"); \
     [ \"$1$2$3$4\" = e2e1f5e0 ] || { echo 'root.erofs: missing EROFS superblock magic at byte 1024' >&2; exit 1; }; \
     [ \"$(wc -l < \"$manifest\")\" -eq 4 ] || { echo 'manifest: expected header plus exactly three payload entries' >&2; exit 1; }; \
     [ \"$(head -n 1 \"$manifest\")\" = td-deployment-v1 ] || { echo 'manifest: unsupported or missing td-deployment-v1 header' >&2; exit 1; }; \
     for a in bzImage initramfs.cpio root.erofs; do \
         grep -q -E \"^[0-9a-f]{64}  $a$\" \"$manifest\" || { echo \"manifest: missing strict SHA-256 entry for $a\" >&2; exit 1; }; \
     done"
        // Name the declared BusyBox input exactly; a store wildcard could accept
        // an unrelated or stale BusyBox output.
        //
        // Validate EVERY packed applet, not just the greeter-critical few. Names are all
        // shell-safe identifiers, so a space-joined `for` list is safe unquoted. uutils
        // cannot execute in the build sandbox because its absolute interpreter exists only
        // inside the assembled root; compare symlink text without resolving it. The headless
        // boot oracle executes uutils after pivoting and remains the behavioral runtime check.
        .replace("@BUSYBOX_APPLETS@", &BUSYBOX_APPLETS.join(" "))
        .replace("@INITRAMFS_APPLETS@", &INITRAMFS_APPLETS.join(" "))
        // The dropped-name sweep tests -e AND -L because a repacked /bin entry pointing at a
        // target the build tree does not hold is DANGLING, which -e alone reads as absent.
        .replace("@DROPPED_APPLETS@", &DROPPED_APPLETS.join(" "))
        .replace("@UUTILS_APPLETS@", &UUTILS_APPLETS.join(" "))
        .replace("@TD_UTIL_APPLETS@", &TD_UTIL_APPLETS.join(" "))
        .replace("@TD_TXT_APPLETS@", &TD_TXT_APPLETS.join(" "))
        .replace("@TD_INIT_APPLETS@", &td_init_applets().join(" "))
        .replace("@TD_LOGIN_APPLETS@", &TD_LOGIN_APPLETS.join(" "))
        .replace("@TD_SVC_UNITS@", &TD_SVC_UNITS.join(" "))
        .replace("@TD_SVC_CONF@", TD_SVC_CONF)
        .replace("@TD_SVC_CONF_NAME@", td_svc_conf_etc_name())

        // `<etc path>=<target>` pairs, and the etc paths alone. Both lists are
        // space-joined and unquoted in the script, which
        // `mutable_etc_paths_are_shell_safe_and_well_formed` keeps safe.
        .replace(
            "@MUTABLE_ETC@",
            &MUTABLE_ETC
                .iter()
                .map(|entry| format!("{}={}", entry.etc, entry.target))
                .collect::<Vec<_>>()
                .join(" "),
        )
        .replace("@MUTABLE_ETC_NAMES@", &mutable_etc_names().join(" "))
        // One glob per directory the table uses, relative to /etc — a sweep for
        // symlinks the table does not name. Globs rather than a recursive walk
        // because the ladder guard bans the host directory-walk tools by name, and
        // because the table is what decides which directories can hold one.
        .replace("@ETC_GLOBS@", &etc_globs().join(" "))
        .replace("@MUTABLE_ETC_DIRS@", &mutable_etc_dirs().join(" "))
        .replace("@MUTABLE_ETC_COUNT@", &MUTABLE_ETC.len().to_string())
}

/// The `/etc`-relative paths of the table, for the allowlist sweep.
fn mutable_etc_names() -> Vec<&'static str> {
    MUTABLE_ETC.iter().map(|entry| entry.etc).collect()
}

/// `*` plus one `<dir>/*` per subdirectory the table uses.
/// `*` and `.*` (a dot-name is not matched by `*`), plus the same pair inside every
/// directory the table declares. The dir-allowlist leg above is what makes that
/// scope a proof rather than a guess: no OTHER directory may exist under /etc.
fn etc_globs() -> Vec<String> {
    let mut globs = vec!["*".to_string(), ".*".to_string()];
    for dir in mutable_etc_dirs() {
        globs.push(format!("{dir}/*"));
        globs.push(format!("{dir}/.*"));
    }
    globs
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
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "chmod 0600 '{root}/real-root/etc/shadow'",
            ],
        )
        .env("PATH", &post_bootstrap_path()),
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
                POST_BOOTSTRAP_SH,
                "-c",
                "'{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/selector.spec' > '{root}/selector-initramfs.cpio'; \
                 '{in:linux-x86-64}/gen_init_cpio' -t 1 '{root}/deployment.spec' > '{root}/initramfs.cpio'",
            ],
        )
        .env("PATH", &post_bootstrap_path()),
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
    steps.push(
        Step::run("{out}", &[POST_BOOTSTRAP_SH, "-c", &shape_check()])
            .env("PATH", &post_bootstrap_path()),
    );

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
        // td-txt: the static text multicall (empty runtime closure, CopyTree'd), serving
        //   /bin/grep and /bin/sed. Unlike td-util's farm these are on the boot path —
        //   /etc/rootcheck and four other generated scripts grep /proc with them.
        // td-init: the static boot-glue multicall (empty runtime closure, CopyTree'd). Not a
        //   farm like the others: it is /init (PID 1), the deployment initramfs' pivot, and
        //   the sysinit `hostname -F`, so the image's boot path runs it on every boot.
        // td-firstboot: the static per-machine identity provisioner (empty runtime closure,
        //   CopyTree'd). One /bin entry, run once per boot as a sysinit job; it is what
        //   fills the /var targets the MUTABLE_ETC symlinks point at.
        // td-login: the static credential multicall (empty runtime closure, CopyTree'd),
        //   serving the /bin/{login,su} farm. Load-bearing like td-init rather than
        //   probe-only like td-util: `login -f` is how the greeter is reached and `su` is
        //   how every unprivileged health leg runs. See td-login/THREAT-MODEL.md.
        // td-svc: the static service supervisor that starts every real-root job.
        // td-seatd/td-compositor: the static single-user UI substrate and demo client,
        // copied directly.
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
            "td-txt",
            "td-init",
            "td-firstboot",
            "td-login",
            "td-svc",
            "td-seatd",
            "td-compositor",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;


    /// Parse the generated unit table into `(name, [(key, value)])`.
    ///
    /// The tests below assert on DECLARED EDGES, not on line positions. Under the
    /// inittab, order WAS position — `::sysinit:` lines ran top to bottom — so a test
    /// could only compare line numbers. Under td-svc the order is a graph, and a test
    /// still reading positions would pass for the wrong reason: it would keep passing
    /// after someone deleted an `after=` and left the stanzas in a lucky order.
    fn parse_td_svc_conf() -> Vec<(String, Vec<(String, String)>)> {
        let table = build_td_svc_conf();
        let mut units: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for raw in table.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                units.push((name.to_string(), Vec::new()));
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| unreachable!("table line {line:?} is not key=value"));
            match units.last_mut() {
                Some((_, keys)) => keys.push((key.to_string(), value.to_string())),
                None => unreachable!("table line {line:?} precedes any [unit]"),
            }
        }
        units
    }

    /// One unit's value for a key, or None.
    fn unit_key(name: &str, key: &str) -> Option<String> {
        parse_td_svc_conf()
            .into_iter()
            .find(|(unit, _)| unit == name)
            .and_then(|(_, keys)| {
                keys.into_iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, value)| value)
            })
    }

    /// Everything a unit declares it starts after.
    fn unit_after(name: &str) -> Vec<String> {
        unit_key(name, "after")
            .map(|v| v.split(',').map(|d| d.trim().to_string()).collect())
            .unwrap_or_default()
    }

    /// Is `earlier` an ancestor of `later` through `after=` edges? The transitive
    /// question is the one that matters — `sshd after netup after rootcheck after
    /// td-firstboot` orders sshd behind the identity just as surely as naming it.
    fn ordered_before(earlier: &str, later: &str) -> bool {
        let mut frontier = unit_after(later);
        let mut seen: Vec<String> = Vec::new();
        while let Some(name) = frontier.pop() {
            if name == earlier {
                return true;
            }
            if seen.contains(&name) {
                continue;
            }
            frontier.extend(unit_after(&name));
            seen.push(name);
        }
        false
    }

    /// PID 1 keeps ONLY what it must own. Everything else is a unit.
    ///
    /// The mounts stay because td-svc reads /proc — its own group and session, every
    /// containment query, every liveness check. A td-svc started before /proc exists
    /// comes up unable to signal a process group at all. sysinit runs to completion
    /// before any respawn line, so the position of these three lines IS that guarantee,
    /// and it is the one place in this file where line order still carries meaning.
    #[test]
    fn the_inittab_keeps_only_what_pid_one_must_own() {
        let inittab = build_inittab();
        let lines: Vec<&str> = inittab
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(
            lines,
            vec![
                "::sysinit:/bin/mount -t devtmpfs devtmpfs /dev",
                "::sysinit:/bin/mount -t proc proc /proc",
                "::sysinit:/bin/mount -t sysfs sysfs /sys",
                &format!("::respawn:/bin/td-svc run -f {TD_SVC_CONF}"),
            ],
            "PID 1's table is the pseudo-filesystem mounts plus td-svc, and nothing else \
             — a service that reappears here is one td-svc cannot order, restart, or stop"
        );
        let proc = lines
            .iter()
            .position(|l| l.contains("proc /proc"))
            .unwrap_or_else(|| unreachable!("no /proc mount"));
        let svc = lines
            .iter()
            .position(|l| l.contains("/bin/td-svc"))
            .unwrap_or_else(|| unreachable!("no td-svc line"));
        assert!(
            proc < svc,
            "/proc must be mounted before td-svc starts: it reads /proc/self/stat for its \
             own group and session, and fails closed without them"
        );
    }

    /// Nothing may be lost in the cutover. Every command the inittab used to run is
    /// either still PID 1's or is now a unit — a job that quietly vanished is a boot
    /// that comes up missing a service with no diagnostic anywhere.
    #[test]
    fn every_job_the_inittab_used_to_run_still_runs() {
        let table = build_td_svc_conf();
        let inittab = build_inittab();
        for command in [
            "/bin/hostname -F /etc/hostname",
            "/bin/td-firstboot provision",
            "/etc/rootcheck",
            "/bin/td-seatd assign",
            "/etc/netup",
            "/bin/td-compositor run",
            "/bin/td-ui-demo run",
            "/etc/bootsuccess",
            "/etc/bootfail",
            "/bin/sshd serve",
            "/etc/tty-session",
        ] {
            assert!(
                table.contains(command) || inittab.contains(command),
                "{command} ran under the old inittab and is now in neither PID 1's table \
                 nor td-svc's"
            );
        }
        // ...and the roster the shape check greps for must match the table itself, or
        // the build-time check silently stops covering a unit.
        let declared: Vec<String> = parse_td_svc_conf().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            declared,
            TD_SVC_UNITS.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            "TD_SVC_UNITS is what shape_check greps `td-svc check`'s plan for; a unit \
             missing from it is a unit whose absence from the plan nothing would catch"
        );
    }

    /// sshd's output is captured AND copied to the console.
    ///
    /// Both halves matter and neither is implied by the other. Without `log=`,
    /// the one shipped service that talks to the network writes to a console
    /// that scrolls and is never kept. Without `console=yes`, capture takes
    /// sshd's failures OUT of the serial output the qemu boot oracle prints
    /// when sshd is the reason the boot failed — which is exactly when they
    /// are needed.
    #[test]
    fn the_sshd_unit_is_captured_and_still_reaches_the_console() {
        let table = build_td_svc_conf();
        let sshd = table
            .split("[sshd]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .unwrap_or_default()
            .to_string();
        assert!(
            sshd.contains("log=/var/log/svc/sshd.log"),
            "sshd's output is not captured: {sshd}"
        );
        assert!(
            sshd.contains("console=yes"),
            "sshd is captured but no longer reaches the console; the boot oracle's \
             'Last serial output' would stop carrying the reason sshd failed: {sshd}"
        );
    }

    /// Every oneshot carries an explicit `timeout=`.
    ///
    /// td-svc defaults one, but its default is sized for a small job and these are not
    /// — netup runs DHCP with retries, bootsuccess has a cmdline-configurable wait. A
    /// oneshot killed early is a service that silently did not happen, so the values are
    /// backstops against a hang, well above what the job can take. Daemons get none:
    /// td-svc rejects `timeout=` on a daemon, which uses `ready-timeout=` instead.
    #[test]
    fn every_oneshot_bounds_itself_and_no_daemon_does() {
        for (name, keys) in parse_td_svc_conf() {
            let kind = keys
                .iter()
                .find(|(k, _)| k == "type")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| unreachable!("unit {name} declares no type="));
            let timeout = keys.iter().find(|(k, _)| k == "timeout").map(|(_, v)| v);
            match kind.as_str() {
                "oneshot" => {
                    let secs: u32 = timeout
                        .unwrap_or_else(|| unreachable!("oneshot {name} has no timeout="))
                        .parse()
                        .unwrap_or_else(|_| unreachable!("{name}: timeout= is not a number"));
                    assert!(
                        secs >= svc_timeouts::HOSTNAME,
                        "{name}: a {secs}s bound is below even the trivial jobs' — a \
                         timeout that fires early makes a service silently not happen"
                    );
                }
                "daemon" => assert!(
                    timeout.is_none(),
                    "{name}: td-svc REJECTS timeout= on a daemon, so this table would \
                     fail `td-svc check` and the image build with it"
                ),
                other => unreachable!("unit {name} has unknown type '{other}'"),
            }
        }
    }

    /// The timeout VALUES, pinned against the worst case each one covers.
    ///
    /// `every_oneshot_bounds_itself_and_no_daemon_does` asserts only that a bound exists
    /// and clears a 30s floor, so `NETUP` could go from 300 to 31 with every host test
    /// green — and 31 is below what the job takes: td-netd is 3 tries x 2s DISCOVER plus
    /// the same REQUEST (12s), then under the nettest token a 3s-timeout resolve and a 5s
    /// connect. A oneshot that outruns its bound is TERMed and marked failed, so netup
    /// would silently not configure the link and sshd would start against a half-open
    /// network — visible only to the qemu oracle. Under the inittab these jobs had NO
    /// bound, so a too-tight value is a failure mode this landing introduces, and the
    /// values are therefore reviewed constants rather than free parameters.
    #[test]
    fn each_timeout_stays_above_the_worst_case_its_comment_claims() {
        // A backstop must clear its worst case with room, not by a hair: these run on a
        // TCG-emulated VM with a cold page cache, and the numbers below are computed from
        // read timeouts and spawn counts, not measured. So the rule is 2x the worst case.
        // Verified red at NETUP=31 — which clears the raw 26s and still fails here, which
        // is the point: 31 was the value review proposed as obviously-too-tight.
        const HEADROOM: u32 = 2;
        // (value, worst case in seconds, what that worst case is)
        let floors: [(u32, u32, &str); 7] = [
            (svc_timeouts::HOSTNAME, 1, "one file read plus sethostname(2)"),
            (svc_timeouts::FIRSTBOOT, 60, "ed25519 keygen, writes to /var, then sync"),
            (svc_timeouts::ROOTCHECK, 50, "~50 process spawns incl. su, plus a sync"),
            (
                svc_timeouts::SEAT,
                1,
                "one sysfs/devtmpfs scan plus ownership and mode verification",
            ),
            (
                svc_timeouts::NETUP,
                26,
                "DHCP 3x2s DISCOVER + 3x2s REQUEST, then a 3s resolve and a 5s connect",
            ),
            (
                svc_timeouts::BOOTSUCCESS,
                // The loop is clamped to this many iterations; budget a slow one each.
                (BOOT_SUCCESS_RETRY_MAX_SECS as u32).saturating_mul(30),
                "clamped iterations of seven su probe blocks plus a td-boot install",
            ),
            (
                svc_timeouts::BOOTFAIL,
                BOOT_FAIL_PARK_WAIT_SECS as u32,
                "clamped iterations of a grep and a 1s sleep",
            ),
        ];
        for (value, worst, why) in floors {
            let floor = worst.saturating_mul(HEADROOM);
            assert!(
                value >= floor,
                "a {value}s bound leaves no headroom over the {worst}s this job can take \
                 ({why}); a backstop must clear its worst case {HEADROOM}x, or it TERMs a \
                 healthy job on a slow boot and marks the service failed"
            );
            // td-svc's parser rejects anything above MAX_DURATION_SECS, so a value that
            // large never reaches the boot — it reds `td-svc check` and the image build.
            assert!(
                value <= 3600,
                "a {value}s bound exceeds td-svc's MAX_DURATION_SECS and would be \
                 REJECTED by `td-svc check`, failing the image build"
            );
        }
    }

    /// The COMPLETE `after=` edge set, pinned.
    ///
    /// This is the guard that actually protects the boot order, and it exists because
    /// the obvious check does not work: td-svc breaks ties in DECLARATION order, so
    /// deleting `after=td-firstboot` from rootcheck leaves the resolved plan
    /// byte-identical. Verified by doing exactly that — `td-svc check` still printed
    /// the same eight lines and exited 0. Only the declared edges distinguish an
    /// ordering that is guaranteed from one that is currently lucky, so they are
    /// pinned exactly: a deletion reds here, and so does an addition nobody reviewed.
    ///
    /// `hostname -> td-firstboot` is here for a second reason: td-svc starts every unit
    /// whose edges are settled in one pass, so an edge omitted because "nothing reads
    /// across them" buys CONCURRENCY that init's serial sysinit never had.
    #[test]
    fn the_declared_edges_are_exactly_these() {
        let sysinit = ["hostname", "td-firstboot", "rootcheck", "netup"];
        let expected: Vec<(&str, Vec<&str>)> = vec![
            ("hostname", vec![]),
            ("td-firstboot", vec!["hostname"]),
            ("rootcheck", vec!["td-firstboot"]),
            ("seat", vec!["rootcheck"]),
            ("netup", vec!["rootcheck"]),
            ("wayland", vec!["seat"]),
            ("ui-demo", vec!["wayland"]),
            (
                "bootsuccess",
                sysinit
                    .iter()
                    .copied()
                    .chain(["wayland", "ui-demo"])
                    .collect(),
            ),
            ("bootfail", sysinit.to_vec()),
            ("sshd", sysinit.to_vec()),
            ("greeter", sysinit.to_vec()),
        ];
        for (unit, after) in expected {
            assert_eq!(
                unit_after(unit),
                after.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
                "{unit}'s after= is not what the boot order was reviewed against"
            );
        }
    }

    /// init ran `::sysinit:` lines ONE AT A TIME, each to completion. td-svc has no such
    /// rule — it starts everything whose edges are settled in the same pass — so the
    /// serialization has to be spelled out as a total chain. Without it two sysinit jobs
    /// overlap, which is a behaviour change no part of a cutover should be making.
    #[test]
    fn the_sysinit_chain_is_total_so_no_two_of_them_ever_overlap() {
        let sysinit = ["hostname", "td-firstboot", "rootcheck", "netup"];
        for pair in sysinit.windows(2) {
            let (Some(prev), Some(next)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            assert!(
                unit_after(next).contains(&(*prev).to_string()),
                "{next} does not declare after={prev}, so td-svc would start them \
                 CONCURRENTLY — init ran every sysinit line to completion before the \
                 next, and this chain is the only thing that still says so"
            );
        }
    }

    /// Everything that was `::once:` or `::respawn:` started only after init had run
    /// EVERY `::sysinit:` job to completion. That is the edge set those units must
    /// declare — naming only the last member would silently weaken if the chain is
    /// ever reordered.
    #[test]
    fn the_post_sysinit_units_wait_for_the_whole_sysinit_set() {
        let sysinit = ["hostname", "td-firstboot", "rootcheck", "netup"];
        for unit in ["bootsuccess", "bootfail", "sshd", "greeter"] {
            let after = unit_after(unit);
            for job in sysinit {
                assert!(
                    after.contains(&job.to_string()),
                    "{unit} does not declare after={job}; under the inittab it could not \
                     start until every sysinit job had finished, and this is that edge"
                );
            }
        }
        assert!(
            unit_after("bootsuccess").contains(&"wayland".to_string()),
            "bootsuccess must be ordered after the graphical readiness decision"
        );
        assert!(
            unit_after("bootsuccess").contains(&"ui-demo".to_string()),
            "bootsuccess must wait for the first client frame"
        );
        assert_eq!(
            unit_key("bootsuccess", "requires").as_deref(),
            Some("ui-demo"),
            "deployment health must be skipped when the graphical client failed; \
             after= alone settles on either success or failure"
        );
    }

    /// The console keeps td-svc's I5 protection: ordering only, never a strict
    /// dependency. `requires=` is what makes a unit skippable, and td-svc's table
    /// parser refuses it on a `tty=` unit — so a table that grew one would red the
    /// image build, but this says why before it gets there.
    #[test]
    fn the_greeter_is_ordered_but_never_made_skippable() {
        assert!(
            unit_key("greeter", "requires").is_none(),
            "a tty= unit may not declare requires=: the console is never skippable, and \
             td-svc check would reject this table"
        );
        assert!(
            !unit_after("greeter").is_empty(),
            "the greeter should still PREFER to start after the system is up"
        );
        assert_eq!(
            unit_key("greeter", "restart").as_deref(),
            Some("always"),
            "the greeter replaced a `respawn` line and must restart like one"
        );
        assert_eq!(
            unit_key("sshd", "restart").as_deref(),
            Some("always"),
            "sshd replaced a `respawn` line and must restart like one"
        );
        assert_eq!(
            unit_key("wayland", "requires").as_deref(),
            Some("seat"),
            "the unprivileged compositor must not start when seat assignment failed"
        );
        assert_eq!(
            unit_key("wayland", "restart").as_deref(),
            Some("always"),
            "the graphical session is supervised and restartable"
        );
        assert_eq!(
            unit_key("ui-demo", "requires").as_deref(),
            Some("wayland"),
            "the demo client must not start without its compositor"
        );
        assert_eq!(
            unit_key("ui-demo", "restart").as_deref(),
            Some("always"),
            "the graphical client is supervised and restartable"
        );
    }

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
            valid_account_name(SYSTEM.autologin),
            "autologin user '{}' must be a plain [A-Za-z0-9._-] name: /etc/autologin execs \
             `/bin/login -f <name>` unquoted, as root, before any session exists",
            SYSTEM.autologin
        );
        assert!(
            SYSTEM
                .users
                .iter()
                .find(|user| user.name == SYSTEM.autologin)
                .is_some_and(|user| user.uid != 0),
            "autologin user '{}' must resolve uniquely to an unprivileged user",
            SYSTEM.autologin
        );
        assert!(
            SYSTEM
                .users
                .iter()
                .find(|user| user.name == SYSTEM.autologin)
                .is_some_and(|user| {
                    user.name == UI_USER && user.uid == UI_UID && user.gid == UI_GID
                }),
            "the first UI profile is deliberately one fixed seat: build_td_svc_conf, \
             td-seatd, XDG_RUNTIME_DIR, and WAYLAND_DISPLAY all bind tester 1000:1000; \
             make those generated before changing the graphical account"
        );
        // td-login refuses `login -f` for a LOCKED account — stricter than busybox, whose
        // `-f` skips the account database entirely (td-login/THREAT-MODEL.md section 3). So a
        // `passwordless: false` auto-login user is an image that boots to a getty respawn
        // loop and never reaches a greeter, with the oracle able to report only a timeout.
        // `build_shadow` writes `!` for such a user, which is exactly what td-login locks on.
        assert!(
            SYSTEM
                .users
                .iter()
                .find(|user| user.name == SYSTEM.autologin)
                .is_some_and(|user| user.passwordless),
            "autologin user '{}' must be passwordless: build_shadow writes `!` otherwise, and \
             td-login denies a locked account even under `login -f`",
            SYSTEM.autologin
        );
        for u in SYSTEM.users {
            assert!(
                valid_account_name(u.name),
                "user name '{}' must be a plain [A-Za-z0-9._-] name of at most 32 bytes: it \
                 is embedded UNQUOTED in generated root shell (`/bin/su … <name> -c …`, \
                 `/bin/login -f <name>`) and in the colon-separated /etc/{{passwd,group,shadow}} \
                 this recipe writes",
                u.name
            );
            assert!(
                valid_home(u.uid, u.home),
                "user '{}' home '{}' must be /root for uid 0 or one shell-safe direct \
                 child of /home for an unprivileged uid",
                u.name,
                u.home
            );
            // td-login execs the shell by ABSOLUTE path (`db::account` REFUSES a relative
            // one, and there is no PATH search), and we only pack applets under /bin, so the
            // shell MUST be "/bin/<applet>" packed by a farm. A bare "sh" would pass a naive
            // basename check yet fail at runtime (execv("sh") -> ENOENT -> getty respawn-
            // loops); reject it.
            let packed_applet = u.shell.strip_prefix("/bin/");
            assert!(
                packed_applet
                    .is_some_and(|a| BUSYBOX_APPLETS.contains(&a) || UUTILS_APPLETS.contains(&a)),
                "user '{}' login shell '{}' must be \"/bin/<applet>\" packed by a /bin farm \
                 (td-login execs it by absolute path)",
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

    /// The greeter is the only process that can see whether `login` handed it the
    /// terminal, so the check for that lives in the profile it sources. It is a
    /// diagnostic, not a gate — see the comment at the site — which is exactly why
    /// it needs a test: nothing reds if it silently stops being emitted.
    #[test]
    fn greeter_checks_the_login_terminal_was_handed_over() {
        let profile = build_profile(&SYSTEM);
        let user = SYSTEM
            .users
            .iter()
            .filter(|u| u.name == SYSTEM.autologin)
            .next()
            .expect("the autologin account must exist");
        assert!(
            profile.contains("/bin/tty"),
            "the greeter must ask the kernel which terminal it is on"
        );
        assert!(
            profile.contains("crw-------"),
            "0600 is the whole point of the hand-over; a mode check that accepts \
             group- or world-access would green a terminal the next session can read"
        );
        assert!(
            profile.contains(&format!("[ \"$3\" = {} ] && [ \"$4\" = {} ]", user.uid, user.gid)),
            "the terminal must be checked against the autologin account's OWN ids, \
             not merely against 'not root'"
        );
        // A subshell, so `set -f` cannot leak into the operator's interactive shell.
        assert!(
            profile.contains("(t=$(/bin/tty") && profile.contains("set -f;"),
            "the check must run in a subshell: `set -f` in a sourced profile persists"
        );
    }

    /// getty auto-logs-in via `-l /etc/autologin`, and login needs both applets; the
    /// respawn line is inert without them. `reboot` is what `tty-session` execs when the
    /// greeter session ends (the in-guest power-off path). `switch_root` is the stage-1
    /// pivot applet — without it the two-stage boot cannot enter the erofs root, and
    /// `mount`/`umount` are what bring every filesystem up and let it go again. These are
    /// all boot-critical and must stay on a STATIC multicall (no runtime closure) — busybox
    /// for the login pair, td-init for the rest since the cutovers: belt-and-braces against
    /// a farm edit that drops one or reroutes it to dynamically-linked uutils (the shape
    /// check catches it at build time, this catches it at test time).
    #[test]
    fn greeter_and_pivot_applets_are_present() {
        // Split across THREE static multicalls now, so each name is pinned to the ONE that
        // serves it. Asserting only "some farm has it" would let a boot name drift between
        // binaries unnoticed — and for these names that drift IS the boot.
        for a in ["sh", "getty"] {
            assert!(
                BUSYBOX_APPLETS.contains(&a),
                "boot-critical applet '{a}' missing from BUSYBOX_APPLETS"
            );
        }
        for a in ["login", "su"] {
            assert!(
                TD_LOGIN_APPLETS.contains(&a),
                "credential applet '{a}' missing from the td-login farm"
            );
            assert!(
                !BUSYBOX_APPLETS.contains(&a),
                "'{a}' is still on busybox; the td-login cutover replaces it, and a name in \
                 both farms ships whichever Symlink step ran last"
            );
        }
        for a in ["init", "reboot", "switch_root", "hostname", "mount", "umount"] {
            assert!(
                td_init_applets().contains(&a),
                "boot-glue applet '{a}' missing from the td-init farm"
            );
        }
        // ...and the pair that MOVED is gone from the one it left, so a stray re-add cannot
        // pack two symlinks for one name (the disjointness test below would catch that, but
        // this is the assertion that says which direction the cutover went).
        for a in ["mount", "umount"] {
            assert!(
                !BUSYBOX_APPLETS.contains(&a),
                "'{a}' is td-init's since the mount(2)/umount2(2) amendment"
            );
        }
        assert!(
            !BUSYBOX_APPLETS.contains(&"losetup"),
            "'losetup' is td-init's since the LOOP_SET_FD amendment"
        );
    }

    /// The /bin farms must be DISJOINT — a name in both would pack two conflicting symlinks
    /// for one applet (last-writer-wins, non-deterministic) and blur the static-vs-dynamic
    /// boot-safety boundary. Also pin the boot-critical names to a STATIC multicall: every
    /// one of them runs somewhere no dynamic loader is reachable (the pre-pivot initramfs)
    /// or where a failure has nowhere to be reported (PID 1's own sysinit), so what matters
    /// is not which static binary serves them but that uutils never does.
    /// Every /bin farm, name-tagged. ONE table: two tests consume it, and a seventh farm
    /// added to only one of them would leave the other silently narrower.
    fn bin_farms<'a>(td_init: &'a [&'static str]) -> [(&'static str, &'a [&'static str]); 6] {
        [
            ("busybox", BUSYBOX_APPLETS),
            ("uutils", UUTILS_APPLETS),
            ("td-util", TD_UTIL_APPLETS),
            ("td-txt", TD_TXT_APPLETS),
            ("td-init", td_init),
            ("td-login", TD_LOGIN_APPLETS),
        ]
    }

    #[test]
    fn applet_farms_are_disjoint_and_boot_names_stay_static() {
        // Check every pair: a name served twice emits two Symlink steps for one link and the
        // LAST one silently wins.
        let td_init = td_init_applets();
        let farms = bin_farms(&td_init);
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
        for a in [
            "hostname", "mount", "umount", "sh", "init", "switch_root", "login", "su",
        ] {
            assert!(
                BUSYBOX_APPLETS.contains(&a)
                    || td_init.contains(&a)
                    || TD_LOGIN_APPLETS.contains(&a),
                "boot-critical applet '{a}' must be served by a STATIC multicall (busybox, \
                 td-init or td-login) - it runs where no dynamic loader is reachable"
            );
            assert!(
                !UUTILS_APPLETS.contains(&a),
                "boot-critical applet '{a}' must NOT be served by dynamically-linked uutils"
            );
        }
    }

    /// A dropped name is invisible to every other check here: absent from all four farms it
    /// looks exactly like a name nobody ever considered, so nothing would notice `vi` coming
    /// back as a busybox symlink or as a td-util applet. This is the assertion that makes the
    /// drop a decision rather than an omission — including through the multiplexer, since a
    /// dropped name in INITRAMFS_APPLETS would put it back as `busybox vi` with a shape-check
    /// probe behind it. The last leg pins shape_check's own scan of the STAGED tree, which is
    /// what catches a `/bin/vi` packed by some route these lists never model.
    #[test]
    fn dropped_applets_stay_dropped() {
        // An emptied list would make every loop below vacuous, which is the one way this
        // guard can be disabled without deleting it.
        assert!(
            !DROPPED_APPLETS.is_empty(),
            "DROPPED_APPLETS is empty - either every dropped name was reinstated (say so \
             here) or the guard was hollowed out"
        );
        let td_init = td_init_applets();
        let farms = bin_farms(&td_init);
        let packed = packed_bin_names();
        for a in DROPPED_APPLETS {
            for (farm, set) in &farms {
                assert!(
                    !set.contains(a),
                    "'{a}' is in DROPPED_APPLETS but the {farm} farm serves it - the busybox \
                     retirement dropped this name instead of reimplementing it; remove it from \
                     DROPPED_APPLETS in the landing that brings it back"
                );
            }
            assert!(
                !INITRAMFS_APPLETS.contains(a),
                "'{a}' is in DROPPED_APPLETS but INITRAMFS_APPLETS declares it as \
                 `busybox {a}` - a dropped name gets no /bin entry and no declared \
                 multiplexed use"
            );
            assert!(
                !packed.iter().any(|p| p == a),
                "'{a}' is in DROPPED_APPLETS but real_root_steps packs /bin/{a} - the drop \
                 did not reach the symlink steps"
            );
            // Both cpios too: shape_check's sweep sees only the real root, and an initramfs
            // /bin is a second, independent farm.
            for phase in [Phase::Selector, Phase::Deployment] {
                assert!(
                    !initramfs_bin_names(phase).iter().any(|p| p == a),
                    "'{a}' is in DROPPED_APPLETS but a boot cpio packs /bin/{a} - the drop \
                     did not reach build_initramfs_spec"
                );
            }
        }
        // The build-time half, asserted the way the td-init legs above it are. Match each
        // leg's whole condition, not just the loop header: the header survives every
        // weakening the legs can suffer, `-L` included.
        let shape = shape_check();
        assert!(
            shape.contains(&format!(
                "for a in {}; do if [ -e \"$root/bin/$a\" ] || [ -L \"$root/bin/$a\" ]; then",
                DROPPED_APPLETS.join(" ")
            )),
            "shape_check no longer sweeps DROPPED_APPLETS over the staged /bin with both \
             tests - only the lists in this file would still be checked, and a /bin/vi \
             packed by any other route would ship"
        );
        // The cpio legs check the SHIPPED archive listing; the initramfs_bin_names loop
        // above checks the spec that generated it. Losing these leaves only the model.
        for (list, cpio) in [("selector_list", "selector"), ("init_list", "deployment")] {
            assert!(
                shape.contains(&format!(
                    "printf '%s\\n' \"${list}\" | grep -q -x -F \"bin/$a\""
                )),
                "shape_check no longer sweeps DROPPED_APPLETS over the {cpio} initramfs \
                 listing - a dropped name repacked into that cpio would only be caught by \
                 this file's model of the spec"
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

    /// `in_command_position` itself, which nothing else pins.
    ///
    /// Its only consumer is a NEGATIVE assertion no current script trips, so replacing
    /// the whole body with `false` disables the guard while leaving every test green —
    /// the guard can be switched off without being deleted. These are the shapes this
    /// file actually writes, in both directions.
    #[test]
    fn command_position_is_recognised_in_the_shapes_this_file_writes() {
        for yes in [
            "",                      // start of text
            "foo\n",                 // start of line
            "cmd; ",                 // after a separator
            "a && ",
            "a || ",
            "a | ",
            "if ! ",                 // negation
            "if ",
            "while ",
            "then ",
            "do ",
            "case x in x) ",         // a case arm's body
            "exec ",
            "env ",
            "TDVAR=1 ",              // an assignment prefix
            "sh -c '",               // the -c body this file writes five times
            "/bin/busybox sh -c \"",
            "cmd \\\n     ",          // a line continuation
        ] {
            assert!(
                in_command_position(yes),
                "{yes:?} leaves a command next, but the guard would let a bare token there pass"
            );
        }
        for no in [
            "echo \"td-util: ",       // prose — the diagnostics this file emits
            "echo 'td-init: ",
            "dir {in:",              // a store-path component
            "TD_UTIL",               // a marker name
            "/bin/",                 // handled by the caller, not here
            "grep -q ",              // an operand, not a command
            "motif ",                // ends with a keyword but is not one
            "ado ",
        ] {
            assert!(
                !in_command_position(no),
                "{no:?} is not command position; flagging it makes the guard cry wolf on \
                 prose and invites its deletion"
            );
        }
    }


    /// The words of the `test` expression starting at `rest`, up to the first shell
    /// separator OUTSIDE quotes and `$( )`.
    ///
    /// A naive `split_whitespace` turns `"$(/bin/td-util cat /f)" = v` into five words
    /// and reads `cat` as the operator, so the scan above would either miss a bad
    /// operator or reject a good one.
    #[cfg(test)]
    fn test_words(rest: &str) -> Vec<String> {
        let (mut words, mut cur) = (Vec::new(), String::new());
        let (mut dquote, mut depth) = (false, 0usize);
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    dquote = !dquote;
                    cur.push(c);
                }
                '$' if chars.peek() == Some(&'(') => {
                    depth += 1;
                    cur.push(c);
                }
                '(' if depth > 0 => cur.push(c),
                ')' if depth > 0 => {
                    depth -= 1;
                    cur.push(c);
                }
                ';' | '&' | '|' | '\n' if !dquote && depth == 0 => break,
                c if c.is_whitespace() && !dquote && depth == 0 => {
                    if !cur.is_empty() {
                        words.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            words.push(cur);
        }
        words
    }

    /// One of td-util `test`'s operator rosters, read out of the applet's own source.
    ///
    /// Restating them here is what the sibling applet scan avoids by deriving from
    /// `applet_table`: a roster copied into this file lets `-s` disappear from the
    /// applet while this scan stays green and a `test -s` call site breaks at boot.
    #[cfg(test)]
    fn test_operators(name: &str) -> Vec<String> {
        const SRC: &str = include_str!("../../../td-util/src/test.rs");
        let Some(after) = SRC.split_once(&format!("const {name}: &[&str] = &[")) else {
            return Vec::new();
        };
        let Some((body, _)) = after.1.split_once(']') else {
            return Vec::new();
        };
        body.split(',')
            .filter_map(|tok| {
                let t = tok.trim();
                t.strip_prefix('"')?.strip_suffix('"').map(str::to_string)
            })
            .collect()
    }

    /// Every operator a generated script hands `/bin/td-util test` must be one the
    /// applet SERVES.
    ///
    /// An unserved operator is not a build error and not a loud runtime one either:
    /// the applet reports it on stderr and exits 2, and `if test -w /var; then A; else
    /// B; fi` takes the ELSE branch on a 2 exactly as it does on a 1. So re-adding
    /// `test -w` — the prediction this landing deleted — would silently answer "no"
    /// at every call site while the console scrolled past the reason. A whitelist,
    /// not a `-r`/`-w`/`-x` denylist, so an operator nobody has thought about yet
    /// reds too.
    #[test]
    fn every_scripted_test_operator_is_one_the_applet_serves() {
        let unary = test_operators("UNARY");
        let binary = test_operators("BINARY");
        let refused = test_operators("REFUSED");
        assert!(
            unary.len() >= 5 && binary.len() >= 5 && refused.len() == 3,
            "could not read the operator rosters out of td-util's test.rs \
             ({unary:?} {binary:?} {refused:?}); the scan below would vacuously pass"
        );
        let mut checked = 0usize;
        for (name, text, _) in script_sources() {
            for (idx, _) in text.match_indices("/bin/td-util test ") {
                let Some(rest) = text.get(idx + "/bin/td-util test ".len()..) else {
                    continue;
                };
                let words = test_words(rest);
                let mut words = words.iter().map(String::as_str).peekable();
                if words.peek() == Some(&"!") {
                    words.next();
                }
                let Some(first) = words.next() else {
                    panic!("{name}: `/bin/td-util test` with an empty expression")
                };
                // Mirrors the applet's own dispatch: a leading `-token` is a unary
                // operator, anything else is an operand and the operator follows it.
                if first.starts_with('-') {
                    assert!(
                        unary.iter().any(|op| op == first),
                        "{name} asks `test {first}`, which td-util does not serve — it \
                         exits 2 and every `if` around it silently takes the else branch"
                    );
                } else {
                    let op = words.next().unwrap_or("");
                    assert!(
                        binary.iter().any(|b| b == op),
                        "{name} asks `test {first} {op}`, whose operator td-util does not \
                         serve — it exits 2 and every `if` around it silently takes the \
                         else branch"
                    );
                }
                checked += 1;
            }
        }
        assert!(
            checked >= 16,
            "only {checked} `/bin/td-util test` call sites found (18 today); the scan is not \
             reaching the generated scripts and would pass vacuously"
        );
    }

    /// Every `td-util <applet>` / `td-init <applet>` a generated script invokes must be an
    /// applet that multicall actually serves.
    ///
    /// The busybox scan below has covered its own multiplexer since it existed. When the
    /// nine coreutils names moved to `td-util` and `sync` to `td-init`, those call sites
    /// left that scan's reach and landed under NO check at all: `/bin/td-util cta` would
    /// have shipped, and the first sign would have been a boot script failing at run time.
    /// Same discipline, same failure mode, so the same shape of test — read from each
    /// crate's own APPLETS table, not from a list restated here, since a restated one drifts.
    ///
    /// These are invoked as `td-util <applet>` rather than `/bin/<applet>` because uutils
    /// owns those `/bin` names and the farms are disjoint.
    #[test]
    fn scripted_multicall_applets_are_served() {
        for (token, source) in [
            ("td-util", include_str!("../../../td-util/src/main.rs")),
            ("td-init", include_str!("../../../td-init/src/main.rs")),
        ] {
            let served = applet_table(source);
            assert!(
                served.len() > 1,
                "could not read {token}'s APPLETS table; the scan below would vacuously pass"
            );
            let mut calls = 0usize;
            for (name, text, _) in script_sources() {
                for (idx, _) in text.match_indices(token) {
                    let before = text.get(..idx).unwrap_or("");
                    if !before.ends_with("/bin/") {
                        // Not a call through the absolute path. Unlike `busybox`, this
                        // token is also a store-path component, a marker name and a
                        // word in diagnostics, so the busybox scan's blanket refusal
                        // would reject prose that is not an invocation at all. What
                        // must not escape is a bare token in COMMAND position, which
                        // would resolve through $PATH unchecked — so that is what is
                        // asserted, rather than skipping every non-`/bin/` mention.
                        assert!(
                            !in_command_position(before),
                            "{name} runs `{token}` as a bare token rather than \
                             `/bin/{token} <applet>`. In command position that resolves \
                             through $PATH, invisibly to this scan, so its applet would \
                             never be checked against the roster; write the absolute form"
                        );
                        continue;
                    }
                    let Some(rest) = text.get(idx + token.len()..) else {
                        continue;
                    };
                    let rest = rest.trim_start();
                    let applet: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                        .collect();
                    // Fail CLOSED on a form this cannot resolve statically, for the reason
                    // the busybox scan gives: the one call that most needs review is
                    // exactly the one an unresolvable form would let through.
                    assert!(
                        !applet.is_empty(),
                        "{name} invokes /bin/{token} with a form this scan cannot resolve \
                         ({:?}); invoke the applet under a literal name",
                        rest.chars().take(24).collect::<String>()
                    );
                    // `--list` is the multicall's own flag, not an applet. Named
                    // explicitly rather than skipping every dash-led operand, so a
                    // mistyped flag still reds instead of being read as "not an applet".
                    if applet.starts_with('-') {
                        assert_eq!(
                            applet, "--list",
                            "{name} invokes /bin/{token} with an unknown multicall flag"
                        );
                        continue;
                    }
                    assert!(
                        served.iter().any(|a| a == &applet),
                        "{name} invokes `{token} {applet}`, which {token} does not serve \
                         (it serves {served:?})"
                    );
                    calls = calls.saturating_add(1);
                }
            }
            // A scan that matched nothing proves nothing. Both multicalls ARE called by
            // the generated scripts, so zero here means the spelling moved and the check
            // silently stopped looking.
            assert!(calls > 0, "no /bin/{token} <applet> call sites found; the scan is inert");
        }
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
                seen.iter().any(|s| s == a),
                "INITRAMFS_APPLETS lists '{a}', but no generated script invokes `busybox {a}` \
                 - drop it, or move it to the /bin farm if it needs a symlink"
            );
        }
    }

    /// `awk` is DROPPED now, and this is what keeps it dropped: no generated script
    /// invokes it, and re-introducing a call would put a boot-path text decision back on
    /// a farm the td-txt corpus does not cover. The ban matters more since the applet
    /// left — a script naming `awk` used to run busybox's, and would now simply fail at
    /// run time, on the boot path, with nothing red at build time.
    ///
    /// The assertion is the whole SUBSTRING, not a scan of call spellings, because
    /// deciding "is this `awk` in command position" needs a shell grammar and every
    /// approximation of one leaves a spelling that escapes: `/bin/awk` (being in the farm
    /// is what makes that symlink real, and `/bin/<applet>` is the spelling d95578ce
    /// endorsed for grep/sed), `/bin/busybox  awk` on the whitespace, and a bare `awk`
    /// found through the PATH td-init exports. No generated script contains the three
    /// letters at all, so banning them outright costs nothing and cannot be evaded. A
    /// script that some day needs them for an unrelated word reds here and gets a
    /// deliberate decision instead of silently reopening the hole.
    #[test]
    fn no_generated_script_invokes_awk() {
        for (name, text, _) in script_sources() {
            assert!(
                !text.contains("awk"),
                "{name} contains `awk`. The image's text predicates run on td-txt, whose \
                 conformance corpus is what makes a wrong answer a red build; awk has no \
                 such corpus. Express it with grep/sed, or land an awk corpus first"
            );
        }
    }

    /// The ERE that replaced rootcheck's awk field test, pinned because its correctness is
    /// not local: `ro` must match as a whole comma-delimited option, so `errors=remount-ro`
    /// and `rootcontext=…` must NOT satisfy it. The truth table lives where it can actually
    /// be EXECUTED — the td-txt corpus runs this pattern against the real binary — so this
    /// asserts BOTH ends carry it. One end alone is not a pin: the script and the corpus
    /// could drift apart with each suite still green, leaving the corpus scoring a pattern
    /// the image no longer runs.
    #[test]
    fn rootcheck_pins_the_td_volume_ere() {
        // The quoted pattern is the substring the two ends SHARE: rootcheck spells it
        // `/bin/grep -Eq <PATTERN> /proc/mounts`, a corpus case `grep -Eq <PATTERN> mounts`.
        const PATTERN: &str = "'^[^ ]+ /run/td-volume btrfs ([^ ]*,)?ro(,[^ ]*)?( |$)'";
        const CORPUS: &str = include_str!("../../../td-txt/spec/grep-cli.test.txt");
        const OVERLAY: &str = include_str!("../../../td-txt/spec/expectations.txt");
        const SCORED: usize = 17;
        let text = build_rootcheck(&SYSTEM);
        assert!(
            text.contains(&format!("/bin/grep -Eq {PATTERN} /proc/mounts")),
            "rootcheck no longer runs the td-volume ERE the td-txt corpus scores.\n\
             want: /bin/grep -Eq {PATTERN} /proc/mounts\ngot:\n{text}"
        );
        // `## argv:` lines, not raw occurrences: only an argv line is a case the harness
        // runs, so counting text would keep this green with the whole block commented out.
        let scored = CORPUS
            .lines()
            .filter(|l| l.starts_with("## argv:") && l.contains(PATTERN))
            .count();
        assert!(
            scored >= SCORED,
            "td-txt/spec/grep-cli.test.txt runs the td-volume ERE in {scored} case(s), want \
             at least the {SCORED} `rootcheck td-volume` ones. The image decides boot health \
             with this pattern and the corpus is the only thing that executes it"
        );
        // ...and each must still be RUN and ASSERTED. The overlay tolerates a case by name,
        // so an `xfail`/`skip` entry would retire the boot-critical predicate from scoring
        // with the count above, both suites, and the whole gate still green.
        for line in OVERLAY.lines() {
            let entry = line.trim();
            assert!(
                entry.starts_with('#') || !entry.contains("rootcheck td-volume"),
                "td-txt/spec/expectations.txt tolerates a `rootcheck td-volume` case \
                 ({entry:?}) - the predicate the image decides boot health with would stop \
                 being scored while everything stayed green"
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

    /// Read a `const NAME: &str = "…";` value out of td-firstboot's own source — the
    /// text the recipe embeds and rustc compiles, so this is what the shipped binary
    /// will actually use rather than a second declaration that could drift.
    fn firstboot_const(name: &str) -> Option<&'static str> {
        const FIRSTBOOT_MAIN_RS: &str = include_str!("../../../td-firstboot/src/main.rs");
        let (_, after) = FIRSTBOOT_MAIN_RS.split_once(&format!("const {name}: &str = \""))?;
        let (value, _) = after.split_once('"')?;
        Some(value)
    }

    /// The table and td-firstboot are two crates that must agree on four paths, and
    /// nothing in the type system makes them: the image points `/etc` symlinks at
    /// `/var/lib/td/...` while the provisioner independently decides where to write.
    /// Disagree and the symlinks dangle forever — the machine boots, `/etc` looks
    /// right, and every identity file is missing.
    #[test]
    fn firstboot_and_the_mutable_etc_table_agree_on_every_path() {
        assert_eq!(
            firstboot_const("DEFAULT_STATE_DIR"),
            Some(STATE_DIR),
            "td-firstboot writes its state somewhere other than where MUTABLE_ETC points"
        );
        // Each persistent entry's target must be exactly the state dir joined with the
        // relative path td-firstboot declares for it.
        for (name, etc) in [
            ("MACHINE_ID", "machine-id"),
            ("HOST_KEY", "ssh/ssh_host_ed25519_key"),
            ("HOST_KEY_PUB", "ssh/ssh_host_ed25519_key.pub"),
            ("AUTHORIZED_KEYS", "ssh/authorized_keys"),
        ] {
            let relative = firstboot_const(name);
            assert_eq!(
                relative,
                Some(etc),
                "td-firstboot's `{name}` is not the /etc name MUTABLE_ETC uses"
            );
            let entry = MUTABLE_ETC
                .iter()
                .find(|entry| entry.etc == etc)
                .unwrap_or_else(|| unreachable!("MUTABLE_ETC has no entry for {etc}"));
            assert_eq!(
                entry.target,
                format!("{STATE_DIR}/{etc}"),
                "/etc/{etc} points somewhere other than td-firstboot's state dir"
            );
            assert_eq!(
                entry.state,
                State::Persistent,
                "/etc/{etc} is provisioned by td-firstboot into /var, so it cannot be volatile"
            );
        }
        // Every PERSISTENT entry must be one td-firstboot actually creates: a table
        // entry with no provisioner is a symlink that dangles for the life of the
        // machine, which is worse than no entry at all.
        let provisioned = ["MACHINE_ID", "HOST_KEY", "HOST_KEY_PUB", "AUTHORIZED_KEYS"]
            .iter()
            .filter_map(|name| firstboot_const(name))
            .collect::<Vec<_>>();
        for entry in MUTABLE_ETC.iter().filter(|e| e.state == State::Persistent) {
            assert!(
                provisioned.contains(&entry.etc),
                "/etc/{} is persistent state but nothing in td-firstboot provisions it",
                entry.etc
            );
        }
    }

    /// The table's invariants, including the ones that keep it safe to interpolate
    /// unquoted into `shape_check`'s and `rootcheck`'s generated shell.
    #[test]
    fn mutable_etc_paths_are_shell_safe_and_well_formed() {
        assert!(!MUTABLE_ETC.is_empty());
        for entry in MUTABLE_ETC {
            let etc = entry.etc;
            assert!(
                !etc.is_empty() && !etc.starts_with('/') && !etc.ends_with('/'),
                "/etc/{etc}: the table stores a path RELATIVE to /etc"
            );
            assert!(
                !etc.contains("..") && !etc.contains("//"),
                "/etc/{etc}: a traversal or empty component would escape /etc"
            );
            assert!(
                !etc.starts_with('-') && !etc.contains("/-"),
                "/etc/{etc}: a leading '-' in any component is read as an OPTION by the \
                 `grep`/`test` the generated shell runs on it"
            );
            // At most one directory level: `etc_globs` sweeps exactly the levels the
            // table declares, and a deeper path would be swept by nothing.
            assert!(
                etc.matches('/').count() <= 1,
                "/etc/{etc} is nested deeper than one directory, so the unreviewed-symlink \
                 sweep in shape_check would not reach it"
            );
            for (label, path) in [("etc", etc), ("target", entry.target)] {
                assert!(
                    path.bytes().all(|b| b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b'_' | b'-' | b'/')),
                    "{label} {path:?} has a character that is not safe unquoted in the \
                     generated shell (or that td-builder's ASCII config reader would mangle)"
                );
            }
            assert!(
                entry.target.starts_with('/'),
                "/etc/{etc} must point at an ABSOLUTE path: a relative symlink out of /etc \
                 would resolve against /etc itself"
            );
            let wanted_root = match entry.state {
                // Volatile state must be on the /run tmpfs, which starts every boot
                // empty — that is what makes it volatile rather than merely mutable.
                State::Volatile => "/run/",
                State::Persistent => "/var/",
            };
            assert!(
                entry.target.starts_with(wanted_root),
                "/etc/{etc} is {:?} state but points at {} instead of {wanted_root}",
                match entry.state {
                    State::Volatile => "volatile",
                    State::Persistent => "persistent",
                },
                entry.target
            );
            assert!(
                entry.why.len() > 20,
                "/etc/{etc} needs a real reason it cannot be image content - every hole in \
                 the read-only /etc is an individually reviewed decision"
            );
        }
        // Duplicates would stage two symlinks at one path (last wins) and make the
        // allowlist sweep pass on a name nobody meant to review twice.
        let mut names = mutable_etc_names();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(count, names.len(), "MUTABLE_ETC lists a path twice");
        let mut targets: Vec<&str> = MUTABLE_ETC.iter().map(|e| e.target).collect();
        targets.sort_unstable();
        targets.dedup();
        assert_eq!(
            count,
            targets.len(),
            "two MUTABLE_ETC entries share a target, so one /etc name silently shadows another"
        );
    }

    /// A `MUTABLE_ETC` entry must not collide with generated image config: the same
    /// name cannot be both a regular file in the erofs and a symlink out of it, and
    /// whichever step ran last would decide which — silently.
    #[test]
    fn no_mutable_etc_entry_shadows_a_generated_etc_file() {
        for (generated, _, _) in etc_files(&SYSTEM) {
            assert!(
                !mutable_etc_names().contains(&generated),
                "/etc/{generated} is both generated image config and a MUTABLE_ETC entry"
            );
        }
    }

    /// The staged tree must carry one symlink per table entry, with the recorded
    /// target, and the directory to hold it. Asserted on the STEPS, so a table entry
    /// that never became a staging step reds here rather than in a full image build.
    #[test]
    fn every_mutable_etc_entry_is_staged_as_a_symlink() {
        let steps = real_root_steps(&SYSTEM);
        for entry in MUTABLE_ETC {
            let link = format!("{{root}}/real-root/etc/{}", entry.etc);
            assert!(
                steps.iter().any(|step| matches!(
                    step,
                    Step::Symlink { target, link: at } if at == &link && target == entry.target
                )),
                "nothing stages /etc/{} as a symlink to {}",
                entry.etc,
                entry.target
            );
            // …and NOT as a file: a WriteFile at the same path would win or lose
            // depending on step order.
            assert!(
                !steps.iter().any(|step| matches!(
                    step,
                    Step::WriteFile { path, .. } if path == &link
                )),
                "/etc/{} is written as a regular file as well as symlinked",
                entry.etc
            );
        }
        for dir in mutable_etc_dirs() {
            let path = format!("{{root}}/real-root/etc/{dir}");
            assert!(
                steps
                    .iter()
                    .any(|step| matches!(step, Step::MkDir { path: at } if at == &path)),
                "/etc/{dir} is never created, so the symlinks it holds cannot be staged"
            );
        }
    }

    /// The sshd service must read its identity and its authorization from per-machine
    /// state, not from image content. Both paths it names have to be table entries —
    /// otherwise a rebuild is the only way to rotate a host key or grant access, and
    /// every machine booting the image shares both.
    #[test]
    fn the_sshd_service_reads_only_mutable_etc_paths() {
        let exec = unit_key("sshd", "exec").unwrap_or_else(|| unreachable!("no sshd unit"));
        for path in [SSHD_HOST_KEY, SSHD_AUTHORIZED_KEYS] {
            let relative = path
                .strip_prefix("/etc/")
                .unwrap_or_else(|| unreachable!("{path} must be under /etc"));
            assert!(
                mutable_etc_names().contains(&relative),
                "the sshd service reads {path}, which is not a reviewed MUTABLE_ETC entry"
            );
        }
        assert!(
            exec.contains(&format!("--host-key {SSHD_HOST_KEY} ")),
            "the sshd unit's exec= must present the per-machine host key; without --host-key \
             it falls back to the PUBLIC committed builtin key whenever no client is authorized"
        );
        assert!(
            exec.contains(&format!("--authorized-keys {SSHD_AUTHORIZED_KEYS}")),
            "the sshd unit's exec= must authorize from per-machine state"
        );
    }

    /// td-firstboot mints the per-machine identity, so it must be ordered BEFORE
    /// everything that reads or checks what it writes.
    ///
    /// This used to compare line positions in the inittab, because "sysinit runs in
    /// table order" made position the whole guarantee. It is now a declared edge, and
    /// the transitive form is what matters: sshd is ordered behind the identity whether
    /// it names td-firstboot directly or reaches it through netup and rootcheck.
    #[test]
    fn firstboot_provisions_before_anything_reads_the_identity() {
        for (label, later) in [
            // rootcheck asserts the identity is readable THROUGH the /etc symlinks.
            ("rootcheck", "rootcheck"),
            // td-netd writes the volatile /run targets; ordering here is only about
            // keeping the identity ahead of the network coming up.
            ("netup", "netup"),
            // sshd reads --host-key and --authorized-keys.
            ("sshd", "sshd"),
        ] {
            assert!(
                ordered_before("td-firstboot", later),
                "{label} is not ordered after td-firstboot, so it can run before the \
                 identity it reads has been minted"
            );
        }
        assert_eq!(
            unit_key("td-firstboot", "type").as_deref(),
            Some("oneshot"),
            "td-firstboot must be a oneshot: a daemon releases its dependents when it \
             SPAWNS, and the identity is not minted until it EXITS"
        );
    }

    #[test]
    fn uutils_expansion_is_shipped_and_behavior_probed() {
        let mut probed = std::collections::BTreeSet::new();
        for probe in UUTILS_BEHAVIOR_PROBES {
            let applet = probe.applet();
            assert!(
                UUTILS_APPLETS.contains(&applet),
                "probed applet '{applet}' is not shipped in the uutils /bin farm"
            );
            assert!(
                probed.insert(applet),
                "uutils applet '{applet}' has duplicate behavior probes"
            );
        }

        let bootsuccess = build_bootsuccess(&SYSTEM);
        let mut rendered_probes = String::new();
        for probe in UUTILS_BEHAVIOR_PROBES {
            let applet = probe.applet();
            let segment = uutils_behavior_probe(probe);
            rendered_probes.push_str(&segment);
            assert!(
                !segment.contains('\''),
                "the /bin/{applet} probe would break the enclosing single-quoted su script"
            );
            assert!(
                bootsuccess.contains(&segment),
                "the health target does not carry the generated /bin/{applet} probe"
            );
            if let UutilsProbe::Output { args, expected, .. } = probe {
                for (label, value) in [("argument", *args), ("expected output", *expected)] {
                    assert!(
                        value.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'/' | b'-' | b'.' | b'_')
                        }),
                        "/bin/{applet} {label} is not a shell-literal-safe token: {value:?}"
                    );
                }
            }
        }
        for (contract, expected) in [
            (
                "basename exact output",
                "if o=$(/bin/basename /tmp/td-uutils-probe/source 2>&1); then \
                 [ \"$o\" = \"source\" ]",
            ),
            (
                "dirname exact output",
                "if o=$(/bin/dirname /tmp/td-uutils-probe/source 2>&1); then \
                 [ \"$o\" = \"/tmp/td-uutils-probe\" ]",
            ),
            ("true zero status", "/bin/true ||"),
            ("false captured status", "o=$(/bin/false 2>&1); s=$?;"),
            (
                "false exact status and output",
                "if [ \"$s\" != 1 ] || [ -n \"$o\" ]; then",
            ),
            ("printenv exact lookup", "/bin/printenv TD_UUTILS_PROBE"),
            (
                "hard-link creation",
                "/bin/link /tmp/td-uutils-probe/source /tmp/td-uutils-probe/hard",
            ),
            (
                "hard link is not a symlink",
                "if [ -h /tmp/td-uutils-probe/hard ]; then",
            ),
            (
                "hard-link identity",
                "if o=$(/bin/cat /tmp/td-uutils-probe/hard 2>&1); then \
                 if [ \"$o\" = td-uutils-after ]; then h=1",
            ),
            (
                "unlink removal",
                "/bin/unlink /tmp/td-uutils-probe/hard",
            ),
            (
                "unlink entry absence",
                "if [ -e /tmp/td-uutils-probe/hard ] || \
                 [ -h /tmp/td-uutils-probe/hard ]; then",
            ),
            (
                "unlink source preservation",
                "if o=$(/bin/cat /tmp/td-uutils-probe/source 2>&1); then \
                 [ \"$o\" = td-uutils-after ]",
            ),
        ] {
            assert!(
                bootsuccess.contains(expected),
                "the uutils probe does not enforce {contract}"
            );
        }
        let link = UUTILS_BEHAVIOR_PROBES
            .iter()
            .position(|probe| matches!(probe, UutilsProbe::Link));
        let unlink = UUTILS_BEHAVIOR_PROBES
            .iter()
            .position(|probe| matches!(probe, UutilsProbe::Unlink));
        assert!(
            matches!((link, unlink), (Some(link), Some(unlink)) if link < unlink),
            "the link probe must create the hard link before unlink consumes it"
        );
        let gated_probes = format!(
            "if /bin/su -s /bin/sh {user} -c \
             'u=1; h=0; /bin/cat /etc/os-release >/dev/null 2>&1 || \
             {{ echo \"uutils: /bin/cat failed\"; u=0; }}; \
             /bin/rm -rf /tmp/td-uutils-probe; \
             /bin/mkdir /tmp/td-uutils-probe || \
             {{ echo \"uutils: /bin/mkdir could not create probe directory\"; u=0; }}; \
             {rendered_probes}/bin/rm -rf /tmp/td-uutils-probe || \
             {{ echo \"uutils: /bin/rm could not remove probe directory\"; u=0; }}; \
             [ \"$u\" = 1 ]'; then \
             [ \"$mu\" = 1 ] || {{ echo {UUTILS_RUNTIME_MARKER}; mu=1; }}; \
             else healthy=0; fi;",
            user = SYSTEM.autologin
        );
        assert!(
            bootsuccess.contains(&gated_probes),
            "the complete unprivileged behavior probe must gate the uutils marker"
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
        // These moved out of the inittab and into the unit table; each must still be
        // there, running the same command, or the cutover dropped a job.
        for (unit, exec) in [
            ("greeter", "/etc/tty-session"),
            ("rootcheck", "/etc/rootcheck"),
            ("netup", "/etc/netup"),
            ("bootsuccess", "/etc/bootsuccess"),
            ("bootfail", "/etc/bootfail"),
        ] {
            assert_eq!(
                unit_key(unit, "exec").as_deref(),
                Some(exec),
                "the {unit} unit must run {exec}"
            );
        }
        assert_eq!(
            unit_key("greeter", "tty").as_deref(),
            Some("ttyS0"),
            "the greeter must be handed ttyS0, as `ttyS0::respawn:` did"
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
        // getty must gate the reboot (`&&`), so a FAILED session respawns rather than tearing
        // a live system down and masking a broken greeter as a clean exit-0 shutdown. This is
        // the only thing keeping "the greeter never started" and "the user logged out"
        // distinguishable.
        assert!(
            session.contains("/bin/getty ")
                && session.contains("-l /etc/autologin ")
                && session.contains("&& exec /bin/td-svc reboot")
                && !session.contains("reboot -f"),
            "tty-session must run getty (autologin at /etc/autologin) then, only on success, \
             ask td-svc to reboot, while a failed login retries"
        );
        // The wrapper must not tear the system down itself: td-svc owns the ordered teardown,
        // and a second copy of that sequence here would reset the machine with services still
        // running.
        assert!(
            !session.contains("/etc/shutdown"),
            "the greeter wrapper must delegate the teardown to td-svc, not inline /etc/shutdown"
        );
        // td-svc's own diagnostics must reach /dev/console, not the hung-up tty. This runs
        // AFTER the greeter shell — the ttyS0 session leader — exited, so the kernel has
        // vhangup'd that terminal and writes through the inherited descriptor return EIO.
        // Without this a refused or unreachable control socket is silent and the machine just
        // sits there. Observed as a reboot with no output at all, not theorised.
        assert!(
            session.contains("exec /bin/td-svc reboot >/dev/console 2>&1"),
            "the reboot client must write to /dev/console, not the hung-up tty it inherits \
             from the ended login session - otherwise its errors go to a descriptor returning \
             EIO and a greeter that cannot reach td-svc looks like a hang with no explanation"
        );
        let shutdown = build_shutdown();
        assert!(
            shutdown.contains("/bin/td-init sync || {")
                && shutdown.contains("/bin/umount /var || {")
                && shutdown.contains("/bin/umount -a -r --exclude /run || {")
                && shutdown.contains("/bin/td-util test \"$ok\" = 1")
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
                && init.contains("/bin/umount /proc")
                && init.contains("/bin/umount /dev"),
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
            // rootcheck's write probe embeds the home inside DOUBLE quotes
            // (`sh -c ": > <home>/.tdwr"`), where these two are live where the
            // old single-quoted `test -w <home>` made them inert.
            "/home/a$b",
            "/home/a\"b",
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
        // The two negative probes must ABORT the su script on success (`&& exit 1`)
        // and the positive one on failure (`|| exit 1`); a probe whose status is
        // discarded proves nothing.
        assert!(
            rootcheck.contains(SYSTEM_STATE_OWNER_MARKER)
                && rootcheck.contains("/bin/sh -c \": > /var/.tdwr-su\" 2>/dev/null && exit 1")
                && rootcheck
                    .contains("/bin/sh -c \": > /var/root/.tdwr-su\" 2>/dev/null && exit 1")
                && rootcheck.contains(".tdwr-su\" 2>/dev/null || exit 1"),
            "rootcheck must prove the login user cannot own system state by WRITING"
        );
        // The pre-clear must come BEFORE the su, and its failure must count. A stale
        // root-owned probe file makes the unprivileged write fail with EACCES even
        // where `/var` is world-writable, and that failure reads as a pass — so
        // moving both clears after the su, or letting one fail quietly, reopens the
        // hole while leaving the assertion above green.
        let clear = "/bin/td-util rm -f /var/.tdwr-su /var/root/.tdwr-su";
        let (Some(first_clear), Some(su), Some(last_clear)) = (
            rootcheck.find(clear),
            rootcheck.find("if /bin/su -s /bin/sh"),
            rootcheck.rfind(clear),
        ) else {
            panic!("rootcheck lost either the probe clears or the su block")
        };
        assert!(
            first_clear < su && last_clear > su,
            "the probe files must be cleared on BOTH sides of the su ({first_clear} \
             {su} {last_clear})"
        );
        assert_eq!(
            rootcheck.matches(&format!("{clear} /home/tester/.tdwr-su || ok=0")).count(),
            2,
            "a clear that fails is the one case the clear exists for; it must set ok=0"
        );
        // `-w` must not come back. It is access(2), which no td multicall serves, and
        // the mode-bits stand-in reads root's own bit on 0755 `/var`. Spelled with the
        // surrounding characters so a `grep -w` elsewhere is not what trips it.
        assert!(
            !rootcheck.contains(" -w /") && !rootcheck.contains(" -w \""),
            "rootcheck must not ask `-w`: the write attempts above replaced that prediction"
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
                && rootcheck.contains("&& /bin/td-init sync"),
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
                && bootfail.contains("exec /bin/td-svc reboot >/dev/console 2>&1")
                && !bootfail.contains("/etc/shutdown")
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
                && profile.contains("while :; do /bin/td-util sleep 300; done"),
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
            profile.contains("export XDG_RUNTIME_DIR=/run/user/1000")
                && profile.contains("export WAYLAND_DISPLAY=wayland-0"),
            "the graphical login environment must name the seat-owned runtime \
             directory and compositor socket"
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
            "dir /sys 0755 0 0",
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
        // The mirror image is a NAME, not a payload. Both phases pack td-init (both mount
        // devtmpfs and proc before anything else), so the capability line is drawn at the
        // `/bin/switch_root` symlink: the selector has no branch that enters a root and must
        // not carry the applet that would.
        for (phase, spec) in [("selector", &selector), ("deployment", &deployment)] {
            assert!(
                spec.contains(
                    "file {in:td-init}/bin/td-init {in:td-init}/bin/td-init 0755 0 0"
                ) && spec.contains("slink /bin/mount {in:td-init}/bin/td-init 0777 0 0")
                    && spec.contains("slink /bin/umount {in:td-init}/bin/td-init 0777 0 0"),
                "the {phase} initramfs must pack td-init and expose its mount pair - its \
                 /init mounts devtmpfs and proc before it does anything else"
            );
        }
        // The read-back that makes the root loop's read-only status a CHECKED fact
        // rather than an assumed one reads /sys/dev/block/<maj>:<min>/ro, so sysfs
        // has to be mounted before `td-boot root-loop` runs. Asserted here because
        // the symptom otherwise is an ENOENT that stops the boot, a layer away from
        // this decision. Deployment only: the selector never binds a loop, so it
        // mounts no sysfs and the pair is pinned together — a sysfs that reappeared
        // there would be a mount nothing reads.
        assert!(
            deployment.contains("dir /sys 0755 0 0"),
            "the deployment initramfs has no /sys for sysfs to be mounted on"
        );
        let init = build_deployment_init(&SYSTEM);
        let sysfs = init
            .match_indices("mount -t sysfs sysfs /sys")
            .next()
            .map(|(at, _)| at);
        let boot = init.match_indices("/bin/td-boot").next().map(|(at, _)| at);
        assert!(
            sysfs.is_some(),
            "/init must mount sysfs; losetup's read-back has nothing to read otherwise"
        );
        assert!(
            boot.is_none() || sysfs < boot,
            "sysfs must be mounted BEFORE td-boot runs, not after"
        );
        // The loop node is created INTO the devtmpfs, so the mount must come first:
        // before it, the node lands in the initramfs rootfs and is then shadowed by
        // the mount, leaving td-boot with nothing to open and no error saying why.
        let devtmpfs = init
            .match_indices("mount -t devtmpfs dev /dev")
            .next()
            .map(|(at, _)| at);
        let mknod = init.match_indices("/bin/mknod ").next().map(|(at, _)| at);
        assert!(
            devtmpfs.is_some() && mknod.is_some(),
            "/init must mount devtmpfs and create the loop node"
        );
        assert!(
            devtmpfs < mknod && mknod < boot,
            "mknod must run AFTER the devtmpfs mount and BEFORE td-boot binds the loop"
        );
        // ...and released before the pivot, with /proc and /dev. switch_root MOVES
        // whatever of the API mounts it finds, so leaving this one behind stacks a
        // second sysfs under the one sysinit mounts on the real root.
        let released = |m: &str| {
            init.match_indices(&format!("/bin/umount {m}"))
                .next()
                .map(|(at, _)| at)
        };
        for api in ["/proc", "/dev", "/sys"] {
            let at = released(api);
            assert!(
                at.is_some() && at < init.match_indices("switch_root").next().map(|(p, _)| p),
                "the deployment /init must umount {api} before it pivots"
            );
        }
        assert!(
            !build_selector_init().contains("sysfs"),
            "the selector binds no loop, so it must mount no sysfs"
        );
        assert!(
            deployment.contains("slink /bin/switch_root {in:td-init}/bin/td-init 0777 0 0")
                && !selector.contains("switch_root"),
            "only the deployment initramfs may expose the pivot as /bin/switch_root"
        );
        assert!(
            deployment.contains("slink /bin/mknod {in:td-init}/bin/td-init 0777 0 0")
                && !selector.contains("mknod"),
            "only the deployment initramfs creates /dev/loop0, so only it may carry mknod"
        );
        // td-boot invokes NO busybox applet now: `losetup` was the last, and it is a
        // td-init applet since the LOOP_SET_FD amendment. Asserted rather than left to
        // review, because re-introducing one would put the ability to boot back on a
        // third-party multicall's argv parsing with no build-time signal.
        assert!(
            !INITRAMFS_APPLETS.contains(&"losetup"),
            "losetup is td-init's now; a busybox entry for it would be the dependency \
             the amendment removed, silently restored"
        );
        // td-boot calls mount/umount/losetup by their /bin names, so each must be a
        // td-init farm entry AND actually linked into the cpio of every phase that
        // runs it. Which phases those are is NOT uniform: `td-boot boot` and
        // `td-boot root-loop` mount, but only `root-loop` binds the loop, and only
        // the deployment /init runs it.
        let td_init = td_init_applets();
        for applet in td_boot_protocol::REQUIRED_TD_INIT_APPLETS {
            assert!(
                td_init.contains(applet),
                "td-boot invokes /bin/{applet}, which the td-init farm does not serve"
            );
        }
        for applet in [
            td_boot_protocol::MOUNT_APPLET,
            td_boot_protocol::UMOUNT_APPLET,
        ] {
            for (phase, spec) in [("selector", &selector), ("deployment", &deployment)] {
                assert!(
                    spec.contains(&format!("slink /bin/{applet} ")),
                    "the {phase} initramfs does not link /bin/{applet}, which td-boot runs there"
                );
            }
        }
        let losetup = format!("slink /bin/{} ", td_boot_protocol::LOSETUP_APPLET);
        assert!(
            deployment.contains(&losetup) && !selector.contains(&losetup),
            "the loop bind is a deployment capability: `td-boot root-loop` runs only there, \
             and the selector must not carry the name that would let it bind one"
        );
    }

    /// td-txt is packed, serves both /bin names, and each is exercised by the greeter — the
    /// same three-part contract td-util's test below states, with one addition that is the
    /// point of this farm: the grep probe must be DISCRIMINATING. `/etc/rootcheck` decides
    /// the boot is healthy with `/bin/grep -Eq`, so a grep that selected every line would
    /// pass a "did it exit 0" probe while calling a writable root read-only. The probe
    /// therefore asserts a match AND a non-match, and this test pins both halves.
    #[test]
    fn td_txt_serves_its_farm_and_the_grep_probe_discriminates() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-txt}" && dest == "{root}/real-root{in:td-txt}"
            )),
            "td-txt must be CopyTree'd into the real root (static, empty closure)"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-txt}/bin/td-txt"
                        && link == "{root}/real-root/bin/td-txt"
            )),
            "/bin/td-txt must symlink into the store td-txt package"
        );
        assert!(
            !TD_TXT_APPLETS.is_empty(),
            "an empty farm would make every assertion below, and shape_check's own farm \
             loop, silently vacuous"
        );
        // Exactly one claimant per link, for the reason spelled out in the td-util test.
        for applet in TD_TXT_APPLETS {
            let link = format!("{{root}}/real-root/bin/{applet}");
            let targets: Vec<&str> = steps
                .iter()
                .filter_map(|s| match s {
                    Step::Symlink { target, link: l } if *l == link => Some(target.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                targets.as_slice(),
                ["{in:td-txt}/bin/td-txt"],
                "/bin/{applet} must resolve to the staged td-txt multicall, and to nothing else"
            );
        }
        // Match the WHOLE generated invocation, not just the name: every diagnostic
        // below also says `/bin/grep`, so a name-only assertion is satisfied by the
        // `echo` alone — delete the command, keep the message, and the test still
        // passes. The td-init test documents the same trap.
        let probes = build_td_txt_probes();
        for invocation in [
            r#"/bin/grep -Eq "^[^ ]+ / erofs ro[, ]" /proc/mounts ||"#,
            r#"s=$(/bin/sed -n "s/^ID=//p" /etc/os-release) ||"#,
            r#"n=$(/bin/sed -n "2p" /etc/os-release) ||"#,
        ] {
            assert!(
                probes.contains(invocation),
                "the greeter never runs `{invocation}`, so that applet's symlink and \
                 argv[0] dispatch would ship unexercised"
            );
        }
        for applet in TD_TXT_APPLETS {
            assert!(
                probes.contains(&format!("/bin/{applet} ")),
                "no probe names /bin/{applet} at all"
            );
        }
        // The negative leg. Without it the probe is satisfied by a grep that matches
        // everything — precisely the failure that would let rootcheck pass a rw root.
        assert!(
            probes.contains(r#"if /bin/grep -Eq "^[^ ]+ / erofs rw[, ]" /proc/mounts; then"#),
            "the grep probe must also assert a NON-match; a match-only probe cannot tell a \
             working grep from one that selects every line"
        );
        // Both legs must be able to fail the marker, or the probe is decoration.
        assert_eq!(
            probes.matches("t=0").count(),
            6,
            "each probe leg needs its own `t=0': a leg that cannot clear the flag reports \
             success no matter what the applet did"
        );
        let bootsuccess = build_bootsuccess(&SYSTEM);
        assert!(
            bootsuccess.contains(&format!("echo {TD_TXT_RUNTIME_MARKER}")),
            "the greeter must print the td-txt marker the boot oracle waits for"
        );
    }

    /// The greeter wraps the probes in `su -s /bin/sh USER -c '\u{2026}'`, so a single quote
    /// anywhere in them closes that argument and scatters the rest into the OUTER shell
    /// as stray words: `grep -Eq "^[^ ]+ / erofs ro[, ]"` becomes `grep -Eq ^[^` plus
    /// five loose arguments, `$t` is set in the wrong shell, and the marker can never be
    /// earned. The result still PARSES, so `sh -n` greens it and only the qemu boot
    /// oracle — a whole-image check — would ever notice. Hence a unit test.
    ///
    /// Two halves, and both are needed: the probes carry no single quote, and they sit
    /// immediately after the opening quote of the `su -c` argument. The second is what
    /// makes the first a proof rather than a convention about a wrapper nothing pins.
    #[test]
    fn td_txt_probes_survive_the_greeters_quoting() {
        let probes = build_td_txt_probes();
        assert!(
            !probes.contains('\''),
            "a single quote in the td-txt probes would close the greeter's `su -c` \
             argument: {probes}"
        );
        assert!(
            build_bootsuccess(&SYSTEM).contains(&format!("-c '{probes}")),
            "the greeter must run the td-txt probes verbatim inside its `su -c` argument"
        );
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
            let args = td_util_probe_args(applet);
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
        // A pager with no operand reads stdin. Deriving the args above means the
        // assertion follows whatever the generator does, so state the one property
        // the derivation cannot: that `less` is given something to page.
        assert!(
            !td_util_probe_args("less").trim().is_empty(),
            "the less probe must carry an operand: with none it reads STDIN, and the \
             probe would then pass or hang depending on what td-svc happens to wire \
             /dev/stdin to for a unit with no tty"
        );
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
        // recipe-checks tier, so nothing else here would notice their deletion.
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

    /// td-login is packed, owns `/bin/{login,su}`, and the credential switch it performs is
    /// VERIFIED on the image rather than assumed.
    ///
    /// This farm differs from td-util's and td-init's in what its failures look like. A dead
    /// `/bin/login` or `/bin/su` fails the boot outright — nothing reaches a greeter and no
    /// unprivileged health leg runs — so the SUCCESS path needs no synthetic probe and gets
    /// none. The failure worth a test is the opposite: a switch that started a perfectly
    /// working session while leaving a residual credential attached. `setuid(2)` issued
    /// before `setgroups(2)` drops the uid and keeps root's supplementary groups; every other
    /// marker on this image still prints. So the assertions here are about the READBACK —
    /// that the health target actually runs it, through `su`, with the credentials the
    /// shipped /etc/{passwd,group} imply, and clears the marker gate when it disagrees.
    #[test]
    fn td_login_serves_its_farm_and_the_credential_switch_is_verified() {
        // THREAT-MODEL.md section 4 says td-login is never installed setuid-root, and
        // creds::apply refuses to switch unless all four uid columns are 0 so that a
        // setuid exec cannot reach the switch. This is the other half: the shipped
        // artifact is inspected, because the crate's own tests can only see modes
        // td-login itself constructs, never the one the packer left on the file.
        let shape = shape_check();
        assert!(
            shape.contains("*[sS]*)") && shape.contains("setuid/setgid bit"),
            "the shape check must refuse a packed td-login carrying a setuid or \
             setgid bit"
        );

        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-login}" && dest == "{root}/real-root{in:td-login}"
            )),
            "td-login must be CopyTree'd into the real root (static, empty closure)"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-login}/bin/td-login"
                        && link == "{root}/real-root/bin/td-login"
            )),
            "/bin/td-login must symlink into the store td-login package - the health target \
             runs `td-login verify-credentials` by that path"
        );
        // Exactly one Symlink step per farm name: Step::Symlink is last-writer-wins, so a
        // name left in two farms would ship whichever loop ran last while a first-match probe
        // still found the other and passed.
        assert!(!TD_LOGIN_APPLETS.is_empty(), "an empty farm makes this vacuous");
        for applet in TD_LOGIN_APPLETS {
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
                "exactly one Symlink step may claim /bin/{applet}, found {}",
                targets.len()
            );
            assert_eq!(
                targets.first().copied(),
                Some("{in:td-login}/bin/td-login"),
                "/bin/{applet} must resolve to the staged td-login multicall"
            );
        }
        // The readback PROBE gets no /bin name: it is not an applet, and a farm-less /bin
        // entry is one no list in this file accounts for and no shape check verifies.
        assert!(
            !packed_bin_names().iter().any(|n| n == "verify-credentials"),
            "verify-credentials is a probe, not an applet; it must not be packed into /bin"
        );

        // The health target must RUN the readback through /bin/su — the shipped symlink and
        // the real credential switch — and clear the marker gate when it disagrees.
        let bootsuccess = build_bootsuccess(&SYSTEM);
        let user = SYSTEM
            .users
            .iter()
            .find(|user| user.name == SYSTEM.autologin)
            .expect("the autologin user resolves");
        let groups: Vec<String> = supplementary_gids(&SYSTEM, user.name)
            .iter()
            .map(|gid| gid.to_string())
            .collect();
        assert!(
            bootsuccess.contains(&format!(
                "/bin/su -s /bin/sh {} -c '{}", SYSTEM.autologin, "l=1;"
            )),
            "the td-login leg must run THROUGH /bin/su as the login user: that IS the \
             credential switch under test, and a leg run as root would verify nothing"
        );
        assert!(
            bootsuccess.contains(&format!(
                "/bin/td-login verify-credentials --uid {} --gid {} --groups \"{}\"",
                user.uid,
                user.gid,
                groups.join(",")
            )),
            "the health target must read the switched process's credentials back with the \
             uid, gid and supplementary set the shipped /etc/passwd and /etc/group imply, \
             and must QUOTE the group list: a user with no supplementary groups yields an \
             empty value that an unquoted --groups drops from the argv entirely, so the \
             probe errors and the marker is withheld on a healthy image"
        );
        // The EMPTY case, which the stock SYSTEM never exercises because tester is in
        // wheel — and which is the whole reason the value is quoted. An unquoted empty
        // list vanishes from the argv, `--groups` then has no argument, and the probe
        // errors on a healthy image.
        const LONE: &[User] = &[User {
            name: "solo",
            uid: 1001,
            gid: 1001,
            gecos: "No Groups",
            home: "/home/solo",
            shell: "/bin/sh",
            groups: &[],
            passwordless: true,
        }];
        let lone = SystemDef {
            autologin: "solo",
            users: LONE,
            ..SYSTEM
        };
        assert!(supplementary_gids(&lone, "solo").is_empty());
        assert!(
            td_login_probe(&lone).contains("--groups \"\" ||"),
            "a user with no supplementary groups must still produce a well-formed, \
             QUOTED empty --groups value: {}",
            td_login_probe(&lone)
        );
        // A SystemDef whose autologin user does not resolve must produce a probe that
        // FAILS, never an empty one: `su -c ''` exits 0 and would print the marker
        // unconditionally.
        assert!(
            td_login_probe(&SystemDef {
                autologin: "nobody-here",
                ..SYSTEM
            })
            .contains("false"),
            "an unresolvable autologin user must yield a failing probe, not an empty one"
        );
        // The gate, not just the command: without `l=0` on the failure branch the marker is
        // unconditional and the oracle greens a switch that left a residual group attached.
        assert!(
            bootsuccess.contains("l=0; }; [ \"$l\" = 1 ]"),
            "a failed readback must clear the marker gate"
        );
        assert!(
            bootsuccess.contains(&format!(
                "[ \"$l\" = 1 ]'; then [ \"$mtl\" = 1 ] || {{ echo {TD_LOGIN_RUNTIME_MARKER}; \
                 mtl=1; }}; else healthy=0; fi"
            )),
            "the health target must emit the td-login marker from that leg alone, so an \
             absent TD-LOGIN-RUN-OK names the credential switch rather than some component \
             upstream of it"
        );
        // ...and the group set must be DERIVED from the generated /etc/group, not a constant
        // that quietly stops matching it. wheel is the membership the stock SYSTEM grants.
        assert_eq!(
            supplementary_gids(&SYSTEM, "tester"),
            vec![10],
            "the shipped /etc/group grants tester wheel(10) and nothing else; if that changed \
             deliberately, the probe follows it automatically and this line records the new \
             expectation"
        );
        assert!(
            supplementary_gids(&SYSTEM, "root").is_empty(),
            "a user's PRIMARY group must not appear in the supplementary set: build_group \
             writes those lines with an empty member field, and td-login folds the primary \
             gid in itself"
        );
    }

    /// The account-name grammar this recipe enforces is the one td-login enforces.
    ///
    /// Two different graders would be worse than one: a name this accepted but
    /// `login` refused would ship an image whose auto-login user cannot log in, and a
    /// name td-login accepted but this refused would block a legitimate tailoring.
    /// The charset is copied across a crate boundary, so it is pinned to the source
    /// it came from — and the injection cases are asserted here rather than left to
    /// the reader to believe.
    #[test]
    fn the_account_grammar_matches_the_one_td_login_uses() {
        let source =
            super::super::td_login::source("login").expect("the recipe embeds src/login.rs");
        assert!(
            source.contains("b'.' | b'_' | b'-'"),
            "td-login's plausible_name no longer uses this charset; re-derive \
             valid_account_name from it"
        );
        for good in ["root", "tester", "td.user", "a_b-c"] {
            assert!(valid_account_name(good), "{good} should be a legal account name");
        }
        // Each of these reaches a ROOT shell unquoted through /etc/rootcheck,
        // /etc/bootsuccess or /etc/autologin, or restructures /etc/passwd.
        for bad in [
            "",
            "$(id)",
            "`id`",
            "a b",
            "a;id",
            "a:x:0:0::/root:/bin/sh",
            "a\nroot",
            "a$IFS",
            "*",
            "verylongnameverylongnameverylongnameverylong",
        ] {
            assert!(
                !valid_account_name(bad),
                "{bad:?} must be refused: it is embedded unquoted in generated root shell"
            );
        }
        // ...and the shipped definition passes its own guard.
        for user in SYSTEM.users {
            assert!(valid_account_name(user.name));
        }
    }

    /// The flags the health probe spells out must be flags td-login actually parses.
    ///
    /// `verify-credentials --uid/--gid/--groups` is copied by hand across a crate boundary,
    /// exactly like td-init's refusal diagnostics. Rename one and the probe fails on a
    /// HEALTHY image: TD-LOGIN-RUN-OK is withheld, and the only thing that would have caught
    /// it is a manual qemu-boot-system run. The recipe already embeds the source via
    /// `include_str!`, so the link costs nothing.
    #[test]
    fn the_credential_readback_probe_uses_flags_td_login_parses() {
        let source = super::super::td_login::source("main")
            .expect("the td-login recipe embeds src/main.rs");
        for spelling in ["verify-credentials", "--uid", "--gid", "--groups"] {
            assert!(
                source.contains(spelling),
                "the health probe spells {spelling:?}, which does not appear in \
                 td-login/src/main.rs - the readback would fail on a healthy image and \
                 withhold TD-LOGIN-RUN-OK"
            );
        }
        // ...and the probe the recipe generates is built from exactly those.
        let probe = td_login_probe(&SYSTEM);
        assert!(probe.contains("verify-credentials"), "probe: {probe}");
        assert!(probe.starts_with("l=1;"), "probe: {probe}");
    }

    /// Every diagnostic a refusal probe waits for must exist in the applet that emits it.
    ///
    /// `Probe::Refuses` asserts on td-init's own words — "unrecognised argument", "refusing to
    /// switch", "usage: cttyhack" — copied by hand across a crate boundary. Americanise one
    /// spelling or reword one refusal and the probe stops matching: the marker is withheld on
    /// a HEALTHY image, and the only thing that would have caught it is a manual
    /// qemu-boot-system run, which no gate runs. The recipe already
    /// embeds every applet source via `include_str!`, so the link costs nothing.
    #[test]
    fn every_expected_refusal_is_a_string_the_applet_actually_emits() {
        // applet -> the module that implements it (reboot/poweroff/halt are one module, and
        // so are mount/umount — they share the mount-table reading).
        fn module(applet: &str) -> &str {
            match applet {
                "reboot" | "poweroff" | "halt" => "halt",
                "switch_root" => "switchroot",
                "umount" => "mount",
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

    /// Every script that decides to end the boot must go through td-svc.
    ///
    /// Under busybox `/bin/reboot` signalled PID 1 into `::shutdown:/etc/shutdown`. td-init
    /// supervises with NO signals, so that action does not exist and nothing catches a bare
    /// `exec /bin/reboot`: services stay running, /var stays mounted (unclean Btrfs) and the
    /// shutdown marker never prints. The rule used to be "run /etc/shutdown first", which each
    /// initiator satisfied by inlining the teardown; that stopped being enough once td-svc had
    /// units to stop, because the teardown is now a SEQUENCE (stop every unit in reverse
    /// dependency order, then /etc/shutdown, then the power applet) and only td-svc knows it.
    /// So the invariant tightened: the power applets belong to td-svc alone, and a generated
    /// script asks for one by NAMING it to td-svc.
    ///
    /// Scanning every generated /etc file, not the one initiator that existed when the rule was
    /// written: `/etc/bootfail` became a second one while an earlier branch was in review, and
    /// a per-file assertion would not have noticed.
    #[test]
    fn reboots_run_the_teardown_first() {
        // The refusal PROBES name the applets on purpose — with a bogus argument they must
        // refuse — so strip the generated probe segments first; they are the one invocation
        // that does not end a boot.
        // All three applets, not just reboot: poweroff and halt end a boot identically, so a
        // script switched to either would escape a reboot-only scan while the count still held.
        const POWER: [&str; 3] = ["reboot", "poweroff", "halt"];
        let mut initiators = 0;
        for (name, body, _) in etc_files(&SYSTEM) {
            let mut body = body;
            for (applet, probe) in TD_INIT_FARM {
                body = body.replace(&td_init_probe(applet, probe, &SYSTEM), "");
            }
            for applet in POWER {
                // Match `/bin/reboot`, not `exec /bin/reboot`: a caller that drops the `exec`
                // still reboots, and still skips the teardown, so keying on the `exec` would
                // let exactly that edit through (Agy review).
                assert!(
                    !body.contains(&format!("/bin/{applet}")),
                    "/etc/{name} runs /bin/{applet} directly - the power applets belong to \
                     td-svc, which stops every unit in reverse dependency order and runs \
                     /etc/shutdown before exec'ing one; calling it here resets the machine \
                     with services still running and no marker printed"
                );
                initiators += body.matches(&format!("/bin/td-svc {applet}")).count();
            }
            // The teardown script is td-svc's to run, at the point in the sequence where every
            // unit is already down. An initiator that also runs it directly runs it twice, the
            // first time against a live system.
            assert!(
                !body.contains("/etc/shutdown"),
                "/etc/{name} runs /etc/shutdown itself - td-svc runs it, after the units are \
                 stopped; running it here unmounts /var out from under them"
            );
        }
        assert_eq!(
            initiators, 2,
            "expected exactly the two known power initiators (tty-session, bootfail), each \
             asking td-svc; a new one must ask it too, and a vanished one means this stopped \
             covering it"
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

    /// No `@PLACEHOLDER@` survives into the shipped shape check.
    ///
    /// The script is built by `.replace`-ing tokens into a literal, so a token added to
    /// the text and not to the replace chain stays in the shell verbatim. That does not
    /// fail loudly: `for f in ... @TD_SVC_CONF_NAME@ ...` iterates over the literal token
    /// and asserts a file by that name exists, and `check -f "$root@TD_SVC_CONF@"` reads a
    /// path that cannot exist. Both would red the build, but for a reason that names the
    /// token rather than the check, and a token in a position the shell simply ignores
    /// would verify nothing at all and stay green. Cheaper to assert none survive.
    #[test]
    fn the_shape_check_ships_no_unreplaced_placeholders() {
        let shape = shape_check();
        let mut left: Vec<String> = Vec::new();
        let mut rest = shape.as_str();
        while let Some(open) = rest.find('@') {
            let after = rest.get(open.saturating_add(1)..).unwrap_or_default();
            match after.find('@') {
                Some(close) => {
                    let token = after.get(..close).unwrap_or_default();
                    if !token.is_empty()
                        && token
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                    {
                        left.push(format!("@{token}@"));
                    }
                    rest = after.get(close.saturating_add(1)..).unwrap_or_default();
                }
                None => break,
            }
        }
        assert!(
            left.is_empty(),
            "shape_check still contains {left:?}; add each to the .replace chain, or the \
             shipped script tests a literal token instead of what it names"
        );
    }

    /// One path, three consumers, and nothing that lets them drift.
    ///
    /// The doc on TD_SVC_CONF used to claim this and it was not true: `etc_files` spelled
    /// "td-svc.conf" literally and shape_check spelled "$root/etc/td-svc.conf" literally,
    /// so only the inittab used the const. Moving TD_SVC_CONF left every test green — the
    /// inittab assertion builds its expected line from the same const, so it is a
    /// tautology — while the image shipped PID 1 naming a file nothing generated.
    ///
    /// That failure is silent AND terminal: td-svc handed a missing table prints one line
    /// and idles forever with zero units, and since it has no exit path PID 1 never
    /// respawns it. No console, no sshd, no network, no diagnostic after the first line.
    /// So the relationship gets a test rather than a comment.
    #[test]
    fn the_unit_table_path_has_exactly_one_source_of_truth() {
        assert!(
            TD_SVC_CONF.starts_with("/etc/"),
            "TD_SVC_CONF must live under /etc: etc_files is what generates it, and \
             td_svc_conf_etc_name derives the filename by stripping that prefix"
        );
        let name = td_svc_conf_etc_name();
        assert_eq!(
            format!("/etc/{name}"),
            TD_SVC_CONF,
            "the generated /etc entry and the path PID 1 names must be the same file"
        );
        assert!(
            !name.contains('/'),
            "TD_SVC_CONF must sit directly in /etc: etc_files writes a flat name, so a \
             nested path would generate nothing and PID 1 would name a missing file"
        );
        // The inittab's -f, spelled out rather than rebuilt from the const, so this is
        // not the tautology it replaces.
        assert!(
            build_inittab().contains("/bin/td-svc run -f /etc/td-svc.conf"),
            "PID 1's only respawn line must name the generated table literally"
        );
        // And the file really is generated at that name.
        assert!(
            etc_files(&SYSTEM).iter().any(|(n, _, _)| *n == name),
            "no etc_files entry generates {name}, so PID 1 would exec td-svc against a \
             path that does not exist"
        );
    }

    /// td-svc must be PACKED, not merely symlinked and listed as an input.
    ///
    /// This test exists because its absence was the bug: the cutover added the /bin/td-svc
    /// symlink and the native_inputs entry and stopped there, and nothing on the host
    /// noticed. A store item reaches the erofs root by CopyTree (static) or
    /// StageRuntimeClosure (dynamic); being a recipe INPUT only makes the path resolvable
    /// at build time. So the symlink resolved in the recipe text and dangled on the image,
    /// and since PID 1's only respawn line execs it, the machine would have come up with
    /// no identity, no network, no sshd and no console — the shape check says exactly
    /// that, but shape_check runs in the recipe-checks tier, which needs the loop
    /// toolchain. Every other packed binary here has a test of this shape; this one is td-svc
    /// paying the same rent.
    #[test]
    fn td_svc_is_packed_and_not_merely_symlinked() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-svc}" && dest == "{root}/real-root{in:td-svc}"
            )),
            "td-svc must be CopyTree'd into the real root (static, empty closure) - a \
             symlink alone dangles on the image and PID 1's only respawn line execs nothing"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-svc}/bin/td-svc"
                        && link == "{root}/real-root/bin/td-svc"
            )),
            "/bin/td-svc must symlink into the store td-svc package"
        );
        let native_inputs = recipe().native_inputs.expect("system native inputs");
        assert!(
            native_inputs.iter().any(|i| i == "td-svc"),
            "td-svc must be a declared native input, or {{in:td-svc}} does not resolve"
        );
    }
}
