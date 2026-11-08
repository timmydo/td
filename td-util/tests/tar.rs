#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};
use td_fs::TempDir;

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_td-util"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn header(name: &str, kind: u8, size: u64, target: &str) -> Vec<u8> {
    header_mode(name, kind, size, target, 0o755)
}

fn header_mode(name: &str, kind: u8, size: u64, target: &str, mode: u32) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
    h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    h[136..148].copy_from_slice(b"00000000001\0");
    h[148..156].fill(b' ');
    h[156] = kind;
    h[157..157 + target.len()].copy_from_slice(target.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

fn archive(name: &str) -> Vec<u8> {
    let mut bytes = header(name, b'0', 7, "");
    bytes.extend_from_slice(b"fixture");
    bytes.resize(1024, 0);
    bytes.extend(header("link", b'2', 0, name));
    bytes.extend(header("hard", b'1', 0, name));
    bytes.resize(bytes.len() + 1024, 0);
    bytes
}

#[test]
fn extracts_plain_and_gzip_archives_with_modes_times_and_links() {
    let d = TempDir::new("td-util-tar-extract").unwrap();
    fs::write(d.join("input.tar"), archive("bin/tool")).unwrap();
    fs::create_dir(d.join("plain")).unwrap();
    fs::create_dir(d.join("compressed")).unwrap();
    let result = run(&d, &["tar", "-xfinput.tar", "-Cplain"]);
    assert!(result.status.success(), "{result:?}");
    let result = run(&d, &["gzip", "input.tar"]);
    assert!(result.status.success(), "{result:?}");
    let result = run(&d, &["tar", "-xzf", "input.tar.gz", "-C", "compressed"]);
    assert!(result.status.success(), "{result:?}");
    for name in ["plain", "compressed"] {
        let out = d.join(name);
        assert_eq!(fs::read(out.join("bin/tool")).unwrap(), b"fixture");
        assert_eq!(fs::read(out.join("link")).unwrap(), b"fixture");
        let meta = fs::metadata(out.join("bin/tool")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o755);
        assert_eq!(meta.mtime(), 1);
        assert_eq!(meta.ino(), fs::metadata(out.join("hard")).unwrap().ino());
    }
}

#[test]
fn refuses_traversal_and_existing_symlink_parents() {
    let d = TempDir::new("td-util-tar-paths").unwrap();
    fs::create_dir(d.join("out")).unwrap();
    fs::create_dir(d.join("outside")).unwrap();
    symlink("../outside", d.join("out/bin")).unwrap();
    for name in ["../outside/escape", "/escape", "bin/escape"] {
        fs::write(d.join("input.tar"), archive(name)).unwrap();
        let result = run(&d, &["tar", "-xf", "input.tar", "-C", "out"]);
        assert!(!result.status.success(), "{name}: {result:?}");
        assert!(!d.join("outside/escape").exists());
    }
}

#[test]
fn refuses_bad_checksums_truncation_and_oversized_entries() {
    let d = TempDir::new("td-util-tar-malformed").unwrap();
    fs::create_dir(d.join("out")).unwrap();
    let mut bad_checksum = archive("bin/tool");
    bad_checksum[0] ^= 1;
    for bytes in [
        bad_checksum,
        vec![0; 511],
        header("huge", b'0', 1024 * 1024 * 1024 + 1, ""),
    ] {
        fs::write(d.join("input.tar"), bytes).unwrap();
        let result = run(&d, &["tar", "-x", "-f", "input.tar", "-C", "out"]);
        assert!(!result.status.success(), "{result:?}");
        assert!(fs::read_dir(d.join("out")).unwrap().next().is_none());
    }
    fs::write(d.join("input.tar"), archive("bin/tool")).unwrap();
    assert!(run(&d, &["gzip", "input.tar"]).status.success());
    let mut bytes = fs::read(d.join("input.tar.gz")).unwrap();
    let crc_offset = bytes.len() - 8;
    bytes[crc_offset] ^= 1;
    fs::write(d.join("input.tar.gz"), bytes).unwrap();
    let result = run(&d, &["tar", "-xzf", "input.tar.gz", "-C", "out"]);
    assert!(!result.status.success(), "{result:?}");
    assert!(fs::read_dir(d.join("out")).unwrap().next().is_none());
}

#[test]
fn rejects_unsupported_or_ambiguous_arguments() {
    let d = TempDir::new("td-util-tar-args").unwrap();
    for args in [
        vec!["tar"],
        vec!["tar", "-cf", "new.tar"],
        vec!["tar", "-xf"],
        vec!["tar", "-xf", "-"],
        vec!["tar", "-xxf", "a"],
        vec!["tar", "-xf", "a", "-f", "b"],
        vec!["tar", "-xf", "a", "-C", "missing"],
    ] {
        assert!(!run(&d, &args).status.success(), "{args:?}");
    }
    assert!(run(&d, &["tar", "--help"]).status.success());
}

#[test]
fn preserves_private_directories_and_applies_umask_after_children() {
    let d = TempDir::new("td-util-tar-metadata").unwrap();
    let mut bytes = header_mode("private", b'5', 0, "", 0o700);
    bytes.extend(header_mode("readonly", b'5', 0, "", 0o555));
    bytes.extend(header_mode("private/data", b'0', 1, "", 0o644));
    bytes.push(b'p');
    bytes.resize(2048, 0);
    bytes.extend(header_mode("readonly/data", b'0', 1, "", 0o6755));
    bytes.push(b'r');
    bytes.resize(4096, 0);
    fs::write(d.join("input.tar"), bytes).unwrap();
    for mask in ["022", "077"] {
        fs::create_dir(d.join(mask)).unwrap();
        let result = Command::new("sh")
            .args([
                "-c",
                "umask \"$1\"; shift; exec \"$@\"",
                "tar-fixture",
                mask,
            ])
            .arg(env!("CARGO_BIN_EXE_td-util"))
            .args(["tar", "-xf", "input.tar", "-C", mask])
            .current_dir(&d)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        let out = d.join(mask);
        assert_eq!(
            fs::metadata(out.join("private")).unwrap().mode() & 0o7777,
            0o700
        );
        assert_eq!(fs::metadata(out.join("private")).unwrap().mtime(), 1);
        let expected = if mask == "077" { 0o600 } else { 0o644 };
        assert_eq!(
            fs::metadata(out.join("private/data")).unwrap().mode() & 0o7777,
            expected
        );
        let expected = if mask == "077" { 0o500 } else { 0o555 };
        assert_eq!(
            fs::metadata(out.join("readonly")).unwrap().mode() & 0o7777,
            expected
        );
        assert_eq!(fs::metadata(out.join("readonly")).unwrap().mtime(), 1);
        assert_eq!(fs::read(out.join("readonly/data")).unwrap(), b"r");
        let expected = if mask == "077" { 0o700 } else { 0o755 };
        assert_eq!(
            fs::metadata(out.join("readonly/data")).unwrap().mode() & 0o7777,
            expected
        );
    }
}

#[test]
fn truncated_private_files_remain_private() {
    let d = TempDir::new("td-util-tar-private-partial").unwrap();
    fs::create_dir(d.join("out")).unwrap();
    let mut bytes = header_mode("secret", b'0', 4096, "", 0o600);
    bytes.extend_from_slice(b"partial secret");
    fs::write(d.join("input.tar"), bytes).unwrap();
    let result = run(&d, &["tar", "-xf", "input.tar", "-C", "out"]);
    assert!(!result.status.success(), "{result:?}");
    assert_eq!(
        fs::metadata(d.join("out/secret")).unwrap().mode() & 0o077,
        0
    );
}

/// A tree holding each kind `tar -c` stores, and the names past 100 bytes that
/// take long-name records.
fn tree(root: &Path) -> String {
    let long = "d".repeat(120);
    fs::create_dir_all(root.join("src/nested").join(&long)).unwrap();
    fs::create_dir(root.join("src/empty")).unwrap();
    fs::write(root.join("src/plain"), b"plain bytes").unwrap();
    fs::write(root.join("src/tool"), vec![7u8; 70_000]).unwrap();
    fs::set_permissions(root.join("src/tool"), fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(root.join("src/na\u{ef}ve"), b"").unwrap();
    // Its 100th byte splits a character.
    let split = format!("{}\u{e9}", "s".repeat(99));
    fs::write(root.join("src").join(&split), b"split").unwrap();
    fs::write(root.join("src/nested").join(&long).join("inner"), b"deep").unwrap();
    symlink("plain", root.join("src/near")).unwrap();
    let far = format!("nested/{long}/inner");
    symlink(&far, root.join("src/far")).unwrap();
    fs::hard_link(root.join("src/plain"), root.join("src/again")).unwrap();
    long
}

/// The named file in two trees is the same: kind, bytes, target and mode.
fn same(a: &Path, b: &Path, name: &str) {
    let (left, right) = (a.join(name), b.join(name));
    let (lm, rm) = (
        fs::symlink_metadata(&left).unwrap(),
        fs::symlink_metadata(&right).unwrap(),
    );
    assert_eq!(lm.file_type(), rm.file_type(), "{name}");
    if lm.file_type().is_symlink() {
        assert_eq!(
            fs::read_link(&left).unwrap(),
            fs::read_link(&right).unwrap()
        );
        return;
    }
    assert_eq!(lm.mode() & 0o777, rm.mode() & 0o777, "{name}");
    if lm.is_file() {
        assert_eq!(
            fs::read(&left).unwrap(),
            fs::read(&right).unwrap(),
            "{name}"
        );
        assert_eq!(lm.mtime(), rm.mtime(), "{name}");
    }
}

#[test]
fn creates_archives_its_own_extraction_restores() {
    let d = TempDir::new("td-util-tar-create").unwrap();
    let long = tree(&d);
    let members = [
        "plain".to_string(),
        "tool".to_string(),
        "na\u{ef}ve".to_string(),
        "near".to_string(),
        "far".to_string(),
        "again".to_string(),
        "empty".to_string(),
        format!("nested/{long}/inner"),
        format!("{}\u{e9}", "s".repeat(99)),
    ];
    for (flags, archive) in [("-cf", "out.tar"), ("-czf", "out.tar.gz")] {
        let result = run(&d, &["tar", flags, archive, "-C", "src", "."]);
        assert!(result.status.success(), "{result:?}");
        let into = format!("into-{archive}");
        fs::create_dir(d.join(&into)).unwrap();
        let extract = if archive.ends_with(".gz") {
            "-xzf"
        } else {
            "-xf"
        };
        let result = run(&d, &["tar", extract, archive, "-C", &into]);
        assert!(result.status.success(), "{result:?}");
        for name in &members {
            same(&d.join("src"), &d.join(&into), name);
        }
        // A hard link is another copy.
        assert_ne!(
            fs::metadata(d.join(&into).join("again")).unwrap().ino(),
            fs::metadata(d.join(&into).join("plain")).unwrap().ino()
        );
    }
}

/// Names, kinds and owners in an archive with no long names.
fn members(archive: &[u8]) -> Vec<(String, u8, u64)> {
    let octal = |field: &[u8]| {
        let digits = std::str::from_utf8(field.split(|b| *b == 0).next().unwrap()).unwrap();
        u64::from_str_radix(digits, 8).unwrap()
    };
    let mut found = Vec::new();
    let mut at = 0;
    while at + 512 <= archive.len() && archive[at] != 0 {
        let h = &archive[at..at + 512];
        let name = h[..100].split(|b| *b == 0).next().unwrap();
        let size = octal(&h[124..136]);
        found.push((
            String::from_utf8(name.to_vec()).unwrap(),
            h[156],
            octal(&h[108..116]),
        ));
        at += 512 + size.div_ceil(512) as usize * 512;
    }
    assert_eq!(&archive[at..], &[0; 1024][..], "two zero blocks end it");
    found
}

#[test]
fn writes_to_standard_output_in_byte_order_the_same_each_time() {
    let d = TempDir::new("td-util-tar-stdout").unwrap();
    fs::create_dir_all(d.join("t/b/inner")).unwrap();
    fs::write(d.join("t/b/inner/x"), b"x").unwrap();
    fs::write(d.join("t/a"), b"a").unwrap();
    fs::write(d.join("t/B"), b"B").unwrap();
    symlink("a", d.join("t/c")).unwrap();
    let first = run(&d, &["tar", "-cf", "-", "t", "./t/a"]);
    assert!(first.status.success(), "{first:?}");
    let second = run(&d, &["tar", "-c", "-f-", "--", "t", "./t/a"]);
    assert_eq!(first.stdout, second.stdout);
    let uid = fs::metadata(d.join("t/a")).unwrap().uid();
    let found = members(&first.stdout);
    let names: Vec<(&str, u8)> = found.iter().map(|(n, k, _)| (n.as_str(), *k)).collect();
    assert_eq!(
        names,
        [
            ("t/", b'5'),
            ("t/B", b'0'),
            ("t/a", b'0'),
            ("t/b/", b'5'),
            ("t/b/inner/", b'5'),
            ("t/b/inner/x", b'0'),
            ("t/c", b'2'),
            ("t/a", b'0'),
        ]
    );
    assert!(found.iter().all(|(_, _, owner)| *owner == u64::from(uid)));
}

#[test]
fn leaves_out_the_archive_itself_and_sockets() {
    let d = TempDir::new("td-util-tar-self").unwrap();
    fs::create_dir(d.join("t")).unwrap();
    fs::write(d.join("t/kept"), b"kept").unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(d.join("t/socket")).unwrap();
    let result = run(&d, &["tar", "-cf", "t/self.tar", "-C", "t", "."]);
    assert!(result.status.success(), "{result:?}");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("the archive itself is not stored"),
        "{stderr}"
    );
    assert!(stderr.contains("a socket is not stored"), "{stderr}");
    let found = members(&fs::read(d.join("t/self.tar")).unwrap());
    let names: Vec<&str> = found.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, ["kept"]);
}

