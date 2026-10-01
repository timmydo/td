#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

use std::{
    fs::{self, File},
    io,
    os::unix::{fs::symlink, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use td_mta::{
    ids::AccountId,
    store_fs::Directory,
    store_paths::{AccountEntry, Name, RootEntry},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-mta-dir-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn anchor(&self) -> Directory {
        Directory::from_file(File::open(&self.0).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn identity(metadata: fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}
fn descriptors_for(path: &Path) -> Vec<PathBuf> {
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|entry| fs::read_link(entry).is_ok_and(|target| target == path))
        .collect()
}

#[test]
fn generated_nested_paths_and_pinned_parent_survive_path_replacement() {
    let fixture = Fixture::new();
    let original = fixture.0.join("root");
    let moved = fixture.0.join("moved");
    let name = Name::account(AccountId::from_bytes([7; 16]), AccountEntry::Metadata).unwrap();
    fs::create_dir_all(original.join(name.as_path().unwrap())).unwrap();
    let root = Directory::from_file(File::open(&original).unwrap()).unwrap();
    let before = root.open(&name).unwrap();
    let expected = identity(before.metadata().unwrap());
    fs::rename(&original, &moved).unwrap();
    fs::create_dir_all(original.join(name.as_path().unwrap())).unwrap();
    let after = root.open(&name).unwrap();
    assert_eq!(identity(after.metadata().unwrap()), expected);
    assert_ne!(
        identity(fs::metadata(original.join(name.as_path().unwrap())).unwrap()),
        expected
    );
    assert_eq!(identity(before.metadata().unwrap()), expected);
}

#[test]
fn reject_final_and_intermediate_links_even_when_they_stay_beneath_anchor() {
    let fixture = Fixture::new();
    let root = fixture.anchor();
    let account = AccountId::from_bytes([8; 16]);
    let nested = Name::account(account, AccountEntry::Metadata).unwrap();
    let accounts = Name::root(RootEntry::Accounts).unwrap();
    fs::create_dir_all(fixture.0.join(format!("real/{account}/metadata"))).unwrap();
    symlink("real", fixture.0.join("accounts")).unwrap();
    for name in [&accounts, &nested] {
        assert_eq!(root.open(name).unwrap_err().raw_os_error(), Some(40));
    }
    fs::remove_file(fixture.0.join("accounts")).unwrap();
    symlink("/", fixture.0.join("accounts")).unwrap();
    assert_eq!(root.open(&accounts).unwrap_err().raw_os_error(), Some(40));
}

#[test]
fn refuse_regular_files_missing_entries_and_nondirectory_anchors() {
    let fixture = Fixture::new();
    let root = fixture.anchor();
    let name = Name::root(RootEntry::Accounts).unwrap();
    assert_eq!(
        root.open(&name).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let path = fixture.0.join("accounts");
    fs::write(&path, b"unchanged").unwrap();
    assert_eq!(
        root.open(&name).unwrap_err().kind(),
        io::ErrorKind::NotADirectory
    );
    assert_eq!(
        Directory::from_file(File::open(&path).unwrap())
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotADirectory
    );
    assert!(descriptors_for(&fs::canonicalize(&path).unwrap()).is_empty());
    assert_eq!(fs::read(path).unwrap(), b"unchanged");
}

#[test]
fn descriptor_ownership_closes_on_drop_and_does_not_cross_exec() {
    const CHILD_PATH: &str = "TD_MTA_DIRECTORY_EXEC_TEST";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        assert!(descriptors_for(Path::new(&path)).is_empty());
        return;
    }
    let fixture = Fixture::new();
    let path = fixture.0.join("accounts");
    fs::create_dir(&path).unwrap();
    let path = fs::canonicalize(path).unwrap();
    let root = fixture.anchor();
    let name = Name::root(RootEntry::Accounts).unwrap();
    let child = root.open(&name).unwrap();
    assert_eq!(descriptors_for(&path).len(), 1);
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "descriptor_ownership_closes_on_drop_and_does_not_cross_exec",
        ])
        .env(CHILD_PATH, &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "{stdout}"
    );
    assert_eq!(descriptors_for(&path).len(), 1);
    drop(child);
    assert!(descriptors_for(&path).is_empty());
}
