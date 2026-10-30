//! Keep existing portal stores isolated and locked at boot.

use super::{secret_store, Failure};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

fn store_parent(state: &Path) -> Result<File, Failure> {
    let parent = state.join("secrets");
    match fs::DirBuilder::new().mode(0o755).create(&parent) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(Failure::Failed(format!("create secret-store parent: {e}"))),
    }
    let parent_directory = super::open_directory(&parent)
        .map_err(|e| Failure::Failed(format!("open secret-store parent: {e}")))?;
    let state_directory =
        super::open_directory(state).map_err(|e| Failure::Failed(e.to_string()))?;
    if parent_directory
        .metadata()
        .map_err(|e| Failure::Failed(e.to_string()))?
        .uid()
        != state_directory
            .metadata()
            .map_err(|e| Failure::Failed(e.to_string()))?
            .uid()
    {
        return Err(Failure::Failed(
            "secret-store parent has the wrong owner".into(),
        ));
    }
    parent_directory
        .set_permissions(fs::Permissions::from_mode(0o755))
        .and_then(|()| parent_directory.sync_all())
        .map_err(|e| Failure::Failed(e.to_string()))?;
    File::open(state)
        .and_then(|file| file.sync_all())
        .map_err(|e| Failure::Failed(e.to_string()))?;
    Ok(parent_directory)
}

fn prepare_missing_store(state: &Path, uid: u32, owner: u32) -> Result<(), Failure> {
    let path = state.join("secrets").join(uid.to_string());
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            secret_store::Store::open_owned(&path, uid, owner, true)
                .and_then(|store| store.prepare_boot())
                .map_err(Failure::Failed)
        }
        Err(error) => Err(Failure::Failed(format!(
            "inspect credential store: {error}"
        ))),
    }
}

