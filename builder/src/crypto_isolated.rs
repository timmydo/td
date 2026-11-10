//! Fixed-path commands run only with the declared portable input mounts.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
use crate::crypto_musl::Architecture;
const BINARIES: &[&str] = &[
    "td-mta",
    "td-crypto-smoke",
    "td-mta-cli-smoke",
    "td-mta-config-smoke",
    "td-mta-format-smoke",
    "td-mta-transport-smoke",
    "td-mta-rust-allocation-probe",
    "td-mta-rss-probe",
];
const ARM_BINARIES: &[&str] = &[
    "td-mta",
    "td-crypto-smoke",
    "td-mta-config-smoke",
    "td-mta-format-smoke",
    "td-mta-transport-smoke",
];

fn binary_names(architecture: Architecture) -> &'static [&'static str] {
    match architecture {
        Architecture::X86_64 => BINARIES,
        Architecture::Aarch64 => ARM_BINARIES,
    }
}

/// The Debian build root's directories the compile namespace binds, each at
/// its path in the root: `/usr` holds GCC, binutils, and the glibc, libgcc_s
/// and zlib the upstream Rust host tools and build scripts run on; `/lib`
/// and `/lib64` are the root's usrmerge links, through which its loader
/// (`/lib64/ld-linux-x86-64.so.2`) and `libc.so` script name them. No td
/// glibc is mounted where the root's GCC would resolve an interpreter.
const BUILD_ROOT_BINDS: &[(&str, &str)] = &[
    ("usr", "/usr"),
    ("usr/lib", "/lib"),
    ("usr/lib64", "/lib64"),
];
const HOST_CC: &str = "/usr/bin/gcc";
const HOST_BINUTILS: &str = "/usr/bin/";

/// One target's binutils from the build root, by absolute path: `nm` lists
/// symbols, `objdump` disassembles the branch-free routines and `objcopy`
/// splits debug companions. Nothing compiles C.
struct Tools {
    nm: &'static str,
    objdump: &'static str,
    objcopy: &'static str,
}

fn tools(architecture: Architecture) -> Tools {
    match architecture {
        Architecture::X86_64 => Tools {
            nm: "/usr/bin/nm",
            objdump: "/usr/bin/objdump",
            objcopy: "/usr/bin/objcopy",
        },
        Architecture::Aarch64 => Tools {
            nm: "/usr/bin/aarch64-linux-gnu-nm",
            objdump: "/usr/bin/aarch64-linux-gnu-objdump",
            objcopy: "/usr/bin/aarch64-linux-gnu-objcopy",
        },
    }
}

#[cfg(test)]
pub(crate) fn rust_flags() -> String {
    rust_flags_architecture(Architecture::X86_64)
}

pub(crate) fn rust_flags_architecture(architecture: Architecture) -> String {
    let mut flags = vec![
        "-C",
        "linker=/rust/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
        "-C",
        "linker-flavor=ld.lld",
        "-C",
        "target-feature=+crt-static",
        "-C",
        match architecture {
            Architecture::X86_64 => "target-cpu=x86-64",
            Architecture::Aarch64 => "target-cpu=generic",
        },
        "-C",
        "force-frame-pointers=yes",
        "-C",
        "link-arg=--build-id=sha1",
        "-C",
        "debuginfo=1",
        "--remap-path-prefix=/source=/td-build",
        "--remap-path-prefix=/vendor=/td-cargo/vendor",
        "--remap-path-prefix=/output=/td-build-root",
    ];
    if architecture == Architecture::Aarch64 {
        // Rust's ARM target defaults to ET_EXEC. Select the pinned musl
        // static-PIE startup objects explicitly, using stable linker flags.
        flags.extend([
            "-C",
            "link-self-contained=no",
            "-C",
            "relocation-model=pic",
            "-L",
            "/rust/lib/rustlib/aarch64-unknown-linux-musl/lib/self-contained",
            "-C",
            "link-arg=-pie",
            "-C",
            "link-arg=--no-dynamic-linker",
            "-C",
            "link-arg=/rust/lib/rustlib/aarch64-unknown-linux-musl/lib/self-contained/rcrt1.o",
            "-C",
            "link-arg=/rust/lib/rustlib/aarch64-unknown-linux-musl/lib/self-contained/crti.o",
            "-C",
            "link-arg=/rust/lib/rustlib/aarch64-unknown-linux-musl/lib/self-contained/crtbeginS.o",
            "-C",
            "link-arg=/rust/lib/rustlib/aarch64-unknown-linux-musl/lib/self-contained/crtendS.o",
            "-C",
            "link-arg=/rust/lib/rustlib/aarch64-unknown-linux-musl/lib/self-contained/crtn.o",
        ]);
    }
    flags.join("\x1f")
}

pub(crate) fn cargo(verb: &str, package: &str, architecture: Architecture) -> Command {
    cargo_manifest_architecture(
        verb,
        Path::new(&format!("/source/{package}/Cargo.toml")),
        architecture,
    )
}

pub(crate) fn cargo_manifest_architecture(
    verb: &str,
    manifest: &Path,
    architecture: Architecture,
) -> Command {
    let mut command = Command::new("/rust/bin/cargo");
    command
        .current_dir("/tmp")
        .env_clear()
        .args([
            verb,
            "--frozen",
            "--target",
            architecture.triple(),
            "--manifest-path",
        ])
        .arg(manifest)
        .args([
            "--config",
            "source.crates-io.replace-with=\"td-crypto-vendor\"",
            "--config",
            "source.td-crypto-vendor.directory=\"/vendor\"",
        ])
        .env("HOME", "/tmp/home")
        .env("CARGO_HOME", "/tmp/home")
        .env("CARGO_TARGET_DIR", "/output/target")
        .env("PATH", "/rust/bin")
        .env("RUSTC", "/rust/bin/rustc")
        .env("RUSTDOC", "/rust/bin/rustdoc")
        .env("CARGO_BUILD_JOBS", "2")
        .env("SOURCE_DATE_EPOCH", "0")
        .env("TD_HOST_SANDBOX", "1")
        .env("CARGO_PROFILE_RELEASE_STRIP", "none")
        .env("CARGO_PROFILE_BENCH_STRIP", "none")
        .env(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
            "/td-crypto-host-linker",
        )
        .env(
            "CARGO_ENCODED_RUSTFLAGS",
            rust_flags_architecture(architecture),
        )
        .env("CMAKE", "/td-crypto-decoy")
        .env("PKG_CONFIG", "/td-crypto-decoy")
        .env("OPENSSL_DIR", "/undeclared-openssl")
        .stdin(Stdio::null());
    // No admitted package compiles C: a build script reaching for a C
    // compiler or archiver, for either target, runs the decoy and fails.
    for suffix in [
        String::new(),
        format!("_{}", architecture.triple()),
        format!("_{}", architecture.triple().replace('-', "_")),
        "_x86_64-unknown-linux-gnu".to_string(),
        "_x86_64_unknown_linux_gnu".to_string(),
    ] {
        command
            .env(format!("CC{suffix}"), "/td-crypto-decoy")
            .env(format!("AR{suffix}"), "/td-crypto-decoy");
    }
    for key in ["HOST_CC", "HOST_AR", "TARGET_CC", "TARGET_AR"] {
        command.env(key, "/td-crypto-decoy");
    }
    crate::host_bin::arm_check_child(&mut command);
    command
}

pub(crate) fn bounded_output(
    command: &mut Command,
    name: &str,
    limit: u64,
    seconds: u64,
) -> Result<String> {
    bounded_exit_output(command, name, limit, seconds, 0)
}

pub(crate) fn bounded_exit_output(
    command: &mut Command,
    name: &str,
    limit: u64,
    seconds: u64,
    expected: i32,
) -> Result<String> {
    let path = Path::new("/output").join(format!("{name}.log"));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("create portable command log: {e}"))?;
    command.stdout(file);
    let mut child = command.spawn().map_err(|e| format!("start {name}: {e}"))?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or("portable command deadline overflow")?;
    let _ = crate::host_bin::wait_with_deadline(&mut child, Some(deadline));
    let status = child
        .try_wait()
        .map_err(|e| format!("inspect {name} status: {e}"))?;
    if Instant::now() >= deadline || status.and_then(|status| status.code()) != Some(expected) {
        return Err(format!("portable {name} failed or exceeded its deadline"));
    }
    read_output(&path, name, limit)
}

pub(crate) fn read_output(path: &Path, name: &str, limit: u64) -> Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| format!("open command output: {e}"))?
        .take(limit.checked_add(1).ok_or("output limit overflow")?)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read command output: {e}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("portable {name} output exceeds its limit"));
    }
    String::from_utf8(bytes).map_err(|e| format!("portable {name} output is not UTF-8: {e}"))
}

/// `main`'s argv0 dispatch of the linker applet, whole, so `main` names this
/// host-only module in its dispatch arm alone (engine_set.rs).
pub(crate) fn host_linker_applet(args: &[String]) -> std::process::ExitCode {
    let linked = host_linker(args.get(1..).unwrap_or(&[])).map(|()| 0);
    crate::applet_exit("td-crypto-host-linker", linked)
}

/// The decoy applet's dispatch, as [`host_linker_applet`].
pub(crate) fn decoy_applet() -> std::process::ExitCode {
    crate::applet_exit("td-crypto-decoy", decoy().map(|()| 0))
}

/// Build scripts and proc macros link with the root's GCC against the
/// root's own glibc, whose loader the namespace mounts at its usual path.
pub(crate) fn host_linker(args: &[String]) -> Result<()> {
    require_namespace(&[HOST_CC, "/usr/bin/ld", "/lib64/ld-linux-x86-64.so.2"])?;
    let mut command = Command::new(HOST_CC);
    command.arg(format!("-B{HOST_BINUTILS}")).args(args);
    crate::host_bin::arm_check_child(&mut command);
    let status = command
        .status()
        .map_err(|e| format!("portable host linker: {e}"))?;
    if !status.success() {
        return Err(format!("portable host linker failed ({status})"));
    }
    Ok(())
}

pub(crate) fn decoy() -> Result<()> {
    require_namespace(&["/output"])?;
    record_decoy(Path::new("/output"))
}

fn record_decoy(output: &Path) -> Result<()> {
    fs::write(
        output.join("native-fallback-called"),
        b"unapproved native build fallback\n",
    )
    .map_err(|e| format!("record native fallback: {e}"))?;
    Err("portable build invoked an unapproved native tool".into())
}

pub(crate) fn artifact_json(line: &str) -> Result<td_engine::json::Json> {
    if line.len() > 256 * 1024 {
        return Err("Cargo artifact record exceeds 256 KiB".into());
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in line.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 64 {
                        return Err("Cargo artifact JSON nesting exceeds 64".into());
                    }
                }
                b'}' | b']' => depth = depth.checked_sub(1).ok_or("unbalanced Cargo JSON")?,
                _ => {}
            }
        }
    }
    td_engine::json::parse(line).map_err(|e| format!("Cargo artifact JSON: {e}"))
}

#[derive(Clone, Copy)]
pub(crate) enum ArtifactKind {
    Installed,
    LibraryTest,
    BinaryTest,
    IntegrationTest,
}
impl ArtifactKind {
    fn target(self) -> &'static str {
        match self {
            Self::Installed | Self::BinaryTest => "bin",
            Self::LibraryTest => "lib",
            Self::IntegrationTest => "test",
        }
    }
    fn test(self) -> bool {
        !matches!(self, Self::Installed)
    }
}

pub(crate) fn artifact_path(
    message: &str,
    package: &str,
    expected: ArtifactKind,
) -> Result<PathBuf> {
    let mut executable = None;
    for line in message.lines() {
        let value = artifact_json(line)?;
        if value.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let target = value.get("target").ok_or("Cargo artifact has no target")?;
        if target.get("name").and_then(|v| v.as_str()) != Some(package) {
            continue;
        }
        let expected_kind = expected.target();
        let kind = target.get("kind").and_then(|v| v.as_arr());
        let profile_test = value.get("profile").and_then(|v| v.get("test"));
        if !matches!(kind, Some([v]) if v.as_str() == Some(expected_kind))
            || profile_test != Some(&td_engine::json::Json::Bool(expected.test()))
        {
            return Err(format!("unexpected artifact kind/profile for {package}"));
        }
        let Some(path) = value.get("executable").and_then(|v| v.as_str()) else {
            continue;
        };
        if executable.replace(std::path::PathBuf::from(path)).is_some() {
            return Err(format!("ambiguous executable for {package}"));
        }
    }
    executable.ok_or_else(|| format!("no executable for {package}"))
}

fn executable(message: &str, package: &str, kind: ArtifactKind) -> Result<PathBuf> {
    executable_for(message, package, kind, Architecture::X86_64)
}

fn executable_for(
    message: &str,
    package: &str,
    kind: ArtifactKind,
    architecture: Architecture,
) -> Result<PathBuf> {
    let actual = confined_path(
        &artifact_path(message, package, kind)?,
        Path::new("/output/target"),
    )?;
    qualify_binary_architecture(&actual, architecture)?;
    Ok(actual)
}

fn confined_path(path: &Path, parent: &Path) -> Result<PathBuf> {
    let actual = path
        .canonicalize()
        .map_err(|e| format!("resolve Cargo executable: {e}"))?;
    if !actual.starts_with(parent) {
        return Err("Cargo executable escaped its output directory".into());
    }
    Ok(actual)
}

fn executable_architecture(path: &Path, architecture: Architecture) -> Result<()> {
    match architecture {
        Architecture::X86_64 => {
            crate::elf::assert_x86_64_executable_bounded(path, 256 * 1024 * 1024)
        }
        Architecture::Aarch64 => {
            crate::elf::assert_aarch64_executable_bounded(path, 256 * 1024 * 1024)
        }
    }
}

fn qualify_binary_architecture(path: &Path, architecture: Architecture) -> Result<()> {
    executable_architecture(path, architecture)?;
    crate::elf::assert_static_pie(path)?;
    crate::elf::assert_symbol_table(path)
}

fn split_mail_debug(receipt: &mut String) -> Result<()> {
    split_mail_debug_architecture(receipt, Architecture::X86_64)
}

fn split_mail_debug_architecture(receipt: &mut String, architecture: Architecture) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let runtime = Path::new("/output/artifacts/td-mta");
    let debug = Path::new("/output/artifacts/td-mta.debug");
    crate::elf::read_build_id(runtime)?;
    let mut commands = vec![vec![
        "--only-keep-debug".to_owned(),
        path_text(runtime)?,
        path_text(debug)?,
    ]];
    let mut prune: Vec<String> = td_engine::target_profile::PRUNED_DEBUG_SECTIONS
        .iter()
        .map(|section| format!("--remove-section={section}"))
        .collect();
    prune.push(path_text(debug)?);
    commands.push(prune);
    commands.push(vec!["--strip-all".into(), path_text(runtime)?]);
    for (index, args) in commands.iter().enumerate() {
        let mut command = Command::new(tools(architecture).objcopy);
        command.args(args).env_clear().stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("mail-debug-{index}");
        command_record(&command, &name, receipt)?;
        bounded_output(&mut command, &name, 4096, 30)?;
    }
    fs::set_permissions(debug, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("set portable debug mode: {e}"))?;
    crate::elf::assert_debug_pair(runtime, debug)
}

/// td-primitives routines whose compiled form must hold no conditional
/// branch at all, by demangled name (td-crypto/DESIGN.md, "Compiled
/// constant-time form"). Each is `#[inline(never)]` in td-primitives, so its
/// symbol is the body that runs, and must be a symbol of the receiver: a
/// rename, or an inlining that leaves no symbol, fails rather than shrinking
/// the check. `Modulus::mul` is not listed: it is inlined into its callers,
/// and an out-of-line copy would say nothing of them.
const BRANCH_FREE: &[&str] = &[
    "<td_primitives::ec::Curve<4>>::add",
    "<td_primitives::ec::Curve<6>>::add",
    "td_primitives::aes_gcm::sbox",
    "td_primitives::aes_gcm::ortho",
    "td_primitives::aes_gcm::shift_rows",
    "<td_primitives::aes_gcm::HashKey>::absorb",
];

/// What else a branch-free routine may call or jump to: the slice-generic
/// Montgomery multiplication, whose branches count its loops over public
/// lengths. It was reviewed, and its body is not gated.
const COUNTED_CALLEES: &[&str] = &["td_primitives::bignum::mont_mul"];

/// One `nm --defined-only --demangle --print-size` text symbol.
struct Symbol<'a> {
    address: u64,
    size: u64,
    name: &'a str,
}

/// Every defined text symbol with a size.
fn text_symbols(symbols: &str) -> Result<Vec<Symbol<'_>>> {
    let mut found = Vec::new();
    for line in symbols.lines() {
        let mut fields = line.splitn(4, ' ');
        let (Some(address), Some(size), Some(kind), Some(name)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !matches!(kind, "t" | "T") {
            continue;
        }
        let parse = |text: &str| {
            u64::from_str_radix(text, 16).map_err(|_| format!("malformed symbol line: {line}"))
        };
        found.push(Symbol {
            address: parse(address)?,
            size: parse(size)?,
            name,
        });
    }
    Ok(found)
}

/// One `objdump --disassemble --insn-width=16` instruction line.
struct Instruction<'a> {
    address: u64,
    /// Bytes, from the raw encoding objdump prints beside it.
    length: u64,
    /// Past any prefix objdump prints as a word.
    mnemonic: &'a str,
    operand: &'a str,
}

/// An instruction line, `ADDRESS:\tBYTES\tMNEMONIC OPERANDS`, or `None` for
/// any other line (headers, labels, blanks). A line with an address and no
/// mnemonic, a wrapped encoding, is an error rather than skipped, so the
/// byte count stays exact.
fn instruction(line: &str) -> Option<Result<Instruction<'_>>> {
    let (address, rest) = line.trim_start().split_once(":\t")?;
    let address = u64::from_str_radix(address, 16).ok()?;
    let (bytes, text) = match rest.split_once('\t') {
        Some((bytes, text)) => (bytes, text),
        None => return Some(Err(format!("wrapped or bare instruction at {address:#x}"))),
    };
    let digits = bytes.chars().filter(|c| !c.is_whitespace()).count();
    if digits == 0
        || digits % 2 != 0
        || !bytes
            .trim()
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ' ')
    {
        return Some(Err(format!("unreadable encoding at {address:#x}")));
    }
    let mut words = text.split_whitespace();
    let mut mnemonic = words.next().unwrap_or_default();
    // The prefixes rustc's code generation and LLD emit and objdump prints as
    // words; another would be read as the mnemonic and fail as unrecognised
    // only if it is a branch form below.
    while matches!(
        mnemonic,
        "bnd"
            | "notrack"
            | "lock"
            | "rep"
            | "repz"
            | "repnz"
            | "repe"
            | "repne"
            | "data16"
            | "cs"
            | "ds"
            | "es"
            | "ss"
            | "fs"
            | "gs"
            | "addr32"
    ) {
        mnemonic = words.next().unwrap_or_default();
    }
    Some(Ok(Instruction {
        address,
        length: (digits / 2) as u64,
        mnemonic,
        operand: words.next().unwrap_or_default(),
    }))
}

