//! Engine-native GNU Mes bootstrap rung (re #469): the Rust port of the mes
//! tarball's `configure.sh` + generated `bootstrap.sh` + `install.sh` under
//! td's fixed rung configuration — x86-linux-mes, mes libc, compiler mescc,
//! bootstrap mode, in-tree build (`srcdir=.`, `srcdest=""`), exactly what the
//! deleted shell-driven recipe ran with `--host=i686-linux-gnu` and CC unset.
//!
//! Why a port and not the scripts: the scripts' only host needs were a shell
//! and coreutils/sed for ORCHESTRATION (template subst, object-name mangling,
//! `cat`-built archives, tree copies). The compilation itself is done by
//! recipe outputs: stage0's kaem drives upstream's own `kaem.run` (M2-Planet →
//! blood-elf → M1 → hex2 → `bin/mes-m2`), and the mescc phases run upstream's
//! `scripts/mescc.scm` under the just-built mes. This module does the
//! orchestration in std::fs and spawns ONLY those recipe-built binaries, so
//! the rung declares no host tool at all.
//!
//! The x86 (32-bit) target is the chain's: mes feeds MesCC-built x86 archives
//! (`lib/x86-mes/libc+tcc.a`) to the tcc rung, mirroring guix's mes-boot.
//!
//! Fixed-config fidelity: the `*_SOURCES` lists are `build-aux/
//! configure-lib.sh`'s, evaluated for (libc=mes, kernel=linux, cpu=x86) and
//! parameterized by compiler exactly where the script parameterizes
//! (`$mes_cpu-mes-$compiler`); a pin bump that changes the lists reds here on
//! the missing file, named. `@BASH@`/`@SHELL@` substitute to `/bin/sh` — the
//! installed `bin/mescc`/`bin/mesar` wrappers are DATA in this rung's output
//! (consumers exec them through their own declared shell; the tcc rung
//! patches the shebang into its tool farm), never a host store path.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const MES_CPU: &str = "x86";
const CC_CPU: &str = "i386";
const STAGE0_CPU: &str = "x86";
const MES_KERNEL: &str = "linux";
const MES_BITS: &str = "32";
const MES_LIBC: &str = "mes";
const MES_SYSTEM: &str = "x86-linux-mes";
const HOST: &str = "i686-linux-gnu";
const GUILE_EFFECTIVE_VERSION: &str = "2.2";
// The neutral shebang for installed wrapper scripts (see module doc).
const SHELL: &str = "/bin/sh";
// The recipe's mes interpreter limits (the deleted recipe's exact env).
const MES_ARENA: &str = "100000000";
const MES_STACK: &str = "8000000";

fn s_vec(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_string()).collect()
}

// ---- build-aux/configure-lib.sh, evaluated for (mes, linux, x86) ----------

fn libc_mini_shared_sources(compiler: &str) -> Vec<String> {
    let mut v = s_vec(&[
        "lib/mes/__init_io.c",
        "lib/mes/eputs.c",
        "lib/mes/oputs.c",
        // mes_libc = mes:
        "lib/mes/globals.c",
        "lib/stdlib/exit.c",
    ]);
    v.push(format!("lib/linux/x86-mes-{compiler}/_exit.c"));
    v.push(format!("lib/linux/x86-mes-{compiler}/_write.c"));
    v.extend(s_vec(&["lib/stdlib/puts.c", "lib/string/strlen.c"]));
    v
}

fn libc_mini_sources(compiler: &str) -> Vec<String> {
    let mut v = libc_mini_shared_sources(compiler);
    v.push("lib/mes/write.c".to_string());
    v
}

fn libmescc_sources(compiler: &str) -> Vec<String> {
    vec![
        "lib/mes/globals.c".to_string(),
        format!("lib/linux/x86-mes-{compiler}/syscall-internal.c"),
    ]
}

fn libmes_sources(compiler: &str) -> Vec<String> {
    let mut v = libc_mini_shared_sources(compiler);
    v.extend(s_vec(&[
        "lib/ctype/isnumber.c",
        "lib/mes/abtol.c",
        "lib/mes/cast.c",
        "lib/mes/eputc.c",
        "lib/mes/fdgetc.c",
        "lib/mes/fdputc.c",
        "lib/mes/fdputs.c",
        "lib/mes/fdungetc.c",
        "lib/mes/itoa.c",
        "lib/mes/ltoa.c",
        "lib/mes/ltoab.c",
        "lib/mes/mes_open.c",
        "lib/mes/ntoab.c",
        "lib/mes/oputc.c",
        "lib/mes/ultoa.c",
        "lib/mes/utoa.c",
        "lib/stub/__raise.c",
        // mes_libc = mes:
        "lib/ctype/isdigit.c",
        "lib/ctype/isspace.c",
        "lib/ctype/isxdigit.c",
        "lib/mes/assert_msg.c",
        "lib/posix/write.c",
        "lib/stdlib/atoi.c",
        // mes_kernel = linux:
        "lib/linux/lseek.c",
    ]));
    v
}

