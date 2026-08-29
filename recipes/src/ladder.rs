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

pub const TD_APPLICATION_PACKAGE_ROOT: &str = "/td/store";
pub const TD_APPLICATION_STATE_ROOT: &str = ".td/app";
pub const TD_APPLICATION_RUNTIME_ROOT: &str = "td-app";
pub const TD_APPLICATION_CONFIG_PATH: &str = "/etc/td-app.conf";
pub const TD_APPLICATION_REGISTRY: &str = "/etc/td-applications.tsv";
pub const TD_APPLICATION_LAUNCHER_TABLE: &str = "/etc/td-launcher.tsv";
pub const TD_APPLICATION_CGROUP_ROOT: &str = "/sys/fs/cgroup/td-user-1000";
pub const TD_APPLICATION_CGROUP_SESSION: &str =
    "/sys/fs/cgroup/td-user-1000/session";
pub const TD_APPLICATION_CGROUP_MEMBERSHIP_ROOT: &str = "/td-user-1000";
pub const TD_JAIL_FIXTURE_NAME: &str = "td-jail-fixture";
pub const TD_JAIL_FIXTURE_ENTRY: &str = "/app/bin/td-compositor";
pub const TD_JAIL_FIXTURE_ALIAS: &str = "org.td.JailFixture";
pub const TD_JAIL_FIXTURE_DISPLAY_NAME: &str = "Jail Fixture";
pub const TD_JAIL_FIXTURE_DOWNLOAD_PERMISSION: &str = "xdg-download";
pub const TD_JAIL_FIXTURE_DOWNLOAD_TARGET: &str = "/home/td/Downloads";
pub const TD_JAIL_FIXTURE_PICTURES_PERMISSION: &str = "xdg-pictures";
pub const TD_JAIL_FIXTURE_PICTURES_TARGET: &str = "/home/td/Pictures";
pub const TD_JAIL_FIXTURE_GRANT_FILE: &str = "/var/td-jail-fixture-file";
pub const TD_JAIL_FIXTURE_GRANT_ROOT: &str = "/mnt/td-jail-fixture-pictures";
pub const TD_JAIL_FIXTURE_SEARCH_TERMS: &[&str] =
    &["jail", "fixture", "sandbox", "wayland"];
pub const TD_APPLICATION_CONFIG_TEXT: &str = concat!(
    "format=1\n",
    "package-root=/td/store\n",
    "state-root=.td/app\n",
    "registry=/etc/td-applications.tsv\n",
    "launcher-table=/etc/td-launcher.tsv\n",
    "cgroup-root=/sys/fs/cgroup/td-user-1000\n",
);

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

/// The tool-farm step that symlinks a prior binutils rung's executables into
/// `{tools}` (as/ld/ar/ranlib/nm/strip/…). These names are explicit because a
/// command glob may not enumerate a staged input (APPLICATIONS.md section B.8).
#[derive(Clone, Copy)]
pub enum BinutilsRung {
    Mesboot0,
    Mesboot1,
    Mesboot,
    V244,
}

impl BinutilsRung {
    fn catalog_stem(self) -> &'static str {
        match self {
            BinutilsRung::Mesboot0 => "binutils-mesboot0",
            BinutilsRung::Mesboot1 => "binutils-mesboot1",
            BinutilsRung::Mesboot => "binutils-mesboot",
            BinutilsRung::V244 => "binutils-244",
        }
    }
}

pub fn link_bins(rung: BinutilsRung) -> Step {
    let mut names = vec![
        "addr2line",
        "ar",
        "as",
        "c++filt",
        "gprof",
        "ld",
        "nm",
        "objcopy",
        "objdump",
        "ranlib",
        "readelf",
        "size",
        "strings",
        "strip",
    ];
    if matches!(rung, BinutilsRung::V244) {
        names.extend(["elfedit", "ld.bfd"]);
    }
    let binutils_rung = rung.catalog_stem();
    Step::ToolFarm {
        links: names
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    format!("{{in:{binutils_rung}}}/bin/{name}"),
                )
            })
            .collect(),
    }
}

/// The declared shell (the sandbox has no /bin/sh): the td-built `bash-mesboot`
/// output, not a host bash.
pub const SH: &str = "{in:bash-mesboot}/bin/bash";

/// One target-only direct-rustc invocation. The shared arguments follow every
/// recipe's functional arguments so a local `-C` option cannot quietly override
/// the target-wide frame/debug/build-ID policy.
pub fn target_rustc(dir: &str, rustc: &str, args: &[&str]) -> Step {
    target_rustc_at_roots(dir, rustc, args, "{root}", "{src}")
}

/// Direct rustc with explicit ephemeral roots. This exists for the bounded
/// two-root reproducibility oracle; shipped recipes otherwise use
/// `target_rustc`, whose roots are the standard build templates.
pub fn target_rustc_at_roots(
    dir: &str,
    rustc: &str,
    args: &[&str],
    build_root: &str,
    source_root: &str,
) -> Step {
    let mut argv =
        Vec::with_capacity(1 + td_engine::target_profile::DIRECT_RUSTC_ARGS.len() + args.len());
    argv.push(rustc.to_string());
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    argv.extend(
        td_engine::target_profile::direct_rustc_args(build_root, source_root),
    );
    Step::Run {
        argv,
        env: Vec::new(),
        dir: dir.into(),
    }
}

/// Split every installed runtime ELF in a direct td recipe with the final
/// source-built binutils. Keeping this adjacent to `target_rustc` makes the
/// compile and post-link halves of the global policy one reusable path.
pub fn split_target_debug(root: &str) -> Step {
    Step::split_debug_tree(root, "{in:binutils-x86-64-self}/bin/objcopy")
}

const DEBUG_LINE_AWK: &str = r#"
function under(path, prefix) {
  return path == prefix || index(path, prefix "/") == 1
}
function allowed(path) {
  return under(path, "/td-build") || under(path, "/td-build-root") ||
         under(path, "/td-cargo") || under(path, "/td/store")
}
function reject(reason, value) {
  print "debug line-table " reason ": " value > "/dev/stderr"
  bad=1
}
function clear_dirs(key) {
  for (key in dirs) delete dirs[key]
}
function normalize(path, parts, stack, count, top, i, part, result) {
  count=split(path, parts, "/")
  top=0
  for (i=1; i<=count; i++) {
    part=parts[i]
    if (part == "" || part == ".") continue
    if (part == "..") {
      if (top == 0) return ""
      delete stack[top]
      top--
      continue
    }
    stack[++top]=part
  }
  result="/"
  for (i=1; i<=top; i++) {
    if (i > 1) result=result "/"
    result=result stack[i]
  }
  return result
}
function table_value(line, value) {
  value=line
  if (match(value, /\):[[:space:]]/))
    return substr(value, RSTART + RLENGTH)
  if (table == "directory")
    sub(/^[[:space:]]*[0-9]+[[:space:]]+/, "", value)
  else
    sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+/, "", value)
  return value
}
function resolve(base, path) {
  if (substr(path, 1, 1) == "/") return normalize(path)
  return normalize(base "/" path)
}
function record_path(raw, base, resolved) {
  resolved=resolve(base, raw)
  if (resolved == "") reject("escapes its source root", raw)
  else if (!allowed(resolved))
    reject("resolves outside the stable roots", resolved)
  return resolved
}
/^[[:space:]]*Offset:/ {
  clear_dirs()
  table=""
  version=0
  next
}
/DWARF Version:/ {
  version=$NF + 0
  next
}
/Directory Table/ {
  if (version != 5) reject("uses unsupported DWARF version", version)
  table="directory"
  next
}
/File Name Table/ {
  table="file"
  next
}
/Line Number Statements/ {
  table=""
  next
}
index($0, "define new File Table entry") {
  reject("uses a dynamic file definition", $0)
}
index($0, "guix-build") {
  reject("retains build scratch text", $0)
}
table == "directory" && $1 ~ /^[0-9]+$/ {
  raw=table_value($0)
  if ($1 == 0 && substr(raw, 1, 1) != "/")
    reject("has a non-absolute compilation directory", raw)
  if ($1 != 0 && !(0 in dirs))
    reject("precedes its compilation directory", raw)
  base=$1 == 0 ? "/" : dirs[0]
  dirs[$1]=record_path(raw, base)
  next
}
table == "file" && $1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ {
  raw=table_value($0)
  if (!($2 in dirs)) {
    reject("uses an unknown directory index", $2)
    next
  }
  resolved=record_path(raw, dirs[$2])
  if (raw == source) {
    if (under(resolved, root)) seen=1
    else reject("places " source " outside " root, resolved)
  }
  next
}
END {
  if (!seen) reject("does not place " source " below " root, source)
  if (bad) exit 1
}
"#;

fn debug_line_validation_command(producer: &str, source: &str, source_root: &str) -> String {
    format!(
        "{producer} | awk -v source='{source}' -v root='{source_root}' '{DEBUG_LINE_AWK}'"
    )
}

/// Validate one retained DWARF-5 source file against the line-table-only
/// companion policy. Every directory and file entry resolves below a declared
/// stable root; the named source must resolve below `source_root`.
pub fn debug_line_source_root_check(
    readelf: &str,
    debug: &str,
    source: &str,
    source_root: &str,
) -> Step {
    let producer = format!("'{readelf}' --debug-dump=rawline '{debug}' 2>/dev/null");
    let command = debug_line_validation_command(&producer, source, source_root);
    Step::run("{root}", &[POST_BOOTSTRAP_SH, "-c", &command]).env("PATH", &post_bootstrap_path())
}

