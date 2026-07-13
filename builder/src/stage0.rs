//! stage0 — the guix-free stage0 td-builder provisioning chain, in Rust
//! (re #469: the check/setup path must run NO ambient host shell).
//!
//! This absorbs the retired shell chain tests/stage0-builder.sh →
//! tools/bootstrap-td-builder.sh → tools/provision-{rust,cc}.sh into
//! td-builder itself, verbs `stage0-place` / `provision-rust` /
//! `provision-cc`. Same contract, no `sh`:
//!
//! - `provision_rust` / `provision_cc` — resolve the SEED build's toolchain
//!   guix-free and return a PATH fragment (colon-joined bin dirs). Resolution
//!   order (first hit wins; DESIGN §Provenance head; human 2026-07-01 "we can
//!   expect the user to provide it, otherwise use rustup in the scripts to
//!   fetch"):
//!     1. TD_RUST_HOME / TD_CC_HOME — an explicitly PROVIDED toolchain; a
//!        provided-but-unusable home is an ERROR, not a fallthrough.
//!     2. the pinned lock (TD_LOCK, default tests/td-builder-rust.lock;
//!        retired LAST, DESIGN §5) — used ONLY when its /gnu/store paths are
//!        actually present, so a guix dev loop stays byte-identical while a
//!        guix-less host falls through.
//!     3. rustup (`TD_RUST_VERSION`, default 1.96.0) / the system cc on PATH.
//!
//!   NEVER guix/guile. Unresolvable is exit 3 at the verb (operator guidance).
//!
//! - `provision_glibc_static` — resolve a MATCHED glibc `static` output (its
//!   `lib/libc.a`) for the STATIC link below: TD_GLIBC_STATIC_HOME, else a
//!   `-glibc-*-static` lock pin. Fail-closed (no host libs leak, re #469).
//!
//! - `bootstrap_stage0` — cargo-compile td-builder from builder/ source under
//!   a CLEARED environment (only the provisioned toolchain on PATH — the
//!   `env -i` of the old script), offline + frozen; guards that no guix/guile
//!   leaked onto the toolchain PATH. The build is STATICALLY linked (against the
//!   matched static glibc) so the placed builder has an EMPTY runtime closure:
//!   staging it into a sandbox pulls in NO host `lib/` (re #469). Asserted
//!   static before use.
//!
//! - `stage0_place` — the ONE entry point every stage0 consumer goes through
//!   (cache-lib's load_stage0, the check prelude, td-recipe-eval's
//!   check-runner, gate 171): memoized on a `tree-fingerprint` of the builder
//!   source (BASEDIR/.stage0-meta records fingerprint + placed path), locked
//!   against concurrent placers sharing BASEDIR, and the stage0 places ITSELF
//!   via its own `store-add-builder` — no guix-built td-builder anywhere.
//!   Stale placements from earlier fingerprints are swept (#309).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The resolver's inputs, read from the environment ONCE at the entry point
/// (`from_env`) and passed down — so the resolution logic itself is a pure
/// function of this struct and unit tests need no env-var mutation.
pub(crate) struct ProvisionEnv {
    /// TD_RUST_HOME — an explicitly provided Rust toolchain root.
    pub(crate) rust_home: Option<String>,
    /// TD_CC_HOME — an explicitly provided C toolchain root.
    pub(crate) cc_home: Option<String>,
    /// TD_GLIBC_STATIC_HOME — a glibc `static` output (with lib/libc.a) MATCHING
    /// the C toolchain's glibc. Required: the stage0 td-builder is statically
    /// linked so its runtime closure is EMPTY — no host `lib/` is staged into a
    /// build sandbox, so no host library (or a stray +x libtool archive beside
    /// it) can leak past the #469 boundary. A dynamic builder would drag its
    /// glibc/libgcc `lib/` in; this is the seed that lets us avoid that.
    pub(crate) glibc_static_home: Option<String>,
    /// TD_LOCK (default tests/td-builder-rust.lock), resolved against root.
    pub(crate) lock: PathBuf,
    /// TD_RUST_VERSION — the rustup toolchain to install on a guix-less host.
    pub(crate) rust_version: String,
    /// The PATH searched for rustup / the system cc (never for the build).
    pub(crate) search_path: String,
}

