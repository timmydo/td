use crate::ladder::{
    post_bootstrap_path, AUTOTEST_CMDLINE_TOKEN, BOOT_FAIL_TARGET_CMDLINE_TOKEN,
    BOOT_SUCCESS_WAIT_CMDLINE_PREFIX, CODEX_BWRAP_VERSION_OUTPUT, CODEX_RUNTIME_MARKER,
    CODEX_VERSION_OUTPUT, DEPLOY_INSTALL_CMDLINE_TOKEN, FIREFOX_INPUT_CMDLINE_TOKEN,
    FIREFOX_NETWORK_RUNTIME_MARKER, GIT_HTTPS_RUNTIME_MARKER,
    GIT_HTTPS_TEST_URL, GIT_RUNTIME_MARKER, GREETER_MARKER,
    NETTEST_CMDLINE_TOKEN, NETTEST_DEFAULT_HOST, NETTEST_DEFAULT_PORT, PERSIST_READ_CMDLINE_TOKEN,
    PERSIST_WRITE_CMDLINE_TOKEN, POST_BOOTSTRAP_SH, RIPGREP_FD_RUNTIME_MARKER, SSHD_MARKER,
    SYSTEM_BOOT_SUCCESS_MARKER, SYSTEM_DEPLOY_INSTALL_MARKER, SYSTEM_DEPLOY_ROLLBACK_MARKER,
    SYSTEM_ETC_MUTABLE_MARKER, SYSTEM_ETC_RO_MARKER, SYSTEM_NET_REACH_MARKER,
    SYSTEM_NET_RESOLVE_MARKER, SYSTEM_NET_UP_MARKER,
    SYSTEM_PERSIST_READ_MARKER, SYSTEM_PERSIST_WRITE_MARKER, SYSTEM_ROOT_RO_MARKER,
    SYSTEM_SHUTDOWN_MARKER, SYSTEM_STATE_OWNER_MARKER, SYSTEM_STATE_WRITABLE_MARKER,
    TD_BUSD_RUNTIME_MARKER, TD_FIREFOX_BOOT_MARKER, TD_FIREFOX_CONTENT_MARKER,
    TD_FIREFOX_SUPPORT_MARKER, TD_INIT_RUNTIME_MARKER, TD_JAIL_SECCOMP_PROBE_MARKER,
    TD_JAIL_TRANSITION_MARKER, TD_LOGIN_RUNTIME_MARKER, TD_PORTAL_CHANNEL_RUNTIME_MARKER,
    TD_PORTAL_REQUEST_RUNTIME_MARKER, TD_PORTAL_RUNTIME_MARKER,
    TD_PORTAL_UNAVAILABLE_RUNTIME_MARKER, TD_SANDBOX_KERNEL_MARKER, TD_TXT_RUNTIME_MARKER,
    TD_UTIL_RUNTIME_MARKER, UUTILS_RUNTIME_MARKER,
};
use crate::types::{Recipe, Step};

use crate::td_boot_protocol;

const BOOT_SUCCESS_RETRY_SECS: u8 = 3;
/// How many /etc/bootsuccess sweeps the bus marker may be missing for before
/// the script stops waiting for it.
///
/// The bus leg does not set `healthy=0` — see APPLICATIONS.md §D on why an
/// application must not be able to reach the rollback decision — and that
/// removed something nobody was thinking about: `healthy=0` was also what kept
/// the loop ITERATING. Every other leg still votes, so a sweep in which only
/// the bus failed now runs straight to `td-boot success` and `exit 0`, the
/// marker never prints, and the image oracle reds on a transient — a broker
/// that `restart=always` happened to be restarting during the first sweep.
///
/// So the marker gets a bounded grace of its own, on the SUCCESS gate rather
/// than on `healthy`: while the bus leg has failed fewer than this many times
/// the script keeps sweeping, and after that it proceeds regardless. It is a
/// retry, not a veto — the distinction is the whole point, and it is why the
/// counter can only delay `td-boot success` by a fixed number of seconds
/// instead of withholding it.
const BUS_MARKER_GRACE_SWEEPS: u8 = 2;
const BOOT_SUCCESS_RETRY_MAX_SECS: u8 = 10;
/// What ONE iteration of the boot-success loop may cost on a slow TCG guest: ten
/// `su` probe blocks, four `td-boot update` passes and a `rollback`. Exactly ONE of
/// those copies an image; what the rest add is deployment-sized READS, and the
/// distinction is worth the words because the QEMU volume budget turns on it
/// — see BOOTSUCCESS below. Named rather than spelled twice because the td-svc
/// backstop and the host's own ceiling are both derived from it, and a figure that
/// drifted between them would leave one of the two killing a healthy boot.
///
/// The eighth block is the session-bus probe, and it costs BOTH: the `su`, `sh` and
/// `td-busd` it forks like every other block, and a bounded wait none of the others
/// have. `td-busd probe` allows five seconds over the connect and the answer
/// together — one deadline, not one each, which is why five and not ten — and a
/// broker that is wedged rather than absent spends all five, where an absent one
/// costs nothing because `connect` is refused at once. So this is +7 and not +5: two
/// for an eighth block's spawn, which is the per-block share the old 45 implied, and
/// five for a wait the old figure had no equivalent of. The ninth block is Git's
/// local init/clone/commit/push/reclone/fsck and shell-porcelain workflow. Its local
/// transport forks both service programs and performs pack/object work, so reserve
/// 18 seconds on TCG rather than only the two-second spawn share. A budget that
/// covered only the healthy path would not be a backstop. The tenth starts the large
/// Codex CLI and then drives a read-only command through its Bubblewrap backend;
/// reserve six seconds for those dynamic starts and the namespace transition on TCG.
#[cfg(test)]
const BOOT_SUCCESS_ITERATION_BUDGET_SECS: u32 = 76;
const BOOT_FAIL_PARK_WAIT_SECS: u8 = 30;
const BOOT_FAIL_PARKED: &str = "td-boot-parked-v1";

// system-x86-64 (re #541, #550): a MINIMAL, TAILORABLE Rust-first Linux
// deployment, selected from persistent Btrfs and entered through kexec onto a
// disk-backed READ-ONLY EROFS root.
//
// This is the "system definition" recipe. It composes artifacts that already exist in
// the ladder — the source-built `linux-x86-64` kernel and td's own static Rust
// userland — into a first-class deployment bundle:
//
//   boot/{selector-initramfs.cpio,manifest}
//   deployment/{bzImage,initramfs.cpio,root.erofs,debug-size,manifest}
//
// The direct-boot selector initramfs carries td-sh (the shell), td-init (for
// the mount pair), td-boot, td-util and td-kexec; it has no branch that can
// enter a deployment directly, because it links no `/bin/switch_root`. It verifies
// current/previous from the Btrfs volume and kexecs the selected deployment.
// That deployment's distinct initramfs requires the td.deployment handoff,
// re-verifies root.erofs, binds it to a read-only loop device, mounts @var from
// Btrfs, and switch_roots. `/etc` stays deployment-owned and immutable, with ONE
// reviewed symlink per mutable file out to writable state (the `MUTABLE_ETC` table
// below) rather than an overlay — so the read-only-`/etc` assertion survives while
// per-machine identity still persists; `/home` and `/root` are root-image symlinks
// into `/var`. The real root is store-native: uutils, ripgrep, fd, Git, Codex and
// Bubblewrap at their /td/store paths, a /bin symlink farm, and generated /etc. The
// typed PackErofs step invokes
// the dependency-free control-plane image writer directly; no recipe process can
// execute td-builder through PATH or argv. Strict manifests separately hash the
// selector and the three deployment payloads.
//
// The greeter auto-logs-in a test user to a shell with a welcome banner. EDIT the
// `SYSTEM` const below to tailor the distro (hostname, users, the auto-login user, the
// login shell, the applet set). A producer-rung shape check on the deployment
// bundle and its scratch root tree is the automated build guard; the interactive
// `td-recipe-eval run` boots the selector, kexecs the verified deployment under
// host qemu, and gives you a shell. The headless `td-recipe-eval
// qemu-boot-system` asserts the deployment state machine across repeated boots.
//
// Userland strategy (v0): the static Rust td-init multicall provides the boot
// glue — PID 1, the pivot, and every mount/umount on the machine — the static
// Rust td-login serves the credential switch (/bin/{login,su}), td-util the
// diagnostics, td-sh the shell (/bin/{sh,ash}), td-txt the text tools uutils
// lacks, and — since the getty applet landed — the tty setup that was busybox's
// last job here; source-built Rust uutils provides the interactive core
// file/text userland with its declared glibc runtime closure. NO third-party
// multicall is packed at all: every /bin entry resolves into a td-built binary
// or uutils.

//
// Layout: the image is STORE-NATIVE. Every packed binary sits at its
// content-addressed /td/store/<hash>-<name>/bin path, and /bin is a PURE symlink
// farm whose every entry (and /init) points straight into one. There is no
// /usr and no /sbin. Generated system config lives under immutable /etc; the other
// non-store root entries are mountpoints plus /home and /root links into /var.

/// One account materialised into `/etc/passwd`, `/etc/group`, `/etc/shadow`, and a
/// home directory. `passwordless` writes an EMPTY shadow password — convenient for
/// a throwaway VM (the auto-login path bypasses auth anyway). With both account
/// class flags false the password is ordinarily locked; `service_only` selects
/// td-login's distinct non-human service marker instead.
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
    /// Mark this account with td-login's exact service-only shadow class.
    /// Such an account is refused by login, login -f, su, and ordinary
    /// exec-as; only exec-service-as may enter it.
    service_only: bool,
}

/// One application selected into this immutable deployment. The package is a
/// data input: image composition may copy and read its authenticated export,
/// but no recipe command receives its path.
struct ShippedApplication {
    /// Builder-authenticated recipe identity and `/bin` launcher key. This is
    /// distinct from `package`, which is a catalog dependency key.
    name: &'static str,
    package: &'static str,
    package_recipe: fn() -> Recipe,
    runtime: &'static str,
    runtime_recipe: fn() -> Recipe,
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
    applications: &'static [ShippedApplication],
}

const APPLICATION_REGISTRY: &str = crate::ladder::TD_APPLICATION_REGISTRY;
const APPLICATION_LAUNCHER_TABLE: &str =
    crate::ladder::TD_APPLICATION_LAUNCHER_TABLE;
const APPLICATION_CONFIG: &str = crate::ladder::TD_APPLICATION_CONFIG_PATH;
const PROFILER_OBJECT_INDEX: &str = "/etc/td-profiler-objects.tsv";
const PROFILER_APPLICATION_ROOTS: &str = "/etc/td-profiler-application-roots.tsv";
const PROFILER_CAPTURE_ROOT: &str = "/var/lib/td-profiler/captures";
const PROFILER_USER: &str = "profiler";
const PROFILER_UID: u32 = 997;
const PROFILER_GID: u32 = 997;
const PROFILER_READ_GID: u32 = 996;
const SSHD_PRIVSEP_USER: &str = "sshd";
const SSHD_PRIVSEP_UID: u32 = 995;
const SSHD_PRIVSEP_GID: u32 = 995;
const SSHD_PRIVSEP_PATH: &str = "/run/sshd-empty";
const AUDIO_USER: &str = "audio";
const AUDIO_UID: u32 = td_engine::permissions::TD_AUDIO_UID;
const AUDIO_GID: u32 = td_engine::permissions::TD_AUDIO_GID;
pub(super) const AUDIO_RUNTIME: &str = td_engine::permissions::TD_AUDIO_RUNTIME_PATH;
const PROFILER_CAPTURE_SECS: u16 = 60;
const PROFILER_EVIDENCE_TIMEOUT_SECS: u16 = 300;
const PROFILER_EVIDENCE_SERVICE_TIMEOUT_SECS: u16 = 315;
const FIREFOX_NAME: &str = "firefox";
const FIREFOX_APP_ID: &str = "org.mozilla.firefox";
const FIREFOX_CONTENT_RGB_A: &str = "ff00ff";
const FIREFOX_CONTENT_RGB_B: &str = "00ff00";
const FIREFOX_HTTPS_DOCUMENT: &str = concat!(
    "<!doctype html><title>TD-FIREFOX-HTTPS-CONTENT-V1</title>",
    "<style>html,body{width:100%;min-height:300vh;margin:0}",
    "body{display:grid;grid-template-columns:1fr 1fr;cursor:crosshair}",
    ".a,.b{height:300vh}.a{background:#ff00ff}.b{background:#00ff00}",
    "#td-input{position:fixed;left:58%;top:24px;width:28%;z-index:1}",
    "#td-download{position:fixed;left:58%;top:64px;z-index:1}",
    "#td-upload{position:fixed;left:58%;top:104px;z-index:1}",
    "#td-upload::file-selector-button{width:100%;height:100%}",
    "</style><div class=a></div><div class=b></div>",
    "<input id=td-input aria-label=td-input>",
    "<a id=td-download href=download.txt ",
    "download=td-firefox-download.txt>Download</a>",
    "<input id=td-upload type=file aria-label=td-upload>",
);
const FIREFOX_DOWNLOAD_FIXTURE: &str = "TD-FIREFOX-DOWNLOAD-V1";
const FIREFOX_AUTOTEST_HOST_ROOT: &str = "/run/user/1000/td-app/firefox";
const FIREFOX_AUTOTEST_PROFILE: &str = "/run/user/1000/td-app/profile";
const FIREFOX_TLS_ROOT: &str = "/run/td-firefox-autotest";
const FIREFOX_TLS_ORIGIN: &str = "/run/td-firefox-autotest/origin";
const FIREFOX_TLS_URL: &str = "https://localhost:8443/content.html";
const FIREFOX_TLS_POLICY: &str = concat!(
    "{\"policies\":{\"Certificates\":{\"Install\":[",
    "\"/etc/firefox/policies/td-firefox-autotest-ca.pem\"",
    "]}}}\n",
);
const FIREFOX_WINDOW_READY_SOCKET: &str = "/run/user/1000/td-firefox-window-ready";
const PORTAL_WAYLAND_SOCKET: &str = "/run/user/1000/td-portal-wayland-0";
const PORTAL_SERVICE_LOG: &str = "/run/td-portal.log";
const PORTAL_FILE_CHOOSER_COMPLETED: &str =
    "TD-PORTAL-FILE-CHOOSER-COMPLETED";
const FIREFOX_DOWNLOAD_SOURCE: &str = "/var/home/tester/Downloads";
const FIREFOX_DOWNLOAD_PATH: &str =
    "/var/home/tester/Downloads/td-firefox-download.txt";
const FIREFOX_DOWNLOAD_PART_PATH: &str =
    "/var/home/tester/Downloads/td-firefox-download.txt.part";
const FIREFOX_XDG_MOUNT_MARKER: &str = "/run/td-firefox-downloads-mounted";
const FIREFOX_EVIDENCE_PATH: &str = "/run/td-firefox-evidence-ok";
const FIREFOX_EVIDENCE_TMP_PATH: &str = "/run/.td-firefox-evidence.tmp";
const FIREFOX_EVIDENCE: &str = "td-firefox-evidence-v1";
const FIREFOX_COMPLETION_PATH: &str = "/run/td-firefox-evidence-complete";
const FIREFOX_COMPLETION_TMP_PATH: &str =
    "/run/.td-firefox-evidence-complete.tmp";
const FIREFOX_COMPLETION: &str = "td-firefox-evidence-complete-v1";
const FIREFOX_INPUT_COMPLETION_PATH: &str = "/run/td-firefox-input-complete";
const FIREFOX_INPUT_COMPLETION_TMP_PATH: &str = "/run/.td-firefox-input-complete.tmp";
const FIREFOX_INPUT_COMPLETION: &str = "td-firefox-input-complete-v1";
// Each td-jail connection has this independent wall-clock deadline. Three
// attempts cover the intentional host/guest hand-off race without multiplying
// a stalled Marionette endpoint into an hour-long service.
const FIREFOX_INPUT_TIMEOUT_SECS: u16 = 60;
const FIREFOX_INPUT_ATTEMPTS: u16 = 3;
const FIREFOX_RETRIED_INPUT_STAGES: u16 = 6;
const FIREFOX_DOWNLOAD_TIMEOUT_SECS: u16 = 40;
const FIREFOX_FILE_CHOOSER_TIMEOUT_SECS: u16 = 60;
const FIREFOX_FILE_CHOOSER_STAGES: u16 = 4;
const FIREFOX_DOWNLOAD_OBSERVE_ATTEMPTS: u16 = 20;
const FIREFOX_INPUT_POLL_SLEEP_SECS: u16 =
    FIREFOX_RETRIED_INPUT_STAGES * FIREFOX_INPUT_ATTEMPTS.saturating_sub(1)
        + FIREFOX_DOWNLOAD_OBSERVE_ATTEMPTS;
const FIREFOX_READY_TIMEOUT_SECS: u16 = 180;
const FIREFOX_READY_ATTEMPTS: u16 = 2;
const FIREFOX_RETRY_MARGIN_SECS: u16 = 60;
const FIREFOX_SUPPORT_TIMEOUT_SECS: u16 = 60;
const FIREFOX_SUPPORT_ATTEMPTS: u16 = 3;
const FIREFOX_NETWORK_TIMEOUT_SECS: u16 = 60;
// The evidence unit polls itself so its deadline is not widened by td-svc's
// exponential restart backoff. Autotest allows two cold starts plus margin.
const FIREFOX_EVIDENCE_WAIT_ITERATIONS: u16 =
    FIREFOX_READY_TIMEOUT_SECS * FIREFOX_READY_ATTEMPTS
        + FIREFOX_RETRY_MARGIN_SECS;
// `after=` releases this daemon when firefox-evidence starts, not when its
// atomic completion appears. Cover the evidence poll loop plus every support
// session that can legally extend one of those iterations.
const FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS: u16 =
    FIREFOX_EVIDENCE_WAIT_ITERATIONS
        + FIREFOX_SUPPORT_TIMEOUT_SECS * FIREFOX_SUPPORT_ATTEMPTS
        + FIREFOX_NETWORK_TIMEOUT_SECS;
// The greeter may observe deployment health before Firefox's first ready
// timeout starts the evidence unit. Its allowance includes that offset and
// each separately bounded Firefox support and staged-input attempt.
const FIREFOX_GREETER_WAIT_ITERATIONS: u16 =
    FIREFOX_READY_TIMEOUT_SECS
        + FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS
        + FIREFOX_INPUT_TIMEOUT_SECS
            * FIREFOX_INPUT_ATTEMPTS
            * FIREFOX_RETRIED_INPUT_STAGES
        + FIREFOX_DOWNLOAD_TIMEOUT_SECS
        + FIREFOX_FILE_CHOOSER_TIMEOUT_SECS * FIREFOX_FILE_CHOOSER_STAGES
        + FIREFOX_INPUT_POLL_SLEEP_SECS;

const SHIPPED_APPLICATIONS: &[ShippedApplication] = &[ShippedApplication {
    name: FIREFOX_NAME,
    package: FIREFOX_NAME,
    package_recipe: super::firefox::recipe,
    runtime: "freedesktop-platform-25-08",
    runtime_recipe: super::freedesktop_platform_25_08::recipe,
}];

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
    if uid == AUDIO_UID {
        return home == AUDIO_RUNTIME;
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
            service_only: false,
        },
        User {
            name: PROFILER_USER,
            uid: PROFILER_UID,
            gid: PROFILER_GID,
            gecos: "System Profiler",
            home: "/home/profiler",
            shell: "/bin/sh",
            groups: &[],
            passwordless: false,
            service_only: false,
        },
        User {
            name: AUDIO_USER,
            uid: AUDIO_UID,
            gid: AUDIO_GID,
            gecos: "System Audio",
            home: AUDIO_RUNTIME,
            shell: "/bin/false",
            groups: &[],
            passwordless: false,
            service_only: true,
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
            service_only: false,
        },
    ],
    applications: SHIPPED_APPLICATIONS,
};

const UI_USER: &str = "tester";
const UI_UID: u32 = td_engine::application_spec::APPLICATION_UID;
const UI_GID: u32 = 1000;
#[cfg(test)]
const UI_HOME: &str = "/home/tester";
const TD_PORTAL_SETTINGS_PATH: &str = "/etc/td-portal-settings";
const TD_PORTAL_SETTINGS: &str = include_str!("../../../td-portal/default-settings.conf");
// ────────────────────────────────────────────────────────────────────────────────

/// The real-root `/bin` is split across six closed applet farms plus the
/// open-ended application farm. The closed farms dispatch
/// on argv[0]'s basename except td-sh, which answers to both its names with one
/// program: the static Rust **td-sh** (the shell — see `TD_SH_APPLETS`), the static
/// Rust **td-init** (the boot glue and the tty setup — see `TD_INIT_FARM`), the static
/// Rust **td-login** (the credential switch — see `TD_LOGIN_APPLETS`), the static Rust
/// **td-txt** (grep/sed — see `TD_TXT_APPLETS`), the static Rust **td-util**
/// (diagnostics), and the dynamically-linked Rust **uutils** `coreutils` (the core
/// file/text userland — #547's cutover). A name goes in exactly one list;
/// `shape_check` asserts the owning binary actually provides it.
///
/// The application farm is empty until the jail exists. BusyBox is gone: `getty` was
/// the last name it held,
/// and it moved to td-init with the `TCGETS`/`TCSETS` amendment that lets the multicall
/// set a line's speed and put it back in canonical mode. The binary went with the name:
/// nothing packs it, `/bin/busybox` is not a symlink any more, and
/// `nothing_on_the_image_is_busybox` is what keeps it that way. Every /bin entry now
/// resolves into a binary td built from source — the four static multicalls, uutils,
/// ripgrep, fd and sshd.
///
/// What that cost, recorded because it is a real loss rather than a clean win: `find`
/// and `xargs` are not on the image at all. They were never `/bin` symlinks (the
/// ladder's findutils dead-axis lock forbids those tokens in step text and cannot tell
/// a cpio member name from a host invocation), so they were reachable only as
/// `busybox find` / `busybox xargs`, and uutils ships neither — they are findutils, not
/// coreutils. `/bin/fd` covers the common `find` case; `xargs` has no replacement here.
/// `find_and_xargs_left_the_image` is what makes that a checked drop rather than a
/// remark; it cannot be `DROPPED_APPLETS`, for the reason recorded there.

/// The shell, served by the static td-sh binary — the `sh` and `ash` busybox used to
/// own. NOT a multicall: td-sh is one program and both names run it, so unlike the
/// three farms above there is no `--list` to probe. What `shape_check` probes instead
/// is BEHAVIOUR, because a shell that dispatches is not the question — a shell that
/// runs the image's own scripts is, and every one of them is interpreted by this
/// binary from the first line of `/init` onward.
///
/// `ash` is here because it is the name busybox answered to and scripts may still
/// spell; it runs the same binary, which is what makes the two indistinguishable
/// rather than merely both present.
///
/// `sh` was the last THIRD-PARTY program either initramfs ran, so nothing in either
/// archive is a program td did not build. That used to be a claim about the ARCHIVES
/// alone, because the real root still served `getty` from the multicall; with getty on
/// td-init it holds for the whole image.
const TD_SH_APPLETS: &[&str] = &["sh", "ash"];

/// The text userland, served by the static td-txt multicall — the `grep` and `sed` busybox
/// used to own, because uutils ships neither. Like every other farm here it dispatches on
/// argv[0]'s basename, so a `/bin/<applet>` -> td-txt symlink runs that applet; like td-util
/// and td-init (and unlike uutils) it is an ET_EXEC with an EMPTY runtime closure.
///
/// These names are LOAD-BEARING, which is what separates this farm from td-util's: the real
/// root's `/etc/rootcheck` decides whether the boot is healthy with eight `grep -Eq` runs over
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
/// The two names the multicall's departure dropped are deliberately NOT here, and the
/// reason is worth stating: this list is spliced into `shape_check`'s shell text, and the
/// ladder's findutils dead-axis lock rejects the tokens `find` and `xargs` anywhere in a
/// step's command surface — the same lock that kept them off `/bin` in the first place.
/// So their drop is asserted in Rust instead, by `find_and_xargs_left_the_image`.
const DROPPED_APPLETS: &[&str] = &["vi", "more", "awk"];
/// The credential switch, served by the static td-login multicall — the two busybox applets
/// that change WHO A PROCESS IS. They are their own binary, and their own entry in
/// UNSAFE.md, because a credential-ordering bug in them is privilege escalation rather than
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
    // exiting immediately as a session leader — vhangups it. That cost stands whatever
    // else uses the applet, and since the terminal became the boot's first client its
    // success path IS on the boot path: td-term execs `cttyhack --stdin /bin/sh` for
    // every session. The usage refusal still pins the packed name and its dispatch.
    ("cttyhack", Probe::Refuses("", "usage: cttyhack")),
    // Probed by REFUSAL for `mount`'s reason, which is its own: with no arguments this
    // MOUNTS, and the greeter is unprivileged so a real run would EPERM anyway. The
    // refusal proves the packed name and that arguments are read before /dev is touched.
    // Its boot use is the sysinit line below, which is where the real exercise happens.
    ("devpts", Probe::Refuses("--not-an-option", "takes no arguments")),
    // Probed by REFUSAL, and this one could not be anything else: a getty that RAN
    // would claim a terminal, put a session on it and exec the login program — on the
    // greeter's own console, mid-boot. The refusal still proves the packed name, the
    // argv[0] dispatch, and the property the shipped `/etc/tty-session` line depends
    // on: the command line is parsed before any terminal is opened or any session
    // made. Its success path is proven by the boot itself, since this applet is how
    // the machine reaches a login prompt at all.
    ("getty", Probe::Refuses("", "usage: getty")),
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
    "dirname", "true", "false", "printenv", "link", "unlink", "cut", "tr", "expr",
];

enum UutilsProbe {
    Output {
        applet: &'static str,
        args: &'static str,
        expected: &'static str,
    },
    Succeeds(&'static str),
    Fails(&'static str),
    Cut,
    Tr,
    Expr,
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
    UutilsProbe::Cut,
    UutilsProbe::Tr,
    UutilsProbe::Expr,
    UutilsProbe::Printenv,
    UutilsProbe::Link,
    UutilsProbe::Unlink,
];

impl UutilsProbe {
    fn applet(&self) -> &'static str {
        match self {
            Self::Output { applet, .. } | Self::Succeeds(applet) | Self::Fails(applet) => applet,
            Self::Cut => "cut",
            Self::Tr => "tr",
            Self::Expr => "expr",
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
        UutilsProbe::Cut => format!(
            "if o=$(/bin/printf \"%s\\n\" left:right | /bin/{applet} -d: -f2 2>&1); then \
             [ \"$o\" = right ] || \
             {{ echo \"uutils: /bin/{applet} returned unexpected output: $o\"; u=0; }}; \
             else echo \"uutils: /bin/{applet} failed: $o\"; u=0; fi; "
        ),
        UutilsProbe::Tr => format!(
            "if o=$(/bin/printf \"%s\\n\" TD | /bin/{applet} A-Z a-z 2>&1); then \
             [ \"$o\" = td ] || \
             {{ echo \"uutils: /bin/{applet} returned unexpected output: $o\"; u=0; }}; \
             else echo \"uutils: /bin/{applet} failed: $o\"; u=0; fi; "
        ),
        UutilsProbe::Expr => format!(
            "if o=$(/bin/{applet} abc : \"a.*\" 2>&1); then \
             [ \"$o\" = 3 ] || \
             {{ echo \"uutils: /bin/{applet} returned unexpected output: $o\"; u=0; }}; \
             else echo \"uutils: /bin/{applet} failed: $o\"; u=0; fi; "
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
    s.push_str(&format!(
        "{SSHD_PRIVSEP_USER}:x:{SSHD_PRIVSEP_UID}:{SSHD_PRIVSEP_GID}:OpenSSH privilege separation:{SSHD_PRIVSEP_PATH}:/bin/false\n"
    ));
    s
}

fn build_group(sys: &SystemDef) -> String {
    let mut s = String::new();
    // Primary group per user (group name == user name).
    for u in sys.users {
        s.push_str(&format!("{}:x:{}:\n", u.name, u.gid));
    }
    s.push_str(&format!(
        "{SSHD_PRIVSEP_USER}:x:{SSHD_PRIVSEP_GID}:\n"
    ));
    // A `wheel` group (gid 10) whose members are the users that declare it.
    let wheel: Vec<&str> = sys
        .users
        .iter()
        .filter(|u| u.groups.contains(&"wheel"))
        .map(|u| u.name)
        .collect();
    s.push_str(&format!("wheel:x:10:{}\n", wheel.join(",")));
    // Capture files use this members-capable group. No interactive account is
    // enrolled by default; an analysis identity must be granted it explicitly.
    s.push_str(&format!("profiler-read:x:{PROFILER_READ_GID}:\n"));
    s.push_str("tty:x:5:\n");
    s
}

fn gets_generic_persistent_home_setup(user: &User) -> bool {
    // Root is handled explicitly; audio's home is volatile.
    user.uid != 0 && user.name != AUDIO_USER
}

fn build_shadow(sys: &SystemDef) -> String {
    let mut s = String::new();
    for u in sys.users {
        // Empty password field => no password. `!` is an ordinary lock, while
        // the exact td marker is denied by every human path and admitted only
        // by exec-service-as. A fixed last-change day keeps this reproducible.
        let pw = if u.service_only {
            "!td-service"
        } else if u.passwordless {
            ""
        } else {
            "!"
        };
        s.push_str(&format!("{}:{}:19000:0:99999:7:::\n", u.name, pw));
    }
    s.push_str(&format!(
        "{SSHD_PRIVSEP_USER}:!:19000:0:99999:7:::\n"
    ));
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
         ::sysinit:/bin/mount -t cgroup2 -o nosuid,nodev,noexec cgroup2 /sys/fs/cgroup\n\
         ::sysinit:/bin/devpts\n\
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

pub(super) const ROOTCHECK_ETC_NAME: &str = "rootcheck";
pub(super) const SHADOW_ETC_NAME: &str = "shadow";

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

fn td_portal_settings_etc_name() -> &'static str {
    match TD_PORTAL_SETTINGS_PATH.strip_prefix("/etc/") {
        Some(name) => name,
        None => TD_PORTAL_SETTINGS_PATH,
    }
}

/// Every unit `/etc/td-svc.conf` must resolve into a start order.
///
/// The shape check greps `td-svc check`'s printed plan for each. `check` already reds
/// on a table it cannot parse, but a unit SILENTLY dropped from the plan — skipped for
/// an unsatisfiable dependency — is a clean exit with a shorter list, and that is the
/// regression this catches: the boot comes up missing a service and says nothing.
const TD_SVC_UNITS: [&str; 24] = [
    "hostname",
    "td-firstboot",
    "rootcheck",
    "profiler",
    "profiler-evidence",
    "seat",
    "audio",
    "netup",
    "busd",
    "portal",
    "portal-evidence",
    "wayland",
    "portal-channel-evidence",
    "terminal",
    "firefox-tls-setup",
    "firefox-tls-origin",
    "firefox-autotest",
    "firefox",
    "firefox-evidence",
    "firefox-input",
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
    /// `td-netd up` is DHCP with bounded retries. Under the nettest token it also
    /// resolves and reaches the upstream, then Git has libcurl's 300-second connect
    /// bound plus the explicit 10-second low-speed transfer bound. Keep the service
    /// above twice that reviewed 336-second worst case.
    pub const NETUP: u32 = 700;
    /// Mints one RSA test authority and one server identity in the slow VM.
    pub const FIREFOX_TLS_SETUP: u32 = 120;
    /// The script's own retry loop is clamped to BOOT_SUCCESS_RETRY_MAX_SECS iterations,
    /// but each runs a large probe farm (ten `su` blocks) and can run four
    /// transactional `td-boot update` passes plus a `rollback`, so an iteration is worth
    /// seconds on a slow disk, not one. Two of the four are cheap by construction — a
    /// refusal and an idle tick each read a bounded manifest and stop.
    ///
    /// Exactly ONE copies an image, and that is the first install. The REINSTALL the
    /// rollback pass restores with copies nothing: the deployment is already published,
    /// so `publish_bundle` finds the destination and takes its existing-bundle path. An
    /// earlier version of this comment said two copies, which is wrong in a direction
    /// that matters — `create_persistent_volume_layout` sizes the fixture for exactly
    /// three deployment-sized copies, so a reader who believed it would conclude the
    /// volume is a whole deployment short and go and "fix" a budget that is correct.
    /// What the rollback and the reinstall really add is deployment-sized READS: the
    /// candidate hashed again to verify it, the published copy hashed to confirm it is
    /// the same bytes, and the FALLBACK's payload digests verified by `verify_slot` —
    /// the fallback being the deployment that is running, since this fixture has two
    /// deployments and not three.
    ///
    /// Raised again for the ninth, Git-heavy `su` block and the tenth Codex/Bubblewrap
    /// block, and by the rule rather than by a measurement: this backstop must clear
    /// the guest loop's own worst case TWICE. What the number bounds is a HANG — the
    /// loop exits as soon as it is healthy — so the cost of the increase is only how
    /// long a wedged health target takes to be called one.
    ///
    /// This is independent of the host oracle's derived deadline: the oracle is not
    /// the only thing that boots this image, and on real hardware there is no host to
    /// give up first.
    pub const BOOTSUCCESS: u32 = 1600;
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
///     symlinks on a still-read-only /etc), and OpenSSH (whose immutable config
///     names those mutable paths).
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
         # Root opens the system-wide perf descriptors, then td-profiler drops all\n\
         # credentials to the dedicated writer identity before collecting. Reports\n\
         # remain local, bounded, and group-readable for an explicitly enrolled agent.\n\
         [profiler]\n\
         type=daemon\n\
         exec=/bin/td-profiler collect --uid {profiler_uid} --gid {profiler_read_gid} --duration-secs {profiler_capture_secs} --deployment {{out}} --profiler-build {{in:td-profiler}}\n\
         after=rootcheck\n\
         requires=rootcheck\n\
         restart=on-failure\n\
         log=/var/log/svc/td-profiler.log\n\
         console=yes\n\
         \n\
         # A trusted one-shot prints the shared boot marker only after one current-boot\n\
         # capture has nonzero samples, complete reports, and no loss/corruption. On\n\
         # the exact QEMU autotest token it also supplies a deterministic CPU workload\n\
         # and requires the persisted line report to attribute that named function.\n\
         [profiler-evidence]\n\
         type=oneshot\n\
         exec=/bin/td-profiler evidence {profiler_capture_root} --timeout-secs {profiler_evidence_timeout_secs} --uid {profiler_uid} --gid {profiler_read_gid} --attribution-cmdline-token {profiler_attribution_cmdline_token}\n\
         after=profiler\n\
         requires=profiler\n\
         timeout={profiler_evidence_service_timeout_secs}\n\
         log=/var/log/svc/td-profiler-evidence.log\n\
         console=yes\n\
         \n\
         # The graphical user owns the framebuffer and evdev seat; audio gets its volatile runtime.\n\
         [seat]\n\
         type=oneshot\n\
         exec=/bin/td-seatd assign --uid {ui_uid} --gid {ui_gid} --audio-uid {audio_uid} --audio-gid {audio_gid}\n\
         after=rootcheck\n\
         timeout={seat}\n\
         \n\
         # The service-only audio identity owns the playback PCMs and daemon\n\
         # socket. A live connect is required before applications may launch.\n\
         [audio]\n\
         type=daemon\n\
         exec=/bin/td-seatd exec-audio --uid {ui_uid} --gid {ui_gid} --audio-uid {audio_uid} --audio-gid {audio_gid} -- /bin/td-login exec-service-as {audio_user} -- /bin/td-audio serve --socket {audio_socket}\n\
         after=seat\n\
         requires=seat\n\
         ready=/bin/td-login exec-service-as {audio_user} -- /bin/td-audio probe --socket {audio_socket}\n\
         ready-timeout=30\n\
         restart=always\n\
         log=/var/log/svc/td-audio.log\n\
         console=yes\n\
         \n\
         # Networking after the read-only-root self-check.\n\
         [netup]\n\
         type=oneshot\n\
         exec=/etc/netup\n\
         after=rootcheck\n\
         timeout={netup}\n\
         \n\
         # The session bus. `after=seat` AND `requires=seat` because td-seatd is\n\
         # what makes the /run/user/{ui_uid} the broker binds inside: without it\n\
         # `bind` would create that directory itself, 0700 and owned by whoever\n\
         # ran first, which is a different machine from the one this table\n\
         # describes. td-jail is the first consumer: it registers each launch\n\
         # with this broker and refuses to release the application if that\n\
         # fails, so Firefox below now depends on this unit answering and\n\
         # not merely on its socket existing. `after` and not `requires`, for\n\
         # the reason recorded at Firefox.\n\
         # `exec-as` rather than `su -c`, so the argv is literal and the\n\
         # environment is the unit's rather than the boot path's.\n\
         [busd]\n\
         type=daemon\n\
         cgroup=session\n\
         exec=/bin/td-login exec-as {ui_user} -- /bin/td-busd run --socket /run/user/{ui_uid}/bus\n\
         after=seat\n\
         requires=seat\n\
         ready=/bin/td-login exec-as {ui_user} -- /bin/td-busd probe /run/user/{ui_uid}/bus\n\
         ready-timeout=30\n\
         restart=always\n\
         \n\
         # Root holds td-busd's one-shot portal capability and supervises one\n\
         # literal td-login exec-as child. The child is therefore both uid 1000\n\
         # and the live direct descendant the broker authorizes to own the\n\
         # reserved public name. Settings needs no Wayland surface, so this first\n\
         # portal landing is ordered only after the bus it serves on.\n\
         [portal]\n\
         type=daemon\n\
         exec=/bin/td-portal supervise --bus /run/user/{ui_uid}/bus --settings {portal_settings}\n\
         after=busd\n\
         requires=busd\n\
         ready=/bin/td-login exec-as {ui_user} -- /bin/td-portal probe --bus /run/user/{ui_uid}/bus --settings {portal_settings}\n\
         ready-timeout=30\n\
         restart=always\n\
         log={portal_service_log}\n\
         console=yes\n\
         \n\
         # Readiness output is discarded by td-svc. This separate client repeats\n\
         # the exact live Settings, Request and unsupported-portal exchanges\n\
         # with console output, making routing, reply, directed-signal and\n\
         # refusal QEMU evidence rather than a startup assertion.\n\
         # Wait for TLS setup because its key generator writes raw progress to\n\
         # the console without line framing; once it settles, td-svc's one-write\n\
         # service prefix is an attributable exact line. This is ordering only:\n\
         # TLS setup is deliberately not required by portal evidence.\n\
         # td-recipe-eval requires exact {portal_runtime_marker},\n\
         # {portal_request_runtime_marker}, and\n\
         # {portal_unavailable_runtime_marker} lines.\n\
         [portal-evidence]\n\
         type=oneshot\n\
         cgroup=session\n\
         exec=/bin/td-login exec-as {ui_user} -- /bin/td-portal probe --bus /run/user/{ui_uid}/bus --settings {portal_settings}\n\
         after=portal,firefox-tls-setup\n\
         requires=portal\n\
         timeout=30\n\
         log=/var/log/svc/td-portal-evidence.log\n\
         console=yes\n\
         \n\
         # No shell-owned device setup: td-seatd assigned the nodes, td-login drops\n\
         # credentials, and the compositor opens only those fixed paths.\n\
         [wayland]\n\
         type=daemon\n\
         cgroup=session\n\
         exec=/bin/su -s /bin/sh {ui_user} -c '/bin/td-compositor run --framebuffer /dev/fb0 --input /dev/input --socket /run/user/{ui_uid}/wayland-0 --portal-socket {portal_wayland_socket} --launcher-application {firefox_name} --terminal-client /bin/td-term --application-ready-socket {firefox_window_ready_socket} --application-app-id {firefox_app_id} --application-content-rgb-a {firefox_content_rgb_a} --application-content-rgb-b {firefox_content_rgb_b}'\n\
         after=seat\n\
         requires=seat\n\
         ready=/bin/su -s /bin/sh {ui_user} -c '/bin/td-compositor probe /run/user/{ui_uid}/wayland-0'\n\
         ready-timeout=30\n\
         restart=always\n\
         \n\
         # The private path is the privileged portal transport boundary. This\n\
         # separate uid-1000 client proves its exact eleven-global registry and\n\
         # exercises td_portal_manager_v1 standalone and dismissal acknowledgements.\n\
         # Wait for TLS setup for the same line-framing reason as portal-evidence:\n\
         # its key generator writes raw progress dots to the shared console.\n\
         # td-recipe-eval requires the exact {portal_channel_runtime_marker} line.\n\
         [portal-channel-evidence]\n\
         type=oneshot\n\
         cgroup=session\n\
         exec=/bin/td-login exec-as {ui_user} -- /bin/td-portal channel-probe --wayland {portal_wayland_socket}\n\
         after=wayland,firefox-tls-setup\n\
         requires=wayland\n\
         timeout=30\n\
         log=/var/log/svc/td-portal-channel-evidence.log\n\
         console=yes\n\
         \n\
         # The first td-native client stays mapped, and it is the TERMINAL: the\n\
         # machine boots to a shell prompt rather than to a demo. Its readiness\n\
         # probe is exposed only after a frame presented at a size the compositor\n\
         # CHOSE, a PTY the kernel agrees is that grid, and a started child --\n\
         # more than the demo's probe proved, except that the demo also required\n\
         # a seat advertising a POINTER and this needs only a keyboard.\n\
         [terminal]\n\
         type=daemon\n\
         cgroup=session\n\
         exec=/bin/su -s /bin/sh {ui_user} -c '/bin/td-term run --socket /run/user/{ui_uid}/wayland-0 --ready-socket /run/user/{ui_uid}/td-term-ready'\n\
         after=wayland\n\
         requires=wayland\n\
         ready=/bin/su -s /bin/sh {ui_user} -c '/bin/td-term probe /run/user/{ui_uid}/td-term-ready'\n\
         ready-timeout=30\n\
         restart=always\n\
         \n\
         # The HTTPS origin uses a source-built TLS implementation. Setup mints\n\
         # an ephemeral CA plus localhost identity under /run; the root-owned CA\n\
         # and exact policy are the only two extra files td-jail will admit into\n\
         # Firefox's synthetic /etc. Ordinary boots create neither file.\n\
         [firefox-tls-setup]\n\
         type=oneshot\n\
         exec=/etc/firefox-tls-setup\n\
         after=seat\n\
         requires=seat\n\
         timeout={firefox_tls_setup_timeout}\n\
         \n\
         [firefox-tls-origin]\n\
         type=daemon\n\
         cgroup=session\n\
         exec=/bin/td-login exec-as {ui_user} -- /etc/firefox-tls-origin\n\
         after=firefox-tls-setup\n\
         requires=firefox-tls-setup\n\
         ready=/etc/firefox-tls-ready\n\
         ready-timeout=30\n\
         restart=on-failure\n\
         \n\
         # The QEMU-only profile is volatile test state, not consent recorded for\n\
         # the installed browser. Mozilla's own automation preferences suppress\n\
         # first-run pre-onboarding only when td.autotest=1 is on the kernel\n\
         # command line. Trust comes from the exact root-owned policy above.\n\
         [firefox-autotest]\n\
         type=oneshot\n\
         cgroup=session\n\
         exec=/bin/td-login exec-as {ui_user} -- /bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" {autotest_cmdline_token} \"*) root={firefox_autotest_host_root}; /bin/mkdir -p \"$root/profile\" && /bin/td-util chmod 0700 \"$root\" \"$root/profile\" && /bin/rm -f \"$root/.user.js.tmp\" && /bin/td-util printf \"%s\\n\" \"user_pref(\\\"browser.preonboarding.enabled\\\", false);\" \"user_pref(\\\"termsofuse.bypassNotification\\\", true);\" \"user_pref(\\\"browser.download.useDownloadDir\\\", true);\" \"user_pref(\\\"browser.download.folderList\\\", 2);\" \"user_pref(\\\"browser.download.dir\\\", \\\"/home/td/Downloads\\\");\" > \"$root/.user.js.tmp\" && /bin/td-util chmod 0600 \"$root/.user.js.tmp\" && /bin/mv \"$root/.user.js.tmp\" \"$root/profile/user.js\";; *) :;; esac'\n\
         after=seat\n\
         requires=seat\n\
         timeout=30\n\
         \n\
         # Firefox is also QEMU boot evidence. td-login passes a literal argv and\n\
         # td-jail resolves argv[0] through the immutable image registry. Under\n\
         # autotest only, Firefox receives the volatile profile, verified URL,\n\
         # and loopback-only Marionette server used by the support oracle;\n\
         # ordinary boots retain Firefox's own first-run and default-profile flow.\n\
         [firefox]\n\
         type=daemon\n\
         cgroup=session\n\
         exec=/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" {autotest_cmdline_token} \"*) exec /bin/td-login exec-as {ui_user} -- /bin/{firefox_name} --marionette --remote-allow-system-access --profile {firefox_autotest_profile} {firefox_tls_url};; *) exec /bin/td-login exec-as {ui_user} -- /bin/{firefox_name};; esac'\n\
         after=audio,busd,portal,wayland,firefox-autotest,firefox-tls-origin\n\
         requires=wayland,firefox-autotest,firefox-tls-origin\n\
         ready=/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" {autotest_cmdline_token} \"*) exec /bin/td-login exec-as {ui_user} -- /bin/td-compositor probe-application {firefox_window_ready_socket} {firefox_app_id} {firefox_content_rgb_a} {firefox_content_rgb_b} --quiet;; *) exit 0;; esac'\n\
         ready-timeout={firefox_ready_timeout}\n\
         restart=always\n\
         \n\
         # This application is test evidence, not deployment health. The exact\n\
         # compositor, live-cgroup and Firefox support probes must succeed before\n\
         # trusted unit code prints the marker, but mutable user state cannot\n\
         # block bootsuccess.\n\
         # A failed cold start is covered by bounded cheap polling, not td-svc's\n\
         # exponential restart backoff. Once those probes pass, at most three\n\
         # separately deadline-bounded support sessions may run. The network oracle\n\
         # adds one bounded public Firefox navigation before publication. Any later\n\
         # publication error is terminal, so the navigation cannot be repeated. No\n\
         # unit consumes this unit's spawn-ready state; atomic marker publication is\n\
         # the authority.\n\
         [firefox-evidence]\n\
         type=daemon\n\
         exec=/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" {autotest_cmdline_token} \"*) :;; *) exit 0;; esac; n=0; s=0; while [ \"$n\" -lt {firefox_evidence_wait} ]; do if application=$(/bin/td-login exec-as {ui_user} -- /bin/td-compositor probe-application {firefox_window_ready_socket} {firefox_app_id} {firefox_content_rgb_a} {firefox_content_rgb_b} 2>/dev/null) && content=$(/bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-process-token {firefox_name} -contentproc 2>/dev/null) && /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-resource-caps {firefox_name}; then if support=$(/bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-support); then network=; case \" $(/bin/cat /proc/cmdline) \" in *\" {nettest_cmdline_token} \"*) network=$(/bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-network) || exit 1; [ \"$network\" = {firefox_network_marker} ] || exit 1;; esac; /bin/rm -f {firefox_evidence_tmp_path} {firefox_completion_tmp_path} && /bin/td-util printf \"%s\\n\" {firefox_evidence} > {firefox_evidence_tmp_path} && /bin/td-util chmod 0644 {firefox_evidence_tmp_path} && /bin/mv {firefox_evidence_tmp_path} {firefox_evidence_path} && /bin/td-util printf \"%s\\n\" \"$application\" && /bin/td-util printf \"%s\\n\" \"$content\" && /bin/td-util printf \"%s\\n\" \"$support\" && /bin/td-util printf \"%s\\n\" \"$network\" && /bin/echo {firefox_marker} && /bin/echo {firefox_content_marker} && /bin/echo {firefox_support_marker} && /bin/td-util printf \"%s\\n\" {firefox_completion} > {firefox_completion_tmp_path} && /bin/td-util chmod 0644 {firefox_completion_tmp_path} && /bin/mv {firefox_completion_tmp_path} {firefox_completion_path} && exit 0; exit 1; fi; s=$((s+1)); [ \"$s\" -lt {firefox_support_attempts} ] || exit 1; fi; n=$((n+1)); /bin/td-util sleep 1; done; exit 1'\n\
         after=firefox,netup\n\
         restart=never\n\
         \n\
         # The first full-system QEMU boot alone asks this root-owned oracle to\n\
         # stage physical virtio input. It first waits for the support oracle's\n\
         # atomic completion so two Marionette sessions never race. Firefox then\n\
         # arms content and chrome listeners before the host advances through an\n\
         # open-menu, outside-dismiss, terminal-to-browser clipboard, and real\n\
         # HTTPS download and real FileChooser handshakes. The FileChooser arm\n\
         # and fresh read-only evidence sessions prove a trusted browser-focus\n\
         # click, then close before physical Control+O invokes Firefox's native\n\
         # Open File command. Only the portal's captured success line admits a\n\
         # fresh result session, so automation supplies no competing input or\n\
         # DOM mutation.\n\
         [firefox-input]\n\
         type=daemon\n\
         exec=/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" {firefox_input_cmdline_token} \"*) :;; *) exit 0;; esac; /bin/rm -f {firefox_download_path} {firefox_download_part_path} || exit 1; n=0; while [ \"$n\" -lt {firefox_input_evidence_wait} ]; do evidence=$(/bin/td-util cat {firefox_completion_path} 2>/dev/null); [ \"$evidence\" = {firefox_completion} ] && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_evidence_wait} ] || exit 1; n=0; while [ \"$n\" -lt {firefox_input_wait} ]; do /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input arm && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_wait} ] || exit 1; n=0; while [ \"$n\" -lt {firefox_input_wait} ]; do /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input menu && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_wait} ] || exit 1; n=0; while [ \"$n\" -lt {firefox_input_wait} ]; do /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input final && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_wait} ] || exit 1; n=0; while [ \"$n\" -lt {firefox_input_wait} ]; do /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input clipboard-refocus-arm && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_wait} ] || exit 1; n=0; while [ \"$n\" -lt {firefox_input_wait} ]; do /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input clipboard-refocus && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_wait} ] || exit 1; n=0; while [ \"$n\" -lt {firefox_input_wait} ]; do /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input clipboard && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_input_wait} ] || exit 1; /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input download || exit 1; n=0; while [ \"$n\" -lt {firefox_download_observe_wait} ]; do if download=$(/bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-download); then /bin/td-util printf \"%s\\n\" \"$download\" && break; fi; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_download_observe_wait} ] || exit 1; portal_done=$(/bin/rg -c \"^{portal_file_chooser_completed} .* response=0$\" {portal_service_log} 2>/dev/null || :); [ -n \"$portal_done\" ] || portal_done=0; /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input file-chooser || exit 1; /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input file-chooser-focus || exit 1; n=0; while [ \"$n\" -lt {firefox_file_chooser_wait} ]; do portal_now=$(/bin/rg -c \"^{portal_file_chooser_completed} .* response=0$\" {portal_service_log} 2>/dev/null || :); [ -n \"$portal_now\" ] || portal_now=0; [ \"$portal_now\" -gt \"$portal_done\" ] && break; n=$((n+1)); /bin/td-util sleep 1; done; [ \"$n\" -lt {firefox_file_chooser_wait} ] || exit 1; /bin/td-login exec-as {ui_user} -- /bin/td-jail --probe-firefox-input file-chooser-result || exit 1; /bin/rm -f {firefox_input_completion_tmp_path} && /bin/td-util printf \"%s\\n\" {firefox_input_completion} > {firefox_input_completion_tmp_path} && /bin/td-util chmod 0644 {firefox_input_completion_tmp_path} && /bin/mv {firefox_input_completion_tmp_path} {firefox_input_completion_path} && exit 0'\n\
         after=firefox-evidence\n\
         restart=never\n\
         \n\
         [bootsuccess]\n\
         type=oneshot\n\
         exec=/etc/bootsuccess\n\
         # Keep the process-heavy runtime probe farm out of the profiler's first\n\
         # bounded capture. This is ordering only: profiler evidence cannot decide\n\
         # whether a deployment is healthy, and a failed evidence unit still settles.\n\
         after={sysinit},busd,wayland,terminal,profiler-evidence,sshd\n\
         requires=terminal\n\
         timeout={bootsuccess}\n\
         \n\
         [bootfail]\n\
         type=oneshot\n\
         exec=/etc/bootfail\n\
         after={sysinit}\n\
         timeout={bootfail}\n\
         \n\
         # OpenSSH binds all interfaces on port 22 as root, then puts the\n\
         # network-facing pre-auth process behind its dedicated locked uid,\n\
         # chroot, and seccomp filter. The immutable config names only the\n\
         # per-machine Ed25519 host key, so a missing identity is fail-closed.\n\
         [sshd]\n\
         type=daemon\n\
         exec=/bin/sshd -D -e -f /etc/ssh/sshd_config\n\
         after={sysinit}\n\
         ready=/bin/td-netd reach 127.0.0.1 22\n\
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
        audio_uid = AUDIO_UID,
        audio_gid = AUDIO_GID,
        audio_user = AUDIO_USER,
        audio_socket = td_engine::permissions::TD_AUDIO_SOCKET_PATH,
        portal_settings = TD_PORTAL_SETTINGS_PATH,
        portal_runtime_marker = TD_PORTAL_RUNTIME_MARKER,
        portal_request_runtime_marker = TD_PORTAL_REQUEST_RUNTIME_MARKER,
        portal_unavailable_runtime_marker = TD_PORTAL_UNAVAILABLE_RUNTIME_MARKER,
        portal_channel_runtime_marker = TD_PORTAL_CHANNEL_RUNTIME_MARKER,
        portal_wayland_socket = PORTAL_WAYLAND_SOCKET,
        portal_service_log = PORTAL_SERVICE_LOG,
        portal_file_chooser_completed = PORTAL_FILE_CHOOSER_COMPLETED,
        ui_gid = UI_GID,
        profiler_uid = PROFILER_UID,
        profiler_read_gid = PROFILER_READ_GID,
        profiler_capture_secs = PROFILER_CAPTURE_SECS,
        profiler_capture_root = PROFILER_CAPTURE_ROOT,
        profiler_evidence_timeout_secs = PROFILER_EVIDENCE_TIMEOUT_SECS,
        profiler_evidence_service_timeout_secs = PROFILER_EVIDENCE_SERVICE_TIMEOUT_SECS,
        profiler_attribution_cmdline_token = AUTOTEST_CMDLINE_TOKEN,
        autotest_cmdline_token = AUTOTEST_CMDLINE_TOKEN,
        nettest_cmdline_token = NETTEST_CMDLINE_TOKEN,
        firefox_tls_setup_timeout = svc_timeouts::FIREFOX_TLS_SETUP,
        firefox_name = FIREFOX_NAME,
        firefox_app_id = FIREFOX_APP_ID,
        firefox_content_rgb_a = FIREFOX_CONTENT_RGB_A,
        firefox_content_rgb_b = FIREFOX_CONTENT_RGB_B,
        firefox_autotest_host_root = FIREFOX_AUTOTEST_HOST_ROOT,
        firefox_autotest_profile = FIREFOX_AUTOTEST_PROFILE,
        firefox_tls_url = FIREFOX_TLS_URL,
        firefox_window_ready_socket = FIREFOX_WINDOW_READY_SOCKET,
        firefox_marker = TD_FIREFOX_BOOT_MARKER,
        firefox_content_marker = TD_FIREFOX_CONTENT_MARKER,
        firefox_support_marker = TD_FIREFOX_SUPPORT_MARKER,
        firefox_network_marker = FIREFOX_NETWORK_RUNTIME_MARKER,
        firefox_evidence = FIREFOX_EVIDENCE,
        firefox_evidence_path = FIREFOX_EVIDENCE_PATH,
        firefox_evidence_tmp_path = FIREFOX_EVIDENCE_TMP_PATH,
        firefox_completion = FIREFOX_COMPLETION,
        firefox_completion_path = FIREFOX_COMPLETION_PATH,
        firefox_completion_tmp_path = FIREFOX_COMPLETION_TMP_PATH,
        firefox_ready_timeout = FIREFOX_READY_TIMEOUT_SECS,
        firefox_evidence_wait = FIREFOX_EVIDENCE_WAIT_ITERATIONS,
        firefox_input_evidence_wait = FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS,
        firefox_support_attempts = FIREFOX_SUPPORT_ATTEMPTS,
        firefox_input_cmdline_token = FIREFOX_INPUT_CMDLINE_TOKEN,
        firefox_input_wait = FIREFOX_INPUT_ATTEMPTS,
        firefox_download_path = FIREFOX_DOWNLOAD_PATH,
        firefox_download_part_path = FIREFOX_DOWNLOAD_PART_PATH,
        firefox_download_observe_wait = FIREFOX_DOWNLOAD_OBSERVE_ATTEMPTS,
        firefox_file_chooser_wait = FIREFOX_FILE_CHOOSER_TIMEOUT_SECS,
        firefox_input_completion = FIREFOX_INPUT_COMPLETION,
        firefox_input_completion_path = FIREFOX_INPUT_COMPLETION_PATH,
        firefox_input_completion_tmp_path = FIREFOX_INPUT_COMPLETION_TMP_PATH,
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
     /bin/td-util printf '%s\\n' 2 > /proc/sys/kernel/perf_event_paranoid\n\
     /bin/td-util test \"$(/bin/td-util cat /proc/sys/kernel/perf_event_paranoid)\" = 2 || { echo 'td-init: kernel.perf_event_paranoid did not realize the pinned value 2' >&2; exit 1; }\n\
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
        if gets_generic_persistent_home_setup(user) {
            init.push_str(&format!(" /sysroot/var{}", user.home));
        }
    }
    init.push_str(&format!(
        "\nif /bin/td-util readlink /sysroot{FIREFOX_DOWNLOAD_SOURCE} >/dev/null 2>&1; then\n\
           echo 'td-init: Firefox Downloads source is a symlink; grant disabled' >&2\n\
         elif /bin/td-util test -e /sysroot{FIREFOX_DOWNLOAD_SOURCE} && ! /bin/td-util test -d /sysroot{FIREFOX_DOWNLOAD_SOURCE}; then\n\
           echo 'td-init: Firefox Downloads source is not a directory; grant disabled' >&2\n\
         elif /bin/td-util mkdir -p /sysroot{FIREFOX_DOWNLOAD_SOURCE}; then\n\
           /bin/td-util chown {UI_UID}:{UI_GID} /sysroot{FIREFOX_DOWNLOAD_SOURCE}\n\
           /bin/td-util chmod 0700 /sysroot{FIREFOX_DOWNLOAD_SOURCE}\n\
           /bin/mount -o bind /sysroot{FIREFOX_DOWNLOAD_SOURCE} /sysroot{FIREFOX_DOWNLOAD_SOURCE}\n\
           /bin/td-util printf '' > /sysroot{FIREFOX_XDG_MOUNT_MARKER}\n\
         else\n\
           echo 'td-init: cannot prepare Firefox Downloads source; grant disabled' >&2\n\
         fi\n\
         /bin/sh -c 'umask 077; /bin/td-util mkdir -p /sysroot/var/root'\n\
         /bin/td-util rm -rf /sysroot/var/run\n\
         /bin/td-util ln -s /run /sysroot/var/run\n\
         /bin/td-util chown 0:0 /sysroot/var /sysroot/var/log /sysroot/var/home /sysroot/var/root\n\
         /bin/td-util chmod 0755 /sysroot/var /sysroot/var/log /sysroot/var/home\n\
         /bin/td-util chmod 0700 /sysroot/var/root\n"
    ));
    for user in sys.users {
        if gets_generic_persistent_home_setup(user) {
            init.push_str(&format!(
                "/bin/td-util chmod 0700 /sysroot/var{}\n",
                user.home
            ));
        }
    }
    init.push_str(&format!(
        "/bin/td-util mkdir -p /sysroot/var/lib/td-profiler/captures\n\
         /bin/td-util chown 0:0 /sysroot/var/lib /sysroot/var/lib/td-profiler\n\
         /bin/td-util chmod 0755 /sysroot/var/lib /sysroot/var/lib/td-profiler\n\
         /bin/td-util chown {PROFILER_UID}:{PROFILER_READ_GID} /sysroot/var/lib/td-profiler/captures\n\
         /bin/td-util chmod 2750 /sysroot/var/lib/td-profiler/captures\n\
         if /bin/td-util test -e /sysroot/var/lib/td-test/td-jail-seccomp-probe; then\n\
         /bin/td-util test -f /sysroot/var/lib/td-test/td-jail-seccomp-probe\n\
         /bin/td-util chown 0:0 /sysroot/var/lib /sysroot/var/lib/td-test \
         /sysroot/var/lib/td-test/td-jail-seccomp-probe\n\
         /bin/td-util chmod 0755 /sysroot/var/lib /sysroot/var/lib/td-test\n\
         /bin/td-util chmod 0555 /sysroot/var/lib/td-test/td-jail-seccomp-probe\n\
         fi\n\
         if /bin/td-util test -e /sysroot{QEMU_OPENSSH_ADMIN_PRIVATE_KEY}; then\n\
         /bin/td-util test -f /sysroot{QEMU_OPENSSH_ADMIN_PRIVATE_KEY}\n\
         /bin/td-util test -f /sysroot{SSHD_AUTHORIZED_KEYS_STATE}\n\
         /bin/td-util chown 0:0 /sysroot/var/lib /sysroot/var/lib/td-test \
         /sysroot{QEMU_OPENSSH_ADMIN_PRIVATE_KEY} /sysroot/var/lib/td \
         /sysroot/var/lib/td/ssh /sysroot{SSHD_AUTHORIZED_KEYS_STATE}\n\
         /bin/td-util chmod 0755 /sysroot/var/lib /sysroot/var/lib/td-test \
         /sysroot/var/lib/td\n\
         /bin/td-util chmod 0700 /sysroot/var/lib/td/ssh\n\
         /bin/td-util chmod 0600 /sysroot{QEMU_OPENSSH_ADMIN_PRIVATE_KEY} \
         /sysroot{SSHD_AUTHORIZED_KEYS_STATE}\n\
         fi\n\
         exec /bin/switch_root /sysroot /init\n"
    ));
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
         if /bin/td-util test -e {FIREFOX_XDG_MOUNT_MARKER}; then\n\
           /bin/umount {FIREFOX_DOWNLOAD_SOURCE} || {{ echo 'td-shutdown: umount Firefox Downloads failed' >&2; ok=0; }}\n\
         fi\n\
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
    // OpenSSH chroots its network-facing pre-auth process here after switching
    // to the dedicated locked account. Recreate the volatile directory on every
    // boot so it is empty, root-owned, and settled before the sshd unit starts.
    s.push_str(&format!(
        "/bin/rm -rf {SSHD_PRIVSEP_PATH}\n\
         /bin/mkdir {SSHD_PRIVSEP_PATH} || ok=0\n\
         /bin/chown 0:0 {SSHD_PRIVSEP_PATH} || ok=0\n\
         /bin/chmod 0755 {SSHD_PRIVSEP_PATH} || ok=0\n"
    ));
    // Persistent home ownership below /var. The audio account's home is the
    // volatile runtime that td-seatd creates after this check.
    for u in sys.users {
        if gets_generic_persistent_home_setup(u) {
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
            // builtin's redirection fails, and td-sh keeps that behaviour (the /etc
            // probe below is the same constraint). Both spell it `/bin/sh` now: the
            // multiplexed `busybox sh` spelling left with the flip, and there is no
            // second shell left for the two to disagree about.
            //
            // Root clears the probe files on BOTH sides, and the pre-clear is
            // load-bearing: a stale root-owned `/var/.tdwr-su` makes the unprivileged
            // write fail with EACCES even where `/var` is world-writable, and that
            // failure would read as a pass. `home` sits inside DOUBLE quotes, so `$`
            // and `"` would be live. The image-wide source contract pins the
            // autologin account to UI_HOME, a shell-safe direct child of /home.
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
        "if /bin/sh -c ': > /etc/.tdwr' 2>/dev/null; then /bin/td-util rm -f /etc/.tdwr; ok=0; else echo {SYSTEM_ETC_RO_MARKER}; fi\n"
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
    // The pty instance sysinit mounted. Each option is checked because the mount
    // SUCCEEDS having ignored it: `mode=620,gid=5` is the tty convention (owner
    // read/write, tty group write -- how anything reaches a terminal it does not
    // own), and `ptmxmode=666` is the one whose absence stops td-term dead, an
    // instance ptmx being mode 0000 by default. `/dev/ptmx` must be the relative
    // symlink, which is the setup the kernel's devpts documentation describes;
    // `-c` beside it proves the node's TYPE, not its mode -- the node exists at
    // 0000 on a mount that dropped `ptmxmode`, which is the leg above's to catch.
    // `newinstance` is deliberately NOT matched -- modern kernels accept it and
    // echo nothing back. The SLAVE's own gid and mode are proven by opening one,
    // which lands with the client that opens the first pty.
    //
    // The numbers here are the kernel's spelling, NOT the mount's: devpts prints
    // its modes with `%03o`, so the `mode=0620` this image asks for comes back as
    // `mode=620` and a check written to match what was passed would red every
    // correct boot.
    s.push_str(
        "/bin/grep -Eq '^devpts /dev/pts devpts ([^ ]*,)?mode=620(,[^ ]*)?( |$)' \
         /proc/mounts || ok=0\n\
         /bin/grep -Eq '^devpts /dev/pts devpts ([^ ]*,)?gid=5(,[^ ]*)?( |$)' \
         /proc/mounts || ok=0\n\
         /bin/grep -Eq '^devpts /dev/pts devpts ([^ ]*,)?ptmxmode=666(,[^ ]*)?( |$)' \
         /proc/mounts || ok=0\n\
         [ \"$(/bin/td-util readlink /dev/ptmx)\" = pts/ptmx ] || ok=0\n\
         [ -c /dev/pts/ptmx ] || ok=0\n",
    );
    s.push_str(&build_mutable_etc_check(sys));
    let mut probe_paths = "/var /run /tmp /home /root".to_string();
    for user in sys.users {
        if gets_generic_persistent_home_setup(user) {
            probe_paths.push(' ');
            probe_paths.push_str(user.home);
        }
    }
    s.push_str(&format!(
        "for d in {probe_paths}; do \
         if /bin/sh -c ': > \"$1/.tdwr\"' td-probe \"$d\" 2>/dev/null; then /bin/td-util rm -f \"$d/.tdwr\"; else ok=0; fi; \
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

/// The `exec-as` half of the td-login farm, which runs as ROOT rather than through `su`.
///
/// It has to: `exec-as` changes credentials, so a copy running as the unprivileged user
/// would fail `setgroups(2)` with EPERM and prove nothing about the applet. That is also
/// why it cannot join `td_login_probe` above — the two legs need different privilege.
///
/// What it exercises is the whole path a supervisor unit will take, end to end and on the
/// real image: the parser, the shared `authorize` policy, the credential switch, and the
/// `exec`. It does that by pointing `exec-as` at the READBACK — `exec-as USER --
/// td-login verify-credentials …` — so the process that reports whether the switch took
/// is the process `exec-as` itself started. Without this leg `exec-as` ships entirely
/// unexecuted: nothing else on the image invokes it, and `parse` plus `session_for` are
/// the only parts a unit test can reach.
///
/// Single quotes are fine here, unlike every probe inside the greeter's `su -c '…'`:
/// this one is a root-level command in the generated script rather than an argument to
/// one.
fn td_login_exec_as_probe(sys: &SystemDef) -> String {
    let Some(user) = sys.users.iter().find(|user| user.name == sys.autologin) else {
        // Fail CLOSED, for `td_login_probe`'s reason: an empty command would be a
        // success the marker gate cannot tell from a real one.
        return "{ echo \"td-login: no autologin user to exec-as\"; false; }".into();
    };
    let groups: Vec<String> = supplementary_gids(sys, user.name)
        .iter()
        .map(|gid| gid.to_string())
        .collect();
    format!(
        "{{ /bin/td-login exec-as {name} -- /bin/td-login verify-credentials \
         --uid {uid} --gid {gid} --groups \"{groups}\" || \
         {{ echo \"td-login: exec-as {name} did not produce uid {uid} gid {gid} \
         groups [{groups}]\"; false; }}; }}",
        name = user.name,
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

/// The `/proc` nodes whose mere EXISTENCE is one pinned kernel symbol, with the symbol
/// each stands for and what a jail loses without it.
///
/// Existence is a sound test for exactly these: procfs builds `/proc/<pid>/ns/<kind>`
/// from a table whose entries are `#ifdef`ed on their namespace symbol, and
/// `fs/notify/inotify/inotify_user.c` — compiled only under CONFIG_INOTIFY_USER —
/// is the sole registrant of the `fs/inotify` sysctl directory. So a missing node is a
/// missing feature and not a permission problem: the reader is the process itself,
/// which may always stat its own namespace links.
///
/// Note what is NOT sound this way, since it looks like it should be:
/// `/proc/sys/user/max_*_namespaces` exists whatever the namespace symbols say —
/// `kernel/ucount.c` registers all seven entries under CONFIG_SYSCTL alone. Those are
/// read for their VALUE in [`SANDBOX_KERNEL_UCOUNTS`] instead.
const SANDBOX_KERNEL_NODES: [(&str, &str, &str); 6] = [
    (
        "/proc/cgroups",
        "CONFIG_CGROUPS",
        "no cgroup v2 at all, so an application has no aggregate CPU, memory, or pid cap",
    ),
    (
        "/proc/self/ns/user",
        "CONFIG_USER_NS",
        "unshare(CLONE_NEWUSER) returns EINVAL, so td-jail cannot build a sandbox at all",
    ),
    (
        "/proc/self/ns/pid",
        "CONFIG_PID_NS",
        "a jailed app would see, and could signal, every process on the machine",
    ),
    (
        "/proc/self/ns/uts",
        "CONFIG_UTS_NS",
        "a jail could not present a hostname of its own",
    ),
    (
        "/proc/self/ns/net",
        "CONFIG_NET_NS",
        "an app without shared=network could not be cut off from the network stack",
    ),
    (
        "/proc/sys/fs/inotify/max_user_watches",
        "CONFIG_INOTIFY_USER",
        "GLib file monitoring inside a jail degrades to polling",
    ),
];

/// The `/proc/self/status` fields procfs emits only under their symbol. `Seccomp:` is
/// written under CONFIG_SECCOMP and `Seccomp_filters:` under CONFIG_SECCOMP_FILTER, so
/// these two lines are how a running kernel reports a feature that has no node of its
/// own — and the second is the only runtime evidence for a symbol that cannot be pinned
/// (it is `def_bool y` on `SECCOMP && NET`, computed rather than answered).
const SANDBOX_KERNEL_STATUS_FIELDS: [(&str, &str, &str); 2] = [
    (
        "Seccomp:",
        "CONFIG_SECCOMP",
        "seccomp(2) returns ENOSYS, so td-jail would ship namespaces with no syscall filter",
    ),
    (
        "Seccomp_filters:",
        "CONFIG_SECCOMP_FILTER",
        "no BPF syscall filtering — SECCOMP or NET was taken away underneath it",
    ),
];

/// The cgroup controllers `/proc/cgroups` can be made to witness — which is NOT all of
/// them, and the exclusion is the interesting part.
///
/// `proc_cgroupstats_show` skips any subsystem for which `cgroup1_subsys_absent()`
/// holds: `legacy_cftypes == NULL && dfl_cftypes` — a controller with a v2 interface and
/// no v1 one. `pids` registers `legacy_cftypes` unconditionally so it is listed, but
/// memcg registers them under `#ifdef CONFIG_MEMCG_V1`, which resolves to `n` here — so
/// **`memory` is absent from `/proc/cgroups` on this kernel even though `CONFIG_MEMCG=y`**.
/// The kernel says so itself, once, on the console: "/proc/cgroups lists only v1
/// controllers, use cgroup.controllers of root cgroup for v2 info".
///
/// MEMCG's runtime witness is therefore the mounted cgroup2 table below rather than
/// this legacy table.
///
/// The match is anchored on the `enabled` column and not just the name, because a
/// `cgroup_disable=pids` on the command line leaves the row in place and clears that
/// column — the one failure no config guard can see, which is the whole reason a
/// runtime leg is worth having here at all.
const SANDBOX_KERNEL_CONTROLLERS: [(&str, &str, &str); 1] = [(
    "pids",
    "CONFIG_CGROUP_PIDS",
    "pids.max never exists, so nothing bounds a fork bomb inside a jail",
)];

/// The controllers the mounted unified hierarchy must actually expose. Unlike
/// `/proc/cgroups`, this is authoritative for the v2 memory controller.
const SANDBOX_KERNEL_CGROUP2_CONTROLLERS: [(&str, &str, &str); 3] = [
    (
        "cpu",
        "CONFIG_CGROUP_SCHED",
        "the cgroup v2 CPU controller does not exist",
    ),
    (
        "memory",
        "CONFIG_MEMCG",
        "memory.high and memory.max never exist, so a jail has no aggregate memory cap",
    ),
    (
        "pids",
        "CONFIG_CGROUP_PIDS",
        "pids.max never exists in cgroup v2, so a jail cannot contain a fork bomb",
    ),
];

/// Controller files whose presence witnesses scheduler features narrower than
/// the controller itself.
const SANDBOX_KERNEL_CGROUP2_NODES: [(&str, &str, &str, &str); 1] = [(
    crate::ladder::TD_APPLICATION_CGROUP_SESSION,
    "cpu.weight",
    "CONFIG_FAIR_GROUP_SCHED",
    "fair-scheduler cgroup accounting does not exist",
)];

/// Rows compiled into controller files only with the named scheduler feature.
const SANDBOX_KERNEL_CGROUP2_ROWS: [(&str, &str, &str, &str, &str); 1] = [(
    crate::ladder::TD_APPLICATION_CGROUP_SESSION,
    "cpu.stat",
    "nr_periods",
    "CONFIG_CFS_BANDWIDTH",
    "a jail cannot enforce CPU bandwidth",
)];

/// The per-namespace ucount ceilings, one per `CLONE_NEW*` td-jail's single `unshare(2)`
/// asks for. Each is a kill switch a compiled-in namespace cannot survive: set to 0, the
/// feature is present, its `/proc/self/ns/` node exists, and `unshare` fails `ENOSPC`.
///
/// All four are read, not just the user one. An earlier draft checked
/// `max_user_namespaces` alone, which left three of the four namespaces in §C's
/// `unshare` unguarded — and the pid one is the sharpest, since `CLONE_NEWPID` failing
/// leaves an application looking at every process on the machine.
///
/// Read for VALUE rather than existence: `kernel/ucount.c` registers the whole `user`
/// table under CONFIG_SYSCTL, so the FILE is there on a kernel with no namespaces at
/// all. That is also why the unreadable arm's diagnostic names a missing `/proc/sys`
/// rather than a missing namespace — the cause it used to name could not produce it.
const SANDBOX_KERNEL_UCOUNTS: [(&str, &str); 4] = [
    ("max_user_namespaces", "CLONE_NEWUSER"),
    ("max_pid_namespaces", "CLONE_NEWPID"),
    ("max_uts_namespaces", "CLONE_NEWUTS"),
    ("max_net_namespaces", "CLONE_NEWNET"),
];

/// The kernel-capability farm (APPLICATIONS.md §0), run as the unprivileged login user
/// because that is the uid an application will be jailed at.
///
/// It observes the RUNNING kernel rather than re-reading the `.config` the producer
/// already grepped: a config pin constrains what was built, and this asserts that what
/// BOOTED still has the features — which is what makes a regression red the image
/// instead of the first application.
///
/// It runs ONCE, before the health-retry loop, and not inside it like every other farm.
/// The others test userland that can plausibly become ready a second later; a kernel's
/// capabilities are fixed at boot, so re-running only re-proves what cannot have
/// changed. That is not merely wasteful: on a kernel missing a symbol the loop would
/// reprint the same diagnostics every second for the whole retry budget, and the
/// 80-line console tail the oracle's error message tells an operator to read would be
/// filled with identical repeats — pushing out the other farms' diagnostics.
fn build_sandbox_kernel_probes() -> String {
    // NO SINGLE QUOTE may appear below: this string is interpolated INSIDE the greeter's
    // `su -s /bin/sh USER -c '…'` argument, exactly as build_td_txt_probes is, and one
    // would close that argument and scatter the rest into the outer shell.
    let mut p = String::from("k=1; ");
    for (path, symbol, cost) in SANDBOX_KERNEL_NODES {
        p.push_str(&format!(
            "[ -e {path} ] || \
             {{ echo \"kernel: {path} missing ({symbol} off) — {cost}\"; k=0; }}; "
        ));
    }
    for (field, symbol, cost) in SANDBOX_KERNEL_STATUS_FIELDS {
        p.push_str(&format!(
            "/bin/grep -q \"^{field}\" /proc/self/status || \
             {{ echo \"kernel: no {field} field in /proc/self/status ({symbol} off) — {cost}\"; \
             k=0; }}; "
        ));
    }
    for (controller, symbol, cost) in SANDBOX_KERNEL_CONTROLLERS {
        // name, hierarchy, num_cgroups, enabled — the last must be 1, or the controller
        // is compiled in and switched off.
        p.push_str(&format!(
            "/bin/grep -Eq \"^{controller}[[:space:]].*[[:space:]]1$\" /proc/cgroups || \
             {{ echo \"kernel: /proc/cgroups does not list {controller} as enabled \
             ({symbol} off, or cgroup_disable={controller}) — {cost}\"; k=0; }}; "
        ));
    }
    for (controller, symbol, cost) in SANDBOX_KERNEL_CGROUP2_CONTROLLERS {
        p.push_str(&format!(
            "/bin/grep -Eq \"(^|[[:space:]]){controller}([[:space:]]|$)\" \
             /sys/fs/cgroup/cgroup.controllers || \
             {{ echo \"kernel: cgroup2 does not expose {controller} \
             ({symbol} off, disabled, or not mounted) — {cost}\"; k=0; }}; "
        ));
    }
    for (directory, node, symbol, cost) in SANDBOX_KERNEL_CGROUP2_NODES {
        p.push_str(&format!(
            "[ -e {directory}/{node} ] || \
             {{ echo \"kernel: {directory}/{node} missing \
             ({symbol} off, or td-svc cgroup delegation failed) — {cost}\"; k=0; }}; "
        ));
    }
    for (directory, node, row, symbol, cost) in SANDBOX_KERNEL_CGROUP2_ROWS {
        p.push_str(&format!(
            "/bin/grep -Eq \"^{row}[[:space:]][0-9]+$\" {directory}/{node} || \
             {{ echo \"kernel: {directory}/{node} lacks {row} \
             ({symbol} off, or td-svc cgroup delegation failed) — {cost}\"; k=0; }}; "
        ));
    }
    for (limit, clone_flag) in SANDBOX_KERNEL_UCOUNTS {
        // The unreadable arm substitutes 1 rather than 0 so the `case` falls through
        // silently: it has already reported and cleared `k`, and 0 would make the last
        // arm print a second, fabricated diagnostic for one cause.
        p.push_str(&format!(
            "m=$(/bin/cat /proc/sys/user/{limit} 2>/dev/null) || \
             {{ echo \"kernel: /proc/sys/user/{limit} unreadable — no /proc/sys, since \
             the kernel registers this file whatever the namespace symbols say\"; \
             k=0; m=1; }}; \
             case \"$m\" in \"\"|*[!0-9]*) \
             echo \"kernel: {limit} is not a number: $m\"; k=0;; \
             0) echo \"kernel: {limit} is 0 — unshare({clone_flag}) fails ENOSPC however \
             the kernel was built\"; k=0;; esac; "
        ));
    }
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
    let td_login_exec_as_probe = td_login_exec_as_probe(sys);
    let td_txt_probes = build_td_txt_probes();
    let sandbox_kernel_probes = build_sandbox_kernel_probes();
    // Named rather than spelled, so a renamed key or a reworded refusal cannot
    // leave the oracle's negative pass quietly satisfied by any failure at all.
    let trusted_key = td_boot_protocol::VOLUME_TRUSTED_KEY;
    let wrong_key = crate::ladder::DEPLOY_WRONG_KEY;
    let unauthenticated = td_boot_protocol::MANIFEST_UNAUTHENTICATED;
    let channel = td_boot_protocol::VOLUME_CHANNEL_DIR;
    let idle_channel = crate::ladder::DEPLOY_IDLE_CHANNEL;
    let update = td_boot_protocol::UPDATE_VERB;
    // The tester may read its private self-test key but must not own the public
    // AuthorizedKeysFile that grants it access. Under the QEMU autotest token, a
    // separately preseeded root-only fixture exercises the persistent default
    // authorization path without changing it during the boot.
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
         wait={BOOT_SUCCESS_RETRY_SECS}; admin_fixture=0\n\
         for token in $(/bin/td-util cat /proc/cmdline); do \
         case \"$token\" in \
         {AUTOTEST_CMDLINE_TOKEN}) admin_fixture=1;; \
         {BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}*) \
         wait=${{token#{BOOT_SUCCESS_WAIT_CMDLINE_PREFIX}}};; esac; done\n\
         case \"$wait\" in ''|*[!0-9]*|0) wait={BOOT_SUCCESS_RETRY_SECS};; esac\n\
         [ \"$wait\" -gt {BOOT_SUCCESS_RETRY_MAX_SECS} ] && wait={BOOT_SUCCESS_RETRY_MAX_SECS}\n\
         /bin/rm -f /run/td-ssh-selftest /run/td-ssh-selftest.pub \
         {SSHD_SELFTEST_AUTHORIZED_KEYS} /run/td-ssh-known-hosts\n\
         /bin/ssh-keygen -q -t ed25519 -N '' -C td-ssh-selftest -f /run/td-ssh-selftest || fail\n\
         /bin/chown {UI_UID}:{UI_GID} /run/td-ssh-selftest || fail\n\
         /bin/chmod 0600 /run/td-ssh-selftest || fail\n\
         /bin/chmod 0644 /run/td-ssh-selftest.pub || fail\n\
         set -- $(/bin/td-util cat /run/td-ssh-selftest.pub 2>/dev/null)\n\
         [ \"$1\" = ssh-ed25519 ] && [ -n \"$2\" ] || fail\n\
         /bin/td-util printf 'restrict,from=\"127.0.0.1\" %s %s td-ssh-selftest\\n' \
         \"$1\" \"$2\" > {SSHD_SELFTEST_AUTHORIZED_KEYS} || fail\n\
         /bin/chown 0:0 {SSHD_SELFTEST_AUTHORIZED_KEYS} || fail\n\
         /bin/chmod 0644 {SSHD_SELFTEST_AUTHORIZED_KEYS} || fail\n\
         /bin/rm -f /run/td-ssh-selftest.pub || fail\n\
         set -- $(/bin/td-util cat {SSHD_HOST_KEY}.pub 2>/dev/null)\n\
         [ \"$1\" = ssh-ed25519 ] && [ -n \"$2\" ] || fail\n\
         /bin/td-util printf '127.0.0.1 %s %s\\n' \"$1\" \"$2\" > /run/td-ssh-known-hosts || fail\n\
         /bin/chown {UI_UID}:{UI_GID} /run/td-ssh-known-hosts || fail\n\
         /bin/chmod 0644 /run/td-ssh-known-hosts || fail\n\
         if [ \"$admin_fixture\" = 1 ]; then \
         /bin/td-util test -f {QEMU_OPENSSH_ADMIN_PRIVATE_KEY} || fail; \
         admin=$(/bin/ssh -F /dev/null -i {QEMU_OPENSSH_ADMIN_PRIVATE_KEY} \
         -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
         -o UserKnownHostsFile=/run/td-ssh-known-hosts \
         -o GlobalKnownHostsFile=/dev/null \
         -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2 \
         -o KexAlgorithms={OPENSSH_KEX_ALGORITHMS} \
         -o HostKeyAlgorithms={OPENSSH_KEY_ALGORITHMS} \
         -o PubkeyAcceptedAlgorithms={OPENSSH_KEY_ALGORITHMS} \
         -o Ciphers={OPENSSH_CIPHERS} -o Compression=no \
         root@127.0.0.1 /bin/echo TD-OPENSSH-ADMIN-ROUNDTRIP 2>&1); admin_status=$?\n\
         [ \"$admin_status\" = 0 ] && [ \"$admin\" = TD-OPENSSH-ADMIN-ROUNDTRIP ] || \
         {{ echo \"OpenSSH: persistent administrator path failed: $admin\"; fail; }}; fi\n\
         n=0\n\
         bg={BUS_MARKER_GRACE_SWEEPS}\n\
         [ \"$bg\" -ge \"$wait\" ] && bg=$((wait-1))\n\
         mu=0; mrf=0; mg=0; mc=0; ms=0; mtu=0; mti=0; mtl=0; mtt=0; mtb=0; btb=0\n\
         msk=0; mtj=0; mts=1\n\
         if /bin/su -s /bin/sh {} -c \
         '{sandbox_kernel_probes}[ \"$k\" = 1 ]'; then \
         echo {TD_SANDBOX_KERNEL_MARKER}; msk=1; fi\n\
         if /bin/su -s /bin/sh {} -c \
         'j=$(TD_JAIL_TEST_LEAK_FD=1 /bin/td-jail --probe-transition 2>&1) || \
         {{ echo \"td-jail: target transition probe failed: $j\"; exit 1; }}; \
         [ \"$j\" = \"{TD_JAIL_TRANSITION_MARKER} pid=1\" ] || \
         {{ echo \"td-jail: target transition returned unexpected output: $j\"; \
         exit 1; }}'; then echo {TD_JAIL_TRANSITION_MARKER}; mtj=1; fi\n\
         if [ -e /var/lib/td-test/td-jail-seccomp-probe ]; then \
         mts=0; /bin/rm -rf /run/td-jail-seccomp-probe; \
         if [ -f /var/lib/td-test/td-jail-seccomp-probe ] \
         && /bin/mkdir /run/td-jail-seccomp-probe \
         && /bin/cp /var/lib/td-test/td-jail-seccomp-probe \
         /run/td-jail-seccomp-probe/probe \
         && /bin/td-jail --internal-write-seccomp-filter \
         >/run/td-jail-seccomp-probe/filter.bpf \
         && /bin/chown 0:0 /run/td-jail-seccomp-probe \
         /run/td-jail-seccomp-probe/probe /run/td-jail-seccomp-probe/filter.bpf \
         && /bin/chmod 0555 /run/td-jail-seccomp-probe \
         /run/td-jail-seccomp-probe/probe \
         && /bin/chmod 0444 /run/td-jail-seccomp-probe/filter.bpf; then \
         if /bin/su -s /bin/sh {} -c \
         '[ -x /run/td-jail-seccomp-probe/probe ] \
         && [ ! -w /run/td-jail-seccomp-probe/probe ] \
         && [ -r /run/td-jail-seccomp-probe/filter.bpf ] \
         && [ ! -w /run/td-jail-seccomp-probe/filter.bpf ] \
         && [ ! -w /run/td-jail-seccomp-probe ] || \
         {{ echo \"td-jail: target seccomp inputs are not immutable\"; exit 1; }}; \
         p=$(/run/td-jail-seccomp-probe/probe \
         /run/td-jail-seccomp-probe/filter.bpf 2>&1) || \
         {{ echo \"td-jail: target seccomp behavior probe failed\"; exit 1; }}; \
         [ \"$p\" = {TD_JAIL_SECCOMP_PROBE_MARKER} ] || \
         {{ echo \"td-jail: target seccomp behavior returned unexpected output\"; \
         exit 1; }}'; then echo {TD_JAIL_SECCOMP_PROBE_MARKER}; mts=1; fi; \
         else echo \"td-jail: could not prepare immutable target seccomp inputs\"; fi; fi\n\
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
         if /bin/su -s /bin/sh {git_user} -c \
         'HOME=/tmp/td-git-probe/home; export HOME; \
         XDG_CONFIG_HOME=/tmp/td-git-probe/xdg; export XDG_CONFIG_HOME; \
         GIT_CONFIG_GLOBAL=/dev/null; export GIT_CONFIG_GLOBAL; \
         GIT_CONFIG_NOSYSTEM=1; export GIT_CONFIG_NOSYSTEM; \
         GIT_SSH_COMMAND=\"/bin/ssh -F /dev/null -i /run/td-ssh-selftest \
         -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
         -o UserKnownHostsFile=/run/td-ssh-known-hosts \
         -o GlobalKnownHostsFile=/dev/null \
         -o KexAlgorithms={OPENSSH_KEX_ALGORITHMS} \
         -o HostKeyAlgorithms={OPENSSH_KEY_ALGORITHMS} \
         -o PubkeyAcceptedAlgorithms={OPENSSH_KEY_ALGORITHMS} \
         -o Ciphers={OPENSSH_CIPHERS} -o Compression=no\"; export GIT_SSH_COMMAND; \
         /bin/rm -rf /tmp/td-git-probe || \
         {{ echo \"git: could not clear the health-probe directory\"; exit 1; }}; \
         /bin/mkdir -p \"$HOME\" \"$XDG_CONFIG_HOME\" || \
         {{ echo \"git: could not create isolated HOME and XDG config\"; exit 1; }}; \
         cd /tmp/td-git-probe || \
         {{ echo \"git: could not enter the health-probe directory\"; exit 1; }}; \
         /bin/git init --bare -b main origin >/dev/null 2>&1 || \
         {{ echo \"git: bare init failed\"; exit 1; }}; \
         remote=ssh://{git_user}@127.0.0.1/tmp/td-git-probe/origin; \
         /bin/git clone \"$remote\" work >/dev/null 2>&1 || \
         {{ echo \"git: SSH upload-pack clone failed\"; exit 1; }}; \
         /bin/git -C work config user.name td-boot || \
         {{ echo \"git: setting the probe user name failed\"; exit 1; }}; \
         /bin/git -C work config user.email td-boot@example.invalid || \
         {{ echo \"git: setting the probe user email failed\"; exit 1; }}; \
         /bin/printf \"%s\\n\" installed > work/tracked || \
         {{ echo \"git: writing the tracked fixture failed\"; exit 1; }}; \
         /bin/git -C work add tracked || {{ echo \"git: add failed\"; exit 1; }}; \
         /bin/git -C work commit -m installed >/dev/null 2>&1 || \
         {{ echo \"git: commit failed\"; exit 1; }}; \
         push=$(/bin/git -C work push -u origin HEAD:refs/heads/main 2>&1) || \
         {{ echo \"git: receive-pack push failed: $push\"; exit 1; }}; \
         /bin/git clone \"$remote\" verify >/dev/null 2>&1 || \
         {{ echo \"git: SSH upload-pack reclone failed\"; exit 1; }}; \
         [ \"$(/bin/git -C verify rev-list --count main)\" = 1 ] || \
         {{ echo \"git: recloned history was not exactly one commit\"; exit 1; }}; \
         /bin/git -C verify fsck --strict >/dev/null 2>&1 || \
         {{ echo \"git: fsck rejected the recloned repository\"; exit 1; }}; \
         if /bin/git -C work submodule --td-invalid \
         >/tmp/td-git-probe/submodule.err 2>&1; then \
         echo \"git: submodule accepted an invalid option\"; exit 1; fi; \
         /bin/grep -q -F \"usage: git submodule\" \
         /tmp/td-git-probe/submodule.err || \
         {{ echo \"git: shell porcelain did not produce its usage text\"; exit 1; }}; \
         /bin/grep -q -F -- \"-----BEGIN CERTIFICATE-----\" \
         /etc/ssl/certs/ca-certificates.crt || \
         {{ echo \"git: the installed CA bundle has no PEM certificate\"; exit 1; }}'; then \
         [ \"$mg\" = 1 ] || {{ echo {GIT_RUNTIME_MARKER}; mg=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {probe_user} -c \
         'c=$(/bin/codex --version 2>&1) || \
         {{ echo \"codex: /bin/codex --version failed: $c\"; exit 1; }}; \
         [ \"$c\" = \"{codex_version}\" ] || \
         {{ echo \"codex: unexpected installed version: $c\"; exit 1; }}; \
         b=$(/bin/bwrap --version 2>&1) || \
         {{ echo \"bwrap: /bin/bwrap --version failed: $b\"; exit 1; }}; \
         [ \"$b\" = \"{bwrap_version}\" ] || \
         {{ echo \"bwrap: unexpected installed version: $b\"; exit 1; }}; \
         /bin/rm -rf {codex_probe_root} || \
         {{ echo \"codex: could not clear the sandbox probe\"; exit 1; }}; \
         /bin/mkdir -p {codex_probe_root}/home/.codex \
         {codex_probe_root}/work || \
         {{ echo \"codex: could not prepare the sandbox probe\"; exit 1; }}; \
         /bin/printf \"%s\\n\" outside > {codex_probe_root}/work/fixture || \
         {{ echo \"codex: could not write the outer sandbox fixture\"; exit 1; }}; \
         /bin/readlink /proc/self/ns/net > {codex_probe_root}/work/outer-net || \
         {{ echo \"codex: could not record the outer network namespace\"; exit 1; }}; \
         s=$(HOME={codex_probe_root}/home \
         CODEX_HOME={codex_probe_root}/home/.codex PATH=/bin \
         /bin/codex sandbox -P :read-only -C {codex_probe_root}/work \
         /bin/sh -c '\\''if {{ /bin/printf sandboxed >> fixture; }} 2>/dev/null; then \
         echo write-was-not-confined; exit 1; fi; \
         v=$(/bin/cat fixture) || \
         {{ /bin/echo sandbox-fixture-unreadable; exit 1; }}; \
         [ \"$v\" = outside ] || \
         {{ /bin/echo sandbox-fixture-changed: \"$v\"; exit 1; }}; \
         outer_net=$(/bin/cat outer-net) || \
         {{ /bin/echo sandbox-outer-network-namespace-unreadable; exit 1; }}; \
         inner_net=$(/bin/readlink /proc/self/ns/net) || \
         {{ /bin/echo sandbox-inner-network-namespace-unreadable; exit 1; }}; \
         [ \"$inner_net\" != \"$outer_net\" ] || \
         {{ /bin/echo sandbox-network-namespace-unchanged: \"$inner_net\"; exit 1; }}; \
         /bin/echo TD-CODEX-SANDBOX-OK'\\'' 2>&1) || \
         {{ echo \"codex: read-only Bubblewrap transition failed: $s\"; exit 1; }}; \
         /bin/printf \"%s\\n\" \"$s\" | \
         /bin/grep -q -x -F TD-CODEX-SANDBOX-OK || \
         {{ echo \"codex: sandbox transition omitted its success evidence: $s\"; \
         exit 1; }}; \
         /bin/rm -rf {codex_probe_root} || \
         {{ echo \"codex: could not clean the sandbox probe\"; exit 1; }}'; then \
         [ \"$mc\" = 1 ] || {{ echo {CODEX_RUNTIME_MARKER}; mc=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         'o=$(/bin/ssh -F /dev/null -i /run/td-ssh-selftest \
         -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
         -o UserKnownHostsFile=/run/td-ssh-known-hosts \
         -o GlobalKnownHostsFile=/dev/null \
         -o KexAlgorithms={OPENSSH_KEX_ALGORITHMS} \
         -o HostKeyAlgorithms={OPENSSH_KEY_ALGORITHMS} \
         -o PubkeyAcceptedAlgorithms={OPENSSH_KEY_ALGORITHMS} \
         -o Ciphers={OPENSSH_CIPHERS} -o Compression=no \
         {git_user}@127.0.0.1 /bin/echo TD-OPENSSH-ROUNDTRIP 2>&1) || \
         {{ echo \"OpenSSH: loopback command failed: $o\"; exit 1; }}; \
         [ \"$o\" = TD-OPENSSH-ROUNDTRIP ] || \
         {{ echo \"OpenSSH: unexpected loopback output: $o\"; exit 1; }}'; then \
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
         '{td_login_probe}' && {td_login_exec_as_probe}; then \
         [ \"$mtl\" = 1 ] || {{ echo {TD_LOGIN_RUNTIME_MARKER}; mtl=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         '{td_txt_probes}[ \"$t\" = 1 ]'; then \
         [ \"$mtt\" = 1 ] || {{ echo {TD_TXT_RUNTIME_MARKER}; mtt=1; }}; else healthy=0; fi; \
         if /bin/su -s /bin/sh {} -c \
         'b=$(/bin/td-busd probe /run/user/{UI_UID}/bus 2>&1) || \
         {{ echo \"td-busd: the session bus did not answer on /run/user/{UI_UID}/bus: \
         $b\"; \
         exit 1; }}'; then \
         [ \"$mtb\" = 1 ] || {{ echo {TD_BUSD_RUNTIME_MARKER}; mtb=1; }}; \
         else btb=$((btb+1)); fi; \
         [ \"$msk\" = 1 ] || healthy=0; \
         [ \"$mtj\" = 1 ] || healthy=0; \
         [ \"$mts\" = 1 ] || healthy=0; \
         if [ \"$healthy\" = 1 ] \
         && {{ [ \"$mtb\" = 1 ] || [ \"$btb\" -ge \"$bg\" ]; }} \
         && /bin/td-boot success /dev/vda /run/td-update \"$deployment\" >/run/td-success-id; then \
         if /bin/grep -q -F '{DEPLOY_INSTALL_CMDLINE_TOKEN}' /proc/cmdline; then \
         if /bin/td-boot {update} /dev/vda /run/td-update /run/td-volume \
         /run/td-volume/{channel} /run/td-volume/{wrong_key} \
         >/run/td-refused-id 2>/run/td-refused-err; then \
         echo 'td-boot update accepted a bundle under the wrong key'; healthy=0; \
         elif ! /bin/grep -q -F '{unauthenticated}' /run/td-refused-err; then \
         echo 'td-boot update refused under the wrong key for another reason'; \
         healthy=0; \
         elif ! /bin/td-boot {update} /dev/vda /run/td-update /run/td-volume \
         /run/td-volume/{idle_channel} /run/td-volume/{trusted_key} \
         >/run/td-idle-id; then \
         echo 'td-boot update failed on a channel with nothing in it'; healthy=0; \
         elif [ -s /run/td-idle-id ]; then \
         echo 'td-boot update named a deployment for an empty channel'; healthy=0; \
         elif ! /bin/td-boot {update} /dev/vda /run/td-update /run/td-volume \
         /run/td-volume/{channel} /run/td-volume/{trusted_key} \
         >/run/td-installed-id; then \
         echo 'td-boot update failed on the channel holding a bundle'; healthy=0; \
         elif ! [ -s /run/td-installed-id ]; then \
         echo 'td-boot update installed nothing from the channel holding a bundle'; \
         healthy=0; \
         else echo {SYSTEM_DEPLOY_INSTALL_MARKER}; \
         if ! /bin/td-boot rollback /dev/vda /run/td-update >/run/td-rolled-id; then \
         echo 'td-boot rollback failed after the update installed a deployment'; \
         healthy=0; \
         elif ! /bin/grep -q -x -F \"$deployment\" /run/td-rolled-id; then \
         echo 'td-boot rollback did not return to the deployment that booted'; \
         healthy=0; \
         elif ! /bin/td-boot success /dev/vda /run/td-update \"$deployment\" \
         >/run/td-rolled-current; then \
         echo 'td-boot rollback printed an id without making it current'; healthy=0; \
         elif ! /bin/td-boot {update} /dev/vda /run/td-update /run/td-volume \
         /run/td-volume/{channel} /run/td-volume/{trusted_key} \
         >/run/td-reinstalled-id; then \
         echo 'td-boot update could not reinstall the deployment after a rollback'; \
         healthy=0; \
         elif ! /bin/grep -q -x -F -f /run/td-installed-id /run/td-reinstalled-id; then \
         echo 'the reinstall after a rollback named a different deployment'; healthy=0; \
         else echo {SYSTEM_DEPLOY_ROLLBACK_MARKER}; fi; fi; \
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
        sys.autologin,
        sys.autologin,
        sys.autologin,
        sys.autologin,
        git_user = sys.autologin,
        probe_user = UI_USER,
        codex_probe_root = format!("/run/user/{UI_UID}/td-codex-sandbox-probe"),
        codex_version = CODEX_VERSION_OUTPUT,
        bwrap_version = CODEX_BWRAP_VERSION_OUTPUT,
        hostname = sys.hostname,
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
    // The login shell (td-sh, invoked as `-sh` by td-login) sources this. We print
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
         exec /bin/sh -c 'cd / \
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
         input_required=0; case \" $(/bin/td-util cat /proc/cmdline) \" in \
         *\" {firefox_input_cmdline_token} \"*) input_required=1;; esac; \
         n=0; firefox_wait=0; while [ \"$n\" -lt \"$wait\" ]; do \
         status=$(/bin/td-util cat /run/td-boot-success-ok 2>/dev/null); \
         firefox=$(/bin/td-util cat {firefox_evidence_path} 2>/dev/null); \
         firefox_complete=$(/bin/td-util cat {firefox_completion_path} 2>/dev/null); \
         input_ok=1; if [ \"$input_required\" = 1 ]; then input_ok=0; \
         input=$(/bin/td-util cat {firefox_input_completion_path} 2>/dev/null); \
         [ \"$input\" = {firefox_input_completion} ] && input_ok=1; fi; \
         [ \"$status\" = td-boot-success-v1 ] && [ \"$firefox\" = {firefox_evidence} ] && \
         [ \"$firefox_complete\" = {firefox_completion} ] && [ \"$input_ok\" = 1 ] && break; \
         if [ \"$status\" = td-boot-success-v1 ]; then \
         firefox_wait=$((firefox_wait+1)); \
         [ \"$firefox_wait\" -ge {firefox_greeter_wait} ] && break; fi; \
         [ \"$status\" = td-boot-failure-v1 ] && break; \
         n=$((n+1)); /bin/td-util sleep 1; done; \
         exit 0; fi\n",
        firefox_evidence = FIREFOX_EVIDENCE,
        firefox_evidence_path = FIREFOX_EVIDENCE_PATH,
        firefox_completion = FIREFOX_COMPLETION,
        firefox_completion_path = FIREFOX_COMPLETION_PATH,
        firefox_input_cmdline_token = FIREFOX_INPUT_CMDLINE_TOKEN,
        firefox_input_completion = FIREFOX_INPUT_COMPLETION,
        firefox_input_completion_path = FIREFOX_INPUT_COMPLETION_PATH,
        firefox_greeter_wait = FIREFOX_GREETER_WAIT_ITERATIONS,
    ));
    s
}

/// The sysinit network bring-up glue, run AS ROOT once at boot. `td-netd up`
/// autodetects the link, DHCP-configures it, and writes resolv.conf + hosts (a
/// NIC-less boot is a clean no-op). Under the `NETTEST_CMDLINE_TOKEN` the headless
/// `qemu-boot-net` oracle appends, it additionally self-tests the stack — resolve
/// the default host via the DHCP-provided nameserver, TCP-reach it, then run an
/// unprivileged Git HTTPS query with the installed CA trust — printing the four
/// net markers on ttyS0. Off the token (normal boot, or the `-nic none`
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
         /bin/su -s /bin/sh {UI_USER} -c \
         'HOME=/tmp/td-git-net-home; export HOME; \
         XDG_CONFIG_HOME=/tmp/td-git-net-xdg; export XDG_CONFIG_HOME; \
         /bin/rm -rf \"$HOME\" \"$XDG_CONFIG_HOME\" && \
         /bin/mkdir -p \"$HOME\" \"$XDG_CONFIG_HOME\" && \
         GIT_CONFIG_GLOBAL=/dev/null; export GIT_CONFIG_GLOBAL; \
         GIT_CONFIG_NOSYSTEM=1; export GIT_CONFIG_NOSYSTEM; \
         GIT_TERMINAL_PROMPT=0; export GIT_TERMINAL_PROMPT; \
         GIT_HTTP_LOW_SPEED_LIMIT=1; export GIT_HTTP_LOW_SPEED_LIMIT; \
         GIT_HTTP_LOW_SPEED_TIME=10; export GIT_HTTP_LOW_SPEED_TIME; \
         GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt; export GIT_SSL_CAINFO; \
         r=$(/bin/git ls-remote {GIT_HTTPS_TEST_URL} HEAD) && \
         set -- $r && [ \"$#\" = 2 ] && [ \"${{#1}}\" = 40 ] && [ \"$2\" = HEAD ]' && \
         echo {GIT_HTTPS_RUNTIME_MARKER}; \
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

/// A name under the read-only `/etc` that points INTO the store.
///
/// The opposite of `MUTABLE_ETC` in every respect that matters: the target is
/// image content rather than per-machine state, so the symlink resolves at
/// build time rather than dangling, and it must never be written to. It
/// exists because some readers only know one path — ncurses looks a terminal
/// up under `TERMINFO`, and the store path a package lands at is content
/// addressed and so cannot be spelled in an environment variable.
struct ImmutableEtc {
    /// Path under `/etc` — the stable name the reader knows.
    etc: &'static str,
    /// Absolute symlink target inside the store.
    target: &'static str,
    /// Which reader only knows the `/etc` name, and why the store path cannot
    /// be given to it directly.
    why: &'static str,
}

const IMMUTABLE_ETC: &[ImmutableEtc] = &[
    ImmutableEtc {
        etc: "terminfo",
        target: "{in:td-compositor}/share/terminfo",
        why: "ncurses resolves TERM through TERMINFO, and td-term hands its child \
              TERMINFO=/etc/terminfo because a content-addressed store path is not \
              a name any child could have been given",
    },
    ImmutableEtc {
        etc: "ssl/certs/ca-certificates.crt",
        target: "{in:ca-certificates}/share/ca-certificates/ca-bundle.crt",
        why: "The static curl transport used by Git needs this CA bundle path, \
              while the pinned Mozilla extract remains immutable store content",
    },
];

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
        target: SSHD_AUTHORIZED_KEYS_STATE,
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
const SSHD_AUTHORIZED_KEYS_STATE: &str = "/var/lib/td/ssh/authorized_keys";
const SSHD_SELFTEST_USER: &str = UI_USER;
const SSHD_SELFTEST_AUTHORIZED_KEYS: &str = "/run/td-ssh-selftest-authorized_keys";
const QEMU_OPENSSH_ADMIN_PRIVATE_KEY: &str = "/var/lib/td-test/openssh-admin-selftest";
const OPENSSH_KEX_ALGORITHMS: &str =
    "mlkem768x25519-sha256,sntrup761x25519-sha512,curve25519-sha256";
const OPENSSH_KEY_ALGORITHMS: &str = "ssh-ed25519";
const OPENSSH_CIPHERS: &str = "chacha20-poly1305@openssh.com";

fn build_ssh_config() -> String {
    format!(
        "Host *\n\
         \tKexAlgorithms {OPENSSH_KEX_ALGORITHMS}\n\
         \tHostKeyAlgorithms {OPENSSH_KEY_ALGORITHMS}\n\
         \tPubkeyAcceptedAlgorithms {OPENSSH_KEY_ALGORITHMS}\n\
         \tCiphers {OPENSSH_CIPHERS}\n\
         \tCompression no\n\
         \tForwardAgent no\n\
         \tHashKnownHosts yes\n\
         \tVerifyHostKeyDNS no\n"
    )
}

pub(super) fn build_sshd_config() -> String {
    format!(
        "Port 22\n\
         ListenAddress 0.0.0.0\n\
         HostKey {SSHD_HOST_KEY}\n\
         AuthorizedKeysFile {SSHD_AUTHORIZED_KEYS}\n\
         AuthenticationMethods publickey\n\
         PubkeyAuthentication yes\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         ChallengeResponseAuthentication no\n\
         HostbasedAuthentication no\n\
         PermitEmptyPasswords no\n\
         PermitRootLogin prohibit-password\n\
         StrictModes yes\n\
         KexAlgorithms {OPENSSH_KEX_ALGORITHMS}\n\
         HostKeyAlgorithms {OPENSSH_KEY_ALGORITHMS}\n\
         PubkeyAcceptedAlgorithms {OPENSSH_KEY_ALGORITHMS}\n\
         Ciphers {OPENSSH_CIPHERS}\n\
         Compression no\n\
         DisableForwarding yes\n\
         PermitTTY yes\n\
         PermitUserEnvironment no\n\
         PermitUserRC no\n\
         UseDNS no\n\
         PrintMotd no\n\
         LoginGraceTime 30\n\
         MaxAuthTries 3\n\
         MaxSessions 4\n\
         PidFile /run/sshd.pid\n\
         Match User {SSHD_SELFTEST_USER}\n\
         \tAuthorizedKeysFile {SSHD_SELFTEST_AUTHORIZED_KEYS}\n"
    )
}

/// Every parent directory needed by either `/etc` table, including intermediate
/// parents. The same list drives staging and the fail-closed directory and symlink
/// sweeps, so adding a nested immutable name cannot create an unchecked subtree.
fn etc_dirs() -> Vec<&'static str> {
    let mut dirs = Vec::new();
    for path in MUTABLE_ETC
        .iter()
        .map(|entry| entry.etc)
        .chain(IMMUTABLE_ETC.iter().map(|entry| entry.etc))
    {
        for (index, _) in path.match_indices('/') {
            if let Some(dir) = path.get(..index) {
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
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

fn build_application_config() -> String {
    crate::ladder::TD_APPLICATION_CONFIG_TEXT.to_string()
}

fn application_etc_name(path: &'static str) -> &'static str {
    match path.strip_prefix("/etc/") {
        Some(name) => name,
        None => path,
    }
}

fn application_names(sys: &SystemDef) -> Vec<&'static str> {
    sys.applications
        .iter()
        .map(|application| application.name)
        .collect()
}

fn application_payload_inputs(sys: &SystemDef) -> Vec<&'static str> {
    let mut inputs: Vec<&str> = sys
        .applications
        .iter()
        .flat_map(|application| [application.package, application.runtime])
        .collect();
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

fn profiler_application_roots(sys: &SystemDef) -> String {
    let mut rows: Vec<_> = sys
        .applications
        .iter()
        .map(|application| {
            let package = (application.package_recipe)();
            let runtime = (application.runtime_recipe)();
            format!(
                "{}\t{}-{}\t{}\t{}-{}\t{}\n",
                application.name,
                package.name,
                package.version,
                if package.is_foreign() { "foreign" } else { "source" },
                runtime.name,
                runtime.version,
                if runtime.is_foreign() { "foreign" } else { "source" },
            )
        })
        .collect();
    rows.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut table = String::from("td-profiler-application-roots-v1\n");
    for row in rows {
        table.push_str(&row);
    }
    table
}

/// QEMU-only trusted setup for Firefox's real NSS/TLS proof. The authority,
/// server identity and policy live on `/run`, so neither the CA nor its private
/// key survives a boot or enters the installed browser's ordinary profile.
fn build_firefox_tls_setup() -> String {
    format!(
        "#!/bin/sh\n\
         set -eu\n\
         case \" $(/bin/cat /proc/cmdline) \" in\n\
         *\" {autotest} \"*) :;;\n\
         *) exit 0;;\n\
         esac\n\
         /bin/td-netd loopback\n\
         root={root}\n\
         origin={origin}\n\
         openssl=/bin/openssl\n\
         OPENSSL_CONF=\"$root/openssl.cnf\"\n\
         export OPENSSL_CONF\n\
         umask 077\n\
         /bin/mkdir -p \"$origin\"\n\
         /bin/td-util printf '%s\\n' \
           '[req]' \
           'distinguished_name=dn' \
           '[dn]' \
           '[server]' \
           'basicConstraints=critical,CA:FALSE' \
           'keyUsage=critical,digitalSignature,keyEncipherment' \
           'extendedKeyUsage=serverAuth' \
           'subjectAltName=DNS:localhost,IP:127.0.0.1' \
           > \"$OPENSSL_CONF\"\n\
         \"$openssl\" req -new -x509 -newkey rsa:2048 -nodes -sha256 -days 1 \
           -config \"$OPENSSL_CONF\" \
           -set_serial 1 -subj '/CN=td Firefox autotest CA' \
           -addext 'basicConstraints=critical,CA:TRUE' \
           -addext 'keyUsage=critical,keyCertSign,cRLSign' \
           -keyout \"$root/ca.key\" -out \"$root/ca.pem\"\n\
         \"$openssl\" req -new -newkey rsa:2048 -nodes \
           -config \"$OPENSSL_CONF\" \
           -subj '/CN=localhost' -keyout \"$root/server.key\" \
           -out \"$root/server.csr\"\n\
         \"$openssl\" x509 -req -in \"$root/server.csr\" \
           -CA \"$root/ca.pem\" -CAkey \"$root/ca.key\" -set_serial 2 \
           -sha256 -days 1 -extfile \"$OPENSSL_CONF\" -extensions server \
           -out \"$root/server.pem\"\n\
         /bin/td-util printf '%s\\n' '{policy}' > \"$root/policies.json\"\n\
         /bin/td-util printf '%s\\n' '{document}' > \"$origin/content.html\"\n\
         /bin/td-util printf '%s\\n' '{download}' > \"$origin/download.txt\"\n\
         /bin/td-util chmod 0444 \"$root/ca.pem\" \"$root/policies.json\" \
           \"$root/server.pem\" \"$origin/content.html\" \"$origin/download.txt\"\n\
         /bin/td-util chmod 0400 \"$root/server.key\"\n\
         /bin/chown {ui_uid}:{ui_gid} \"$root/server.key\"\n\
         /bin/td-util chmod 0755 \"$root\"\n\
         /bin/td-util chmod 0555 \"$origin\"\n",
        autotest = AUTOTEST_CMDLINE_TOKEN,
        root = FIREFOX_TLS_ROOT,
        origin = FIREFOX_TLS_ORIGIN,
        policy = FIREFOX_TLS_POLICY.trim_end(),
        document = FIREFOX_HTTPS_DOCUMENT,
        download = FIREFOX_DOWNLOAD_FIXTURE,
        ui_uid = UI_UID,
        ui_gid = UI_GID,
    )
}

/// LibreSSL's `-accept` grammar takes only a port, so this listens on all guest
/// interfaces. The autotest harness uses either `-nic none` or exact QEMU
/// user-mode NAT without host forwarding; the token is unsupported elsewhere.
/// `s_server` discards its accept-loop result, so any return is translated to a
/// service failure; a requested stop is classified separately by td-svc.
fn build_firefox_tls_origin() -> String {
    format!(
        "#!/bin/sh\n\
         set -eu\n\
         case \" $(/bin/cat /proc/cmdline) \" in\n\
         *\" {autotest} \"*) :;;\n\
         *) exit 0;;\n\
         esac\n\
         OPENSSL_CONF=/dev/null\n\
         export OPENSSL_CONF\n\
         cd {origin}\n\
         /bin/openssl s_server \
           -accept 8443 -cert {root}/server.pem -key {root}/server.key \
           -WWW -quiet\n\
         exit 1\n",
        autotest = AUTOTEST_CMDLINE_TOKEN,
        origin = FIREFOX_TLS_ORIGIN,
        root = FIREFOX_TLS_ROOT,
    )
}

fn build_firefox_tls_ready() -> String {
    format!(
        "#!/bin/sh\n\
         set -eu\n\
         case \" $(/bin/cat /proc/cmdline) \" in\n\
         *\" {autotest} \"*) :;;\n\
         *) exit 0;;\n\
         esac\n\
         OPENSSL_CONF=/dev/null\n\
         export OPENSSL_CONF\n\
         /bin/td-util printf 'GET /content.html HTTP/1.0\\r\\nHost: localhost\\r\\n\\r\\n' | \
           /bin/openssl s_client \
             -connect 127.0.0.1:8443 -servername localhost \
             -CAfile {root}/ca.pem -verify 5 -verify_return_error \
             -quiet 2>/dev/null | \
           /bin/grep 'TD-FIREFOX-HTTPS-CONTENT-V1' >/dev/null\n",
        autotest = AUTOTEST_CMDLINE_TOKEN,
        root = FIREFOX_TLS_ROOT,
    )
}

/// The generated /etc files (config + the login-glue and boot-check scripts). `exec`
/// marks the ones getty/init reference as executables. Shared by the real-root staging
/// (written under `{root}/real-root/etc`) and the shape check (which asserts they landed).
fn etc_files(sys: &SystemDef) -> Vec<(&'static str, String, bool)> {
    vec![
        ("passwd", build_passwd(sys), false),
        ("group", build_group(sys), false),
        (SHADOW_ETC_NAME, build_shadow(sys), false),
        ("hostname", format!("{}\n", sys.hostname), false),
        ("os-release", build_os_release(sys), false),
        (
            td_portal_settings_etc_name(),
            TD_PORTAL_SETTINGS.to_string(),
            false,
        ),
        ("ssh/ssh_config", build_ssh_config(), false),
        ("ssh/sshd_config", build_sshd_config(), false),
        ("mutable-state", build_mutable_state(), false),
        (
            application_etc_name(APPLICATION_CONFIG),
            build_application_config(),
            false,
        ),
        (
            application_etc_name(PROFILER_APPLICATION_ROOTS),
            profiler_application_roots(sys),
            false,
        ),
        ("inittab", build_inittab(), false),
        (td_svc_conf_etc_name(), build_td_svc_conf(), false),
        ("profile", build_profile(sys), false),
        // Executable glue (mode 0755): getty execs autologin; init respawns tty-session
        // and runs rootcheck at sysinit. They live in /etc so /bin stays a pure
        // store-symlink farm.
        ("autologin", build_autologin(sys), true),
        ("tty-session", build_tty_session(), true),
        ("shutdown", build_shutdown(), true),
        (ROOTCHECK_ETC_NAME, build_rootcheck(sys), true),
        ("netup", build_netup(), true),
        ("firefox-tls-setup", build_firefox_tls_setup(), true),
        ("firefox-tls-origin", build_firefox_tls_origin(), true),
        ("firefox-tls-ready", build_firefox_tls_ready(), true),
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
    // The shell, at its content-addressed /td/store path, with the cpio's /bin/sh
    // pointing straight at it. This is td-sh, and busybox is NOT HERE: it was packed
    // into both initramfs images for this one symlink, so the flip does not merely
    // repoint /bin/sh, it takes the third-party multicall out of the boot path
    // altogether. td-sh is a static ET_EXEC with an empty runtime closure, which is
    // what lets it run here — nothing has mounted the real root yet, so a
    // dynamically-linked shell would be a kernel panic rather than a degraded boot.
    s.push_str("dir {in:td-sh} 0755 0 0\n");
    s.push_str("dir {in:td-sh}/bin 0755 0 0\n");
    s.push_str("file {in:td-sh}/bin/td-sh {in:td-sh}/bin/td-sh 0755 0 0\n");
    s.push_str("dir /bin 0755 0 0\n");
    s.push_str("slink /bin/sh {in:td-sh}/bin/td-sh 0777 0 0\n");
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
/// PackErofs step later packs it into the deployment output. Uses typed steps (no shell): each
/// packaged multicall is copied to its /td/store path, /bin is a symlink farm into them
/// (the shell resolving to td-sh), /init is a symlink to td-init, /etc holds the generated
/// config, and the pseudo-fs + writable mountpoint dirs are created empty
/// (stage-1/init mount over them). `/home` and `/root`
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
    steps.push(Step::CopyTree {
        from: "{in:td-sh}".into(),
        dest: "{root}/real-root{in:td-sh}".into(),
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
    // The static system-wide collector and offline reader. It opens perf while
    // privileged, then runs and writes captures as the dedicated account.
    steps.push(Step::CopyTree {
        from: "{in:td-profiler}".into(),
        dest: "{root}/real-root{in:td-profiler}".into(),
    });
    // td-jail is static so the running-kernel transition oracle does not depend
    // on the dynamic userland it helps confine.
    steps.push(Step::CopyTree {
        from: "{in:td-jail}".into(),
        dest: "{root}/real-root{in:td-jail}".into(),
    });
    // The software UI is static and owns no dynamic runtime closure.
    steps.push(Step::CopyTree {
        from: "{in:td-seatd}".into(),
        dest: "{root}/real-root{in:td-seatd}".into(),
    });
    steps.push(Step::CopyTree {
        from: "{in:td-audio}".into(),
        dest: "{root}/real-root{in:td-audio}".into(),
    });
    steps.push(Step::CopyTree {
        from: "{in:td-compositor}".into(),
        dest: "{root}/real-root{in:td-compositor}".into(),
    });
    // The session bus broker: static, dependency-free, and copied directly like
    // the rest of the session substrate above it.
    steps.push(Step::CopyTree {
        from: "{in:td-busd}".into(),
        dest: "{root}/real-root{in:td-busd}".into(),
    });
    // The activated Settings portal is a separate static process. Its root
    // supervisor and unprivileged child execute the same binary, while the
    // broker retains the capability that authorizes only that direct child.
    steps.push(Step::CopyTree {
        from: "{in:td-portal}".into(),
        dest: "{root}/real-root{in:td-portal}".into(),
    });
    // Codex's exact Bubblewrap helper is static, so preserve its canonical package
    // directly instead of treating source-provenance strings as runtime edges.
    steps.push(Step::CopyTree {
        from: "{in:codex-bwrap}".into(),
        dest: "{root}/real-root{in:codex-bwrap}".into(),
    });
    // The CA extract is immutable data, not an executable runtime closure. Copy
    // the package at its canonical store path so IMMUTABLE_ETC can expose the
    // conventional filename without duplicating the bundle in /etc.
    steps.push(Step::CopyTree {
        from: "{in:ca-certificates}".into(),
        dest: "{root}/real-root{in:ca-certificates}".into(),
    });
    // The QEMU HTTPS origin needs only LibreSSL's static command and its debug
    // companion. Keep the development archives and headers out of the image.
    steps.push(Step::MkDir {
        path: "{root}/real-root{in:libressl-x86-64}".into(),
    });
    for child in ["bin", "lib/debug"] {
        steps.push(Step::CopyTree {
            from: format!("{{in:libressl-x86-64}}/{child}"),
            dest: format!("{{root}}/real-root{{in:libressl-x86-64}}/{child}"),
        });
    }
    // Stage the dynamically linked userland and every transitively referenced store item
    // at its canonical absolute path. uutils, ripgrep, fd, OpenSSH, and Codex pull their
    // td glibc closures. The engine admits only direct recipe inputs, so a Rust bootstrap
    // or other build-only reference fails closed rather than entering the EROFS image.
    let mut runtime_roots = vec![
        "{in:uutils}".into(),
        "{in:ripgrep}".into(),
        "{in:fd}".into(),
        "{in:openssh-x86-64}".into(),
        "{in:git-x86-64}".into(),
        "{in:codex}".into(),
    ];
    runtime_roots.extend(
        application_payload_inputs(sys)
            .into_iter()
            .map(|input| format!("{{payload:{input}}}")),
    );
    steps.push(Step::StageRuntimeClosure {
        roots: runtime_roots,
        dest: "{root}/real-root".into(),
    });
    steps.push(Step::CompileApplicationTables {
        names: sys
            .applications
            .iter()
            .map(|application| application.name.to_string())
            .collect(),
        packages: sys
            .applications
            .iter()
            .map(|application| format!("{{payload:{}}}", application.package))
            .collect(),
        runtimes: sys
            .applications
            .iter()
            .map(|application| format!("{{payload:{}}}", application.runtime))
            .collect(),
        registry: format!("{{root}}/real-root{APPLICATION_REGISTRY}"),
        launcher: format!("{{root}}/real-root{APPLICATION_LAUNCHER_TABLE}"),
    });
    // The shell. Two names, ONE static binary — td-sh is not a multicall and does not
    // dispatch on argv[0]; `ash` runs the same program `sh` does, which is what makes a
    // script that spells either one get the same shell.
    for app in TD_SH_APPLETS {
        steps.push(Step::Symlink {
            target: "{in:td-sh}/bin/td-sh".into(),
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
    for (name, target) in [
        ("rg", "{in:ripgrep}/bin/rg"),
        ("fd", "{in:fd}/bin/fd"),
        ("git", "{in:git-x86-64}/bin/git"),
        (
            "git-receive-pack",
            "{in:git-x86-64}/bin/git-receive-pack",
        ),
        (
            "git-upload-archive",
            "{in:git-x86-64}/bin/git-upload-archive",
        ),
        (
            "git-upload-pack",
            "{in:git-x86-64}/bin/git-upload-pack",
        ),
        ("openssl", "{in:libressl-x86-64}/bin/openssl"),
        ("codex", "{in:codex}/bin/codex"),
        ("bwrap", "{in:codex-bwrap}/bin/bwrap"),
    ] {
        steps.push(Step::Symlink {
            target: target.into(),
            link: format!("{{root}}/real-root/bin/{name}"),
        });
    }
    for application in sys.applications {
        steps.push(Step::Symlink {
            target: "/bin/td-jail".into(),
            link: format!("{{root}}/real-root/bin/{}", application.name),
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
        target: "{in:td-jail}/bin/td-jail".into(),
        link: "{root}/real-root/bin/td-jail".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-seatd}/bin/td-seatd".into(),
        link: "{root}/real-root/bin/td-seatd".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-audio}/bin/td-audio".into(),
        link: "{root}/real-root/bin/td-audio".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-compositor}/bin/td-compositor".into(),
        link: "{root}/real-root/bin/td-compositor".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-compositor}/bin/td-term".into(),
        link: "{root}/real-root/bin/td-term".into(),
    });
    // /bin/td-busd — the session bus. Named in full by the busd unit's exec and
    // ready lines; no basename dispatch and no applet farm.
    steps.push(Step::Symlink {
        target: "{in:td-busd}/bin/td-busd".into(),
        link: "{root}/real-root/bin/td-busd".into(),
    });
    steps.push(Step::Symlink {
        target: "{in:td-portal}/bin/td-portal".into(),
        link: "{root}/real-root/bin/td-portal".into(),
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
    for dir in etc_dirs() {
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
    // The immutable /etc: names that resolve INTO the store. Unlike the table
    // above these are not dangling — the target is image content — and nothing
    // ever writes through them.
    for entry in IMMUTABLE_ETC {
        steps.push(Step::Symlink {
            target: entry.target.into(),
            link: format!("{{root}}/real-root/etc/{}", entry.etc),
        });
    }
    // The bounded OpenSSH profile exposes the client, host-key tool, and daemon
    // through /bin. sshd's mandatory split helpers stay at their compiled
    // content-addressed libexec paths and are reached only by the daemon.
    for name in ["ssh", "ssh-keygen", "sshd"] {
        steps.push(Step::Symlink {
            target: format!("{{in:openssh-x86-64}}/bin/{name}"),
            link: format!("{{root}}/real-root/bin/{name}"),
        });
    }
    steps.push(Step::Symlink {
        target: "{in:td-profiler}/bin/td-profiler".into(),
        link: "{root}/real-root/bin/td-profiler".into(),
    });
    // Generated /etc.
    for (name, content, exec) in etc_files(sys) {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/real-root/etc/{name}"),
            content,
            exec,
        });
    }
    // Compile the final deployment's object map only after all source-built
    // closures and generated application tables exist. The registry names exact
    // package roots; the indexer binds their builder-authenticated manifest/spec
    // metadata to the catalog-derived package/runtime provenance table. Application
    // manifest provenance describes containment and is deliberately not transferred
    // to either root.
    steps.push(Step::run(
        "{root}",
        &[
            "{in:td-profiler}/bin/td-profiler",
            "index",
            "{root}/real-root",
            "{root}/real-root/etc/td-profiler-objects.tsv",
            "--exclude-registry",
            "{root}/real-root/etc/td-applications.tsv",
            "{root}/real-root/etc/td-profiler-application-roots.tsv",
        ],
    ));
    steps
}

/// A producer-rung shape check on the deployment bundle and staged real-root
/// scratch tree. For the cpio: real newc magic, a size floor (the static shell alone is
/// larger), a `busybox cpio -t` parse (the declared INPUT's, as a build tool — this
/// recipe's own steps run under `busybox sh` too), the members that make it bootable
/// (incl. the /init pivot script), and each packed binary under /td/store. For the root
/// tree: /init and /bin/sh are symlinks into /td/store, the key /etc files exist, and
/// NOTHING busybox is packed — neither a `/bin/busybox` multiplexer entry nor the
/// package under /td/store. That assertion replaced its own opposite when `getty` moved
/// to td-init: the check that the binary WAS packed, and that it implemented every name
/// symlinked at it. What used to be config drift leaving a dead `/bin/getty` is now a
/// build tool leaking into an image, and both are things only a build-time check sees.
/// For the
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
         [ \"$sz\" -ge 65536 ] || { echo \"initramfs $archive: implausibly small ($sz bytes) - the static shell alone is larger\" >&2; exit 1; }; \
         set -- $(od -An -tx1 -N 6 \"$archive\"); \
         [ \"$1$2$3$4$5$6\" = 303730373031 ] || { echo \"initramfs $archive: missing the newc cpio magic 070701\" >&2; exit 1; }; \
         \"$bb\" cpio -t < \"$archive\" >/dev/null 2>&1 || { echo \"initramfs $archive: busybox cpio -t could not parse the archive\" >&2; exit 1; }; \
     done; \
     selector_list=$(\"$bb\" cpio -t < \"$selector\" 2>/dev/null); \
     init_list=$(\"$bb\" cpio -t < \"$init\" 2>/dev/null); \
     for m in init bin/sh bin/td-boot bin/mount bin/umount bin/td-util dev/console proc run volume sysroot; do \
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
     printf '%s\\n' \"$selector_list\" | grep -qE '^td/store/[^/]+/bin/td-sh$' || { echo 'selector initramfs: the shell store member is missing - /bin/sh would dangle and the kernel could not run /init' >&2; exit 1; }; \
     printf '%s\\n' \"$init_list\" | grep -qE '^td/store/[^/]+/bin/td-sh$' || { echo 'deployment initramfs: the shell store member is missing - /bin/sh would dangle and the kernel could not run /init' >&2; exit 1; }; \
     [ -f \"$root/init\" ] || [ -L \"$root/init\" ] || { echo 'root tree: /init missing' >&2; exit 1; }; \
     case $(readlink \"$root/init\") in /td/store/*) : ;; *) echo 'root tree: /init is not a symlink into /td/store' >&2; exit 1;; esac; \
     case $(readlink \"$root/bin/sh\") in /td/store/*) : ;; *) echo 'root tree: /bin/sh is not a symlink into /td/store - the store-native /bin farm regressed' >&2; exit 1;; esac; \
     for f in passwd group shadow hostname os-release @TD_PORTAL_SETTINGS_NAME@ mutable-state inittab @TD_SVC_CONF_NAME@ @APPLICATION_CONFIG_NAME@ profile autologin tty-session shutdown rootcheck netup bootsuccess bootfail; do \
         [ -f \"$root/etc/$f\" ] || { echo \"root tree: /etc/$f missing\" >&2; exit 1; }; \
         if [ -L \"$root/etc/$f\" ]; then echo \"root tree: /etc/$f is a symlink - immutable image config must be a regular file in the erofs, not a hole in the read-only /etc\" >&2; exit 1; fi; \
     done; \
     for f in @APPLICATION_REGISTRY_NAME@ @APPLICATION_LAUNCHER_NAME@; do \
         [ -f \"$root/etc/$f\" ] || { echo \"root tree: /etc/$f missing - compileApplicationTables did not materialize the application image contract\" >&2; exit 1; }; \
         if [ -L \"$root/etc/$f\" ]; then echo \"root tree: /etc/$f is a symlink - application selection must be immutable image content\" >&2; exit 1; fi; \
         [ \"$(wc -l < \"$root/etc/$f\")\" -eq @APPLICATION_COUNT@ ] || { echo \"root tree: /etc/$f does not have one row per shipped application\" >&2; exit 1; }; \
     done; \
     for pair in @MUTABLE_ETC@; do \
         l=${pair%%=*}; t=${pair#*=}; \
         [ \"$(readlink \"$root/etc/$l\")\" = \"$t\" ] || { echo \"root tree: /etc/$l must be a symlink to $t - it is a reviewed MUTABLE_ETC entry, so its writes must land on the state it names and nowhere else\" >&2; exit 1; }; \
         grep -q -F \"$l  \" \"$root/etc/mutable-state\" || { echo \"root tree: /etc/mutable-state does not document /etc/$l - the shipped list of holes in the read-only /etc must name every one of them\" >&2; exit 1; }; \
     done; \
     @IMMUTABLE_ETC_WHY@; \
     for pair in @IMMUTABLE_ETC@; do \
         l=${pair%%=*}; t=${pair#*=}; \
         [ \"$(readlink \"$root/etc/$l\")\" = \"$t\" ] || { echo \"root tree: /etc/$l must be a symlink to $t - it is a reviewed IMMUTABLE_ETC entry, so it must name the store path its readers resolve through\" >&2; exit 1; }; \
         [ -e \"{root}/real-root$t\" ] || { echo \"root tree: /etc/$l points at $t, which is not packed under real-root - unlike a MUTABLE_ETC hole, an IMMUTABLE_ETC target is image content and a dangle here is a fault the running system cannot repair\" >&2; exit 1; }; \
         if grep -q -F \"$l  \" \"$root/etc/mutable-state\"; then echo \"root tree: /etc/mutable-state documents /etc/$l as per-machine state, but it is an IMMUTABLE_ETC entry pointing into the read-only store - one of the two tables is wrong\" >&2; exit 1; fi; \
     done; \
     ( cd \"$root/etc\" || exit 1; \
       for p in @ETC_GLOBS@; do \
           { [ -d \"$p\" ] && [ ! -L \"$p\" ]; } || continue; \
           case $p in .|..|*/.|*/..) continue;; esac; \
           seen=0; for d in @ETC_DIRS@; do if [ \"$d\" = \"$p\" ]; then seen=1; fi; done; \
           [ \"$seen\" = 1 ] || { echo \"root tree: /etc/$p is a directory neither etc table declares, so the symlink sweep below cannot look inside it - add the entry that needs it (or the sweep stops being a proof)\" >&2; exit 1; }; \
       done; \
       m=0; i=0; \
       for p in @ETC_GLOBS@; do \
           [ -L \"$p\" ] || continue; \
           case $p in .|..) continue;; esac; \
           seen=0; for a in @MUTABLE_ETC_NAMES@; do if [ \"$a\" = \"$p\" ]; then seen=1; fi; done; \
           if [ \"$seen\" = 1 ]; then m=$((m+1)); continue; fi; \
           for a in @IMMUTABLE_ETC_NAMES@; do if [ \"$a\" = \"$p\" ]; then seen=2; fi; done; \
           [ \"$seen\" = 2 ] || { echo \"root tree: /etc/$p is a symlink out of the immutable /etc but is in NEITHER the MUTABLE_ETC nor the IMMUTABLE_ETC table - the read-only-/etc invariant is only as strong as the list of holes in it\" >&2; exit 1; }; \
           i=$((i+1)); \
       done; \
       [ \"$m\" = @MUTABLE_ETC_COUNT@ ] || { echo \"root tree: found $m mutable symlinks under /etc but MUTABLE_ETC declares @MUTABLE_ETC_COUNT@ - the counts must agree or a hole is unaccounted for in either direction\" >&2; exit 1; }; \
       [ \"$i\" = @IMMUTABLE_ETC_COUNT@ ] || { echo \"root tree: found $i store-pointing symlinks under /etc but IMMUTABLE_ETC declares @IMMUTABLE_ETC_COUNT@ - the counts must agree or a link into the store is unaccounted for in either direction\" >&2; exit 1; }; \
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
     tsh=\"{root}/real-root{in:td-sh}/bin/td-sh\"; tshtgt=\"{in:td-sh}/bin/td-sh\"; { [ -f \"$tsh\" ] && [ -x \"$tsh\" ]; } || { echo 'root tree: the td-sh binary is not packed/executable at real-root{in:td-sh}/bin/td-sh - /bin/sh would dangle and NOTHING on the boot path would run, since every /init and /etc script is interpreted by it' >&2; exit 1; }; \
     for a in @TD_SH_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tshtgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged shell ($tshtgt) - the td-sh /bin farm regressed\" >&2; exit 1; }; \
     done; \
     [ \"$(\"$tsh\" -c 'echo TD-SH-OK')\" = TD-SH-OK ] || { echo 'td-sh: the packed shell did not run the simplest possible command' >&2; exit 1; }; \
     [ \"$(\"$tsh\" -c 'x=1; for i in a b c; do x=\"$x$i\"; done; echo \"$x\"')\" = 1abc ] || { echo 'td-sh: the packed shell miscomputed a loop with assignment and expansion - the shape every generated script is built out of' >&2; exit 1; }; \
     [ \"$(\"$tsh\" -c 'if [ -d / ]; then echo yes; else echo no; fi')\" = yes ] || { echo 'td-sh: the packed shell got a test/if wrong' >&2; exit 1; }; \
     [ \"$(\"$tsh\" -c 'umask 077; umask')\" = 0077 ] || { echo 'td-sh: the packed shell cannot set and report a umask - the /init line that used to spell this `busybox sh -c` depends on it' >&2; exit 1; }; \
     [ \"$(\"$tsh\" -c 'echo \"${TD_UNSET:-fallback}\"')\" = fallback ] || { echo 'td-sh: the packed shell got parameter expansion with a default wrong' >&2; exit 1; }; \
     shst=0; \"$tsh\" -c 'exit 3' || shst=$?; [ \"$shst\" = 3 ] || { echo 'td-sh: the packed shell did not report an exit status - /etc/rootcheck decides the boot is healthy from these' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-login\" 2>/dev/null)\" = \"{in:td-login}/bin/td-login\" ] || { echo 'root tree: /bin/td-login is not a symlink to the staged credential multicall' >&2; exit 1; }; \
     tdl=\"{root}/real-root{in:td-login}/bin/td-login\"; tdltgt=\"{in:td-login}/bin/td-login\"; { [ -f \"$tdl\" ] && [ -x \"$tdl\" ]; } || { echo 'root tree: the td-login binary is not packed/executable at real-root{in:td-login}/bin/td-login - getty would exec a dangling /bin/login and no session could start' >&2; exit 1; }; \
     tdllist=$(\"$tdl\" --list 2>/dev/null) || { echo 'td-login --list failed - cannot verify the credential farm' >&2; exit 1; }; \
     for a in @TD_LOGIN_APPLETS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"$tdltgt\" ] || { echo \"root tree: /bin/$a is not a symlink to the staged td-login multicall ($tdltgt) - the credential /bin farm regressed\" >&2; exit 1; }; \
         printf '%s\\n' \"$tdllist\" | grep -q -x -F \"$a\" || { echo \"td-login does not serve applet '$a' - its packed /bin/$a symlink would dispatch to nothing (usage, exit 2)\" >&2; exit 1; }; \
     done; \
     [ -e \"$root/bin/verify-credentials\" ] && { echo 'root tree: verify-credentials is a readback PROBE, not an applet; a /bin symlink for it is a name no farm list accounts for' >&2; exit 1; }; \
     [ -e \"$root/bin/exec-as\" ] && { echo 'root tree: exec-as is a SUBCOMMAND, not an applet; a /bin/exec-as symlink would be a name no farm list in system-x86-64.rs accounts for, and one a reader could mistake for a general-purpose run-as-anyone tool beside su. It is not a privilege boundary - creds::may_switch is - so this refuses an unaccounted NAME, not a reachable capability' >&2; exit 1; }; \
     [ -e \"$root/bin/exec-service-as\" ] && { echo 'root tree: exec-service-as is a SUBCOMMAND, not an applet' >&2; exit 1; }; \
     \"$tdl\" exec-as 2>/dev/null && { echo 'td-login exec-as ACCEPTED an argv with no user and no program - its parser is what keeps a supervisor unit from starting something nobody named' >&2; exit 1; }; \
     \"$tdl\" exec-service-as 2>/dev/null && { echo 'td-login exec-service-as ACCEPTED an argv with no service account and no program' >&2; exit 1; }; \
     \"$tdl\" verify-credentials --uid 4294967294 --gid 4294967294 >/dev/null 2>&1 && { echo 'td-login verify-credentials ACCEPTED credentials this build process cannot have - the readback the TD-LOGIN-RUN-OK marker gates on proves nothing' >&2; exit 1; }; \
     set -- $(ls -l \"$tdl\"); case \"$1\" in *[sS]*) echo \"root tree: the packed td-login carries a setuid/setgid bit (mode $1). td-login is NEVER installed setuid-root (td-login/THREAT-MODEL.md section 4): with one, an unprivileged caller starts with euid 0 and 'su root' becomes root without authenticating\" >&2; exit 1;; esac; \
     tditab=$(\"$tdi\" init --dry-run -f \"$root/etc/inittab\" 2>&1) || { echo 'td-init init --dry-run REJECTED the inittab this image ships - PID 1 would come up having understood only part of its table. Its per-line diagnostics:' >&2; printf '%s\\n' \"$tditab\" >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-svc\" 2>/dev/null)\" = \"{in:td-svc}/bin/td-svc\" ] || { echo 'root tree: /bin/td-svc is not a symlink to the staged service supervisor - PID 1s only respawn line would exec nothing and the machine would have no userland at all' >&2; exit 1; }; \
     tds=\"{root}/real-root{in:td-svc}/bin/td-svc\"; { [ -f \"$tds\" ] && [ -x \"$tds\" ]; } || { echo 'root tree: the td-svc binary is not packed/executable at real-root{in:td-svc}/bin/td-svc - no identity, no network, no sshd and no console' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-profiler\" 2>/dev/null)\" = \"{in:td-profiler}/bin/td-profiler\" ] || { echo 'root tree: /bin/td-profiler is not a symlink to the staged collector' >&2; exit 1; }; \
     tdp=\"{root}/real-root{in:td-profiler}/bin/td-profiler\"; { [ -f \"$tdp\" ] && [ -x \"$tdp\" ]; } || { echo 'root tree: td-profiler is not packed and executable' >&2; exit 1; }; \
     pindex=\"$root@PROFILER_OBJECT_INDEX@\"; [ -s \"$pindex\" ] || { echo 'root tree: deployment profiler object index is absent or empty' >&2; exit 1; }; \
     [ \"$(head -n 1 \"$pindex\")\" = td-profiler-objects-v1 ] || { echo 'root tree: deployment profiler object index has the wrong header' >&2; exit 1; }; \
     codexrow=$(grep -F \"{in:codex}/bin/codex\" \"$pindex\") || { echo 'root tree: deployment profiler object index omits Codex' >&2; exit 1; }; \
     case \"$codexrow\" in *';assembly-boundary=1'*) : ;; *) echo 'root tree: deployment profiler object index omits the Codex assembly boundary' >&2; exit 1;; esac; \
     case \"$codexrow\" in *';line-attribution-boundary=1'*) : ;; *) echo 'root tree: deployment profiler object index omits the Codex line-attribution boundary' >&2; exit 1;; esac; \
     [ \"$(readlink \"$root/bin/td-jail\" 2>/dev/null)\" = \"{in:td-jail}/bin/td-jail\" ] || { echo 'root tree: /bin/td-jail is not a symlink to the staged confinement boundary' >&2; exit 1; }; \
     tdj=\"{root}/real-root{in:td-jail}/bin/td-jail\"; { [ -f \"$tdj\" ] && [ -x \"$tdj\" ]; } || { echo 'root tree: td-jail is not packed and executable, so the running-kernel transition oracle cannot run' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-seatd\" 2>/dev/null)\" = \"{in:td-seatd}/bin/td-seatd\" ] || { echo 'root tree: /bin/td-seatd is not a symlink to the staged single-user seat assigner' >&2; exit 1; }; \
     seat=\"{root}/real-root{in:td-seatd}/bin/td-seatd\"; { [ -f \"$seat\" ] && [ -x \"$seat\" ]; } || { echo 'root tree: td-seatd is not packed and executable' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-audio\" 2>/dev/null)\" = \"{in:td-audio}/bin/td-audio\" ] || { echo 'root tree: /bin/td-audio is not a symlink to the staged audio daemon' >&2; exit 1; }; \
     audio=\"{root}/real-root{in:td-audio}/bin/td-audio\"; { [ -f \"$audio\" ] && [ -x \"$audio\" ]; } || { echo 'root tree: td-audio is not packed and executable' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-compositor\" 2>/dev/null)\" = \"{in:td-compositor}/bin/td-compositor\" ] || { echo 'root tree: /bin/td-compositor is not a symlink to the staged software Wayland compositor' >&2; exit 1; }; \
     compositor=\"{root}/real-root{in:td-compositor}/bin/td-compositor\"; { [ -f \"$compositor\" ] && [ -x \"$compositor\" ]; } || { echo 'root tree: td-compositor is not packed and executable' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-term\" 2>/dev/null)\" = \"{in:td-compositor}/bin/td-term\" ] || { echo 'root tree: /bin/td-term is not a symlink to the staged terminal' >&2; exit 1; }; \
     tdterm=\"{root}/real-root{in:td-compositor}/bin/td-term\"; { [ -f \"$tdterm\" ] && [ -x \"$tdterm\" ]; } || { echo 'root tree: td-term is not packed/executable at real-root{in:td-compositor}/bin/td-term - the /bin/td-term symlink would dangle' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-busd\" 2>/dev/null)\" = \"{in:td-busd}/bin/td-busd\" ] || { echo 'root tree: /bin/td-busd is not a symlink to the staged session bus broker - the busd unit names it in full, so this is the only thing standing between that unit and exec-ing nothing' >&2; exit 1; }; \
     tdbusd=\"{root}/real-root{in:td-busd}/bin/td-busd\"; { [ -f \"$tdbusd\" ] && [ -x \"$tdbusd\" ]; } || { echo 'root tree: td-busd is not packed/executable at real-root{in:td-busd}/bin/td-busd - the /bin/td-busd symlink would dangle' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-portal\" 2>/dev/null)\" = \"{in:td-portal}/bin/td-portal\" ] || { echo 'root tree: /bin/td-portal is not a symlink to the staged Settings portal' >&2; exit 1; }; \
     tdportal=\"{root}/real-root{in:td-portal}/bin/td-portal\"; { [ -f \"$tdportal\" ] && [ -x \"$tdportal\" ]; } || { echo 'root tree: td-portal is not packed/executable at real-root{in:td-portal}/bin/td-portal - the portal supervisor, child, and live probe would all fail' >&2; exit 1; }; \
     for a in @APPLICATIONS@; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = /bin/td-jail ] || { echo \"root tree: /bin/$a is not an application launcher pointing to /bin/td-jail - another packed /bin provider replaced it\" >&2; exit 1; }; \
     done; \
     [ -f \"{root}/real-root{in:td-compositor}/share/terminfo/t/td-term\" ] || { echo 'root tree: the td-term terminfo entry is not packed, so /etc/terminfo resolves to a tree without it' >&2; exit 1; }; \
     tdsplan=$(\"$tds\" check -f \"$root@TD_SVC_CONF@\" 2>&1) || { echo 'td-svc check REJECTED the unit table this image ships - the boot would run a table the supervisor only partly understood. Its diagnostics:' >&2; printf '%s\\n' \"$tdsplan\" >&2; exit 1; }; \
     for u in @TD_SVC_UNITS@; do \
         printf '%s\\n' \"$tdsplan\" | grep -q -E \"^[0-9]+\\. $u\\$\" || { echo \"td-svc check resolved a start order without '$u' - a unit the inittab used to run is missing from the plan\" >&2; exit 1; }; \
     done; \
     : 'Order as the SHIPPED binary resolves the SHIPPED table. This cannot see a'; \
     : 'DELETED after= edge - ties break in declaration order, so dropping one leaves'; \
     : 'the plan identical. the_declared_edges_are_exactly_these pins the edge set on'; \
     : 'the host; this pins that td-svc itself still resolves them this way.'; \
     svcpos() { printf '%s\\n' \"$tdsplan\" | grep -n -E \"^[0-9]+\\. $1\\$\" | cut -d: -f1; }; \
     hn=$(svcpos hostname); fb=$(svcpos td-firstboot); rc=$(svcpos rootcheck); pf=$(svcpos profiler); pe=$(svcpos profiler-evidence); st=$(svcpos seat); au=$(svcpos audio); nu=$(svcpos netup); wl=$(svcpos wayland); ts=$(svcpos firefox-tls-setup); pc=$(svcpos portal-channel-evidence); tm=$(svcpos terminal); ff=$(svcpos firefox); fe=$(svcpos firefox-evidence); bs=$(svcpos bootsuccess); sd=$(svcpos sshd); gr=$(svcpos greeter); bd=$(svcpos busd); po=$(svcpos portal); pv=$(svcpos portal-evidence); \
     [ \"$hn\" -lt \"$fb\" ] || { echo 'td-svc would not serialize hostname before td-firstboot - init ran every sysinit line to completion before the next, and td-svc starts settled units in the same pass' >&2; exit 1; }; \
     [ \"$fb\" -lt \"$rc\" ] || { echo 'td-svc would start rootcheck before td-firstboot - rootcheck asserts the identity td-firstboot mints is readable' >&2; exit 1; }; \
     [ \"$rc\" -lt \"$pf\" ] && [ \"$pf\" -lt \"$pe\" ] || { echo 'td-svc would not serialize rootcheck -> profiler -> profiler evidence' >&2; exit 1; }; \
     [ \"$rc\" -lt \"$nu\" ] || { echo 'td-svc would start netup before rootcheck - networking must follow the read-only-root self-check' >&2; exit 1; }; \
     [ \"$nu\" -lt \"$sd\" ] || { echo 'td-svc would start sshd before netup - sshd binds loopback, which netup brings up' >&2; exit 1; }; \
     [ \"$fb\" -lt \"$sd\" ] || { echo 'td-svc would start sshd before td-firstboot - sshd is fail-closed on the host key td-firstboot mints, so it would refuse to start on every boot' >&2; exit 1; }; \
     [ \"$nu\" -lt \"$gr\" ] || { echo 'td-svc would start the greeter before netup' >&2; exit 1; }; \
     [ \"$rc\" -lt \"$st\" ] && [ \"$st\" -lt \"$wl\" ] && [ \"$wl\" -lt \"$pc\" ] && [ \"$ts\" -lt \"$pc\" ] && [ \"$wl\" -lt \"$tm\" ] && [ \"$wl\" -lt \"$ff\" ] && [ \"$ff\" -lt \"$fe\" ] && [ \"$tm\" -lt \"$bs\" ] && [ \"$pe\" -lt \"$bs\" ] || { echo 'td-svc would not serialize rootcheck -> seat -> wayland plus TLS setup -> private portal-channel evidence, wayland -> terminal + Firefox evidence, and profiler evidence -> independent bootsuccess' >&2; exit 1; }; \
     [ \"$st\" -lt \"$au\" ] && [ \"$au\" -lt \"$ff\" ] || { echo 'td-svc would not serialize seat -> audio -> Firefox' >&2; exit 1; }; \
     [ \"$st\" -lt \"$bd\" ] && [ \"$bd\" -lt \"$bs\" ] || { echo 'td-svc would not serialize seat -> busd -> bootsuccess - the broker binds inside the runtime directory td-seatd makes, and /etc/bootsuccess probes the RUNNING broker rather than a selftest' >&2; exit 1; }; \
     [ \"$bd\" -lt \"$po\" ] && [ \"$po\" -lt \"$pv\" ] && [ \"$po\" -lt \"$ff\" ] || { echo 'td-svc would not serialize busd -> portal -> live portal evidence and Firefox' >&2; exit 1; }; \
     mkdir -p '{root}/pivot-probe' && cp \"$tdi\" '{root}/pivot-probe/init' || { echo 'root tree: could not build the switch_root probe NEWROOT' >&2; exit 1; }; \
     tdipiv=$(\"$tdi\" switch_root '{root}/pivot-probe' /init 2>&1) && { echo 'td-init switch_root ACCEPTED a NEWROOT that is not a mount point - the last refusal standing between a bad pivot and a panicked kernel is gone' >&2; exit 1; }; \
     case \"$tdipiv\" in *'not a mount point'*) : ;; *) echo \"td-init switch_root refused a non-mount NEWROOT for the WRONG reason, so the mount-point guard is untested: $tdipiv\" >&2; exit 1;; esac; \
     [ \"$(readlink \"$root/home\")\" = var/home ] || { echo 'root tree: /home must point to var/home' >&2; exit 1; }; \
     [ \"$(readlink \"$root/root\")\" = var/root ] || { echo 'root tree: /root must point to var/root' >&2; exit 1; }; \
     if [ -e \"$root/bin/busybox\" ] || [ -L \"$root/bin/busybox\" ]; then echo 'root tree: /bin/busybox is packed - the multicall left this image with its last applet, and a symlink is how it would come back' >&2; exit 1; fi; \
     if [ -e \"{root}/real-root{in:busybox-x86-64}\" ] || [ -L \"{root}/real-root{in:busybox-x86-64}\" ]; then echo 'root tree: the busybox package is staged under /td/store - it is a BUILD tool for this recipe and must reach no image' >&2; exit 1; fi; \
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
     git=\"{root}/real-root{in:git-x86-64}/bin/git\"; gittgt=\"{in:git-x86-64}/bin/git\"; \
     { [ -f \"$git\" ] && [ -x \"$git\" ]; } || { echo 'root tree: Git is not packed/executable at real-root{in:git-x86-64}/bin/git - /bin/git would dangle and StageRuntimeClosure did not stage it' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/git\" 2>/dev/null)\" = \"$gittgt\" ] || { echo 'root tree: /bin/git is not a symlink to staged Git' >&2; exit 1; }; \
     for a in git-receive-pack git-upload-archive git-upload-pack; do \
         githelper=\"{root}/real-root{in:git-x86-64}/bin/$a\"; \
         { [ -f \"$githelper\" ] && [ -x \"$githelper\" ]; } || { echo \"root tree: $a is not packed/executable at real-root{in:git-x86-64}/bin/$a - /bin/$a would dangle\" >&2; exit 1; }; \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"{in:git-x86-64}/bin/$a\" ] || { echo \"root tree: /bin/$a is not a symlink to staged Git\" >&2; exit 1; }; \
     done; \
     codex=\"{root}/real-root{in:codex}/bin/codex\"; codextgt=\"{in:codex}/bin/codex\"; \
     { [ -f \"$codex\" ] && [ -x \"$codex\" ]; } || { echo 'root tree: Codex is not packed/executable at real-root{in:codex}/bin/codex - /bin/codex would dangle and StageRuntimeClosure did not stage it' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/codex\" 2>/dev/null)\" = \"$codextgt\" ] || { echo 'root tree: /bin/codex is not a symlink to staged Codex' >&2; exit 1; }; \
     [ -f \"{root}/real-root{in:codex}/lib/debug/bin/codex.debug\" ] || { echo 'root tree: the staged Codex package lacks its debug companion' >&2; exit 1; }; \
     [ -f \"{root}/real-root{in:codex}/lib/debug/.td-assembly-exception\" ] || { echo 'root tree: the staged Codex package lacks its assembly-boundary marker' >&2; exit 1; }; \
     codexline=\"{root}/real-root{in:codex}/lib/debug/.td-line-attribution-exception\"; \
     [ -f \"$codexline\" ] && grep -q -x -F 'output=codex' \"$codexline\" && grep -q -x -F 'runtime=bin/codex' \"$codexline\" || { echo 'root tree: the staged Codex package lacks its bound line-attribution marker' >&2; exit 1; }; \
     bwrap=\"{root}/real-root{in:codex-bwrap}/bin/bwrap\"; bwraptgt=\"{in:codex-bwrap}/bin/bwrap\"; \
     { [ -f \"$bwrap\" ] && [ -x \"$bwrap\" ]; } || { echo 'root tree: Codex Bubblewrap is not packed/executable at real-root{in:codex-bwrap}/bin/bwrap - /bin/bwrap would dangle' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/bwrap\" 2>/dev/null)\" = \"$bwraptgt\" ] || { echo 'root tree: /bin/bwrap is not a symlink to the source-built Codex helper' >&2; exit 1; }; \
     [ -f \"{root}/real-root{in:codex-bwrap}/lib/debug/bin/bwrap.debug\" ] || { echo 'root tree: the staged Codex Bubblewrap package lacks its debug companion' >&2; exit 1; }; \
     [ -s \"{root}/real-root{in:ca-certificates}/share/ca-certificates/ca-bundle.crt\" ] || { echo 'root tree: the pinned CA bundle is missing or empty' >&2; exit 1; }; \
     [ \"$(readlink \"$root/etc/ssl/certs/ca-certificates.crt\" 2>/dev/null)\" = \"{in:ca-certificates}/share/ca-certificates/ca-bundle.crt\" ] || { echo 'root tree: Git curl CA path does not resolve to the pinned bundle' >&2; exit 1; }; \
     openssl=\"{root}/real-root{in:libressl-x86-64}/bin/openssl\"; openssltgt=\"{in:libressl-x86-64}/bin/openssl\"; \
     { [ -f \"$openssl\" ] && [ -x \"$openssl\" ]; } || { echo 'root tree: the source-built LibreSSL command is not packed or executable' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/openssl\" 2>/dev/null)\" = \"$openssltgt\" ] || { echo 'root tree: /bin/openssl is not a symlink to the source-built LibreSSL command' >&2; exit 1; }; \
     [ -f \"{root}/real-root{in:libressl-x86-64}/lib/debug/bin/openssl.debug\" ] || { echo 'root tree: the source-built LibreSSL command lacks its debug companion' >&2; exit 1; }; \
     for dev in include lib/libcrypto.a lib/libssl.a; do [ ! -e \"{root}/real-root{in:libressl-x86-64}/$dev\" ] || { echo \"root tree: LibreSSL development path $dev entered the image\" >&2; exit 1; }; done; \
     for rel in bin/ssh bin/ssh-keygen bin/sshd libexec/sshd-session libexec/sshd-auth; do \
         openssh=\"{root}/real-root{in:openssh-x86-64}/$rel\"; \
         { [ -f \"$openssh\" ] && [ -x \"$openssh\" ]; } || { echo \"root tree: OpenSSH $rel is not packed/executable under real-root{in:openssh-x86-64}\" >&2; exit 1; }; \
     done; \
     for a in ssh ssh-keygen sshd; do \
         [ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = \"{in:openssh-x86-64}/bin/$a\" ] || { echo \"root tree: /bin/$a is not a symlink to staged OpenSSH\" >&2; exit 1; }; \
     done; \
     tdf=\"{root}/real-root{in:td-firstboot}/bin/td-firstboot\"; tdftgt=\"{in:td-firstboot}/bin/td-firstboot\"; \
     { [ -f \"$tdf\" ] && [ -x \"$tdf\" ]; } || { echo 'root tree: the td-firstboot binary is not packed/executable at real-root{in:td-firstboot}/bin/td-firstboot - the sysinit job would fail and the machine would have no identity, so OpenSSH would refuse to start' >&2; exit 1; }; \
     [ \"$(readlink \"$root/bin/td-firstboot\" 2>/dev/null)\" = \"$tdftgt\" ] || { echo 'root tree: /bin/td-firstboot is not a symlink to the staged identity provisioner' >&2; exit 1; }; \
     \"$tdf\" --help >/dev/null 2>&1 || { echo 'the packed td-firstboot does not run (it is static with an empty closure, so it must)' >&2; exit 1; }; \
     \"$tdf\" --nonesuch >/dev/null 2>&1; [ $? -eq 2 ] || { echo 'td-firstboot must exit 2 on an unknown argument (usage error) rather than provisioning something unasked' >&2; exit 1; }; \
     dsz=$(wc -c < \"$disk\"); \
     [ \"$dsz\" -ge 4096 ] || { echo \"root.erofs: implausibly small ($dsz bytes)\" >&2; exit 1; }; \
     set -- $(od -An -tx1 -j 1024 -N 4 \"$disk\"); \
     [ \"$1$2$3$4\" = e2e1f5e0 ] || { echo 'root.erofs: missing EROFS superblock magic at byte 1024' >&2; exit 1; }; \
     [ \"$(wc -l < \"$manifest\")\" -eq 4 ] || { echo 'manifest: expected header plus exactly three boot payload entries' >&2; exit 1; }; \
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
        // The dropped-name sweep tests -e AND -L because a repacked /bin entry pointing at a
        // target the build tree does not hold is DANGLING, which -e alone reads as absent.
        .replace("@DROPPED_APPLETS@", &DROPPED_APPLETS.join(" "))
        .replace("@UUTILS_APPLETS@", &UUTILS_APPLETS.join(" "))
        .replace("@TD_UTIL_APPLETS@", &TD_UTIL_APPLETS.join(" "))
        .replace("@TD_TXT_APPLETS@", &TD_TXT_APPLETS.join(" "))
        .replace("@TD_SH_APPLETS@", &TD_SH_APPLETS.join(" "))
        .replace("@TD_INIT_APPLETS@", &td_init_applets().join(" "))
        .replace("@TD_LOGIN_APPLETS@", &TD_LOGIN_APPLETS.join(" "))
        .replace("@APPLICATIONS@", &application_names(&SYSTEM).join(" "))
        .replace("@TD_SVC_UNITS@", &TD_SVC_UNITS.join(" "))
        .replace("@TD_SVC_CONF@", TD_SVC_CONF)
        .replace("@PROFILER_OBJECT_INDEX@", PROFILER_OBJECT_INDEX)
        .replace("@TD_SVC_CONF_NAME@", td_svc_conf_etc_name())
        .replace(
            "@TD_PORTAL_SETTINGS_NAME@",
            td_portal_settings_etc_name(),
        )
        .replace(
            "@APPLICATION_CONFIG_NAME@",
            application_etc_name(APPLICATION_CONFIG),
        )
        .replace(
            "@APPLICATION_REGISTRY_NAME@",
            application_etc_name(APPLICATION_REGISTRY),
        )
        .replace(
            "@APPLICATION_LAUNCHER_NAME@",
            application_etc_name(APPLICATION_LAUNCHER_TABLE),
        )
        .replace("@APPLICATION_COUNT@", &SYSTEM.applications.len().to_string())

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
        .replace(
            "@IMMUTABLE_ETC@",
            &IMMUTABLE_ETC
                .iter()
                .map(|entry| format!("{}={}", entry.etc, entry.target))
                .collect::<Vec<_>>()
                .join(" "),
        )
        .replace(
            "@IMMUTABLE_ETC_NAMES@",
            &IMMUTABLE_ETC
                .iter()
                .map(|entry| entry.etc)
                .collect::<Vec<_>>()
                .join(" "),
        )
        .replace("@IMMUTABLE_ETC_COUNT@", &IMMUTABLE_ETC.len().to_string())
        // Each reason rides into the generated script as a `:` no-op, the form
        // this file already uses for in-script commentary. A hole in the
        // read-only /etc should say why it is there where it is checked.
        .replace(
            "@IMMUTABLE_ETC_WHY@",
            &IMMUTABLE_ETC
                .iter()
                .map(|entry| format!(": '/etc/{}: {}'", entry.etc, entry.why))
                .collect::<Vec<_>>()
                .join("; "),
        )
        // One glob per directory the table uses, relative to /etc — a sweep for
        // symlinks the table does not name. Globs rather than a recursive walk
        // because the ladder guard bans the host directory-walk tools by name, and
        // because the table is what decides which directories can hold one.
        .replace("@ETC_GLOBS@", &etc_globs().join(" "))
        .replace("@ETC_DIRS@", &etc_dirs().join(" "))
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
    for dir in etc_dirs() {
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

    // 1) Stage the real-root tree in build scratch. WriteFile exposes a fixed
    //    mode, so set the shadow-file mode explicitly before packing.
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
    steps.push(Step::assert_debug_size(
        "{root}/real-root",
        "{out}/deployment/debug-size",
        "deployment",
        td_engine::target_profile::DEPLOYMENT_DEBUG_CEILING_BYTES,
    ));
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
            "{out}/deployment/debug-size".into(),
            "{out}/boot/selector-initramfs.cpio".into(),
            "{out}/boot/manifest".into(),
        ],
        exec: false,
    });
    steps.push(
        Step::run("{out}", &[POST_BOOTSTRAP_SH, "-c", &shape_check()])
            .env("PATH", &post_bootstrap_path()),
    );

    let recipe = Recipe::mesboot("system-x86-64", "0.2")
        // busybox: a BUILD TOOL ONLY since `getty` moved to td-init — this recipe's own
        // steps run under `busybox sh` (POST_BOOTSTRAP_SH) and shape_check parses both
        // cpio archives with it. Nothing it provides is packed; `nothing_on_the_image_is_
        // busybox` and shape_check's own staged-tree leg are what hold that apart.
        // linux-x86-64: the EXPORTED gen_init_cpio packer (verified STATICALLY linked).
        // uutils: the dynamically-linked `coreutils` multicall packed as the /bin file/text
        //   userland (#547).
        // ripgrep/fd: dynamically linked Rust search tools exposed as /bin/rg and /bin/fd.
        // Git: the source-built local and HTTP(S) client plus its executable helpers.
        // Codex: the source-built dynamic CLI plus its source-built static Bubblewrap helper.
        // ca-certificates: immutable Mozilla trust data at curl's conventional path.
        // OpenSSH: the source-built client, key generator, daemon, and mandatory split
        //   helpers. Its deliberately libcrypto-free closure is reached by
        //   StageRuntimeClosure.
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
        // td-jail: the static application boundary; its target-kernel transition probe
        //   and the Firefox launch both run on every boot.
        // td-seatd/td-compositor: the static single-user UI substrate and terminal.
        // td-audio: the static ALSA/Pulse daemon for the service-only audio account.
        // td-busd: the static session D-Bus broker used by every Firefox launch.
        // td-portal: the static Settings service, activation supervisor, and
        //   unprivileged live client probe.
        .native_inputs(&[
            "busybox-x86-64",
            "linux-x86-64",
            "uutils",
            "ripgrep",
            "fd",
            "git-x86-64",
            "codex",
            "codex-bwrap",
            "ca-certificates",
            "libressl-x86-64",
            "glibc-x86-64",
            "openssh-x86-64",
            "td-netd",
            "td-boot",
            "td-kexec",
            "td-util",
            "td-txt",
            "td-sh",
            "td-init",
            "td-firstboot",
            "td-login",
            "td-svc",
            "td-profiler",
            "td-jail",
            "td-seatd",
            "td-audio",
            "td-compositor",
            "td-busd",
            "td-portal",
        ])
        .steps(steps);
    let application_inputs = application_payload_inputs(&SYSTEM);
    if application_inputs.is_empty() {
        recipe
    } else {
        recipe.payload_inputs(&application_inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    /// Every unit whose leader is td-login declares `cgroup=session`, and no
    /// other unit does.
    ///
    /// td-login joins the uid-1000 session leaf before it drops privilege, so
    /// such a unit's processes leave any leaf td-svc made for it. Declaring it
    /// is what makes td-svc refuse a limit there instead of writing one into a
    /// cgroup the service does not occupy. The realistic regression is not a
    /// wrong declaration but a MISSING one on a unit added later, so this is
    /// derived from the `exec=` line rather than compared against a list — a
    /// list would have to be updated by the same person who forgot.
    #[test]
    fn a_unit_that_hands_its_process_to_td_login_says_so() {
        let handoff_leaders = [
            format!("/bin/td-login exec-as {UI_USER} "),
            format!("/bin/su -s /bin/sh {UI_USER} "),
        ];
        let mut session = Vec::new();
        let mut service = Vec::new();
        for (name, keys) in parse_td_svc_conf() {
            let exec = keys
                .iter()
                .find(|(key, _)| key == "exec")
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            let declared = keys.iter().any(|(k, v)| k == "cgroup" && v == "session");
            // Two ways a unit's LEADER becomes td-login: it is the leader, or a
            // shell leader `exec`s it and is replaced. The second is what the
            // first version of this test missed — `firefox` reaches td-login
            // through `sh -c '… exec …'`, so a prefix match called it a service
            // and would have let a limit be written into a leaf its processes
            // had already left. Without `exec` the shell STAYS, which is why
            // `firefox-evidence` and `firefox-input` — which run td-login in
            // loops and command substitutions — are not handoffs.
            let hands_off = handoff_leaders
                .iter()
                .any(|p| exec.starts_with(p.as_str()) || exec.contains(&format!("exec {p}")));
            if hands_off {
                session.push(name.clone());
                assert!(
                    declared,
                    "{name} execs td-login as {UI_USER}, so its processes are moved \
                     into the session leaf — it must declare cgroup=session, or a \
                     limit on it would be written where they are not"
                );
            } else {
                service.push(name.clone());
                assert!(
                    !declared,
                    "{name} declares cgroup=session but its leader is not td-login \
                     as {UI_USER}, so it does own a leaf and can be bounded"
                );
            }
        }
        // Both sides are non-empty, so neither arm can be vacuous: a parse that
        // silently returned nothing would otherwise pass this test.
        assert!(!session.is_empty(), "no session units parsed");
        assert!(!service.is_empty(), "no service units parsed");
    }

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

    #[test]
    fn openssh_privilege_separation_identity_is_locked_and_prepared() {
        assert!(
            SYSTEM.users.iter().all(|user| {
                user.name != SSHD_PRIVSEP_USER
                    && user.uid != SSHD_PRIVSEP_UID
                    && user.gid != SSHD_PRIVSEP_GID
            }),
            "the fixed OpenSSH account name, uid, and gid must not collide with an \
             interactive SystemDef user"
        );
        assert!(build_passwd(&SYSTEM).contains(&format!(
            "{SSHD_PRIVSEP_USER}:x:{SSHD_PRIVSEP_UID}:{SSHD_PRIVSEP_GID}:OpenSSH privilege separation:{SSHD_PRIVSEP_PATH}:/bin/false\n"
        )));
        assert!(build_group(&SYSTEM).contains(&format!(
            "{SSHD_PRIVSEP_USER}:x:{SSHD_PRIVSEP_GID}:\n"
        )));
        assert!(build_shadow(&SYSTEM).contains(&format!("{SSHD_PRIVSEP_USER}:!:")));

        let rootcheck = build_rootcheck(&SYSTEM);
        for required in [
            format!("/bin/rm -rf {SSHD_PRIVSEP_PATH}"),
            format!("/bin/mkdir {SSHD_PRIVSEP_PATH} || ok=0"),
            format!("/bin/chown 0:0 {SSHD_PRIVSEP_PATH} || ok=0"),
            format!("/bin/chmod 0755 {SSHD_PRIVSEP_PATH} || ok=0"),
        ] {
            assert!(
                rootcheck.lines().any(|line| line == required),
                "rootcheck does not prepare the OpenSSH privsep chroot with {required:?}"
            );
        }
        assert!(
            ordered_before("rootcheck", "sshd"),
            "OpenSSH may not start before its empty root-owned privsep chroot exists"
        );
        assert!(
            ordered_before("sshd", "bootsuccess"),
            "deployment health must wait for the running OpenSSH listener"
        );
    }

    #[test]
    fn audio_identity_is_locked_and_seat_prepares_its_runtime() {
        let account = SYSTEM
            .users
            .iter()
            .find(|user| user.name == AUDIO_USER)
            .unwrap_or_else(|| unreachable!("no audio account"));
        assert_eq!(
            (
                account.uid,
                account.gid,
                account.passwordless,
                account.service_only,
            ),
            (AUDIO_UID, AUDIO_GID, false, true)
        );
        assert!(build_passwd(&SYSTEM).contains(&format!(
            "{AUDIO_USER}:x:{AUDIO_UID}:{AUDIO_GID}:System Audio:{AUDIO_RUNTIME}:/bin/false\n"
        )));
        assert!(build_group(&SYSTEM).contains(&format!(
            "{AUDIO_USER}:x:{AUDIO_GID}:\n"
        )));
        assert!(
            build_shadow(&SYSTEM).contains(&format!("{AUDIO_USER}:!td-service:"))
        );
        let init = build_deployment_init(&SYSTEM);
        let rootcheck = build_rootcheck(&SYSTEM);
        assert!(!init.contains("/sysroot/var/run/td-audio"));
        assert!(!rootcheck.contains(AUDIO_RUNTIME));
    }

    #[test]
    fn audio_daemon_is_packed_supervised_and_ready_before_firefox() {
        let steps = recipe().steps.unwrap_or_default();
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyTree { from, dest }
                if from == "{in:td-audio}" && dest == "{root}/real-root{in:td-audio}"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Symlink { target, link }
                if target == "{in:td-audio}/bin/td-audio"
                    && link == "{root}/real-root/bin/td-audio"
        )));
        let native_inputs = recipe().native_inputs.unwrap_or_default();
        assert!(native_inputs.iter().any(|input| input == "td-audio"));
        assert_eq!(
            unit_key("audio", "exec"),
            Some(format!(
                "/bin/td-seatd exec-audio --uid {UI_UID} --gid {UI_GID} \
                 --audio-uid {AUDIO_UID} --audio-gid {AUDIO_GID} -- \
                 /bin/td-login exec-service-as {AUDIO_USER} -- /bin/td-audio \
                 serve --socket {}",
                td_engine::permissions::TD_AUDIO_SOCKET_PATH
            ))
        );
        assert_eq!(
            unit_key("audio", "ready"),
            Some(format!(
                "/bin/td-login exec-service-as {AUDIO_USER} -- /bin/td-audio \
                 probe --socket {}",
                td_engine::permissions::TD_AUDIO_SOCKET_PATH
            ))
        );
        assert_eq!(unit_key("audio", "requires").as_deref(), Some("seat"));
        assert_eq!(unit_key("audio", "restart").as_deref(), Some("always"));
        assert!(ordered_before("audio", "firefox"));
    }

    #[test]
    fn profiler_is_static_indexed_persistent_and_privilege_separated() {
        let account = SYSTEM
            .users
            .iter()
            .find(|user| user.name == PROFILER_USER)
            .unwrap_or_else(|| unreachable!("no profiler account"));
        assert_eq!(
            (account.uid, account.gid, account.passwordless),
            (PROFILER_UID, PROFILER_GID, false)
        );
        assert!(
            build_group(&SYSTEM).contains(&format!(
                "profiler-read:x:{PROFILER_READ_GID}:\n"
            )),
            "the reader group must exist without enrolling an interactive account"
        );
        assert!(build_shadow(&SYSTEM).contains("profiler:!:"));

        let init = build_deployment_init(&SYSTEM);
        for required in [
            "/sysroot/var/lib/td-profiler/captures",
            "chown 997:996 /sysroot/var/lib/td-profiler/captures",
            "chmod 2750 /sysroot/var/lib/td-profiler/captures",
            "printf '%s\\n' 2 > /proc/sys/kernel/perf_event_paranoid",
            "cat /proc/sys/kernel/perf_event_paranoid)\" = 2",
        ] {
            assert!(init.contains(required), "missing persistent profiler setup: {required}");
        }

        assert_eq!(
            unit_key("profiler", "after").as_deref(),
            Some("rootcheck")
        );
        assert_eq!(
            unit_key("profiler", "requires").as_deref(),
            Some("rootcheck")
        );
        let collector = unit_key("profiler", "exec").unwrap_or_default();
        for required in [
            "/bin/td-profiler collect",
            "--uid 997",
            "--gid 996",
            "--duration-secs 60",
            "--deployment {out}",
            "--profiler-build {in:td-profiler}",
        ] {
            assert!(collector.contains(required), "collector unit omitted {required}");
        }
        assert_eq!(unit_key("profiler", "restart").as_deref(), Some("on-failure"));
        assert_eq!(
            unit_key("profiler-evidence", "after").as_deref(),
            Some("profiler")
        );
        assert!(
            unit_key("profiler-evidence", "exec")
                .unwrap_or_default()
                .contains(PROFILER_CAPTURE_ROOT)
        );
        let evidence = unit_key("profiler-evidence", "exec").unwrap_or_default();
        for required in [
            "--timeout-secs 300",
            "--uid 997",
            "--gid 996",
            "--attribution-cmdline-token td.autotest=1",
        ] {
            assert!(evidence.contains(required), "evidence unit omitted {required}");
        }
        assert_eq!(
            unit_key("profiler-evidence", "log").as_deref(),
            Some("/var/log/svc/td-profiler-evidence.log")
        );
        assert_eq!(
            PROFILER_EVIDENCE_SERVICE_TIMEOUT_SECS,
            PROFILER_EVIDENCE_TIMEOUT_SECS + 15,
            "td-svc must not terminate the evidence process before its bounded wait ends"
        );
        assert!(
            unit_after("bootsuccess").contains(&"profiler-evidence".to_string()),
            "the process-heavy health probes must wait for the initial profiler capture"
        );
        assert_eq!(
            unit_key("bootsuccess", "requires").as_deref(),
            Some("terminal"),
            "profiler evidence is an ordering boundary, not deployment health"
        );

        let steps = real_root_steps(&SYSTEM);
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::WriteFile { path, content, exec: false }
                if path == "{root}/real-root/etc/td-profiler-application-roots.tsv"
                    && content == "td-profiler-application-roots-v1\n\
firefox\tfirefox-154.0\tforeign\tfreedesktop-platform-25-08-25.08\tforeign\n"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyTree { from, dest }
                if from == "{in:td-profiler}"
                    && dest == "{root}/real-root{in:td-profiler}"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Run { argv, .. }
                if argv == &[
                    "{in:td-profiler}/bin/td-profiler",
                    "index",
                    "{root}/real-root",
                    "{root}/real-root/etc/td-profiler-objects.tsv",
                    "--exclude-registry",
                    "{root}/real-root/etc/td-applications.tsv",
                    "{root}/real-root/etc/td-profiler-application-roots.tsv",
                ]
        )));
        assert!(
            recipe()
                .native_inputs
                .as_deref()
                .unwrap_or_default()
                .contains(&"td-profiler".to_string())
        );
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
                "::sysinit:/bin/mount -t cgroup2 -o nosuid,nodev,noexec cgroup2 /sys/fs/cgroup",
                "::sysinit:/bin/devpts",
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
            "/bin/td-term run",
            "/etc/bootsuccess",
            "/etc/bootfail",
            "/bin/sshd -D -e -f /etc/ssh/sshd_config",
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

    #[test]
    fn generated_service_comments_begin_at_column_zero() {
        let services = build_td_svc_conf();
        for line in services
            .lines()
            .filter(|line| line.trim_start().starts_with('#'))
        {
            assert!(
                line.starts_with('#'),
                "generated service comment carries indentation: {line:?}"
            );
        }
    }

    /// The terminal is deployment health; the packaged application is separate evidence.
    ///
    /// The two live in different files — the unit table here, the oracle in
    /// `td-recipe-eval` — and nothing but this ties them. Point the unit back
    /// at the demo and the oracle waits for a line nothing prints: a boot that
    /// runs to its timeout with no cause on the console.
    #[test]
    fn the_boot_starts_the_terminal_and_the_oracle_waits_for_its_marker() {
        let exec = unit_key("terminal", "exec").unwrap_or_default();
        assert!(exec.contains("/bin/td-term run "), "{exec}");
        let ready = unit_key("terminal", "ready").unwrap_or_default();
        assert!(ready.contains("/bin/td-term probe "), "{ready}");
        // The SAME socket in both, which containing the right program does not
        // give: a probe dialling a path the client never publishes burns the
        // 30s ready-timeout and then settles as FAILED with td-term still
        // running — `restart=` is evaluated on EXIT, so `restart=always` does
        // not retry a unit that failed its probe — and `bootsuccess` requires
        // this unit, so the boot never reaches its success gate. An earlier
        // version of this comment said "restarts forever", which is what a
        // daemon that DIES does.
        let published = exec.split("--ready-socket ").nth(1).unwrap_or_default();
        let published = published.split(['\'', ' ']).next().unwrap_or_default();
        let dialled = ready.split("probe ").nth(1).unwrap_or_default();
        let dialled = dialled.split(['\'', ' ']).next().unwrap_or_default();
        assert!(!published.is_empty(), "the unit publishes no ready socket");
        assert_eq!(
            published, dialled,
            "the terminal publishes {published} but its probe dials {dialled}"
        );
        assert_eq!(unit_key("terminal", "requires").as_deref(), Some("wayland"));
        // bootsuccess turns on it, so a boot that reaches no terminal is not a
        // success — which is what makes the oracle's wait a proof and not a
        // hopeful grep.
        assert_eq!(
            unit_key("bootsuccess", "requires").as_deref(),
            Some("terminal")
        );
        assert!(unit_after("bootsuccess").contains(&"terminal".to_string()));
        assert!(!unit_after("bootsuccess").contains(&"firefox".to_string()));
        // Which marker the ORACLE selects cannot be seen from this crate's
        // lib — it is a `const` in the `td-recipe-eval` bin — so the pin for
        // that lives beside it, in `qemu_boot.rs`'s own tests.
        assert_eq!(
            unit_key("firefox-tls-setup", "exec").as_deref(),
            Some("/etc/firefox-tls-setup")
        );
        assert_eq!(
            unit_key("firefox-tls-setup", "requires").as_deref(),
            Some("seat")
        );
        assert_eq!(
            unit_key("firefox-tls-setup", "timeout").as_deref(),
            Some("120")
        );
        assert_eq!(
            unit_key("firefox-tls-origin", "exec").as_deref(),
            Some("/bin/td-login exec-as tester -- /etc/firefox-tls-origin")
        );
        assert_eq!(
            unit_key("firefox-tls-origin", "requires").as_deref(),
            Some("firefox-tls-setup")
        );
        assert_eq!(
            unit_key("firefox-tls-origin", "ready").as_deref(),
            Some("/etc/firefox-tls-ready")
        );
        let tls_setup = build_firefox_tls_setup();
        for required in [
            "/bin/td-netd loopback",
            "openssl=/bin/openssl",
            "OPENSSL_CONF=\"$root/openssl.cnf\"",
            "export OPENSSL_CONF",
            "distinguished_name=dn",
            "-config \"$OPENSSL_CONF\"",
            "req -new -x509 -newkey rsa:2048 -nodes -sha256 -days 1",
            "basicConstraints=critical,CA:TRUE",
            "x509 -req",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
            FIREFOX_TLS_POLICY.trim_end(),
            FIREFOX_HTTPS_DOCUMENT,
            FIREFOX_DOWNLOAD_FIXTURE,
            "\"$origin/download.txt\"",
            "/bin/td-util chmod 0444 \"$root/ca.pem\" \"$root/policies.json\"",
            "/bin/chown 1000:1000 \"$root/server.key\"",
            "/bin/td-util chmod 0555 \"$origin\"",
        ] {
            assert!(tls_setup.contains(required), "TLS setup omitted {required:?}");
        }
        assert_eq!(tls_setup.matches("-sha256").count(), 2);
        let tls_origin = build_firefox_tls_origin();
        assert!(tls_origin.contains("OPENSSL_CONF=/dev/null"));
        assert!(tls_origin.contains(
            "openssl s_server -accept 8443 \
             -cert /run/td-firefox-autotest/server.pem \
             -key /run/td-firefox-autotest/server.key -WWW -quiet"
        ));
        assert!(!tls_origin.contains("exec /bin/openssl s_server"));
        assert!(tls_origin.ends_with("exit 1\n"));
        let tls_ready = build_firefox_tls_ready();
        for required in [
            "OPENSSL_CONF=/dev/null",
            "GET /content.html HTTP/1.0\\r\\nHost: localhost",
            "openssl s_client",
            "-connect 127.0.0.1:8443",
            "-servername localhost",
            "-CAfile /run/td-firefox-autotest/ca.pem",
            "-verify 5",
            "-verify_return_error",
            "TD-FIREFOX-HTTPS-CONTENT-V1",
        ] {
            assert!(tls_ready.contains(required), "TLS ready omitted {required:?}");
        }
        for forbidden in [
            "acceptInsecureCerts",
            "security.tls.insecure_fallback_hosts",
            "--ignore-certificate-errors",
        ] {
            assert!(!tls_setup.contains(forbidden));
            assert!(!tls_origin.contains(forbidden));
            assert!(!tls_ready.contains(forbidden));
        }
        let jail_authority = include_str!("../../../td-jail/src/authority.rs");
        assert!(jail_authority.contains("FIREFOX_AUTOTEST_POLICY"));
        assert!(jail_authority.contains(
            r#""{\"policies\":{\"Certificates\":{\"Install\":["#
        ));
        assert!(jail_authority.contains(
            "\\\"/etc/firefox/policies/td-firefox-autotest-ca.pem\\\""
        ));
        assert!(jail_authority.contains(r#""]}}}\n","#));
        let firefox_probe = include_str!("../../../td-jail/src/firefox.rs");
        let deadline = format!(
            "const PROBE_DEADLINE: Duration = Duration::from_secs({});",
            FIREFOX_SUPPORT_TIMEOUT_SECS
        );
        assert_eq!(firefox_probe.matches(&deadline).count(), 1);
        assert!(firefox_probe.contains(&format!(
            "const DOWNLOAD_PROBE_DEADLINE: Duration = Duration::from_secs({});",
            FIREFOX_DOWNLOAD_TIMEOUT_SECS
        )));
        let firefox_autotest =
            unit_key("firefox-autotest", "exec").unwrap_or_default();
        assert!(firefox_autotest.starts_with(&format!(
            "/bin/td-login exec-as tester -- /bin/sh -c 'case \" $(/bin/cat \
             /proc/cmdline) \" in *\" {AUTOTEST_CMDLINE_TOKEN} \"*) \
             root={FIREFOX_AUTOTEST_HOST_ROOT}; "
        )));
        for required in [
            "/bin/mkdir -p \"$root/profile\"",
            "/bin/td-util chmod 0700 \"$root\" \"$root/profile\"",
            "user_pref(\\\"browser.preonboarding.enabled\\\", false);",
            "user_pref(\\\"termsofuse.bypassNotification\\\", true);",
            "user_pref(\\\"browser.download.useDownloadDir\\\", true);",
            "user_pref(\\\"browser.download.folderList\\\", 2);",
            "user_pref(\\\"browser.download.dir\\\", \\\"/home/td/Downloads\\\");",
            "/bin/td-util chmod 0600 \"$root/.user.js.tmp\"",
            "/bin/mv \"$root/.user.js.tmp\" \"$root/profile/user.js\"",
        ] {
            assert!(
                firefox_autotest.contains(required),
                "autotest preparation omitted {required:?}"
            );
        }
        assert!(firefox_autotest.ends_with(";; *) :;; esac'"));
        assert_eq!(
            unit_key("firefox-autotest", "type").as_deref(),
            Some("oneshot")
        );
        assert_eq!(
            unit_key("firefox-autotest", "requires").as_deref(),
            Some("seat")
        );
        assert_eq!(
            unit_key("firefox-autotest", "timeout").as_deref(),
            Some("30")
        );
        let firefox = unit_key("firefox", "exec").unwrap_or_default();
        assert_eq!(
            firefox,
            format!(
                "/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in \
                 *\" {AUTOTEST_CMDLINE_TOKEN} \"*) exec /bin/td-login exec-as tester -- \
                 /bin/{FIREFOX_NAME} --marionette --remote-allow-system-access \
                 --profile {FIREFOX_AUTOTEST_PROFILE} \
                 {FIREFOX_TLS_URL};; *) exec \
                 /bin/td-login exec-as tester -- /bin/{FIREFOX_NAME};; esac'")
        );
        assert!(FIREFOX_HTTPS_DOCUMENT.starts_with("<!doctype html>"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("body{display:grid"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("min-height:300vh"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("cursor:crosshair"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains(".a{background:#ff00ff}"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains(".b{background:#00ff00}"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("<div class=a></div>"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("width:100%;min-height:300vh"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("<input id=td-input"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains("<input id=td-upload type=file"));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains(
            "#td-upload{position:fixed;left:58%;top:104px;z-index:1}"
        ));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains(
            "#td-upload::file-selector-button{width:100%;height:100%}"
        ));
        assert!(FIREFOX_HTTPS_DOCUMENT.contains(
            "<a id=td-download href=download.txt download=td-firefox-download.txt>"
        ));
        assert!(!FIREFOX_HTTPS_DOCUMENT.bytes().any(|byte| {
            matches!(byte, b'\'' | b'"' | b'$' | b'\\' | b'\n') || byte == 0x60
        }));
        assert_eq!(
            FIREFOX_HTTPS_DOCUMENT
                .matches("TD-FIREFOX-HTTPS-CONTENT-V1")
                .count(),
            1
        );
        assert_eq!(
            unit_key("firefox", "requires").as_deref(),
            Some("wayland,firefox-autotest,firefox-tls-origin")
        );
        let firefox_ready = unit_key("firefox", "ready").unwrap_or_default();
        assert_eq!(
            firefox_ready,
            format!(
                "/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in \
                 *\" {AUTOTEST_CMDLINE_TOKEN} \"*) exec /bin/td-login exec-as tester -- \
                 /bin/td-compositor probe-application \
                 {FIREFOX_WINDOW_READY_SOCKET} {FIREFOX_APP_ID} \
                 {FIREFOX_CONTENT_RGB_A} {FIREFOX_CONTENT_RGB_B} --quiet;; \
                 *) exit 0;; esac'"
            )
        );
        let wayland = unit_key("wayland", "exec").unwrap_or_default();
        assert!(wayland.contains(&format!(
            "--application-ready-socket {FIREFOX_WINDOW_READY_SOCKET} \
             --application-app-id {FIREFOX_APP_ID} \
             --application-content-rgb-a {FIREFOX_CONTENT_RGB_A} \
             --application-content-rgb-b {FIREFOX_CONTENT_RGB_B}"
        )));
        assert_eq!(
            unit_key("firefox", "ready-timeout"),
            Some(FIREFOX_READY_TIMEOUT_SECS.to_string())
        );
        assert_eq!(
            FIREFOX_EVIDENCE_WAIT_ITERATIONS,
            FIREFOX_READY_TIMEOUT_SECS * FIREFOX_READY_ATTEMPTS
                + FIREFOX_RETRY_MARGIN_SECS
        );
        assert_eq!(
            FIREFOX_GREETER_WAIT_ITERATIONS,
            FIREFOX_READY_TIMEOUT_SECS
                + FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS
                + FIREFOX_INPUT_TIMEOUT_SECS
                    * FIREFOX_INPUT_ATTEMPTS
                    * FIREFOX_RETRIED_INPUT_STAGES
                + FIREFOX_DOWNLOAD_TIMEOUT_SECS
                + FIREFOX_FILE_CHOOSER_TIMEOUT_SECS * FIREFOX_FILE_CHOOSER_STAGES
                + FIREFOX_INPUT_POLL_SLEEP_SECS
        );
        assert_eq!(
            FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS,
            FIREFOX_EVIDENCE_WAIT_ITERATIONS
                + FIREFOX_SUPPORT_TIMEOUT_SECS * FIREFOX_SUPPORT_ATTEMPTS
                + FIREFOX_NETWORK_TIMEOUT_SECS
        );
        assert!(
            u64::from(FIREFOX_GREETER_WAIT_ITERATIONS)
                <= crate::ladder::DEFAULT_BOOT_TIMEOUT_SECS
                    .saturating_sub(crate::ladder::QEMU_GUEST_WAIT_MARGIN_SECS)
                    .saturating_sub(u64::from(BOOT_SUCCESS_ITERATION_BUDGET_SECS)),
            "the default guest wait must preserve the complete Firefox evidence window"
        );
        let evidence = unit_key("firefox-evidence", "exec").unwrap_or_default();
        assert!(evidence.starts_with(&format!(
            "/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" {AUTOTEST_CMDLINE_TOKEN} \"*) :;; *) exit 0;; esac; n=0; s=0; while [ \"$n\" -lt {FIREFOX_EVIDENCE_WAIT_ITERATIONS} ]; do if application=$(/bin/td-login exec-as tester -- /bin/td-compositor probe-application {FIREFOX_WINDOW_READY_SOCKET} {FIREFOX_APP_ID} {FIREFOX_CONTENT_RGB_A} {FIREFOX_CONTENT_RGB_B} 2>/dev/null) && content=$(/bin/td-login exec-as tester -- /bin/td-jail --probe-process-token {FIREFOX_NAME} -contentproc 2>/dev/null) && /bin/td-login exec-as tester -- /bin/td-jail --probe-resource-caps {FIREFOX_NAME}; then if support=$(/bin/td-login exec-as tester -- /bin/td-jail --probe-firefox-support); then "
        )));
        assert!(evidence.contains(&format!(
            "s=$((s+1)); [ \"$s\" -lt {FIREFOX_SUPPORT_ATTEMPTS} ] || exit 1"
        )));
        let network_probe = evidence
            .find(&format!(
                "*\" {NETTEST_CMDLINE_TOKEN} \"*) network=$(/bin/td-login exec-as \
                 tester -- /bin/td-jail --probe-firefox-network) || exit 1; \
                 [ \"$network\" = {FIREFOX_NETWORK_RUNTIME_MARKER} ] || exit 1"
            ))
            .expect("Firefox public-network probe gate missing");
        assert_eq!(
            evidence.matches("--probe-firefox-network").count(),
            1,
            "the evidence program must contain one public-navigation command"
        );
        assert!(evidence.contains(&format!(
            "/bin/mv {FIREFOX_COMPLETION_TMP_PATH} {FIREFOX_COMPLETION_PATH} \
             && exit 0; exit 1; fi; s=$((s+1))"
        )));
        let firefox_probe = include_str!("../../../td-jail/src/firefox.rs");
        assert!(firefox_probe.contains(
            "const NETWORK_PROBE_DEADLINE: Duration = Duration::from_secs(60);"
        ));
        assert!(firefox_probe.contains(&format!(
            "const FIREFOX_NETWORK_TEST_URL: &str = \
             \"{}\";",
            crate::ladder::FIREFOX_NETWORK_TEST_URL
        )));
        assert!(firefox_probe.contains(&format!(
            "\"{FIREFOX_NETWORK_RUNTIME_MARKER}\""
        )));
        assert!(evidence.contains(&format!(
            "/bin/rm -f {FIREFOX_EVIDENCE_TMP_PATH} {FIREFOX_COMPLETION_TMP_PATH}"
        )));
        let evidence_write = evidence
            .find(&format!(
                "{FIREFOX_EVIDENCE} > {FIREFOX_EVIDENCE_TMP_PATH}"
            ))
            .expect("Firefox evidence temporary write missing");
        let evidence_chmod = evidence
            .find(&format!(
                "/bin/td-util chmod 0644 {FIREFOX_EVIDENCE_TMP_PATH}"
            ))
            .expect("Firefox evidence chmod missing");
        let evidence_marker = evidence
            .find(&format!("/bin/echo {TD_FIREFOX_BOOT_MARKER}"))
            .expect("Firefox evidence marker missing");
        let content_marker = evidence
            .find(&format!("/bin/echo {TD_FIREFOX_CONTENT_MARKER}"))
            .expect("Firefox content marker missing");
        let application_report = evidence
            .find("/bin/td-util printf \"%s\\n\" \"$application\"")
            .expect("Firefox compositor high-water report missing");
        let content_report = evidence
            .find("/bin/td-util printf \"%s\\n\" \"$content\"")
            .expect("Firefox content-process report missing");
        let support_report = evidence
            .find("/bin/td-util printf \"%s\\n\" \"$support\"")
            .expect("Firefox support report missing");
        let network_report = evidence
            .find("/bin/td-util printf \"%s\\n\" \"$network\"")
            .expect("Firefox network report missing");
        let support_marker = evidence
            .find(&format!("/bin/echo {TD_FIREFOX_SUPPORT_MARKER}"))
            .expect("Firefox support marker missing");
        let evidence_publish = evidence
            .find(&format!(
                "/bin/mv {FIREFOX_EVIDENCE_TMP_PATH} {FIREFOX_EVIDENCE_PATH}"
            ))
            .expect("Firefox evidence publication missing");
        let completion_write = evidence
            .find(&format!(
                "{FIREFOX_COMPLETION} > {FIREFOX_COMPLETION_TMP_PATH}"
            ))
            .expect("Firefox completion temporary write missing");
        let completion_chmod = evidence
            .find(&format!(
                "/bin/td-util chmod 0644 {FIREFOX_COMPLETION_TMP_PATH}"
            ))
            .expect("Firefox completion chmod missing");
        let completion_publish = evidence
            .find(&format!(
                "/bin/mv {FIREFOX_COMPLETION_TMP_PATH} {FIREFOX_COMPLETION_PATH}"
            ))
            .expect("Firefox completion publication missing");
        assert!(
            evidence_write < evidence_chmod
                && network_probe < evidence_write
                && evidence_chmod < evidence_publish
                && evidence_publish < application_report
                && application_report < content_report
                && content_report < support_report
                && support_report < network_report
                && network_report < evidence_marker
                && evidence_marker < content_marker
                && content_marker < support_marker
                && support_marker < completion_write
                && completion_write < completion_chmod
                && completion_chmod < completion_publish,
            "evidence, marker and completion must have one exact order"
        );
        assert!(evidence.ends_with(&format!(
            "&& /bin/mv {FIREFOX_COMPLETION_TMP_PATH} {FIREFOX_COMPLETION_PATH} && exit 0; exit 1; fi; s=$((s+1)); [ \"$s\" -lt {FIREFOX_SUPPORT_ATTEMPTS} ] || exit 1; fi; n=$((n+1)); /bin/td-util sleep 1; done; exit 1'"
        )));
        assert_eq!(
            unit_key("firefox-evidence", "type").as_deref(),
            Some("daemon")
        );
        assert_eq!(
            unit_key("firefox-evidence", "restart").as_deref(),
            Some("never")
        );
        assert!(
            unit_after("firefox-evidence").contains(&"firefox".to_string())
        );
        assert!(
            unit_after("firefox-evidence").contains(&"netup".to_string())
        );
        assert!(unit_key("firefox-evidence", "requires").is_none());

        let input = unit_key("firefox-input", "exec").unwrap_or_default();
        assert!(input.starts_with(&format!(
            "/bin/sh -c 'case \" $(/bin/cat /proc/cmdline) \" in *\" \
             {FIREFOX_INPUT_CMDLINE_TOKEN} \"*) :;; *) exit 0;; esac; \
             /bin/rm -f {FIREFOX_DOWNLOAD_PATH} \
             {FIREFOX_DOWNLOAD_PART_PATH} || exit 1; n=0; while \
             [ \"$n\" -lt {FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS} ]; do \
             evidence=$(/bin/td-util cat {FIREFOX_COMPLETION_PATH} 2>/dev/null); \
             [ \"$evidence\" = {FIREFOX_COMPLETION} ] && break"
        )));
        assert!(input.contains(&format!(
            "[ \"$n\" -lt {FIREFOX_INPUT_EVIDENCE_WAIT_ITERATIONS} ] || exit 1; \
             n=0; while [ \"$n\" -lt {FIREFOX_INPUT_ATTEMPTS} ]; do \
             /bin/td-login exec-as tester -- /bin/td-jail \
             --probe-firefox-input arm && break"
        )));
        let stages = [
            "arm &&",
            "menu &&",
            "final &&",
            "clipboard-refocus-arm &&",
            "clipboard-refocus &&",
            "clipboard &&",
            "download ||",
            "file-chooser ||",
            "file-chooser-focus ||",
            "file-chooser-result ||",
        ];
        assert_eq!(
            stages.len(),
            usize::from(FIREFOX_RETRIED_INPUT_STAGES.saturating_add(4))
        );
        for stage in stages {
            assert_eq!(
                input.matches(&format!("--probe-firefox-input {stage}")).count(),
                1
            );
        }
        let arm = input.find("--probe-firefox-input arm &&").unwrap();
        let menu = input.find("--probe-firefox-input menu &&").unwrap();
        let final_stage = input.find("--probe-firefox-input final &&").unwrap();
        let clipboard_refocus_arm = input
            .find("--probe-firefox-input clipboard-refocus-arm &&")
            .unwrap();
        let clipboard_refocus = input
            .find("--probe-firefox-input clipboard-refocus &&")
            .unwrap();
        let clipboard = input.find("--probe-firefox-input clipboard &&").unwrap();
        let download = input.find("--probe-firefox-input download ||").unwrap();
        let file_probe = input.find("--probe-firefox-download").unwrap();
        let file_chooser = input
            .find("--probe-firefox-input file-chooser ||")
            .unwrap();
        let file_chooser_focus = input
            .find("--probe-firefox-input file-chooser-focus ||")
            .unwrap();
        let portal_completions = input
            .match_indices(PORTAL_FILE_CHOOSER_COMPLETED)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(portal_completions.len(), 2);
        let file_chooser_result = input
            .find("--probe-firefox-input file-chooser-result ||")
            .unwrap();
        let completion = input.find(FIREFOX_INPUT_COMPLETION).unwrap();
        assert!(
            arm < menu
                && menu < final_stage
                && final_stage < clipboard_refocus_arm
                && clipboard_refocus_arm < clipboard_refocus
                && clipboard_refocus < clipboard
                && clipboard < download
                && download < file_probe
                && file_probe < portal_completions[0]
                && portal_completions[0] < file_chooser
                && file_chooser < file_chooser_focus
                && file_chooser_focus < portal_completions[1]
                && portal_completions[1] < file_chooser_result
                && file_chooser_result < completion,
            "input evidence and completion must retain their exact stage order"
        );
        assert!(input.contains("/bin/td-util printf \"%s\\n\" \"$download\""));
        assert!(input.contains(&format!(
            "while [ \"$n\" -lt {FIREFOX_DOWNLOAD_OBSERVE_ATTEMPTS} ]; do if download="
        )));
        assert!(input.contains(&format!(
            "while [ \"$n\" -lt {FIREFOX_FILE_CHOOSER_TIMEOUT_SECS} ]; do portal_now=$(/bin/rg -c \
             \"^{PORTAL_FILE_CHOOSER_COMPLETED} .* response=0$\" {PORTAL_SERVICE_LOG}"
        )));
        assert!(input.contains("[ \"$portal_now\" -gt \"$portal_done\" ] && break"));
        assert!(input.contains(&format!(
            "{FIREFOX_INPUT_COMPLETION} > {FIREFOX_INPUT_COMPLETION_TMP_PATH}"
        )));
        assert!(input.contains(&format!(
            "/bin/mv {FIREFOX_INPUT_COMPLETION_TMP_PATH} {FIREFOX_INPUT_COMPLETION_PATH}"
        )));
        assert_eq!(unit_after("firefox-input"), vec!["firefox-evidence"]);
        assert_eq!(unit_key("firefox-input", "type").as_deref(), Some("daemon"));
        assert_eq!(unit_key("firefox-input", "restart").as_deref(), Some("never"));
        assert!(unit_key("firefox-input", "requires").is_none());

        assert!(
            !build_td_svc_conf().contains("/bin/td-ui-demo"),
            "the synthetic client must not remain on the system boot path"
        );
    }

    /// Every native client path the compositor is handed is a name the image STAGES.
    ///
    /// The compositor cannot resolve any of them: each is an absolute `/bin`
    /// name, and the launcher refuses a relative program rather than
    /// searching. A flag naming a binary this recipe does not symlink is a
    /// native launcher entry that spawns nothing. Firefox is activation-only
    /// and therefore has no client-path flag.
    ///
    /// What this test uniquely holds is the MAPPING. That the path is staged
    /// is already covered by `direct_bin_calls_resolve_to_a_packed_name`,
    /// which sweeps every `/bin/NAME` in the table; what nothing else sees is
    /// WHICH flag carries the terminal program.
    ///
    /// The client flags are enumerated out of the unit rather than only listed,
    /// so a renamed or vanished flag reds the count instead of quietly leaving
    /// the mapping unchecked.
    #[test]
    fn native_launcher_client_flags_name_binaries_the_image_stages() {
        let exec = unit_key("wayland", "exec").unwrap_or_default();
        let steps = real_root_steps(&SYSTEM);
        let words: Vec<&str> = exec.split_ascii_whitespace().collect();
        let flags: Vec<(&str, &str)> = words
            .iter()
            .enumerate()
            .filter(|(_, word)| word.starts_with("--") && word.ends_with("-client"))
            .map(|(index, word)| {
                // The unit's exec is a `su -c '…'` word, so the LAST flag's
                // value carries the closing quote — strip it as the shell does.
                let value = words
                    .get(index.saturating_add(1))
                    .copied()
                    .unwrap_or_default()
                    .trim_end_matches('\'');
                (*word, value)
            })
            .collect();
        // The enumeration is only a proof if it FOUND them, and the count is
        // what says so: a renamed flag would silently yield a shorter list.
        assert_eq!(flags.len(), 1, "expected one native --*-client flag");
        for (flag, path) in &flags {
            let program = path.strip_prefix("/bin/").unwrap_or_default();
            assert!(
                !program.is_empty() && !program.contains('/'),
                "{flag} passes {path}, which is not a /bin name"
            );
            let link = format!("{{root}}/real-root{path}");
            let expected_target = match *flag {
                "--terminal-client" => "{in:td-compositor}/bin/td-term",
                _ => "",
            };
            assert!(steps.iter().any(|step| matches!(
                step,
                Step::Symlink { link: at, target }
                    if at == &link && target == expected_target
            )), "{flag} passes {path}, but nothing stages it through {expected_target}");
        }
        assert_eq!(flags, vec![("--terminal-client", "/bin/td-term")]);
    }

    #[test]
    fn wayland_service_uses_activation_only_firefox_and_a_native_terminal() {
        let expected = format!(
            "/bin/su -s /bin/sh tester -c '/bin/td-compositor run \
             --framebuffer /dev/fb0 --input /dev/input \
             --socket /run/user/1000/wayland-0 \
             --portal-socket {PORTAL_WAYLAND_SOCKET} \
             --launcher-application {FIREFOX_NAME} \
             --terminal-client /bin/td-term \
             --application-ready-socket {FIREFOX_WINDOW_READY_SOCKET} \
             --application-app-id {FIREFOX_APP_ID} \
             --application-content-rgb-a {FIREFOX_CONTENT_RGB_A} \
             --application-content-rgb-b {FIREFOX_CONTENT_RGB_B}'"
        );
        assert_eq!(
            unit_key("wayland", "exec").as_deref(),
            Some(expected.as_str())
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
    /// The td-svc backstop is not the only clock over this loop: the HOST kills the
    /// VM at `DEFAULT_BOOT_TIMEOUT_SECS`. If it gives up first, an unhealthy boot
    /// stops being diagnosable — the guest never reaches the branch that prints WHICH
    /// probe failed, and the oracle reports a bare timeout instead. So the ceiling
    /// must outlast the profiler-evidence ordering boundary plus the guest's own
    /// patience, which is the clamped iteration count times what an iteration may
    /// cost.
    ///
    /// Review found this the other way round: the rollback pass raised the guest's
    /// per-iteration budget past the host's whole ceiling, which no test noticed
    /// because nothing related the two.
    #[test]
    fn the_host_ceiling_outlasts_the_guest_loop_it_waits_for() {
        let guest = u64::from(BOOT_SUCCESS_RETRY_MAX_SECS)
            .saturating_mul(u64::from(BOOT_SUCCESS_ITERATION_BUDGET_SECS));
        let serial = u64::from(svc_timeouts::HOSTNAME)
            .saturating_add(u64::from(svc_timeouts::FIRSTBOOT))
            .saturating_add(u64::from(svc_timeouts::ROOTCHECK));
        let predecessor = u64::from(PROFILER_EVIDENCE_SERVICE_TIMEOUT_SECS)
            .max(u64::from(svc_timeouts::NETUP));
        let required = serial
            .saturating_add(guest)
            .saturating_add(predecessor)
            .saturating_add(crate::ladder::QEMU_GUEST_WAIT_MARGIN_SECS);
        assert!(
            crate::ladder::DEFAULT_BOOT_TIMEOUT_SECS >= required,
            "the host gives up after {}s while the guest's boot-success loop may run \
             {}s ({BOOT_SUCCESS_RETRY_MAX_SECS} iterations x \
             {BOOT_SUCCESS_ITERATION_BUDGET_SECS}s), after {}s of serial identity/root \
             checks and the slower of the profiler evidence and network services' \
             {}s ceilings; the VM would be killed mid-loop or before its {}s diagnostic \
             margin elapsed, and the failure would be reported with no guest-side \
             reason in it",
            crate::ladder::DEFAULT_BOOT_TIMEOUT_SECS,
            guest,
            serial,
            predecessor,
            crate::ladder::QEMU_GUEST_WAIT_MARGIN_SECS
        );
    }

    #[test]
    fn each_timeout_stays_above_the_worst_case_its_comment_claims() {
        // A backstop must clear its worst case with room, not by a hair: these run on a
        // TCG-emulated VM with a cold page cache, and the numbers below are computed from
        // read timeouts and spawn counts, not measured. So the rule is 2x the worst case.
        // Verified red at NETUP=31 — which cleared the old raw 26s and still failed here.
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
                336,
                "DHCP and td-netd resolve/reach, then libcurl's 300s connect and 10s \
                 low-speed transfer bounds",
            ),
            (
                svc_timeouts::BOOTSUCCESS,
                // The loop is clamped to this many iterations; budget a slow one each.
                (BOOT_SUCCESS_RETRY_MAX_SECS as u32)
                    .saturating_mul(BOOT_SUCCESS_ITERATION_BUDGET_SECS),
                "clamped iterations of ten su probe blocks, four td-boot updates and \
                 a rollback",
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
            ("audio", vec!["seat"]),
            ("netup", vec!["rootcheck"]),
            ("busd", vec!["seat"]),
            ("portal", vec!["busd"]),
            ("portal-evidence", vec!["portal", "firefox-tls-setup"]),
            ("wayland", vec!["seat"]),
            (
                "portal-channel-evidence",
                vec!["wayland", "firefox-tls-setup"],
            ),
            ("terminal", vec!["wayland"]),
            ("firefox-tls-setup", vec!["seat"]),
            ("firefox-tls-origin", vec!["firefox-tls-setup"]),
            ("firefox-autotest", vec!["seat"]),
            (
                "firefox",
                vec![
                    "audio",
                    "busd",
                    "portal",
                    "wayland",
                    "firefox-autotest",
                    "firefox-tls-origin",
                ],
            ),
            ("firefox-evidence", vec!["firefox", "netup"]),
            ("firefox-input", vec!["firefox-evidence"]),
            (
                "bootsuccess",
                sysinit
                    .iter()
                    .copied()
                    .chain([
                        "busd",
                        "wayland",
                        "terminal",
                        "profiler-evidence",
                        "sshd",
                    ])
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

    /// The image ships exactly ONE application. This is a review tripwire on a
    /// second packaged policy surface, not a proof that the bus has one peer.
    ///
    /// td-jail binds `/run/user/1000/bus` into every jail because the broker is
    /// the policy boundary. Well-known names, match rules, per-caller filtering
    /// and per-instance admission have landed. Two shared surfaces remain: the
    /// global descriptor budget is not charged per instance, and
    /// `GetConnectionCredentials` reports init-namespace pids.
    ///
    /// Which is NOT what this counts, and the gap is stated here rather than
    /// left for a reader to assume away. A package count does not bound peers:
    /// `td-portal` speaks D-Bus and is not a `ShippedApplication`, and a
    /// direct `/bin/firefox` request can add another peer without changing the
    /// package count. The compositor card itself is activation-only.
    ///
    /// So this is a tripwire on the likeliest next step, put where adding the
    /// entry breaks the build rather than promised in prose a future change
    /// would not have to read. APPLICATIONS.md §D carries the rest of the
    /// sentence, including what the tripwire does not cover.
    #[test]
    fn the_image_stays_single_application_until_remaining_bus_sharing_gaps_land() {
        assert_eq!(
            SHIPPED_APPLICATIONS.len(),
            1,
            "a second application would share td-busd's global descriptor \
             budget and could observe init-namespace pids through \
             GetConnectionCredentials. See APPLICATIONS.md §D for these \
             remaining peer-attribution gaps and the limits of this package \
             count tripwire"
        );
    }

    /// The session bus is a unit, runs as the UI user, and binds where the seat
    /// assignment put the runtime directory.
    ///
    /// Every path is spelled out rather than derived at boot: `exec-as` empties the
    /// environment, so nothing here can come from `XDG_RUNTIME_DIR` or from
    /// whatever the boot path happened to export. The `ready=` line is `td-busd
    /// probe`, which completes the `EXTERNAL` handshake under the uid the kernel
    /// reports for it — so a broker that bound the path and cannot serve it never
    /// reaches ready, and td-svc marks the unit FAILED and says so on the console
    /// rather than leaving /etc/bootsuccess to probe a socket with nothing behind
    /// it.
    ///
    /// It does not RESTART it, and an earlier version of this comment said it
    /// did. A readiness probe that never succeeds changes the unit's phase and
    /// leaves the process running, while `restart=` is evaluated when a process
    /// EXITS. So `restart=always` below covers a broker that dies, which is the
    /// common case, and not one that is up and not serving; nothing re-probes a
    /// unit once it has failed that way. APPLICATIONS.md §D says the same thing,
    /// and this comment contradicting it is what made the error findable.
    #[test]
    fn the_session_bus_runs_unprivileged_where_the_seat_put_its_runtime_dir() {
        assert_eq!(
            unit_key("busd", "exec"),
            Some(format!(
                "/bin/td-login exec-as {UI_USER} -- /bin/td-busd run \
                 --socket /run/user/{UI_UID}/bus"
            )),
            "the broker must be started by literal argv as the UI user"
        );
        assert_eq!(
            unit_key("busd", "ready"),
            Some(format!(
                "/bin/td-login exec-as {UI_USER} -- /bin/td-busd probe \
                 /run/user/{UI_UID}/bus"
            )),
            "readiness must be a real client completing the handshake"
        );
        assert_eq!(unit_key("busd", "type").as_deref(), Some("daemon"));
        assert_eq!(unit_key("busd", "restart").as_deref(), Some("always"));
        assert_eq!(
            unit_key("busd", "requires").as_deref(),
            Some("seat"),
            "without the seat there is no /run/user/{UI_UID} to bind in, and `bind` \
             would silently make one"
        );
        assert!(
            ordered_before("busd", "bootsuccess"),
            "/etc/bootsuccess probes the RUNNING broker, so it must be ordered \
             after the unit that starts it"
        );
    }

    #[test]
    fn settings_portal_activation_and_live_probe_are_exact() {
        let probe = format!(
            "/bin/td-login exec-as {UI_USER} -- /bin/td-portal probe \
             --bus /run/user/{UI_UID}/bus --settings {TD_PORTAL_SETTINGS_PATH}"
        );
        assert_eq!(
            unit_key("portal", "exec"),
            Some(format!(
                "/bin/td-portal supervise --bus /run/user/{UI_UID}/bus \
                 --settings {TD_PORTAL_SETTINGS_PATH}"
            )),
            "root must retain the broker capability while supervising its direct child"
        );
        assert_eq!(unit_key("portal", "ready"), Some(probe.clone()));
        assert_eq!(unit_key("portal", "after").as_deref(), Some("busd"));
        assert_eq!(unit_key("portal", "requires").as_deref(), Some("busd"));
        assert_eq!(unit_key("portal", "restart").as_deref(), Some("always"));
        assert_eq!(unit_key("portal", "ready-timeout").as_deref(), Some("30"));
        assert_eq!(
            unit_key("portal", "log").as_deref(),
            Some(PORTAL_SERVICE_LOG)
        );
        assert_eq!(unit_key("portal", "console").as_deref(), Some("yes"));

        assert_eq!(unit_key("portal-evidence", "exec"), Some(probe));
        assert_eq!(
            unit_key("portal-evidence", "after").as_deref(),
            Some("portal,firefox-tls-setup")
        );
        assert_eq!(
            unit_key("portal-evidence", "requires").as_deref(),
            Some("portal")
        );
        assert_eq!(
            unit_key("portal-evidence", "type").as_deref(),
            Some("oneshot")
        );
        assert_eq!(
            unit_key("portal-evidence", "timeout").as_deref(),
            Some("30")
        );
        assert_eq!(
            unit_key("portal-evidence", "log").as_deref(),
            Some("/var/log/svc/td-portal-evidence.log")
        );
        assert_eq!(
            unit_key("portal-evidence", "console").as_deref(),
            Some("yes")
        );
        assert!(ordered_before("portal", "firefox"));
        assert!(
            !unit_key("firefox", "requires")
                .unwrap_or_default()
                .split(',')
                .any(|dependency| dependency == "portal"),
            "Settings availability is application evidence, not deployment health"
        );
    }

    #[test]
    fn private_portal_channel_evidence_is_exact_and_separate() {
        assert_eq!(
            unit_key("wayland", "exec"),
            Some(format!(
                "/bin/su -s /bin/sh {UI_USER} -c '/bin/td-compositor run \
                 --framebuffer /dev/fb0 --input /dev/input \
                 --socket /run/user/{UI_UID}/wayland-0 \
                 --portal-socket {PORTAL_WAYLAND_SOCKET} \
                 --launcher-application {FIREFOX_NAME} --terminal-client /bin/td-term \
                 --application-ready-socket {FIREFOX_WINDOW_READY_SOCKET} \
                 --application-app-id {FIREFOX_APP_ID} \
                 --application-content-rgb-a {FIREFOX_CONTENT_RGB_A} \
                 --application-content-rgb-b {FIREFOX_CONTENT_RGB_B}'"
            ))
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "exec"),
            Some(format!(
                "/bin/td-login exec-as {UI_USER} -- /bin/td-portal \
                 channel-probe --wayland {PORTAL_WAYLAND_SOCKET}"
            ))
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "after").as_deref(),
            Some("wayland,firefox-tls-setup")
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "requires").as_deref(),
            Some("wayland")
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "type").as_deref(),
            Some("oneshot")
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "timeout").as_deref(),
            Some("30")
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "log").as_deref(),
            Some("/var/log/svc/td-portal-channel-evidence.log")
        );
        assert_eq!(
            unit_key("portal-channel-evidence", "console").as_deref(),
            Some("yes")
        );
        assert!(
            !unit_after("bootsuccess")
                .contains(&"portal-channel-evidence".to_string()),
            "private portal evidence must not gain deployment-health authority"
        );
    }

    #[test]
    fn immutable_portal_settings_have_one_canonical_source() {
        assert_eq!(td_portal_settings_etc_name(), "td-portal-settings");
        let generated = etc_files(&SYSTEM)
            .into_iter()
            .find(|(name, _, _)| *name == td_portal_settings_etc_name());
        assert_eq!(
            generated,
            Some(("td-portal-settings", TD_PORTAL_SETTINGS.to_string(), false)),
            "the tested policy must be regular immutable /etc content"
        );
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
            unit_after("bootsuccess").contains(&"terminal".to_string()),
            "bootsuccess must wait for the first client frame"
        );
        assert_eq!(
            unit_key("bootsuccess", "requires").as_deref(),
            Some("terminal"),
            "deployment health must be skipped when the terminal failed, while the \
             mutable Firefox launch remains independent QEMU evidence"
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
            unit_key("terminal", "requires").as_deref(),
            Some("wayland"),
            "the terminal must not start without its compositor"
        );
        assert_eq!(
            unit_key("terminal", "restart").as_deref(),
            Some("always"),
            "the graphical client is supervised and restartable"
        );
        // ORDERED after the bus, and STRICT on the compositor plus the two
        // finite QEMU setup authorities, and the asymmetry is the point.
        // td-jail RESOLVES /run/user/1000/bus before
        // it unshares and fails the launch if it is not a socket owned by the
        // login user, so Firefox needs that socket to EXIST — `busd` and
        // `wayland` are siblings, each requiring only `seat`, so without an
        // ordering edge td-svc may release Firefox the moment the
        // compositor is ready, onto a bus that has not bound.
        //
        // Firefox DOES need a working broker: since §D's registration
        // landed in td-jail, stage 0 opens a connection and refuses the launch
        // if `Register` or `Complete` fails, because an application the broker
        // has no record of resolves `Unconfined`. The edge is still ORDERING
        // rather than `requires`, and the reason it is has not changed — only
        // the size of what it covers. A draft made the edge `requires=busd` on
        // the grounds that a FAILED broker settles for ordering too. The
        // grounds are right and the remedy was wrong. td-svc sets
        // `phase = Failed` with `retry_at` on a `restart=always` daemon that
        // is merely in its restart BACKOFF, `requires_failed` reads that phase
        // and cannot tell it from a permanent failure, and the dependent is
        // then set `Failed` with `retry_at = None`. So one busd crash inside
        // the boot window would permanently kill the application tier of a
        // machine whose broker recovers a second later.
        //
        // Ordering alone recovers instead. Firefox's own `restart=always`
        // retries the launch until the broker is answering, which is
        // self-healing where the strict edge was terminal — and it covers the
        // broker that is up but not yet serving as well as the one that never
        // bound, since a registration that fails is a failed launch and a
        // failed launch is a restart. The diagnostic that made the strict edge
        // tempting is no longer the argument for it either: `session_socket`
        // labels its errors, so Firefox dying on an absent bus says "session
        // bus /run/user/1000/bus" rather than a bare ENOENT, and one dying on
        // a wedged broker names the instance it could not register.
        assert_eq!(
            unit_key("firefox", "requires").as_deref(),
            Some("wayland,firefox-autotest,firefox-tls-origin"),
            "Firefox must not start without its compositor — and \
             its finite autotest preparation and verified origin — and must \
             not be made strictly dependent on a broker whose restart backoff \
             td-svc cannot tell from a permanent failure"
        );
        assert_eq!(
            unit_key("firefox", "restart").as_deref(),
            Some("always"),
            "Firefox is supervised and restartable"
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
        let expected_roots = vec![
            "{in:uutils}".to_string(),
            "{in:ripgrep}".to_string(),
            "{in:fd}".to_string(),
            "{in:openssh-x86-64}".to_string(),
            "{in:git-x86-64}".to_string(),
            "{in:codex}".to_string(),
            format!("{{payload:{FIREFOX_NAME}}}"),
            "{payload:freedesktop-platform-25-08}".to_string(),
        ];
        assert_eq!(
            roots.as_slice(),
            expected_roots.as_slice(),
            "the shipped programs and application packages are explicit runtime roots"
        );
        assert_eq!(dest.as_str(), "{root}/real-root");
        // Keep the closing `}` in the prefix: the separately reviewed
        // codex-bwrap static helper legitimately uses CopyTree.
        assert!(
            steps.iter().all(|step| !matches!(
                step,
                Step::CopyTree { from, .. }
                    if from.contains("uutils")
                        || from.contains("ripgrep")
                        || from.starts_with("{in:fd}")
                        || from.contains("git-x86-64")
                        || from.starts_with("{in:codex}")
                        || from.contains("glibc-x86-64")
            )),
            "runtime store items must not bypass StageRuntimeClosure"
        );
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(
                    step,
                    Step::CopyTree { from, dest }
                        if from == "{in:codex-bwrap}"
                            && dest == "{root}/real-root{in:codex-bwrap}"
                ))
                .count(),
            1,
            "the static Codex Bubblewrap helper must be copied once at its canonical path"
        );
        let libressl_copies: Vec<(&str, &str)> = steps
            .iter()
            .filter_map(|step| match step {
                Step::CopyTree { from, dest }
                    if from.starts_with("{in:libressl-x86-64}/") =>
                {
                    Some((from.as_str(), dest.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            libressl_copies,
            [
                (
                    "{in:libressl-x86-64}/bin",
                    "{root}/real-root{in:libressl-x86-64}/bin",
                ),
                (
                    "{in:libressl-x86-64}/lib/debug",
                    "{root}/real-root{in:libressl-x86-64}/lib/debug",
                ),
            ],
            "the image needs LibreSSL's command and debug companion, not its development tree"
        );
        for (name, target) in [
            ("rg", "{in:ripgrep}/bin/rg"),
            ("fd", "{in:fd}/bin/fd"),
            ("git", "{in:git-x86-64}/bin/git"),
            (
                "git-receive-pack",
                "{in:git-x86-64}/bin/git-receive-pack",
            ),
            (
                "git-upload-archive",
                "{in:git-x86-64}/bin/git-upload-archive",
            ),
            (
                "git-upload-pack",
                "{in:git-x86-64}/bin/git-upload-pack",
            ),
            ("codex", "{in:codex}/bin/codex"),
            ("bwrap", "{in:codex-bwrap}/bin/bwrap"),
            ("openssl", "{in:libressl-x86-64}/bin/openssl"),
        ] {
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
        for required in ["codex", "codex-bwrap", "libressl-x86-64"] {
            assert!(
                native_inputs.iter().any(|input| input == required),
                "shipped input {required} must be declared"
            );
        }
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
        let group_text = build_group(&SYSTEM);
        let groups: Vec<(&str, u32)> = group_text
            .lines()
            .map(|line| {
                let mut fields = line.split(':');
                let name = fields
                    .next()
                    .unwrap_or_else(|| unreachable!("group without a name"));
                let _password = fields
                    .next()
                    .unwrap_or_else(|| unreachable!("group without a password field"));
                let gid = fields
                    .next()
                    .unwrap_or_else(|| unreachable!("group without a gid"))
                    .parse::<u32>()
                    .unwrap_or_else(|_| unreachable!("group with a non-numeric gid"));
                (name, gid)
            })
            .collect();
        for (name, gid) in &groups {
            assert_eq!(
                groups.iter().filter(|(candidate, _)| candidate == name).count(),
                1,
                "group name '{name}' must be unique"
            );
            assert_eq!(
                groups
                    .iter()
                    .filter(|(_, candidate)| candidate == gid)
                    .count(),
                1,
                "group gid {gid} must be unique"
            );
        }
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
            assert_eq!(
                SYSTEM
                    .users
                    .iter()
                    .filter(|candidate| candidate.uid == user.uid)
                    .count(),
                1,
                "uid {} must belong to exactly one user",
                user.uid
            );
            assert_eq!(
                SYSTEM
                    .users
                    .iter()
                    .filter(|candidate| candidate.gid == user.gid)
                    .count(),
                1,
                "primary gid {} must belong to exactly one user",
                user.gid
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
                    user.name == UI_USER
                        && user.uid == UI_UID
                        && user.gid == UI_GID
                        && user.home == UI_HOME
                }),
            "the first UI profile is deliberately one fixed seat: build_td_svc_conf, \
             td-seatd, XDG_RUNTIME_DIR, and WAYLAND_DISPLAY all bind tester 1000:1000; \
             make those generated before changing the graphical account"
        );
        assert_eq!(
            FIREFOX_DOWNLOAD_SOURCE,
            format!("/var{UI_HOME}/Downloads"),
            "Firefox's Downloads bind must follow the fixed UI profile's real home"
        );
        assert_eq!(
            unit_key("seat", "exec"),
            Some(format!(
                "/bin/td-seatd assign --uid {UI_UID} --gid {UI_GID} \
                 --audio-uid {AUDIO_UID} --audio-gid {AUDIO_GID}"
            )),
            "the seat service must create both compiled runtime identities"
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
                !(u.passwordless && u.service_only),
                "user '{}' cannot be both passwordless and service-only",
                u.name
            );
            if u.service_only {
                assert_eq!(
                    u.shell, "/bin/false",
                    "service-only user '{}' must have the non-login shell",
                    u.name
                );
            }
            assert_ne!(
                u.uid, SSHD_PRIVSEP_UID,
                "user '{}' collides with the reserved OpenSSH privilege-separation uid {}",
                u.name, SSHD_PRIVSEP_UID
            );
            assert_ne!(
                u.gid, SSHD_PRIVSEP_GID,
                "user '{}' collides with the reserved OpenSSH privilege-separation gid {}",
                u.name, SSHD_PRIVSEP_GID
            );
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
                packed_applet.is_some_and(|a| {
                    TD_SH_APPLETS.contains(&a) || UUTILS_APPLETS.contains(&a)
                }),
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
        assert!(
            SYSTEM.applications.len() <= td_engine::launcher::MAX_APPLICATIONS,
            "the image selects {} applications; the launcher/registry limit is {}",
            SYSTEM.applications.len(),
            td_engine::launcher::MAX_APPLICATIONS
        );
        for application in SYSTEM.applications {
            assert!(
                td_engine::application::validate_application_identity(application.name)
                    .is_ok(),
                "shipped application name {:?} is not a valid launcher key",
                application.name
            );
            assert!(
                td_engine::application::validate_application_name(application.package).is_ok(),
                "shipped application package {:?} is not a plain catalog key",
                application.package
            );
            assert_eq!(
                SYSTEM
                    .applications
                    .iter()
                    .filter(|candidate| candidate.name == application.name)
                    .count(),
                1,
                "shipped application name {:?} must be unique",
                application.name
            );
            assert_eq!(
                SYSTEM
                    .applications
                    .iter()
                    .filter(|candidate| candidate.package == application.package)
                    .count(),
                1,
                "shipped application package {:?} must be unique",
                application.package
            );
            assert!(
                td_engine::application::validate_application_name(application.runtime).is_ok(),
                "shipped application runtime {:?} is not a plain recipe key",
                application.runtime
            );
            assert_ne!(
                application.package, application.runtime,
                "an application package and runtime must be distinct inputs"
            );
            let package = (application.package_recipe)();
            assert_eq!(
                package.name, application.package,
                "shipped application package provenance must come from its catalog recipe"
            );
            if application.name == FIREFOX_NAME {
                assert_eq!(
                    package
                        .application
                        .as_ref()
                        .and_then(|declaration| declaration.alias()),
                    Some(FIREFOX_APP_ID),
                    "the compositor observer must match Firefox's authenticated alias"
                );
            }
            let runtime = (application.runtime_recipe)();
            assert_eq!(
                runtime.name, application.runtime,
                "shipped application runtime provenance must come from its catalog recipe"
            );
        }
    }

    /// The `tty` group's gid is written into the mount options in td-init and
    /// into this check here, and neither knows about `build_group`. Deriving
    /// the expectation from the group table is what the repo already does for
    /// `supplementary_gids`, and for the same reason: a `tty` that moved would
    /// otherwise leave all three sites stale and green, serving slaves that
    /// name a group nobody has -- or, if gid 5 were reassigned, one that the
    /// mount's group-write bit then hands a terminal to.
    #[test]
    fn the_devpts_gid_is_the_image_s_own_tty_group() {
        let group = build_group(&SYSTEM);
        let tty = group
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(':');
                match (fields.next(), fields.next(), fields.next()) {
                    (Some("tty"), Some(_), Some(gid)) => Some(gid.to_string()),
                    _ => None,
                }
            })
            .next()
            .unwrap_or_else(|| unreachable!("the image has no tty group"));
        let applet = super::super::td_init::module_source("devpts")
            .unwrap_or_else(|| unreachable!("td-init serves no devpts module"));
        assert!(
            applet.contains(&format!("gid={tty}")),
            "td-init's devpts mount must ask for the image's own tty gid ({tty})"
        );
        assert!(
            build_rootcheck(&SYSTEM).contains(&format!("gid={tty}(")),
            "rootcheck must expect the image's own tty gid ({tty})"
        );
    }

    /// Every option here can be ignored by a mount that still succeeds, and a
    /// machine missing any of them boots and looks fine: without `ptmxmode`
    /// the instance ptmx is 0000 and no unprivileged process can open it at
    /// all, and without `mode`/`gid` the slaves miss the tty convention that
    /// lets anything write to a terminal it does not own.
    #[test]
    fn rootcheck_proves_the_pty_instance_sysinit_mounted() {
        let profile = build_rootcheck(&SYSTEM);
        // A devpts line as the kernel actually writes it. Every literal the
        // check greps for is matched against THIS rather than against itself,
        // which is what stops a check written in the mount's spelling —
        // `mode=0620`, which devpts renders `%03o` — from passing its own test
        // while failing on every real machine.
        let proc_mounts = "devpts /dev/pts devpts rw,nosuid,noexec,relatime,\
                           gid=5,mode=620,ptmxmode=666 0 0";
        for option in ["mode=620", "gid=5", "ptmxmode=666"] {
            assert!(
                profile.contains(&format!("([^ ]*,)?{option}(,[^ ]*)?( |$)")),
                "rootcheck must check devpts was mounted with {option}"
            );
            assert!(
                proc_mounts.contains(option),
                "{option} is not how the kernel spells it in /proc/mounts"
            );
        }
        assert!(
            profile.contains("^devpts /dev/pts devpts "),
            "the check must name the mount point, not merely find devpts anywhere"
        );
        assert!(
            profile.contains("readlink /dev/ptmx)\" = pts/ptmx"),
            "/dev/ptmx must be the RELATIVE symlink into the instance"
        );
        assert!(
            profile.contains("[ -c /dev/pts/ptmx ]"),
            "the instance's own ptmx is what the symlink resolves to"
        );
        // `newinstance` is deliberately absent: modern kernels accept the token
        // and echo nothing back, so requiring it would red a correct boot.
        assert!(
            !profile.contains("newinstance"),
            "rootcheck must not require /proc/mounts to echo newinstance"
        );
        // devpts prints its modes with `%03o`. Matching the `0620` the mount
        // ASKS for would never match what the kernel writes back, so rootcheck
        // would fail on a perfectly correct boot -- and rootcheck is a gate.
        assert!(
            !profile.contains("mode=0620"),
            "rootcheck must match the kernel's %03o spelling, not the mount's"
        );
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
    /// for `getty` alone now, td-login for the credential pair, td-sh for the shell and
    /// td-init for the rest since the cutovers: belt-and-braces against a farm edit that
    /// drops one or reroutes it to dynamically-linked uutils (the shape check catches it
    /// at build time, this catches it at test time).
    #[test]
    fn greeter_and_pivot_applets_are_present() {
        // Split across FOUR static multicalls now, so each name is pinned to the ONE that
        // serves it. Asserting only "some farm has it" would let a boot name drift between
        // binaries unnoticed — and for these names that drift IS the boot.
        assert!(
            td_init_applets().contains(&"getty"),
            "boot-critical applet 'getty' missing from the td-init farm - it is how the \
             machine reaches a login prompt, and it is td's own since the TCGETS/TCSETS \
             amendment"
        );
        // The shell is td-sh's since the flip, and is BOTH names busybox answered
        // to: a script spelling `ash` must reach the same binary `sh` does, or the
        // two are a difference nothing tests.
        for a in ["sh", "ash"] {
            assert!(
                TD_SH_APPLETS.contains(&a),
                "shell name '{a}' missing from the td-sh farm"
            );
        }
        for a in ["login", "su"] {
            assert!(
                TD_LOGIN_APPLETS.contains(&a),
                "credential applet '{a}' missing from the td-login farm"
            );
        }
        for a in ["init", "reboot", "switch_root", "hostname", "mount", "umount", "getty"] {
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
    /// Every /bin farm, name-tagged. ONE table: two tests consume it, and an eighth
    /// farm added to only one of them would leave the other silently narrower.
    fn bin_farms<'a>(
        td_init: &'a [&'static str],
        applications: &'a [&'static str],
    ) -> Vec<(&'static str, &'a [&'static str])> {
        vec![
            ("uutils", UUTILS_APPLETS),
            ("td-util", TD_UTIL_APPLETS),
            ("td-txt", TD_TXT_APPLETS),
            ("td-init", td_init),
            ("td-login", TD_LOGIN_APPLETS),
            ("td-sh", TD_SH_APPLETS),
            ("applications", applications),
        ]
    }

    #[test]
    fn application_layout_is_configured_and_composed_by_typed_steps() {
        assert_eq!(
            build_application_config(),
            "format=1\npackage-root=/td/store\nstate-root=.td/app\n\
             registry=/etc/td-applications.tsv\nlauncher-table=/etc/td-launcher.tsv\n\
             cgroup-root=/sys/fs/cgroup/td-user-1000\n"
        );
        for path in [
            APPLICATION_CONFIG,
            APPLICATION_REGISTRY,
            APPLICATION_LAUNCHER_TABLE,
        ] {
            assert!(
                path.starts_with("/etc/") && !application_etc_name(path).contains('/'),
                "application image config path must be one immutable /etc file: {path}"
            );
        }

        const APPLICATIONS: &[ShippedApplication] = &[ShippedApplication {
            name: "fixture",
            package: "fixture-package",
            package_recipe: super::super::td_jail_fixture::recipe,
            runtime: "fixture-runtime",
            runtime_recipe: super::super::empty_runtime::recipe,
        }];
        let fixture = SystemDef {
            hostname: SYSTEM.hostname,
            os_name: SYSTEM.os_name,
            os_version: SYSTEM.os_version,
            motd: SYSTEM.motd,
            autologin: SYSTEM.autologin,
            users: SYSTEM.users,
            applications: APPLICATIONS,
        };
        let steps = real_root_steps(&fixture);
        assert_eq!(
            application_payload_inputs(&fixture),
            vec!["fixture-package", "fixture-runtime"]
        );
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::StageRuntimeClosure { roots, dest }
                if dest == "{root}/real-root"
                    && roots.contains(&"{payload:fixture-package}".to_string())
                    && roots.contains(&"{payload:fixture-runtime}".to_string())
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CompileApplicationTables { names, packages, runtimes, registry, launcher }
                if names == &["fixture".to_string()]
                    && packages == &["{payload:fixture-package}".to_string()]
                    && runtimes == &["{payload:fixture-runtime}".to_string()]
                    && registry == "{root}/real-root/etc/td-applications.tsv"
                    && launcher == "{root}/real-root/etc/td-launcher.tsv"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Symlink { target, link }
                if target == "/bin/td-jail"
                    && link == "{root}/real-root/bin/fixture"
        )));

        assert_eq!(SYSTEM.applications.len(), 1);
        let shipped = SYSTEM
            .applications
            .first()
            .expect("one shipped application");
        assert_eq!(
            (shipped.name, shipped.package, shipped.runtime),
            (FIREFOX_NAME, FIREFOX_NAME, "freedesktop-platform-25-08"),
            "the system image must pair the reviewed Firefox package and runtime"
        );
        let system_recipe = recipe();
        assert_eq!(
            system_recipe.payload_inputs,
            Some(vec![
                FIREFOX_NAME.into(),
                "freedesktop-platform-25-08".into()
            ])
        );
    }

    #[test]
    fn applet_farms_are_disjoint_and_boot_names_stay_static() {
        // Check every pair: a name served twice emits two Symlink steps for one link and the
        // LAST one silently wins.
        let td_init = td_init_applets();
        let applications = application_names(&SYSTEM);
        let farms = bin_farms(&td_init, &applications);
        assert_eq!(farms.len(), 7, "the open application farm must stay registered");
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
        let collisions = application_name_collisions(&SYSTEM);
        assert!(
            collisions.is_empty(),
            "application names collide with existing direct or farmed /bin providers: {collisions:?}"
        );
        for a in [
            "hostname", "mount", "umount", "sh", "init", "switch_root", "login", "su",
        ] {
            assert!(
                TD_SH_APPLETS.contains(&a)
                    || td_init.contains(&a)
                    || TD_LOGIN_APPLETS.contains(&a),
                "boot-critical applet '{a}' must be served by a STATIC binary (td-sh, \
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
    /// drop a decision rather than an omission. The last leg pins shape_check's own scan of
    /// the STAGED tree, which is
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
        let applications = application_names(&SYSTEM);
        let farms = bin_farms(&td_init, &applications);
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
            "/bin/sh -c \"",
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

    /// NO generated script invokes the busybox multiplexer, and this is what keeps it
    /// that way. The four `/bin/busybox sh -c '…'` call sites that used to live here
    /// were spelled that way because busybox had `umask` and td-sh did not; once
    /// `umask(2)` entered td-sh's confined surface they became `/bin/sh -c`, and with
    /// them went the last reason any script reached the multiplexer.
    ///
    /// The assertion is the whole SUBSTRING, as `no_generated_script_invokes_awk` is
    /// and for the same reason: deciding "is this token in command position" needs a
    /// shell grammar, and every approximation of one leaves a spelling that reads as
    /// prose and escapes — a bare `busybox` found through PATH most of all. No
    /// generated script contains the word at all, so banning it outright costs nothing
    /// and cannot be evaded. A script that some day needs the multiplexer reds here and
    /// gets a deliberate decision — reinstating the packed binary, the `/bin` entry
    /// and a `shape_check` probe — rather than quietly reviving the dependency.
    #[test]
    fn no_generated_script_invokes_the_busybox_multiplexer() {
        const TOKEN: &str = "busybox";
        // Guard the guard: a shrunken source list would make the ban vacuous
        // while every assertion in it stayed green. By NAME rather than by
        // count, because a count is satisfied by the WRONG eighteen -- one
        // script dropped and one config file added leaves the total right and
        // the ban blind to whatever left. The two `/init`s matter most, since
        // they carried all four `/bin/busybox sh -c` call sites this replaced,
        // but every /etc script is a boot path too.
        let sources = script_sources();
        for required in [
            "/init (selector)",
            "/init (deployment)",
            "/etc/profile",
            "/etc/autologin",
            "/etc/tty-session",
            "/etc/shutdown",
            "/etc/rootcheck",
            "/etc/netup",
            "/etc/bootsuccess",
            "/etc/bootfail",
            "/etc/inittab",
            "/etc/td-svc.conf",
        ] {
            assert!(
                sources.iter().any(|(n, _, _)| n == required),
                "{required} is not among the scanned sources - the ban no longer \
                 covers it, and nothing else would say so"
            );
        }
        for (name, text, _) in &sources {
            assert!(
                !text.contains(TOKEN),
                "{name} names `{TOKEN}`. Nothing on this image runs the multiplexer: \
                 `sh` was the last applet any script reached it for, `getty` the last \
                 name it held, and the binary is not packed at all. Re-adding a call \
                 means packing it again and giving `shape_check` a probe for it"
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
    fn packed_bin_names_for(sys: &SystemDef) -> Vec<String> {
        const LINK_PREFIX: &str = "{root}/real-root/bin/";
        let mut names = Vec::new();
        for step in real_root_steps(sys) {
            if let Step::Symlink { link, .. } = step {
                if let Some(name) = link.strip_prefix(LINK_PREFIX) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    fn packed_bin_names() -> Vec<String> {
        packed_bin_names_for(&SYSTEM)
    }

    fn application_name_collisions(sys: &SystemDef) -> Vec<&'static str> {
        let without_applications = SystemDef {
            hostname: sys.hostname,
            os_name: sys.os_name,
            os_version: sys.os_version,
            motd: sys.motd,
            autologin: sys.autologin,
            users: sys.users,
            applications: &[],
        };
        let existing = packed_bin_names_for(&without_applications);
        sys.applications
            .iter()
            .filter_map(|application| {
                existing
                    .iter()
                    .any(|name| name == application.name)
                    .then_some(application.name)
            })
            .collect()
    }

    #[test]
    fn application_names_cannot_shadow_direct_bin_links() {
        const COLLIDING: &[ShippedApplication] = &[
            ShippedApplication {
                name: "rg",
                package: "catalog-rg",
                package_recipe: super::super::td_jail_fixture::recipe,
                runtime: "runtime-rg",
                runtime_recipe: super::super::empty_runtime::recipe,
            },
            ShippedApplication {
                name: "td-netd",
                package: "catalog-netd",
                package_recipe: super::super::td_jail_fixture::recipe,
                runtime: "runtime-netd",
                runtime_recipe: super::super::empty_runtime::recipe,
            },
        ];
        let fixture = SystemDef {
            hostname: SYSTEM.hostname,
            os_name: SYSTEM.os_name,
            os_version: SYSTEM.os_version,
            motd: SYSTEM.motd,
            autologin: SYSTEM.autologin,
            users: SYSTEM.users,
            applications: COLLIDING,
        };
        assert_eq!(application_name_collisions(&fixture), vec!["rg", "td-netd"]);
        let shape = shape_check();
        assert!(shape.contains(&format!(
            "for a in {}; do",
            application_names(&SYSTEM).join(" ")
        )));
        assert!(shape.contains(
            "[ \"$(readlink \"$root/bin/$a\" 2>/dev/null)\" = /bin/td-jail ]"
        ));
    }

    /// The mirror of the busybox-multiplexer ban, for the form this commit actually
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
        for a in UUTILS_APPLETS
            .iter()
            .chain(TD_UTIL_APPLETS)
            .chain(TD_TXT_APPLETS)
            .chain(TD_SH_APPLETS)
            .chain(&td_init)
            .chain(&["td-util", "td-init"])
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
                // `sh` is the anchor now that busybox is not in either cpio: every
                // line of both /init scripts runs through it, so a derived /bin
                // without it is a derivation that stopped working, not a boot that
                // stopped needing the shell.
                assert!(
                    names.iter().any(|n| n == "sh"),
                    "{name} runs from a cpio whose derived /bin does not contain sh - \
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

    /// Every immutable-/etc name resolves INTO the store, is staged as a
    /// symlink, and is never also written as a file. The distinction from the
    /// mutable table is the point: those are deliberately dangling, and a
    /// dangling `/etc/terminfo` is a terminal whose child cannot look up its
    /// own TERM — which nothing on the boot path reports, because a program
    /// that cannot resolve TERM just draws badly.
    #[test]
    fn every_immutable_etc_entry_points_into_the_store() {
        let steps = real_root_steps(&SYSTEM);
        for entry in IMMUTABLE_ETC {
            let link = format!("{{root}}/real-root/etc/{}", entry.etc);
            assert!(
                entry.target.starts_with("{in:"),
                "/etc/{} does not point into a staged package, so it is not immutable content",
                entry.etc
            );
            assert!(
                steps.iter().any(|step| matches!(
                    step,
                    Step::Symlink { target, link: at } if at == &link && target == entry.target
                )),
                "nothing stages /etc/{} as a symlink to {}",
                entry.etc,
                entry.target
            );
            // The target must name a package this image actually STAGES.
            // Pointing INTO the store is only syntax:
            // `{in:busybox-x86-64}/share/foo` satisfies it and would ship a
            // dangling link, busybox being a BUILD tool the shape check
            // separately refuses to find under real-root at all.
            let input = entry
                .target
                .split_once('}')
                .map(|(head, _)| format!("{head}}}"))
                .unwrap_or_default();
            let staged = steps.iter().any(|step| match step {
                Step::CopyTree { from, dest } => {
                    from == &input && dest == &format!("{{root}}/real-root{input}")
                }
                Step::StageRuntimeClosure { roots, dest } => {
                    roots.contains(&input) && dest == "{root}/real-root"
                }
                _ => false,
            });
            assert!(
                staged,
                "/etc/{} points into {input}, which nothing stages under real-root",
                entry.etc
            );
            // Exactly ONE step may touch the name, whatever its kind. A
            // WriteFile, a MkDir, or a second Symlink with a different target
            // would all be resolved by step ORDER rather than by this table.
            let touching = steps
                .iter()
                .filter(|step| match step {
                    Step::Symlink { link: at, .. } => at == &link,
                    Step::WriteFile { path, .. } | Step::MkDir { path } => path == &link,
                    _ => false,
                })
                .count();
            assert_eq!(
                touching, 1,
                "/etc/{} is staged by {touching} steps, not one",
                entry.etc
            );
            // The two tables must not name the same path: one dangles by
            // design and the other must not, so whichever step ran last would
            // decide which.
            assert!(
                !MUTABLE_ETC.iter().any(|mutable| mutable.etc == entry.etc),
                "/etc/{} is in both the mutable and immutable tables",
                entry.etc
            );
        }
    }

    /// The `/etc` symlink sweep must know about BOTH tables.
    ///
    /// It is not a check that merely skips what it does not recognise: it
    /// counts every symlink under `/etc` and compares the total against a
    /// declared one, so a table it has never heard of does not go unchecked —
    /// it reports the link as unreviewed and fails every system build. That
    /// failure is invisible to `cargo test`, because the sweep is shell that
    /// runs inside the image build, which is why it is asserted here.
    #[test]
    fn the_etc_symlink_sweep_counts_both_tables() {
        let check = shape_check();
        let names = |table: &[&str]| format!("for a in {}; do", table.join(" "));
        let immutable: Vec<&str> = IMMUTABLE_ETC.iter().map(|entry| entry.etc).collect();
        let mutable = mutable_etc_names();
        assert!(
            check.contains(&names(&immutable)),
            "the sweep's allowlist does not name every IMMUTABLE_ETC entry"
        );
        assert!(
            check.contains(&names(&mutable)),
            "the sweep's allowlist does not name every MUTABLE_ETC entry"
        );
        // Each table is counted against its OWN length rather than a merged
        // total, so a link moving between the tables is still a mismatch.
        assert!(
            check.contains(&format!(r#"[ "$m" = {} ]"#, MUTABLE_ETC.len())),
            "the sweep does not compare the mutable count against MUTABLE_ETC"
        );
        assert!(
            check.contains(&format!(r#"[ "$i" = {} ]"#, IMMUTABLE_ETC.len())),
            "the sweep does not compare the immutable count against IMMUTABLE_ETC"
        );
        assert!(
            check.contains("case $p in .|..|*/.|*/..) continue;; esac"),
            "the directory sweep mistakes each declared directory's `.` and `..` glob \
             entries for undeclared children"
        );
    }

    /// An `IMMUTABLE_ETC` row reaches the shape check as an unquoted
    /// `name=target` word, and its target as a `{in:…}` reference the engine
    /// expands. Both halves must survive that unquoted.
    #[test]
    fn immutable_etc_paths_are_shell_safe_and_well_formed() {
        assert!(!IMMUTABLE_ETC.is_empty());
        let safe = |path: &str| {
            path.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        };
        for entry in IMMUTABLE_ETC {
            let etc = entry.etc;
            assert!(
                !etc.is_empty() && !etc.starts_with('/') && !etc.ends_with('/'),
                "/etc/{etc}: the table stores a path RELATIVE to /etc"
            );
            assert!(
                !etc.starts_with('-') && !etc.contains("/-"),
                "/etc/{etc}: a leading '-' in any component is read as an OPTION by the \
                 `readlink`/`test` the generated shell runs on it"
            );
            assert!(
                !etc.contains("..") && !etc.contains("//"),
                "/etc/{etc}: a traversal or empty component would escape /etc"
            );
            assert!(safe(etc), "/etc/{etc} is not safe unquoted in the generated shell");
            // The target is a store reference rather than a literal path: the
            // hash a package lands under is not knowable here.
            let rest = entry
                .target
                .strip_prefix("{in:")
                .and_then(|rest| rest.split_once('}'))
                .map(|(input, rest)| {
                    assert!(safe(input), "{input:?} is not a safe input name");
                    rest
                });
            let Some(rest) = rest else {
                panic!("/etc/{etc} points at {:?}, which is not a {{in:…}} store reference \
                        - an absolute path would name a store hash this file cannot know",
                       entry.target);
            };
            assert!(
                rest.starts_with('/') && safe(rest),
                "/etc/{etc}: {rest:?} is not a safe absolute path under the input"
            );
            assert!(
                entry.why.len() > 20,
                "/etc/{etc}: a hole in the read-only /etc is recorded with WHY it exists"
            );
            // The reason is emitted inside a single-quoted `:` no-op, which has
            // no escape for a quote of its own.
            assert!(
                !entry.why.contains('\'') && !entry.why.contains('\n'),
                "/etc/{etc}: the reason would break out of the `:` no-op that carries it"
            );
        }
    }

    /// The terminal is packaged under its own name. Asserted on the steps
    /// because the boot does not start it yet — nothing else would notice the
    /// symlink going missing until the cutover landing tried to run it.
    #[test]
    fn the_terminal_is_packaged_as_bin_td_term() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|step| matches!(
                step,
                Step::Symlink { target, link }
                    if target == "{in:td-compositor}/bin/td-term"
                        && link == "{root}/real-root/bin/td-term"
            )),
            "/bin/td-term does not symlink into the staged td-compositor package"
        );
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
        for dir in etc_dirs() {
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
    /// state, not from image content. Both paths its immutable config names have to be
    /// table entries —
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
        assert_eq!(
            exec, "/bin/sshd -D -e -f /etc/ssh/sshd_config",
            "the service must run the foreground OpenSSH daemon against the reviewed config"
        );
        let config = build_sshd_config();
        for required in [
            format!("HostKey {SSHD_HOST_KEY}"),
            format!("AuthorizedKeysFile {SSHD_AUTHORIZED_KEYS}"),
            "AuthenticationMethods publickey".into(),
            "PasswordAuthentication no".into(),
            "KbdInteractiveAuthentication no".into(),
            "HostbasedAuthentication no".into(),
            format!("KexAlgorithms {OPENSSH_KEX_ALGORITHMS}"),
            format!("HostKeyAlgorithms {OPENSSH_KEY_ALGORITHMS}"),
            format!("PubkeyAcceptedAlgorithms {OPENSSH_KEY_ALGORITHMS}"),
            format!("Ciphers {OPENSSH_CIPHERS}"),
            "Compression no".into(),
            "DisableForwarding yes".into(),
        ] {
            assert!(
                config.lines().any(|line| line == required),
                "the reviewed OpenSSH server policy lost {required:?}"
            );
        }
        assert!(
            !config.lines().any(|line| line.starts_with("Subsystem ")),
            "the minimal server profile must not expose the unneeded SFTP subsystem"
        );
        assert!(
            config.contains(&format!(
                "Match User {SSHD_SELFTEST_USER}\n\
                 \tAuthorizedKeysFile {SSHD_SELFTEST_AUTHORIZED_KEYS}\n"
            )),
            "the volatile boot key must authorize only the unprivileged tester account"
        );
    }

    #[test]
    fn boot_health_exercises_both_openssh_authorization_paths() {
        let bootsuccess = build_bootsuccess(&SYSTEM);
        let gate_at = bootsuccess
            .find("if [ \"$admin_fixture\" = 1 ]; then")
            .unwrap_or_else(|| unreachable!("the disposable admin fixture is not gated"));
        let key_at = bootsuccess
            .find(QEMU_OPENSSH_ADMIN_PRIVATE_KEY)
            .unwrap_or_else(|| unreachable!("the disposable admin private key is not used"));
        let login_at = bootsuccess
            .find("root@127.0.0.1 /bin/echo TD-OPENSSH-ADMIN-ROUNDTRIP")
            .unwrap_or_else(|| unreachable!("the persistent administrator path is not used"));
        assert!(gate_at < key_at && key_at < login_at);
        assert!(
            bootsuccess.contains(&format!(
                "{AUTOTEST_CMDLINE_TOKEN}) admin_fixture=1"
            )),
            "only an explicit QEMU autotest boot may expect the disposable admin fixture"
        );
        assert!(
            !bootsuccess.contains(SSHD_AUTHORIZED_KEYS)
                && !bootsuccess.contains(SSHD_AUTHORIZED_KEYS_STATE),
            "boot health must never read, append, replace, or remove live administrator state"
        );
        assert!(
            bootsuccess.contains("tester@127.0.0.1 /bin/echo TD-OPENSSH-ROUNDTRIP"),
            "the tester-only volatile Match override also needs a successful login"
        );
        assert!(
            !bootsuccess.contains(&format!(
                "/bin/chown {UI_UID}:{UI_GID} /run/td-ssh-selftest.pub"
            )),
            "the tester must not be able to rewrite the file that authorizes it"
        );
        assert!(
            bootsuccess.contains(&format!(
                "/bin/chown 0:0 {SSHD_SELFTEST_AUTHORIZED_KEYS}"
            )) && bootsuccess.contains(&format!(
                "/bin/chmod 0644 {SSHD_SELFTEST_AUTHORIZED_KEYS}"
            )),
            "the tester authorization must stay root-owned but be readable after \
             sshd drops privilege"
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
            // sshd's immutable config names the host key and authorized_keys.
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
        let xdg_guard =
            format!("if /bin/td-util test -e {FIREFOX_XDG_MOUNT_MARKER}; then");
        let download_unmount = format!("/bin/umount {FIREFOX_DOWNLOAD_SOURCE} || {{");
        assert!(
            shutdown.contains("/bin/td-init sync || {")
                && shutdown.contains(&download_unmount)
                && shutdown.contains("/bin/umount /var || {")
                && shutdown.contains("/bin/umount -a -r --exclude /run || {")
                && shutdown.contains("/bin/td-util test \"$ok\" = 1")
                && shutdown.contains(SYSTEM_SHUTDOWN_MARKER),
            "the teardown must attempt every safety step and emit its marker only when all pass"
        );
        assert_eq!(shutdown.matches(&xdg_guard).count(), 1);
        assert!(
            shutdown.find(&xdg_guard) < shutdown.find(&download_unmount)
                && shutdown.find(&download_unmount) < shutdown.find("/bin/umount /var || {"),
            "Firefox's Downloads self-bind must be released only when init installed it"
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
                && init.contains("mount -t tmpfs -o mode=0755 tmpfs /sysroot/run")
                && init.contains("tmpfs /sysroot/tmp")
                && init.contains("rm -rf /sysroot/var/run")
                && init.contains("ln -s /run /sysroot/var/run"),
            "stage-1 init must mount persistent @var, keep runtime state volatile, and link /var/run into /run"
        );
        assert!(
            !init.lines().any(|line| {
                line.contains("-t tmpfs") && line.trim_end().ends_with(" /sysroot/var")
            })
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
            if gets_generic_persistent_home_setup(user) {
                assert!(
                    init.contains(&path),
                    "stage-1 init must create state directory {path} before switch_root"
                );
                assert!(
                    init.contains(&format!("chmod 0700 {path}")),
                    "stage-1 init must make application home {path} private"
                );
            } else if user.uid == 0 {
                assert!(init.contains("/sysroot/var/root"));
            } else {
                assert_eq!(user.name, AUDIO_USER);
                assert!(!init.contains(&path));
            }
        }
        assert!(
            init.contains("umask 077") && init.contains("mkdir -p /sysroot/var/root"),
            "stage-1 init must create the root home with mode 0700"
        );
        assert!(
            init.contains("chown 0:0 /sysroot/var")
                && init.contains("chmod 0755 /sysroot/var")
                && init.contains("chmod 0700 /sysroot/var/root")
                && init.contains(
                    "chown 0:0 /sysroot/var/lib /sysroot/var/lib/td-test"
                )
                && init.contains(
                    "chmod 0555 /sysroot/var/lib/td-test/td-jail-seccomp-probe"
                )
                && init.contains(&format!(
                    "chmod 0600 /sysroot{QEMU_OPENSSH_ADMIN_PRIVATE_KEY}",
                ))
                && init.contains(&format!(
                    "/sysroot{SSHD_AUTHORIZED_KEYS_STATE}"
                )),
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
    fn firefox_downloads_is_prepared_once_before_switch_root() {
        let init = build_deployment_init(&SYSTEM);
        let source_mkdir = format!(
            "elif /bin/td-util mkdir -p /sysroot{FIREFOX_DOWNLOAD_SOURCE}; then"
        );
        let download_mount = format!(
            "/bin/mount -o bind /sysroot{FIREFOX_DOWNLOAD_SOURCE} /sysroot{FIREFOX_DOWNLOAD_SOURCE}"
        );
        let download_link_guard = format!(
            "if /bin/td-util readlink /sysroot{FIREFOX_DOWNLOAD_SOURCE} >/dev/null 2>&1; then"
        );
        let download_directory_guard = format!(
            "elif /bin/td-util test -e /sysroot{FIREFOX_DOWNLOAD_SOURCE} && ! /bin/td-util test -d /sysroot{FIREFOX_DOWNLOAD_SOURCE}; then"
        );
        let source_chown = format!(
            "/bin/td-util chown {UI_UID}:{UI_GID} /sysroot{FIREFOX_DOWNLOAD_SOURCE}"
        );
        let source_chmod = format!(
            "/bin/td-util chmod 0700 /sysroot{FIREFOX_DOWNLOAD_SOURCE}"
        );
        let mount_marker =
            format!("/bin/td-util printf '' > /sysroot{FIREFOX_XDG_MOUNT_MARKER}");
        let downloads_end =
            "/bin/sh -c 'umask 077; /bin/td-util mkdir -p /sysroot/var/root'";
        assert_eq!(init.matches(&source_mkdir).count(), 1);
        assert_eq!(init.matches(&download_link_guard).count(), 1);
        assert_eq!(init.matches(&download_directory_guard).count(), 1);
        assert_eq!(init.matches(&source_chown).count(), 1);
        assert_eq!(init.matches(&source_chmod).count(), 1);
        assert_eq!(init.matches(&download_mount).count(), 1);
        assert_eq!(init.matches(&mount_marker).count(), 1);
        assert_eq!(init.matches("grant disabled").count(), 3);
        assert!(
            !init
                .split_once(&download_link_guard)
                .expect("Firefox Downloads setup start")
                .1
                .split_once(downloads_end)
                .expect("Firefox Downloads setup end")
                .0
                .contains("exit 1"),
            "untrusted persistent Downloads entries must disable the grant, not abort init"
        );
        assert!(
            init.find(&download_link_guard) < init.find(&download_directory_guard)
                && init.find(&download_directory_guard) < init.find(&source_mkdir)
                && init.find(&source_mkdir) < init.find(&source_chown)
                && init.find(&source_chown) < init.find(&source_chmod)
                && init.find(&source_chmod) < init.find(&download_mount)
                && init.find(&download_mount) < init.find(&mount_marker)
                && init.find(&mount_marker) < init.find("exec /bin/switch_root /sysroot /init"),
            "the Downloads self-bind must precede switch_root"
        );
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
        assert!(valid_home(AUDIO_UID, AUDIO_RUNTIME));
        assert!(!valid_home(AUDIO_UID, "/home/audio"));
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
        let services = build_td_svc_conf();
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
                && bootsuccess.contains("/bin/ssh-keygen -q -t ed25519 -N ''")
                && bootsuccess.contains("GIT_SSH_COMMAND=\"/bin/ssh -F /dev/null")
                && bootsuccess.contains(&format!(
                    "-o KexAlgorithms={OPENSSH_KEX_ALGORITHMS}"
                ))
                && bootsuccess.contains(&format!(
                    "-o Ciphers={OPENSSH_CIPHERS}"
                ))
                && bootsuccess.contains("/bin/git init --bare -b main origin")
                && bootsuccess
                    .contains("/bin/git -C work push -u origin HEAD:refs/heads/main")
                && bootsuccess.contains("/bin/git clone \"$remote\" work")
                && bootsuccess.contains("/bin/git clone \"$remote\" verify")
                && !bootsuccess.contains("--upload-pack=")
                && !bootsuccess.contains("remote.origin.receivepack")
                && bootsuccess.contains("remote=ssh://tester@127.0.0.1")
                && bootsuccess.contains("HOME=/tmp/td-git-probe/home")
                && bootsuccess.contains("GIT_CONFIG_GLOBAL=/dev/null")
                && bootsuccess.contains("/bin/git -C work submodule --td-invalid")
                && bootsuccess.contains("git: SSH upload-pack clone failed")
                && bootsuccess.contains("git: receive-pack push failed")
                && bootsuccess.contains("/etc/ssl/certs/ca-certificates.crt")
                && bootsuccess.contains(GIT_RUNTIME_MARKER)
                && bootsuccess.contains("/bin/codex --version")
                && bootsuccess.contains(CODEX_VERSION_OUTPUT)
                && bootsuccess.contains("/bin/bwrap --version")
                && bootsuccess.contains(CODEX_BWRAP_VERSION_OUTPUT)
                && bootsuccess.contains("/bin/codex sandbox -P :read-only")
                && bootsuccess
                    .contains("/run/user/1000/td-codex-sandbox-probe/home/.codex")
                && bootsuccess.contains("/bin/printf \"%s\\n\" outside >")
                && bootsuccess.contains("/bin/readlink /proc/self/ns/net >")
                && bootsuccess.contains(
                    "/bin/sh -c '\\''if { /bin/printf sandboxed >> fixture; }"
                )
                && bootsuccess.contains("sandbox-network-namespace-unchanged")
                && bootsuccess.contains("codex: could not clean the sandbox probe")
                && bootsuccess.contains("TD-CODEX-SANDBOX-OK")
                && bootsuccess.contains(CODEX_RUNTIME_MARKER)
                && bootsuccess.contains("TD-OPENSSH-ROUNDTRIP")
                && bootsuccess.contains(SSHD_MARKER)
                && bootsuccess.contains("/bin/td-util --list")
                && bootsuccess.contains(TD_UTIL_RUNTIME_MARKER)
                && bootsuccess.contains("/bin/td-jail --probe-transition")
                && bootsuccess.contains(&format!(
                    "\"{TD_JAIL_TRANSITION_MARKER} pid=1\""
                ))
                && bootsuccess.contains("/bin/td-jail --internal-write-seccomp-filter")
                && bootsuccess.contains("/var/lib/td-test/td-jail-seccomp-probe")
                && bootsuccess.contains("/run/td-jail-seccomp-probe/probe")
                && bootsuccess.contains("[ ! -w /run/td-jail-seccomp-probe/filter.bpf ]")
                && bootsuccess.contains(TD_JAIL_SECCOMP_PROBE_MARKER)
                && bootsuccess.contains(&format!(
                    "/bin/td-busd probe /run/user/{UI_UID}/bus"
                ))
                && bootsuccess.contains(TD_BUSD_RUNTIME_MARKER)
                // The GATE, and the whole leg, not merely that the command
                // and the marker both appear somewhere in the script. Moving
                // the `echo` out of the `then` branch leaves both `contains`
                // above matching while the marker prints for a bus that never
                // answered. Every farm beside it is pinned leg-whole for the
                // same reason; see the td-login assertion further down.
                //
                // What this leg's `else` holds is NOT `healthy=0`, and an
                // earlier version of this comment named dropping that as the
                // mutation to catch — while the commit it was written for
                // dropped it deliberately. It counts instead: `btb` is what
                // gives the marker its bounded grace on the success gate, and
                // a leg whose `else` disappeared would take the retry with it
                // and turn a restarting broker into a red image.
                && bootsuccess.contains(&format!(
                    "[ \"$mtb\" = 1 ] || {{ echo {TD_BUSD_RUNTIME_MARKER}; mtb=1; }}; \
                     else btb=$((btb+1)); fi"
                ))
                && bootsuccess.contains(
                    "&& { [ \"$mtb\" = 1 ] || [ \"$btb\" -ge \"$bg\" ]; } \
                     && /bin/td-boot success"
                )
                // And the two lines that make that gate a RETRY. The
                // initialiser, because an unset `bg` makes the comparison
                // `[ 1 -ge "" ]` and the gate an error rather than a wait; and
                // the CLAMP, which is the whole safety argument. `wait` comes
                // off the kernel command line and may legitimately be 1, and
                // with an unclamped grace of 2 a single failed probe could
                // never reach the threshold — `td-boot success` would never be
                // called, the loop would end in `fail`, and the rollback lever
                // this landing removed would be back by another route. Neither
                // line has a test of its own anywhere else, and both were
                // silently removable before this assertion.
                && bootsuccess.contains(&format!("bg={BUS_MARKER_GRACE_SWEEPS}"))
                && bootsuccess
                    .contains("[ \"$bg\" -ge \"$wait\" ] && bg=$((wait-1))")
                // And the probe's own words reach the console. The oracle's
                // failure text asks the reader to tell a refused bind from a
                // refused uid from a bus that accepted and said nothing, and
                // only the probe knows which of those it was: a leg that
                // redirected the diagnostic to /dev/null would leave one fixed
                // sentence on ttyS0 for five different faults.
                && bootsuccess.contains("/run/user/1000/bus: $b\"; exit 1; }")
                && bootsuccess.contains("[ \"$mtj\" = 1 ] || healthy=0")
                && bootsuccess.contains("[ \"$mts\" = 1 ] || healthy=0")
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
            services.contains(&format!(
                "/bin/td-compositor probe-application {FIREFOX_WINDOW_READY_SOCKET} \
                 {FIREFOX_APP_ID} {FIREFOX_CONTENT_RGB_A} \
                 {FIREFOX_CONTENT_RGB_B} 2>/dev/null"
            )) && services.contains(&format!(
                "/bin/td-jail --probe-process-token {FIREFOX_NAME} -contentproc 2>/dev/null"
            )) && services.contains(&format!(
                "{FIREFOX_EVIDENCE} > {FIREFOX_EVIDENCE_TMP_PATH}"
            )) && services.contains(&format!(
                "/bin/echo {TD_FIREFOX_BOOT_MARKER}"
            )) && services.contains(&format!(
                "/bin/echo {TD_FIREFOX_CONTENT_MARKER}"
            )) && services.contains(&format!(
                "/bin/mv {FIREFOX_EVIDENCE_TMP_PATH} {FIREFOX_EVIDENCE_PATH}"
            )) && services.contains(&format!(
                "{FIREFOX_COMPLETION} > {FIREFOX_COMPLETION_TMP_PATH}"
            )) && services.contains(&format!(
                "/bin/mv {FIREFOX_COMPLETION_TMP_PATH} {FIREFOX_COMPLETION_PATH}"
            )) && !bootsuccess.contains(TD_FIREFOX_BOOT_MARKER)
                && !bootsuccess.contains(TD_FIREFOX_CONTENT_MARKER)
                && !bootsuccess.contains(TD_FIREFOX_SUPPORT_MARKER)
                && profile.contains(&format!(
                    "firefox_complete=$(/bin/td-util cat {FIREFOX_COMPLETION_PATH} 2>/dev/null)"
                ))
                && profile.contains(&format!(
                    "input=$(/bin/td-util cat {FIREFOX_INPUT_COMPLETION_PATH} 2>/dev/null)"
                ))
                && profile.contains(&format!(
                    "[ \"$input\" = {FIREFOX_INPUT_COMPLETION} ] && input_ok=1"
                ))
                && profile.contains(&format!(
                    "[ \"$status\" = td-boot-success-v1 ] && [ \"$firefox\" = {FIREFOX_EVIDENCE} ] && [ \"$firefox_complete\" = {FIREFOX_COMPLETION} ] && [ \"$input_ok\" = 1 ] && break"
                )),
            "Firefox evidence must be exact without controlling deployment health"
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
                && bootsuccess.find("/bin/git init --bare").unwrap()
                    < bootsuccess
                        .find("&& /bin/td-boot success /dev/vda")
                        .unwrap()
                && bootsuccess.find("/bin/codex --version").unwrap()
                    < bootsuccess
                        .find("&& /bin/td-boot success /dev/vda")
                        .unwrap()
                && bootsuccess.find("/bin/bwrap --version").unwrap()
                    < bootsuccess
                        .find("&& /bin/td-boot success /dev/vda")
                        .unwrap()
                && bootsuccess.find("TD-OPENSSH-ROUNDTRIP").unwrap()
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
        assert!(
            bootsuccess.contains(&format!(
                "/bin/grep -q -x -F TD-CODEX-SANDBOX-OK || \
                 {{ echo \"codex: sandbox transition omitted its success evidence: $s\"; \
                 exit 1; }}; \
                 /bin/rm -rf /run/user/{UI_UID}/td-codex-sandbox-probe || \
                 {{ echo \"codex: could not clean the sandbox probe\"; exit 1; }}'; then \
                 [ \"$mc\" = 1 ] || {{ echo {CODEX_RUNTIME_MARKER}; mc=1; }}; \
                 else healthy=0; fi"
            )),
            "Codex must drive a confined command through Bubblewrap before their shared marker"
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
                && bootsuccess.contains(&format!(
                    "td-boot {} /dev/vda /run/td-update",
                    td_boot_protocol::UPDATE_VERB
                ))
                && bootsuccess.contains(SYSTEM_DEPLOY_INSTALL_MARKER),
            "the root-owned health target must wire the transactional update through \
             td-boot's own update verb"
        );
        // EVERY pass names the volume, including the wrong-key one, which is the
        // only one no whole-fragment pin covers. `update` requires it to be a
        // real mount point, so a pass that dropped it would not merely lose the
        // check — it would take the CHANNEL as the volume and fail for a reason
        // that has nothing to do with what the pass is about.
        assert_eq!(
            bootsuccess
                .matches("/run/td-update /run/td-volume /run/td-volume/")
                .count(),
            4,
            "all four update passes — wrong key, idle, real, and the reinstall \
             after the rollback — must name the volume between the mountpoint \
             and the channel"
        );
        // The verb takes a CHANNEL, not a bundle. Passing the candidate would
        // make it look for `<candidate>/candidate` and read every tick as
        // nothing to do — a silent no-op that leaves every other marker green.
        //
        // Scoped to the update chain rather than asked of the whole script,
        // which review called correctly: it is a bare substring test for a
        // common English word over 10 KB that also carries the uutils, td-init,
        // td-util, td-txt and td-login probe farms and the configured hostname,
        // so unscoped it false-REDS on any innocent later use of the word —
        // rewording a diagnostic here would do it — and the message it fails
        // with points at these four passes. The scan grew with the rollback
        // pass and so did that hazard: its four diagnostics sit inside the
        // region now, and one reworded to say "a different candidate" would
        // red this for no reason. None of them does today.
        let chain = bootsuccess
            .split_once(DEPLOY_INSTALL_CMDLINE_TOKEN)
            .and_then(|(_, rest)| rest.split_once(SYSTEM_DEPLOY_ROLLBACK_MARKER))
            .map(|(chain, _)| chain)
            .expect("the update chain runs from the cmdline token to the rollback marker");
        assert!(
            !chain.contains(td_boot_protocol::CHANNEL_CANDIDATE),
            "the update passes a channel; naming the candidate would look one \
             level too deep and read as nothing to do"
        );
        // An IDLE channel is the state a machine is in on almost every tick, so
        // the oracle drives it: exit 0 AND print nothing. Only the second half
        // separates it from an update that silently installed something.
        //
        // Pinned as ONE fragment rather than as two `contains` calls, because
        // the two halves are only a contract when they are joined: review
        // showed that dropping the `!` — which inverts the exit test — and
        // redirecting the command somewhere other than the file `-s` reads
        // BOTH left the separate assertions green. The second is the worse of
        // the two: `/run/td-idle-id` would then never be written, `-s` would be
        // false forever, and the pass would fall through to the install having
        // asserted nothing at all.
        let idle_branch = format!(
            "elif ! /bin/td-boot {update} /dev/vda /run/td-update {volume} {volume}/{idle} \
{volume}/{key} >{out}; then echo 'td-boot update failed on a channel with nothing \
in it'; healthy=0; elif [ -s {out} ]; then",
            update = td_boot_protocol::UPDATE_VERB,
            volume = "/run/td-volume",
            idle = crate::ladder::DEPLOY_IDLE_CHANNEL,
            key = td_boot_protocol::VOLUME_TRUSTED_KEY,
            out = "/run/td-idle-id",
        );
        assert!(
            bootsuccess.contains(&idle_branch),
            "the oracle must drive an empty channel under the REAL key, require exit \
             0, and read the same file it redirected; wanted\n{idle_branch}\nin\n{bootsuccess}"
        );
        // The REAL pass, held to BOTH halves as the idle one is. Review found it
        // held to only the first: it asserted exit 0 and never looked at what it
        // redirected, and `update` exits 0 printing nothing whenever a channel
        // holds no candidate — so ANY wrong channel here printed the install
        // marker and the boot-success marker with every gate assertion green.
        // That is the silent no-op this whole item is about, reintroduced by the
        // very commit that added the idle pass to catch it.
        //
        // The pair is pinned rather than the channel alone, and that is not
        // belt-and-braces: `VOLUME_CHANNEL_DIR` is a strict PREFIX of
        // `DEPLOY_IDLE_CHANNEL` (`td/incoming` of `td/incoming-idle`), so a bare
        // `contains` on the channel — or a `find` for it — is satisfied by the
        // IDLE pass and says nothing about this one.
        let real_branch = format!(
            "elif ! /bin/td-boot {update} /dev/vda /run/td-update {volume} {volume}/{channel} \
{volume}/{key} >{out}; then echo 'td-boot update failed on the channel holding a \
bundle'; healthy=0; elif ! [ -s {out} ]; then echo 'td-boot update installed nothing \
from the channel holding a bundle'; healthy=0; else echo {marker};",
            update = td_boot_protocol::UPDATE_VERB,
            volume = "/run/td-volume",
            channel = td_boot_protocol::VOLUME_CHANNEL_DIR,
            key = td_boot_protocol::VOLUME_TRUSTED_KEY,
            out = "/run/td-installed-id",
            marker = SYSTEM_DEPLOY_INSTALL_MARKER,
        );
        assert!(
            bootsuccess.contains(&real_branch),
            "the real update pass must name the channel AND the real key, and must \
             require that it INSTALLED something rather than only that it exited 0; \
             wanted\n{real_branch}\nin\n{bootsuccess}"
        );
        // The ROLLBACK pass, §11's third oracle, pinned whole for the reason the
        // other two are: its five branches are only a contract joined, and
        // separating them lets one pass while another does nothing — a rollback
        // whose id is never compared could land anywhere, and a reinstall never
        // compared to what the first install named would leave the volume in a
        // state the boots after this one do not expect and blame on selection.
        //
        // The `success` branch is the one review had to add, and it is the only
        // branch that observes an EFFECT rather than a printed id. A rollback
        // that printed the right id and never rewrote `current` satisfies the
        // comparison above it; the reinstall then finds its own id already
        // current, takes `install_deployment`'s idempotent branch, prints that
        // same id, and satisfies the comparison below it — so the whole pass
        // went green without anything having rolled back. `success` refuses
        // unless the id it is given IS current, so it is the assertion; on a
        // deployment already marked successful it returns before doing anything
        // else, so it is only that.
        let rollback_branch = format!(
            "else echo {install}; if ! /bin/td-boot rollback /dev/vda /run/td-update >{rolled}; \
then echo 'td-boot rollback failed after the update installed a deployment'; healthy=0; \
elif ! /bin/grep -q -x -F \"$deployment\" {rolled}; then echo 'td-boot rollback did not \
return to the deployment that booted'; healthy=0; elif ! /bin/td-boot success /dev/vda \
/run/td-update \"$deployment\" >{current}; then echo 'td-boot rollback printed an id \
without making it current'; healthy=0; elif ! /bin/td-boot {update} /dev/vda \
/run/td-update {volume} {volume}/{channel} {volume}/{key} >{again}; then echo 'td-boot \
update could not reinstall the deployment after a rollback'; healthy=0; elif ! /bin/grep \
-q -x -F -f {installed} {again}; then echo 'the reinstall after a rollback named a \
different deployment'; healthy=0; else echo {marker}; fi; fi;",
            install = SYSTEM_DEPLOY_INSTALL_MARKER,
            update = td_boot_protocol::UPDATE_VERB,
            volume = "/run/td-volume",
            channel = td_boot_protocol::VOLUME_CHANNEL_DIR,
            key = td_boot_protocol::VOLUME_TRUSTED_KEY,
            rolled = "/run/td-rolled-id",
            current = "/run/td-rolled-current",
            installed = "/run/td-installed-id",
            again = "/run/td-reinstalled-id",
            marker = SYSTEM_DEPLOY_ROLLBACK_MARKER,
        );
        assert!(
            bootsuccess.contains(&rollback_branch),
            "the rollback pass must roll back, require the id it lands on to be the \
             deployment that BOOTED, reinstall, and require that reinstall to name \
             what the first install did; wanted\n{rollback_branch}\nin\n{bootsuccess}"
        );
        // The NEGATIVE half, guarded here because no gate boots a VM: the oracle
        // that would notice its absence runs only under `qemu-boot-system`. The
        // candidate is signed whether or not td-boot is told to check it, so
        // without a refused pass the trusted-key argument could be ignored
        // outright and every marker would still appear.
        assert!(
            bootsuccess.contains(&format!(
                "/run/td-volume/{}",
                crate::ladder::DEPLOY_WRONG_KEY
            )) && bootsuccess.contains(&format!(
                "/run/td-volume/{}",
                td_boot_protocol::VOLUME_TRUSTED_KEY
            )),
            "the update pass must run under BOTH the wrong key and the real one"
        );
        assert!(
            bootsuccess.contains(td_boot_protocol::MANIFEST_UNAUTHENTICATED),
            "the refused pass must check WHY it was refused: a missing key or a \
             busy lock would satisfy a bare failure check"
        );
        // Bound rather than compared as `Option`s: `None < Some(_)` is true,
        // so a MISSING wrong key would satisfy the ordering it is meant to fail.
        let wrong_at = bootsuccess.find(crate::ladder::DEPLOY_WRONG_KEY).unwrap();
        let real_at = bootsuccess
            .find(td_boot_protocol::VOLUME_TRUSTED_KEY)
            .unwrap();
        assert!(
            wrong_at < real_at,
            "the wrong key must be tried FIRST, so a refusal that did not refuse \
             cannot be masked by the install that follows it"
        );
        // The refusal pattern is interpolated into SINGLE-QUOTED shell. A reword
        // carrying an apostrophe emits a broken script that no gate executes, and
        // an empty one turns `grep -F ''` into a match on anything — either way
        // the refused pass stops testing what it says it does while staying green.
        let pattern = td_boot_protocol::MANIFEST_UNAUTHENTICATED;
        assert!(
            pattern.len() >= 8 && !pattern.chars().any(|c| c == '\'' || c == '\\' || c.is_control()),
            "the refusal pattern must be a non-trivial, single-quotable string: {pattern:?}"
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
        // Persistent home ownership is fixed for every applicable user below
        // /var. Audio's home is the volatile runtime created by td-seatd.
        for u in SYSTEM.users {
            if gets_generic_persistent_home_setup(u) {
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
                && profile.contains("firefox_wait=0")
                && profile.contains(&format!(
                    "[ \"$firefox_wait\" -ge {FIREFOX_GREETER_WAIT_ITERATIONS} ] && break"
                ))
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
        // token self-tests resolve + reach + verified Git HTTPS, printing four net
        // markers. Each marker must be gated on its real operation so a failure drops
        // the marker and reds the qemu-boot-net oracle rather than false-passing.
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
        assert!(
            netup.contains("GIT_CONFIG_GLOBAL=/dev/null")
                && netup.contains("GIT_TERMINAL_PROMPT=0")
                && !netup.contains("GIT_SSL_NO_VERIFY")
                && netup.contains("GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt")
                && netup.contains(&format!("/bin/git ls-remote {GIT_HTTPS_TEST_URL} HEAD"))
                && netup.contains(&format!("echo {GIT_HTTPS_RUNTIME_MARKER}")),
            "the Git HTTPS marker must follow an isolated, noninteractive, verified upstream query"
        );
    }

    /// Every cpio member `shape_check` demands is one the specs actually emit.
    ///
    /// This is the mismatch nothing else models: `shape_check` runs inside the
    /// BUILD, so a member it requires and the spec no longer writes reds at step
    /// 146 of 146 with every host test green — which is exactly what happened
    /// when `sh` left busybox and three assertions kept asking for
    /// `bin/busybox`. Checking the two texts against each other here turns that
    /// into a cargo-test failure.
    #[test]
    fn shape_check_asks_the_initramfs_for_members_it_actually_packs() {
        let check = shape_check();
        let specs = [
            ("selector", build_initramfs_spec("selector-init", Phase::Selector)),
            ("deployment", build_initramfs_spec("deployment-init", Phase::Deployment)),
        ];
        // The `for m in … ; do` sweep: every name is a cpio member path, and both
        // archives are swept with the same list.
        let marker = "for m in ";
        let start = check.find(marker).expect("shape_check sweeps the cpio members");
        let rest = check.get(start + marker.len()..).unwrap_or_default();
        let list = rest.split(';').next().unwrap_or_default();
        // No filter: the top-level members (`init`, `proc`, `run`, `volume`,
        // `sysroot`) have no slash in them and are exactly the ones a
        // slash-requiring filter drops -- five of the eleven, silently.
        let members: Vec<&str> = list.split_whitespace().collect();
        // Bounded EXACTLY, not `>=`: the ban test's count guard was replaced two
        // hunks up because slack keeps a check green while the thing it guards
        // quietly shrinks, and a floor here would do the same for a member
        // shape_check stopped demanding.
        assert_eq!(members.len(), 11, "the member sweep parsed as {members:?}");
        for (phase, spec) in &specs {
            for m in &members {
                // A member is whatever the spec declares it as -- dir, slink,
                // file or nod -- written with a LEADING slash where the cpio
                // lists it without.
                let declared = ["dir", "slink", "file", "nod"]
                    .iter()
                    .any(|kind| spec.contains(&format!("{kind} /{m} ")));
                assert!(
                    declared,
                    "shape_check requires cpio member '{m}', but the {phase} spec \
                     declares no such path - the build would red at shape_check \
                     with every test here green"
                );
            }
        }
        // ...and the store members it greps for by regex, which is how a dangling
        // symlink is caught: the slink can be there while its target is not. The
        // script is one continuous line, so these are found by the PATTERN rather
        // than by scanning lines.
        const PAT: &str = "^td/store/[^/]+/bin/";
        let mut greps = 0usize;
        for (at, _) in check.match_indices(PAT) {
            let name: String = check
                .get(at + PAT.len()..)
                .unwrap_or_default()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            greps += 1;
            // WHICH archive is asked is part of the claim: td-kexec is
            // selector-only and losetup deployment-only, so "some spec carries
            // this file" would let a payload sit in the archive that does not
            // need it while the one that does greps for nothing. The phase comes
            // from the list variable the grep reads, which is the same text.
            //
            // The member sweep above does NOT cover this: it pins `bin/<name>`,
            // the /bin SYMLINK, where these are `td/store/…/bin/<name>`, the
            // payload it points at. Different paths, and the dangling case is
            // exactly one present without the other.
            let before = check.get(..at).unwrap_or_default();
            // Bounded to THIS statement. The script is one continuous line, so
            // searching the whole prefix answers about whatever ran earlier: past
            // the `for l in …` sweep every later grep looks like it named both
            // lists, whichever one it actually reads. A statement starts at its
            // own `printf`, which is where the list variable is named.
            let stmt_at = before.rfind("printf").unwrap_or_default();
            let stmt = before.get(stmt_at..).unwrap_or_default();
            // Both halves below read that window, so its premise is pinned rather
            // than assumed: the `printf` found must be THIS statement's. A grep
            // led by anything else (`if echo "$init_list" | grep …`) would inherit
            // the previous statement's window, and for the SENSE that is not a
            // safe direction -- a forbid would be read as a requirement.
            assert!(
                !stmt.contains("; "),
                "the `bin/{name}` store grep is not led by a `printf` of the list it \
                 reads - this scan takes both its archive and its sense from that \
                 one statement"
            );
            let sel = stmt.contains(r#""$selector_list""#);
            let dep = stmt.contains(r#""$init_list""#);
            // Naming NEITHER means the grep is inside the sweep over both, which
            // is required to be exactly that text rather than inferred: a shape
            // this cannot read is the bug the bounding exists to catch, so it
            // fails here instead of guessing at an archive.
            let in_both = !sel
                && !dep
                && before
                    .get(..stmt_at)
                    .unwrap_or_default()
                    .ends_with("for l in \"$selector_list\" \"$init_list\"; do ");
            assert!(
                sel || dep || in_both,
                "the `bin/{name}` store grep names no list variable and is not the \
                 two-list sweep - this scan cannot say which archive it asks about"
            );
            // Some of these greps assert ABSENCE -- `if … grep -q …; then error`
            // is how td-kexec is kept selector-only -- so the sense matters as
            // much as the phase. A negative read as a requirement demands the
            // opposite of what the script says.
            let positive = {
                let stmt = before.rfind("printf");
                let negated = before.rfind("if printf");
                !matches!((stmt, negated), (Some(t), Some(n)) if n + 3 == t)
            };
            let want: &[&str] = if in_both || (sel && dep) {
                &["selector", "deployment"]
            } else if sel {
                &["selector"]
            } else {
                &["deployment"]
            };
            // The `file` KIND is required, not merely the name: `slink /bin/X
            // {in:X}/bin/X …` contains the target's path too, so a substring
            // match is satisfied by the very symlink whose danglingness this is
            // supposed to catch.
            let payload = format!("/bin/{name}");
            for phase in want {
                let packed = specs.iter().any(|(p, spec)| {
                    p == phase
                        && spec.lines().any(|l| {
                            l.strip_prefix("file ")
                                .and_then(|r| r.split_whitespace().next())
                                .is_some_and(|dest| dest.ends_with(&payload))
                        })
                });
                assert_eq!(
                    packed,
                    positive,
                    "shape_check {} the {phase} initramfs to carry a `bin/{name}` store \
                     member and that spec {} - the build would red at shape_check with \
                     every test here green",
                    if positive { "requires" } else { "forbids" },
                    if packed { "packs one" } else { "packs none" }
                );
            }
        }
        // Exact for the same reason: a floor stays green while shape_check quietly
        // stops asking one archive for a payload the other still gets checked for.
        assert_eq!(greps, 9, "{greps} store-member greps found - the scan has gone stale");
    }

    /// `find` and `xargs` left the image with the multicall, and this is the
    /// only place that says so. They were never `/bin` names — the ladder's
    /// findutils dead-axis lock forbids those tokens in step text and cannot
    /// tell a member name from an invocation — so they lived only as
    /// `busybox find` / `busybox xargs`, and uutils ships neither: they are
    /// findutils, not coreutils. `DROPPED_APPLETS` would be the natural home
    /// for the assertion and cannot hold it, because that list is spliced into
    /// shape_check's shell text and would trip the very lock in question.
    ///
    /// This is a LOSS, recorded as one. `/bin/fd` covers the common `find`
    /// case; nothing here replaces `xargs`.
    ///
    /// The farm and symlink legs below were ALREADY true before the multicall
    /// left — that is this comment's own premise, so on their own they assert
    /// nothing this landing changed. What makes the drop real is that the only
    /// thing which ever served these names is not packed, so the first leg is
    /// the load-bearing one: restore the `CopyTree` and `busybox find` works
    /// again, and without it the rest of this test would not notice.
    #[test]
    fn find_and_xargs_left_the_image() {
        // The provider, gone. Same scan as `nothing_on_the_image_is_busybox`,
        // asserted here too because THIS is the test that claims these two
        // names are unreachable, and they are reachable the moment it is back.
        for step in real_root_steps(&SYSTEM) {
            if let Step::CopyTree { from, .. } = &step {
                assert!(
                    !from.contains("busybox"),
                    "the busybox package is packed again, so `busybox find` and \
                     `busybox xargs` are reachable and this test's claim is false"
                );
            }
            if let Step::Symlink { target, .. } = &step {
                assert!(
                    !target.contains("busybox"),
                    "a /bin entry resolves into busybox again, so the multiplexer \
                     is back and with it these two names"
                );
            }
        }
        let td_init = td_init_applets();
        let applications = application_names(&SYSTEM);
        let packed = packed_bin_names();
        for name in ["find", "xargs"] {
            for (farm, set) in &bin_farms(&td_init, &applications) {
                assert!(
                    !set.contains(&name),
                    "'{name}' is in the {farm} farm - it left with busybox, and bringing \
                     it back means a recipe that builds it, not a symlink"
                );
            }
            assert!(
                !packed.iter().any(|p| p == name),
                "/bin/{name} is packed, but nothing on this image provides it"
            );
        }
        // `fd` is what carries the common case, so its /bin entry is part of
        // this claim rather than an unrelated fact.
        assert!(
            packed.iter().any(|p| p == "fd"),
            "/bin/fd is not packed - it is what `find` leaving was acceptable because of"
        );
    }

    /// NOTHING on this image is busybox — not the archives, and since `getty`
    /// moved to td-init, not the real root either. This is the property the
    /// retirement is FOR, and nothing else asserts it: the ban test covers the
    /// CALL SITES and the member sweeps cover what each archive is required to
    /// pack, but a busybox reintroduced BESIDE td's own binaries trips neither —
    /// every script would still say `/bin/sh`, `/bin/sh` would still be td-sh,
    /// and the multicall would simply be back with nothing red.
    ///
    /// The real-root leg is the one that changed, so it is the one worth being
    /// precise about: it scans the STEPS, which is where a `CopyTree` of the
    /// package or a `/bin/busybox` symlink would have to appear. `shape_check`
    /// then re-proves it against the staged tree at build time, because a step
    /// list is what this file intends and the tree is what the image gets.
    #[test]
    fn nothing_on_the_image_is_busybox() {
        for (phase, spec) in [
            ("selector", build_initramfs_spec("selector-init", Phase::Selector)),
            ("deployment", build_initramfs_spec("deployment-init", Phase::Deployment)),
        ] {
            assert!(
                !spec.contains("busybox"),
                "the {phase} initramfs spec names busybox - the multicall is back in \
                 the boot archives. Nothing else here would have said so"
            );
        }
        // Every string a real-root step can carry a store path in. Spelled out
        // per variant rather than read off a `Debug` render, which `Step` does
        // not derive. The wildcard below is a runtime backstop, NOT a
        // compile-time one: a new variant compiles fine here and fails only if
        // `real_root_steps` emits it — which is the moment it would matter.
        for step in real_root_steps(&SYSTEM) {
            let paths: Vec<String> = match step {
                Step::Symlink { target, link } => vec![target, link],
                Step::CopyTree { from, dest } => vec![from, dest],
                Step::CopyFiles { files, dest } => {
                    files.into_iter().chain(std::iter::once(dest)).collect()
                }
                Step::StageRuntimeClosure { roots, dest } => {
                    roots.into_iter().chain(std::iter::once(dest)).collect()
                }
                Step::CompileApplicationTables {
                    names,
                    packages,
                    runtimes,
                    registry,
                    launcher,
                } => names
                    .into_iter()
                    .chain(packages)
                    .chain(runtimes)
                    .chain([registry, launcher])
                    .collect(),
                Step::MkDir { path } => vec![path],
                Step::WriteFile { path, content, .. } => vec![path, content],
                Step::Run { argv, env, dir } => argv
                    .into_iter()
                    .chain(env.into_iter().flat_map(|(key, value)| [key, value]))
                    .chain(std::iter::once(dir))
                    .collect(),
                // Not a variant real_root_steps emits today. Failing beats
                // skipping: an unmodelled variant is one this scan cannot see
                // into, which is exactly how a repacked binary would get past.
                _ => panic!("real_root_steps emits a step variant this scan does not model"),
            };
            for path in paths {
                assert!(
                    !path.contains("busybox"),
                    "a real-root step names busybox - the multicall left this image \
                     with its last applet, and packing it again is a decision, not a \
                     step: {path}"
                );
            }
        }
        // The staged-tree half of the argument, pinned like its neighbours. The
        // Rust scan above walks ONE function; a `/bin/busybox` packed by any
        // other route — etc_files, a new step producer, a CopyTree added to the
        // top-level steps() — is caught only by shape_check, and a check nothing
        // pins can be weakened while every test here stays green. `-L` is quoted
        // with the rest because a symlink to a target the build tree does not
        // hold is DANGLING, which `-e` alone reads as absent: exactly how a
        // repacked multiplexer entry would slip through.
        let shape = shape_check();
        for leg in [
            r#"if [ -e "$root/bin/busybox" ] || [ -L "$root/bin/busybox" ]; then"#,
            r#"if [ -e "{root}/real-root{in:busybox-x86-64}" ] || [ -L "{root}/real-root{in:busybox-x86-64}" ]; then"#,
        ] {
            assert!(
                shape.contains(leg),
                "shape_check no longer proves this against the STAGED tree, and the \
                 scan above only reads real_root_steps: {leg}"
            );
        }
        // The recipe still DECLARES busybox as an input, and must: its own steps
        // run under `busybox sh` (the post-bootstrap tool tier), and shape_check
        // parses both archives with `busybox cpio -t`. A build tool is not an
        // image artifact, and conflating the two is how this test would be
        // "fixed" wrongly the day the input is noticed.
        assert!(
            shape_check().contains("cpio -t"),
            "shape_check no longer parses the archives with the declared build-tool \
             busybox; if that went away the input should have gone with it"
        );
    }

    #[test]
    fn initramfs_packs_the_verified_boot_chain() {
        let selector = build_initramfs_spec("selector-init", Phase::Selector);
        let deployment = build_initramfs_spec("deployment-init", Phase::Deployment);
        for entry in [
            "file {in:td-boot}/bin/td-boot {in:td-boot}/bin/td-boot 0755 0 0",
            "slink /bin/td-boot {in:td-boot}/bin/td-boot 0777 0 0",
            // The shell, in BOTH phases: every line of both /init scripts is
            // interpreted by it, so the `file` entry is as load-bearing as the
            // symlink -- a slink to a member the cpio does not carry is a
            // dangling /bin/sh and a kernel that cannot run /init at all.
            "file {in:td-sh}/bin/td-sh {in:td-sh}/bin/td-sh 0755 0 0",
            "slink /bin/sh {in:td-sh}/bin/td-sh 0777 0 0",
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

    /// The kernel recipe's own source, read so the runtime probes can be tied to the
    /// pins they exist to observe. These are two different files that must agree, and
    /// nothing but a test can make them.
    const LINUX_RECIPE: &str = include_str!("linux-x86-64.rs");

    /// Every kernel symbol APPLICATIONS.md §0 turns on for the application tier, plus
    /// the one it deliberately does NOT turn on by hand.
    ///
    /// Listed here rather than derived from the probe tables so the two can disagree:
    /// a symbol that gains a pin but no probe, or a probe naming a symbol nothing pins,
    /// fails below. What this roster cannot catch is a pin added to the kernel recipe
    /// and to nothing else — the recipe's own `.config` guard is what covers that, and
    /// it fails the producer build rather than this test.
    const SANDBOX_SYMBOLS: [&str; 13] = [
        "CONFIG_USER_NS",
        "CONFIG_PID_NS",
        "CONFIG_UTS_NS",
        "CONFIG_NET_NS",
        "CONFIG_SECCOMP",
        "CONFIG_SECCOMP_FILTER",
        "CONFIG_INOTIFY_USER",
        "CONFIG_CGROUPS",
        "CONFIG_CGROUP_SCHED",
        "CONFIG_FAIR_GROUP_SCHED",
        "CONFIG_CFS_BANDWIDTH",
        "CONFIG_MEMCG",
        "CONFIG_CGROUP_PIDS",
    ];

    /// The symbols this rung pins but cannot witness at runtime, each with the reason.
    ///
    /// An exception list rather than a silent gap: a symbol that is merely missing from
    /// the probes looks identical to one nobody got round to, and this is the difference
    /// between the two.
    const SANDBOX_SYMBOLS_WITHOUT_A_RUNTIME_WITNESS: [(&str, &str); 0] = [];

    /// Each pinned symbol is observed at RUNTIME by a probe that names it, and each
    /// probed symbol is one the kernel recipe actually pins. A pin with no probe ships a
    /// feature nothing on the image ever confirms; a probe with no pin waits for a
    /// feature nobody asked the kernel for.
    ///
    /// The symbol is matched as `({symbol} off` — the diagnostic's own spelling — and
    /// never as a bare substring, because `CONFIG_SECCOMP` occurs inside
    /// `CONFIG_SECCOMP_FILTER`: deleting the whole `Seccomp:` leg would otherwise leave
    /// this green on the strength of the leg below it. Matching the message is only half
    /// the test, and `every_sandbox_probe_runs_a_command_not_just_an_echo` is the other:
    /// a symbol name appears in this string ONLY inside an `echo`, so this assertion
    /// alone is satisfied by a probe whose command tests the wrong path entirely.
    #[test]
    fn every_sandbox_symbol_is_both_pinned_and_probed() {
        let probes = build_sandbox_kernel_probes();
        for symbol in SANDBOX_SYMBOLS {
            let excused = SANDBOX_SYMBOLS_WITHOUT_A_RUNTIME_WITNESS
                .iter()
                .any(|(excused, _)| *excused == symbol);
            assert_eq!(
                probes.contains(&format!("({symbol} off")),
                !excused,
                "{symbol}: a pinned symbol is probed at runtime, or it is on the \
                 exception list with a reason — never neither, and never both"
            );
            assert!(
                LINUX_RECIPE.contains(&format!("grep -q '^{symbol}=y' .config")),
                "the kernel recipe does not guard {symbol} over the RESOLVED config"
            );
        }
        let mut probed: Vec<&str> = SANDBOX_KERNEL_NODES
            .iter()
            .chain(SANDBOX_KERNEL_STATUS_FIELDS.iter())
            .chain(SANDBOX_KERNEL_CONTROLLERS.iter())
            .chain(SANDBOX_KERNEL_CGROUP2_CONTROLLERS.iter())
            .map(|(_, symbol, _)| *symbol)
            .collect();
        probed.extend(
            SANDBOX_KERNEL_CGROUP2_NODES
                .iter()
                .map(|(_, _, symbol, _)| *symbol),
        );
        probed.extend(
            SANDBOX_KERNEL_CGROUP2_ROWS
                .iter()
                .map(|(_, _, _, symbol, _)| *symbol),
        );
        for symbol in &probed {
            assert!(
                SANDBOX_SYMBOLS.contains(symbol),
                "{symbol} is probed but is not one of the symbols §0 pins"
            );
        }
        assert!(
            LINUX_RECIPE.contains("# CONFIG_RT_GROUP_SCHED is not set"),
            "the kernel must keep real-time group scheduling outside the delegated CPU contract"
        );
        assert!(
            LINUX_RECIPE.contains("grep -q '^CONFIG_RT_GROUP_SCHED=y' .config"),
            "the resolved kernel config must reject real-time group scheduling"
        );
    }

    /// Every leg actually TESTS something, and tests the thing its diagnostic names.
    ///
    /// This is the test `every_sandbox_symbol_is_both_pinned_and_probed` cannot be: the
    /// symbol names live only inside `echo` strings, so renaming `/proc/self/ns/user` to
    /// a path that does not exist leaves that one green while the farm asserts nothing
    /// — the probe would simply always fail, which on this image means the boot never
    /// greens and nobody learns why from a unit test. Matching whole invocations rather
    /// than message fragments is the same discipline
    /// `td_txt_probes_survive_the_greeters_quoting` applies next door.
    #[test]
    fn every_sandbox_probe_runs_a_command_not_just_an_echo() {
        let probes = build_sandbox_kernel_probes();
        for (path, _, _) in SANDBOX_KERNEL_NODES {
            assert!(
                probes.contains(&format!("[ -e {path} ] ||")),
                "no leg tests {path} for existence"
            );
        }
        for (field, _, _) in SANDBOX_KERNEL_STATUS_FIELDS {
            assert!(
                probes.contains(&format!("\"^{field}\" /proc/self/status ||")),
                "no leg greps /proc/self/status for {field}"
            );
        }
        for (controller, _, _) in SANDBOX_KERNEL_CONTROLLERS {
            assert!(
                probes.contains(&format!(
                    "\"^{controller}[[:space:]].*[[:space:]]1$\" /proc/cgroups ||"
                )),
                "no leg greps /proc/cgroups for {controller} with its enabled column"
            );
        }
        for (controller, _, _) in SANDBOX_KERNEL_CGROUP2_CONTROLLERS {
            assert!(probes.contains(&format!(
                "\"(^|[[:space:]]){controller}([[:space:]]|$)\" /sys/fs/cgroup/cgroup.controllers ||"
            )));
        }
        for (directory, node, _, _) in SANDBOX_KERNEL_CGROUP2_NODES {
            let path = format!("{directory}/{node}");
            assert!(
                probes.contains(&format!("[ -e {path} ] ||")),
                "no leg tests {path} for existence"
            );
        }
        for (directory, node, row, _, _) in SANDBOX_KERNEL_CGROUP2_ROWS {
            let path = format!("{directory}/{node}");
            assert!(
                probes.contains(&format!(
                    "/bin/grep -Eq \"^{row}[[:space:]][0-9]+$\" {path} ||"
                )),
                "no leg tests {path} for {row}"
            );
        }
        for (limit, clone_flag) in SANDBOX_KERNEL_UCOUNTS {
            assert!(
                probes.contains(&format!("/proc/sys/user/{limit} 2>/dev/null")),
                "no leg reads /proc/sys/user/{limit}"
            );
            assert!(
                probes.contains(&format!("unshare({clone_flag}) fails ENOSPC")),
                "the {limit} leg does not say which unshare a zero would break"
            );
        }
    }

    /// SECCOMP is pinned and SECCOMP_FILTER is NOT, and the asymmetry is the whole
    /// reason this test exists. SECCOMP carries an explicit `prompt` above its
    /// `def_bool y`, so allnoconfig can answer it `n` and a pin is what restores it.
    /// SECCOMP_FILTER has no prompt at all — kconfig computes it from `SECCOMP && NET`
    /// — so a line naming it in the pin list would be silently dropped by olddefconfig
    /// and would read, to anyone auditing the list, as a guarantee that was never made.
    ///
    /// Verified against the pinned linux-7.1.4 by resolving the config: with SECCOMP
    /// pinned and SECCOMP_FILTER absent from the pin list, the resolved `.config`
    /// carries `CONFIG_SECCOMP_FILTER=y`.
    #[test]
    fn seccomp_filter_is_guarded_but_never_pinned() {
        assert!(
            LINUX_RECIPE.contains("'CONFIG_SECCOMP=y'"),
            "SECCOMP is prompted, so it must be pinned or allnoconfig answers it n"
        );
        assert!(
            !LINUX_RECIPE.contains("'CONFIG_SECCOMP_FILTER=y'"),
            "SECCOMP_FILTER must not appear in the pin list: it is unprompted, so the \
             line would be dropped by olddefconfig while looking like a pin that took"
        );
        assert!(
            LINUX_RECIPE.contains("grep -q '^CONFIG_SECCOMP_FILTER=y' .config"),
            "a derived symbol can only be observed after resolution, so it must be guarded"
        );
    }

    /// IPC_NS is refused rather than merely unpinned. It is `default y` behind
    /// `SYSVIPC || POSIX_MQUEUE`, so the same olddefconfig step that handed td `NET_NS`
    /// unasked would hand it an IPC namespace the moment somebody pins SysV IPC for an
    /// unrelated reason — and td-jail omits `CLONE_NEWIPC` on the strength of it being
    /// off. A negative guard makes that a decision instead of a side effect.
    #[test]
    fn ipc_ns_stays_refused_until_someone_argues_for_it() {
        assert!(
            LINUX_RECIPE.contains("if grep -q '^CONFIG_IPC_NS=y' .config; then"),
            "IPC_NS must be guarded OFF, not just left out of the pin list"
        );
        for symbol in ["CONFIG_IPC_NS", "CONFIG_SYSVIPC", "CONFIG_FUSE_FS"] {
            assert!(
                !LINUX_RECIPE.contains(&format!("'{symbol}=y'")),
                "{symbol} is deferred by APPLICATIONS.md §0 and must not be pinned on"
            );
        }
    }

    /// Every probe leg can clear the flag, and the farm is quoted so the greeter's
    /// `su -c '…'` survives it — `td_txt_probes_survive_the_greeters_quoting`'s argument,
    /// which applies verbatim to any farm added beside it.
    #[test]
    fn sandbox_kernel_probes_can_fail_and_survive_the_greeters_quoting() {
        let probes = build_sandbox_kernel_probes();
        assert!(
            !probes.contains('\''),
            "a single quote in the kernel probes would close the greeter's `su -c` \
             argument: {probes}"
        );
        assert!(
            build_bootsuccess(&SYSTEM).contains(&format!("-c '{probes}")),
            "the greeter must run the kernel probes verbatim inside its `su -c` argument"
        );
        // One `k=0` per table entry, and THREE per ucount limit — unreadable, not a
        // number, zero — since each is a distinct way for one file to fail to answer.
        // A leg that cannot clear `k` reports success whatever the kernel did, which is
        // the failure this farm exists to make impossible.
        let legs = SANDBOX_KERNEL_NODES.len()
            + SANDBOX_KERNEL_STATUS_FIELDS.len()
            + SANDBOX_KERNEL_CONTROLLERS.len()
            + SANDBOX_KERNEL_CGROUP2_CONTROLLERS.len()
            + SANDBOX_KERNEL_CGROUP2_NODES.len()
            + SANDBOX_KERNEL_CGROUP2_ROWS.len()
            + SANDBOX_KERNEL_UCOUNTS.len() * 3;
        assert_eq!(
            probes.matches("k=0").count(),
            legs,
            "each probe leg needs its own `k=0'"
        );
        assert!(
            build_bootsuccess(&SYSTEM).contains(&format!("echo {TD_SANDBOX_KERNEL_MARKER}")),
            "the greeter must print the sandbox-kernel marker the boot oracle waits for"
        );
    }

    #[test]
    fn td_jail_is_packed_and_its_target_probe_gates_boot_success() {
        let steps = real_root_steps(&SYSTEM);
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyTree { from, dest }
                if from == "{in:td-jail}" && dest == "{root}/real-root{in:td-jail}"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Symlink { target, link }
                if target == "{in:td-jail}/bin/td-jail"
                    && link == "{root}/real-root/bin/td-jail"
        )));
        assert!(recipe()
            .native_inputs
            .as_ref()
            .is_some_and(|inputs| inputs.contains(&"td-jail".to_string())));

        let bootsuccess = build_bootsuccess(&SYSTEM);
        let probe = bootsuccess
            .find("/bin/td-jail --probe-transition")
            .expect("target transition probe missing");
        let leaked_fd = bootsuccess
            .find("TD_JAIL_TEST_LEAK_FD=1 /bin/td-jail")
            .expect("target transition descriptor-leak control missing");
        let gate = bootsuccess
            .find("[ \"$mtj\" = 1 ] || healthy=0")
            .expect("target transition health gate missing");
        let seccomp = bootsuccess
            .find("/run/td-jail-seccomp-probe/probe ")
            .expect("target seccomp behavior probe missing");
        let seccomp_gate = bootsuccess
            .find("[ \"$mts\" = 1 ] || healthy=0")
            .expect("target seccomp behavior health gate missing");
        assert!(
            !bootsuccess.contains("probe failed: $p")
                && !bootsuccess.contains("unexpected output: $p"),
            "a failing target probe must not reflect a success marker into console evidence"
        );
        assert!(
            !bootsuccess.contains(TD_FIREFOX_BOOT_MARKER),
            "mutable application state must not control deployment boot success"
        );
        assert!(
            !bootsuccess.contains(TD_FIREFOX_CONTENT_MARKER),
            "browser content evidence must not control deployment boot success"
        );
        assert!(
            !bootsuccess.contains(TD_FIREFOX_SUPPORT_MARKER),
            "browser support evidence must not control deployment boot success"
        );
        let services = build_td_svc_conf();
        let evidence_probe = services
            .find(&format!(
                "/bin/td-compositor probe-application {FIREFOX_WINDOW_READY_SOCKET} \
                 {FIREFOX_APP_ID} {FIREFOX_CONTENT_RGB_A} \
                 {FIREFOX_CONTENT_RGB_B} 2>/dev/null"
            ))
            .expect("Firefox content evidence probe missing");
        let process_probe = services
            .find(&format!(
                "/bin/td-jail --probe-process-token {FIREFOX_NAME} -contentproc 2>/dev/null"
            ))
            .expect("Firefox content-process evidence probe missing");
        let firefox_marker = services
            .find(&format!("/bin/echo {TD_FIREFOX_BOOT_MARKER}"))
            .expect("Firefox evidence marker missing");
        let content_marker = services
            .find(&format!("/bin/echo {TD_FIREFOX_CONTENT_MARKER}"))
            .expect("Firefox content marker missing");
        let evidence_publish = services
            .find(&format!(
                "/bin/mv {FIREFOX_EVIDENCE_TMP_PATH} {FIREFOX_EVIDENCE_PATH}"
            ))
            .expect("Firefox evidence publication missing");
        let completion_publish = services
            .find(&format!(
                "/bin/mv {FIREFOX_COMPLETION_TMP_PATH} {FIREFOX_COMPLETION_PATH}"
            ))
            .expect("Firefox evidence completion publication missing");
        let success = bootsuccess
            .find(&format!("echo {SYSTEM_BOOT_SUCCESS_MARKER}"))
            .expect("boot success marker missing");
        assert!(
            leaked_fd < probe
                && probe < seccomp
                && seccomp < gate
                && gate < seccomp_gate
                && seccomp_gate < success
                && evidence_probe < process_probe
                && process_probe < evidence_publish
                && evidence_publish < firefox_marker
                && firefox_marker < content_marker
                && content_marker < completion_publish,
            "target transition must close a leaked descriptor, run the optional target filter \
             oracle, gate boot success, and keep Firefox evidence independent"
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
        // No subcommand gets a /bin name: none is an applet, and a farm-less /bin
        // entry is one no list in this file accounts for and no shape check verifies.
        // Roster hygiene rather than a boundary — `/bin/td-login` is a shipped symlink,
        // so the subcommand is reachable either way; `creds::may_switch` is the gate.
        for subcommand in ["verify-credentials", "exec-as", "exec-service-as"] {
            assert!(
                !packed_bin_names().iter().any(|n| n == subcommand),
                "{subcommand} is a subcommand, not an applet; it must not be packed into /bin"
            );
        }

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
            service_only: false,
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
        // Both halves gate the ONE marker: the unprivileged readback through `su`, and
        // the root-side `exec-as` that starts a session of its own. They are `&&`ed
        // rather than given a marker each because they prove the same thing — that this
        // crate's credential switch produced the credentials it named — through the two
        // front ends a supervised image actually uses.
        assert!(
            bootsuccess.contains(&format!(
                "[ \"$l\" = 1 ]' && {}; then [ \"$mtl\" = 1 ] || {{ echo \
                 {TD_LOGIN_RUNTIME_MARKER}; mtl=1; }}; else healthy=0; fi",
                td_login_exec_as_probe(&SYSTEM)
            )),
            "the health target must emit the td-login marker from that leg alone, so an \
             absent TD-LOGIN-RUN-OK names the credential switch rather than some component \
             upstream of it"
        );
        // The fixture uses `exec-as`, while this independent health leg verifies
        // its credential readback. Its ROOT placement is already pinned by the
        // whole-leg assertion above, so nothing here repeats that.
        let exec_as = td_login_exec_as_probe(&SYSTEM);
        assert!(
            exec_as.contains("/bin/td-login exec-as tester -- /bin/td-login verify-credentials"),
            "the exec-as leg must point exec-as at the readback, so the process reporting \
             the switch is the one exec-as started: {exec_as}"
        );
        // The empty supplementary set, for `td_login_probe`'s reason and against this
        // probe's own independent copy of the formatting: an unquoted empty value
        // vanishes from the argv, leaving `--groups` with no argument, and rung 5's
        // `audio` uid is exactly the account that will have none.
        assert!(
            td_login_exec_as_probe(&lone).contains("--groups \"\" ||"),
            "exec-as must quote an empty group list too: {}",
            td_login_exec_as_probe(&lone)
        );
        // Fail-closed, asserted on the DIAGNOSTIC rather than on `false`: the success
        // path ends `…; false; }; }` too, so a `contains("false")` here would be
        // satisfied by either branch and would green a probe that resolved a
        // non-existent user to the ordinary text.
        let unresolvable = td_login_exec_as_probe(&SystemDef {
            autologin: "nobody-here",
            ..SYSTEM
        });
        assert!(
            unresolvable.contains("no autologin user to exec-as")
                && !unresolvable.contains("exec-as nobody-here"),
            "an unresolvable autologin user must yield a failing exec-as leg that says \
             so, not an empty one and not an ordinary one: {unresolvable}"
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

    /// td-sh must be PACKED into the real root, not merely symlinked and listed
    /// as an input — the same rent td-svc pays below, for the same reason.
    ///
    /// This one is the shell, so the blast radius is everything: with the
    /// `CopyTree` gone the real root's `/bin/sh` is a symlink to nothing, which
    /// takes out getty -> login -> the operator's session, `/etc/rootcheck`,
    /// `/etc/bootsuccess`, `/etc/autologin` and every td-svc job, since each is
    /// a script this binary interprets. Confirmed by deleting the step: the
    /// whole host suite stayed green, and only `shape_check` — the last of 146
    /// steps, in the recipe-checks tier — would have said so. That is precisely
    /// the failure class this commit closed for the cpio half, left open for
    /// the real-root half of its own new binary.
    #[test]
    fn td_sh_is_packed_and_not_merely_symlinked() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-sh}" && dest == "{root}/real-root{in:td-sh}"
            )),
            "td-sh must be CopyTree'd into the real root (static, empty closure) - a \
             symlink alone dangles on the image and /bin/sh interprets every script \
             the boot runs"
        );
        // Both names, because `ash` is not decoration: a script spelling it must
        // reach the same binary `sh` does or the two are a difference nothing
        // tests, and the farm is what makes them one.
        for applet in TD_SH_APPLETS {
            assert!(
                steps.iter().any(|s| matches!(
                    s,
                    Step::Symlink { target, link }
                        if target == "{in:td-sh}/bin/td-sh"
                            && link == &format!("{{root}}/real-root/bin/{applet}")
                )),
                "/bin/{applet} must symlink into the store td-sh package"
            );
        }
        let native_inputs = recipe().native_inputs.expect("system native inputs");
        assert!(
            native_inputs.iter().any(|i| i == "td-sh"),
            "td-sh must be a declared native input, or {{in:td-sh}} does not resolve"
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

    /// td-busd must be PACKED, not merely symlinked and listed as an input.
    ///
    /// The same rent td-svc pays above, for the same reason and with one extra
    /// one. Being a recipe INPUT makes `{in:td-busd}` resolve at build time and
    /// puts nothing on the image; only a CopyTree (static) or a
    /// StageRuntimeClosure (dynamic) does that. So a symlink alone resolves in
    /// the recipe text and dangles on the erofs root.
    ///
    /// The extra reason is that this one would be quiet. td-svc dangling means
    /// PID 1 execs nothing and the machine has no userland; a dangling
    /// /bin/td-busd means one daemon fails to start, td-svc restarts it for
    /// ever, and the only thing that notices is the health probe — which runs
    /// in `shape_check`'s tier, needs the loop toolchain, and so is exactly the
    /// tier a host-side change cannot reach. This test is what makes the
    /// mistake visible where the change is made.
    #[test]
    fn td_busd_is_packed_and_not_merely_symlinked() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::CopyTree { from, dest }
                    if from == "{in:td-busd}" && dest == "{root}/real-root{in:td-busd}"
            )),
            "td-busd must be CopyTree'd into the real root (static, empty closure) - a \
             symlink alone dangles on the image and the busd unit execs nothing"
        );
        assert!(
            steps.iter().any(|s| matches!(
                s,
                Step::Symlink { target, link }
                    if target == "{in:td-busd}/bin/td-busd"
                        && link == "{root}/real-root/bin/td-busd"
            )),
            "/bin/td-busd must symlink into the store td-busd package - the busd \
             unit names it in full, in both its exec and its ready line"
        );
        let native_inputs = recipe().native_inputs.expect("system native inputs");
        assert!(
            native_inputs.iter().any(|i| i == "td-busd"),
            "td-busd must be a declared native input, or {{in:td-busd}} does not resolve"
        );
    }

    #[test]
    fn td_portal_is_packed_and_not_merely_symlinked() {
        let steps = real_root_steps(&SYSTEM);
        assert!(
            steps.iter().any(|step| matches!(
                step,
                Step::CopyTree { from, dest }
                    if from == "{in:td-portal}"
                        && dest == "{root}/real-root{in:td-portal}"
            )),
            "td-portal must be CopyTree'd into the immutable root"
        );
        assert!(
            steps.iter().any(|step| matches!(
                step,
                Step::Symlink { target, link }
                    if target == "{in:td-portal}/bin/td-portal"
                        && link == "{root}/real-root/bin/td-portal"
            )),
            "/bin/td-portal must name the staged static package"
        );
        let native_inputs = recipe().native_inputs.expect("system native inputs");
        assert!(
            native_inputs.iter().any(|input| input == "td-portal"),
            "td-portal must be a declared native input"
        );
    }
}
