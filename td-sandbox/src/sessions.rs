//! A CLI session owns a locked marker. Unlinking it stops that relay.

use crate::state::{open_private, Profile};
use crate::{failure, refused, text};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const PREFIX: &str = "session-";
// The profile holds its record, lock and markers (root admits 16 launches).
const MAX_ENTRIES: usize = 1024;

fn parse_id(word: &str) -> Option<u64> {
    if word.len() != 16
        || !word
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    u64::from_str_radix(word, 16).ok()
}

/// Hold the profile directory lock only while registering, selecting or removing markers.
pub struct Registry(File);

impl Registry {
    pub(super) fn open(profile: &Path) -> io::Result<Self> {
        let directory = File::open(profile)?;
        directory.lock()?;
        Ok(Self(directory))
    }

    fn path(&self, id: u64) -> io::Result<String> {
        text(format_args!(
            "/proc/self/fd/{}/{PREFIX}{id:016x}",
            self.0.as_raw_fd()
        ))
    }

    pub fn active(&self) -> io::Result<Vec<u64>> {
        self.scan(MAX_ENTRIES)
    }

    fn scan(&self, limit: usize) -> io::Result<Vec<u64>> {
        let mut active = Vec::new();
        let root = text(format_args!("/proc/self/fd/{}", self.0.as_raw_fd()))?;
        for (index, entry) in fs::read_dir(root)?.enumerate() {
            if index >= limit {
                return Err(io::Error::other("too many entries in sandbox profile"));
            }
            let name = entry?.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix(PREFIX))
                .and_then(parse_id)
            else {
                continue;
            };
            let path = self.path(id)?;
            let marker = open_private(Path::new(&path), false)?;
            match marker.try_lock() {
                Ok(()) => fs::remove_file(path)?,
                Err(fs::TryLockError::WouldBlock) => {
                    td_alloc::push(&mut active, id).map_err(refused)?
                }
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        active.sort_unstable();
        Ok(active)
    }

    pub fn stop(&self, name: &str, selected: Option<&str>) -> io::Result<()> {
        let ids = self.active()?;
        let id = match (selected, ids.as_slice()) {
            (Some(word), _) => {
                let id = parse_id(word).ok_or_else(|| {
                    io::Error::other("expected a 16-digit lowercase hex session ID")
                })?;
                if !ids.contains(&id) {
                    return Err(failure(format_args!(
                        "no active session {word} for sandbox {name}"
                    )));
                }
                id
            }
            (None, [id]) => *id,
            (None, []) => return Err(io::Error::other("no active sandbox shell or command")),
            (None, _) => {
                return Err(failure(format_args!(
                    "multiple sessions for {name}: {ids:016x?}; use td-sandbox stop {name} ID"
                )))
            }
        };
        fs::remove_file(self.path(id)?)
    }
}

pub struct Session<'a> {
    profile: &'a Profile,
    pub marker: File,
    pub id: u64,
}

impl<'a> Session<'a> {
    pub fn register(profile: &'a Profile) -> io::Result<Self> {
        let registry = profile.sessions()?;
        registry.scan(MAX_ENTRIES - 1)?; // Reap the unlocked markers of crashed clients.
        let mut bytes = [0; 8];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let id = u64::from_ne_bytes(bytes);
        let marker = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(registry.path(id)?)?;
        marker.try_lock()?;
        Ok(Session {
            profile,
            marker,
            id,
        })
    }

    fn remove(&self) -> io::Result<()> {
        let registry = self.profile.sessions()?;
        if self.marker.metadata()?.nlink() != 0 {
            fs::remove_file(registry.path(self.id)?)?;
        }
        Ok(())
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{profiles, removal, Access};

    #[test]
    fn stop_selects_one_live_session_and_exits_release_the_profile() -> io::Result<()> {
        let root = td_fs::TempDir::new("sandbox-sessions")?;
        let profiles = profiles(&root)?;
        let create = Profile::create(&profiles, "tools", None, None)?;
        let open = || Profile::open(&profiles, "tools", Access::Session);
        assert!(open().is_err());
        drop(create);
        let first = open()?;
        let second = open()?;
        let a = Session::register(&first)?;
        let b = Session::register(&second)?;
        let a_id = format!("{:016x}", a.id);
        let b_id = format!("{:016x}", b.id);
        let registry = first.sessions()?;
        let error = registry
            .stop("tools", None)
            .err()
            .ok_or_else(|| io::Error::other("an ambiguous stop succeeded"))?
            .to_string();
        assert!(error.contains(&a_id) && error.contains(&b_id));
        assert!(registry.stop("tools", Some("../lock")).is_err());
        assert_eq!(a.marker.metadata()?.nlink(), 1);
        assert_eq!(b.marker.metadata()?.nlink(), 1);
        assert!(removal(&profiles, "tools").is_err());
        assert!(Profile::open(&profiles, "tools", Access::Change).is_err());
        registry.stop("tools", Some(&a_id))?;
        assert_eq!(a.marker.metadata()?.nlink(), 0);
        assert_eq!(b.marker.metadata()?.nlink(), 1);
        assert!(registry.stop("tools", Some(&a_id)).is_err());
        assert_eq!(registry.active()?, [b.id]);
        drop(registry);
        drop(a);
        drop(first);
        assert!(removal(&profiles, "tools").is_err());
        second.sessions()?.stop("tools", None)?;
        assert_eq!(b.marker.metadata()?.nlink(), 0);
        drop(b);
        assert!(second.sessions()?.stop("tools", None).is_err());
        // After a crash the pathname survives but the kernel releases the lock.
        let dir = profiles.join("tools");
        let stale = dir.join("session-0000000000000001");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&stale)?;
        let c = Session::register(&second)?;
        assert!(!stale.exists());
        let id = c.id;
        drop(c);
        assert!(!dir.join(format!("session-{id:016x}")).exists());
        drop(second);
        removal(&profiles, "tools")?
            .ok_or_else(|| io::Error::other("lost sandbox record"))?
            .remove()?;
        assert!(open().is_err());
        Ok(())
    }
}
