//! The `rust-dist` build system: the upstream Rust release, retargeted onto td's
//! runtime (`rust-toolchain`). The module's name is historical: it first held the
//! imperative x86_64 self-hosting toolchain builds the stage0 ladder ended in.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

// --- rust-dist: the upstream Rust release, retargeted onto td's runtime ----------
//
// The engine phase runner for buildSystem "rust-dist" (`rust-toolchain`). It unpacks
// the pinned rustc, rust-std, Cargo and Clippy release components with the engine's
// own archive readers, assembles one toolchain tree, links td's runtime into its lib/
// (found through the UNCHANGED upstream RUNPATH `$ORIGIN/../lib`), and relinks each
// shipped executable's interpreter onto td's glibc loader with `crate::elf_edit::set_interp`
// (which grows the slot, #258). No compilation and no subprocess.
//
//   TD_SRC        the rustc component tarball (Class::Source).
//   TD_INPUT_MAP  name -> /td/store path:
//                   rust-std-dist, rust-cargo-dist, rust-clippy-dist
//                   glibc-x86-64   staged at GLIBC_X86_64_STAGE (loader + sonames)
//                   gcc-runtime    lib/libgcc_s.so.1
//                   zlib-x86-64    lib/libz.so.1
//   out           the toolchain tree.
//
// The runtime libraries are symlinks into their own outputs, so each keeps its own
// provenance and the closure names them; imported Debian libraries have no td companions. Same pins + same inputs => the same
// tree (the double-build oracle proves it).

/// The staged install-prefix subpath of the x86_64 glibc rung output (a mesboot rung
/// installs into <out>/stage/td/store/<prefix>).
pub const GLIBC_X86_64_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

/// The upstream target triple whose host toolchain and standard library td ships.
const RUST_TARGET_BIN: &str = "lib/rustlib/x86_64-unknown-linux-gnu/bin";

/// The shipped executables, relative to the tree, each relinked onto td's loader.
const RUST_EXECUTABLES: &[&str] = &[
    "bin/rustc",
    "bin/rustdoc",
    "bin/cargo",
    "bin/cargo-clippy",
    "bin/clippy-driver",
    "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
];

/// Upstream files td does not ship: the installer manifest each component
/// copies to the root, debugger scripts for absent debuggers, the
/// rust-analyzer server, linker flavors for other platforms, the wasm
/// component linker, and an objcopy whose RUNPATH cannot reach libLLVM.
const RUST_DROPPED: &[&str] = &[
    "manifest.in",
    "bin/rust-gdb",
    "bin/rust-gdbgui",
    "bin/rust-lldb",
    "libexec",
    "lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld",
    "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy",
    "lib/rustlib/x86_64-unknown-linux-gnu/bin/wasm-component-ld",
];

/// The shared libraries in lib/ the executables load, by name prefix. The
/// prefix names libLLVM's soname file, not the `libLLVM-*.so` linker script.
const RUST_SHARED_LIBS: &[&str] = &["librustc_driver-", "libLLVM.so."];

/// glibc sonames the upstream binaries need, linked from td glibc into lib/.
const RUST_GLIBC_SONAMES: &[&str] = &[
    "libc.so.6",
    "libdl.so.2",
    "librt.so.1",
    "libpthread.so.0",
    "libm.so.6",
];

/// The resolved transform inputs, read from the drv env.
pub struct RustDistInputs {
    /// Individually unpacked upstream component trees.
    pub rustc_src: PathBuf,
    pub std_src: PathBuf,
    pub cargo_src: PathBuf,
    pub clippy_src: PathBuf,
    /// glibc-x86-64 (staged): the loader and the sonames linked into lib/.
    pub glibc: PathBuf,
    /// gcc-runtime's `lib/libgcc_s.so.1`.
    pub libgcc_s: PathBuf,
    /// zlib-x86-64's `lib/libz.so.1`.
    pub libz: PathBuf,
    /// `$out`.
    pub out: PathBuf,
}

