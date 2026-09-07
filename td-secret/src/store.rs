//! Per-user encrypted records, with an explicitly enrolled TPM backend.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::{crypto, tpm};
use std::collections::BTreeMap;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Open,
    Create,
    Inspect,
}

pub struct Store {
    directory: File,
    lock: File,
    uid: u32,
    file_owner: u32,
}

/// Firstboot holds the root-owned secrets parent before the human session starts.
/// A root-owned leaf is the restartable intermediate state of this transfer.
#[derive(Debug)]
pub struct MigrationFailure {
    pub quarantined: bool,
    pub message: String,
}

pub fn migrate_owner(parent: &File, uid: u32, owner: u32) -> Result<(), MigrationFailure> {
    let mut quarantined = false;
    migrate_owner_inner(parent, uid, owner, &mut quarantined)
        .map_err(|message| MigrationFailure { quarantined, message })
}

fn migrate_owner_inner(parent: &File, uid: u32, owner: u32, quarantined: &mut bool) -> Result<(), String> {
    require_root()?;
    if !(1000..=65533).contains(&uid) || !(1..=999).contains(&owner) {
        return Err("invalid credential service ownership assignment".into());
    }
    private_metadata(parent, 0, 0o755, true)?;
    let path = pinned(parent).join(uid.to_string());
    let directory = match directory(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("open credential ownership migration: {e}")),
    };
    let metadata = directory.metadata().map_err(|e| e.to_string())?;
    if ![0, uid, owner].contains(&metadata.uid()) {
        return Err("credential directory has an invalid migration identity".into());
    }
    let already_published = metadata.uid() == owner;
    // Restrict traversal before inspecting children. No user process from the
    // preceding boot survives this sysinit migration; root remains trusted.
    directory.set_permissions(fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    std::os::unix::fs::fchown(&directory, Some(0), Some(0)).map_err(|e| e.to_string())?;
    directory.sync_all().map_err(|e| e.to_string())?;
    *quarantined = true;
    let lock_path = pinned(&directory).join("lock");
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(file) => {
            file.set_permissions(fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
            file
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(lock_path)
            .map_err(|e| e.to_string())?,
        Err(e) => return Err(format!("open credential ownership lock: {e}")),
    };
    let metadata = lock.metadata().map_err(|e| e.to_string())?;
    let mode = metadata.mode() & 0o7777;
    private_metadata(&lock, metadata.uid(), mode, false)?;
    if ![0, uid, owner].contains(&metadata.uid()) || metadata.len() != 0 || mode & !0o600 != 0 {
        return Err("credential ownership lock has an invalid identity or length".into());
    }
    // An interrupted empty-lock creation may have only umask-masked owner bits.
    lock.set_permissions(fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    lock.try_lock().map_err(|e| format!("lock credential ownership migration: {e}"))?;
    let result = (|| {
        let mut files = Vec::new();
        for entry in fs::read_dir(pinned(&directory)).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().into_string().map_err(|_| "invalid credential filename")?;
            if name == "lock" {
                continue;
            }
            let record = name.split_once('.').is_some_and(|(app, name)| valid_name(app) && valid_name(name));
            let temporary = name.strip_prefix("tmp-").is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            });
            if name != "master" && name != "sealed" && !record && !temporary {
                return Err("unknown entry in credential ownership migration".into());
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(pinned(&directory).join(&name))
                .map_err(|e| e.to_string())?;
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            let mode = metadata.mode() & 0o7777;
            let interrupted_root = temporary && metadata.uid() == 0 && metadata.len() == 0
                && mode & !0o600 == 0;
            private_metadata(&file, metadata.uid(), if interrupted_root { mode } else { 0o600 }, false)?;
            if (!interrupted_root && metadata.uid() != owner && (already_published || metadata.uid() != uid))
                || metadata.len() > MAX_BUNDLE as u64
            {
                return Err("credential file has an invalid migration identity or length".into());
            }
            files.push((file, name, interrupted_root));
            if files.len() > MAX_ENTRIES + 32 {
                return Err("too many files in credential ownership migration".into());
            }
        }
        // Validate and retain every inode before changing any file ownership.
        for (file, name, interrupted_root) in &files {
            if *interrupted_root {
                // atomic_write has not written bytes before its ownership switch.
                fs::remove_file(pinned(&directory).join(name)).map_err(|e| e.to_string())?;
            } else {
                std::os::unix::fs::fchown(file, Some(owner), Some(owner)).map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
            }
        }
        std::os::unix::fs::fchown(&lock, Some(owner), Some(owner)).map_err(|e| e.to_string())?;
        lock.sync_all().map_err(|e| e.to_string())?;
        directory.sync_all().map_err(|e| e.to_string())?;
        std::os::unix::fs::fchown(&directory, Some(owner), Some(owner)).map_err(|e| e.to_string())?;
        *quarantined = false;
        directory.sync_all().map_err(|e| e.to_string())?;
        parent.sync_all().map_err(|e| e.to_string())
    })();
    let unlocked = lock.unlock().map_err(|e| format!("unlock credential ownership migration: {e}"));
    result.and(unlocked)
}

