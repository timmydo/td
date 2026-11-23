#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use td_fs::TempDir;
use td_mta::{
    operator_files,
    postgres_input::{Error, Loaded},
};

fn write(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
#[ignore = "host qualification: TMPDIR must have trusted, non-shared-writable ancestors"]
fn protected_files_enforce_modes_and_refuse_links() {
    let root = TempDir::new("pg-input").unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let connection = root.join("connection");
    let uri = b"postgresql://user:secret@/mail?host=/run/postgresql";
    write(&connection, uri, 0o600);
    let path = connection.to_str().unwrap();
    let loaded = Loaded::open(path, "desktop").unwrap();
    assert_eq!(loaded.bootstrap.profile(), "desktop");
    assert_eq!(
        loaded.bootstrap.socket().as_path(),
        Path::new("/run/postgresql/.s.PGSQL.5432")
    );
    for mode in [0o644, 0o640, 0o660, 0o700] {
        fs::set_permissions(&connection, fs::Permissions::from_mode(mode)).unwrap();
        assert!(Loaded::open(path, "desktop").is_err());
    }
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(Loaded::open(path, "desktop").is_ok());
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.join("linked");
    std::os::unix::fs::symlink(&connection, &link).unwrap();
    assert_eq!(
        Loaded::open(link.to_str().unwrap(), "desktop").err(),
        Some(Error::Input(operator_files::Error::Type))
    );

    write(&connection, &vec![b'x'; 16 * 1024 + 3], 0o600);
    assert_eq!(Loaded::open(path, "desktop").err(), Some(Error::Capacity));
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(Loaded::open(path, "desktop").err(), Some(Error::Root));
}
