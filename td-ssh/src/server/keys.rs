//! The host key file, judged as strictly as any other credential.

use super::account::SingleUser;
use super::authkeys::check_path;
use crate::keyfile::{wipe_text, PrivateKey, PrivateKeyFile, MAX_PRIVATE_KEY_FILE};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const GROUP_OTHER_ACCESS: u32 = 0o077;

pub fn load_host_key(path: &Path, single_user: Option<&SingleUser>) -> Result<PrivateKey, String> {
    // /etc/ssh/ssh_host_ed25519_key is a reviewed link into persistent state.
    let resolved =
        fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let owner = single_user.map_or(0, |single| single.uid);
    check_path(&resolved, owner)?;
    let metadata =
        fs::metadata(&resolved).map_err(|error| format!("{}: {error}", resolved.display()))?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & GROUP_OTHER_ACCESS != 0
        || metadata.len() > MAX_PRIVATE_KEY_FILE as u64
    {
        return Err(format!(
            "{}: a host key must be a small regular file only uid {owner} can access",
            resolved.display()
        ));
    }
    let text = fs::read_to_string(&resolved)
        .map_err(|error| format!("{}: {error}", resolved.display()))?;
    let file = PrivateKeyFile::parse(&text).map_err(|error| error.to_string());
    wipe_text(text);
    let file = file?;
    if file.is_encrypted() {
        return Err(format!(
            "{}: a host key cannot be encrypted",
            resolved.display()
        ));
    }
    file.decrypt(None).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn single(uid: u32) -> SingleUser {
        SingleUser {
            name: "t".into(),
            uid,
            gid: uid,
            home: "/".into(),
            shell: "/bin/sh".into(),
        }
    }

    #[test]
    fn modes_owner_and_encryption_are_all_enforced() {
        let dir = td_fs::TempDir::new("td-ssh-hostkey").unwrap();
        let path = dir.path().join("key");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let key = PrivateKey::from_seed(&[1; 32], "").unwrap();
        fs::write(&path, key.to_file(None, &[0; 16], 1).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_host_key(&path, Some(&single(uid))).is_ok());
        assert!(load_host_key(&path, Some(&single(uid.wrapping_add(1)))).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(load_host_key(&path, Some(&single(uid))).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, key.to_file(Some(b"x"), &[0; 16], 1).unwrap()).unwrap();
        assert!(load_host_key(&path, Some(&single(uid))).is_err());
        fs::write(&path, "not a key").unwrap();
        assert!(load_host_key(&path, Some(&single(uid))).is_err());
        assert!(load_host_key(&dir.path().join("gone"), Some(&single(uid))).is_err());
    }
}
