//! Private, bounded device verifiers. The caller owns HTTP and account admission.
use super::{same_file, LockedRoot, MAX_PATH_BYTES};
use crate::{
    ids::{AccountId, DeviceId},
    store_paths::{Name, RootEntry},
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};
use td_crypto::{Crypto, Digest, Entropy, Provider};

pub const DEVICE_CAPACITY: usize = 256;
pub const DEVICE_LABEL_BYTES: usize = 128;
const FILE_BYTES: usize = 65536;
const MAGIC: &[u8; 8] = b"TDDEV001";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceError {
    Invalid,
    Capacity,
    NotFound,
    Unavailable,
}
impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid device record",
            Self::Capacity => "device capacity exhausted",
            Self::NotFound => "device not found",
            Self::Unavailable => "device registry unavailable",
        })
    }
}
impl std::error::Error for DeviceError {}
impl From<io::Error> for DeviceError {
    fn from(_: io::Error) -> Self {
        Self::Unavailable
    }
}
impl From<td_crypto::Error> for DeviceError {
    fn from(_: td_crypto::Error) -> Self {
        Self::Unavailable
    }
}

/// Deliberately neither Debug nor Clone. Explicit exposure is for the local receipt.
pub struct DevicePassword([u8; 97]);
impl DevicePassword {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}
impl Drop for DevicePassword {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
struct Secret([u8; 32]);
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}
#[derive(Clone)]
struct Record {
    id: DeviceId,
    salt: [u8; 32],
    verifier: [u8; 32],
    created: u64,
    revoked: Option<u64>,
    label: String,
}

