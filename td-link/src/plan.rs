//! rustc's gcc-style link line, translated into one GNU-flavor rust-lld argv.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// The store paths a target build compiles in.
#[derive(Debug)]
pub struct Toolchain<'a> {
    /// The toolchain's `rust-lld`, run in its GNU flavor.
    pub lld: &'a str,
    /// td glibc's `lib`: start files, libc, and the loader.
    pub glibc: &'a str,
    /// gcc-runtime's `lib`: crtbegin/crtend, libgcc.a and libgcc_eh.a.
    pub runtime: &'a str,
}

/// What the link produces, which selects the start files and the ELF shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    DynamicPie,
    DynamicExec,
    StaticExec,
    StaticPie,
    Shared,
}

/// One translated link.
#[derive(Debug)]
pub struct Plan {
    pub output: Output,
    pub path: OsString,
    start_files: bool,
    /// Caller inputs, libraries, search paths and linker options, in order.
    items: Vec<OsString>,
    /// The positional inputs among `items`.
    inputs: Vec<OsString>,
}

/// Inputs a link may name: objects, archives, rlibs and shared objects.
fn is_link_input(arg: &[u8]) -> bool {
    [b".o".as_slice(), b".a", b".rlib", b".so"]
        .iter()
        .any(|suffix| arg.ends_with(suffix))
        || arg.windows(4).any(|w| w == b".so.")
}

/// What every link passes first. rustc asks for `--eh-frame-hdr` on every
/// link, static ET_EXEC included, where GCC's own spec would omit it. An
/// object's `.deplibs` section would make LLD open libraries, or any path,
/// that the line never names; rustc writes none.
const HEADER: &[&str] = &[
    "-m",
    "elf_x86_64",
    "--hash-style=gnu",
    "--build-id=sha1",
    "--eh-frame-hdr",
    "--no-dependent-libraries",
];

/// Options that ask for compilation or preprocessing rather than a link.
const COMPILE_OPTIONS: &[&[u8]] = &[b"-c", b"-S", b"-E", b"-x", b"-M", b"-MM", b"-MD", b"-MMD"];

/// The linker options rustc 1.96's GNU-cc flavor writes for
/// x86_64-unknown-linux-gnu (its GccLinker and target spec), with td's
/// `--build-id=sha1` and td-link-test's `--no-as-needed`. Any other `-Wl,`
/// or `-Xlinker` piece is refused.
const LINKER_FLAGS: &[&[u8]] = &[
    b"--as-needed",
    b"--no-as-needed",
    b"-Bstatic",
    b"-Bdynamic",
    b"--whole-archive",
    b"--no-whole-archive",
    b"--gc-sections",
    b"--no-gc-sections",
    b"--eh-frame-hdr",
    b"-O1",
    b"--strip-debug",
    b"--strip-all",
    b"--no-undefined-version",
    b"--build-id=sha1",
];

/// `-z` keywords rustc writes: the non-executable stack and its RELRO levels.
const Z_KEYWORDS: &[&[u8]] = &[b"noexecstack", b"relro", b"norelro", b"now"];

/// Host system prefixes no link input, search path or version script may
/// resolve into: td links td's store and its own build tree, never a host's
/// (or the Debian build root's) libraries. Guix and Nix stores are a
/// developer host's `/usr`.
const HOST_PREFIXES: &[&str] = &[
    "/usr", "/lib", "/lib32", "/lib64", "/libx32", "/bin", "/sbin", "/etc", "/opt", "/gnu", "/nix",
];

fn err(arg: &OsStr, why: &str) -> String {
    format!("td-link: {why}: {}", arg.to_string_lossy())
}

fn joined(prefix: &[u8], value: &[u8]) -> OsString {
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(value);
    OsString::from_vec(bytes)
}

/// The option's value: the rest of `arg` after `flag`, else the next argument.
fn value(
    arg: &OsStr,
    flag: &[u8],
    rest: &mut std::vec::IntoIter<OsString>,
) -> Result<OsString, String> {
    match arg.as_bytes().strip_prefix(flag) {
        Some(inline) if !inline.is_empty() => Ok(OsString::from_vec(inline.to_vec())),
        _ => rest.next().ok_or_else(|| err(arg, "option needs a value")),
    }
}

/// `path` made absolute against the working directory with `.` and `..`
/// folded, and symbolic links resolved when it exists.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("td-link: working directory: {e}"))?
            .join(path)
    };
    let mut folded = PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                folded.pop();
            }
            Component::Normal(name) => folded.push(name),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Ok(folded)
}