impl Store {
    fn bundle(&self) -> Result<Option<Bundle>, String> {
        match self.read("sealed", MAX_BUNDLE) {
            Ok(bytes) => {
                let bundle = Bundle::decode(&bytes, self.uid)?;
                Ok(Some(bundle))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read sealed credential store: {e}")),
        }
    }

    #[cfg(test)]
    fn seal_with(
        &self,
        seal: impl FnOnce(&[u8; 32]) -> Result<Vec<u8>, String>,
        unseal: impl FnOnce(&[u8]) -> Result<[u8; 32], String>,
    ) -> Result<(), String> {
        if self.bundle()?.is_some() {
            return Err("credential store is already TPM sealed".into());
        }
        self.rotate_with(seal, |blob, expected| {
            let mut verified = unseal(blob)?;
            let matches = &verified == expected;
            verified.fill(0);
            if !matches {
                return Err("TPM enrollment roundtrip changed the key".into());
            }
            Ok(())
        })
    }

    #[cfg(test)]
    fn rotate_with(
        &self,
        seal: impl FnOnce(&[u8; 32]) -> Result<Vec<u8>, String>,
        verify: impl FnOnce(&[u8], &[u8; 32]) -> Result<(), String>,
    ) -> Result<(), String> {
        let previous = self.bundle()?;
        self.rotate_using(previous.as_ref(), seal, verify, |app, name, bundle| {
            self.get_from(app, name, bundle)
        })
    }

    fn rotate_using(
        &self,
        previous: Option<&Bundle>,
        seal: impl FnOnce(&[u8; 32]) -> Result<Vec<u8>, String>,
        verify: impl FnOnce(&[u8], &[u8; 32]) -> Result<(), String>,
        read: impl Fn(&str, &str, Option<&Bundle>) -> Result<Option<Vec<u8>>, String>,
    ) -> Result<(), String> {
        let legacy = self.legacy_names()?;
        let names = match previous {
            Some(bundle) => bundle.records.keys().cloned().collect(),
            None => legacy,
        };
        let mut master = random::<32>()?;
        let result = (|| {
            let key = seal(&master)?;
            validate_key(&key, self.uid)?;
            verify(&key, &master)?;
            let mut records = BTreeMap::new();
            for file in &names {
                let Some((app, name)) = file.split_once('.') else {
                    continue;
                };
                let (_, aad) = Self::record(app, name)?;
                let nonce = random::<12>()?;
                let mut secret = read(app, name, previous)?
                    .ok_or("credential disappeared during sealing")?;
                let mut derived = crypto::derive(&master, app);
                let mut record = MAGIC.to_vec();
                record.extend_from_slice(&nonce);
                record.extend_from_slice(&crypto::seal(&derived, &nonce, &aad, &secret));
                derived.fill(0);
                secret.fill(0);
                records.insert(file.clone(), record);
            }
            let bundle = Bundle { key, records };
            let bytes = bundle.encode()?;
            Bundle::decode(&bytes, self.uid)?;
            // This is the only backend-selection commit point.
            self.write("sealed", &bytes)?;
            self.retire_legacy()
        })();
        master.fill(0);
        result
    }

    fn legacy_names(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(pinned(&self.directory)).map_err(|e| e.to_string())? {
            let name = entry
                .map_err(|e| e.to_string())?
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF8 credential store entry")?;
            if name == "lock" || name == "sealed" {
                continue;
            }
            let record = name
                .split_once('.')
                .is_some_and(|(app, name)| valid_name(app) && valid_name(name));
            let temporary = name.strip_prefix("tmp-").is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            });
            if name != "master" && !record && !temporary {
                return Err("unknown entry in credential store; refusing migration".into());
            }
            // Refuse planted links and nonregular entries even during cleanup.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(pinned(&self.directory).join(&name))
                .map_err(|e| e.to_string())?;
            private_metadata(&file, self.file_owner, 0o600, false)?;
            if file.metadata().map_err(|e| e.to_string())?.len() > MAX_BUNDLE as u64 {
                return Err("oversized credential store entry".into());
            }
            names.push(name);
            if names.len() > MAX_ENTRIES + 32 {
                return Err("too many credential store entries".into());
            }
        }
        Ok(names)
    }

    fn retire_legacy(&self) -> Result<(), String> {
        for name in self.legacy_names()? {
            fs::remove_file(pinned(&self.directory).join(name)).map_err(|e| e.to_string())?;
        }
        self.directory.sync_all().map_err(|e| e.to_string())
    }

    #[cfg(test)]
    fn release_into(
        &self,
        bundle: &Bundle,
        runtime: &File,
        unseal: impl FnOnce(&[u8]) -> Result<[u8; 32], String>,
    ) -> Result<(), String> {
        lock_runtime(runtime)?;
        self.publish_release(bundle, runtime, unseal)
    }

    fn publish_release(
        &self,
        bundle: &Bundle,
        runtime: &File,
        unseal: impl FnOnce(&[u8]) -> Result<[u8; 32], String>,
    ) -> Result<(), String> {
        let mut master = unseal(&bundle.key)?;
        let result = (|| {
            self.retire_legacy()?;
            let mut bytes = crypto::digest(&bundle.key).to_vec();
            bytes.extend_from_slice(&master);
            let result = atomic_write(runtime, self.file_owner, "key", &bytes);
            bytes.fill(0);
            result
        })();
        master.fill(0);
        result
    }

    /// A structural snapshot only: this never authenticates or releases a key.
    pub fn inspect_owned(path: &Path, uid: u32, owner: u32) -> Result<u8, String> {
        let (_store, bundle) = Self::open_mode(path, uid, owner, OpenMode::Inspect)?;
        let Some(bundle) = bundle else { return Ok(0) };
        if !bundle.token_protected() {
            return Ok(1);
        }
        let protection = super::fido_metadata::Protection::decode(&bundle.key, uid)?;
        Ok(if protection.has_recovery() { 3 } else { 2 })
    }

    pub fn token_protected(&self) -> Result<bool, String> {
        Ok(self.bundle()?.is_some_and(|bundle| bundle.token_protected()))
    }

    pub fn sealed(&self) -> Result<bool, String> {
        Ok(self.bundle()?.is_some())
    }

    /// Firstboot never releases a key, including for legacy platform stores.
    pub fn prepare_boot(&self) -> Result<(), String> {
        if self.bundle()?.is_none() { return Ok(()); }
        require_root()?;
        lock_runtime(&runtime_directory(self.uid, true)?)?;
        self.retire_legacy()?;
        eprintln!("td-secret: session {} remains locked pending secure attention", self.uid);
        Ok(())
    }

    /// Only a token-protected and currently released store serves applications.
    pub fn application_secret(&self, app: &str, name: &str) -> Result<Option<Vec<u8>>, String> {
        let bundle = self.bundle()?.ok_or("credential store requires token enrollment")?;
        if !bundle.token_protected() { return Err("credential store requires token enrollment".into()); }
        self.get_from(app, name, Some(&bundle))
    }

    /// The caller must have completed enrollment on the trusted input path.
    pub fn enroll_tokens(
        &self,
        metadata: super::fido_metadata::Metadata,
        pcrs: tpm::Pcrs,
    ) -> Result<(), String> {
        require_root()?;
        let runtime = runtime_directory(self.uid, true)?;
        self.enroll_tokens_into(metadata, pcrs, &runtime, || {
            Ok(tpm::Client::new(tpm::Device::open()?))
        })
    }

    fn enroll_tokens_into<T: tpm::Transport>(
        &self,
        metadata: super::fido_metadata::Metadata,
        pcrs: tpm::Pcrs,
        runtime: &File,
        client: impl Fn() -> Result<tpm::Client<T>, String>,
    ) -> Result<(), String> {
        lock_runtime(runtime)?;
        let mut previous_master = None;
        let rotated = (|| {
            let previous = self.bundle()?;
            if previous.as_ref().is_some_and(Bundle::token_protected) {
                return Err("credential store is already token enrolled".into());
            }
            let metadata = super::fido_metadata::Metadata::decode(&metadata.encode()?, self.uid)?;
            let binding = metadata.binding()?;
            if let Some(bundle) = &previous {
                // The old platform-only key stays private during migration.
                previous_master = Some(client()?.unseal(&tpm::SealedKey::decode(&bundle.key)?)?);
            }
            self.rotate_using(
                previous.as_ref(),
                |master| {
                    let key = client()?.seal_bound(self.uid, pcrs, master, &binding)?;
                    super::fido_metadata::Protection::new(self.uid, metadata, key)?.encode()
                },
                |blob, master| {
                    super::fido_metadata::Protection::decode(blob, self.uid)?
                        .verify_sealed_master(master, client()?)
                },
                |app, name, bundle| {
                    self.get_from_key(app, name, bundle, || match &previous_master {
                        Some(master) => Ok(crypto::derive(master, app)),
                        None => self.key(app, bundle),
                    })
                },
            )
        })();
        if let Some(master) = &mut previous_master {
            master.fill(0);
        }
        // Publication may have succeeded even if directory sync or cleanup failed.
        let locked = lock_runtime(runtime);
        match (rotated, locked) {
            (Err(error), Err(cleanup)) => Err(format!("{error}; lock after enrollment: {cleanup}")),
            (result, locked) => result.and(locked),
        }
    }

    /// The caller obtains the fresh challenge only after trusted presentation.
    pub fn token_request(
        &self,
        role: super::fido_metadata::Role,
        challenge: [u8; 32],
        info: &super::fido_enroll::Info,
    ) -> Result<super::fido_metadata::BoundRelease, String> {
        require_root()?;
        lock_runtime(&runtime_directory(self.uid, true)?)?;
        let bundle = self.bundle()?.ok_or("credential store is not token enrolled")?;
        if !bundle.token_protected() {
            return Err("credential store is not token enrolled".into());
        }
        let protection = super::fido_metadata::Protection::decode(&bundle.key, self.uid)?;
        protection.request(role, challenge, info)
    }

    pub fn release_token<T: tpm::Transport>(
        &self,
        request: super::fido_metadata::BoundRelease,
        response: &[u8],
        client: tpm::Client<T>,
    ) -> Result<(), String> {
        require_root()?;
        let runtime = runtime_directory(self.uid, true)?;
        self.release_token_into(&runtime, request, response, client)
    }

    fn release_token_into<T: tpm::Transport>(
        &self,
        runtime: &File,
        request: super::fido_metadata::BoundRelease,
        response: &[u8],
        client: tpm::Client<T>,
    ) -> Result<(), String> {
        lock_runtime(runtime)?;
        let bundle = self.bundle()?.ok_or("credential store is not token enrolled")?;
        if !bundle.token_protected() {
            return Err("credential store is not token enrolled".into());
        }
        if &crypto::digest(&bundle.key) != request.protection_id() {
            return Err("credential protector changed during token release".into());
        }
        self.publish_release(&bundle, runtime, |_| request.unseal(response, client))
    }

    /// Commit one admitted write without publishing a session release.
    pub fn set_token<T: tpm::Transport>(
        &self,
        app: &str,
        name: &str,
        plaintext: &[u8],
        request: super::fido_metadata::BoundRelease,
        response: &[u8],
        client: tpm::Client<T>,
    ) -> Result<(), String> {
        require_root()?;
        let protector = *request.protection_id();
        with_memory_state(
            fs::read_to_string("/proc/swaps"),
            fs::read_to_string("/proc/self/limits"),
            || {
                self.set_token_with(app, name, plaintext, &protector, || {
                    request.unseal(response, client)
                })
            },
        )
    }

    fn set_token_with(
        &self,
        app: &str,
        name: &str,
        plaintext: &[u8],
        protector: &[u8; 32],
        unseal: impl FnOnce() -> Result<[u8; 32], String>,
    ) -> Result<(), String> {
        if plaintext.is_empty() || plaintext.len() > MAX_SECRET {
            return Err(format!("credential must contain 1..{MAX_SECRET} bytes"));
        }
        let (file, aad) = Self::record(app, name)?;
        let mut bundle = self
            .bundle()?
            .ok_or("credential store is not token enrolled")?;
        if !bundle.token_protected() {
            return Err("credential store is not token enrolled".into());
        }
        if &crypto::digest(&bundle.key) != protector {
            return Err("credential protector changed during token write".into());
        }
        if bundle.records.len() >= MAX_ENTRIES && !bundle.records.contains_key(&file) {
            return Err("credential store has no free record slot".into());
        }
        let mut master = unseal()?;
        let mut key = crypto::derive(&master, app);
        master.fill(0);
        let result = (|| {
            let nonce = random::<12>()?;
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&nonce);
            bytes.extend_from_slice(&crypto::seal(&key, &nonce, &aad, plaintext));
            bundle.records.insert(file, bytes);
            self.write("sealed", &bundle.encode()?)
        })();
        key.fill(0);
        result
    }

    /// Only firstboot creates the uid leaf. All ancestors must already exist.
    pub fn open(path: &Path, uid: u32, create: bool) -> Result<Self, String> {
        Self::open_owned(path, uid, uid, create)
    }

    /// TPM identity remains the human session when a service owns the files.
    pub fn open_owned(
        path: &Path,
        uid: u32,
        file_owner: u32,
        create: bool,
    ) -> Result<Self, String> {
        Self::open_mode(
            path,
            uid,
            file_owner,
            if create {
                OpenMode::Create
            } else {
                OpenMode::Open
            },
        )
        .map(|(store, _)| store)
    }

    fn open_mode(
        path: &Path,
        uid: u32,
        file_owner: u32,
        mode: OpenMode,
    ) -> Result<(Self, Option<Bundle>), String> {
        if !path.is_absolute() {
            return Err("credential store path must be absolute".into());
        }
        let components = path.components().collect::<Vec<_>>();
        let mut parent = directory(Path::new("/")).map_err(|e| e.to_string())?;
        let mut created_leaf = false;
        for (index, component) in components.iter().enumerate().skip(1) {
            let Component::Normal(name) = component else {
                return Err("invalid credential store path".into());
            };
            let child = pinned(&parent).join(name);
            let last = index + 1 == components.len();
            let created = if last && mode == OpenMode::Create {
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
                created_leaf = true;
                std::os::unix::fs::fchown(&opened, Some(file_owner), None)
                    .map_err(|e| e.to_string())?;
                opened
                    .set_permissions(fs::Permissions::from_mode(0o700))
                    .and_then(|()| opened.sync_all())
                    .map_err(|e| e.to_string())?;
                parent.sync_all().map_err(|e| e.to_string())?;
            }
            if last {
                private_metadata(&opened, file_owner, 0o700, true)?;
            }
            parent = opened;
        }
        private_metadata(&parent, file_owner, 0o700, true)?;
        let lock_path = pinned(&parent).join("lock");
        let lock = if mode == OpenMode::Inspect {
            OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(&lock_path)
                .map_err(|e| format!("open credential lock: {e}"))?
        } else {
            match OpenOptions::new()
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
                    std::os::unix::fs::fchown(&file, Some(file_owner), None)
                        .map_err(|e| e.to_string())?;
                    file
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                    .open(lock_path)
                    .map_err(|e| e.to_string())?,
                Err(e) => return Err(format!("open credential lock: {e}")),
            }
        };
        private_metadata(&lock, file_owner, 0o600, false)?;
        lock.try_lock()
            .map_err(|e| format!("lock credential store: {e}"))?;
        let store = Self {
            directory: parent,
            lock,
            uid,
            file_owner,
        };
        if let Some(bundle) = store.bundle()? {
            return Ok((store, Some(bundle)));
        }
        match store.read("master", 32) {
            Ok(mut bytes) if bytes.len() == 32 => bytes.fill(0),
            Ok(_) => return Err("invalid credential master length".into()),
            Err(e) if created_leaf && e.kind() == io::ErrorKind::NotFound => {
                // Only a newly created leaf may receive its first master.
                for entry in fs::read_dir(pinned(&store.directory)).map_err(|e| e.to_string())? {
                    if entry.map_err(|e| e.to_string())?.file_name() != "lock" {
                        return Err("missing master in a nonempty credential store".into());
                    }
                }
                store.write("master", &random::<32>()?)?;
            }
            Err(e) => return Err(format!("read credential master: {e}")),
        }
        Ok((store, None))
    }

    fn read(&self, name: &str, max: usize) -> io::Result<Vec<u8>> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(pinned(&self.directory).join(name))?;
        private_metadata(&file, self.file_owner, 0o600, false).map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        file.take((max + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > max {
            return Err(io::Error::other("oversized credential record"));
        }
        Ok(bytes)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        atomic_write(&self.directory, self.file_owner, name, bytes)
    }

    fn key(&self, app: &str, bundle: Option<&Bundle>) -> Result<[u8; 32], String> {
        let mut bytes = if let Some(bundle) = bundle {
            runtime_key(self.uid, self.file_owner, &bundle.key)?.to_vec()
        } else {
            self.read("master", 32).map_err(|e| e.to_string())?
        };
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
        let bundle = self.bundle()?;
        self.get_from(app, name, bundle.as_ref())
    }

    fn get_from(&self, app: &str, name: &str, bundle: Option<&Bundle>) -> Result<Option<Vec<u8>>, String> {
        self.get_from_key(app, name, bundle, || self.key(app, bundle))
    }

    fn get_from_key(
        &self,
        app: &str,
        name: &str,
        bundle: Option<&Bundle>,
        key: impl FnOnce() -> Result<[u8; 32], String>,
    ) -> Result<Option<Vec<u8>>, String> {
        let (file, aad) = Self::record(app, name)?;
        let bytes = if let Some(bundle) = bundle {
            match bundle.records.get(&file) {
                Some(bytes) => bytes.clone(),
                None => return Ok(None),
            }
        } else {
            match self.read(&file, MAX_SECRET + 36) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(format!("read credential: {e}")),
            }
        };
        if bytes.get(..8) != Some(MAGIC.as_slice()) {
            return Err("invalid credential record format".into());
        }
        let nonce = bytes
            .get(8..20)
            .and_then(|s| s.try_into().ok())
            .ok_or("truncated credential nonce")?;
        let sealed = bytes.get(20..).ok_or("truncated credential record")?;
        let mut key = key()?;
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
        let mut bundle = self.bundle()?;
        let mut key = self.key(app, bundle.as_ref())?;
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&crypto::seal(&key, &nonce, &aad, plaintext));
        key.fill(0);
        if let Some(bundle) = bundle.as_mut() {
            bundle.records.insert(file, bytes);
            self.write("sealed", &bundle.encode()?)
        } else {
            self.write(&file, &bytes)
        }
    }
}

