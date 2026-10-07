#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

use std::{
    fs::{self},
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
        Directory::from_path(self.0.to_str().unwrap()).unwrap()
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
fn retained_metadata_and_path_lookup_have_distinct_lifetimes() {
    let fixture = Fixture::new();
    let original = fixture.0.join("root");
    let moved = fixture.0.join("moved");
    let name = Name::account(AccountId::from_bytes([7; 16]), AccountEntry::Temporary).unwrap();
    fs::create_dir_all(original.join(name.as_path().unwrap())).unwrap();
    let root = Directory::from_path(original.to_str().unwrap()).unwrap();
    let before = root.open(&name).unwrap();
    let expected = identity(before.metadata().unwrap());
    fs::rename(&original, &moved).unwrap();
    fs::create_dir_all(original.join(name.as_path().unwrap())).unwrap();
    let after = root.open(&name).unwrap();
    assert_ne!(identity(after.metadata().unwrap()), expected);
    assert_ne!(
        identity(fs::metadata(original.join(name.as_path().unwrap())).unwrap()),
        expected
    );
    assert_eq!(identity(before.metadata().unwrap()), expected);
}

#[test]
fn reject_existing_final_and_intermediate_links() {
    let fixture = Fixture::new();
    let root = fixture.anchor();
    let account = AccountId::from_bytes([8; 16]);
    let nested = Name::account(account, AccountEntry::Temporary).unwrap();
    let accounts = Name::root(RootEntry::Accounts).unwrap();
    fs::create_dir_all(fixture.0.join(format!("real/{account}/tmp"))).unwrap();
    symlink("real", fixture.0.join("accounts")).unwrap();
    for name in [&accounts, &nested] {
        assert_eq!(
            root.open(name).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
    }
    fs::remove_file(fixture.0.join("accounts")).unwrap();
    symlink("/", fixture.0.join("accounts")).unwrap();
    assert_eq!(
        root.open(&accounts).unwrap_err().kind(),
        io::ErrorKind::NotADirectory
    );
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
        Directory::from_path(path.to_str().unwrap())
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

#[test]
fn private_root_checks_modes_ancestry_and_lexical_bounds() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use td_mta::store_fs::{PrivateRoot, RootError, MAX_PATH_BYTES, MAX_ROOT_BYTES};
    let parent = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(parent).unwrap();
    let base = parent.join(format!("private-root-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
    let fixture = Fixture(base);
    let child = fixture.0.join("data");
    fs::DirBuilder::new().mode(0o700).create(&child).unwrap();
    let path = child.to_str().unwrap();
    let owner = fs::metadata(&child).unwrap().uid();
    if owner == 0 {
        assert!(matches!(PrivateRoot::open(path), Err(RootError::Owner)));
    } else {
        let trusted = child.ancestors().skip(1).all(|path| {
            fs::symlink_metadata(path).is_ok_and(|m| {
                m.is_dir() && (m.uid() == 0 || m.uid() == owner) && m.mode() & 0o022 == 0
            })
        });
        if trusted {
            let root = PrivateRoot::open(path).unwrap();
            assert_eq!(root.directory().metadata().unwrap().uid(), owner);
            let locked = root.try_lock().unwrap();
            assert_eq!(locked.root().directory().metadata().unwrap().uid(), owner);
            assert!(matches!(
                PrivateRoot::open(path).unwrap().try_lock(),
                Err(td_mta::store_fs::LockError::Busy)
            ));
            drop(locked);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match PrivateRoot::open(path).unwrap().try_lock() {
                    Err(td_mta::store_fs::LockError::Busy)
                        if std::time::Instant::now() < deadline =>
                    {
                        // A parallel test's child can retain LOCK until exec.
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    result => {
                        drop(result.unwrap());
                        break;
                    }
                }
            }
        } else {
            eprintln!("positive private-root case unavailable: untrusted fixture ancestry");
            assert!(PrivateRoot::open(path).is_err());
        }
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(matches!(
            PrivateRoot::open(path),
            Err(RootError::WritableAncestor)
        ));
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let link = fixture.0.join("link");
    symlink(&child, &link).unwrap();
    assert!(matches!(PrivateRoot::open(link.to_str().unwrap()),
        Err(RootError::Io(error)) if error.kind() == io::ErrorKind::NotADirectory));
    let through_link = link.join("nested");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(child.join("nested"))
        .unwrap();
    if owner != 0 {
        assert!(matches!(PrivateRoot::open(through_link.to_str().unwrap()),
            Err(RootError::Io(error)) if error.kind() == io::ErrorKind::NotADirectory));
    }
    for mode in [0o750, 0o755, 0o1700, 0o2700, 0o4700] {
        fs::set_permissions(&child, fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(
            PrivateRoot::open(path),
            Err(RootError::PrivateMode)
        ));
    }
    for path in [
        "",
        "relative",
        "/tmp/../root",
        "/tmp/./root",
        "//tmp",
        "/tmp/",
        "/bad\0name",
    ] {
        assert!(matches!(PrivateRoot::open(path), Err(RootError::Path)));
        assert_eq!(
            Directory::from_path(path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(matches!(
        PrivateRoot::open(&format!("/{}", "x".repeat(MAX_ROOT_BYTES))),
        Err(RootError::Path)
    ));
    assert_eq!(
        Directory::from_path(&format!("/{}", "x".repeat(MAX_PATH_BYTES)))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}
