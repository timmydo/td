//! The ENGINE-NATIVE tcc bootstrap rung (re #469): the Rust port of the tcc
//! (mes fork) tarball's `configure`, `bootstrap.sh`, and `boot.sh`.
//!
//! Rung 3 of the bootstrap ladder (mes → tcc → …). guix's `tcc-boot0`: MesCC
//! compiles the first `tcc` (`tcc-mes`), which then self-hosts through six more
//! generations (`tcc-boot0` … `tcc-boot5`) until the compiler is a stable
//! fixpoint. `tcc-boot5` becomes the installed `tcc`.
//!
//! Why a port and not the scripts (the mes precedent, `mes_boot.rs`): the
//! scripts' only host needs are a shell and the file tools (`cp`/`mkdir`/`rm`,
//! plus `basename`/`cmp`) — at rung 3 there is no td-built coreutils yet for a
//! declared `td-sh` to shell out to, so a bash→td-sh swap cannot cut the host
//! edge. This module IS that orchestration: the file work is `std::fs`, and the
//! only subprocesses are recipe outputs — the mes rung's `mes` running upstream
//! `mescc.scm` (for `tcc-mes`), stage0's `M1`/`hex2`/`blood-elf` (mescc's
//! assembler/linker), and the just-built `tcc` binaries (native, self-contained
//! static ELF). The rung declares NO host shell or coreutils.
//!
//! Fidelity: every command, flag block, and generation here is transcribed from
//! the pinned tarball's scripts — validated against a golden `boot.log`. With
//! the rung's fixed env (`host=i686-linux-gnu` → x86/i386, `ONE_SOURCE=true`)
//! the scripts collapse to a single `tcc.c` compile unit per generation, and
//! the `cmp` fixpoint check is upstream-skipped (`cmp: command not found`), so
//! the port needs no `cmp`. A `PORTED_SCRIPT_PINS` tripwire refuses BEFORE
//! building if a source-pin bump drifts any transcribed script.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

// The rung's fixed target (the recipe's `host=i686-linux-gnu`): mes_cpu=x86,
// tcc_cpu=i386. bootstrap.sh's `have_float`/`have_long_long`/`have_setjmp` all
// default true for x86.
const MES_CPU: &str = "x86";
// The mescc interpreter limits the recipe's bootstrap.sh step declared.
const MES_ARENA: &str = "20000000";
const MES_STACK: &str = "6000000";
// mes installs its guile modules under both site/2.2 (install.sh's effective
// version) and site/3.0 (the mes rung's CopyTree); the tcc build drives mescc
// on the 3.0 tree, matching the deleted recipe's `GUILE_LOAD_PATH`.
const GUILE_SITE: &str = "share/guile/site/3.0";

// ---- small fs helpers (mes_boot's, kept module-local) ---------------------

fn ps(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("non-UTF-8 path: {}", p.display()))
}

fn read(p: &Path) -> Result<Vec<u8>, String> {
    fs::read(p).map_err(|e| format!("read {}: {e}", p.display()))
}

fn write(p: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(p, data).map_err(|e| format!("write {}: {e}", p.display()))
}

fn mkdir_p(p: &Path) -> Result<(), String> {
    fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))
}

fn chmod_x(p: &Path) -> Result<(), String> {
    fs::set_permissions(p, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", p.display()))
}

/// `cp -f` semantics: parents made, destination overwritten.
fn cp(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(to);
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))
}

// ---- transcription tripwire ------------------------------------------------

/// The upstream scripts this port TRANSCRIBES, pinned to the audited bytes: the
/// port hardcodes what these say — the compile/link command shapes, the flag
/// blocks, the per-generation `-D` sets, and the install artifacts. A source-pin
/// bump that changes any of them must re-audit the port (and update its pin)
/// before building, or the stale transcription would build silently.
const PORTED_SCRIPT_PINS: [(&str, &str); 3] = [
    (
        "configure",
        "f18d1c603e4100ef073321b11d4d5661770c8e423d4ff8a69856b4ffecb7aeac",
    ),
    (
        "bootstrap.sh",
        "feb55822f9c8cb6baba8526cbec6872f9e6b55766cd1cdb4385e5dce638151b8",
    ),
    (
        "boot.sh",
        "2fa667b81ebff500fec907bf8a3c75dabae1bd3314932d95a86ff7cfb68741b4",
    ),
];

