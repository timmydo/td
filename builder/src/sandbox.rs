//! The S3 build sandbox: execute a parsed `.drv` in a fresh user namespace,
//! replicating the pinned daemon's guest-visible contract (read off
//! nix/libstore/build.cc):
//!   - namespaces: NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS. NEWNET makes the
//!     build offline by construction; NEWPID (in the same unshare as NEWUSER, so
//!     the PID ns is owned by the new user ns) forks the builder to PID 1 of its
//!     own pid namespace with a FRESH procfs — the build sees only its own process
//!     tree, not the host's (the daemon, other concurrent builds, their
//!     /proc/<pid>/environ), full parity with host_shell / `guix shell -C`;
//!   - uid/gid: guest 30001/30000 mapped over the invoking user, setgroups
//!     denied (build.cc defaultGuestUID/GID, initializeUserNamespace);
//!   - chroot: the build pivot_roots into a MINIMAL fresh-tmpfs root holding only
//!     the staged /gnu/store, a writable /tmp, /dev rbind'd from the invoking
//!     namespace, a fresh /proc, and a minimal /etc — nothing else of the host
//!     filesystem. So `build` is SELF-hermetic, not dependent on the outer
//!     host-sandbox to hide /etc, /home, /usr, /var/guix … from the builder
//!     (own-builder-daemon: self-hermetic build sandbox);
//!   - store: every closure item bind-mounted into a staged directory which
//!     is then rbind-mounted over the new root's /gnu/store, so the builder sees
//!     real store paths while writes land in the scratch directory (the rootless
//!     rung's mechanics) and the bound inputs stay protected by their host-root
//!     ownership;
//!   - build dir: a fresh tmpfs /tmp with /tmp/guix-build-<drvname>-0 (0700,
//!     <drvname> keeps the .drv suffix), cwd there;
//!   - env: cleared, then PATH/HOME/NIX_STORE/NIX_BUILD_CORES, the drv's
//!     env, then NIX_BUILD_TOP/TMPDIR/TEMPDIR/TMP/TEMP/PWD — build.cc's
//!     exact set and override order (the TMPDIR group wins over drv env).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
#![allow(unsafe_code)] // confined raw-syscall / low-level layer (AGENTS.md)

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::drv::Derivation;
use crate::sys;

/// The store prefix WITH a trailing slash (e.g. `/gnu/store/`, or `/td/store/` when
/// `TD_STORE_DIR` selects td's own store). Every store-path operation strips/joins this
/// so a build targeting `/td/store` stages its inputs and writes its outputs there
/// NATIVELY — no post-hoc `/gnu/store -> /td/store` byte rewrite. The prefix is part of
/// the content hash (`store::make_store_path_in`), so a `/td/store` build is a distinct,
/// guix-independent store, not a relabel of a `/gnu/store` one.
fn store_prefix() -> String {
    format!("{}/", crate::store::store_dir())
}
const GUEST_UID: u32 = 30001;
const GUEST_GID: u32 = 30000;

fn err(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, what)
}

/// Map exactly one uid/gid pair into a user namespace already entered via
/// `unshare(2)`/`CLONE_NEWUSER` (a separate call — its flags and failure handling differ
/// per caller, so this covers only the part that's IDENTICAL everywhere: the ordering-
/// sensitive id-mapping triplet). Order matters: `setgroups` MUST be denied BEFORE the
/// `gid_map` write — the kernel refuses an unprivileged gid_map write otherwise
/// (CVE-2014-8989). `host_uid`/`host_gid` are the real ids as seen from OUTSIDE the
/// namespace (the map's second column); `uid_target`/`gid_target` are what the process
/// appears as INSIDE it (the map's first column).
pub fn map_userns_id(host_uid: u32, host_gid: u32, uid_target: u32, gid_target: u32) -> io::Result<()> {
    fs::write("/proc/self/setgroups", "deny")?;
    fs::write("/proc/self/uid_map", format!("{uid_target} {host_uid} 1"))?;
    fs::write("/proc/self/gid_map", format!("{gid_target} {host_gid} 1"))?;
    Ok(())
}

/// A closure entry is either a bare CANONICAL store path or `CANONICAL\tON-DISK`.
/// The canonical half is the `/gnu/store/<base>` path the build must SEE; the
/// on-disk half is where the tree physically lives on the host to bind FROM. They
/// differ only for a td-interned item (e.g. a source td restored into its OWN store
/// dir, never registered with the daemon) — every daemon-resident item is a bare
/// path, so on-disk defaults to canonical. This keeps a td-owned store reachable by
/// the sandbox with no extra argument, the encoding riding through `closure.txt`.
pub fn split_closure_entry(entry: &str) -> (&str, &str) {
    match entry.split_once('\t') {
        Some((canonical, on_disk)) => (canonical, on_disk),
        None => (entry, entry),
    }
}

/// build.cc storePathToName: strip the store dir and the 32-char base32
/// hash + dash. For a drv path the result KEEPS the .drv suffix.
/// Pure core: `prefix` is the store dir WITH trailing slash, so this is testable for
/// any store (`/gnu/store/` or `/td/store/`) without touching process env.
fn store_path_name_in<'a>(prefix: &str, path: &'a str) -> io::Result<&'a str> {
    let base = path
        .strip_prefix(prefix)
        .ok_or_else(|| err(format!("{path}: not a store path")))?;
    if base.len() > 33 && base.as_bytes()[32] == b'-' && !base.contains('/') {
        Ok(&base[33..])
    } else {
        Err(err(format!("{path}: malformed store path basename")))
    }
}

/// Strip the active store dir + hash, yielding the path name (`store::store_dir()`-aware).
pub fn store_path_name(path: &str) -> io::Result<&str> {
    store_path_name_in(&store_prefix(), path)
}

/// Per-build leaf-cgroup names are unique within this process via a counter
/// alongside the pid (the build daemon realizes drvs serially in one process).
static CGROUP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Parse `TD_BUILD_MEM_MAX` into a per-build byte cap. Accepts a bare integer
/// (bytes) or an integer with a `K`/`M`/`G` suffix (1024-based, case-insensitive,
/// optional space). An absent, empty, zero, or unparseable value yields None —
/// the cap is **OFF by default**, so the loop can never go spuriously red. A cap
/// is reproducibility-safe like `nice`: a build that exceeds it FAILS, it never
/// produces different bytes.
fn parse_mem_max(raw: Option<String>) -> Option<u64> {
    let s = raw?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1024_u64),
        Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let bytes = num.trim().parse::<u64>().ok()?.checked_mul(mult)?;
    (bytes != 0).then_some(bytes)
}