#[test]
fn leaves_out_the_archive_on_standard_output_and_follows_a_linked_directory() {
    let d = TempDir::new("td-util-tar-stdout-self").unwrap();
    fs::create_dir(d.join("real")).unwrap();
    fs::write(d.join("real/kept"), b"kept").unwrap();
    symlink("real", d.join("current")).unwrap();
    let out = fs::File::create(d.join("real/out.tar")).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .current_dir(&d)
        .args(["tar", "-cf", "-", "-C", "current", "."])
        .stdout(out)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let found = members(&fs::read(d.join("real/out.tar")).unwrap());
    let names: Vec<&str> = found.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, ["kept"]);
}

#[test]
fn refuses_paths_outside_the_directory_and_empty_archives() {
    let d = TempDir::new("td-util-tar-create-args").unwrap();
    fs::create_dir(d.join("t")).unwrap();
    for args in [
        vec!["tar", "-cf", "a.tar"],
        vec!["tar", "-c", "t"],
        vec!["tar", "-cf", "a.tar", "/etc"],
        vec!["tar", "-cf", "a.tar", "t/../t"],
        vec!["tar", "-cf", "a.tar", ""],
        vec!["tar", "-cf", "a.tar", "missing"],
        vec!["tar", "-cxf", "a.tar", "t"],
        vec!["tar", "-cf", "a.tar", "-C", "missing", "."],
        vec!["tar", "-xf", "a.tar", "t"],
    ] {
        assert!(!run(&d, &args).status.success(), "{args:?}");
    }
    for (args, reason) in [
        (vec!["tar", "-cf", "a.tar", "/etc"], "relative"),
        (vec!["tar", "-cf", "a.tar", "t/../t"], "relative"),
        (
            vec!["tar", "-cf", "a.tar", "t", "-C", "t"],
            "before the PATHs",
        ),
        (vec!["tar", "-cf", "a.tar"], "empty archive"),
    ] {
        let result = run(&d, &args);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(reason), "{args:?}: {stderr}");
    }
    // An error is 2, which a changed file's 1 is not.
    assert_eq!(
        run(&d, &["tar", "-cf", "a.tar", "missing"]).status.code(),
        Some(2)
    );
    // A PATH through a symlink, which tar -x would refuse.
    symlink("t", d.join("lt")).unwrap();
    fs::write(d.join("t/a"), b"a").unwrap();
    let result = run(&d, &["tar", "-cf", "a.tar", "lt", "lt/a"]);
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("through this symlink"),
        "{result:?}"
    );
    fs::remove_file(d.join("t/a")).unwrap();
    // A name tar -x would refuse is refused here.
    use std::os::unix::ffi::OsStrExt;
    fs::write(d.join("t").join(std::ffi::OsStr::from_bytes(b"\xff")), b"").unwrap();
    let result = run(&d, &["tar", "-cf", "a.tar", "t"]);
    assert!(!result.status.success(), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("not UTF-8"));
}