fn libc_sources(compiler: &str) -> Vec<String> {
    let mut v = libmes_sources(compiler);
    v.extend(s_vec(&[
        "lib/dirent/__getdirentries.c",
        "lib/dirent/closedir.c",
        "lib/dirent/opendir.c",
        "lib/mes/__assert_fail.c",
        "lib/mes/__buffered_read.c",
        "lib/mes/__mes_debug.c",
        "lib/posix/execv.c",
        "lib/posix/getcwd.c",
        "lib/posix/getenv.c",
        "lib/posix/isatty.c",
        "lib/posix/open.c",
        "lib/posix/buffered-read.c",
        "lib/posix/setenv.c",
        "lib/posix/wait.c",
        "lib/stdio/fgetc.c",
        "lib/stdio/fputc.c",
        "lib/stdio/fputs.c",
        "lib/stdio/getc.c",
        "lib/stdio/getchar.c",
        "lib/stdio/putc.c",
        "lib/stdio/putchar.c",
        "lib/stdio/ungetc.c",
        "lib/stdlib/calloc.c",
        "lib/stdlib/free.c",
        "lib/stdlib/realloc.c",
        "lib/string/memchr.c",
        "lib/string/memcmp.c",
        "lib/string/memcpy.c",
        "lib/string/memmove.c",
        "lib/string/memset.c",
        "lib/string/strcmp.c",
        "lib/string/strcpy.c",
        "lib/string/strncmp.c",
        "lib/posix/raise.c",
        // mes_kernel = linux:
        "lib/linux/access.c",
        "lib/linux/brk.c",
        "lib/linux/chdir.c",
        "lib/linux/chmod.c",
        "lib/linux/clock_gettime.c",
        "lib/linux/close.c",
        "lib/linux/dup.c",
        "lib/linux/dup2.c",
        "lib/linux/execve.c",
        "lib/linux/fcntl.c",
        "lib/linux/fork.c",
        "lib/linux/fstat.c",
        "lib/linux/fsync.c",
        "lib/linux/_getcwd.c",
        "lib/linux/getdents.c",
        "lib/linux/gettimeofday.c",
        "lib/linux/ioctl3.c",
        "lib/linux/link.c",
        "lib/linux/lstat.c",
        "lib/linux/_open3.c",
        "lib/linux/malloc.c",
        "lib/linux/mkdir.c",
        "lib/linux/nanosleep.c",
        "lib/linux/pipe.c",
        "lib/linux/_read.c",
        "lib/linux/readdir.c",
        "lib/linux/rename.c",
        "lib/linux/rmdir.c",
        "lib/linux/stat.c",
        "lib/linux/symlink.c",
        "lib/linux/time.c",
        "lib/linux/umask.c",
        "lib/linux/uname.c",
        "lib/linux/unlink.c",
        "lib/linux/utimensat.c",
        "lib/linux/wait4.c",
        "lib/linux/waitpid.c",
    ]));
    v.push(format!("lib/linux/x86-mes-{compiler}/syscall.c"));
    v.extend(s_vec(&["lib/linux/getpid.c", "lib/linux/kill.c"]));
    v
}

fn libc_tcc_sources(compiler: &str) -> Vec<String> {
    let mut v = libc_sources(compiler);
    v.extend(s_vec(&[
        "lib/ctype/islower.c",
        "lib/ctype/isupper.c",
        "lib/ctype/tolower.c",
        "lib/ctype/toupper.c",
        "lib/mes/abtod.c",
        "lib/mes/dtoab.c",
        "lib/mes/search-path.c",
        "lib/posix/execvp.c",
        "lib/stdio/fclose.c",
        "lib/stdio/fdopen.c",
        "lib/stdio/ferror.c",
        "lib/stdio/fflush.c",
        "lib/stdio/fopen.c",
        "lib/stdio/fprintf.c",
        "lib/stdio/fread.c",
        "lib/stdio/fseek.c",
        "lib/stdio/ftell.c",
        "lib/stdio/fwrite.c",
        "lib/stdio/printf.c",
        "lib/stdio/remove.c",
        "lib/stdio/snprintf.c",
        "lib/stdio/sprintf.c",
        "lib/stdio/sscanf.c",
        "lib/stdio/vfprintf.c",
        "lib/stdio/vprintf.c",
        "lib/stdio/vsnprintf.c",
        "lib/stdio/vsprintf.c",
        "lib/stdio/vsscanf.c",
        "lib/stdlib/qsort.c",
        "lib/stdlib/strtod.c",
        "lib/stdlib/strtof.c",
        "lib/stdlib/strtol.c",
        "lib/stdlib/strtold.c",
        "lib/stdlib/strtoll.c",
        "lib/stdlib/strtoul.c",
        "lib/stdlib/strtoull.c",
        "lib/string/memmem.c",
        "lib/string/strcat.c",
        "lib/string/strchr.c",
        "lib/string/strlwr.c",
        "lib/string/strncpy.c",
        "lib/string/strrchr.c",
        "lib/string/strstr.c",
        "lib/string/strupr.c",
        "lib/stub/sigaction.c",
        "lib/stub/ldexp.c",
        "lib/stub/mprotect.c",
        "lib/stub/localtime.c",
        "lib/stub/putenv.c",
        "lib/stub/realpath.c",
        "lib/stub/sigemptyset.c",
    ]));
    v.push(format!("lib/x86-mes-{compiler}/setjmp.c"));
    v
}

fn libc_gnu_sources(compiler: &str) -> Vec<String> {
    let mut v = libc_tcc_sources(compiler);
    v.extend(s_vec(&[
        "lib/ctype/isalnum.c",
        "lib/ctype/isalpha.c",
        "lib/ctype/isascii.c",
        "lib/ctype/iscntrl.c",
        "lib/ctype/isgraph.c",
        "lib/ctype/isprint.c",
        "lib/ctype/ispunct.c",
        "lib/math/ceil.c",
        "lib/math/fabs.c",
        "lib/math/floor.c",
        "lib/mes/fdgets.c",
        "lib/posix/alarm.c",
        "lib/posix/execl.c",
        "lib/posix/execlp.c",
        "lib/posix/mktemp.c",
        "lib/posix/pathconf.c",
        "lib/posix/sbrk.c",
        "lib/posix/sleep.c",
        "lib/posix/unsetenv.c",
        "lib/stdio/clearerr.c",
        "lib/stdio/feof.c",
        "lib/stdio/fgets.c",
        "lib/stdio/fileno.c",
        "lib/stdio/freopen.c",
        "lib/stdio/fscanf.c",
        "lib/stdio/perror.c",
        "lib/stdio/vfscanf.c",
        "lib/stdlib/__exit.c",
        "lib/stdlib/abort.c",
        "lib/stdlib/abs.c",
        "lib/stdlib/alloca.c",
        "lib/stdlib/atexit.c",
        "lib/stdlib/atof.c",
        "lib/stdlib/atol.c",
        "lib/stdlib/mbstowcs.c",
        "lib/string/bcmp.c",
        "lib/string/bcopy.c",
        "lib/string/bzero.c",
        "lib/string/index.c",
        "lib/string/rindex.c",
        "lib/string/strcspn.c",
        "lib/string/strdup.c",
        "lib/string/strerror.c",
        "lib/string/strncat.c",
        "lib/string/strpbrk.c",
        "lib/string/strspn.c",
        "lib/stub/__cleanup.c",
        "lib/stub/atan2.c",
        "lib/stub/bsearch.c",
        "lib/stub/chown.c",
        "lib/stub/cos.c",
        "lib/stub/ctime.c",
        "lib/stub/exp.c",
        "lib/stub/fpurge.c",
        "lib/stub/freadahead.c",
        "lib/stub/frexp.c",
        "lib/stub/getgrgid.c",
        "lib/stub/getgrnam.c",
        "lib/stub/getlogin.c",
        "lib/stub/getpgid.c",
        "lib/stub/getpgrp.c",
        "lib/stub/getpwnam.c",
        "lib/stub/getpwuid.c",
        "lib/stub/gmtime.c",
        "lib/stub/log.c",
        "lib/stub/mktime.c",
        "lib/stub/modf.c",
        "lib/stub/pclose.c",
        "lib/stub/popen.c",
        "lib/stub/pow.c",
        "lib/stub/rand.c",
        "lib/stub/rewind.c",
        "lib/stub/setbuf.c",
        "lib/stub/setgrent.c",
        "lib/stub/setlocale.c",
        "lib/stub/setvbuf.c",
        "lib/stub/sigaddset.c",
        "lib/stub/sigblock.c",
        "lib/stub/sigdelset.c",
        "lib/stub/sigsetmask.c",
        "lib/stub/sin.c",
        "lib/stub/sqrt.c",
        "lib/stub/strftime.c",
        "lib/stub/sys_siglist.c",
        "lib/stub/system.c",
        "lib/stub/times.c",
        "lib/stub/ttyname.c",
        "lib/stub/utime.c",
        // mes_kernel = linux:
        "lib/linux/getegid.c",
        "lib/linux/geteuid.c",
        "lib/linux/getgid.c",
        "lib/linux/getppid.c",
        "lib/linux/getrusage.c",
        "lib/linux/getuid.c",
        "lib/linux/ioctl.c",
        "lib/linux/mknod.c",
        "lib/linux/readlink.c",
        "lib/linux/setgid.c",
        "lib/linux/settimer.c",
        "lib/linux/setuid.c",
        "lib/linux/signal.c",
        "lib/linux/sigprogmask.c",
    ]));
    v
}

