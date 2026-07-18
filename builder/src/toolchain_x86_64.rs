//! Structured Rust port of the x86_64 SELF-HOST + rust toolchain builds. Rung X2 (the
//! NATIVE binutils 2.44 + gcc 14.3.0) has since become the `binutils-x86-64-native` →
//! `gcc-x86-64-native` recipe graph (recipes/src/recipes/, driven by td-recipe-eval
//! check-run) — the `toolchain-recipe x86_64-native` path is retired. What
//! remains here: `td-builder toolchain-recipe x86_64-self` rebuilds the SAME binutils +
//! gcc with the NATIVE /td/store toolchain as the builder (rung X3 — self-hosting,
//! gcc-rebuilds-gcc: the compiler that compiles the compiler is itself a td-built ELF64
//! x86_64 binary; its `[builder-arch]` leg REJECTS an i686 builder, so rung X2's cross
//! gcc cannot silently stand in). This X3 path still lives as imperative shell-driven
//! Rust pending its own recipe conversion — the same "port the shell driver → structured
//! recipe" move #229 made for the seed/mes rungs, extended to the x86_64 track.
//!
//! Neither flavor is byte-reproducible (trust = the input-addressed lock name +
//! the ed25519 substitute signature, see `tests/td-toolchain-x86_64-native.lock`), so
//! this is deliberately NOT a `bootstrap::Recipe` (whose leg skeleton double-builds and
//! asserts byte-identity). It is the build half only; the recipe check owns the
//! output assertions and own-root behavioral verify. (The X3 check adds a `[codegen]` agreement leg
//! in shell: the INPUT native gcc — now built by the `gcc-x86-64-native` recipe (rung
//! X2) — and the self-rebuilt gcc (this module's `build_gcc_x86_64`, rung X3) must emit
//! byte-identical `-O2 -S` assembly. That fixpoint SILENTLY DEPENDS on the two builds
//! sharing every code-gen-affecting configure flag: `gcc-x86-64-native.rs`'s configure
//! line and `build_gcc_x86_64` below must stay in lockstep (they differ only in the
//! `--prefix` suffix and the `--with-as`/`--with-ld` paths, which don't affect cc1's
//! code gen). Change one, change the other, or the X3 `[codegen]` leg reds.)
//!
//! Inputs (the builder toolchain + pinned sources) are passed by the caller — the gate
//! has them as shell vars (fetched from the substitute closure or built from seed). The
//! port mirrors the shell's every configure flag, env var and wrapper; the divergences
//! that are load-bearing carry the shell's own comments.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Which toolchain drives the build, and how the outputs are named. Only `SelfHost`
/// (rung X3, built BY td's own native gcc into `…-x86_64-self`) remains imperative
/// here — rung X2 (`Native`) became the `gcc-x86-64-native` recipe. The single-variant
/// enum is kept so the shared build helpers stay flavor-symmetric; it collapses when
/// rung X3 is likewise converted to a recipe.
#[derive(Clone, Copy)]
enum Flavor {
    SelfHost,
}

impl Flavor {
    /// The /td/store name suffix (`binutils-2.44-x86_64-<sfx>`, `gcc-14.3.0-x86_64-<sfx>`).
    fn suffix(self) -> &'static str {
        match self {
            Flavor::SelfHost => "self",
        }
    }
    /// The env-var prefix the gate passes inputs under.
    fn env_prefix(self) -> &'static str {
        match self {
            Flavor::SelfHost => "TDXS",
        }
    }
}

/// Everything the self rung needs. The gate populates it (from the fetched-or-built
/// native builder closure + the warmed pinned sources) via `TDXS_*` env vars.
pub struct BuildInputs {
    /// Scaffolding PATH tail (coreutils/bash/… — the host-PATH build tools).
    pub cpath: String,
    /// The DRIVING C compiler, full path: td's own native plain `gcc` (SelfHost; ELF64).
    pub builder_cc: PathBuf,
    /// The driving C++ compiler (`g++`), same tree as `builder_cc`.
    pub builder_cxx: PathBuf,
    /// The driving toolchain's binutils `bin/` dir, prepended to the build PATH — the
    /// native plain `as`/`ld` (+ `readelf`, which the SelfHost `[builder-arch]` leg uses).
    pub builder_tools: PathBuf,
    /// The x86_64 glibc 2.41 tree (`XGLIBC`; `libc.so.6` + `ld-linux-x86-64.so.2`).
    pub glibc: PathBuf,
    /// binutils-2.44.tar.xz.
    pub binutils_tar: PathBuf,
    /// gcc-14.3.0.tar.xz.
    pub gcc_tar: PathBuf,
    /// gmp-6.3.0.tar.xz.
    pub gmp_tar: PathBuf,
    /// mpfr-4.2.1.tar.xz.
    pub mpfr_tar: PathBuf,
    /// mpc-1.3.1.tar.gz.
    pub mpc_tar: PathBuf,
    /// The x86_64 kernel UAPI headers tarball (.tar.gz).
    pub kernel_headers_tar: PathBuf,
    /// Where to build (the caller's scratch). Outputs land under here.
    pub out: PathBuf,
    flavor: Flavor,
}

impl BuildInputs {
    /// Read the inputs from the flavor's env vars (how the gate passes them):
    /// `TDXS_BUILDER_GCC`/`TDXS_BUILDER_BINUTILS` name the NATIVE trees (plain-named
    /// drivers — the self build's compiler is td's own native gcc).
    fn from_env(flavor: Flavor) -> Result<BuildInputs, String> {
        let p = flavor.env_prefix();
        let g = |k: String| -> Result<String, String> {
            std::env::var(&k).map_err(|_| format!("env {k} unset"))
        };
        let (builder_cc, builder_cxx, builder_tools) = match flavor {
            Flavor::SelfHost => {
                let gcc = PathBuf::from(g(format!("{p}_BUILDER_GCC"))?);
                let bu = PathBuf::from(g(format!("{p}_BUILDER_BINUTILS"))?);
                (gcc.join("bin/gcc"), gcc.join("bin/g++"), bu.join("bin"))
            }
        };
        Ok(BuildInputs {
            cpath: std::env::var(format!("{p}_CPATH")).unwrap_or_default(),
            builder_cc,
            builder_cxx,
            builder_tools,
            glibc: PathBuf::from(g(format!("{p}_GLIBC"))?),
            binutils_tar: PathBuf::from(g(format!("{p}_BINUTILS_TAR"))?),
            gcc_tar: PathBuf::from(g(format!("{p}_GCC_TAR"))?),
            gmp_tar: PathBuf::from(g(format!("{p}_GMP_TAR"))?),
            mpfr_tar: PathBuf::from(g(format!("{p}_MPFR_TAR"))?),
            mpc_tar: PathBuf::from(g(format!("{p}_MPC_TAR"))?),
            kernel_headers_tar: PathBuf::from(g(format!("{p}_KERNEL_HEADERS_TAR"))?),
            out: PathBuf::from(g(format!("{p}_OUT"))?),
            flavor,
        })
    }
}

// --- CLI -------------------------------------------------------------------------

const USAGE: &str =
    "usage: td-builder toolchain-recipe {x86_64-self}  (inputs via TDXS_* env)";

