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

fn rustc_sysroot(rustc: &Path) -> Result<String, String> {
    let out = Command::new(rustc)
        .arg("--print")
        .arg("sysroot")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", rustc.display()))?;
    if !out.status.success() {
        return Err(format!("`rustc --print sysroot` failed for {}", rustc.display()));
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // A wrapper rustc that exits 0 without handling `--print sysroot` answers with
    // nothing, and `Path::new("").join("bin")` is the RELATIVE path `bin`. That would
    // both choose a PATH fragment and decide whether the musl std is missing — the
    // second of which spends a network fetch and mutates the host — off a directory
    // this process happens to be standing in.
    if !Path::new(&sysroot).is_absolute() {
        return Err(format!(
            "`rustc --print sysroot` from {} answered {sysroot:?}, which is not an absolute path",
            rustc.display()
        ));
    }
    Ok(sysroot)
}

/// A resolved toolchain: the bin dir to put on the cleared-env PATH, and the sysroot
/// its rustc reported. The two travel together because asking twice is both a second
/// subprocess and an unchecked assumption that the two answers agree.
struct Toolchain {
    bin: String,
    sysroot: String,
}

/// The toolchain `rustc` belongs to. A `rustc` is usually a rustup SHIM
/// (`~/.cargo/bin/rustc`) that re-enters rustup to pick a toolchain, and
/// [`bootstrap_stage0`] runs cargo under a CLEARED environment with `HOME` pointed at
/// its own scratch — where the shim finds neither a `RUSTUP_HOME` nor a default and
/// exits before cargo starts. So `bin` is the SYSROOT's bin dir whenever that holds a
/// complete rustc+cargo, which is what turns a shim into the toolchain it stands for.
/// A sysroot with no such dir falls back to where `rustc` was found, which is what
/// keeps guix and nix (cargo in a separate output) on their own layout. It is not
/// shim-specific: any rustc whose sysroot bin holds both binaries is followed there,
/// so a flag-injecting wrapper is bypassed in favour of what it wraps. Shims are a
/// property of the BINARIES,
/// not of how they were found, so `TD_RUST_HOME` goes through this too — pointing it
/// at `~/.cargo` is a plausible reading of "the rust home" and used to fail exactly
/// like the PATH case, deep in the build.
fn toolchain_at(rustc: &Path) -> Result<Toolchain, String> {
    let sysroot = rustc_sysroot(rustc)?;
    let sysbin = Path::new(&sysroot).join("bin");
    let bin = if is_exec(&sysbin.join("rustc")) && is_exec(&sysbin.join("cargo")) {
        sysbin.to_string_lossy().into_owned()
    } else {
        rustc
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(Toolchain { bin, sysroot })
}

/// Add [`MUSL_TARGET`] to the toolchain `rust_bin_dir` came from, when rustup owns
/// it. Resolution path 3 already does this for a toolchain rustup INSTALLS; a host
/// that brought its own rustup toolchain takes path 2 instead and used to get a hard
/// error telling it to run this very command by hand. `rustup which rustc` names the
/// ACTIVE toolchain's rustc: only when that is the toolchain we resolved may we add
/// to it, otherwise the target lands somewhere this build never looks. The add itself
/// takes no `--toolchain` (unlike path 3, which names the one it just installed), so
/// what closes the gap between the two calls is the caller re-checking the sysroot
/// afterwards — an add that landed elsewhere leaves the std still missing and reds.
///
/// This fetches over the network and mutates the host toolchain. `TD_RUST_HOME` is the
/// way to decline: it resolves at path 1, which never reaches rustup.
fn rustup_add_musl_target(search_path: &str, rust_bin_dir: &str) -> Result<(), String> {
    let rustup =
        find_in_path(search_path, "rustup").ok_or_else(|| "no rustup on PATH".to_string())?;
    let which = Command::new(&rustup)
        .args(["which", "rustc"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", rustup.display()))?;
    if !which.status.success() {
        // Path 3 forwards on the identical call: WHY rustup refused is the entire
        // content of this failure, and capturing it drops it.
        forward_to_stderr(&which);
        return Err("`rustup which rustc` failed".to_string());
    }
    // Compare the two rustc paths RESOLVED, never lexically: rustup answers out of
    // `$HOME` while the sysroot answer comes from rustc itself, and this very host
    // reaches one toolchain as both /home/timmy/.rustup/… and
    // /var/home/timmy/.rustup/… because /home is a symlink. A lexical compare
    // refuses the add on exactly the stock rustup host it exists for.
    // A path that will not canonicalize is `None`, and two `None`s must not read as
    // agreement, so an unresolvable side refuses.
    let active = PathBuf::from(String::from_utf8_lossy(&which.stdout).trim());
    let resolved = Path::new(rust_bin_dir).join("rustc");
    let same = match (active.canonicalize(), resolved.canonicalize()) {
        (Ok(a), Ok(r)) => a == r,
        _ => false,
    };
    if !same {
        return Err(format!(
            "rustup's active rustc ({}) is not the resolved toolchain ({})",
            active.display(),
            resolved.display()
        ));
    }
    eprintln!("td-builder: adding the {MUSL_TARGET} target to rustup's active toolchain");
    let add = Command::new(&rustup)
        .args(["target", "add", MUSL_TARGET])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", rustup.display()))?;
    forward_to_stderr(&add);
    if !add.status.success() {
        return Err(format!("`rustup target add {MUSL_TARGET}` failed"));
    }
    Ok(())
}

/// Why the musl check said no, split by whether an ABSENCE was actually diagnosed.
/// Only `Missing` may be answered with a host-mutating `rustup target add`; anything
/// else would install a target nobody established was missing.
enum MuslErr {
    /// The self-contained `libc.a` is not there.
    Missing(String),
    /// The question could not be answered: rustc would not report a sysroot, or the
    /// `libc.a` could not be stat'd (permissions, I/O, a symlink loop), or something
    /// that is not a file sits at its path.
    Undiagnosed(String),
}

impl MuslErr {
    fn into_message(self) -> String {
        match self {
            MuslErr::Missing(m) | MuslErr::Undiagnosed(m) => m,
        }
    }
}

/// The provisioned rust MUST ship [`MUSL_TARGET`]'s self-contained static std
/// (`rust-std-x86_64-unknown-linux-musl`) — the source of the `+crt-static`
/// `libc.a` that replaced the guix glibc:static pin. Verify it once, up front,
/// with a clear message rather than a cryptic link failure deep in the build.
fn musl_std_in(sysroot: &str) -> Result<(), MuslErr> {
    let libc_a = Path::new(sysroot)
        .join("lib/rustlib")
        .join(MUSL_TARGET)
        .join("lib/self-contained/libc.a");
    // Only a NotFound is an absence. A permission error, an I/O error or a symlink
    // loop all leave the question open, and answering them by installing the target
    // is a host mutation on top of a fault nobody diagnosed.
    match std::fs::metadata(&libc_a) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(MuslErr::Undiagnosed(format!(
            "{} exists but is not a file; the toolchain's musl rust-std is unusable",
            libc_a.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(MuslErr::Missing(format!(
            "the provisioned rust toolchain lacks the {MUSL_TARGET} static std (missing {}) — \
             add it with `rustup target add {MUSL_TARGET}` or provide a TD_RUST_HOME whose \
             rust-std ships the self-contained musl libc.a",
            libc_a.display()
        ))),
        Err(e) => Err(MuslErr::Undiagnosed(format!("stat {}: {e}", libc_a.display()))),
    }
}

/// `musl_std_in` for a caller holding a bin dir rather than a sysroot (resolution
/// path 3, whose toolchain rustup just named): ask rustc, then check.
fn ensure_musl_target(rust_bin_dir: &str) -> Result<(), MuslErr> {
    let sysroot =
        rustc_sysroot(&Path::new(rust_bin_dir).join("rustc")).map_err(MuslErr::Undiagnosed)?;
    musl_std_in(&sysroot)
}

/// `ensure_musl_target` for the callers that have no retry to gate: any failure is
/// simply Broken.
fn require_musl_target(rust_bin_dir: &str) -> Result<(), ProvisionErr> {
    ensure_musl_target(rust_bin_dir).map_err(|e| ProvisionErr::Broken(e.into_message()))
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

/// Tag a failed child's message as a provisioning gap only on gate-run's OWN
/// two-part test: code 69 AND the sentinel `unprovisioned_exit` prints on its way
/// out. The verb re-emits that sentinel downstream, so trusting the code alone
/// would let any other 69 mint one and turn a regression into a tolerated skip.
fn tag_child_failure(out: &std::process::Output, msg: String) -> String {
    if td_engine::exit::child_reported_host_gap(out.status.code(), &out.stdout, &out.stderr) {
        return format!("{}{msg}", crate::check_loop::UNPROVISIONED_TAG);
    }
    msg
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
        let tc = toolchain_at(&bp.join("rustc")).map_err(ProvisionErr::Broken)?;
        musl_std_in(&tc.sysroot).map_err(|e| ProvisionErr::Broken(e.into_message()))?;
        return Ok(tc.bin);
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
            let tc = toolchain_at(&rustc).map_err(ProvisionErr::Broken)?;
            // Both PATH dirs survive only when the sysroot supplied no bin dir of its
            // own: rustc and cargo can legitimately live apart on a distro host, but a
            // resolved toolchain's bin holds them both.
            let frag = if tc.bin == rb {
                emit_frag(&rb, &cb)
            } else {
                tc.bin.clone()
            };
            // Only a MISSING std is worth a `rustup target add`; a sysroot that could
            // not be asked is a different fault and reds as it stands.
            match musl_std_in(&tc.sysroot) {
                Ok(()) => {}
                Err(MuslErr::Undiagnosed(m)) => return Err(ProvisionErr::Broken(m)),
                Err(MuslErr::Missing(_)) => {
                    // The refused-add case gets its OWN message: `missing` ends with
                    // "add it with `rustup target add …`", and appending "that would
                    // install into a different toolchain" to it tells the operator to
                    // run a command the same sentence just said would not help.
                    rustup_add_musl_target(&env.search_path, &tc.bin).map_err(|why| {
                        ProvisionErr::Broken(format!(
                            "the resolved rust toolchain ({}) lacks the {MUSL_TARGET} static \
                             std and td could not add it: {why}. Add it to THAT toolchain \
                             (`rustup target add --toolchain <its name> {MUSL_TARGET}`), make \
                             it rustup's active toolchain, or set TD_RUST_HOME to a toolchain \
                             whose rust-std already ships the self-contained musl libc.a",
                            tc.bin
                        ))
                    })?;
                    musl_std_in(&tc.sysroot)
                        .map_err(|e| ProvisionErr::Broken(e.into_message()))?;
                }
            }
            return Ok(frag);
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
        require_musl_target(&db)?;
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
/// The memo, or WHY it could not be reused. A miss costs a compile, which in the
/// loop sandbox is not merely slow but impossible (no toolchain there), so the
/// reason has to be legible — "unprovisioned" alone sends the reader looking for
/// a missing toolchain when the real event is a memo that did not match.
fn stage0_memo_hit(meta: &Path, fp: &str, store: &Path, db: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(meta)
        .map_err(|e| format!("no memo at {}: {e}", meta.display()))?;
    let mut lines = text.lines();
    let old_fp = lines
        .next()
        .ok_or_else(|| format!("memo {} is empty", meta.display()))?;
    let cb = lines
        .next()
        .ok_or_else(|| format!("memo {} has no placement line", meta.display()))?
        .trim();
    if cb.is_empty() {
        return Err(format!("memo {} names no placement", meta.display()));
    }
    if old_fp != fp {
        return Err(format!(
            "builder source fingerprint moved (memo {old_fp}, tree {fp})"
        ));
    }
    let name = Path::new(cb)
        .file_name()
        .ok_or_else(|| format!("memo names a malformed placement `{cb}'"))?;
    let placed = store.join(name).join("bin/td-builder");
    if !is_exec(&placed) {
        return Err(format!("placement {} is missing", placed.display()));
    }
    if !std::fs::metadata(db).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return Err(format!("builder db {} is missing or empty", db.display()));
    }
    Ok(cb.to_string())
}

/// Every path an `include_str!`/`include_bytes!` in `files` names, resolved
/// against the file that names it, keeping only those that exist (so a literal
/// in a comment cannot invent a root).
///
/// DERIVED, not listed: the recipes crate embeds TARGET-crate sources from
/// outside `recipes/` — td-init, td-login, td-util and eight more — and a fixed
/// root list would silently miss them, leaving the memo to serve an evaluator
/// that emits yesterday's source as this build's recipe. Deriving it means a new
/// `include_str!` is covered without anyone remembering to edit this.
fn embedded_include_paths(files: &[PathBuf]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for f in files {
        let (Some(dir), Ok(text)) = (f.parent(), std::fs::read_to_string(f)) else {
            continue;
        };
        for mac in ["include_str!", "include_bytes!"] {
            let mut rest = text.as_str();
            while let Some(i) = rest.find(mac) {
                let after = rest.get(i + mac.len()..).unwrap_or("");
                let arg = after.trim_start_matches(|c: char| c == '(' || c.is_whitespace());
                // Fail CLOSED on a composed path (`concat!`/`env!`): the literal
                // scan cannot resolve one, and dropping it silently is how a
                // compile input escapes the fingerprint — the whole hazard here.
                // The crate already composes an `include!` this way, so treat a
                // nested macro as a demand to extend this, not as no input.
                if arg.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    && arg
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .is_some_and(|k| arg.get(k..k + 1) == Some("!"))
                {
                    return Err(format!(
                        "{}: {mac} takes a composed path, which the fingerprint scan cannot \
                         resolve — extend embedded_include_paths before using this form",
                        f.display()
                    ));
                }
                let Some(open) = after.find('"') else { break };
                let tail = after.get(open + 1..).unwrap_or("");
                let Some(end) = tail.find('"') else { break };
                if let Some(p) = tail.get(..end) {
                    // A literal naming nothing on disk is prose (`include_str!` in
                    // a comment), not an input; canonicalize so one file reached
                    // by two spellings hashes once.
                    let joined = dir.join(p);
                    if joined.is_file() {
                        let c = std::fs::canonicalize(&joined).unwrap_or(joined);
                        out.push(c.to_string_lossy().into_owned());
                    }
                }
                rest = tail.get(end + 1..).unwrap_or("");
            }
        }
    }
    Ok(out)
}

/// The evaluator's compile inputs — the mirror of the stage0 roots below, with
/// `recipes/` in place of `builder/`, PLUS everything the crate embeds (above)
/// and the tool script itself, whose static-linking contract a memo hit skips.
/// The seed-digest table is `include_str!`d into td-recipe-eval too, so a new
/// seed pin must not leave a stale compiled table in force.
fn recipe_eval_fp_roots(root: &Path) -> Result<Vec<String>, String> {
    let mut roots: Vec<String> = [
        "recipes/src",
        "recipes/build.rs",
        "recipes/Cargo.toml",
        "engine/src",
        "engine/Cargo.toml",
        "Cargo.toml",
        "Cargo.lock",
        "seed/seed-digests.txt",
        "tests/recipe-eval-tool.sh",
    ]
    .iter()
    .map(|p| root.join(p).to_string_lossy().into_owned())
    .collect();
    // Scan the two crates compiled INTO the evaluator. Only `.rs` can carry a
    // macro, and the fingerprint reads every file again to hash it.
    let scan: Vec<String> = ["recipes/src", "engine/src"]
        .iter()
        .map(|p| root.join(p).to_string_lossy().into_owned())
        .collect();
    let rs: Vec<PathBuf> = crate::regular_files_under(&scan)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    let mut embedded = embedded_include_paths(&rs)?;
    embedded.sort();
    embedded.dedup();
    roots.extend(embedded);
    Ok(roots)
}

/// The evaluator memo, or WHY it cannot serve.
fn recipe_eval_memo_hit(meta: &Path, fp: &str) -> Result<String, String> {
    let text =
        std::fs::read_to_string(meta).map_err(|e| format!("no memo at {}: {e}", meta.display()))?;
    let mut lines = text.lines();
    let old_fp = lines
        .next()
        .ok_or_else(|| format!("memo {} is empty", meta.display()))?;
    let bin = lines
        .next()
        .ok_or_else(|| format!("memo {} has no binary line", meta.display()))?
        .trim();
    if bin.is_empty() {
        return Err(format!("memo {} names no binary", meta.display()));
    }
    if old_fp != fp {
        return Err(format!(
            "recipes source fingerprint moved (memo {old_fp}, tree {fp})"
        ));
    }
    if !is_exec(Path::new(bin)) {
        return Err(format!("built evaluator {bin} is missing"));
    }
    // Unlike stage0's, this memo names a REWRITABLE path, so the path alone
    // vouches for nothing: a later build that lands bytes there and then fails
    // its static assertion writes no memo, and a revert would serve those bytes
    // under this fingerprint. Bind the contents.
    let want = lines
        .next()
        .ok_or_else(|| format!("memo {} records no digest", meta.display()))?
        .trim();
    let got = crate::sha256::sha256_file(Path::new(bin))
        .map_err(|e| format!("sha256 {bin}: {e}"))?;
    if got != want {
        return Err(format!("built evaluator {bin} is not the one memoized"));
    }
    Ok(bin.to_string())
}

/// Build td-recipe-eval under BASE and return its path, memoized on the recipes
/// source exactly as [`stage0_place`] memoizes the builder.
///
/// Speed is not the point: a HIT needs no toolchain, so the loop sandbox — which
/// has none, by design — can reuse what the host prelude built. A MISS still
/// needs a compiler, so only the prelude can serve one.
pub(crate) fn recipe_eval_place(root: &Path, base: &Path) -> Result<String, String> {
    let meta = base.join(".recipe-eval-meta");
    let fp = crate::tree_fingerprint(&recipe_eval_fp_roots(root)?)?;

    // Fast path: a valid memo needs no lock (warm loops skip the compile AND the
    // lock wait).
    if let Ok(bin) = recipe_eval_memo_hit(&meta, &fp) {
        ensure_recipe_eval_sentinel(base, &bin)?;
        return Ok(bin);
    }

    // Slow path: serialize builders sharing BASE, then re-check — a waiter may
    // now find the holder's fresh memo.
    std::fs::create_dir_all(base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
    let lock_path = base.join(".recipe-eval.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    lock_file
        .lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    let why = match recipe_eval_memo_hit(&meta, &fp) {
        Ok(bin) => {
            ensure_recipe_eval_sentinel(base, &bin)?;
            return Ok(bin);
        }
        Err(why) => why,
    };
    eprintln!(
        "td-builder: recipe-eval-place: rebuilding under {} — {why}",
        base.display()
    );

    let mut cmd = Command::new("sh");
    cmd.arg("tests/recipe-eval-tool.sh")
        .arg(base)
        .current_dir(root)
        .stdin(Stdio::null());
    // The tool resolves its toolchain through `$TD_BUILDER_SELF provision-{rust,cc}`.
    // We ARE a td-builder: name ourselves rather than rely on the gate-run export,
    // so a dev invocation works too.
    if let Ok(self_exe) = std::env::current_exe() {
        cmd.env("TD_BUILDER_SELF", self_exe);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("spawn sh tests/recipe-eval-tool.sh: {e}"))?;
    if !out.status.success() {
        forward_to_stderr(&out);
        let msg = "recipe-eval-tool.sh could not build td-recipe-eval (see stderr)".to_string();
        return Err(tag_child_failure(&out, msg));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let bin = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| "recipe-eval-tool.sh printed no td-recipe-eval path".to_string())?;
    if !is_exec(Path::new(bin)) {
        return Err(format!(
            "recipe-eval-tool.sh printed `{bin}', which is not executable"
        ));
    }
    let digest = crate::sha256::sha256_file(Path::new(bin))
        .map_err(|e| format!("sha256 {bin}: {e}"))?;
    std::fs::write(&meta, format!("{fp}\n{bin}\n{digest}\n"))
        .map_err(|e| format!("write {}: {e}", meta.display()))?;
    ensure_recipe_eval_sentinel(base, bin)?;
    Ok(bin.to_string())
}

/// Keep `recipe-eval-path` naming the binary the memo just served. The tool
/// script writes it on a BUILD, but a memo hit skips the script — and cache-lib's
/// `load_recipe_eval` and `resolve_recipe_eval` both read the sentinel, not the
/// memo, so a hit that left it absent or stale would send them elsewhere.
fn ensure_recipe_eval_sentinel(base: &Path, bin: &str) -> Result<(), String> {
    let sentinel = base.join("recipe-eval-path");
    if std::fs::read_to_string(&sentinel).is_ok_and(|t| t.trim() == bin) {
        return Ok(());
    }
    // Atomic: the fast path runs WITHOUT the place lock, and a truncating write
    // lets a concurrent gate read an empty sentinel and red with no cause.
    crate::write_atomic(&sentinel, format!("{bin}\n").as_bytes())
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
    if let Ok(cb) = stage0_memo_hit(&meta, &fp, &store, &db) {
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
    let why = match stage0_memo_hit(&meta, &fp, &store, &db) {
        Ok(cb) => {
            ensure_builder_lineage(&db, &cb, &fp)?;
            return Ok(cb);
        }
        Err(why) => why,
    };
    // About to COMPILE. Say so and say why the memo did not serve, because in the
    // loop sandbox this cannot succeed and the bare provisioning error that
    // follows names a missing toolchain rather than the reuse that failed.
    eprintln!("td-builder: stage0-place: rebuilding under {} — {why}", base.display());

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
    /// resolves in this environment (see `sh_shebang`), then PROVE it execs before
    /// handing it to the code under test.
    ///
    /// The proof is not ceremony — see `crate::spawn`, which owns the waiting and
    /// the reason for it: a sibling thread forking while our write fd is open holds
    /// that fd until its own exec, and the file has a writer for exactly that long.
    /// What is particular to HERE is where the failure lands. It surfaces inside
    /// whatever production call the fixture was written for, so it reads as a
    /// resolver bug — `spawn …/rustc: Text file busy` reported as a Broken
    /// toolchain — which is why the wait belongs at the fixture rather than in
    /// `provision_rust`, which would then carry it only for its tests. The probe
    /// argument matches no `case` arm in any fixture body, so running one is inert.
    fn write_exec(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{}{body}", sh_shebang())).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut probe = Command::new(p);
        probe.arg("--td-fixture-probe").stdin(Stdio::null());
        if let Err(e) = crate::spawn::past_a_busy_program(|| probe.output()) {
            assert!(
                !crate::spawn::is_busy(&e),
                "fixture {} never stopped being Text-file-busy",
                p.display()
            );
        }
    }

    fn exec_file(p: &Path) {
        write_exec(p, "");
    }

    fn musl_libc_path(sysroot: &Path) -> PathBuf {
        sysroot
            .join("lib/rustlib")
            .join(MUSL_TARGET)
            .join("lib/self-contained/libc.a")
    }

    fn write_musl_libc(sysroot: &Path) {
        let libc = musl_libc_path(sysroot);
        std::fs::create_dir_all(libc.parent().unwrap()).unwrap();
        std::fs::write(&libc, b"!<arch>\n").unwrap();
    }

    /// A fake `rustup` for the path-2 target-add leg: `which` names `active_rustc`
    /// (whatever the caller wants rustup's active toolchain to be) and `target add`
    /// materializes the musl std under `adds_to`. Splitting the two is what lets a
    /// test assert the add is REFUSED when they disagree.
    fn write_fake_rustup(dir: &Path, active_rustc: &Path, adds_to: &Path) {
        let libc = musl_libc_path(adds_to);
        write_exec(
            &dir.join("rustup"),
            &format!(
                "case \"$1\" in\n  which) echo '{}' ;;\n  target) mkdir -p '{}' && printf \
                 '!<arch>\\n' > '{}' ;;\nesac\n",
                active_rustc.display(),
                libc.parent().unwrap().display(),
                libc.display(),
            ),
        );
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
        write_musl_libc(sysroot);
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
        // Remove the libc.a → the same toolchain now reds with guidance, and the
        // failure is discriminated as MISSING: that is the only one path 2 may
        // answer with a host-mutating `rustup target add`.
        std::fs::remove_file(musl_libc_path(&sysroot)).unwrap();
        let err = ensure_musl_target(&bin.to_string_lossy()).unwrap_err();
        assert!(matches!(&err, MuslErr::Missing(m) if m.contains(MUSL_TARGET)));
        // A rustc that cannot answer `--print sysroot` at all is a DIFFERENT fault:
        // nothing diagnosed a target as missing, so nothing may be installed.
        write_exec(&bin.join("rustc"), "exit 1\n");
        assert!(matches!(
            ensure_musl_target(&bin.to_string_lossy()).unwrap_err(),
            MuslErr::Undiagnosed(_)
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The other half of the same rule, and the one that reaches the path-2 match
    /// arm: the sysroot resolves fine, but the `libc.a` path is occupied by something
    /// that is not a file. Nothing was diagnosed as ABSENT, so nothing may be
    /// installed — `rustup target add` would not fix a directory sitting there.
    #[test]
    fn an_undiagnosable_libc_a_never_reaches_rustup() {
        let d = scratch("musl-undiagnosable");
        let toolchain = d.join("toolchain");
        let rbin = toolchain.join("bin");
        write_exec(
            &rbin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                toolchain.display()
            ),
        );
        exec_file(&rbin.join("cargo"));
        // A DIRECTORY where the libc.a belongs.
        std::fs::create_dir_all(musl_libc_path(&toolchain)).unwrap();
        let rustup = d.join("rustup");
        write_fake_rustup(&rustup, &rbin.join("rustc"), &toolchain);
        let mut env = base_env();
        env.search_path = format!("{}:{}", rbin.display(), rustup.display());
        let err = provision_rust(&env).unwrap_err();
        assert!(
            matches!(&err, ProvisionErr::Broken(m) if m.contains("not a file")),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A shim that cannot resolve a toolchain fails `--print sysroot`, and answering
    /// THAT by installing a musl target would be a host mutation in response to an
    /// unrelated fault. Refused before the musl question is even asked.
    #[test]
    fn a_sysroot_failure_never_reaches_rustup() {
        let d = scratch("musl-sysroot-fault");
        let rbin = d.join("bin");
        write_exec(&rbin.join("rustc"), "exit 1\n");
        exec_file(&rbin.join("cargo"));
        // A rustup that WOULD add the target if it were ever asked.
        let rustup = d.join("rustup");
        write_fake_rustup(&rustup, &rbin.join("rustc"), &d.join("toolchain"));
        let mut env = base_env();
        env.search_path = format!("{}:{}", rbin.display(), rustup.display());
        assert!(matches!(provision_rust(&env).unwrap_err(), ProvisionErr::Broken(_)));
        assert!(
            !musl_libc_path(&d.join("toolchain")).exists(),
            "a sysroot failure triggered a target install"
        );
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

    /// A `rustc` on PATH is usually a rustup SHIM, and `bootstrap_stage0` runs cargo
    /// with a CLEARED environment where a shim finds neither a rustup home nor a
    /// default toolchain and exits before cargo starts. Resolution must hand back the
    /// toolchain the shim stands for, not the shim dir.
    #[test]
    fn a_shim_on_path_resolves_to_the_toolchain_it_stands_for() {
        let d = scratch("shim");
        let toolchain = d.join("toolchain");
        write_rust_toolchain(&toolchain.join("bin"), &toolchain);
        let shim = d.join("shim");
        write_exec(
            &shim.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                toolchain.display()
            ),
        );
        exec_file(&shim.join("cargo"));
        let mut env = base_env();
        env.search_path = shim.to_string_lossy().into_owned();
        assert_eq!(
            provision_rust(&env).unwrap(),
            toolchain.join("bin").to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Being a shim is a property of the BINARIES, not of how they were found, so an
    /// explicitly PROVIDED home gets the same rewrite: `TD_RUST_HOME=~/.cargo` is a
    /// plausible reading of "the rust home", and it used to pass the bin/rustc +
    /// bin/cargo check and then fail deep in the cleared-env build.
    #[test]
    fn a_provided_home_that_is_a_shim_dir_resolves_too() {
        let d = scratch("provided-shim");
        let toolchain = d.join("toolchain");
        write_rust_toolchain(&toolchain.join("bin"), &toolchain);
        let home = d.join("cargo");
        write_exec(
            &home.join("bin/rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                toolchain.display()
            ),
        );
        exec_file(&home.join("bin/cargo"));
        let mut env = base_env();
        env.rust_home = Some(home.to_string_lossy().into_owned());
        assert_eq!(
            provision_rust(&env).unwrap(),
            toolchain.join("bin").to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The shim rewrite is a PREFERENCE, not a requirement: a toolchain whose sysroot
    /// ships no `bin/` (a distro or guix rust) must keep the PATH dirs it resolved to
    /// rather than be pointed at a directory holding no toolchain at all.
    #[test]
    fn a_sysroot_without_rustc_and_cargo_keeps_the_path_dirs() {
        let d = scratch("nobin");
        let rbin = d.join("bin");
        let sysroot = d.join("sysroot");
        write_exec(
            &rbin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                sysroot.display()
            ),
        );
        exec_file(&rbin.join("cargo"));
        write_musl_libc(&sysroot);
        let mut env = base_env();
        env.search_path = rbin.to_string_lossy().into_owned();
        assert_eq!(provision_rust(&env).unwrap(), rbin.to_string_lossy());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Resolution path 3 already `rustup target add`s the musl std for a toolchain
    /// rustup INSTALLS. A host that brought its own rustup toolchain takes path 2, and
    /// used to be told to run that identical command by hand — the difference between
    /// a stock rustup host building out of the box and one that stops.
    #[test]
    fn a_missing_musl_std_is_added_through_rustup_rather_than_refused() {
        let d = scratch("musl-add");
        let toolchain = d.join("toolchain");
        let rbin = toolchain.join("bin");
        write_exec(
            &rbin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                toolchain.display()
            ),
        );
        exec_file(&rbin.join("cargo"));
        let libc = musl_libc_path(&toolchain);
        let rustup = d.join("rustup");
        write_fake_rustup(&rustup, &rbin.join("rustc"), &toolchain);
        let mut env = base_env();
        env.search_path = format!("{}:{}", rbin.display(), rustup.display());
        assert_eq!(provision_rust(&env).unwrap(), rbin.to_string_lossy());
        assert!(libc.is_file(), "the musl std was not added");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// rustup answers `which` out of `$HOME` while the sysroot answer comes from
    /// rustc, and those two spellings of one toolchain need not match as TEXT: this
    /// repo's own host reaches it as both /home/timmy/… and /var/home/timmy/…
    /// because /home is a symlink. A lexical compare refuses the add on exactly the
    /// stock rustup host it exists for.
    #[test]
    fn a_symlinked_home_is_still_the_same_toolchain() {
        let d = scratch("musl-symlink");
        let toolchain = d.join("real/toolchain");
        let rbin = toolchain.join("bin");
        write_exec(
            &rbin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                toolchain.display()
            ),
        );
        exec_file(&rbin.join("cargo"));
        // rustup names the SAME rustc through a symlinked parent.
        std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
        let rustup = d.join("rustup");
        write_fake_rustup(
            &rustup,
            &d.join("link/toolchain/bin/rustc"),
            &toolchain,
        );
        let mut env = base_env();
        env.search_path = format!("{}:{}", rbin.display(), rustup.display());
        assert_eq!(provision_rust(&env).unwrap(), rbin.to_string_lossy());
        assert!(musl_libc_path(&toolchain).is_file(), "the musl std was not added");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `rustup target add` acts on the ACTIVE toolchain. When that is not the one
    /// resolved off PATH, the std would land where this build never looks — so the
    /// add is refused and the original missing-std error stands.
    #[test]
    fn the_target_is_never_added_to_a_toolchain_this_build_will_not_use() {
        let d = scratch("musl-elsewhere");
        let toolchain = d.join("toolchain");
        let rbin = toolchain.join("bin");
        write_exec(
            &rbin.join("rustc"),
            &format!(
                "case \"$*\" in *'--print sysroot'*) echo '{}' ;; esac\n",
                toolchain.display()
            ),
        );
        exec_file(&rbin.join("cargo"));
        let libc = musl_libc_path(&toolchain);
        // rustup's active rustc is a DIFFERENT toolchain's; adding there is useless.
        // It must EXIST, or the refusal would come from an unresolvable path instead
        // of from two real toolchains disagreeing.
        let other = d.join("other/bin/rustc");
        exec_file(&other);
        let rustup = d.join("rustup");
        write_fake_rustup(&rustup, &other, &toolchain);
        let mut env = base_env();
        env.search_path = format!("{}:{}", rbin.display(), rustup.display());
        let err = provision_rust(&env).unwrap_err();
        assert!(
            matches!(&err, ProvisionErr::Broken(m) if m.contains(MUSL_TARGET)),
            "unexpected error: {err}"
        );
        assert!(!libc.is_file(), "the musl std was added to the wrong toolchain");
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
        assert_eq!(
            stage0_memo_hit(&meta, "fp1", &store, &db),
            Ok(cb.to_string())
        );
        // A CHANGED builder-source fingerprint must rebuild — and SAY so. A miss
        // costs a compile, which in the loop sandbox is impossible, so the reason
        // is what tells the reader this was reuse and not a missing toolchain.
        let moved = stage0_memo_hit(&meta, "fp2", &store, &db).unwrap_err();
        assert!(moved.contains("fingerprint moved"), "{moved}");
        // A memo whose placement bytes are gone must rebuild, not be trusted.
        std::fs::remove_dir_all(store.join("abc123-td-builder-0.1.0")).unwrap();
        let gone = stage0_memo_hit(&meta, "fp1", &store, &db).unwrap_err();
        assert!(gone.contains("is missing"), "{gone}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The evaluator's fingerprint must follow `include_str!` OUT of recipes/:
    /// the recipe files embed target-crate sources (td-init, td-util, …) that
    /// live elsewhere in the tree, and a root set that stopped at recipes/ would
    /// reuse an evaluator emitting yesterday's source as this build's recipe.
    #[test]
    fn embedded_sources_outside_the_crate_join_the_fingerprint() {
        let d = scratch("embed");
        let crate_src = d.join("recipes/src/recipes");
        let outside = d.join("td-util/src");
        std::fs::create_dir_all(&crate_src).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("free.rs"), "// embedded\n").unwrap();
        std::fs::write(crate_src.join("local.h"), "// local\n").unwrap();
        let recipe = crate_src.join("td-util.rs");
        std::fs::write(
            &recipe,
            "const A: &str = include_str!(\"../../../td-util/src/free.rs\");\n\
             const B: &str = include_str!(\"local.h\");\n\
             const C: &str = include_str!(\"../../../td-util/src/gone.rs\");\n",
        )
        .unwrap();

        let found = embedded_include_paths(&[recipe.clone()]).unwrap();
        let has = |needle: &str| found.iter().any(|p| p.ends_with(needle));
        assert!(has("td-util/src/free.rs"), "must follow the crate: {found:?}");
        assert!(has("local.h"), "and keep in-tree includes: {found:?}");
        // A literal naming nothing on disk invents no root (a commented-out or
        // stale include must not make the fingerprint unresolvable).
        assert!(!has("gone.rs"), "absent target must be dropped: {found:?}");

        // A COMPOSED path is a compile input the scan cannot resolve. Dropping it
        // silently is the stale-evaluator hazard itself, so it must fail closed.
        std::fs::write(
            &recipe,
            "const D: &str = include_str!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
        )
        .unwrap();
        let err = embedded_include_paths(&[recipe]).unwrap_err();
        assert!(err.contains("composed path"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The real invariant, against the real tree: every literal include under the
    /// scanned crates resolves. A synthetic fixture cannot catch a form the tree
    /// grows later — this reds the first time one appears.
    #[test]
    fn every_literal_include_in_the_tree_resolves() {
        let Ok(cwd) = std::env::current_dir() else {
            return;
        };
        // builder/ is the cargo cwd under `cargo test`; the repo root is its parent.
        let root = cwd.parent().unwrap_or(&cwd).to_path_buf();
        if !root.join("recipes/src").is_dir() {
            return; // not a repo checkout (packaged build) — nothing to assert
        }
        let roots = recipe_eval_fp_roots(&root).expect("composed include, or unreadable tree");
        for crate_dir in ["td-init", "td-login", "td-util", "td-sh"] {
            assert!(
                roots.iter().any(|r| r.contains(crate_dir)),
                "{crate_dir} is embedded by a recipe but absent from the fingerprint roots"
            );
        }
    }

    /// A tolerated skip must stay EVIDENCE, not inference. `recipe-eval-place`
    /// re-emits the sentinel for whatever it tags, so tagging on the exit code
    /// alone would let any other 69 in the tool script mint a skip and hide a
    /// regression behind it — the exact failure the two-part test exists to stop.
    #[test]
    fn only_a_69_that_carries_the_sentinel_is_tagged_unprovisioned() {
        use std::os::unix::process::ExitStatusExt;
        let sentinel = crate::check_loop::UNPROVISIONED_SENTINEL;
        let tag = crate::check_loop::UNPROVISIONED_TAG;
        // ExitStatus::from_raw takes a wait(2) status word: code << 8.
        let out = |code: i32, stdout: &str, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let m = || "boom".to_string();

        let tagged = tag_child_failure(&out(69, "", &format!("nope\n{sentinel}\n")), m());
        assert_eq!(tagged, format!("{tag}boom"), "69 + sentinel on stderr is the skip");
        let tagged = tag_child_failure(&out(69, sentinel, ""), m());
        assert_eq!(tagged, format!("{tag}boom"), "the sentinel counts on stdout too");

        assert_eq!(
            tag_child_failure(&out(69, "", "cargo: error: linker not found\n"), m()),
            "boom",
            "a 69 with no sentinel is SOME OTHER failure and must red, not skip"
        );
        assert_eq!(
            tag_child_failure(&out(1, "", &format!("{sentinel}\n")), m()),
            "boom",
            "the sentinel alone does not make a skip; the code must agree"
        );
    }

    /// The evaluator memo is the reason a warm tree needs no toolchain, so it
    /// must reuse ONLY an intact placement of the CURRENT recipes source — and
    /// name the reason when it will not, for the same reason stage0 does.
    #[test]
    fn recipe_eval_memo_reuses_only_a_matching_intact_build() {
        let d = scratch("re-memo");
        let base = d.join("re");
        let meta = base.join(".recipe-eval-meta");
        let bin = base.join("target/x86_64-unknown-linux-musl/release/td-recipe-eval");
        exec_file(&bin);
        let bin_s = bin.to_string_lossy().into_owned();
        let digest = crate::sha256::sha256_file(&bin).unwrap();
        std::fs::write(&meta, format!("fp1\n{bin_s}\n{digest}\n")).unwrap();
        assert_eq!(recipe_eval_memo_hit(&meta, "fp1"), Ok(bin_s.clone()));
        // The memo names a REWRITABLE path, so the contents are what it vouches
        // for: bytes that changed under a matching fingerprint are not the ones
        // memoized (a build that landed here and then failed assert-static).
        write_exec(&bin, "different bytes");
        let swapped = recipe_eval_memo_hit(&meta, "fp1").unwrap_err();
        assert!(swapped.contains("not the one memoized"), "{swapped}");
        std::fs::write(&meta, format!("fp1\n{bin_s}\n{}\n", crate::sha256::sha256_file(&bin).unwrap())).unwrap();
        // An edited recipes tree must rebuild rather than evaluate with the old
        // binary — a stale evaluator would emit yesterday's recipes.
        let moved = recipe_eval_memo_hit(&meta, "fp2").unwrap_err();
        assert!(moved.contains("fingerprint moved"), "{moved}");
        // A memo naming a binary that is gone must not be trusted.
        std::fs::remove_file(&bin).unwrap();
        let gone = recipe_eval_memo_hit(&meta, "fp1").unwrap_err();
        assert!(gone.contains("is missing"), "{gone}");
        // No memo at all is a miss, not a panic.
        let absent = recipe_eval_memo_hit(&base.join("nope"), "fp1").unwrap_err();
        assert!(absent.contains("no memo"), "{absent}");
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
