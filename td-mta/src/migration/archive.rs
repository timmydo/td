use super::*;
use crate::{
    operator_files::{Inputs, Role},
    store_fs::{LockedRoot, PrivateRoot},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};
use td_crypto::{Entropy, SystemEntropy};
pub(super) struct Archive {
    path: PathBuf,
    lock: LockedRoot,
    maximum: u64,
    used: u64,
    entries: usize,
}
impl Archive {
    pub fn open(path: &str, maximum: u64) -> Result<Self> {
        let lock = PrivateRoot::open(path)
            .map_err(|_| "migration-archive-root")?
            .try_lock()
            .map_err(|_| "migration-archive-locked")?;
        let mut archive = Self {
            path: PathBuf::from(path),
            lock,
            maximum,
            used: 0,
            entries: 0,
        };
        for entry in fs::read_dir(path).map_err(|_| "migration-archive-read")? {
            let entry = entry.map_err(|_| "migration-archive-read")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "migration-archive-entry")?;
            if name == "LOCK" {
                continue;
            }
            let permitted = matches!(name.as_str(), "source.json" | "manifest.json" | "partial")
                || name.strip_suffix(".eml").is_some_and(hash_name)
                || name.strip_suffix(".json").is_some_and(hash_name);
            if !permitted {
                return Err("migration-archive-entry");
            }
            let file = archive.open_file(&name)?;
            let size = file.metadata().map_err(|_| "migration-archive-read")?.len();
            archive.used = archive.used.checked_add(size).ok_or("migration-capacity")?;
            archive.entries += 1;
            if archive.entries > 210_000 || archive.used > maximum {
                return Err("migration-archive-capacity");
            }
        }
        if archive
            .path
            .join("partial")
            .try_exists()
            .map_err(|_| "migration-archive-read")?
        {
            let size = archive
                .open_file("partial")?
                .metadata()
                .map_err(|_| "migration-archive-read")?
                .len();
            fs::remove_file(archive.path.join("partial")).map_err(|_| "migration-archive-write")?;
            archive.used = archive.used.checked_sub(size).ok_or("migration-capacity")?;
            archive.entries = archive.entries.saturating_sub(1);
            archive.sync()?;
        }
        Ok(archive)
    }
    fn sync(&self) -> Result<()> {
        File::open(&self.path)
            .and_then(|f| f.sync_all())
            .map_err(|_| "migration-archive-sync")
    }
    fn open_file(&self, name: &str) -> Result<File> {
        let path = self.path.join(name);
        let path = path.to_str().ok_or("migration-archive-path")?;
        let mut inputs = Inputs::new();
        inputs
            .bind(self.lock.root())
            .map_err(|_| "migration-archive-owner")?;
        inputs
            .open(path, Role::Secret)
            .map_err(|_| "migration-archive-file")
    }
    fn json(&self, name: &str) -> Result<Json> {
        parse(&read_bounded(self.open_file(name)?, INDEX_LIMIT)?)
    }
    fn exists(&self, name: &str) -> Result<bool> {
        match fs::symlink_metadata(self.path.join(name)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("migration-archive-read"),
        }
    }
    fn temporary(&self, size: u64) -> Result<File> {
        if self.used.checked_add(size).is_none_or(|n| n > self.maximum) || self.entries >= 210_000 {
            return Err("migration-archive-capacity");
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join("partial"))
            .map_err(|_| "migration-archive-write")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "migration-archive-write")?;
        Ok(file)
    }
    fn install(&mut self, name: &str, size: u64) -> Result<()> {
        let old = if self.exists(name)? {
            self.open_file(name)?
                .metadata()
                .map_err(|_| "migration-archive-read")?
                .len()
        } else {
            self.entries += 1;
            0
        };
        fs::rename(self.path.join("partial"), self.path.join(name))
            .map_err(|_| "migration-archive-write")?;
        self.sync()?;
        self.used = self
            .used
            .checked_sub(old)
            .and_then(|n| n.checked_add(size))
            .ok_or("migration-capacity")?;
        Ok(())
    }
    fn write_json(&mut self, name: &str, value: &Json) -> Result<()> {
        let bytes = value.to_string();
        if bytes.len() > INDEX_LIMIT {
            return Err("migration-capacity");
        }
        let mut file = self.temporary(bytes.len() as u64)?;
        file.write_all(bytes.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| "migration-archive-write")?;
        self.install(name, bytes.len() as u64)
    }
    pub fn bind(&mut self, endpoint: &str, account: &str, username: &str) -> Result<Json> {
        if self.exists("source.json")? {
            let source = self.json("source.json")?;
            if integer(&source, "version")? != 1
                || string(&source, "session")? != endpoint
                || string(&source, "account")? != account
                || string(&source, "username")? != username
            {
                return Err("migration-source-mismatch");
            }
            validate_source(&source)?;
            // A previous crash may have happened after rename, before directory sync.
            self.open_file("source.json")?
                .sync_all()
                .map_err(|_| "migration-archive-sync")?;
            self.sync()?;
            return Ok(source);
        }
        if self.entries != 0 {
            return Err("migration-missing-source-binding");
        }
        let mut instance = [0; 16];
        SystemEntropy::try_new()
            .map_err(|_| "migration-entropy")?
            .fill(&mut instance)
            .map_err(|_| "migration-entropy")?;
        let source = json!({"version":1,"instance":hex(&instance),"session":endpoint,"account":account,"username":username});
        self.write_json("source.json", &source)?;
        Ok(source)
    }
    pub fn body(&mut self, source: &source::Source, email: &Json) -> Result<String> {
        let id = string(email, "id")?;
        let blob = string(email, "blobId")?;
        let size = integer(email, "size")?;
        let cache_name = format!("{}.json", digest(id.as_bytes())?);
        if self.exists(&cache_name)? {
            let cached = self.json(&cache_name)?;
            if string(&cached, "id")? != id {
                return Err("migration-cache-collision");
            }
            if string(&cached, "blob")? == blob && integer(&cached, "size")? == size {
                let body = string(&cached, "body")?;
                self.verify_body(body, size)?;
                return Ok(body.to_owned());
            }
        }
        let mut file = self.temporary(size)?;
        let mut sha = Sha256::try_new().map_err(|_| "migration-digest")?;
        let url = source.download_url(blob)?;
        let response = source.request("GET", &url, &[], size as usize, |part| {
            sha.update(part)
                .map_err(|_| crate::outbound_http::Error::Sink)?;
            file.write_all(part)
                .map_err(|_| crate::outbound_http::Error::Sink)
        })?;
        if response.bytes as u64 != size {
            return Err("migration-body-size");
        }
        let body = hex(&sha.finish().map_err(|_| "migration-digest")?);
        file.sync_all().map_err(|_| "migration-archive-sync")?;
        let name = format!("{body}.eml");
        if self.exists(&name)? {
            self.verify_body(&body, size)?;
            fs::remove_file(self.path.join("partial")).map_err(|_| "migration-archive-write")?;
            self.sync()?;
        } else {
            self.install(&name, size)?;
        }
        let cached = json!({"id":id,"blob":blob,"size":size,"body":body});
        self.write_json(&cache_name, &cached)?;
        Ok(body)
    }
    fn verify_body(&self, body: &str, size: u64) -> Result<()> {
        if !hash_name(body) || size > crate::outbound_http::MAX_BODY as u64 {
            return Err("migration-invalid-body");
        }
        let mut file = self.open_file(&format!("{body}.eml"))?;
        if file.metadata().map_err(|_| "migration-archive-read")?.len() != size {
            return Err("migration-body-size");
        }
        let mut sha = Sha256::try_new().map_err(|_| "migration-digest")?;
        let mut buffer = [0; 8192];
        let mut count = 0u64;
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|_| "migration-archive-read")?;
            if n == 0 {
                break;
            }
            count = count
                .checked_add(n as u64)
                .filter(|n| *n <= size)
                .ok_or("migration-body-size")?;
            sha.update(buffer.get(..n).ok_or("migration-body-size")?)
                .map_err(|_| "migration-digest")?;
        }
        if count != size || hex(&sha.finish().map_err(|_| "migration-digest")?) != body {
            return Err("migration-body-digest");
        }
        Ok(())
    }
    pub fn publish(&mut self, manifest: &Json) -> Result<()> {
        self.write_json("manifest.json", manifest)
    }
    pub fn verify(&self) -> Result<Json> {
        let source = self.json("source.json")?;
        validate_source(&source)?;
        let manifest = self.json("manifest.json")?;
        validate_manifest(&manifest)?;
        if field(&manifest, "source")? != &source {
            return Err("migration-source-mismatch");
        }
        let records = array(&manifest, "messages")?;
        let mut bytes = 0u64;
        for record in records {
            let size = integer(field(record, "email")?, "size")?;
            self.verify_body(string(record, "body")?, size)?;
            bytes = bytes.checked_add(size).ok_or("migration-capacity")?;
        }
        Ok(
            json!({"status":"verified-provisional","messages":records.len(),"mailboxes":array(&manifest,"mailboxes")?.len(),"messageBytes":bytes,"archiveBytes":self.used,"manifestSha256":digest(manifest.to_string().as_bytes())?,"coverage":"per-mailbox-query","cutoverReady":false}),
        )
    }
}
fn validate_source(source: &Json) -> Result<()> {
    if integer(source, "version")? != 1 {
        return Err("migration-archive-version");
    }
    let instance = string(source, "instance")?;
    if instance.len() != 32 || !instance.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("migration-invalid-source");
    }
    crate::config::endpoint::HttpsUri::parse(string(source, "session")?)
        .map_err(|_| "migration-invalid-source")?;
    identifier(string(source, "account")?)?;
    identifier(string(source, "username")?)?;
    Ok(())
}