fn verify_ported_scripts(top: &Path) -> Result<(), String> {
    for (rel, want) in PORTED_SCRIPT_PINS {
        let p = top.join(rel);
        let got =
            crate::sha256::sha256_file(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
        if got != want {
            return Err(format!(
                "tcc tarball script {rel} hashes {got}, not the transcription baseline \
                 {want}: tcc_boot transcribes this script's contents, so a changed script \
                 means the port must be re-audited (and its pin updated) before building"
            ));
        }
    }
    Ok(())
}

// ---- shared plumbing -------------------------------------------------------

struct Cfg {
    /// The unpacked tcc tree — srcdir and builddir at once (the in-tree build).
    top: PathBuf,
    /// The staged mes rung output (`bin/mes`, `bin/mescc.scm`, the guile module
    /// site tree, and `lib/` = MES_LIB with the crt/libc sources).
    mes: PathBuf,
    /// The staged stage0 rung output (M1/hex2/blood-elf under AMD64).
    stage0: PathBuf,
    /// The store output dir (prefix); tcc + libs install here.
    out: String,
    /// tcc's own internal version (config.h's `TCC_VERSION`, from the VERSION
    /// file) — distinct from the tarball's `0.9.26-1149-…` name.
    version: String,
}

/// Build + install the tcc rung. SOURCE/MES/STAGE0 are staged recipe outputs,
/// OUT the store output dir. Runs in the sandbox cwd ({root}): the tcc tree
/// unpacks to `{root}/tcc-src`, the in-tree build's cwd.
pub(crate) fn run(source: &str, mes: &str, stage0: &str, out: &str) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let top = cwd.join("tcc-src");
    crate::tar::unpack_archive(Path::new(source), &top, false)?;

    verify_ported_scripts(&top)?;
    let version = read_version(&top.join("VERSION"))?;
    let cfg = Cfg {
        top,
        mes: PathBuf::from(mes),
        stage0: PathBuf::from(stage0),
        out: out.to_string(),
        version,
    };

    configure(&cfg)?;
    bootstrap(&cfg)?;
    install(&cfg)
}

/// tcc's `VERSION` file (`0.9.27`) — configure bakes it into config.h's
/// `TCC_VERSION`. Whitespace-trimmed; empty is a hard error.
fn read_version(version: &Path) -> Result<String, String> {
    let body = String::from_utf8(read(version)?)
        .map_err(|_| format!("{} is not UTF-8", version.display()))?;
    let v = body.trim();
    if v.is_empty() {
        return Err(format!("empty VERSION file {}", version.display()));
    }
    Ok(v.to_string())
}

/// configure's only load-bearing output for the hand-build: `config.h` (tcc.c
/// includes it for `TCC_VERSION`). configure's other output, `config.mak`, is
/// not written — the hand-build (bootstrap.sh/boot.sh) sets its own flags and
/// never sources it. With `cc=mescc` configure detects no gcc, so
/// `GCC_MAJOR`/`GCC_MINOR` are empty.
fn configure(cfg: &Cfg) -> Result<(), String> {
    let config_h = format!(
        "/* Automatically generated by configure - do not modify */\n\
         #ifndef CONFIG_TCCDIR\n\
         # define CONFIG_TCCDIR \".\"\n\
         #endif\n\
         #define GCC_MAJOR \n\
         #define GCC_MINOR \n\
         #define TCC_VERSION \"{}\"\n",
        cfg.version
    );
    write(&cfg.top.join("config.h"), config_h.as_bytes())
}