/// Refuse a path that resolves under a host system prefix.
fn not_host(path: &[u8], what: &str) -> Result<(), String> {
    // LLD reads a search directory's leading `=` as the (empty) sysroot,
    // and GNU ld also `$SYSROOT`; refused for every path, over-broadly.
    if path.starts_with(b"=") || path.starts_with(b"$SYSROOT") {
        return Err(err(
            OsStr::from_bytes(path),
            &format!("{what} is sysroot-relative"),
        ));
    }
    let path = Path::new(OsStr::from_bytes(path));
    let real = resolved(path)?;
    if HOST_PREFIXES.iter().any(|prefix| real.starts_with(prefix)) {
        return Err(err(
            path.as_os_str(),
            &format!(
                "{what} resolves to {} under a host system prefix",
                real.display()
            ),
        ));
    }
    Ok(())
}

/// libgcc is static in every td output: only the `-lgcc_s` spelling is
/// rewritten, so every other way of naming the shared libgcc is refused.
fn names_libgcc_s(file: &[u8]) -> bool {
    file.starts_with(b"libgcc_s")
}

/// A `-l` library. libgcc is static in every td output, so `-lgcc_s`, which
/// rustc names for dynamic links, is gcc-runtime's static unwinder and libgcc.
fn library(name: &[u8], items: &mut Vec<OsString>) -> Result<(), String> {
    match name {
        b"" => return Err("td-link: -l needs a library name".into()),
        b"gcc_s" => items.extend([OsString::from("-lgcc_eh"), OsString::from("-lgcc")]),
        // A name with a slash would walk out of the search paths.
        _ if name.contains(&b'/') => {
            return Err(err(&joined(b"-l", name), "a library name is not a path"));
        }
        _ if name.strip_prefix(b":").is_some_and(names_libgcc_s) => {
            return Err(err(
                &joined(b"-l", name),
                "libgcc is static in every td output",
            ));
        }
        _ => items.push(joined(b"-l", name)),
    }
    Ok(())
}

/// One argument for the linker itself (`-Wl,` piece or `-Xlinker`). Only
/// what rustc writes is accepted; `-z` takes the next piece as its keyword.
fn linker_option(
    piece: &[u8],
    z_pending: &mut bool,
    items: &mut Vec<OsString>,
) -> Result<(), String> {
    let arg = OsStr::from_bytes(piece);
    if std::mem::take(z_pending) {
        if !Z_KEYWORDS.contains(&piece) {
            return Err(err(arg, "unsupported -z keyword"));
        }
        items.extend([OsString::from("-z"), arg.to_owned()]);
        return Ok(());
    }
    let interpreter = piece == b"-dynamic-linker"
        || piece == b"--dynamic-linker"
        || piece == b"--no-dynamic-linker"
        || piece.starts_with(b"--dynamic-linker=")
        || piece.starts_with(b"-dynamic-linker=")
        || piece.starts_with(b"-I");
    if interpreter {
        return Err(err(
            arg,
            "the dynamic linker is td glibc's and cannot be named",
        ));
    }
    if piece.is_empty() {
        return Err("td-link: an empty linker argument would take the next one's place".into());
    }
    if piece.starts_with(b"@") {
        return Err(err(
            arg,
            "a linker response file is read by LLD, not checked by td-link",
        ));
    }
    if piece == b"-z" {
        *z_pending = true;
        return Ok(());
    }
    if let Some(script) = piece.strip_prefix(b"--version-script=") {
        not_host(script, "version script")?;
    } else if let Some(name) = piece.strip_prefix(b"-soname=") {
        if name.is_empty() || name.contains(&b'/') {
            return Err(err(arg, "a shared object's name is one file name"));
        }
    } else if !LINKER_FLAGS.contains(&piece) {
        return Err(err(arg, "unsupported linker option"));
    }
    items.push(arg.to_owned());
    Ok(())
}

#[derive(Default)]
struct Kind {
    shared: bool,
    static_exec: bool,
    static_pie: bool,
    pie: Option<bool>,
}

impl Kind {
    fn resolve(&self) -> Result<Output, String> {
        let conflict = |what: &str| Err(format!("td-link: conflicting output kinds: {what}"));
        if self.shared {
            if self.static_exec || self.static_pie || self.pie == Some(true) {
                return conflict("-shared with -static, -static-pie or -pie");
            }
            return Ok(Output::Shared);
        }
        if self.static_pie {
            if self.static_exec || self.pie.is_some() {
                return conflict("-static-pie with -static, -pie or -no-pie");
            }
            return Ok(Output::StaticPie);
        }
        if self.static_exec {
            if self.pie == Some(true) {
                return conflict("-static with -pie (a static PIE is -static-pie)");
            }
            return Ok(Output::StaticExec);
        }
        Ok(match self.pie {
            Some(false) => Output::DynamicExec,
            _ => Output::DynamicPie,
        })
    }
}

