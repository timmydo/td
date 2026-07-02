//! Structured Rust port of the NATIVE x86_64 toolchain build (rung X2) — the
//! `build_binutils_x86_64_native` + `build_gcc_x86_64_native` drivers that until now
//! lived as imperative shell in `tests/x86_64-cross-fns.sh`. This kills that ad-hoc
//! build logic: `td-builder toolchain-recipe x86_64-native` builds the NATIVE (ELF
//! 64-bit, `--host=x86_64`) binutils 2.44 + gcc 14.3.0 from the CROSS toolchain, so
//! the build is one typed unit the loop (and, later, the substitute publisher) drives
//! uniformly — the same "port the shell driver → structured recipe" move #229 made for
//! the seed/mes rungs, extended to the x86_64 track named by this workstream.
//!
//! The NATIVE build is NOT byte-reproducible (trust = the input-addressed lock name +
//! the ed25519 substitute signature, see `tests/td-toolchain-x86_64-native.lock`), so
//! this is deliberately NOT a `bootstrap::Recipe` (whose leg skeleton double-builds and
//! asserts byte-identity). It is the build half only; the gate keeps interning the
//! outputs at their lock-keyed paths (`store-add-input-addressed`), running the own-root
//! behavioral verify, and `subst-export`ing them — those are generic `td-builder`
//! subcommands, not ad-hoc build logic.
//!
//! Inputs (the cross toolchain + pinned sources) are passed by the caller — the gate has
//! them as shell vars (fetched from the substitute closure or built from seed). The port
//! mirrors the shell's every configure flag, env var and wrapper; the divergences that
//! are load-bearing carry the shell's own comments.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const XTARGET: &str = "x86_64-pc-linux-gnu";

/// Everything a native rung needs. The gate populates it (from the fetched-or-built
/// cross closure + the warmed pinned sources) via the CLI spec.
pub struct NativeInputs {
    /// Scaffolding PATH tail (coreutils/bash/… — the exposed /gnu/store build tools).
    pub cpath: String,
    /// The CROSS gcc 14.3.0 stage2 tree (`XGCC2`; an i686 binary emitting x86_64).
    pub cross_gcc: PathBuf,
    /// The CROSS binutils 2.44 tree (`XBU`; i686 host `x86_64-pc-linux-gnu-*`).
    pub cross_binutils: PathBuf,
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
}

impl NativeInputs {
    /// Read the inputs from `TDXN_*` env vars (how the gate passes them).
    fn from_env() -> Result<NativeInputs, String> {
        let g = |k: &str| -> Result<String, String> {
            std::env::var(k).map_err(|_| format!("env {k} unset"))
        };
        Ok(NativeInputs {
            cpath: std::env::var("TDXN_CPATH").unwrap_or_default(),
            cross_gcc: PathBuf::from(g("TDXN_CROSS_GCC")?),
            cross_binutils: PathBuf::from(g("TDXN_CROSS_BINUTILS")?),
            glibc: PathBuf::from(g("TDXN_GLIBC")?),
            binutils_tar: PathBuf::from(g("TDXN_BINUTILS_TAR")?),
            gcc_tar: PathBuf::from(g("TDXN_GCC_TAR")?),
            gmp_tar: PathBuf::from(g("TDXN_GMP_TAR")?),
            mpfr_tar: PathBuf::from(g("TDXN_MPFR_TAR")?),
            mpc_tar: PathBuf::from(g("TDXN_MPC_TAR")?),
            kernel_headers_tar: PathBuf::from(g("TDXN_KERNEL_HEADERS_TAR")?),
            out: PathBuf::from(g("TDXN_OUT")?),
        })
    }

    fn xgcc_gcc(&self) -> PathBuf {
        self.cross_gcc.join("bin").join(format!("{XTARGET}-gcc"))
    }
    fn xgcc_gpp(&self) -> PathBuf {
        self.cross_gcc.join("bin").join(format!("{XTARGET}-g++"))
    }
}

// --- CLI -------------------------------------------------------------------------

const USAGE: &str = "usage: td-builder toolchain-recipe x86_64-native  (inputs via TDXN_* env)";