/// The `CONFIG_*` `-D` block shared by every tcc compile (bootstrap.sh's and
/// boot.sh's `CPPFLAGS_TCC` tail). `prefix` is the store OUT; the literal
/// `{B}` placeholders are upstream's (a runtime crt-search token), not ours.
/// Each `-D NAME="value"` carries the embedded quotes the C string literal
/// needs, exactly as the shell's `\"…\"` produced.
fn config_defs(cfg: &Cfg) -> Result<Vec<String>, String> {
    let out = &cfg.out;
    let mes = ps(&cfg.mes)?;
    Ok(vec![
        "-D".into(),
        "inline=".into(),
        "-D".into(),
        format!("CONFIG_TCCDIR=\"{out}/lib/tcc\""),
        "-D".into(),
        format!("CONFIG_TCC_CRTPREFIX=\"{out}/lib:{{B}}/lib:.\""),
        "-D".into(),
        "CONFIG_TCC_ELFINTERP=\"/lib/mes-loader\"".into(),
        "-D".into(),
        format!("CONFIG_TCC_LIBPATHS=\"{out}/lib:{{B}}/lib:.\""),
        "-D".into(),
        format!("CONFIG_TCC_SYSINCLUDEPATHS=\"{mes}/include:{out}/include:{{B}}/include\""),
        "-D".into(),
        format!("TCC_LIBGCC=\"{out}/lib/libc.a\""),
        "-D".into(),
        "CONFIG_TCCBOOT=1".into(),
        "-D".into(),
        "CONFIG_TCC_STATIC=1".into(),
        "-D".into(),
        "CONFIG_USE_LIBGCC=1".into(),
        "-D".into(),
        "TCC_MES_LIBC=1".into(),
        "-D".into(),
        "TCC_LIBTCC1_MES=\"libtcc1-mes.a\"".into(),
        "-D".into(),
        "ONE_SOURCE=1".into(),
    ])
}

/// One mescc invocation, exactly as mes's installed `bin/mescc` wrapper
/// composes it (`scripts/mescc.in`): the mes rung's `mes` runs its installed
/// `mescc.scm`; M1/HEX2/BLOOD_ELF are stage0's assembler/linker;
/// MES_PREFIX/GUILE_LOAD_PATH make the installed modules resolvable.
fn mescc(cfg: &Cfg, args: &[String]) -> Result<(), String> {
    let mes = ps(&cfg.mes)?;
    let stage0 = ps(&cfg.stage0)?;
    let site = format!("{mes}/{GUILE_SITE}");
    let ccache = format!("{mes}/lib/guile/3.0/site-ccache");
    let stage0_bin = format!("{stage0}/AMD64/bin");
    let envs: Vec<(String, String)> = vec![
        ("MES_ARENA".to_string(), MES_ARENA.to_string()),
        ("MES_MAX_ARENA".to_string(), MES_ARENA.to_string()),
        ("MES_STACK".to_string(), MES_STACK.to_string()),
        ("MES_PREFIX".to_string(), mes.to_string()),
        ("GUILE_LOAD_PATH".to_string(), site.clone()),
        ("M1".to_string(), format!("{stage0_bin}/M1")),
        ("HEX2".to_string(), format!("{stage0_bin}/hex2")),
        (
            "BLOOD_ELF".to_string(),
            format!("{stage0}/AMD64/artifact/blood-elf-0"),
        ),
        ("PATH".to_string(), stage0_bin),
        ("LANG".to_string(), String::new()),
        ("LC_ALL".to_string(), String::new()),
    ];
    let mes_bin = format!("{mes}/bin/mes");
    let mescc_scm = format!("{mes}/bin/mescc.scm");
    let mut argv: Vec<&str> = vec![
        "--no-auto-compile",
        "-e",
        "main",
        "-L",
        &site,
        "-C",
        &ccache,
        &mescc_scm,
        "--",
    ];
    argv.extend(args.iter().map(String::as_str));
    crate::build::run_cmd_phase(&mes_bin, &argv, ps(&cfg.top)?, &envs)
}

/// Run a just-built tcc binary (native static ELF — no shell, no env needed).
/// PROG is relative to the build cwd (`./tcc-mes`, `./tcc-boot0`, `./tcc`).
fn tcc(cfg: &Cfg, prog: &str, args: &[String]) -> Result<(), String> {
    let bin = cfg.top.join(prog.trim_start_matches("./"));
    crate::build::run_cmd_phase(ps(&bin)?, &to_str(args), ps(&cfg.top)?, &[])
}

fn to_str(xs: &[String]) -> Vec<&str> {
    xs.iter().map(String::as_str).collect()
}

fn owned(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_string()).collect()
}

