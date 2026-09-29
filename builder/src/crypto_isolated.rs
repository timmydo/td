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

pub(crate) fn bounded_output(command: &mut Command, name: &str, limit: u64, seconds: u64) -> Result<String> {
    bounded_exit_output(command, name, limit, seconds, 0)
}

pub(crate) fn bounded_exit_output(command: &mut Command, name: &str, limit: u64, seconds: u64, expected: i32) -> Result<String> {
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
    let status = child.try_wait().map_err(|e| format!("inspect {name} status: {e}"))?;
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
enum ArtifactKind {
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

fn artifact_path(message: &str, package: &str, expected: ArtifactKind) -> Result<PathBuf> {
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
    refuse_decoy(Path::new("/output"))?;
    fs::create_dir("/output/artifacts").map_err(|e| format!("portable artifacts: {e}"))?;
    copy_binary(&mail, Path::new("/output/artifacts/td-mta"))?;
    copy_binary(&crypto, Path::new("/output/artifacts/td-crypto-smoke"))?;
    copy_binary(&config, Path::new("/output/artifacts/td-mta-config-smoke"))?;
    copy_binary(&format, Path::new("/output/artifacts/td-mta-format-smoke"))?;
    crate::crypto_api::qualify(&mut receipt)?;
    refuse_decoy(Path::new("/output"))?;
    write_new(Path::new("/output/artifacts/COMMANDS"), receipt.as_bytes())?;
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
    let expected = ["COMMANDS", "td-mta", "td-crypto-smoke", "td-mta-config-smoke", "td-mta-format-smoke"]
        .map(std::ffi::OsString::from)
        .into_iter()
        .collect();
    if names != expected {
        return Err("unexpected build artifact inventory".into());
    }
    let receipt = read_output(&source.join("COMMANDS"), "commands", 64 * 1024)?;
    fs::create_dir(destination).map_err(|e| format!("create private artifact directory: {e}"))?;
    for name in ["td-mta", "td-crypto-smoke", "td-mta-config-smoke", "td-mta-format-smoke"] {
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
    for package in ["td-crypto", "td-mta"] {
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
    let value = values.next().ok_or("portable stack measurement is missing")?;
    if values.next().is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("portable stack measurement is ambiguous or malformed".into());
    }
    let bytes = value.parse::<usize>().map_err(|_| "portable stack measurement overflows")?;
    if bytes == 0 || bytes > ceiling {
        return Err("portable stack measurement exceeds its ceiling".into());
    }
    Ok(bytes)
}

pub(crate) fn runtime_inner() -> Result<()> {
    require_namespace(&["/artifacts/td-mta", "/artifacts/td-crypto-smoke", "/artifacts/td-mta-config-smoke", "/artifacts/td-mta-format-smoke", "/output"])?;
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
    ]
    .iter()
    .enumerate()
    {
        let mut command = Command::new(format!("/artifacts/{binary}"));
        command
            .args(["--exact", *case, "--test-threads=1"])
            .env_clear()
            .stdin(Stdio::null());
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
                _ => return Err("unknown portable stack qualification case".into()),
            };
            let bytes = stack_evidence(&output, prefix, ceiling)?;
            println!("portable runtime: {prefix}{bytes}");
        }
    }
    println!("portable runtime: version, SHA-256 facade/failure and mail-format probes, entropy and P-256/oracle probes, explicit provider, sixteen TLS cases and both bounded configuration stacks passed without toolchain mounts");
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
    for binary in ["td-mta", "td-crypto-smoke", "td-mta-config-smoke", "td-mta-format-smoke"] {
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
    fn stack_measurement_requires_one_bounded_decimal_observation() {
        for (prefix, ceiling, size) in [
            ("config_stack_mapping_bytes=", 176 * 1024, 167936),
            ("config_materialized_stack_mapping_bytes=", 256 * 1024, 249856),
        ] {
            assert_eq!(stack_evidence(&format!("noise\n{prefix}{size}\n"), prefix, ceiling).unwrap(), size);
            assert_eq!(stack_evidence(&format!("{prefix}{ceiling}"), prefix, ceiling).unwrap(), ceiling);
            let other = if prefix == "config_stack_mapping_bytes=" {
                "config_materialized_stack_mapping_bytes="
            } else { "config_stack_mapping_bytes=" };
            assert!(stack_evidence(&format!("{other}1000"), prefix, ceiling).is_err());
            for bad in [String::new(), prefix.to_owned(), format!("{prefix}0"),
                format!("{prefix}{}", ceiling + 1), format!("{prefix}+1"),
                format!("{prefix}9999999999999999999999999999"),
                format!("{prefix}1\n{prefix}1"), "unrelated_mapping=1".into()] {
                assert!(stack_evidence(&bad, prefix, ceiling).is_err(), "{bad}");
            }
        }
    }

    #[test]
    fn host_collection_refuses_missing_outputs_links_extras_and_oversized_receipts() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let output = scratch.0.join("output");
        let source = output.join("artifacts");
        fs::create_dir_all(&source).unwrap();
        for name in ["td-mta", "td-crypto-smoke", "td-mta-config-smoke", "td-mta-format-smoke", "COMMANDS"] {
            fs::write(source.join(name), name).unwrap();
        }
        fs::remove_file(source.join("td-mta-format-smoke")).unwrap();
        assert!(collect_artifacts(&output, &scratch.0.join("missing-format")).is_err());
        fs::write(source.join("td-mta-format-smoke"), b"td-mta-format-smoke").unwrap();
        let good = scratch.0.join("good");
        assert_eq!(collect_artifacts(&output, &good).unwrap(), "COMMANDS");
        assert_eq!(fs::read(good.join("td-mta")).unwrap(), b"td-mta");
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
        assert!(artifact_path(&format!("{record}\n{record}"), "td-mta", ArtifactKind::Installed).is_err());
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
        let integration = test.replace("td_crypto", "config_stack").replace("\"lib\"", "\"test\"");
        assert!(artifact_path(&integration, "config_stack", ArtifactKind::IntegrationTest).is_ok());
        assert!(artifact_path(&test, "td_crypto", ArtifactKind::IntegrationTest).is_err());
        assert!(artifact_path(&integration, "config_stack", ArtifactKind::LibraryTest).is_err());
    }

    #[test]
    fn source_staging_excludes_configuration_and_refuses_links_and_missing_inputs() {
        let scratch = Scratch::create(&std::env::temp_dir(), "td-crypto-test").unwrap();
        let root = scratch.0.join("checkout");
        for package in ["td-mta", "td-crypto"] {
            let path = root.join(package);
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(path.join("src/lib.rs"), "source").unwrap();
            fs::write(path.join("Cargo.toml"), "manifest").unwrap();
            fs::write(path.join("Cargo.lock"), "lock").unwrap();
            fs::write(path.join("build.rs"), "do not copy").unwrap();
            fs::create_dir(path.join(".cargo")).unwrap();
            fs::write(path.join(".cargo/config.toml"), "do not copy").unwrap();
        }
        for relative in ["engine/src/sha256.rs", "td-secret/src/fido_p256.rs", "td-secret/tests/p256_vectors.txt"] {
            let input = root.join(relative);
            fs::create_dir_all(input.parent().unwrap()).unwrap();
            fs::write(input, relative).unwrap();
        }
        fs::write(root.join("engine/src/not-an-oracle.rs"), "excluded").unwrap();
        let destination = scratch.0.join("staged");
        stage_sources(&root, &destination).unwrap();
        for relative in ["engine/src/sha256.rs", "td-secret/src/fido_p256.rs", "td-secret/tests/p256_vectors.txt"] {
            assert_eq!(fs::read(destination.join(relative)).unwrap(), relative.as_bytes());
        }
        assert!(!destination.join("engine/src/not-an-oracle.rs").exists());
        fs::remove_file(root.join("td-secret/tests/p256_vectors.txt")).unwrap();
        assert!(stage_sources(&root, &scratch.0.join("missing-oracle"))
            .unwrap_err().starts_with("inspect oracle source:"));
        symlink("../../engine/src/sha256.rs", root.join("td-secret/tests/p256_vectors.txt")).unwrap();
        assert_eq!(stage_sources(&root, &scratch.0.join("linked-oracle")).unwrap_err(),
            "oracle sources must be regular files, not links");
        fs::remove_file(root.join("td-secret/tests/p256_vectors.txt")).unwrap();
        fs::write(root.join("td-secret/tests/p256_vectors.txt"), "restored").unwrap();
        for (index, relative) in ["engine", "engine/src", "td-secret", "td-secret/src", "td-secret/tests"].iter().enumerate() {
            let source = root.join(relative);
            let retained = scratch.0.join("retained-oracle-directory");
            fs::rename(&source, &retained).unwrap();
            symlink(&retained, &source).unwrap();
            assert_eq!(stage_sources(&root, &scratch.0.join(format!("linked-oracle-parent-{index}"))).unwrap_err(),
                "oracle source ancestors must be directories, not links");
            fs::remove_file(&source).unwrap();
            fs::rename(&retained, &source).unwrap();
        }
        assert!(!destination.join("td-mta/.cargo").exists());
        assert!(!destination.join("td-mta/build.rs").exists());
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
        for (key, value) in crate::crypto_build::CONTROLS {
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
