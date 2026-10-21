//! Make filesystems from real trees, then read them back with td-btrfs's own
//! check, list and restore.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::fs;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use td_btrfs::check::check;
use td_btrfs::export::Contents;
use td_btrfs::format::{parse_uuid, MIB, NODESIZE};
use td_btrfs::mkfs::{make, Options};
use td_btrfs::read::Volume;
use td_btrfs::tree::seal;
use td_fs::TempDir;

const UUID: &str = "12345678-1234-4234-8234-123456789abc";

fn options(root: Option<&Path>, subvols: &[&str], target: &Path) -> Options {
    Options {
        byte_count: None,
        uuid: Some(parse_uuid(UUID).unwrap()),
        label: b"td-system".to_vec(),
        rootdir: root.map(Path::to_path_buf),
        subvols: subvols.iter().map(PathBuf::from).collect(),
        target: target.to_path_buf(),
    }
}

fn image(dir: &Path, len: u64) -> PathBuf {
    let path = dir.join("volume.img");
    fs::File::create(&path).unwrap().set_len(len).unwrap();
    path
}

fn mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn listing(path: &Path) -> String {
    let mut out = Vec::new();
    Contents::read(Volume::open(path).unwrap())
        .unwrap()
        .list(&mut out)
        .unwrap();
    String::from_utf8(out).unwrap()
}

fn assert_clean(path: &Path) {
    let report = check(path).unwrap();
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
}

/// Every file under `a` equals the one under `b`, and the trees hold the
/// same names.
fn same_tree(a: &Path, b: &Path) {
    let mut names_a: Vec<_> = fs::read_dir(a)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut names_b: Vec<_> = fs::read_dir(b)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names_a.sort();
    names_b.sort();
    assert_eq!(names_a, names_b, "{}", a.display());
    for name in names_a {
        let (x, y) = (a.join(&name), b.join(&name));
        let meta = fs::symlink_metadata(&x).unwrap();
        if meta.file_type().is_symlink() {
            assert_eq!(fs::read_link(&x).unwrap(), fs::read_link(&y).unwrap());
        } else if meta.is_dir() {
            same_tree(&x, &y);
        } else {
            assert_eq!(
                fs::read(&x).unwrap(),
                fs::read(&y).unwrap(),
                "{}",
                x.display()
            );
        }
    }
}

#[test]
fn a_populated_tree_round_trips_through_check_list_and_restore() {
    let dir = TempDir::new("td-btrfs-round-trip").unwrap();
    let seed = dir.join("seed");
    for sub in [
        "td/boot",
        "td/deployments/abc",
        "td/incoming",
        "@var/lib/td",
        "@var/nested/deep",
        "big",
    ] {
        fs::create_dir_all(seed.join(sub)).unwrap();
    }
    // Sizes either side of the inline limit and of a sector.
    for size in [0usize, 1, 2047, 2048, 2049, 4095, 4096, 4097, 300_000] {
        let bytes: Vec<u8> = (0..size).map(|n| (n * 7 + size) as u8).collect();
        fs::write(seed.join(format!("big/f{size}")), bytes).unwrap();
    }
    for n in 0..3000 {
        fs::write(
            seed.join(format!("td/deployments/abc/file{n:04}")),
            format!("{n}\n"),
        )
        .unwrap();
    }
    fs::write(seed.join("@var/lib/td/hostname"), "my-td\n").unwrap();
    fs::write(seed.join("@var/nested/deep/x"), "x\n").unwrap();
    fs::hard_link(seed.join("big/f4097"), seed.join("big/link-a")).unwrap();
    fs::hard_link(seed.join("big/f4097"), seed.join("td/link-b")).unwrap();
    // A link into another subvolume is a separate inode.
    fs::hard_link(seed.join("big/f2048"), seed.join("@var/lib/td/linked")).unwrap();
    std::os::unix::fs::symlink("../deployments/abc", seed.join("td/boot/current")).unwrap();
    mode(&seed.join("td/incoming"), 0o700);
    mode(&seed.join("big/f1"), 0o4755);
    mode(&seed, 0o755);

    let path = image(&dir, 512 * MIB);
    let made = make(&options(Some(&seed), &["@var", "@var/nested"], &path)).unwrap();
    assert_eq!(made.total, 512 * MIB);
    assert_clean(&path);

    let list = listing(&path);
    let line = |path: &str| {
        list.lines()
            .find(|line| line.split(' ').nth(5) == Some(path))
            .unwrap_or_else(|| panic!("{path} missing from\n{list}"))
            .to_string()
    };
    let (uid, gid) = {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&seed).unwrap();
        (meta.uid(), meta.gid())
    };
    assert_eq!(
        line("/"),
        format!("d 0755 {uid} {gid} 18 / [subvolume 5 rw]")
    );
    assert!(line("/@var").ends_with("[subvolume 256 rw]"), "{list}");
    assert!(
        line("/@var/nested").ends_with("[subvolume 257 rw]"),
        "{list}"
    );
    assert_eq!(
        line("/td/incoming"),
        format!("d 0700 {uid} {gid} 0 /td/incoming")
    );
    assert_eq!(line("/big/f1"), format!("f 4755 {uid} {gid} 1 /big/f1"));
    assert_eq!(
        line("/td/boot/current"),
        format!("l 0777 {uid} {gid} 18 /td/boot/current -> ../deployments/abc")
    );
    // /, @var with lib, td, hostname, linked, nested, deep, x; big with
    // nine files and a link; td, boot, current, deployments, abc and its
    // files, incoming, link-b.
    assert_eq!(list.lines().count(), 1 + 8 + 11 + 3 + 2 + 3000 + 2);

    let restored = dir.join("restored");
    fs::create_dir(&restored).unwrap();
    Contents::read(Volume::open(&path).unwrap())
        .unwrap()
        .restore(&restored)
        .unwrap();
    same_tree(&seed, &restored);
}

