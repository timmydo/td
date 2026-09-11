#![forbid(unsafe_code)]

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

mod apply;
mod upstream;
#[path = "../../td-boot/src/protocol.rs"]
#[allow(dead_code, reason = "shared boot deployment contract")]
mod protocol;
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code, reason = "shared streaming SHA-256 implementation")]
mod sha256;

type Result<T> = std::result::Result<T, String>;
const SOURCE: &str = "/run/td-volume/td/source";
// Linux x86_64; std opens File descriptors close-on-exec.
const O_NOFOLLOW: i32 = 0x20000;
const O_NONBLOCK: i32 = 0x800;
const TARGET: &str = "x86_64-unknown-linux-gnu";
const HELP: &str = "usage: td-update [install | build | init]\n\n  install  Build this checkout and request installation (the default).\n  build    Build without installing.\n  init     Clone the bundled release into ~/src/td if absent.\n\nInstallation requires secure-attention confirmation. Restart afterwards to boot it.\n";

fn io<T>(result: std::io::Result<T>, action: &str) -> Result<T> {
    result.map_err(|e| format!("{action}: {e}"))
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("inspect {}: {e}", path.display())),
    }
}

fn private_directory(path: &Path, uid: u32) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", path.display())),
    }
    let metadata = io(fs::symlink_metadata(path), "inspect source state directory")?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("source state must be a private directory owned by this user".into());
    }
    Ok(())
}

fn source_lock(home: &Path) -> Result<File> {
    let uid = io(fs::metadata("/proc/self"), "read current UID")?.uid();
    let state = home.join(".td-update");
    private_directory(&state, uid)?;
    let file = io(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(state.join("source.lock")),
        "open source initialization lock",
    )?;
    let metadata = io(file.metadata(), "inspect source initialization lock")?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err("source initialization lock has an unexpected owner, type or mode".into());
    }
    file.try_lock()
        .map_err(|e| format!("source initialization or a Git worker is still active: {e}"))?;
    Ok(file)
}

fn git(tool: &Path, directory: &Path) -> Command {
    let mut command = Command::new(tool);
    command
        .env_clear()
        .env("PATH", "/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["--no-replace-objects", "-C"])
        .arg(directory)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.templateDir=",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "transfer.fsckObjects=true",
        ]);
    command
}

fn run_git(command: &mut Command, lease: &File, action: &str) -> Result<String> {
    let mut diagnostics = io(lease.try_clone(), "retain Git worker lease")?;
    io(diagnostics.set_len(0), "clear Git diagnostics")?;
    io(diagnostics.rewind(), "rewind Git diagnostics")?;
    // Workers retain this same lock even if initialization is interrupted.
    command
        .stdin(Stdio::from(io(lease.try_clone(), "retain source lease")?))
        .stderr(Stdio::from(diagnostics));
    let mut child = io(command.stdout(Stdio::piped()).spawn(), action)?;
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .ok_or("Git stdout was not captured".to_string())
        .and_then(|stdout| {
            io(
                stdout.take(4097).read_to_end(&mut bytes),
                "read Git metadata",
            )
        });
    if result.is_err() || bytes.len() > 4096 {
        let _ = child.kill();
        let _ = child.wait();
        result?;
        return Err(format!("{action} returned oversized metadata"));
    }
    let status = io(child.wait(), "wait for Git")?;
    if !status.success() {
        return Err(format!(
            "{action} failed ({status}); see ~/.td-update/source.lock"
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{action} returned non-UTF-8 metadata"))
}

fn revision(source: &Path) -> Result<String> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(source.join("revision")),
        "open release revision",
    )?;
    if !io(file.metadata(), "inspect release revision")?.is_file() {
        return Err("release revision must be a regular file".into());
    }
    let mut bytes = Vec::new();
    io(
        file.take(66).read_to_end(&mut bytes),
        "read release revision",
    )?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "release revision is not UTF-8")?;
    let commit = text
        .strip_suffix('\n')
        .ok_or("release revision is not newline terminated")?;
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("release revision must be a lowercase Git commit ID".into());
    }
    Ok(commit.to_string())
}