/// `td-builder toolchain-recipe <name>`. The downloaded Rust transform is a
/// first-class, stage0-only recipe (`rust-stage0`); it is not a CLI subcommand.
pub fn cli(args: &[String]) -> ExitCode {
    match args.get(2).map(String::as_str) {
        Some("x86_64-self") => {
            let result = BuildInputs::from_env(Flavor::SelfHost).and_then(|inp| run_self(&inp));
            finish("x86_64-self", result)
        }
        Some("--list") | Some("list") => {
            println!("x86_64-self");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn finish(name: &str, result: Result<String, String>) -> ExitCode {
    match result {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("FAIL: toolchain-recipe {name}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Rung X3 (self-hosting, gcc-rebuilds-gcc): the NATIVE /td/store toolchain rebuilds
/// binutils 2.44 + gcc 14.3.0. Same binutils+gcc build as rung X2's `gcc-x86-64-native`
/// recipe except the DRIVER: the
/// `[builder-arch]` leg first asserts the gcc doing the building is ITSELF an ELF64
/// x86_64 binary — the discriminator vs rung X2, whose builder (the cross gcc) is an
/// i686 ELF32 binary. Pointing the builder at the cross gcc reds here (verified-red).
/// Prints `SELF_BINUTILS=`/`SELF_GCC=` lines the gate reads.
pub fn run_self(inp: &BuildInputs) -> Result<String, String> {
    let mut report = String::new();
    let readelf = inp.builder_tools.join("readelf");
    if !is_exec(&readelf) {
        return Err(format!(
            "[builder-arch] no readelf at {} — the builder binutils tree must ship plain-named tools",
            readelf.display()
        ));
    }
    let hdr = readelf_header(&readelf, &inp.builder_cc)?;
    if !header_is_elf64(&hdr) || !header_is_x86_64(&hdr) {
        return Err(format!(
            "[builder-arch] the builder gcc ({}) is NOT an ELF64 x86_64 binary — rung X3 \
             requires the NATIVE /td/store gcc as the builder (an i686 cross gcc is rung \
             X2's builder, not self-hosting)",
            inp.builder_cc.display()
        ));
    }
    report.push_str(
        "   [builder-arch] the builder gcc IS an ELF64 x86_64 binary — the compiler compiling the compiler is td's own native /td/store gcc\n",
    );
    let sbu = build_binutils_x86_64(inp)?;
    report.push_str(&format!(
        "   [build] SELF-HOSTED x86_64 binutils 2.44 built by the native toolchain (ELF64) at {}\n",
        sbu.display()
    ));
    let sgcc = build_gcc_x86_64(inp, &sbu)?;
    report.push_str(&format!(
        "   [build] SELF-HOSTED x86_64 gcc 14.3.0 (c,c++) built by the native gcc (ELF64 x86-64) at {}\n",
        sgcc.display()
    ));
    // Machine-readable lines for the gate.
    report.push_str(&format!("SELF_BINUTILS={}\n", sbu.display()));
    report.push_str(&format!("SELF_GCC={}\n", sgcc.display()));
    Ok(report)
}

// --- rust-stage0: exact upstream bootstrap snapshot retarget transform -----------
//
// The engine phase runner for buildSystem "rust-stage0". It unpacks the exact
// rustc/rust-std/Cargo component tarballs named by Rust source's src/stage0, co-locates
// the /td/store runtime closure (glibc sonames + libgcc_s + libz, found via the UNCHANGED
// upstream RUNPATH $ORIGIN/../lib — the portable, content-addressed-store-safe choice) and
// RELINKS rustc/cargo's ELF interpreter — `crate::elf::set_interp`, GROWS the slot per
// #258, NOT patchelf — onto the /td/store x86_64 glibc loader.
//
// It runs in the HERMETIC drv sandbox entirely as td-builder Rust: engine-native
// archive readers, filesystem copies, and crate::elf; no unpacker subprocess runs.
//
//   TD_SRC        the exact upstream rustc component tarball (Class::Source).
//   TD_INPUT_MAP  lock name -> /td/store path; the transform reads its inputs BY RECIPE NAME
//                 and also resolves rust-std/Cargo source component tarballs:
//                   rust-stage0-std-source
//                   rust-stage0-cargo-source
//                   glibc-x86-64       the x86_64 glibc 2.41 tree (interp + sonames), at its
//                                      staged stage/td/store/glibc-2.41-x86_64 prefix
//                   gcc-x86-64-stage2  the cross gcc final — libgcc_s.so.1 (found recursively)
//                   zlib-x86-64        the td-built libz.so.1 tree
//   out           the stage0-only rustc/rustdoc/Cargo/sysroot tree.
//
// The interp target is <glibc-x86-64 staged>/lib/ld-linux-x86-64.so.2 — the input's own
// /td/store path. Same pinned source + same inputs => byte-identical tree (the `td-builder
// check` double-build oracle proves it); a missing input reds at drv-assembly
// (build-recipe), before a build.

/// The staged install-prefix subpaths of the x86_64 toolchain rung outputs (a mesboot rung
/// installs into <out>/stage/td/store/<prefix>). Kept beside the transform so the resolution
/// stays in one place if a rung's version bumps.
const GLIBC_X86_64_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";
const GCC_X86_64_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64";
const ZLIB_X86_64_LIB: &str = "stage/td/store/zlib-1.3.1/lib";

/// The resolved stage0 transform inputs, read from the drv env.
struct RustStage0Inputs {
    /// Individually unpacked upstream component trees.
    rustc_src: PathBuf,
    cargo_src: PathBuf,
    std_src: PathBuf,
    /// glibc-x86-64 (staged) — the x86_64 glibc 2.41 tree (interp target + co-located sonames).
    glibc: PathBuf,
    /// The dir under gcc-x86-64-stage2 holding `libgcc_s.so.1` (found recursively).
    libgcc_dir: PathBuf,
    /// zlib-x86-64 (staged lib) — holds `libz.so.1.*`.
    libz_dir: PathBuf,
    /// `<glibc>/lib/ld-linux-x86-64.so.2` — the /td/store loader rustc/cargo relink onto.
    glibc_interp: String,
    /// `$out` — the transform's output tree.
    out: PathBuf,
}

impl RustStage0Inputs {
    fn from_drv_env() -> Result<RustStage0Inputs, String> {
        let g = |k: &str| -> Result<String, String> {
            std::env::var(k).map_err(|_| format!("env {k} unset"))
        };
        let map = crate::json::parse(&g("TD_INPUT_MAP")?)
            .map_err(|e| format!("TD_INPUT_MAP JSON: {e}"))?;
        let pick = |name: &str| -> Result<PathBuf, String> {
            map.get(name)
                .and_then(crate::json::Json::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| format!("rust-stage0: lock is missing the `{name}' input (needed by the transform)"))
        };
        // #410: inputs resolved BY RECIPE NAME to their staged install prefixes — the
        // recipe-graph model (glibc/libgcc/libz are the rungs build-plan --auto chained).
        let glibc = pick("glibc-x86-64")?.join(GLIBC_X86_64_STAGE);
        let gcc = pick("gcc-x86-64-stage2")?.join(GCC_X86_64_STAGE);
        let libgcc = find_file(&gcc, "libgcc_s.so.1").ok_or_else(|| {
            format!("rust-stage0: no libgcc_s.so.1 under the gcc-x86-64-stage2 input {}", gcc.display())
        })?;
        let libgcc_dir = libgcc
            .parent()
            .ok_or("rust-stage0: libgcc_s.so.1 has no parent dir")?
            .to_path_buf();
        let libz_dir = pick("zlib-x86-64")?.join(ZLIB_X86_64_LIB);
        let glibc_interp = format!("{}/lib/ld-linux-x86-64.so.2", glibc.display());
        // Unpack all three exact snapshot components with the engine-native tar reader.
        let rustc_tar = PathBuf::from(g("TD_SRC")?);
        let std_tar = pick("rust-stage0-std-source")?;
        let cargo_tar = pick("rust-stage0-cargo-source")?;
        let rustc_src = mktemp_dir("td-rust-stage0-rustc")?;
        let std_src = mktemp_dir("td-rust-stage0-std")?;
        let cargo_src = mktemp_dir("td-rust-stage0-cargo")?;
        unpack_rust_component(&rustc_tar, &rustc_src, "rustc/bin/rustc")?;
        unpack_rust_component(
            &std_tar,
            &std_src,
            "rust-std-x86_64-unknown-linux-gnu/lib/rustlib",
        )?;
        unpack_rust_component(&cargo_tar, &cargo_src, "cargo/bin/cargo")?;
        Ok(RustStage0Inputs {
            rustc_src,
            cargo_src,
            std_src,
            glibc,
            libgcc_dir,
            libz_dir,
            glibc_interp,
            out: PathBuf::from(g("out")?),
        })
    }
}

/// Unpack the pinned Rust release `.tar.gz` into `dest` (top-level dir stripped) with the
/// ENGINE's own gzip/tar readers (`crate::tar::unpack_archive`) — no subprocess, no
/// unpacker inputs at all: tar/gzip left the host-tool tier with `Step::Unpack`, and the
/// transform admits no host tool (re #469). The combined installer carries components
/// `assemble_rust_tree` never reads (docs, rustfmt, clippy, …); the full unpack writes
/// them to the build scratch only — the assemble step copies rustc/, cargo/, and
/// rust-std-*/ into `$out`, so the extra members cost scratch bytes, never output bytes.
/// NOTE at the next rust-pin bump: the engine tar reader caps a single entry at
/// `tar::MAX_TAR_ENTRY_BYTES` (256 MiB) — a limit host `tar` never imposed. Today's
/// members are well under it; a future release with a >256 MiB member (a fat
/// libLLVM/debuginfo blob) would red here, and the cap would need raising in lockstep.
fn unpack_rust_component(
    tarball: &Path,
    dest: &Path,
    required: &str,
) -> Result<(), String> {
    crate::tar::unpack_archive(tarball, dest, false)?;
    if !dest.join(required).exists() {
        return Err(format!(
            "unpack {}: extracted component is missing {required} (release layout drift?)",
            tarball.display()
        ));
    }
    Ok(())
}

/// Assemble and retarget the exact downloaded stage0 snapshot. No compilation.
pub fn run_rust_stage0_build() -> Result<(), String> {
    let inp = RustStage0Inputs::from_drv_env()?;
    assemble_rust_tree(&inp)?;
    relink_rust_interp(&inp.out, &inp.glibc_interp)?;
    Ok(())
}

/// Copy rustc/cargo + rustlib from the extracted release tree (TD_SRC), provenance-check,
/// and co-locate the /td/store runtime closure into `<out>/lib`.
fn assemble_rust_tree(inp: &RustStage0Inputs) -> Result<(), String> {
    let tree = &inp.out;
    fs::create_dir_all(tree.join("bin")).map_err(ioerr("mkdir out/bin"))?;
    fs::create_dir_all(tree.join("lib")).map_err(ioerr("mkdir out/lib"))?;
    for bin in ["rustc", "rustdoc"] {
        fs::copy(
            inp.rustc_src.join("rustc/bin").join(bin),
            tree.join("bin").join(bin),
        )
        .map_err(ioerr("cp rustc component binary"))?;
    }
    fs::copy(inp.cargo_src.join("cargo/bin/cargo"), tree.join("bin/cargo"))
        .map_err(ioerr("cp cargo"))?;
    // librustc_driver, libLLVM, libstd*.so, AND rustc's own rustlib/.
    copy_tree_contents(&inp.rustc_src.join("rustc/lib"), &tree.join("lib"))
        .map_err(ioerr("cp rustc/lib"))?;
    let std_rustlib = inp
        .std_src
        .join("rust-std-x86_64-unknown-linux-gnu/lib/rustlib");
    if std_rustlib.is_dir() {
        copy_tree_contents(&std_rustlib, &tree.join("lib/rustlib")).map_err(ioerr("merge rustlib"))?;
    }
    make_writable(tree).map_err(ioerr("chmod tree"))?;

    // the sysroot MUST hold a libstd rlib — else rustc has nothing to link a program to.
    let rlib_dir = tree.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
    if !glob_exists(&rlib_dir, "libstd-", ".rlib") {
        return Err("assembled rust sysroot has no libstd rlib (rustlib missing) — rustc could not link a program".into());
    }

    // [provenance] the upstream binaries + librustc_driver carry NO /gnu/store bytes.
    let mut provenance = vec![
        tree.join("bin/rustc"),
        tree.join("bin/rustdoc"),
        tree.join("bin/cargo"),
    ];
    if let Some(drv) = glob_first_in(&tree.join("lib"), "librustc_driver-", ".so") {
        provenance.push(drv);
    }
    for b in &provenance {
        if contains_gnu_store(b)? {
            return Err(format!("{} contains /gnu/store bytes — not guix-free upstream", b.display()));
        }
    }

    // co-locate the full external runtime closure in lib/ (found via RUNPATH $ORIGIN/../lib):
    // glibc sonames + libgcc_s (+ the bare .so link the rust link's -lgcc_s resolves) + libz.
    for soname in ["libc.so.6", "libdl.so.2", "librt.so.1", "libpthread.so.0", "libm.so.6"] {
        let src = inp.glibc.join("lib").join(soname);
        if !src.exists() {
            return Err(format!("x86_64 glibc 2.41 is missing {soname}"));
        }
        copy_deref(&src, &tree.join("lib").join(soname))?;
    }
    copy_deref(&inp.libgcc_dir.join("libgcc_s.so.1"), &tree.join("lib/libgcc_s.so.1"))?;
    symlink_force("libgcc_s.so.1", &tree.join("lib/libgcc_s.so"))?;
    let libz = glob_first_in(&inp.libz_dir, "libz.so.1", "").ok_or_else(|| {
        format!("rust-stage0: no libz.so.1* under the zlib-x86-64 input {}", inp.libz_dir.display())
    })?;
    copy_deref(&libz, &tree.join("lib/libz.so.1"))?;
    make_writable(tree).map_err(ioerr("chmod tree"))?;
    Ok(())
}

/// Relink stage0 rustc, rustdoc, and Cargo's ELF interpreter to `glibc_interp`,
/// GROWS the slot when the new path is longer per #258 — no patchelf), asserting each
/// landed under /td/store.
fn relink_rust_interp(tree: &Path, glibc_interp: &str) -> Result<(), String> {
    for b in ["rustc", "rustdoc", "cargo"] {
        let bin = tree.join("bin").join(b);
        crate::elf::set_interp(&bin, glibc_interp)?;
        let got = crate::elf::read_interp(&bin)?.unwrap_or_default();
        let ok = got.starts_with("/td/store/") && got.ends_with("/lib/ld-linux-x86-64.so.2");
        if !ok {
            return Err(format!("interp of {b} not relinked to the /td/store glibc loader (got: {got})"));
        }
    }
    Ok(())
}

// --- x86_64 binutils 2.44 (native + self flavors) ----------------------------------

/// Port of `build_binutils_x86_64_native`. GNU Binutils 2.44
/// (`--build=--host=--target=x86_64-pc-linux-gnu`), built STATIC by the SelfHost
/// flavor's builder gcc (td's own native gcc) vs the /td/store x86_64 glibc 2.41
/// static archives. Returns the install prefix (`<out>/binutils`) — plain-named
/// ELF64 `as`/`ld`/`ar`/… (shared with the gcc-x86-64-native recipe, which ports
/// the same build for rung X2.)
fn build_binutils_x86_64(inp: &BuildInputs) -> Result<PathBuf, String> {
    let out = inp.out.join("binutils");
    reset_dir(&out)?;
    let xz = which("xz").ok_or("no xz on PATH")?;
    let csh = shell();

    // x86_64 kernel UAPI headers beside the glibc headers (glibc headers #include <linux/…>).
    let khd = mktemp_dir("td-xn-kh")?;
    untar(&xz, &inp.kernel_headers_tar, &khd, 0, TarComp::Gz)?;
    let cip = format!("{}:{}", inp.glibc.join("include").display(), khd.display());

    // -shared-aware static wrapper (handles binutils' ld libdep.la shared module).
    let wb = mktemp_dir("td-xn-wb")?;
    let cc = wb.join("cc");
    mk_native_static_wrapper(&inp.builder_cc, &inp.glibc, &cc, None)?;

    let tb = mktemp_dir("td-xn-tb")?;
    xbin(&tb)?;

    let src = mktemp_dir("td-xn-binutils")?;
    untar(&xz, &inp.binutils_tar, &src, 1, TarComp::Xz)?;

    let bp = format!("{}:{}:{}", inp.builder_tools.display(), tb.display(), inp.cpath);

    // configure
    let mut cfg = Command::new(&csh);
    cfg.arg("./configure")
        .arg("--build=x86_64-pc-linux-gnu")
        .arg("--host=x86_64-pc-linux-gnu")
        .arg("--target=x86_64-pc-linux-gnu")
        .arg(format!("--prefix=/td/store/binutils-2.44-x86_64-{}", inp.flavor.suffix()))
        .arg("--disable-nls")
        .arg("--disable-gold")
        .arg("--disable-werror")
        .arg("--enable-deterministic-archives")
        .arg("--disable-plugins")
        .arg("--disable-gprofng")
        .arg("--disable-multilib")
        .current_dir(&src)
        .env("PATH", &bp)
        .env("CONFIG_SHELL", &csh)
        .env("SHELL", &csh)
        .env("CC", &cc)
        .env("CC_FOR_BUILD", &cc)
        .env("C_INCLUDE_PATH", &cip);
    run(cfg, "native x86_64 binutils configure")?;

    // make
    let mut mk = Command::new("make");
    mk.arg(make_j())
        .arg("MAKEINFO=true")
        .current_dir(&src)
        .env("PATH", &bp)
        .env("CONFIG_SHELL", &csh)
        .env("SHELL", &csh)
        .env("C_INCLUDE_PATH", &cip);
    clear_makeflags(&mut mk);
    run(mk, "native x86_64 binutils make")?;

    // install prefix=out
    let mut inst = Command::new("make");
    inst.arg("MAKEINFO=true")
        .arg("install")
        .arg(format!("prefix={}", out.display()))
        .current_dir(&src)
        .env("PATH", &bp)
        .env("CONFIG_SHELL", &csh)
        .env("SHELL", &csh);
    clear_makeflags(&mut inst);
    run(inst, "native x86_64 binutils install")?;

    for t in ["as", "ld", "readelf"] {
        if !is_exec(&out.join("bin").join(t)) {
            return Err(format!("no native {t} produced"));
        }
    }
    // native 'as' must itself be ELF64.
    if !readelf_is_elf64(&out.join("bin").join("readelf"), &out.join("bin").join("as"))? {
        return Err("native binutils 'as' is not ELF64 x86_64".into());
    }
    Ok(out)
}

// --- x86_64 gcc 14.3.0 (native + self flavors) --------------------------------------

/// Port of `build_gcc_x86_64_native`. GCC 14.3.0 (c,c++;
/// `--build=--host=--target=x86_64-pc-linux-gnu`), built STATIC by the SelfHost flavor's
/// builder gcc (td's own native gcc — the gcc-rebuilds-gcc step) vs the /td/store x86_64
/// glibc 2.41, gmp/mpfr/mpc in-tree, as/ld = the freshly built sibling binutils. Returns
/// the staged prefix `<out>/gcc/stage/td/store/gcc-14.3.0-x86_64-<flavor-suffix>`.
fn build_gcc_x86_64(inp: &BuildInputs, fresh_binutils: &Path) -> Result<PathBuf, String> {
    let out = inp.out.join("gcc");
    reset_dir(&out)?;
    let xz = which("xz").ok_or("no xz on PATH")?;
    let csh = shell();

    // gcc + gmp/mpfr/mpc unpacked in-tree (gmp/mpfr/mpc NOT strip-components).
    untar(&xz, &inp.gcc_tar, &out, 1, TarComp::Xz)?;
    untar(&xz, &inp.gmp_tar, &out, 0, TarComp::Xz)?;
    untar(&xz, &inp.mpfr_tar, &out, 0, TarComp::Xz)?;
    untar(&xz, &inp.mpc_tar, &out, 0, TarComp::Gz)?;
    symlink_force("gmp-6.3.0", &out.join("gmp"))?;
    symlink_force("mpfr-4.2.1", &out.join("mpfr"))?;
    symlink_force("mpc-1.3.1", &out.join("mpc"))?;

    // combined build sysroot: include = glibc headers + kernel UAPI; lib = glibc libs + crt.
    let sysroot = out.join("sysroot");
    fs::create_dir_all(sysroot.join("include")).map_err(ioerr("mkdir sysroot/include"))?;
    fs::create_dir_all(sysroot.join("lib")).map_err(ioerr("mkdir sysroot/lib"))?;
    copy_tree_contents(&inp.glibc.join("include"), &sysroot.join("include"))
        .map_err(ioerr("stage glibc headers into the sysroot"))?;
    untar(&xz, &inp.kernel_headers_tar, &sysroot.join("include"), 0, TarComp::Gz)?;
    copy_tree_contents(&inp.glibc.join("lib"), &sysroot.join("lib"))
        .map_err(ioerr("stage glibc libs into the sysroot"))?;
    // Relocate glibc's GNU ld scripts (libc.so, libm.so AND libm.a — the cross build
    // only relocated *.so) to BARE names: a fully-static host link pulls libm.a whose
    // GROUP script else points at the absolute configure prefix.
    relocate_ld_scripts(&sysroot.join("lib"))?;

    // -shared-aware static wrappers; -B at the RELOCATED sysroot/lib; headers via -idirafter.
    let wb = out.join("wb");
    fs::create_dir_all(&wb).map_err(ioerr("mkdir wb"))?;
    let inc = sysroot.join("include");
    mk_native_static_wrapper(&inp.builder_cc, &sysroot, &wb.join("gcc"), Some(&inc))?;
    mk_native_static_wrapper(&inp.builder_cxx, &sysroot, &wb.join("g++"), Some(&inc))?;

    let tb = mktemp_dir("td-xn-tb")?;
    xbin(&tb)?;

    // glibc + kernel headers via the wrapper's -idirafter (NOT C_INCLUDE_PATH — that
    // breaks the libstdc++ <cstdlib> #include_next); CIP carries only the in-tree mpfr src.
    let cip = out.join("mpfr").join("src");
    let lp = sysroot.join("lib");

    // rewrite `#!/bin/sh` shebangs in the source tree to the curated shell.
    rewrite_binsh_shebangs(&out, &csh)?;

    let bld = out.join("bld");
    reset_dir(&bld)?;

    let bp = format!(
        "{}:{}:{}:{}",
        fresh_binutils.join("bin").display(),
        inp.builder_tools.display(),
        tb.display(),
        inp.cpath
    );
    let wgcc = wb.join("gcc");
    let wgpp = wb.join("g++");
    let cpp = format!("{} -E", wgcc.display());

    // configure
    let mut cfg = Command::new(&csh);
    cfg.arg("../configure")
        .arg(format!("--prefix=/td/store/gcc-14.3.0-x86_64-{}", inp.flavor.suffix()))
        .arg("--build=x86_64-pc-linux-gnu")
        .arg("--host=x86_64-pc-linux-gnu")
        .arg("--target=x86_64-pc-linux-gnu")
        .arg(format!("--with-as={}", fresh_binutils.join("bin/as").display()))
        .arg(format!("--with-ld={}", fresh_binutils.join("bin/ld").display()))
        .arg(format!("--with-build-sysroot={}", sysroot.display()))
        .arg("--with-native-system-header-dir=/include")
        .arg("--disable-bootstrap")
        .arg("--disable-multilib")
        .arg("--disable-shared")
        .arg("--enable-static")
        .arg("--enable-languages=c,c++")
        .arg("--enable-threads=single")
        .arg("--disable-libstdcxx-pch")
        .arg("--disable-libatomic")
        .arg("--disable-libgomp")
        .arg("--disable-libitm")
        .arg("--disable-libsanitizer")
        .arg("--disable-libssp")
        .arg("--disable-libvtv")
        .arg("--disable-libquadmath")
        .arg("--disable-lto")
        .arg("--disable-plugin")
        .arg("--disable-libcc1")
        .arg("--disable-decimal-float")
        .arg("--disable-werror")
        .current_dir(&bld)
        .env("PATH", &bp)
        .env("CONFIG_SHELL", &csh)
        .env("CC", &wgcc)
        .env("CXX", &wgpp)
        .env("CPP", &cpp)
        .env("CC_FOR_BUILD", &wgcc)
        .env("CXX_FOR_BUILD", &wgpp)
        .env("C_INCLUDE_PATH", &cip)
        .env("CPLUS_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp)
        .env("LDFLAGS", "-static");
    run(cfg, "native x86_64 gcc-14.3.0 configure")?;

    // make
    let mut mk = Command::new("make");
    mk.arg(make_j())
        .arg(format!("SHELL={csh}"))
        .arg(format!("CONFIG_SHELL={csh}"))
        .arg("MAKEINFO=true")
        .arg("LDFLAGS=-static")
        .arg("LDFLAGS_FOR_TARGET=-static")
        .current_dir(&bld)
        .env("PATH", &bp)
        .env("CONFIG_SHELL", &csh)
        .env("C_INCLUDE_PATH", &cip)
        .env("CPLUS_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp);
    clear_makeflags(&mut mk);
    run(mk, "native x86_64 gcc-14.3.0 make")?;

    // install DESTDIR=out/stage
    let mut inst = Command::new("make");
    inst.arg(format!("SHELL={csh}"))
        .arg("MAKEINFO=true")
        .arg("install")
        .arg(format!("DESTDIR={}", out.join("stage").display()))
        .current_dir(&bld)
        .env("PATH", &bp)
        .env("CONFIG_SHELL", &csh)
        .env("C_INCLUDE_PATH", &cip)
        .env("CPLUS_INCLUDE_PATH", &cip)
        .env("LIBRARY_PATH", &lp);
    clear_makeflags(&mut inst);
    run(inst, "native x86_64 gcc-14.3.0 install")?;

    let g = out.join(format!("stage/td/store/gcc-14.3.0-x86_64-{}", inp.flavor.suffix()));
    if !is_exec(&g.join("bin/gcc")) || !is_exec(&g.join("bin/g++")) {
        return Err("no native gcc/g++ produced".into());
    }
    if find_file(&g, "cc1").is_none() {
        return Err("native gcc produced no cc1".into());
    }
    let readelf = fresh_binutils.join("bin/readelf");
    if !readelf_is_elf64(&readelf, &g.join("bin/gcc"))?
        || !readelf_is_x86_64(&readelf, &g.join("bin/gcc"))?
    {
        return Err("native gcc is not ELF64 x86_64".into());
    }
    Ok(g)
}

// --- shared helpers (ports of the shell's _*) ------------------------------------

/// Port of `_mk_native_static_wrapper`: a single-token CC wrapper that adds `-static`
/// for executables/conftests but DROPS it when the link is `-shared` (libtool building
/// a shared module) — an x86_64-specific R_X86_64_32-vs-non-PIC-crt guard. Optional
/// header dir added with `-idirafter` (NOT -isystem: must come after gcc's own C++ dirs
/// so libstdc++'s `<cstdlib> #include_next <stdlib.h>` resolves).
fn mk_native_static_wrapper(cc: &Path, glibc: &Path, dst: &Path, hdr: Option<&Path>) -> Result<(), String> {
    let bsh = shell();
    let ida = match hdr {
        Some(h) => format!(" -idirafter {}", h.display()),
        None => String::new(),
    };
    let cc = cc.display();
    let gl = glibc.display();
    let body = format!(
        "#!{bsh}\n\
         for a in \"$@\"; do case \"$a\" in -shared) exec \"{cc}\"{ida} -B{gl}/lib \"$@\";; esac; done\n\
         exec \"{cc}\" -static{ida} -B{gl}/lib \"$@\"\n"
    );
    fs::write(dst, body).map_err(ioerr("write native static wrapper"))?;
    set_mode(dst, 0o555)
}

/// Port of `_xbin`: a bin/ of build-time scaffolding tools (resolved from the host
/// PATH — the host brings them, like the rust/cc seed; no host-store scavenging)
/// the autoconf/recursive-make builds need. Symlinks lex→flex, yacc→bison.
fn xbin(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(ioerr("mkdir xbin"))?;
    let tools = [
        "awk", "gawk", "sed", "grep", "make", "m4", "bison", "flex", "cmp", "diff", "msgfmt",
        "makeinfo", "python3", "gzip",
    ];
    for name in tools {
        if let Some(b) = which(name) {
            symlink_force_abs(&b, &dir.join(name))?;
        }
    }
    // lex→flex, yacc→bison (best-effort; only if present).
    if dir.join("flex").exists() {
        symlink_force("flex", &dir.join("lex"))?;
    }
    if dir.join("bison").exists() {
        symlink_force("bison", &dir.join("yacc"))?;
    }
    Ok(())
}

/// `command -v NAME` over `$PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if is_exec(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Relocate glibc GNU ld scripts (`*.so` and `*.a`) in `dir` to bare names — strip the
/// absolute configure prefix `/td/store/glibc-2.41-x86_64/lib/`.
fn relocate_ld_scripts(dir: &Path) -> Result<(), String> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(OsStr::to_str).unwrap_or("");
        if ext != "so" && ext != "a" {
            continue;
        }
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // head -c 80 | grep 'GNU ld script'
        let head_len = bytes.len().min(80);
        let head = bytes.get(..head_len).unwrap_or(&[]);
        if !contains_sub(head, b"GNU ld script") {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let fixed = text.replace("/td/store/glibc-2.41-x86_64/lib/", "");
        fs::write(&p, fixed.as_bytes()).map_err(ioerr("rewrite ld script"))?;
    }
    Ok(())
}

/// Rewrite `#!/bin/sh` (and `#! /bin/sh`) shebang lines under `root` to `#!<shell>`.
fn rewrite_binsh_shebangs(root: &Path, shell: &str) -> Result<(), String> {
    fn walk(dir: &Path, shell: &str) -> io::Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            let ft = e.file_type()?;
            let p = e.path();
            if ft.is_dir() {
                walk(&p, shell)?;
            } else if ft.is_file() {
                let bytes = match fs::read(&p) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // only touch files starting with `#!` and a first-line /bin/sh.
                let first_end = bytes.iter().position(|&b| b == b'\n').unwrap_or(bytes.len());
                let first = match bytes.get(..first_end) {
                    Some(f) => f,
                    None => continue,
                };
                if !first.starts_with(b"#!") {
                    continue;
                }
                let line = String::from_utf8_lossy(first);
                // after "#!", skip leading spaces, then the interpreter token (up to the
                // next whitespace); it must end in /bin/sh (matches `^#! */bin/sh`).
                let after_bang = line.get(2..).unwrap_or("");
                let ws_len = after_bang.len() - after_bang.trim_start().len();
                let interp = after_bang.trim_start().split(char::is_whitespace).next().unwrap_or("");
                if !interp.ends_with("/bin/sh") {
                    continue;
                }
                // PRESERVE the tail after the interpreter (args like " -e"), exactly as the
                // shell `1s,^#! *[^ ]*/bin/sh,#!$csh,` keeps everything past /bin/sh — dropping
                // it would silently change a `#!/bin/sh -e` script's error behavior.
                let interp_end = 2 + ws_len + interp.len();
                let tail = line.get(interp_end..).unwrap_or("");
                let rest = bytes.get(first_end..).unwrap_or(&[]);
                let mut new = format!("#!{shell}{tail}").into_bytes();
                new.extend_from_slice(rest);
                let _ = fs::write(&p, new);
            }
        }
        Ok(())
    }
    walk(root, shell).map_err(ioerr("rewrite shebangs"))
}

// --- tiny std-only utilities -----------------------------------------------------

fn make_j() -> String {
    std::env::var("X86_MAKE_J").unwrap_or_else(|_| "-j4".to_string())
}

/// The curated shell: `bash` if on PATH, else `sh`.
fn shell() -> String {
    which("bash")
        .or_else(|| which("sh"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn clear_makeflags(c: &mut Command) {
    for v in ["MAKEFLAGS", "MFLAGS", "GNUMAKEFLAGS", "MAKELEVEL"] {
        c.env(v, "");
    }
}

enum TarComp {
    Gz,
    Xz,
}

/// Unpack `tarball` into `dest`. `strip` = `--strip-components`. For `.tar.xz` the shell
/// pipes `xz -dc | tar -xf -` (the sandbox tar has no `-J`); for `.tar.gz` it uses `tar -xzf`.
fn untar(xz: &Path, tarball: &Path, dest: &Path, strip: u32, comp: TarComp) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(ioerr("mkdir untar dest"))?;
    let what = format!("unpack {}", tarball.display());
    match comp {
        TarComp::Gz => {
            let mut c = Command::new("tar");
            c.arg("-xzf").arg(tarball).arg("-C").arg(dest);
            if strip > 0 {
                c.arg(format!("--strip-components={strip}"));
            }
            run(c, &what)
        }
        TarComp::Xz => {
            // xz -dc TARBALL | tar -xf - -C dest [--strip-components=N]
            let mut dec = Command::new(xz)
                .arg("-dc")
                .arg(tarball)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("{what}: spawn xz: {e}"))?;
            let stdout = dec.stdout.take().ok_or_else(|| format!("{what}: xz produced no stdout"))?;
            let mut tar = Command::new("tar");
            tar.arg("-xf").arg("-").arg("-C").arg(dest);
            if strip > 0 {
                tar.arg(format!("--strip-components={strip}"));
            }
            let status = tar
                .stdin(Stdio::from(stdout))
                .status()
                .map_err(|e| format!("{what}: spawn tar: {e}"))?;
            // Reap the xz child + surface its exit: a corrupt/truncated .tar.xz makes xz
            // fail while tar can still exit 0 on the partial stream, so check BOTH.
            let dec_status = dec.wait().map_err(|e| format!("{what}: wait xz: {e}"))?;
            if !status.success() {
                return Err(format!("{what}: tar failed"));
            }
            if !dec_status.success() {
                return Err(format!("{what}: xz decompression failed"));
            }
            Ok(())
        }
    }
}

/// Run a build command; on failure include the tail of stderr.
fn run(mut c: Command, what: &str) -> Result<(), String> {
    let out = c.output().map_err(|e| format!("exec {what}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let start = lines.len().saturating_sub(30);
        let tail = lines.get(start..).unwrap_or(&[]).join("\n");
        return Err(format!("{what} failed:\n{tail}"));
    }
    Ok(())
}

fn reset_dir(p: &Path) -> Result<(), String> {
    if p.exists() {
        // make writable then remove (cp -a'd store trees keep 0555 dirs).
        let _ = make_writable(p);
        fs::remove_dir_all(p).map_err(ioerr("rm scratch dir"))?;
    }
    fs::create_dir_all(p).map_err(ioerr("mkdir scratch dir"))
}

fn mktemp_dir(prefix: &str) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{n}", std::process::id()));
    fs::create_dir_all(&dir).map_err(ioerr("mktemp dir"))?;
    Ok(dir)
}

fn set_mode(p: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(ioerr("chmod"))
}

fn is_exec(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0).unwrap_or(false)
}

fn make_writable(root: &Path) -> io::Result<()> {
    fn walk(p: &Path) -> io::Result<()> {
        let md = fs::symlink_metadata(p)?;
        if md.is_dir() {
            let mut perm = md.permissions();
            perm.set_mode(perm.mode() | 0o700);
            let _ = fs::set_permissions(p, perm);
            for e in fs::read_dir(p)? {
                walk(&e?.path())?;
            }
        }
        Ok(())
    }
    walk(root)
}

fn symlink_force(target: &str, link: &Path) -> Result<(), String> {
    if link.exists() || fs::symlink_metadata(link).is_ok() {
        let _ = fs::remove_file(link);
    }
    std::os::unix::fs::symlink(target, link).map_err(ioerr("symlink"))
}

fn symlink_force_abs(target: &Path, link: &Path) -> Result<(), String> {
    if fs::symlink_metadata(link).is_ok() {
        let _ = fs::remove_file(link);
    }
    std::os::unix::fs::symlink(target, link).map_err(ioerr("symlink abs"))
}

/// Copy the *contents* of `src` into `dst` (like `cp -a src/. dst/`), preserving
/// symlinks and permission bits.
fn copy_tree_contents(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        copy_entry(&e.path(), &dst.join(e.file_name()))?;
    }
    Ok(())
}

fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(from)?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(from)?;
        if fs::symlink_metadata(to).is_ok() {
            let _ = fs::remove_file(to);
        }
        std::os::unix::fs::symlink(target, to)?;
    } else if ft.is_dir() {
        fs::create_dir_all(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_entry(&e.path(), &to.join(e.file_name()))?;
        }
        let _ = fs::set_permissions(to, md.permissions());
    } else if ft.is_file() {
        // cp -f semantics: an existing dest may be read-only (rust ships 0444 files, and the
        // rust-std rustlib merge overlays rustc's rustlib), and fs::copy would then fail with
        // EACCES opening it for write — remove it first so the overlay always lands.
        if fs::symlink_metadata(to).is_ok() {
            let _ = fs::remove_file(to);
        }
        fs::copy(from, to)?;
    }
    Ok(())
}

/// Find the first file named `name` anywhere under `root`, DETERMINISTICALLY: at each
/// directory level the immediate files are checked in sorted order, then the subdirectories are
/// recursed in sorted order (a files-first depth-first search). An unreadable subdirectory is
/// SKIPPED, not fatal — so one bad dir cannot mask a match elsewhere in the tree (a `.ok()?` in
/// the loop would abort the whole search). Determinism matters: the rust-stage0 transform
/// co-locates the libgcc_s.so.1 this returns, so a filesystem-order pick would make the relinked
/// tree non-reproducible when a gcc install ships more than one copy.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let rd = fs::read_dir(root).ok()?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let ft = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            dirs.push(e.path());
        } else if ft.is_file() {
            files.push(e.path());
        }
    }
    files.sort();
    if let Some(hit) = files
        .into_iter()
        .find(|p| p.file_name() == Some(OsStr::new(name)))
    {
        return Some(hit);
    }
    dirs.sort();
    for d in dirs {
        if let Some(hit) = find_file(&d, name) {
            return Some(hit);
        }
    }
    None
}


