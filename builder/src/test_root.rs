//! Opt-in filesystem identity fixture for tests inside rootless namespaces.
//! This preserves ambient files, not a filesystem security isolation boundary.

use crate::sandbox::{self, Bind};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

pub const ENV: &str = "TD_TEST_TRUSTED_ROOT";

pub fn enabled(value: Option<&std::ffi::OsStr>) -> io::Result<bool> {
    match value {
        None => Ok(false),
        Some(value) if value == "1" => Ok(true),
        Some(_) => Err(io::Error::other("TD_TEST_TRUSTED_ROOT must be absent or 1")),
    }
}

fn text(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("trusted test root requires UTF-8 paths"))
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for attempt in 0..128 {
            let path = Path::new("/tmp").join(format!(
                "td-test-root-{}-{stamp:x}-{attempt}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other(
            "trusted test root scratch names exhausted",
        ))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only our exclusive fixture; namespace mounts are already gone.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bind(path: &Path) -> io::Result<Bind> {
    Ok(Bind {
        src: text(path)?,
        dest: None,
        readonly: false,
        ro_optional: false,
    })
}

fn resolvable(path: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        match fs::metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                eprintln!(
                    "trusted test root: omitting dangling root link {}",
                    path.display()
                );
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn bindings(cwd: &Path, programs: &[PathBuf], temporary: &Path) -> io::Result<Vec<Bind>> {
    if cwd == Path::new("/tmp") {
        return Err(io::Error::other(
            "trusted test root requires a working directory other than /tmp",
        ));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir("/")? {
        let path = entry?.path();
        // host_shell supplies private procfs, minimal /dev and scratch /tmp.
        if !matches!(path.to_str(), Some("/proc" | "/dev" | "/tmp" | "/oldroot"))
            && resolvable(&path)?
        {
            paths.push(path);
        }
    }
    paths.sort();
    if cwd.starts_with("/tmp") {
        paths.push(cwd.to_owned());
    }
    for program in programs {
        if program.starts_with("/tmp")
            && !(cwd.starts_with("/tmp") && program.starts_with(cwd))
            && !paths.contains(program)
        {
            paths.push(program.to_owned());
        }
    }
    let mut binds = vec![Bind {
        src: text(temporary)?,
        dest: Some("/tmp".into()),
        readonly: false,
        ro_optional: false,
    }];
    for path in paths {
        binds.push(bind(&path)?);
    }
    Ok(binds)
}

pub fn run(program: &str, args: &[String]) -> io::Result<ExitStatus> {
    let cwd = std::env::current_dir()?;
    let program = fs::canonicalize(program)?;
    let supervisor = fs::canonicalize(std::env::current_exe()?)?;
    let scratch = Scratch::new()?;
    let temporary = scratch.0.join("tmp");
    fs::create_dir(&temporary)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o1777))?;
    let binds = bindings(&cwd, &[program.clone(), supervisor.clone()], &temporary)?;
    let mut env = Vec::new();
    for (key, value) in std::env::vars_os() {
        if key == ENV || key == "TMPDIR" {
            continue;
        }
        let key = key
            .into_string()
            .map_err(|_| io::Error::other("non-UTF-8 test environment key"))?;
        let value = value
            .into_string()
            .map_err(|_| io::Error::other("non-UTF-8 test environment value"))?;
        env.push((key, value));
    }
    let mut child_args = vec!["test-root-child".into(), text(&program)?];
    child_args.extend_from_slice(args);
    sandbox::host_shell(
        &text(&supervisor)?,
        &child_args,
        &binds,
        &[],
        &std::env::var("PATH").unwrap_or_default(),
        &std::env::var("HOME").unwrap_or_else(|_| "/".into()),
        &text(&cwd)?,
        &env,
        &[],
        &scratch.0,
    )
}

pub fn exit_code(status: ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    let code = status
        .code()
        .unwrap_or_else(|| 128i32.saturating_add(status.signal().unwrap_or(1)));
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Namespace PID 1 supervises an ordinary child, preserving default signals.
pub fn child(args: &[String]) -> ExitCode {
    let Some((program, rest)) = args.split_first() else {
        eprintln!("td-builder test-root-child: missing test executable");
        return ExitCode::FAILURE;
    };
    match Command::new(program).args(rest).status() {
        Ok(status) => exit_code(status),
        Err(error) => {
            eprintln!("td-builder test-root-child: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;

    #[test]
    fn opt_in_is_explicit_and_invalid_values_fail_closed() {
        assert!(!enabled(None).unwrap());
        assert!(enabled(Some("1".as_ref())).unwrap());
        for value in ["", "0", "true", "yes"] {
            assert!(enabled(Some(value.as_ref())).is_err());
        }
    }

    #[test]
    fn private_tmp_binding_precedes_worktree_and_external_executables() {
        let temporary = Path::new("/tmp/fixture/tmp");
        let cwd = Path::new("/tmp/project");
        let programs = [
            PathBuf::from("/tmp/artifacts/test"),
            PathBuf::from("/tmp/runner"),
        ];
        let binds = bindings(cwd, &programs, temporary).unwrap();
        assert_eq!(binds.first().unwrap().dest.as_deref(), Some("/tmp"));
        assert_eq!(binds.first().unwrap().src, "/tmp/fixture/tmp");
        for path in ["/tmp/project", "/tmp/artifacts/test", "/tmp/runner"] {
            assert!(binds.iter().any(|b| b.src == path));
        }
        assert!(!binds
            .iter()
            .any(|b| matches!(b.src.as_str(), "/tmp" | "/proc" | "/dev" | "/oldroot")));
        assert!(bindings(Path::new("/tmp"), &programs, temporary).is_err());
        assert!(bindings(Path::new("/"), &programs, temporary)
            .unwrap()
            .iter()
            .any(|b| b.src == "/tmp/artifacts/test"));
    }

    #[test]
    fn dangling_links_are_omitted_but_missing_entries_are_errors() {
        let dir = Scratch::new().unwrap();
        let path = dir.0.join("link");
        assert!(resolvable(&path).is_err());
        symlink(dir.0.join("missing"), &path).unwrap();
        assert!(!resolvable(&path).unwrap());
        fs::write(dir.0.join("missing"), b"x").unwrap();
        assert!(resolvable(&path).unwrap());
    }

    #[test]
    fn private_root_preserves_cwd_caps_and_socket_permissions() {
        if let Some(mode) = std::env::var_os("TD_TEST_ROOT_PROBE") {
            assert_ne!(
                std::process::id(),
                1,
                "tests must not inherit PID 1 signal semantics"
            );
            let uid = crate::sys::getuid();
            for path in ["/", "/tmp"] {
                let metadata = fs::metadata(path).unwrap();
                assert_eq!(metadata.uid(), uid);
                assert_eq!(metadata.mode() & 0o1777, 0o1777);
            }
            assert!(std::env::var_os(ENV).is_none());
            assert_eq!(std::env::var("TMPDIR").unwrap(), "/tmp");
            assert_eq!(
                std::env::current_dir().unwrap(),
                PathBuf::from(std::env::var("TD_TEST_ROOT_CWD").unwrap())
            );
            let limits = crate::sys::get_rlimit(crate::sys::RLIMIT_DATA).unwrap();
            assert_eq!(
                format!("{},{}", limits.0, limits.1),
                std::env::var("TD_TEST_ROOT_LIMITS").unwrap()
            );
            let dir = Scratch::new().unwrap();
            fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
            let path = dir.0.join("control");
            let listener = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_eq!(metadata.uid(), uid);
            drop(listener);
            fs::write("proof", b"child executed").unwrap();
            if mode == "exit" {
                std::process::exit(23);
            }
            if mode == "signal" {
                crate::sys::kill_recorded(
                    crate::sys::KillTarget::Pid(i64::from(std::process::id())),
                    crate::sys::SIGTERM,
                    "trusted-root self-signal regression probe",
                )
                .unwrap();
                std::process::exit(99);
            }
            return;
        }
        let exe = std::env::current_exe().unwrap();
        // The same release runner Cargo requires; build it before cargo test.
        let runner = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/td-builder");
        let ambient = crate::sys::get_rlimit(crate::sys::RLIMIT_DATA).unwrap();
        let limits = crate::run_capped::effective_limits(32 * 1024 * 1024, ambient);
        for (mode, expected) in [("ok", 0), ("exit", 23), ("signal", 143)] {
            let dir = Scratch::new().unwrap();
            let output = Command::new(&runner)
                .arg("run-capped")
                .arg(&exe)
                .args([
                    "--exact",
                    "test_root::tests::private_root_preserves_cwd_caps_and_socket_permissions",
                    "--nocapture",
                ])
                .current_dir(&dir.0)
                .env(ENV, "1")
                .env("TD_RUN_CAPPED_MIB", "32")
                .env("TD_TEST_ROOT_PROBE", mode)
                .env("TD_TEST_ROOT_CWD", &dir.0)
                .env("TD_TEST_ROOT_LIMITS", format!("{},{}", limits.0, limits.1))
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                fs::read(dir.0.join("proof")).unwrap(),
                b"child executed",
                "successful zero-test libtest runs are not evidence"
            );
        }
        let dir = Scratch::new().unwrap();
        let output = Command::new(&runner)
            .arg("run-capped")
            .arg(&exe)
            .args([
                "--exact",
                "test_root::tests::private_root_preserves_cwd_caps_and_socket_permissions",
            ])
            .current_dir(&dir.0)
            .env(ENV, "0")
            .env("TD_TEST_ROOT_PROBE", "ok")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(!dir.0.join("proof").exists());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be absent or 1"));
    }
}