fn sync_tree(path: &Path, remaining: &mut usize, depth: usize) -> Result<()> {
    if depth > 64 || *remaining == 0 {
        return Err("source checkout exceeds initialization tree bounds".into());
    }
    *remaining -= 1;
    let metadata = io(fs::symlink_metadata(path), "inspect source checkout")?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in io(fs::read_dir(path), "enumerate source checkout")? {
            sync_tree(
                &io(entry, "read source entry")?.path(),
                remaining,
                depth + 1,
            )?;
        }
    } else if !metadata.is_file() {
        return Err("source checkout contains a special file".into());
    }
    io(
        File::open(path).and_then(|f| f.sync_all()),
        "sync source checkout",
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Initialization {
    Unavailable,
    Existing,
    Created,
}

fn initialize(source: &Path, home: &Path, tool: &Path) -> Result<Initialization> {
    let destination = home.join("src/td");
    if present(&destination)? {
        return Ok(Initialization::Existing);
    }
    if !present(source)? {
        return Ok(Initialization::Unavailable);
    }
    let lease = source_lock(home)?;
    if present(&destination)? {
        return Ok(Initialization::Existing);
    }
    let commit = revision(source)?;
    let upstream = upstream::read(source)?;
    let bundle = source.join("repository.bundle");
    if !io(fs::symlink_metadata(&bundle), "inspect release bundle")?.is_file() {
        return Err("release bundle must be a regular file".into());
    }
    let parent = home.join("src");
    match DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && parent.is_dir() => {}
        Err(error) => return Err(format!("create source parent: {error}")),
    }
    let staging = parent.join(".td-update-source.tmp");
    if present(&staging)? {
        if !io(
            fs::symlink_metadata(&staging),
            "inspect interrupted source checkout",
        )?
        .is_dir()
        {
            return Err("reserved source staging path is not a directory".into());
        }
        io(
            fs::remove_dir_all(&staging),
            "remove interrupted source checkout",
        )?;
    }
    let heads = run_git(
        git(tool, home).args(["bundle", "list-heads"]).arg(&bundle),
        &lease,
        "inspect release bundle heads",
    )?;
    if heads != format!("{commit} HEAD\n") {
        return Err("release bundle does not name the recorded commit".into());
    }
    run_git(
        git(tool, home)
            .args(["clone", "--no-checkout", "--no-hardlinks", "--"])
            .arg(&bundle)
            .arg(&staging),
        &lease,
        "clone bundled source",
    )?;
    let branch = upstream.as_ref().map(|value| value.branch.as_str()).unwrap_or("main");
    run_git(
        git(tool, &staging).args(["switch", "--create", branch, &commit]),
        &lease,
        "check out release source",
    )?;
    run_git(
        git(tool, &staging).args(["remote", "remove", "origin"]),
        &lease,
        "remove bootstrap-only Git remote",
    )?;
    if let Some(upstream) = &upstream {
        run_git(
            git(tool, &staging).args(["remote", "add", "origin", &upstream.origin]),
            &lease, "configure source origin",
        )?;
        for (key, value) in [
            (format!("branch.{branch}.remote"), "origin".to_string()),
            (format!("branch.{branch}.merge"), format!("refs/heads/{branch}")),
        ] {
            run_git(git(tool, &staging).args(["config", "--local", &key, &value]),
                &lease, "configure source tracking branch")?;
        }
    }
    let actual = run_git(
        git(tool, &staging).args(["rev-parse", "HEAD"]),
        &lease,
        "verify source commit",
    )?;
    if actual != format!("{commit}\n") {
        return Err("cloned source does not match the release commit".into());
    }
    sync_tree(&staging, &mut 200_000, 0)?;
    if present(&destination)? {
        io(
            fs::remove_dir_all(&staging),
            "remove superseded source staging",
        )?;
        return Ok(Initialization::Existing);
    }
    io(
        fs::rename(&staging, &destination),
        "publish release source checkout",
    )?;
    io(
        File::open(&parent).and_then(|f| f.sync_all()),
        "sync source parent",
    )?;
    Ok(Initialization::Created)
}

