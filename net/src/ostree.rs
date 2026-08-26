//! Bounded acquisition of one immutable Flatpak OSTree deploy graph.
//!
//! This is host control-plane code. It addresses the reviewed commit object
//! directly, authenticates every object through `td_engine::ostree`, walks only
//! the commit's `files/` subtree, and publishes a cache directory only after
//! whole-graph bounds pass. Reuse re-authenticates and re-walks the complete
//! cache without network access; the eventual materializer repeats object
//! authentication before publishing a foreign payload.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use td_engine::ostree::{
    self, ArchiveFileKind, Checksum, Commit, DirectoryEntry, Dirtree, MAX_ARCHIVE_FILE_BYTES,
    MAX_ARCHIVE_INPUT_BYTES, MAX_METADATA_BYTES,
};

pub(crate) const MAX_GRAPH_OBJECTS: usize = 300_000;
pub(crate) const MAX_GRAPH_PATHS: usize = 262_144;
pub(crate) const MAX_GRAPH_PATH_BYTES: usize = 4_095;
pub(crate) const MAX_GRAPH_TOTAL_PATH_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_GRAPH_TRANSFER_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const MAX_GRAPH_DECODED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_GRAPH_DEPTH: usize = 256;
const MAX_FETCH_WORKERS: usize = 16;
const MAX_FILE_FETCH_WORKERS: usize = 8;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MANIFEST_NAME: &str = "graph.v1";
const OWNER_NAME: &str = "td-ostree-cache.v1";
const OWNER_STAGING_NAME: &str = "td-ostree-cache.v1.staging";
const RESERVATION_NAME: &str = "reservation";
const MAX_OWNER_BYTES: u64 = 4 * 1024;
const MAX_GRAPH_FETCH_TIME: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy)]
struct GraphLimits {
    objects: usize,
    paths: usize,
    path_bytes: usize,
    total_path_bytes: u64,
    transfer_bytes: u64,
    decoded_bytes: u64,
    depth: usize,
}

const GRAPH_LIMITS: GraphLimits = GraphLimits {
    objects: MAX_GRAPH_OBJECTS,
    paths: MAX_GRAPH_PATHS,
    path_bytes: MAX_GRAPH_PATH_BYTES,
    total_path_bytes: MAX_GRAPH_TOTAL_PATH_BYTES,
    transfer_bytes: MAX_GRAPH_TRANSFER_BYTES,
    decoded_bytes: MAX_GRAPH_DECODED_BYTES,
    depth: MAX_GRAPH_DEPTH,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AcquireSpec {
    repository: String,
    exact_ref: String,
    commit: Checksum,
    content: Checksum,
}

impl AcquireSpec {
    pub(crate) fn parse(
        repository: &str,
        exact_ref: &str,
        commit: &str,
        content: &str,
    ) -> Result<Self, String> {
        let repository = normalize_repository(repository)?;
        validate_exact_ref(exact_ref)?;
        Ok(Self {
            repository,
            exact_ref: exact_ref.to_string(),
            commit: Checksum::from_hex(commit)?,
            content: Checksum::from_hex(content)?,
        })
    }

    pub(crate) fn commit_hex(&self) -> String {
        self.commit.to_hex()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GraphStats {
    pub(crate) objects: usize,
    pub(crate) paths: usize,
    pub(crate) directories: usize,
    pub(crate) regular_files: usize,
    pub(crate) symlinks: usize,
    pub(crate) path_bytes: u64,
    pub(crate) decoded_bytes: u64,
    pub(crate) transfer_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ObjectKind {
    Commit,
    Dirtree,
    Dirmeta,
    File,
}

impl ObjectKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Dirtree => "dirtree",
            Self::Dirmeta => "dirmeta",
            Self::File => "filez",
        }
    }

    fn max_transfer(self) -> Result<u64, String> {
        let limit = match self {
            Self::File => MAX_ARCHIVE_INPUT_BYTES,
            Self::Commit | Self::Dirtree | Self::Dirmeta => MAX_METADATA_BYTES,
        };
        u64::try_from(limit).map_err(|_| "OSTree object limit does not fit u64".to_string())
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "commit" => Ok(Self::Commit),
            "dirtree" => Ok(Self::Dirtree),
            "dirmeta" => Ok(Self::Dirmeta),
            "filez" => Ok(Self::File),
            _ => Err(format!("unknown OSTree object kind {value:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectKey {
    kind: ObjectKind,
    checksum: Checksum,
}

impl ObjectKey {
    fn new(kind: ObjectKind, checksum: Checksum) -> Self {
        Self { kind, checksum }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObjectRecord {
    key: ObjectKey,
    transfer_bytes: u64,
    payload_bytes: u64,
    file_kind: Option<FileNodeKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileNodeKind {
    Regular,
    Symlink,
}

impl FileNodeKind {
    fn token(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Symlink => "symlink",
        }
    }

    fn parse(value: &str) -> Result<Option<Self>, String> {
        match value {
            "-" => Ok(None),
            "regular" => Ok(Some(Self::Regular)),
            "symlink" => Ok(Some(Self::Symlink)),
            _ => Err(format!("unknown OSTree file node kind {value:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryTask {
    path: String,
    tree: Checksum,
    meta: Checksum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchMode {
    FetchMissing,
    CacheOnly,
}

#[derive(Clone, Copy)]
struct FetchPolicy {
    mode: FetchMode,
    limits: GraphLimits,
    deadline: Option<Instant>,
}

struct WorkDirectory {
    path: Option<PathBuf>,
    transaction: PathBuf,
}

impl WorkDirectory {
    fn new(path: PathBuf, transaction: PathBuf) -> Self {
        Self {
            path: Some(path),
            transaction,
        }
    }

    fn path(&self) -> Result<&Path, String> {
        self.path
            .as_deref()
            .ok_or_else(|| "OSTree work directory was already published".to_string())
    }

    fn disarm(&mut self) {
        self.path = None;
    }

    fn transaction(&self) -> &Path {
        &self.transaction
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_dir_all(path);
            let _ = fs::remove_dir(&self.transaction);
        }
    }
}

fn normalize_repository(repository: &str) -> Result<String, String> {
    if repository.is_empty() || repository.len() > 2_048 {
        return Err("OSTree repository URL length must be in 1..=2048 bytes".into());
    }
    if repository.bytes().any(|byte| byte.is_ascii_whitespace())
        || repository.contains('?')
        || repository.contains('#')
    {
        return Err("OSTree repository URL contains whitespace, a query, or a fragment".into());
    }
    let normalized = repository.trim_end_matches('/');
    let scheme_end = normalized
        .find("://")
        .and_then(|at| at.checked_add(3))
        .ok_or_else(|| "OSTree repository URL has no authority".to_string())?;
    let scheme = normalized
        .get(..scheme_end.saturating_sub(3))
        .ok_or_else(|| "OSTree repository URL has no scheme".to_string())?;
    let remainder = normalized
        .get(scheme_end..)
        .ok_or_else(|| "OSTree repository URL has no authority".to_string())?;
    let authority = remainder.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err("OSTree repository URL has an empty authority".into());
    }
    if authority.contains('@') || authority.contains('\\') {
        return Err("OSTree repository URL authority contains userinfo or a backslash".into());
    }
    let secure = scheme == "https";
    let loopback = if scheme == "http" {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return Err("loopback-test HTTP repository has no explicit port".into());
        };
        let port = port
            .parse::<u16>()
            .map_err(|_| "loopback-test HTTP repository has an invalid port".to_string())?;
        matches!(host, "127.0.0.1" | "localhost") && port != 0
    } else {
        false
    };
    if !secure && !loopback {
        return Err("OSTree repository URL must use HTTPS (HTTP is loopback-test only)".into());
    }
    Ok(normalized.to_string())
}

fn validate_exact_ref(exact_ref: &str) -> Result<(), String> {
    if exact_ref.len() > 512 {
        return Err("OSTree ref exceeds its 512-byte limit".into());
    }
    let parts: Vec<&str> = exact_ref.split('/').collect();
    if parts.len() != 4 || !matches!(parts.first().copied(), Some("app" | "runtime")) {
        return Err("OSTree ref must be app/NAME/ARCH/BRANCH or runtime/NAME/ARCH/BRANCH".into());
    }
    for part in parts.iter().skip(1) {
        if part.is_empty()
            || matches!(*part, "." | "..")
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(format!("OSTree ref component {part:?} is unsafe"));
        }
    }
    Ok(())
}

fn object_relative_path(key: ObjectKey) -> Result<String, String> {
    let hex = key.checksum.to_hex();
    let prefix = hex
        .get(..2)
        .ok_or_else(|| "OSTree checksum has no two-byte path prefix".to_string())?;
    let tail = hex
        .get(2..)
        .ok_or_else(|| "OSTree checksum has no path tail".to_string())?;
    Ok(format!("objects/{prefix}/{tail}.{}", key.kind.suffix()))
}

fn object_path(root: &Path, key: ObjectKey) -> Result<PathBuf, String> {
    Ok(root.join(object_relative_path(key)?))
}

fn object_url(spec: &AcquireSpec, key: ObjectKey) -> Result<String, String> {
    Ok(format!(
        "{}/{}",
        spec.repository,
        object_relative_path(key)?
    ))
}

fn read_regular_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "OSTree object {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(format!(
            "OSTree object {} size must be in 1..={max_bytes} bytes",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| format!("OSTree object {} is too large for memory", path.display()))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != metadata.len() {
        return Err(format!(
            "OSTree object {} changed while reading",
            path.display()
        ));
    }
    Ok(bytes)
}

fn authenticate_object(
    key: ObjectKey,
    bytes: &[u8],
) -> Result<(u64, Option<FileNodeKind>), String> {
    match key.kind {
        ObjectKind::Commit => {
            let _ = ostree::parse_commit_verified(key.checksum, bytes)?;
            Ok((0, None))
        }
        ObjectKind::Dirtree => {
            let _ = ostree::parse_dirtree_verified(key.checksum, bytes)?;
            Ok((0, None))
        }
        ObjectKind::Dirmeta => {
            let _ = ostree::parse_dirmeta_verified(key.checksum, bytes)?;
            Ok((0, None))
        }
        ObjectKind::File => {
            let file = ostree::decode_archive_file_verified(key.checksum, bytes)?;
            let (size, kind) = match file.kind {
                ArchiveFileKind::Regular(contents) => (contents.len(), FileNodeKind::Regular),
                ArchiveFileKind::Symlink(target) => (target.len(), FileNodeKind::Symlink),
            };
            Ok((
                u64::try_from(size)
                    .map_err(|_| "OSTree file payload size does not fit u64".to_string())?,
                Some(kind),
            ))
        }
    }
}

fn prepare_object(
    spec: &AcquireSpec,
    root: &Path,
    key: ObjectKey,
    transfer_budget: &AtomicU64,
    policy: FetchPolicy,
) -> Result<u64, String> {
    let path = object_path(root, key)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("OSTree object path {} has no parent", path.display()))?;
    let max_transfer = key.kind.max_transfer()?;
    let (transfer_bytes, already_charged) = match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.len() > 0
                && metadata.len() <= max_transfer =>
        {
            (metadata.len(), false)
        }
        Ok(_) if policy.mode == FetchMode::CacheOnly => {
            return Err(format!(
                "cached OSTree object {} is not one bounded regular file",
                path.display()
            ));
        }
        Err(error)
            if policy.mode == FetchMode::CacheOnly
                && error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Err(format!(
                "cached OSTree object {} is missing",
                path.display()
            ));
        }
        Err(error) if policy.mode == FetchMode::CacheOnly => {
            return Err(format!("stat cached object {}: {error}", path.display()));
        }
        Ok(_) | Err(_) => {
            fs::create_dir_all(parent)
                .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
            let deadline = policy
                .deadline
                .ok_or_else(|| "OSTree network fetch has no absolute graph deadline".to_string())?;
            let downloaded = crate::http::get_to_file_accounted_no_redirects_before(
                &object_url(spec, key)?,
                &path,
                max_transfer,
                transfer_budget,
                policy.limits.transfer_bytes,
                deadline,
            )?;
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("stat {}: {error}", path.display()))?;
            if !metadata.file_type().is_file()
                || metadata.len() == 0
                || metadata.len() > max_transfer
                || metadata.len() != downloaded
            {
                return Err(format!(
                    "downloaded OSTree object {} is not one bounded regular file",
                    path.display()
                ));
            }
            (metadata.len(), true)
        }
    };
    if !already_charged {
        transfer_budget
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(transfer_bytes)
                    .filter(|next| *next <= policy.limits.transfer_bytes)
            })
            .map_err(|used| {
                format!(
                    "OSTree graph transfer exceeds {} bytes after {used}",
                    policy.limits.transfer_bytes
                )
            })?;
    }
    Ok(transfer_bytes)
}