/// Unpack one pinned release component into `dest` (top-level dir stripped) with the
/// engine's own gzip/xz/tar readers — no subprocess. A component is checked for the
/// one member it must carry, so a layout change upstream fails here.
/// NOTE at the next pin bump: the engine tar reader caps a single entry at
/// `tar::MAX_TAR_ENTRY_BYTES` (256 MiB), a limit host `tar` never imposed.
pub fn unpack_rust_component(tarball: &Path, dest: &Path, required: &str) -> Result<(), String> {
    crate::tar::unpack_archive(tarball, dest, false)?;
    if !dest.join(required).exists() {
        return Err(format!(
            "unpack {}: extracted component is missing {required} (release layout drift?)",
            tarball.display()
        ));
    }
    Ok(())
}

/// Assemble and retarget the pinned upstream release. No compilation.
pub fn assemble(inp: &RustDistInputs) -> Result<(), String> {
    assemble_rust_tree(inp)?;
    relink_rust_interp(&inp.out, &inp.glibc)?;
    Ok(())
}

/// Copy the components into one tree, drop what td does not ship, link the runtime
/// into lib/, and provenance-check the result.
fn assemble_rust_tree(inp: &RustDistInputs) -> Result<(), String> {
    let tree = &inp.out;
    for (component, dir) in [
        (&inp.rustc_src, "rustc"),
        (&inp.cargo_src, "cargo"),
        (&inp.clippy_src, "clippy-preview"),
        (&inp.std_src, "rust-std-x86_64-unknown-linux-gnu"),
    ] {
        merge_component(&component.join(dir), tree, Path::new(""))?;
    }
    make_writable(tree).map_err(ioerr("chmod tree"))?;
    for dropped in RUST_DROPPED {
        let path = tree.join(dropped);
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| format!("rust-dist: upstream no longer ships {dropped}: {e}"))?;
        if meta.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        }
        .map_err(|e| format!("rust-dist: remove {dropped}: {e}"))?;
    }
    // rustc's default link passes `-B…/gcc-ld -fuse-ld=lld`, which runs
    // `gcc-ld/ld.lld` under a gcc built without `--with-ld` (td's ignores it). As a
    // link to rust-lld, lld takes its GNU flavor from that name and `$ORIGIN` is
    // rust-lld's own directory, so the upstream wrapper and its unreachable
    // RUNPATH are not needed.
    let gcc_ld = tree.join(RUST_TARGET_BIN).join("gcc-ld");
    fs::create_dir_all(&gcc_ld).map_err(ioerr("mkdir gcc-ld"))?;
    symlink_force("../rust-lld", &gcc_ld.join("ld.lld"))?;

    // The sysroot must hold the standard library rlibs a program links.
    let rlib_dir = tree.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
    for crate_name in ["libstd-", "libproc_macro-", "libtest-"] {
        if !glob_exists(&rlib_dir, crate_name, ".rlib") {
            return Err(format!("assembled rust sysroot has no {crate_name}*.rlib"));
        }
    }

    // [provenance] the upstream binaries and libraries carry no /gnu/store bytes.
    let mut provenance: Vec<PathBuf> = RUST_EXECUTABLES.iter().map(|b| tree.join(b)).collect();
    provenance.extend(rust_shared_libs(tree)?);
    for b in &provenance {
        if contains_gnu_store(b)? {
            return Err(format!(
                "{} contains /gnu/store bytes — not guix-free upstream",
                b.display()
            ));
        }
    }

    // The runtime, linked from its own outputs into lib/, which every shipped
    // binary's RUNPATH reaches.
    let lib = tree.join("lib");
    let mut runtime: Vec<(PathBuf, &str)> = RUST_GLIBC_SONAMES
        .iter()
        .map(|soname| (inp.glibc.join("lib").join(soname), *soname))
        .collect();
    runtime.push((inp.libgcc_s.clone(), "libgcc_s.so.1"));
    runtime.push((inp.libz.clone(), "libz.so.1"));
    for (target, soname) in runtime {
        if !target.is_file() {
            return Err(format!(
                "rust-dist: runtime library {} is missing",
                target.display()
            ));
        }
        symlink_force_abs(&target, &lib.join(soname))?;
    }
    Ok(())
}