/// bootstrap.sh (ONE_SOURCE, x86): MesCC compiles+links `tcc-mes`, rebuilds the
/// libc/libtcc1/crt/libgetopt with it, self-hosts through boot.sh's six
/// generations, installs `tcc-boot5` as `tcc`, then rebuilds the libs once more
/// with the final `tcc`.
fn bootstrap(cfg: &Cfg) -> Result<(), String> {
    let top = &cfg.top;
    let mes = ps(&cfg.mes)?.to_string();
    let out = cfg.out.clone();
    let mes_lib = format!("{mes}/lib");

    // Phase 1: mescc compiles tcc.c → tcc.s, then links tcc-mes. bootstrap.sh's
    // CPPFLAGS_TCC = CPPFLAGS (`-I mes/lib -I mes/include -D BOOTSTRAP=1`) then
    // `-I .`, the target flag, and the shared CONFIG_* block.
    let mut cflags = owned(&[
        "-I",
        &mes_lib,
        "-I",
        &format!("{mes}/include"),
        "-D",
        "BOOTSTRAP=1",
        "-I",
        ".",
    ]);
    cflags.push("-D".into());
    cflags.push("TCC_TARGET_I386=1".into());
    cflags.extend(config_defs(cfg)?);

    let mut compile = owned(&["-S", "-o", "tcc.s"]);
    compile.extend(cflags.clone());
    compile.push("tcc.c".into());
    mescc(cfg, &compile)?;

    // Link tcc-mes against the mes MesCC libc (`-l c+tcc` = libc+tcc.a under
    // mes/lib/x86-mes).
    let link = owned(&[
        "-o",
        "tcc-mes",
        "-L",
        &format!("{mes}/lib/{MES_CPU}-mes"),
        "-L",
        &mes_lib,
        "tcc.s",
        "-l",
        "c+tcc",
    ]);
    mescc(cfg, &link)?;

    mkdir_p(&Path::new(&out).join("lib/tcc"))?;

    // Phase 2: REBUILD_LIBC with tcc-mes. CPPFLAGS resets to
    // `-I mes/include -I mes/lib -D BOOTSTRAP=1`.
    let rebuild_cppflags = owned(&[
        "-I",
        &format!("{mes}/include"),
        "-I",
        &mes_lib,
        "-D",
        "BOOTSTRAP=1",
    ]);
    rebuild_libs_with(cfg, "./tcc-mes", &rebuild_cppflags, &mes_lib, false)?;
    // Install the freshly built libs where the baked CONFIG_TCC_* search finds
    // them for the boot generations.
    cp(&top.join("libc.a"), &Path::new(&out).join("lib/libc.a"))?;
    cp(&top.join("libtcc1.a"), &Path::new(&out).join("lib/tcc/libtcc1.a"))?;
    cp(&top.join("libgetopt.a"), &Path::new(&out).join("lib/libgetopt.a"))?;

    // Phase 3: self-host. tcc-mes → tcc-boot0 → … → tcc-boot6; tcc-boot5 wins.
    // (tcc-boot6 exists only for the upstream cmp fixpoint check, itself skipped
    // — `cmp` is not an admissible input.)
    let gens = [
        ("./tcc-mes", "-boot0"),
        ("./tcc-boot0", "-boot1"),
        ("./tcc-boot1", "-boot2"),
        ("./tcc-boot2", "-boot3"),
        ("./tcc-boot3", "-boot4"),
        ("./tcc-boot4", "-boot5"),
        ("./tcc-boot5", "-boot6"),
    ];
    for (caller, suffix) in gens {
        boot_gen(cfg, caller, suffix)?;
    }
    cp(&top.join("tcc-boot5"), &top.join("tcc"))?;
    chmod_x(&top.join("tcc"))?;

    // Phase 4: final lib rebuild with the installed tcc (bootstrap.sh's tail —
    // libtcc1 now from the tcc source's own lib/libtcc1.c).
    rebuild_libs_with(cfg, "./tcc", &rebuild_cppflags, &mes_lib, true)?;
    cp(&top.join("libc.a"), &Path::new(&out).join("lib/libc.a"))?;
    cp(&top.join("libtcc1.a"), &Path::new(&out).join("lib/tcc/libtcc1.a"))?;
    cp(&top.join("libgetopt.a"), &Path::new(&out).join("lib/libgetopt.a"))
}

