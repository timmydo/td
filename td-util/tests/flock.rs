#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use td_fs::TempDir;

fn flock() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_td-util"));
    cmd.arg("flock");
    cmd
}

#[test]
fn inherited_descriptor_holds_the_lock_until_unlock_or_last_close() {
    let dir = TempDir::new("flock-inherited").unwrap();
    let path = dir.path().join("lock");
    let holder = File::create(&path).unwrap();
    for action in ["-x", "-u", "-x"] {
        assert!(flock()
            .args([action, "0"])
            .stdin(holder.try_clone().unwrap())
            .status()
            .unwrap()
            .success());
        let status = flock().arg("-n").arg(&path).arg("true").status().unwrap();
        assert_eq!(status.code(), Some(if action == "-u" { 0 } else { 1 }));
    }
    drop(holder);
    assert!(flock()
        .arg("-n")
        .arg(path)
        .arg("true")
        .status()
        .unwrap()
        .success());
}

#[test]
fn command_receives_its_arguments_verbatim_and_returns_its_status() {
    let dir = TempDir::new("flock-command").unwrap();
    let path = dir.path().join(OsStr::from_bytes(b"lock\xff"));
    let arg = dir.path().join(OsStr::from_bytes(b"a b\xfe"));
    let out = flock()
        .arg(&path)
        .arg(env!("CARGO_BIN_EXE_td-util"))
        .arg("flock")
        .arg(&arg)
        .args(["sh", "-c", "printf locked; exit 23"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(23));
    assert_eq!(out.stdout, b"locked");
    assert!(arg.is_file());
    assert!(path.is_file());
}

#[test]
fn locked_command_can_release_the_inherited_description() {
    if let Some(path) = std::env::var_os("TD_FLOCK_TEST_FILE") {
        assert_eq!(
            flock()
                .arg("-n")
                .arg(&path)
                .arg("true")
                .status()
                .unwrap()
                .code(),
            Some(1)
        );
        let descriptor = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .find(|entry| std::fs::read_link(entry.path()).is_ok_and(|p| p.as_os_str() == path))
            .unwrap()
            .file_name();
        assert!(flock()
            .arg("-u")
            .arg(descriptor)
            .status()
            .unwrap()
            .success());
        assert!(flock()
            .arg("-n")
            .arg(path)
            .arg("true")
            .status()
            .unwrap()
            .success());
        return;
    }
    let dir = TempDir::new("flock-transfer").unwrap();
    let path = dir.path().join("lock");
    assert!(flock()
        .arg(&path)
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "locked_command_can_release_the_inherited_description"
        ])
        .env("TD_FLOCK_TEST_FILE", path)
        .status()
        .unwrap()
        .success());
}