/// Clear a session release before attempting to open untrusted persistent bytes.
pub fn lock_session(uid: u32) -> Result<(), String> {
    require_root()?;
    lock_runtime(&runtime_directory(uid, true)?)
}

fn lock_runtime(runtime: &File) -> Result<(), String> {
    // Remove the key first: an unrelated malformed entry cannot retain it.
    match fs::remove_file(pinned(runtime).join("key")) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(format!("lock credential runtime: {error}")),
    }
    for entry in fs::read_dir(pinned(runtime)).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("invalid credential runtime entry")?;
        if !name.strip_prefix("tmp-").is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err("unknown credential runtime entry".into());
        }
        fs::remove_file(entry.path()).map_err(|e| format!("lock credential runtime: {e}"))?;
    }
    Ok(())
}

const MAX_ENTRIES: usize = 128;
const MAX_BUNDLE: usize = 600_000;

struct Bundle {
    key: Vec<u8>,
    records: BTreeMap<String, Vec<u8>>,
}
impl Bundle {
    fn token_protected(&self) -> bool {
        self.key.starts_with(b"TDFIDO01")
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        if self.records.len() > MAX_ENTRIES {
            return Err("sealed store holds at most 128 credentials".into());
        }
        let mut bytes = if self.token_protected() { b"TDSEAL02" } else { b"TDSEAL01" }.to_vec();
        field(&mut bytes, &self.key)?;
        bytes.extend_from_slice(&(self.records.len() as u32).to_be_bytes());
        for (name, record) in &self.records {
            field(&mut bytes, name.as_bytes())?;
            field(&mut bytes, record)?;
        }
        if bytes.len() > MAX_BUNDLE {
            return Err("oversized sealed store".into());
        }
        Ok(bytes)
    }
    fn decode(mut bytes: &[u8], expected_uid: u32) -> Result<Self, String> {
        if bytes.len() > MAX_BUNDLE {
            return Err("oversized sealed store".into());
        }
        let token = match take(&mut bytes, 8)? {
            b"TDSEAL01" => false,
            b"TDSEAL02" => true,
            _ => return Err("invalid sealed store format".into()),
        };
        let limit = if token { super::fido_metadata::MAX_PROTECTION } else { tpm::MAX_PACKET };
        let key = read_field(&mut bytes, limit)?.to_vec();
        if token != key.starts_with(b"TDFIDO01") {
            return Err("sealed store and protector versions disagree".into());
        }
        validate_key(&key, expected_uid)?;
        let count = number(&mut bytes)?;
        if count > MAX_ENTRIES {
            return Err("too many sealed credentials".into());
        }
        let mut records = BTreeMap::new();
        for _ in 0..count {
            let name = std::str::from_utf8(read_field(&mut bytes, 129)?)
                .map_err(|_| "invalid credential name")?;
            let (app, entry) = name.split_once('.').ok_or("invalid credential name")?;
            Store::record(app, entry)?;
            let record = read_field(&mut bytes, MAX_SECRET + 36)?;
            if record.len() < 37 || record.get(..8) != Some(MAGIC.as_slice()) {
                return Err("invalid sealed credential record".into());
            }
            if records.insert(name.to_string(), record.to_vec()).is_some() {
                return Err("duplicate sealed credential name".into());
            }
        }
        if !bytes.is_empty() {
            return Err("trailing sealed store data".into());
        }
        Ok(Self { key, records })
    }
}
fn validate_key(bytes: &[u8], uid: u32) -> Result<(), String> {
    if bytes.starts_with(b"TDFIDO01") {
        super::fido_metadata::Protection::decode(bytes, uid)?;
    } else if tpm::SealedKey::decode(bytes)?.uid != uid {
        return Err("sealed credential store belongs to another user".into());
    }
    Ok(())
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    out.extend_from_slice(
        &u32::try_from(bytes.len())
            .map_err(|_| "oversized store field")?
            .to_be_bytes(),
    );
    out.extend_from_slice(bytes);
    Ok(())
}
fn take<'a>(bytes: &mut &'a [u8], size: usize) -> Result<&'a [u8], String> {
    let head = bytes.get(..size).ok_or("truncated sealed store")?;
    *bytes = bytes.get(size..).ok_or("truncated sealed store")?;
    Ok(head)
}
fn number(bytes: &mut &[u8]) -> Result<usize, String> {
    Ok(u32::from_be_bytes(
        take(bytes, 4)?
            .try_into()
            .map_err(|_| "short store number")?,
    ) as usize)
}
fn read_field<'a>(bytes: &mut &'a [u8], max: usize) -> Result<&'a [u8], String> {
    let size = number(bytes)?;
    if size > max {
        return Err("oversized store field".into());
    }
    take(bytes, size)
}

pub fn require_root() -> Result<(), String> {
    let status = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    check_root(&status)
}

fn check_root(status: &str) -> Result<(), String> {
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or("missing process credentials")?
        .split_whitespace()
        .collect::<Vec<_>>();
    if ids != ["0", "0", "0", "0"] {
        return Err("TPM enrollment and release require the root console".into());
    }
    Ok(())
}