/// Protect every existing session store even when its application home is broken.
pub(super) fn isolate_stores(
    state: &Path,
    registry: &crate::principals::Registry,
) -> Result<(), Failure> {
    let parent = store_parent(state)?;
    for session in registry.sessions() {
        if let Err(error) = secret_store::migrate_owner(&parent, session.owner, session.portal) {
            let disposition = if error.quarantined {
                "store quarantined; credentials unavailable"
            } else {
                "isolation not confirmed; existing filesystem access may remain"
            };
            super::emit_err(&format!(
                "td-firstboot: credential migration for uid {} refused ({disposition}): {}\n",
                session.owner, error.message,
            ));
            continue;
        }
        prepare_missing_store(state, session.owner, session.portal)?;
        let name = session.owner.to_string();
        let held_path =
            std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(&name);
        match fs::symlink_metadata(held_path) {
            Ok(_) => {
                if let Err(error) = secret_store::lock_session(session.owner) {
                    super::emit_err(&format!(
                        "td-firstboot: credential lock for uid {} not confirmed: {error}\n",
                        session.owner,
                    ));
                    continue;
                }
                if let Err(error) = secret_store::Store::open_owned(
                    &state.join("secrets").join(name),
                    session.owner,
                    session.portal,
                    true,
                )
                .and_then(|store| store.prepare_boot())
                {
                    super::emit_err(&format!(
                        "td-firstboot: credentials for uid {} remain unavailable: {error}\n",
                        session.owner,
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => {
                return Err(Failure::Failed(format!(
                    "inspect session credential store: {e}"
                )))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_fresh_session_has_an_enrollable_store_without_a_placeholder_record() {
        let guard = td_fs::TempDir::new("td-empty-portal-store").unwrap();
        let root = guard.path();
        let owner = fs::metadata(root).unwrap().uid();
        drop(store_parent(root).unwrap());
        prepare_missing_store(root, 1000, owner).unwrap();
        let path = root.join("secrets/1000");
        let master = fs::read(path.join("master")).unwrap();
        let store = secret_store::Store::open_owned(&path, 1000, owner, false).unwrap();
        assert!(store.get("mail", "main").unwrap().is_none());
        assert!(store.application_secret("mail", "main").is_err());
        drop(store);
        prepare_missing_store(root, 1000, owner).unwrap();
        assert_eq!(fs::read(path.join("master")).unwrap(), master);
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    #[ignore = "requires a disposable root VM without a TPM and TD_TEST_ROOT_TD_UTIL=/bin/td-util"]
    fn token_store_boot_stays_locked_and_provisioning_preserves_records() {
        secret_store::require_root().unwrap();
        assert_eq!(
            std::env::var("TD_TEST_ROOT_TD_UTIL").unwrap(),
            "/bin/td-util"
        );
        assert!(!Path::new("/dev/tpmrm0").exists());
        let guard = td_fs::TempDir::new("td-token-boot").unwrap();
        let root = guard.path().to_path_buf();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        drop(store_parent(&root).unwrap());
        let path = root.join("secrets/1000");
        let store = secret_store::Store::open(&path, 1000, true).unwrap();
        store
            .set("mail", "main", b"preserved locked credential")
            .unwrap();
        fs::create_dir_all("/run/td-secret").unwrap();
        fs::set_permissions("/run/td-secret", fs::Permissions::from_mode(0o777)).unwrap();
        let refusal = store
            .enroll_tokens(
                crate::fido_metadata::tests::metadata_fixture(1000, true),
                crate::tpm::Pcrs::parse("7").unwrap(),
            )
            .unwrap_err();
        assert!(
            refusal.contains("invalid owner, mode, type, or link count"),
            "{refusal}"
        );
        assert_eq!(
            store.get("mail", "main").unwrap().unwrap(),
            b"preserved locked credential"
        );
        assert!(!path.join("sealed").exists());
        fs::set_permissions("/run/td-secret", fs::Permissions::from_mode(0o755)).unwrap();
        drop(store);
        let record = fs::read(path.join("mail.main")).unwrap();
        // Structural fixture only: this test must never attempt an unseal.
        let protector = crate::fido_metadata::tests::protection_fixture(1000, true)
            .encode()
            .unwrap();
        let mut bundle = b"TDSEAL02".to_vec();
        bundle.extend_from_slice(&(protector.len() as u32).to_be_bytes());
        bundle.extend_from_slice(&protector);
        bundle.extend_from_slice(&1u32.to_be_bytes());
        for field in [b"mail.main".as_slice(), record.as_slice()] {
            bundle.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bundle.extend_from_slice(field);
        }
        let human = ApplicationHome {
            home: root.clone(),
            uid: 1000,
            gid: 1000,
        };
        write_durably_owned(&path.join("sealed"), &bundle, 0o600, Some(&human)).unwrap();
        let registry =
            crate::principals::Registry::parse("td-principals-v1\nsession\t1000\t993\t992\t991\n")
                .unwrap();
        for _ in 0..2 {
            isolate_stores(&root, &registry).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().uid(), 991);
            assert!(!path.join("master").exists());
            assert!(!path.join("mail.main").exists());
            assert!(!Path::new("/run/td-secret/1000/key").exists());
            assert_eq!(fs::read(path.join("sealed")).unwrap(), bundle);
            let service = ApplicationHome {
                home: root.clone(),
                uid: 991,
                gid: 991,
            };
            write_durably_owned(
                Path::new("/run/td-secret/1000/key"),
                &[42; 64],
                0o600,
                Some(&service),
            )
            .unwrap();
        }
        isolate_stores(&root, &registry).unwrap();
        assert!(!Path::new("/run/td-secret/1000/key").exists());
        let service = ApplicationHome {
            home: root.clone(),
            uid: 991,
            gid: 991,
        };
        write_durably_owned(
            &path.join("sealed"),
            b"corrupt protector",
            0o600,
            Some(&service),
        )
        .unwrap();
        write_durably_owned(
            Path::new("/run/td-secret/1000/key"),
            &[42; 64],
            0o600,
            Some(&service),
        )
        .unwrap();
        isolate_stores(&root, &registry).unwrap();
        assert!(!Path::new("/run/td-secret/1000/key").exists());
        assert_eq!(fs::read(path.join("sealed")).unwrap(), b"corrupt protector");
        fs::remove_file(path.join("sealed")).unwrap();
        isolate_stores(&root, &registry).unwrap();
        assert!(!path.join("master").exists());
        assert!(!path.join("mail.main").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires a disposable root VM without a TPM and TD_TEST_ROOT_TD_UTIL=/bin/td-util"]
    fn legacy_tpm_boot_never_unseals_and_rejects_a_stale_release() {
        secret_store::require_root().unwrap();
        let td_util = std::env::var("TD_TEST_ROOT_TD_UTIL").unwrap();
        assert_eq!(td_util, "/bin/td-util");
        assert!(!Path::new("/dev/tpmrm0").exists());
        let root = std::env::temp_dir().join(format!("td-isolated-release-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        drop(store_parent(&root).unwrap());
        let path = root.join("secrets/1000");
        let store = secret_store::Store::open(&path, 1000, true).unwrap();
        store
            .set("mail", "main", b"recoverable credential")
            .unwrap();
        drop(store);
        let master = fs::read(path.join("master")).unwrap();
        let record = fs::read(path.join("mail.main")).unwrap();
        let envelope = crate::tpm::tests::fixture(1000);
        let mut bundle = b"TDSEAL01".to_vec();
        bundle.extend_from_slice(&(envelope.len() as u32).to_be_bytes());
        bundle.extend_from_slice(&envelope);
        bundle.extend_from_slice(&1u32.to_be_bytes());
        for field in [b"mail.main".as_slice(), record.as_slice()] {
            bundle.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bundle.extend_from_slice(field);
        }
        let owner = ApplicationHome {
            home: root.clone(),
            uid: 1000,
            gid: 1000,
        };
        write_durably_owned(&path.join("sealed"), &bundle, 0o600, Some(&owner)).unwrap();
        let registry =
            crate::principals::Registry::parse("td-principals-v1\nsession\t1000\t993\t992\t991\n")
                .unwrap();
        // Boot isolation succeeds without any TPM device.
        isolate_stores(&root, &registry).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().uid(), 991);
        let store = secret_store::Store::open_owned(&path, 1000, 991, false).unwrap();
        assert!(store.get("mail", "main").is_err());
        assert!(!Path::new("/run/td-secret/1000/key").exists());
        // A stale TPM-only runtime key cannot satisfy application policy.
        let mut released = crate::crypto::digest(&envelope).to_vec();
        released.extend_from_slice(&master);
        let service = ApplicationHome {
            home: root.clone(),
            uid: 991,
            gid: 991,
        };
        write_durably_owned(
            Path::new("/run/td-secret/1000/key"),
            &released,
            0o600,
            Some(&service),
        )
        .unwrap();
        assert_eq!(
            store.get("mail", "main").unwrap().unwrap(),
            b"recoverable credential"
        );
        assert_eq!(
            store.application_secret("mail", "main").unwrap_err(),
            "credential store requires token enrollment"
        );
        store.prepare_boot().unwrap();
        assert!(!Path::new("/run/td-secret/1000/key").exists());
        assert_eq!(fs::read(path.join("sealed")).unwrap(), bundle);
        drop(store);
        for published in [false, true] {
            fs::remove_dir_all(&path).unwrap();
            let store = secret_store::Store::open(&path, 1000, true).unwrap();
            store
                .set("mail", "main", b"quarantined credential")
                .unwrap();
            drop(store);
            if published {
                isolate_stores(&root, &registry).unwrap();
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(path.join("notes"), b"unrecognized entry").unwrap();
            isolate_stores(&root, &registry).unwrap();
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!((metadata.uid(), metadata.mode() & 0o7777), (0, 0o700));
            assert!(secret_store::Store::open_owned(&path, 1000, 991, false).is_err());
            // The positive control: the probe reads the file as root, so a
            // td-util that fails for any reason cannot pass the denials below.
            assert!(std::process::Command::new(&td_util)
                .arg("cat")
                .arg(path.join("master"))
                .output()
                .unwrap()
                .status
                .success());
            for uid in [1000, 991] {
                use std::os::unix::process::CommandExt;
                assert!(!std::process::Command::new(&td_util)
                    .arg("cat")
                    .arg(path.join("master"))
                    .uid(uid)
                    .gid(uid)
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
        }
        fs::remove_dir_all(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        isolate_stores(&root, &registry).unwrap();
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(secret_store::Store::open_owned(&path, 1000, 991, false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    use crate::{write_durably_owned, ApplicationHome};
}