/// How the gate reads one instruction.
#[derive(Debug, PartialEq)]
enum Flow {
    /// Falls through, or returns.
    Straight,
    /// Branches on a flag or register value.
    Conditional,
    /// A call or jump: `Some(address)` when direct.
    Transfer(Option<u64>),
    /// Neither decodable as an instruction nor a branch form the gate knows,
    /// which fails rather than passing as straight-line code.
    Unrecognised,
}

/// aarch64 data-processing mnemonics that begin with `b`; anything else that
/// begins with `b`, or with `ret` or `eret`, is a branch form.
const AARCH64_B_DATA: &[&str] = &[
    "bic", "bics", "bif", "bit", "bsl", "bfi", "bfxil", "bfm", "bfc", "bcax", "bfcvt", "bfcvtn",
    "bfcvtn2", "bfdot", "bfmlalb", "bfmlalt", "bfmmla", "bti",
];

/// x86's `jcc`, `jrcxz` and `loop` family are conditional; `call` and `jmp`
/// transfer. aarch64's `b.cond`, `bc.cond`, `cbz`, `cbnz`, `tbz`, `tbnz` and
/// every other `cb` form are conditional (FEAT_CMPBR's `cb<cc>` is one, should
/// `target-cpu` ever be raised to emit it); `b` and `bl` transfer directly,
/// `br` and `blr` indirectly; `ret` returns. Pointer-authenticated forms
/// (`braa`, `blraa`, `retaa` and the rest) and any other `b`, `br`, `bl` or
/// `ret` form are unrecognised.
fn flow(instruction: &Instruction<'_>, architecture: Architecture) -> Flow {
    let mnemonic = instruction.mnemonic;
    if mnemonic.is_empty() || mnemonic.starts_with('.') || mnemonic.starts_with('(') {
        return Flow::Unrecognised;
    }
    let direct = || {
        let target = instruction.operand.trim_start_matches("0x");
        Flow::Transfer(u64::from_str_radix(target, 16).ok())
    };
    match architecture {
        Architecture::X86_64 => {
            if (mnemonic.starts_with('j') && !mnemonic.starts_with("jmp"))
                || mnemonic.starts_with("loop")
            {
                Flow::Conditional
            } else if mnemonic.starts_with("call") || mnemonic.starts_with("jmp") {
                direct()
            } else {
                Flow::Straight
            }
        }
        Architecture::Aarch64 => {
            if mnemonic.starts_with("b.")
                || mnemonic.starts_with("bc.")
                || mnemonic.starts_with("cb")
                || matches!(mnemonic, "tbz" | "tbnz")
            {
                Flow::Conditional
            } else if matches!(mnemonic, "b" | "bl") {
                direct()
            } else if matches!(mnemonic, "br" | "blr") {
                Flow::Transfer(None)
            } else if mnemonic == "ret" {
                Flow::Straight
            } else if mnemonic.starts_with("ret")
                || mnemonic.starts_with("eret")
                || (mnemonic.starts_with('b') && !AARCH64_B_DATA.contains(&mnemonic))
            {
                Flow::Unrecognised
            } else {
                Flow::Straight
            }
        }
    }
}

/// Check one routine's disassembly of [start, start + size): its
/// instructions must run back to back from `start` to exactly the end, each
/// one recognised, with no conditional branch, and every call or jump
/// leaving it must reach a branch-free routine or a counted callee by a
/// direct address. Returns the instruction count and the calls leaving it.
fn branch_free(
    disassembly: &str,
    architecture: Architecture,
    symbols: &[Symbol<'_>],
    name: &str,
    start: u64,
    size: u64,
) -> Result<(usize, usize)> {
    let end = start.checked_add(size).ok_or("symbol range overflows")?;
    let instructions = disassembly
        .lines()
        .filter_map(instruction)
        .collect::<Result<Vec<_>>>()
        .map_err(|e| format!("{name}: {e}"))?;
    let mut next = start;
    let mut branches = Vec::new();
    let mut calls = 0usize;
    for instruction in &instructions {
        let (address, mnemonic) = (instruction.address, instruction.mnemonic);
        if address != next {
            return Err(format!(
                "{name}: instruction at {address:#x} where {next:#x} was due: the listing does not cover it exactly"
            ));
        }
        next = address
            .checked_add(instruction.length)
            .filter(|after| *after <= end)
            .ok_or_else(|| format!("{name}: instruction at {address:#x} lies outside it"))?;
        let target = match flow(instruction, architecture) {
            Flow::Straight => continue,
            Flow::Conditional => {
                branches.push(format!("{mnemonic} at {address:#x}"));
                continue;
            }
            Flow::Unrecognised => {
                return Err(format!(
                    "{name}: unrecognised instruction {mnemonic} at {address:#x}"
                ));
            }
            Flow::Transfer(target) => {
                target.ok_or_else(|| format!("{name}: indirect {mnemonic} at {address:#x}"))?
            }
        };
        if (start..end).contains(&target) {
            continue;
        }
        calls += 1;
        let allowed = symbols.iter().any(|symbol| {
            symbol.address == target
                && (BRANCH_FREE.contains(&symbol.name) || COUNTED_CALLEES.contains(&symbol.name))
        });
        if !allowed {
            return Err(format!(
                "{name}: {mnemonic} at {address:#x} reaches {target:#x}, which is neither branch-free nor a counted callee"
            ));
        }
    }
    if instructions.is_empty() || next != end {
        return Err(format!(
            "{name}: the listing stops at {next:#x}, short of its end at {end:#x}"
        ));
    }
    if !branches.is_empty() {
        return Err(format!(
            "{name} must hold no conditional branch, and holds {}: {}",
            branches.len(),
            branches.join(", ")
        ));
    }
    Ok((instructions.len(), calls))
}

/// Disassemble each branch-free routine of the unstripped receiver with the
/// build root's binutils and refuse a conditional branch in it or a call
/// out of it to anything unreviewed.
fn branch_free_routines(receipt: &mut String, architecture: Architecture) -> Result<()> {
    let receiver = "/output/artifacts/td-mta";
    let mut command = Command::new(tools(architecture).nm);
    command
        .args(["--defined-only", "--demangle", "--print-size", receiver])
        .env_clear()
        .stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    command_record(&command, "branch-free-symbols", receipt)?;
    let listing = bounded_output(&mut command, "branch-free-symbols", 64 * 1024 * 1024, 30)?;
    let symbols = text_symbols(&listing)?;
    let mut index = 0usize;
    for name in BRANCH_FREE {
        let copies: Vec<&Symbol<'_>> = symbols.iter().filter(|s| s.name == *name).collect();
        if copies.is_empty() {
            return Err(format!("{name} is not a symbol of the receiver"));
        }
        for symbol in copies {
            let end = symbol
                .address
                .checked_add(symbol.size)
                .filter(|_| symbol.size > 0)
                .ok_or_else(|| format!("{name} has no extent"))?;
            let mut command = Command::new(tools(architecture).objdump);
            command
                .args([
                    "--disassemble",
                    "--insn-width=16",
                    &format!("--start-address={:#x}", symbol.address),
                    &format!("--stop-address={end:#x}"),
                    receiver,
                ])
                .env_clear()
                .stdin(Stdio::null());
            crate::host_bin::arm_check_child(&mut command);
            let label = format!("branch-free-{index}");
            index += 1;
            command_record(&command, &label, receipt)?;
            let disassembly = bounded_output(&mut command, &label, 4 * 1024 * 1024, 30)?;
            let (count, calls) = branch_free(
                &disassembly,
                architecture,
                &symbols,
                name,
                symbol.address,
                symbol.size,
            )?;
            println!(
                "portable constant-time form: {name} at {:#x}: {count} instructions, no conditional branch, {calls} calls out",
                symbol.address
            );
        }
    }
    Ok(())
}

fn qualify_artifacts_architecture(directory: &Path, architecture: Architecture) -> Result<()> {
    for binary in binary_names(architecture) {
        let path = directory.join(binary);
        if *binary == "td-mta" {
            executable_architecture(&path, architecture)?;
            crate::elf::assert_static_pie(&path)?;
            crate::elf::assert_debug_pair(&path, &directory.join("td-mta.debug"))?;
        } else {
            qualify_binary_architecture(&path, architecture)?;
        }
    }
    Ok(())
}

fn require_namespace(paths: &[&str]) -> Result<()> {
    if std::env::var_os("TD_HOST_SANDBOX").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Err("portable inner commands require the owned host sandbox".into());
    }
    for path in paths {
        fs::metadata(path).map_err(|e| format!("portable namespace lacks {path}: {e}"))?;
    }
    Ok(())
}

pub(crate) fn command_record(command: &Command, name: &str, receipt: &mut String) -> Result<()> {
    receipt.push_str(&format!(
        "command {name} {:?} {:?} cwd={:?}\n",
        command.get_program(),
        command.get_args().collect::<Vec<_>>(),
        command.get_current_dir()
    ));
    for (key, value) in command.get_envs() {
        let value = value.ok_or("portable environment unexpectedly removes a variable")?;
        receipt.push_str(&format!("env {key:?}={value:?}\n"));
    }
    Ok(())
}

fn refuse_decoy(output: &Path) -> Result<()> {
    match fs::symlink_metadata(output.join("native-fallback-called")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect native fallback marker: {e}")),
        Ok(_) => Err("native fallback decoy was invoked".into()),
    }
}