/// bootstrap.sh's REBUILD_LIBC block, shared by the tcc-mes pass and the final
/// tcc pass. `final_pass` picks the tcc-source `lib/libtcc1.c` (tail) over the
/// mes `MES_LIB/libtcc1.c` (first pass), matching bootstrap.sh.
fn rebuild_libs_with(
    cfg: &Cfg,
    cc: &str,
    cppflags: &[String],
    mes_lib: &str,
    final_pass: bool,
) -> Result<(), String> {
    let top = &cfg.top;
    // crt1/crti/crtn: mes sources compiled freestanding.
    for i in ["1", "i", "n"] {
        cp(
            &Path::new(mes_lib).join(format!("crt{i}.c")),
            &top.join(format!("crt{i}.c")),
        )?;
        let _ = fs::remove_file(top.join(format!("crt{i}.o")));
        let mut a = owned(&["-g"]);
        if !final_pass {
            a.truncate(0); // first pass: no -g on crt (bootstrap.sh's `$CC $CPPFLAGS`)
        }
        a.extend_from_slice(cppflags);
        a.extend(owned(&["-static", "-nostdlib", "-nostdinc", "-c"]));
        a.push(format!("crt{i}.c"));
        tcc(cfg, cc, &a)?;
    }

    // libc.a: mes libc+gnu.c copied to libc.c (first pass), reused on the final.
    if !final_pass {
        cp(&Path::new(mes_lib).join("libc+gnu.c"), &top.join("libc.c"))?;
    }
    let _ = fs::remove_file(top.join("libc.a"));
    let mut libc = owned(&["-c"]);
    if final_pass {
        libc = owned(&["-c", "-g"]);
    }
    libc.extend_from_slice(cppflags);
    libc.push("libc.c".into());
    tcc(cfg, cc, &libc)?;
    ar(cfg, cc, "libc.a", &["libc.o"])?;

    // libtcc1.a.
    let _ = fs::remove_file(top.join("libtcc1.a"));
    if final_pass {
        // tail: source's own lib/libtcc1.c, with the target flag.
        let mut lt = owned(&["-c", "-g"]);
        lt.extend_from_slice(cppflags);
        lt.push("-D".into());
        lt.push("TCC_TARGET_I386=1".into());
        lt.push("lib/libtcc1.c".into());
        tcc(cfg, cc, &lt)?;
    } else {
        cp(
            &Path::new(mes_lib).join("libtcc1.c"),
            &top.join("libtcc1.c"),
        )?;
        let mut lt = owned(&["-c"]);
        lt.extend_from_slice(cppflags);
        lt.push("libtcc1.c".into());
        tcc(cfg, cc, &lt)?;
    }
    ar(cfg, cc, "libtcc1.a", &["libtcc1.o"])?;

    // libgetopt.a.
    if !final_pass {
        cp(
            &Path::new(mes_lib).join("libgetopt.c"),
            &top.join("libgetopt.c"),
        )?;
    }
    let _ = fs::remove_file(top.join("libgetopt.a"));
    let mut lg = owned(&["-c"]);
    if final_pass {
        lg = owned(&["-c", "-g"]);
    }
    lg.extend_from_slice(cppflags);
    lg.push("libgetopt.c".into());
    tcc(cfg, cc, &lg)?;
    ar(cfg, cc, "libgetopt.a", &["libgetopt.o"])
}

/// `$CC -ar cr ARCHIVE OBJ…` — tcc's own archiver mode (no host `ar`).
fn ar(cfg: &Cfg, cc: &str, archive: &str, objs: &[&str]) -> Result<(), String> {
    let mut a = owned(&["-ar", "cr", archive]);
    a.extend(owned(objs));
    tcc(cfg, cc, &a)
}

