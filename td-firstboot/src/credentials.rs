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

pub(super) fn provision(
    state: &Path,
    owner: &ApplicationHome,
    directory: &File,
) -> Result<(), Failure> {
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
    let store = secret_store::Store::open(&parent.join(owner.uid.to_string()), owner.uid, true)
        .map_err(Failure::Failed)?;
    store.release().map_err(Failure::Failed)?;
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
        provision(&root, &owner, &directory).unwrap();
        assert_eq!(
            fs::metadata(root.join("secrets")).unwrap().mode() & 0o7777,
            0o755
        );
        provision(&root, &owner, &directory).unwrap();
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
            assert!(provision(&root, &owner, &directory).is_err());
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