fn seal_fetched_object(path: &Path, mode: FetchMode) -> Result<(), String> {
    if mode == FetchMode::FetchMissing {
        fs::set_permissions(path, fs::Permissions::from_mode(0o400))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    Ok(())
}

fn finish_object(
    root: &Path,
    key: ObjectKey,
    transfer_bytes: u64,
    mode: FetchMode,
) -> Result<ObjectRecord, String> {
    let path = object_path(root, key)?;
    let bytes = read_regular_bounded(&path, key.kind.max_transfer()?)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != transfer_bytes {
        return Err(format!(
            "OSTree object {} changed between admission and authentication",
            path.display()
        ));
    }
    let (payload_bytes, file_kind) = authenticate_object(key, &bytes)?;
    seal_fetched_object(&path, mode)?;
    Ok(ObjectRecord {
        key,
        transfer_bytes,
        payload_bytes,
        file_kind,
    })
}

fn finish_file_object(
    root: &Path,
    key: ObjectKey,
    transfer_bytes: u64,
    references: u64,
    decoded_bytes: &mut u64,
    mode: FetchMode,
    limits: GraphLimits,
) -> Result<ObjectRecord, String> {
    let path = object_path(root, key)?;
    let bytes = read_regular_bounded(&path, key.kind.max_transfer()?)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != transfer_bytes {
        return Err(format!(
            "OSTree object {} changed between admission and authentication",
            path.display()
        ));
    }
    let payload_bytes = ostree::archive_file_logical_size(&bytes)?;
    let logical = payload_bytes
        .checked_mul(references)
        .ok_or_else(|| "OSTree graph decoded byte count overflow".to_string())?;
    let next = decoded_bytes
        .checked_add(logical)
        .ok_or_else(|| "OSTree graph decoded byte count overflow".to_string())?;
    if next > limits.decoded_bytes {
        return Err(format!(
            "OSTree graph decodes beyond {} bytes",
            limits.decoded_bytes
        ));
    }
    let (authenticated_bytes, file_kind) = authenticate_object(key, &bytes)?;
    if authenticated_bytes != payload_bytes {
        return Err("OSTree archive admission and authentication sizes disagree".into());
    }
    *decoded_bytes = next;
    seal_fetched_object(&path, mode)?;
    Ok(ObjectRecord {
        key,
        transfer_bytes,
        payload_bytes,
        file_kind,
    })
}

fn fetch_object(
    spec: &AcquireSpec,
    root: &Path,
    key: ObjectKey,
    transfer_budget: &AtomicU64,
    policy: FetchPolicy,
) -> Result<ObjectRecord, String> {
    let transfer_bytes = prepare_object(spec, root, key, transfer_budget, policy)?;
    finish_object(root, key, transfer_bytes, policy.mode)
}

fn record_worker_error(error: &Mutex<Option<String>>, message: String) {
    if let Ok(mut slot) = error.lock() {
        if slot.is_none() {
            *slot = Some(message);
        }
    }
}

fn fetch_objects(
    spec: &AcquireSpec,
    root: &Path,
    keys: &[ObjectKey],
    transfer_budget: &AtomicU64,
    policy: FetchPolicy,
) -> Result<Vec<ObjectRecord>, String> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    if keys.iter().any(|key| key.kind == ObjectKind::File) {
        return Err("OSTree metadata fetch batch contains a file object".into());
    }
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let results = Mutex::new(Vec::with_capacity(keys.len()));
    let error = Mutex::new(None);
    let workers = keys.len().min(MAX_FETCH_WORKERS);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let index = next.fetch_add(1, Ordering::AcqRel);
                let Some(key) = keys.get(index).copied() else {
                    return;
                };
                match fetch_object(spec, root, key, transfer_budget, policy) {
                    Ok(record) => match results.lock() {
                        Ok(mut records) => records.push(record),
                        Err(_) => {
                            stop.store(true, Ordering::Release);
                            record_worker_error(
                                &error,
                                "OSTree fetch result lock was poisoned".into(),
                            );
                            return;
                        }
                    },
                    Err(message) => {
                        stop.store(true, Ordering::Release);
                        record_worker_error(&error, message);
                        return;
                    }
                }
            });
        }
    });
    let error = error
        .into_inner()
        .map_err(|_| "OSTree fetch error lock was poisoned".to_string())?;
    if let Some(message) = error {
        return Err(message);
    }
    let mut records = results
        .into_inner()
        .map_err(|_| "OSTree fetch result lock was poisoned".to_string())?;
    if records.len() != keys.len() {
        return Err(format!(
            "OSTree fetch completed {}/{} objects",
            records.len(),
            keys.len()
        ));
    }
    records.sort_by_key(|record| record.key);
    Ok(records)
}

fn fetch_file_objects(
    spec: &AcquireSpec,
    root: &Path,
    keys: &[ObjectKey],
    references: &BTreeMap<Checksum, u64>,
    transfer_budget: &AtomicU64,
    decoded_bytes: &mut u64,
    policy: FetchPolicy,
) -> Result<Vec<ObjectRecord>, String> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    if keys.iter().any(|key| key.kind != ObjectKind::File) {
        return Err("OSTree file fetch batch contains metadata".into());
    }
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let prepared = Mutex::new(Vec::with_capacity(keys.len()));
    let error = Mutex::new(None);
    let workers = keys.len().min(MAX_FILE_FETCH_WORKERS);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let index = next.fetch_add(1, Ordering::AcqRel);
                let Some(key) = keys.get(index).copied() else {
                    return;
                };
                match prepare_object(spec, root, key, transfer_budget, policy) {
                    Ok(transfer_bytes) => match prepared.lock() {
                        Ok(mut records) => records.push((key, transfer_bytes)),
                        Err(_) => {
                            stop.store(true, Ordering::Release);
                            record_worker_error(
                                &error,
                                "OSTree file fetch result lock was poisoned".into(),
                            );
                            return;
                        }
                    },
                    Err(message) => {
                        stop.store(true, Ordering::Release);
                        record_worker_error(&error, message);
                        return;
                    }
                }
            });
        }
    });
    let error = error
        .into_inner()
        .map_err(|_| "OSTree file fetch error lock was poisoned".to_string())?;
    if let Some(message) = error {
        return Err(message);
    }
    let mut prepared = prepared
        .into_inner()
        .map_err(|_| "OSTree file fetch result lock was poisoned".to_string())?;
    if prepared.len() != keys.len() {
        return Err(format!(
            "OSTree file fetch completed {}/{} objects",
            prepared.len(),
            keys.len()
        ));
    }
    prepared.sort_by_key(|(key, _)| *key);

    let mut records = Vec::with_capacity(prepared.len());
    // Archive authentication can hold both a 257 MiB transfer and a 256 MiB
    // decoded file. Keep that work serial even though the constant-memory HTTP
    // downloads above are concurrent.
    for (key, transfer_bytes) in prepared {
        let references = references
            .get(&key.checksum)
            .copied()
            .ok_or_else(|| "OSTree file object has no reference count".to_string())?;
        records.push(finish_file_object(
            root,
            key,
            transfer_bytes,
            references,
            decoded_bytes,
            policy.mode,
            policy.limits,
        )?);
    }
    Ok(records)
}