/// Exclusive mutable root borrow prevents two registries sharing one lock.
/// Any failed publication retires this owner, including all authentication.
pub struct DeviceStore<'r> {
    root: &'r LockedRoot,
    account: AccountId,
    records: Vec<Record>,
    owner: u32,
    healthy: bool,
    #[cfg(test)]
    fail_after_rename: bool,
}
impl<'r> DeviceStore<'r> {
    /// Missing files are errors. Only explicit local creation initializes a registry.
    pub fn open(root: &'r mut LockedRoot, account: AccountId) -> Result<Self, DeviceError> {
        Self::load(root, account, false)
    }
    pub fn open_or_create(
        root: &'r mut LockedRoot,
        account: AccountId,
    ) -> Result<Self, DeviceError> {
        Self::load(root, account, true)
    }
    fn load(
        root: &'r mut LockedRoot,
        account: AccountId,
        create: bool,
    ) -> Result<Self, DeviceError> {
        let owner = root.root.directory.metadata()?.uid();
        let mut store = Self {
            root,
            account,
            records: Vec::new(),
            owner,
            healthy: true,
            #[cfg(test)]
            fail_after_rename: false,
        };
        let mut path = [0; MAX_PATH_BYTES];
        let name = Name::root(RootEntry::Devices).map_err(|_| DeviceError::Invalid)?;
        let path = store.root.root.directory.join(&name, &mut path)?;
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                store.check(&meta)?;
                let file = File::open(path)?;
                if !same_file(&meta, &file.metadata()?) {
                    return Err(DeviceError::Invalid);
                }
                let mut bytes = Vec::new();
                (&file)
                    .take(FILE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > FILE_BYTES {
                    return Err(DeviceError::Capacity);
                }
                store.records = decode(&bytes, account)?;
                // A previous rename may have succeeded before its directory sync failed.
                file.sync_all()?;
                store.root.root.directory.file.sync_all()?;
                store.cleanup()?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                store.cleanup()?;
                store.publish()?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(store)
    }
    fn check(&self, meta: &Metadata) -> Result<(), DeviceError> {
        if !meta.is_file()
            || meta.uid() != self.owner
            || meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 1
        {
            return Err(DeviceError::Invalid);
        }
        Ok(())
    }
    fn cleanup(&self) -> Result<(), DeviceError> {
        let mut path = [0; MAX_PATH_BYTES];
        let name = Name::root(RootEntry::DevicesPending).map_err(|_| DeviceError::Invalid)?;
        let path = self.root.root.directory.join(&name, &mut path)?;
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                self.check(&meta)?;
                fs::remove_file(path)?;
                self.root.root.directory.file.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn publish(&mut self) -> Result<(), DeviceError> {
        self.healthy = false;
        let bytes = encode(&self.records, self.account)?;
        let directory = &self.root.root.directory;
        let mut pending = [0; MAX_PATH_BYTES];
        let mut selected = [0; MAX_PATH_BYTES];
        let pn = Name::root(RootEntry::DevicesPending).map_err(|_| DeviceError::Invalid)?;
        let sn = Name::root(RootEntry::Devices).map_err(|_| DeviceError::Invalid)?;
        let pending = directory.join(&pn, &mut pending)?;
        let selected = directory.join(&sn, &mut selected)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(pending)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        if let Err(error) = self.check(&file.metadata()?) {
            // This invocation exclusively created the file in a trusted namespace.
            fs::remove_file(pending)?;
            directory.file.sync_all()?;
            return Err(error);
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(pending, selected)?;
        #[cfg(test)]
        if self.fail_after_rename {
            return Err(DeviceError::Unavailable);
        }
        directory.file.sync_all()?;
        self.healthy = true;
        Ok(())
    }
    pub fn create(
        &mut self,
        label: &str,
        now: u64,
        entropy: &mut impl Entropy,
    ) -> Result<(DeviceId, DevicePassword), DeviceError> {
        self.available()?;
        valid_label(label)?;
        if self.records.len() >= DEVICE_CAPACITY {
            return Err(DeviceError::Capacity);
        }
        let mut id = [0; 16];
        let mut salt = [0; 32];
        let mut secret = Secret([0; 32]);
        entropy.fill(&mut id)?;
        entropy.fill(&mut salt)?;
        entropy.fill(&mut secret.0)?;
        let id = DeviceId::from_bytes(id);
        if self.records.iter().any(|record| record.id == id) {
            return Err(DeviceError::Unavailable);
        }
        let verifier = verify_hash(self.account, id, &salt, &secret.0)?;
        let mut password = DevicePassword([0; 97]);
        for (out, byte) in password
            .0
            .get_mut(..32)
            .ok_or(DeviceError::Invalid)?
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(id.as_bytes())
        {
            out.copy_from_slice(&hex(*byte));
        }
        *password.0.get_mut(32).ok_or(DeviceError::Invalid)? = b'.';
        for (out, byte) in password
            .0
            .get_mut(33..)
            .ok_or(DeviceError::Invalid)?
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(&secret.0)
        {
            out.copy_from_slice(&hex(*byte));
        }
        self.records.push(Record {
            id,
            salt,
            verifier,
            created: now,
            revoked: None,
            label: label.to_owned(),
        });
        self.publish()?;
        Ok((id, password))
    }
    /// Includes revoked IDs; never exposes verifiers or passwords.
    pub fn device_ids(&self) -> Result<impl Iterator<Item = DeviceId> + '_, DeviceError> {
        self.available()?;
        Ok(self.records.iter().map(|record| record.id))
    }
    /// Activity check for an already authenticated device, never authentication.
    pub(crate) fn active(&self, account: AccountId, id: DeviceId) -> Result<bool, DeviceError> {
        self.available()?;
        Ok(self.account == account
            && self
                .records
                .iter()
                .any(|record| record.id == id && record.revoked.is_none()))
    }
    pub fn revoke(&mut self, id: DeviceId, now: u64) -> Result<(), DeviceError> {
        self.available()?;
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(DeviceError::NotFound)?;
        if record.revoked.is_some() {
            return Ok(());
        }
        record.revoked = Some(now);
        self.publish()
    }
    /// Authentication is a current observation, never permanent connection authority.
    pub fn authenticate(&self, password: &[u8]) -> Result<Option<DeviceId>, DeviceError> {
        self.available()?;
        if password.len() != 97 || password.get(32) != Some(&b'.') {
            return Ok(None);
        }
        let id = match password
            .get(..32)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| DeviceId::parse(text).ok())
        {
            Some(id) => id,
            None => return Ok(None),
        };
        let mut secret = Secret([0; 32]);
        for (out, pair) in secret.0.iter_mut().zip(
            password
                .get(33..)
                .ok_or(DeviceError::Invalid)?
                .as_chunks::<2>()
                .0
                .iter(),
        ) {
            let Some(value) = pair
                .first()
                .and_then(|b| unhex(*b))
                .zip(pair.get(1).and_then(|b| unhex(*b)))
                .map(|(hi, lo)| hi * 16 + lo)
            else {
                return Ok(None);
            };
            *out = value;
        }
        let record = self.records.iter().find(|record| record.id == id);
        let salt = record.map(|record| record.salt).unwrap_or([0; 32]);
        let expected = record.map(|record| record.verifier).unwrap_or([0; 32]);
        let actual = verify_hash(self.account, id, &salt, &secret.0)?;
        let equal = Provider.equal_digest(&actual, &expected);
        Ok(record
            .filter(|record| record.revoked.is_none() && equal)
            .map(|record| record.id))
    }
    fn available(&self) -> Result<(), DeviceError> {
        if self.healthy {
            Ok(())
        } else {
            Err(DeviceError::Unavailable)
        }
    }
}
fn hex(byte: u8) -> [u8; 2] {
    fn digit(n: u8) -> u8 {
        if n < 10 {
            b'0' + n
        } else {
            b'a' + n - 10
        }
    }
    [digit(byte >> 4), digit(byte & 15)]
}
fn unhex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
fn valid_label(label: &str) -> Result<(), DeviceError> {
    if label.is_empty() || label.len() > DEVICE_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(DeviceError::Invalid);
    }
    Ok(())
}
fn verify_hash(
    account: AccountId,
    id: DeviceId,
    salt: &[u8; 32],
    secret: &[u8; 32],
) -> Result<[u8; 32], DeviceError> {
    let mut digest = Provider.sha256()?;
    for bytes in [
        b"td-mta device verifier v1\0".as_slice(),
        account.as_bytes(),
        id.as_bytes(),
        salt,
        secret,
    ] {
        digest.update(bytes)?;
    }
    Ok(digest.finish()?)
}
fn checksum(bytes: &[u8]) -> Result<[u8; 32], DeviceError> {
    let mut digest = Provider.sha256()?;
    digest.update(bytes)?;
    Ok(digest.finish()?)
}
fn encode(records: &[Record], account: AccountId) -> Result<Vec<u8>, DeviceError> {
    let mut bytes = Vec::with_capacity(FILE_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(account.as_bytes());
    bytes.extend_from_slice(
        &u16::try_from(records.len())
            .map_err(|_| DeviceError::Capacity)?
            .to_be_bytes(),
    );
    for record in records {
        bytes.extend_from_slice(record.id.as_bytes());
        bytes.extend_from_slice(&record.salt);
        bytes.extend_from_slice(&record.verifier);
        bytes.extend_from_slice(&record.created.to_be_bytes());
        bytes.push(u8::from(record.revoked.is_some()));
        bytes.extend_from_slice(&record.revoked.unwrap_or(0).to_be_bytes());
        bytes.extend_from_slice(
            &u16::try_from(record.label.len())
                .map_err(|_| DeviceError::Capacity)?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(record.label.as_bytes());
    }
    let digest = checksum(&bytes)?;
    bytes.extend_from_slice(&digest);
    if bytes.len() > FILE_BYTES {
        return Err(DeviceError::Capacity);
    }
    Ok(bytes)
}
fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], DeviceError> {
    let (head, rest) = bytes.split_at_checked(N).ok_or(DeviceError::Invalid)?;
    *bytes = rest;
    head.try_into().map_err(|_| DeviceError::Invalid)
}
fn decode(bytes: &[u8], account: AccountId) -> Result<Vec<Record>, DeviceError> {
    let (data, digest) = bytes
        .split_at_checked(bytes.len().checked_sub(32).ok_or(DeviceError::Invalid)?)
        .ok_or(DeviceError::Invalid)?;
    if !Provider.equal_digest(
        &checksum(data)?,
        &digest.try_into().map_err(|_| DeviceError::Invalid)?,
    ) {
        return Err(DeviceError::Invalid);
    }
    let mut data = data;
    if &take::<8>(&mut data)? != MAGIC || &take::<16>(&mut data)? != account.as_bytes() {
        return Err(DeviceError::Invalid);
    }
    let count = usize::from(u16::from_be_bytes(take(&mut data)?));
    if count > DEVICE_CAPACITY {
        return Err(DeviceError::Invalid);
    }
    let mut records: Vec<Record> = Vec::with_capacity(count);
    for _ in 0..count {
        let id = DeviceId::from_bytes(take(&mut data)?);
        let salt = take(&mut data)?;
        let verifier = take(&mut data)?;
        let created = u64::from_be_bytes(take(&mut data)?);
        let flag = take::<1>(&mut data)?;
        let revoked = u64::from_be_bytes(take(&mut data)?);
        let revoked = match flag {
            [0] if revoked == 0 => None,
            [1] => Some(revoked),
            _ => return Err(DeviceError::Invalid),
        };
        let length = usize::from(u16::from_be_bytes(take(&mut data)?));
        let (label, rest) = data.split_at_checked(length).ok_or(DeviceError::Invalid)?;
        data = rest;
        let label = std::str::from_utf8(label).map_err(|_| DeviceError::Invalid)?;
        valid_label(label)?;
        if records.iter().any(|record| record.id == id) {
            return Err(DeviceError::Invalid);
        }
        records.push(Record {
            id,
            salt,
            verifier,
            created,
            revoked,
            label: label.to_owned(),
        });
    }
    if !data.is_empty() {
        return Err(DeviceError::Invalid);
    }
    Ok(records)
}

#[cfg(test)]
#[path = "devices_tests.rs"]
mod tests;
