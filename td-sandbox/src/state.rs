//! The human's own record of each sandbox: its name, the runtime it asks
//! for, and, for one imported from before sandbox accounts, where its old
//! home lies. Its account, grants and network are root's (td-authd's
//! ledger and grant table), never kept here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// A record's first line.
const RECORD_FORMAT: &str = "td-sandbox-profile-v1";
/// The most profiles listed or imported.
pub const MAX_PROFILES: usize = 1024;

/// One sandbox's record, locked while it is open: a session, or a change
/// to it, holds the lock.
pub struct Profile {
    pub root: PathBuf,
    /// A declared runtime's td name; none for a native sandbox. Only
    /// setting it asks whether it is declared: one a later system no
    /// longer declares still opens, shows and clears, and root refuses
    /// its launch.
    pub runtime: Option<String>,
    /// The home of the profile this one was imported from, which the
    /// import left as it was.
    pub old_home: Option<PathBuf>,
    _lock: Option<File>,
}

/// A declared runtime's td name.
pub fn runtime(name: &str) -> io::Result<&'static str> {
    crate::flathub_app::flathub_runtime(name)
        .map(|runtime| runtime.name)
        .ok_or_else(|| crate::failure(format_args!("{name} is no declared runtime")))
}

/// A sandbox's name: the ledger's grammar, since root's account is kept
/// under it.
pub fn name(name: &str) -> io::Result<()> {
    if !crate::workload_accounts::valid_name(name) {
        return Err(io::Error::other(
            "a sandbox's name is 1–64 lowercase letters, digits, '.', '_' or '-', \
             beginning with a letter",
        ));
    }
    Ok(())
}

/// `~/.local/share/td-sandbox`, private, canonical.
pub fn base(home: &Path) -> io::Result<PathBuf> {
    let root = home.join(".local/share/td-sandbox");
    td_fs::private_dir(&root)?;
    fs::canonicalize(root)
}

/// Where the records lie below `base`: a name no profile from before
/// sandbox accounts could have taken, since theirs held no `.`.
pub fn profiles(base: &Path) -> io::Result<PathBuf> {
    let profiles = base.join(".profiles");
    td_fs::private_dir(&profiles)?;
    Ok(profiles)
}

pub fn text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .filter(|p| !p.contains(['\n', '\r', '\0']))
        .ok_or_else(|| io::Error::other("path must be UTF-8 without line breaks"))
}

fn open_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(0o400_000 | 0o4000)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != fs::metadata("/proc/self")?.uid()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(io::Error::other(
            "sandbox metadata is not a private, singly linked regular file",
        ));
    }
    Ok(file)
}

/// The names of the records below `profiles`, sorted.
pub fn list(profiles: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(profiles)?.take(MAX_PROFILES) {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(n) = entry.file_name().to_str().filter(|n| name(n).is_ok()) {
                td_alloc::push(&mut names, crate::owned(n)?).map_err(crate::refused)?;
            }
        }
    }
    // Directory entries' names are distinct, so no order among equals is
    // lost by an unstable sort, which allocates nothing.
    names.sort_unstable();
    Ok(names)
}

/// Whether a record for `name` exists below `profiles`.
pub fn exists(profiles: &Path, name_: &str) -> io::Result<bool> {
    name(name_)?;
    match fs::symlink_metadata(profiles.join(name_)) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// A record held for its removal, readable or not: its lock taken, so no
/// session or change of it runs meanwhile.
pub struct Removal {
    root: PathBuf,
    _lock: File,
}

impl Removal {
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.root)
    }
}

/// The record of `name` held for removal, none when there is none. An
/// incomplete or unreadable record is held all the same, so it can be
/// deleted.
pub fn removal(profiles: &Path, name_: &str) -> io::Result<Option<Removal>> {
    if !exists(profiles, name_)? {
        return Ok(None);
    }
    let root = profiles.join(name_);
    td_fs::check_private_dir(&root)?;
    let lock = open_private(&root.join("lock"))?;
    lock.try_lock()
        .map_err(|e| crate::failure(format_args!("sandbox is active or being changed: {e}")))?;
    Ok(Some(Removal { root, _lock: lock }))
}