fn admit_object_count(total: usize, limits: GraphLimits) -> Result<(), String> {
    if total > limits.objects {
        return Err(format!(
            "OSTree graph has more than {} objects",
            limits.objects
        ));
    }
    Ok(())
}

fn add_new_objects(
    spec: &AcquireSpec,
    root: &Path,
    wanted: impl IntoIterator<Item = ObjectKey>,
    objects: &mut BTreeMap<ObjectKey, ObjectRecord>,
    transfer_budget: &AtomicU64,
    policy: FetchPolicy,
) -> Result<(), String> {
    let wanted: BTreeSet<ObjectKey> = wanted
        .into_iter()
        .filter(|key| !objects.contains_key(key))
        .collect();
    let total = objects
        .len()
        .checked_add(wanted.len())
        .ok_or_else(|| "OSTree graph object count overflow".to_string())?;
    admit_object_count(total, policy.limits)?;
    let keys: Vec<ObjectKey> = wanted.into_iter().collect();
    if keys.iter().any(|key| key.kind == ObjectKind::File) {
        return Err("OSTree metadata object set contains a file object".into());
    }
    let fetched = fetch_objects(spec, root, &keys, transfer_budget, policy)?;
    for record in fetched {
        if objects.insert(record.key, record).is_some() {
            return Err("OSTree graph fetched one object twice".into());
        }
    }
    Ok(())
}

fn add_file_objects(
    spec: &AcquireSpec,
    root: &Path,
    references: &BTreeMap<Checksum, u64>,
    objects: &mut BTreeMap<ObjectKey, ObjectRecord>,
    transfer_budget: &AtomicU64,
    decoded_bytes: &mut u64,
    policy: FetchPolicy,
) -> Result<(), String> {
    let keys: Vec<ObjectKey> = references
        .keys()
        .copied()
        .map(|checksum| ObjectKey::new(ObjectKind::File, checksum))
        .filter(|key| !objects.contains_key(key))
        .collect();
    let total = objects
        .len()
        .checked_add(keys.len())
        .ok_or_else(|| "OSTree graph object count overflow".to_string())?;
    admit_object_count(total, policy.limits)?;
    let fetched = fetch_file_objects(
        spec,
        root,
        &keys,
        references,
        transfer_budget,
        decoded_bytes,
        policy,
    )?;
    for record in fetched {
        if objects.insert(record.key, record).is_some() {
            return Err("OSTree graph fetched one file object twice".into());
        }
    }
    Ok(())
}

fn read_cached(root: &Path, key: ObjectKey) -> Result<Vec<u8>, String> {
    read_regular_bounded(&object_path(root, key)?, key.kind.max_transfer()?)
}

fn parse_cached_commit(root: &Path, checksum: Checksum) -> Result<Commit, String> {
    let key = ObjectKey::new(ObjectKind::Commit, checksum);
    ostree::parse_commit_verified(checksum, &read_cached(root, key)?)
}

fn parse_cached_dirtree(root: &Path, checksum: Checksum) -> Result<Dirtree, String> {
    let key = ObjectKey::new(ObjectKind::Dirtree, checksum);
    ostree::parse_dirtree_verified(checksum, &read_cached(root, key)?)
}

fn select_files_root(
    spec: &AcquireSpec,
    commit: &Commit,
    root_tree: &Dirtree,
) -> Result<DirectoryTask, String> {
    commit.metadata.require_exact_ref(&spec.exact_ref)?;
    if commit.content_checksum() != spec.content {
        return Err(format!(
            "commit content checksum {} does not match reviewed checksum {}",
            commit.content_checksum().to_hex(),
            spec.content.to_hex()
        ));
    }
    let matches: Vec<&DirectoryEntry> = root_tree
        .directories
        .iter()
        .filter(|entry| entry.name == "files")
        .collect();
    if matches.len() != 1 {
        return Err(if matches.is_empty() {
            "reviewed OSTree commit has no files/ directory".into()
        } else {
            "reviewed OSTree commit has more than one files/ directory".into()
        });
    }
    let Some(files) = matches.first().copied() else {
        return Err("reviewed OSTree commit has no files/ directory".into());
    };
    Ok(DirectoryTask {
        path: String::new(),
        tree: files.tree,
        meta: files.meta,
    })
}

struct PathBudget {
    count: usize,
    directories: usize,
    bytes: u64,
    limits: GraphLimits,
}

impl PathBudget {
    fn new(limits: GraphLimits) -> Self {
        Self {
            count: 0,
            directories: 0,
            bytes: 0,
            limits,
        }
    }

    fn admit(&mut self, path: &str) -> Result<(), String> {
        if path.is_empty() || path.len() > self.limits.path_bytes {
            return Err(format!(
                "OSTree deploy path length must be in 1..={} bytes",
                self.limits.path_bytes
            ));
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| "OSTree graph path count overflow".to_string())?;
        if self.count > self.limits.paths {
            return Err(format!(
                "OSTree graph has more than {} paths",
                self.limits.paths
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(
                u64::try_from(path.len())
                    .map_err(|_| "OSTree path length does not fit u64".to_string())?,
            )
            .ok_or_else(|| "OSTree aggregate path bytes overflow".to_string())?;
        if self.bytes > self.limits.total_path_bytes {
            return Err(format!(
                "OSTree graph path text exceeds {} bytes",
                self.limits.total_path_bytes
            ));
        }
        Ok(())
    }
}

fn child_path(parent: &str, name: &str, max_bytes: usize) -> Result<String, String> {
    let needed = parent
        .len()
        .checked_add(usize::from(!parent.is_empty()))
        .and_then(|length| length.checked_add(name.len()))
        .ok_or_else(|| "OSTree deploy path length overflow".to_string())?;
    if needed > max_bytes {
        return Err(format!("OSTree deploy path exceeds {max_bytes} bytes"));
    }
    if parent.is_empty() {
        Ok(name.to_string())
    } else {
        Ok(format!("{parent}/{name}"))
    }
}

fn expand_directory(
    task: &DirectoryTask,
    tree: &Dirtree,
    paths: &mut PathBudget,
    files: &mut BTreeMap<Checksum, u64>,
    next: &mut Vec<DirectoryTask>,
) -> Result<(), String> {
    for file in &tree.files {
        let path = child_path(&task.path, &file.name, paths.limits.path_bytes)?;
        paths.admit(&path)?;
        let references = files.entry(file.checksum).or_insert(0);
        *references = references
            .checked_add(1)
            .ok_or_else(|| "OSTree file reference count overflow".to_string())?;
    }
    for directory in &tree.directories {
        let path = child_path(&task.path, &directory.name, paths.limits.path_bytes)?;
        paths.admit(&path)?;
        paths.directories = paths
            .directories
            .checked_add(1)
            .ok_or_else(|| "OSTree directory count overflow".to_string())?;
        next.push(DirectoryTask {
            path,
            tree: directory.tree,
            meta: directory.meta,
        });
    }
    Ok(())
}

fn build_graph(
    spec: &AcquireSpec,
    root: &Path,
    mode: FetchMode,
) -> Result<(GraphStats, String), String> {
    build_graph_with_limits(spec, root, mode, GRAPH_LIMITS)
}

fn admit_depth(depth: usize, limits: GraphLimits) -> Result<(), String> {
    if depth >= limits.depth {
        return Err(format!(
            "OSTree files/ tree exceeds {} directory levels",
            limits.depth
        ));
    }
    Ok(())
}

fn build_graph_with_limits(
    spec: &AcquireSpec,
    root: &Path,
    mode: FetchMode,
    limits: GraphLimits,
) -> Result<(GraphStats, String), String> {
    let deadline = match mode {
        FetchMode::FetchMissing => Some(
            Instant::now()
                .checked_add(MAX_GRAPH_FETCH_TIME)
                .ok_or_else(|| "OSTree graph deadline overflow".to_string())?,
        ),
        FetchMode::CacheOnly => None,
    };
    let policy = FetchPolicy {
        mode,
        limits,
        deadline,
    };
    let transfer_budget = AtomicU64::new(0);
    let mut objects = BTreeMap::new();
    add_new_objects(
        spec,
        root,
        [ObjectKey::new(ObjectKind::Commit, spec.commit)],
        &mut objects,
        &transfer_budget,
        policy,
    )?;
    let commit = parse_cached_commit(root, spec.commit)?;

    add_new_objects(
        spec,
        root,
        [
            ObjectKey::new(ObjectKind::Dirtree, commit.root_tree),
            ObjectKey::new(ObjectKind::Dirmeta, commit.root_meta),
        ],
        &mut objects,
        &transfer_budget,
        policy,
    )?;
    let root_tree = parse_cached_dirtree(root, commit.root_tree)?;
    let mut frontier = vec![select_files_root(spec, &commit, &root_tree)?];
    let mut path_budget = PathBudget::new(limits);
    let mut file_references = BTreeMap::<Checksum, u64>::new();
    let mut parsed_trees = BTreeMap::<Checksum, Dirtree>::new();

    for depth in 0..=limits.depth {
        if frontier.is_empty() {
            break;
        }
        admit_depth(depth, limits)?;
        let wanted = frontier.iter().flat_map(|task| {
            [
                ObjectKey::new(ObjectKind::Dirtree, task.tree),
                ObjectKey::new(ObjectKind::Dirmeta, task.meta),
            ]
        });
        add_new_objects(spec, root, wanted, &mut objects, &transfer_budget, policy)?;
        for task in &frontier {
            if let std::collections::btree_map::Entry::Vacant(entry) = parsed_trees.entry(task.tree)
            {
                let tree = parse_cached_dirtree(root, task.tree)?;
                entry.insert(tree);
            }
        }
        let mut next = Vec::new();
        for task in &frontier {
            let tree = parsed_trees
                .get(&task.tree)
                .ok_or_else(|| "OSTree parsed-tree cache lost an entry".to_string())?;
            expand_directory(
                task,
                tree,
                &mut path_budget,
                &mut file_references,
                &mut next,
            )?;
        }
        frontier = next;
    }

    let mut decoded_bytes = 0u64;
    add_file_objects(
        spec,
        root,
        &file_references,
        &mut objects,
        &transfer_budget,
        &mut decoded_bytes,
        policy,
    )?;
    let mut regular_files = 0usize;
    let mut symlinks = 0usize;
    let mut checked_decoded_bytes = 0u64;
    for (checksum, references) in file_references {
        let record = objects
            .get(&ObjectKey::new(ObjectKind::File, checksum))
            .ok_or_else(|| "OSTree graph lost a fetched file object".to_string())?;
        let logical = record
            .payload_bytes
            .checked_mul(references)
            .ok_or_else(|| "OSTree graph decoded byte count overflow".to_string())?;
        checked_decoded_bytes = checked_decoded_bytes
            .checked_add(logical)
            .ok_or_else(|| "OSTree graph decoded byte count overflow".to_string())?;
        let references = usize::try_from(references)
            .map_err(|_| "OSTree file reference count does not fit usize".to_string())?;
        match record.file_kind {
            Some(FileNodeKind::Regular) => {
                regular_files = regular_files
                    .checked_add(references)
                    .ok_or_else(|| "OSTree regular-file count overflow".to_string())?;
            }
            Some(FileNodeKind::Symlink) => {
                symlinks = symlinks
                    .checked_add(references)
                    .ok_or_else(|| "OSTree symlink count overflow".to_string())?;
            }
            None => return Err("OSTree file object has no node kind".into()),
        }
    }
    if checked_decoded_bytes != decoded_bytes {
        return Err("OSTree decoded-byte admission and accounting disagree".into());
    }

    let stats = GraphStats {
        objects: objects.len(),
        paths: path_budget.count,
        directories: path_budget.directories,
        regular_files,
        symlinks,
        path_bytes: path_budget.bytes,
        decoded_bytes,
        transfer_bytes: transfer_budget.load(Ordering::Acquire),
    };
    Ok((stats, render_manifest(spec, stats, &objects)))
}

fn render_manifest(
    spec: &AcquireSpec,
    stats: GraphStats,
    objects: &BTreeMap<ObjectKey, ObjectRecord>,
) -> String {
    let mut manifest = format!(
        "format=1\nrepository={}\nref={}\ncommit={}\ncontent={}\nobject-count={}\npath-count={}\ndirectory-count={}\nregular-count={}\nsymlink-count={}\npath-bytes={}\ndecoded-bytes={}\ntransfer-bytes={}\n",
        spec.repository,
        spec.exact_ref,
        spec.commit.to_hex(),
        spec.content.to_hex(),
        stats.objects,
        stats.paths,
        stats.directories,
        stats.regular_files,
        stats.symlinks,
        stats.path_bytes,
        stats.decoded_bytes,
        stats.transfer_bytes
    );
    for record in objects.values() {
        manifest.push_str("object=");
        manifest.push_str(record.key.kind.suffix());
        manifest.push(',');
        manifest.push_str(&record.key.checksum.to_hex());
        manifest.push(',');
        manifest.push_str(&record.transfer_bytes.to_string());
        manifest.push(',');
        manifest.push_str(&record.payload_bytes.to_string());
        manifest.push(',');
        manifest.push_str(record.file_kind.map(FileNodeKind::token).unwrap_or("-"));
        manifest.push('\n');
    }
    manifest
}

fn render_owner(spec: &AcquireSpec) -> String {
    format!(
        "format=1\nrepository={}\nref={}\ncommit={}\ncontent={}\n",
        spec.repository,
        spec.exact_ref,
        spec.commit.to_hex(),
        spec.content.to_hex()
    )
}

fn require_owned_destination(spec: &AcquireSpec, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(destination)
        .map_err(|error| format!("stat {}: {error}", destination.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "refusing to replace non-cache destination {}",
            destination.display()
        ));
    }
    let owner_path = destination.join(OWNER_NAME);
    let bytes = read_regular_bounded(&owner_path, MAX_OWNER_BYTES).map_err(|error| {
        format!(
            "refusing to replace unowned destination {}: {error}",
            destination.display()
        )
    })?;
    if bytes != render_owner(spec).as_bytes() {
        return Err(format!(
            "refusing to replace destination {} owned by another OSTree graph; \
             use a distinct destination for each reviewed pin",
            destination.display()
        ));
    }
    Ok(())
}

fn take_field<'a>(lines: &mut impl Iterator<Item = &'a str>, key: &str) -> Result<&'a str, String> {
    let line = lines
        .next()
        .ok_or_else(|| format!("OSTree graph manifest has no {key} field"))?;
    line.strip_prefix(key)
        .and_then(|value| value.strip_prefix('='))
        .ok_or_else(|| format!("OSTree graph manifest expected {key}=..."))
}

