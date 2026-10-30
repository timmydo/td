#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
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