fn command(program: &Path, directory: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(directory)
        .env_clear()
        .env("PATH", "/bin")
        .env("HOME", home)
        .env("TMPDIR", "/tmp")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

fn success(command: &mut Command, action: &str) -> Result<()> {
    let status = io(command.status(), action)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{action} failed ({status})"))
    }
}

fn build(root: &Path, home: &Path) -> Result<PathBuf> {
    build_with_tools(
        root,
        home,
        Path::new("/bin/cargo"),
        Path::new("/bin/td-feed"),
    )
}

fn build_with_tools(root: &Path, home: &Path, cargo: &Path, feed: &Path) -> Result<PathBuf> {
    for path in [
        "builder/Cargo.toml",
        "recipes/Cargo.toml",
        "net/Cargo.lock",
        "Cargo.lock",
        "AGENTS.md",
    ] {
        if !root.join(path).is_file() {
            return Err("run ./update build from the td checkout root".into());
        }
    }
    let target = root.join(".td-build-cache/update-tools");
    eprintln!("td-update: preparing build tools from this checkout");
    success(
        command(cargo, root, home)
            .args([
                "build",
                "--offline",
                "--frozen",
                "--release",
                "--workspace",
                "--bin",
                "td-builder",
                "--bin",
                "td-recipe-eval",
                "--target",
                TARGET,
                "--target-dir",
            ])
            .arg(&target)
            .env("RUSTC", "/bin/rustc")
            .env("RUSTDOC", "/bin/rustdoc"),
        "build native update tools",
    )?;
    let bin = target.join(TARGET).join("release");
    // The new fetch tool needs its locked sources before it can warm the graph.
    eprintln!("td-update: fetching build-tool dependencies");
    success(
        command(feed, root, home)
            .args(["warm", "crate-local", "net", "td-net"])
            .env("TD_ROOT", root)
            .env("TD_BUILDER_SELF", bin.join("td-builder")),
        "fetch native td-net dependencies",
    )?;
    eprintln!("td-update: preparing system sources and the fetch tool");
    success(
        command(&bin.join("td-recipe-eval"), root, home)
            .args(["warm", "system-x86-64"])
            .env("TD_BUILDER_SELF", bin.join("td-builder")),
        "warm system",
    )?;
    eprintln!("td-update: building the system image");
    let mut child = io(command(&bin.join("td-recipe-eval"), root, home)
        .args(["build-run", "system-x86-64"])
        .env("TD_BUILDER_SELF", bin.join("td-builder"))
        .stdout(Stdio::piped()).spawn(), "build system")?;
    let result = child.stdout.take().ok_or("missing build output".into())
        .and_then(|stdout| build_output(BufReader::new(stdout), &mut std::io::stdout().lock()));
    if result.is_err() { let _ = child.kill(); }
    let status = io(child.wait(), "wait for system build")?;
    let output = result.map_err(|error| format!("{error} (system build status: {status})"))?;
    if !status.success() { return Err(format!("build system failed ({status})")); }
    Ok(output.join("deployment"))
}

fn build_output(mut input: impl BufRead, output: &mut impl Write) -> Result<PathBuf> {
    const PREFIX: &[u8] = b"TD_RECIPE_RUN_OUT system-x86-64 ";
    let mut deployment = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = io(input.by_ref().take(65537).read_until(b'\n', &mut line), "read build output")?;
        if count == 0 { break; }
        if count > 65536 { return Err("system build emitted an oversized output line".into()); }
        io(output.write_all(&line), "display build output")?;
        if let Some(path) = line.strip_prefix(PREFIX) {
            if deployment.is_some() { return Err("system build returned duplicate output receipts".into()); }
            let text = std::str::from_utf8(path).map_err(|_| "build output path is not UTF-8")?;
            let text = text.strip_suffix('\n').ok_or("incomplete build output receipt")?;
            let path = Path::new(text);
            if !path.is_absolute() || text.contains('\0') || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
                return Err("invalid system build output path".into());
            }
            deployment = Some(path.to_path_buf());
        }
    }
    deployment.ok_or_else(|| "system build returned no output receipt".into())
}