fn libtcc1_sources() -> Vec<String> {
    s_vec(&["lib/libtcc1.c"])
}

fn mes_sources() -> Vec<String> {
    s_vec(&[
        "src/builtins.c",
        "src/cc.c",
        "src/core.c",
        "src/display.c",
        "src/eval-apply.c",
        "src/gc.c",
        "src/globals.c",
        "src/hash.c",
        "src/lib.c",
        "src/math.c",
        "src/mes.c",
        "src/module.c",
        "src/posix.c",
        "src/reader.c",
        "src/stack.c",
        "src/string.c",
        "src/struct.c",
        "src/symbol.c",
        "src/variable.c",
        "src/vector.c",
    ])
}

// ---- shared plumbing ------------------------------------------------------

struct Cfg {
    /// The unpacked mes tree — srcdir, builddir, and MES_PREFIX all at once
    /// (the in-tree build the deleted recipe ran).
    top: PathBuf,
    /// The unpacked nyacc tree (mescc's C parser rides GUILE_LOAD_PATH).
    nyacc: PathBuf,
    /// The staged stage0 rung output (kaem/M2-Planet/M1/hex2/blood-elf + the
    /// mescc-tools-extra file tools, all under AMD64/bin).
    stage0: PathBuf,
    out: String,
    version: String,
}

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

fn chmod_x(p: &Path) -> Result<(), String> {
    fs::set_permissions(p, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", p.display()))
}

fn cp(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))
}

/// `sed -re s,^[.]+/,, -e s,/,-,g -e s,[.]c$,,` + `.o` — bootstrap.sh's
/// object-name mangling.
fn obj_name(c: &str) -> String {
    let c = c.trim_start_matches("./");
    let stem = c.strip_suffix(".c").unwrap_or(c);
    format!("{}.o", stem.replace('/', "-"))
}

/// configure.sh's `subst`: replace each `@KEY@` with its value. Keys absent
/// from the map stay literal, exactly like a sed expression list. (This is
/// replace-ALL where sed's unflagged `s,,,` replaces the first match per
/// line — no pinned template repeats a key on a line, and the template pins
/// red before the semantics could ever diverge.)
fn subst(template: &Path, dest: &Path, map: &[(String, String)]) -> Result<(), String> {
    let body = String::from_utf8(read(template)?)
        .map_err(|_| format!("template {} is not UTF-8", template.display()))?;
    let mut out = body;
    for (k, v) in map {
        out = out.replace(&format!("@{k}@"), v);
    }
    write(dest, out.as_bytes())?;
    chmod_x(dest)
}

// ---- the rung -------------------------------------------------------------

/// The upstream scripts this port TRANSCRIBES, pinned to the audited bytes
/// (round-9 review): the port hardcodes what these files say — the
/// `*_SOURCES` lists and their order, the flag blocks, the install tree, the
/// `@VAR@` template keys, kaem.run's env contract and its final
/// exec-bit-dropping `cp`. A REMOVED source already reds on the missing file,
/// but an ADDED or REORDERED entry, a new flag, or a new template key would
/// otherwise build silently against the stale transcription. So a source-pin
/// bump that changes any of these refuses here, before anything compiles;
/// re-audit the port against the new script, then update its pin.
const PORTED_SCRIPT_PINS: [(&str, &str); 8] = [
    (
        "configure.sh",
        "3f79a202ed711a1247eacded9ec31fc22d4e3b33bf6bb0e4ac2a318c8a89e6ae",
    ),
    (
        "kaem.run",
        "1de53bcdcadeb0555b79047b26cb70357c75079de848ad503f9d38631b8a440d",
    ),
    (
        "build-aux/configure-lib.sh",
        "3257f4f32f4314047475d17bc609e04b19fc2fbc89a600c169d083fb93f5bb2a",
    ),
    (
        "build-aux/bootstrap.sh.in",
        "0f508b40cb362cb3518cf3d8856cbaa30b5dc1667b8f429244aa3099dd16909f",
    ),
    (
        "build-aux/install.sh.in",
        "a3937a84783ad599d92846d3119efa0f21bffb1d5e76391bbeeefa16eaa11490",
    ),
    (
        "scripts/mescc.in",
        "0a1fc4e29aeed3dd4a2a7e025d3db717f522995e25e4cf46ff0f7ea68efa1bb1",
    ),
    (
        "scripts/mesar.in",
        "d0bae4ad0f74fd41278df15177951ea2ba5a9e64e7c0bb01c3a8ef79d13ef2fe",
    ),
    (
        "scripts/mescc.scm.in",
        "4eb5b4a139e940c2c0dc8bd1346cad58daeef7efb0e7e0eed2be3debc5b2b35f",
    ),
];

