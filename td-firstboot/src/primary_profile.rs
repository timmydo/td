//! Publish one coherent primary identity before any account consumer starts.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::principals::{self, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};

fn child(parent: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name)
}

fn at(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

fn directory(path: &Path, logical: &Path, owner: (u32, u32)) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
        .map_err(|error| at(logical, error))?;
    let metadata = file.metadata().map_err(|error| at(logical, error))?;
    if (metadata.uid(), metadata.gid()) != owner || metadata.mode() & 0o7777 != 0o755 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "primary-profile directory {} requires owner {}:{} and mode 0755",
                logical.display(),
                owner.0,
                owner.1
            ),
        ));
    }
    Ok(file)
}

fn saved_name(root: &Path, owner: (u32, u32)) -> io::Result<Option<String>> {
    let mut logical = root.to_owned();
    let root = directory(root, &logical, owner)?;
    logical.push("var");
    let mut parent = directory(&child(&root, "var"), &logical, owner)?;
    for name in ["lib", "td"] {
        logical.push(name);
        parent = match directory(&child(&parent, name), &logical, owner) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    }
    logical.push("username");
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(child(&parent, "username"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(at(&logical, error)),
    };
    let metadata = file.metadata().map_err(|error| at(&logical, error))?;
    if !metadata.is_file()
        || (metadata.uid(), metadata.gid()) != owner
        || metadata.mode() & 0o7777 != 0o644
        || metadata.nlink() != 1
        || metadata.len() > 33
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "saved username {} requires one trusted mode-0644 regular file of at most 33 bytes",
                logical.display()
            ),
        ));
    }
    let mut text = String::new();
    file.take(34)
        .read_to_string(&mut text)
        .map_err(|error| at(&logical, error))?;
    let name = text
        .strip_suffix('\n')
        .filter(|_| text.len() <= 33)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "saved username {} requires one bounded newline-terminated name",
                    logical.display()
                ),
            )
        })?;
    principals::primary_account::validate_name(name).map_err(|error| at(&logical, error))?;
    Ok(Some(name.to_owned()))
}

fn mount(arguments: &[&std::ffi::OsStr]) -> Result<(), String> {
    let status = Command::new("/bin/mount")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|error| format!("start primary account bind: {error}"))?;
    if !status.success() {
        return Err(format!("primary account bind failed: {status}"));
    }
    Ok(())
}

fn publish(source: &Path, target: &Path) -> Result<(), String> {
    let expected = source.metadata().map_err(|error| error.to_string())?;
    mount(&[
        "-o".as_ref(),
        "bind".as_ref(),
        source.as_os_str(),
        target.as_os_str(),
    ])?;
    mount(&[
        "-o".as_ref(),
        "remount,bind,ro,nodev,nosuid,noexec".as_ref(),
        target.as_os_str(),
        target.as_os_str(),
    ])?;
    let actual = target.metadata().map_err(|error| error.to_string())?;
    if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
        return Err("primary account bind does not retain its prepared inode".into());
    }
    Ok(())
}

fn read_only(target: &Path) -> Result<(), String> {
    match OpenOptions::new().write(true).open(target) {
        Err(error) if error.kind() == io::ErrorKind::ReadOnlyFilesystem => Ok(()),
        Err(error) => Err(format!("primary account read-only check: {error}")),
        Ok(_) => Err("primary account bind permits a write-open".into()),
    }
}