/// `cp -L SRC DST` — copy following a symlink (fs::copy reads through the symlink),
/// making the destination writable (the source .so may be 0444).
fn copy_deref(src: &Path, dst: &Path) -> Result<(), String> {
    fs::copy(src, dst).map_err(|e| format!("cp {} -> {}: {e}", src.display(), dst.display()))?;
    let mut perm = fs::metadata(dst).map_err(ioerr("stat copy"))?.permissions();
    perm.set_mode(perm.mode() | 0o644);
    fs::set_permissions(dst, perm).map_err(ioerr("chmod copy"))
}

/// True iff any file in `dir` is named `<prefix>…<suffix>`.
fn glob_exists(dir: &Path, prefix: &str, suffix: &str) -> bool {
    glob_first_in(dir, prefix, suffix).is_some()
}

/// The first (name-sorted) file in `dir` named `<prefix>…<suffix>`.
fn glob_first_in(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let mut hits: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with(prefix) && s.ends_with(suffix)
        })
        .map(|e| e.path())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// True iff the file contains the literal `/gnu/store` byte sequence.
fn contains_gnu_store(p: &Path) -> Result<bool, String> {
    let bytes = fs::read(p).map_err(|e| format!("read {}: {e}", p.display()))?;
    Ok(contains_sub(&bytes, b"/gnu/store"))
}