impl ProvisionEnv {
    pub(crate) fn from_env(root: &Path) -> Self {
        // `${VAR:-default}` semantics: an EMPTY env var falls through like an
        // unset one (the old scripts' `[ -n "${TD_RUST_HOME:-}" ]`).
        let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let lock = nonempty("TD_LOCK")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("tests/td-builder-rust.lock"));
        let lock = if lock.is_absolute() {
            lock
        } else {
            root.join(lock)
        };
        ProvisionEnv {
            rust_home: nonempty("TD_RUST_HOME"),
            cc_home: nonempty("TD_CC_HOME"),
            glibc_static_home: nonempty("TD_GLIBC_STATIC_HOME"),
            lock,
            rust_version: nonempty("TD_RUST_VERSION").unwrap_or_else(|| "1.96.0".to_string()),
            search_path: std::env::var("PATH").unwrap_or_default(),
        }
    }
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Colon-join two bin dirs, de-duplicated when they are the same directory.
fn emit_frag(a: &str, b: &str) -> String {
    if a == b {
        a.to_string()
    } else {
        format!("{a}:{b}")
    }
}

/// The lock's 2nd whitespace-separated field per line (`NAME PATH ...`). An
/// absent or empty lock yields no candidates (the fallthrough, not an error).
fn lock_paths(lock: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(lock) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
        .collect()
}

/// The old resolver's shell glob `-rust-[0-9]`: "-rust-" immediately followed
/// by a digit — a versioned rust store item, not e.g. a `-rust-src` doc dir.
fn names_rust_version(path: &str) -> bool {
    path.match_indices("-rust-")
        .any(|(i, m)| path.as_bytes().get(i + m.len()).is_some_and(u8::is_ascii_digit))
}

/// Lock leg of the rust resolver: the first `*-rust-[0-9]*-cargo` path is
/// cargo, the first other `*-rust-[0-9]*` path is rustc; both bin dirs must
/// actually carry their executable (a guix-less host, where the /gnu/store
/// paths do not exist, falls through to rustup).
fn lock_rust_frag(lock: &Path) -> Option<String> {
    let mut rust: Option<String> = None;
    let mut cargo: Option<String> = None;
    for p in lock_paths(lock) {
        if !p.contains('/') || !names_rust_version(&p) {
            continue;
        }
        if p.ends_with("-cargo") {
            if cargo.is_none() {
                cargo = Some(p);
            }
        } else if rust.is_none() {
            rust = Some(p);
        }
    }
    let (rb, cb) = (format!("{}/bin", rust?), format!("{}/bin", cargo?));
    (is_exec(&Path::new(&rb).join("rustc")) && is_exec(&Path::new(&cb).join("cargo")))
        .then(|| emit_frag(&rb, &cb))
}

fn find_in_path(search_path: &str, bin: &str) -> Option<PathBuf> {
    search_path
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(bin))
        .find(|p| is_exec(p))
}

/// Mirror the old scripts' `>&2` redirections: a captured child's streams go
/// to OUR stderr so stdout stays reserved for the machine-read result.
fn forward_to_stderr(out: &std::process::Output) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = err.write_all(&out.stdout);
    let _ = err.write_all(&out.stderr);
}