/// Execute the shared parser against hostile captured-table shapes. This is a
/// recipe step so the oracle uses the same declared BusyBox awk as production.
pub fn debug_line_validator_regression_steps() -> Vec<Step> {
    const GOOD: &str = r#"  Offset: 0
  DWARF Version: 5
 The Directory Table
  Entry Name
  0 (indirect line string, offset: 0): /td-build
  1 (indirect line string, offset: 1): ./src/..
  2 (indirect line string, offset: 2): /td/store/include
 The File Name Table
  Entry Dir Name
  0 1 (indirect line string, offset: 3): main.c
  1 2 (indirect line string, offset: 4): stdio.h
 Line Number Statements:
"#;
    const WRONG_DIRECTORY: &str = r#"  Offset: 0
  DWARF Version: 5
 The Directory Table
  Entry Name
  0 (indirect line string, offset: 0): /td-build
  1 (indirect line string, offset: 1): /td-cargo/wrong
 The File Name Table
  Entry Dir Name
  0 1 (indirect line string, offset: 2): main.c
 Line Number Statements:
"#;
    const TRAVERSAL: &str = r#"  Offset: 0
  DWARF Version: 5
 The Directory Table
  Entry Name
  0 (indirect line string, offset: 0): /td-build/../../home/leak
 The File Name Table
  Entry Dir Name
  0 0 (indirect line string, offset: 1): main.c
 Line Number Statements:
"#;
    const SPACED_PATH: &str = r#"  Offset: 0
  DWARF Version: 5
 The Directory Table
  Entry Name
  0 (indirect line string, offset: 0): /td-build
  1 (indirect line string, offset: 1): /home/user name
 The File Name Table
  Entry Dir Name
  0 0 (indirect line string, offset: 2): main.c
 Line Number Statements:
"#;
    const DWARF4: &str = r#"  Offset: 0
  DWARF Version: 4
 The Directory Table
  Entry Name
  1 /td-build
 The File Name Table
  Entry Dir Time Size Name
  1 1 0 0 main.c
 Line Number Statements:
"#;
    const STATEMENT_SCRATCH: &str = r#"  Offset: 0
  DWARF Version: 5
 The Directory Table
  Entry Name
  0 (indirect line string, offset: 0): /td-build
 The File Name Table
  Entry Dir Name
  0 0 (indirect line string, offset: 1): main.c
 Line Number Statements:
  define new File Table entry: /tmp/guix-build-leak/x.c
"#;
    const DYNAMIC_OUTSIDE: &str = r#"  Offset: 0
  DWARF Version: 5
 The Directory Table
  Entry Name
  0 (indirect line string, offset: 0): /td-build
 The File Name Table
  Entry Dir Name
  0 0 (indirect line string, offset: 1): main.c
 Line Number Statements:
  define new File Table entry: /home/user name/x.c
"#;

    fn producer(fixture: &str) -> String {
        format!("printf '%s' '{fixture}'")
    }

    fn fixture_command(fixture: &str) -> String {
        format!(
            "{} | awk -v source='main.c' -v root='/td-build' \
             -f '{{root}}/debug-line-validator.awk'",
            producer(fixture)
        )
    }

    let good = fixture_command(GOOD);
    let wrong = fixture_command(WRONG_DIRECTORY);
    let traversal = fixture_command(TRAVERSAL);
    let spaced = fixture_command(SPACED_PATH);
    let dwarf4 = fixture_command(DWARF4);
    let statement_scratch = fixture_command(STATEMENT_SCRATCH);
    let dynamic_outside = fixture_command(DYNAMIC_OUTSIDE);
    let command = format!(
        "{good} || {{ echo 'canonical rawline fixture was rejected' >&2; exit 1; }}; \
         if {wrong} 2>/dev/null; then echo 'wrong-directory rawline fixture passed' >&2; exit 1; fi; \
         if {traversal} 2>/dev/null; then echo 'traversal rawline fixture passed' >&2; exit 1; fi; \
         if {spaced} 2>/dev/null; then echo 'spaced-path rawline fixture passed' >&2; exit 1; fi; \
         if {dwarf4} 2>/dev/null; then echo 'DWARF-4 rawline fixture passed' >&2; exit 1; fi; \
         if {statement_scratch} 2>/dev/null; then echo 'statement-scratch rawline fixture passed' >&2; exit 1; fi; \
         if {dynamic_outside} 2>/dev/null; then echo 'dynamic-file rawline fixture passed' >&2; exit 1; fi"
    );
    vec![
        Step::WriteFile {
            path: "{root}/debug-line-validator.awk".into(),
            content: DEBUG_LINE_AWK.into(),
            exec: false,
        },
        Step::run("{root}", &[POST_BOOTSTRAP_SH, "-c", &command])
            .env("PATH", &post_bootstrap_path()),
    ]
}

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
///
/// Raised again, for the same reason and by the same route: the session-bus probe is
/// an eighth `su` block in the health farm, and it is the only one with a bounded
/// wait of its own — `td-busd probe` allows five seconds for an `OK` line, which a
/// broker that is WEDGED rather than absent will spend in full. That went into the
/// guest's per-iteration budget, and this follows it.
///
/// Raised again when Git added a ninth, process-heavy health block. The initial
/// profiler evidence is serialized ahead of that workload so its bounded capture
/// stays deterministic. The host must outlast the serial identity/root checks, the
/// slower of that 315-second service and the 700-second network service, then the
/// clamped 760-second health loop and the diagnostic margin. The tenth Codex block
/// restores the former 70-second host margin at the value below.
///
/// The Firefox download proof adds a 40-second one-shot browser boundary and a
/// separate 20-second asynchronous file-observation window. Raising this by 45
/// seconds keeps the host beyond both guest bounds and their diagnostic margin.
pub const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 2055;
pub const QEMU_GUEST_WAIT_MARGIN_SECS: u64 = 30;

/// The source release identities and exact `--version` output shared by the
/// package checks and the deployed health probe. Keeping these beside the boot
/// contract prevents a package update from leaving a stale image assertion.
pub const CODEX_RECIPE_VERSION: &str = "0.148.0";
pub const CODEX_VERSION_OUTPUT: &str = "codex-cli 0.148.0";
pub const CODEX_BWRAP_UPSTREAM_VERSION: &str = "0.11.2";
pub const CODEX_BWRAP_RECIPE_VERSION: &str = "0.11.2-codex-0.148.0";
pub const CODEX_BWRAP_VERSION_OUTPUT: &str = "bubblewrap 0.11.2";

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

/// Printed after the root-owned target passes immutable-state checks plus every
/// unprivileged shipped-userland runtime probe, then td-boot records or confirms
/// the deployment successful.
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

/// Printed by the root-owned health target only after the unprivileged user can
/// initialize a bare repository, clone it, commit and push to it, then clone it
/// again and verify the resulting history. The same leg requires a pinned CA
/// bundle at the image's conventional path. Verified use of that bundle and Git's
/// compiled HTTPS helper is the operator-only `GIT_HTTPS_RUNTIME_MARKER` below.
pub const GIT_RUNTIME_MARKER: &str = "TD-GIT-RUN-OK";

/// Printed by the root-owned health target only after the unprivileged user can
/// execute the installed source-built Codex CLI and its source-built Bubblewrap
/// helper by their `/bin` names, both report their exact pinned versions, and a
/// Codex read-only sandbox enters a distinct network namespace while refusing a
/// fixture write without changing or hiding the fixture.
pub const CODEX_RUNTIME_MARKER: &str = "TD-CODEX-BWRAP-RUN-OK";

/// Printed by the root-owned health target only after the unprivileged OpenSSH client
/// authenticates to the running OpenSSH daemon on loopback with an ephemeral Ed25519 key
/// and executes a command using the image's exact modern-only algorithm policy. The Git
/// leg independently clones, pushes, and reclones over that same SSH transport. Together
/// they prove the kernel loopback path, client/server protocol, split daemon helpers,
/// public-key authentication, remote exec, and the libcrypto-free runtime closure.
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
///
/// It now gates on `exec-as` as well, and that half needed a leg of its own for a
/// reason worth recording: `exec-as` is the front end a SUPERVISOR uses, so it runs
/// as root and drops, where every other unprivileged health leg has already dropped
/// by the time it runs. A copy inside the greeter's `su` would fail `setgroups(2)`
/// with EPERM and prove nothing. Both legs point at the same readback, so the marker
/// still means one thing — this crate's credential switch produced the credentials
/// it named — proven now through both front ends the image has rather than one.
pub const TD_LOGIN_RUNTIME_MARKER: &str = "TD-LOGIN-RUN-OK";