/// The directory rustc's self-contained linker search adds with `-B`: the
/// `gcc-ld` beside the compiled rust-lld. td-link is the linker, so it has
/// nothing to offer.
fn is_rustc_gcc_ld(dir: &[u8], toolchain: &Toolchain) -> bool {
    let Some(bin) = Path::new(toolchain.lld).parent() else {
        return false;
    };
    let expected = bin.join("gcc-ld");
    let mut dir = dir;
    while let Some(trimmed) = dir.strip_suffix(b"/") {
        dir = trimmed;
    }
    let dir = Path::new(OsStr::from_bytes(dir));
    dir == expected
        || matches!(
            (std::fs::canonicalize(dir), std::fs::canonicalize(&expected)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Read a gcc-style link line. Anything it does not know is refused, so a
/// compile request or an unexpected option fails here, not inside the link.
pub fn plan(args: Vec<OsString>, toolchain: &Toolchain) -> Result<Plan, String> {
    let mut kind = Kind::default();
    let mut path = None;
    let mut start_files = true;
    let mut default_libs = true;
    let mut z_pending = false;
    let mut items = Vec::new();
    let mut inputs: Vec<OsString> = Vec::new();
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        let bytes = arg.as_bytes();
        if z_pending && !(bytes.starts_with(b"-Wl,") || bytes == b"-Xlinker") {
            return Err(err(&arg, "-z needs its keyword before"));
        }
        match bytes {
            b"-shared" => kind.shared = true,
            b"-static" => kind.static_exec = true,
            b"-static-pie" => kind.static_pie = true,
            b"-pie" => kind.pie = Some(true),
            b"-no-pie" => kind.pie = Some(false),
            b"-nostdlib" => {
                start_files = false;
                default_libs = false;
            }
            b"-nostartfiles" => start_files = false,
            b"-nodefaultlibs" => default_libs = false,
            // Every td output links libgcc statically; rustc's dynamic
            // `-lgcc_s` is rewritten to match (see `library`).
            b"-static-libgcc" => {}
            b"-m64" | b"-fuse-ld=lld" => {}
            b"-rdynamic" => items.push(OsString::from("--export-dynamic")),
            b"-Xlinker" => {
                let piece = rest
                    .next()
                    .ok_or_else(|| err(&arg, "option needs a value"))?;
                linker_option(piece.as_bytes(), &mut z_pending, &mut items)?;
            }
            b"-z" => {
                let keyword = rest
                    .next()
                    .ok_or_else(|| err(&arg, "option needs a value"))?;
                z_pending = true;
                linker_option(keyword.as_bytes(), &mut z_pending, &mut items)?;
            }
            b"-u" => {
                let symbol = rest
                    .next()
                    .ok_or_else(|| err(&arg, "option needs a value"))?;
                items.extend([arg, symbol]);
            }
            b"-" => return Err(err(&arg, "standard input is not a link input")),
            _ if bytes.starts_with(b"-Wl,") => {
                for piece in bytes.get(4..).unwrap_or_default().split(|&b| b == b',') {
                    linker_option(piece, &mut z_pending, &mut items)?;
                }
            }
            _ if bytes.starts_with(b"-o") => path = Some(value(&arg, b"-o", &mut rest)?),
            _ if bytes.starts_with(b"-l") => {
                library(value(&arg, b"-l", &mut rest)?.as_bytes(), &mut items)?
            }
            _ if bytes.starts_with(b"-L") => {
                let dir = value(&arg, b"-L", &mut rest)?;
                not_host(dir.as_bytes(), "library search path")?;
                items.push(joined(b"-L", dir.as_bytes()))
            }
            _ if bytes.starts_with(b"-B") => {
                let dir = value(&arg, b"-B", &mut rest)?;
                if !is_rustc_gcc_ld(dir.as_bytes(), toolchain) {
                    return Err(err(
                        &arg,
                        "-B names a directory td-link would not search (only rustc's gcc-ld is accepted)",
                    ));
                }
            }
            _ if COMPILE_OPTIONS.contains(&bytes) => {
                return Err(err(&arg, "td-link links; it does not compile"));
            }
            _ if bytes.starts_with(b"-") => {
                return Err(err(&arg, "unsupported option"));
            }
            _ if is_link_input(bytes) => {
                let file = Path::new(&arg).file_name().map(OsStr::as_bytes);
                if file.is_some_and(names_libgcc_s) {
                    return Err(err(&arg, "libgcc is static in every td output"));
                }
                not_host(bytes, "link input")?;
                inputs.push(arg.clone());
                items.push(arg)
            }
            _ => {
                return Err(err(
                    &arg,
                    "not an object, archive or shared object (td-link does not compile)",
                ))
            }
        }
    }
    if z_pending {
        return Err("td-link: -z needs its keyword".into());
    }
    let output = kind.resolve()?;
    // rustc names every library itself; td-link adds none of its own.
    if default_libs {
        return Err("td-link: a link without -nodefaultlibs is refused (rustc passes it)".into());
    }
    Ok(Plan {
        output,
        path: path.ok_or("td-link: a link without -o is refused (rustc passes it)")?,
        start_files,
        items,
        inputs,
    })
}

/// An input's leading bytes: an ELF object or an ordinary archive. LLD reads
/// any other file but bitcode as a linker script, which could name host
/// libraries or lay out sections, and a thin archive names its members by
/// path.
fn is_object_or_archive(head: &[u8]) -> bool {
    head.starts_with(b"\x7fELF") || head.starts_with(b"!<arch>\n")
}

impl Plan {
    /// Refuse a positional input that is not an ELF object or an ordinary
    /// archive. Run before linking, on the files the line names.
    pub fn check_inputs(&self) -> Result<(), String> {
        use std::io::Read;
        for input in &self.inputs {
            let mut head = Vec::with_capacity(8);
            std::fs::File::open(input)
                .and_then(|file| file.take(8).read_to_end(&mut head))
                .map_err(|e| err(input, &format!("read link input: {e}")))?;
            if !is_object_or_archive(&head) {
                return Err(err(
                    input,
                    "not an ELF object or ordinary archive (a linker script or thin archive)",
                ));
            }
        }
        Ok(())
    }

    /// The GNU-flavor rust-lld arguments, in GCC's order: mode, output, start
    /// files, the caller's line, td's search paths, end files.
    pub fn ld_args(&self, toolchain: &Toolchain) -> Vec<OsString> {
        let glibc = |file: &str| OsString::from(format!("{}/{file}", toolchain.glibc));
        let runtime = |file: &str| OsString::from(format!("{}/{file}", toolchain.runtime));
        let (mode, start, end): (&[&str], _, _) = match self.output {
            Output::DynamicPie => (
                &["-pie", "-dynamic-linker"],
                vec![glibc("Scrt1.o"), glibc("crti.o"), runtime("crtbeginS.o")],
                vec![runtime("crtendS.o"), glibc("crtn.o")],
            ),
            Output::DynamicExec => (
                &["-no-pie", "-dynamic-linker"],
                vec![glibc("crt1.o"), glibc("crti.o"), runtime("crtbegin.o")],
                vec![runtime("crtend.o"), glibc("crtn.o")],
            ),
            Output::StaticExec => (
                &["-static", "-no-pie"],
                vec![glibc("crt1.o"), glibc("crti.o"), runtime("crtbeginT.o")],
                vec![runtime("crtend.o"), glibc("crtn.o")],
            ),
            Output::StaticPie => (
                &["-static", "-pie", "--no-dynamic-linker", "-z", "text"],
                vec![glibc("rcrt1.o"), glibc("crti.o"), runtime("crtbeginS.o")],
                vec![runtime("crtendS.o"), glibc("crtn.o")],
            ),
            Output::Shared => (
                &["-shared"],
                vec![glibc("crti.o"), runtime("crtbeginS.o")],
                vec![runtime("crtendS.o"), glibc("crtn.o")],
            ),
        };
        let mut args: Vec<OsString> = HEADER.iter().map(OsString::from).collect();
        args.extend(mode.iter().map(OsString::from));
        if matches!(self.output, Output::DynamicPie | Output::DynamicExec) {
            args.push(glibc("ld-linux-x86-64.so.2"));
        }
        args.extend([OsString::from("-o"), self.path.clone()]);
        if self.start_files {
            args.extend(start);
        }
        args.extend(self.items.iter().cloned());
        args.push(joined(b"-L", toolchain.glibc.as_bytes()));
        args.push(joined(b"-L", toolchain.runtime.as_bytes()));
        if !matches!(self.output, Output::StaticExec | Output::StaticPie) {
            // DT_RPATH, as GNU ld wrote: it also governs the libraries a
            // dependency loads, and LD_LIBRARY_PATH cannot pre-empt it.
            args.extend([
                OsString::from("--disable-new-dtags"),
                OsString::from("-rpath"),
                OsString::from(toolchain.glibc),
            ]);
        }
        if self.start_files {
            args.extend(end);
        }
        args
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const TC: Toolchain<'static> = Toolchain {
        lld: "/td/store/rust/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
        glibc: "/td/store/glibc/lib",
        runtime: "/td/store/gcc-runtime/lib",
    };
    const GCC_LD: &str = "-B/td/store/rust/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld";

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn ld(args: &[&str]) -> Vec<OsString> {
        plan(os(args), &TC).unwrap().ld_args(&TC)
    }

    fn refused(args: &[&str]) -> String {
        plan(os(args), &TC).unwrap_err()
    }

    /// The tail every rustc line shares, as rustc 1.96.0 writes it.
    const RUSTC_TAIL: &[&str] = &[
        "-L",
        "/tmp/rustcX/raw-dylibs",
        GCC_LD,
        "-fuse-ld=lld",
        "-Wl,--eh-frame-hdr",
        "-Wl,-z,noexecstack",
        "-L",
        "/td/store/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib",
        "-o",
        "out",
    ];

    fn rustc_line(head: &[&str], tail: &[&str]) -> Vec<String> {
        let mut line = vec!["-m64".to_string(), "/tmp/rustcX/symbols.o".into()];
        line.extend(head.iter().map(|s| s.to_string()));
        line.extend(RUSTC_TAIL.iter().map(|s| s.to_string()));
        line.extend(tail.iter().map(|s| s.to_string()));
        line
    }

    fn strs(line: &[String]) -> Vec<&str> {
        line.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_rustc_dynamic_pie_line_becomes_glibc_start_files_and_static_libgcc() {
        let line = rustc_line(
            &[
                "m.rcgu.o",
                "-Wl,--as-needed",
                "-Wl,-Bstatic",
                "/r/libstd-1.rlib",
                "-Wl,-Bdynamic",
                "-lgcc_s",
                "-lutil",
                "-lc",
            ],
            &[
                "-Wl,--gc-sections",
                "-pie",
                "-Wl,-z,relro,-z,now",
                "-nodefaultlibs",
                "-Wl,--build-id=sha1",
            ],
        );
        assert_eq!(
            ld(&strs(&line)),
            os(&[
                "-m",
                "elf_x86_64",
                "--hash-style=gnu",
                "--build-id=sha1",
                "--eh-frame-hdr",
                "--no-dependent-libraries",
                "-pie",
                "-dynamic-linker",
                "/td/store/glibc/lib/ld-linux-x86-64.so.2",
                "-o",
                "out",
                "/td/store/glibc/lib/Scrt1.o",
                "/td/store/glibc/lib/crti.o",
                "/td/store/gcc-runtime/lib/crtbeginS.o",
                "/tmp/rustcX/symbols.o",
                "m.rcgu.o",
                "--as-needed",
                "-Bstatic",
                "/r/libstd-1.rlib",
                "-Bdynamic",
                "-lgcc_eh",
                "-lgcc",
                "-lutil",
                "-lc",
                "-L/tmp/rustcX/raw-dylibs",
                "--eh-frame-hdr",
                "-z",
                "noexecstack",
                "-L/td/store/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib",
                "--gc-sections",
                "-z",
                "relro",
                "-z",
                "now",
                "--build-id=sha1",
                "-L/td/store/glibc/lib",
                "-L/td/store/gcc-runtime/lib",
                "--disable-new-dtags",
                "-rpath",
                "/td/store/glibc/lib",
                "/td/store/gcc-runtime/lib/crtendS.o",
                "/td/store/glibc/lib/crtn.o",
            ])
        );
    }

    /// A cdylib or proc macro: rustc's version script, the unused-version
    /// check, a dylib's soname, and -O1 from an optimized build.
    #[test]
    fn a_rustc_shared_line_keeps_its_version_script_and_soname() {
        let line = rustc_line(
            &[
                "-Wl,--version-script=/tmp/rustcX/list",
                "-Wl,--no-undefined-version",
                "-Wl,-soname=libm.so",
                "-Wl,--whole-archive",
                "/r/liba.rlib",
                "-Wl,--no-whole-archive",
            ],
            &["-shared", "-Wl,-O1", "-nodefaultlibs"],
        );
        let args = ld(&strs(&line));
        for expected in [
            "--version-script=/tmp/rustcX/list",
            "--no-undefined-version",
            "-soname=libm.so",
            "--whole-archive",
            "--no-whole-archive",
            "-O1",
        ] {
            assert!(args.iter().any(|a| a == expected), "missing {expected}");
        }
        assert_eq!(args[HEADER.len()], "-shared");
    }

    fn start_and_end(args: &[OsString]) -> (Vec<&str>, Vec<&str>) {
        let crt: Vec<&str> = args
            .iter()
            .filter_map(|a| a.to_str())
            .filter(|a| a.ends_with("crt1.o") || a.contains("/crt"))
            .collect();
        let split = crt
            .iter()
            .position(|a| a.contains("crtend"))
            .unwrap_or(crt.len());
        (crt[..split].to_vec(), crt[split..].to_vec())
    }

    #[test]
    fn each_output_kind_takes_its_own_start_files_and_mode() {
        // (flags, output, mode, start files, end files)
        type Case = (
            &'static [&'static str],
            Output,
            &'static [&'static str],
            &'static [&'static str],
            &'static [&'static str],
        );
        let cases: &[Case] = &[
            (
                &["-static", "-no-pie"],
                Output::StaticExec,
                &["-static", "-no-pie"],
                &["crt1.o", "crti.o", "crtbeginT.o"],
                &["crtend.o", "crtn.o"],
            ),
            (
                &["-static-pie"],
                Output::StaticPie,
                &["-static", "-pie", "--no-dynamic-linker", "-z", "text"],
                &["rcrt1.o", "crti.o", "crtbeginS.o"],
                &["crtendS.o", "crtn.o"],
            ),
            (
                &["-no-pie"],
                Output::DynamicExec,
                &["-no-pie", "-dynamic-linker"],
                &["crt1.o", "crti.o", "crtbegin.o"],
                &["crtend.o", "crtn.o"],
            ),
            (
                &["-shared"],
                Output::Shared,
                &["-shared"],
                &["crti.o", "crtbeginS.o"],
                &["crtendS.o", "crtn.o"],
            ),
            (
                &[],
                Output::DynamicPie,
                &["-pie", "-dynamic-linker"],
                &["Scrt1.o", "crti.o", "crtbeginS.o"],
                &["crtendS.o", "crtn.o"],
            ),
        ];
        for (flags, output, mode, start, end) in cases {
            let mut line = vec!["a.o", "-nodefaultlibs", "-o", "out"];
            line.extend_from_slice(flags);
            let planned = plan(os(&line), &TC).unwrap();
            assert_eq!(planned.output, *output);
            let args = planned.ld_args(&TC);
            assert_eq!(
                &args[HEADER.len()..HEADER.len() + mode.len()],
                os(mode).as_slice(),
                "{output:?}"
            );
            let (got_start, got_end) = start_and_end(&args);
            let names = |paths: Vec<&str>| -> Vec<String> {
                paths
                    .iter()
                    .map(|p| p.rsplit('/').next().unwrap().to_string())
                    .collect()
            };
            assert_eq!(names(got_start), *start, "{output:?}");
            assert_eq!(names(got_end), *end, "{output:?}");
            let interp = args.iter().any(|a| a == "-dynamic-linker");
            let rpath = args.iter().any(|a| a == "-rpath");
            let old_dtags = args.iter().any(|a| a == "--disable-new-dtags");
            assert_eq!(rpath, old_dtags, "{output:?}");
            let dynamic = matches!(output, Output::DynamicPie | Output::DynamicExec);
            assert_eq!(interp, dynamic, "{output:?}");
            assert_eq!(rpath, dynamic || *output == Output::Shared, "{output:?}");
        }
    }

    #[test]
    fn rustc_static_lines_keep_its_libraries_and_order() {
        let line = rustc_line(
            &[
                "-Wl,-Bstatic",
                "/r/liblibc.rlib",
                "-lutil",
                "-lc",
                "-lgcc_eh",
                "-lgcc",
                "-lc",
                "/r/libcore.rlib",
            ],
            &[
                "-Wl,-Bdynamic",
                "-Wl,--gc-sections",
                "-static",
                "-no-pie",
                "-nodefaultlibs",
            ],
        );
        let args = ld(&strs(&line));
        let libs: Vec<&str> = args
            .iter()
            .filter_map(|a| a.to_str())
            .filter(|a| a.starts_with("-l"))
            .collect();
        assert_eq!(libs, ["-lutil", "-lc", "-lgcc_eh", "-lgcc", "-lc"]);
        assert_eq!(
            &args[HEADER.len()..HEADER.len() + 2],
            os(&["-static", "-no-pie"]).as_slice()
        );
    }

    #[test]
    fn start_files_follow_nostdlib_and_nostartfiles() {
        let bare = ld(&["a.o", "-nostdlib", "-o", "out"]);
        assert!(!bare
            .iter()
            .any(|a| a.as_bytes().ends_with(b".o") && a != "a.o"));
        let no_start = ld(&["a.o", "-nostartfiles", "-nodefaultlibs", "-o", "out"]);
        assert!(!no_start.iter().any(|a| a.as_bytes().ends_with(b"crti.o")));
        // td-link adds no library of its own.
        assert!(!no_start
            .iter()
            .any(|a| a.as_bytes().starts_with(b"-l") || a == "--start-group"));
    }

    #[test]
    fn option_spellings_join_or_take_the_next_argument() {
        let args = ld(&[
            "-nodefaultlibs",
            "-o",
            "x",
            "-l",
            "m",
            "-lpthread",
            "-L",
            "/d",
            "-L/e",
            "-Xlinker",
            "--gc-sections",
            "-Xlinker",
            "-z",
            "-Xlinker",
            "relro",
            "-z",
            "now",
            "-u",
            "sym",
            "-rdynamic",
            "a.o",
            "libb.a",
            "libc.so.6",
            "-ofinal",
        ]);
        for expected in [
            "-lm",
            "-lpthread",
            "-L/d",
            "-L/e",
            "--gc-sections",
            "relro",
            "now",
            "sym",
            "--export-dynamic",
            "libb.a",
            "libc.so.6",
        ] {
            assert!(args.iter().any(|a| a == expected), "missing {expected}");
        }
        let out = args.iter().position(|a| a == "-o").unwrap();
        assert_eq!(args[out + 1], "final");
    }

    #[test]
    fn non_utf8_arguments_pass_through_unchanged() {
        let input = OsString::from_vec(b"obj-\xff.o".to_vec());
        let dir = OsString::from_vec(b"-L/lib-\xfe".to_vec());
        let mut line = vec![input.clone(), dir.clone()];
        line.extend(os(&["-nodefaultlibs", "-o", "out"]));
        let args = plan(line, &TC).unwrap().ld_args(&TC);
        assert!(args.contains(&input));
        assert!(args.contains(&dir));
    }

    #[test]
    fn compile_requests_and_unknown_options_are_refused() {
        for (line, why) in [
            (&["-c", "a.c"][..], "does not compile"),
            (&["-E", "a.c"], "does not compile"),
            (&["-S", "a.c"], "does not compile"),
            (&["-x", "c", "a.o"], "does not compile"),
            (&["a.c", "-o", "a"], "does not compile"),
            (&["a.rs"], "does not compile"),
            (&["a.S"], "does not compile"),
            (&["-"], "standard input"),
            (&["a.o", "-O2"], "unsupported option"),
            (&["a.o", "-m32"], "unsupported option"),
            (&["a.o", "-fuse-ld=bfd"], "unsupported option"),
            (&["a.o", "-shared-libgcc"], "unsupported option"),
            (&["a.o", "-B/td/store/gcc/bin/"], "-B names"),
            (&["a.o", "-B", "/td/store/glibc/lib"], "-B names"),
            // Another toolchain's gcc-ld is not the compiled rust-lld's.
            (
                &[
                    "a.o",
                    "-B/elsewhere/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld",
                ],
                "-B names",
            ),
            (&["a.o", "-shared", "-static"], "conflicting"),
            (&["a.o", "-shared", "-pie"], "conflicting"),
            (&["a.o", "-static", "-pie"], "conflicting"),
            (&["a.o", "-static-pie", "-no-pie"], "conflicting"),
            (&["a.o", "-o"], "needs a value"),
            (&["a.o", "-l"], "needs a value"),
            // rustc always passes -nodefaultlibs and -o.
            (&["a.o", "-o", "out"], "-nodefaultlibs"),
            (&["a.o", "-nodefaultlibs"], "without -o"),
        ] {
            let error = refused(line);
            assert!(error.contains(why), "{line:?}: {error}");
        }
        // rustc's own self-contained directory is accepted, slash or not.
        let ok = |extra: &str| plan(os(&["a.o", "-nodefaultlibs", "-o", "out", extra]), &TC);
        assert!(ok(GCC_LD).is_ok());
        assert!(ok(&format!("{GCC_LD}/")).is_ok());
    }

    /// Linker options outside what rustc writes are refused, as is anything
    /// that names an interpreter, a response file LLD would read, or a host
    /// directory.
    #[test]
    fn linker_options_are_an_allowlist() {
        for (piece, why) in [
            ("-Wl,--dynamic-linker,/x", "dynamic linker"),
            ("-Wl,-dynamic-linker=/x", "dynamic linker"),
            ("-Wl,--no-dynamic-linker", "dynamic linker"),
            ("-Wl,-I/x", "dynamic linker"),
            ("-Wl,@/tmp/args", "response file"),
            ("-Wl,", "empty linker argument"),
            ("-Wl,-rpath,", "unsupported linker option"),
            ("-Wl,--as-needed,,-Bstatic", "empty linker argument"),
            ("-Wl,-rpath,/usr/lib", "unsupported linker option"),
            ("-Wl,-rpath=/td/store/x", "unsupported linker option"),
            ("-Wl,-T,/tmp/script", "unsupported linker option"),
            ("-Wl,--script=/tmp/script", "unsupported linker option"),
            ("-Wl,--sysroot=/", "unsupported linker option"),
            ("-Wl,--dynamic-list=/tmp/l", "unsupported linker option"),
            ("-Wl,-L/usr/lib", "unsupported linker option"),
            ("-Wl,-lgcc_s", "unsupported linker option"),
            (
                "-Wl,/usr/lib/x86_64-linux-gnu/libc.so.6",
                "unsupported linker option",
            ),
            ("-Wl,--build-id=md5", "unsupported linker option"),
            ("-Wl,-z,execstack", "-z keyword"),
            ("-Wl,-z", "-z needs its keyword"),
            ("-Wl,-soname=/x/y", "one file name"),
            ("-Wl,--version-script=/usr/share/v", "host system prefix"),
            ("-Wl,--version-script=/tmp/../etc/v", "host system prefix"),
        ] {
            let error = refused(&["a.o", "-nodefaultlibs", "-o", "out", piece]);
            assert!(error.contains(why), "{piece}: {error}");
        }
        for (line, why) in [
            (&["-Xlinker", "-I/x"][..], "dynamic linker"),
            (&["-Xlinker", "@f"], "response file"),
            (
                &["-Xlinker", "-rpath", "-Xlinker", "/x"],
                "unsupported linker option",
            ),
            (&["-z", "execstack"], "-z keyword"),
            // -z's keyword cannot be some other argument.
            (&["-Wl,-z", "a.o"], "-z needs its keyword"),
            (&["-Xlinker", "-z"], "-z needs its keyword"),
        ] {
            let mut full = vec!["b.o", "-nodefaultlibs", "-o", "out"];
            full.extend_from_slice(line);
            let error = refused(&full);
            assert!(error.contains(why), "{line:?}: {error}");
        }
    }

    #[test]
    fn host_libraries_and_search_paths_are_refused() {
        for (line, why) in [
            (&["-L/usr/lib/x86_64-linux-gnu"][..], "host system prefix"),
            (&["-L", "/lib64"], "host system prefix"),
            (&["-L/tmp/../usr/lib"], "host system prefix"),
            (
                &["/usr/lib/x86_64-linux-gnu/libc.so.6"],
                "host system prefix",
            ),
            (&["/lib/x86_64-linux-gnu/crt1.o"], "host system prefix"),
            (&["/gnu/store/x-glibc/lib/libc.a"], "host system prefix"),
            (&["/td/store/../../opt/x.a"], "host system prefix"),
        ] {
            let mut full = vec!["a.o", "-nodefaultlibs", "-o", "out"];
            full.extend_from_slice(line);
            let error = refused(&full);
            assert!(error.contains(why), "{line:?}: {error}");
        }
        // A prefix is a whole component: /usrx is not /usr.
        assert!(plan(
            os(&["/usrx/a.o", "-L/libfoo", "-nodefaultlibs", "-o", "out"]),
            &TC
        )
        .is_ok());
    }

    #[test]
    fn every_link_refuses_dependent_libraries() {
        for line in [
            &["a.o", "-nodefaultlibs", "-o", "out"][..],
            &["a.o", "-nodefaultlibs", "-static", "-o", "out"],
            &["a.o", "-nodefaultlibs", "-shared", "-o", "out"],
        ] {
            assert!(
                ld(line).contains(&OsString::from("--no-dependent-libraries")),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_sysroot_relative_path_is_refused() {
        for line in [
            &["-L=/usr/lib/x86_64-linux-gnu"][..],
            &["-L", "=/usr/lib"],
            &["-L$SYSROOT/usr/lib"],
            &["=/usr/lib/libssl.a"],
            &["-Wl,--version-script==/usr/x.map"],
        ] {
            let mut full = vec!["a.o", "-nodefaultlibs", "-o", "out"];
            full.extend_from_slice(line);
            let error = plan(os(&full), &TC).unwrap_err();
            assert!(error.contains("sysroot-relative"), "{line:?}: {error}");
        }
    }

    /// Only ELF objects and ordinary archives reach LLD from the line: a
    /// text input would be read as a linker script.
    #[test]
    fn a_linker_script_or_thin_archive_input_is_refused() {
        let dir = td_fs::TempDir::new("td-link-inputs").unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path.to_str().unwrap().to_string()
        };
        let object = write("a.o", b"\x7fELF\x02\x01\x01");
        let archive = write("libx.rlib", b"!<arch>\nmember");
        let script = write("libssl.so", b"INPUT(/usr/lib/libssl.so.3)\n");
        let thin = write("libt.a", b"!<thin>\n/usr/lib/x.o");
        let line =
            |input: &str| plan(os(&[&object, input, "-nodefaultlibs", "-o", "out"]), &TC).unwrap();
        assert!(line(&archive).check_inputs().is_ok());
        for bad in [&script, &thin] {
            let error = line(bad).check_inputs().unwrap_err();
            assert!(error.contains("linker script or thin archive"), "{error}");
        }
        let missing = dir.join("gone.o");
        assert!(line(missing.to_str().unwrap()).check_inputs().is_err());
    }

    #[test]
    fn a_symlink_into_a_host_prefix_is_refused() {
        let dir = td_fs::TempDir::new("td-link-host-link").unwrap();
        let link = dir.join("libz.so");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        if link.exists() {
            let error = refused(&[link.to_str().unwrap(), "-nodefaultlibs", "-o", "out"]);
            assert!(error.contains("host system prefix"), "{error}");
        }
    }

    /// Only `-lgcc_s` is rewritten to the static libgcc; every other way of
    /// naming the shared one is refused.
    #[test]
    fn the_shared_libgcc_is_refused_in_every_other_spelling() {
        for line in [
            &["-l:libgcc_s.so.1"][..],
            &["-l", ":libgcc_s.so"],
            &["-l:../../usr/lib/libz.so"],
            &["-lz/../../../usr/lib/x"],
            &["/td/store/gcc/lib/libgcc_s.so.1"],
            &["libgcc_s.so"],
            &["-Wl,-lgcc_s"],
            &["-Xlinker", "-l:libgcc_s.so.1"],
        ] {
            let mut full = vec!["a.o", "-nodefaultlibs", "-o", "out"];
            full.extend_from_slice(line);
            assert!(plan(os(&full), &TC).is_err(), "{line:?}");
        }
        assert!(plan(
            os(&["a.o", "-l:libz.a", "-lgcc", "-nodefaultlibs", "-o", "out"]),
            &TC
        )
        .is_ok());
    }
}