/// Resolve a guix-free Rust toolchain (rustc + cargo) for the td-builder SEED
/// build and return a PATH fragment putting both on PATH. See the module doc
/// for the resolution order. NEVER invokes guix/guile.
pub(crate) fn provision_rust(env: &ProvisionEnv) -> Result<String, String> {
    // 1. Explicitly provided toolchain.
    if let Some(home) = &env.rust_home {
        let b = format!("{home}/bin");
        let bp = Path::new(&b);
        if !(is_exec(&bp.join("rustc")) && is_exec(&bp.join("cargo"))) {
            return Err(format!("TD_RUST_HOME={home} has no bin/rustc + bin/cargo"));
        }
        return Ok(b);
    }

    // 2. The pinned (guix seed) lock — only when its store paths are present.
    if let Some(frag) = lock_rust_frag(&env.lock) {
        return Ok(frag);
    }

    // 3. rustup — fetch the pinned toolchain (guix-less host).
    if let Some(rustup) = find_in_path(&env.search_path, "rustup") {
        let ver = &env.rust_version;
        let install = Command::new(&rustup)
            .args(["toolchain", "install", ver, "--profile", "minimal", "--no-self-update"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("spawn {}: {e}", rustup.display()))?;
        forward_to_stderr(&install);
        if !install.status.success() {
            return Err(format!("rustup could not install toolchain {ver}"));
        }
        let which = Command::new(&rustup)
            .args(["which", "--toolchain", ver, "rustc"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("spawn {}: {e}", rustup.display()))?;
        if !which.status.success() {
            forward_to_stderr(&which);
            return Err(format!("'rustup which rustc' failed for {ver}"));
        }
        let rustc = String::from_utf8_lossy(&which.stdout).trim().to_string();
        let d = Path::new(&rustc)
            .parent()
            .ok_or_else(|| format!("rustup gave a rootless rustc path `{rustc}'"))?;
        if !(is_exec(&d.join("rustc")) && is_exec(&d.join("cargo"))) {
            return Err(format!(
                "rustup toolchain {ver} at {} lacks rustc+cargo",
                d.display()
            ));
        }
        return Ok(d.to_string_lossy().into_owned());
    }

    Err("no Rust toolchain found — set TD_RUST_HOME to a provided toolchain, ensure the \
         pinned lock seed is present, or install rustup (DESIGN §Provenance)"
        .to_string())
}

fn has_cc(bin_dir: &Path) -> bool {
    is_exec(&bin_dir.join("gcc")) || is_exec(&bin_dir.join("cc"))
}

/// Resolve a C toolchain (gcc/cc — rustc's linker driver) for the td-builder
/// SEED build's link step and return its bin-dir PATH fragment. See the
/// module doc for the resolution order. NEVER invokes guix.
pub(crate) fn provision_cc(env: &ProvisionEnv) -> Result<String, String> {
    // 1. Explicitly provided toolchain.
    if let Some(home) = &env.cc_home {
        let b = format!("{home}/bin");
        if !has_cc(Path::new(&b)) {
            return Err(format!("TD_CC_HOME={home} has no bin/gcc or bin/cc"));
        }
        return Ok(b);
    }

    // 2. The pinned (guix seed) gcc-toolchain — only when present on disk.
    if let Some(g) = lock_paths(&env.lock)
        .into_iter()
        .find(|p| p.contains('/') && p.contains("-gcc-toolchain-"))
    {
        let gb = format!("{g}/bin");
        if has_cc(Path::new(&gb)) {
            return Ok(gb);
        }
    }

    // 3. System cc/gcc (guix-less host: build-essential).
    if let Some(cc) =
        find_in_path(&env.search_path, "cc").or_else(|| find_in_path(&env.search_path, "gcc"))
    {
        if let Some(d) = cc.parent() {
            if !has_cc(d) {
                return Err(format!("the system cc at {} is not usable", d.display()));
            }
            return Ok(d.to_string_lossy().into_owned());
        }
    }

    Err("no C toolchain found — set TD_CC_HOME to a provided toolchain, ensure the pinned \
         lock seed is present, or install a system cc/gcc (build-essential)"
        .to_string())
}

/// Resolve a static glibc — its `lib/` carrying `libc.a` — for the stage0
/// td-builder's STATIC link, MATCHED to the C toolchain's glibc (a mismatched
/// static libc links but crashes at startup). See the module doc for WHY the
/// stage0 builder is linked statically. NEVER invokes guix. Resolution mirrors
/// `provision_cc`: an explicit `TD_GLIBC_STATIC_HOME`, then a `-glibc-*-static`
/// pin from the lock. Fail-closed: without it the builder would link
/// dynamically and leak its host `lib/` into every sandbox (re #469).
pub(crate) fn provision_glibc_static(env: &ProvisionEnv) -> Result<String, String> {
    let has_static_libc =
        |lib: &Path| std::fs::metadata(lib.join("libc.a")).is_ok_and(|m| m.is_file());
    // 1. Explicitly provided static glibc.
    if let Some(home) = &env.glibc_static_home {
        let lib = Path::new(home).join("lib");
        if !has_static_libc(&lib) {
            return Err(format!(
                "TD_GLIBC_STATIC_HOME={home} has no lib/libc.a — not a glibc `static` output"
            ));
        }
        return Ok(lib.to_string_lossy().into_owned());
    }
    // 2. The pinned (guix seed) glibc:static — only when present on disk.
    if let Some(lib) = lock_paths(&env.lock)
        .into_iter()
        .filter(|p| p.contains('/') && p.contains("-glibc-") && p.ends_with("-static"))
        .map(|p| Path::new(&p).join("lib"))
        .find(|lib| has_static_libc(lib))
    {
        return Ok(lib.to_string_lossy().into_owned());
    }
    Err("no static glibc found — set TD_GLIBC_STATIC_HOME to a glibc `static` output (with \
         lib/libc.a) matching the C toolchain, or pin one in the lock. REQUIRED so the stage0 \
         sandbox builder links statically: a dynamic builder drags its host glibc/libgcc lib/ \
         into every build sandbox, which the #469 boundary must deny (DESIGN §Provenance)"
        .to_string())
}

/// A scratch dir under the system temp dir, unique per process (pid + a
/// counter — no clock/randomness), removed by `RemoveOnDrop`.
fn scratch_dir(tag: &str) -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for n in 0..1000u32 {
        let d = base.join(format!("td-{tag}.{pid}.{n}"));
        match std::fs::create_dir(&d) {
            Ok(()) => return Ok(d),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("mkdir {}: {e}", d.display())),
        }
    }
    Err(format!(
        "could not create a scratch dir under {}",
        base.display()
    ))
}

