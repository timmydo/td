//! Provision the store before publishing portal configuration; resume after a crash.

use super::{secret_store, write_durably_owned, ApplicationHome, Failure};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const LEGACY: &str = "password_file = \"/home/td/.config/tmc/password\"";
const PLACEHOLDER: &[u8] = b"replace-me\n";

fn migrate_config(config: Option<&str>, has_legacy: bool) -> Result<Option<String>, Failure> {
    let Some(config) = config else {
        return Ok(None);
    };
    // The provisioner emits single-line strings. A line scanner cannot associate
    // assignments inside an operator's multiline string with an account safely.
    if config.contains("\"\"\"") || config.contains("'''") {
        if has_legacy || config.contains("password_file") {
            return Err(Failure::Failed(
                "mail uses multiline strings; migrate its credential explicitly".into(),
            ));
        }
        return Ok(None);
    }
    let mut section = "";
    let mut legacy = 0;
    let mut portal_main = 0;
    let mut commands = false;
    let mut main_sources = 0;
    for line in config.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let key = line.split_once('=').map(|(key, _)| key.trim());
        commands |= key == Some("password_command");
        if key == Some("password_file") {
            if line != LEGACY || section != "[account.main]" {
                return Err(Failure::Failed("mail has a custom password_file or account; migrate it explicitly before enabling portal mode".into()));
            }
            legacy += 1;
        }
        if section == "[account.main]" {
            main_sources += usize::from(matches!(
                key,
                Some("secret" | "password_file" | "password_command")
            ));
            if line == "secret = \"portal\"" {
                portal_main += 1;
            }
        }
    }
    if legacy > 1
        || portal_main > 1
        || (legacy != 0 && portal_main != 0)
        || (legacy != 0 && main_sources != 1)
        || (commands && has_legacy)
    {
        return Err(Failure::Failed(
            "mail has ambiguous credential sources; refusing migration".into(),
        ));
    }
    if legacy == 1 {
        if !has_legacy {
            return Err(Failure::Failed(
                "legacy mail credential is missing; refusing to replace it".into(),
            ));
        }
        // Only replace the parsed assignment, never an occurrence inside a comment.
        return Ok(Some(
            config
                .split_inclusive('\n')
                .map(|line| {
                    if line.trim() == LEGACY {
                        line.replacen(LEGACY, "secret = \"portal\"", 1)
                    } else {
                        line.to_string()
                    }
                })
                .collect(),
        ));
    }
    if has_legacy && (portal_main != 1 || main_sources != 1) {
        return Err(Failure::Failed(
            "legacy mail credential has no matching portal account; refusing to retire it".into(),
        ));
    }
    Ok(None)
}

