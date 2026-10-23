//! Prepare td's on-demand compiler before entering an offline review jail.
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn prepare(scratch: &Path) -> Result<Option<PathBuf>, String> {
    match fs::metadata("/etc/td-rust.tsv") {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    let output = Command::new("/bin/td-rust")
        .arg("path")
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("prepare account Rust cache: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "prepare account Rust cache exited {}",
            output.status
        ));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "invalid Rust cache path")?
        .trim_end();
    if text.len() > 4096 || !Path::new(text).is_absolute() || text.contains(['\n', '\r', '\0']) {
        return Err("invalid Rust cache path".into());
    }
    let source = fs::canonicalize(text).map_err(|error| error.to_string())?;
    let destination = scratch.join("rust-toolchain");
    copy_toolchain(&source, &destination)?;
    Ok(Some(destination.join("bin")))
}

fn copy_toolchain(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination).map_err(|error| error.to_string())?;
    let result = copy_directory(source, destination, source, 0, &mut 0, &mut 0);
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn copy_directory(
    source: &Path,
    destination: &Path,
    root: &Path,
    depth: usize,
    entries: &mut u64,
    bytes: &mut u64,
) -> Result<(), String> {
    if depth > 64 {
        return Err("Rust tree exceeds directory depth bound".into());
    }
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        *entries += 1;
        if *entries > 20_000 {
            return Err("Rust tree exceeds entry bound".into());
        }
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            fs::create_dir(&target).map_err(|error| error.to_string())?;
            copy_directory(&path, &target, root, depth + 1, entries, bytes)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(&path).map_err(|error| error.to_string())?;
            let allowed = if link.is_absolute() {
                crate::jail::SYSTEM_TREES
                    .iter()
                    .any(|tree| link.starts_with(tree))
                    && !link
                        .components()
                        .any(|part| matches!(part, Component::ParentDir))
            } else {
                fs::canonicalize(&path).is_ok_and(|resolved| resolved.starts_with(root))
            };
            if !allowed {
                return Err(format!(
                    "Rust cache link leaves its runtime: {}",
                    path.display()
                ));
            }
            symlink(link, target).map_err(|error| error.to_string())?;
        } else if kind.is_file() {
            let (file, metadata) = td_fs::open_real_file(&path, "review Rust input")
                .map_err(|error| error.to_string())?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|error| error.to_string())?;
            let copied = io::copy(
                &mut file.take(2 * 1024 * 1024 * 1024 - *bytes + 1),
                &mut output,
            )
            .map_err(|error| error.to_string())?;
            *bytes += copied;
            if *bytes > 2 * 1024 * 1024 * 1024 {
                return Err("Rust tree exceeds size bound".into());
            }
            fs::set_permissions(target, fs::Permissions::from_mode(metadata.mode() & 0o777))
                .map_err(|error| error.to_string())?;
        } else {
            return Err(format!("unsupported Rust cache member {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn review_copy_cannot_write_account_cache_and_rejects_external_links() {
        let dir = td_fs::TempDir::new("review-rust-copy").unwrap();
        let source = dir.join("source");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/rustc"), b"compiler").unwrap();
        fs::set_permissions(source.join("bin/rustc"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("rustc", source.join("bin/rustdoc")).unwrap();
        let destination = dir.join("copy");
        copy_toolchain(&source, &destination).unwrap();
        assert_ne!(
            fs::metadata(source.join("bin/rustc")).unwrap().ino(),
            fs::metadata(destination.join("bin/rustc")).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(destination.join("bin/rustc")).unwrap().mode() & 0o777,
            0o755
        );
        fs::write(destination.join("bin/rustc"), b"changed").unwrap();
        assert_eq!(fs::read(source.join("bin/rustc")).unwrap(), b"compiler");
        assert_eq!(
            fs::read(destination.join("bin/rustdoc")).unwrap(),
            b"changed"
        );
        symlink("/home/secret", source.join("escape")).unwrap();
        let rejected = dir.join("rejected");
        assert!(copy_toolchain(&source, &rejected).is_err());
        assert!(!rejected.exists());
    }
    #[test]
    #[ignore = "needs TD_AGENT_TEST_PROGRAM, TD_AGENT_JAIL, TD_AGENT_TXT and executable TMPDIR"]
    fn prepared_compiler_runs_in_an_offline_review_jail() {
        use crate::host::{Call, Up};
        use std::time::{Duration, Instant};
        let dir = td_fs::TempDir::new("review-rust-jail").unwrap();
        let scratch = dir.join("scratch");
        let home = dir.join("home");
        let specs = dir.join("specs");
        for path in [&scratch, &home, &specs] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let output = Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let root = fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
        let compiler_before = fs::read(root.join("bin/rustc")).unwrap();
        let copied = scratch.join("rust-toolchain");
        copy_toolchain(&root, &copied).unwrap();
        // Guix wraps rustc with an absolute path; exercise the copied ELF.
        let real = copied.join("bin/.rustc-real");
        if real.is_file() {
            fs::remove_file(copied.join("bin/rustc")).unwrap();
            fs::copy(real, copied.join("bin/rustc")).unwrap();
        }
        let programs = crate::jail::Programs {
            agent: std::env::var_os("TD_AGENT_TEST_PROGRAM").unwrap().into(),
            jail: std::env::var_os(crate::jail::JAIL_VAR).unwrap().into(),
            txt: std::env::var_os(crate::jail::TXT_VAR).unwrap().into(),
        };
        let policy = crate::jail::Policy {
            home: home.clone(),
            worktrees: vec![scratch.clone()],
            ..Default::default()
        };
        let mut client = crate::jail::launch(&programs, &policy, &specs).unwrap();
        let mut invoke = |program: String, args: Vec<String>| {
            let call = Call::ReviewShell {
                command: String::new(),
                cargo_args: Some(args),
                program: Some(program),
                build_env: vec![],
                rust_bin: Some(copied.join("bin").display().to_string()),
                timeout_ms: Some(60_000),
                workdir: None,
                target_dir: scratch.join("target").display().to_string(),
                runner: None,
                cargo_home: None,
            };
            let id = client.call(call).unwrap();
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                assert!(Instant::now() < deadline, "review compiler probe timed out");
                if let Some(reply) = client.next_reply(Duration::from_millis(50)) {
                    if let Up::Done { id: got, outcome } = reply.unwrap() {
                        if got == id {
                            return outcome.unwrap().text;
                        }
                    }
                }
            }
        };
        let selected = invoke("sh".into(), vec!["-c".into(), "command -v rustc".into()]);
        assert!(
            selected.contains(&copied.join("bin/rustc").display().to_string()),
            "{selected}"
        );
        // The Guix compiler embeds its original sysroot; select the fixture.
        let sysroot = invoke(
            "rustc".into(),
            vec![
                "--sysroot".into(),
                copied.display().to_string(),
                "--print".into(),
                "sysroot".into(),
            ],
        );
        assert!(sysroot.starts_with("[exit status 0]"), "{sysroot}");
        assert!(sysroot.contains(&copied.display().to_string()), "{sysroot}");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let source = scratch.join("probe.rs");
        fs::write(&source, format!(r#"fn main() {{
            let home = std::env::var("HOME").unwrap();
            let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
            assert!(mounts.lines().any(|line| {{ let words: Vec<_> = line.split_whitespace().collect(); words.get(4) == Some(&home.as_str()) && words.get(5).unwrap().split(',').any(|flag| flag == "noexec") }}));
            assert!(std::net::TcpStream::connect(("127.0.0.1", {port})).is_err());
            println!("REVIEW-PRIVATE-RUST-OK");
        }}"#)).unwrap();
        let executable = scratch.join("probe");
        let runtime = Command::new("cc")
            .arg("-print-file-name=libgcc_s.so.1")
            .output()
            .unwrap();
        assert!(runtime.status.success());
        let runtime = fs::canonicalize(String::from_utf8(runtime.stdout).unwrap().trim()).unwrap();
        let runpath = format!(
            "-Clink-arg=-Wl,-rpath,{}",
            runtime.parent().unwrap().display()
        );
        let built = invoke(
            "rustc".into(),
            vec![
                source.display().to_string(),
                "-Clink-arg=-Wl,--threads=1".into(),
                runpath,
                "--sysroot".into(),
                copied.display().to_string(),
                "-o".into(),
                executable.display().to_string(),
            ],
        );
        assert!(built.starts_with("[exit status 0]"), "{built}");
        let result = invoke(executable.display().to_string(), vec![]);
        assert!(result.contains("REVIEW-PRIVATE-RUST-OK"), "{result}");
        assert!(!home.join(".td-rust").exists());
        assert_eq!(fs::read(root.join("bin/rustc")).unwrap(), compiler_before);
    }
}