fn readelf_header(readelf: &Path, bin: &Path) -> Result<String, String> {
    let out = Command::new(readelf)
        .arg("-h")
        .arg(bin)
        .output()
        .map_err(|e| format!("exec readelf: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn header_is_elf64(h: &str) -> bool {
    h.lines().any(|l| {
        let ll = l.to_ascii_lowercase();
        ll.contains("class:") && ll.contains("elf64")
    })
}

fn header_is_x86_64(h: &str) -> bool {
    h.lines().any(|l| {
        let ll = l.to_ascii_lowercase();
        ll.contains("machine:") && ll.contains("x86-64")
    })
}

fn readelf_is_elf64(readelf: &Path, bin: &Path) -> Result<bool, String> {
    Ok(header_is_elf64(&readelf_header(readelf, bin)?))
}

fn readelf_is_x86_64(readelf: &Path, bin: &Path) -> Result<bool, String> {
    Ok(header_is_x86_64(&readelf_header(readelf, bin)?))
}

fn contains_sub(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn ioerr(ctx: &'static str) -> impl Fn(io::Error) -> String {
    move |e| format!("{ctx}: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(prefix: &str) -> PathBuf {
        mktemp_dir(prefix).expect("mktemp")
    }

    // A minimal uncompressed ustar archive of `(path, contents)` members —
    // `unpack_archive` dispatches to its raw-tar reader when there is no
    // compression magic, so this drives `unpack_rust_release` end to end with
    // no gzip writer. One 512-byte header per member + content padded to 512,
    // then two zero blocks.
    fn ustar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, body) in members {
            let mut h = [0u8; 512];
            let name = path.as_bytes();
            h.get_mut(..name.len()).map(|s| s.copy_from_slice(name));
            let put = |h: &mut [u8; 512], off: usize, s: &str| {
                let b = s.as_bytes();
                h.get_mut(off..off + b.len()).map(|d| d.copy_from_slice(b));
            };
            put(&mut h, 100, "0000644\0"); // mode
            put(&mut h, 108, "0000000\0"); // uid
            put(&mut h, 116, "0000000\0"); // gid
            put(&mut h, 124, &format!("{:011o}\0", body.len())); // size
            put(&mut h, 136, "00000000000\0"); // mtime
            h.get_mut(156).map(|b| *b = b'0'); // typeflag: regular file
            put(&mut h, 257, "ustar\0"); // magic
            put(&mut h, 263, "00"); // version
            // Checksum: sum of all bytes with the checksum field taken as spaces.
            h.get_mut(148..156).map(|s| s.fill(b' '));
            let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
            put(&mut h, 148, &format!("{sum:06o}\0 "));
            out.extend_from_slice(&h);
            out.extend_from_slice(body);
            let pad = (512 - body.len() % 512) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    // PR-tier coverage for the engine-native Rust component unpack. A
    // well-formed component strips its top directory; a component missing its
    // required member reds at this boundary instead of several rungs later.
    #[test]
    fn unpack_rust_component_strips_top_and_guards_layout() {
        let d = tmp("td-rust-unpack");
        let top = "rustc-1.95.0-x86_64-unknown-linux-gnu";
        let good = d.join("good.tar");
        fs::write(
            &good,
            ustar(&[(&format!("{top}/rustc/bin/rustc"), b"#!rustc\n")]),
        )
        .expect("write good tar");
        let out = d.join("good-out");
        unpack_rust_component(&good, &out, "rustc/bin/rustc")
            .expect("well-formed component unpacks");
        assert!(out.join("rustc/bin/rustc").is_file(), "rustc not stripped into place");

        // The same archive cannot masquerade as the separately pinned Cargo component.
        let bad = d.join("bad.tar");
        fs::write(&bad, ustar(&[(&format!("{top}/rustc/bin/rustc"), b"#!rustc\n")]))
            .expect("write bad tar");
        let err = unpack_rust_component(&bad, &d.join("bad-out"), "cargo/bin/cargo")
            .unwrap_err();
        assert!(err.contains("missing cargo/bin/cargo"), "{err}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn native_wrapper_has_shared_guard_and_idirafter() {
        let d = tmp("td-xn-test-wrap");
        let dst = d.join("cc");
        mk_native_static_wrapper(
            Path::new("/xg/bin/x86_64-pc-linux-gnu-gcc"),
            Path::new("/gl"),
            &dst,
            Some(Path::new("/gl/include")),
        )
        .expect("write wrapper");
        let body = fs::read_to_string(&dst).expect("read wrapper");
        // executable path present, -static on the default line, -shared drops -static.
        assert!(body.contains("/xg/bin/x86_64-pc-linux-gnu-gcc"), "body:\n{body}");
        assert!(body.contains("-idirafter /gl/include"), "body:\n{body}");
        assert!(body.contains("-B/gl/lib"), "body:\n{body}");
        assert!(body.contains("-shared)"), "body:\n{body}");
        // the default (non -shared) exec line carries -static; the -shared branch does not.
        let default_line = body.lines().find(|l| l.contains("-static")).expect("a -static line");
        assert!(default_line.contains("-static -idirafter /gl/include -B/gl/lib"), "line: {default_line}");
        // mode is 0555 (executable).
        let mode = fs::metadata(&dst).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "wrapper not 0555");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn native_wrapper_without_hdr_omits_idirafter() {
        let d = tmp("td-xn-test-wrap2");
        let dst = d.join("cc");
        mk_native_static_wrapper(Path::new("/xg/gcc"), Path::new("/gl"), &dst, None).expect("write");
        let body = fs::read_to_string(&dst).expect("read");
        assert!(!body.contains("-idirafter"), "unexpected -idirafter:\n{body}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn find_file_returns_deterministic_recursive_match() {
        let d = tmp("td-xn-test-find");
        // libgcc_s.so.1 nested under a gcc-style tree; a decoy dir sorts earlier.
        fs::create_dir_all(d.join("aaa/decoy")).unwrap();
        fs::create_dir_all(d.join("lib/gcc/x86_64-pc-linux-gnu/14.3.0")).unwrap();
        fs::write(d.join("lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc_s.so.1"), b"x").unwrap();
        let hit = find_file(&d, "libgcc_s.so.1").expect("found libgcc");
        assert!(
            hit.ends_with("lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc_s.so.1"),
            "hit: {}",
            hit.display()
        );
        // absent name → None (the transform's "no libgcc_s.so.1 under …" red).
        assert!(find_file(&d, "libz.so.1.3.1").is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn find_file_picks_sorted_first_on_multiple_matches() {
        // Two copies (as a multilib gcc install can ship): the files-first sorted DFS must pick
        // a STABLE one every run, else the co-located libgcc makes the relinked tree non-repro.
        let d = tmp("td-xn-test-find2");
        fs::create_dir_all(d.join("zdir")).unwrap();
        fs::create_dir_all(d.join("adir")).unwrap();
        fs::write(d.join("zdir/libgcc_s.so.1"), b"x").unwrap();
        fs::write(d.join("adir/libgcc_s.so.1"), b"x").unwrap();
        let hit = find_file(&d, "libgcc_s.so.1").expect("found");
        assert!(
            hit.ends_with("adir/libgcc_s.so.1"),
            "expected the sorted-first (adir) match, got {}",
            hit.display()
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ld_scripts_relocated_only_when_marked() {
        let d = tmp("td-xn-test-ld");
        let lib = d.join("lib");
        fs::create_dir_all(&lib).unwrap();
        // a GNU ld script referencing the absolute prefix → prefix stripped.
        let script = "/* GNU ld script */\nGROUP ( /td/store/glibc-2.41-x86_64/lib/libc.so.6 /td/store/glibc-2.41-x86_64/lib/libc_nonshared.a )\n";
        fs::write(lib.join("libc.so"), script).unwrap();
        // a real (binary-ish) .so that is NOT a GNU ld script → untouched.
        fs::write(lib.join("libc.so.6"), b"\x7fELF fake binary /td/store/glibc-2.41-x86_64/lib/keep").unwrap();
        relocate_ld_scripts(&lib).expect("relocate");
        let got = fs::read_to_string(lib.join("libc.so")).unwrap();
        assert!(got.contains("GROUP ( libc.so.6 libc_nonshared.a )"), "got: {got}");
        assert!(!got.contains("/td/store/glibc-2.41-x86_64/lib/"), "prefix not stripped: {got}");
        // the non-script file keeps its bytes (prefix NOT stripped).
        let bin = fs::read(lib.join("libc.so.6")).unwrap();
        assert!(contains_sub(&bin, b"/td/store/glibc-2.41-x86_64/lib/keep"), "binary was rewritten");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn binsh_shebangs_rewritten_selectively() {
        let d = tmp("td-xn-test-sh");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.sh"), "#!/bin/sh\necho hi\n").unwrap();
        fs::write(d.join("sub/b.sh"), "#! /bin/sh -e\necho ho\n").unwrap();
        fs::write(d.join("c.pl"), "#!/usr/bin/perl\nprint 1;\n").unwrap();
        fs::write(d.join("d.txt"), "not a script /bin/sh inside\n").unwrap();
        rewrite_binsh_shebangs(&d, "/curated/bash").expect("rewrite");
        assert_eq!(fs::read_to_string(d.join("a.sh")).unwrap(), "#!/curated/bash\necho hi\n");
        // the shebang TAIL (args like ` -e`) is preserved, exactly as the shell sed keeps
        // everything past /bin/sh — dropping it would change the script's error behavior.
        assert_eq!(fs::read_to_string(d.join("sub/b.sh")).unwrap(), "#!/curated/bash -e\necho ho\n");
        // non-/bin/sh interpreter untouched.
        assert_eq!(fs::read_to_string(d.join("c.pl")).unwrap(), "#!/usr/bin/perl\nprint 1;\n");
        // non-shebang file untouched.
        assert_eq!(fs::read_to_string(d.join("d.txt")).unwrap(), "not a script /bin/sh inside\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn self_flavor_naming() {
        // the self build names its outputs `…-x86_64-self` / reads `TDXS_*` — distinct
        // from rung X2's gcc-x86-64-native recipe (`…-x86_64-native`), so a published
        // self artifact can never be confused with the native one.
        assert_eq!(Flavor::SelfHost.suffix(), "self");
        assert_eq!(Flavor::SelfHost.env_prefix(), "TDXS");
    }

    #[test]
    fn contains_sub_matches() {
        assert!(contains_sub(b"aa GNU ld script bb", b"GNU ld script"));
        assert!(!contains_sub(b"nope", b"GNU ld script"));
        assert!(!contains_sub(b"anything", b""));
    }

    #[test]
    fn glob_first_in_matches_prefix_and_suffix() {
        let d = tmp("td-xn-test-glob");
        fs::write(d.join("libstd-abc123.rlib"), b"x").unwrap();
        fs::write(d.join("libstd-abc123.so"), b"x").unwrap();
        fs::write(d.join("libcore-xyz.rlib"), b"x").unwrap();
        assert!(glob_exists(&d, "libstd-", ".rlib"));
        assert!(!glob_exists(&d, "libstd-", ".dylib"));
        // name-sorted first match.
        let hit = glob_first_in(&d, "lib", ".rlib").unwrap();
        assert!(hit.file_name().unwrap().to_string_lossy().starts_with("libcore-"), "got {hit:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn contains_gnu_store_detects_bytes() {
        let d = tmp("td-xn-test-gnu");
        fs::write(d.join("clean"), b"\x7fELF ordinary binary").unwrap();
        fs::write(d.join("dirty"), b"\x7fELF refers to /gnu/store/abc-foo/lib").unwrap();
        assert!(!contains_gnu_store(&d.join("clean")).unwrap());
        assert!(contains_gnu_store(&d.join("dirty")).unwrap());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_tree_overlays_a_read_only_dest() {
        // the rustlib merge overlays rust-std over rustc's rustlib; rust ships 0444 files, so
        // the overlay must succeed over a read-only destination (cp -f), not fail with EACCES.
        let d = tmp("td-xn-test-overlay");
        let src = d.join("src");
        let dst = d.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("f"), b"OLD").unwrap();
        set_mode(&dst.join("f"), 0o444).unwrap(); // read-only existing dest
        fs::write(src.join("f"), b"NEW").unwrap();
        copy_tree_contents(&src, &dst).expect("overlay over a read-only dest must succeed");
        assert_eq!(fs::read(dst.join("f")).unwrap(), b"NEW");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_deref_follows_symlink_and_makes_writable() {
        let d = tmp("td-xn-test-deref");
        let real = d.join("libc-real.so");
        fs::write(&real, b"REALBYTES").unwrap();
        set_mode(&real, 0o444).unwrap();
        let link = d.join("libc.so.6");
        std::os::unix::fs::symlink("libc-real.so", &link).unwrap();
        let dst = d.join("out/libc.so.6");
        fs::create_dir_all(d.join("out")).unwrap();
        // copy through the symlink → the destination holds the TARGET's bytes and is writable.
        copy_deref(&link, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"REALBYTES");
        assert!(!fs::symlink_metadata(&dst).unwrap().file_type().is_symlink(), "dst should be a real file, not a symlink");
        assert!(fs::metadata(&dst).unwrap().permissions().mode() & 0o200 != 0, "dst should be writable");
        let _ = fs::remove_dir_all(&d);
    }
}