#[test]
fn the_superblock_is_one_td_boot_admits() {
    let dir = TempDir::new("td-btrfs-super").unwrap();
    let seed = dir.join("seed");
    fs::create_dir_all(seed.join("@var")).unwrap();
    let path = image(&dir, 256 * MIB);
    make(&options(Some(&seed), &["@var"], &path)).unwrap();
    let file = fs::File::open(&path).unwrap();
    for offset in [65536u64, 64 * MIB] {
        let mut raw = vec![0u8; 4096];
        file.read_exact_at(&mut raw, offset).unwrap();
        assert_eq!(&raw[64..72], b"_BHRfS_M");
        assert_eq!(&raw[32..48], &parse_uuid(UUID).unwrap());
        assert_eq!(u64::from_le_bytes(raw[48..56].try_into().unwrap()), offset);
        assert_eq!(
            u64::from_le_bytes(raw[56..64].try_into().unwrap()),
            1,
            "flags"
        );
        assert_eq!(
            u64::from_le_bytes(raw[136..144].try_into().unwrap()),
            1,
            "devices"
        );
        assert_eq!(
            u16::from_le_bytes(raw[196..198].try_into().unwrap()),
            0,
            "crc32c"
        );
        assert_eq!(&raw[299..309], b"td-system\0");
        let sum = td_btrfs::crc32c::checksum(&raw[32..]);
        assert_eq!(u32::from_le_bytes(raw[0..4].try_into().unwrap()), sum);
    }
}

#[test]
fn a_regular_file_grows_to_the_byte_count_and_a_longer_one_keeps_its_length() {
    let dir = TempDir::new("td-btrfs-size").unwrap();
    let path = image(&dir, 0);
    let mut grow = options(None, &[], &path);
    grow.byte_count = Some(200 * MIB);
    make(&grow).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), 200 * MIB);
    assert_clean(&path);
    let longer = image(&dir, 600 * MIB);
    let mut within = options(None, &[], &longer);
    within.byte_count = Some(300 * MIB);
    let made = make(&within).unwrap();
    assert_eq!(
        (made.total, fs::metadata(&longer).unwrap().len()),
        (300 * MIB, 600 * MIB)
    );
    assert_clean(&longer);
    assert_eq!(listing(&longer), "d 0755 0 0 0 / [subvolume 5 rw]\n");
}