/// The old scripts' `trap 'rm -rf "$work"' EXIT` — best-effort cleanup on
/// every exit path, success or error.
struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Produce a STAGE0 td-builder from the checked-in builder/ source using ONLY
/// a Rust toolchain — NO guix, NO Guile, NO guix-daemon, NO host shell. Writes
/// OUT_DIR/bin/td-builder and returns its path. td-builder has ZERO external
/// crate deps (std-only), so the OFFLINE `--frozen` build needs only
/// rustc/cargo + a gcc linker; the build runs under a CLEARED environment with
/// only the provisioned toolchain on PATH (the old `env -i`).
///
/// The build is STATICALLY linked against `glibc_static_lib` (a matched glibc
/// `static` output's `lib/`): a static builder has an EMPTY runtime closure, so
/// staging it into a build sandbox pulls in NO host `lib/` — the sole way to
/// keep host libraries (and stray +x libtool archives beside them) out of the
/// sandbox entirely (re #469). The result is asserted static before it is used.
pub(crate) fn bootstrap_stage0(
    root: &Path,
    penv: &ProvisionEnv,
    glibc_static_lib: &str,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    let rustpath =
        provision_rust(penv).map_err(|e| format!("could not provision a Rust toolchain: {e}"))?;
    let ccpath =
        provision_cc(penv).map_err(|e| format!("could not provision a C toolchain: {e}"))?;
    // The bootstrap PATH carries ONLY the provisioned Rust + C toolchains —
    // assert no guix/guile leaks in (the stage0 build must be guix-free,
    // mirroring the corpus gates' scrubbed-PATH guard).
    let bootpath = format!("{rustpath}:{ccpath}");
    if bootpath.contains("guix") || bootpath.contains("guile") {
        return Err(
            "guix/guile on the stage0 toolchain PATH — not a guix-free build".to_string(),
        );
    }

    let work = scratch_dir("stage0-boot")?;
    let _work_guard = RemoveOnDrop(work.clone());
    // Resolve cargo to an absolute path ourselves — the child's PATH is the
    // cleared bootpath, and the binary we exec must come from it, not from
    // any ambient lookup.
    let cargo = find_in_path(&bootpath, "cargo")
        .ok_or_else(|| format!("no cargo on the provisioned toolchain PATH ({bootpath})"))?;
    // Static link (re #469): `+crt-static` links glibc's `libc.a`/`libm.a`; the
    // NON-pie relocation model dodges a static-PIE glibc startup crash; `-L`
    // points the linker at the MATCHED static glibc (its crt objects come from
    // the gcc driver — only the archives need the search path). RUSTFLAGS is
    // added to the CLEARED env; it applies to the build script too, which then
    // also links + runs static (proven fine with the non-pie model).
    let rustflags =
        format!("-C target-feature=+crt-static -C relocation-model=static -L {glibc_static_lib}");
    let build = Command::new(&cargo)
        .env_clear()
        .env("PATH", &bootpath)
        .env("HOME", &work)
        .env("CARGO_HOME", work.join("cargo"))
        .env("RUSTFLAGS", &rustflags)
        .args(["build", "--release", "--offline", "--frozen", "--manifest-path"])
        .arg(root.join("builder/Cargo.toml"))
        .arg("--target-dir")
        .arg(work.join("target"))
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", cargo.display()))?;
    forward_to_stderr(&build);
    if !build.status.success() {
        return Err("the stage0 cargo build failed (see stderr)".to_string());
    }

    let built = work.join("target/release/td-builder");
    let bin_dir = out_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).map_err(|e| format!("mkdir {}: {e}", bin_dir.display()))?;
    let dest = bin_dir.join("td-builder");
    std::fs::copy(&built, &dest)
        .map_err(|e| format!("copy {} -> {}: {e}", built.display(), dest.display()))?;
    // Enforce the no-leakage invariant at the SOURCE: the placed builder MUST be
    // fully static (no PT_INTERP, no DT_NEEDED, no run-path). If a future
    // toolchain silently linked it dynamically, fail here rather than stage its
    // host lib/ into a sandbox (re #469).
    crate::elf::assert_static(&dest)?;
    Ok(dest)
}