pub(crate) fn build_inner() -> Result<()> {
    require_namespace(&[
        "/output",
        "/source/td-mta/Cargo.toml",
        "/vendor",
        "/rust/bin/cargo",
        HOST_CC,
        "/usr/bin/nm",
        "/usr/bin/objdump",
        "/usr/bin/objcopy",
        "/lib64/ld-linux-x86-64.so.2",
    ])?;
    fs::create_dir_all("/tmp/home").map_err(|e| format!("portable Cargo home: {e}"))?;
    let architecture = Architecture::X86_64;
    let mut receipt = String::from("cargo-environment cleared\n");
    for package in ["td-crypto", "td-mta"] {
        let mut command = cargo("tree", package, architecture);
        command.args([
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--no-dedupe",
            "--format",
            "{p}|{f}",
        ]);
        let name = format!("{package}-graph");
        command_record(&command, &name, &mut receipt)?;
        let graph = bounded_output(&mut command, &name, 256 * 1024, 1200)?;
        crate::crypto_policy::active_graph(Path::new("/source"), package, &graph)?;
    }
    let mut command = cargo("build", "td-mta", architecture);
    command.args([
        "--release",
        "--bin",
        "td-mta",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "mail-build", &mut receipt)?;
    let output = bounded_output(&mut command, "mail-build", 8 * 1024 * 1024, 1200)?;
    let mail = executable(&output, "td-mta", ArtifactKind::Installed)?;
    let mut command = cargo("test", "td-mta", architecture);
    command.args([
        "--release",
        "--test",
        "operator_cli",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command.env("TD_MTA_PORTABLE_CLI", "1");
    command_record(&command, "mail-cli-build", &mut receipt)?;
    let output = bounded_output(&mut command, "mail-cli-build", 8 * 1024 * 1024, 1200)?;
    let cli = executable(&output, "operator_cli", ArtifactKind::IntegrationTest)?;
    let mut command = cargo("test", "td-crypto", architecture);
    command.args([
        "--release",
        "--lib",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "crypto-build", &mut receipt)?;
    let output = bounded_output(&mut command, "crypto-build", 8 * 1024 * 1024, 1200)?;
    let crypto = executable(&output, "td_crypto", ArtifactKind::LibraryTest)?;
    let mut command = cargo("test", "td-mta", architecture);
    command.args([
        "--release",
        "--test",
        "config_stack",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "config-stack-build", &mut receipt)?;
    let output = bounded_output(&mut command, "config-stack-build", 8 * 1024 * 1024, 1200)?;
    let config = executable(&output, "config_stack", ArtifactKind::IntegrationTest)?;
    let mut command = cargo("test", "td-mta", architecture);
    command.args([
        "--release",
        "--test",
        "format_rows",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "format-digest-build", &mut receipt)?;
    let output = bounded_output(&mut command, "format-digest-build", 8 * 1024 * 1024, 1200)?;
    let format = executable(&output, "format_rows", ArtifactKind::IntegrationTest)?;
    let mut command = cargo("test", "td-mta", architecture);
    command.args([
        "--release",
        "--lib",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "mail-transport-build", &mut receipt)?;
    let output = bounded_output(&mut command, "mail-transport-build", 8 * 1024 * 1024, 1200)?;
    let transport = executable(&output, "td_mta", ArtifactKind::LibraryTest)?;
    let mut command = cargo("test", "td-mta", architecture);
    command.args([
        "--release",
        "--test",
        "rust_alloc_probe",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "rust-allocation-build", &mut receipt)?;
    let output = bounded_output(&mut command, "rust-allocation-build", 8 * 1024 * 1024, 1200)?;
    let allocation = executable(&output, "rust_alloc_probe", ArtifactKind::IntegrationTest)?;
    let mut command = cargo("test", "td-mta", architecture);
    command.args([
        "--release",
        "--test",
        "rss_probe",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "rss-probe-build", &mut receipt)?;
    let output = bounded_output(&mut command, "rss-probe-build", 8 * 1024 * 1024, 1200)?;
    let rss = executable(&output, "rss_probe", ArtifactKind::IntegrationTest)?;
    for (index, (binary, probe)) in [
        (&mail, false),
        (&cli, false),
        (&crypto, false),
        (&config, false),
        (&format, false),
        (&transport, false),
        (&allocation, true),
        (&rss, false),
    ]
    .into_iter()
    .enumerate()
    {
        let mut command = Command::new(tools(Architecture::X86_64).nm);
        command
            .arg("--defined-only")
            .arg(binary)
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("allocator-symbols-{index}");
        command_record(&command, &name, &mut receipt)?;
        let symbols = bounded_output(&mut command, &name, 8 * 1024 * 1024, 30)?;
        allocation_symbols(&symbols, probe)?;
        foreign_symbols(&symbols)?;
        rss_symbols(&symbols, binary == &rss)?;
    }
    refuse_decoy(Path::new("/output"))?;
    fs::create_dir("/output/artifacts").map_err(|e| format!("portable artifacts: {e}"))?;
    copy_binary(&mail, Path::new("/output/artifacts/td-mta"))?;
    copy_binary(&cli, Path::new("/output/artifacts/td-mta-cli-smoke"))?;
    copy_binary(&crypto, Path::new("/output/artifacts/td-crypto-smoke"))?;
    copy_binary(&config, Path::new("/output/artifacts/td-mta-config-smoke"))?;
    copy_binary(&format, Path::new("/output/artifacts/td-mta-format-smoke"))?;
    copy_binary(
        &transport,
        Path::new("/output/artifacts/td-mta-transport-smoke"),
    )?;
    copy_binary(
        &allocation,
        Path::new("/output/artifacts/td-mta-rust-allocation-probe"),
    )?;
    copy_binary(&rss, Path::new("/output/artifacts/td-mta-rss-probe"))?;
    branch_free_routines(&mut receipt, architecture)?;
    split_mail_debug(&mut receipt)?;
    crate::crypto_api::qualify(&mut receipt, architecture)?;
    refuse_decoy(Path::new("/output"))?;
    write_new(Path::new("/output/artifacts/COMMANDS"), receipt.as_bytes())?;
    Ok(())
}

pub(crate) fn cross_build_inner() -> Result<()> {
    let architecture = Architecture::Aarch64;
    require_namespace(&[
        "/output",
        "/source/td-mta/Cargo.toml",
        "/vendor",
        "/rust/bin/cargo",
        HOST_CC,
        "/usr/bin/aarch64-linux-gnu-nm",
        "/usr/bin/aarch64-linux-gnu-objdump",
        "/usr/bin/aarch64-linux-gnu-objcopy",
        "/lib64/ld-linux-x86-64.so.2",
    ])?;
    fs::create_dir_all("/tmp/home").map_err(|e| format!("portable Cargo home: {e}"))?;
    let mut receipt = String::from("cargo-environment cleared\nruntime not-run cross-compiled\n");
    for package in ["td-crypto", "td-mta"] {
        let manifest = format!("/source/{package}/Cargo.toml");
        let mut command = cargo_manifest_architecture("tree", Path::new(&manifest), architecture);
        command.args([
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--no-dedupe",
            "--format",
            "{p}|{f}",
        ]);
        let name = format!("{package}-graph");
        command_record(&command, &name, &mut receipt)?;
        let graph = bounded_output(&mut command, &name, 256 * 1024, 1200)?;
        crate::crypto_policy::active_graph(Path::new("/source"), package, &graph)?;
    }
    fs::create_dir("/output/artifacts").map_err(|e| format!("portable artifacts: {e}"))?;
    for (package, name, target_args, kind, installed) in [
        (
            "td-mta",
            "td-mta",
            &["--bin", "td-mta"][..],
            ArtifactKind::Installed,
            "td-mta",
        ),
        (
            "td-crypto",
            "td_crypto",
            &["--lib"][..],
            ArtifactKind::LibraryTest,
            "td-crypto-smoke",
        ),
        (
            "td-mta",
            "td_mta",
            &["--lib"][..],
            ArtifactKind::LibraryTest,
            "td-mta-transport-smoke",
        ),
        (
            "td-mta",
            "config_stack",
            &["--test", "config_stack"][..],
            ArtifactKind::IntegrationTest,
            "td-mta-config-smoke",
        ),
        (
            "td-mta",
            "format_rows",
            &["--test", "format_rows"][..],
            ArtifactKind::IntegrationTest,
            "td-mta-format-smoke",
        ),
    ] {
        let manifest = format!("/source/{package}/Cargo.toml");
        let test = kind.test();
        let mut command = cargo_manifest_architecture(
            if test { "test" } else { "build" },
            Path::new(&manifest),
            architecture,
        );
        command
            .args(["--release", "--message-format=json-render-diagnostics"])
            .args(target_args);
        if test {
            command.arg("--no-run");
        }
        let label = format!("arm-{installed}-build");
        command_record(&command, &label, &mut receipt)?;
        let output = bounded_output(&mut command, &label, 8 * 1024 * 1024, 1200)?;
        let binary = executable_for(&output, name, kind, architecture)?;
        let mut symbols = Command::new(tools(architecture).nm);
        symbols
            .arg("--defined-only")
            .arg(&binary)
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut symbols);
        let label = format!("arm-{installed}-symbols");
        command_record(&symbols, &label, &mut receipt)?;
        let symbols = bounded_output(&mut symbols, &label, 8 * 1024 * 1024, 30)?;
        allocation_symbols(&symbols, false)?;
        foreign_symbols(&symbols)?;
        rss_symbols(&symbols, false)?;
        copy_binary(&binary, &Path::new("/output/artifacts").join(installed))?;
    }
    refuse_decoy(Path::new("/output"))?;
    branch_free_routines(&mut receipt, architecture)?;
    split_mail_debug_architecture(&mut receipt, architecture)?;
    crate::crypto_api::qualify(&mut receipt, architecture)?;
    refuse_decoy(Path::new("/output"))?;
    write_new(Path::new("/output/artifacts/COMMANDS"), receipt.as_bytes())?;
    Ok(())
}

fn tls_allocation_evidence(output: &str) -> Result<()> {
    let phases = [
        "baseline",
        "config",
        "generation",
        "buffers",
        "reserved",
        "constructed",
        "overlap",
        "two_sessions",
        "refused",
        "released",
        "repeated",
        "dropped",
    ];
    tls_phase_evidence(output, "", "client", &phases)
}

fn tls_handshake_evidence(output: &str) -> Result<()> {
    tls_phase_evidence(
        output,
        "-handshake",
        "handshake",
        &[
            "baseline",
            "material",
            "config",
            "generation",
            "buffers",
            "constructed",
            "handshake",
            "record",
            "repeated",
            "client_released",
            "released",
            "buffers_released",
            "dropped",
        ],
    )
}

const REMOTE_CHAIN_PHASES: &[&str] = &[
    "baseline",
    "config",
    "generation",
    "buffers",
    "constructed",
    "handshake",
    "record",
    "repeated",
    "released",
    "buffers_released",
    "dropped",
];

fn remote_chain_evidence<'a>(output: &'a str, domain: &str, scenario: &str) -> Result<&'a str> {
    if !matches!(scenario, "remote12" | "remote13" | "remote13large") {
        return Err("unknown remote chain scenario".into());
    }
    let begin = "remote-chain-output-begin\n";
    let end = "remote-chain-output-end\n";
    if output.matches(begin).count() != 1 || output.matches(end).count() != 1 {
        return Err("ambiguous remote chain observation boundaries".into());
    }
    let (_, body) = output
        .split_once(begin)
        .ok_or("missing remote chain output")?;
    let (body, tail) = body
        .split_once(end)
        .ok_or("unterminated remote chain output")?;
    let completion = format!("remote-chain-controller-v2: {domain} {scenario} passed\n");
    let mut summaries = tail.lines().filter(|line| line.starts_with("test result:"));
    let passed = summaries
        .next()
        .is_some_and(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    if !tail.starts_with(&completion) || !passed || summaries.next().is_some() {
        return Err("remote chain controller did not complete".into());
    }
    match domain {
        "rust" => tls_phase_evidence(body, &format!("-{scenario}"), scenario, REMOTE_CHAIN_PHASES)?,
        "rss" => rss_evidence(body, scenario)?,
        _ => return Err("unknown remote chain observation domain".into()),
    }
    Ok(body)
}

fn tls_generation_evidence(output: &str, scenario: &str) -> Result<()> {
    if !matches!(
        scenario,
        "generation" | "generation-routing" | "generation-trust"
    ) {
        return Err("unknown generation observation scenario".into());
    }
    tls_phase_evidence(
        output,
        &format!("-{scenario}"),
        scenario,
        &[
            "baseline",
            "material",
            "config",
            "first",
            "candidate",
            "overlap",
            "refused",
            "old_released",
            "repeated",
            "current_released",
            "dropped",
        ],
    )
}

fn tls_large_chain_evidence(output: &str) -> Result<()> {
    tls_phase_evidence(
        output,
        "-large-chain",
        "large-chain",
        &[
            "baseline",
            "material",
            "config",
            "generation",
            "buffers",
            "constructed",
            "handshake",
            "record",
            "repeated",
            "client_released",
            "released",
            "buffers_released",
            "dropped",
        ],
    )
}

fn tls_fragment_evidence(output: &str) -> Result<()> {
    tls_phase_evidence(
        output,
        "-fragment",
        "fragment",
        &[
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "over_limit",
            "repeated",
            "dropped",
        ],
    )
}

fn tls_certificate_list_evidence(output: &str) -> Result<()> {
    tls_phase_evidence(
        output,
        "-certificate-list",
        "certificate-list",
        &[
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "repeated",
            "dropped",
        ],
    )
}

fn entropy_worker_evidence(output: &str) -> Result<()> {
    tls_phase_evidence(
        output,
        "-entropy",
        "entropy",
        &[
            "baseline",
            "spawned",
            "first_warm",
            "all_warm",
            "repeated",
            "joined",
            "dropped",
        ],
    )
}

fn tls_phase_evidence(output: &str, suffix: &str, scenario: &str, phases: &[&str]) -> Result<()> {
    let mut lines = output.lines();
    for phase in phases {
        let prefix = format!("tls-rust{suffix} {phase} ");
        let row = lines
            .next()
            .and_then(|line| line.strip_prefix(&prefix))
            .ok_or("missing or reordered TLS allocation phase")?;
        let mut columns = 0usize;
        for value in row.split(' ') {
            if value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
                || value.parse::<usize>().is_err()
            {
                return Err("invalid TLS allocation measurement".into());
            }
            columns += 1;
        }
        if columns != 7 {
            return Err("wrong TLS allocation measurement width".into());
        }
    }
    let version = if matches!(scenario, "handshake" | "large-chain") {
        2
    } else {
        1
    };
    let success = format!("tls-{scenario}-allocation-v{version}: rust passed");
    if lines.next() != Some(success.as_str()) || lines.next().is_some() || !output.ends_with('\n') {
        return Err("invalid TLS allocation completion".into());
    }
    Ok(())
}

fn rss_symbols(symbols: &str, probe: bool) -> Result<()> {
    if symbols.trim().is_empty() || symbols.contains("rss_probe") != probe {
        return Err("RSS diagnostic symbol boundary failed".into());
    }
    Ok(())
}

fn rss_evidence(output: &str, scenario: &str) -> Result<()> {
    let phases: &[&str] = match scenario {
        "control" => &["baseline", "touched", "dropped"],
        "client" => &[
            "baseline",
            "config",
            "generation",
            "buffers",
            "reserved",
            "constructed",
            "overlap",
            "two_sessions",
            "refused",
            "released",
            "repeated",
            "dropped",
        ],
        "remote12" | "remote13" | "remote13large" => REMOTE_CHAIN_PHASES,
        "generation" | "generation-routing" | "generation-trust" => &[
            "baseline",
            "material",
            "config",
            "first",
            "candidate",
            "overlap",
            "refused",
            "old_released",
            "repeated",
            "current_released",
            "dropped",
        ],
        "handshake" | "large-chain" => &[
            "baseline",
            "material",
            "config",
            "generation",
            "buffers",
            "constructed",
            "handshake",
            "record",
            "repeated",
            "client_released",
            "released",
            "buffers_released",
            "dropped",
        ],
        "entropy" => &[
            "baseline",
            "spawned",
            "first_warm",
            "all_warm",
            "repeated",
            "joined",
            "dropped",
        ],
        "fragment" => &[
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "over_limit",
            "repeated",
            "dropped",
        ],
        "certificate-list" => &[
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "repeated",
            "dropped",
        ],
        _ => return Err("unknown RSS observation scenario".into()),
    };
    let mut lines = output.lines();
    for phase in phases {
        let prefix = format!("rss {scenario} {phase} ");
        let value = lines
            .next()
            .and_then(|line| line.strip_prefix(&prefix))
            .ok_or("missing or reordered RSS phase")?;
        if value.is_empty()
            || !value.bytes().all(|b| b.is_ascii_digit())
            || value.parse::<u64>().map_err(|_| "invalid RSS value")? == 0
        {
            return Err("invalid RSS observation".into());
        }
    }
    let completion = format!("rss-observation-v2: {scenario} passed");
    if lines.next() != Some(completion.as_str())
        || lines.next().is_some()
        || !output.ends_with('\n')
    {
        return Err("invalid RSS completion".into());
    }
    Ok(())
}

/// No libc allocator interposition and no AWS-LC, OpenSSL-family or ring C
/// symbol. This refuses the native backends by their symbol prefixes; it
/// does not by itself prove every cryptographic routine is td-primitives.
fn foreign_symbols(symbols: &str) -> Result<()> {
    if symbols.trim().is_empty() {
        return Err("portable symbol listing is empty".into());
    }
    for name in symbols
        .lines()
        .filter_map(|line| line.split_whitespace().last())
    {
        if name.starts_with("__wrap_") {
            return Err(format!("portable executable interposes {name}"));
        }
        // aws-lc-sys and ring prefix their C symbols; the AWS-LC Rust crates
        // show in mangled paths. td's own names may mention AWS-LC in prose,
        // as tests do.
        if ["aws_lc_", "OPENSSL_", "BORINGSSL_", "ring_core_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            || ["aws_lc_rs", "aws_lc_sys"]
                .iter()
                .any(|krate| name.contains(krate))
        {
            return Err(format!(
                "portable executable carries native crypto symbol {name}"
            ));
        }
    }
    Ok(())
}

fn allocation_symbols(symbols: &str, probe: bool) -> Result<()> {
    let has_counter = symbols.contains("TD_MTA_ALLOCATION_COUNTERS");
    let has_probe = symbols.contains("rust_alloc_probe");
    if symbols.trim().is_empty()
        || if probe {
            !has_counter || !has_probe
        } else {
            has_counter || has_probe
        }
    {
        return Err("portable allocation probe symbol boundary failed".into());
    }
    Ok(())
}

const ALLOCATION_SUCCESS: &str =
    "rust-allocation-probe-v1: counter-model forwarding hot-paths passed\n";

fn allocation_evidence(output: &str) -> Result<()> {
    if output != ALLOCATION_SUCCESS {
        return Err("portable Rust allocation probe did not complete exactly".into());
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("create private artifact {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write artifact: {e}"))
}

fn copy_binary(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(source).map_err(|e| format!("inspect binary: {e}"))?;
    if !meta.is_file() || meta.len() > 256 * 1024 * 1024 {
        return Err("portable binary must be a regular file within 256 MiB".into());
    }
    source_tree(source, destination, 0)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("set artifact executable mode: {e}"))
}

#[cfg(test)]
fn collect_artifacts(output: &Path, destination: &Path) -> Result<String> {
    collect_artifacts_architecture(output, destination, Architecture::X86_64)
}

fn collect_artifacts_architecture(
    output: &Path,
    destination: &Path,
    architecture: Architecture,
) -> Result<String> {
    let source = output.join("artifacts");
    if !fs::symlink_metadata(&source)
        .map_err(|e| format!("inspect build artifacts: {e}"))?
        .is_dir()
    {
        return Err("build artifacts must be a real directory".into());
    }
    let mut names = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&source).map_err(|e| format!("read build artifacts: {e}"))? {
        let entry = entry.map_err(|e| format!("artifact entry: {e}"))?;
        if !entry
            .file_type()
            .map_err(|e| format!("artifact type: {e}"))?
            .is_file()
        {
            return Err("build artifacts must contain only regular files".into());
        }
        names.insert(entry.file_name());
    }
    let expected = binary_names(architecture)
        .iter()
        .copied()
        .chain(["td-mta.debug", "COMMANDS"])
        .map(std::ffi::OsString::from)
        .collect();
    if names != expected {
        return Err("unexpected build artifact inventory".into());
    }
    let receipt = read_output(&source.join("COMMANDS"), "commands", 256 * 1024)?;
    fs::create_dir(destination).map_err(|e| format!("create private artifact directory: {e}"))?;
    for name in binary_names(architecture) {
        copy_binary(&source.join(name), &destination.join(name))?;
    }
    let debug = destination.join("td-mta.debug");
    copy_binary(&source.join("td-mta.debug"), &debug)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(debug, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("set collected debug mode: {e}"))?;
    Ok(receipt)
}

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new(root: &Path) -> Result<Self> {
        let parent = root.join(".td-build-cache");
        fs::create_dir_all(&parent).map_err(|e| format!("portable build parent: {e}"))?;
        Self::create(&parent, "crypto-build")
    }

    fn create(parent: &Path, label: &str) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        for attempt in 0..128 {
            let path = parent.join(format!("{label}-{}-{attempt}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("portable build scratch: {e}")),
            }
        }
        Err("portable build scratch names exhausted".into())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn path_text(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or("portable paths must be UTF-8")?;
    if text.as_bytes().contains(&0) {
        return Err("portable path contains NUL".into());
    }
    Ok(text.into())
}

fn bind(source: &Path, destination: &str, readonly: bool) -> Result<crate::sandbox::Bind> {
    Ok(crate::sandbox::Bind {
        src: path_text(source)?,
        dest: Some(destination.into()),
        readonly,
        ro_optional: false,
    })
}

/// Every mount of the compile namespace: the declared inputs read-only, the
/// private output, and from the authenticated build root `BUILD_ROOT_BINDS`
/// alone.
fn compile_binds(
    source: &Path,
    vendor: &Path,
    inputs: &crate::crypto_portable::Inputs,
    helper: &Path,
    output: &Path,
) -> Result<Vec<crate::sandbox::Bind>> {
    let mut binds = vec![
        bind(source, "/source", true)?,
        bind(vendor, "/vendor", true)?,
        bind(&inputs.rust, "/rust", true)?,
        bind(helper, "/td-builder", true)?,
        bind(helper, "/td-crypto-host-linker", true)?,
        bind(helper, "/td-crypto-decoy", true)?,
        bind(output, "/output", false)?,
    ];
    for (inside, destination) in BUILD_ROOT_BINDS {
        let directory = inputs.build_root.directory.join(inside);
        if !fs::symlink_metadata(&directory).is_ok_and(|m| m.is_dir()) {
            return Err(format!(
                "build root lacks a real directory {}",
                directory.display()
            ));
        }
        binds.push(bind(&directory, destination, true)?);
    }
    Ok(binds)
}

fn source_tree(source: &Path, destination: &Path, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err("portable source nesting exceeds 64 directories".into());
    }
    let metadata =
        fs::symlink_metadata(source).map_err(|e| format!("inspect portable source: {e}"))?;
    if metadata.is_dir() {
        fs::create_dir(destination).map_err(|e| format!("create private source directory: {e}"))?;
        for entry in fs::read_dir(source).map_err(|e| format!("read portable source: {e}"))? {
            let entry = entry.map_err(|e| format!("portable source entry: {e}"))?;
            source_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                depth + 1,
            )?;
        }
    } else if metadata.is_file() {
        let mut input = fs::File::open(source).map_err(|e| format!("open source: {e}"))?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(|e| format!("create private source: {e}"))?;
        std::io::copy(&mut input, &mut output).map_err(|e| format!("copy source: {e}"))?;
    } else {
        return Err(format!(
            "portable sources must be regular files/directories: {}",
            source.display()
        ));
    }
    Ok(())
}

fn stage_sources(root: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination).map_err(|e| format!("create portable source root: {e}"))?;
    for package in crate::crypto_policy::LOCAL_SOURCES {
        let source = root.join(package);
        let staged = destination.join(package);
        fs::create_dir(&staged).map_err(|e| format!("create portable crate directory: {e}"))?;
        for name in ["Cargo.toml", "Cargo.lock", "src"] {
            source_tree(&source.join(name), &staged.join(name), 0)?;
        }
        match fs::symlink_metadata(source.join("tests")) {
            Ok(_) => source_tree(&source.join("tests"), &staged.join("tests"), 0)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("inspect crate tests: {e}")),
        }
    }
    for (package, directory, names) in [
        ("engine", "src", &["sha256.rs"][..]),
        ("td-fido", "src", &["fido_p256.rs"][..]),
        ("td-fido", "tests", &["p256_vectors.txt"][..]),
    ] {
        for ancestor in [root.join(package), root.join(package).join(directory)] {
            if !fs::symlink_metadata(&ancestor)
                .map_err(|e| format!("inspect oracle source directory: {e}"))?
                .is_dir()
            {
                return Err("oracle source ancestors must be directories, not links".into());
            }
        }
        let target = destination.join(package).join(directory);
        fs::create_dir_all(&target).map_err(|e| format!("create oracle source directory: {e}"))?;
        for name in names {
            let input = root.join(package).join(directory).join(name);
            if !fs::symlink_metadata(&input)
                .map_err(|e| format!("inspect oracle source: {e}"))?
                .is_file()
            {
                return Err("oracle sources must be regular files, not links".into());
            }
            source_tree(&input, &target.join(name), 0)?;
        }
    }
    Ok(())
}

fn enter(
    command: &str,
    args: &[String],
    binds: &[crate::sandbox::Bind],
    scratch: &Path,
    runtime: bool,
) -> Result<()> {
    let _ = path_text(scratch)?;
    let mut tmpfs = vec!["/tmp".into()];
    if runtime {
        tmpfs.push("/output".into());
    }
    let status = crate::sandbox::host_shell(
        command,
        args,
        binds,
        &tmpfs,
        "",
        "/tmp",
        "/tmp",
        &[],
        &[],
        scratch,
    )
    .map_err(|e| format!("portable namespace: {e}"))?;
    if !status.success() {
        return Err(format!("portable namespace failed ({status})"));
    }
    Ok(())
}

fn stack_evidence(output: &str, prefix: &str, ceiling: usize) -> Result<usize> {
    let mut values = output.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = values
        .next()
        .ok_or("portable stack measurement is missing")?;
    if values.next().is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("portable stack measurement is ambiguous or malformed".into());
    }
    let bytes = value
        .parse::<usize>()
        .map_err(|_| "portable stack measurement overflows")?;
    if bytes == 0 || bytes > ceiling {
        return Err("portable stack measurement exceeds its ceiling".into());
    }
    Ok(bytes)
}

fn cli_evidence(output: &str) -> Result<()> {
    let mut summaries = output
        .lines()
        .filter(|line| line.starts_with("test result:"));
    let summary = summaries
        .next()
        .ok_or("portable CLI test summary missing")?;
    if summaries.next().is_some()
        || !summary.starts_with(
            "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;",
        )
    {
        return Err("portable CLI suite did not pass all 3 tests".into());
    }
    Ok(())
}

/// `td-crypto-smoke`'s named functional cases, each run in its own process.
const CRYPTO_SMOKE_CASES: &[&str] = &[
    "backend_tests::keys_aws_lc_wrote_load_and_sign_here",
    "backend_tests::keys_written_here_have_the_shape_aws_lc_loaded",
    "backend_tests::policy_verifiers_take_exactly_their_own_signatures",
    "backend_tests::committed_certificate_paths_verify_under_the_policy",
    "backend_tests::the_loader_checks_scalar_range_and_embedded_point",
    "sha256::tests::known_answers_and_fragmented_updates",
    "sha256::tests::differential_padding_blocks_and_every_split",
    "sha256::tests::refusal_retires_and_clears_state",
    "sha256::tests::inline_storage_and_fixed_debug",
    "bulk_sha256::tests::bulk_matches_owned_digest_across_padding_and_chunk_boundaries",
    "bulk_sha256::tests::length_refusal_retires_native_state_before_update",
    "bulk_sha256::tests::returned_error_and_unwind_retire_partially_updated_state",
    "entropy::tests::local_randomness_smoke",
    "entropy::tests::construction_requires_nonempty_successful_initialization",
    "entropy::tests::synthetic_partial_error_clears_the_entire_caller_slice",
    "pem::tests::canonical_base64_and_transactional_output",
    "pem::tests::certificate_decoding_matches_independent_pem_reader",
    "pem::tests::certificate_envelopes_and_retry",
    "pem::tests::certificate_limits_and_der_envelopes",
    "pem::tests::p256_pem_loading_and_refusals",
    "pem::tests::branch_free_sextet_matches_every_byte",
    "pem::tests::branch_free_base64_classes_padding_and_refusals",
    "pem::tests::branch_free_base64_matches_the_reference_on_random_input",
    "der::tests::certificate_der_lengths_bit_strings_and_oids",
    "der::tests::certificate_calendar_boundaries",
    "certificate_algorithms::tests::certificate_algorithm_inventory_fails_closed",
    "certificate_algorithms::tests::certificate_key_admission_known_answers",
    "identity::tests::local_identity_owns_material_and_checks_validity",
    "identity::tests::narrowed_identity_shares_material_and_cannot_expand_or_revive_bindings",
    "identity::tests::identity_refuses_wrong_key_name_time_and_order",
    "identity::tests::local_identity_enforces_key_usage_and_extensions",
    "identity::tests::local_identity_name_and_extension_limits",
    "identity::tests::local_identity_chain_constraints_and_maximum_depth",
    "identity::tests::local_identity_accepts_rsa_issuer_and_refuses_unsupported_algorithms",
    "identity::tests::identity_admission_unwind_drops_unpublished_state",
    "identity::tests::local_identity_metadata_bounds_and_malformed_values",
    "trust::tests::private_trust_replaces_public_and_other_private_roots",
    "trust::tests::private_trust_bundle_limits_duplicates_and_atomic_refusal",
    "trust::tests::private_trust_anchor_policy_and_ignored_self_signature",
    "trust::tests::public_trust_inventory_and_source_are_fixed",
    "trust::tests::trust_construction_unwind_drops_unpublished_state",
    "provider::tests::factory_digest_and_fixed_comparison",
    "provider::tests::accepted_pkcs8_variants_and_public_point_known_answer",
    "provider::tests::malformed_der_and_inconsistent_keys_are_refused",
    "provider::tests::generated_keys_and_signatures_pass_independent_verification",
    "provider::tests::generation_maps_random_failure_and_exhausted_draws",
    "provider::tests::generated_documents_differ",
    "provider::tests::capacity_failure_and_retired_keys_preserve_caller_output",
    "provider::tests::shared_key_serializes_success_and_terminal_failure",
    "sha256_oracle::tests::empty_input",
    "sha256_oracle::tests::abc",
    "sha256_oracle::tests::two_block_message",
    "sha256_oracle::tests::million_a",
    "p256_oracle::tests::independent_modular_arithmetic_covers_carries_and_borrows",
    "p256_oracle::tests::nist_ecdh_and_openssl_scalar_boundaries_match",
    "p256_oracle::tests::nist_signature_acceptance_and_refusal_and_rare_x_reduction",
    "p256_oracle::tests::nist_public_key_validation_preserves_overwide_rejections",
    "p256_oracle::tests::scalar_and_coordinate_admission_never_reduce_untrusted_values",
    "p256_oracle::tests::exceptional_point_cases_and_projective_representations",
    "p256_oracle::tests::signature_scalar_ranges_and_infinity_result_are_refused",
    "tests::explicit_provider_and_roots_construct_without_global_default",
    "provider::tests::signature_transform_failure_retires_before_key_restore",
    "tls_signer::tests::canonical_tls_signature_encoding_boundaries",
    "tls_signer::tests::tls_signer_uses_owned_key_and_hashes_once",
    "tls_signer::tests::shared_tls_signers_observe_transform_retirement",
    "identity::tests::retained_tls_identity_completes_handshakes_and_survives_remote_refusal",
    "identity::tests::retained_tls_identity_shares_key_lifecycle",
    "tls_clock::tests::injected_clock_missing_time_and_shared_retirement",
    "tls_clock::tests::injected_clock_serializes_and_drops_panicking_source",
    "tls_client::tests::client_configuration_pins_policy_and_redacts_state",
    "tls_client::tests::client_configuration_verifies_trust_name_alpn_and_full_handshakes",
    "tls_client::tests::client_configuration_clock_errors_reach_handshake_and_open_tickets",
    "tls_client::tests::client_verifier_checks_chain_bounds_before_path_work",
    "tls_client::tests::tls12_session_save_can_ignore_transient_clock_failure",
    "tls_server::tests::server_configuration_bounds_bindings_roles_and_private_trust",
    "tls_server::tests::server_configuration_routes_names_and_refuses_resumption",
    "tls_server::tests::server_configuration_pins_disabled_backend_features",
    "tls_server::tests::server_configuration_requires_private_client_certificates",
    "tls_server::tests::server_configuration_checks_cold_material_and_supplied_verifier_time",
    "tls_server::tests::backend_acceptor_discards_ip_literal_sni_before_routing",
    "tls_server::tests::server_configuration_selects_distinct_owned_identities",
    "tls_server::tests::server_configuration_omits_oversized_ca_hint_lists",
    "tls_server::tests::server_configuration_refuses_offered_resumption",
    "tls_session::tests::client_session_fragmented_handshake_and_simultaneous_writes",
    "tls_session::tests::client_session_retires_handshake_output_reserve_and_refuses_backend_growth",
    "tls_session::tests::client_session_retains_ignored_tls12_clock_failure",
    "tls_session::tests::client_session_checks_time_without_native_time_requests",
    "tls_session::tests::client_session_close_halves_and_transport_eof",
    "tls_session::tests::client_session_tls12_alert_refuses_pending_ciphertext_and_socket_tail",
    "tls_session::tests::client_session_never_emits_after_close_or_reuses_failed_state",
    "tls_session::tests::client_session_unwind_consumes_state_and_shared_clock_failure_fences_reuse",
    "tls_session::tests::client_session_unread_plaintext_blocks_without_consuming_next_record",
    "tls_session::tests::client_session_name_verification_maps_and_retires_without_output",
    "tls_session::tests::client_session_key_update_response_advances_without_application_write",
    "tls_session::tests::client_session_handshake_reassembly_counts_retained_record_headers",
    "tls_session::tests::client_session_errors_are_fixed_and_do_not_export_backend_diagnostics",
    "tls_session::tests::client_session_ticket_clock_failure_after_finished_is_terminal",
    "tls_session::tests::client_session_ticket_flight_capacity_is_terminal_after_finished",
    "tls_session::tests::client_session_unfinished_cancellation_eof_and_alert_refuse_evidence",
    "session_clock::tests::observers_isolate_transient_errors_and_share_retirement",
    "tls_record::tests::record_bounds_track_protection_not_finished",
    "tls_record::tests::record_requires_one_complete_exact_frame",
    "tls_session::tests::client_session_tls12_hello_request_never_adds_post_close_output",
    "tls_session::tests::client_session_tls12_simultaneous_close_pins_pending_output_refusal",
    "tls_hello::tests::raw_hello_fragmentation_names_and_retry",
    "tls_hello::tests::raw_hello_refuses_malformed_sni_before_completion",
    "tls_hello::tests::raw_hello_fixed_state_skips_large_extensions_and_bounds_messages",
    "tls_session::tests::server::server_session_public_round_trip_and_evidence",
    "tls_session::tests::server::server_session_refuses_raw_ip_sni_across_record_fragments",
    "tls_session::tests::server::server_session_mandatory_client_proof_and_typed_refusals",
    "tls_session::tests::server::server_session_valid_retry_and_fragmented_raw_retry_sni_refusal",
    "tls_session::tests::server::server_session_public_pair_tiny_pipes_simultaneous_writes_and_close",
    "tls_session::tests::server::server_session_selected_validity_and_shared_key_retirement",
    "tls_session::tests::server::server_session_clock_failures_and_acceptor_unwind_retire_owned_state",
    "tls_policy::tests::exact_tls_algorithm_inventory",
    "tls_policy::tests::handshake_mapping_drift_is_refused",
    "tls_policy::tests::excluded_certificate_signature_is_refused",
    "tls_smoke::explicit_policy_negotiates_classical_groups_and_suites",
    "tls_smoke::explicit_policy_refuses_excluded_peer_algorithms",
    "tls_smoke::tls12_local_round_trip",
    "tls_smoke::tls13_local_round_trip",
    "tls_smoke::rejects_wrong_server_name",
    "tls_smoke::rejects_untrusted_chain",
    "tls_smoke::rejects_expired_certificate",
    "tls_smoke::rejects_malformed_record",
    "tls_smoke::rejects_bad_certificate_signature",
    "tls_smoke::rejects_tampered_ciphertext",
    "tls_smoke::tls12_mutual_authentication",
    "tls_smoke::tls13_mutual_authentication",
    "tls_smoke::mutual_authentication_requires_certificate",
    "tls_smoke::mutual_authentication_refuses_unknown_issuer",
    "tls_smoke::mutual_authentication_refuses_expired_certificate",
    "tls_smoke::mutual_authentication_refuses_server_only_usage",
    "tls_smoke::mutual_authentication_refuses_bad_signature",
    "tls_smoke::mutual_authentication_refuses_mismatched_key_before_connect",
];

/// `td-mta-format-smoke`'s named functional cases, each run in its own process.
const FORMAT_SMOKE_CASES: &[&str] = &[
    "provider_hashes_blob_fixture",
    "provider_hashes_import_snapshot_fixtures",
];

/// `td-mta-config-smoke`'s ordinary cases: the stack probes' scenarios run
/// without a measured stack.
const CONFIG_SMOKE_CASES: &[&str] = &["host_loader_scenarios", "host_materialized_scenarios"];

/// `td-mta-transport-smoke`'s named functional cases, each run in its own process.
const TRANSPORT_SMOKE_CASES: &[&str] = &[
    "generations::tests::current_retired_and_candidate_share_two_slots_before_construction",
    "generations::tests::failed_stale_and_foreign_candidates_preserve_active_state_and_owners",
    "generations::tests::final_worker_release_destroys_payload_before_returning_capacity",
    "generations::tests::racing_preparation_and_release_never_construct_a_third_live_payload",
    "tls_policy::tests::compiled_policy_roles_bind_names_trust_and_generation_ids",
    "tls_policy::tests::queued_and_native_reservations_retain_capacity_and_recover_exact_buffers",
    "tls_policy::tests::gateway_comparison_survives_reordering_but_rejects_policy_or_binding_changes",
    "tls_policy::tests::material_errors_never_publish_and_acme_requests_are_explicit",
    "tls_policy::tests::material_reader_caps_bytes_eof_and_interrupted_work",
    "tls_policy::tests::compiled_configs_enforce_relay_trust_protocol_and_gateway_client_auth",
    "tls_policy::tests::https_profiles_narrow_shared_smtp_names_before_native_routing",
    "tls_policy::tests::io_tests::handoff_refuses_plaintext_tails_and_deadlines_and_returns_original_buffers",
    "tls_policy::tests::io_tests::authorized_connections_release_handshake_capacity_and_retain_generation_until_teardown",
    "tls_policy::tests::io_tests::gateway_without_client_certificate_never_exposes_mail_proof_and_recovers_permits",
    "tls_policy::tests::io_tests::handshake_deadline_can_only_tighten_and_failure_is_sticky",
    "tls_policy::tests::io_tests::changed_gateway_policy_aborts_pending_connection_and_releases_capacity",
    "tls_policy::tests::io_tests::established_clock_failure_clears_cached_authorization_and_aborts_socket",
    "tls_policy::tests::client_tests::client_only_acme_bootstrap_never_opens_server_material",
    "tls_policy::tests::client_tests::expired_server_does_not_block_clients_but_invalid_client_trust_still_refuses",
    "tls_policy::tests::client_tests::client_generation_transitions_to_complete_within_the_same_two_slots",
    "tls_policy::starttls::tests::starttls_reply_requires_short_writes_and_complete_flush",
    "tls_policy::starttls::tests::starttls_reply_refuses_invalid_write_counts_and_flush_failure",
    "tls_policy::tests::starttls_tests::server_starttls_flushes_220_before_handoff_and_verifies_real_tls",
    "tls_policy::tests::starttls_tests::server_starttls_refuses_unframed_parameterized_tailed_and_wrong_role_commands",
    "tls_policy::tests::starttls_tests::server_starttls_deadlines_clock_refusal_and_cancel_release_reservations",
    "tls_policy::tests::gateway_process_tests::gateway_starttls_process_requires_verified_pin_after_plaintext_reply",
    "smtp_wire::tests::ehlo_offer_requires_a_complete_advertisement_and_no_tail",
    "smtp_wire::tests::ehlo_offer_handles_fragmentation_and_skips_malformed_extensions",
    "tls_policy::tests::client_starttls_tests::client_starttls_and_server_upgrade_verify_real_tls",
    "tls_policy::tests::client_starttls_tests::client_starttls_refuses_bad_replies_and_accepts_fragmented_220",
    "tls_policy::tests::client_starttls_tests::client_starttls_roles_scratch_deadlines_and_cancel_return_buffers",
    "smtp_wire::tests::line_split_at_every_boundary_retains_exact_tail",
    "smtp_wire::tests::strict_line_framing_is_terminal_after_failure",
    "smtp_wire::tests::control_line_counts_crlf_and_clears_only_its_reservation",
    "smtp_wire::tests::reply_syntax_accepts_bare_codes_and_refuses_ambiguous_separators",
    "smtp_wire::tests::multiline_replies_require_one_code_and_leave_the_next_reply_unread",
    "smtp_wire::tests::fragmented_220_reply_stops_before_plaintext_or_record_tail",
    "smtp_wire::tests::reply_aggregate_cap_includes_all_lines_and_the_final_crlf",
    "smtp_wire::tests::ehlo_greeting_is_never_a_starttls_advertisement",
    "tls_policy::tests::gateway_process_tests::gateway_mutual_tls_accepts_current_and_next_verified_leaf_pins",
    "tls_policy::tests::gateway_process_tests::gateway_mutual_tls_refuses_verified_leaf_with_wrong_pin_or_actual_peer",
    "gateway_policy::tests::canonical_gateway_policy_ignores_only_representation_and_server_material",
    "gateway_policy::tests::gateway_filters_and_material_limits_refuse_without_fallback",
    "gateway_policy::tests::gateway_tls_requires_a_client_certificate_on_the_same_valid_server",
    "tls_admission::tests::capacities_saturate_and_reuse_without_losing_live_reservations",
    "tls_admission::tests::concurrent_reservations_and_worker_returns_preserve_the_global_cap",
    "tls_admission::tests::overlapping_release_and_reservation_never_over_admit_or_leak_capacity",
    "tls_admission::tests::refusal_and_pool_drop_preserve_linear_cleanup",
    "clock::tests::runtime_clock_keeps_utc_and_monotonic_domains_separate",
    "clock::tests::tls_clock_conversion_never_falls_back_after_refusal",
    "transport::tests::tcp_bounded_io_actual_peer_and_unbuffered_flush",
    "transport::tests::tcp_peer_eof_preserves_write_half_and_local_close_preserves_read_half",
    "transport::tests::tcp_abort_drop_and_errors_fence_both_directions",
    "tls_io::tests::final_handshake_write_cannot_publish_past_its_deadline",
    "tls_io::tests::flush_backpressure_withholds_finished_and_bad_transport_counts_abort",
    "tls_io::tests::public_tls_round_trip_with_tiny_pipes_and_independent_close",
    "tls_io::tests::simultaneous_full_chunks_progress_without_growing_wire_storage",
    "tls_io::tests::deadlines_and_missing_tls_time_discard_evidence_and_never_revive",
    "tls_io::tests::malformed_records_truncation_and_constructor_refusal_close_the_transport",
    "tls_io::tests::public_crypto_sessions_exchange_mail_bytes_over_local_tcp",
    "tls_io::tests::established_sessions_outlive_handshake_deadline_and_preserve_backpressure",
    "tls_io::tests::established_tcp_eof_without_tls_close_is_terminal_even_at_record_boundary",
    "tls_io::tests::returned_buffers_are_reused_and_close_paths_keep_exclusive_ownership",
    "tls_io::tests::constructor_refusals_return_moved_pool_buffers_without_logging_them",
];

/// Ignored resource probes the clean x86-64 runtime runs by name; they
/// measure x86-64 stack mappings and refuse other targets.
const STACK_PROBES: &[(&str, &str)] = &[
    ("td-mta-config-smoke", "portable_loader_stack"),
    ("td-mta-config-smoke", "portable_materialized_stack"),
    (
        "td-mta-transport-smoke",
        "tls_policy::tests::stack_tests::portable_tls_policy_transport_stack",
    ),
];

/// The functional cases, by executable, that every architecture's runtime
/// runs by exact name.
pub(crate) const FUNCTIONAL_CASES: &[(&str, &[&str])] = &[
    ("td-crypto-smoke", CRYPTO_SMOKE_CASES),
    ("td-mta-format-smoke", FORMAT_SMOKE_CASES),
    ("td-mta-config-smoke", CONFIG_SMOKE_CASES),
    ("td-mta-transport-smoke", TRANSPORT_SMOKE_CASES),
];

/// The clean x86-64 runtime's cases: every functional case, then the stack
/// probes, as (executable, exact name, ignored).
fn portable_cases() -> impl Iterator<Item = (&'static str, &'static str, bool)> {
    FUNCTIONAL_CASES
        .iter()
        .flat_map(|(binary, cases)| cases.iter().map(move |case| (*binary, *case, false)))
        .chain(
            STACK_PROBES
                .iter()
                .map(|(binary, case)| (*binary, *case, true)),
        )
}

/// The aarch64 functional cases under the mounted emulator, one process per
/// case. The resource probes are x86-64 only and are not run.
pub(crate) fn emulated_runtime_inner() -> Result<()> {
    require_namespace(&[
        EMULATOR,
        "/output",
        "/artifacts/td-mta",
        "/artifacts/td-crypto-smoke",
        "/artifacts/td-mta-config-smoke",
        "/artifacts/td-mta-format-smoke",
        "/artifacts/td-mta-transport-smoke",
    ])?;
    let mut command = Command::new(EMULATOR);
    command
        .args(["/artifacts/td-mta", "--version"])
        .env_clear()
        .stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    if bounded_output(&mut command, "arm-version", 4096, 120)? != "td-mta 0.1.0\n" {
        return Err("emulated aarch64 binary returned an unexpected version".into());
    }
    println!("aarch64 emulated: PASS td-mta --version");
    let mut passed = 0usize;
    for (index, (binary, case)) in FUNCTIONAL_CASES
        .iter()
        .flat_map(|(binary, cases)| cases.iter().map(move |case| (*binary, *case)))
        .enumerate()
    {
        let started = Instant::now();
        let mut command = Command::new(EMULATOR);
        command
            .arg(format!("/artifacts/{binary}"))
            .args(["--exact", case, "--test-threads=1"])
            .env_clear()
            .stdin(Stdio::null());
        if binary == "td-mta-transport-smoke" {
            command
                .env("TD_MTA_TEST_TLS_PEER", "/artifacts/td-crypto-smoke")
                .env("TD_MTA_TEST_PEER_EMULATOR", EMULATOR);
        }
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("arm-{binary}-{index}");
        let output = bounded_output(&mut command, &name, 64 * 1024, 300).inspect_err(|_| {
            println!("aarch64 emulated: FAIL {binary} {case}");
            let path = Path::new("/output").join(format!("{name}.log"));
            if let Ok(log) = read_output(&path, &name, 64 * 1024) {
                eprintln!("emulated aarch64 failure in {case}:\n{log}");
            }
        })?;
        if !output
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;"))
        {
            println!("aarch64 emulated: FAIL {binary} {case} (no single passing test)");
            return Err(format!("emulated aarch64 case did not execute: {case}"));
        }
        passed += 1;
        println!(
            "aarch64 emulated: PASS {binary} {case} ({:.2}s)",
            started.elapsed().as_secs_f64()
        );
    }
    println!(
        "aarch64 emulated runtime: version and all {passed} named functional cases passed under user-mode emulation; the x86-64 stack, allocation and RSS probes were not run"
    );
    Ok(())
}

pub(crate) fn runtime_inner() -> Result<()> {
    require_namespace(&[
        "/artifacts/td-mta",
        "/artifacts/td-crypto-smoke",
        "/artifacts/td-mta-cli-smoke",
        "/artifacts/td-mta-config-smoke",
        "/artifacts/td-mta-format-smoke",
        "/artifacts/td-mta-transport-smoke",
        "/artifacts/td-mta-rust-allocation-probe",
        "/artifacts/td-mta-rss-probe",
        "/output",
    ])?;
    // Deadline enforcement stays outside the CLI namespace; its init is a helper.
    let mut command = Command::new("/td-builder");
    command
        .args([
            "host-sandbox",
            "--store-item-at",
            "/artifacts/td-mta",
            "/artifacts/td-mta",
            "--store-item-at",
            "/artifacts/td-mta-cli-smoke",
            "/artifacts/td-mta-cli-smoke",
            "--",
            "/artifacts/td-mta-cli-smoke",
            "--test-threads=1",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", "/tmp/home")
        .env("TMPDIR", "/output")
        .stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    let started = Instant::now();
    let output = bounded_output(&mut command, "mail-cli", 64 * 1024, 120).inspect_err(|_| {
        if let Ok(log) = read_output(Path::new("/output/mail-cli.log"), "mail-cli", 64 * 1024) {
            eprintln!("portable CLI failure:\n{log}");
        }
    })?;
    cli_evidence(&output)?;
    println!(
        "portable CLI: all 3 PostgreSQL operator argument tests passed with no helper installed ({:.2}s, {} output bytes)",
        started.elapsed().as_secs_f64(),
        output.len()
    );
    let mut command = Command::new("/artifacts/td-mta");
    command.arg("--version").env_clear().stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    let version = bounded_output(&mut command, "version", 4096, 30)?;
    if version != "td-mta 0.1.0\n" {
        return Err("portable binary returned an unexpected version".into());
    }
    for (index, (binary, case, ignored)) in portable_cases().enumerate() {
        let mut command = Command::new(format!("/artifacts/{binary}"));
        command
            .args(["--exact", case, "--test-threads=1"])
            .env_clear()
            .stdin(Stdio::null());
        if binary == "td-mta-transport-smoke" {
            command.env("TD_MTA_TEST_TLS_PEER", "/artifacts/td-crypto-smoke");
        }
        if ignored {
            command.args(["--ignored", "--show-output"]);
        }
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("{binary}-{index}");
        let output = bounded_output(&mut command, &name, 64 * 1024, 30).inspect_err(|_| {
            let path = Path::new("/output").join(format!("{name}.log"));
            if let Ok(log) = read_output(&path, &name, 64 * 1024) {
                eprintln!("portable smoke failure in {case}:\n{log}");
            }
        })?;
        if !output
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;"))
        {
            return Err(format!("portable smoke case did not execute: {case}"));
        }
        if ignored {
            let (prefix, ceiling) = match case {
                "portable_loader_stack" => ("config_stack_mapping_bytes=", 176 * 1024),
                "portable_materialized_stack" => {
                    ("config_materialized_stack_mapping_bytes=", 256 * 1024)
                }
                "tls_policy::tests::stack_tests::portable_tls_policy_transport_stack" => {
                    ("tls_policy_transport_stack_mapping_bytes=", 256 * 1024)
                }
                _ => return Err("unknown portable stack qualification case".into()),
            };
            let bytes = stack_evidence(&output, prefix, ceiling)?;
            println!("portable runtime: {prefix}{bytes}");
        }
    }
    let mut command = Command::new("/artifacts/td-mta-rust-allocation-probe");
    command.env_clear().stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    let output = bounded_output(&mut command, "rust-allocation-probe", 4096, 30)?;
    allocation_evidence(&output)?;
    println!("portable runtime: Rust allocation counter model, forwarding and digest/SMTP hot paths passed");
    type Evidence = fn(&str) -> Result<()>;
    let scenarios: &[(&str, &str, Evidence)] = &[
        ("client", "--tls-clients", tls_allocation_evidence),
        ("handshake", "--tls-handshake", tls_handshake_evidence),
        ("entropy", "--entropy-workers", entropy_worker_evidence),
        ("fragment", "--tls-fragments", tls_fragment_evidence),
        (
            "certificate-list",
            "--tls-certificate-list",
            tls_certificate_list_evidence,
        ),
        ("large-chain", "--tls-large-chain", tls_large_chain_evidence),
        ("generation", "--tls-generations", |output| {
            tls_generation_evidence(output, "generation")
        }),
        ("generation-routing", "--tls-generation-routing", |output| {
            tls_generation_evidence(output, "generation-routing")
        }),
        ("generation-trust", "--tls-generation-trust", |output| {
            tls_generation_evidence(output, "generation-trust")
        }),
    ];
    for (scenario, argument, evidence) in scenarios {
        let mut command = Command::new("/artifacts/td-mta-rust-allocation-probe");
        command.arg(argument).env_clear().stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("tls-{scenario}-allocation-rust");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        evidence(&output)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
    }

    for (scenario, argument) in [
        ("control", None),
        ("client", Some("--tls-clients")),
        ("handshake", Some("--tls-handshake")),
        ("entropy", Some("--entropy-workers")),
        ("fragment", Some("--tls-fragments")),
        ("certificate-list", Some("--tls-certificate-list")),
        ("large-chain", Some("--tls-large-chain")),
        ("generation", Some("--tls-generations")),
        ("generation-routing", Some("--tls-generation-routing")),
        ("generation-trust", Some("--tls-generation-trust")),
    ] {
        let mut command = Command::new("/artifacts/td-mta-rss-probe");
        command.env_clear().stdin(Stdio::null());
        if let Some(argument) = argument {
            command.arg(argument);
        }
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("rss-probe-{scenario}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        rss_evidence(&output, scenario)?;
        for line in output.lines() {
            println!("portable RSS diagnostic (KiB): {line}");
        }
    }

    for (version, scenario, case) in [
        (
            "1.2",
            "remote12",
            "tls_memory_process_tests::remote_chain_observations",
        ),
        (
            "1.3",
            "remote13",
            "tls_memory_process_tests::remote_chain_observations",
        ),
        (
            "1.3",
            "remote13large",
            "tls_memory_process_tests::remote_large_ticket_observations",
        ),
    ] {
        for (domain, binary) in [
            ("rust", "td-mta-rust-allocation-probe"),
            ("rss", "td-mta-rss-probe"),
        ] {
            let mut command = Command::new("/artifacts/td-mta-transport-smoke");
            command
                .args([
                    "--exact",
                    case,
                    "--ignored",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env_clear()
                .env("TD_MTA_TEST_TLS_PEER", "/artifacts/td-crypto-smoke")
                .env("TD_MTA_TEST_MEMORY_PROBE", format!("/artifacts/{binary}"))
                .env("TD_MTA_TEST_MEMORY_DOMAIN", domain)
                .env("TD_MTA_TEST_PEER_VERSION", version)
                .stdin(Stdio::null());
            crate::host_bin::arm_check_child(&mut command);
            let name = format!("remote-chain-{domain}-{scenario}");
            let output = bounded_output(&mut command, &name, 64 * 1024, 30).inspect_err(|_| {
                let path = Path::new("/output").join(format!("{name}.log"));
                if let Ok(log) = read_output(&path, &name, 64 * 1024) {
                    eprintln!("portable remote-chain failure in {name}:\n{log}");
                }
            })?;
            let observations = remote_chain_evidence(&output, domain, scenario)?;
            for line in observations.lines() {
                println!("portable remote-chain diagnostic: {line}");
            }
        }
    }

    println!("portable runtime: version, SHA-256 facade/failure and mail-format probes, PEM/identity/trust, entropy and P-256/oracle probes, explicit algorithm policy, owned TLS signing, inbound/outbound configuration/clock and eighteen backend TLS cases, both configuration host scenarios and both bounded configuration stacks passed without toolchain mounts");
    println!("portable runtime: sixty-five mail SMTP/policy/generation/gateway/admission/clock/TCP/TLS cases passed without toolchain mounts");
    Ok(())
}

pub(crate) fn build(root: &Path, archives: &Path) -> Result<std::path::PathBuf> {
    build_architecture(root, archives, Architecture::X86_64)
}

pub(crate) fn cross_build(root: &Path, archives: &Path) -> Result<std::path::PathBuf> {
    build_architecture(root, archives, Architecture::Aarch64)
}

/// The static, source-fingerprinted td-builder that enters each namespace.
fn portable_helper(root: &Path) -> Result<PathBuf> {
    let helper_base = root.join(".td-build-cache/stage0");
    let placed = crate::stage0::stage0_place(root, &helper_base)?;
    let placed = Path::new(&placed)
        .file_name()
        .ok_or("portable helper has a malformed store path")?;
    let helper = helper_base
        .join("store")
        .join(placed)
        .join("bin/td-builder");
    crate::elf::assert_static(&helper)?;
    Ok(helper)
}

/// Where the emulated runtime's namespace mounts the host's emulator.
const EMULATOR: &str = "/emulator/qemu-aarch64";
const EMULATOR_NAME: &str = "qemu-aarch64";
const ARM_ARTIFACT_PREFIX: &str = "crypto-artifact-aarch64-cross-";

/// The host's QEMU user-mode emulator for aarch64, the first `qemu-aarch64`
/// on PATH. It is a host tool of this qualification, mounted read-only into
/// the runtime namespace only, and never a build or target input.
fn host_emulator() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        let Ok(resolved) = directory.join(EMULATOR_NAME).canonicalize() else {
            continue;
        };
        if fs::metadata(&resolved).is_ok_and(|m| m.is_file()) {
            return Ok(resolved);
        }
    }
    Err(format!(
        "the aarch64 runtime needs QEMU user-mode emulation, and no {EMULATOR_NAME} is on PATH; nothing was run"
    ))
}

/// The library paths `ld.so --list` printed, each as the loader named it:
/// the target of `=>`, or a line's own absolute path (the interpreter).
/// The vDSO has no path. An unresolved library refuses.
fn loader_paths(listing: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for line in listing.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("not found") {
            return Err(format!("emulator library unresolved: {line}"));
        }
        let named = match line.split_once("=>") {
            Some((_, target)) => target.trim(),
            None => line,
        };
        let Some(path) = named.split_whitespace().next() else {
            continue;
        };
        if path.starts_with('/') && !paths.iter().any(|known| known == path) {
            paths.push(path.to_owned());
        }
    }
    Ok(paths)
}

/// Where a loader-named spelling lands inside the namespace, whose
/// directories are all plain: `.` and empty components dropped and each
/// `..` taking its parent. A path that is not absolute, climbs past the
/// root or names no file refuses. (Guix's loader prints RUNPATH spellings
/// such as `.../lib/../lib/libffi.so.8`.)
fn lexical_path(path: &str) -> Result<String> {
    let rest = path
        .strip_prefix('/')
        .ok_or_else(|| format!("emulator library path is not absolute: {path}"))?;
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts
                    .pop()
                    .ok_or_else(|| format!("emulator library path climbs past the root: {path}"))?;
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(format!("emulator library path names no file: {path}"));
    }
    Ok(format!("/{}", parts.join("/")))
}

/// A loader-named library's host file and the namespace path to bind it at.
/// On the host a `..` climbs out of a symlinked directory physically, so the
/// spelling and its lexical form must reach the same file, or it refuses
/// rather than bind one file where the guest's loader expects another.
fn library_bind(path: &str) -> Result<(PathBuf, String)> {
    let lexical = lexical_path(path)?;
    let source = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("resolve emulator library {path}: {e}"))?;
    let reached = Path::new(&lexical)
        .canonicalize()
        .map_err(|e| format!("resolve emulator library {lexical}: {e}"))?;
    if source != reached {
        return Err(format!(
            "emulator library {path} is {} on the host, but its spelling reaches {}",
            source.display(),
            reached.display()
        ));
    }
    Ok((source, lexical))
}

/// What a dynamically linked emulator loads, as its own interpreter resolves
/// it; a static emulator loads nothing.
fn emulator_closure(emulator: &Path) -> Result<Vec<String>> {
    let bytes = fs::read(emulator).map_err(|e| format!("read {EMULATOR_NAME}: {e}"))?;
    let Some((offset, size)) = td_engine::elf::interp_slot(&bytes)? else {
        return Ok(Vec::new());
    };
    let interpreter = bytes
        .get(offset..offset.saturating_add(size))
        .ok_or("emulator interpreter is out of range")?;
    let interpreter = std::str::from_utf8(interpreter)
        .map_err(|_| "emulator interpreter is not UTF-8")?
        .trim_end_matches('\0');
    lexical_path(interpreter)?;
    let output = Command::new(interpreter)
        .arg("--list")
        .arg(emulator)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("list {EMULATOR_NAME}'s libraries: {e}"))?;
    if !output.status.success() {
        return Err(format!("{interpreter} --list {EMULATOR_NAME} failed"));
    }
    let listing = String::from_utf8(output.stdout)
        .map_err(|_| "emulator library listing is not UTF-8".to_owned())?;
    let mut paths = loader_paths(&listing)?;
    if !paths.iter().any(|path| path == interpreter) {
        paths.push(interpreter.to_owned());
    }
    Ok(paths)
}

/// Run a published aarch64 cross-build's functional cases under the host's
/// user-mode emulator, in a fresh namespace holding only those executables,
/// the emulator and what it loads. Functional evidence only: emulation says
/// nothing of aarch64 timing or of a real core's behavior.
pub(crate) fn emulated_runtime(root: &Path, artifact: &Path) -> Result<()> {
    if std::env::consts::ARCH != "x86_64" || std::env::consts::OS != "linux" {
        return Err("the emulated aarch64 runtime currently runs on x86-64 Linux only".into());
    }
    let emulator = host_emulator()?;
    let cache = root
        .join(".td-build-cache")
        .canonicalize()
        .map_err(|e| format!("portable cache: {e}"))?;
    let artifact = artifact
        .canonicalize()
        .map_err(|e| format!("aarch64 artifact: {e}"))?;
    let digest = artifact
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(ARM_ARTIFACT_PREFIX))
        .filter(|_| artifact.parent() == Some(cache.as_path()))
        .ok_or(
            "name a published crypto-artifact-aarch64-cross-<hash> directory in .td-build-cache",
        )?;
    let actual = crate::sandbox::nar_hash_of(&artifact)
        .map_err(|e| format!("hash aarch64 artifact: {e}"))?;
    if actual.strip_prefix("sha256:") != Some(digest) {
        return Err("aarch64 artifact differs from its published hash".into());
    }
    let scratch = Scratch::new(root)?;
    let artifacts = scratch.0.join("artifacts");
    fs::create_dir(&artifacts).map_err(|e| format!("create emulated artifacts: {e}"))?;
    for name in ARM_BINARIES.iter().chain(&["td-mta.debug"]) {
        copy_binary(&artifact.join(name), &artifacts.join(name))?;
    }
    qualify_artifacts_architecture(&artifacts, Architecture::Aarch64)?;
    let version = Command::new(&emulator)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run {EMULATOR_NAME} --version: {e}"))?;
    let version = String::from_utf8_lossy(&version.stdout);
    let version = version.lines().next().unwrap_or_default().to_owned();
    let helper = portable_helper(root)?;
    let mut binds = vec![
        bind(&artifacts, "/artifacts", true)?,
        bind(&helper, "/td-builder", true)?,
        bind(&emulator, EMULATOR, true)?,
    ];
    let mut bound: Vec<String> = Vec::new();
    for path in emulator_closure(&emulator)? {
        let (source, destination) = library_bind(&path)?;
        if !bound.contains(&destination) {
            binds.push(bind(&source, &destination, true)?);
            bound.push(destination);
        }
    }
    println!(
        "aarch64 runtime: {} ({version}), user-mode emulation; functional evidence only, no aarch64 timing or core behavior",
        emulator.display()
    );
    enter(
        "/td-builder",
        &[
            "gate-crates".into(),
            "crypto-portable-emulated-runtime-inner".into(),
        ],
        &binds,
        &scratch.0.join("runtime"),
        true,
    )
}