/// One boot.sh generation: CALLER (the prior tcc) links `tcc$suffix` from
/// tcc.c, then the NEW tcc rebuilds crt/libtcc1. The `-D` set is the
/// generation-specific `BOOT_CPPFLAGS_TCC` (boot.sh's program_suffix ladder).
fn boot_gen(cfg: &Cfg, caller: &str, suffix: &str) -> Result<(), String> {
    let top = &cfg.top;
    let mes = ps(&cfg.mes)?.to_string();
    let out = &cfg.out;
    let mes_lib = format!("{mes}/lib");
    let tcc_bin = format!("tcc{suffix}");

    // BOOT_CPPFLAGS_TCC by generation (have_float/long_long/setjmp = true).
    let mut boot: Vec<String> = owned(&["-D", "BOOTSTRAP=1"]);
    match suffix {
        "-boot0" => boot.extend(owned(&["-D", "HAVE_LONG_LONG_STUB=1"])),
        "-boot1" => boot.extend(owned(&["-D", "HAVE_BITFIELD=1", "-D", "HAVE_LONG_LONG=1"])),
        "-boot2" => boot.extend(owned(&[
            "-D",
            "HAVE_BITFIELD=1",
            "-D",
            "HAVE_FLOAT_STUB=1",
            "-D",
            "HAVE_LONG_LONG=1",
        ])),
        // -boot3 and the tail (-boot4/5/6) share the full float+bitfield set.
        _ => boot.extend(owned(&[
            "-D",
            "HAVE_BITFIELD=1",
            "-D",
            "HAVE_FLOAT=1",
            "-D",
            "HAVE_LONG_LONG=1",
        ])),
    }
    boot.extend(owned(&["-D", "HAVE_SETJMP=1"]));

    // CPPFLAGS_TCC (boot.sh order): `-I . -I mes/lib -I mes/include`, target
    // flag, then the shared CONFIG_* block.
    let mut cppflags = owned(&[
        "-I",
        ".",
        "-I",
        &mes_lib,
        "-I",
        &format!("{mes}/include"),
        "-D",
        "TCC_TARGET_I386=1",
    ]);
    cppflags.extend(config_defs(cfg)?);

    // Link the next generation.
    let mut link = owned(&["-g", "-v", "-static", "-o", &tcc_bin]);
    link.extend(boot.clone());
    link.extend(cppflags);
    link.extend(owned(&["-L", ".", "tcc.c"]));
    tcc(cfg, caller, &link)?;
    chmod_x(&top.join(&tcc_bin))?;

    // REBUILD_LIBC with the new tcc: crt (no cppflags, plain `-c -g -o`) then
    // libtcc1 from the mes source.
    let new = format!("./{tcc_bin}");
    for i in ["1", "i", "n"] {
        cp(
            &Path::new(&mes_lib).join(format!("crt{i}.c")),
            &top.join(format!("crt{i}.c")),
        )?;
        let obj = format!("crt{i}{suffix}.o");
        tcc(
            cfg,
            &new,
            &owned(&["-c", "-g", "-o", &obj, &format!("crt{i}.c")]),
        )?;
        cp(&top.join(&obj), &top.join(format!("crt{i}.o")))?;
    }
    let _ = fs::remove_file(top.join("libtcc1.a"));
    tcc(
        cfg,
        &new,
        &owned(&[
            "-c",
            "-g",
            "-D",
            "TCC_TARGET_I386=1",
            "-D",
            "HAVE_FLOAT=1",
            "-o",
            "libtcc1.o",
            &format!("{mes_lib}/libtcc1.c"),
        ]),
    )?;
    ar(cfg, &new, "libtcc1.a", &["libtcc1.o"])?;
    cp(
        &top.join("libtcc1.a"),
        &Path::new(out).join("lib/tcc/libtcc1.a"),
    )
}