/// The DERIVED builder-lineage registry dir (re #469 round-10 P0 #2): one
/// record per NAR hash of a builder tree that `stage0_place` ITSELF compiled
/// from this repo's builder/ source and placed. `ControlPlaneBuilder` typing
/// REQUIRES a record here (`verify_builder_lineage` in main): content
/// addressing (`authenticate_ca_db`) proves a TD_BUILDER_* tree's INTEGRITY,
/// not its ORIGIN — `store-add-builder` is placement mechanics anyone can run
/// over any self-addressed tree, so the origin claim must come from the one
/// code path that actually produced the builder. Derived like the blessed
/// seed-closure db (no argv/env-of-the-moment selects it per request), and in
/// the same trust domain: a same-user writer can forge a record at the derived
/// location; the daemon-owned provenance db is the #472 follow-on.
pub(crate) fn builder_lineage_dir() -> Result<PathBuf, String> {
    Ok(crate::check_loop::daemon_runtime_dir()?.join("builder-lineage"))
}

/// The registry filename for a `sha256:<hex>` NAR hash — validated so a db-
/// supplied hash can never traverse out of the registry dir.
fn lineage_key(nar_hash: &str) -> Result<String, String> {
    let hex = nar_hash
        .strip_prefix("sha256:")
        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("builder lineage: malformed NAR hash `{nar_hash}'"))?;
    Ok(hex.to_string())
}

/// Record lineage for a placed builder tree, keyed by its NAR hash. Idempotent
/// (tmp + atomic rename): concurrent placers of the same bytes converge on the
/// same record; a pre-existing record is left untouched.
pub(crate) fn record_builder_lineage_in(
    dir: &Path,
    nar_hash: &str,
    canonical: &str,
    source_fp: &str,
) -> Result<(), String> {
    let key = lineage_key(nar_hash)?;
    let f = dir.join(&key);
    if f.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{key}.tmp.{}", std::process::id()));
    std::fs::write(
        &tmp,
        format!("td-builder-lineage v1\ncanonical {canonical}\nsource-fp {source_fp}\n"),
    )
    .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &f)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), f.display()))
}

/// Is a lineage record present (and well-formed) for this NAR hash?
pub(crate) fn builder_lineage_recorded_in(dir: &Path, nar_hash: &str) -> Result<bool, String> {
    let f = dir.join(lineage_key(nar_hash)?);
    match std::fs::read_to_string(&f) {
        Ok(t) => Ok(t.starts_with("td-builder-lineage v1")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("read {}: {e}", f.display())),
    }
}

/// Ensure the placed builder's lineage record exists: read the placement db's
/// hashed row for CB and record it. Runs on BOTH the memo-hit and slow paths of
/// `stage0_place`, so a placement made before the lineage registry existed is
/// enrolled the next time it is resolved (warm caches keep working).
fn ensure_builder_lineage(db: &Path, cb: &str, source_fp: &str) -> Result<(), String> {
    let data = std::fs::read(db).map_err(|e| format!("read {}: {e}", db.display()))?;
    let rows = crate::store_db_read::Db::open(data)?.hashes_by_path()?;
    let hash = rows
        .get(cb)
        .ok_or_else(|| format!("builder db {} has no hashed row for {cb}", db.display()))?;
    record_builder_lineage_in(&builder_lineage_dir()?, hash, cb, source_fp)
}

/// A valid memo: the recorded fingerprint matches AND the placement + db are
/// present and intact. Returns the memoized canonical store path.
fn stage0_memo_hit(meta: &Path, fp: &str, store: &Path, db: &Path) -> Option<String> {
    let text = std::fs::read_to_string(meta).ok()?;
    let mut lines = text.lines();
    let old_fp = lines.next()?;
    let cb = lines.next()?.trim();
    if old_fp != fp || cb.is_empty() {
        return None;
    }
    let placed = store.join(Path::new(cb).file_name()?).join("bin/td-builder");
    let db_ok = std::fs::metadata(db).is_ok_and(|m| m.is_file() && m.len() > 0);
    (is_exec(&placed) && db_ok).then(|| cb.to_string())
}