fn verify_ported_scripts(top: &Path) -> Result<(), String> {
    for (rel, want) in PORTED_SCRIPT_PINS {
        let p = top.join(rel);
        let got =
            crate::sha256::sha256_file(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
        if got != want {
            return Err(format!(
                "mes tarball script {rel} hashes {got}, not the transcription baseline \
                 {want}: mes_boot transcribes this script's contents, so a changed script \
                 means the port must be re-audited (and its pin updated) before building"
            ));
        }
    }
    Ok(())
}

/// The running kernel's ability to execute 32-bit (i386/x86) ELF binaries.
/// The mes rung's first artifact, `bin/mes-m2`, is x86/ELFCLASS32 (mes is a
/// 32-bit interpreter, `--host=i686-linux-gnu`), as is the whole i686 mesboot
/// toolchain up to the x86-64 cross transition — so the build kernel needs
/// Linux `CONFIG_IA32_EMULATION`.
enum Ia32Verdict {
    /// The kernel config sets `CONFIG_IA32_EMULATION=y` (or an older-kernel
    /// equivalent) — 32-bit ELF execution is compiled in.
    Supported,
    /// The kernel config explicitly disables it (`# … is not set` / `=n`) — a
    /// definitive NO; the i686 chain cannot execute here.
    Unsupported,
    /// No readable kernel config — capability cannot be PROVEN without an exec
    /// probe, so the caller must not hard-fail on this.
    Unknown,
}

/// Classify a kernel `.config` text (the `/proc/config.gz` or
/// `/boot/config-<release>` body). Split out from the file reads so the
/// three verdicts unit-test without a kernel: `CONFIG_IA32_EMULATION=y` (or
/// the `CONFIG_COMPAT_32`/`CONFIG_X86_32` equivalents) → Supported; an
/// explicit not-set/`=n` → Unsupported; neither present → Unknown. (A kernel
/// built `=y` but booted `ia32_emulation=0` reads Supported here — a rare
/// runtime override the config alone can't see; that path still ENOEXECs at
/// the exec below, just with a less specific message.)
fn ia32_from_kconfig(text: &str) -> Ia32Verdict {
    if ["CONFIG_IA32_EMULATION=y", "CONFIG_COMPAT_32=y", "CONFIG_X86_32=y"]
        .iter()
        .any(|k| text.contains(k))
    {
        return Ia32Verdict::Supported;
    }
    if text.contains("# CONFIG_IA32_EMULATION is not set")
        || text.contains("CONFIG_IA32_EMULATION=n")
    {
        return Ia32Verdict::Unsupported;
    }
    Ia32Verdict::Unknown
}

/// Read the running kernel's config (pure std, no syscall) and classify its
/// 32-bit-ELF support: `/proc/config.gz` first (the definitive live signal,
/// needs `CONFIG_IKCONFIG_PROC`), then `/boot/config-<osrelease>` as a
/// fallback. A read/decompress error or an inconclusive body yields Unknown —
/// never a false negative.
fn detect_ia32() -> Ia32Verdict {
    // /proc/config.gz reports size 0 in procfs, so read-to-EOF then decompress
    // (the engine's own gzip reader — no host `zcat`).
    if let Ok(gz) = std::fs::read("/proc/config.gz") {
        if let Ok(raw) = crate::gzip::decompress_bytes(&gz) {
            if let Ok(text) = String::from_utf8(raw) {
                match ia32_from_kconfig(&text) {
                    Ia32Verdict::Unknown => {}
                    v => return v,
                }
            }
        }
    }
    if let Ok(rel) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let rel = rel.trim();
        if !rel.is_empty() {
            if let Ok(text) = std::fs::read_to_string(format!("/boot/config-{rel}")) {
                return ia32_from_kconfig(&text);
            }
        }
    }
    Ia32Verdict::Unknown
}

/// Fail fast, with actionable guidance, if the build kernel PROVABLY cannot run
/// 32-bit ELF binaries — so the operator gets a named cause here instead of a
/// bare ENOEXEC deep in the kaem run. Only an EXPLICIT negative reds; an
/// unreadable/absent kernel config passes through (the exec itself is the
/// backstop) so a locked-down `/proc` never manufactures a false failure.
fn preflight_ia32() -> Result<(), String> {
    ia32_preflight_result(detect_ia32())
}

/// The preflight POLICY, factored out of the live-kernel probe so it is pure
/// (no `/proc` read) and unit-testable on any host: only a PROVEN negative
/// reds; Supported and Unknown both pass (the exec is the backstop for the
/// unprovable case).
fn ia32_preflight_result(verdict: Ia32Verdict) -> Result<(), String> {
    match verdict {
        Ia32Verdict::Unsupported => Err(
            "the build kernel cannot execute 32-bit ELF binaries \
             (CONFIG_IA32_EMULATION is not set): the mes rung builds and runs bin/mes-m2, an \
             i686/ELFCLASS32 binary, and every i686 mesboot rung up to the x86-64 cross \
             transition the same — none can execute on this kernel. Build on a kernel with \
             CONFIG_IA32_EMULATION=y (and without the ia32_emulation=0 boot parameter)."
                .to_string(),
        ),
        // Supported, or Unknown (unproven — do not hard-fail; the exec is the backstop).
        Ia32Verdict::Supported | Ia32Verdict::Unknown => Ok(()),
    }
}