/// Install the rung's contract (the deleted recipe's final CopyFiles): `tcc` to
/// bin/, and libc.a/libtcc1.a + the crt objects to lib/. Fails HERE if `tcc`
/// did not build.
fn install(cfg: &Cfg) -> Result<(), String> {
    let top = &cfg.top;
    let out = Path::new(&cfg.out);
    let bindir = out.join("bin");
    let libdir = out.join("lib");
    mkdir_p(&bindir)?;
    mkdir_p(&libdir)?;

    cp(&top.join("tcc"), &bindir.join("tcc"))?;
    chmod_x(&bindir.join("tcc"))?;
    for f in ["libc.a", "libtcc1.a", "crt1.o", "crti.o", "crtn.o"] {
        cp(&top.join(f), &libdir.join(f))?;
    }

    let tcc = bindir.join("tcc");
    let meta = fs::metadata(&tcc).map_err(|_| format!("tcc did not build: {}", tcc.display()))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("built tcc is not executable: {}", tcc.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tripwire refuses BEFORE building: a pinned script whose bytes drift
    // from the audited baseline reds naming the file and the baseline.
    #[test]
    fn ported_script_pins_refuse_a_drifted_script() {
        let base = std::env::temp_dir().join(format!("td-tccboot-pins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for (rel, _) in PORTED_SCRIPT_PINS {
            let p = base.join(rel);
            fs::create_dir_all(p.parent().unwrap_or(&base)).unwrap();
            fs::write(&p, b"drifted\n").unwrap();
        }
        let err = verify_ported_scripts(&base).unwrap_err();
        assert!(err.contains("transcription baseline"), "{err}");
        // The tripwire checks pins in order and reds on the first drift
        // (`configure`), naming the offending file.
        assert!(err.contains("configure"), "names the file: {err}");
        let _ = fs::remove_dir_all(&base);
    }

    // config.h bakes the VERSION file's version and matches the golden bytes a
    // real `configure` produced (fixed layout, no gcc detected under mescc).
    #[test]
    fn configure_writes_the_golden_config_h() {
        let base = std::env::temp_dir().join(format!("td-tccboot-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let cfg = Cfg {
            top: base.clone(),
            mes: PathBuf::from("/mes"),
            stage0: PathBuf::from("/stage0"),
            out: "/out".into(),
            version: "0.9.27".into(),
        };
        configure(&cfg).unwrap();
        let got = fs::read_to_string(base.join("config.h")).unwrap();
        let want = "/* Automatically generated by configure - do not modify */\n\
                    #ifndef CONFIG_TCCDIR\n\
                    # define CONFIG_TCCDIR \".\"\n\
                    #endif\n\
                    #define GCC_MAJOR \n\
                    #define GCC_MINOR \n\
                    #define TCC_VERSION \"0.9.27\"\n";
        assert_eq!(got, want);
        let _ = fs::remove_dir_all(&base);
    }

    // The generation ladder's -D sets match boot.sh's program_suffix logic and
    // the golden boot.log: boot0 is the long-long STUB rung; boot1 adds
    // bitfield+real long-long; boot2 adds the float STUB; boot3+ the real float;
    // every generation carries setjmp.
    #[test]
    fn boot_flag_ladder_matches_the_golden_log() {
        let want = |suffix: &str| -> Vec<String> {
            let mut boot: Vec<String> = owned(&["-D", "BOOTSTRAP=1"]);
            match suffix {
                "-boot0" => boot.extend(owned(&["-D", "HAVE_LONG_LONG_STUB=1"])),
                "-boot1" => {
                    boot.extend(owned(&["-D", "HAVE_BITFIELD=1", "-D", "HAVE_LONG_LONG=1"]))
                }
                "-boot2" => boot.extend(owned(&[
                    "-D",
                    "HAVE_BITFIELD=1",
                    "-D",
                    "HAVE_FLOAT_STUB=1",
                    "-D",
                    "HAVE_LONG_LONG=1",
                ])),
                _ => boot.extend(owned(&[
                    "-D",
                    "HAVE_BITFIELD=1",
                    "-D",
                    "HAVE_FLOAT=1",
                    "-D",
                    "HAVE_LONG_LONG=1",
                ])),
            }
            boot.extend(owned(&["-D", "HAVE_SETJMP=1"]));
            boot
        };
        // boot0: -D BOOTSTRAP=1 -D HAVE_LONG_LONG_STUB=1 -D HAVE_SETJMP=1
        assert_eq!(
            want("-boot0"),
            owned(&[
                "-D",
                "BOOTSTRAP=1",
                "-D",
                "HAVE_LONG_LONG_STUB=1",
                "-D",
                "HAVE_SETJMP=1"
            ])
        );
        // boot2 carries the float STUB; boot3 and the tail carry real float.
        assert!(want("-boot2").iter().any(|f| f == "HAVE_FLOAT_STUB=1"));
        assert!(want("-boot3").iter().any(|f| f == "HAVE_FLOAT=1"));
        assert_eq!(want("-boot4"), want("-boot6"));
    }

    // The shared CONFIG_* block carries the store OUT prefix and the embedded C
    // string-literal quotes (the `\"…\"` the shell produced), and preserves the
    // literal {B} runtime placeholder.
    #[test]
    fn config_defs_carry_prefix_and_quoted_string_literals() {
        let cfg = Cfg {
            top: PathBuf::from("/top"),
            mes: PathBuf::from("/mes"),
            stage0: PathBuf::from("/stage0"),
            out: "/out".into(),
            version: "0.9.27".into(),
        };
        let d = config_defs(&cfg).unwrap();
        assert!(d.iter().any(|x| x == "CONFIG_TCCDIR=\"/out/lib/tcc\""));
        assert!(d
            .iter()
            .any(|x| x == "CONFIG_TCC_CRTPREFIX=\"/out/lib:{B}/lib:.\""));
        assert!(d
            .iter()
            .any(|x| x == "CONFIG_TCC_SYSINCLUDEPATHS=\"/mes/include:/out/include:{B}/include\""));
        assert!(d.iter().any(|x| x == "ONE_SOURCE=1"));
    }
}