/// Best-effort true-RSS cap. When the operator delegates a writable cgroup2 dir
/// via `TD_BUILD_CGROUP`, create a per-build leaf cgroup with `memory.max = cap`
/// and return it; the build child joins it before unsharing and the parent
/// removes it afterward. td uses a DELEGATED cgroup the way a kubelet hands a
/// container its own — it does not try to conjure one inside the read-only,
/// rootless loop sandbox, where this returns None and the `setrlimit` backstop
/// alone applies. Any failure (no delegation, RO cgroupfs, EBUSY, missing
/// controller) warns and degrades to that backstop.
fn setup_build_cgroup(cap: u64) -> Option<PathBuf> {
    let base = std::env::var("TD_BUILD_CGROUP").ok().filter(|s| !s.is_empty())?;
    let base = PathBuf::from(base);
    if !base.is_dir() {
        sys::warn(b"td-builder: TD_BUILD_CGROUP is not a directory; memory cap uses the rlimit backstop\n");
        return None;
    }
    // The memory controller must be delegated to children for the leaf's
    // memory.max to bind; harmless if already enabled (an empty delegated base).
    let _ = fs::write(base.join("cgroup.subtree_control"), "+memory");
    let seq = CGROUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let leaf = base.join(format!("td-build-{}-{}", std::process::id(), seq));
    if let Err(e) = fs::create_dir(&leaf) {
        if e.kind() != io::ErrorKind::AlreadyExists {
            sys::warn(b"td-builder: could not create build cgroup; memory cap uses the rlimit backstop\n");
            return None;
        }
    }
    if fs::write(leaf.join("memory.max"), cap.to_string()).is_err() {
        sys::warn(b"td-builder: could not set cgroup memory.max; memory cap uses the rlimit backstop\n");
        let _ = fs::remove_dir(&leaf);
        return None;
    }
    Some(leaf)
}

/// Cap `cmd`'s child — and everything it forks/execs — at `bytes` of data
/// segment via a pre_exec setrlimit(RLIMIT_DATA). td's own prlimit(1)
/// replacement for the gate-runner per-process memory backstop (#319): no
/// host util-linux binary, so it works inside the loop sandbox. The requested
/// cap is clamped to the ambient HARD limit (raising a hard limit is EPERM
/// for an unprivileged process, and a host whose hard limit sits below the
/// default cap would otherwise red every gate with an opaque spawn error —
/// review finding; the tighter-than-requested cap is still fail-closed).
/// A refused setrlimit still fails the spawn (the gate reds) rather than
/// running the body uncapped.
pub fn cap_child_data_rlimit(cmd: &mut Command, bytes: u64) {
    let bytes = match sys::get_rlimit(sys::RLIMIT_DATA) {
        Ok((_, hard)) => bytes.min(hard),
        Err(_) => bytes,
    };
    // Post-fork safe: set_rlimit is one raw syscall; its error path is
    // io::Error::from_raw_os_error (no allocation).
    unsafe {
        cmd.pre_exec(move || sys::set_rlimit(sys::RLIMIT_DATA, bytes, bytes));
    }
}

/// WHO issued the authority for a staged input's hash — its provenance CLASS
/// (re #469). Integrity and provenance are distinct: integrity is "the bytes
/// match a recorded hash"; provenance is "the authority that recorded that
/// hash is allowed to do so". An `InputOrigin` is constructed ONLY at the
/// planner's typed db-intake sites — the plan's seed db, a prior step's
/// td.db, a td-interned source/vendor placement db, the stage0 builder
/// placement db, the daemon's blessed seed-lock closure db — each declared
/// with its class in code where the planner hands it over. A raw path,
/// environment variable, database row, or cache file can locate bytes, but
/// no production function turns one directly into an `InputOrigin`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputOrigin {
    /// A pinned, hash-verified seed registration: the interned seed db whose
    /// entries the compiled seed-digest table gated at synthesis, or a
    /// td-interned source/vendored-crate placement db (a declared
    /// fixed-output fetch td restored itself).
    AuditedSeed,
    /// A prior td recipe output: a build-plan step's td.db row, written by
    /// the engine after that step's own verified build (deriver recorded).
    RecipeOutput,
    /// The control-plane td-builder staged as the drv's builder — the stage0
    /// placement db `store-add-builder` wrote for the binary driving this
    /// build.
    ControlPlaneBuilder,
    /// The daemon flow's blessed seed-lock closure: the `seed-bless` db over
    /// the REPO-DECLARED roots (`seed_lock_roots`), hashed once per root set.
    BlessedSeedClosure,
}

impl InputOrigin {
    /// The audit token (`provenance.manifest`'s origin column).
    pub fn as_str(self) -> &'static str {
        match self {
            InputOrigin::AuditedSeed => "audited-seed",
            InputOrigin::RecipeOutput => "recipe-output",
            InputOrigin::ControlPlaneBuilder => "control-plane-builder",
            InputOrigin::BlessedSeedClosure => "blessed-seed-closure",
        }
    }
}

/// One staged input's authority record: the expected NAR hash
/// (`sha256:<hex>`, the `ValidPaths.hash` wire format) plus WHO issued it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StagedInput {
    pub nar_hash: String,
    pub origin: InputOrigin,
}

/// The staged-input provenance manifest (re #469): canonical store path →
/// (expected NAR hash, origin class), assembled by the planner from TYPED
/// td-owned store DBs ONLY (interned-seed registrations, prior build-plan
/// steps' td.dbs, the source/builder placement dbs). EVERY build carries one —
/// there is no non-strict mode: each closure item must have a record and its
/// on-disk bytes must hash to it, or it refuses to stage. A caller-supplied
/// store DIRECTORY is thereby a byte source, never an authority: the bytes
/// bind only if a td-owned registration vouches for them.
///
/// Cost, decided deliberately: verification re-hashes every closure item at
/// EVERY strict step — O(closure bytes) per step, not amortized. The bootstrap
/// rungs are small, a streaming SHA-256 is cheap next to the build it guards,
/// and trusting a prior step's verification is exactly the assume-the-cache
/// hole this manifest exists to close.
pub type StageManifest = std::collections::BTreeMap<String, StagedInput>;