/// Produce a stage0 td-builder and PLACE it into a td-owned store under BASE
/// using STAGE0'S OWN `store-add-builder` (stage0 places itself — no
/// guix-built td-builder anywhere). Writes BASE/{store/<base>/…, builder.db,
/// .stage0-meta} and returns the placed builder's canonical store path (Cb).
///
/// Memoized: .stage0-meta records (builder-source fingerprint, Cb); a call
/// whose fingerprint matches AND whose placement is intact reuses it, so warm
/// loops skip the ~8s compile. Concurrent placers sharing BASE serialize on
/// BASE/.stage0.lock (double-checked memo after the lock) — the check-engine
/// smoke tier runs several stage0-using gates at once, and unserialized
/// `store-add-builder`s collide ("File exists").
pub(crate) fn stage0_place(root: &Path, base: &Path) -> Result<String, String> {
    let penv = ProvisionEnv::from_env(root);
    if !std::fs::metadata(&penv.lock).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return Err(format!("no toolchain lock {}", penv.lock.display()));
    }
    // Resolve the matched static glibc up front: the stage0 builder is linked
    // statically (re #469), so this is a genuine compile input — fail fast if it
    // is missing, and FOLD it into the memo fingerprint below so a change in the
    // static glibc re-places (like the seed tables in fp_roots).
    let glibc_static_lib = provision_glibc_static(&penv)?;
    let store = base.join("store");
    let db = base.join("builder.db");
    let meta = base.join(".stage0-meta");

    // Fingerprint the builder source the stage0 is compiled from — reuse only
    // if unchanged. Absolute roots: the caller's cwd must not matter. The two
    // seed tables are `include_str!`-compiled INTO the builder (main.rs
    // SEED_DIGESTS / CONTROL_PLANE_SEED_PINS), so they are genuine compile
    // inputs to the placed binary and MUST be fingerprinted too — otherwise
    // adding a source pin (a new seed-digests row) leaves the prior placement's
    // compiled table in force and the new pin reads as an unpinned seed (re #469).
    let fp_roots: Vec<String> = [
        "builder/src",
        "builder/build.rs",
        "builder/Cargo.toml",
        "builder/Cargo.lock",
        "seed/seed-digests.txt",
        "seed/control-plane-seed-pins.txt",
    ]
    .iter()
    .map(|p| root.join(p).to_string_lossy().into_owned())
    .collect();
    let fp = crate::tree_fingerprint(&fp_roots)?;
    // The static glibc is linked into the placed binary — a compile input, so it
    // joins the fingerprint (single line: `.stage0-meta` is `fp\ncb\n`, a store
    // path carries no newline). A different static glibc → a re-place.
    let fp = format!("{fp} static-glibc={glibc_static_lib}");

    // Fast path: a valid memo needs no lock (warm loops skip the compile AND
    // the lock wait).
    if let Some(cb) = stage0_memo_hit(&meta, &fp, &store, &db) {
        ensure_builder_lineage(&db, &cb, &fp)?;
        return Ok(cb);
    }

    // Slow path: serialize build+place across concurrent placers sharing BASE.
    std::fs::create_dir_all(base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
    let lock_path = base.join(".stage0.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    lock_file
        .lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    // Double-checked: a placer that waited for the lock may now find the
    // holder's fresh memo — reuse it rather than rebuild+re-place.
    if let Some(cb) = stage0_memo_hit(&meta, &fp, &store, &db) {
        ensure_builder_lineage(&db, &cb, &fp)?;
        return Ok(cb);
    }

    // 1. cargo-compile stage0 from builder/ source (guix/Guile-free, offline).
    let work = scratch_dir("stage0-place")?;
    let _work_guard = RemoveOnDrop(work.clone());
    let s0_dir = work.join("s0");
    let s0 = bootstrap_stage0(root, &penv, &glibc_static_lib, &s0_dir)?;
    if !is_exec(&s0) {
        return Err("bootstrap produced no stage0 td-builder".to_string());
    }

    // 2. stage0 places ITSELF into the td store (its OWN store-add-builder;
    //    refs scanned vs the seed store dir's entries — a readdir, NO
    //    /var/guix/db read (#313), so a guix-less host cold-starts: /gnu/store
    //    absent → no candidates → no refs, exactly right for a rustup/
    //    system-cc stage0 that embeds no store paths).
    std::fs::create_dir_all(&store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;
    let place = Command::new(&s0)
        .args(["store-add-builder", "td-builder-0.1.0"])
        .arg(&s0_dir)
        .arg(&store)
        .arg(&db)
        .arg("/gnu/store")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", s0.display()))?;
    if !place.status.success() {
        forward_to_stderr(&place);
        return Err("stage0 store-add-builder failed (see stderr)".to_string());
    }
    let cb = String::from_utf8_lossy(&place.stdout).trim().to_string();
    if !(cb.starts_with("/gnu/store/") && cb.ends_with("-td-builder-0.1.0")) {
        return Err(format!("store-add-builder gave a malformed path `{cb}'"));
    }
    let cur = Path::new(&cb)
        .file_name()
        .ok_or_else(|| format!("store-add-builder gave a rootless path `{cb}'"))?
        .to_os_string();
    if !is_exec(&store.join(&cur).join("bin/td-builder")) {
        return Err(format!("stage0 not restored under {}", store.display()));
    }
    std::fs::write(&meta, format!("{fp}\n{cb}\n"))
        .map_err(|e| format!("write {}: {e}", meta.display()))?;
    // The LINEAGE record (re #469 round-10 P0 #2): this is the only writer —
    // the placement above was compiled from THIS repo's builder/ source by
    // this very fn, which is exactly the origin claim `ControlPlaneBuilder`
    // typing verifies against. `store-add-builder` alone mints no authority.
    ensure_builder_lineage(&db, &cb, &fp)?;

    // 3. GC stale placements (#309): this slow path just placed the CURRENT
    //    stage0 and store-add-builder rewrote builder.db to reference ONLY it,
    //    so every OTHER *-td-builder-* dir under the store is a placement from
    //    an earlier builder/ fingerprint — unbounded disk on a long-lived warm
    //    runner and a latent hazard for glob-style resolvers (the #293 red).
    //    Safe under the still-held .stage0.lock; best-effort per dir (a failed
    //    rm must never fail the PLACEMENT — the next slow path retries).
    let mut swept = 0u32;
    if let Ok(entries) = std::fs::read_dir(&store) {
        for ent in entries.flatten() {
            let name = ent.file_name();
            if name == cur || !name.to_string_lossy().contains("-td-builder-") {
                continue;
            }
            if !ent.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if std::fs::remove_dir_all(ent.path()).is_ok() {
                swept += 1;
            }
        }
    }
    if swept > 0 {
        eprintln!(
            "stage0-place: swept {swept} stale placement(s) from {} (kept {})",
            store.display(),
            cur.to_string_lossy()
        );
    }
    Ok(cb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("td-stage0-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn exec_file(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A hermetic resolver env: no homes, an absent lock, an EMPTY search
    /// path — so no host rustup/cc can leak into a test's resolution.
    fn base_env(lock: &Path) -> ProvisionEnv {
        ProvisionEnv {
            rust_home: None,
            cc_home: None,
            glibc_static_home: None,
            lock: lock.to_path_buf(),
            rust_version: "1.96.0".to_string(),
            search_path: String::new(),
        }
    }

    #[test]
    fn provision_resolves_the_provided_toolchain_first_and_reds_an_unusable_one() {
        let d = scratch("provided");
        exec_file(&d.join("rust/bin/rustc"));
        exec_file(&d.join("rust/bin/cargo"));
        exec_file(&d.join("cc/bin/gcc"));
        let mut env = base_env(&d.join("no-such-lock"));
        env.rust_home = Some(d.join("rust").to_string_lossy().into_owned());
        env.cc_home = Some(d.join("cc").to_string_lossy().into_owned());
        assert_eq!(
            provision_rust(&env).unwrap(),
            format!("{}/bin", d.join("rust").display())
        );
        assert_eq!(
            provision_cc(&env).unwrap(),
            format!("{}/bin", d.join("cc").display())
        );
        // A PROVIDED-but-unusable home is an ERROR (the operator asked for
        // it), not a silent fallthrough to some other toolchain.
        env.rust_home = Some(d.join("empty").to_string_lossy().into_owned());
        env.cc_home = env.rust_home.clone();
        assert!(provision_rust(&env).unwrap_err().contains("TD_RUST_HOME"));
        assert!(provision_cc(&env).unwrap_err().contains("TD_CC_HOME"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn provision_falls_back_to_present_lock_paths_then_reds_with_guidance() {
        let d = scratch("lock");
        let r = d.join("store/abc123-rust-1.93.0");
        let c = d.join("store/def456-rust-1.93.0-cargo");
        let g = d.join("store/aaa111-gcc-toolchain-15.2.0");
        exec_file(&r.join("bin/rustc"));
        exec_file(&c.join("bin/cargo"));
        exec_file(&g.join("bin/gcc"));
        let lock = d.join("lock");
        // The cargo path listed FIRST: the `-cargo` suffix must classify it as
        // cargo, never as the rustc dir.
        std::fs::write(
            &lock,
            format!(
                "rust-cargo {}\nrust {}\ngcc-toolchain {}\n",
                c.display(),
                r.display(),
                g.display()
            ),
        )
        .unwrap();
        let env = base_env(&lock);
        assert_eq!(
            provision_rust(&env).unwrap(),
            format!("{}/bin:{}/bin", r.display(), c.display())
        );
        assert_eq!(provision_cc(&env).unwrap(), format!("{}/bin", g.display()));

        // An ABSENT lock falls through; with no rustup / system cc on the
        // (empty) search path the resolver reds with operator guidance — the
        // exit-3 arm at the verb.
        let env2 = base_env(&d.join("absent-lock"));
        assert!(provision_rust(&env2).unwrap_err().contains("no Rust toolchain"));
        assert!(provision_cc(&env2).unwrap_err().contains("no C toolchain"));

        // System leg: a cc on the search path resolves to its bin dir.
        let sysd = d.join("sysbin");
        exec_file(&sysd.join("cc"));
        let mut env3 = base_env(&d.join("absent-lock"));
        env3.search_path = sysd.to_string_lossy().into_owned();
        assert_eq!(provision_cc(&env3).unwrap(), sysd.to_string_lossy());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn provision_glibc_static_resolves_home_then_lock_then_reds() {
        let d = scratch("glibc-static");
        // libc.a is a plain data file (an `ar` archive), never executable.
        let touch = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"!<arch>\n").unwrap();
        };

        // 1. Explicit TD_GLIBC_STATIC_HOME with lib/libc.a → its lib dir.
        let home = d.join("store/hhh-glibc-2.41-static");
        touch(&home.join("lib/libc.a"));
        let mut env = base_env(&d.join("absent-lock"));
        env.glibc_static_home = Some(home.to_string_lossy().into_owned());
        assert_eq!(
            provision_glibc_static(&env).unwrap(),
            format!("{}/lib", home.display())
        );

        // A PROVIDED home WITHOUT lib/libc.a is an ERROR (the operator asked for
        // it), not a silent fallthrough.
        let mut env_bad = base_env(&d.join("absent-lock"));
        env_bad.glibc_static_home = Some(d.join("store/empty").to_string_lossy().into_owned());
        assert!(provision_glibc_static(&env_bad)
            .unwrap_err()
            .contains("TD_GLIBC_STATIC_HOME"));

        // 2. Lock leg: a `-glibc-*-static` pin whose lib/libc.a exists.
        let g = d.join("store/ggg-glibc-2.41-static");
        touch(&g.join("lib/libc.a"));
        let lock = d.join("lock");
        std::fs::write(&lock, format!("glibc-static {}\n", g.display())).unwrap();
        assert_eq!(
            provision_glibc_static(&base_env(&lock)).unwrap(),
            format!("{}/lib", g.display())
        );

        // 3. No home, no lock match → reds with operator guidance (fail-closed).
        assert!(provision_glibc_static(&base_env(&d.join("absent-lock")))
            .unwrap_err()
            .contains("no static glibc"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stage0_memo_reuses_only_a_matching_intact_placement() {
        let d = scratch("memo");
        let base = d.join("s0");
        let store = base.join("store");
        let db = base.join("builder.db");
        let meta = base.join(".stage0-meta");
        let cb = "/gnu/store/abc123-td-builder-0.1.0";
        exec_file(&store.join("abc123-td-builder-0.1.0/bin/td-builder"));
        std::fs::write(&db, "x").unwrap();
        std::fs::write(&meta, format!("fp1\n{cb}\n")).unwrap();
        assert_eq!(stage0_memo_hit(&meta, "fp1", &store, &db), Some(cb.to_string()));
        // A CHANGED builder-source fingerprint must rebuild.
        assert_eq!(stage0_memo_hit(&meta, "fp2", &store, &db), None);
        // A memo whose placement bytes are gone must rebuild, not be trusted.
        std::fs::remove_dir_all(store.join("abc123-td-builder-0.1.0")).unwrap();
        assert_eq!(stage0_memo_hit(&meta, "fp1", &store, &db), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    // re #469 round-10 P0 #2: the lineage registry round-trip — absent reads
    // false (the verifier fails closed on it), a record persists idempotently,
    // and a malformed hash can neither write nor read (no path traversal out
    // of the registry dir).
    #[test]
    fn builder_lineage_registry_roundtrip_and_fail_closed() {
        let d = scratch("lineage");
        let dir = d.join("registry");
        let h = format!("sha256:{}", "cd".repeat(32));
        assert!(!builder_lineage_recorded_in(&dir, &h).unwrap());
        record_builder_lineage_in(&dir, &h, "/gnu/store/x-td-builder-0.1.0", "fp").unwrap();
        assert!(builder_lineage_recorded_in(&dir, &h).unwrap());
        // Idempotent: a re-record of the same bytes is a no-op, never an error.
        record_builder_lineage_in(&dir, &h, "/gnu/store/x-td-builder-0.1.0", "fp").unwrap();
        // Malformed hashes are rejected before any filesystem access.
        assert!(record_builder_lineage_in(&dir, "sha256:../escape", "c", "f").is_err());
        assert!(builder_lineage_recorded_in(&dir, "md5:00").is_err());
        assert!(builder_lineage_recorded_in(&dir, "sha256:").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