#[test]
fn what_cannot_be_copied_or_placed_is_refused() {
    let dir = TempDir::new("td-btrfs-refuse").unwrap();
    let seed = dir.join("seed");
    fs::create_dir_all(seed.join("@var")).unwrap();
    fs::write(seed.join("file"), "x").unwrap();
    let path = image(&dir, 256 * MIB);
    let refused = |options: Options, needle: &str| {
        let error = make(&options).unwrap_err().to_string();
        assert!(error.contains(needle), "{error:?} lacks {needle:?}");
    };
    refused(
        options(Some(&seed), &["file"], &path),
        "must be a directory",
    );
    refused(
        options(Some(&seed), &["../seed"], &path),
        "plain relative path",
    );
    refused(options(Some(&seed), &["missing"], &path), "not in the root");
    refused(options(None, &["@var"], &path), "--subvol needs --rootdir");
    refused(options(Some(&seed), &[], &dir.join("absent")), "absent");
    refused(
        options(Some(&seed), &[], &seed),
        "neither a block device nor a regular file",
    );
    let small = dir.join("small.img");
    fs::File::create(&small).unwrap().set_len(40 * MIB).unwrap();
    refused(options(Some(&seed), &[], &small), "too small");
    // The target inside the tree it copies.
    let inside = seed.join("inside.img");
    fs::File::create(&inside)
        .unwrap()
        .set_len(256 * MIB)
        .unwrap();
    refused(options(Some(&seed), &[], &inside), "the target itself");
    fs::remove_file(&inside).unwrap();
    // A socket is neither a directory, a regular file nor a symlink.
    let socket = seed.join("sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    refused(
        options(Some(&seed), &[], &path),
        "a socket cannot be copied",
    );
    // A refusal is known before the target is touched: a short regular
    // file is not grown for a tree that cannot be copied.
    let short = image(&dir, 0);
    let mut grow = options(Some(&seed), &[], &short);
    grow.byte_count = Some(256 * MIB);
    refused(grow, "a socket cannot be copied");
    assert_eq!(fs::metadata(&short).unwrap().len(), 0);
}