impl Profile {
    /// A new record, refused when one exists: a crash before it is
    /// complete may leave an incomplete record for the human to delete.
    pub fn create(
        profiles: &Path,
        name_: &str,
        runtime: Option<String>,
        old_home: Option<PathBuf>,
    ) -> io::Result<Self> {
        name(name_)?;
        let root = profiles.join(name_);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => match crate::text(format_args!(
                    "a sandbox named {name_} already exists here: `td-sandbox inspect \
                     {name_}` shows it, `td-sandbox delete {name_}` deletes it"
                )) {
                    Ok(message) => io::Error::new(e.kind(), message),
                    Err(refusal) => refusal,
                },
                _ => e,
            })?;
        let profile = Self {
            root,
            runtime,
            old_home,
            _lock: None,
        };
        let written = (|| {
            let lock = open_private(&profile.root.join("lock"))?;
            lock.try_lock()
                .map_err(|e| crate::failure(format_args!("sandbox is being changed: {e}")))?;
            let mut record = open_private(&profile.root.join("record"))?;
            record.write_all(profile.render()?.as_bytes())?;
            record.sync_all()?;
            Ok(lock)
        })();
        match written {
            Ok(lock) => Ok(Self {
                _lock: Some(lock),
                ..profile
            }),
            Err(e) => {
                let _ = fs::remove_dir_all(&profile.root);
                Err(e)
            }
        }
    }

    /// The record of `name`, locked: refused while a session or another
    /// change holds it.
    pub fn open(profiles: &Path, name_: &str) -> io::Result<Self> {
        let mut profile = Self::read(profiles, name_)?;
        let lock = open_private(&profile.root.join("lock"))?;
        lock.try_lock()
            .map_err(|e| crate::failure(format_args!("sandbox is active or being changed: {e}")))?;
        profile._lock = Some(lock);
        Ok(profile)
    }

    /// The record of `name`, unlocked, to show.
    pub fn read(profiles: &Path, name_: &str) -> io::Result<Self> {
        name(name_)?;
        let root = profiles.join(name_);
        td_fs::check_private_dir(&root).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => crate::failure(format_args!("no sandbox named {name_}")),
            _ => e,
        })?;
        let record = td_fs::read_bounded_real_file(&root.join("record"), "sandbox record", 8192)?;
        let record = String::from_utf8(record).map_err(io::Error::other)?;
        let (runtime, old_home) = parse(&record)?;
        Ok(Self {
            root,
            runtime,
            old_home,
            _lock: None,
        })
    }

    pub fn save(&self) -> io::Result<()> {
        td_fs::replace(&self.root.join("record"), self.render()?.as_bytes(), 0o600)
    }

    /// The record removed, with its lock and stop socket.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.root)
    }

    fn render(&self) -> io::Result<String> {
        let mut record = crate::text(format_args!("{RECORD_FORMAT}\n"))?;
        if let Some(runtime) = &self.runtime {
            let line = crate::text(format_args!("runtime={runtime}\n"))?;
            td_alloc::push_str(&mut record, &line).map_err(crate::refused)?;
        }
        if let Some(home) = &self.old_home {
            let line = crate::text(format_args!("old-home={}\n", text(home)?))?;
            td_alloc::push_str(&mut record, &line).map_err(crate::refused)?;
        }
        Ok(record)
    }
}