/// `td-builder toolchain-recipe <name>`.
pub fn cli(args: &[String]) -> ExitCode {
    match args.get(2).map(String::as_str) {
        Some("x86_64-native") => {
            let result = NativeInputs::from_env().and_then(|inp| run_native(&inp));
            match result {
                Ok(report) => {
                    print!("{report}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("FAIL: toolchain-recipe x86_64-native: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("--list") | Some("list") => {
            println!("x86_64-native");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// Build the native binutils then the native gcc; return the leg-by-leg report. The
/// two output trees are `<out>/binutils/stage-prefix` and `<out>/gcc/stage-prefix`;
/// their staged store-prefix subdirs are printed as `NATIVE_BINUTILS=`/`NATIVE_GCC=`
/// lines the gate reads (like `build-recipe`'s `OUT=` line).
pub fn run_native(inp: &NativeInputs) -> Result<String, String> {
    let mut report = String::new();
    let nbu = build_binutils_native(inp)?;
    report.push_str(&format!(
        "   [build] native x86_64 binutils 2.44 built (ELF64) at {}\n",
        nbu.display()
    ));
    let ngcc = build_gcc_native(inp, &nbu)?;
    report.push_str(&format!(
        "   [build] native x86_64 gcc 14.3.0 (c,c++) built (ELF64 x86-64) at {}\n",
        ngcc.display()
    ));
    // Machine-readable lines for the gate.
    report.push_str(&format!("NATIVE_BINUTILS={}\n", nbu.display()));
    report.push_str(&format!("NATIVE_GCC={}\n", ngcc.display()));
    Ok(report)
}

// --- native binutils 2.44 --------------------------------------------------------

/// Port of `build_binutils_x86_64_native`. NATIVE GNU Binutils 2.44
/// (`--build=--host=--target=x86_64-pc-linux-gnu`), built STATIC by the cross gcc
/// 14.3.0 vs the /td/store x86_64 glibc 2.41 static archives. Returns the install
/// prefix (`<out>/binutils`) — plain-named ELF64 `as`/`ld`/`ar`/…
fn build_binutils_native(inp: &NativeInputs) -> Result<PathBuf, String> {
    let out = inp.out.join("binutils");
    reset_dir(&out)?;
    let xz = store_tool("xz", "xz-").ok_or("no xz")?;
    let csh = shell();

    // x86_64 kernel UAPI headers beside the glibc headers (glibc headers #include <linux/…>).
    let khd = mktemp_dir("td-xn-kh")?;
    untar(&xz, &inp.kernel_headers_tar, &khd, 0, TarComp::Gz)?;
    let cip = format!("{}:{}", inp.glibc.join("include").display(), khd.display());

    // -shared-aware static wrapper (handles binutils' ld libdep.la shared module).
    let wb = mktemp_dir("td-xn-wb")?;
    let cc = wb.join("cc");
    mk_native_static_wrapper(&inp.xgcc_gcc(), &inp.glibc, &cc, None)?;

    let tb = mktemp_dir("td-xn-tb")?;
    xbin(&tb)?;

    let src = mktemp_dir("td-xn-binutils")?;
    untar(&xz, &inp.binutils_tar, &src, 1, TarComp::Xz)?;

    let bp = format!(
        "{}:{}:{}",
        inp.cross_binutils.join("bin").display(),
        tb.display(),
        inp.cpath
    );

    // configure
    let mut cfg = Command::new(&csh);
    cfg.arg("./configure")
        .arg("--build=x86_64-pc-linux-gnu")
        .arg("--host=x86_64-pc-linux-gnu")
        .arg("--target=x86_64-pc-linux-gnu")
        .arg("--prefix=/td/store/binutils-2.44-x86_64-native")
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

// --- native gcc 14.3.0 -----------------------------------------------------------

/// Port of `build_gcc_x86_64_native`. NATIVE GCC 14.3.0 (c,c++;
/// `--build=--host=--target=x86_64-pc-linux-gnu`), built STATIC by the cross gcc
/// 14.3.0 vs the /td/store x86_64 glibc 2.41, gmp/mpfr/mpc in-tree, as/ld = the native
/// binutils. Returns the staged prefix `<out>/gcc/stage/td/store/gcc-14.3.0-x86_64-native`.
fn build_gcc_native(inp: &NativeInputs, native_binutils: &Path) -> Result<PathBuf, String> {
    let out = inp.out.join("gcc");
    reset_dir(&out)?;
    let xz = store_tool("xz", "xz-").ok_or("no xz")?;
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
    mk_native_static_wrapper(&inp.xgcc_gcc(), &sysroot, &wb.join("gcc"), Some(&inc))?;
    mk_native_static_wrapper(&inp.xgcc_gpp(), &sysroot, &wb.join("g++"), Some(&inc))?;

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
        native_binutils.join("bin").display(),
        inp.cross_binutils.join("bin").display(),
        tb.display(),
        inp.cpath
    );
    let wgcc = wb.join("gcc");
    let wgpp = wb.join("g++");
    let cpp = format!("{} -E", wgcc.display());

    // configure
    let mut cfg = Command::new(&csh);
    cfg.arg("../configure")
        .arg("--prefix=/td/store/gcc-14.3.0-x86_64-native")
        .arg("--build=x86_64-pc-linux-gnu")
        .arg("--host=x86_64-pc-linux-gnu")
        .arg("--target=x86_64-pc-linux-gnu")
        .arg(format!("--with-as={}", native_binutils.join("bin/as").display()))
        .arg(format!("--with-ld={}", native_binutils.join("bin/ld").display()))
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

    let g = out.join("stage/td/store/gcc-14.3.0-x86_64-native");
    if !is_exec(&g.join("bin/gcc")) || !is_exec(&g.join("bin/g++")) {
        return Err("no native gcc/g++ produced".into());
    }
    if find_file(&g, "cc1").is_none() {
        return Err("native gcc produced no cc1".into());
    }
    let readelf = native_binutils.join("bin/readelf");
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

/// Port of `_xbin`: a bin/ of build-time scaffolding tools (from PATH or the exposed
/// /gnu/store) the autoconf/recursive-make builds need. Symlinks lex→flex, yacc→bison.
fn xbin(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(ioerr("mkdir xbin"))?;
    // (tool, guix-pkg-substring)
    let tools = [
        ("awk", "gawk"),
        ("gawk", "gawk"),
        ("sed", "sed"),
        ("grep", "grep"),
        ("make", "make"),
        ("m4", "m4"),
        ("bison", "bison"),
        ("flex", "flex"),
        ("cmp", "diffutils"),
        ("diff", "diffutils"),
        ("msgfmt", "gettext"),
        ("makeinfo", "texinfo"),
        ("python3", "python"),
        ("gzip", "gzip"),
    ];
    for (name, pkg) in tools {
        if let Some(b) = store_tool(name, pkg) {
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

/// Port of `_store_tool`: `command -v NAME`, else the first `/gnu/store/*PKG*/bin/NAME`.
fn store_tool(name: &str, pkg: &str) -> Option<PathBuf> {
    if let Some(p) = which(name) {
        return Some(p);
    }
    // ls /gnu/store/*pkg*/bin/name | sort | head -1
    let store = Path::new("/gnu/store");
    let mut hits: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(store) {
        for e in rd.flatten() {
            let fname = e.file_name();
            let s = fname.to_string_lossy();
            if s.contains(pkg) {
                let cand = e.path().join("bin").join(name);
                if cand.exists() {
                    hits.push(cand);
                }
            }
        }
    }
    hits.sort();
    hits.into_iter().next()
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
        fs::copy(from, to)?;
    }
    Ok(())
}

/// Find the first file named `name` anywhere under `root`.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = fs::read_dir(&d).ok()?;
        for e in rd.flatten() {
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && e.file_name() == OsStr::new(name) {
                return Some(p);
            }
        }
    }
    None
}

fn readelf_header(readelf: &Path, bin: &Path) -> Result<String, String> {
    let out = Command::new(readelf)
        .arg("-h")
        .arg(bin)
        .output()
        .map_err(|e| format!("exec readelf: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn readelf_is_elf64(readelf: &Path, bin: &Path) -> Result<bool, String> {
    let h = readelf_header(readelf, bin)?;
    Ok(h.lines().any(|l| {
        let ll = l.to_ascii_lowercase();
        ll.contains("class:") && ll.contains("elf64")
    }))
}

fn readelf_is_x86_64(readelf: &Path, bin: &Path) -> Result<bool, String> {
    let h = readelf_header(readelf, bin)?;
    Ok(h.lines().any(|l| {
        let ll = l.to_ascii_lowercase();
        ll.contains("machine:") && ll.contains("x86-64")
    }))
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
    fn contains_sub_matches() {
        assert!(contains_sub(b"aa GNU ld script bb", b"GNU ld script"));
        assert!(!contains_sub(b"nope", b"GNU ld script"));
        assert!(!contains_sub(b"anything", b""));
    }
}
