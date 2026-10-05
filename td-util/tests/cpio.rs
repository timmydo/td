//! `td-util cpio`, run as a process against an archive the kernel's own
//! format produces: list, extract with directories, the traversal refusals,
//! hard-link groups and late directory modes.
#![allow(clippy::unwrap_used)]

use std::process::Command;

fn header(out: &mut Vec<u8>, mode: u32, size: usize, name: &str) {
    header_at(out, 1, mode, 1, 0, size, name);
}

fn header_at(
    out: &mut Vec<u8>,
    ino: u32,
    mode: u32,
    nlink: u32,
    mtime: u32,
    size: usize,
    name: &str,
) {
    out.extend_from_slice(b"070701");
    let fields = [
        ino,
        mode,
        0,
        0,
        nlink,
        mtime,
        size as u32,
        0,
        0,
        0,
        0,
        name.len() as u32 + 1,
        0,
    ];
    for f in fields {
        out.extend_from_slice(format!("{f:08X}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn archive(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, mode, data) in entries {
        header(&mut out, *mode, data.len(), name);
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    header(&mut out, 0, 0, "TRAILER!!!");
    out
}

#[test]
fn lists_and_extracts_newc() {
    let d = std::env::temp_dir().join(format!("td-util-cpio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let a = archive(&[
        ("etc", 0o040755, b""),
        ("etc/motd", 0o100644, b"hello\n"),
        ("bin/sh", 0o120777, b"td-sh"),
        ("../escape", 0o100644, b"x"),
    ]);
    std::fs::write(d.join("a.cpio"), &a).unwrap();
    let tdu = env!("CARGO_BIN_EXE_td-util");
    let out = Command::new(tdu)
        .args(["cpio", "-t", "-F", "a.cpio"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "etc\netc/motd\nbin/sh\n../escape\n"
    );
    let out = Command::new(tdu)
        .args(["cpio", "-t", "-F", "a.cpio", "etc/*"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "etc/motd\n");
    std::fs::create_dir_all(d.join("x")).unwrap();
    let st = Command::new(tdu)
        .args(["cpio", "-id", "-F", "../a.cpio"])
        .current_dir(d.join("x"))
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(1), "the ../ member is refused and reported");
    assert_eq!(std::fs::read(d.join("x/etc/motd")).unwrap(), b"hello\n");
    assert_eq!(
        std::fs::read_link(d.join("x/bin/sh")).unwrap().to_str(),
        Some("td-sh")
    );
    assert!(!d.join("escape").exists());
    std::fs::remove_dir_all(&d).unwrap();
}

/// (ino, mode, nlink, mtime, name, data) members, then the trailer.
fn archive_at(entries: &[(u32, u32, u32, u32, &str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (ino, mode, nlink, mtime, name, data) in entries {
        header_at(&mut out, *ino, *mode, *nlink, *mtime, data.len(), name);
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    header(&mut out, 0, 0, "TRAILER!!!");
    out
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("td-util-cpio-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("x")).unwrap();
    d
}

fn extract(d: &std::path::Path, archive: &[u8]) -> Option<i32> {
    std::fs::write(d.join("a.cpio"), archive).unwrap();
    Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(["cpio", "-id", "-F", "../a.cpio"])
        .current_dir(d.join("x"))
        .status()
        .unwrap()
        .code()
}

#[test]
fn a_planted_link_is_not_written_through() {
    let d = scratch("link");
    std::fs::create_dir_all(d.join("victim")).unwrap();
    let a = archive(&[
        ("l", 0o120777, b"../victim"),
        ("l/pwned", 0o100644, b"x"),
        ("l", 0o040555, b""),
    ]);
    assert_eq!(extract(&d, &a), Some(1));
    assert!(!d.join("victim/pwned").exists());
    let mode = std::fs::metadata(d.join("victim")).unwrap().permissions();
    assert_ne!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o555
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn hard_link_groups_share_the_last_members_data() {
    let d = scratch("hl");
    let a = archive_at(&[
        (7, 0o100644, 2, 0, "f", b""),
        (7, 0o100644, 2, 0, "g", b"DATA"),
        (9, 0o100600, 2, 0, "e1", b""),
        (9, 0o100600, 2, 0, "e2", b""),
    ]);
    assert_eq!(extract(&d, &a), Some(0));
    let x = d.join("x");
    assert_eq!(std::fs::read(x.join("f")).unwrap(), b"DATA");
    let ino = |n: &str| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(x.join(n)).unwrap());
    assert_eq!(ino("f"), ino("g"));
    assert_eq!(ino("e1"), ino("e2"));
    assert_eq!(std::fs::read(x.join("e2")).unwrap(), b"");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn directory_modes_and_times_land_last() {
    let d = scratch("dir");
    let a = archive_at(&[
        (1, 0o040555, 1, 1_000_000_000, "ro", b""),
        (2, 0o100644, 1, 0, "ro/f", b"f"),
    ]);
    assert_eq!(extract(&d, &a), Some(0));
    let ro = d.join("x/ro");
    let meta = std::fs::metadata(&ro).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
        0o555
    );
    assert_eq!(std::os::unix::fs::MetadataExt::mtime(&meta), 1_000_000_000);
    std::fs::set_permissions(&ro, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_pattern_that_skips_the_data_carrier_still_fills_its_links() {
    let d = scratch("skip");
    let a = archive_at(&[
        (7, 0o100644, 3, 0, "f", b""),
        (7, 0o100644, 3, 0, "g", b""),
        (7, 0o100644, 3, 0, "h", b"DATA"),
    ]);
    std::fs::write(d.join("a.cpio"), &a).unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(["cpio", "-id", "-F", "../a.cpio", "[fg]"])
        .current_dir(d.join("x"))
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(0));
    assert_eq!(std::fs::read(d.join("x/f")).unwrap(), b"DATA");
    assert_eq!(std::fs::read(d.join("x/g")).unwrap(), b"DATA");
    assert!(!d.join("x/h").exists());
    std::fs::remove_dir_all(&d).unwrap();
}