/// Relink each shipped executable's ELF interpreter onto td glibc's loader, and
/// check every library each shipped ELF needs resolves through its own RUNPATH
/// inside the tree.
fn relink_rust_interp(tree: &Path, glibc: &Path) -> Result<(), String> {
    let interp = format!("{}/lib/ld-linux-x86-64.so.2", glibc.display());
    for b in RUST_EXECUTABLES {
        let bin = tree.join(b);
        crate::elf_edit::set_interp(&bin, &interp)?;
        let got = crate::elf::read_interp(&bin)?.unwrap_or_default();
        if got != interp {
            return Err(format!(
                "interp of {b} not relinked to td glibc's loader (got: {got})"
            ));
        }
    }
    // An executable upstream adds would otherwise keep its /lib64 loader.
    let mut interpreted = Vec::new();
    collect_interpreted(tree, tree, &mut interpreted)?;
    if let Some(stray) = interpreted
        .iter()
        .find(|rel| !RUST_EXECUTABLES.contains(&rel.as_str()))
    {
        return Err(format!(
            "rust-dist: {stray} has an ELF interpreter but is neither relinked nor dropped"
        ));
    }
    let mut elfs: Vec<PathBuf> = RUST_EXECUTABLES.iter().map(|b| tree.join(b)).collect();
    elfs.extend(rust_shared_libs(tree)?);
    for elf in &elfs {
        let origin = elf
            .parent()
            .ok_or_else(|| format!("{} has no parent", elf.display()))?;
        let runpath = crate::elf::read_rpath(elf)?.unwrap_or_default();
        let needed = crate::elf::read_needed(elf)?;
        needed_resolve(tree, origin, &runpath, &needed)
            .map_err(|e| format!("rust-dist: {}: {e}", elf.display()))?;
    }
    Ok(())
}

/// Copy one component's tree into `dst`. Components share directories but no
/// file except the installer manifest each carries at its root, which is
/// dropped later; any other collision means the release layout changed.
fn merge_component(src: &Path, dst: &Path, rel: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let rel = rel.join(entry.file_name());
        let from_dir = fs::symlink_metadata(&from)
            .map_err(|e| format!("stat {}: {e}", from.display()))?
            .is_dir();
        match fs::symlink_metadata(&to) {
            Ok(existing) if from_dir && existing.is_dir() => merge_component(&from, &to, &rel)?,
            Ok(_) if rel == Path::new("manifest.in") => {
                copy_entry(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?
            }
            Ok(_) => {
                return Err(format!("rust-dist: two components ship {}", rel.display()));
            }
            Err(_) => {
                copy_entry(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?
            }
        }
    }
    Ok(())
}

/// `path` with `.` and `..` resolved without touching the filesystem; `None`
/// when `..` climbs above the root.
fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// Tree-relative paths of every regular file under `dir` that is an ELF with a
/// program interpreter.
fn collect_interpreted(tree: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("read {}: {e}", dir.display()))?
            .path();
        let meta =
            fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            collect_interpreted(tree, &path, out)?;
        } else if meta.is_file()
            && is_elf(&path)?
            && crate::elf::read_interp(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .is_some()
        {
            let rel = path
                .strip_prefix(tree)
                .map_err(|_| format!("{} is outside {}", path.display(), tree.display()))?;
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn is_elf(path: &Path) -> Result<bool, String> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let mut file = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == *b"\x7fELF"),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Each of `RUST_SHARED_LIBS` in the tree's lib/; a missing one is an error.
fn rust_shared_libs(tree: &Path) -> Result<Vec<PathBuf>, String> {
    RUST_SHARED_LIBS
        .iter()
        .map(|prefix| {
            glob_first_in(&tree.join("lib"), prefix, "")
                .ok_or_else(|| format!("rust-dist: lib/ has no {prefix}*"))
        })
        .collect()
}

/// Every RUNPATH entry is `$ORIGIN`-relative and stays inside `tree`, and
/// every DT_NEEDED other than the loader resolves through one of them.
fn needed_resolve(
    tree: &Path,
    origin: &Path,
    runpath: &str,
    needed: &[String],
) -> Result<(), String> {
    let mut dirs = Vec::new();
    let entries = runpath.split(':').filter(|_| !runpath.is_empty());
    for entry in entries {
        let rest = entry
            .strip_prefix("$ORIGIN")
            .ok_or_else(|| format!("RUNPATH entry {entry:?} is not $ORIGIN-relative"))?;
        let dir = lexical_normalize(&origin.join(rest.trim_start_matches('/')))
            .filter(|dir| dir.starts_with(tree))
            .ok_or_else(|| format!("RUNPATH entry {entry:?} leaves the toolchain tree"))?;
        dirs.push(dir);
    }
    match needed
        .iter()
        .filter(|name| name.as_str() != "ld-linux-x86-64.so.2")
        .find(|name| !dirs.iter().any(|dir| dir.join(name).exists()))
    {
        Some(missing) => Err(format!(
            "needs {missing}, which its RUNPATH {runpath:?} does not reach"
        )),
        None => Ok(()),
    }
}

// --- x86_64 binutils 2.44 (native + self flavors) ----------------------------------

// --- shared helpers (ports of the shell's _*) ------------------------------------

// --- tiny std-only utilities -----------------------------------------------------

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
        // cp -f semantics: an existing dest may be read-only (rust ships 0444 files, and each
        // component's root manifest.in overwrites the last), and fs::copy would then fail
        // with EACCES opening it for write — remove it first so the overlay always lands.
        if fs::symlink_metadata(to).is_ok() {
            let _ = fs::remove_file(to);
        }
        fs::copy(from, to)?;
    }
    Ok(())
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

