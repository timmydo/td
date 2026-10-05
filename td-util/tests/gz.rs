//! `td-util gzip`/`gunzip`, run as processes: the file round trip with its
//! mode and time, an existing or dangling target, and input that is not gzip.
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("td-util-gz-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(dir: &Path, args: &[&str]) -> i32 {
    Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .code()
        .unwrap_or(-1)
}

#[test]
fn a_file_round_trips_with_its_mode_and_time() {
    let d = scratch("trip");
    let f = d.join("f");
    std::fs::write(&f, "hello hello hello\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(t)
        .unwrap();
    assert_eq!(run(&d, &["gzip", "-9", "f"]), 0);
    assert!(!f.exists());
    let gz = std::fs::metadata(d.join("f.gz")).unwrap();
    assert_eq!(gz.permissions().mode() & 0o777, 0o640);
    assert_eq!(gz.mtime(), 1_000_000_000);
    assert_eq!(run(&d, &["gunzip", "f.gz"]), 0);
    assert_eq!(std::fs::read(&f).unwrap(), b"hello hello hello\n");
    assert_eq!(std::fs::metadata(&f).unwrap().mtime(), 1_000_000_000);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn an_existing_target_is_refused_and_never_written_through() {
    let d = scratch("target");
    std::fs::write(d.join("f"), "data\n").unwrap();
    std::os::unix::fs::symlink("elsewhere", d.join("f.gz")).unwrap();
    assert_eq!(run(&d, &["gzip", "f"]), 1, "a dangling link is a name too");
    assert!(!d.join("elsewhere").exists());
    assert!(d.join("f").exists());
    assert_eq!(run(&d, &["gzip", "-f", "f"]), 0);
    assert!(!d.join("elsewhere").exists(), "-f replaces the link itself");
    assert!(!std::fs::symlink_metadata(d.join("f.gz"))
        .unwrap()
        .is_symlink());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn input_that_is_not_gzip_is_refused() {
    let d = scratch("magic");
    std::fs::write(d.join("x.gz"), "plain text\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(["gunzip", "x.gz"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in gzip format"));
    assert!(d.join("x.gz").exists());
    std::fs::remove_dir_all(&d).unwrap();
}