/// The caller authenticates ROOT, mounts persistent /var and fresh /run, and
/// serializes publication before any user process or competing root writer.
pub(crate) fn prepare(root: &Path) -> Result<String, String> {
    let original = principals::primary_in_root(root)
        .map_err(|error| format!("primary account in {}: {error}", root.display()))?;
    let selected = saved_name(root, (0, 0))
        .map_err(|error| format!("saved primary account in {}: {error}", root.display()))?;
    let expected = selected.as_deref().unwrap_or(original.name()).to_owned();
    if let Some(name) = selected {
        let root_directory = directory(root, root, (0, 0))
            .map_err(|error| format!("primary account root: {error}"))?;
        let _runtime = directory(&child(&root_directory, "run"), &root.join("run"), (0, 0))
            .map_err(|error| format!("primary account runtime: {error}"))?;
        let staged = root.join("run/td-primary");
        principals::stage_primary_name(root, &name, &staged)?;
        for leaf in ["passwd", "group", "shadow"] {
            publish(&staged.join("etc").join(leaf), &root.join("etc").join(leaf))
                .map_err(|error| format!("publish primary {leaf}: {error}"))?;
        }
    }
    for leaf in ["passwd", "group", "shadow"] {
        read_only(&root.join("etc").join(leaf))
            .map_err(|error| format!("primary {leaf}: {error}"))?;
    }
    let published = principals::primary_in_root(root)
        .map_err(|error| format!("verify published primary tables in {}: {error}", root.display()))?;
    if published.name() != expected {
        return Err("published primary account does not match the selected name".into());
    }
    let home = crate::primary_home::prepare(root)?;
    let marker = format!("TD-PRIMARY-PROFILE-READY {expected}\n");
    io::stderr().lock().write_all(marker.as_bytes())
        .map_err(|error| format!("report primary profile: {error}"))?;
    Ok(home)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        root: PathBuf,
        owner: (u32, u32),
    }
    impl Fixture {
        fn fresh_root(stamp: u128) -> io::Result<PathBuf> {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            for _ in 0..64 {
                let root = std::env::temp_dir().join(format!(
                    "td-profile-{}-{stamp}-{}",
                    std::process::id(),
                    SERIAL.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::DirBuilder::new().mode(0o755).create(&root) {
                    Ok(()) => return Ok(root),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::other("profile fixture names exhausted"))
        }

        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = Self::fresh_root(stamp).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
            for relative in ["var", "var/lib", "var/lib/td"] {
                let path = root.join(relative);
                fs::DirBuilder::new().mode(0o755).create(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let metadata = root.metadata().unwrap();
            Self {
                root,
                owner: (metadata.uid(), metadata.gid()),
            }
        }
        fn path(&self) -> PathBuf {
            self.root.join("var/lib/td/username")
        }
        fn put(&self, value: &[u8]) {
            fs::write(self.path(), value).unwrap();
            fs::set_permissions(self.path(), fs::Permissions::from_mode(0o644)).unwrap();
        }
        fn read(&self) -> io::Result<Option<String>> {
            saved_name(&self.root, self.owner)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn absence_is_optional_and_valid_choices_are_exact() {
        let fixture = Fixture::new();
        assert_eq!(fixture.read().unwrap(), None);
        for name in ["alice", "tester", "a-b_c1", &"a".repeat(32)] {
            fixture.put(format!("{name}\n").as_bytes());
            assert_eq!(fixture.read().unwrap().as_deref(), Some(name));
        }
        fs::remove_dir_all(fixture.root.join("var/lib")).unwrap();
        assert_eq!(fixture.read().unwrap(), None);
        assert!(!fixture.root.join("var/lib").exists());
    }

    #[test]
    fn malformed_or_unbounded_choices_never_become_a_name() {
        let fixture = Fixture::new();
        for value in [
            b"".as_slice(),
            b"\n",
            b"alice",
            b"alice\n\n",
            b"alice,root\n",
            b"*\n",
            b"Alice\n",
            b"../alice\n",
            b"alice\r\n",
            b"\xff\n",
        ] {
            fixture.put(value);
            let error = fixture.read().unwrap_err().to_string();
            assert!(
                error.contains(fixture.path().to_str().unwrap()),
                "{value:?}: {error}"
            );
            assert_eq!(fs::read(fixture.path()).unwrap(), value);
        }
        fixture.put(format!("{}\n", "a".repeat(33)).as_bytes());
        assert!(fixture.read().is_err());
    }

    #[test]
    fn aliases_permissions_and_nonfiles_refuse_without_repair() {
        let fixture = Fixture::new();
        fixture.put(b"alice\n");
        for mode in [0o600, 0o664, 0o666, 0o4644] {
            fs::set_permissions(fixture.path(), fs::Permissions::from_mode(mode)).unwrap();
            assert!(fixture.read().is_err());
            assert_eq!(fixture.path().metadata().unwrap().mode() & 0o7777, mode);
        }
        fixture.put(b"alice\n");
        let alias = fixture.root.join("alias");
        fs::hard_link(fixture.path(), &alias).unwrap();
        assert!(fixture.read().is_err());
        fs::remove_file(&alias).unwrap();
        fs::rename(fixture.path(), &alias).unwrap();
        symlink(&alias, fixture.path()).unwrap();
        assert!(fixture.read().is_err());
        fs::remove_file(fixture.path()).unwrap();
        symlink("missing", fixture.path()).unwrap();
        assert!(fixture.read().is_err());
        fs::remove_file(fixture.path()).unwrap();
        fs::create_dir(fixture.path()).unwrap();
        assert!(fixture.read().is_err());
    }

    #[test]
    fn untrusted_ancestors_do_not_turn_absence_into_a_default() {
        let fixture = Fixture::new();
        let state = fixture.root.join("var/lib/td");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(fixture
            .read()
            .unwrap_err()
            .to_string()
            .contains(state.to_str().unwrap()));
        fs::remove_dir(&state).unwrap();
        symlink("missing", &state).unwrap();
        assert!(fixture.read().is_err());
    }
}