fn contains_sub(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn ioerr(ctx: &'static str) -> impl Fn(io::Error) -> String {
    move |e| format!("{ctx}: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(prefix: &str) -> crate::test_temp::TempDir {
        crate::test_temp::TempDir::new(&format!("td-toolchain-test-{prefix}")).unwrap()
    }

    // A minimal uncompressed ustar archive of `(path, contents)` members —
    // `unpack_archive` dispatches to its raw-tar reader when there is no
    // compression magic, so this drives `unpack_rust_component` end to end with
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
        let top = "rustc-1.99.1-x86_64-unknown-linux-gnu";
        let good = d.join("good.tar");
        fs::write(
            &good,
            ustar(&[(&format!("{top}/rustc/bin/rustc"), b"#!rustc\n")]),
        )
        .expect("write good tar");
        let out = d.join("good-out");
        unpack_rust_component(&good, &out, "rustc/bin/rustc")
            .expect("well-formed component unpacks");
        assert!(
            out.join("rustc/bin/rustc").is_file(),
            "rustc not stripped into place"
        );

        // The same archive cannot masquerade as the separately pinned Cargo component.
        let bad = d.join("bad.tar");
        fs::write(
            &bad,
            ustar(&[(&format!("{top}/rustc/bin/rustc"), b"#!rustc\n")]),
        )
        .expect("write bad tar");
        let err = unpack_rust_component(&bad, &d.join("bad-out"), "cargo/bin/cargo").unwrap_err();
        assert!(err.contains("missing cargo/bin/cargo"), "{err}");
        let _ = fs::remove_dir_all(&d);
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
        assert!(
            hit.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("libcore-"),
            "got {hit:?}"
        );
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

    /// rust-lld's RUNPATH reaches lib/ through `$ORIGIN/../../../`; a
    /// library no entry reaches, or any entry not `$ORIGIN`-relative, is refused.
    #[test]
    fn needed_libraries_resolve_only_through_origin_runpath() {
        let d = tmp("td-rust-needed");
        let bin = d.join("lib/rustlib/x86_64-unknown-linux-gnu/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(d.join("lib/libLLVM.so.23.1"), b"").unwrap();
        fs::write(d.join("lib/libc.so.6"), b"").unwrap();
        let needed: Vec<String> = ["libLLVM.so.23.1", "libc.so.6", "ld-linux-x86-64.so.2"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let runpath = "$ORIGIN/../../../:$ORIGIN/../lib";
        assert_eq!(needed_resolve(&d, &bin, runpath, &needed), Ok(()));
        assert_eq!(needed_resolve(&d, &bin, "", &[]), Ok(()));
        let mut more = needed.clone();
        more.push("libz.so.1".into());
        let err = needed_resolve(&d, &bin, runpath, &more).unwrap_err();
        assert!(err.contains("needs libz.so.1"), "{err}");
        // An absolute entry is refused, though it names the directory.
        let absolute = format!("$ORIGIN/../../../:{}", d.join("lib").display());
        let err = needed_resolve(&d, &bin, &absolute, &needed).unwrap_err();
        assert!(err.contains("not $ORIGIN-relative"), "{err}");
        // So are an empty entry (the working directory) and a climb out.
        let err = needed_resolve(&d, &bin, "$ORIGIN/../../../:", &needed).unwrap_err();
        assert!(err.contains("not $ORIGIN-relative"), "{err}");
        let err = needed_resolve(&d, &bin, "$ORIGIN/../../../../../lib", &needed).unwrap_err();
        assert!(err.contains("leaves the toolchain tree"), "{err}");
        let _ = fs::remove_dir_all(&d);
    }

    /// The shared-library list names libLLVM's soname file, so its 42-byte
    /// `libLLVM-*.so` linker script, which sorts first, is never the one
    /// checked; a tree missing either library is refused.
    #[test]
    fn shared_libraries_are_the_real_objects_and_required() {
        let d = tmp("td-rust-shared");
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(d.join("lib/libLLVM-23-rust-1.99.1-stable.so"), b"INPUT(x)").unwrap();
        fs::write(d.join("lib/librustc_driver-4d71126a08f22b4a.so"), b"").unwrap();
        let err = rust_shared_libs(&d).unwrap_err();
        assert!(err.contains("libLLVM.so."), "{err}");
        fs::write(d.join("lib/libLLVM.so.23.1-rust-1.99.1-stable"), b"").unwrap();
        let libs = rust_shared_libs(&d).unwrap();
        assert_eq!(
            libs,
            [
                d.join("lib/librustc_driver-4d71126a08f22b4a.so"),
                d.join("lib/libLLVM.so.23.1-rust-1.99.1-stable"),
            ]
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// Components may share directories; a shared file other than the root
    /// installer manifest is refused.
    #[test]
    fn components_merge_without_collisions() {
        let d = tmp("td-rust-merge");
        let (a, b, tree) = (d.join("a"), d.join("b"), d.join("tree"));
        for (component, bin) in [(&a, "rustc"), (&b, "cargo")] {
            fs::create_dir_all(component.join("bin")).unwrap();
            fs::write(component.join("bin").join(bin), bin).unwrap();
            fs::write(component.join("manifest.in"), bin).unwrap();
        }
        merge_component(&a, &tree, Path::new("")).unwrap();
        merge_component(&b, &tree, Path::new("")).unwrap();
        assert!(tree.join("bin/rustc").is_file() && tree.join("bin/cargo").is_file());
        let c = d.join("c");
        fs::create_dir_all(c.join("bin")).unwrap();
        fs::write(c.join("bin/rustc"), "other").unwrap();
        let err = merge_component(&c, &tree, Path::new("")).unwrap_err();
        assert!(err.contains("two components ship bin/rustc"), "{err}");
        let _ = fs::remove_dir_all(&d);
    }

    /// Only regular ELF files with a program interpreter are collected; a
    /// symlink to one, a library without one, and a non-ELF file are not.
    #[test]
    fn interpreted_files_are_found_anywhere_in_the_tree() {
        let d = tmp("td-rust-interp");
        let tree = d.join("tree");
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::create_dir_all(tree.join("lib/deep")).unwrap();
        let interp = crate::elf_synth::synth_interp_elf("/lib64/ld-linux-x86-64.so.2", true);
        fs::write(tree.join("bin/rustc"), &interp).unwrap();
        fs::write(tree.join("lib/deep/stray"), &interp).unwrap();
        let library = crate::elf_synth::synth_needed_elf(&["libc.so.6"], true);
        fs::write(tree.join("lib/libfoo.so"), library).unwrap();
        std::os::unix::fs::symlink("../bin/rustc", tree.join("lib/link")).unwrap();
        fs::write(tree.join("bin/script"), b"#!/bin/sh\n").unwrap();
        fs::write(tree.join("lib/short"), b"\x7fE").unwrap();
        let mut found = Vec::new();
        collect_interpreted(&tree, &tree, &mut found).unwrap();
        found.sort();
        assert_eq!(found, ["bin/rustc", "lib/deep/stray"]);
        let _ = fs::remove_dir_all(&d);
    }
}
