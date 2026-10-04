//! Fixed-path commands run only with the declared portable input mounts.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
const TARGET: &str = "x86_64-unknown-linux-musl";
const LIBRARIES: &str = "/lib64:/gcc-runtime:/zlib:/rust/lib";
const NATIVE_FLAGS: &str = "-nostdinc -isystem /musl/include -isystem /cc/lib/gcc/x86_64-pc-linux-gnu/14.3.0/include -B/binutils/bin/ -march=x86-64 -mtune=generic -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer -g1 -ffile-prefix-map=/source=/td-build -ffile-prefix-map=/vendor=/td-cargo/vendor -ffile-prefix-map=/output=/td-build-root";

pub(crate) fn rust_flags() -> String {
    [
        "-C",
        "linker=/rust/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
        "-C",
        "linker-flavor=ld.lld",
        "-C",
        "target-feature=+crt-static",
        "-C",
        "target-cpu=x86-64",
        "-C",
        "force-frame-pointers=yes",
        "-C",
        "debuginfo=1",
        "--remap-path-prefix=/source=/td-build",
        "--remap-path-prefix=/vendor=/td-cargo/vendor",
        "--remap-path-prefix=/output=/td-build-root",
    ]
    .join("\x1f")
}

pub(crate) fn cargo(verb: &str, package: &str) -> Command {
    cargo_manifest(verb, Path::new(&format!("/source/{package}/Cargo.toml")))
}