/// The format line, an optional `runtime`, then an optional `old-home`.
fn parse(record: &str) -> io::Result<(Option<String>, Option<PathBuf>)> {
    let invalid = || io::Error::other("invalid sandbox record");
    let mut lines = record.lines().peekable();
    if lines.next() != Some(RECORD_FORMAT) {
        return Err(invalid());
    }
    let runtime = match lines.peek().and_then(|line| line.strip_prefix("runtime=")) {
        Some(runtime) if crate::flathub_app::valid_name(runtime) => {
            let runtime = crate::owned(runtime)?;
            lines.next();
            Some(runtime)
        }
        Some(_) => return Err(invalid()),
        None => None,
    };
    let old_home = match lines.next().map(|line| line.strip_prefix("old-home=")) {
        Some(Some(path)) if Path::new(path).is_absolute() => Some(PathBuf::from(path)),
        Some(_) => return Err(invalid()),
        None => None,
    };
    if lines.next().is_some() {
        return Err(invalid());
    }
    Ok((runtime, old_home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_private_locked_and_round_trips() -> io::Result<()> {
        let base = td_fs::TempDir::new("sandbox-record")?;
        let profiles = profiles(&base)?;
        let created = Profile::create(&profiles, "tools", None, None)?;
        assert!(Profile::open(&profiles, "tools").is_err());
        assert!(Profile::create(&profiles, "tools", None, None).is_err());
        drop(created);
        let mut profile = Profile::open(&profiles, "tools")?;
        assert_eq!(
            (profile.runtime.as_deref(), profile.old_home.as_deref()),
            (None, None)
        );
        profile.runtime = Some(runtime("freedesktop-sdk-25-08")?.to_string());
        profile.save()?;
        drop(profile);
        assert_eq!(
            fs::read_to_string(profiles.join("tools/record"))?,
            "td-sandbox-profile-v1\nruntime=freedesktop-sdk-25-08\n"
        );
        // Reading takes no lock: a session does not hide its record.
        let session = Profile::open(&profiles, "tools")?;
        assert_eq!(
            Profile::read(&profiles, "tools")?.runtime.as_deref(),
            Some("freedesktop-sdk-25-08")
        );
        drop(session);
        let imported = Profile::create(
            &profiles,
            "default",
            None,
            Some(PathBuf::from("/h/.local/share/td-sandbox/default/home")),
        )?;
        drop(imported);
        assert_eq!(
            fs::read_to_string(profiles.join("default/record"))?,
            "td-sandbox-profile-v1\nold-home=/h/.local/share/td-sandbox/default/home\n"
        );
        assert_eq!(list(&profiles)?, ["default", "tools"]);
        let again = Profile::create(&profiles, "default", None, None).err();
        assert!(again.is_some_and(|e| e.to_string().contains("already exists")));
        Profile::open(&profiles, "tools")?.remove()?;
        assert_eq!(list(&profiles)?, ["default"]);
        assert!(!exists(&profiles, "tools")?);
        assert!(Profile::read(&profiles, "tools").is_err());
        Ok(())
    }

    /// A record that cannot be read, a crash's half-written one, is still
    /// removed, under its lock; none is nothing to remove.
    #[test]
    fn an_unreadable_record_is_still_removed() -> io::Result<()> {
        let base = td_fs::TempDir::new("sandbox-record-broken")?;
        let profiles = profiles(&base)?;
        assert!(removal(&profiles, "tools")?.is_none());
        drop(Profile::create(&profiles, "tools", None, None)?);
        fs::remove_file(profiles.join("tools/record"))?;
        assert!(Profile::read(&profiles, "tools").is_err());
        let session = removal(&profiles, "tools")?;
        assert!(removal(&profiles, "tools").is_err());
        session
            .ok_or_else(|| io::Error::other("no removal"))?
            .remove()?;
        assert!(!exists(&profiles, "tools")?);
        Ok(())
    }

    /// A runtime a later system no longer declares leaves the record
    /// usable: it opens and shows the name, and clears to native.
    #[test]
    fn a_retired_runtime_still_opens_and_clears() -> io::Result<()> {
        let base = td_fs::TempDir::new("sandbox-retired-runtime")?;
        let profiles = profiles(&base)?;
        drop(Profile::create(&profiles, "tools", None, None)?);
        td_fs::replace(
            &profiles.join("tools/record"),
            b"td-sandbox-profile-v1\nruntime=freedesktop-platform-24-08\n",
            0o600,
        )?;
        assert!(runtime("freedesktop-platform-24-08").is_err());
        let mut profile = Profile::open(&profiles, "tools")?;
        assert_eq!(
            profile.runtime.as_deref(),
            Some("freedesktop-platform-24-08")
        );
        profile.runtime = None;
        profile.save()?;
        drop(profile);
        assert_eq!(Profile::read(&profiles, "tools")?.runtime, None);
        Ok(())
    }

    #[test]
    fn records_and_names_are_exact() {
        for bad in [
            "",
            "format=1\nnetwork=off\n",
            "td-sandbox-profile-v1\nnetwork=off\n",
            "td-sandbox-profile-v1\nruntime=Org.Freedesktop\n",
            "td-sandbox-profile-v1\nruntime=\n",
            "td-sandbox-profile-v1\nold-home=relative\n",
            "td-sandbox-profile-v1\nold-home=/a\nruntime=freedesktop-sdk-25-08\n",
            "td-sandbox-profile-v1\nold-home=/a\nold-home=/b\n",
            "td-sandbox-profile-v1\nwrite=/w\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        for good in ["tools", "a", "my.box_1-2", &"a".repeat(64)] {
            assert!(name(good).is_ok(), "{good}");
        }
        for bad in [
            "1box",
            "-x",
            "Tools",
            "../x",
            ".profiles",
            "",
            &"a".repeat(65),
        ] {
            assert!(name(bad).is_err(), "{bad}");
        }
    }
}