fn parse_usize(value: &str, what: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("OSTree graph manifest {what} is not a usize"))
}

fn parse_u64(value: &str, what: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("OSTree graph manifest {what} is not a u64"))
}

fn parse_manifest(
    spec: &AcquireSpec,
    text: &str,
) -> Result<(GraphStats, Vec<ObjectRecord>), String> {
    if !text.ends_with('\n') || text.as_bytes().contains(&0) {
        return Err("OSTree graph manifest is not canonical newline text".into());
    }
    let mut lines = text.lines();
    if take_field(&mut lines, "format")? != "1"
        || take_field(&mut lines, "repository")? != spec.repository
        || take_field(&mut lines, "ref")? != spec.exact_ref
        || take_field(&mut lines, "commit")? != spec.commit.to_hex()
        || take_field(&mut lines, "content")? != spec.content.to_hex()
    {
        return Err("OSTree graph manifest does not match the requested exact deploy".into());
    }
    let stats = GraphStats {
        objects: parse_usize(take_field(&mut lines, "object-count")?, "object-count")?,
        paths: parse_usize(take_field(&mut lines, "path-count")?, "path-count")?,
        directories: parse_usize(
            take_field(&mut lines, "directory-count")?,
            "directory-count",
        )?,
        regular_files: parse_usize(take_field(&mut lines, "regular-count")?, "regular-count")?,
        symlinks: parse_usize(take_field(&mut lines, "symlink-count")?, "symlink-count")?,
        path_bytes: parse_u64(take_field(&mut lines, "path-bytes")?, "path-bytes")?,
        decoded_bytes: parse_u64(take_field(&mut lines, "decoded-bytes")?, "decoded-bytes")?,
        transfer_bytes: parse_u64(take_field(&mut lines, "transfer-bytes")?, "transfer-bytes")?,
    };
    if stats.objects > MAX_GRAPH_OBJECTS
        || stats.paths > MAX_GRAPH_PATHS
        || stats.path_bytes > MAX_GRAPH_TOTAL_PATH_BYTES
        || stats.decoded_bytes > MAX_GRAPH_DECODED_BYTES
        || stats.transfer_bytes > MAX_GRAPH_TRANSFER_BYTES
    {
        return Err("OSTree graph manifest exceeds a whole-graph bound".into());
    }
    let typed_paths = stats
        .directories
        .checked_add(stats.regular_files)
        .and_then(|count| count.checked_add(stats.symlinks))
        .ok_or_else(|| "OSTree graph typed path count overflow".to_string())?;
    if typed_paths != stats.paths {
        return Err("OSTree graph typed path counts disagree".into());
    }
    let mut records = Vec::with_capacity(stats.objects);
    let mut seen = BTreeSet::new();
    let mut transfer = 0u64;
    for line in lines {
        let value = line
            .strip_prefix("object=")
            .ok_or_else(|| "OSTree graph manifest has an unknown field".to_string())?;
        let mut fields = value.split(',');
        let kind = ObjectKind::parse(
            fields
                .next()
                .ok_or_else(|| "OSTree graph object has no kind".to_string())?,
        )?;
        let checksum = Checksum::from_hex(
            fields
                .next()
                .ok_or_else(|| "OSTree graph object has no checksum".to_string())?,
        )?;
        let transfer_bytes = parse_u64(
            fields
                .next()
                .ok_or_else(|| "OSTree graph object has no transfer size".to_string())?,
            "object transfer size",
        )?;
        let payload_bytes = parse_u64(
            fields
                .next()
                .ok_or_else(|| "OSTree graph object has no payload size".to_string())?,
            "object payload size",
        )?;
        let file_kind = FileNodeKind::parse(
            fields
                .next()
                .ok_or_else(|| "OSTree graph object has no file node kind".to_string())?,
        )?;
        if fields.next().is_some() {
            return Err("OSTree graph object has trailing fields".into());
        }
        let key = ObjectKey::new(kind, checksum);
        if !seen.insert(key) {
            return Err("OSTree graph manifest duplicates an object".into());
        }
        if transfer_bytes == 0 || transfer_bytes > kind.max_transfer()? {
            return Err("OSTree graph object transfer size is outside its bound".into());
        }
        if (kind == ObjectKind::File) != file_kind.is_some() {
            return Err("OSTree graph object kind and file node kind disagree".into());
        }
        let max_payload = if kind == ObjectKind::File {
            u64::try_from(MAX_ARCHIVE_FILE_BYTES)
                .map_err(|_| "OSTree file payload limit does not fit u64".to_string())?
        } else {
            0
        };
        if payload_bytes > max_payload {
            return Err("OSTree graph object payload size is outside its bound".into());
        }
        transfer = transfer
            .checked_add(transfer_bytes)
            .ok_or_else(|| "OSTree graph transfer total overflow".to_string())?;
        records.push(ObjectRecord {
            key,
            transfer_bytes,
            payload_bytes,
            file_kind,
        });
    }
    if records.len() != stats.objects || transfer != stats.transfer_bytes {
        return Err("OSTree graph manifest object totals disagree".into());
    }
    Ok((stats, records))
}

fn existing_graph(spec: &AcquireSpec, destination: &Path) -> Result<GraphStats, String> {
    let metadata = fs::symlink_metadata(destination)
        .map_err(|error| format!("stat {}: {error}", destination.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "OSTree graph destination {} is not a directory",
            destination.display()
        ));
    }
    let manifest_path = destination.join(MANIFEST_NAME);
    let bytes = read_regular_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "OSTree graph manifest is not UTF-8".to_string())?;
    let (manifest_stats, _) = parse_manifest(spec, text)?;
    let (verified_stats, verified_manifest) = build_graph(spec, destination, FetchMode::CacheOnly)?;
    if manifest_stats != verified_stats || text != verified_manifest {
        return Err("OSTree graph manifest does not match its authenticated objects".into());
    }
    Ok(verified_stats)
}

fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat old cache {}: {error}", path.display()))?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).map_err(|error| format!("remove {}: {error}", path.display()))
    } else {
        fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync directory {}: {error}", path.display()))
}

fn sync_object_directories(root: &Path) -> Result<(), String> {
    let objects = root.join("objects");
    let entries =
        fs::read_dir(&objects).map_err(|error| format!("read {}: {error}", objects.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", objects.display()))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("stat {}: {error}", path.display()))?;
        if !metadata.file_type().is_dir() {
            return Err(format!(
                "OSTree object fanout entry {} is not a directory",
                path.display()
            ));
        }
        sync_directory(&path)?;
    }
    sync_directory(&objects)
}

fn write_owner_marker(spec: &AcquireSpec, directory: &Path) -> Result<(), String> {
    let staging_path = directory.join(OWNER_STAGING_NAME);
    let owner_path = directory.join(OWNER_NAME);
    let mut owner_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o400)
        .open(&staging_path)
        .map_err(|error| format!("create {}: {error}", staging_path.display()))?;
    if let Err(error) = owner_file
        .write_all(render_owner(spec).as_bytes())
        .and_then(|()| owner_file.sync_all())
    {
        let _ = fs::remove_file(&staging_path);
        return Err(format!("write {}: {error}", staging_path.display()));
    }
    drop(owner_file);
    if let Err(error) = fs::rename(&staging_path, &owner_path) {
        let _ = fs::remove_file(&staging_path);
        return Err(format!(
            "publish owner marker {}: {error}",
            owner_path.display()
        ));
    }
    sync_directory(directory)
}

pub(crate) fn destination_parent(destination: &Path) -> Result<&Path, String> {
    match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent),
        Some(_) => Ok(Path::new(".")),
        None => Err(format!(
            "destination {} has no parent",
            destination.display()
        )),
    }
}

fn destination_identity(destination: &Path) -> Result<String, String> {
    let name = destination
        .file_name()
        .ok_or_else(|| format!("destination {} has no file name", destination.display()))?;
    let mut hasher = td_engine::sha256::Sha256::new();
    hasher.update(name.as_bytes());
    Ok(td_engine::sha256::to_base16(&hasher.finalize()))
}

fn transaction_path(parent: &Path, identity: &str) -> PathBuf {
    parent.join(format!(".{identity}.td-ostree-transaction"))
}

fn directory_is_empty(path: &Path) -> Result<bool, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Ok(false);
    }
    let mut entries =
        fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(format!("read {}: {error}", path.display())),
    }
}

fn recover_incomplete_transaction_directory(
    spec: &AcquireSpec,
    path: &Path,
) -> Result<bool, String> {
    let ownership_error = match require_owned_destination(spec, path) {
        Ok(()) => return Ok(true),
        Err(error) => error,
    };
    if directory_is_empty(path)? {
        fs::remove_dir(path).map_err(|error| format!("remove {}: {error}", path.display()))?;
        return Ok(false);
    }
    let staging = path.join(OWNER_STAGING_NAME);
    let mut entries =
        fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let only_staging = match (entries.next(), entries.next()) {
        (Some(Ok(entry)), None) => entry.file_name().as_bytes() == OWNER_STAGING_NAME.as_bytes(),
        _ => false,
    };
    if !only_staging {
        return Err(ownership_error);
    }
    let metadata = fs::symlink_metadata(&staging)
        .map_err(|error| format!("stat {}: {error}", staging.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_OWNER_BYTES {
        return Err(ownership_error);
    }
    fs::remove_file(&staging).map_err(|error| format!("remove {}: {error}", staging.display()))?;
    fs::remove_dir(path).map_err(|error| format!("remove {}: {error}", path.display()))?;
    Ok(false)
}

fn recover_reservation(
    spec: &AcquireSpec,
    destination: &Path,
    reservation: &Path,
) -> Result<(), String> {
    if !recover_incomplete_transaction_directory(spec, reservation)? {
        return Ok(());
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            if require_owned_destination(spec, destination).is_ok() {
                remove_path(reservation)?;
                return Ok(());
            }
            if !directory_is_empty(destination)? {
                return Err(format!(
                    "refusing to complete OSTree reservation into unowned destination {}",
                    destination.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(destination)
                .map_err(|error| format!("create {}: {error}", destination.display()))?;
        }
        Err(error) => return Err(format!("stat {}: {error}", destination.display())),
    }
    let owner = reservation.join(OWNER_NAME);
    let destination_owner = destination.join(OWNER_NAME);
    fs::rename(&owner, &destination_owner).map_err(|error| {
        format!(
            "complete OSTree reservation {}: {error}",
            destination.display()
        )
    })?;
    sync_directory(destination)?;
    sync_directory(destination_parent(destination)?)?;
    fs::remove_dir(reservation)
        .map_err(|error| format!("remove {}: {error}", reservation.display()))
}

fn recover_transaction(
    spec: &AcquireSpec,
    destination: &Path,
    transaction: &Path,
) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(transaction) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("stat {}: {error}", transaction.display())),
    };
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "OSTree transaction {} is not a directory",
            transaction.display()
        ));
    }
    let work = transaction.join("work");
    let old = transaction.join("old");
    let reservation = transaction.join(RESERVATION_NAME);
    for entry in fs::read_dir(transaction)
        .map_err(|error| format!("read {}: {error}", transaction.display()))?
    {
        let entry = entry.map_err(|error| format!("read {}: {error}", transaction.display()))?;
        if !matches!(
            entry.file_name().as_bytes(),
            b"work" | b"old" | b"reservation"
        ) {
            return Err(format!(
                "OSTree transaction {} contains an unknown entry",
                transaction.display()
            ));
        }
    }
    if fs::symlink_metadata(&reservation).is_ok() {
        recover_reservation(spec, destination, &reservation)?;
    }
    let work_exists = fs::symlink_metadata(&work).is_ok();
    let old_exists = fs::symlink_metadata(&old).is_ok();
    let work_owned = work_exists && recover_incomplete_transaction_directory(spec, &work)?;
    if old_exists {
        require_owned_destination(spec, &old)?;
    }
    if old_exists {
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                require_owned_destination(spec, destination)?;
                remove_path(&old)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::rename(&old, destination).map_err(|error| {
                    format!(
                        "restore stale OSTree cache {} to {}: {error}",
                        old.display(),
                        destination.display()
                    )
                })?;
                sync_directory(destination_parent(destination)?)?;
            }
            Err(error) => return Err(format!("stat {}: {error}", destination.display())),
        }
    }
    if work_owned {
        remove_path(&work)?;
    }
    fs::remove_dir(transaction)
        .map_err(|error| format!("remove {}: {error}", transaction.display()))
}

fn create_work_directory(spec: &AcquireSpec, transaction: &Path) -> Result<WorkDirectory, String> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(transaction)
        .map_err(|error| format!("create {}: {error}", transaction.display()))?;
    let path = transaction.join("work");
    let work = WorkDirectory::new(path.clone(), transaction.to_path_buf());
    builder
        .create(&path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    write_owner_marker(spec, work.path()?)?;
    Ok(work)
}

fn reserve_destination(
    spec: &AcquireSpec,
    destination: &Path,
    transaction: &Path,
) -> Result<bool, String> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            require_owned_destination(spec, destination)?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(transaction)
                .map_err(|error| format!("create {}: {error}", transaction.display()))?;
            let reservation = transaction.join(RESERVATION_NAME);
            builder
                .create(&reservation)
                .map_err(|error| format!("create {}: {error}", reservation.display()))?;
            write_owner_marker(spec, &reservation)?;
            sync_directory(transaction)?;
            match builder.create(destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    require_owned_destination(spec, destination)?;
                    remove_path(&reservation)?;
                    fs::remove_dir(transaction)
                        .map_err(|error| format!("remove {}: {error}", transaction.display()))?;
                    return Ok(false);
                }
                Err(error) => {
                    return Err(format!("create {}: {error}", destination.display()));
                }
            }
            let owner = reservation.join(OWNER_NAME);
            fs::rename(&owner, destination.join(OWNER_NAME)).map_err(|error| {
                format!(
                    "complete OSTree reservation {}: {error}",
                    destination.display()
                )
            })?;
            sync_directory(destination)?;
            sync_directory(destination_parent(destination)?)?;
            fs::remove_dir(&reservation)
                .map_err(|error| format!("remove {}: {error}", reservation.display()))?;
            fs::remove_dir(transaction)
                .map_err(|error| format!("remove {}: {error}", transaction.display()))?;
            Ok(true)
        }
        Err(error) => Err(format!("stat {}: {error}", destination.display())),
    }
}

fn publish_work(work: WorkDirectory, destination: &Path, spec: &AcquireSpec) -> Result<(), String> {
    publish_work_after_owner_check(work, destination, spec, || Ok(()))
}

