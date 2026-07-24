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
//!     2. rustc+cargo / the system cc already on PATH — the primary guix-free
//!        resolution (rustup's default, a distro package, or a guix-home
//!        profile): whatever the host provides, no /gnu/store pin.
//!     3. rustup (`TD_RUST_VERSION`, default 1.96.0) — installs the pinned
//!        toolchain AND `rustup target add x86_64-unknown-linux-musl`.
//!
//!   The resolved rust MUST ship the [`MUSL_TARGET`] self-contained static std
//!   (`ensure_musl_target`) — the source of the `+crt-static` libc.a that
//!   replaced the retired guix glibc:static pin. NEVER guix/guile. An ABSENT
//!   toolchain ([`ProvisionErr::Unavailable`]) is `EXIT_UNPROVISIONED` (69) at
//!   the verb — a tolerated Unprovisioned skip; a RESOLVED-but-unusable one
//!   ([`ProvisionErr::Broken`]) fails hard (RED), never silenced (re #469).
//!
//! - `bootstrap_stage0` — cargo-compile td-builder from builder/ source for
//!   [`MUSL_TARGET`] under a CLEARED environment (only the provisioned toolchain
//!   on PATH — the `env -i` of the old script), offline + frozen. The build is
//!   fully STATIC (musl's self-contained libc.a linked by the bundled `rust-lld`)
//!   so the placed builder has an EMPTY runtime-LINK closure (no PT_INTERP, no
//!   DT_NEEDED): the sandbox stages NO host `lib/` for it, so no host library —
//!   or a stray +x libtool archive beside one — leaks in (re #469). Asserted
//!   static AND smoke-run (a broken/absent musl std links nothing) before use.
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
    /// TD_RUST_VERSION — the rustup toolchain to install on a host without rust
    /// on PATH.
    pub(crate) rust_version: String,
    /// The PATH searched for rustc/cargo/rustup and the system cc.
    pub(crate) search_path: String,
}