fn check_swap_state(swaps: io::Result<String>) -> Result<(), String> {
    let swaps = match swaps {
        Ok(swaps) => swaps,
        // Linux registers /proc/swaps only when CONFIG_SWAP is enabled.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    if swaps.lines().next().is_none() || swaps.lines().skip(1).any(|line| !line.trim().is_empty()) {
        return Err("TPM credential release requires swap to be disabled".into());
    }
    Ok(())
}

fn with_memory_state<T>(
    swaps: io::Result<String>,
    limits: io::Result<String>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    check_swap_state(swaps)?;
    check_core_limit(&limits.map_err(|e| e.to_string())?)?;
    operation()
}

fn runtime_directory(uid: u32, create: bool) -> Result<File, String> {
    with_memory_state(
        fs::read_to_string("/proc/swaps"),
        fs::read_to_string("/proc/self/limits"),
        || Ok(()),
    )?;
    // /run is an image-owned tmpfs. No environment variable can relocate keys.
    let mut mounts = String::new();
    File::open("/proc/self/mountinfo")
        .and_then(|file| file.take(1_048_577).read_to_string(&mut mounts))
        .map_err(|e| e.to_string())?;
    if mounts.len() > 1_048_576 {
        return Err("oversized mount table".into());
    }
    check_runtime_mount(&mounts)?;
    runtime_directory_at(Path::new("/run"), 0, uid, create)
}

fn check_core_limit(limits: &str) -> Result<(), String> {
    let core = limits
        .lines()
        .find_map(|line| line.strip_prefix("Max core file size"));
    if core.and_then(|line| line.split_whitespace().next()) != Some("0") {
        return Err("TPM credential access requires a zero core-dump soft limit".into());
    }
    Ok(())
}

fn runtime_directory_at(base: &Path, owner: u32, uid: u32, create: bool) -> Result<File, String> {
    let mut parent = directory(base).map_err(|e| e.to_string())?;
    private_metadata(&parent, owner, 0o755, true)?;
    for component in ["td-secret".to_string(), uid.to_string()] {
        let child = pinned(&parent).join(component);
        let created = if create {
            match fs::DirBuilder::new().mode(0o755).create(&child) {
                Ok(()) => true,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
                Err(e) => return Err(e.to_string()),
            }
        } else {
            false
        };
        let opened = directory(&child).map_err(|e| format!("credential store is locked: {e}"))?;
        if created && opened.metadata().map_err(|e| e.to_string())?.uid() == owner {
            opened
                .set_permissions(fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        private_metadata(&opened, owner, 0o755, true)?;
        parent = opened;
    }
    Ok(parent)
}

fn check_runtime_mount(mounts: &str) -> Result<(), String> {
    let mut found = false;
    for line in mounts.lines() {
        let (left, right) = line.split_once(" - ").ok_or("invalid mount table")?;
        let path = left.split_whitespace().nth(4).ok_or("invalid mount path")?;
        if path == "/run" {
            if right.split_whitespace().next() != Some("tmpfs") {
                return Err("credential runtime requires tmpfs at /run".into());
            }
            found = true;
        } else if path == "/run/td-secret" || path.starts_with("/run/td-secret/") {
            return Err("credential runtime has a nested mount".into());
        }
    }
    if !found {
        return Err("credential runtime requires a /run tmpfs mount".into());
    }
    Ok(())
}

fn runtime_key(uid: u32, file_owner: u32, sealed: &[u8]) -> Result<[u8; 32], String> {
    runtime_key_in(&runtime_directory(uid, false)?, file_owner, sealed)
}

fn runtime_key_in(runtime: &File, file_owner: u32, sealed: &[u8]) -> Result<[u8; 32], String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(pinned(runtime).join("key"))
        .map_err(|_| "TPM credential store is locked")?;
    private_metadata(&file, file_owner, 0o600, false)?;
    let mut bytes = Vec::new();
    file.take(65)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        if bytes.len() != 64 || bytes.get(..32) != Some(crypto::digest(sealed).as_slice()) {
            return Err("credential release belongs to another sealed key".into());
        }
        bytes
            .get(32..)
            .and_then(|key| key.try_into().ok())
            .ok_or_else(|| "invalid runtime key".into())
    })();
    bytes.fill(0);
    result
}

