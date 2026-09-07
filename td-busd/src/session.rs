//! Fixed stock-session admission; EXTERNAL claims never select a role.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// Mirrored by engine::permissions and checked by the image recipe.
pub(crate) const UID: u32 = 992;
pub(crate) const HUMAN_UID: u32 = 1000;
pub(crate) const SOCKET: &str = "/run/td-bus/1000/bus";

pub(crate) fn check(uid: u32) -> Result<(), String> {
    if uid != UID {
        return Err(format!("the stock session broker requires uid {UID}"));
    }
    let parent = Path::new(SOCKET)
        .parent()
        .ok_or("the session bus path has no parent")?;
    // The socket constant supplies the whole chain; no parallel path list can
    // drift away from the endpoint the listener actually binds.
    for (index, path) in parent.ancestors().enumerate() {
        let owner = if index == 0 { UID } else { 0 };
        directory(path, (owner, owner), index < 2)?;
    }
    Ok(())
}

fn directory(path: &Path, owner: (u32, u32), exact: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect session bus directory {}: {error}", path.display()))?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || mode & 0o7022 != 0
        || mode & 0o001 == 0
        || (exact && mode != 0o755)
    {
        return Err(format!(
            "{} must be an unredirected traversable directory owned by uid {} gid {} without other writers{}; got uid {} gid {} mode {mode:04o}",
            path.display(),
            owner.0,
            owner.1,
            if exact { " and with mode 0755" } else { "" },
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(())
}

/// The shared immutable policy reader also serves jail, display and audio.
pub(crate) fn load_policy() -> Result<crate::app_policy::Policy, String> {
    let policy = crate::app_policy::load()?;
    if policy.owner() != HUMAN_UID {
        return Err("bus application policy names another session".into());
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn shared_directory_refuses_redirects_and_other_writers() {
        assert!(check(HUMAN_UID).is_err());
        let root = std::env::temp_dir().join(format!("td-bus-directory-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        let gid = fs::metadata(&root).unwrap().gid();
        directory(&root, (uid, gid), true).unwrap();
        assert!(directory(&root, (uid.wrapping_add(1), gid), true).is_err());
        let link = root.with_extension("link");
        symlink(&root, &link).unwrap();
        assert!(directory(&link, (uid, gid), true).is_err());
        fs::remove_file(link).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(directory(&root, (uid, gid), true).is_err());
        for mode in [0o555, 0o711] {
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            directory(&root, (uid, gid), false).unwrap();
            assert!(directory(&root, (uid, gid), true).is_err());
        }
        for mode in [0o750, 0o775, 0o1755] {
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            assert!(directory(&root, (uid, gid), false).is_err());
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
