//! `td-util od`, run as a process. Every expected output was taken from GNU od
//! 9.1 on the same input: field widths per type, the address radixes, `*`
//! folding and `-v`, the closing address, and `-j`/`-N` across files.
#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn od(dir: &Path, args: &[&str], stdin: &[u8]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .arg("od")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Only a stdin case reads it; the rest may exit before a write lands.
    let mut pipe = child.stdin.take().unwrap();
    if !stdin.is_empty() {
        pipe.write_all(stdin).unwrap();
    }
    drop(pipe);
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("td-util-od-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let mut bytes = b"ABCDEFGHIJKLMNOP".repeat(3);
    bytes.extend_from_slice(b"xyz\x80\xff");
    std::fs::write(d.join("in"), bytes).unwrap();
    std::fs::write(d.join("ab"), b"ab").unwrap();
    std::fs::write(d.join("zero"), [0u8; 4096]).unwrap();
    std::fs::write(d.join("empty"), b"").unwrap();
    d
}

fn ok(s: &str) -> (i32, String) {
    (0, s.to_string())
}

#[test]
fn types_radixes_and_folding_match_gnu() {
    let d = scratch("types");
    let run = |args: &[&str]| od(&d, args, b"");
    assert_eq!(
        run(&["-tx1", "in"]),
        ok(
            "0000000 41 42 43 44 45 46 47 48 49 4a 4b 4c 4d 4e 4f 50\n*\n\
            0000060 78 79 7a 80 ff\n0000065\n"
        )
    );
    assert_eq!(
        run(&["-An", "-tx1", "in"]),
        ok(" 41 42 43 44 45 46 47 48 49 4a 4b 4c 4d 4e 4f 50\n*\n 78 79 7a 80 ff\n")
    );
    let line = "101 102 103 104 105 106 107 110 111 112 113 114 115 116 117 120";
    assert_eq!(
        run(&["-Ad", "-to1", "-v", "in"]),
        ok(&format!(
            "0000000 {line}\n0000016 {line}\n0000032 {line}\n\
             0000048 170 171 172 200 377\n0000053\n"
        ))
    );
    assert_eq!(
        run(&["-Ax", "-tu1", "-j", "45", "in"]),
        ok("00002d  78  79  80 120 121 122 128 255\n000035\n")
    );
    assert_eq!(
        run(&["-Ax", "-td1", "-j", "0x2d", "-N", "5", "in"]),
        ok("00002d   78   79   80  120  121\n000032\n")
    );
    // Octal counts, and an attached and a detached type.
    assert_eq!(
        run(&["-t", "x1", "-j", "063", "in"]),
        ok("0000063 80 ff\n0000065\n")
    );
    assert_eq!(
        run(&["-tx1", "zero"]),
        ok("0000000 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00\n*\n0010000\n")
    );
    assert_eq!(
        run(&["-An", "-tx1", "-j", "510", "-N", "2", "zero"]),
        ok(" 00 00\n")
    );
    assert_eq!(run(&["-An", "-tx1", "empty"]), ok(""));
    assert_eq!(run(&["-tx1", "empty"]), ok("0000000\n"));
    assert_eq!(run(&["-An", "-tx1", "-N", "0", "in"]), ok(""));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn files_are_one_stream_for_skip_and_count() {
    let d = scratch("stream");
    let run = |args: &[&str], stdin: &[u8]| od(&d, args, stdin);
    assert_eq!(
        run(&["-An", "-tx1", "ab", "in", "-N", "3"], b""),
        ok(" 61 62 41\n")
    );
    assert_eq!(
        run(&["-An", "-tx1", "-j", "2", "ab", "in", "-N", "3"], b""),
        ok(" 41 42 43\n")
    );
    assert_eq!(
        run(
            &["-An", "-tx1", "-j", "3", "-", "ab"],
            b"abcdefghijklmnopq\n"
        ),
        ok(" 64 65 66 67 68 69 6a 6b 6c 6d 6e 6f 70 71 0a 61\n 62\n")
    );
    assert_eq!(run(&["-An", "-tx1"], b"ab\n"), ok(" 61 62 0a\n"));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A skip past the end and an unopenable file exit 1 as GNU's do (the
/// readable files still dump); a form outside the served subset is refused,
/// never approximated.
#[test]
fn errors_and_refusals_exit_1() {
    let d = scratch("errors");
    let run = |args: &[&str]| od(&d, args, b"");
    assert_eq!(run(&["-tx1", "-j", "100", "in"]), (1, String::new()));
    assert_eq!(
        run(&["-tx1", "nosuch", "ab"]),
        (1, "0000000 61 62\n0000002\n".into())
    );
    for refused in [
        &["ab"][..],
        &["-tx2", "ab"],
        &["-c", "ab"],
        &["-tx1", "-tx1", "ab"],
        &["-tx1", "-N", "1k", "ab"],
        &["-tx1", "-A"],
        // A multibyte option character is refused, not split mid-character.
        &["-tx1", "-\u{e9}", "ab"],
        &["-tx1", "-N", "0x+4", "ab"],
        &["-tx1", "-N", "0+4", "ab"],
        &["-tx1", "-j", "18446744073709551615", "-N", "1", "ab"],
    ] {
        assert_eq!(run(refused), (1, String::new()), "{refused:?}");
    }
    // A file that opens but cannot be read is reported and passed over, and
    // the stream goes on; when nothing opens there is no closing address,
    // even for -N 0.
    std::fs::create_dir_all(d.join("dir")).unwrap();
    assert_eq!(
        run(&["-tx1", "ab", "dir", "ab"]),
        (1, "0000000 61 62 61 62\n0000004\n".into())
    );
    assert_eq!(run(&["-tx1", "nosuch"]), (1, String::new()));
    assert_eq!(run(&["-tx1", "-N0", "nosuch"]), (1, String::new()));
    std::fs::remove_dir_all(&d).unwrap();
}

/// Grouped short options, GNU's number syntax, and fold runs that end, restart
/// and are cut by -N, all as GNU od 9.1 prints them.
#[test]
fn grouping_numbers_and_fold_runs_match_gnu() {
    let d = scratch("fold");
    let block = |c: u8| [c; 16];
    let a = format!(" {}", vec!["41"; 16].join(" "));
    let b = format!(" {}", vec!["42"; 16].join(" "));
    std::fs::write(
        d.join("aaaba"),
        [
            block(b'A'),
            block(b'A'),
            block(b'A'),
            block(b'B'),
            block(b'A'),
        ]
        .concat(),
    )
    .unwrap();
    std::fs::write(
        d.join("aabb"),
        [block(b'A'), block(b'A'), block(b'B'), block(b'B')].concat(),
    )
    .unwrap();
    let run = |args: &[&str]| od(&d, args, b"");
    assert_eq!(
        run(&["-tx1", "aaaba"]),
        ok(&format!("0000000{a}\n*\n0000060{b}\n0000100{a}\n0000120\n"))
    );
    assert_eq!(
        run(&["-tx1", "aabb"]),
        ok(&format!("0000000{a}\n*\n0000040{b}\n*\n0000100\n"))
    );
    assert_eq!(
        run(&["-tx1", "-N", "33", "aaaba"]),
        ok(&format!("0000000{a}\n*\n0000040 41\n0000041\n"))
    );
    assert_eq!(run(&["-vAn", "-tx1", "-N2", "aabb"]), ok(" 41 41\n"));
    assert_eq!(
        run(&["-vtx1", "-N", "+020", "aabb"]),
        ok(&format!("0000000{a}\n0000020\n"))
    );
    assert_eq!(
        run(&["-An", "-tx1", "-j", " 0x30", "-N", "1", "aabb"]),
        ok(" 42\n")
    );
    std::fs::remove_dir_all(&d).unwrap();
}
