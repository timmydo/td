//! Fixed stock-session admission; EXTERNAL claims never select a role.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// Mirrored by engine::permissions and checked by the image recipe.
pub(crate) const UID: u32 = 992;
pub(crate) const HUMAN_UID: u32 = 1000;
pub(crate) const SOCKET: &str = "/run/td-bus/1000/bus";
// Linux x86-64, matching this crate's syscall surface.
const O_NOFOLLOW: i32 = 0x20000;
const O_DIRECTORY: i32 = 0x10000;
const O_NONBLOCK: i32 = 0x800;

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

/// The immutable image owns both the policy file and its parent.
pub(crate) fn load_policy() -> Result<crate::app_policy::Policy, String> {
    let directory = Path::new(crate::app_policy::PATH)
        .parent()
        .ok_or("bus policy has no parent")?;
    load_policy_from(directory, (0, 0))
}

fn load_policy_from(
    directory: &Path,
    owner: (u32, u32),
) -> Result<crate::app_policy::Policy, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let etc = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(directory)
        .map_err(|error| format!("open bus policy directory: {error}"))?;
    let metadata = etc.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || metadata.mode() & 0o7022 != 0
    {
        return Err("bus policy directory is not root-controlled".into());
    }
    let name = Path::new(crate::app_policy::PATH)
        .file_name()
        .ok_or("bus policy has no filename")?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(Path::new(&format!("/proc/self/fd/{}", etc.as_raw_fd())).join(name))
        .map_err(|error| format!("open bus application policy: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || metadata.mode() & 0o7777 != 0o444
        || metadata.nlink() != 1
        || metadata.len() > crate::app_policy::MAX_BYTES as u64
    {
        return Err("bus application policy must be one bounded root-owned 0444 file".into());
    }
    let mut text = String::new();
    file.take(crate::app_policy::MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read bus application policy: {error}"))?;
    let policy = crate::app_policy::Policy::parse(&text)?;
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
    fn policy_loader_refuses_mutable_redirected_and_noncanonical_files() {
        let root = std::env::temp_dir().join(format!("td-bus-policy-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(&root).unwrap();
        let owner = (metadata.uid(), metadata.gid());
        let path = root.join(Path::new(crate::app_policy::PATH).file_name().unwrap());
        let valid = "td-bus-applications-v1\t1000\n65536\tfirefox\torg.mozilla.firefox\n";
        assert!(load_policy_from(&root, owner).is_err());
        fs::write(&path, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(load_policy_from(&root, owner).unwrap().to_tsv(), valid);
        assert!(load_policy_from(&root, (owner.0.wrapping_add(1), owner.1)).is_err());
        for mode in [0o644, 0o400, 0o1444] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(load_policy_from(&root, owner).is_err(), "mode {mode:o}");
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let other = root.join("other");
        fs::hard_link(&path, &other).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&other, &path).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_file(&other).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::remove_dir(&path).unwrap();
        for content in [
            valid.replace("1000", "1001"),
            valid.replace("65536", "065536"),
            "x".repeat(crate::app_policy::MAX_BYTES + 1),
        ] {
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
            assert!(load_policy_from(&root, owner).is_err());
            fs::remove_file(&path).unwrap();
        }
        fs::write(&path, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.with_extension("link");
        symlink(&root, &link).unwrap();
        assert!(load_policy_from(&link, owner).is_err());
        fs::remove_file(link).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

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