pub(crate) fn cargo_manifest(verb: &str, manifest: &Path) -> Command {
    let mut command = Command::new("/rust/bin/cargo");
    command
        .current_dir("/tmp")
        .env_clear()
        .args([verb, "--frozen", "--target", TARGET, "--manifest-path"])
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
        .env("PATH", "/rust/bin:/cc/bin:/binutils/bin")
        .env("LD_LIBRARY_PATH", LIBRARIES)
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
        .env("CARGO_ENCODED_RUSTFLAGS", rust_flags())
        .env("CMAKE", "/td-crypto-decoy")
        .env("PKG_CONFIG", "/td-crypto-decoy")
        .env("OPENSSL_DIR", "/undeclared-openssl")
        .stdin(Stdio::null());
    for suffix in [
        "",
        "_x86_64-unknown-linux-musl",
        "_x86_64_unknown_linux_musl",
    ] {
        command
            .env(format!("CC{suffix}"), "/cc/bin/gcc")
            .env(format!("AR{suffix}"), "/binutils/bin/ar")
            .env(format!("CFLAGS{suffix}"), NATIVE_FLAGS);
    }
    for (key, value) in crate::crypto_build::CONTROLS {
        command.env(key, value);
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

pub(crate) fn host_linker(args: &[String]) -> Result<()> {
    require_namespace(&["/cc/bin/gcc", "/binutils/bin/ld"])?;
    let mut command = Command::new("/cc/bin/gcc");
    command
        .args([
            "-B/binutils/bin/",
            "-B/lib64/",
            "-L/lib64/",
            "-Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2",
            "-Wl,-rpath,/lib64",
        ])
        .args(args);
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
    IntegrationTest,
}
impl ArtifactKind {
    fn target(self) -> &'static str {
        match self {
            Self::Installed => "bin",
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
    let actual = confined_path(
        &artifact_path(message, package, kind)?,
        Path::new("/output/target"),
    )?;
    qualify_binary(&actual)?;
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

fn qualify_binary(path: &Path) -> Result<()> {
    crate::elf::assert_x86_64_executable_bounded(path, 256 * 1024 * 1024)?;
    crate::elf::assert_static_pie(path)?;
    let _ = crate::elf::debug_line_requires_debug_str(path)?;
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
        "/musl/include/stdint.h",
        "/cc/bin/gcc",
        "/binutils/bin/ar",
        "/lib64",
        "/gcc-runtime",
        "/zlib",
    ])?;
    fs::create_dir_all("/tmp/home").map_err(|e| format!("portable Cargo home: {e}"))?;
    let mut receipt = String::from("cargo-environment cleared\n");
    for package in ["td-crypto", "td-mta"] {
        let mut command = cargo("tree", package);
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
    let mut command = cargo("build", "td-mta");
    command.args([
        "--release",
        "--bin",
        "td-mta",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "mail-build", &mut receipt)?;
    let output = bounded_output(&mut command, "mail-build", 8 * 1024 * 1024, 1200)?;
    let mail = executable(&output, "td-mta", ArtifactKind::Installed)?;
    let mut command = cargo("test", "td-crypto");
    command.args([
        "--release",
        "--lib",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "crypto-build", &mut receipt)?;
    let output = bounded_output(&mut command, "crypto-build", 8 * 1024 * 1024, 1200)?;
    let crypto = executable(&output, "td_crypto", ArtifactKind::LibraryTest)?;
    let mut command = cargo("test", "td-mta");
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
    let mut command = cargo("test", "td-mta");
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
    let mut command = cargo("test", "td-mta");
    command.args([
        "--release",
        "--lib",
        "--no-run",
        "--message-format=json-render-diagnostics",
    ]);
    command_record(&command, "mail-transport-build", &mut receipt)?;
    let output = bounded_output(&mut command, "mail-transport-build", 8 * 1024 * 1024, 1200)?;
    let transport = executable(&output, "td_mta", ArtifactKind::LibraryTest)?;
    let mut command = cargo("test", "td-mta");
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
    let mut command = cargo("rustc", "td-mta");
    command.args([
        "--release",
        "--test",
        "native_alloc_probe",
        "--message-format=json-render-diagnostics",
        "--",
        "--cfg",
        "td_native_alloc_probe",
        "-D",
        "warnings",
    ]);
    for symbol in NATIVE_WRAPPERS {
        command.args(["-C", &format!("link-arg=--wrap={symbol}")]);
    }
    command_record(&command, "native-allocation-build", &mut receipt)?;
    let output = bounded_output(
        &mut command,
        "native-allocation-build",
        8 * 1024 * 1024,
        1200,
    )?;
    let native = executable(&output, "native_alloc_probe", ArtifactKind::IntegrationTest)?;
    let mut command = cargo("test", "td-mta");
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
    for (index, (binary, probe, native_probe)) in [
        (&mail, false, false),
        (&crypto, false, false),
        (&config, false, false),
        (&format, false, false),
        (&transport, false, false),
        (&allocation, true, false),
        (&native, false, true),
        (&rss, false, false),
    ]
    .into_iter()
    .enumerate()
    {
        let mut command = Command::new("/binutils/bin/nm");
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
        native_allocation_symbols(&symbols, native_probe)?;
        rss_symbols(&symbols, binary == &rss)?;
    }
    refuse_decoy(Path::new("/output"))?;
    fs::create_dir("/output/artifacts").map_err(|e| format!("portable artifacts: {e}"))?;
    copy_binary(&mail, Path::new("/output/artifacts/td-mta"))?;
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
    copy_binary(
        &native,
        Path::new("/output/artifacts/td-mta-native-allocation-probe"),
    )?;
    copy_binary(&rss, Path::new("/output/artifacts/td-mta-rss-probe"))?;
    crate::crypto_api::qualify(&mut receipt)?;
    refuse_decoy(Path::new("/output"))?;
    write_new(Path::new("/output/artifacts/COMMANDS"), receipt.as_bytes())?;
    Ok(())
}

const NATIVE_WRAPPERS: &[&str] = &[
    "malloc",
    "calloc",
    "realloc",
    "free",
    "posix_memalign",
    "aligned_alloc",
];
const NATIVE_SUCCESS: &str = "native-allocation-probe-v1: forwarding provider diagnostic passed\n";

fn tls_allocation_evidence(output: &str, native: bool) -> Result<()> {
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
    tls_phase_evidence(output, native, "", "client", &phases)
}

fn tls_handshake_evidence(output: &str, native: bool) -> Result<()> {
    tls_phase_evidence(
        output,
        native,
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
        "rust" | "native" => tls_phase_evidence(
            body,
            domain == "native",
            &format!("-{scenario}"),
            scenario,
            REMOTE_CHAIN_PHASES,
        )?,
        "rss" => rss_evidence(body, scenario)?,
        _ => return Err("unknown remote chain observation domain".into()),
    }
    Ok(body)
}

fn tls_generation_evidence(output: &str, native: bool, scenario: &str) -> Result<()> {
    if !matches!(
        scenario,
        "generation" | "generation-routing" | "generation-trust"
    ) {
        return Err("unknown generation observation scenario".into());
    }
    tls_phase_evidence(
        output,
        native,
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

fn tls_large_chain_evidence(output: &str, native: bool) -> Result<()> {
    tls_phase_evidence(
        output,
        native,
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

fn tls_fragment_evidence(output: &str, native: bool) -> Result<()> {
    tls_phase_evidence(
        output,
        native,
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

fn tls_certificate_list_evidence(output: &str, native: bool) -> Result<()> {
    tls_phase_evidence(
        output,
        native,
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

fn entropy_worker_evidence(output: &str, native: bool) -> Result<()> {
    tls_phase_evidence(
        output,
        native,
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

fn tls_phase_evidence(
    output: &str,
    native: bool,
    suffix: &str,
    scenario: &str,
    phases: &[&str],
) -> Result<()> {
    let domain = if native { "native" } else { "rust" };
    let width = if native { 9 } else { 7 };
    let mut lines = output.lines();
    for phase in phases {
        let prefix = format!("tls-{domain}{suffix} {phase} ");
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
        if columns != width {
            return Err("wrong TLS allocation measurement width".into());
        }
    }
    let version = if matches!(scenario, "handshake" | "large-chain") {
        2
    } else {
        1
    };
    let success = format!("tls-{scenario}-allocation-v{version}: {domain} passed");
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

fn native_allocation_evidence(output: &str, zero_resize: bool) -> Result<()> {
    if zero_resize {
        if output != "native-allocation-probe-v1: zero-resize invalidated\n" {
            return Err("portable native zero-resize control did not complete exactly".into());
        }
        return Ok(());
    }
    if output.lines().count() != 4 || !output.ends_with(NATIVE_SUCCESS) {
        return Err("portable native allocation probe did not complete exactly".into());
    }
    for (prefix, ceiling) in [
        ("native_registry_storage_bytes=", 2 * 1024 * 1024),
        ("native_counter_storage_bytes=", 4096),
        ("native_thread_flag_bytes=", 64),
    ] {
        stack_evidence(output, prefix, ceiling)?;
    }
    Ok(())
}

fn native_allocation_symbols(symbols: &str, probe: bool) -> Result<()> {
    let names: std::collections::BTreeSet<_> = symbols
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .collect();
    let has_registry = symbols.contains("TD_MTA_NATIVE_REGISTRY");
    let has_probe = symbols.contains("native_alloc_probe");
    if symbols.trim().is_empty() || has_registry != probe || has_probe != probe {
        return Err("portable native allocation probe sentinel boundary failed".into());
    }
    for name in NATIVE_WRAPPERS {
        if names.contains(format!("__wrap_{name}").as_str()) != probe {
            return Err("portable native allocation wrapper symbol boundary failed".into());
        }
    }
    if names.iter().any(|name| {
        name.starts_with("__wrap_")
            && (!probe
                || !NATIVE_WRAPPERS
                    .iter()
                    .any(|allowed| *name == format!("__wrap_{allowed}")))
    }) {
        return Err("portable executable contains an unadmitted allocation wrapper".into());
    }
    if probe
        && names.iter().any(|name| {
            *name == "sdallocx"
                || name.starts_with("OPENSSL_memory_")
                || matches!(*name, "memalign" | "valloc" | "pvalloc" | "reallocarray")
        })
    {
        return Err("portable native allocation probe resolves an alternate allocator hook".into());
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

fn collect_artifacts(output: &Path, destination: &Path) -> Result<String> {
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
    let expected = [
        "COMMANDS",
        "td-mta",
        "td-crypto-smoke",
        "td-mta-config-smoke",
        "td-mta-format-smoke",
        "td-mta-transport-smoke",
        "td-mta-rust-allocation-probe",
        "td-mta-native-allocation-probe",
        "td-mta-rss-probe",
    ]
    .map(std::ffi::OsString::from)
    .into_iter()
    .collect();
    if names != expected {
        return Err("unexpected build artifact inventory".into());
    }
    let receipt = read_output(&source.join("COMMANDS"), "commands", 64 * 1024)?;
    fs::create_dir(destination).map_err(|e| format!("create private artifact directory: {e}"))?;
    for name in [
        "td-mta",
        "td-crypto-smoke",
        "td-mta-config-smoke",
        "td-mta-format-smoke",
        "td-mta-transport-smoke",
        "td-mta-rust-allocation-probe",
        "td-mta-native-allocation-probe",
        "td-mta-rss-probe",
    ] {
        copy_binary(&source.join(name), &destination.join(name))?;
    }
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
        ("td-secret", "src", &["fido_p256.rs"][..]),
        ("td-secret", "tests", &["p256_vectors.txt"][..]),
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

pub(crate) fn runtime_inner() -> Result<()> {
    require_namespace(&[
        "/artifacts/td-mta",
        "/artifacts/td-crypto-smoke",
        "/artifacts/td-mta-config-smoke",
        "/artifacts/td-mta-format-smoke",
        "/artifacts/td-mta-transport-smoke",
        "/artifacts/td-mta-rust-allocation-probe",
        "/artifacts/td-mta-native-allocation-probe",
        "/artifacts/td-mta-rss-probe",
        "/output",
    ])?;
    let mut command = Command::new("/artifacts/td-mta");
    command.arg("--version").env_clear().stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    let version = bounded_output(&mut command, "version", 4096, 30)?;
    if version != "td-mta 0.1.0\n" {
        return Err("portable binary returned an unexpected version".into());
    }
    for (index, (binary, case, ignored)) in [
        ("td-crypto-smoke", "tests::admitted_native_backend_sha256_smoke", false),
        ("td-crypto-smoke", "sha256::tests::known_answers_and_fragmented_updates", false),
        ("td-crypto-smoke", "sha256::tests::differential_padding_blocks_and_every_split", false),
        ("td-crypto-smoke", "sha256::tests::refusal_retires_and_clears_state", false),
        ("td-crypto-smoke", "sha256::tests::inline_storage_and_fixed_debug", false),
        ("td-crypto-smoke", "entropy::tests::local_randomness_smoke", false),
        ("td-crypto-smoke", "entropy::tests::construction_requires_nonempty_successful_initialization", false),
        ("td-crypto-smoke", "entropy::tests::synthetic_partial_error_clears_the_entire_caller_slice", false),
        ("td-crypto-smoke", "pem::tests::canonical_base64_and_transactional_output", false),
        ("td-crypto-smoke", "pem::tests::certificate_decoding_matches_independent_pem_reader", false),
        ("td-crypto-smoke", "pem::tests::certificate_envelopes_and_retry", false),
        ("td-crypto-smoke", "pem::tests::certificate_limits_and_der_envelopes", false),
        ("td-crypto-smoke", "pem::tests::p256_pem_loading_and_refusals", false),
        ("td-crypto-smoke", "der::tests::certificate_der_lengths_bit_strings_and_oids", false),
        ("td-crypto-smoke", "der::tests::certificate_calendar_boundaries", false),
        ("td-crypto-smoke", "certificate_algorithms::tests::certificate_algorithm_inventory_fails_closed", false),
        ("td-crypto-smoke", "identity::tests::local_identity_owns_material_and_checks_validity", false),
        ("td-crypto-smoke", "identity::tests::narrowed_identity_shares_material_and_cannot_expand_or_revive_bindings", false),
        ("td-crypto-smoke", "identity::tests::identity_refuses_wrong_key_name_time_and_order", false),
        ("td-crypto-smoke", "identity::tests::local_identity_enforces_key_usage_and_extensions", false),
        ("td-crypto-smoke", "identity::tests::local_identity_name_and_extension_limits", false),
        ("td-crypto-smoke", "identity::tests::local_identity_chain_constraints_and_maximum_depth", false),
        ("td-crypto-smoke", "identity::tests::local_identity_accepts_rsa_issuer_and_refuses_unsupported_algorithms", false),
        ("td-crypto-smoke", "identity::tests::identity_admission_unwind_drops_unpublished_state", false),
        ("td-crypto-smoke", "identity::tests::local_identity_metadata_bounds_and_malformed_values", false),
        ("td-crypto-smoke", "trust::tests::private_trust_replaces_public_and_other_private_roots", false),
        ("td-crypto-smoke", "trust::tests::private_trust_bundle_limits_duplicates_and_atomic_refusal", false),
        ("td-crypto-smoke", "trust::tests::private_trust_anchor_policy_and_ignored_self_signature", false),
        ("td-crypto-smoke", "trust::tests::public_trust_inventory_and_source_are_fixed", false),
        ("td-crypto-smoke", "trust::tests::trust_construction_unwind_drops_unpublished_state", false),
        ("td-crypto-smoke", "provider::tests::factory_digest_and_fixed_comparison", false),
        ("td-crypto-smoke", "provider::tests::accepted_pkcs8_variants_and_public_point_known_answer", false),
        ("td-crypto-smoke", "provider::tests::malformed_der_and_inconsistent_keys_are_refused", false),
        ("td-crypto-smoke", "provider::tests::generated_keys_and_signatures_pass_independent_verification", false),
        ("td-crypto-smoke", "provider::tests::capacity_failure_and_retired_keys_preserve_caller_output", false),
        ("td-crypto-smoke", "provider::tests::shared_key_serializes_success_and_terminal_failure", false),
        ("td-crypto-smoke", "sha256_oracle::tests::empty_input", false),
        ("td-crypto-smoke", "sha256_oracle::tests::abc", false),
        ("td-crypto-smoke", "sha256_oracle::tests::two_block_message", false),
        ("td-crypto-smoke", "sha256_oracle::tests::million_a", false),
        ("td-crypto-smoke", "p256_oracle::tests::independent_modular_arithmetic_covers_carries_and_borrows", false),
        ("td-crypto-smoke", "p256_oracle::tests::nist_ecdh_and_openssl_scalar_boundaries_match", false),
        ("td-crypto-smoke", "p256_oracle::tests::nist_signature_acceptance_and_refusal_and_rare_x_reduction", false),
        ("td-crypto-smoke", "p256_oracle::tests::nist_public_key_validation_preserves_overwide_rejections", false),
        ("td-crypto-smoke", "p256_oracle::tests::scalar_and_coordinate_admission_never_reduce_untrusted_values", false),
        ("td-crypto-smoke", "p256_oracle::tests::exceptional_point_cases_and_projective_representations", false),
        ("td-crypto-smoke", "p256_oracle::tests::signature_scalar_ranges_and_infinity_result_are_refused", false),
        ("td-crypto-smoke", "tests::explicit_aws_provider_and_roots_construct_without_global_default", false),
        ("td-crypto-smoke", "provider::tests::signature_transform_failure_retires_before_key_restore", false),
        ("td-crypto-smoke", "tls_signer::tests::canonical_tls_signature_encoding_boundaries", false),
        ("td-crypto-smoke", "tls_signer::tests::tls_signer_uses_owned_key_and_hashes_once", false),
        ("td-crypto-smoke", "tls_signer::tests::shared_tls_signers_observe_transform_retirement", false),
        ("td-crypto-smoke", "identity::tests::retained_tls_identity_completes_handshakes_and_survives_remote_refusal", false),
        ("td-crypto-smoke", "identity::tests::retained_tls_identity_shares_key_lifecycle", false),
        ("td-crypto-smoke", "tls_clock::tests::injected_clock_missing_time_and_shared_retirement", false),
        ("td-crypto-smoke", "tls_clock::tests::injected_clock_serializes_and_drops_panicking_source", false),
        ("td-crypto-smoke", "tls_client::tests::client_configuration_pins_policy_and_redacts_state", false),
        ("td-crypto-smoke", "tls_client::tests::client_configuration_verifies_trust_name_alpn_and_full_handshakes", false),
        ("td-crypto-smoke", "tls_client::tests::client_configuration_clock_errors_reach_handshake_and_open_tickets", false),
        ("td-crypto-smoke", "tls_client::tests::client_verifier_checks_chain_bounds_before_path_work", false),
        ("td-crypto-smoke", "tls_client::tests::tls12_session_save_can_ignore_transient_clock_failure", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_bounds_bindings_roles_and_private_trust", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_routes_names_and_refuses_resumption", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_pins_disabled_backend_features", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_requires_private_client_certificates", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_checks_cold_material_and_supplied_verifier_time", false),
        ("td-crypto-smoke", "tls_server::tests::backend_acceptor_discards_ip_literal_sni_before_routing", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_selects_distinct_owned_identities", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_omits_oversized_ca_hint_lists", false),
        ("td-crypto-smoke", "tls_server::tests::server_configuration_refuses_offered_resumption", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_fragmented_handshake_and_simultaneous_writes", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_retires_handshake_output_reserve_and_refuses_backend_growth", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_retains_ignored_tls12_clock_failure", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_checks_time_without_native_time_requests", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_close_halves_and_transport_eof", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_tls12_alert_refuses_pending_ciphertext_and_socket_tail", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_never_emits_after_close_or_reuses_failed_state", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_unwind_consumes_state_and_shared_clock_failure_fences_reuse", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_unread_plaintext_blocks_without_consuming_next_record", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_name_verification_maps_and_retires_without_output", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_key_update_response_advances_without_application_write", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_handshake_reassembly_counts_retained_record_headers", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_errors_are_fixed_and_do_not_export_backend_diagnostics", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_ticket_clock_failure_after_finished_is_terminal", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_ticket_flight_capacity_is_terminal_after_finished", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_unfinished_cancellation_eof_and_alert_refuse_evidence", false),
        ("td-crypto-smoke", "session_clock::tests::observers_isolate_transient_errors_and_share_retirement", false),
        ("td-crypto-smoke", "tls_record::tests::record_bounds_track_protection_not_finished", false),
        ("td-crypto-smoke", "tls_record::tests::record_requires_one_complete_exact_frame", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_tls12_hello_request_never_adds_post_close_output", false),
        ("td-crypto-smoke", "tls_session::tests::client_session_tls12_simultaneous_close_pins_pending_output_refusal", false),
        ("td-crypto-smoke", "tls_hello::tests::raw_hello_fragmentation_names_and_retry", false),
        ("td-crypto-smoke", "tls_hello::tests::raw_hello_refuses_malformed_sni_before_completion", false),
        ("td-crypto-smoke", "tls_hello::tests::raw_hello_fixed_state_skips_large_extensions_and_bounds_messages", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_public_round_trip_and_evidence", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_refuses_raw_ip_sni_across_record_fragments", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_mandatory_client_proof_and_typed_refusals", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_valid_retry_and_fragmented_raw_retry_sni_refusal", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_public_pair_tiny_pipes_simultaneous_writes_and_close", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_selected_validity_and_shared_key_retirement", false),
        ("td-crypto-smoke", "tls_session::tests::server::server_session_clock_failures_and_acceptor_unwind_retire_owned_state", false),
        ("td-crypto-smoke", "tls_policy::tests::exact_tls_algorithm_inventory", false),
        ("td-crypto-smoke", "tls_policy::tests::handshake_mapping_drift_is_refused", false),
        ("td-crypto-smoke", "tls_policy::tests::excluded_certificate_signature_has_a_valid_baseline", false),
        ("td-crypto-smoke", "tls_smoke::explicit_policy_negotiates_classical_groups_and_suites", false),
        ("td-crypto-smoke", "tls_smoke::explicit_policy_refuses_excluded_peer_algorithms", false),
        ("td-crypto-smoke", "tls_smoke::tls12_local_round_trip", false),
        ("td-crypto-smoke", "tls_smoke::tls13_local_round_trip", false),
        ("td-crypto-smoke", "tls_smoke::rejects_wrong_server_name", false),
        ("td-crypto-smoke", "tls_smoke::rejects_untrusted_chain", false),
        ("td-crypto-smoke", "tls_smoke::rejects_expired_certificate", false),
        ("td-crypto-smoke", "tls_smoke::rejects_malformed_record", false),
        ("td-crypto-smoke", "tls_smoke::rejects_bad_certificate_signature", false),
        ("td-crypto-smoke", "tls_smoke::rejects_tampered_ciphertext", false),
        ("td-crypto-smoke", "tls_smoke::tls12_mutual_authentication", false),
        ("td-crypto-smoke", "tls_smoke::tls13_mutual_authentication", false),
        ("td-crypto-smoke", "tls_smoke::mutual_authentication_requires_certificate", false),
        ("td-crypto-smoke", "tls_smoke::mutual_authentication_refuses_unknown_issuer", false),
        ("td-crypto-smoke", "tls_smoke::mutual_authentication_refuses_expired_certificate", false),
        ("td-crypto-smoke", "tls_smoke::mutual_authentication_refuses_server_only_usage", false),
        ("td-crypto-smoke", "tls_smoke::mutual_authentication_refuses_bad_signature", false),
        ("td-crypto-smoke", "tls_smoke::mutual_authentication_refuses_mismatched_key_before_connect", false),
        ("td-mta-format-smoke", "provider_hashes_container_and_binding_fixtures", false),
        ("td-mta-format-smoke", "provider_hashes_import_snapshot_fixtures", false),
        ("td-mta-config-smoke", "portable_loader_stack", true),
        ("td-mta-config-smoke", "portable_materialized_stack", true),
        ("td-mta-transport-smoke", "generations::tests::current_retired_and_candidate_share_two_slots_before_construction", false),
        ("td-mta-transport-smoke", "generations::tests::failed_stale_and_foreign_candidates_preserve_active_state_and_owners", false),
        ("td-mta-transport-smoke", "generations::tests::final_worker_release_destroys_payload_before_returning_capacity", false),
        ("td-mta-transport-smoke", "generations::tests::racing_preparation_and_release_never_construct_a_third_live_payload", false),
        ("td-mta-transport-smoke", "tls_policy::tests::compiled_policy_roles_bind_names_trust_and_generation_ids", false),
        ("td-mta-transport-smoke", "tls_policy::tests::queued_and_native_reservations_retain_capacity_and_recover_exact_buffers", false),
        ("td-mta-transport-smoke", "tls_policy::tests::gateway_comparison_survives_reordering_but_rejects_policy_or_binding_changes", false),
        ("td-mta-transport-smoke", "tls_policy::tests::material_errors_never_publish_and_acme_requests_are_explicit", false),
        ("td-mta-transport-smoke", "tls_policy::tests::material_reader_caps_bytes_eof_and_interrupted_work", false),
        ("td-mta-transport-smoke", "tls_policy::tests::compiled_configs_enforce_relay_trust_protocol_and_gateway_client_auth", false),
        ("td-mta-transport-smoke", "tls_policy::tests::https_profiles_narrow_shared_smtp_names_before_native_routing", false),
        ("td-mta-transport-smoke", "tls_policy::tests::io_tests::handoff_refuses_plaintext_tails_and_deadlines_and_returns_original_buffers", false),
        ("td-mta-transport-smoke", "tls_policy::tests::io_tests::authorized_connections_release_handshake_capacity_and_retain_generation_until_teardown", false),
        ("td-mta-transport-smoke", "tls_policy::tests::io_tests::gateway_without_client_certificate_never_exposes_mail_proof_and_recovers_permits", false),
        ("td-mta-transport-smoke", "tls_policy::tests::io_tests::handshake_deadline_can_only_tighten_and_failure_is_sticky", false),
        ("td-mta-transport-smoke", "tls_policy::tests::io_tests::changed_gateway_policy_aborts_pending_connection_and_releases_capacity", false),
        ("td-mta-transport-smoke", "tls_policy::tests::io_tests::established_clock_failure_clears_cached_authorization_and_aborts_socket", false),
        ("td-mta-transport-smoke", "tls_policy::tests::client_tests::client_only_acme_bootstrap_never_opens_server_material", false),
        ("td-mta-transport-smoke", "tls_policy::tests::client_tests::expired_server_does_not_block_clients_but_invalid_client_trust_still_refuses", false),
        ("td-mta-transport-smoke", "tls_policy::tests::client_tests::client_generation_transitions_to_complete_within_the_same_two_slots", false),
        ("td-mta-transport-smoke", "tls_policy::starttls::tests::starttls_reply_requires_short_writes_and_complete_flush", false),
        ("td-mta-transport-smoke", "tls_policy::starttls::tests::starttls_reply_refuses_invalid_write_counts_and_flush_failure", false),
        ("td-mta-transport-smoke", "tls_policy::tests::starttls_tests::server_starttls_flushes_220_before_handoff_and_verifies_real_tls", false),
        ("td-mta-transport-smoke", "tls_policy::tests::starttls_tests::server_starttls_refuses_unframed_parameterized_tailed_and_wrong_role_commands", false),
        ("td-mta-transport-smoke", "tls_policy::tests::starttls_tests::server_starttls_deadlines_clock_refusal_and_cancel_release_reservations", false),
        ("td-mta-transport-smoke", "tls_policy::tests::gateway_process_tests::gateway_starttls_process_requires_verified_pin_after_plaintext_reply", false),
        ("td-mta-transport-smoke", "tls_policy::tests::stack_tests::portable_tls_policy_transport_stack", true),
        ("td-mta-transport-smoke", "smtp_wire::tests::ehlo_offer_requires_a_complete_advertisement_and_no_tail", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::ehlo_offer_handles_fragmentation_and_skips_malformed_extensions", false),
        ("td-mta-transport-smoke", "tls_policy::tests::client_starttls_tests::client_starttls_and_server_upgrade_verify_real_tls", false),
        ("td-mta-transport-smoke", "tls_policy::tests::client_starttls_tests::client_starttls_refuses_bad_replies_and_accepts_fragmented_220", false),
        ("td-mta-transport-smoke", "tls_policy::tests::client_starttls_tests::client_starttls_roles_scratch_deadlines_and_cancel_return_buffers", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::line_split_at_every_boundary_retains_exact_tail", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::strict_line_framing_is_terminal_after_failure", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::control_line_counts_crlf_and_clears_only_its_reservation", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::reply_syntax_accepts_bare_codes_and_refuses_ambiguous_separators", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::multiline_replies_require_one_code_and_leave_the_next_reply_unread", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::fragmented_220_reply_stops_before_plaintext_or_record_tail", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::reply_aggregate_cap_includes_all_lines_and_the_final_crlf", false),
        ("td-mta-transport-smoke", "smtp_wire::tests::ehlo_greeting_is_never_a_starttls_advertisement", false),
        ("td-mta-transport-smoke", "tls_policy::tests::gateway_process_tests::gateway_mutual_tls_accepts_current_and_next_verified_leaf_pins", false),
        ("td-mta-transport-smoke", "tls_policy::tests::gateway_process_tests::gateway_mutual_tls_refuses_verified_leaf_with_wrong_pin_or_actual_peer", false),
        ("td-mta-transport-smoke", "gateway_policy::tests::canonical_gateway_policy_ignores_only_representation_and_server_material", false),
        ("td-mta-transport-smoke", "gateway_policy::tests::gateway_filters_and_material_limits_refuse_without_fallback", false),
        ("td-mta-transport-smoke", "gateway_policy::tests::gateway_tls_requires_a_client_certificate_on_the_same_valid_server", false),
        ("td-mta-transport-smoke", "tls_admission::tests::capacities_saturate_and_reuse_without_losing_live_reservations", false),
        ("td-mta-transport-smoke", "tls_admission::tests::concurrent_reservations_and_worker_returns_preserve_the_global_cap", false),
        ("td-mta-transport-smoke", "tls_admission::tests::overlapping_release_and_reservation_never_over_admit_or_leak_capacity", false),
        ("td-mta-transport-smoke", "tls_admission::tests::refusal_and_pool_drop_preserve_linear_cleanup", false),
        ("td-mta-transport-smoke", "clock::tests::runtime_clock_keeps_utc_and_monotonic_domains_separate", false),
        ("td-mta-transport-smoke", "clock::tests::tls_clock_conversion_never_falls_back_after_refusal", false),
        ("td-mta-transport-smoke", "transport::tests::tcp_bounded_io_actual_peer_and_unbuffered_flush", false),
        ("td-mta-transport-smoke", "transport::tests::tcp_peer_eof_preserves_write_half_and_local_close_preserves_read_half", false),
        ("td-mta-transport-smoke", "transport::tests::tcp_abort_drop_and_errors_fence_both_directions", false),
        ("td-mta-transport-smoke", "tls_io::tests::final_handshake_write_cannot_publish_past_its_deadline", false),
        ("td-mta-transport-smoke", "tls_io::tests::flush_backpressure_withholds_finished_and_bad_transport_counts_abort", false),
        ("td-mta-transport-smoke", "tls_io::tests::public_tls_round_trip_with_tiny_pipes_and_independent_close", false),
        ("td-mta-transport-smoke", "tls_io::tests::simultaneous_full_chunks_progress_without_growing_wire_storage", false),
        ("td-mta-transport-smoke", "tls_io::tests::deadlines_and_missing_tls_time_discard_evidence_and_never_revive", false),
        ("td-mta-transport-smoke", "tls_io::tests::malformed_records_truncation_and_constructor_refusal_close_the_transport", false),
        ("td-mta-transport-smoke", "tls_io::tests::public_crypto_sessions_exchange_mail_bytes_over_local_tcp", false),
        ("td-mta-transport-smoke", "tls_io::tests::established_sessions_outlive_handshake_deadline_and_preserve_backpressure", false),
        ("td-mta-transport-smoke", "tls_io::tests::established_tcp_eof_without_tls_close_is_terminal_even_at_record_boundary", false),
        ("td-mta-transport-smoke", "tls_io::tests::returned_buffers_are_reused_and_close_paths_keep_exclusive_ownership", false),
        ("td-mta-transport-smoke", "tls_io::tests::constructor_refusals_return_moved_pool_buffers_without_logging_them", false),
    ]
    .iter()
    .enumerate()
    {
        let mut command = Command::new(format!("/artifacts/{binary}"));
        command
            .args(["--exact", *case, "--test-threads=1"])
            .env_clear()
            .stdin(Stdio::null());
        if *binary == "td-mta-transport-smoke" {
            command.env("TD_MTA_TEST_TLS_PEER", "/artifacts/td-crypto-smoke");
        }
        if *ignored {
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
        if *ignored {
            let (prefix, ceiling) = match *case {
                "portable_loader_stack" => ("config_stack_mapping_bytes=", 176 * 1024),
                "portable_materialized_stack" => ("config_materialized_stack_mapping_bytes=", 256 * 1024),
                "tls_policy::tests::stack_tests::portable_tls_policy_transport_stack" => ("tls_policy_transport_stack_mapping_bytes=", 256 * 1024),
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
    let mut command = Command::new("/artifacts/td-mta-native-allocation-probe");
    command.env_clear().stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    let output = bounded_output(&mut command, "native-allocation-probe", 4096, 30)?;
    native_allocation_evidence(&output, false)?;
    for line in output.lines().take(3) {
        println!("portable native diagnostic: {line}");
    }
    let mut command = Command::new("/artifacts/td-mta-native-allocation-probe");
    command
        .arg("--zero-resize")
        .env_clear()
        .stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    let output = bounded_output(&mut command, "native-allocation-zero-resize", 4096, 30)?;
    native_allocation_evidence(&output, true)?;
    println!("portable runtime: native allocator forwarding/provider diagnostics passed");
    for (native, path) in [
        (false, "/artifacts/td-mta-rust-allocation-probe"),
        (true, "/artifacts/td-mta-native-allocation-probe"),
    ] {
        let mut command = Command::new(path);
        command
            .arg("--tls-clients")
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let domain = if native { "native" } else { "rust" };
        let name = format!("tls-client-allocation-{domain}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        tls_allocation_evidence(&output, native)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
        let mut command = Command::new(path);
        command
            .arg("--tls-handshake")
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("tls-handshake-allocation-{domain}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        tls_handshake_evidence(&output, native)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
        let mut command = Command::new(path);
        command
            .arg("--entropy-workers")
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("tls-entropy-allocation-{domain}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        entropy_worker_evidence(&output, native)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
        let mut command = Command::new(path);
        command
            .arg("--tls-fragments")
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("tls-fragment-allocation-{domain}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        tls_fragment_evidence(&output, native)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
        let mut command = Command::new(path);
        command
            .arg("--tls-certificate-list")
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("tls-certificate-list-allocation-{domain}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        tls_certificate_list_evidence(&output, native)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
        let mut command = Command::new(path);
        command
            .arg("--tls-large-chain")
            .env_clear()
            .stdin(Stdio::null());
        crate::host_bin::arm_check_child(&mut command);
        let name = format!("tls-large-chain-allocation-{domain}");
        let output = bounded_output(&mut command, &name, 8192, 30)?;
        tls_large_chain_evidence(&output, native)?;
        for line in output.lines() {
            println!("portable allocation diagnostic: {line}");
        }
        for (scenario, argument) in [
            ("generation", "--tls-generations"),
            ("generation-routing", "--tls-generation-routing"),
            ("generation-trust", "--tls-generation-trust"),
        ] {
            let mut command = Command::new(path);
            command.arg(argument).env_clear().stdin(Stdio::null());
            crate::host_bin::arm_check_child(&mut command);
            let name = format!("tls-{scenario}-allocation-{domain}");
            let output = bounded_output(&mut command, &name, 8192, 30)?;
            tls_generation_evidence(&output, native, scenario)?;
            for line in output.lines() {
                println!("portable allocation diagnostic: {line}");
            }
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
            ("native", "td-mta-native-allocation-probe"),
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

    println!("portable runtime: version, SHA-256 facade/failure and mail-format probes, PEM/identity/trust, entropy and P-256/oracle probes, explicit algorithm policy, owned TLS signing, inbound/outbound configuration/clock and eighteen backend TLS cases and both bounded configuration stacks passed without toolchain mounts");
    println!("portable runtime: sixty-five mail SMTP/policy/generation/gateway/admission/clock/TCP/TLS cases passed without toolchain mounts");
    Ok(())
}

pub(crate) fn build(root: &Path, archives: &Path) -> Result<std::path::PathBuf> {
    if std::env::consts::ARCH != "x86_64" || std::env::consts::OS != "linux" {
        return Err("portable build host qualification currently covers x86-64 Linux only".into());
    }
    crate::crypto_build::validate(root)?;
    let inputs = crate::crypto_portable::inputs(root, archives)?;
    let vendor = crate::host_bin::prepare_crypto_vendor(root)?;
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
    if kit_digest != "sha256:dce3040c995d6572fd47df15fded5d5e3ea6c87455b5daef82f4dea2503942b1" {
        return Err("prepared Rust kit differs from the qualified portable kit".into());
    }
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
    let output = scratch.0.join("output");
    fs::create_dir(&output).map_err(|e| format!("create portable output: {e}"))?;
    let mut binds = vec![
        bind(&source, "/source", true)?,
        bind(&vendor, "/vendor", true)?,
        bind(&inputs.rust, "/rust", true)?,
        bind(&inputs.headers, "/musl", true)?,
        bind(&helper, "/td-builder", true)?,
        bind(&helper, "/td-crypto-host-linker", true)?,
        bind(&helper, "/td-crypto-decoy", true)?,
        bind(&output, "/output", false)?,
    ];
    let header_digest = crate::sandbox::nar_hash_of(&inputs.headers)
        .map_err(|e| format!("hash portable headers: {e}"))?;
    let helper_digest =
        crate::sandbox::nar_hash_of(&helper).map_err(|e| format!("hash portable helper: {e}"))?;
    let mut receipt = format!(
        "portable-build 1\nsource {source_digest}\nvendor {vendor_digest}\nrust-kit {kit_digest}\nheaders {header_digest}\nhelper {helper_digest}\ntarget {TARGET}\n"
    );
    for input in inputs.native {
        binds.push(bind(&input.directory, input.inside, true)?);
        receipt.push_str(&input.record);
    }
    enter(
        "/td-builder",
        &["gate-crates".into(), "crypto-portable-build-inner".into()],
        &binds,
        &scratch.0.join("compile"),
        false,
    )?;
    let artifacts = scratch.0.join("artifacts");
    receipt.push_str(&collect_artifacts(&output, &artifacts)?);
    for binary in [
        "td-mta",
        "td-crypto-smoke",
        "td-mta-config-smoke",
        "td-mta-format-smoke",
        "td-mta-transport-smoke",
        "td-mta-rust-allocation-probe",
        "td-mta-native-allocation-probe",
        "td-mta-rss-probe",
    ] {
        qualify_binary(&artifacts.join(binary))?;
    }
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
    for package in fs::read_dir(&vendor).map_err(|e| format!("read vendor notices: {e}"))? {
        let package = package.map_err(|e| format!("vendor notice entry: {e}"))?;
        if !package
            .file_type()
            .map_err(|e| format!("vendor notice type: {e}"))?
            .is_dir()
            || retain_notices(
                &package.path(),
                &artifacts.join("notices/vendor").join(package.file_name()),
                0,
            )? == 0
        {
            return Err(format!(
                "vendor package lacks notices: {}",
                package.path().display()
            ));
        }
    }
    retain_notices(&inputs.rust, &artifacts.join("notices/rust"), 0)?;
    retain_notices(&inputs.headers, &artifacts.join("notices/musl"), 0)?;
    write_new(&artifacts.join("BUILD-INPUTS"), receipt.as_bytes())?;
    let digest = crate::sandbox::nar_hash_of(&artifacts)
        .map_err(|e| format!("hash portable artifact: {e}"))?;
    crate::crypto_portable::publish_tree(
        &artifacts,
        &root.join(".td-build-cache"),
        "artifact",
        &digest,
    )
}

fn retain_notices(source: &Path, destination: &Path, depth: usize) -> Result<usize> {
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
        for native in [false, true] {
            let domain = if native { "native" } else { "rust" };
            let values = if native {
                "1 2 3 4 5 6 7 8 9"
            } else {
                "1 2 3 4 5 6 7"
            };
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
            assert!(tls_fragment_evidence(&output, native).is_ok());
            assert!(tls_fragment_evidence(&output, !native).is_err());
            assert!(tls_handshake_evidence(&output, native).is_err());
            assert!(entropy_worker_evidence(&output, native).is_err());
            for bad in [
                output.replace("large_pending", "large_refused"),
                output.replace(" 1 2", " 1"),
                output.replace(" 1 2", " x 2"),
                output.replace("passed", "failed"),
                format!("{output}extra\n"),
            ] {
                assert!(tls_fragment_evidence(&bad, native).is_err());
            }
        }
    }

    #[test]
    fn certificate_list_records_require_pending_refusal_and_repetition() {
        for native in [false, true] {
            let domain = if native { "native" } else { "rust" };
            let values = if native {
                "1 2 3 4 5 6 7 8 9"
            } else {
                "1 2 3 4 5 6 7"
            };
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
            assert!(tls_certificate_list_evidence(&output, native).is_ok());
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

            assert!(tls_certificate_list_evidence(&output, !native).is_err());
            assert!(tls_handshake_evidence(&output, native).is_err());
            assert!(tls_fragment_evidence(&output, native).is_err());
            assert!(tls_certificate_list_evidence(
                &output.replace("certificate-list", "fragment"),
                native
            )
            .is_err());
            assert!(entropy_worker_evidence(&output, native).is_err());
            for bad in [
                output.replace("large_pending", "large_refused"),
                output.replace(" 1 2", " 1"),
                output.replace(" 1 2", " x 2"),
                output.replace("passed", "failed"),
                output.replace("-v1:", "-v2:"),
                output.trim_end().to_owned(),
                format!("{output}extra\n"),
            ] {
                assert!(tls_certificate_list_evidence(&bad, native).is_err());
            }
        }
    }

    #[test]
    fn entropy_worker_records_require_teardown_and_distinct_phases() {
        for native in [false, true] {
            let domain = if native { "native" } else { "rust" };
            let values = if native {
                "1 2 3 4 5 6 7 8 9"
            } else {
                "1 2 3 4 5 6 7"
            };
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
            assert!(entropy_worker_evidence(&output, native).is_ok());
            assert!(entropy_worker_evidence(&output, !native).is_err());
            assert!(tls_handshake_evidence(&output, native).is_err());
            assert!(tls_allocation_evidence(&output, native).is_err());
            for bad in [
                output.replace("joined", "repeated"),
                output.replace(" 1 2", " 1"),
                output.replace(" 1 2", " -1 2"),
                output.replace("passed", "failed"),
                format!("{output}extra\n"),
            ] {
                assert!(entropy_worker_evidence(&bad, native).is_err());
            }
        }
    }

    #[test]
    fn tls_handshake_records_are_distinct_from_client_construction() {
        for native in [false, true] {
            let domain = if native { "native" } else { "rust" };
            let values = if native {
                "1 2 3 4 5 6 7 8 9"
            } else {
                "1 2 3 4 5 6 7"
            };
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
            assert!(tls_handshake_evidence(&output, native).is_ok());
            assert!(tls_handshake_evidence(&output.replace("-v2:", "-v1:"), native).is_err());
            assert!(tls_handshake_evidence(&output, !native).is_err());
            assert!(tls_allocation_evidence(&output, native).is_err());
            for bad in [
                output.replace("material", "config"),
                output.replace(" 1 2", " 1"),
                output.replace(" 1 2", " x 2"),
                output.replace("passed", "failed"),
                format!("{output}extra\n"),
            ] {
                assert!(tls_handshake_evidence(&bad, native).is_err());
            }
        }
    }

    #[test]
    fn remote_chain_records_require_the_controller_and_inner_schema() {
        for scenario in ["remote12", "remote13", "remote13large"] {
            for domain in ["rust", "native", "rss"] {
                let mut body = String::new();
                for phase in REMOTE_CHAIN_PHASES {
                    if domain == "rss" {
                        body.push_str(&format!("rss {scenario} {phase} 100\n"));
                    } else {
                        let values = if domain == "native" {
                            "1 2 3 4 5 6 7 8 9"
                        } else {
                            "1 2 3 4 5 6 7"
                        };
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
            for native in [false, true] {
                let domain = if native { "native" } else { "rust" };
                let values = if native {
                    "1 2 3 4 5 6 7 8 9"
                } else {
                    "1 2 3 4 5 6 7"
                };
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
                assert!(tls_generation_evidence(&output, native, scenario).is_ok());
                let unknown = output.replace(scenario, "unknown");
                assert!(tls_generation_evidence(&unknown, native, "unknown").is_err());
                assert!(rss_evidence(&rss.replace(scenario, "unknown"), "unknown").is_err());
                assert!(rss_evidence(&rss, scenario).is_ok());
                assert!(tls_generation_evidence(&output, !native, scenario).is_err());
                for other in [
                    "generation",
                    "generation-routing",
                    "generation-trust",
                    "unknown",
                ] {
                    if other != scenario {
                        assert!(tls_generation_evidence(&output, native, other).is_err());
                        assert!(rss_evidence(&rss, other).is_err());
                    }
                }
                assert!(tls_large_chain_evidence(&output, native).is_err());
                for bad in [
                    output.replace("candidate", "overlap"),
                    output.replace("current_released", "old_released"),
                    output.replace("-v1:", "-v2:"),
                    output.replace(&format!("tls-{domain}-{scenario} refused {values}\n"), ""),
                    format!("{output}extra\n"),
                    output.trim_end().to_owned(),
                ] {
                    assert!(tls_generation_evidence(&bad, native, scenario).is_err());
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
    }

    #[test]
    fn large_chain_records_are_distinct_from_ordinary_handshake() {
        for native in [false, true] {
            let domain = if native { "native" } else { "rust" };
            let values = if native {
                "1 2 3 4 5 6 7 8 9"
            } else {
                "1 2 3 4 5 6 7"
            };
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
            assert!(tls_large_chain_evidence(&output, native).is_ok());
            assert!(tls_large_chain_evidence(&output.replace("-v2:", "-v1:"), native).is_err());
            assert!(tls_large_chain_evidence(&output, !native).is_err());
            assert!(tls_handshake_evidence(&output, native).is_err());
            for bad in [
                output.replace("material", "config"),
                output.replace(" 1 2", " 1"),
                output.replace(" 1 2", " x 2"),
                output.replace("passed", "failed"),
                format!("{output}extra\n"),
            ] {
                assert!(tls_large_chain_evidence(&bad, native).is_err());
            }
        }
    }

    #[test]
    fn tls_allocation_records_require_all_ordered_phases_and_exact_width() {
        for native in [false, true] {
            let domain = if native { "native" } else { "rust" };
            let values = if native {
                "1 2 3 4 5 6 7 8 9"
            } else {
                "1 2 3 4 5 6 7"
            };
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
            assert!(tls_allocation_evidence(&output, native).is_ok());
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
                assert!(tls_allocation_evidence(&bad, native).is_err());
            }
            assert!(tls_allocation_evidence(&output, !native).is_err());
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
    fn native_allocation_probe_requires_exact_wrappers_and_rejects_hooks() {
        let mut symbols = String::from("000 b native_alloc_probe_TD_MTA_NATIVE_REGISTRY\n");
        for name in NATIVE_WRAPPERS {
            symbols.push_str(&format!("000 T __wrap_{name}\n"));
        }
        assert!(native_allocation_symbols(&symbols, true).is_ok());
        assert!(native_allocation_symbols(&symbols, false).is_err());
        assert!(native_allocation_symbols("000 t ordinary_main", false).is_ok());
        assert!(native_allocation_symbols("", false).is_err());
        for name in NATIVE_WRAPPERS {
            assert!(native_allocation_symbols(
                &symbols.replace(&format!("000 T __wrap_{name}\n"), ""),
                true
            )
            .is_err());
        }
        for name in [
            "sdallocx",
            "OPENSSL_memory_alloc",
            "OPENSSL_memory_free",
            "__wrap_memalign",
            "memalign",
            "valloc",
            "pvalloc",
            "reallocarray",
        ] {
            assert!(native_allocation_symbols(&format!("{symbols}000 T {name}\n"), true).is_err());
        }
        assert!(native_allocation_symbols("000 T __wrap_memalign", false).is_err());
        assert!(!rust_flags().contains("--wrap"));
        assert!(!NATIVE_FLAGS.contains("--wrap"));
        let measured = format!("native_registry_storage_bytes=1048608\nnative_counter_storage_bytes=56\nnative_thread_flag_bytes=1\n{NATIVE_SUCCESS}");
        assert!(native_allocation_evidence(&measured, false).is_ok());
        assert!(native_allocation_evidence(&measured.replace("=56", "=0"), false).is_err());
        assert!(native_allocation_evidence(&measured.replace("=56", "=4097"), false).is_err());
        assert!(native_allocation_evidence(
            &measured.replace(
                "native_thread_flag_bytes=1",
                "native_counter_storage_bytes=1"
            ),
            false
        )
        .is_err());
        assert!(native_allocation_evidence(NATIVE_SUCCESS, false).is_err());
        assert!(native_allocation_evidence(NATIVE_SUCCESS, true).is_err());
        let zero = "native-allocation-probe-v1: zero-resize invalidated\n";
        assert!(native_allocation_evidence(zero, true).is_ok());
        assert!(native_allocation_evidence(zero, false).is_err());
        for bad in [
            "",
            NATIVE_SUCCESS.trim_end(),
            "test result: ok. 0 passed; 0 failed;\n",
            "native allocation probe unqualified: use the isolated musl build\n",
        ] {
            assert!(native_allocation_evidence(bad, false).is_err());
            assert!(native_allocation_evidence(bad, true).is_err());
        }
        assert!(
            native_allocation_evidence(&format!("{NATIVE_SUCCESS}{NATIVE_SUCCESS}"), false)
                .is_err()
        );
    }

    #[test]
    fn host_collection_refuses_missing_outputs_links_extras_and_oversized_receipts() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let output = scratch.0.join("output");
        let source = output.join("artifacts");
        fs::create_dir_all(&source).unwrap();
        for name in [
            "td-mta",
            "td-crypto-smoke",
            "td-mta-config-smoke",
            "td-mta-format-smoke",
            "td-mta-transport-smoke",
            "td-mta-rust-allocation-probe",
            "td-mta-native-allocation-probe",
            "td-mta-rss-probe",
            "COMMANDS",
        ] {
            fs::write(source.join(name), name).unwrap();
        }
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
            "td-secret/src/fido_p256.rs",
            "td-secret/tests/p256_vectors.txt",
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
            "td-secret/src/fido_p256.rs",
            "td-secret/tests/p256_vectors.txt",
        ] {
            assert_eq!(
                fs::read(destination.join(relative)).unwrap(),
                relative.as_bytes()
            );
        }
        assert!(!destination.join("engine/src/not-an-oracle.rs").exists());
        fs::remove_file(root.join("td-secret/tests/p256_vectors.txt")).unwrap();
        assert!(stage_sources(&root, &scratch.0.join("missing-oracle"))
            .unwrap_err()
            .starts_with("inspect oracle source:"));
        symlink(
            "../../engine/src/sha256.rs",
            root.join("td-secret/tests/p256_vectors.txt"),
        )
        .unwrap();
        assert_eq!(
            stage_sources(&root, &scratch.0.join("linked-oracle")).unwrap_err(),
            "oracle sources must be regular files, not links"
        );
        fs::remove_file(root.join("td-secret/tests/p256_vectors.txt")).unwrap();
        fs::write(root.join("td-secret/tests/p256_vectors.txt"), "restored").unwrap();
        for (index, relative) in [
            "engine",
            "engine/src",
            "td-secret",
            "td-secret/src",
            "td-secret/tests",
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
    fn cargo_uses_fixed_tools_flags_and_native_fallback_decoys() {
        let command = cargo("build", "td-mta");
        let env: BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_string_lossy(), v.to_string_lossy())))
            .collect();
        assert_eq!(env.get("CC").unwrap(), "/cc/bin/gcc");
        assert_eq!(env.get("AR").unwrap(), "/binutils/bin/ar");
        assert_eq!(env.get("CMAKE").unwrap(), "/td-crypto-decoy");
        assert_eq!(env.get("PKG_CONFIG").unwrap(), "/td-crypto-decoy");
        assert!(env.get("CFLAGS").unwrap().contains("-nostdinc"));
        assert!(env
            .get("CARGO_ENCODED_RUSTFLAGS")
            .unwrap()
            .contains("force-frame-pointers=yes"));
        for &(key, value) in crate::crypto_build::CONTROLS {
            assert_eq!(env.get(key).unwrap(), value);
        }
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
}