fn atomic_write(directory: &File, uid: u32, name: &str, bytes: &[u8]) -> Result<(), String> {
    let suffix = random::<16>()?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = pinned(directory).join(format!("tmp-{suffix}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("create credential record: {e}"))?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        std::os::unix::fs::fchown(&file, Some(uid), None).map_err(|e| e.to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&path, pinned(directory).join(name)).map_err(|e| e.to_string())?;
        directory.sync_all().map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decrypt(
        bundle: &Bundle,
        master: &[u8; 32],
        app: &str,
        name: &str,
    ) -> Result<Vec<u8>, String> {
        let (file, aad) = Store::record(app, name)?;
        let record = bundle.records.get(&file).ok_or("missing test credential")?;
        crypto::open(
            &crypto::derive(master, app),
            &record[8..20].try_into().unwrap(),
            &aad,
            &record[20..],
        )
    }

    /// Run only in a disposable root VM with TD_TEST_ROOT_BUSYBOX=/bin/busybox.
    #[test]
    #[ignore = "requires a disposable root VM and explicit busybox fixture"]
    fn ownership_transfer_preserves_bytes_and_removes_human_access() {
        use std::os::unix::process::CommandExt;
        require_root().unwrap();
        let busybox = std::env::var("TD_TEST_ROOT_BUSYBOX").unwrap();
        assert!(Path::new(&busybox).is_absolute());
        let root = std::env::temp_dir().join(format!("td-owner-migration-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let parent = directory(&root).unwrap();
        let make = |uid: u32| {
            let path = root.join(uid.to_string());
            let store = Store::open(&path, uid, true).unwrap();
            store.set("mail", "main", b"preserved credential").unwrap();
            drop(store);
            path
        };
        let read = |uid: u32, path: &Path| {
            std::process::Command::new(&busybox).arg("cat").arg(path)
                .uid(uid).gid(uid).output().unwrap()
        };
        let path = make(1000);
        let master = fs::read(path.join("master")).unwrap();
        let record = fs::read(path.join("mail.main")).unwrap();
        assert!(read(1000, &path.join("master")).status.success());
        migrate_owner(&parent, 1000, 991).unwrap();
        migrate_owner(&parent, 1000, 991).unwrap();
        for name in ["master", "mail.main", "lock"] {
            let file = path.join(name);
            assert_eq!(fs::metadata(&file).unwrap().uid(), 991);
            assert!(!read(1000, &file).status.success());
            assert!(!read(65536, &file).status.success());
        }
        assert_eq!(read(991, &path.join("master")).stdout, master);
        assert_eq!(fs::read(path.join("mail.main")).unwrap(), record);
        assert_eq!(Store::open_owned(&path, 1000, 991, false).unwrap()
            .get("mail", "main").unwrap().unwrap(), b"preserved credential");

        // A crash after the leaf restriction and one file transfer is resumable.
        let path = make(1001);
        std::os::unix::fs::chown(&path, Some(0), Some(0)).unwrap();
        std::os::unix::fs::chown(path.join("master"), Some(991), Some(991)).unwrap();
        migrate_owner(&parent, 1001, 991).unwrap();
        assert_eq!(Store::open_owned(&path, 1001, 991, false).unwrap()
            .get("mail", "main").unwrap().unwrap(), b"preserved credential");

        for (uid, bad) in [(1002, "unknown"), (1003, "symlink"), (1004, "hardlink"), (1005, "owner")] {
            let path = make(uid);
            match bad {
                "unknown" => fs::write(path.join("foreign"), b"foreign").unwrap(),
                "symlink" => std::os::unix::fs::symlink("master", path.join("mail.other")).unwrap(),
                "hardlink" => fs::hard_link(path.join("master"), path.join("mail.other")).unwrap(),
                _ => std::os::unix::fs::chown(path.join("mail.main"), Some(999), None).unwrap(),
            }
            assert!(migrate_owner(&parent, uid, 991).unwrap_err().quarantined);
            assert_eq!(fs::metadata(&path).unwrap().uid(), 0);
            assert_eq!(fs::metadata(path.join("master")).unwrap().uid(), uid);
            assert!(!read(uid, &path.join("master")).status.success());
        }
        let path = make(1006);
        let store = Store::open(&path, 1006, false).unwrap();
        assert!(migrate_owner(&parent, 1006, 991).is_err());
        drop(store);
        migrate_owner(&parent, 1006, 991).unwrap();
        for (uid, mode, published) in [(1007, 0o600, false), (1008, 0, false), (1009, 0o600, true), (1010, 0, true)] {
            let path = make(uid);
            if published { migrate_owner(&parent, uid, 991).unwrap(); }
            let scratch = path.join("tmp-0123456789abcdef0123456789abcdef");
            fs::write(&scratch, b"").unwrap();
            fs::set_permissions(&scratch, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(fs::metadata(&scratch).unwrap().uid(), 0);
            migrate_owner(&parent, uid, 991).unwrap();
            assert!(!scratch.exists());
            assert_eq!(Store::open_owned(&path, uid, 991, false).unwrap()
                .get("mail", "main").unwrap().unwrap(), b"preserved credential");
        }
        let path = make(1011);
        std::os::unix::fs::chown(path.join("lock"), Some(0), Some(0)).unwrap();
        fs::set_permissions(path.join("lock"), fs::Permissions::from_mode(0o000)).unwrap();
        migrate_owner(&parent, 1011, 991).unwrap();
        assert_eq!(Store::open_owned(&path, 1011, 991, false).unwrap()
            .get("mail", "main").unwrap().unwrap(), b"preserved credential");
        let path = make(1012);
        std::os::unix::fs::chown(&path, Some(999), Some(999)).unwrap();
        assert!(!migrate_owner(&parent, 1012, 991).unwrap_err().quarantined);
        assert_eq!(fs::metadata(&path).unwrap().uid(), 999);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn token_enrollment_is_one_atomic_rotation_and_never_uses_legacy_fallback() {
        let root = std::env::temp_dir().join(format!("td-token-store-{}-{}", std::process::id(), u64::from_le_bytes(random::<8>().unwrap())));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("store");
        let store = Store::open_owned(&path, 1000, owner, true).unwrap();
        store.set("mail", "main", b"mail credential").unwrap();
        store.set("news", "work", b"news credential").unwrap();
        let original = store.read("master", 32).unwrap();
        let protection = super::super::fido_metadata::tests::protection_fixture(1000, true).encode().unwrap();
        assert!(store.rotate_with(|_| Ok(protection.clone()), |_,_| Err("roundtrip refused".into())).is_err());
        assert_eq!(store.get("mail", "main").unwrap().unwrap(), b"mail credential");
        assert!(!path.join("sealed").exists());
        let master = std::cell::Cell::new([0;32]);
        store.rotate_with(|key| {master.set(*key); Ok(protection.clone())}, |_,_| Ok(())).unwrap();
        assert!(store.token_protected().unwrap());
        assert!(!path.join("master").exists());
        assert!(!path.join("mail.main").exists());
        assert!(!path.join("news.work").exists());
        let bundle = store.bundle().unwrap().unwrap();
        assert_eq!(decrypt(&bundle, &master.get(), "mail", "main").unwrap(), b"mail credential");
        assert_eq!(decrypt(&bundle, &master.get(), "news", "work").unwrap(), b"news credential");
        assert!(store.get("mail", "main").is_err());
        store.write("master", &original).unwrap();
        assert!(store.get("mail", "main").is_err());
        let encoded = bundle.encode().unwrap();
        assert_eq!(encoded.get(..8).unwrap(), b"TDSEAL02");
        assert!(!encoded.windows(32).any(|value| value == master.get()));
        assert!(!encoded.windows(15).any(|value| value == b"mail credential"));
        let mut downgraded = encoded.clone();downgraded[7] = b'1';
        assert!(Bundle::decode(&downgraded, 1000).is_err());
        assert!(Bundle::decode(&encoded, 1001).is_err());
        store.retire_legacy().unwrap();
        drop(store);
        assert!(Store::open_owned(&path, 1000, owner, false).unwrap().token_protected().unwrap());
        fs::remove_file(path.join("sealed")).unwrap();
        assert!(Store::open_owned(&path, 1000, owner, true).is_err());
        assert!(!path.join("master").exists());
        fs::remove_file(path.join("lock")).unwrap();
        assert!(Store::open_owned(&path, 1000, owner, true).is_err());
        assert!(!path.join("master").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn service_ownership_does_not_change_the_sealed_session_identity() {
        let root = std::env::temp_dir().join(format!(
            "td-service-secret-{}-{}",
            std::process::id(),
            u64::from_le_bytes(random::<8>().unwrap())
        ));
        fs::create_dir(&root).unwrap();
        let file_owner = fs::metadata(&root).unwrap().uid();
        let session = if file_owner == 1000 { 1001 } else { 1000 };
        let path = root.join("store");
        let store = Store::open_owned(&path, session, file_owner, true).unwrap();
        store.set("mail", "main", b"credential").unwrap();
        assert_eq!(store.get("mail", "main").unwrap().unwrap(), b"credential");
        let key = std::cell::Cell::new([0; 32]);
        store
            .seal_with(
                |master| {
                    key.set(*master);
                    Ok(tpm::tests::fixture(session))
                },
                |_| Ok(key.get()),
            )
            .unwrap();
        let bundle = store.bundle().unwrap().unwrap();
        assert_eq!(tpm::SealedKey::decode(&bundle.key).unwrap().uid, session);
        assert_eq!(fs::metadata(path.join("sealed")).unwrap().uid(), file_owner);
        let runtime = root.join("runtime");
        fs::create_dir(&runtime).unwrap();
        let runtime = directory(&runtime).unwrap();
        store.release_into(&bundle, &runtime, |_| Ok(key.get())).unwrap();
        assert_eq!(runtime_key_in(&runtime, file_owner, &bundle.key).unwrap(), key.get());
        assert!(runtime_key_in(&runtime, session, &bundle.key).is_err());
        drop(store);
        assert!(Store::open_owned(&path, file_owner, file_owner, false).is_err());
        assert!(Store::open_owned(&path, session, session, false).is_err());
        assert!(Store::open_owned(&path, session, file_owner, false).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sealing_rotates_atomically_and_never_falls_back_after_publication() {
        let root = std::env::temp_dir().join(format!("td-sealed-migration-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        let path = root.join("store");
        let store = Store::open(&path, uid, true).unwrap();
        store.set("mail", "main", b"original credential").unwrap();
        store.set("news", "work", b"separate credential").unwrap();
        let old_master: [u8; 32] = store.read("master", 32).unwrap().try_into().unwrap();
        let key = std::cell::Cell::new([0; 32]);
        let seal = |master: &[u8; 32]| {
            key.set(*master);
            Ok(tpm::tests::fixture(uid))
        };
        assert!(store
            .seal_with(seal, |_| Err("TPM failed before publication".into()))
            .is_err());
        assert!(path.join("master").exists());
        assert!(!path.join("sealed").exists());
        assert_eq!(
            store.get("mail", "main").unwrap().unwrap(),
            b"original credential"
        );
        store.seal_with(seal, |_| Ok(key.get())).unwrap();
        assert!(!path.join("master").exists());
        assert!(!path.join("mail.main").exists());
        assert!(!path.join("news.work").exists());
        assert_ne!(old_master, key.get());
        let bundle = store.bundle().unwrap().unwrap();
        assert_eq!(
            decrypt(&bundle, &key.get(), "mail", "main").unwrap(),
            b"original credential"
        );
        assert_eq!(
            decrypt(&bundle, &key.get(), "news", "work").unwrap(),
            b"separate credential"
        );
        assert!(decrypt(&bundle, &old_master, "mail", "main").is_err());
        assert!(
            store.get("mail", "main").is_err(),
            "locked TPM store returned a credential"
        );
        assert!(store.set("mail", "main", b"overwrite").is_err());
        // A crash between bundle publication and retirement leaves old files.
        store.write("master", &old_master).unwrap();
        assert!(
            store.get("mail", "main").is_err(),
            "legacy master bypassed TPM selection"
        );
        store.retire_legacy().unwrap();
        assert!(!path.join("master").exists());
        let encoded = bundle.encode().unwrap();
        for size in 0..encoded.len() {
            assert!(Bundle::decode(&encoded[..size], uid).is_err());
        }
        store.write("sealed", b"damaged bundle").unwrap();
        drop(store);
        assert!(
            Store::open(&path, uid, true).is_err(),
            "corrupt TPM store was reprovisioned"
        );
        fs::remove_file(path.join("sealed")).unwrap();
        // Missing master plus retained ciphertext must remain an error.
        fs::write(path.join("mail.main"), b"ciphertext").unwrap();
        assert!(Store::open(&path, uid, true).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swap_compiled_out_is_safe_and_active_or_unreadable_swap_is_refused() {
        assert!(check_swap_state(Err(io::ErrorKind::NotFound.into())).is_ok());
        assert!(check_swap_state(Ok("Filename Type Size Used Priority\n".into())).is_ok());
        assert!(check_swap_state(Ok(
            "Filename Type Size Used Priority\n/dev/zram0 partition 1024 0 1\n".into()
        ))
        .is_err());
        assert!(check_swap_state(Err(io::ErrorKind::PermissionDenied.into())).is_err());
        assert!(check_swap_state(Ok(String::new())).is_err());
    }

    #[test]
    fn runtime_requires_tmpfs_without_nested_mounts() {
        let good = "12 1 0:12 / /run rw - tmpfs tmpfs rw\n";
        assert!(check_runtime_mount(good).is_ok());
        assert!(check_runtime_mount("").is_err());
        assert!(check_runtime_mount(&good.replace("tmpfs", "ext4")).is_err());
        assert!(check_runtime_mount(&format!(
            "{good}13 12 0:13 / /run/td-secret/1000 rw - ext4 disk rw\n"
        ))
        .is_err());
    }

    #[test]
    fn release_publishes_only_the_matching_key_and_locks_on_failure() {
        let root = std::env::temp_dir().join(format!("td-release-test-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o755).create(&root).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        assert!(check_root("Uid:\t0\t0\t0\t0\n").is_ok());
        assert!(check_core_limit("Max core file size        0 unlimited bytes\n").is_ok());
        for limits in [
            "",
            "Max core file size 1024 unlimited bytes",
            "Max core file size unlimited unlimited bytes",
        ] {
            assert!(check_core_limit(limits).is_err());
        }
        for status in ["", "Uid: 1000 0 0 0", "Uid: 0 0 0", "Uid: 0 0 0 0 0"] {
            assert!(check_root(status).is_err());
        }
        let store = Store::open(&root.join("store"), uid, true).unwrap();
        store
            .set("mail", "main", b"release fixture credential")
            .unwrap();
        let master = std::cell::Cell::new([0; 32]);
        store
            .seal_with(
                |key| {
                    master.set(*key);
                    Ok(tpm::tests::fixture(uid))
                },
                |_| Ok(master.get()),
            )
            .unwrap();
        let bundle = store.bundle().unwrap().unwrap();
        let runtime = runtime_directory_at(&root, uid, uid, true).unwrap();
        assert!(runtime_key_in(&runtime, uid, &bundle.key).is_err());
        let interrupted = pinned(&runtime).join("tmp-0123456789abcdef0123456789abcdef");
        fs::write(&interrupted, b"interrupted volatile key").unwrap();
        store
            .release_into(&bundle, &runtime, |_| Ok(master.get()))
            .unwrap();
        assert!(!interrupted.exists());
        let released = runtime_key_in(&runtime, uid, &bundle.key).unwrap();
        assert_eq!(released, master.get());
        assert_eq!(
            decrypt(&bundle, &released, "mail", "main").unwrap(),
            b"release fixture credential"
        );
        assert!(runtime_key_in(&runtime, uid, b"another envelope").is_err());
        fs::write(&interrupted, b"another interrupted key").unwrap();
        assert!(store
            .release_into(&bundle, &runtime, |_| Err("PCR mismatch".into()))
            .is_err());
        assert!(!interrupted.exists());
        assert!(runtime_key_in(&runtime, uid, &bundle.key).is_err());
        assert!(store.bundle().unwrap().is_some());
        let key_path = pinned(&runtime).join("key");
        std::os::unix::fs::symlink(root.join("store/sealed"), &key_path).unwrap();
        assert!(runtime_key_in(&runtime, uid, &bundle.key).is_err());
        fs::remove_file(&key_path).unwrap();
        store
            .release_into(&bundle, &runtime, |_| Ok(master.get()))
            .unwrap();
        let unknown = pinned(&runtime).join("unknown");
        fs::write(&unknown, b"unexpected runtime entry").unwrap();
        let unsealed = std::cell::Cell::new(false);
        assert!(store.release_into(&bundle, &runtime, |_| {
            unsealed.set(true);
            Ok(master.get())
        }).is_err());
        assert!(!unsealed.get());
        assert!(!key_path.exists());
        fs::remove_file(unknown).unwrap();
        store.release_into(&bundle, &runtime, |_| Ok(master.get())).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(runtime_key_in(&runtime, uid, &bundle.key).is_err());
        assert!(runtime_directory_at(&root, uid.wrapping_add(1), uid, false).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(runtime_directory_at(&root, uid, uid, false).is_err());
        drop(store);
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_token_store_reopens_and_releases_only_the_bound_role() {
        use crate::fido_metadata::{tests as fixtures, Protection, Role};
        let root = std::env::temp_dir().join(format!("td-fido-store-{}-{}", std::process::id(), u64::from_le_bytes(random::<8>().unwrap())));
        assert!(!root.exists());
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let uid = 1000;
        let emulator = tpm::tests::Emulator::start(&root.join("tpm"));
        emulator.extend(&[9; 32]);
        let path = root.join("store");
        let store = Store::open_owned(&path, uid, owner, true).unwrap();
        store.set("mail", "main", b"token protected credential").unwrap();
        let runtime = runtime_directory_at(&root, owner, uid, true).unwrap();
        atomic_write(&runtime, owner, "key", &[42; 64]).unwrap();
        store.enroll_tokens_into(
            fixtures::metadata_fixture(uid, true), tpm::Pcrs::parse("7").unwrap(),
            &runtime, || Ok(emulator.client()),
        ).unwrap();
        assert!(!pinned(&runtime).join("key").exists());
        drop(store);
        let store = Store::open_owned(&path, uid, owner, false).unwrap();
        let bundle = store.bundle().unwrap().unwrap();
        let protection = Protection::decode(&bundle.key, uid).unwrap();
        for role in [Role::Primary, Role::Recovery] {
            let (request, response) = fixtures::release_fixture(&protection, role);
            store.release_token_into(&runtime, request, &response, emulator.client()).unwrap();
            let master = runtime_key_in(&runtime, owner, &bundle.key).unwrap();
            assert_eq!(decrypt(&bundle, &master, "mail", "main").unwrap(), b"token protected credential");
            for entry in fs::read_dir(&path).unwrap() {
                let bytes = fs::read(entry.unwrap().path()).unwrap();
                assert!(!bytes.windows(32).any(|bytes| bytes == master));
                assert!(!bytes.windows(26).any(|bytes| bytes == b"token protected credential"));
            }
            let (request, mut response) = fixtures::release_fixture(&protection, role);
            *response.last_mut().unwrap() ^= 1;
            assert!(store.release_token_into(&runtime, request, &response, emulator.client()).is_err());
            assert!(runtime_key_in(&runtime, owner, &bundle.key).is_err());
        }
        let (request, response) = fixtures::release_fixture(&protection, Role::Primary);
        store.release_token_into(&runtime, request, &response, emulator.client()).unwrap();
        let alternate = fixtures::protection_fixture(uid, false);
        let (request, response) = fixtures::release_fixture(&alternate, Role::Primary);
        assert!(store.release_token_into(&runtime, request, &response, emulator.client()).is_err());
        assert!(runtime_key_in(&runtime, owner, &bundle.key).is_err());
        let (request, response) = fixtures::release_fixture(&protection, Role::Recovery);
        store.release_token_into(&runtime, request, &response, emulator.client()).unwrap();
        let interrupted = Store::open_owned(&root.join("interrupted"), uid, owner, true).unwrap();
        interrupted.set("mail", "main", b"retained after cleanup failure").unwrap();
        let calls = std::cell::Cell::new(0);
        let failure = interrupted.enroll_tokens_into(
            fixtures::metadata_fixture(uid, false), tpm::Pcrs::parse("7").unwrap(), &runtime,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 { interrupted.write("unknown", b"cleanup refusal")?; }
                Ok(emulator.client())
            },
        ).unwrap_err();
        assert!(failure.contains("unknown entry"), "{failure}");
        assert!(interrupted.token_protected().unwrap());
        assert!(!pinned(&runtime).join("key").exists());
        fs::remove_file(pinned(&interrupted.directory).join("unknown")).unwrap();
        interrupted.retire_legacy().unwrap();
        drop(interrupted);
        emulator.extend(&[8; 32]);
        let (request, response) = fixtures::release_fixture(&protection, Role::Recovery);
        assert!(store.release_token_into(&runtime, request, &response, emulator.client()).is_err());
        assert!(runtime_key_in(&runtime, owner, &bundle.key).is_err());
        drop(runtime);
        drop(store);
        drop(emulator);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refused_enrollment_clears_release_before_metadata_or_protector_admission() {
        let root = std::env::temp_dir().join(format!(
            "td-enroll-refused-{}-{}", std::process::id(),
            u64::from_le_bytes(random::<8>().unwrap()),
        ));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let store = Store::open_owned(&root.join("store"), 1000, owner, true).unwrap();
        let runtime = runtime_directory_at(&root, owner, 1000, true).unwrap();
        let key_path = pinned(&runtime).join("key");
        for case in 0..3 {
            if case == 1 {
                store.write("sealed", b"malformed protector").unwrap();
            } else if case == 2 {
                let bundle = Bundle {
                    key: crate::fido_metadata::tests::protection_fixture(1000, false).encode().unwrap(),
                    records: BTreeMap::new(),
                };
                store.write("sealed", &bundle.encode().unwrap()).unwrap();
            }
            let before = store.read("sealed", MAX_BUNDLE).ok();
            atomic_write(&runtime, owner, "key", &[42; 64]).unwrap();
            let error = store.enroll_tokens_into(
                crate::fido_metadata::tests::metadata_fixture(1001, false),
                tpm::Pcrs::parse("7").unwrap(), &runtime,
                || -> Result<tpm::Client<tpm::tests::Socket>, String> {
                    panic!("refused metadata or protector reached the TPM");
                },
            ).unwrap_err();
            assert_eq!(error, match case {
                0 => "enrollment metadata belongs to another session",
                1 => "invalid sealed store format",
                _ => "credential store is already token enrolled",
            });
            assert!(!key_path.exists(), "refused enrollment retained a release");
            assert_eq!(store.read("sealed", MAX_BUNDLE).ok(), before);
            if case == 0 {
                assert!(store.bundle().unwrap().is_none());
                assert_eq!(store.read("master", 32).unwrap().len(), 32);
            }
        }
        drop(runtime);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_enrolls_a_locked_tpm_store_without_a_portal_release() {
        use crate::fido_metadata::{tests as fixtures, Protection, Role};
        let root = std::env::temp_dir().join(format!(
            "td-enroll-locked-{}-{}", std::process::id(),
            u64::from_le_bytes(random::<8>().unwrap()),
        ));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let uid = 1000;
        let emulator = tpm::tests::Emulator::start(&root.join("tpm"));
        emulator.extend(&[9; 32]);
        let pcrs = tpm::Pcrs::parse("7").unwrap();
        let runtime = runtime_directory_at(&root, owner, uid, true).unwrap();
        let key_path = pinned(&runtime).join("key");
        let path = root.join("store");
        let store = Store::open_owned(&path, uid, owner, true).unwrap();
        store.set("mail", "main", b"mail migration credential").unwrap();
        store.set("news", "account", b"news migration credential").unwrap();
        store.seal_with(
            |master| emulator.client().seal(uid, pcrs, master)?.encode(),
            |blob| emulator.client().unseal(&tpm::SealedKey::decode(blob)?),
        ).unwrap();
        let before = store.read("sealed", MAX_BUNDLE).unwrap();
        assert!(!key_path.exists());
        assert!(!path.join("master").exists());
        let old_bundle = store.bundle().unwrap().unwrap();
        let old_master = emulator.client().unseal(
            &tpm::SealedKey::decode(&old_bundle.key).unwrap(),
        ).unwrap();

        // An unseal failure must preserve the entire old store and stay locked.
        let failed = store.enroll_tokens_into(
            fixtures::metadata_fixture(uid, true), pcrs, &runtime,
            || Err::<tpm::Client<tpm::tests::Socket>, _>("TPM unavailable".into()),
        ).unwrap_err();
        assert_eq!(failed, "TPM unavailable");
        assert_eq!(store.read("sealed", MAX_BUNDLE).unwrap(), before);
        assert!(!key_path.exists());

        let calls = std::cell::Cell::new(0);
        store.enroll_tokens_into(
            fixtures::metadata_fixture(uid, true), pcrs, &runtime,
            || {
                assert!(!key_path.exists(), "migration published a portal key");
                calls.set(calls.get() + 1);
                Ok(emulator.client())
            },
        ).unwrap();
        assert_eq!(calls.get(), 3); // old unseal, new seal, new roundtrip
        assert!(!key_path.exists());
        drop(store);
        let store = Store::open_owned(&path, uid, owner, false).unwrap();
        let bundle = store.bundle().unwrap().unwrap();
        let protection = Protection::decode(&bundle.key, uid).unwrap();
        assert!(decrypt(&bundle, &old_master, "mail", "main").is_err());
        for role in [Role::Primary, Role::Recovery] {
            let (request, response) = fixtures::release_fixture(&protection, role);
            store.release_token_into(&runtime, request, &response, emulator.client()).unwrap();
            let master = runtime_key_in(&runtime, owner, &bundle.key).unwrap();
            assert_eq!(decrypt(&bundle, &master, "mail", "main").unwrap(), b"mail migration credential");
            assert_eq!(decrypt(&bundle, &master, "news", "account").unwrap(), b"news migration credential");
        }
        let enrolled = store.read("sealed", MAX_BUNDLE).unwrap();
        for private in [old_master.as_slice(), b"mail migration credential", b"news migration credential"] {
            assert!(!enrolled.windows(private.len()).any(|bytes| bytes == private));
        }
        assert!(key_path.exists());
        assert!(store.enroll_tokens_into(
            fixtures::metadata_fixture(uid, false), pcrs, &runtime,
            || -> Result<tpm::Client<tpm::tests::Socket>, String> {
                panic!("token replacement reached the TPM");
            },
        ).unwrap_err().contains("already token enrolled"));
        assert!(!key_path.exists());
        assert_eq!(store.read("sealed", MAX_BUNDLE).unwrap(), enrolled);
        drop(store);
        drop(runtime);
        drop(emulator);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_migrates_real_store_without_persisting_the_new_master() {
        let root = std::env::temp_dir().join(format!("td-tpm-store-{}", std::process::id()));
        assert!(!root.exists());
        fs::create_dir(&root).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        let emulator = tpm::tests::Emulator::start(&root.join("tpm"));
        emulator.extend(&[9; 32]);
        let path = root.join("store");
        let store = Store::open(&path, uid, true).unwrap();
        store
            .set("mail", "main", b"real TPM store credential")
            .unwrap();
        store
            .seal_with(
                |master| {
                    emulator
                        .client()
                        .seal(uid, tpm::Pcrs::parse("7")?, master)?
                        .encode()
                },
                |blob| emulator.client().unseal(&tpm::SealedKey::decode(blob)?),
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path, uid, false).unwrap();
        let bundle = store.bundle().unwrap().unwrap();
        let key = tpm::SealedKey::decode(&bundle.key).unwrap();
        let runtime = runtime_directory_at(&root, uid, uid, true).unwrap();
        store
            .release_into(&bundle, &runtime, |blob| {
                emulator.client().unseal(&tpm::SealedKey::decode(blob)?)
            })
            .unwrap();
        let master = runtime_key_in(&runtime, uid, &bundle.key).unwrap();
        assert_eq!(
            decrypt(&bundle, &master, "mail", "main").unwrap(),
            b"real TPM store credential"
        );
        for entry in fs::read_dir(&path).unwrap() {
            let bytes = fs::read(entry.unwrap().path()).unwrap();
            assert!(!bytes.windows(32).any(|bytes| bytes == master));
            assert!(!bytes
                .windows(25)
                .any(|bytes| bytes == b"real TPM store credential"));
        }
        emulator.extend(&[8; 32]);
        assert!(emulator.client().unseal(&key).is_err());
        assert!(store
            .release_into(&bundle, &runtime, |blob| emulator
                .client()
                .unseal(&tpm::SealedKey::decode(blob)?))
            .is_err());
        assert!(runtime_key_in(&runtime, uid, &bundle.key).is_err());
        drop(runtime);
        drop(store);
        drop(emulator);
        fs::remove_dir_all(root).unwrap();
    }

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
    fn protected_memory_refusal_prevents_the_key_operation() {
        let never = || -> Result<(), String> { panic!("unsafe memory reached unseal") };
        let no_swap = || Ok("Filename Type Size Used Priority\n".into());
        let no_core = || Ok("Max core file size 0 unlimited bytes\n".into());
        assert!(with_memory_state(
            Ok("Filename Type Size Used Priority\n/swap file 1 1 0\n".into()),
            no_core(),
            never
        )
        .is_err());
        assert!(with_memory_state(
            Err(io::ErrorKind::PermissionDenied.into()),
            no_core(),
            never
        )
        .is_err());
        assert!(with_memory_state(
            no_swap(),
            Ok("Max core file size unlimited unlimited bytes\n".into()),
            never
        )
        .is_err());
        assert!(with_memory_state(
            no_swap(),
            Err(io::ErrorKind::PermissionDenied.into()),
            never
        )
        .is_err());
        assert_eq!(
            with_memory_state(no_swap(), no_core(), || Ok(42)).unwrap(),
            42
        );
        assert_eq!(
            with_memory_state(Err(io::ErrorKind::NotFound.into()), no_core(), || Ok(43)).unwrap(),
            43
        );
    }

    #[test]
    fn token_write_refuses_invalid_target_or_protector_before_unseal() {
        let root = std::env::temp_dir().join(format!(
            "td-token-write-{}-{}",
            std::process::id(),
            u64::from_le_bytes(random::<8>().unwrap())
        ));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("store");
        let store = Store::open_owned(&path, 1000, owner, true).unwrap();
        store.set("mail", "main", b"old credential").unwrap();
        let never = || -> Result<[u8; 32], String> { panic!("invalid write reached unseal") };
        assert_eq!(
            store
                .set_token_with("mail", "main", b"new", &[0; 32], never)
                .unwrap_err(),
            "credential store is not token enrolled"
        );
        store
            .write(
                "sealed",
                &Bundle {
                    key: tpm::tests::fixture(1000),
                    records: BTreeMap::new(),
                }
                .encode()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            store
                .set_token_with("mail", "main", b"new", &[0; 32], never)
                .unwrap_err(),
            "credential store is not token enrolled"
        );
        let key = crate::fido_metadata::tests::protection_fixture(1000, true)
            .encode()
            .unwrap();
        let protector = crypto::digest(&key);
        let mut bundle = Bundle {
            key,
            records: BTreeMap::new(),
        };
        store.write("sealed", &bundle.encode().unwrap()).unwrap();
        let before = store.read("sealed", MAX_BUNDLE).unwrap();
        for (app, name, bytes) in [
            ("mail", "../main", b"new".as_slice()),
            ("../mail", "main", b"new"),
            ("mail", "main", b""),
        ] {
            assert!(store
                .set_token_with(app, name, bytes, &protector, never)
                .is_err());
        }
        assert!(store
            .set_token_with("mail", "main", &[7; MAX_SECRET + 1], &protector, never)
            .is_err());
        assert_eq!(
            store
                .set_token_with("mail", "main", b"new", &[0; 32], never)
                .unwrap_err(),
            "credential protector changed during token write"
        );
        assert_eq!(
            store
                .set_token_with("mail", "main", b"new", &protector, || Err(
                    "signature refused".into()
                ))
                .unwrap_err(),
            "signature refused"
        );
        assert_eq!(store.read("sealed", MAX_BUNDLE).unwrap(), before);
        for index in 0..128 {
            bundle.records.insert(
                format!("mail.entry{index}"),
                fs::read(path.join("mail.main")).unwrap(),
            );
        }
        store.write("sealed", &bundle.encode().unwrap()).unwrap();
        assert_eq!(
            store
                .set_token_with("mail", "main", b"new", &protector, never)
                .unwrap_err(),
            "credential store has no free record slot"
        );
        assert_eq!(
            store.read("sealed", MAX_BUNDLE).unwrap(),
            bundle.encode().unwrap()
        );
        store
            .set_token_with(
                "mail",
                "entry0",
                b"replacement at capacity",
                &protector,
                || Ok([42; 32]),
            )
            .unwrap();
        let updated = store.bundle().unwrap().unwrap();
        assert_eq!(updated.records.len(), MAX_ENTRIES);
        assert_eq!(
            decrypt(&updated, &[42; 32], "mail", "entry0").unwrap(),
            b"replacement at capacity"
        );
        assert_eq!(
            updated.records.get("mail.entry1"),
            bundle.records.get("mail.entry1")
        );
        if owner != 0 {
            struct Never;
            impl tpm::Transport for Never {
                fn exchange(&mut self, _: &[u8]) -> Result<Vec<u8>, String> {
                    panic!("unprivileged write reached TPM")
                }
            }
            let protection = crate::fido_metadata::Protection::decode(&updated.key, 1000).unwrap();
            let (request, response) = crate::fido_metadata::tests::release_fixture(
                &protection,
                crate::fido_metadata::Role::Primary,
            );
            assert_eq!(
                store
                    .set_token(
                        "mail",
                        "entry0",
                        b"forbidden",
                        request,
                        &response,
                        tpm::Client::new(Never)
                    )
                    .unwrap_err(),
                require_root().unwrap_err()
            );
        }
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_token_write_changes_one_record_without_releasing_the_store() {
        use crate::fido_metadata::{tests as fixtures, Protection, Role};
        let root = std::env::temp_dir().join(format!(
            "td-token-write-emulator-{}-{}",
            std::process::id(),
            u64::from_le_bytes(random::<8>().unwrap())
        ));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let emulator = tpm::tests::Emulator::start(&root.join("tpm"));
        emulator.extend(&[9; 32]);
        let path = root.join("store");
        let store = Store::open_owned(&path, 1000, owner, true).unwrap();
        store.set("mail", "main", b"old mail credential").unwrap();
        store
            .set("news", "work", b"preserved news credential")
            .unwrap();
        let runtime = runtime_directory_at(&root, owner, 1000, true).unwrap();
        store
            .enroll_tokens_into(
                fixtures::metadata_fixture(1000, true),
                tpm::Pcrs::parse("7").unwrap(),
                &runtime,
                || Ok(emulator.client()),
            )
            .unwrap();
        let initial = store.bundle().unwrap().unwrap();
        let protection = Protection::decode(&initial.key, 1000).unwrap();
        for role in [Role::Primary, Role::Recovery] {
            let (request, response) = fixtures::release_fixture(&protection, role);
            let id = *request.protection_id();
            store
                .set_token_with("mail", "main", b"new mail credential", &id, || {
                    request.unseal(&response, emulator.client())
                })
                .unwrap();
            assert!(!pinned(&runtime).join("key").exists());
            let updated = store.bundle().unwrap().unwrap();
            assert_eq!(updated.key, initial.key);
            assert_eq!(updated.records.len(), 2);
            assert_eq!(
                updated.records.get("news.work"),
                initial.records.get("news.work")
            );
            assert_ne!(
                updated.records.get("mail.main"),
                initial.records.get("mail.main")
            );
            let (request, response) = fixtures::release_fixture(&protection, role);
            let mut master = request.unseal(&response, emulator.client()).unwrap();
            assert_eq!(
                decrypt(&updated, &master, "mail", "main").unwrap(),
                b"new mail credential"
            );
            assert_eq!(
                decrypt(&updated, &master, "news", "work").unwrap(),
                b"preserved news credential"
            );
            let persisted = fs::read(path.join("sealed")).unwrap();
            assert!(!persisted.windows(32).any(|bytes| bytes == master));
            assert!(!persisted
                .windows(19)
                .any(|bytes| bytes == b"new mail credential"));
            master.fill(0);
            let (request, mut response) = fixtures::release_fixture(&protection, role);
            *response.last_mut().unwrap() ^= 1;
            let id = *request.protection_id();
            assert!(store
                .set_token_with("mail", "main", b"forbidden", &id, || request
                    .unseal(&response, emulator.client()))
                .is_err());
            assert_eq!(fs::read(path.join("sealed")).unwrap(), persisted);
        }
        let before = store.bundle().unwrap().unwrap();
        let (request, response) = fixtures::release_fixture(&protection, Role::Primary);
        store
            .release_token_into(&runtime, request, &response, emulator.client())
            .unwrap();
        let runtime_before = fs::read(pinned(&runtime).join("key")).unwrap();
        let (request, response) = fixtures::release_fixture(&protection, Role::Recovery);
        let id = *request.protection_id();
        store
            .set_token_with("mail", "secondary", b"new account credential", &id, || {
                request.unseal(&response, emulator.client())
            })
            .unwrap();
        let added = store.bundle().unwrap().unwrap();
        assert_eq!(added.records.len(), 3);
        for (name, record) in &before.records {
            assert_eq!(added.records.get(name), Some(record));
        }
        assert_eq!(
            fs::read(pinned(&runtime).join("key")).unwrap(),
            runtime_before
        );
        let mut master = runtime_key_in(&runtime, owner, &added.key).unwrap();
        assert_eq!(
            decrypt(&added, &master, "mail", "secondary").unwrap(),
            b"new account credential"
        );
        assert_eq!(
            decrypt(&added, &master, "mail", "main").unwrap(),
            b"new mail credential"
        );
        assert_eq!(
            decrypt(&added, &master, "news", "work").unwrap(),
            b"preserved news credential"
        );
        master.fill(0);
        drop(runtime);
        drop(store);
        drop(emulator);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn applications_refuse_unenrolled_backends_even_with_readable_credentials() {
        let root = std::env::temp_dir().join(format!("td-app-secret-{}-{}", std::process::id(), u64::from_le_bytes(random::<8>().unwrap())));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("store");
        let store = Store::open_owned(&path, 1000, owner, true).unwrap();
        store.set("mail", "main", b"unenrolled credential").unwrap();
        assert_eq!(store.get("mail", "main").unwrap().unwrap(), b"unenrolled credential");
        assert_eq!(store.application_secret("mail", "main").unwrap_err(), "credential store requires token enrollment");
        store.write("sealed", &Bundle { key: tpm::tests::fixture(1000), records: BTreeMap::new() }.encode().unwrap()).unwrap();
        assert_eq!(store.application_secret("mail", "main").unwrap_err(), "credential store requires token enrollment");
        for recovery in [false, true] {
            let key = super::super::fido_metadata::tests::protection_fixture(1000, recovery).encode().unwrap();
            store.write("sealed", &Bundle { key, records: BTreeMap::new() }.encode().unwrap()).unwrap();
            // The enrolled policy admits lookup; a missing entry stays missing.
            assert_eq!(store.application_secret("mail", "absent").unwrap(), None);
        }
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inspection_preserves_store_bytes_and_refuses_missing_or_invalid_state() {
        let root = std::env::temp_dir().join(format!("td-inspect-{}-{}", std::process::id(), u64::from_le_bytes(random::<8>().unwrap())));
        fs::create_dir(&root).unwrap();
        let owner = fs::metadata(&root).unwrap().uid();
        let path = root.join("store");
        assert!(Store::inspect_owned(&path, 1000, owner).is_err());
        assert!(!path.exists());
        let store = Store::open_owned(&path, 1000, owner, true).unwrap();
        store.set("mail", "main", b"private inspection fixture").unwrap();
        assert!(Store::inspect_owned(&path, 1000, owner).is_err());
        drop(store);
        let snapshot = || fs::read_dir(&path).unwrap().map(|entry| {
            let entry = entry.unwrap(); (entry.file_name(), fs::read(entry.path()).unwrap())
        }).collect::<BTreeMap<_,_>>();
        let initial = snapshot();
        assert_eq!(Store::inspect_owned(&path, 1000, owner).unwrap(), 0);
        assert_eq!(snapshot(), initial);
        fs::remove_file(path.join("master")).unwrap();
        assert!(Store::inspect_owned(&path, 1000, owner).is_err());
        assert!(!path.join("master").exists());
        for bytes in [&[][..], &[1; 31][..], &[1; 33][..]] {
            fs::write(path.join("master"), bytes).unwrap();
            fs::set_permissions(path.join("master"), fs::Permissions::from_mode(0o600)).unwrap();
            assert!(Store::inspect_owned(&path, 1000, owner).is_err());
            assert_eq!(fs::read(path.join("master")).unwrap(), bytes);
        }
        fs::write(path.join("master"), initial.get(std::ffi::OsStr::new("master")).unwrap()).unwrap();
        let store = Store::open_owned(&path, 1000, owner, false).unwrap();
        store.write("sealed", &Bundle { key: tpm::tests::fixture(1000), records: BTreeMap::new() }.encode().unwrap()).unwrap();
        drop(store);
        let initial = snapshot();
        assert_eq!(Store::inspect_owned(&path, 1000, owner).unwrap(), 1);
        assert!(Store::inspect_owned(&path, 1001, owner).is_err());
        assert_eq!(snapshot(), initial);
        fs::remove_file(path.join("sealed")).unwrap();
        for recovery in [false, true] {
            let protection = super::super::fido_metadata::tests::protection_fixture(1000, recovery).encode().unwrap();
            let store = Store::open_owned(&path, 1000, owner, false).unwrap();
            store.write("sealed", &Bundle { key: protection, records: BTreeMap::new() }.encode().unwrap()).unwrap();
            drop(store);
            let initial = snapshot();
            assert_eq!(Store::inspect_owned(&path, 1000, owner).unwrap(), if recovery { 3 } else { 2 });
            assert_eq!(snapshot(), initial);
            assert!(Store::inspect_owned(&path, 1001, owner).is_err());
            fs::remove_file(path.join("sealed")).unwrap();
        }
        fs::write(path.join("sealed"), b"malformed protector").unwrap();
        fs::set_permissions(path.join("sealed"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Store::inspect_owned(&path, 1000, owner).is_err());
        fs::remove_file(path.join("sealed")).unwrap();
        fs::remove_file(path.join("lock")).unwrap();
        assert!(Store::inspect_owned(&path, 1000, owner).is_err());
        assert!(!path.join("lock").exists());
        fs::remove_dir_all(root).unwrap();
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