fn build_architecture(
    root: &Path,
    archives: &Path,
    architecture: Architecture,
) -> Result<std::path::PathBuf> {
    if std::env::consts::ARCH != "x86_64" || std::env::consts::OS != "linux" {
        return Err("portable build host qualification currently covers x86-64 Linux only".into());
    }
    crate::crypto_build::validate(root)?;
    let inputs = crate::crypto_portable::inputs_architecture(root, archives, architecture)?;
    let vendor = crate::host_bin::prepare_mail_vendor(root)?;
    let scratch = Scratch::new(root)?;
    let source = scratch.0.join("source");
    stage_sources(root, &source)?;
    crate::crypto_build::validate_sources(&source)?;
    let source_digest =
        crate::sandbox::nar_hash_of(&source).map_err(|e| format!("hash portable source: {e}"))?;
    let vendor_digest =
        crate::sandbox::nar_hash_of(&vendor).map_err(|e| format!("hash portable vendor: {e}"))?;
    let kit_digest = crate::sandbox::nar_hash_of(&inputs.rust)
        .map_err(|e| format!("hash portable Rust: {e}"))?;
    let expected_kit = match architecture {
        Architecture::X86_64 => {
            "sha256:7a54a59363f8c6168c6a7dad8be14ea357ce45e51e8d569a69e36737fd9ad962"
        }
        Architecture::Aarch64 => {
            "sha256:984aaf7e17b2d9a4b2a5043e090c76e63abdda088d6b5ed2deec0a837af71210"
        }
    };
    if kit_digest != expected_kit {
        return Err("prepared Rust kit differs from the qualified portable kit".into());
    }
    let helper = portable_helper(root)?;
    let output = scratch.0.join("output");
    fs::create_dir(&output).map_err(|e| format!("create portable output: {e}"))?;
    let binds = compile_binds(&source, &vendor, &inputs, &helper, &output)?;
    let musl_digest = crate::sandbox::nar_hash_of(&inputs.musl)
        .map_err(|e| format!("hash portable musl notice: {e}"))?;
    let helper_digest =
        crate::sandbox::nar_hash_of(&helper).map_err(|e| format!("hash portable helper: {e}"))?;
    let target = architecture.triple();
    let mut receipt = format!(
        "portable-build 1\nsource {source_digest}\nvendor {vendor_digest}\nrust-kit {kit_digest}\nmusl-notice {musl_digest}\nhelper {helper_digest}\ntarget {target}\n"
    );
    receipt.push_str(&inputs.build_root.record);
    enter(
        "/td-builder",
        &[
            "gate-crates".into(),
            match architecture {
                Architecture::X86_64 => "crypto-portable-build-inner".into(),
                Architecture::Aarch64 => "crypto-portable-cross-build-inner".into(),
            },
        ],
        &binds,
        &scratch.0.join("compile"),
        false,
    )?;
    let artifacts = scratch.0.join("artifacts");
    receipt.push_str(&collect_artifacts_architecture(
        &output,
        &artifacts,
        architecture,
    )?);
    qualify_artifacts_architecture(&artifacts, architecture)?;
    if architecture == Architecture::X86_64 {
        let runtime = vec![
            bind(&artifacts, "/artifacts", true)?,
            bind(&helper, "/td-builder", true)?,
        ];
        enter(
            "/td-builder",
            &["gate-crates".into(), "crypto-portable-runtime-inner".into()],
            &runtime,
            &scratch.0.join("runtime"),
            true,
        )?;
    }
    vendor_notices(
        &vendor,
        &artifacts.join("notices/vendor"),
        &crate::crypto_policy::active_vendor_dirs(),
    )?;
    retain_notices(&inputs.rust, &artifacts.join("notices/rust"), 0)?;
    retain_notices(&inputs.musl, &artifacts.join("notices/musl"), 0)?;
    write_new(&artifacts.join("BUILD-INPUTS"), receipt.as_bytes())?;
    let digest = crate::sandbox::nar_hash_of(&artifacts)
        .map_err(|e| format!("hash portable artifact: {e}"))?;
    crate::crypto_portable::publish_tree(
        &artifacts,
        &root.join(".td-build-cache"),
        match architecture {
            Architecture::X86_64 => "artifact",
            Architecture::Aarch64 => "artifact-aarch64-cross",
        },
        &digest,
    )
}