/// Stream-hash a tree/file in NAR form — `sha256:<hex>`, the `ValidPaths.hash`
/// wire format every td store registration records. `pub(crate)` because the
/// loop-userland cache (check_loop.rs) verifies its durable items with the
/// same hash before mounting them.
pub(crate) fn nar_hash_of(path: &Path) -> io::Result<String> {
    struct W(crate::sha256::Sha256);
    impl io::Write for W {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut w = W(crate::sha256::Sha256::new());
    crate::nar::write_nar(&mut w, path)?;
    Ok(format!("sha256:{}", crate::sha256::to_base16(&w.0.finalize())))
}

/// Verify ONE closure item against the provenance manifest — split out of
/// `build` so the rejection paths unit-test without a namespace. Refuses (a)
/// an item no td-owned db vouches for and (b) on-disk bytes that do not hash
/// to the recorded NAR hash.
pub fn verify_staged_item(
    manifest: &StageManifest,
    canonical: &str,
    on_disk: &str,
) -> io::Result<()> {
    let Some(want) = manifest.get(canonical) else {
        return Err(err(format!(
            "provenance rejected: closure item {canonical} has no td-owned store-db record — refusing to stage it (re #469)"
        )));
    };
    let got = nar_hash_of(Path::new(on_disk))?;
    if got != want.nar_hash {
        return Err(err(format!(
            "provenance rejected: closure item {canonical} (on disk {on_disk}) hashes {got} but its td-owned registration ({}) records {} — refusing to stage tampered bytes (re #469)",
            want.origin.as_str(),
            want.nar_hash
        )));
    }
    Ok(())
}

/// Run the drv's builder inside the namespace sandbox. `closure` lists every
/// store path the build may see (the staged store's contents); `scratch` is
/// a writable host directory. `manifest` is the #469 staging gate — REQUIRED,
/// not optional: no engine path may stage inputs a td-owned db does not vouch
/// for. Every closure item is provenance-verified (`verify_staged_item`)
/// BEFORE its bind target is staged; the item binds are then locked
/// READ-ONLY in the child, so the verified bytes cannot be rewritten through
/// a live bind for the build's duration (the hash runs in the parent and the
/// mount in the child, so the lock — not the hash — is what holds after
/// staging). On success returns (output name, host-side path under
/// scratch/newstore) for every drv output, each verified to exist.
pub fn build(
    drv: &Derivation,
    drv_path: &str,
    closure: &[String],
    scratch: &Path,
    manifest: &StageManifest,
) -> io::Result<Vec<(String, PathBuf)>> {
    if drv.platform != "x86_64-linux" {
        return Err(err(format!(
            "platform `{}' is not x86_64-linux — refusing to build",
            drv.platform
        )));
    }

    // The active store dir (default /gnu/store; /td/store under TD_STORE_DIR). Every
    // closure path the build SEES is under this prefix, the new root mounts its store
    // here, and NIX_STORE points at it — so a /td/store build is native, not rewritten.
    let store_dir_str = crate::store::store_dir();
    let store_prefix = format!("{store_dir_str}/");

    // Stage the bind targets in the parent (plain file ops on our scratch);
    // the mounts themselves happen in the child's namespace.
    let newstore = scratch.join("newstore");
    fs::create_dir_all(&newstore)?;
    let mut binds: Vec<(CString, CString)> = Vec::with_capacity(closure.len());
    for entry in closure {
        // CANONICAL is the store path the build SEES; ON-DISK is where to bind FROM
        // (== canonical for daemon-resident items, a td store dir for td-interned ones).
        let (canonical, on_disk) = split_closure_entry(entry);
        verify_staged_item(manifest, canonical, on_disk)?;
        let meta = fs::symlink_metadata(on_disk)
            .map_err(|e| err(format!("closure item {canonical} (on disk {on_disk}): {e}")))?;
        // BASENAME-keyed: a closure can span MULTIPLE store prefixes (/gnu/store deps +
        // /td/store td-built deps, e.g. a chained toolchain — brick 8). Each item is staged
        // flat under newstore/<base> (store hashes are unique); newstore is then mounted at
        // EVERY prefix the closure spans below, so /gnu/store/<b> and /td/store/<b> both
        // resolve to their item. For a single-prefix closure this is exactly the old layout.
        let base = canonical
            .rsplit('/')
            .next()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| err(format!("closure item {canonical}: not a store path")))?;
        let target = newstore.join(base);
        if meta.is_dir() {
            fs::create_dir_all(&target)?;
        } else if meta.is_file() {
            fs::File::create(&target)?;
        } else {
            // A symlink cannot be bind-mounted; no pinned-channel closure
            // has top-level symlink store items — refuse rather than guess.
            return Err(err(format!("closure item {canonical}: unsupported file type")));
        }
        binds.push((
            CString::new(on_disk).map_err(|_| err(format!("{on_disk}: NUL in path")))?,
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
        ));
    }

    // The build dir is `guix-build-<drvName>-0`. For a store-path drv that is
    // storePathToName(drvPath). For an emitted `.drv` handed in from outside the
    // store (td-drv-build builds the file td WROTE), derive the same name from the
    // first output's store name + ".drv" (drvName == outName + ".drv" for these
    // single-output subjects). Store-path inputs (the td-builder rung) are
    // unaffected — the first branch still wins.
    let drv_name = match store_path_name(drv_path) {
        Ok(n) => n.to_string(),
        Err(_) => {
            let out0 = drv
                .outputs
                .first()
                .ok_or_else(|| err("derivation has no outputs".into()))?;
            format!("{}.drv", store_path_name(&out0.path)?)
        }
    };
    let build_dir = format!("/tmp/guix-build-{}-0", drv_name);
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    // A fresh tmpfs becomes the build's MINIMAL root: the staged /gnu/store, a
    // writable /tmp, a minimal /dev, a fresh /proc and a minimal /etc — and
    // NOTHING ELSE of the host filesystem. Without this pivot the build inherited
    // the invoking root (only /gnu/store + /tmp overlaid), so /etc, /home, /usr …
    // leaked in and the build was hermetic ONLY when wrapped in the outer
    // host-sandbox. Pivoting here makes `build` SELF-hermetic (own-builder-daemon
    // track). The build now also unshares NEWPID and forks the builder to PID 1 of
    // its own pid namespace; the /proc mounted below is a FRESH procfs reflecting
    // that namespace, not the invoking one.
    let newroot = scratch.join("buildroot");
    fs::create_dir_all(&newroot)?;
    let cstr = |p: &Path| CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let newstore_c = cstr(&newstore);
    let root_c = CString::new("/").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    let procfs_c = CString::new("proc").unwrap();
    let newroot_c = cstr(&newroot);
    // The store dir INSIDE the new root, e.g. <newroot>/gnu/store or <newroot>/td/store
    // (store_dir_str is absolute; strip the leading '/' to make it root-relative).
    let store_dir = newroot.join(store_dir_str.trim_start_matches('/'));
    let store_dir_c = cstr(&store_dir);
    let tmp_dir = newroot.join("tmp");
    let tmp_dir_c = cstr(&tmp_dir);
    let dev_dir = newroot.join("dev");
    let dev_dir_c = cstr(&dev_dir);
    let proc_dir = newroot.join("proc");
    let proc_dir_c = cstr(&proc_dir);
    let etc_dir = newroot.join("etc");
    let etc_passwd = etc_dir.join("passwd");
    let etc_group = etc_dir.join("group");
    let oldroot_rel = newroot.join("oldroot");
    let oldroot_rel_c = cstr(&oldroot_rel);
    let oldroot_abs_c = CString::new("/oldroot").unwrap();
    // EXTRA store prefixes the closure spans beyond the active one (e.g. /td/store toolchain
    // inputs in a /gnu/store-native corpus build — brick 8). newstore is rbind'd at each of
    // these too, so those inputs are visible at their canonical prefix. Empty for the common
    // single-store build → the mount sequence below is unchanged.
    let mut extra_prefixes: Vec<String> = closure
        .iter()
        .map(|e| split_closure_entry(e).0)
        .filter_map(|c| c.rsplit_once('/').map(|(d, _)| d.to_string()))
        .filter(|d| *d != store_dir_str)
        .collect();
    extra_prefixes.sort();
    extra_prefixes.dedup();
    let extra_store_dirs: Vec<PathBuf> = extra_prefixes
        .iter()
        .map(|p| newroot.join(p.trim_start_matches('/')))
        .collect();
    let extra_store_cs: Vec<CString> = extra_store_dirs.iter().map(|d| cstr(d)).collect();
    // /dev is rbind'd whole from the invoking namespace rather than rebuilt node by
    // node: re-binding a device node onto a fresh unprivileged-userns tmpfs strips
    // device access (the re-bound /dev/null returns EACCES on write), whereas an
    // rbind preserves the source mount's working device binds. In the loop the
    // source is host_shell's ALREADY-minimal /dev (null/zero/…/shm/pts, no host
    // device tree); a future standalone daemon would reuse that minimal-/dev builder.
    let dev_src_c = CString::new("/dev").unwrap();
    // Minimal /etc (daemon build-chroot parity): passwd + group so getpwuid/getgrgid
    // resolve the build user, with NO host /etc reachable.
    let passwd_body = format!(
        "root:x:0:0:System administrator:/:/noshell\n\
         nixbld:x:{GUEST_UID}:{GUEST_GID}:Build user:/build-top:/noshell\n\
         nobody:x:65534:65534:Nobody:/:/noshell\n"
    );
    let group_body = format!("root:x:0:\nnixbld:x:{GUEST_GID}:\nnogroup:x:65534:\n");
    let build_dir_owned = build_dir.clone();

    // Per-build resource caps (opt-in via TD_BUILD_MEM_MAX; OFF by default).
    // The cgroup leaf — when an operator delegates one via TD_BUILD_CGROUP — is
    // a true RSS cap; the setrlimit(RLIMIT_DATA) backstop applied in pre_exec
    // works everywhere (rootless, CI). Both are inherited onto the PID-1 builder.
    let mem_cap = parse_mem_max(std::env::var("TD_BUILD_MEM_MAX").ok());
    let cgroup_leaf = mem_cap.and_then(setup_build_cgroup);
    let cgroup_procs = cgroup_leaf.as_ref().map(|d| d.join("cgroup.procs"));

    let mut cmd = Command::new(&drv.builder);
    cmd.args(&drv.args);
    cmd.env_clear();
    // build.cc's exact assembly order; Command's env map gives the same
    // override semantics (later set wins).
    cmd.env("PATH", "/path-not-set");
    cmd.env("HOME", "/homeless-shelter");
    cmd.env("NIX_STORE", &store_dir_str);
    cmd.env("NIX_BUILD_CORES", "1");
    for (k, v) in &drv.env {
        cmd.env(k, v);
    }
    for k in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        cmd.env(k, &build_dir);
    }

    unsafe {
        cmd.pre_exec(move || {
            // Arm parent-death reaping before anything else: if the outer
            // td-builder dies during setup, this process is SIGKILLed rather than
            // left running. (Still in the outer PID namespace here, so getppid is
            // meaningful; the re-check closes the parent-died-mid-setup race.)
            let parent = sys::getppid();
            sys::set_pdeathsig(sys::SIGKILL)?;
            if sys::getppid() != parent {
                sys::exit_group(0);
            }
            // Per-build memory caps, applied BEFORE the unshare (host cgroupfs is
            // still writable as the invoking user) and BEFORE the fork (so the
            // PID-1 builder and its whole tree inherit them). Best-effort: a cap
            // can only make a build FAIL, so a setup hiccup warns and continues
            // rather than killing the build — never silently weakens isolation.
            if let Some(cap) = mem_cap {
                // True RSS cap: join the delegated leaf cgroup (memory.max set in
                // the parent). cgroup membership survives the unshare+fork below.
                if let Some(procs) = &cgroup_procs {
                    if fs::write(procs, format!("{}\n", std::process::id())).is_err() {
                        sys::warn(b"td-builder: could not join build cgroup; rlimit backstop only\n");
                    }
                }
                // Portable backstop (rootless, CI): cap the data segment.
                if sys::set_rlimit(sys::RLIMIT_DATA, cap, cap).is_err() {
                    sys::warn(b"td-builder: could not set RLIMIT_DATA build memory cap\n");
                }
            }
            // New USER + PID + mount + net + IPC + UTS namespaces. NEWPID rides in
            // the SAME unshare as NEWUSER so the new PID namespace is owned by the
            // new user namespace; the fork below then lands the builder at PID 1 of
            // that namespace, where a fresh /proc reflects only the build's own
            // process tree — the host's processes (the daemon, other concurrent
            // builds, their /proc/<pid>/environ) are no longer visible or signalable.
            sys::unshare(
                sys::CLONE_NEWUSER
                    | sys::CLONE_NEWNS
                    | sys::CLONE_NEWPID
                    | sys::CLONE_NEWNET
                    | sys::CLONE_NEWIPC
                    | sys::CLONE_NEWUTS,
            )?;
            // Map the guest ids before touching anything else so file
            // creation below happens as 30001/30000, not the overflow id.
            map_userns_id(host_uid, host_gid, GUEST_UID, GUEST_GID)?;
            // Fork: the child is PID 1 of the new PID namespace and does the mount
            // setup + (via std) exec of the builder; THIS process (the PID-ns
            // parent, still in the outer PID ns) only waits for it and propagates
            // its exit. It must NOT fall through to std's exec path — the builder is
            // exec'd exactly once, as PID 1. Stdio is inherited, so output streams.
            let pid = sys::fork()?;
            if pid != 0 {
                let status = sys::waitpid(pid)?;
                let code = if status & 0x7f == 0 {
                    (status >> 8) & 0xff
                } else {
                    128 + (status & 0x7f)
                };
                sys::exit_group(code);
            }
            // --- PID 1 of the new PID namespace from here on ---
            // Re-arm parent-death reaping (fork cleared it): if the PID-ns parent
            // waiting above dies, PID 1 is SIGKILLed and the kernel tears down the
            // whole namespace, reaping the build. PDEATHSIG survives the execve.
            sys::set_pdeathsig(sys::SIGKILL)?;
            // Keep every mount below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)?;
            // Stage each closure item into newstore (host scratch, OUTSIDE the new
            // root), then rbind newstore over the new root's /gnu/store below.
            // Each INPUT bind is locked read-only immediately (remount of the
            // bind just created — load-bearing, so a failure is fatal): the
            // builder runs as the mapped owner uid and could otherwise write
            // straight through the live bind into the on-disk store the
            // manifest verified at staging, so the verify-then-bind boundary
            // would hold only for an instant (re #469). newstore itself stays
            // writable — outputs land as NEW entries beside the binds, never
            // through one — and the /gnu/store rbind below carries each
            // child's ro flag along.
            for (src, dst) in &binds {
                sys::mount(Some(src), dst, None, sys::MS_BIND, None)?;
                sys::mount(
                    None,
                    dst,
                    None,
                    sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY,
                    None,
                )?;
            }
            // The fresh minimal root, then its skeleton dirs.
            sys::mount(Some(&tmpfs_c), &newroot_c, Some(&tmpfs_c), 0, None)?;
            fs::create_dir_all(&store_dir)?;
            fs::create_dir_all(&tmp_dir)?;
            fs::create_dir_all(&dev_dir)?;
            fs::create_dir_all(&proc_dir)?;
            fs::create_dir_all(&etc_dir)?;
            fs::create_dir_all(&oldroot_rel)?;
            // Staged store → /gnu/store (rbind carries the per-item binds); outputs
            // the build writes under /gnu/store land in newstore on the host.
            sys::mount(Some(&newstore_c), &store_dir_c, None, sys::MS_BIND | sys::MS_REC, None)?;
            // … and at every EXTRA prefix the closure spans (e.g. /td/store toolchain inputs):
            // the SAME newstore (basename-keyed) rbind'd there too, so those canonical paths
            // resolve. Empty for a single-store build, so this is a no-op in the common case.
            for (i, dst) in extra_store_cs.iter().enumerate() {
                fs::create_dir_all(&extra_store_dirs[i])?;
                sys::mount(Some(&newstore_c), dst, None, sys::MS_BIND | sys::MS_REC, None)?;
            }
            // Writable build tmpfs.
            sys::mount(Some(&tmpfs_c), &tmp_dir_c, Some(&tmpfs_c), 0, None)?;
            // /dev rbind'd whole (preserves working device binds; see note above).
            sys::mount(Some(&dev_src_c), &dev_dir_c, None, sys::MS_BIND | sys::MS_REC, None)?;
            // A FRESH procfs reflecting the build's OWN pid namespace (we are PID 1),
            // not the invoking namespace's /proc.
            sys::mount(Some(&procfs_c), &proc_dir_c, Some(&procfs_c), 0, None)?;
            // Minimal /etc.
            fs::write(&etc_passwd, &passwd_body)?;
            fs::write(&etc_group, &group_body)?;
            // Pivot into the minimal root and drop the host root entirely.
            sys::pivot_root(&newroot_c, &oldroot_rel_c)?;
            std::env::set_current_dir("/")?;
            sys::umount2(&oldroot_abs_c, sys::MNT_DETACH)?;
            let _ = fs::remove_dir("/oldroot");
            // The build dir lives on the fresh /tmp tmpfs.
            fs::DirBuilder::new().mode(0o700).create(&build_dir_owned)?;
            std::env::set_current_dir(&build_dir_owned)?;
            Ok(())
        });
    }

    let status = cmd
        .status()
        .map_err(|e| err(format!("spawning builder {}: {e}", drv.builder)))?;
    // The build tree has exited, so the leaf cgroup is empty — tear it down
    // (best-effort) whether the build passed or failed, before any early return.
    if let Some(leaf) = &cgroup_leaf {
        let _ = fs::remove_dir(leaf);
    }
    if !status.success() {
        return Err(err(format!(
            "builder for {drv_path} failed: {status}"
        )));
    }

    let mut outputs = Vec::with_capacity(drv.outputs.len());
    for o in &drv.outputs {
        let host = newstore.join(
            o.path
                .strip_prefix(&store_prefix)
                .ok_or_else(|| err(format!("output {}: not a store path", o.path)))?,
        );
        fs::symlink_metadata(&host).map_err(|_| {
            err(format!(
                "builder exited 0 but did not produce output `{}' ({})",
                o.name, o.path
            ))
        })?;
        outputs.push((o.name.clone(), host));
    }
    Ok(outputs)
}

