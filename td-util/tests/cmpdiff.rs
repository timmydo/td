//! `td-util cmp` and `td-util diff`, run as processes: exit statuses (0 same,
//! 1 different, 2 trouble), the brief and recursive forms, and `-N`.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("td-util-cd-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("l/sub")).unwrap();
    std::fs::create_dir_all(d.join("r/sub")).unwrap();
    std::fs::write(d.join("l/same"), "x\n").unwrap();
    std::fs::write(d.join("r/same"), "x\n").unwrap();
    std::fs::write(d.join("l/sub/f"), "a\nb\n").unwrap();
    std::fs::write(d.join("r/sub/f"), "a\nc\n").unwrap();
    std::fs::write(d.join("l/only"), "o\n").unwrap();
    std::fs::write(d.join("bin1"), b"a\0b").unwrap();
    std::fs::write(d.join("bin2"), b"a\0c").unwrap();
    d
}

#[test]
fn cmp_statuses_and_listing() {
    let d = scratch("cmp");
    assert_eq!(run(&d, &["cmp", "l/same", "r/same"]), (0, String::new()));
    assert_eq!(
        run(&d, &["cmp", "l/sub/f", "r/sub/f"]),
        (1, "l/sub/f r/sub/f differ: char 3, line 2\n".to_string())
    );
    assert_eq!(
        run(&d, &["cmp", "-s", "l/sub/f", "r/sub/f"]),
        (1, String::new())
    );
    assert_eq!(
        run(&d, &["cmp", "-l", "bin1", "bin2"]),
        (1, "3 142 143\n".to_string())
    );
    assert_eq!(run(&d, &["cmp", "l/same", "l/sub/f"]).0, 1);
    assert_eq!(run(&d, &["cmp", "l/same", "missing"]).0, 2);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn diff_files_dirs_and_flags() {
    let d = scratch("diff");
    assert_eq!(run(&d, &["diff", "l/same", "r/same"]), (0, String::new()));
    assert_eq!(
        run(&d, &["diff", "l/sub/f", "r/sub/f"]),
        (1, "2c2\n< b\n---\n> c\n".to_string())
    );
    assert_eq!(
        run(&d, &["diff", "-u", "l/sub/f", "r/sub/f"]),
        (
            1,
            "--- l/sub/f\n+++ r/sub/f\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n".to_string()
        )
    );
    assert_eq!(
        run(&d, &["diff", "-q", "l/sub/f", "r/sub/f"]),
        (1, "Files l/sub/f and r/sub/f differ\n".to_string())
    );
    assert_eq!(
        run(&d, &["diff", "bin1", "bin2"]),
        (1, "Binary files bin1 and bin2 differ\n".to_string())
    );
    assert_eq!(
        run(&d, &["diff", "-rq", "l", "r"]),
        (
            1,
            "Only in l: only\nFiles l/sub/f and r/sub/f differ\n".to_string()
        )
    );
    let (rc, out) = run(&d, &["diff", "-ruN", "l", "r"]);
    assert_eq!(rc, 1);
    assert!(
        out.contains("--- l/only\n+++ r/only\n@@ -1 +0,0 @@\n-o\n"),
        "{out}"
    );
    assert_eq!(run(&d, &["diff", "l/same", "missing"]).0, 2);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn diff_rn_adds_new_directories_and_keeps_bytes() {
    let d = scratch("newdir");
    std::fs::create_dir_all(d.join("r/new/deeper")).unwrap();
    std::fs::write(d.join("r/new/deeper/g"), "g\n").unwrap();
    let (rc, out) = run(&d, &["diff", "-ruN", "l", "r"]);
    assert_eq!(rc, 1);
    assert!(
        out.contains("--- l/new/deeper/g\n+++ r/new/deeper/g\n@@ -0,0 +1 @@\n+g\n"),
        "{out}"
    );

    // Text that is not UTF-8 comes out as the same bytes, not replaced.
    std::fs::write(d.join("lat1"), b"caf\xe9\n").unwrap();
    std::fs::write(d.join("lat2"), b"caf\xe8\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(["diff", "lat1", "lat2"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"1c1\n< caf\xe9\n---\n> caf\xe8\n");
    std::fs::remove_dir_all(&d).unwrap();
}