/// Build + install the mes rung: SOURCE/NYACC are the staged tarballs, STAGE0
/// the staged stage0 output, OUT the store output dir. Runs in the sandbox
/// cwd ({root}): the mes tree unpacks to `{root}/mes-src`, nyacc to
/// `{root}/nyacc` (the recipe's guile-site CopyTree reads it there).
pub(crate) fn run(source: &str, nyacc: &str, stage0: &str, out: &str) -> Result<(), String> {
    // The first i686/ELFCLASS32 execution in the whole toolchain is this rung's
    // bin/mes-m2 — red early with a named cause if the kernel provably lacks
    // 32-bit ELF support, rather than ENOEXEC mid-kaem (re #469 bootstrap
    // robustness).
    preflight_ia32()?;
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let top = cwd.join("mes-src");
    crate::tar::unpack_archive(Path::new(source), &top, false)?;
    let nyacc_dir = cwd.join("nyacc");
    crate::tar::unpack_archive(Path::new(nyacc), &nyacc_dir, false)?;

    verify_ported_scripts(&top)?;
    let version = read_configure_version(&top.join("configure.sh"))?;
    let cfg = Cfg {
        top,
        nyacc: nyacc_dir,
        stage0: PathBuf::from(stage0),
        out: out.to_string(),
        version,
    };

    configure(&cfg)?;
    kaem_phase(&cfg)?;
    mescc_lib_phase(&cfg)?;
    mes_link_phase(&cfg)?;
    gcc_source_lib_phase(&cfg)?;
    install_phase(&cfg)
}

/// `VERSION=x.y.z` from configure.sh — the tarball states its own version.
fn read_configure_version(configure: &Path) -> Result<String, String> {
    let body = String::from_utf8(read(configure)?)
        .map_err(|_| format!("{} is not UTF-8", configure.display()))?;
    body.lines()
        .find_map(|l| l.strip_prefix("VERSION="))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("no VERSION= line in {}", configure.display()))
}