fn publish_work_after_owner_check(
    mut work: WorkDirectory,
    destination: &Path,
    spec: &AcquireSpec,
    after_owner_check: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let parent = destination_parent(destination)?;
    require_owned_destination(spec, destination)?;
    after_owner_check()?;
    let old = work.transaction().join("old");
    if fs::symlink_metadata(&old).is_ok() {
        return Err(format!(
            "OSTree transaction already has an old cache at {}",
            old.display()
        ));
    }
    let old = {
        fs::rename(destination, &old).map_err(|error| {
            format!(
                "move old OSTree cache {} to {}: {error}",
                destination.display(),
                old.display()
            )
        })?;
        sync_directory(work.transaction())?;
        sync_directory(parent)?;
        old
    };
    if let Err(ownership_error) = require_owned_destination(spec, &old) {
        return match fs::symlink_metadata(destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::rename(&old, destination).map_err(|restore_error| {
                    format!(
                        "destination ownership changed during publication: {ownership_error}; \
                         restore {}: {restore_error}",
                        destination.display()
                    )
                })?;
                sync_directory(parent)?;
                Err(format!(
                    "destination ownership changed during publication: {ownership_error}"
                ))
            }
            Ok(_) => {
                work.disarm();
                Err(format!(
                    "destination ownership changed during publication: {ownership_error}; \
                     preserved moved state at {}",
                    old.display()
                ))
            }
            Err(error) => {
                work.disarm();
                Err(format!(
                    "destination ownership changed during publication: {ownership_error}; \
                     stat {}: {error}",
                    destination.display()
                ))
            }
        };
    }
    let work_path = work.path()?.to_path_buf();
    if let Err(error) = fs::rename(&work_path, destination) {
        let restore = fs::rename(&old, destination);
        if restore.is_err() {
            work.disarm();
        }
        return Err(match restore {
            Ok(()) => {
                let _ = sync_directory(parent);
                format!("publish OSTree graph {}: {error}", destination.display())
            }
            Err(restore_error) => format!(
                "publish OSTree graph {}: {error}; restore {}: {restore_error}",
                destination.display(),
                old.display()
            ),
        });
    }
    work.disarm();
    sync_directory(parent)?;
    if let Err(error) = require_owned_destination(spec, &old) {
        eprintln!(
            "warning: published OSTree graph but preserved changed old cache {}: {error}",
            old.display()
        );
        return Ok(());
    }
    if let Err(error) = remove_path(&old) {
        eprintln!(
            "warning: published OSTree graph but could not remove stale cache {}: {error}",
            old.display()
        );
        return Ok(());
    }
    if let Err(error) = fs::remove_dir(work.transaction()) {
        eprintln!(
            "warning: published OSTree graph but could not remove transaction {}: {error}",
            work.transaction().display()
        );
    }
    Ok(())
}