/// Replace every occurrence of `from` in the image with `to`, and reseal
/// each tree block that held one.
fn rewrite_metadata(path: &Path, from: &[u8], to: &[u8]) {
    let mut bytes = fs::read(path).unwrap();
    let node = NODESIZE as usize;
    let mut blocks = std::collections::BTreeSet::new();
    let mut at = 0;
    while let Some(found) = bytes[at..].windows(from.len()).position(|w| w == from) {
        let start = at + found;
        bytes[start..start + from.len()].copy_from_slice(to);
        blocks.insert(start / node * node);
        at = start + from.len();
    }
    assert!(!blocks.is_empty());
    for block in blocks {
        seal(&mut bytes[block..block + node]).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_system_chunk_whose_length_overflows_is_refused() {
    let dir = TempDir::new("td-btrfs-overflow").unwrap();
    let path = image(&dir, 128 * MIB);
    make(&options(None, &[], &path)).unwrap();
    // The first system chunk's key offset (its logical address), 9 bytes
    // into the sys_chunk_array at 811, moved to the top of the address
    // space with the chunk root (at 88) inside it, so the chunk tree's
    // lookup reaches the chunk's end; the primary superblock resealed.
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut sb = vec![0u8; 4096];
    file.read_exact_at(&mut sb, 65536).unwrap();
    let top = 0xffff_ffff_ffff_f000u64.to_le_bytes();
    sb[811 + 9..811 + 17].copy_from_slice(&top);
    sb[88..96].copy_from_slice(&top);
    let sum = td_btrfs::crc32c::checksum(&sb[32..]);
    sb[..4].copy_from_slice(&sum.to_le_bytes());
    file.write_all_at(&sb, 65536).unwrap();
    let error = check(&path).unwrap_err().to_string();
    assert!(error.contains("impossible length"), "{error}");
}

#[test]
fn a_name_that_is_not_a_path_component_is_refused_by_every_reader() {
    let dir = TempDir::new("td-btrfs-names").unwrap();
    let seed = dir.join("seed");
    fs::create_dir_all(seed.join("sub")).unwrap();
    fs::write(seed.join("sub/evil-name"), "x").unwrap();
    let path = image(&dir, 128 * MIB);
    make(&options(Some(&seed), &[], &path)).unwrap();
    assert_clean(&path);
    rewrite_metadata(&path, b"evil-name", b"evil/name");
    let refusal = "not a path component";
    let error = check(&path)
        .map(|r| r.errors.join("; "))
        .unwrap_err()
        .to_string();
    assert!(error.contains(refusal), "{error}");
    let error = Contents::read(Volume::open(&path).unwrap())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(refusal), "{error}");
    // Nothing is written, inside the destination or beside it.
    let dest = dir.join("restored");
    fs::create_dir(&dest).unwrap();
    let restore = Command::new(env!("CARGO_BIN_EXE_td-btrfs"))
        .arg("restore")
        .arg(&path)
        .arg(&dest)
        .output()
        .unwrap();
    assert!(!restore.status.success());
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    assert!(!dir.join("evil").exists() && !dest.join("sub").exists());
}

#[test]
fn check_finds_damaged_data_and_metadata() {
    let dir = TempDir::new("td-btrfs-damage").unwrap();
    let seed = dir.join("seed");
    fs::create_dir_all(&seed).unwrap();
    fs::write(seed.join("data"), vec![7u8; 100_000]).unwrap();
    let path = image(&dir, 256 * MIB);
    make(&options(Some(&seed), &[], &path)).unwrap();
    assert_clean(&path);
    let volume = Volume::open(&path).unwrap();
    let data = volume
        .chunks
        .iter()
        .find(|c| c.flags & 1 != 0)
        .unwrap()
        .stripes[0]
        .offset;
    let metadata = volume
        .chunks
        .iter()
        .find(|c| c.flags & 4 != 0)
        .unwrap()
        .clone();
    drop(volume);
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all_at(b"X", data + 5).unwrap();
    let report = check(&path).unwrap();
    assert_eq!(report.errors.len(), 1, "{:#?}", report.errors);
    assert!(report.errors[0].contains("fails its checksum"));
    file.write_all_at(&[7], data + 5).unwrap();
    assert_clean(&path);
    // The second copy of the first metadata block: the root tree's.
    file.write_all_at(b"X", metadata.stripes[1].offset + 500)
        .unwrap();
    let error = check(&path).unwrap_err().to_string();
    assert!(error.contains("copy 1 fails its checksum"), "{error}");
}

#[test]
fn mkfs_btrfs_by_name_takes_the_btrfs_progs_arguments_td_uses() {
    use std::os::unix::process::CommandExt;
    let dir = TempDir::new("td-btrfs-cli").unwrap();
    let seed = dir.join("seed");
    fs::create_dir_all(seed.join("@var")).unwrap();
    let path = image(&dir, 0);
    let mkfs = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_td-btrfs"))
            .arg0("/bin/mkfs.btrfs")
            .args(args)
            .output()
            .unwrap()
    };
    let target = path.to_str().unwrap();
    let rootdir = seed.to_str().unwrap();
    let made = mkfs(&[
        "--byte-count",
        "256M",
        "--uuid",
        UUID,
        "--label",
        "td-system",
        "--rootdir",
        rootdir,
        "--subvol",
        "rw:@var",
        target,
    ]);
    assert!(made.status.success(), "{made:?}");
    let banner = String::from_utf8(made.stdout).unwrap();
    assert!(
        banner.contains(&format!("UUID:               {UUID}")),
        "{banner}"
    );
    let quiet = mkfs(&[
        "-q",
        "-U",
        UUID,
        "-L",
        "td-system",
        "-r",
        rootdir,
        "--subvol",
        "@var",
        target,
    ]);
    assert!(
        quiet.status.success() && quiet.stdout.is_empty(),
        "{quiet:?}"
    );
    for bad in [
        &["--subvol", "ro:@var", "-r", rootdir, target][..],
        &["--uuid", "00000000-0000-0000-0000-000000000000", target],
        &["--byte-count", "1.5G", target],
        &["--force", target],
        &[target, target],
        &[],
    ] {
        let out = mkfs(bad);
        assert!(!out.status.success(), "{bad:?}");
        assert!(
            String::from_utf8(out.stderr)
                .unwrap()
                .starts_with("ERROR: "),
            "{bad:?}"
        );
    }
    let checked = Command::new(env!("CARGO_BIN_EXE_td-btrfs"))
        .args(["check", target])
        .output()
        .unwrap();
    assert!(checked.status.success(), "{checked:?}");
    assert!(String::from_utf8(checked.stdout)
        .unwrap()
        .ends_with("no errors found\n"));
}