// Every package the artifact compiles must carry a notice. A locked-only
// package (a macOS or wasm backend) is vendored but compiled into nothing, so
// its notices are kept when it ships them and not required when it does not.
fn vendor_notices(
    vendor: &Path,
    destination: &Path,
    active: &std::collections::BTreeSet<String>,
) -> Result<()> {
    for package in fs::read_dir(vendor).map_err(|e| format!("read vendor notices: {e}"))? {
        let package = package.map_err(|e| format!("vendor notice entry: {e}"))?;
        let name = package.file_name();
        let required = name.to_str().is_some_and(|name| active.contains(name));
        if !package
            .file_type()
            .map_err(|e| format!("vendor notice type: {e}"))?
            .is_dir()
            || (retain_notices(&package.path(), &destination.join(&name), 0)? == 0 && required)
        {
            return Err(format!(
                "vendor package lacks notices: {}",
                package.path().display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn retain_notices(source: &Path, destination: &Path, depth: usize) -> Result<usize> {
    if depth > 64 {
        return Err("portable notice nesting exceeds 64 directories".into());
    }
    let mut count = 0usize;
    for entry in fs::read_dir(source).map_err(|e| format!("read notice sources: {e}"))? {
        let entry = entry.map_err(|e| format!("read notice entry: {e}"))?;
        let kind = entry
            .file_type()
            .map_err(|e| format!("inspect notice: {e}"))?;
        let name = entry.file_name();
        let selected = name.to_str().is_some_and(|name| {
            let name = name.to_ascii_uppercase();
            ["LICENSE", "COPYING", "COPYRIGHT", "NOTICE", "AUTHORS"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        });
        if kind.is_dir() {
            count += retain_notices(&entry.path(), &destination.join(name), depth + 1)?;
        } else if selected {
            if !kind.is_file() {
                return Err("portable notice must be a regular file".into());
            }
            fs::create_dir_all(destination).map_err(|e| format!("create notices: {e}"))?;
            source_tree(&entry.path(), &destination.join(name), 0)?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    const GATE_SYMBOLS: &str = "\
0000000000001000 0000000000000020 t td_primitives::aes_gcm::sbox
0000000000002000 0000000000000040 t td_primitives::bignum::mont_mul
0000000000002000 0000000000000040 t td_primitives::bignum::mont_mul_alias
0000000000003000 0000000000000010 T memcpy
0000000000004000 0000000000000010 d td_primitives::aes_gcm::TABLE
";

    fn gate(text: &str, architecture: Architecture, size: u64) -> Result<(usize, usize)> {
        let symbols = text_symbols(GATE_SYMBOLS).unwrap();
        branch_free(
            text,
            architecture,
            &symbols,
            "td_primitives::aes_gcm::sbox",
            0x1000,
            size,
        )
    }

    #[test]
    fn text_symbols_take_sized_code_only() {
        let symbols = text_symbols(GATE_SYMBOLS).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "td_primitives::aes_gcm::sbox",
                "td_primitives::bignum::mont_mul",
                "td_primitives::bignum::mont_mul_alias",
                "memcpy",
            ]
        );
        let second = symbols.get(1).unwrap();
        assert_eq!((second.address, second.size), (0x2000, 0x40));
        assert!(text_symbols("zz 10 t name\n").is_err());
    }

    /// objdump's x86-64 listing at `--insn-width=16`: 0x1b bytes.
    const X86_CLEAN: &str = "\n/output/artifacts/td-mta:     file format elf64-x86-64\n\n\
Disassembly of section .text:\n\n\
0000000000001000 <td_primitives::aes_gcm::sbox>:\n\
    1000:\t55                               \tpush   %rbp\n\
    1001:\t48 89 e5                         \tmov    %rsp,%rbp\n\
    1004:\t48 0f 45 d0                      \tcmovne %rax,%rdx\n\
    1008:\teb 00                            \tjmp    100a <td_primitives::aes_gcm::sbox+0xa>\n\
    100a:\te8 f1 0f 00 00                   \tcall   2000 <td_primitives::bignum::mont_mul>\n\
    100f:\t2e 66 0f 1f 84 00 00 00 00 00    \tcs nopw 0x0(%rax,%rax,1)\n\
    1019:\tf2 c3                            \tbnd ret\n";

    #[test]
    fn x86_branch_free_routine_passes_and_each_branch_form_fails() {
        let clean = X86_CLEAN;
        assert_eq!(gate(clean, Architecture::X86_64, 0x1b).unwrap(), (7, 1));
        for branch in [
            "jne    1000",
            "je     1000",
            "jb     1000",
            "jrcxz  1000",
            "loop   1000",
            "loopne 1000",
            "bnd jae 1000",
        ] {
            let text = clean.replace("cmovne %rax,%rdx", branch);
            let error = gate(&text, Architecture::X86_64, 0x1b).unwrap_err();
            assert!(error.contains("no conditional branch"), "{branch}: {error}");
        }
        for (call, expected) in [
            ("call   3000 <memcpy>", "neither branch-free"),
            ("call   *0x10(%rip)", "indirect"),
            ("jmp    *%rax", "indirect"),
            ("notrack jmp *%rax", "indirect"),
            ("jmp    3000 <memcpy>", "neither branch-free"),
            ("(bad)", "unrecognised"),
            (".byte 0xff", "unrecognised"),
        ] {
            let text = clean.replace("call   2000 <td_primitives::bignum::mont_mul>", call);
            let error = gate(&text, Architecture::X86_64, 0x1b).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
    }

    /// objdump's aarch64 listing: 0x18 bytes.
    const ARM_CLEAN: &str = "\n0000000000001000 <td_primitives::aes_gcm::sbox>:\n\
    1000:\ta9bf7bfd \tstp\tx29, x30, [sp, #-16]!\n\
    1004:\t9a821020 \tcsel\tx0, x1, x2, ne\n\
    1008:\t14000001 \tb\t100c <td_primitives::aes_gcm::sbox+0xc>\n\
    100c:\t94000000 \tbl\t2000 <td_primitives::bignum::mont_mul>\n\
    1010:\ta8c17bfd \tldp\tx29, x30, [sp], #16\n\
    1014:\td65f03c0 \tret\n";

    #[test]
    fn aarch64_branch_free_routine_passes_and_each_branch_form_fails() {
        let clean = ARM_CLEAN;
        assert_eq!(gate(clean, Architecture::Aarch64, 0x18).unwrap(), (6, 1));
        for branch in [
            "b.ne\t1000",
            "b.eq\t1000",
            "bc.lt\t1000",
            "cbz\tx0, 1000",
            "cbnz\tw1, 1000",
            "cbgt\tx0, x1, 1000",
            "tbz\tx2, #3, 1000",
            "tbnz\tx2, #63, 1000",
        ] {
            let text = clean.replace("csel\tx0, x1, x2, ne", branch);
            let error = gate(&text, Architecture::Aarch64, 0x18).unwrap_err();
            assert!(error.contains("no conditional branch"), "{branch}: {error}");
        }
        for data in [
            "bic\tx0, x1, x2",
            "bsl\tv0.16b, v1.16b, v2.16b",
            "bfi\tx0, x1, #3, #4",
            "bti\tc",
        ] {
            let text = clean.replace("csel\tx0, x1, x2, ne", data);
            assert!(gate(&text, Architecture::Aarch64, 0x18).is_ok(), "{data}");
        }
        for (call, expected) in [
            ("bl\t3000 <memcpy>", "neither branch-free"),
            ("blr\tx8", "indirect"),
            ("br\tx16", "indirect"),
            ("b\t3000 <memcpy>", "neither branch-free"),
            ("braa\tx16, x17", "unrecognised"),
            ("braaz\tx16", "unrecognised"),
            ("blraa\tx8, x9", "unrecognised"),
            ("blraaz\tx8", "unrecognised"),
            ("retaa", "unrecognised"),
            ("eret", "unrecognised"),
            ("brk\t#0x1", "unrecognised"),
            (".word\t0xffffffff", "unrecognised"),
            (".inst\t0xd503233f", "unrecognised"),
            ("(bad)", "unrecognised"),
        ] {
            let text = clean.replace("bl\t2000 <td_primitives::bignum::mont_mul>", call);
            let error = gate(&text, Architecture::Aarch64, 0x18).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
        // An x86 reading of aarch64 text sees no jcc, but the aarch64 one does.
        let branchy = clean.replace("csel\tx0, x1, x2, ne", "b.ne\t1000");
        assert!(gate(&branchy, Architecture::X86_64, 0x18).is_ok());
    }

    #[test]
    fn disassembly_must_cover_exactly_the_symbol() {
        assert!(gate(ARM_CLEAN, Architecture::Aarch64, 0x18).is_ok());
        // A listing that stops early, as objdump does at an undecodable tail.
        let error = gate(ARM_CLEAN, Architecture::Aarch64, 0x1c).unwrap_err();
        assert!(error.contains("short of its end"), "{error}");
        let error = gate(ARM_CLEAN, Architecture::Aarch64, 0x14).unwrap_err();
        assert!(error.contains("outside"), "{error}");
        let late = "    1004:\td65f03c0 \tret\n";
        let error = gate(late, Architecture::Aarch64, 0x8).unwrap_err();
        assert!(error.contains("does not cover"), "{error}");
        let gap = ARM_CLEAN.replace(
            "1008:\t14000001 \tb\t100c <td_primitives::aes_gcm::sbox+0xc>\n",
            "",
        );
        let error = gate(&gap, Architecture::Aarch64, 0x18).unwrap_err();
        assert!(error.contains("does not cover"), "{error}");
        assert!(gate("", Architecture::X86_64, 0x1).is_err());
        let wrapped = "    1000:\t48 89 e5\n";
        let error = gate(wrapped, Architecture::X86_64, 0x3).unwrap_err();
        assert!(error.contains("wrapped"), "{error}");
        let x86 = "    1000:\tc3                   \tret\n";
        assert!(gate(x86, Architecture::X86_64, 0x1).is_ok());
    }

    #[test]
    fn branch_free_list_names_distinct_symbols() {
        let mut names = BRANCH_FREE.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BRANCH_FREE.len());
        assert!(COUNTED_CALLEES
            .iter()
            .all(|name| !BRANCH_FREE.contains(name)));
    }

    #[test]
    fn emulator_closure_takes_each_resolved_path_once_and_refuses_a_missing_one() {
        let listing = "\tlinux-vdso.so.1 (0x00007ffd)\n\
            \tlibz.so.1 => /gnu/store/z/lib/libz.so.1 (0x00007f01)\n\
            \tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f02)\n\
            \tlibz.so.1 => /gnu/store/z/lib/libz.so.1 (0x00007f01)\n\
            \t/lib64/ld-linux-x86-64.so.2 (0x00007f03)\n";
        assert_eq!(
            loader_paths(listing).unwrap(),
            [
                "/gnu/store/z/lib/libz.so.1",
                "/lib/x86_64-linux-gnu/libc.so.6",
                "/lib64/ld-linux-x86-64.so.2",
            ]
        );
        assert!(loader_paths("\tlibgnutls.so.30 => not found\n").is_err());
        assert!(loader_paths("").unwrap().is_empty());
        // Guix's loader prints RUNPATH spellings, kept as printed.
        let runpath = "\tlibffi.so.8 => /gnu/store/f/lib/../lib/libffi.so.8 (0x1)\n";
        assert_eq!(
            loader_paths(runpath).unwrap(),
            ["/gnu/store/f/lib/../lib/libffi.so.8"]
        );
    }

    #[test]
    fn library_paths_bind_where_their_spelling_lands_and_nowhere_else() {
        for (path, lexical) in [
            (
                "/gnu/store/f/lib/../lib/libffi.so.8",
                "/gnu/store/f/lib/libffi.so.8",
            ),
            ("/lib/./x.so", "/lib/x.so"),
            ("/lib//x.so", "/lib/x.so"),
            ("/lib64/ld-linux-x86-64.so.2", "/lib64/ld-linux-x86-64.so.2"),
        ] {
            assert_eq!(lexical_path(path).unwrap(), lexical, "{path}");
        }
        for path in ["lib/x.so", "/..", "/lib/../../x.so", "/", "/lib/.."] {
            assert!(lexical_path(path).is_err(), "{path}");
        }
        let scratch = crate::test_temp::TempDir::new("td-crypto-emulator-libs").unwrap();
        let real = scratch.join("store/lib");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("x.so"), b"real").unwrap();
        symlink(real.join("x.so"), real.join("x.so.1")).unwrap();
        let root = scratch.to_str().unwrap();
        // A symlinked file and a `..` through plain directories bind fine.
        let (source, destination) =
            library_bind(&format!("{root}/store/lib/../lib/x.so.1")).unwrap();
        assert_eq!(source, real.join("x.so").canonicalize().unwrap());
        assert_eq!(destination, format!("{root}/store/lib/x.so.1"));
        // A `..` out of a symlinked directory reaches another file on the
        // host than its spelling does in the namespace.
        symlink(&real, scratch.join("link")).unwrap();
        fs::create_dir_all(scratch.join("lib")).unwrap();
        fs::write(scratch.join("lib/x.so"), b"other").unwrap();
        let error = library_bind(&format!("{root}/link/../lib/x.so")).unwrap_err();
        assert!(error.contains("its spelling reaches"), "{error}");
        assert!(library_bind(&format!("{root}/store/lib/missing.so")).is_err());
    }

    #[test]
    fn every_portable_case_is_named_once_and_probes_follow_the_functional_cases() {
        let cases: Vec<_> = portable_cases().collect();
        let functional: usize = FUNCTIONAL_CASES.iter().map(|(_, cases)| cases.len()).sum();
        assert_eq!(cases.len(), functional + STACK_PROBES.len());
        let mut seen = std::collections::BTreeSet::new();
        for (binary, case, ignored) in &cases {
            assert!(seen.insert((*binary, *case)), "{binary} {case}");
            assert_eq!(*ignored, STACK_PROBES.contains(&(*binary, *case)));
            assert!(ARM_BINARIES.contains(binary), "{binary}");
        }
        assert!(cases
            .iter()
            .take(functional)
            .all(|(_, _, ignored)| !ignored));
    }

    #[test]
    fn rss_symbols_require_the_named_probe_only_in_its_artifact() {
        let probe = "000001 T rss_probe::main\n";
        let plain = "000001 T main\n";
        assert!(rss_symbols(probe, true).is_ok());
        assert!(rss_symbols(plain, false).is_ok());
        assert!(rss_symbols(probe, false).is_err());
        assert!(rss_symbols(plain, true).is_err());
        for intended in [false, true] {
            assert!(rss_symbols(" \n", intended).is_err());
        }
    }

    #[test]
    fn rss_handshake_records_require_all_endpoint_release_phases() {
        for scenario in ["handshake", "large-chain"] {
            let mut output = String::new();
            for phase in [
                "baseline",
                "material",
                "config",
                "generation",
                "buffers",
                "constructed",
                "handshake",
                "record",
                "repeated",
                "client_released",
                "released",
                "buffers_released",
                "dropped",
            ] {
                output.push_str(&format!("rss {scenario} {phase} 100\n"));
            }
            output.push_str(&format!("rss-observation-v2: {scenario} passed\n"));
            assert!(rss_evidence(&output, scenario).is_ok());
            for bad in [
                output.replace(&format!("rss {scenario} client_released 100\n"), ""),
                output.replace(&format!("rss {scenario} buffers_released 100\n"), ""),
                output.replace("client_released", "buffers_released"),
                output.replace("buffers_released", "released"),
                output.replace("-v2:", "-v1:"),
                format!("{output}extra\n"),
            ] {
                assert!(rss_evidence(&bad, scenario).is_err());
            }
        }
    }

    #[test]
    fn rss_records_require_positive_values_exact_phases_and_completion() {
        let good = "rss control baseline 100\nrss control touched 17000\nrss control dropped 110\nrss-observation-v2: control passed\n";
        assert!(rss_evidence(good, "control").is_ok());
        assert!(rss_evidence(good, "client").is_err());
        assert!(rss_evidence(good, "unknown").is_err());
        for bad in [
            good.replace("touched", "baseline"),
            good.replace("100", "0"),
            good.replace("100", "-1"),
            good.replace("100", "18446744073709551616"),
            good.replace("100", "100 1"),
            good.replace("passed", "failed"),
            format!("{good}extra\n"),
        ] {
            assert!(rss_evidence(&bad, "control").is_err());
        }
        assert!(rss_evidence(good.trim_end(), "control").is_err());
        assert!(rss_evidence(&good.replace("-v2:", "-v1:"), "control").is_err());
    }

    #[test]
    fn fragment_records_require_pending_refusal_and_repetition() {
        let domain = "rust";
        let values = "1 2 3 4 5 6 7";
        let mut output = String::new();
        for phase in [
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "over_limit",
            "repeated",
            "dropped",
        ] {
            output.push_str(&format!("tls-{domain}-fragment {phase} {values}\n"));
        }
        output.push_str(&format!("tls-fragment-allocation-v1: {domain} passed\n"));
        assert!(tls_fragment_evidence(&output).is_ok());
        assert!(tls_handshake_evidence(&output).is_err());
        assert!(entropy_worker_evidence(&output).is_err());
        for bad in [
            output.replace("large_pending", "large_refused"),
            output.replace(" 1 2", " 1"),
            output.replace(" 1 2", " x 2"),
            output.replace("passed", "failed"),
            format!("{output}extra\n"),
        ] {
            assert!(tls_fragment_evidence(&bad).is_err());
        }
    }

    #[test]
    fn certificate_list_records_require_pending_refusal_and_repetition() {
        let domain = "rust";
        let values = "1 2 3 4 5 6 7";
        let mut output = String::new();
        for phase in [
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "repeated",
            "dropped",
        ] {
            output.push_str(&format!("tls-{domain}-certificate-list {phase} {values}\n"));
        }
        output.push_str(&format!(
            "tls-certificate-list-allocation-v1: {domain} passed\n"
        ));
        assert!(tls_certificate_list_evidence(&output).is_ok());
        let mut rss = String::new();
        for phase in [
            "baseline",
            "policy",
            "storage",
            "large_constructed",
            "large_pending",
            "large_refused",
            "small_constructed",
            "small_pending",
            "small_refused",
            "repeated",
            "dropped",
        ] {
            rss.push_str(&format!("rss certificate-list {phase} 100\n"));
        }
        rss.push_str("rss-observation-v2: certificate-list passed\n");
        assert!(rss_evidence(&rss, "certificate-list").is_ok());
        assert!(rss_evidence(&rss, "fragment").is_err());
        for bad in [
            rss.replace("certificate-list", "fragment"),
            rss.replace("large_pending", "large_refused"),
            rss.replace("-v2:", "-v1:"),
            rss.trim_end().to_owned(),
        ] {
            assert!(rss_evidence(&bad, "certificate-list").is_err());
        }
        assert!(tls_handshake_evidence(&output).is_err());
        assert!(tls_fragment_evidence(&output).is_err());
        assert!(
            tls_certificate_list_evidence(&output.replace("certificate-list", "fragment")).is_err()
        );
        assert!(entropy_worker_evidence(&output).is_err());
        for bad in [
            output.replace("large_pending", "large_refused"),
            output.replace(" 1 2", " 1"),
            output.replace(" 1 2", " x 2"),
            output.replace("passed", "failed"),
            output.replace("-v1:", "-v2:"),
            output.trim_end().to_owned(),
            format!("{output}extra\n"),
        ] {
            assert!(tls_certificate_list_evidence(&bad).is_err());
        }
    }

    #[test]
    fn entropy_worker_records_require_teardown_and_distinct_phases() {
        let domain = "rust";
        let values = "1 2 3 4 5 6 7";
        let mut output = String::new();
        for phase in [
            "baseline",
            "spawned",
            "first_warm",
            "all_warm",
            "repeated",
            "joined",
            "dropped",
        ] {
            output.push_str(&format!("tls-{domain}-entropy {phase} {values}\n"));
        }
        output.push_str(&format!("tls-entropy-allocation-v1: {domain} passed\n"));
        assert!(entropy_worker_evidence(&output).is_ok());
        assert!(tls_handshake_evidence(&output).is_err());
        assert!(tls_allocation_evidence(&output).is_err());
        for bad in [
            output.replace("joined", "repeated"),
            output.replace(" 1 2", " 1"),
            output.replace(" 1 2", " -1 2"),
            output.replace("passed", "failed"),
            format!("{output}extra\n"),
        ] {
            assert!(entropy_worker_evidence(&bad).is_err());
        }
    }

    #[test]
    fn tls_handshake_records_are_distinct_from_client_construction() {
        let domain = "rust";
        let values = "1 2 3 4 5 6 7";
        let mut output = String::new();
        for phase in [
            "baseline",
            "material",
            "config",
            "generation",
            "buffers",
            "constructed",
            "handshake",
            "record",
            "repeated",
            "client_released",
            "released",
            "buffers_released",
            "dropped",
        ] {
            output.push_str(&format!("tls-{domain}-handshake {phase} {values}\n"));
        }
        output.push_str(&format!("tls-handshake-allocation-v2: {domain} passed\n"));
        assert!(tls_handshake_evidence(&output).is_ok());
        assert!(tls_handshake_evidence(&output.replace("-v2:", "-v1:")).is_err());
        assert!(tls_allocation_evidence(&output).is_err());
        for bad in [
            output.replace("material", "config"),
            output.replace(" 1 2", " 1"),
            output.replace(" 1 2", " x 2"),
            output.replace("passed", "failed"),
            format!("{output}extra\n"),
        ] {
            assert!(tls_handshake_evidence(&bad).is_err());
        }
    }

    #[test]
    fn remote_chain_records_require_the_controller_and_inner_schema() {
        for scenario in ["remote12", "remote13", "remote13large"] {
            for domain in ["rust", "rss"] {
                let mut body = String::new();
                for phase in REMOTE_CHAIN_PHASES {
                    if domain == "rss" {
                        body.push_str(&format!("rss {scenario} {phase} 100\n"));
                    } else {
                        let values = "1 2 3 4 5 6 7";
                        body.push_str(&format!("tls-{domain}-{scenario} {phase} {values}\n"));
                    }
                }
                if domain == "rss" {
                    body.push_str(&format!("rss-observation-v2: {scenario} passed\n"));
                } else {
                    body.push_str(&format!("tls-{scenario}-allocation-v1: {domain} passed\n"));
                }
                let output = format!("running 1 test\n\nremote-chain-output-begin\n{body}remote-chain-output-end\nremote-chain-controller-v2: {domain} {scenario} passed\nok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 20 filtered out; finished in 0.1s\n");
                assert_eq!(
                    remote_chain_evidence(&output, domain, scenario).unwrap(),
                    body
                );
                for other in ["remote12", "remote13", "remote13large"] {
                    if other == scenario {
                        continue;
                    }
                    assert!(remote_chain_evidence(&output, domain, other).is_err());
                    let wrong_inner = output.replacen(&body, &body.replace(scenario, other), 1);
                    assert!(remote_chain_evidence(&wrong_inner, domain, scenario).is_err());
                    let wrong_completion = output.replace(
                        &format!("remote-chain-controller-v2: {domain} {scenario} passed"),
                        &format!("remote-chain-controller-v2: {domain} {other} passed"),
                    );
                    assert!(remote_chain_evidence(&wrong_completion, domain, scenario).is_err());
                }
                assert!(remote_chain_evidence(&output, "unknown", scenario).is_err());
                assert!(remote_chain_evidence(&output, domain, "1.1").is_err());
                for bad in [
                    output.replace("remote-chain-output-begin", "missing"),
                    output.replace("remote-chain-output-end", "missing"),
                    output.replace(
                        "remote-chain-output-begin\n",
                        "remote-chain-output-begin\nremote-chain-output-begin\n",
                    ),
                    output.replace("buffers_released", "released"),
                    output.replace("controller-v2", "controller-v1"),
                    output.replace("1 passed;", "0 passed;"),
                    output.replace("0 failed;", "1 failed;"),
                    format!("{output}test result: ok. 0 passed; 0 failed; 0 ignored;\n"),
                    format!("{output}test result: FAILED. 0 passed; 1 failed;\n"),
                    output.replace(
                        "remote-chain-output-end\n",
                        "remote-chain-output-end\nremote-chain-output-end\n",
                    ),
                    output
                        .replace("remote12", "other12")
                        .replace("remote13", "other13"),
                ] {
                    assert!(remote_chain_evidence(&bad, domain, scenario).is_err());
                }
            }
        }
    }

    #[test]
    fn generation_records_require_complete_ordered_owners_and_domains() {
        for scenario in ["generation", "generation-routing", "generation-trust"] {
            let domain = "rust";
            let values = "1 2 3 4 5 6 7";
            let mut output = String::new();
            let mut rss = String::new();
            for phase in [
                "baseline",
                "material",
                "config",
                "first",
                "candidate",
                "overlap",
                "refused",
                "old_released",
                "repeated",
                "current_released",
                "dropped",
            ] {
                output.push_str(&format!("tls-{domain}-{scenario} {phase} {values}\n"));
                rss.push_str(&format!("rss {scenario} {phase} 100\n"));
            }
            output.push_str(&format!("tls-{scenario}-allocation-v1: {domain} passed\n"));
            rss.push_str(&format!("rss-observation-v2: {scenario} passed\n"));
            assert!(tls_generation_evidence(&output, scenario).is_ok());
            let unknown = output.replace(scenario, "unknown");
            assert!(tls_generation_evidence(&unknown, "unknown").is_err());
            assert!(rss_evidence(&rss.replace(scenario, "unknown"), "unknown").is_err());
            assert!(rss_evidence(&rss, scenario).is_ok());
            for other in [
                "generation",
                "generation-routing",
                "generation-trust",
                "unknown",
            ] {
                if other != scenario {
                    assert!(tls_generation_evidence(&output, other).is_err());
                    assert!(rss_evidence(&rss, other).is_err());
                }
            }
            assert!(tls_large_chain_evidence(&output).is_err());
            for bad in [
                output.replace("candidate", "overlap"),
                output.replace("current_released", "old_released"),
                output.replace("-v1:", "-v2:"),
                output.replace(&format!("tls-{domain}-{scenario} refused {values}\n"), ""),
                format!("{output}extra\n"),
                output.trim_end().to_owned(),
            ] {
                assert!(tls_generation_evidence(&bad, scenario).is_err());
            }
            for bad in [
                rss.replace("candidate", "overlap"),
                rss.replace(&format!("rss {scenario} refused 100\n"), ""),
                rss.replace("-v2:", "-v1:"),
                format!("{rss}extra\n"),
            ] {
                assert!(rss_evidence(&bad, scenario).is_err());
            }
        }
    }

    #[test]
    fn large_chain_records_are_distinct_from_ordinary_handshake() {
        let domain = "rust";
        let values = "1 2 3 4 5 6 7";
        let mut output = String::new();
        for phase in [
            "baseline",
            "material",
            "config",
            "generation",
            "buffers",
            "constructed",
            "handshake",
            "record",
            "repeated",
            "client_released",
            "released",
            "buffers_released",
            "dropped",
        ] {
            output.push_str(&format!("tls-{domain}-large-chain {phase} {values}\n"));
        }
        output.push_str(&format!("tls-large-chain-allocation-v2: {domain} passed\n"));
        assert!(tls_large_chain_evidence(&output).is_ok());
        assert!(tls_large_chain_evidence(&output.replace("-v2:", "-v1:")).is_err());
        assert!(tls_handshake_evidence(&output).is_err());
        for bad in [
            output.replace("material", "config"),
            output.replace(" 1 2", " 1"),
            output.replace(" 1 2", " x 2"),
            output.replace("passed", "failed"),
            format!("{output}extra\n"),
        ] {
            assert!(tls_large_chain_evidence(&bad).is_err());
        }
    }

    #[test]
    fn tls_allocation_records_require_all_ordered_phases_and_exact_width() {
        let domain = "rust";
        let values = "1 2 3 4 5 6 7";
        let mut output = String::new();
        for phase in [
            "baseline",
            "config",
            "generation",
            "buffers",
            "reserved",
            "constructed",
            "overlap",
            "two_sessions",
            "refused",
            "released",
            "repeated",
            "dropped",
        ] {
            output.push_str(&format!("tls-{domain} {phase} {values}\n"));
        }
        output.push_str(&format!("tls-client-allocation-v1: {domain} passed\n"));
        assert!(tls_allocation_evidence(&output).is_ok());
        for bad in [
            output.replace("baseline", "config"),
            output.replace(" 1 2", "  1 2"),
            output.replace(" 1 2", " -1 2"),
            output.replace(" 1 2", " 999999999999999999999999999999 2"),
            output.replace(" 1 2", " 1"),
            output.replace("passed", "failed"),
            format!("{output}extra\n"),
            output.trim_end().to_owned(),
        ] {
            assert!(tls_allocation_evidence(&bad).is_err());
        }
    }

    #[test]
    fn stack_measurement_requires_one_bounded_decimal_observation() {
        for (prefix, ceiling, size) in [
            ("config_stack_mapping_bytes=", 176 * 1024, 167936),
            (
                "tls_policy_transport_stack_mapping_bytes=",
                256 * 1024,
                249856,
            ),
            (
                "config_materialized_stack_mapping_bytes=",
                256 * 1024,
                249856,
            ),
        ] {
            assert_eq!(
                stack_evidence(&format!("noise\n{prefix}{size}\n"), prefix, ceiling).unwrap(),
                size
            );
            assert_eq!(
                stack_evidence(&format!("{prefix}{ceiling}"), prefix, ceiling).unwrap(),
                ceiling
            );
            let other = if prefix == "config_stack_mapping_bytes=" {
                "config_materialized_stack_mapping_bytes="
            } else {
                "config_stack_mapping_bytes="
            };
            assert!(stack_evidence(&format!("{other}1000"), prefix, ceiling).is_err());
            for bad in [
                String::new(),
                prefix.to_owned(),
                format!("{prefix}0"),
                format!("{prefix}{}", ceiling + 1),
                format!("{prefix}+1"),
                format!("{prefix}9999999999999999999999999999"),
                format!("{prefix}1\n{prefix}1"),
                "unrelated_mapping=1".into(),
            ] {
                assert!(stack_evidence(&bad, prefix, ceiling).is_err(), "{bad}");
            }
        }
    }

    #[test]
    fn allocation_probe_requires_symbols_and_exact_completion() {
        let counter = "000 b rust_alloc_probe_TD_MTA_ALLOCATION_COUNTERS";
        assert!(allocation_symbols(counter, true).is_ok());
        assert!(allocation_symbols(counter, false).is_err());
        assert!(allocation_symbols("000 t ordinary_main", false).is_ok());
        assert!(allocation_symbols("000 t ordinary_main", true).is_err());
        for text in [
            "",
            "000 b TD_MTA_ALLOCATION_COUNTERS",
            "000 t rust_alloc_probe",
        ] {
            assert!(allocation_symbols(text, true).is_err());
            assert!(allocation_symbols(text, false).is_err());
        }
        assert!(allocation_evidence(ALLOCATION_SUCCESS).is_ok());
        for bad in [
            "",
            ALLOCATION_SUCCESS.trim_end(),
            "test result: ok. 0 passed; 0 failed;\n",
        ] {
            assert!(allocation_evidence(bad).is_err());
        }
        assert!(allocation_evidence(&format!("{ALLOCATION_SUCCESS}{ALLOCATION_SUCCESS}")).is_err());
        assert!(allocation_evidence(&format!("{ALLOCATION_SUCCESS}failed\n")).is_err());
    }

    #[test]
    fn portable_symbols_refuse_interposition_and_native_crypto() {
        assert!(foreign_symbols("000 t ordinary_main\n000 T rust_begin_unwind\n").is_ok());
        // A td test whose name mentions AWS-LC is not AWS-LC.
        assert!(foreign_symbols(
            "000 t _RNvNtCs1a_9td_crypto13backend_tests36keys_aws_lc_wrote_load_and_sign_here\n"
        )
        .is_ok());
        assert!(foreign_symbols("").is_err());
        for name in [
            "__wrap_malloc",
            "__wrap_memalign",
            "aws_lc_0_47_0_EVP_DigestInit",
            "OPENSSL_malloc",
            "OPENSSL_memory_alloc",
            "BORINGSSL_self_test",
            "ring_core_0_17_14_bn_mul_mont",
            "_RNvNtCs1a_9aws_lc_rs6digest6digest",
        ] {
            assert!(
                foreign_symbols(&format!("000 t ordinary_main\n000 T {name}\n")).is_err(),
                "{name}"
            );
        }
        assert!(!rust_flags().contains("--wrap"));
    }

    #[test]
    fn cli_summary_requires_complete_unfiltered_suite() {
        let good = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1s\n";
        assert!(cli_evidence(good).is_ok());
        for bad in [
            String::new(),
            good.replace("3 passed", "0 passed"),
            good.replace("0 ignored", "1 ignored"),
            good.replace("0 filtered out", "1 filtered out"),
            good.replace("ok.", "FAILED."),
            format!("{good}{good}"),
        ] {
            assert!(cli_evidence(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_collection_refuses_missing_outputs_links_extras_and_oversized_receipts() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let output = scratch.0.join("output");
        let source = output.join("artifacts");
        fs::create_dir_all(&source).unwrap();
        for name in BINARIES.iter().copied().chain(["td-mta.debug", "COMMANDS"]) {
            fs::write(source.join(name), name).unwrap();
        }
        fs::remove_file(source.join("td-mta.debug")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-debug")).is_err());
        fs::write(source.join("td-mta.debug"), b"td-mta.debug").unwrap();
        fs::remove_file(source.join("td-mta-cli-smoke")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-cli")).is_err());
        fs::write(source.join("td-mta-cli-smoke"), b"td-mta-cli-smoke").unwrap();
        fs::remove_file(source.join("td-mta-format-smoke")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-format")).is_err());
        fs::write(source.join("td-mta-format-smoke"), b"td-mta-format-smoke").unwrap();
        fs::remove_file(source.join("td-mta-transport-smoke")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-transport")).is_err());
        fs::write(
            source.join("td-mta-transport-smoke"),
            b"td-mta-transport-smoke",
        )
        .unwrap();
        fs::remove_file(source.join("td-mta-rust-allocation-probe")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-allocation")).is_err());
        fs::write(source.join("td-mta-rust-allocation-probe"), b"probe").unwrap();
        fs::remove_file(source.join("td-mta-rss-probe")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-rss")).is_err());
        fs::write(source.join("td-mta-rss-probe"), b"rss").unwrap();
        let good = scratch.0.join("good");
        assert_eq!(collect_artifacts(&output, &good).unwrap(), "COMMANDS");
        assert_eq!(fs::read(good.join("td-mta")).unwrap(), b"td-mta");
        assert_eq!(
            fs::read(good.join("td-mta.debug")).unwrap(),
            b"td-mta.debug"
        );
        assert_eq!(
            fs::metadata(good.join("td-mta.debug"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            fs::read(good.join("td-mta-transport-smoke")).unwrap(),
            b"td-mta-transport-smoke"
        );
        let outside = scratch.0.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        fs::remove_file(source.join("COMMANDS")).unwrap();
        symlink(&outside, source.join("COMMANDS")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("bad-link")).is_err());
        fs::remove_file(source.join("COMMANDS")).unwrap();
        fs::write(source.join("COMMANDS"), vec![b'x'; 64 * 1024 + 1]).unwrap();
        assert_eq!(
            collect_artifacts(&output, &scratch.0.join("expanded"))
                .unwrap()
                .len(),
            64 * 1024 + 1
        );
        fs::write(source.join("COMMANDS"), vec![b'x'; 256 * 1024 + 1]).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("large")).is_err());
        fs::write(source.join("COMMANDS"), b"ok").unwrap();
        symlink(&outside, source.join("notices")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("extra-link")).is_err());
        fs::remove_file(source.join("notices")).unwrap();
        fs::write(source.join("extra"), b"extra").unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("extra-file")).is_err());
        fs::rename(&source, output.join("real")).unwrap();
        symlink(output.join("real"), &source).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("dir-link")).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"untouched");
    }

    #[test]
    fn bounded_output_and_path_refusals_do_not_hide_utf8_or_symlink_escapes() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let text = scratch.0.join("text");
        fs::write(&text, "éé").unwrap();
        assert!(read_output(&text, "fixture", 2)
            .unwrap_err()
            .contains("exceeds"));
        assert_eq!(read_output(&text, "fixture", 4).unwrap(), "éé");
        fs::write(&text, [0xff]).unwrap();
        assert!(read_output(&text, "fixture", 4)
            .unwrap_err()
            .contains("UTF-8"));
        let target = scratch.0.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&text, target.join("escape")).unwrap();
        assert!(confined_path(&target.join("escape"), &target).is_err());
        fs::write(target.join("binary"), b"ok").unwrap();
        assert!(confined_path(&target.join("binary"), &target).is_ok());
        refuse_decoy(&scratch.0).unwrap();
        assert!(record_decoy(&scratch.0).is_err());
        assert!(refuse_decoy(&scratch.0).is_err());
        fs::remove_file(scratch.0.join("native-fallback-called")).unwrap();
        symlink("absent", scratch.0.join("native-fallback-called")).unwrap();
        assert!(refuse_decoy(&scratch.0).is_err());
    }

    #[test]
    fn artifact_records_require_one_correct_profile_and_target() {
        let record = r#"{"reason":"compiler-artifact","target":{"name":"td-mta","kind":["bin"]},"profile":{"test":false},"executable":"/output/target/td-mta"}"#;
        assert_eq!(
            artifact_path(record, "td-mta", ArtifactKind::Installed).unwrap(),
            Path::new("/output/target/td-mta")
        );
        assert!(artifact_path(
            &format!("{record}\n{record}"),
            "td-mta",
            ArtifactKind::Installed
        )
        .is_err());
        assert!(artifact_path(record, "td-mta", ArtifactKind::LibraryTest).is_err());
        assert!(artifact_path(record, "td-mta", ArtifactKind::BinaryTest).is_err());
        assert!(artifact_path(
            &record.replace("false", "true"),
            "td-mta",
            ArtifactKind::BinaryTest
        )
        .is_ok());
        assert!(artifact_path(record, "td_crypto", ArtifactKind::Installed).is_err());
        assert!(artifact_path(
            &record.replace("[\"bin\"]", "[\"bin\",\"lib\"]"),
            "td-mta",
            ArtifactKind::Installed
        )
        .is_err());
        assert!(artifact_path("{", "td-mta", ArtifactKind::Installed).is_err());
        assert!(artifact_json(&format!("{}0{}", "[".repeat(65), "]".repeat(65))).is_err());
        assert!(artifact_json(r#"{"quoted":"[[[\"{}"}"#).is_ok());
        let test = record
            .replace("td-mta", "td_crypto")
            .replace("\"bin\"", "\"lib\"")
            .replace("false", "true");
        assert!(artifact_path(&test, "td_crypto", ArtifactKind::LibraryTest).is_ok());
        let integration = test
            .replace("td_crypto", "config_stack")
            .replace("\"lib\"", "\"test\"");
        assert!(artifact_path(&integration, "config_stack", ArtifactKind::IntegrationTest).is_ok());
        assert!(artifact_path(&test, "td_crypto", ArtifactKind::IntegrationTest).is_err());
        assert!(artifact_path(&integration, "config_stack", ArtifactKind::LibraryTest).is_err());
    }

    #[test]
    fn source_staging_excludes_configuration_and_refuses_links_and_missing_inputs() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let root = scratch.0.join("checkout");
        for package in crate::crypto_policy::LOCAL_SOURCES {
            let path = root.join(package);
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(path.join("src/lib.rs"), "source").unwrap();
            fs::write(path.join("Cargo.toml"), "manifest").unwrap();
            fs::write(path.join("Cargo.lock"), "lock").unwrap();
            fs::write(path.join("build.rs"), "do not copy").unwrap();
            fs::create_dir(path.join(".cargo")).unwrap();
            fs::write(path.join(".cargo/config.toml"), "do not copy").unwrap();
        }
        for relative in [
            "engine/src/sha256.rs",
            "td-fido/src/fido_p256.rs",
            "td-fido/tests/p256_vectors.txt",
        ] {
            let input = root.join(relative);
            fs::create_dir_all(input.parent().unwrap()).unwrap();
            fs::write(input, relative).unwrap();
        }
        fs::write(root.join("engine/src/not-an-oracle.rs"), "excluded").unwrap();
        let destination = scratch.0.join("staged");
        stage_sources(&root, &destination).unwrap();
        for relative in [
            "engine/src/sha256.rs",
            "td-fido/src/fido_p256.rs",
            "td-fido/tests/p256_vectors.txt",
        ] {
            assert_eq!(
                fs::read(destination.join(relative)).unwrap(),
                relative.as_bytes()
            );
        }
        assert!(!destination.join("engine/src/not-an-oracle.rs").exists());
        fs::remove_file(root.join("td-fido/tests/p256_vectors.txt")).unwrap();
        assert!(stage_sources(&root, &scratch.0.join("missing-oracle"))
            .unwrap_err()
            .starts_with("inspect oracle source:"));
        symlink(
            "../../engine/src/sha256.rs",
            root.join("td-fido/tests/p256_vectors.txt"),
        )
        .unwrap();
        assert_eq!(
            stage_sources(&root, &scratch.0.join("linked-oracle")).unwrap_err(),
            "oracle sources must be regular files, not links"
        );
        fs::remove_file(root.join("td-fido/tests/p256_vectors.txt")).unwrap();
        fs::write(root.join("td-fido/tests/p256_vectors.txt"), "restored").unwrap();
        for (index, relative) in [
            "engine",
            "engine/src",
            "td-fido",
            "td-fido/src",
            "td-fido/tests",
        ]
        .iter()
        .enumerate()
        {
            let source = root.join(relative);
            let retained = scratch.0.join("retained-oracle-directory");
            fs::rename(&source, &retained).unwrap();
            symlink(&retained, &source).unwrap();
            assert_eq!(
                stage_sources(
                    &root,
                    &scratch.0.join(format!("linked-oracle-parent-{index}"))
                )
                .unwrap_err(),
                "oracle source ancestors must be directories, not links"
            );
            fs::remove_file(&source).unwrap();
            fs::rename(&retained, &source).unwrap();
        }
        for package in crate::crypto_policy::LOCAL_SOURCES {
            assert_eq!(
                fs::read(destination.join(package).join("src/lib.rs")).unwrap(),
                b"source"
            );
            assert!(!destination.join(package).join(".cargo").exists());
            assert!(!destination.join(package).join("build.rs").exists());
        }
        fs::write(root.join("td-mta/src/lib.rs"), "changed").unwrap();
        assert_eq!(
            fs::read(destination.join("td-mta/src/lib.rs")).unwrap(),
            b"source"
        );
        symlink("lib.rs", root.join("td-mta/src/link.rs")).unwrap();
        assert!(stage_sources(&root, &scratch.0.join("linked"))
            .unwrap_err()
            .contains("regular files"));
        fs::remove_file(root.join("td-mta/src/link.rs")).unwrap();
        fs::remove_file(root.join("td-mta/Cargo.lock")).unwrap();
        assert!(stage_sources(&root, &scratch.0.join("missing")).is_err());
    }

    #[test]
    fn staged_source_policy_is_independent_of_host_ancestor_config() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        fs::create_dir(scratch.0.join(".cargo")).unwrap();
        fs::write(scratch.0.join(".cargo/config.toml"), "poison").unwrap();
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let staged = scratch.0.join("source");
        stage_sources(checkout, &staged).unwrap();
        crate::crypto_build::validate_sources(&staged).unwrap();
        assert!(crate::crypto_build::validate(&staged).is_err());
        fs::write(staged.join("td-mta/Cargo.toml"), "changed").unwrap();
        assert!(crate::crypto_build::validate_sources(&staged).is_err());
    }

    #[test]
    fn notices_include_nested_upstream_attribution_and_refuse_links() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let source = scratch.0.join("source");
        fs::create_dir_all(source.join("third_party/fiat")).unwrap();
        fs::write(source.join("LICENSE"), "root").unwrap();
        fs::write(source.join("third_party/fiat/COPYRIGHT.html"), "nested").unwrap();
        fs::write(source.join("third_party/fiat/implementation.c"), "excluded").unwrap();
        let target = scratch.0.join("notices");
        retain_notices(&source, &target, 0).unwrap();
        assert_eq!(
            fs::read(target.join("third_party/fiat/COPYRIGHT.html")).unwrap(),
            b"nested"
        );
        assert!(!target.join("third_party/fiat/implementation.c").exists());
        symlink("LICENSE", source.join("NOTICE")).unwrap();
        assert!(retain_notices(&source, &scratch.0.join("bad"), 0).is_err());
    }

    #[test]
    fn vendor_notices_are_required_only_of_compiled_packages() {
        let scratch = crate::test_temp::TempDir::new("td-crypto-vendor-notices").unwrap();
        let vendor = scratch.join("vendor");
        fs::create_dir_all(vendor.join("active-1.0.0")).unwrap();
        fs::create_dir_all(vendor.join("locked-only-1.0.0/src")).unwrap();
        fs::create_dir_all(vendor.join("noticed-1.0.0")).unwrap();
        fs::write(vendor.join("noticed-1.0.0/LICENSE-MIT"), "mit").unwrap();
        let active: std::collections::BTreeSet<String> =
            ["active-1.0.0".to_string()].into_iter().collect();
        assert!(vendor_notices(&vendor, &scratch.join("a"), &active).is_err());
        fs::write(vendor.join("active-1.0.0/LICENSE"), "text").unwrap();
        vendor_notices(&vendor, &scratch.join("b"), &active).unwrap();
        assert!(scratch.join("b/active-1.0.0/LICENSE").is_file());
        assert!(scratch.join("b/noticed-1.0.0/LICENSE-MIT").is_file());
        assert!(!scratch.join("b/locked-only-1.0.0").exists());
        fs::write(vendor.join("stray"), "").unwrap();
        assert!(vendor_notices(&vendor, &scratch.join("c"), &active).is_err());
    }

    #[test]
    fn arm_cargo_targets_musl_but_keeps_the_host_linker() {
        let command = cargo_manifest_architecture(
            "build",
            Path::new("/source/td-mta/Cargo.toml"),
            Architecture::Aarch64,
        );
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("aarch64-unknown-linux-musl")));
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for key in [
            "CC",
            "CC_aarch64-unknown-linux-musl",
            "CC_aarch64_unknown_linux_musl",
            "AR_aarch64_unknown_linux_musl",
            "HOST_CC",
            "CC_x86_64-unknown-linux-gnu",
        ] {
            assert_eq!(
                env.get(std::ffi::OsStr::new(key)),
                Some(&Some(std::ffi::OsStr::new("/td-crypto-decoy"))),
                "{key}"
            );
        }
        assert!(!env.contains_key(std::ffi::OsStr::new("CFLAGS")));
        assert_eq!(
            env.get(std::ffi::OsStr::new(
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"
            )),
            Some(&Some(std::ffi::OsStr::new("/td-crypto-host-linker")))
        );
        let rust_flags = rust_flags_architecture(Architecture::Aarch64);
        assert!(rust_flags.contains("link-self-contained=no"));
        assert!(rust_flags.contains("/self-contained/rcrt1.o"));
        assert!(rust_flags.contains("link-arg=--no-dynamic-linker"));
        assert!(rust_flags_architecture(Architecture::Aarch64).contains("target-cpu=generic"));
        assert!(rust_flags_architecture(Architecture::Aarch64).contains("force-frame-pointers=yes"));
    }

    #[test]
    fn cargo_compiles_no_c_and_decoys_every_native_fallback() {
        let command = cargo("build", "td-mta", Architecture::X86_64);
        let env: BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_string_lossy(), v.to_string_lossy())))
            .collect();
        for key in [
            "CC",
            "AR",
            "CC_x86_64-unknown-linux-musl",
            "AR_x86_64_unknown_linux_musl",
            "HOST_CC",
            "TARGET_CC",
            "CMAKE",
            "PKG_CONFIG",
        ] {
            assert_eq!(env.get(key).unwrap(), "/td-crypto-decoy", "{key}");
        }
        assert!(!env.keys().any(|key| key.starts_with("CFLAGS")));
        // The root's /usr/bin holds a real cmake and pkg-config.
        assert_eq!(env.get("PATH").unwrap(), "/rust/bin");
        assert!(!env.contains_key("LD_LIBRARY_PATH"));
        assert!(env
            .get("CARGO_ENCODED_RUSTFLAGS")
            .unwrap()
            .contains("force-frame-pointers=yes"));
        assert_eq!(env.get("CARGO_PROFILE_RELEASE_STRIP").unwrap(), "none");
        assert!(!env.contains_key("RUSTC_BOOTSTRAP"));
        assert!(!env.contains_key("RUSTC_WRAPPER"));
    }

    #[test]
    fn identical_artifacts_reuse_cache_but_changed_bytes_and_symlinks_fail() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let source = scratch.0.join("artifact");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("binary"), b"same").unwrap();
        let hash = crate::sandbox::nar_hash_of(&source).unwrap();
        let publish =
            || crate::crypto_portable::publish_tree(&source, &scratch.0, "artifact", &hash);
        let destination = publish().unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("binary"), b"same").unwrap();
        assert_eq!(publish().unwrap(), destination);
        fs::write(destination.join("binary"), b"evil").unwrap();
        assert!(publish().is_err());
        fs::remove_dir_all(&destination).unwrap();
        symlink(&source, &destination).unwrap();
        assert!(publish().is_err());
    }

    #[test]
    fn the_compile_namespace_mounts_only_the_build_roots_paths_and_no_td_glibc_role() {
        let scratch = crate::test_temp::TempDir::new("td-crypto-binds").unwrap();
        let root = scratch.join("debian-build-root");
        for directory in ["usr/bin", "usr/lib/x86_64-linux-gnu", "usr/lib64", "etc"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        for name in ["lib", "lib64", "bin"] {
            symlink(format!("usr/{name}"), root.join(name)).unwrap();
        }
        let inputs = crate::crypto_portable::Inputs {
            rust: scratch.join("rust"),
            musl: scratch.join("musl"),
            build_root: crate::crypto_portable::BuildRoot {
                directory: root.clone(),
                record: String::new(),
                _lock: std::fs::File::create(scratch.join("ladder.lock")).unwrap(),
            },
        };
        let input = |name: &str| scratch.join(name);
        let binds = compile_binds(
            &input("source"),
            &input("vendor"),
            &inputs,
            &input("helper"),
            &input("output"),
        )
        .unwrap();
        let mounted: BTreeMap<_, _> = binds
            .iter()
            .map(|b| (b.dest.clone().unwrap(), (b.src.clone(), b.readonly)))
            .collect();
        assert_eq!(mounted.len(), binds.len());
        let from_root: BTreeMap<_, _> = mounted
            .iter()
            .filter(|(_, (src, _))| Path::new(src).starts_with(&root))
            .map(|(dest, (src, readonly))| {
                assert!(readonly, "{dest}");
                (dest.as_str(), src.clone())
            })
            .collect();
        let expected: BTreeMap<_, _> = [
            ("/usr", "usr"),
            ("/lib", "usr/lib"),
            ("/lib64", "usr/lib64"),
        ]
        .into_iter()
        .map(|(dest, inside)| (dest, root.join(inside).to_str().unwrap().to_owned()))
        .collect();
        assert_eq!(from_root, expected);
        let declared: Vec<_> = mounted.keys().map(String::as_str).collect();
        assert_eq!(
            declared,
            [
                "/lib",
                "/lib64",
                "/output",
                "/rust",
                "/source",
                "/td-builder",
                "/td-crypto-decoy",
                "/td-crypto-host-linker",
                "/usr",
                "/vendor",
            ]
        );
        assert!(!mounted.get("/output").unwrap().1);
        // A root whose usrmerge target is a link is refused, not followed.
        fs::remove_dir(root.join("usr/lib64")).unwrap();
        symlink("lib", root.join("usr/lib64")).unwrap();
        let refused = compile_binds(
            &input("source"),
            &input("vendor"),
            &inputs,
            &input("helper"),
            &input("output"),
        );
        assert!(refused.is_err_and(|e| e.contains("usr/lib64")));
    }
}