fn optional(
    directory: &Path,
    name: &str,
    uid: u32,
    max: usize,
) -> Result<Option<Vec<u8>>, Failure> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(0o400000 | 0o4000)
        .open(directory.join(name))
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Failure::Failed(format!("open legacy mail {name}: {e}"))),
    };
    let metadata = file
        .metadata()
        .map_err(|e| Failure::Failed(e.to_string()))?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(Failure::Failed(
            "legacy mail credential/configuration has invalid metadata".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take((max + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::Failed(e.to_string()))?;
    if bytes.len() > max {
        return Err(Failure::Failed(
            "oversized legacy mail configuration or credential".into(),
        ));
    }
    Ok(Some(bytes))
}

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

/// Protect every existing session store even when its application home is broken.
pub(super) fn isolate_stores(state: &Path, registry: &crate::principals::Registry) -> Result<(), Failure> {
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
        let name = session.owner.to_string();
        let held_path = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(&name);
        match fs::symlink_metadata(held_path) {
            Ok(_) => {
                if let Err(error) = secret_store::Store::open_owned(
                    &state.join("secrets").join(name), session.owner, session.portal, true,
                ).and_then(|store| store.release()) {
                    super::emit_err(&format!(
                        "td-firstboot: credentials for uid {} remain unavailable: {error}\n",
                        session.owner,
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(Failure::Failed(format!("inspect session credential store: {e}"))),
        }
    }
    Ok(())
}

pub(super) fn provision(
    state: &Path,
    owner: &ApplicationHome,
    directory: &File,
    file_owner: u32,
) -> Result<(), Failure> {
    let _parent = store_parent(state)?;
    let store = secret_store::Store::open_owned(&state.join("secrets").join(owner.uid.to_string()), owner.uid, file_owner, true)
        .map_err(Failure::Failed)?;
    if file_owner == owner.uid {
        store.release().map_err(Failure::Failed)?;
    }
    let pinned = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let config = optional(&pinned, "config.toml", owner.uid, 64 * 1024)?;
    let config = config
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| Failure::Failed("mail configuration is not UTF-8".into()))?;
    let mut legacy = optional(&pinned, "password", owner.uid, secret_store::MAX_SECRET)?;
    let mut stored = store.get("mail", "main").map_err(Failure::Failed)?;
    let replacement = migrate_config(config.as_deref(), legacy.is_some())?;
    if let Some(legacy) = legacy.as_ref() {
        if stored.as_ref().is_some_and(|stored| stored != legacy) {
            return Err(Failure::Failed(
                "legacy and stored mail credentials differ; refusing to discard either".into(),
            ));
        }
    }
    if stored.is_none() {
        store
            .set("mail", "main", legacy.as_deref().unwrap_or(PLACEHOLDER))
            .map_err(Failure::Failed)?;
    }
    if let Some(bytes) = stored.as_mut() {
        bytes.fill(0);
    }
    if let Some(replacement) = replacement {
        write_durably_owned(
            &pinned.join("config.toml"),
            replacement.as_bytes(),
            0o600,
            Some(owner),
        )?;
    }
    if legacy.is_some() {
        if let Some(bytes) = legacy.as_mut() {
            bytes.fill(0);
        }
        fs::remove_file(pinned.join("password"))
            .and_then(|()| directory.sync_all())
            .map_err(|e| Failure::Failed(format!("retire legacy mail credential: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires an explicitly selected disposable root VM without a TPM"]
    fn unavailable_tpm_locks_only_credentials_and_accepts_later_release() {
        secret_store::require_root().unwrap();
        assert_eq!(std::env::var("TD_TEST_ROOT_BUSYBOX").unwrap(), "/bin/busybox");
        assert!(!Path::new("/dev/tpmrm0").exists());
        let root = std::env::temp_dir().join(format!("td-isolated-release-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        drop(store_parent(&root).unwrap());
        let path = root.join("secrets/1000");
        let store = secret_store::Store::open(&path, 1000, true).unwrap();
        store.set("mail", "main", b"recoverable credential").unwrap();
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
        let owner = ApplicationHome { home: root.clone(), uid: 1000, gid: 1000 };
        write_durably_owned(&path.join("sealed"), &bundle, 0o600, Some(&owner)).unwrap();
        let registry = crate::principals::Registry::parse("td-principals-v1\nsession\t1000\t993\t992\t991\n").unwrap();
        // This returns boot success even though the real release opens a missing TPM.
        isolate_stores(&root, &registry).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().uid(), 991);
        let store = secret_store::Store::open_owned(&path, 1000, 991, false).unwrap();
        assert!(store.get("mail", "main").is_err());
        assert!(!Path::new("/run/td-secret/1000/key").exists());
        // Model the checked volatile publication made by a later root release.
        // TPM seal/unseal correctness is exercised separately by the emulator tests.
        let mut released = crate::crypto::digest(&envelope).to_vec();
        released.extend_from_slice(&master);
        let service = ApplicationHome { home: root.clone(), uid: 991, gid: 991 };
        write_durably_owned(Path::new("/run/td-secret/1000/key"), &released, 0o600, Some(&service)).unwrap();
        assert_eq!(store.get("mail", "main").unwrap().unwrap(), b"recoverable credential");
        drop(store);
        fs::remove_file("/run/td-secret/1000/key").unwrap();
        for published in [false, true] {
            fs::remove_dir_all(&path).unwrap();
            let store = secret_store::Store::open(&path, 1000, true).unwrap();
            store.set("mail", "main", b"quarantined credential").unwrap();
            drop(store);
            if published { isolate_stores(&root, &registry).unwrap(); }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(path.join("notes"), b"unrecognized entry").unwrap();
            isolate_stores(&root, &registry).unwrap();
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!((metadata.uid(), metadata.mode() & 0o7777), (0, 0o700));
            assert!(secret_store::Store::open_owned(&path, 1000, 991, false).is_err());
            for uid in [1000, 991] {
                use std::os::unix::process::CommandExt;
                assert!(!std::process::Command::new("/bin/busybox")
                    .arg("cat").arg(path.join("master")).uid(uid).gid(uid)
                    .output().unwrap().status.success());
            }
        }
        fs::remove_dir_all(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        isolate_stores(&root, &registry).unwrap();
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert!(secret_store::Store::open_owned(&path, 1000, 991, false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    #[test]
    fn custom_or_ambiguous_sources_are_not_migrated() {
        for config in [
            format!("[account.renamed]\n{LEGACY}\n"),
            format!("[account.main]\n{LEGACY}\n[account.work]\npassword_file = \"custom\"\n"),
            format!("[account.main]\n{LEGACY}\npassword_command = \"cat password\"\n"),
            "[account.main]\npassword_command = \"cat password\"\n".to_string(),
            format!("[account.main]\n{LEGACY}\nsecret = \"portal\"\n"),
            format!("[account.main]\n{LEGACY}\n{LEGACY}\n"),
        ] {
            assert!(migrate_config(Some(&config), true).is_err(), "{config}");
        }
        let config = format!("# old setting: {LEGACY}\n[account.main]\n{LEGACY}\n");
        let migrated = migrate_config(Some(&config), true).unwrap().unwrap();
        assert!(migrated.starts_with(&format!("# old setting: {LEGACY}\n")));
        assert!(migrate_config(Some(&migrated), true).unwrap().is_none());
        assert!(migrate_config(Some(&config), false).is_err());
    }

    #[test]
    fn migration_preserves_real_credentials_and_repeats_safely() {
        let root = std::env::temp_dir().join(format!("td-secret-migration-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        let owner = ApplicationHome {
            home: root.clone(),
            uid,
            gid: fs::metadata(&root).unwrap().gid(),
        };
        let directory = File::open(&root).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("secrets"))
            .unwrap();
        write_durably_owned(
            &root.join("config.toml"),
            format!("[account.main]\n{LEGACY}\n").as_bytes(),
            0o600,
            Some(&owner),
        )
        .unwrap();
        write_durably_owned(
            &root.join("password"),
            b"existing password\n",
            0o600,
            Some(&owner),
        )
        .unwrap();
        provision(&root, &owner, &directory, owner.uid).unwrap();
        assert_eq!(
            fs::metadata(root.join("secrets")).unwrap().mode() & 0o7777,
            0o755
        );
        provision(&root, &owner, &directory, owner.uid).unwrap();
        assert!(!root.join("password").exists());
        assert!(fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("secret = \"portal\""));
        let store =
            secret_store::Store::open(&root.join(format!("secrets/{uid}")), uid, false).unwrap();
        assert_eq!(
            store.get("mail", "main").unwrap(),
            Some(b"existing password\n".to_vec())
        );
        drop(store);
        for custom in [
            format!("[account.renamed]\n{LEGACY}\n"),
            format!("[account.main]\nusername = \"\"\"\n{LEGACY}\n\"\"\"\n"),
            format!("[account.main]\nusername = '''\n{LEGACY}\n'''\n"),
        ] {
            fs::write(root.join("config.toml"), &custom).unwrap();
            write_durably_owned(
                &root.join("password"),
                b"existing password\n",
                0o600,
                Some(&owner),
            )
            .unwrap();
            assert!(provision(&root, &owner, &directory, owner.uid).is_err());
            assert_eq!(
                fs::read_to_string(root.join("config.toml")).unwrap(),
                custom
            );
            assert_eq!(
                fs::read(root.join("password")).unwrap(),
                b"existing password\n"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