/// A host path to expose inside the loop sandbox (rbind-mounted at the same
/// path in the new root). `src` may be a directory or a regular file — the
/// mountpoint is created to match (a file store item, e.g. a pinned `.crate`,
/// binds onto a created empty file). `readonly` remounts it read-only after
/// binding.
pub struct Bind {
    pub src: String,
    /// Mount `src` at this absolute path inside the new root instead of at `src`
    /// (None ⇒ same path). Lets the user store at a host path (e.g. `~/.td/store`)
    /// appear at td's store prefix (`/td/store`) inside the sandbox — the
    /// own-root/store-ns case, breaking from guix's `/gnu/store`.
    pub dest: Option<String>,
    pub readonly: bool,
    /// When `readonly`, tolerate a FAILED read-only remount by DETACHING the bind
    /// (fail closed — no host-owned subtree left writable in the sandbox) instead
    /// of erroring. Set ONLY for defense-in-depth ro binds the kernel may forbid
    /// remounting in a child user namespace — e.g. `/sys/fs/cgroup` (cgroup2,
    /// owned by the host userns: a child userns lacks CAP_SYS_ADMIN over it, so
    /// MS_REMOUNT|MS_RDONLY → EPERM on some kernels, e.g. GitHub's azure runner).
    /// NEVER for binds whose read-only is load-bearing (the store): those still
    /// error on a failed remount.
    pub ro_optional: bool,
}