impl ProvisionEnv {
    pub(crate) fn from_env(_root: &Path) -> Self {
        // `${VAR:-default}` semantics: an EMPTY env var falls through like an
        // unset one (the old scripts' `[ -n "${TD_RUST_HOME:-}" ]`).
        let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        ProvisionEnv {
            rust_home: nonempty("TD_RUST_HOME"),
            cc_home: nonempty("TD_CC_HOME"),
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

/// The provisioned rust MUST ship [`MUSL_TARGET`]'s self-contained static std
/// (`rust-std-x86_64-unknown-linux-musl`) — the source of the `+crt-static`
/// `libc.a` that replaced the guix glibc:static pin. Verify it once, up front,
/// with a clear message rather than a cryptic link failure deep in the build.
fn ensure_musl_target(rust_bin_dir: &str) -> Result<(), String> {
    let rustc = Path::new(rust_bin_dir).join("rustc");
    let out = Command::new(&rustc)
        .arg("--print")
        .arg("sysroot")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", rustc.display()))?;
    if !out.status.success() {
        return Err(format!("`rustc --print sysroot` failed for {}", rustc.display()));
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let libc_a = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(MUSL_TARGET)
        .join("lib/self-contained/libc.a");
    if std::fs::metadata(&libc_a).is_ok_and(|m| m.is_file()) {
        Ok(())
    } else {
        Err(format!(
            "the provisioned rust toolchain lacks the {MUSL_TARGET} static std (missing {}) — \
             add it with `rustup target add {MUSL_TARGET}` or provide a TD_RUST_HOME whose \
             rust-std ships the self-contained musl libc.a",
            libc_a.display()
        ))
    }
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

/// Why [`provision_rust`]/[`provision_cc`] could not return a toolchain. The two
/// cases map to DIFFERENT exit codes so an in-jail compile gate can tell "nothing
/// to run here" from "a real failure", instead of silencing both as a skip:
/// - `Unavailable` — nothing was there to resolve (no `TD_*_HOME`, nothing on
///   PATH, no rustup). The honest "cannot run here" the loop sandbox hits; a
///   caller maps it to `EXIT_UNPROVISIONED` (69) so the gate degrades to a
///   tolerated Unprovisioned SKIP (re #469).
/// - `Broken` — a toolchain WAS named or found but is unusable (a bad
///   `TD_*_HOME`, a rustup install/target-add failure, a resolved rust missing
///   the musl std). An operator error or real regression: it fails hard (non-69)
///   and REDs, never degrades to a skip.
#[derive(Debug)]
pub(crate) enum ProvisionErr {
    Unavailable(String),
    Broken(String),
}

impl ProvisionErr {
    /// Render for the string-tag exit-code contract the verbs, `bootstrap_stage0`,
    /// and the native gate bodies share: an `Unavailable` gap carries the
    /// [`crate::check_loop::UNPROVISIONED_TAG`] so the CLI maps it to
    /// `EXIT_UNPROVISIONED`; a `Broken` toolchain is untagged so it maps to
    /// `ExitCode::FAILURE` (RED).
    pub(crate) fn tagged(&self) -> String {
        match self {
            ProvisionErr::Unavailable(m) => {
                format!("{}{m}", crate::check_loop::UNPROVISIONED_TAG)
            }
            ProvisionErr::Broken(m) => m.clone(),
        }
    }
}

impl std::fmt::Display for ProvisionErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisionErr::Unavailable(m) | ProvisionErr::Broken(m) => f.write_str(m),
        }
    }
}

/// Resolve a guix-free Rust toolchain (rustc + cargo) for the td-builder SEED
/// build and return a PATH fragment putting both on PATH. The toolchain is the
/// host-supplied control-plane seed the trust model expects; it MUST ship the
/// [`MUSL_TARGET`] static std (`ensure_musl_target`). See the module doc for the
/// resolution order. NEVER invokes guix/guile. A resolved-but-unusable toolchain
/// is [`ProvisionErr::Broken`] (RED); only a wholly absent one is
/// [`ProvisionErr::Unavailable`] (a tolerated skip).
pub(crate) fn provision_rust(env: &ProvisionEnv) -> Result<String, ProvisionErr> {
    // 1. Explicitly provided toolchain.
    if let Some(home) = &env.rust_home {
        let b = format!("{home}/bin");
        let bp = Path::new(&b);
        if !(is_exec(&bp.join("rustc")) && is_exec(&bp.join("cargo"))) {
            return Err(ProvisionErr::Broken(format!(
                "TD_RUST_HOME={home} has no bin/rustc + bin/cargo"
            )));
        }
        ensure_musl_target(&b).map_err(ProvisionErr::Broken)?;
        return Ok(b);
    }

    // 2. rustc + cargo already on PATH — a host-supplied toolchain (rustup's
    //    default, a distro package, or a guix-home profile). This is the primary
    //    guix-free resolution: no /gnu/store pin, just whatever the host provides.
    if let (Some(rustc), Some(cargo)) = (
        find_in_path(&env.search_path, "rustc"),
        find_in_path(&env.search_path, "cargo"),
    ) {
        if let (Some(rd), Some(cd)) = (rustc.parent(), cargo.parent()) {
            let (rb, cb) = (rd.to_string_lossy(), cd.to_string_lossy());
            ensure_musl_target(&rb).map_err(ProvisionErr::Broken)?;
            return Ok(emit_frag(&rb, &cb));
        }
    }

    // 3. rustup — fetch the pinned toolchain + the musl target (a host without
    //    rust on PATH).
    if let Some(rustup) = find_in_path(&env.search_path, "rustup") {
        // rustup is PRESENT: every failure past here is a rustup/toolchain fault
        // (install, target-add, a bad `which`), i.e. Broken — a hard RED, not the
        // absent-toolchain skip.
        let ver = &env.rust_version;
        let broken = |m: String| ProvisionErr::Broken(m);
        let install = Command::new(&rustup)
            .args(["toolchain", "install", ver, "--profile", "minimal", "--no-self-update"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| broken(format!("spawn {}: {e}", rustup.display())))?;
        forward_to_stderr(&install);
        if !install.status.success() {
            return Err(broken(format!("rustup could not install toolchain {ver}")));
        }
        // The musl static std is REQUIRED for the +crt-static build.
        let addtarget = Command::new(&rustup)
            .args(["target", "add", "--toolchain", ver, MUSL_TARGET])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| broken(format!("spawn {}: {e}", rustup.display())))?;
        forward_to_stderr(&addtarget);
        if !addtarget.status.success() {
            return Err(broken(format!(
                "rustup could not add the {MUSL_TARGET} target to {ver}"
            )));
        }
        let which = Command::new(&rustup)
            .args(["which", "--toolchain", ver, "rustc"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| broken(format!("spawn {}: {e}", rustup.display())))?;
        if !which.status.success() {
            forward_to_stderr(&which);
            return Err(broken(format!("'rustup which rustc' failed for {ver}")));
        }
        let rustc = String::from_utf8_lossy(&which.stdout).trim().to_string();
        let d = Path::new(&rustc)
            .parent()
            .ok_or_else(|| broken(format!("rustup gave a rootless rustc path `{rustc}'")))?;
        if !(is_exec(&d.join("rustc")) && is_exec(&d.join("cargo"))) {
            return Err(broken(format!(
                "rustup toolchain {ver} at {} lacks rustc+cargo",
                d.display()
            )));
        }
        let db = d.to_string_lossy().into_owned();
        ensure_musl_target(&db).map_err(ProvisionErr::Broken)?;
        return Ok(db);
    }

    Err(ProvisionErr::Unavailable(
        "no Rust toolchain found — set TD_RUST_HOME to a provided toolchain, put rustc+cargo \
         on PATH, or install rustup (DESIGN §Provenance)"
            .to_string(),
    ))
}

fn has_cc(bin_dir: &Path) -> bool {
    is_exec(&bin_dir.join("gcc")) || is_exec(&bin_dir.join("cc"))
}

/// Resolve a C toolchain (gcc/cc) for the td-builder SEED build. Its role after
/// the musl cutover is NARROW: it links the HOST build script (`build.rs`,
/// compiled for the host triple, never placed) and compiles ring's C/asm in the
/// network tools (`host_cargo_bin`). It is NOT the target link driver — the
/// bundled `rust-lld` links the [`MUSL_TARGET`] binary directly. NEVER invokes guix.
pub(crate) fn provision_cc(env: &ProvisionEnv) -> Result<String, ProvisionErr> {
    // 1. Explicitly provided toolchain.
    if let Some(home) = &env.cc_home {
        let b = format!("{home}/bin");
        if !has_cc(Path::new(&b)) {
            return Err(ProvisionErr::Broken(format!(
                "TD_CC_HOME={home} has no bin/gcc or bin/cc"
            )));
        }
        return Ok(b);
    }

    // 2. System cc/gcc on PATH — the host-supplied control-plane seed.
    if let Some(cc) =
        find_in_path(&env.search_path, "cc").or_else(|| find_in_path(&env.search_path, "gcc"))
    {
        if let Some(d) = cc.parent() {
            if !has_cc(d) {
                return Err(ProvisionErr::Broken(format!(
                    "the system cc at {} is not usable",
                    d.display()
                )));
            }
            return Ok(d.to_string_lossy().into_owned());
        }
    }

    Err(ProvisionErr::Unavailable(
        "no C toolchain found — set TD_CC_HOME to a provided toolchain or put cc/gcc on PATH \
         (build-essential)"
            .to_string(),
    ))
}

/// The target triple every host-side control-plane binary is built for. Its
/// rust-std (`rust-std-x86_64-unknown-linux-musl`) ships the self-contained musl
/// `libc.a` + crt objects, so a `+crt-static` build links a pure-`std` binary
/// with an EMPTY runtime closure — no host glibc, no gcc-driven crt, and no guix
/// `/gnu/store` glibc:static pin (the retired seed). This is the source of the
/// static libc that replaced `provision_glibc_static` (re #469).
pub(crate) const MUSL_TARGET: &str = "x86_64-unknown-linux-musl";

/// The rustc flags that fully-static-link a control-plane binary for
/// [`MUSL_TARGET`], as an ordered arg list: `+crt-static` pulls in musl's
/// self-contained `libc.a`; the bundled `rust-lld` (`-C linker=rust-lld -C
/// linker-flavor=ld.lld`, resolved from rustc's own sysroot) links it with NO
/// external `cc`/`ld`. The result has an EMPTY runtime-link closure (no
/// PT_INTERP, no DT_NEEDED, no DT_RUNPATH), so staging it into a build sandbox
/// pulls in no host `lib/` (re #469).
fn musl_static_flags() -> [&'static str; 6] {
    [
        "-C",
        "target-feature=+crt-static",
        "-C",
        "linker=rust-lld",
        "-C",
        "linker-flavor=ld.lld",
    ]
}

/// [`musl_static_flags`] in `CARGO_ENCODED_RUSTFLAGS` form: one rustc ARGUMENT
/// per `\x1f`-separated field. This is cargo's HIGHEST-precedence rustflags
/// source, so it wins UNCONDITIONALLY over any ambient `RUSTFLAGS` — critically,
/// over the guix cargo wrapper, which injects `RUSTFLAGS="… -C linker=<gcc> -C
/// link-arg=-Wl,-rpath,<gcc-lib>"` at RUNTIME (a per-target
/// `CARGO_TARGET_<triple>_RUSTFLAGS` is OUTRANKED by that global `RUSTFLAGS` and
/// silently loses `rust-lld`, relinking with the gcc driver and baking in a
/// mutable guix-home DT_RUNPATH that fails `assert_static`). Every host-side
/// control-plane build site (`bootstrap_stage0`, `host_cargo_bin`, the recipe-eval
/// gate, `tests/recipe-eval-tool.sh`) sets exactly this so each links IDENTICALLY.
pub(crate) fn musl_static_encoded_rustflags() -> String {
    musl_static_flags().join("\u{1f}")
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

/// The host triple from `rustc -vV`'s `host:` line — the triple cargo compiles
/// build scripts / proc-macros for when the primary build targets [`MUSL_TARGET`].
pub(crate) fn rustc_host_triple(rustc: &Path) -> Result<String, String> {
    let vv = Command::new(rustc)
        .arg("-vV")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", rustc.display()))?;
    if !vv.status.success() {
        return Err(format!("`rustc -vV` failed for {}", rustc.display()));
    }
    String::from_utf8_lossy(&vv.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| format!("no `host:` line in `rustc -vV` from {}", rustc.display()))
}

/// cargo normalizes BOTH `-` and `.` to `_` in the `CARGO_TARGET_<triple>_*`
/// env-var name (host triples are dot-free today, but match cargo's rule exactly).
pub(crate) fn target_linker_var(triple: &str) -> String {
    format!("CARGO_TARGET_{}_LINKER", triple.to_uppercase().replace(['-', '.'], "_"))
}

/// Produce a STAGE0 td-builder from the checked-in builder/ source using ONLY
/// a host Rust toolchain — NO guix daemon, NO Guile, NO host shell. Writes
/// OUT_DIR/bin/td-builder and returns its path. td-builder has ZERO external
/// crate deps (std-only), so the OFFLINE `--frozen` build needs only
/// rustc/cargo (+ a host cc to link the build script); it runs under a CLEARED
/// environment with only the provisioned toolchain on PATH (the old `env -i`).
///
/// The build targets [`MUSL_TARGET`] with [`musl_static_encoded_rustflags`]: a
/// fully static binary with an EMPTY runtime closure, so staging it into a build
/// sandbox pulls in NO host `lib/` — the sole way to keep host libraries (and
/// stray +x libtool archives beside them) out of the sandbox entirely (re #469).
/// The MUSL_TARGET link uses the bundled `rust-lld` (no external cc); the host
/// `build.rs` (compiled for the host triple, never placed) links with the
/// provisioned cc. The result is asserted static before it is used.
pub(crate) fn bootstrap_stage0(
    root: &Path,
    penv: &ProvisionEnv,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    // An ABSENT toolchain is a PROVISIONING gap (re #469): `ProvisionErr::tagged`
    // carries the UNPROVISIONED_TAG so the stage0-place verb maps it to
    // EXIT_UNPROVISIONED and a cold compile with no reachable toolchain (e.g.
    // stage0-cold-start's cold leg in a host-tool-free jail) degrades to
    // Unprovisioned/tolerated. A RESOLVED-but-broken toolchain is untagged →
    // FAILURE → the (blocking) bootstrap gate REDs, never a silent skip.
    let rustpath = provision_rust(penv).map_err(|e| e.tagged())?;
    let ccpath = provision_cc(penv).map_err(|e| e.tagged())?;
    // The host toolchain may legitimately live under a guix profile (the
    // host-supplied control-plane seed the trust model expects); its provenance
    // is NOT what "guix-free" gates. The guix-free guarantee is the STATIC musl
    // OUTPUT (asserted below), which embeds no runtime guix dependency.
    let bootpath = format!("{rustpath}:{ccpath}");

    let work = scratch_dir("stage0-boot")?;
    let _work_guard = RemoveOnDrop(work.clone());
    // Resolve cargo/rustc/cc to absolute paths ourselves — the child's PATH is
    // the cleared bootpath, and the binaries we exec/pin must come from it.
    let cargo = find_in_path(&bootpath, "cargo")
        .ok_or_else(|| format!("no cargo on the provisioned toolchain PATH ({bootpath})"))?;
    let rustc = find_in_path(&bootpath, "rustc")
        .ok_or_else(|| format!("no rustc on the provisioned toolchain PATH ({bootpath})"))?;
    // Links the HOST build script only; `cc` may not exist by that name (a guix
    // profile exposes only `gcc`), so pin gcc/cc explicitly as the host linker.
    let cc = find_in_path(&bootpath, "cc")
        .or_else(|| find_in_path(&bootpath, "gcc"))
        .ok_or_else(|| format!("no cc/gcc on the provisioned toolchain PATH ({bootpath})"))?;
    let host_triple = rustc_host_triple(&rustc)?;
    let build = Command::new(&cargo)
        .env_clear()
        .env("PATH", &bootpath)
        .env("HOME", &work)
        .env("CARGO_HOME", work.join("cargo"))
        // CARGO_ENCODED_RUSTFLAGS (highest precedence) — NOT a per-target
        // CARGO_TARGET_<musl>_RUSTFLAGS: a guix cargo is a wrapper that re-injects
        // `RUSTFLAGS="… -C linker=<gcc> -rpath …"` at RUNTIME (after our env_clear),
        // and that global RUSTFLAGS OUTRANKS the per-target var, silently dropping
        // `rust-lld` and baking a mutable guix-home DT_RUNPATH that fails
        // assert_static. With `--target MUSL_TARGET`, these flags hit the MUSL_TARGET
        // binary ONLY; the host build script/proc-macros link with the provisioned cc.
        .env("CARGO_ENCODED_RUSTFLAGS", musl_static_encoded_rustflags())
        .env(target_linker_var(&host_triple), &cc)
        .args([
            "build",
            "--release",
            "--offline",
            "--frozen",
            "--target",
            MUSL_TARGET,
            "--manifest-path",
        ])
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

    let built = work
        .join("target")
        .join(MUSL_TARGET)
        .join("release/td-builder");
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
    // Smoke: RUN the just-placed static builder (its bare-invocation sentinel).
    // `assert_static` proves the SHAPE; this proves it actually runs — a broken
    // toolchain (missing/incompatible musl std) would link but fail to execute.
    let smoke = Command::new(&dest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn placed stage0 builder {}: {e}", dest.display()))?;
    if !smoke.status.success() {
        forward_to_stderr(&smoke);
        return Err(format!(
            "the placed static stage0 builder {} does not run (exit {:?}) — the provisioned \
             rust toolchain's {MUSL_TARGET} std may be broken or incompatible (re #469)",
            dest.display(),
            smoke.status.code()
        ));
    }
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
    let store = base.join("store");
    let db = base.join("builder.db");
    let meta = base.join(".stage0-meta");

    // Fingerprint the builder source the stage0 is compiled from — reuse only
    // if unchanged. Absolute roots: the caller's cwd must not matter. The
    // seed-digest table is `include_str!`-compiled INTO the builder (main.rs
    // SEED_DIGESTS), so it is a genuine compile input to the placed binary and
    // MUST be fingerprinted too — otherwise adding a source pin (a new
    // seed-digests row) leaves the prior placement's compiled table in force
    // and the new pin reads as an unpinned seed (re #469). The builder now
    // compiles the shared `td-engine` lib (JSON + SHA-256) as a path dependency,
    // resolved through the workspace-root Cargo.toml/Cargo.lock (which also carry
    // the release profile + member set), so engine/src, engine/Cargo.toml, and
    // both workspace-root files are compile inputs and join the fingerprint too —
    // else an engine edit leaves a stale placement in force.
    let fp_roots: Vec<String> = [
        "builder/src",
        "builder/build.rs",
        "builder/Cargo.toml",
        "engine/src",
        "engine/Cargo.toml",
        "Cargo.toml",
        "Cargo.lock",
        "seed/seed-digests.txt",
    ]
    .iter()
    .map(|p| root.join(p).to_string_lossy().into_owned())
    .collect();
    let fp = crate::tree_fingerprint(&fp_roots)?;
    // The fingerprint keys on the builder SOURCE only. The musl static binary is
    // self-contained (its runtime closure is empty), so any conforming host
    // toolchain that builds this source yields an equivalent, correct builder —
    // unlike the retired glibc:static path, a toolchain change carries no
    // crash-risk that would force a re-place (re #469).

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
    let s0 = bootstrap_stage0(root, &penv, &s0_dir)?;
    if !is_exec(&s0) {
        return Err("bootstrap produced no stage0 td-builder".to_string());
    }

    // 2. stage0 places ITSELF into the td store (its OWN store-add-builder;
    //    refs are scanned vs the seed-scan dir's entries — a readdir). The musl
    //    static builder embeds NO external store paths in its runtime closure, so
    //    the scan is vacuous: pass an EMPTY dir so no candidate matches (in
    //    particular the guix rust-sysroot strings in std panic metadata are NOT
    //    registered as refs) → a self-only closure, exactly right guix-free.
    std::fs::create_dir_all(&store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;
    let seedscan = work.join("empty-seedscan");
    std::fs::create_dir_all(&seedscan)
        .map_err(|e| format!("mkdir {}: {e}", seedscan.display()))?;
    let place = Command::new(&s0)
        .args(["store-add-builder", "td-builder-0.1.0"])
        .arg(&s0_dir)
        .arg(&store)
        .arg(&db)
        .arg(&seedscan) // SEED-scan dir: empty — a musl static builder has no external store refs
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", s0.display()))?;
    if !place.status.success() {
        forward_to_stderr(&place);
        return Err("stage0 store-add-builder failed (see stderr)".to_string());
    }
    let cb = String::from_utf8_lossy(&place.stdout).trim().to_string();
    // The canonical name tracks the ACTIVE store prefix — store-add-builder derives
    // it from store::store_dir() (the SEED-scan dir above is unrelated), so validate
    // against that, not a hardcoded /gnu/store now the default is `/td/store`. The
    // subprocess inherited this process's env, so store_dir() here matches its.
    let store_prefix = format!("{}/", crate::store::store_dir());
    if !(cb.starts_with(&store_prefix) && cb.ends_with("-td-builder-0.1.0")) {
        return Err(format!(
            "store-add-builder gave a malformed path `{cb}' (expected prefix {store_prefix})"
        ));
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

    /// A shebang line naming a POSIX shell that EXISTS here: a dev host's
    /// `/bin/sh`, else `sh` resolved from PATH — the loop host-sandbox is
    /// pivot_root'd with no `/bin/sh`, but its busybox userland puts `sh` on PATH.
    /// The fixture rustc/cc stubs `provision_rust`/`provision_cc` exec need a real
    /// interpreter in both, so they run for real in the sandbox rather than
    /// exec-failing.
    fn sh_shebang() -> String {
        if Path::new("/bin/sh").exists() {
            return "#!/bin/sh\n".to_string();
        }
        let sh = find_in_path(&std::env::var("PATH").unwrap_or_default(), "sh")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/bin/sh".to_string());
        format!("#!{sh}\n")
    }

    /// Write an executable shell fixture (a fake `cc`/`gcc`) with a shebang that
    /// resolves in this environment (see `sh_shebang`).
    fn write_exec(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{}{body}", sh_shebang())).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn exec_file(p: &Path) {
        write_exec(p, "");
    }

    /// Write a fake rust toolchain at `bin/` whose `rustc` answers `--print
    /// sysroot` with `sysroot`, and materialize the [`MUSL_TARGET`]
    /// self-contained `libc.a` under it so [`ensure_musl_target`] passes — the
    /// contract `provision_rust` now enforces (a resolved toolchain MUST ship the
    /// musl static std). `cargo` is a bare stub (never exec'd in resolution).
    fn write_rust_toolchain(bin: &Path, sysroot: &Path) {
        write_exec(
            &bin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                sysroot.display()
            ),
        );
        exec_file(&bin.join("cargo"));
        let libc = sysroot
            .join("lib/rustlib")
            .join(MUSL_TARGET)
            .join("lib/self-contained/libc.a");
        std::fs::create_dir_all(libc.parent().unwrap()).unwrap();
        std::fs::write(&libc, b"!<arch>\n").unwrap();
    }

    /// A hermetic resolver env: no homes, an EMPTY search path — so no host
    /// rustup/cc can leak into a test's resolution.
    fn base_env() -> ProvisionEnv {
        ProvisionEnv {
            rust_home: None,
            cc_home: None,
            rust_version: "1.96.0".to_string(),
            search_path: String::new(),
        }
    }

    // Pin the exact `\x1f`-field layout of the encoded musl rustflags (review PR
    // #534, P3): every host-side control-plane build site sets CARGO_ENCODED_RUSTFLAGS
    // (highest precedence — the only form the guix cargo wrapper cannot outrank), and
    // cargo parses one rustc argument per field, so a refactor that merged `-C` with
    // its value (or a space-joined form) would silently mis-apply the static flags.
    // The flags carry NO `-L` (musl's self-contained libc.a is resolved from rust-std,
    // not a glibc:static search dir) and NO external linker (`rust-lld` from rustc's
    // own sysroot links the MUSL_TARGET binary).
    #[test]
    fn musl_static_encoded_rustflags_uses_one_rustc_arg_per_unit_separator_field() {
        let enc = musl_static_encoded_rustflags();
        assert_eq!(
            enc.split('\u{1f}').collect::<Vec<_>>(),
            vec![
                "-C",
                "target-feature=+crt-static",
                "-C",
                "linker=rust-lld",
                "-C",
                "linker-flavor=ld.lld",
            ]
        );
        assert!(!enc.contains(' '), "no field may be space-joined");
    }

    // ensure_musl_target gates a resolved toolchain on the MUSL_TARGET self-
    // contained libc.a actually being present under the rustc sysroot — a clear
    // upfront error rather than a cryptic deep link failure (re #469).
    #[test]
    fn ensure_musl_target_requires_the_self_contained_libc_a() {
        let d = scratch("ensure-musl");
        let bin = d.join("rust/bin");
        let sysroot = d.join("rust");
        // A rustc reporting a sysroot that HAS the musl libc.a passes.
        write_rust_toolchain(&bin, &sysroot);
        assert!(ensure_musl_target(&bin.to_string_lossy()).is_ok());
        // Remove the libc.a → the same toolchain now reds with guidance.
        let libc = sysroot
            .join("lib/rustlib")
            .join(MUSL_TARGET)
            .join("lib/self-contained/libc.a");
        std::fs::remove_file(&libc).unwrap();
        assert!(ensure_musl_target(&bin.to_string_lossy())
            .unwrap_err()
            .contains(MUSL_TARGET));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn provision_resolves_the_provided_toolchain_first_and_reds_an_unusable_one() {
        let d = scratch("provided");
        // A musl-capable fake rust toolchain (rustc answers --print sysroot; the
        // MUSL_TARGET libc.a is present) — provision_rust now enforces that.
        write_rust_toolchain(&d.join("rust/bin"), &d.join("rust"));
        exec_file(&d.join("cc/bin/gcc"));
        let mut env = base_env();
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
        // A PROVIDED-but-unusable home is a BROKEN error (the operator asked for
        // it) — a hard RED, not a silent fallthrough and not a tolerated skip.
        env.rust_home = Some(d.join("empty").to_string_lossy().into_owned());
        env.cc_home = env.rust_home.clone();
        assert!(
            matches!(provision_rust(&env).unwrap_err(), ProvisionErr::Broken(m) if m.contains("TD_RUST_HOME"))
        );
        assert!(
            matches!(provision_cc(&env).unwrap_err(), ProvisionErr::Broken(m) if m.contains("TD_CC_HOME"))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn provision_rust_rejects_a_provided_toolchain_without_the_musl_std() {
        // A resolvable rustc+cargo whose sysroot LACKS the MUSL_TARGET libc.a is
        // rejected up front (guix-free: the static libc comes from rust-std-musl,
        // not a glibc:static pin), pointing the operator at `rustup target add`.
        let d = scratch("no-musl");
        let bin = d.join("rust/bin");
        // rustc reports a sysroot, but we do NOT create the self-contained libc.a.
        write_exec(
            &bin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                d.join("rust").display()
            ),
        );
        exec_file(&bin.join("cargo"));
        let mut env = base_env();
        env.rust_home = Some(d.join("rust").to_string_lossy().into_owned());
        // A resolved toolchain missing the musl std is BROKEN (RED), not absent.
        let err = provision_rust(&env).unwrap_err();
        assert!(
            matches!(&err, ProvisionErr::Broken(m) if m.contains(MUSL_TARGET)),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn provision_falls_back_to_path_then_reds_with_guidance() {
        let d = scratch("path");
        // The PATH leg: rustc+cargo (musl-capable) on the search path resolve to
        // their bin dir — the primary guix-free resolution (no /gnu/store pin).
        let rbin = d.join("toolchain/bin");
        write_rust_toolchain(&rbin, &d.join("toolchain"));
        let mut env = base_env();
        env.search_path = rbin.to_string_lossy().into_owned();
        assert_eq!(provision_rust(&env).unwrap(), rbin.to_string_lossy());

        // An EMPTY search path (no rustc/cargo/rustup) is UNAVAILABLE — the
        // EXIT_UNPROVISIONED (69) / tolerated-skip arm at the verb, distinct from
        // a Broken resolved toolchain.
        let env2 = base_env();
        assert!(
            matches!(provision_rust(&env2).unwrap_err(), ProvisionErr::Unavailable(m) if m.contains("no Rust toolchain"))
        );
        assert!(
            matches!(provision_cc(&env2).unwrap_err(), ProvisionErr::Unavailable(m) if m.contains("no C toolchain"))
        );

        // System cc leg: a cc on the search path resolves to its bin dir.
        let sysd = d.join("sysbin");
        exec_file(&sysd.join("cc"));
        let mut env3 = base_env();
        env3.search_path = sysd.to_string_lossy().into_owned();
        assert_eq!(provision_cc(&env3).unwrap(), sysd.to_string_lossy());
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
