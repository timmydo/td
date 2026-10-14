#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use td_crypto::{Crypto, Provider, P256_PKCS8_CAPACITY};
use td_fs::TempDir;
use td_mta::{
    operator_files,
    postgres_input::{Error, Loaded},
};

#[path = "support/tls_certificate_fixture.rs"]
mod certificate;

fn write(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
#[ignore = "host qualification: TMPDIR must have trusted, non-shared-writable ancestors"]
fn protected_files_enforce_modes_aliases_and_explicit_trust() {
    let root = TempDir::new("pg-input").unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let connection = root.join("connection");
    let ca = root.join("ca.pem");
    let uri = b"postgresql://user:secret@localhost/mail";
    write(&connection, uri, 0o600);
    let path = connection.to_str().unwrap();
    let loaded = Loaded::open(path, "desktop").unwrap();
    assert!(loaded.trust.uses_public_roots());
    assert_eq!(loaded.bootstrap.profile(), "desktop");
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

    let explicit = format!(
        "{}?sslrootcert={}",
        std::str::from_utf8(uri).unwrap(),
        ca.display()
    );
    write(&connection, explicit.as_bytes(), 0o600);
    fs::hard_link(&connection, &ca).unwrap();
    assert_eq!(
        Loaded::open(path, "desktop").err(),
        Some(Error::Input(operator_files::Error::Aliased))
    );
    fs::remove_file(&ca).unwrap();
    write(&ca, b"not a certificate", 0o644);
    assert_eq!(Loaded::open(path, "desktop").err(), Some(Error::Trust));

    let provider = Provider;
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let count = provider.generate_p256(&mut raw).unwrap();
    let key = provider.load_p256(&raw[..count]).unwrap();
    let pem = certificate::pem(
        "CERTIFICATE",
        &certificate::certificate_names(&key, &key, true, 1, &[], false),
    );
    write(&ca, &pem, 0o644);
    let loaded = Loaded::open(path, "desktop").unwrap();
    assert!(!loaded.trust.uses_public_roots());
    assert_eq!(loaded.trust.anchor_count(), 1);
    fs::set_permissions(&ca, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        Loaded::open(path, "desktop").err(),
        Some(Error::Input(operator_files::Error::Writable))
    );
    fs::remove_file(&ca).unwrap();
    assert!(Loaded::open(path, "desktop").is_err());
    write(&connection, &vec![b'x'; 16 * 1024 + 3], 0o600);
    assert_eq!(Loaded::open(path, "desktop").err(), Some(Error::Capacity));
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(Loaded::open(path, "desktop").err(), Some(Error::Root));
}