/// Printed by `/etc/bootsuccess` only after the session bus answered a real
/// client on the real socket: `td-busd probe`, as the unprivileged login user,
/// connects to `/run/user/1000/bus`, completes `AUTH EXTERNAL` under the uid the
/// kernel reports for it, and reads back a well-formed `OK <guid>` line.
///
/// This is evidence for the DAEMON, which the broker's own tests are not: those
/// bind a socket in a temporary directory and probe it inside one process, so
/// they hold up the transport and say nothing about the unit. What only the image
/// can show is that the socket the unit names is reachable, as the login user, in
/// the `/run/user/1000` td-seatd made, by a SEPARATE process.
///
/// Said that way deliberately. The probe checks a PATH, not a pid: it has no
/// association with the unit's process or generation, so what it strictly proves
/// is that SOMETHING there completed the handshake. Nothing else on this image
/// binds that path, which is why the marker is worth having — but the difference
/// matters the moment something else could, and calling this "the broker this unit
/// started" would be claiming a link the probe does not check.
///
/// It stops at the handshake, which is what `probe` is. `Hello`, the unique
/// names, the bus's own interface and directed routing are held up host-side and
/// against sd-bus, libdbus and GDBus; the separate portal marker now holds up
/// one live routed client exchange on the target image. Recording the boundary
/// is the point: this marker alone means the bus is reachable, not that anything
/// has been routed over it.
pub const TD_BUSD_RUNTIME_MARKER: &str = "TD-BUSD-RUN-OK";

/// Printed by a separate unprivileged client only after the supervised portal
/// owns its reserved public name, answers the Settings version property, and
/// returns the exact two-namespace dictionary compiled from the immutable
/// session settings file.
///
/// DUPLICATED as `READY_MARKER` in td-portal/src/main.rs. The td-portal recipe
/// pins the literal and the call that emits it. The host recognizes the exact
/// `portal-evidence: <marker>` line after td-svc applies its trusted service
/// prefix, not this unframed substring.
pub const TD_PORTAL_RUNTIME_MARKER: &str =
    "TD-PORTAL-READY namespaces=2 settings=10 version=1";

/// Printed by that same live client only after it pre-subscribes to the exact
/// caller-derived path, receives the Background method reply carrying that
/// path, and then receives the directed policy-denial Request.Response.
///
/// DUPLICATED as `REQUEST_READY_MARKER` in td-portal/src/main.rs and pinned by
/// the td-portal recipe.
pub const TD_PORTAL_REQUEST_RUNTIME_MARKER: &str =
    "TD-PORTAL-REQUEST-READY response=2";

/// Printed by a separate unprivileged client only after the compositor's
/// private portal socket returns the exact public registry and does not
/// advertise the privileged manager before that manager is implemented.
///
/// DUPLICATED as `READY_MARKER` in td-portal/src/wayland_channel.rs and pinned
/// by the td-portal recipe.
pub const TD_PORTAL_CHANNEL_RUNTIME_MARKER: &str =
    "TD-PORTAL-CHANNEL-READY globals=10 privileged=0";

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
/// a real limit rather than a preference. Those two calls belong to surface #9;
/// inventing a prober for this rung would mean an `unsafe` surface added outside
/// the crate that owns it. So this asserts the kernel is CAPABLE; the functional
/// `unshare` probe lands with td-jail's transition rung and the filter-install
/// probe with its seccomp rung, where each syscall joins the roster. The gap is
/// narrow: `/proc/self/ns/user` exists if
/// and only if `CONFIG_USER_NS`, and `Seccomp_filters:` appears in
/// `/proc/self/status` if and only if `CONFIG_SECCOMP_FILTER`, so what is
/// unproven here is the sysctl and LSM policy around those calls, not the
/// features themselves. `/proc/sys/user/max_user_namespaces` covers the one
/// sysctl that can turn a compiled-in USER_NS into an EPERM.
pub const TD_SANDBOX_KERNEL_MARKER: &str = "TD-SANDBOX-KERNEL-OK";

/// Emitted by td-jail stage 1 only after its child is PID 1 in the fresh namespace,
/// both identity maps read back exactly, every capability is removed, no-new-privileges
/// and the compiled filter read back installed, and PID 1 reaps a filtered descendant.
pub const TD_JAIL_TRANSITION_MARKER: &str = "TD-JAIL-TRANSITION-OK";

/// Emitted only by the QEMU fixture after its non-shipped td-GCC probe installs
/// td-jail's exported filter and observes the compiled errno and kill actions.
pub const TD_JAIL_SECCOMP_PROBE_MARKER: &str = "TD-JAIL-SECCOMP-PROBE-OK";

/// Compatibility marker emitted by the trusted Firefox-evidence unit after
/// the stronger HTTPS-content, live-process and resource-cap proof below.
pub const TD_FIREFOX_BOOT_MARKER: &str = "TD-FIREFOX-FIRST-WINDOW-READY";

/// Emitted by the trusted Firefox-evidence unit only after the compositor has
/// observed the verified in-guest HTTPS document's exact content-pixel region
/// in a painted frame, returned bounded client-resource high-water marks, and
/// td-jail has found a live process with Firefox's exact `-contentproc` argv
/// token in the same application cgroup.
pub const TD_FIREFOX_CONTENT_MARKER: &str = "TD-FIREFOX-HTTPS-CONTENT-READY";

/// Emitted only after Firefox's own privileged support snapshot reports the
/// pinned Wayland/software renderer and fallback sandbox, while every reported
/// live content, GPU, socket and media-role process retains a nested filter.
pub const TD_FIREFOX_SUPPORT_MARKER: &str = "TD-FIREFOX-SUPPORT-READY";

/// Staged markers emitted by td-jail's bounded Firefox physical-input probe.
pub const TD_FIREFOX_INPUT_ARMED_MARKER: &str = "TD-FIREFOX-INPUT-ARMED";
pub const TD_FIREFOX_INPUT_MENU_MARKER: &str = "TD-FIREFOX-INPUT-MENU";
pub const TD_FIREFOX_INPUT_MARKER: &str = "TD-FIREFOX-INPUT-OK";
pub const TD_TERM_CLIPBOARD_FOCUS_PREFIX: &str = "TD-TERM-CLIPBOARD-FOCUS-READY serial=";
pub const TD_TERM_CLIPBOARD_TARGET_PREFIX: &str = "TD-TERM-CLIPBOARD-TARGET-READY ";
pub const TD_TERM_CLIPBOARD_SELECTION_MARKER: &str =
    "TD-TERM-CLIPBOARD-SELECTION-READY bytes=7";
pub const TD_TERM_CLIPBOARD_MARKER: &str = "TD-TERM-CLIPBOARD-READY bytes=7";
pub const TD_TERM_CLIPBOARD_SENT_MARKER: &str = "TD-TERM-CLIPBOARD-SENT bytes=7";
pub const TD_FIREFOX_CLIPBOARD_REFOCUS_ARMED_MARKER: &str =
    "TD-FIREFOX-CLIPBOARD-REFOCUS-ARMED";
pub const TD_FIREFOX_CLIPBOARD_WINDOW_ARMED_MARKER: &str =
    "TD-FIREFOX-CLIPBOARD-WINDOW-ARMED";
pub const TD_FIREFOX_CLIPBOARD_ARMED_MARKER: &str = "TD-FIREFOX-CLIPBOARD-ARMED";
pub const TD_FIREFOX_CLIPBOARD_RETRY_MARKER: &str = "TD-FIREFOX-CLIPBOARD-RETRY-ARMED";
pub const TD_FIREFOX_CLIPBOARD_MARKER: &str = "TD-FIREFOX-CLIPBOARD-OK";
pub const TD_FIREFOX_DOWNLOAD_ARMED_MARKER: &str = "TD-FIREFOX-DOWNLOAD-ARMED";
pub const TD_FIREFOX_DOWNLOAD_MARKER: &str = "TD-FIREFOX-DOWNLOAD-OK bytes=23";
/// Selects the physical-input oracle without changing an ordinary Firefox boot.
pub const FIREFOX_INPUT_CMDLINE_TOKEN: &str = "td.firefox-input=1";
/// Must match the compositor's independently pinned client-cursor dimension cap.
pub const FIREFOX_CURSOR_MAX_DIMENSION: usize = 256;

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

/// Printed by `/etc/netup` after unprivileged Git resolves an upstream repository,
/// launches its HTTPS remote helper, verifies TLS with the image CA bundle, and reads
/// the remote HEAD. Like the other network markers, this is operator-only evidence.
pub const GIT_HTTPS_RUNTIME_MARKER: &str = "TD-GIT-HTTPS-OK";

/// Kernel-cmdline token the headless `qemu-boot-net` oracle appends so `/etc/netup`
/// runs the resolve+reach and Git HTTPS self-tests (and prints the four markers
/// above). Absent it
/// (normal boot, or the `-nic none` `qemu-boot-system` oracle), td-netd still brings
/// the link up but the self-test and its markers are skipped.
pub const NETTEST_CMDLINE_TOKEN: &str = "td.nettest=1";

