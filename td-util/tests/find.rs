//! `td-util find`, run as a process: walk order, operators, actions and
//! exit status against a scratch tree.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_td-util"))
}

fn tree(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("td-util-find-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("a/b")).unwrap();
    std::fs::create_dir_all(d.join("c")).unwrap();
    std::fs::write(d.join("a/x.c"), "int x;").unwrap();
    std::fs::write(d.join("a/b/y.h"), "").unwrap();
    std::fs::write(d.join("c/Z.C"), "z").unwrap();
    std::os::unix::fs::symlink("a", d.join("link")).unwrap();
    d
}

fn find(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(exe())
        .arg("find")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn walks_sorted_and_prints_by_default() {
    let d = tree("walk");
    let (rc, out) = find(&d, &["."]);
    assert_eq!(rc, 0);
    assert_eq!(
        out,
        ".\n./a\n./a/b\n./a/b/y.h\n./a/x.c\n./c\n./c/Z.C\n./link\n"
    );
    let (_, out) = find(&d, &["a/", "-name", "*.h"]);
    assert_eq!(out, "a/b/y.h\n");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn tests_operators_and_depth() {
    let d = tree("ops");
    let (_, out) = find(&d, &[".", "-type", "f", "-iname", "*.c"]);
    assert_eq!(out, "./a/x.c\n./c/Z.C\n");
    let (_, out) = find(
        &d,
        &[".", "-type", "f", "!", "-name", "*.c", "-o", "-type", "l"],
    );
    assert_eq!(out, "./a/b/y.h\n./c/Z.C\n./link\n");
    let (_, out) = find(&d, &[".", "-mindepth", "1", "-maxdepth", "1", "-type", "d"]);
    assert_eq!(out, "./a\n./c\n");
    let (_, out) = find(
        &d,
        &[
            ".", "-path", "./a/*", "-prune", "-o", "-type", "f", "-print",
        ],
    );
    assert_eq!(out, "./c/Z.C\n");
    let (_, out) = find(&d, &[".", "-empty", "-type", "f"]);
    assert_eq!(out, "./a/b/y.h\n");
    let (_, out) = find(
        &d,
        &[".", "-type", "f", "-size", "+0c", "(", "-name", "x*", ")"],
    );
    assert_eq!(out, "./a/x.c\n");
    let (_, out) = find(&d, &["-L", "link", "-type", "f"]);
    assert_eq!(out, "link/b/y.h\nlink/x.c\n");
    let (_, out) = find(&d, &[".", "-depth", "-path", "./a*"]);
    assert_eq!(out, "./a/b/y.h\n./a/b\n./a/x.c\n./a\n");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn exec_print0_delete_and_quit() {
    let d = tree("exec");
    let (_, out) = find(&d, &["c", "-type", "f", "-exec", "echo", "got:{}", ";"]);
    assert_eq!(out, "got:c/Z.C\n");
    let (_, out) = find(&d, &[".", "-name", "*.[ch]", "-exec", "echo", "{}", "+"]);
    assert_eq!(out, "./a/b/y.h ./a/x.c\n");
    let (_, out) = find(&d, &["a", "-type", "f", "-print0"]);
    assert_eq!(out, "a/b/y.h\0a/x.c\0");
    let (_, out) = find(&d, &[".", "-type", "f", "-print", "-quit"]);
    assert_eq!(out, "./a/b/y.h\n");
    let (rc, _) = find(&d, &["a", "-delete"]);
    assert_eq!(rc, 0);
    assert!(!d.join("a").exists());
    let (rc, _) = find(&d, &["c", "-exec", "false", ";"]);
    assert_eq!(rc, 0, "a false -exec is a false test, not an error");
    let (rc, _) = find(&d, &["c", "-exec", "false", "{}", "+"]);
    assert_eq!(rc, 1, "a failed -exec + batch is find's failure");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn errors_are_reported_and_counted() {
    let d = tree("err");
    let (rc, out) = find(&d, &["nope", "."]);
    assert_eq!(rc, 1);
    assert!(out.starts_with(".\n"));
    let (rc, _) = find(&d, &[".", "-bogus"]);
    assert_eq!(rc, 1);
    let (rc, _) = find(&d, &[".", "(", "-name", "x"]);
    assert_eq!(rc, 1);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_followed_loop_is_reported_and_not_walked() {
    let d = tree("loop");
    std::os::unix::fs::symlink("..", d.join("a/b/up")).unwrap();
    let (rc, out) = find(&d, &["-L", "a"]);
    assert_eq!((rc, out.as_str()), (1, "a\na/b\na/b/y.h\na/x.c\n"));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn delete_refuses_a_prune_it_would_ignore() {
    let d = tree("prune");
    let (rc, _) = find(&d, &[".", "-path", "./a", "-prune", "-o", "-delete"]);
    assert_eq!(rc, 1);
    assert!(d.join("a/b/y.h").exists() && d.join("c/Z.C").exists());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn mmin_compares_the_exact_age() {
    let d = tree("mmin");
    let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(90);
    std::fs::File::options()
        .write(true)
        .open(d.join("a/x.c"))
        .unwrap()
        .set_modified(ago)
        .unwrap();
    // 90 s is in minute 2: over one minute, at most two.
    for (spec, want) in [
        ("1", ""),
        ("2", "a/x.c\n"),
        ("+1", "a/x.c\n"),
        ("-2", "a/x.c\n"),
    ] {
        let (_, out) = find(&d, &["a", "-name", "x.c", "-mmin", spec]);
        assert_eq!(out, want, "-mmin {spec}");
    }
    std::fs::remove_dir_all(&d).unwrap();
}