/// Acquire and transactionally publish an exact deploy graph.
///
/// Returns `(stats, fetched)`. A complete existing cache is re-authenticated
/// and re-walked without network I/O before it is accepted as a hit.
pub(crate) fn acquire(
    spec: &AcquireSpec,
    destination: &Path,
) -> Result<(GraphStats, bool), String> {
    let parent = destination_parent(destination)?;
    fs::create_dir_all(parent).map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
    let identity = destination_identity(destination)?;
    let lock_path = parent.join(format!(".{identity}.td-ostree.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
    lock.lock()
        .map_err(|error| format!("lock {}: {error}", lock_path.display()))?;
    let transaction = transaction_path(parent, &identity);
    recover_transaction(spec, destination, &transaction)?;
    let reserved = reserve_destination(spec, destination, &transaction)?;
    if !reserved {
        match existing_graph(spec, destination) {
            Ok(stats) => return Ok((stats, false)),
            Err(error) => eprintln!(
                "td-feed: cached OSTree graph {} was rejected: {error}; fetching an exact replacement",
                destination.display()
            ),
        }
    }

    let work = create_work_directory(spec, &transaction)?;
    let (stats, manifest) = build_graph(spec, work.path()?, FetchMode::FetchMissing)?;
    sync_object_directories(work.path()?)?;
    let manifest_path = work.path()?.join(MANIFEST_NAME);
    let mut manifest_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o400)
        .open(&manifest_path)
        .map_err(|error| format!("create {}: {error}", manifest_path.display()))?;
    manifest_file
        .write_all(manifest.as_bytes())
        .and_then(|()| manifest_file.sync_all())
        .map_err(|error| format!("write {}: {error}", manifest_path.display()))?;
    sync_directory(work.path()?)?;
    publish_work(work, destination, spec)?;
    Ok((stats, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn digest(bytes: &[u8]) -> Checksum {
        let mut hasher = td_engine::sha256::Sha256::new();
        hasher.update(bytes);
        Checksum::from_hex(&td_engine::sha256::to_base16(&hasher.finalize())).unwrap()
    }

    fn offset(value: usize, width: usize) -> Vec<u8> {
        match width {
            1 => vec![u8::try_from(value).unwrap()],
            2 => u16::try_from(value).unwrap().to_le_bytes().to_vec(),
            _ => u32::try_from(value).unwrap().to_le_bytes().to_vec(),
        }
    }

    fn framing_width(total: usize) -> usize {
        if total <= u8::MAX as usize {
            1
        } else if total <= u16::MAX as usize {
            2
        } else {
            4
        }
    }

    fn serialized_width(data_len: usize, offsets: usize) -> usize {
        for width in [1, 2, 4] {
            if framing_width(data_len + offsets * width) == width {
                return width;
            }
        }
        4
    }

    fn tuple(mut fields: Vec<Vec<u8>>, variable: &[usize], alignments: &[usize]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut ends = Vec::new();
        let field_count = fields.len();
        for (index, field) in fields.drain(..).enumerate() {
            let alignment = alignments.get(index).copied().unwrap();
            while data.len() % alignment != 0 {
                data.push(0);
            }
            data.extend(field);
            if variable.contains(&index) && index + 1 != field_count {
                ends.push(data.len());
            }
        }
        let width = serialized_width(data.len(), ends.len());
        for end in ends.into_iter().rev() {
            data.extend(offset(end, width));
        }
        data
    }

    fn array_aligned(values: &[Vec<u8>], alignment: usize) -> Vec<u8> {
        if values.is_empty() {
            return Vec::new();
        }
        let mut data = Vec::new();
        let mut ends = Vec::new();
        for value in values {
            while data.len() % alignment != 0 {
                data.push(0);
            }
            data.extend(value);
            ends.push(data.len());
        }
        let width = serialized_width(data.len(), ends.len());
        for end in ends {
            data.extend(offset(end, width));
        }
        data
    }

    fn array(values: &[Vec<u8>]) -> Vec<u8> {
        array_aligned(values, 1)
    }

    fn text(value: &str) -> Vec<u8> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    fn variant(mut value: Vec<u8>, value_type: &str) -> Vec<u8> {
        value.push(0);
        value.extend(value_type.as_bytes());
        value
    }

    fn metadata_entry(key: &str, value: Vec<u8>, value_type: &str) -> Vec<u8> {
        tuple(
            vec![text(key), variant(value, value_type)],
            &[0, 1],
            &[1, 8],
        )
    }

    fn dirmeta() -> Vec<u8> {
        let mut bytes = 0u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0o040755u32.to_be_bytes());
        bytes
    }

    fn dirtree_file(name: &str, checksum: Checksum) -> Vec<u8> {
        tuple(
            vec![text(name), checksum.as_bytes().to_vec()],
            &[0, 1],
            &[1, 1],
        )
    }

    fn dirtree_directory(name: &str, tree: Checksum, meta: Checksum) -> Vec<u8> {
        tuple(
            vec![
                text(name),
                tree.as_bytes().to_vec(),
                meta.as_bytes().to_vec(),
            ],
            &[0, 1, 2],
            &[1, 1, 1],
        )
    }

    fn dirtree(files: &[Vec<u8>], directories: &[Vec<u8>]) -> Vec<u8> {
        tuple(vec![array(files), array(directories)], &[0, 1], &[1, 1])
    }

    fn raw_stored(contents: &[u8]) -> Vec<u8> {
        let len = u16::try_from(contents.len()).unwrap();
        let mut bytes = vec![1];
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&(!len).to_le_bytes());
        bytes.extend_from_slice(contents);
        bytes
    }

    fn archive_regular(contents: &[u8]) -> (Checksum, Vec<u8>) {
        let header = tuple(
            vec![
                u64::try_from(contents.len())
                    .unwrap()
                    .to_be_bytes()
                    .to_vec(),
                0u32.to_be_bytes().to_vec(),
                0u32.to_be_bytes().to_vec(),
                0o100755u32.to_be_bytes().to_vec(),
                0u32.to_be_bytes().to_vec(),
                text(""),
                Vec::new(),
            ],
            &[5],
            &[8, 4, 4, 4, 4, 1, 1],
        );
        let mut archive = u32::try_from(header.len()).unwrap().to_be_bytes().to_vec();
        archive.extend_from_slice(&[0; 4]);
        archive.extend_from_slice(&header);
        archive.extend_from_slice(&raw_stored(contents));

        let mut canonical = 18u32.to_be_bytes().to_vec();
        canonical.extend_from_slice(&[0; 4]);
        canonical.extend_from_slice(&0u32.to_be_bytes());
        canonical.extend_from_slice(&0u32.to_be_bytes());
        canonical.extend_from_slice(&0o100755u32.to_be_bytes());
        canonical.extend_from_slice(&0u32.to_be_bytes());
        canonical.push(0);
        canonical.push(17);
        canonical.extend_from_slice(contents);
        (digest(&canonical), archive)
    }

    fn commit(exact_ref: &str, root_tree: Checksum, root_meta: Checksum) -> Vec<u8> {
        let entries = vec![
            metadata_entry("ostree.ref-binding", array(&[text(exact_ref)]), "as"),
            metadata_entry("xa.ref", text(exact_ref), "s"),
        ];
        tuple(
            vec![
                array_aligned(&entries, 8),
                Vec::new(),
                Vec::new(),
                text("fixture"),
                text("fixture"),
                1u64.to_be_bytes().to_vec(),
                root_tree.as_bytes().to_vec(),
                root_meta.as_bytes().to_vec(),
            ],
            &[0, 1, 2, 3, 4, 6, 7],
            &[8, 1, 1, 1, 1, 8, 1, 1],
        )
    }

    fn insert_object(objects: &mut BTreeMap<String, Vec<u8>>, key: ObjectKey, bytes: Vec<u8>) {
        objects.insert(format!("/{}", object_relative_path(key).unwrap()), bytes);
    }

    fn synthetic_graph_with_files(
        exact_ref: &str,
        mut files: Vec<(String, Checksum, Vec<u8>)>,
    ) -> (AcquireSpec, BTreeMap<String, Vec<u8>>) {
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let files_meta_bytes = dirmeta();
        let files_meta = digest(&files_meta_bytes);
        let file_entries: Vec<Vec<u8>> = files
            .iter()
            .map(|(name, checksum, _)| dirtree_file(name, *checksum))
            .collect();
        let files_tree_bytes = dirtree(&file_entries, &[]);
        let files_tree = digest(&files_tree_bytes);
        let root_meta_bytes = dirmeta();
        let root_meta = digest(&root_meta_bytes);
        let root_tree_bytes = dirtree(&[], &[dirtree_directory("files", files_tree, files_meta)]);
        let root_tree = digest(&root_tree_bytes);
        let commit_bytes = commit(exact_ref, root_tree, root_meta);
        let commit_checksum = digest(&commit_bytes);
        let parsed = ostree::parse_commit_verified(commit_checksum, &commit_bytes).unwrap();
        let spec = AcquireSpec::parse(
            "http://127.0.0.1:1/repo",
            exact_ref,
            &commit_checksum.to_hex(),
            &parsed.content_checksum().to_hex(),
        )
        .unwrap();
        let mut objects = BTreeMap::new();
        insert_object(
            &mut objects,
            ObjectKey::new(ObjectKind::Commit, commit_checksum),
            commit_bytes,
        );
        insert_object(
            &mut objects,
            ObjectKey::new(ObjectKind::Dirtree, root_tree),
            root_tree_bytes,
        );
        insert_object(
            &mut objects,
            ObjectKey::new(ObjectKind::Dirmeta, root_meta),
            root_meta_bytes,
        );
        insert_object(
            &mut objects,
            ObjectKey::new(ObjectKind::Dirtree, files_tree),
            files_tree_bytes,
        );
        insert_object(
            &mut objects,
            ObjectKey::new(ObjectKind::Dirmeta, files_meta),
            files_meta_bytes,
        );
        for (_, checksum, bytes) in files {
            insert_object(
                &mut objects,
                ObjectKey::new(ObjectKind::File, checksum),
                bytes,
            );
        }
        (spec, objects)
    }

    fn synthetic_graph(exact_ref: &str) -> (AcquireSpec, BTreeMap<String, Vec<u8>>) {
        let (checksum, bytes) = archive_regular(b"hello");
        synthetic_graph_with_files(exact_ref, vec![("hello".into(), checksum, bytes)])
    }

    fn serve_objects(
        objects: BTreeMap<String, Vec<u8>>,
        rounds: usize,
    ) -> Option<(String, std::thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let address = listener.local_addr().ok()?;
        let count = objects.len().checked_mul(rounds)?;
        let server = std::thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut connection, _)) = listener.accept() else {
                    return;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while request.len() < 8_192 && !request.ends_with(b"\r\n\r\n") {
                    let Ok(read) = connection.read(&mut byte) else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|path| path.strip_prefix("/repo"));
                let Some(body) = path.and_then(|path| objects.get(path)) else {
                    let _ = connection.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = connection.write_all(head.as_bytes());
                let _ = connection.write_all(body);
            }
        });
        Some((format!("http://{address}/repo"), server))
    }

    fn fixture_hex(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut high = None;
        for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => panic!("bad fixture hex"),
            };
            if let Some(first) = high.take() {
                out.push(first * 16 + value);
            } else {
                high = Some(value);
            }
        }
        assert!(high.is_none());
        out
    }

    fn firefox_spec() -> AcquireSpec {
        AcquireSpec::parse(
            "https://dl.flathub.org/repo/",
            "app/org.mozilla.firefox/x86_64/stable",
            "86ba63a1c2378a9525b495e1ba2c3ed9dc71ee92f67e45d8016cc4972024b410",
            "e511b540f42135f8703d6ea0f65abe3b798f93d4ab73ad27bf272d372a72fac3",
        )
        .unwrap()
    }

    #[test]
    fn exact_input_has_one_narrow_language() {
        let spec = firefox_spec();
        assert_eq!(spec.repository, "https://dl.flathub.org/repo");
        for bad in [
            "stable",
            "app/org.mozilla.firefox/x86_64",
            "app/org.mozilla.firefox/x86_64/stable/extra",
            "app/org.mozilla.firefox/x86_64/..",
            "sdk/org.example/x86_64/stable",
        ] {
            assert!(validate_exact_ref(bad).is_err(), "{bad}");
        }
        assert!(normalize_repository("http://example.com/repo").is_err());
        assert!(normalize_repository("http://127.0.0.1:1234/repo").is_ok());
        assert!(normalize_repository("http://localhost:80@example.com/repo").is_err());
        assert!(normalize_repository("http://127.0.0.1:80.evil.example/repo").is_err());
        assert!(normalize_repository("https://user@example.com/repo").is_err());
        assert!(normalize_repository("https://example.com/repo?ref=stable").is_err());
    }

    #[test]
    fn object_paths_are_type_separated_and_checksum_addressed() {
        let checksum =
            Checksum::from_hex("86ba63a1c2378a9525b495e1ba2c3ed9dc71ee92f67e45d8016cc4972024b410")
                .unwrap();
        assert_eq!(
            object_relative_path(ObjectKey::new(ObjectKind::Commit, checksum)).unwrap(),
            "objects/86/ba63a1c2378a9525b495e1ba2c3ed9dc71ee92f67e45d8016cc4972024b410.commit"
        );
        assert!(
            object_relative_path(ObjectKey::new(ObjectKind::File, checksum))
                .unwrap()
                .ends_with(".filez")
        );
    }

    #[test]
    fn real_firefox_commit_and_root_select_only_reviewed_files_tree() {
        let spec = firefox_spec();
        let commit_bytes = fixture_hex(include_str!(
            "../../engine/tests/fixtures/flathub-firefox-154.commit.hex"
        ));
        let commit = ostree::parse_commit_verified(spec.commit, &commit_bytes).unwrap();
        let tree_bytes = fixture_hex(include_str!(
            "../../engine/tests/fixtures/flathub-firefox-154-root.dirtree.hex"
        ));
        let root = ostree::parse_dirtree_verified(commit.root_tree, &tree_bytes).unwrap();

        let files = select_files_root(&spec, &commit, &root).unwrap();

        assert_eq!(files.path, "");
        assert_eq!(
            files.tree.to_hex(),
            "cc470790cb3756c9ce173512a2b6f9a882a1f5dd91f21cd9230c349b4db1c8b0"
        );
        let wrong = AcquireSpec {
            content: Checksum::from_hex(
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            )
            .unwrap(),
            ..spec
        };
        assert!(select_files_root(&wrong, &commit, &root)
            .unwrap_err()
            .contains("content checksum"));
    }

    #[test]
    fn logical_paths_and_references_are_counted_before_fetch() {
        let checksum =
            Checksum::from_hex("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let meta =
            Checksum::from_hex("2222222222222222222222222222222222222222222222222222222222222222")
                .unwrap();
        let tree = Dirtree {
            files: vec![ostree::FileEntry {
                name: "firefox".into(),
                checksum,
            }],
            directories: vec![DirectoryEntry {
                name: "lib".into(),
                tree: checksum,
                meta,
            }],
        };
        let mut paths = PathBudget::new(GRAPH_LIMITS);
        let mut files = BTreeMap::new();
        let mut next = Vec::new();
        expand_directory(
            &DirectoryTask {
                path: "bin".into(),
                tree: checksum,
                meta,
            },
            &tree,
            &mut paths,
            &mut files,
            &mut next,
        )
        .unwrap();
        assert_eq!(paths.count, 2);
        assert_eq!(files.get(&checksum), Some(&1));
        assert_eq!(next.first().map(|task| task.path.as_str()), Some("bin/lib"));
    }

    #[test]
    fn manifest_is_strict_and_round_trips_the_completion_record() {
        let spec = firefox_spec();
        let key = ObjectKey::new(ObjectKind::Commit, spec.commit);
        let record = ObjectRecord {
            key,
            transfer_bytes: 123,
            payload_bytes: 0,
            file_kind: None,
        };
        let objects = BTreeMap::from([(key, record)]);
        let stats = GraphStats {
            objects: 1,
            paths: 480,
            directories: 184,
            regular_files: 151,
            symlinks: 145,
            path_bytes: 4_000,
            decoded_bytes: 333_800_000,
            transfer_bytes: 123,
        };
        let manifest = render_manifest(&spec, stats, &objects);
        assert_eq!(parse_manifest(&spec, &manifest).unwrap().0, stats);
        assert!(parse_manifest(&spec, &manifest.replace("format=1", "format=2")).is_err());
        assert!(parse_manifest(&spec, &format!("{manifest}unknown=x\n")).is_err());
        let duplicate = format!("{manifest}object=commit,{},123,0,-\n", spec.commit.to_hex());
        assert!(parse_manifest(&spec, &duplicate).is_err());
    }

    #[test]
    fn acquisition_walks_authenticates_publishes_and_then_reuses() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let exact_ref = "app/org.example.Fixture/x86_64/stable";
        let (mut spec, objects) = synthetic_graph(exact_ref);
        let Some((repository, server)) = serve_objects(objects, 4) else {
            return;
        };
        spec.repository = repository;
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let destination =
            std::env::temp_dir().join(format!("td-ostree-acquire-{}-{nonce}", std::process::id()));

        let (stats, fetched) = acquire(&spec, &destination).unwrap();
        assert!(fetched);
        assert_eq!(stats.objects, 5);
        assert_eq!(stats.paths, 1);
        assert_eq!(stats.directories, 0);
        assert_eq!(stats.regular_files, 1);
        assert_eq!(stats.symlinks, 0);
        assert_eq!(stats.decoded_bytes, 5);
        let (again, fetched) = acquire(&spec, &destination).unwrap();
        assert!(
            !fetched,
            "a complete graph must not contact the dead server"
        );
        assert_eq!(again, stats);

        let manifest = fs::read_to_string(destination.join(MANIFEST_NAME)).unwrap();
        let (_, records) = parse_manifest(&spec, &manifest).unwrap();
        let file = records
            .iter()
            .find(|record| record.key.kind == ObjectKind::File)
            .unwrap();
        let file_path = object_path(&destination, file.key).unwrap();
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut corrupted = fs::read(&file_path).unwrap();
        let first = corrupted.first_mut().unwrap();
        *first ^= 0xff;
        fs::write(&file_path, corrupted).unwrap();

        let (repaired, fetched) = acquire(&spec, &destination).unwrap();
        assert!(
            fetched,
            "same-size cache corruption must force a fresh graph"
        );
        assert_eq!(repaired, stats);

        fs::remove_file(&file_path).unwrap();
        let (restored, fetched) = acquire(&spec, &destination).unwrap();
        assert!(fetched, "a missing cache object must force a fresh graph");
        assert_eq!(restored, stats);

        let manifest_path = destination.join(MANIFEST_NAME);
        let mut malformed_manifest = fs::read_to_string(&manifest_path).unwrap();
        malformed_manifest.push_str("unknown=field\n");
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&manifest_path, malformed_manifest).unwrap();
        let (remade, fetched) = acquire(&spec, &destination).unwrap();
        assert!(fetched, "a malformed manifest must force a fresh graph");
        assert_eq!(remade, stats);
        server.join().unwrap();

        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o444)).unwrap();
        let (final_hit, fetched) = acquire(&spec, &destination).unwrap();
        assert!(!fetched, "the repaired graph must be an offline cache hit");
        assert_eq!(final_hit, stats);
        assert_eq!(
            fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
            0o444,
            "offline authentication must not mutate published cache modes"
        );
        let _ = fs::remove_dir_all(&destination);
        let lock = destination.parent().unwrap().join(format!(
            ".{}.td-ostree.lock",
            destination_identity(&destination).unwrap()
        ));
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn acquisition_never_replaces_an_unowned_destination() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let destination =
            std::env::temp_dir().join(format!("td-ostree-unowned-{}-{nonce}", std::process::id()));
        fs::create_dir(&destination).unwrap();
        let sentinel = destination.join("keep");
        fs::write(&sentinel, b"mine").unwrap();

        let error = acquire(&spec, &destination).unwrap_err();

        assert!(error.contains("unowned destination"));
        assert_eq!(fs::read(&sentinel).unwrap(), b"mine");
        let _ = fs::remove_dir_all(&destination);
        let lock = destination.parent().unwrap().join(format!(
            ".{}.td-ostree.lock",
            destination_identity(&destination).unwrap()
        ));
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn a_destination_is_bound_to_one_exact_pin() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (owner, _) = synthetic_graph("app/org.example.Owner/x86_64/stable");
        let (other, _) = synthetic_graph("app/org.example.Other/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let destination = std::env::temp_dir().join(format!(
            "td-ostree-other-pin-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&destination).unwrap();
        write_owner_marker(&owner, &destination).unwrap();

        let error = acquire(&other, &destination).unwrap_err();

        assert!(error.contains("owned by another OSTree graph"), "{error}");
        require_owned_destination(&owner, &destination).unwrap();
        let _ = fs::remove_dir_all(&destination);
        let lock = destination.parent().unwrap().join(format!(
            ".{}.td-ostree.lock",
            destination_identity(&destination).unwrap()
        ));
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn bare_relative_destinations_use_the_current_directory() {
        assert_eq!(
            destination_parent(Path::new("cache")).unwrap(),
            Path::new(".")
        );
        assert_eq!(
            destination_parent(Path::new("nested/cache")).unwrap(),
            Path::new("nested")
        );
    }

    #[test]
    fn whole_graph_limit_helpers_fail_at_small_boundaries() {
        let one = GraphLimits {
            objects: 1,
            paths: 1,
            path_bytes: 3,
            total_path_bytes: 3,
            transfer_bytes: 1,
            decoded_bytes: 1,
            depth: 1,
        };
        assert!(admit_object_count(1, one).is_ok());
        assert!(admit_object_count(2, one)
            .unwrap_err()
            .contains("more than 1 objects"));
        assert!(admit_depth(0, one).is_ok());
        assert!(admit_depth(1, one)
            .unwrap_err()
            .contains("exceeds 1 directory levels"));

        let mut count = PathBudget::new(GraphLimits {
            path_bytes: 32,
            total_path_bytes: 32,
            ..one
        });
        count.admit("a").unwrap();
        assert!(count.admit("b").unwrap_err().contains("more than 1 paths"));
        let mut length = PathBudget::new(GraphLimits {
            paths: 8,
            total_path_bytes: 32,
            ..one
        });
        assert!(length.admit("four").unwrap_err().contains("1..=3 bytes"));
        let mut aggregate = PathBudget::new(GraphLimits {
            paths: 8,
            path_bytes: 32,
            ..one
        });
        assert!(aggregate
            .admit("four")
            .unwrap_err()
            .contains("path text exceeds 3 bytes"));

        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let key = ObjectKey::new(ObjectKind::File, spec.commit);
        let error = fetch_objects(
            &spec,
            Path::new("/does-not-matter"),
            &[key],
            &AtomicU64::new(0),
            FetchPolicy {
                mode: FetchMode::CacheOnly,
                limits: GRAPH_LIMITS,
                deadline: None,
            },
        )
        .unwrap_err();
        assert!(error.contains("metadata fetch batch contains a file"));
    }

    #[test]
    fn decoded_limit_stops_before_a_later_archive_is_inflated() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let exact_ref = "app/org.example.DecodeBound/x86_64/stable";
        let (good_checksum, good_bytes) = archive_regular(b"hello");
        let (_, mut corrupt_bytes) = archive_regular(b"world");
        let corrupt_checksum =
            Checksum::from_hex("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
                .unwrap();
        *corrupt_bytes.last_mut().unwrap() ^= 0xff;
        let (mut spec, objects) = synthetic_graph_with_files(
            exact_ref,
            vec![
                ("a".into(), good_checksum, good_bytes),
                ("b".into(), corrupt_checksum, corrupt_bytes),
            ],
        );
        let Some((repository, server)) = serve_objects(objects, 1) else {
            return;
        };
        spec.repository = repository;
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "td-ostree-decoded-bound-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let limits = GraphLimits {
            decoded_bytes: 5,
            ..GRAPH_LIMITS
        };

        let error =
            build_graph_with_limits(&spec, &root, FetchMode::FetchMissing, limits).unwrap_err();

        assert!(error.contains("decodes beyond 5 bytes"), "{error}");
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn publication_rechecks_ownership_after_the_fetch_window() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "td-ostree-publish-race-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("cache");
        let identity = destination_identity(&destination).unwrap();
        let transaction = transaction_path(&parent, &identity);
        let work = create_work_directory(&spec, &transaction).unwrap();

        fs::create_dir(&destination).unwrap();
        let sentinel = destination.join("keep");
        fs::write(&sentinel, b"mine").unwrap();
        let error = publish_work(work, &destination, &spec).unwrap_err();

        assert!(error.contains("unowned destination"), "{error}");
        assert_eq!(fs::read(&sentinel).unwrap(), b"mine");
        assert!(!transaction.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn publication_restores_a_destination_swapped_after_its_owner_check() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "td-ostree-owner-check-race-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("cache");
        fs::create_dir(&destination).unwrap();
        write_owner_marker(&spec, &destination).unwrap();
        let displaced = parent.join("displaced");
        let foreign = parent.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("keep"), b"mine").unwrap();
        let identity = destination_identity(&destination).unwrap();
        let transaction = transaction_path(&parent, &identity);
        let work = create_work_directory(&spec, &transaction).unwrap();

        let error = publish_work_after_owner_check(work, &destination, &spec, || {
            fs::rename(&destination, &displaced)
                .map_err(|error| format!("move checked fixture: {error}"))?;
            fs::rename(&foreign, &destination)
                .map_err(|error| format!("install foreign fixture: {error}"))
        })
        .unwrap_err();

        assert!(
            error.contains("ownership changed during publication"),
            "{error}"
        );
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"mine");
        require_owned_destination(&spec, &displaced).unwrap();
        assert!(!transaction.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn stale_owned_transaction_work_is_reaped_under_the_destination_lock() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "td-ostree-stale-work-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("cache");
        let identity = destination_identity(&destination).unwrap();
        let transaction = transaction_path(&parent, &identity);
        let mut work = create_work_directory(&spec, &transaction).unwrap();
        fs::write(work.path().unwrap().join("partial"), b"partial").unwrap();
        work.disarm();
        drop(work);
        assert!(transaction.exists());

        recover_transaction(&spec, &destination, &transaction).unwrap();

        assert!(!transaction.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn an_interrupted_empty_work_directory_is_reaped() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "td-ostree-empty-work-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("cache");
        let identity = destination_identity(&destination).unwrap();
        let transaction = transaction_path(&parent, &identity);
        fs::create_dir(&transaction).unwrap();
        fs::create_dir(transaction.join("work")).unwrap();

        recover_transaction(&spec, &destination, &transaction).unwrap();

        assert!(!transaction.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn an_interrupted_reservation_completes_its_empty_destination() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "td-ostree-reservation-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("cache");
        let identity = destination_identity(&destination).unwrap();
        let transaction = transaction_path(&parent, &identity);
        let reservation = transaction.join(RESERVATION_NAME);
        fs::create_dir(&transaction).unwrap();
        fs::create_dir(&reservation).unwrap();
        write_owner_marker(&spec, &reservation).unwrap();
        fs::create_dir(&destination).unwrap();

        recover_transaction(&spec, &destination, &transaction).unwrap();

        require_owned_destination(&spec, &destination).unwrap();
        assert!(!transaction.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn stale_owned_old_is_restored_when_publication_was_interrupted() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (spec, _) = synthetic_graph("app/org.example.Fixture/x86_64/stable");
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "td-ostree-restore-old-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("cache");
        let identity = destination_identity(&destination).unwrap();
        let transaction = transaction_path(&parent, &identity);
        let old = transaction.join("old");
        fs::create_dir(&transaction).unwrap();
        fs::create_dir(&old).unwrap();
        write_owner_marker(&spec, &old).unwrap();

        recover_transaction(&spec, &destination, &transaction).unwrap();

        require_owned_destination(&spec, &destination).unwrap();
        assert!(!transaction.exists());
        let _ = fs::remove_dir_all(parent);
    }
}