/// One upstream endpoint for every network-oracle leg. qemu user-net forwards DNS
/// and NATs outbound TCP, so td-netd first resolves this host and reaches its HTTPS
/// port, then Git reads the repository URL on the same service. `/etc/netup` compiles
/// these in; there is no runtime cmdline override.
pub const NETTEST_DEFAULT_HOST: &str = "git.kernel.org";
pub const NETTEST_DEFAULT_PORT: &str = "443";
pub const GIT_HTTPS_TEST_URL: &str = "https://git.kernel.org/pub/scm/git/git.git";

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

    #[test]
    fn line_only_source_check_uses_the_shared_dwarf5_parser() {
        let step = super::debug_line_source_root_check(
            "/tools/readelf",
            "/output/lib/debug/bin/tool.debug",
            "main.c",
            "/td-build",
        );
        let Step::Run { argv, .. } = step else {
            panic!("line-table check is not executable");
        };
        let command = argv.get(2).expect("line-table check command");
        for required in [
            "--debug-dump=rawline",
            "-v source='main.c'",
            "-v root='/td-build'",
            "unsupported DWARF version",
        ] {
            assert!(command.contains(required), "line check omits {required}");
        }
        assert!(!command.contains("DW_AT_comp_dir"));
    }

    #[test]
    fn line_only_parser_regressions_wire_one_real_recipe_probe() {
        let steps = super::debug_line_validator_regression_steps();
        assert!(matches!(steps.first(), Some(Step::WriteFile { .. })));
        let Some(Step::Run { argv, .. }) = steps.get(1) else {
            panic!("line-table regression check is not executable");
        };
        let command = argv.get(2).expect("line-table regression command");
        for required in [
            "canonical rawline fixture was rejected",
            "wrong-directory rawline fixture passed",
            "traversal rawline fixture passed",
            "spaced-path rawline fixture passed",
            "DWARF-4 rawline fixture passed",
            "statement-scratch rawline fixture passed",
            "dynamic-file rawline fixture passed",
        ] {
            assert!(command.contains(required), "regression omits {required}");
        }
    }

    #[test]
    fn codex_release_strings_are_one_consistent_pin() {
        assert_eq!(
            super::CODEX_VERSION_OUTPUT,
            format!("codex-cli {}", super::CODEX_RECIPE_VERSION)
        );
        assert_eq!(
            super::CODEX_BWRAP_VERSION_OUTPUT,
            format!("bubblewrap {}", super::CODEX_BWRAP_UPSTREAM_VERSION)
        );
        assert_eq!(
            super::CODEX_BWRAP_RECIPE_VERSION,
            format!(
                "{}-codex-{}",
                super::CODEX_BWRAP_UPSTREAM_VERSION,
                super::CODEX_RECIPE_VERSION
            )
        );
        let exception = td_engine::target_profile::line_attribution_exception("codex")
            .expect("Codex line-attribution exception");
        assert!(exception.reason.contains(super::CODEX_RECIPE_VERSION));
    }

    #[test]
    fn application_config_text_is_built_from_the_contract_constants() {
        assert_eq!(
            super::TD_APPLICATION_CONFIG_TEXT,
            format!(
                "format=1\npackage-root={}\nstate-root={}\nregistry={}\nlauncher-table={}\ncgroup-root={}\n",
                super::TD_APPLICATION_PACKAGE_ROOT,
                super::TD_APPLICATION_STATE_ROOT,
                super::TD_APPLICATION_REGISTRY,
                super::TD_APPLICATION_LAUNCHER_TABLE,
                super::TD_APPLICATION_CGROUP_ROOT,
            )
        );
    }

    #[test]
    fn application_cgroup_paths_share_one_hierarchy() {
        assert_eq!(
            super::TD_APPLICATION_CGROUP_SESSION,
            format!("{}/session", super::TD_APPLICATION_CGROUP_ROOT)
        );
        assert_eq!(
            super::TD_APPLICATION_CGROUP_ROOT.strip_prefix("/sys/fs/cgroup"),
            Some(super::TD_APPLICATION_CGROUP_MEMBERSHIP_ROOT)
        );
    }

    const POST_BOOTSTRAP_BOUNDARY_OUTPUTS: [&str; 6] = [
        "rust-toolchain",
        "gcc-x86-64-self",
        "binutils-x86-64-self",
        "glibc-x86-64",
        "busybox-x86-64",
        // Static CMake is already built with the final native GCC and is the
        // reviewed configure-language boundary for later C/C++ build tools.
        "cmake-x86-64",
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
    const POST_BOOTSTRAP_PROTECTED_INPUT_EXCEPTIONS: [(&str, &str); 7] = [
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
        // Rebuild GNU Make once with the final compiler. The preceding Make is
        // only the build driver; later packages consume make-x86-64-self.
        // The frozen UAPI input is a fixed-output source governed by the seed
        // provenance gate, not a catalog recipe this exception table can name.
        ("make-x86-64-self", "make-x86-64"),
    ];
    const RECIPE_SHEBANG_INTERPRETERS: [&str; 2] = [super::SH, super::POST_BOOTSTRAP_SH];
    const GUEST_LITERAL_SHEBANGS: [(&str, &str); 16] = [
        ("linux-x86-64", "{root}/initramfs/init"),
        ("kexec-spike-x86-64", "{root}/inner-init"),
        ("kexec-spike-x86-64", "{root}/outer-init"),
        (
            "td-jail-test",
            "/home/td-jail-host/packages/00000000000000000000000000000000-firefox-154.0/files/bin/firefox",
        ),
        ("system-x86-64", "{root}/selector-init"),
        ("system-x86-64", "{root}/deployment-init"),
        ("system-x86-64", "{root}/real-root/etc/autologin"),
        ("system-x86-64", "{root}/real-root/etc/tty-session"),
        ("system-x86-64", "{root}/real-root/etc/shutdown"),
        ("system-x86-64", "{root}/real-root/etc/rootcheck"),
        ("system-x86-64", "{root}/real-root/etc/netup"),
        (
            "system-x86-64",
            "{root}/real-root/etc/firefox-tls-setup",
        ),
        (
            "system-x86-64",
            "{root}/real-root/etc/firefox-tls-origin",
        ),
        (
            "system-x86-64",
            "{root}/real-root/etc/firefox-tls-ready",
        ),
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
    /// It frees the IDENTIFIER and nothing else: to this scan a bare `find` in
    /// a comment reads exactly as a quoted `Command::new("find")` does. That
    /// remains true of every SCRIPT body, which is what this is applied to now
    /// — a `.rs` body goes through `rust_source_invokes`, which can tell the
    /// two apart because Rust has string literals to look inside, and a body
    /// on `RUST_NOT_A_COMMAND_SURFACE` through neither.
    fn invokes(s: &str, cmd: &str) -> bool {
        s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|t| t == cmd)
    }

    /// `invokes`'s word rule over a byte slice, so it can be applied to one
    /// string literal rather than to a whole file.
    fn invokes_in(bytes: &[u8], cmd: &str) -> bool {
        let needle = cmd.as_bytes();
        if needle.is_empty() || bytes.len() < needle.len() {
            return false;
        }
        let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
        for start in 0..=bytes.len() - needle.len() {
            if bytes.get(start..start + needle.len()) != Some(needle) {
                continue;
            }
            let before = start.checked_sub(1).and_then(|i| bytes.get(i)).copied();
            let after = bytes.get(start + needle.len()).copied();
            if !before.is_some_and(word) && !after.is_some_and(word) {
                return true;
            }
        }
        false
    }

    /// The length of the char literal at `at`, or `None` where that quote
    /// opens a LIFETIME instead.
    ///
    /// The two are told apart by whether a quote closes within the longest
    /// form Rust allows — `'\u{10FFFF}'`, ten bytes — since a lifetime has no
    /// closing quote at all. Getting this wrong is not cosmetic: `'\"'` holds
    /// a quote, and skipping only the three-byte form leaves that quote to
    /// open a string, whose contents then run to the NEXT quote — so the real
    /// literal after it is read as ordinary text and its command name is never
    /// seen. That is a false NEGATIVE, which is the direction a guard cannot
    /// afford to be wrong in.
    fn char_literal_len(bytes: &[u8], at: usize) -> Option<usize> {
        match bytes.get(at + 1).copied()? {
            // `'\''` closes on the quote AFTER the escaped one, so the scan
            // starts past it.
            b'\\' => {
                let mut end = at + 3;
                while end < bytes.len() && end <= at + 11 {
                    if bytes.get(end) == Some(&b'\'') {
                        return end.checked_sub(at).map(|len| len + 1);
                    }
                    end += 1;
                }
                None
            }
            b'\'' => None,
            _ => (bytes.get(at + 2) == Some(&b'\'')).then_some(3),
        }
    }

    /// The escape at `at` as the byte it denotes, and how many source bytes it
    /// spans.
    ///
    /// The BYTE matters rather than merely the boundary: mapping every escape
    /// to a space leaves `Command::new("\x66ind")` — a compile-time spelling of
    /// `find`, not a run-time one — unseen. Anything undecodable or non-ASCII
    /// becomes a space, which is a boundary and so can only end a token rather
    /// than complete one.
    fn unescape(bytes: &[u8], at: usize) -> (u8, usize) {
        let hex = |from: usize, len: usize| -> Option<u8> {
            let digits = bytes.get(from..from.checked_add(len)?)?;
            let text = std::str::from_utf8(digits).ok()?;
            u32::from_str_radix(text, 16)
                .ok()
                .and_then(|value| u8::try_from(value).ok())
                .filter(u8::is_ascii)
        };
        match bytes.get(at + 1).copied() {
            Some(b'n') => (b'\n', 2),
            Some(b'r') => (b'\r', 2),
            Some(b't') => (b'\t', 2),
            Some(b'0') => (0, 2),
            Some(b'\\') => (b'\\', 2),
            Some(b'\'') => (b'\'', 2),
            Some(b'"') => (b'"', 2),
            Some(b'x') => (hex(at + 2, 2).unwrap_or(b' '), 4),
            // `\u{…}` takes at most six hex digits, so the brace is within ten
            // bytes. Searching further would let a MALFORMED `\u` swallow the
            // span up to some later brace — dropping the text between, which
            // is the false-negative direction.
            Some(b'u') => {
                let width = bytes
                    .get(at..(at + 10).min(bytes.len()))
                    .and_then(|rest| rest.iter().position(|c| *c == b'}'))
                    .map_or(2, |n| n + 1);
                let digits = width.checked_sub(4).unwrap_or(0);
                (hex(at + 3, digits).unwrap_or(b' '), width)
            }
            _ => (b' ', 2),
        }
    }

    /// The contents of every string literal in a Rust source.
    ///
    /// Comments are skipped rather than scanned, because a `"` in one would
    /// otherwise open a literal that swallows the rest of the file. Raw strings
    /// are recognised so their bodies are read as text rather than as escapes,
    /// and char literals — `'\"'` included — are skipped whole, since a quote
    /// inside one that opened a string would shift every literal after it.
    ///
    /// Escapes are DECODED rather than skipped, which is what makes the word
    /// rule mean the same thing inside a literal as outside it. In
    /// `"set -e\nfind /x"` the raw byte before `find` is the `n` of the escape,
    /// so a scan over the source bytes would read `\nfind` as one word and miss
    /// the invocation; and `"\x66ind"` spells `find` at compile time, so an
    /// escape reduced to a boundary would miss that too.
    fn rust_string_literals(text: &str) -> Vec<Vec<u8>> {
        let bytes = text.as_bytes();
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            let byte = bytes.get(i).copied().unwrap_or(0);
            let next = bytes.get(i + 1).copied();
            match byte {
                b'/' if next == Some(b'/') => {
                    i = match bytes.get(i..).and_then(|r| r.iter().position(|c| *c == b'\n')) {
                        Some(n) => i + n + 1,
                        None => bytes.len(),
                    };
                }
                b'/' if next == Some(b'*') => {
                    // Rust nests block comments, so the close has to be counted.
                    let mut depth = 1usize;
                    i += 2;
                    while i < bytes.len() && depth > 0 {
                        match (bytes.get(i).copied(), bytes.get(i + 1).copied()) {
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                i += 2;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                i += 2;
                            }
                            _ => i += 1,
                        }
                    }
                }
                b'\'' => {
                    let _ = next;
                    i += char_literal_len(bytes, i).unwrap_or(1);
                }
                b'r' => {
                    let mut hashes = 0usize;
                    let mut open = i + 1;
                    while bytes.get(open) == Some(&b'#') {
                        hashes += 1;
                        open += 1;
                    }
                    if bytes.get(open) != Some(&b'"') {
                        i += 1;
                        continue;
                    }
                    let start = open + 1;
                    let mut end = start;
                    while end < bytes.len() {
                        if bytes.get(end) == Some(&b'"')
                            && bytes
                                .get(end + 1..end + 1 + hashes)
                                .is_some_and(|tail| tail.iter().all(|c| *c == b'#'))
                        {
                            break;
                        }
                        end += 1;
                    }
                    // A raw string carries no escapes, so its bytes stand.
                    if let Some(literal) = bytes.get(start..end.min(bytes.len())) {
                        out.push(literal.to_vec());
                    }
                    i = (end + 1 + hashes).max(i + 1);
                }
                b'"' => {
                    let mut literal: Vec<u8> = Vec::new();
                    let mut end = i + 1;
                    while end < bytes.len() {
                        match bytes.get(end).copied() {
                            Some(b'"') => break,
                            Some(b'\\') => {
                                let (byte, width) = unescape(bytes, end);
                                literal.push(byte);
                                end += width;
                            }
                            Some(byte) => {
                                literal.push(byte);
                                end += 1;
                            }
                            None => break,
                        }
                    }
                    out.push(literal);
                    i = (end + 1).max(i + 1);
                }
                _ => i += 1,
            }
        }
        out
    }

    /// A staged Rust source is COMPILED rather than interpreted, so a bare
    /// token in one is an identifier, a method name or an English word —
    /// `xs.find(…)`, or a comment saying where to find something. Scanning it
    /// the way a shell script is scanned reds every crate that uses
    /// `Iterator::find`, which is a false positive rather than a host tool.
    ///
    /// What WOULD be a host tool is a command name reaching `Command`, and a
    /// name reaching one has to be spelled in a STRING LITERAL — so a `.rs`
    /// body is scanned only there, with the same word rule scripts get.
    /// `Command::new("find")`, `["xargs", …]`, `"/usr/bin/find"` and a name
    /// buried in an argument to `sh -c` all still red; an identifier and a
    /// comment do not.
    ///
    /// What escapes it is a name ASSEMBLED rather than spelled, and not only at
    /// run time: `concat!("fi", "nd")` and `stringify!` build one at COMPILE
    /// time and are scanned as two literals neither of which is the word. It is
    /// the same blind spot the token scan already has for `$TOOL` in a script.
    fn rust_source_invokes(text: &str, cmd: &str) -> bool {
        rust_string_literals(text)
            .iter()
            .any(|literal| invokes_in(literal, cmd))
    }

    /// A `.rs` body rustc compiles. NON-EXECUTABLE, because `exec` is a file
    /// MODE rather than a promise about the reader: a `.rs` written executable
    /// is a script something interprets, and the literal-only scan below would
    /// read its commands as prose.
    fn is_rust_module(step: &Step) -> bool {
        rust_module_path(step).is_some()
    }

    /// The same question as `is_rust_module`, answering with the path — which
    /// is what every caller that has to match the enum a second time wanted.
    fn rust_module_path(step: &Step) -> Option<&str> {
        match step {
            Step::WriteFile { path, exec, .. } if path.ends_with(".rs") && !exec => {
                Some(path.as_str())
            }
            _ => None,
        }
    }

    fn step_invokes(step: &Step, text: &str, cmd: &str) -> bool {
        if is_rust_module(step) {
            rust_source_invokes(text, cmd)
        } else {
            invokes(text, cmd)
        }
    }

    /// The recipes whose Rust modules are not a command surface at all.
    ///
    /// `rust_source_invokes` above frees the identifier and the comment, which
    /// is most of the problem and leaves one case it cannot reach: a
    /// DIAGNOSTIC. td-txt is scored byte for byte against GNU, and GNU sed
    /// refuses an unresolvable jump with `can't find label for jump to \`X'`
    /// — so matching it puts the bare word in a string literal, which is
    /// exactly where a command name would be. No scan can tell those apart,
    /// because they are the same thing to a scanner: text in a literal.
    ///
    /// So this is a REVIEWED LIST and not an inference. The inference was
    /// written first and deleted, against ten known ways to satisfy "this
    /// crate cannot spawn" and still compile one — `global_asm!`,
    /// `extern "C"`, `include!` from a non-`.rs` body, `Unpack`,
    /// `SubstituteText` rewriting a source after it was read, `Symlink`,
    /// `--cap-lints`, `--force-warn`, a `@response` file, and a multi-line
    /// `#[doc]` holding a line that looks like the crate's lint attribute.
    /// Every one WIDENED the exemption silently. Proving it from recipe text
    /// means proving a property of a compilation this cannot see; a name a
    /// human edits is the same protection without the surface.
    ///
    /// Per FILE and not per recipe, which is the difference between an entry
    /// a human can check and one that grows on its own: a reviewer opens
    /// `sed.rs` and confirms its `find` tokens are messages, where a
    /// crate-wide entry would go on covering modules added to td-txt long
    /// after anyone read it. Silent widening is the failure this whole design
    /// is about, so the roster must not have it either.
    ///
    /// ONE entry, because one that exempts nothing is speculation rather than
    /// a decision: td-seatd is equally spawn-free and was rostered at first,
    /// but nothing it writes holds either word. Adding one is a reviewed
    /// decision, and what to check first is
    /// `the_rostered_recipes_still_cannot_spawn` below: no `Command` in what
    /// the recipe writes, a crate root that forbids `unsafe_code`, and no
    /// `.rs` handed to anything but rustc. That test is a TRIPWIRE and not a
    /// gate — it cannot exempt anything, only complain — which is why it is
    /// safe for it to be approximate where a gate would not be.
    const RUST_NOT_A_COMMAND_SURFACE: [(&str, &str); 1] = [("td-txt", "{src}/sed.rs")];

    /// Whether this step writes a rostered `.rs` body of `stem`.
    ///
    /// The WHOLE path the recipe spells, not the basename: a basename would
    /// exempt a second `sed.rs` written anywhere else in the same recipe, and
    /// an entry that covers a body nobody reviewed is the silent widening this
    /// roster exists to avoid. A path that moves fails CLOSED — the entry stops
    /// matching, and the guard asks for a fresh look.
    fn rostered(stem: &str, step: &Step) -> bool {
        let Some(path) = rust_module_path(step) else {
            return false;
        };
        RUST_NOT_A_COMMAND_SURFACE
            .iter()
            .any(|(recipe, body)| *recipe == stem && *body == path)
    }

    /// The retired tools, named once: the farm branch and the text branch each
    /// scan for them, and a third added to one alone would be invisible on the
    /// other with every test still green.
    const HOST_TOOLS: [&str; 2] = ["find", "xargs"];

    /// A ToolFarm judged link by link, which the flattened text list cannot do.
    ///
    /// A name is excused only by ITS OWN target being the declared busybox,
    /// and only while it is a bare filename — `tools.join(name)` puts anything
    /// else outside the farm. A target is never excused at all: it is what the
    /// link resolves TO. Flattening loses both of those, since it keeps
    /// neither the pairing nor which side a string came from.
    ///
    /// `invokes` rather than `step_invokes`: a farm is never a `.rs` body, so
    /// the literal-only reading cannot apply to one and the two agree here.
    ///
    /// A later link SHADOWS an earlier one of the same name, but a bad link is
    /// reported even where a good one follows it: the farm is the recipe's
    /// declaration of what it wants, and fail-closed is the right direction
    /// for a duplicate nobody meant to write.
    fn farm_invocation(recipe: &Recipe, links: &[(String, String)]) -> Option<(&'static str, String)> {
        let declared = recipe
            .native_inputs
            .as_ref()
            .is_some_and(|inputs| inputs.iter().any(|i| i == "busybox-x86-64"));
        for (name, target) in links {
            let excused = declared
                && !name.contains('/')
                && target == "{in:busybox-x86-64}/bin/busybox";
            for cmd in HOST_TOOLS {
                if invokes(name, cmd) && !excused {
                    return Some((cmd, name.clone()));
                }
                if invokes(target, cmd) {
                    return Some((cmd, target.clone()));
                }
            }
        }
        None
    }

    /// The guard's scan over ONE recipe: the first `find`/`xargs` invocation in
    /// a command surface, or `None` for a clean recipe. Extracted so the
    /// synthetic tests that pin the exemption run the SAME skip the catalog
    /// walk does — a list asserted on its own stays green with this skip wired
    /// backwards.
    fn host_tool_invocation(stem: &str, recipe: &Recipe) -> Option<(&'static str, String)> {
        let steps = recipe.steps.as_ref()?;
        for step in steps {
            // The exemption frees the ROSTERED bodies and only those: another
            // `.rs` in the same recipe, Run argv, a baked Makefile, ToolFarm
            // links and a SubstituteText's `to` all keep the full scan, since
            // a shell reads those whatever the Rust beside them can do.
            if rostered(stem, step) {
                continue;
            }
            if let Step::ToolFarm { links } = step {
                if let Some(hit) = farm_invocation(recipe, links) {
                    return Some(hit);
                }
                continue;
            }
            for text in command_texts(step) {
                for cmd in HOST_TOOLS {
                    if !step_invokes(step, text, cmd) {
                        continue;
                    }
                    // The one exemption lives in `farm_invocation` above, which
                    // this loop no longer sees a ToolFarm for.
                    return Some((cmd, text.to_string()));
                }
            }
        }
        None
    }

    /// Every catalog-authored text of a step that becomes a command or an
    /// interpreted script/Makefile: Run argv, ANY WriteFile body (baked
    /// Makefiles/kaem scripts are written `exec: false` and then run over by a
    /// Run step), ToolFarm links, and the `to` side of the literal SubstituteText
    /// edits (the host-free `patch`/`sed` stand-in). Engine-native steps that
    /// carry only paths (Unpack/CopyTree/Symlink/PatchShebangs/…) cannot invoke a
    /// tool, so they contribute nothing. Shared by the catalog-walk guard and its
    /// coverage test so both exercise exactly the same extraction — EXCEPT a
    /// ToolFarm, which the guard now reads through `farm_invocation` instead,
    /// because a link's two halves mean different things and flattening them
    /// into one list loses that. The arm stays because this is a general
    /// extractor and a farm's strings really are catalog-authored text.
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

    /// The DATA channel walks with the other two. This is a guard rather than a
    /// build path, which is exactly why it must: `post_bootstrap_back_edges` asks
    /// whether a post-bootstrap recipe reaches into the bootstrap interior, and a
    /// back edge spelled `payload_inputs` is still a back edge — one the guard
    /// would simply not see (APPLICATIONS.md §B.8).
    fn direct_recipe_inputs(recipe: &Recipe) -> Vec<&str> {
        recipe
            .inputs
            .iter()
            .flatten()
            .chain(recipe.native_inputs.iter().flatten())
            .chain(recipe.payload_inputs.iter().flatten())
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

    fn command_glob_is_build_local(pattern: &str) -> bool {
        ["{root}/", "{src}/", "{out}/", "{tools}/"]
            .iter()
            .any(|prefix| pattern.starts_with(prefix))
            && !pattern.split('/').any(|component| component == "..")
    }

    #[test]
    fn command_globs_only_read_the_build_tree() {
        for (stem, recipe) in catalog::all() {
            let Some(steps) = recipe.steps else {
                continue;
            };
            for step in steps {
                let Step::Run { argv, .. } = step else {
                    continue;
                };
                for arg in argv {
                    let Some(pattern) = arg.strip_prefix("glob:") else {
                        continue;
                    };
                    assert!(
                        command_glob_is_build_local(pattern),
                        "recipe `{stem}' command glob reads outside its own tree: {pattern}"
                    );
                }
            }
        }
        assert!(!command_glob_is_build_local("{root}/../input/*"));
        assert!(!command_glob_is_build_local(
            "{in:binutils-mesboot0}/bin/*"
        ));
        assert!(command_glob_is_build_local("{src}/objects/*.o"));
        assert!(command_glob_is_build_local("{tools}/wrappers/*"));
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
    /// separately reviewed audit, transition, or boot-artifact edges.
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
            if let Some((cmd, text)) = host_tool_invocation(stem, &recipe) {
                panic!(
                    "recipe `{stem}' invokes `{cmd}' in `{text}' — \
                     GNU findutils was retired from the tool tier; a rung \
                     must expose this command through a ToolFarm link to \
                     its declared td-built busybox-x86-64 input"
                );
            }
        }
    }

    /// The busybox exemption excuses a LINK NAME, never a target beside it.
    ///
    /// `command_texts` flattens a ToolFarm to every string in it, names and
    /// targets alike, and the answer used to be step-wide: one declared
    /// `find` link then excused a second link pointing at a host path in the
    /// same step, which is the one shape this exemption must not cover.
    #[test]
    fn a_busybox_farm_excuses_its_own_link_and_not_a_host_path_beside_it() {
        const BB: &str = "{in:busybox-x86-64}/bin/busybox";
        let farm = |links: Vec<(String, String)>| {
            Recipe::gnu("probe", "1")
                .native_inputs(&["busybox-x86-64"])
                .steps(vec![Step::ToolFarm { links }])
        };

        // The declared links themselves stay exempt, which is what a rung needs.
        let ok = farm(vec![("find".into(), BB.into()), ("date".into(), BB.into())]);
        assert_eq!(host_tool_invocation("probe", &ok), None);

        // A host path beside one does NOT ride on that exemption.
        let mixed = farm(vec![
            ("find".into(), BB.into()),
            ("scan".into(), "/usr/bin/find".into()),
        ]);
        assert_eq!(
            host_tool_invocation("probe", &mixed),
            Some(("find", "/usr/bin/find".into()))
        );

        // A link NAMED for the tool but pointing elsewhere is not a busybox
        // link, so naming it right does not excuse it.
        let liar = farm(vec![("find".into(), "{root}/tools/find".into())]);
        assert_eq!(host_tool_invocation("probe", &liar), Some(("find", "find".into())));

        // ...and the exemption is not narrowed to the bare tool name: a link
        // whose name merely tokenises to it is still a declared busybox link.
        let hyphen = farm(vec![("find-files".into(), BB.into())]);
        assert_eq!(host_tool_invocation("probe", &hyphen), None);

        // Without the declared input there is no exemption at all — and the
        // recipe declares ANOTHER native input, so this pins the input's
        // identity rather than merely that the field is populated.
        let undeclared = Recipe::gnu("probe", "1")
            .native_inputs(&["gcc-x86-64-native"])
            .steps(vec![Step::ToolFarm {
                links: vec![("find".into(), BB.into())],
            }]);
        assert_eq!(
            host_tool_invocation("probe", &undeclared),
            Some(("find", "find".into()))
        );

        // A DUPLICATE name is judged on its own target. The farm applies its
        // links in order and each replaces the last, so this second one is
        // what `{tools}/find` actually becomes.
        let shadowed = farm(vec![
            ("find".into(), BB.into()),
            ("find".into(), "{in:other}/bin/search".into()),
        ]);
        assert_eq!(host_tool_invocation("probe", &shadowed), Some(("find", "find".into())));

        // A TARGET spelled as a sibling's link name is still a target.
        let alias = farm(vec![("find".into(), BB.into()), ("scan".into(), "find".into())]);
        assert_eq!(host_tool_invocation("probe", &alias), Some(("find", "find".into())));

        // `xargs` as well as `find`, because this branch carries its own copy
        // of the tool list: dropping one from it would leave every leg above
        // green, the catalog naming only legitimate `xargs` links.
        let both = farm(vec![
            ("xargs".into(), BB.into()),
            ("scan".into(), "/usr/bin/xargs".into()),
        ]);
        assert_eq!(
            host_tool_invocation("probe", &both),
            Some(("xargs", "/usr/bin/xargs".into()))
        );

        // A name is a BARE filename, not merely a non-absolute one: `sub/find`
        // lands at `{tools}/sub/find` and so is harmless, but refusing it is
        // the fail-closed direction and pins the rule as written.
        let nested = farm(vec![("sub/find".into(), BB.into())]);
        assert_eq!(host_tool_invocation("probe", &nested), Some(("find", "sub/find".into())));

        // A name is excused only while it is a bare filename. ALONE in the
        // farm, so nothing else can catch it: `tools.join("/usr/bin/find")`
        // discards the farm directory outright, so this link is not a tool
        // named `find` inside it whatever it points at.
        let escaped = farm(vec![("/usr/bin/find".into(), BB.into())]);
        assert_eq!(
            host_tool_invocation("probe", &escaped),
            Some(("find", "/usr/bin/find".into()))
        );
    }

    /// The ROSTER, exercised through the guard's own scan.
    ///
    /// The body here is what the literal scan cannot help with: a diagnostic
    /// td-txt must emit byte for byte because GNU sed does, spelled in a
    /// string literal because that is where a message lives — and where a
    /// command name would live too.
    ///
    /// Driven through `host_tool_invocation` rather than by inspecting the
    /// roster, because a list is not a guard: a skip wired backwards leaves
    /// every assertion about the list green.
    #[test]
    fn a_rust_module_in_a_rostered_recipe_is_not_a_command_surface() {
        let rs = |path: &str, content: &str| Step::WriteFile {
            path: path.into(),
            content: content.into(),
            exec: false,
        };
        const SED: &str =
            "fn refuse() -> Error {\n    Error::new(\"can't find label for jump to `\")\n}\n";
        let diagnostic = rs("{src}/sed.rs", SED);
        let found = Some(("find", SED.into()));
        let recipe = Recipe::gnu("td-txt", "1").steps(vec![diagnostic.clone()]);

        // Rostered: the word is a message, not a program.
        assert_eq!(host_tool_invocation("td-txt", &recipe), None);

        // Not rostered: the literal scan catches the same body, which is the
        // whole reason the roster has to exist. This is the assertion that
        // matters most here.
        assert_eq!(host_tool_invocation("td-sh", &recipe), found);
        assert_eq!(host_tool_invocation("gcc-mesboot", &recipe), found);

        // Per FILE: another `.rs` in the SAME rostered recipe keeps the full
        // literal scan, so the entry covers the body a human read and not
        // whatever td-txt grows next. `grep.rs` is a real td-txt module, so
        // rostering it later reds this leg — which is the look being asked
        // for, not a false alarm.
        let sibling = Recipe::gnu("td-txt", "1").steps(vec![rs("{src}/grep.rs", SED)]);
        assert_eq!(host_tool_invocation("td-txt", &sibling), found);

        // ...and by the WHOLE path: a second `sed.rs` written elsewhere in the
        // same recipe is a body nobody reviewed, so a basename match would be
        // the silent widening the roster exists to avoid.
        let elsewhere = Recipe::gnu("td-txt", "1").steps(vec![rs("{src}/vendor/sed.rs", SED)]);
        assert_eq!(host_tool_invocation("td-txt", &elsewhere), found);

        // The ROSTERED path written EXECUTABLE, which is the only shape that
        // proves the `exec` bit revokes the exemption: any other name would be
        // scanned for its name's sake and the leg would assert nothing.
        let script = Recipe::gnu("td-txt", "1").steps(vec![Step::WriteFile {
            path: "{src}/sed.rs".into(),
            content: "#!/bin/sh\nfind . -delete\n".into(),
            exec: true,
        }]);
        assert_eq!(
            host_tool_invocation("td-txt", &script),
            Some(("find", "#!/bin/sh\nfind . -delete\n".into()))
        );

        // The exemption frees `.rs` bodies and NOTHING else: inside the SAME
        // rostered recipe every other command surface keeps the full catch.
        // Each leg pins the reported TEXT, or it would pass on the wrong step
        // being blamed.
        let baked = Recipe::gnu("td-txt", "1").steps(vec![
            diagnostic.clone(),
            Step::WriteFile {
                path: "Makefile".into(),
                content: "clean:\n\tfind . -delete\n".into(),
                exec: false,
            },
        ]);
        assert_eq!(
            host_tool_invocation("td-txt", &baked),
            Some(("find", "clean:\n\tfind . -delete\n".into()))
        );

        let ran = Recipe::gnu("td-txt", "1").steps(vec![
            diagnostic.clone(),
            Step::Run {
                argv: vec!["find".into(), ".".into()],
                env: Vec::new(),
                dir: String::new(),
            },
        ]);
        assert_eq!(host_tool_invocation("td-txt", &ran), Some(("find", "find".into())));

        let farm = Recipe::gnu("td-txt", "1").steps(vec![
            diagnostic.clone(),
            Step::ToolFarm {
                links: vec![("find".into(), "{root}/tools/find".into())],
            },
        ]);
        assert_eq!(host_tool_invocation("td-txt", &farm), Some(("find", "find".into())));

        let edited = Recipe::gnu("td-txt", "1").steps(vec![
            diagnostic,
            Step::SubstituteText {
                file: "configure".into(),
                edits: vec![crate::types::TextEdit::new("rm -f x", "xargs rm -f", 1)],
            },
        ]);
        assert_eq!(
            host_tool_invocation("td-txt", &edited),
            Some(("xargs", "xargs rm -f".into()))
        );
    }

    /// Every rostered entry names a `.rs` that recipe actually writes.
    ///
    /// A typo in either half fails CLOSED — it exempts nothing — but the body
    /// it was meant to name goes back under the scan, and the way that
    /// surfaces is a `find` in shipped source reddening the guard with no hint
    /// that the roster is why.
    ///
    /// Each tuple is checked against the steps ITSELF rather than through
    /// `rostered`, which searches the whole roster: once two entries share a
    /// recipe, a misspelt one would ride on the other's match. EXACTLY one, so
    /// a recipe writing a path twice cannot leave an entry covering a body
    /// that is not the reviewed one.
    #[test]
    fn every_rostered_entry_names_a_body_the_recipe_writes() {
        for (stem, body) in RUST_NOT_A_COMMAND_SURFACE {
            let recipe = catalog::all()
                .into_iter()
                .find(|(name, _)| *name == stem)
                .map(|(_, recipe)| recipe);
            let Some(recipe) = recipe else {
                panic!("rostered recipe `{stem}' is not in the catalog");
            };
            let written = recipe.steps.as_ref().map_or(0, |steps| {
                steps
                    .iter()
                    .filter(|step| rust_module_path(step) == Some(body))
                    .count()
            });
            assert_eq!(
                written, 1,
                "rostered `{stem}'/`{body}' must name exactly one `.rs' that \
                 recipe writes, or the entry covers nothing or too much"
            );
        }
    }

    /// Why a rostered recipe looks able to start a process, or `None`.
    ///
    /// Split out from the test below so the CHECK can be exercised in both
    /// directions. Asserted only against the catalog it would answer `None`
    /// for everything and stay green if it were deleted — the trap the first
    /// version of these tests fell into.
    ///
    /// Approximate in BOTH directions, which is why it is a tripwire and not
    /// a gate. It over-complains: a bare `Command` token also matches a
    /// comment, a string or an `enum Command` in any body the recipe writes;
    /// the `forbid` must open and close on one line AND be the first
    /// substantive one, so an inner attribute above it reds; and only
    /// `main.rs` counts as a crate root, so a rostered recipe growing a
    /// `lib.rs` reds `writes no crate root`. Those cost a human a second look.
    ///
    /// It also UNDER-detects, and that half must not be read as a guarantee:
    /// this is the deleted inference's reading, so every one of the ten
    /// evasions listed on `RUST_NOT_A_COMMAND_SURFACE` defeats it too —
    /// `extern "C"` reaching libc, a `concat!`-spelled name, `include!`,
    /// `--cap-lints`, and Rust arriving by `Unpack`/`CopyTree`, which is not a
    /// `WriteFile` body at all and so has nothing for either this or the scan
    /// to read. The interpreter leg is bounded the same way: it looks for an
    /// argv element ENDING in `.rs`, so `sh -c 'sh {src}/x.rs; echo done'`
    /// passes it. What makes that acceptable here is only that it decides
    /// nothing — as a gate the same reading would have exempted a recipe.
    fn spawn_tripwire(recipe: &Recipe) -> Option<String> {
        let steps = recipe.steps.as_ref()?;
        let mut roots = 0usize;
        for step in steps {
            if let Step::Run { argv, .. } = step {
                let rustc = argv
                    .first()
                    .is_some_and(|arg| arg.rsplit('/').next() == Some("rustc"));
                if !rustc {
                    if let Some(arg) = argv.iter().find(|arg| arg.ends_with(".rs")) {
                        return Some(format!("`{arg}' is handed to a program that is not rustc"));
                    }
                }
            }
            let Step::WriteFile { path, content, .. } = step else {
                continue;
            };
            if invokes(content, "Command") {
                return Some(format!("`{path}' names `Command'"));
            }
            if !is_rust_module(step) || path.rsplit('/').next() != Some("main.rs") {
                continue;
            }
            roots += 1;
            // The first substantive line, so a mention in a comment or a
            // string cannot stand in for the attribute — and the lint has to
            // be an item INSIDE the parens, or `#![forbid(dead_code)] //
            // unsafe_code` reads as the real thing.
            let opens_with_forbid = content
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with("//"))
                .and_then(|line| line.strip_prefix("#![forbid("))
                .and_then(|rest| rest.split_once(')'))
                .is_some_and(|(lints, _)| {
                    lints.split(',').any(|lint| lint.trim() == "unsafe_code")
                });
            if !opens_with_forbid {
                return Some(format!("crate root `{path}' does not forbid unsafe"));
            }
        }
        (roots == 0).then(|| "writes no crate root".to_string())
    }

    /// A TRIPWIRE, deliberately not a gate: the rostered recipes still cannot
    /// start a process.
    ///
    /// The gate is the roster, because inferring this from recipe text was
    /// tried and could not be made sound — see `RUST_NOT_A_COMMAND_SURFACE`.
    /// The same reading is safe HERE because it decides nothing: at worst it
    /// complains about a recipe that is fine, where as a gate it would have
    /// exempted one that is not. What it buys is that td-txt growing a
    /// `Command` reds a test naming the reason, instead of quietly making a
    /// `find` in its source meaningful again.
    #[test]
    fn the_rostered_recipes_still_cannot_spawn() {
        for (stem, recipe) in catalog::all() {
            if !RUST_NOT_A_COMMAND_SURFACE.iter().any(|(name, _)| *name == stem) {
                continue;
            }
            if let Some(why) = spawn_tripwire(&recipe) {
                panic!(
                    "rostered recipe `{stem}': {why} — its `.rs' bodies are \
                     exempt from the find/xargs scan and must not be able to \
                     spawn"
                );
            }
        }

        // ...and the check FIRES, which asserting it over the catalog alone
        // could never show.
        let rs = |path: &str, content: &str| Step::WriteFile {
            path: path.into(),
            content: content.into(),
            exec: false,
        };
        const ROOT: &str = "#![forbid(unsafe_code)]\nfn main() {}\n";
        for (steps, want) in [
            (
                vec![rs("{src}/main.rs", ROOT), rs("{src}/go.rs", "Command::new(x)")],
                "names `Command'",
            ),
            (
                vec![rs("{src}/main.rs", "#![deny(unsafe_code)]\nfn main() {}\n")],
                "does not forbid unsafe",
            ),
            (
                vec![rs("{src}/main.rs", "//! docs\n/*\n#![forbid(unsafe_code)]\n*/\n")],
                "does not forbid unsafe",
            ),
            (
                vec![rs(
                    "{src}/main.rs",
                    "#![forbid(dead_code)] // unsafe_code\nfn main() {}\n",
                )],
                "does not forbid unsafe",
            ),
            (vec![rs("{src}/go.rs", "fn go() {}")], "writes no crate root"),
            (
                vec![
                    rs("{src}/main.rs", ROOT),
                    Step::Run {
                        argv: vec!["sh".into(), "{src}/probe.rs".into()],
                        env: Vec::new(),
                        dir: String::new(),
                    },
                ],
                "is handed to a program that is not rustc",
            ),
        ] {
            let recipe = Recipe::gnu("probe", "1").steps(steps);
            let why = spawn_tripwire(&recipe).unwrap_or_default();
            assert!(why.contains(want), "expected `{want}', got `{why}'");
        }

        // ...and does NOT fire on a forbid naming more than one lint, which
        // reading the parens too strictly would break.
        let many = Recipe::gnu("probe", "1").steps(vec![rs(
            "{src}/main.rs",
            "#![forbid(unsafe_code, dead_code)]\nfn main() {}\n",
        )]);
        assert_eq!(spawn_tripwire(&many), None);
    }

    /// The `.rs` rule above is narrow in the direction that matters: a staged
    /// Rust source that SPAWNS the host tool still reds, and the rule reaches
    /// no file type but `.rs`.
    #[test]
    fn a_rust_body_is_scanned_for_a_spawned_name_and_not_for_a_word() {
        let rust = |content: &str| Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: content.into(),
            exec: false,
        };
        let scan = |step: &Step, text: &str| step_invokes(step, text, "find");

        for benign in [
            "SOURCES.iter().find(|(name, _)| *name == module)",
            "// the peer is the worst place to find out",
            "let found = xs.iter().position(|x| *x == needle);",
            // A quote in a comment must not open a literal that swallows the
            // rest of the body, and a lifetime must not read as a char.
            "// a \" in a comment\nlet s: &'a [u8] = b\"ok\";\nxs.find(&y)",
            "/* find */ let n = xs.iter().find(|x| **x == 1);",
        ] {
            assert!(
                !scan(&rust(benign), benign),
                "a Rust body must not red on `{benign}'"
            );
        }

        // The command is named beside each body rather than sniffed out of it:
        // an escaped spelling is exactly what the source text does NOT contain.
        for (spawning, cmd) in [
            (r#"Command::new("find").arg("/").status()"#, "find"),
            (r#"let argv = ["find", "."];"#, "find"),
            (r#"Command::new("/usr/bin/find")"#, "find"),
            (r#"Command::new("xargs")"#, "xargs"),
            // Buried in a longer string rather than against a quote: spawning
            // `sh -c` with a host tool inside it is the ingress this exists
            // for, and a rule that only looked next to a quote missed it.
            (
                r#"Command::new("sh").arg("-c").arg("cd /x && find . -delete")"#,
                "find",
            ),
            (r#"let script = "set -e\nfind /x -type f\n";"#, "find"),
            (r#"let script = "ls | xargs rm";"#, "xargs"),
            // A comment sits above a real invocation often enough that the two
            // must not be able to mask each other.
            ("// nothing to find here\nCommand::new(\"find\")", "find"),
            // A char literal holding a quote must not open a string: if it
            // does, the literal after it is read as ordinary text and its
            // command name is never seen.
            ("let _ = '\\\"'; Command::new(\"find\");", "find"),
            (
                "let q = '\\''; let r = '\\\\'; Command::new(\"find\");",
                "find",
            ),
            // Spelled in escapes rather than assembled, so the name IS one
            // literal and the scan has to decode it to see it.
            (r#"Command::new("\x66ind")"#, "find"),
            (r#"Command::new("\u{78}args")"#, "xargs"),
            // A malformed `\u` must not swallow the text up to some later
            // brace: the escape is bounded, so the name after it still reads.
            (r#"let s = "\u no brace"; Command::new("find"); let t = "}";"#, "find"),
        ] {
            assert!(
                step_invokes(&rust(spawning), spawning, cmd),
                "a Rust body must red on `{spawning}'"
            );
        }

        // The same text in a Makefile gets the bare-token scan, unchanged.
        let method = "clean:\n\tfind . -name '*.o' -delete\n";
        let makefile = Step::WriteFile {
            path: "Makefile".into(),
            content: method.into(),
            exec: false,
        };
        assert!(
            scan(&makefile, method),
            "the Rust rule must not reach a non-Rust body"
        );
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
        // a substitution. A bare English word in a comment is one of them, and
        // in a SCRIPT it has to be: nothing there separates the two. A `.rs`
        // body does, which is what `rust_source_invokes` is for.
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
    /// A `payload_inputs` on a recipe whose build system cannot READ one is caught
    /// here, at eval, rather than at drv assembly.
    ///
    /// `td-builder` refuses it too — that refusal is the enforcement — but only
    /// once a build is attempted. `Recipe::gnu("x", "1").payload_inputs(&["y"])`
    /// otherwise compiles, emits, round-trips and passes every check in this crate,
    /// so the author learns at build time about a mistake visible at eval time.
    /// `mesboot` has the typed data steps (`unpack`, `copyTree`, and
    /// `stageRuntimeClosure`) that resolve `{payload:NAME}`. Every application
    /// build system also has the outer spec compiler as one typed consumer.
    fn payload_is_misplaced(recipe: &crate::types::Recipe) -> bool {
        let mesboot = matches!(recipe.build_system, crate::types::BuildSystem::Mesboot);
        let application_payload_is_misplaced = match (
            recipe.application.as_ref(),
            recipe.payload_inputs.as_ref(),
        ) {
            (Some(application), Some(payloads)) if !mesboot => {
                payloads.len() != 1
                    || payloads.first().map(String::as_str) != Some(application.runtime())
            }
            (Some(_), None) if !mesboot => true,
            (None, Some(_)) if !mesboot => true,
            _ => false,
        };
        (recipe.is_foreign_source() && !mesboot)
            || application_payload_is_misplaced
    }

    #[test]
    fn payloads_require_mesboot_data_steps_or_the_application_compiler() {
        // The predicate first, over recipes built for it. Nothing in the catalog
        // declares a payload yet, so sweeping the catalog alone filters an EMPTY
        // set and would pass with the rule spelled backwards.
        assert!(payload_is_misplaced(
            &crate::types::Recipe::gnu("x", "1").payload_inputs(&["y"])
        ));
        assert!(!payload_is_misplaced(
            &crate::types::Recipe::mesboot("x", "1").payload_inputs(&["y"])
        ));
        let application = td_engine::application::ApplicationDeclaration::new(
            "empty-runtime",
            "/app/bin/x",
        )
        .unwrap();
        assert!(!payload_is_misplaced(
            &crate::types::Recipe::gnu("x", "1")
                .payload_inputs(&["empty-runtime"])
                .application(application.clone())
        ));
        assert!(payload_is_misplaced(
            &crate::types::Recipe::gnu("x", "1")
                .payload_inputs(&["empty-runtime", "extra"])
                .application(application.clone())
        ));
        assert!(payload_is_misplaced(
            &crate::types::Recipe::gnu("x", "1").application(application)
        ));
        assert!(payload_is_misplaced(
            &crate::types::Recipe::gnu("x", "1").source_input("ripgrep-seed-source")
        ));
        assert!(!payload_is_misplaced(&crate::types::Recipe::gnu("x", "1")));
        let misplaced: Vec<&str> = catalog::all()
            .iter()
            .filter(|(_, recipe)| payload_is_misplaced(recipe))
            .map(|(stem, _)| *stem)
            .collect();
        assert!(
            misplaced.is_empty(),
            "payload_inputs requires mesboot typed data steps or an application spec compiler \
             (APPLICATIONS.md section B.8): {misplaced:?}"
        );
    }

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