fn install(root: &Path, home: &Path) -> Result<()> {
    let source = build(root, home)?;
    let manifest = io(OpenOptions::new().read(true).custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(source.join("manifest")), "open built manifest")?;
    let metadata = io(manifest.metadata(), "inspect built manifest")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 4096 {
        return Err("built manifest is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    io(manifest.take(4097).read_to_end(&mut bytes), "read built manifest")?;
    if bytes.len() > 4096 { return Err("built manifest grew while reading".into()); }
    let deployment = sha256::hex_digest(&bytes);
    eprintln!("td-update: requesting installation of {deployment}");
    success(command(Path::new("/bin/td-authd"), root, home)
        .arg("request-update").arg(source).arg(deployment), "request system installation")
}

fn run(args: &[String]) -> Result<()> {
    if args.first().map(String::as_str) == Some("apply-operation") {
        let [_, expected] = args else {
            return Err("apply-operation requires exactly one approved deployment ID".into());
        };
        return apply::run(expected);
    }
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) && args.len() == 1 {
        return io(std::io::stdout().write_all(HELP.as_bytes()), "write help");
    }
    if args.len() > 1 || !matches!(args.first().map(String::as_str), None | Some("init" | "build" | "install")) {
        return Err(HELP.into());
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let home = io(home.canonicalize(), "resolve home")?;
    match args.first().map(String::as_str) {
        Some("init") if args.len() == 1 => {
            match initialize(Path::new(SOURCE), &home, Path::new("/bin/git"))? {
                Initialization::Unavailable => println!("td-update: no bundled source on this volume"),
                Initialization::Existing => println!("td-update: preserving ~/src/td"),
                Initialization::Created => println!("td-update: release source ready in ~/src/td; use git remote -v to inspect its update origin"),
            }
            Ok(())
        }
        Some("build") if args.len() == 1 => build(
            &io(std::env::current_dir(), "read current directory")?,
            &home,
        ).map(|_| ()),
        None | Some("install") => install(&io(std::env::current_dir(), "read current directory")?, &home),
        _ => Err(HELP.into()),
    }
}

fn main() -> ExitCode {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-update: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn host_git() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .expect("host Git")
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-update-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        }
        fn export(&self, format: &str) -> PathBuf {
            self.git(&["init", "-b", "release", "--object-format", format]);
            fs::write(self.0.join("tracked"), "release source").unwrap();
            self.git(&["add", "tracked"]);
            self.git(&[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "release",
            ]);
            let source = self.0.join(".git/export");
            fs::create_dir(&source).unwrap();
            self.git(&[
                "bundle",
                "create",
                source.join("repository.bundle").to_str().unwrap(),
                "HEAD",
            ]);
            fs::write(source.join("revision"), self.git(&["rev-parse", "HEAD"])).unwrap();
            source
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore = "requires host rustc"]
    fn build_uses_fresh_helpers_and_stops_at_each_failed_phase() {
        let fixture = Fixture::new();
        for marker in [
            "builder/Cargo.toml",
            "recipes/Cargo.toml",
            "net/Cargo.lock",
            "Cargo.lock",
            "AGENTS.md",
        ] {
            let path = fixture.0.join(marker);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fixture").unwrap();
        }
        let source = fixture.0.join("helper.rs");
        fs::write(&source, r#"
use std::{env, fs, io::Write, path::PathBuf};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let root = env::current_dir().unwrap();
    assert_eq!(PathBuf::from(env::var_os("HOME").unwrap()), root);
    assert_eq!(env::var("PATH").unwrap(), "/bin");
    let bin = root.join(".td-build-cache/update-tools/x86_64-unknown-linux-gnu/release");
    let phase = if env::current_exe().unwrap().file_name().unwrap() == "cargo" {
        assert_eq!(&args[..13], ["build", "--offline", "--frozen", "--release", "--workspace",
            "--bin", "td-builder", "--bin", "td-recipe-eval", "--target",
            "x86_64-unknown-linux-gnu", "--target-dir", root.join(".td-build-cache/update-tools").to_str().unwrap()]);
        assert_eq!(args.len(), 13);
        assert_eq!(env::var("RUSTC").unwrap(), "/bin/rustc");
        fs::create_dir_all(&bin).unwrap();
        for helper in ["td-builder", "td-recipe-eval"] {
            fs::copy(env::current_exe().unwrap(), bin.join(helper)).unwrap();
        }
        "cargo"
    } else if env::current_exe().unwrap().file_name().unwrap() == "td-feed" {
        assert_eq!(args, ["warm", "crate-local", "net", "td-net"]);
        assert_eq!(PathBuf::from(env::var_os("TD_ROOT").unwrap()), root);
        assert_eq!(PathBuf::from(env::var_os("TD_BUILDER_SELF").unwrap()), bin.join("td-builder"));
        "vendor"
    } else {
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], "system-x86-64");
        assert!(matches!(args[0].as_str(), "warm" | "build-run"), "unknown evaluator verb");
        assert_eq!(PathBuf::from(env::var_os("TD_BUILDER_SELF").unwrap()), bin.join("td-builder"));
        assert!(bin.join("td-builder").is_file());
        args[0].as_str()
    };
    let mut log = fs::OpenOptions::new().append(true).create(true).open(root.join("calls")).unwrap();
    writeln!(log, "{phase}").unwrap();
    if fs::read_to_string(root.join("fail-phase")).unwrap_or_default() == phase {
        std::process::exit(7);
    }
    if phase == "build-run" { println!("TD_RECIPE_RUN_OUT system-x86-64 {}", root.join("result").display()); }
}
"#).unwrap();
        let cargo = fixture.0.join("cargo");
        assert!(Command::new("rustc")
            .arg("-Clinker=gcc")
            .arg(&source)
            .arg("-o")
            .arg(&cargo)
            .status()
            .unwrap()
            .success());
        let feed = fixture.0.join("td-feed");
        fs::copy(&cargo, &feed).unwrap();
        for (failure, calls) in [
            ("cargo", "cargo\n"),
            ("vendor", "cargo\nvendor\n"),
            ("warm", "cargo\nvendor\nwarm\n"),
            ("build-run", "cargo\nvendor\nwarm\nbuild-run\n"),
            ("", "cargo\nvendor\nwarm\nbuild-run\n"),
        ] {
            fs::write(fixture.0.join("calls"), "").unwrap();
            fs::write(fixture.0.join("fail-phase"), failure).unwrap();
            let result = build_with_tools(&fixture.0, &fixture.0, &cargo, &feed);
            assert_eq!(result.is_ok(), failure.is_empty(), "{result:?}");
            if failure == "build-run" {
                let error = result.as_ref().unwrap_err();
                assert!(error.contains("no output receipt"), "{error}");
                assert!(error.contains("exit status: 7"), "{error}");
            }
            assert_eq!(fs::read_to_string(fixture.0.join("calls")).unwrap(), calls);
        }
    }

    #[test]
    fn build_receipt_is_bounded_complete_and_required() {
        for bytes in [b"ordinary log\n".as_slice(), b"TD_RECIPE_RUN_OUT system-x86-64 relative\n", b"TD_RECIPE_RUN_OUT system-x86-64 /tmp/../other\n", b"TD_RECIPE_RUN_OUT system-x86-64 /output", b"TD_RECIPE_RUN_OUT system-x86-64 /one\nTD_RECIPE_RUN_OUT system-x86-64 /two\n"] {
            assert!(build_output(std::io::Cursor::new(bytes), &mut Vec::new()).is_err());
        }
        assert!(build_output(std::io::Cursor::new(vec![b'x'; 65537]), &mut Vec::new()).is_err());
        let input = b"working\nTD_RECIPE_RUN_OUT system-x86-64 /output with spaces\n";
        let mut output = Vec::new();
        assert_eq!(build_output(std::io::Cursor::new(input), &mut output).unwrap(), PathBuf::from("/output with spaces"));
        assert_eq!(output, input);
    }

    #[test]
    fn internal_apply_arity_refuses_before_resolving_home_or_opening_state() {
        for args in [
            vec!["apply-operation".into()],
            vec!["apply-operation".into(), "id".into(), "extra".into()],
        ] {
            assert_eq!(
                run(&args),
                Err("apply-operation requires exactly one approved deployment ID".into())
            );
        }
    }

    #[test]
    fn invalid_arguments_report_usage_without_resolving_home() {
        for args in [
            vec!["unknown".to_string()],
            vec!["init".into(), "extra".into()],
        ] {
            assert_eq!(run(&args), Err(HELP.to_string()));
        }
    }

    #[test]
    #[ignore = "requires host Git"]
    fn git_diagnostics_restart_at_zero_after_previous_output() {
        let home = Fixture::new();
        let lease = source_lock(&home.0).unwrap();
        lease
            .try_clone()
            .unwrap()
            .write_all(b"old diagnostics")
            .unwrap();
        assert!(run_git(
            git(&host_git(), &home.0).arg("not-a-git-verb"),
            &lease,
            "rejected Git command"
        )
        .is_err());
        let diagnostics = fs::read(home.0.join(".td-update/source.lock")).unwrap();
        assert!(!diagnostics.is_empty());
        assert!(!diagnostics.contains(&0));
    }

    #[test]
    fn absent_bundle_and_existing_work_are_never_modified() {
        let fixture = Fixture::new();
        let source = fixture.0.join("absent");
        assert_eq!(
            initialize(&source, &fixture.0, Path::new("/absent/git")).unwrap(),
            Initialization::Unavailable
        );
        assert!(!fixture.0.join("src").exists());
        fs::create_dir_all(fixture.0.join("src/td")).unwrap();
        fs::write(fixture.0.join("src/td/private"), "keep this").unwrap();
        assert_eq!(
            initialize(&source, &fixture.0, Path::new("/absent/git")).unwrap(),
            Initialization::Existing
        );
        assert_eq!(
            fs::read_to_string(fixture.0.join("src/td/private")).unwrap(),
            "keep this"
        );
        fs::remove_dir_all(fixture.0.join("src/td")).unwrap();
        symlink("/does-not-exist", fixture.0.join("src/td")).unwrap();
        assert_eq!(
            initialize(&source, &fixture.0, Path::new("/absent/git")).unwrap(),
            Initialization::Existing
        );
        assert_eq!(
            fs::read_link(fixture.0.join("src/td")).unwrap(),
            Path::new("/does-not-exist")
        );
    }

    #[test]
    fn malformed_revision_and_untrusted_state_refuse_initialization() {
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        fs::create_dir(&source).unwrap();
        for invalid in [
            "",
            "abc\n",
            "../evil\n",
            &format!("{}\nextra", "a".repeat(40)),
            &format!("{}\n", "A".repeat(40)),
        ] {
            fs::write(source.join("revision"), invalid).unwrap();
            assert!(revision(&source).is_err());
        }
        fs::remove_file(source.join("revision")).unwrap();
        symlink("/dev/zero", source.join("revision")).unwrap();
        assert!(revision(&source).is_err());
        fs::create_dir(fixture.0.join("other")).unwrap();
        symlink("other", fixture.0.join(".td-update")).unwrap();
        assert!(source_lock(&fixture.0).is_err());
    }

    #[test]
    fn source_lease_remains_exclusive_across_descriptor_copies() {
        let fixture = Fixture::new();
        let lease = source_lock(&fixture.0).unwrap();
        let inherited = lease.try_clone().unwrap();
        drop(lease);
        assert!(source_lock(&fixture.0).is_err());
        drop(inherited);
        // A concurrent test's fork can retain CLOEXEC descriptors until exec.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match source_lock(&fixture.0) {
                Ok(_) => break,
                Err(error) if std::time::Instant::now() >= deadline => panic!("{error}"),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        fs::set_permissions(
            fixture.0.join(".td-update/source.lock"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(source_lock(&fixture.0).is_err());
    }

    #[test]
    #[ignore = "requires host Git"]
    fn bundled_upstream_configures_tracking_without_fetching_and_preserves_user_settings() {
        let repository = Fixture::new();
        let source = repository.export("sha1");
        let settings = upstream::Upstream::new("https://example.invalid/td.git", "releases/stable").unwrap();
        fs::write(source.join(upstream::NAME), settings.encode()).unwrap();
        let home = Fixture::new();
        assert_eq!(initialize(&source, &home.0, &host_git()).unwrap(), Initialization::Created);
        let checkout = Fixture(home.0.join("src/td"));
        assert_eq!(checkout.git(&["branch", "--show-current"]), "releases/stable\n");
        assert_eq!(checkout.git(&["config", "remote.origin.url"]), format!("{}\n", settings.origin));
        assert_eq!(checkout.git(&["config", "branch.releases/stable.remote"]), "origin\n");
        assert_eq!(checkout.git(&["config", "branch.releases/stable.merge"]), "refs/heads/releases/stable\n");
        assert_eq!(checkout.git(&["rev-parse", "HEAD"]), repository.git(&["rev-parse", "HEAD"]));
        assert!(!checkout.0.join(".git/FETCH_HEAD").exists());
        checkout.git(&["remote", "set-url", "origin", "https://example.invalid/my-fork.git"]);
        fs::write(source.join(upstream::NAME), "invalid replacement").unwrap();
        assert_eq!(initialize(&source, &home.0, Path::new("/absent/git")).unwrap(), Initialization::Existing);
        assert_eq!(checkout.git(&["config", "remote.origin.url"]), "https://example.invalid/my-fork.git\n");
        let other = Fixture::new();
        assert!(initialize(&source, &other.0, Path::new("/absent/git")).is_err());
        assert!(!other.0.join("src/td").exists());
        fs::remove_file(source.join(upstream::NAME)).unwrap();
        symlink("repository.bundle", source.join(upstream::NAME)).unwrap();
        assert!(upstream::read(&source).is_err());
        fs::remove_file(source.join(upstream::NAME)).unwrap();
        fs::write(source.join(upstream::NAME), vec![b'x'; 4097]).unwrap();
        assert!(upstream::read(&source).is_err());
    }

    #[test]
    #[ignore = "requires host Git"]
    fn offline_checkout_is_complete_and_existing_edits_survive_retry() {
        for format in ["sha1", "sha256"] {
            let repository = Fixture::new();
            let source = repository.export(format);
            let home = Fixture::new();
            let original_parent_mode = if format == "sha1" {
                fs::create_dir_all(home.0.join("src/.td-update-source.tmp")).unwrap();
                fs::write(
                    home.0.join("src/.td-update-source.tmp/interrupted"),
                    "old attempt",
                )
                .unwrap();
                Some(fs::metadata(home.0.join("src")).unwrap().mode() & 0o777)
            } else {
                None
            };
            assert_eq!(
                initialize(&source, &home.0, &host_git()).unwrap(),
                Initialization::Created
            );
            assert_eq!(
                fs::metadata(home.0.join("src")).unwrap().mode() & 0o777,
                original_parent_mode.unwrap_or(0o700)
            );
            let checkout = Fixture(home.0.join("src/td"));
            assert_eq!(
                checkout.git(&["rev-parse", "HEAD"]),
                repository.git(&["rev-parse", "HEAD"])
            );
            assert_eq!(checkout.git(&["branch", "--show-current"]), "main\n");
            assert_eq!(checkout.git(&["remote"]), "");
            fs::write(checkout.0.join("tracked"), "my edits").unwrap();
            fs::write(checkout.0.join("untracked"), "my notes").unwrap();
            assert_eq!(
                initialize(&source, &home.0, Path::new("/absent/git")).unwrap(),
                Initialization::Existing
            );
            assert_eq!(
                fs::read_to_string(checkout.0.join("tracked")).unwrap(),
                "my edits"
            );
            assert_eq!(
                fs::read_to_string(checkout.0.join("untracked")).unwrap(),
                "my notes"
            );
        }
    }

    #[test]
    #[ignore = "requires host Git"]
    fn mismatched_or_damaged_bundle_never_publishes_a_checkout() {
        let repository = Fixture::new();
        let source = repository.export("sha1");
        let home = Fixture::new();
        let correct = fs::read(source.join("revision")).unwrap();
        fs::write(source.join("revision"), format!("{}\n", "0".repeat(40))).unwrap();
        assert!(initialize(&source, &home.0, &host_git()).is_err());
        assert!(!home.0.join("src/td").exists());
        fs::write(source.join("revision"), correct).unwrap();
        fs::write(source.join("repository.bundle"), "corrupt").unwrap();
        assert!(initialize(&source, &home.0, &host_git()).is_err());
        assert!(!home.0.join("src/td").exists());
    }
}