/// configure.sh's work under the rung's fixed flags (`--prefix=$out
/// --host=i686-linux-gnu`, CC unset → compiler=mescc, GUILE=true →
/// effective version 2.2): the generated `include/mes/config.h`, the three
/// arch headers, and the @VAR@ substitution of the INSTALLED scripts
/// (mescc, mescc.scm, mesar). The other templates (GNUmakefile, config.sh,
/// bootstrap.sh, …) exist only to orchestrate — this module IS that
/// orchestration, so they are not generated.
fn configure(cfg: &Cfg) -> Result<(), String> {
    let out = &cfg.out;
    let top = ps(&cfg.top)?.to_string();
    let site = format!("{out}/share/guile/site/{GUILE_EFFECTIVE_VERSION}");
    let ccache = format!("{out}/lib/guile/{GUILE_EFFECTIVE_VERSION}/site-ccache");
    let pairs: Vec<(String, String)> = [
        ("VERSION", cfg.version.as_str()),
        ("PACKAGE", "mes"),
        ("PACKAGE_NAME", "GNU Mes"),
        ("PACKAGE_BUGREPORT", "bug-mes@gnu.org"),
        ("bootstrap", "true"),
        ("build", HOST),
        ("host", HOST),
        ("compiler", "mescc"),
        ("courageous", "false"),
        ("mes_bits", MES_BITS),
        ("mes_kernel", MES_KERNEL),
        ("mes_cpu", MES_CPU),
        ("mes_libc", MES_LIBC),
        ("mes_system", MES_SYSTEM),
        ("abs_top_srcdir", top.as_str()),
        ("abs_top_builddir", top.as_str()),
        ("top_builddir", "."),
        ("srcdest", ""),
        ("srcdir", "."),
        ("prefix", out.as_str()),
        ("program_prefix", ""),
        ("GUILE_EFFECTIVE_VERSION", GUILE_EFFECTIVE_VERSION),
        ("GUILE_LOAD_PATH", ""),
        ("guile_site_dir", site.as_str()),
        ("guile_site_ccache_dir", ccache.as_str()),
        ("V", ""),
        ("BASH", SHELL),
        ("SHELL", SHELL),
        ("GUILD", "true"),
        ("GUILE", "true"),
        ("MES_FOR_BUILD", "mes"),
        ("GIT", ""),
        ("PERL", ""),
        ("CFLAGS", ""),
        ("CPPFLAGS", ""),
        ("LDFLAGS", ""),
        ("HEX2FLAGS", ""),
        ("M1FLAGS", ""),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .chain(
        [
            ("bindir", format!("{out}/bin")),
            ("datadir", format!("{out}/share")),
            ("docdir", format!("{out}/share/doc/mes")),
            ("infodir", format!("{out}/share/info")),
            ("includedir", format!("{out}/include")),
            ("libdir", format!("{out}/lib")),
            ("pkgdatadir", format!("{out}/share/mes")),
            ("mandir", format!("{out}/share/man")),
            ("AR", format!("{top}/pre-inst-env mesar")),
            ("CC", format!("{top}/pre-inst-env mescc")),
            ("DIFF", format!("{top}/pre-inst-env diff.scm")),
            ("BLOOD_ELF", ps(&cfg.stage0)?.to_string() + "/AMD64/artifact/blood-elf-0"),
            ("HEX2", ps(&cfg.stage0)?.to_string() + "/AMD64/bin/hex2"),
            ("M1", ps(&cfg.stage0)?.to_string() + "/AMD64/bin/M1"),
            ("M2_PLANET", ps(&cfg.stage0)?.to_string() + "/AMD64/bin/M2-Planet"),
            ("KAEM", ps(&cfg.stage0)?.to_string() + "/AMD64/bin/kaem"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v)),
    )
    .collect();

    for script in ["mescc", "mescc.scm", "mesar"] {
        let template = cfg.top.join("scripts").join(format!("{script}.in"));
        let dest = cfg.top.join("scripts").join(script);
        subst(&template, &dest, &pairs)?;
    }

    write(
        &cfg.top.join("include/mes/config.h"),
        format!("#undef SYSTEM_LIBC\n#define MES_VERSION \"{}\"\n", cfg.version).as_bytes(),
    )?;
    for h in ["kernel-stat.h", "signal.h", "syscall.h"] {
        cp(
            &cfg.top.join(format!("include/{MES_KERNEL}/{MES_CPU}/{h}")),
            &cfg.top.join(format!("include/arch/{h}")),
        )?;
    }
    Ok(())
}

/// bootstrap.sh phase 1: upstream's own `kaem.run` under stage0's kaem —
/// M2-Planet → blood-elf → M1 → hex2 → `bin/mes-m2` (x86, base 0x1000000),
/// the in-script `mes-m2 -c '(display …)'` self-test, and `cp bin/mes-m2
/// bin/mes`. PATH is stage0's bin (kaem resolves M2-Planet/M1/hex2/blood-elf
/// and the mkdir/cp file tools there); the cpu vars pin the x86 target the
/// script otherwise defaults. Note the deleted recipe's tool farm linked
/// M2-Planet/blood-elf to stage0's DISTRIBUTED `artifact/{M2,blood-elf-0}`;
/// PATH here resolves them to the `bin/` copies stage0's own kaem run REBUILT
/// from source — stronger provenance, and the resulting mes-m2 is
/// byte-identical to the bash ladder's (mescc keeps `artifact/blood-elf-0`,
/// matching what the deleted recipe's configure detected — see `mescc()`).
fn kaem_phase(cfg: &Cfg) -> Result<(), String> {
    let stage0_bin = cfg.stage0.join("AMD64/bin");
    let envs: Vec<(String, String)> = vec![
        ("PATH".to_string(), ps(&stage0_bin)?.to_string()),
        ("mes_cpu".to_string(), MES_CPU.to_string()),
        ("cc_cpu".to_string(), CC_CPU.to_string()),
        ("stage0_cpu".to_string(), STAGE0_CPU.to_string()),
        ("blood_elf_flag".to_string(), "--little-endian".to_string()),
        ("srcdest".to_string(), String::new()),
        ("GUILE_LOAD_PATH".to_string(), String::new()),
    ];
    let kaem = cfg.stage0.join("AMD64/bin/kaem");
    crate::build::run_cmd_phase(
        ps(&kaem)?,
        &["--verbose", "--strict", "-f", "kaem.run"],
        ps(&cfg.top)?,
        &envs,
    )?;
    // kaem.run's final `cp bin/mes-m2 bin/mes` uses stage0's cp, which creates
    // the copy 0600 (the bash ladder had coreutils cp first on PATH, which
    // carried mes-m2's exec bit over) — re-grant it or the mescc phase can't
    // spawn bin/mes.
    chmod_x(&cfg.top.join("bin/mes"))
}

/// One mescc invocation, exactly as `pre-inst-env mescc` composes it: the
/// just-built mes runs upstream's `scripts/mescc.scm`; M1/HEX2/BLOOD_ELF are
/// stage0's assembler/linker; MES_PREFIX/GUILE_LOAD_PATH make the in-tree
/// modules (+ nyacc, mescc's C parser) resolvable.
fn mescc(cfg: &Cfg, dir: &Path, args: &[String]) -> Result<(), String> {
    let out = &cfg.out;
    let top = ps(&cfg.top)?;
    let nyacc = ps(&cfg.nyacc)?;
    let stage0 = ps(&cfg.stage0)?;
    let envs: Vec<(String, String)> = vec![
        ("MES_ARENA".to_string(), MES_ARENA.to_string()),
        ("MES_MAX_ARENA".to_string(), MES_ARENA.to_string()),
        ("MES_STACK".to_string(), MES_STACK.to_string()),
        ("MES_PREFIX".to_string(), top.to_string()),
        ("MES_UNINSTALLED".to_string(), "1".to_string()),
        ("includedir".to_string(), format!("{top}/include")),
        ("libdir".to_string(), format!("{top}/lib")),
        (
            "GUILE_LOAD_PATH".to_string(),
            format!(
                "{top}/module:{top}/mes:{top}/guix:{nyacc}/module:{top}/mes/module:{top}/module"
            ),
        ),
        (
            "GUILE_LOAD_COMPILED_PATH".to_string(),
            format!("{top}/scripts:{top}/module"),
        ),
        ("MES".to_string(), format!("{top}/bin/mes")),
        ("M1".to_string(), format!("{stage0}/AMD64/bin/M1")),
        ("HEX2".to_string(), format!("{stage0}/AMD64/bin/hex2")),
        (
            "BLOOD_ELF".to_string(),
            format!("{stage0}/AMD64/artifact/blood-elf-0"),
        ),
        ("PATH".to_string(), format!("{stage0}/AMD64/bin")),
        ("LANG".to_string(), String::new()),
        ("LC_ALL".to_string(), String::new()),
    ];
    let mes = format!("{top}/bin/mes");
    let mescc_scm = format!("{top}/scripts/mescc.scm");
    let site = format!("{out}/share/guile/site/{GUILE_EFFECTIVE_VERSION}");
    let ccache = format!("{out}/lib/guile/{GUILE_EFFECTIVE_VERSION}/site-ccache");
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
    crate::build::run_cmd_phase(&mes, &argv, ps(dir)?, &envs)
}

/// bootstrap.sh's mescc-lib flag block (cwd `mescc-lib`, `srcdest=../`):
/// `$AM_CPPFLAGS $CPPFLAGS $AM_CFLAGS $CFLAGS` word-split in that order.
fn mescc_lib_flags() -> Vec<String> {
    s_vec(&[
        "-D", "HAVE_CONFIG_H=1", "-I", "../include", "-I", "../include", "-I", "include",
        "-D", "HAVE_CONFIG_H=1", "-I", "include", "-L", "../lib",
    ])
}

/// scripts/mesar's whole job, in fs: the `.a` is the concatenation of the
/// member `.o` files (mescc objects are text), and the sibling `.s` archive
/// is the concatenation of the members' `.s` companions.
fn mesar(dir: &Path, archive: &str, objects: &[String]) -> Result<(), String> {
    let arch_path = dir.join(archive);
    let m1_path = dir.join(format!(
        "{}.s",
        archive.strip_suffix(".a").unwrap_or(archive)
    ));
    let mut a: Vec<u8> = Vec::new();
    let mut s: Vec<u8> = Vec::new();
    for o in objects {
        a.extend_from_slice(&read(&dir.join(o))?);
        let os = format!("{}.s", o.strip_suffix(".o").unwrap_or(o));
        s.extend_from_slice(&read(&dir.join(os))?);
    }
    write(&m1_path, &s)?;
    write(&arch_path, &a)
}

/// Compile every source in LIST (mescc -c, bootstrap.sh's loop) and archive
/// the objects as ARCHIVE under `x86-mes/`.
fn compile_archive(cfg: &Cfg, dir: &Path, list: &[String], archive: &str) -> Result<(), String> {
    let mut objects = Vec::with_capacity(list.len());
    for c in list {
        let src = format!("../{c}");
        if !dir.join(&src).is_file() {
            return Err(format!(
                "mes bootstrap: configure-lib source {c} is missing from the tarball \
                 (pin bump changed build-aux/configure-lib.sh? update mes_boot.rs's lists)"
            ));
        }
        let o = obj_name(c);
        let mut args = vec!["-c".to_string()];
        args.extend(mescc_lib_flags());
        args.extend(["-o".to_string(), o.clone(), src]);
        mescc(cfg, dir, &args)?;
        objects.push(o);
    }
    mesar(dir, &format!("{MES_CPU}-mes/{archive}"), &objects)
}

/// bootstrap.sh phase 2 (cwd `mescc-lib`): crt1 + the four MesCC archives —
/// libc-mini.a, libmescc.a, libc.a, and libc+tcc.a (the tcc rung's input).
fn mescc_lib_phase(cfg: &Cfg) -> Result<(), String> {
    let lib = cfg.top.join("mescc-lib");
    fs::create_dir_all(lib.join(format!("{MES_CPU}-mes")))
        .map_err(|e| format!("mkdir mescc-lib: {e}"))?;
    for link in ["mes", "module", "src"] {
        let l = lib.join(link);
        let _ = fs::remove_file(&l);
        std::os::unix::fs::symlink(format!("../{link}"), &l)
            .map_err(|e| format!("symlink mescc-lib/{link}: {e}"))?;
    }

    cp(
        &cfg.top.join(format!("lib/linux/{MES_CPU}-mes-mescc/crt1.c")),
        &lib.join("crt1.c"),
    )?;
    let mut crt_args = vec!["-c".to_string()];
    crt_args.extend(mescc_lib_flags());
    crt_args.push("crt1.c".to_string());
    mescc(cfg, &lib, &crt_args)?;
    for f in ["crt1.o", "crt1.s"] {
        cp(&lib.join(f), &lib.join(format!("{MES_CPU}-mes/{f}")))?;
    }

    compile_archive(cfg, &lib, &libc_mini_sources("mescc"), "libc-mini.a")?;
    compile_archive(cfg, &lib, &libmescc_sources("mescc"), "libmescc.a")?;
    compile_archive(cfg, &lib, &libc_sources("mescc"), "libc.a")?;
    compile_archive(cfg, &lib, &libc_tcc_sources("mescc"), "libc+tcc.a")
}

/// bootstrap.sh phase 3 (cwd top, `srcdest=""`): compile mes_SOURCES with
/// MesCC and link `bin/mes-mescc` — the self-hosted mes — then `cp` it over
/// `bin/mes` (which until now was mes-m2).
fn mes_link_phase(cfg: &Cfg) -> Result<(), String> {
    let flags = s_vec(&[
        "-D", "HAVE_CONFIG_H=1", "-I", "include", "-I", "../include", "-I", "include",
        "-D", "HAVE_CONFIG_H=1", "-I", "include", "-L", "lib",
    ]);
    let mut objects = Vec::new();
    for c in &mes_sources() {
        let o = obj_name(c);
        let mut args = vec!["-c".to_string()];
        args.extend(flags.clone());
        args.extend(["-o".to_string(), o.clone(), c.clone()]);
        mescc(cfg, &cfg.top, &args)?;
        objects.push(o);
    }
    let mut link = s_vec(&[
        "-L", "lib", "-nostdlib", "-o", "bin/mes-mescc", "-L", "mescc-lib",
        "mescc-lib/crt1.o",
    ]);
    link.extend(objects);
    link.extend(s_vec(&["-lc", "-lmescc"]));
    mescc(cfg, &cfg.top, &link)?;
    let mes_mescc = cfg.top.join("bin/mes-mescc");
    chmod_x(&mes_mescc)?;
    cp(&mes_mescc, &cfg.top.join("bin/mes"))?;
    chmod_x(&cfg.top.join("bin/mes"))
}

/// build-aux/build-source-lib.sh (the bootstrap.sh gcc-lib subshell,
/// compiler=gcc): the SOURCE library later gcc rungs compile for themselves —
/// crt*.c copies, the concatenated libc+gnu.c and libtcc1.c, and libgetopt.c,
/// all under gcc-lib/x86-mes/.
fn gcc_source_lib_phase(cfg: &Cfg) -> Result<(), String> {
    let gcc_lib = cfg.top.join("gcc-lib");
    let dest = gcc_lib.join(format!("{MES_CPU}-mes"));
    fs::create_dir_all(&dest).map_err(|e| format!("mkdir gcc-lib: {e}"))?;

    let crt_dir = cfg.top.join(format!("lib/{MES_KERNEL}/{MES_CPU}-mes-gcc"));
    let mut found_crt = false;
    for ent in
        fs::read_dir(&crt_dir).map_err(|e| format!("read {}: {e}", crt_dir.display()))?
    {
        let ent = ent.map_err(|e| format!("read {}: {e}", crt_dir.display()))?;
        let name = ent.file_name();
        let n = name
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 name under {}", crt_dir.display()))?;
        if n.starts_with("crt") && n.ends_with(".c") {
            cp(&ent.path(), &dest.join(name.as_os_str()))?;
            found_crt = true;
        }
    }
    if !found_crt {
        return Err(format!("no crt*.c under {}", crt_dir.display()));
    }

    let header = |compiler: &str| {
        format!(
            "// Generated from Mes -- do not edit\n\
             // compiler: {compiler}\n\
             // cpu:      {MES_CPU}\n\
             // bits:     {MES_BITS}\n\
             // libc:     {MES_LIBC}\n\
             // kernel:   {MES_KERNEL}\n\
             // system:   {MES_SYSTEM}\n\n"
        )
    };
    for (name, list) in [
        ("libc+gnu.c", libc_gnu_sources("gcc")),
        ("libtcc1.c", libtcc1_sources()),
    ] {
        let mut body = header("gcc").into_bytes();
        for c in &list {
            body.extend_from_slice(format!("// {c}\n").as_bytes());
            body.extend_from_slice(&read(&cfg.top.join(c))?);
            body.push(b'\n');
        }
        write(&gcc_lib.join(name), &body)?;
        cp(&gcc_lib.join(name), &dest.join(name))?;
    }
    cp(&cfg.top.join("lib/posix/getopt.c"), &dest.join("libgetopt.c"))
}

/// install.sh under the rung's config: the bins and wrapper scripts, the doc
/// set, the include tree, the x86-mes/linux trees + the MesCC archives + the
/// gcc source lib into lib/, the module trees into share/mes and the guile
/// site/ccache dirs. The info/man/perl-ChangeLog conditionals are dead in a
/// bootstrap build (nothing generates them) and install.sh itself skips
/// absent files.
fn install_phase(cfg: &Cfg) -> Result<(), String> {
    let out = Path::new(&cfg.out);
    let top = &cfg.top;
    let bindir = out.join("bin");

    for (from, to) in [
        ("bin/mes", "mes"),
        ("bin/mes-m2", "mes-m2"),
        ("bin/mes-mescc", "mes-mescc"),
        ("scripts/mesar", "mesar"),
        ("scripts/mescc.scm", "mescc.scm"),
        ("scripts/mescc", "mescc"),
        ("scripts/diff.scm", "diff.scm"),
    ] {
        let dest = bindir.join(to);
        cp(&top.join(from), &dest)?;
        chmod_x(&dest)?;
    }

    let docdir = out.join("share/doc/mes");
    for doc in [
        "AUTHORS", "BOOTSTRAP", "COPYING", "HACKING", "NEWS", "README", "ROADMAP", "ChangeLog",
    ] {
        cp(&top.join(doc), &docdir.join(doc))?;
    }

    let t = crate::build::copy_tree_writable;
    t(&top.join("include"), &out.join("include"))?;
    t(
        &top.join(format!("lib/{MES_CPU}-mes")),
        &out.join(format!("lib/{MES_CPU}-mes")),
    )?;
    t(
        &top.join(format!("lib/{MES_KERNEL}/{MES_CPU}-mes")),
        &out.join(format!("lib/{MES_KERNEL}/{MES_CPU}-mes")),
    )?;
    t(&top.join(format!("gcc-lib/{MES_CPU}-mes")), &out.join("lib"))?;
    t(
        &top.join(format!("mescc-lib/{MES_CPU}-mes")),
        &out.join(format!("lib/{MES_CPU}-mes")),
    )?;
    t(&top.join("module"), &out.join("share/mes/module"))?;
    t(&top.join("mes/module"), &out.join("share/mes/module"))?;
    let site = out.join(format!("share/guile/site/{GUILE_EFFECTIVE_VERSION}"));
    t(&top.join("module"), &site)?;
    let ccache = out.join(format!("lib/guile/{GUILE_EFFECTIVE_VERSION}/site-ccache"));
    t(&top.join("module"), &ccache)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The transcription tripwire refuses BEFORE building: a pinned script
    // whose bytes drift from the audited baseline reds naming the file and
    // the baseline, so a source-pin bump forces a port re-audit.
    #[test]
    fn ported_script_pins_refuse_a_drifted_script() {
        let base = std::env::temp_dir().join(format!("td-mesboot-pins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for (rel, _) in PORTED_SCRIPT_PINS {
            let p = base.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"drifted\n").unwrap();
        }
        let err = verify_ported_scripts(&base).unwrap_err();
        assert!(err.contains("transcription baseline"), "{err}");
        assert!(err.contains("configure.sh"), "names the file: {err}");
        let _ = fs::remove_dir_all(&base);
    }

    // The i686-ELF32 kernel-capability classifier (re #469): a `=y` (or an
    // older-kernel equivalent) is Supported, an explicit not-set/`=n` is a
    // definitive Unsupported, and anything else is Unknown — the caller only
    // hard-fails on Unsupported, so an unreadable/quiet config never false-fails.
    #[test]
    fn ia32_kconfig_classifies_enabled_disabled_and_unknown() {
        // Enabled — the common x86-64 kernel (mirrors this repo's build host).
        assert!(matches!(
            ia32_from_kconfig("CONFIG_COMPAT=y\nCONFIG_IA32_EMULATION=y\nCONFIG_X86_X32_ABI=y\n"),
            Ia32Verdict::Supported
        ));
        // Older/other configs where the compat symbols carry 32-bit support.
        assert!(matches!(ia32_from_kconfig("CONFIG_COMPAT_32=y\n"), Ia32Verdict::Supported));
        assert!(matches!(ia32_from_kconfig("CONFIG_X86_32=y\n"), Ia32Verdict::Supported));
        // Explicitly disabled — both Kconfig spellings are a definitive NO.
        assert!(matches!(
            ia32_from_kconfig("# CONFIG_IA32_EMULATION is not set\n"),
            Ia32Verdict::Unsupported
        ));
        assert!(matches!(ia32_from_kconfig("CONFIG_IA32_EMULATION=n\n"), Ia32Verdict::Unsupported));
        // Neither present — cannot prove a negative, so Unknown (caller passes).
        assert!(matches!(ia32_from_kconfig("CONFIG_SMP=y\n"), Ia32Verdict::Unknown));
        assert!(matches!(ia32_from_kconfig(""), Ia32Verdict::Unknown));
        // The "is not set" comment must NOT be read as enabled by a loose
        // substring match: an enabled `=y` line elsewhere still wins.
        assert!(matches!(
            ia32_from_kconfig("# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set\nCONFIG_IA32_EMULATION=y\n"),
            Ia32Verdict::Supported
        ));
    }

    // The preflight POLICY reds ONLY on a proven-negative verdict; Supported and
    // Unknown both pass (the exec is the backstop for the unprovable case). This
    // exercises the pure mapping, NOT the live `/proc` probe — so it is
    // deterministic on every host, including a kernel with IA32 emulation
    // disabled, where a live `preflight_ia32()` would (correctly) red and must
    // not fail `cargo test` for a build that requested no i686 rung.
    #[test]
    fn preflight_ia32_reds_only_on_a_proven_negative() {
        assert!(
            ia32_preflight_result(Ia32Verdict::Unsupported).is_err(),
            "a proven-negative kernel must red"
        );
        assert!(
            ia32_preflight_result(Ia32Verdict::Supported).is_ok(),
            "a capable kernel must pass"
        );
        assert!(
            ia32_preflight_result(Ia32Verdict::Unknown).is_ok(),
            "an unprovable kernel must pass (the exec is the backstop)"
        );
    }
}
