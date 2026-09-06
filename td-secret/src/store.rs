//! File-backed increment. The owner uid can read the master; jail mounts cannot.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::crypto;

pub const MAX_SECRET: usize = 4096;
const MAGIC: &[u8; 8] = b"TDSEC001";
const O_NOFOLLOW: i32 = 0o400000;
const O_DIRECTORY: i32 = 0o200000;
const O_NONBLOCK: i32 = 0o4000;

pub fn user_path(uid: u32) -> PathBuf {
    PathBuf::from(format!("/var/lib/td/secrets/{uid}"))
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn target(value: &str) -> Result<(&str, &str), String> {
    let (app, name) = value
        .split_once('/')
        .ok_or("credential must be APPLICATION/NAME")?;
    if !valid_name(app) || !valid_name(name) {
        return Err("invalid credential application or name".into());
    }
    Ok((app, name))
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    File::open("/dev/random")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| format!("credential entropy: {e}"))?;
    Ok(bytes)
}

fn pinned(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
}

fn private_metadata(file: &File, uid: u32, mode: u32, dir: bool) -> Result<(), String> {
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if metadata.uid() != uid
        || metadata.mode() & 0o7777 != mode
        || (dir && !metadata.is_dir())
        || (!dir && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err("credential store has an invalid owner, mode, type, or link count".into());
    }
    Ok(())
}

pub struct Store {
    directory: File,
    lock: File,
    uid: u32,
}