/// The loop-sandbox DEV-SHELL (vs. the build jail above): pivot into a fresh
/// tmpfs root that exposes ONLY `binds` (rbind, the same path inside), a
/// writable tmpfs at each of `tmpfs_dirs`, and a minimal synthetic `/dev` (the
/// standard char devices + shm + a private devpts + fd symlinks, matching
/// `guix shell -C` — NOT the host device tree); the host filesystem is otherwise
/// gone. `path_env` is the full PATH; an empty value stays empty. `home` is HOME;
/// `workdir` is the cwd to enter after pivot (empty → `/`); `extra_env` is
/// caller-preserved env (e.g. the `TD_SUBST_*`/`TD_DAEMON_*` knobs). Runs `cmd args` and
/// returns its exit status. Unshares
/// NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS and runs the command as PID 1 of the
/// new PID namespace with a private /proc mounted by that PID-1 process — full
/// `guix shell -C` parity, so nested containers (the loop-sandbox/loop-rung
/// equivalence oracle, the rootless rung) can create their own PID ns + /proc.
/// uid/gid use the IDENTITY map (host uid → itself) so the host daemon's
/// peer-cred check still sees the real host uid, and its own network namespace
/// (loopback brought up) matches `guix shell -C`'s offline posture.
///
/// `ro_dirs`: absolute in-sandbox directories to lock READ-ONLY (a recursive
/// self-bind, then a non-recursive ro remount of the top mount) after all
/// binds land — the tmpfs dirs that HOLD per-item bind mountpoints (e.g. the
/// seed store dir holding `--store-item` mounts). The items' own bind mounts
/// ride along visible and keep their own ro state; the parent dir itself
/// rejects entry creation afterwards, so an ACCIDENTAL write can't plant a
/// sibling next to the declared inputs. Not a security boundary against a
/// hostile gate: the gate body owns the sandbox's user/mount namespaces
/// (CAP_SYS_ADMIN inside) and can over-mount the parent — same trust model as
/// every mount in this sandbox. A listed dir that no bind created is skipped.
#[allow(clippy::too_many_arguments)]
pub fn host_shell(
    cmd: &str,
    args: &[String],
    binds: &[Bind],
    tmpfs_dirs: &[String],
    path_env: &str,
    home: &str,
    workdir: &str,
    extra_env: &[(String, String)],
    ro_dirs: &[String],
    scratch: &Path,
) -> io::Result<std::process::ExitStatus> {
    let newroot = scratch.join("root");
    fs::create_dir_all(&newroot)?;
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    // Precompute every CString in the parent (the child's pre_exec only does
    // syscalls + fs ops, mirroring `build` above).
    // tmpfs root/dirs are owned by uid 0 of the new userns by default; with the
    // identity uid map (below) that is unmapped, so set the owner explicitly to
    // the host uid/gid via the tmpfs `uid=/gid=` mount data — keeps the dirs
    // writable while the process stays the (non-root) host uid.
    let tmpfs_data = CString::new(format!("uid={host_uid},gid={host_gid}")).unwrap();
    let newroot_c = CString::new(newroot.as_os_str().as_encoded_bytes()).unwrap();
    let root_c = CString::new("/").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    // A FRESH procfs is mounted at <newroot>/proc by the PID-1 child (below), so
    // /proc reflects the sandbox's OWN PID namespace, not the host's. The host
    // /proc is no longer bound in (main.rs drops it from the exposure set).
    let proc_c = CString::new("proc").unwrap();
    let proc_target_dir = newroot.join("proc");
    let proc_target_c = CString::new(proc_target_dir.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_rel = newroot.join("oldroot");
    let oldroot_rel_c = CString::new(oldroot_rel.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_abs_c = CString::new("/oldroot").unwrap();

    // Everything the child's pre_exec needs per bind, precomputed in the
    // parent: the C paths, whether the source is a directory (the mountpoint
    // is created to MATCH — a regular-file source, e.g. a pinned `.crate`
    // store item, binds onto a created empty file, mirroring the mount
    // applet), and a NAMED failure line (the child can only sys::warn a
    // preformatted byte string — a bare "FAILED bind-mounting" with no path
    // made failures undiagnosable).
    struct BindSpec {
        src: CString,
        target_dir: PathBuf,
        target: CString,
        readonly: bool,
        ro_optional: bool,
        src_is_dir: bool,
        fail_msg: Vec<u8>,
    }
    let mut bind_specs: Vec<BindSpec> = Vec::with_capacity(binds.len());
    for b in binds {
        // Bind `src` at `dest` inside the new root (dest defaults to src).
        let inside = b.dest.as_deref().unwrap_or(&b.src);
        let target = newroot.join(inside.strip_prefix('/').unwrap_or(inside));
        bind_specs.push(BindSpec {
            src: CString::new(b.src.as_str())
                .map_err(|_| err(format!("{}: NUL in path", b.src)))?,
            target_dir: target.clone(),
            target: CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
            readonly: b.readonly,
            ro_optional: b.ro_optional,
            // An unreadable source keeps the directory default; the mount then
            // fails exactly as before, now with the path named.
            src_is_dir: fs::metadata(&b.src).map(|m| m.is_dir()).unwrap_or(true),
            fail_msg: format!(
                "td-builder host-sandbox: FAILED bind-mounting {} -> {inside}\n",
                b.src
            )
            .into_bytes(),
        });
    }
    // Read-only parent-dir remounts (see the doc comment): precomputed like the
    // binds — the in-sandbox path, its C string, and a named failure line.
    struct RoDirSpec {
        target_dir: PathBuf,
        target: CString,
        fail_msg: Vec<u8>,
    }
    let mut ro_dir_specs: Vec<RoDirSpec> = Vec::with_capacity(ro_dirs.len());
    for d in ro_dirs {
        let target = newroot.join(d.strip_prefix('/').unwrap_or(d));
        ro_dir_specs.push(RoDirSpec {
            target_dir: target.clone(),
            target: CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
            fail_msg: format!(
                "td-builder host-sandbox: FAILED ro-remounting the bind parent dir {d}\n"
            )
            .into_bytes(),
        });
    }
    // (target_dir, target_c) for each writable tmpfs mount.
    let mut tmpfs_specs: Vec<(PathBuf, CString)> = Vec::with_capacity(tmpfs_dirs.len());
    for d in tmpfs_dirs {
        let target = newroot.join(d.strip_prefix('/').unwrap_or(d));
        tmpfs_specs.push((
            target.clone(),
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
        ));
    }

    // Minimal /dev, precomputed. The old exposure rbind-mounted the WHOLE host
    // /dev read-write, leaking /dev/kmsg (kernel log), /dev/kvm, raw disks, input
    // devices and GPUs into the "hermetic" sandbox. Instead build a fresh tmpfs
    // populated with ONLY the device set `guix shell -C` exposes: the standard
    // char devices (BIND-mounted from the host — a child userns cannot mknod, so
    // only these named nodes are reachable), /dev/shm, a private devpts, and the
    // fd symlinks.
    let dev_dir = newroot.join("dev");
    let dev_dir_c = CString::new(dev_dir.as_os_str().as_encoded_bytes()).unwrap();
    let dev_data = CString::new(format!("uid={host_uid},gid={host_gid},mode=0755")).unwrap();
    let mut dev_node_specs: Vec<(CString, PathBuf, CString)> = Vec::new();
    for n in ["null", "zero", "full", "random", "urandom", "tty"] {
        let src = format!("/dev/{n}");
        if Path::new(&src).exists() {
            let target = dev_dir.join(n);
            dev_node_specs.push((
                CString::new(src).unwrap(),
                target.clone(),
                CString::new(target.as_os_str().as_encoded_bytes()).unwrap(),
            ));
        }
    }
    let dev_shm_dir = dev_dir.join("shm");
    let dev_shm_c = CString::new(dev_shm_dir.as_os_str().as_encoded_bytes()).unwrap();
    let dev_shm_data = CString::new(format!("uid={host_uid},gid={host_gid},mode=1777")).unwrap();
    let dev_pts_dir = dev_dir.join("pts");
    let dev_pts_c = CString::new(dev_pts_dir.as_os_str().as_encoded_bytes()).unwrap();
    let devpts_c = CString::new("devpts").unwrap();
    let devpts_data =
        CString::new(format!("newinstance,ptmxmode=0666,mode=0620,gid={host_gid}")).unwrap();
    // (symlink path under <newroot>/dev, its target). /dev/ptmx → the private pts
    // instance; the std-stream links point into the private /proc mounted below.
    let dev_symlinks: Vec<(PathBuf, &str)> = vec![
        (dev_dir.join("ptmx"), "pts/ptmx"),
        (dev_dir.join("fd"), "/proc/self/fd"),
        (dev_dir.join("stdin"), "/proc/self/fd/0"),
        (dev_dir.join("stdout"), "/proc/self/fd/1"),
        (dev_dir.join("stderr"), "/proc/self/fd/2"),
    ];

    let workdir = if workdir.is_empty() { "/" } else { workdir };
    let workdir_owned = workdir.to_string();

    let mut command = Command::new(cmd);
    command.args(args);
    command.env_clear();
    command.env("PATH", path_env);
    command.env("HOME", home);
    command.env("TMPDIR", "/tmp");
    command.env("TD_HOST_SANDBOX", "1");
    // Caller-preserved env (e.g. the TD_SUBST_*/TD_DAEMON_* knobs).
    for (k, v) in extra_env {
        command.env(k, v);
    }
    // Generic terminal/identity env the gate bodies may read (TERM for terminal
    // output; USER/LOGNAME for any per-user path). Harmless, and keeps behaviour
    // identical to the outer shell.
    for k in ["TERM", "USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(k) {
            command.env(k, v);
        }
    }

    unsafe {
        command.pre_exec(move || {
            // Arm parent-death reaping BEFORE anything else: if the outer
            // td-builder is killed (CI cancellation, a timeout, Ctrl-C) during or
            // after setup, this process is SIGKILLed instead of left running.
            // Re-checked just after, to close the race where the parent died
            // between the getppid and the prctl. (This level is still in the
            // outer PID namespace, so getppid is meaningful here.)
            let parent = sys::getppid();
            sys::set_pdeathsig(sys::SIGKILL)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED arming PR_SET_PDEATHSIG\n"); e })?;
            if sys::getppid() != parent {
                sys::exit_group(0);
            }
            // New USER + PID + mount + net + IPC + UTS namespaces. NEWPID is in
            // the SAME unshare as NEWUSER so the new PID namespace is OWNED by the
            // new user namespace (the kernel applies NEWUSER first); the fork
            // below then lands the command at PID 1 of that PID namespace, where a
            // private /proc reflects it — full parity with `guix shell -C`, so
            // nested containers (the loop-sandbox/loop-rung equivalence oracle and
            // the rootless rung) can create their own PID ns + /proc instead of
            // tripping over the host's root-owned PID 1.
            sys::unshare(
                sys::CLONE_NEWUSER
                    | sys::CLONE_NEWNS
                    | sys::CLONE_NEWPID
                    | sys::CLONE_NEWNET
                    | sys::CLONE_NEWIPC
                    | sys::CLONE_NEWUTS,
            )
            .map_err(|e| {
                sys::warn(b"td-builder host-sandbox: FAILED at unshare(NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS)\n");
                e
            })?;
            // IDENTITY map (host uid/gid → itself), exactly like `guix shell -C`:
            // the process stays the NON-root host uid inside, so file-permission
            // checks (e.g. sqlite's access(W_OK) on the root-owned store DB)
            // behave as on the host — a uid-0 map would make root bypass them and
            // then fail on the real write. tmpfs ownership is handled via the
            // `uid=/gid=` mount data instead. The daemon's SO_PEERCRED sees the
            // real host uid either way.
            map_userns_id(host_uid, host_gid, host_uid, host_gid)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mapping the identity uid/gid\n"); e })?;
            // Own network namespace (offline by construction, like `guix shell
            // -C`); bring its loopback up to match that posture.
            sys::bring_loopback_up()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED bringing loopback up\n"); e })?;
            // Fork: the child is PID 1 of the new PID namespace and goes on to set
            // up the mounts + exec the command; THIS process (the PID-ns parent,
            // still in the outer PID ns) only waits for it and propagates its exit
            // via exit_group. It must NOT fall through to std's exec path — the
            // command is exec'd exactly once, as PID 1. Stdio is inherited
            // directly, so output still streams; only the exit status flows here.
            let pid = sys::fork()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at fork\n"); e })?;
            if pid != 0 {
                let status = sys::waitpid(pid)?;
                let code = if status & 0x7f == 0 {
                    (status >> 8) & 0xff
                } else {
                    128 + (status & 0x7f)
                };
                sys::exit_group(code);
            }
            // --- PID 1 of the new PID namespace, from here on ---
            // Re-arm parent-death reaping FIRST (fork cleared it): if the
            // PID-namespace parent — the process waitpid-ing us just above — dies,
            // we (PID 1) are SIGKILLed, and the kernel then tears down the whole
            // PID namespace, reaping every descendant build/mount. PDEATHSIG
            // survives the upcoming execve, so the exec'd command stays covered.
            sys::set_pdeathsig(sys::SIGKILL)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED re-arming PR_SET_PDEATHSIG in pid 1\n"); e })?;
            // Everything below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at mount(/, MS_REC|MS_PRIVATE)\n"); e })?;
            // A fresh tmpfs is the new root (also makes it a mount point, which
            // pivot_root requires), owned by the host uid/gid.
            sys::mount(Some(&tmpfs_c), &newroot_c, Some(&tmpfs_c), 0, Some(&tmpfs_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting the tmpfs root\n"); e })?;
            // Expose each requested host path (rbind), read-only where asked.
            // The mountpoint matches the source kind: dir → dir, file → file.
            for spec in &bind_specs {
                if spec.src_is_dir {
                    fs::create_dir_all(&spec.target_dir)?;
                } else {
                    if let Some(parent) = spec.target_dir.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    if !spec.target_dir.exists() {
                        fs::File::create(&spec.target_dir)?;
                    }
                }
                sys::mount(Some(&spec.src), &spec.target, None, sys::MS_BIND | sys::MS_REC, None)
                    .map_err(|e| { sys::warn(&spec.fail_msg); e })?;
                if spec.readonly {
                    let ro = sys::mount(
                        None,
                        &spec.target,
                        None,
                        sys::MS_REMOUNT | sys::MS_BIND | sys::MS_REC | sys::MS_RDONLY,
                        None,
                    );
                    // A child userns cannot remount-ro a mount owned by the host
                    // userns (e.g. /sys/fs/cgroup → EPERM on the azure runner). For
                    // ro_optional binds, that failure is tolerated (fail closed
                    // instead of failing the whole sandbox) rather than fatal. For
                    // every other ro bind (the store) the read-only is load-bearing
                    // — a failed remount is fatal.
                    if let Err(e) = ro {
                        if spec.ro_optional {
                            // Can't make it read-only (a child userns cannot
                            // remount-ro a mount owned by the host userns, e.g.
                            // cgroup2 on the azure runner). Rather than leave the
                            // host subtree WRITABLE inside the "hermetic" sandbox,
                            // DETACH it — fail closed, nothing host-owned exposed.
                            // The only ro_optional bind is /sys/fs/cgroup (gate-run's
                            // per-gate memory-limit delegation, issue #328, reads the
                            // hierarchy structure); where the ro-remount succeeds
                            // (most local/dev hosts) it stays bound, so the leftover
                            // empty dir here is harmless.
                            sys::warn(b"td-builder host-sandbox: ro-remount not permitted for an ro_optional bind; detached (fail-closed, no host exposure)\n");
                            let _ = sys::umount2(&spec.target, sys::MNT_DETACH);
                        } else {
                            sys::warn(&spec.fail_msg);
                            sys::warn(b"td-builder host-sandbox: (FAILED ro-remounting the exposed path above)\n");
                            return Err(e);
                        }
                    }
                }
            }
            // Lock each bind-holding parent dir read-only, AFTER every bind has
            // landed: a RECURSIVE self-bind (making the plain tmpfs dir its own
            // vfsmount) then a NON-recursive ro-remount of just that top mount —
            // the per-item child mounts ride along visible (already ro from
            // their own remounts), but creating a sibling entry in the dir
            // itself now fails EROFS. MS_REC is load-bearing: a NON-recursive
            // self-bind would clone only the top mount, SHADOWING every item
            // bind under it with the empty mountpoint dirs (review finding —
            // every store item would read as an empty dir). These dirs are
            // sandbox-owned tmpfs, so the remount is never the host-owned-EPERM
            // case: a failure here is fatal (the read-only is load-bearing,
            // exactly like the item binds').
            for spec in &ro_dir_specs {
                if !spec.target_dir.is_dir() {
                    continue; // no bind created it — nothing to lock
                }
                sys::mount(
                    Some(&spec.target),
                    &spec.target,
                    None,
                    sys::MS_BIND | sys::MS_REC,
                    None,
                )
                .map_err(|e| { sys::warn(&spec.fail_msg); e })?;
                sys::mount(
                    None,
                    &spec.target,
                    None,
                    sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY,
                    None,
                )
                .map_err(|e| { sys::warn(&spec.fail_msg); e })?;
            }
            // Minimal /dev (replaces the dropped blanket host /dev bind): a fresh
            // tmpfs with only the standard char devices (bind-mounted from the
            // host), /dev/shm, a best-effort private devpts, and the fd symlinks.
            // Nothing else from the host /dev is reachable.
            fs::create_dir_all(&dev_dir)?;
            sys::mount(Some(&tmpfs_c), &dev_dir_c, Some(&tmpfs_c), 0, Some(&dev_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting /dev tmpfs\n"); e })?;
            for (src_c, target, target_c) in &dev_node_specs {
                fs::File::create(target)?;
                sys::mount(Some(src_c), target_c, None, sys::MS_BIND, None)
                    .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED binding a /dev node\n"); e })?;
            }
            fs::create_dir_all(&dev_shm_dir)?;
            sys::mount(Some(&tmpfs_c), &dev_shm_c, Some(&tmpfs_c), 0, Some(&dev_shm_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting /dev/shm\n"); e })?;
            // /dev/pts + /dev/ptmx are best-effort: a new devpts instance needs an
            // unprivileged-mountable devpts (most kernels allow it; some restrict).
            // Nothing in the loop needs a real pty, so a missing /dev/pts only
            // affects a direct interactive user of this sandbox.
            fs::create_dir_all(&dev_pts_dir)?;
            if sys::mount(Some(&devpts_c), &dev_pts_c, Some(&devpts_c), 0, Some(&devpts_data))
                .is_err()
            {
                sys::warn(b"td-builder host-sandbox: devpts unavailable; /dev/pts left empty\n");
            }
            for (link, dest) in &dev_symlinks {
                let _ = std::os::unix::fs::symlink(dest, link);
            }
            // A FRESH procfs reflecting THIS PID namespace (we are its PID 1) —
            // NOT the host /proc. Nested containers write /proc/<pid>/setgroups
            // and friends against this; the host /proc (root-owned PID 1) refused
            // those writes from the non-root sandbox uid.
            fs::create_dir_all(&proc_target_dir)?;
            sys::mount(Some(&proc_c), &proc_target_c, Some(&proc_c), 0, None)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting a fresh /proc\n"); e })?;
            // Writable scratch tmpfs mounts (/tmp, HOME), owned by the host uid.
            for (target_dir, target_c) in &tmpfs_specs {
                fs::create_dir_all(target_dir)?;
                sys::mount(Some(&tmpfs_c), target_c, Some(&tmpfs_c), 0, Some(&tmpfs_data))
                    .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting a scratch tmpfs\n"); e })?;
            }
            // pivot into the new root and drop the old one entirely.
            fs::create_dir_all(&oldroot_rel)?;
            sys::pivot_root(&newroot_c, &oldroot_rel_c)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at pivot_root\n"); e })?;
            std::env::set_current_dir("/")?;
            sys::umount2(&oldroot_abs_c, sys::MNT_DETACH)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED unmounting oldroot\n"); e })?;
            let _ = fs::remove_dir("/oldroot");
            // Enter the requested working directory (e.g. the exposed worktree).
            std::env::set_current_dir(&workdir_owned)?;
            Ok(())
        });
    }

    command
        .status()
        .map_err(|e| err(format!("spawning {cmd} in host-sandbox: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The #469 staging gate, exercised at the unit level (no namespace needed):
    // an item no td-owned db vouches for refuses to stage, tampered bytes refuse
    // to stage, and vouched bytes pass. Verified red against the pre-manifest
    // boundary, which bind-mounted any existing on-disk path a closure named.
    #[test]
    fn staging_rejects_unmanifested_and_tampered_items() {
        let dir = std::env::temp_dir().join(format!("td-stage-verify-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let item = dir.join("aaa-tool-1.0");
        fs::write(&item, b"trusted bytes").unwrap();
        let on_disk = item.to_str().unwrap();
        let canonical = "/td/store/aaa-tool-1.0";
        let good_hash = nar_hash_of(&item).unwrap();

        // No record at all → refused before any hashing.
        let empty = StageManifest::new();
        let err = verify_staged_item(&empty, canonical, on_disk).unwrap_err();
        assert!(err.to_string().contains("no td-owned store-db record"), "{err}");

        // A record whose hash the on-disk bytes do not match → refused.
        let mut tampered = StageManifest::new();
        tampered.insert(
            canonical.to_string(),
            StagedInput { nar_hash: "sha256:0000".to_string(), origin: InputOrigin::AuditedSeed },
        );
        let err = verify_staged_item(&tampered, canonical, on_disk).unwrap_err();
        assert!(err.to_string().contains("refusing to stage tampered bytes"), "{err}");

        // The vouched bytes pass.
        let mut vouched = StageManifest::new();
        vouched.insert(
            canonical.to_string(),
            StagedInput { nar_hash: good_hash, origin: InputOrigin::AuditedSeed },
        );
        verify_staged_item(&vouched, canonical, on_disk).unwrap();

        // …and stop passing the moment the bytes change under the same record.
        fs::write(&item, b"tampered bytes").unwrap();
        let err = verify_staged_item(&vouched, canonical, on_disk).unwrap_err();
        assert!(err.to_string().contains("refusing to stage tampered bytes"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_mem_max_handles_suffixes_and_off_by_default() {
        // OFF by default — the whole point of the safe default (no spurious loop reds).
        assert_eq!(parse_mem_max(None), None, "unset -> no cap");
        assert_eq!(parse_mem_max(Some("".into())), None, "empty -> no cap");
        assert_eq!(parse_mem_max(Some("   ".into())), None, "blank -> no cap");
        assert_eq!(parse_mem_max(Some("garbage".into())), None, "garbage -> no cap");
        assert_eq!(parse_mem_max(Some("0".into())), None, "0 -> no cap (opt out)");
        // Bare bytes and 1024-based suffixes (case-insensitive, optional space).
        assert_eq!(parse_mem_max(Some("4096".into())), Some(4096));
        assert_eq!(parse_mem_max(Some(" 512K ".into())), Some(512 * 1024));
        assert_eq!(parse_mem_max(Some("2m".into())), Some(2 * 1024 * 1024));
        assert_eq!(parse_mem_max(Some("8G".into())), Some(8 * 1024 * 1024 * 1024));
        assert_eq!(parse_mem_max(Some("4 G".into())), Some(4 * 1024 * 1024 * 1024));
        // Overflow on the multiply degrades to "no cap" rather than a wrap.
        assert_eq!(parse_mem_max(Some("999999999999G".into())), None);
    }

    #[test]
    fn store_path_name_strips_hash() {
        assert_eq!(
            store_path_name("/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-0.1.0.drv")
                .unwrap(),
            "td-builder-0.1.0.drv"
        );
        assert!(store_path_name("/tmp/x").is_err());
        assert!(store_path_name("/gnu/store/tooshort-x").is_err());
        // A slash after the hash means a path INSIDE an item, not an item.
        assert!(store_path_name(
            "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-0.1.0/bin/td-builder"
        )
        .is_err());
    }

    #[test]
    fn store_path_name_honors_the_active_prefix() {
        // The pure core strips whichever store dir is active — proving a /td/store build
        // recognises its OWN paths natively (no /gnu/store assumption baked in).
        assert_eq!(
            store_path_name_in(
                "/td/store/",
                "/td/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-hello-2.12.1"
            )
            .unwrap(),
            "hello-2.12.1"
        );
        // A /gnu/store path is NOT a /td/store path — the prefix is load-bearing.
        assert!(store_path_name_in(
            "/td/store/",
            "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-hello-2.12.1"
        )
        .is_err());
    }

    #[test]
    fn closure_entry_splits_canonical_from_on_disk() {
        // A bare entry binds from its canonical path (the daemon-resident case).
        let bare = "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-src";
        assert_eq!(split_closure_entry(bare), (bare, bare));
        // A `CANONICAL\tON-DISK` entry binds from the td store dir but the build
        // still SEES the canonical path (the td-interned source case).
        let canonical = "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-src";
        let on_disk = "/scratch/srcstore/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-src";
        let entry = format!("{canonical}\t{on_disk}");
        assert_eq!(split_closure_entry(&entry), (canonical, on_disk));
    }
}