impl Store {
    /// Only firstboot creates the uid leaf. All ancestors must already exist.
    pub fn open(path: &Path, uid: u32, create: bool) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("credential store path must be absolute".into());
        }
        let components = path.components().collect::<Vec<_>>();
        let mut parent = directory(Path::new("/")).map_err(|e| e.to_string())?;
        for (index, component) in components.iter().enumerate().skip(1) {
            let Component::Normal(name) = component else {
                return Err("invalid credential store path".into());
            };
            let child = pinned(&parent).join(name);
            let last = index + 1 == components.len();
            let created = if last && create {
                match fs::DirBuilder::new().mode(0o700).create(&child) {
                    Ok(()) => true,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(e) => return Err(format!("create credential store: {e}")),
                }
            } else {
                false
            };
            let opened =
                directory(&child).map_err(|e| format!("open credential store directory: {e}"))?;
            if created {
                std::os::unix::fs::fchown(&opened, Some(uid), None).map_err(|e| e.to_string())?;
                opened
                    .set_permissions(fs::Permissions::from_mode(0o700))
                    .and_then(|()| opened.sync_all())
                    .map_err(|e| e.to_string())?;
                parent.sync_all().map_err(|e| e.to_string())?;
            }
            if last {
                private_metadata(&opened, uid, 0o700, true)?;
            }
            parent = opened;
        }
        private_metadata(&parent, uid, 0o700, true)?;
        let lock_path = pinned(&parent).join("lock");
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => {
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(|e| e.to_string())?;
                std::os::unix::fs::fchown(&file, Some(uid), None).map_err(|e| e.to_string())?;
                file
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(lock_path)
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(format!("open credential lock: {e}")),
        };
        private_metadata(&lock, uid, 0o600, false)?;
        lock.try_lock()
            .map_err(|e| format!("lock credential store: {e}"))?;
        let store = Self {
            directory: parent,
            lock,
            uid,
        };
        match store.read("master", 32) {
            Ok(mut bytes) if bytes.len() == 32 => bytes.fill(0),
            Ok(_) => return Err("invalid credential master length".into()),
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                // Never rekey a store that already contains ciphertext.
                for entry in fs::read_dir(pinned(&store.directory)).map_err(|e| e.to_string())? {
                    if entry.map_err(|e| e.to_string())?.file_name() != "lock" {
                        return Err("missing master in a nonempty credential store".into());
                    }
                }
                store.write("master", &random::<32>()?)?;
            }
            Err(e) => return Err(format!("read credential master: {e}")),
        }
        Ok(store)
    }

    fn read(&self, name: &str, max: usize) -> io::Result<Vec<u8>> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(pinned(&self.directory).join(name))?;
        private_metadata(&file, self.uid, 0o600, false).map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        file.take((max + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > max {
            return Err(io::Error::other("oversized credential record"));
        }
        Ok(bytes)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let suffix = random::<16>()?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = pinned(&self.directory).join(format!("tmp-{suffix}"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("create credential record: {e}"))?;
        let result = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
            std::os::unix::fs::fchown(&file, Some(self.uid), None).map_err(|e| e.to_string())?;
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(&path, pinned(&self.directory).join(name)).map_err(|e| e.to_string())?;
            self.directory.sync_all().map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&path);
        }
        result
    }

    fn key(&self, app: &str) -> Result<[u8; 32], String> {
        let mut bytes = self.read("master", 32).map_err(|e| e.to_string())?;
        let mut master: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| "invalid credential master length")?;
        let key = crypto::derive(&master, app);
        bytes.fill(0);
        master.fill(0);
        Ok(key)
    }

    fn record(app: &str, name: &str) -> Result<(String, Vec<u8>), String> {
        if !valid_name(app) || !valid_name(name) {
            return Err("invalid credential identity".into());
        }
        let name = format!("{app}.{name}");
        let aad = format!("td-secret/record/v1/{app}/{name}").into_bytes();
        Ok((name, aad))
    }

    pub fn get(&self, app: &str, name: &str) -> Result<Option<Vec<u8>>, String> {
        let (file, aad) = Self::record(app, name)?;
        let bytes = match self.read(&file, MAX_SECRET + 36) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("read credential: {e}")),
        };
        if bytes.get(..8) != Some(MAGIC.as_slice()) {
            return Err("invalid credential record format".into());
        }
        let nonce = bytes
            .get(8..20)
            .and_then(|s| s.try_into().ok())
            .ok_or("truncated credential nonce")?;
        let sealed = bytes.get(20..).ok_or("truncated credential record")?;
        let mut key = self.key(app)?;
        let result = crypto::open(&key, &nonce, &aad, sealed).map(Some);
        key.fill(0);
        result
    }

    pub fn set(&self, app: &str, name: &str, plaintext: &[u8]) -> Result<(), String> {
        if plaintext.is_empty() || plaintext.len() > MAX_SECRET {
            return Err("credential must contain 1..4096 bytes".into());
        }
        let (file, aad) = Self::record(app, name)?;
        let nonce = random::<12>()?;
        let mut key = self.key(app)?;
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&crypto::seal(&key, &nonce, &aad, plaintext));
        key.fill(0);
        self.write(&file, &bytes)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_modes_symlinks_truncated_records_and_busy_store_are_refused() {
        let base = std::env::temp_dir().join(format!("td-secret-metadata-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("store");
        let uid = fs::metadata(&base).unwrap().uid();
        let store = Store::open(&path, uid, true).unwrap();
        store.set("mail", "main", b"credential").unwrap();
        assert!(Store::open(&path, uid, false).is_err());
        let record = path.join("mail.main");
        fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.get("mail", "main").is_err());
        fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&record, b"TDSEC001").unwrap();
        assert!(store.get("mail", "main").is_err());
        fs::remove_file(&record).unwrap();
        std::os::unix::fs::symlink(path.join("master"), &record).unwrap();
        assert!(store.get("mail", "main").is_err());
        drop(store);
        std::os::unix::fs::symlink(&path, base.join("alias")).unwrap();
        assert!(Store::open(&base.join("alias"), uid, false).is_err());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn records_survive_reopen_are_private_and_authenticate_identity() {
        let base = std::env::temp_dir().join(format!("td-secret-test-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("store");
        let uid = fs::metadata(&base).unwrap().uid();
        {
            let store = Store::open(&path, uid, true).unwrap();
            store.set("mail", "main", b"test credential").unwrap();
            assert_eq!(
                store.get("mail", "main").unwrap(),
                Some(b"test credential".to_vec())
            );
            assert_eq!(store.get("news", "main").unwrap(), None);
            assert!(!fs::read(path.join("mail.main"))
                .unwrap()
                .windows(15)
                .any(|b| b == b"test credential"));
            assert!(store.set("../mail", "main", b"x").is_err());
            assert!(store.set("mail", "main", &[0; MAX_SECRET + 1]).is_err());
        }
        {
            let store = Store::open(&path, uid, false).unwrap();
            assert_eq!(
                store.get("mail", "main").unwrap(),
                Some(b"test credential".to_vec())
            );
            fs::copy(path.join("mail.main"), path.join("news.main")).unwrap();
            assert!(store.get("news", "main").is_err());
        }
        fs::remove_file(path.join("master")).unwrap();
        assert!(Store::open(&path, uid, true).is_err());
        fs::remove_dir_all(base).unwrap();
    }
}
