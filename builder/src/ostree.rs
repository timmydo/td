//! Transactional materialization of one authenticated Flatpak OSTree deploy.
//!
//! Acquisition is `td-feed`'s job. This control-plane reader starts from an
//! exact reviewed commit, re-authenticates every reachable object, walks only
//! the commit's `files/` subtree, and publishes a plain tree for recursive
//! content-addressed interning. It neither reads mutable refs nor trusts the
//! acquisition manifest.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use td_engine::ostree::{
    self, ArchiveFileKind, Checksum, Dirtree, GraphLimits, ObjectKey, ObjectKind, GRAPH_LIMITS,
};

use crate::sys;

const REGULAR_TYPE: u32 = 0o100000;
const SYMLINK_TYPE: u32 = 0o120000;
const FILE_TYPE_MASK: u32 = 0o170000;
const EMLINK: i32 = 31;
const EINVAL: i32 = 22;
const ENOSYS: i32 = 38;
const EOPNOTSUPP: i32 = 95;
const MAX_STAGING_ATTEMPTS: u64 = 1_024;
const CURRENT_JAIL_ROOTS: &[&str] = &["dev", "etc", "home", "proc", "run", "tmp", "var"];
const REQUIRED_USR_ALIASES: &[&str] = &["bin", "lib", "lib64", "sbin"];
static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeployRoot {
    App,
    Runtime,
}

impl DeployRoot {
    fn from_exact_ref(exact_ref: &str) -> Result<Self, String> {
        ostree::validate_exact_ref(exact_ref)?;
        match exact_ref.split('/').next() {
            Some("app") => Ok(Self::App),
            Some("runtime") => Ok(Self::Runtime),
            _ => Err("validated OSTree ref lost its app/runtime kind".into()),
        }
    }

    fn component(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Runtime => "usr",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MaterializedStats {
    pub(crate) objects: usize,
    pub(crate) paths: usize,
    pub(crate) directories: usize,
    pub(crate) regular_files: usize,
    pub(crate) symlinks: usize,
    pub(crate) decoded_bytes: u64,
    pub(crate) transfer_bytes: u64,
    pub(crate) warnings: Vec<String>,
}

struct Budget {
    objects: BTreeSet<ObjectKey>,
    paths: usize,
    path_bytes: u64,
    decoded_bytes: u64,
    transfer_bytes: u64,
    limits: GraphLimits,
}

impl Budget {
    fn new(limits: GraphLimits) -> Self {
        Self {
            objects: BTreeSet::new(),
            paths: 0,
            path_bytes: 0,
            decoded_bytes: 0,
            transfer_bytes: 0,
            limits,
        }
    }

    fn admit_object(&mut self, key: ObjectKey, bytes: u64) -> Result<(), String> {
        if !self.objects.insert(key) {
            return Ok(());
        }
        if self.objects.len() > self.limits.objects {
            return Err(format!(
                "OSTree deploy has more than {} unique objects",
                self.limits.objects
            ));
        }
        self.transfer_bytes = self
            .transfer_bytes
            .checked_add(bytes)
            .ok_or_else(|| "OSTree deploy transfer-byte count overflow".to_string())?;
        if self.transfer_bytes > self.limits.transfer_bytes {
            return Err(format!(
                "OSTree deploy transfer exceeds {} bytes",
                self.limits.transfer_bytes
            ));
        }
        Ok(())
    }

    fn admit_path(&mut self, path: &str) -> Result<(), String> {
        if path.is_empty() || path.len() > self.limits.path_bytes {
            return Err(format!(
                "OSTree deploy path length must be in 1..={} bytes",
                self.limits.path_bytes
            ));
        }
        self.paths = self
            .paths
            .checked_add(1)
            .ok_or_else(|| "OSTree deploy path count overflow".to_string())?;
        if self.paths > self.limits.paths {
            return Err(format!(
                "OSTree deploy has more than {} paths",
                self.limits.paths
            ));
        }
        self.path_bytes = self
            .path_bytes
            .checked_add(
                u64::try_from(path.len())
                    .map_err(|_| "OSTree deploy path length does not fit u64".to_string())?,
            )
            .ok_or_else(|| "OSTree deploy path-byte count overflow".to_string())?;
        if self.path_bytes > self.limits.total_path_bytes {
            return Err(format!(
                "OSTree deploy path text exceeds {} bytes",
                self.limits.total_path_bytes
            ));
        }
        Ok(())
    }

    fn admit_decoded(&mut self, bytes: u64, references: usize) -> Result<(), String> {
        let references = u64::try_from(references)
            .map_err(|_| "OSTree file reference count does not fit u64".to_string())?;
        let logical = bytes
            .checked_mul(references)
            .ok_or_else(|| "OSTree deploy decoded-byte count overflow".to_string())?;
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(logical)
            .ok_or_else(|| "OSTree deploy decoded-byte count overflow".to_string())?;
        if self.decoded_bytes > self.limits.decoded_bytes {
            return Err(format!(
                "OSTree deploy decodes beyond {} bytes",
                self.limits.decoded_bytes
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct DirectoryTask {
    path: String,
    tree: Checksum,
    meta: Checksum,
    depth: usize,
}

#[derive(Clone)]
struct FileTask {
    path: String,
    checksum: Checksum,
}

struct StagingTree {
    work: PathBuf,
    tree: PathBuf,
    blobs: PathBuf,
    destination: PathBuf,
    requested_destination: PathBuf,
    parent: File,
}

fn destination_parent(destination: &Path) -> Result<&Path, String> {
    match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent),
        Some(_) => Ok(Path::new(".")),
        None => Err(format!(
            "OSTree deploy destination {} has no parent",
            destination.display()
        )),
    }
}

fn create_staging(destination: &Path) -> Result<StagingTree, String> {
    let parent = destination_parent(destination)?;
    let parent_file = File::open(parent)
        .map_err(|error| format!("open destination parent {}: {error}", parent.display()))?;
    let metadata = parent_file
        .metadata()
        .map_err(|error| format!("stat destination parent {}: {error}", parent.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "OSTree deploy destination parent {} is not a directory",
            parent.display()
        ));
    }
    let destination_name = destination.file_name().ok_or_else(|| {
        format!(
            "OSTree deploy destination {} has no final name",
            destination.display()
        )
    })?;
    let anchored_parent = PathBuf::from(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&parent_file)
    ));
    let anchored_destination = anchored_parent.join(destination_name);
    match fs::symlink_metadata(&anchored_destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(format!(
                "OSTree deploy destination {} already exists",
                destination.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "stat OSTree deploy destination {}: {error}",
                destination.display()
            ));
        }
    }
    for _ in 0..MAX_STAGING_ATTEMPTS {
        let sequence = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let work = anchored_parent.join(format!(
            ".td-ostree-materialize-{}-{sequence}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&work) {
            Ok(()) => {
                let tree = work.join("tree");
                let blobs = work.join("blobs");
                if let Err(error) = fs::create_dir(&tree) {
                    let error = staging_creation_error(
                        &work,
                        format!("create {}: {error}", tree.display()),
                    );
                    return Err(format!(
                        "materialize OSTree deploy {}: {error}",
                        destination.display()
                    ));
                }
                if let Err(error) = fs::create_dir(&blobs) {
                    let error = staging_creation_error(
                        &work,
                        format!("create {}: {error}", blobs.display()),
                    );
                    return Err(format!(
                        "materialize OSTree deploy {}: {error}",
                        destination.display()
                    ));
                }
                return Ok(StagingTree {
                    work,
                    tree,
                    blobs,
                    destination: anchored_destination,
                    requested_destination: destination.to_path_buf(),
                    parent: parent_file,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "materialize OSTree deploy {}: create {}: {error}",
                    destination.display(),
                    work.display()
                ));
            }
        }
    }
    Err("could not allocate a unique OSTree materialization directory".into())
}

fn staging_creation_error(work: &Path, error: String) -> String {
    match fs::remove_dir_all(work) {
        Ok(()) => error,
        Err(cleanup) => format!(
            "{error}; additionally could not remove private staging {}: {cleanup}",
            work.display()
        ),
    }
}

fn object_path(cache: &Path, key: ObjectKey) -> Result<PathBuf, String> {
    Ok(cache.join(key.relative_path()?))
}

fn read_object(cache: &Path, key: ObjectKey, budget: &mut Budget) -> Result<Vec<u8>, String> {
    let path = object_path(cache, key)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("stat OSTree object {}: {error}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > key.kind.max_transfer()?
    {
        return Err(format!(
            "OSTree object {} is not one bounded regular file",
            path.display()
        ));
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| format!("OSTree object {} is too large for memory", path.display()))?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(&path)
        .map_err(|error| format!("open OSTree object {}: {error}", path.display()))?
        .take(key.kind.max_transfer()?.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read OSTree object {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != metadata.len() {
        return Err(format!(
            "OSTree object {} changed while reading",
            path.display()
        ));
    }
    budget.admit_object(key, metadata.len())?;
    Ok(bytes)
}

fn child_path(parent: &str, name: &str, limits: GraphLimits) -> Result<String, String> {
    ostree::join_deploy_path(parent, name, limits.path_bytes)
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

fn select_files_root(
    exact_ref: &str,
    content: Checksum,
    commit: &ostree::Commit,
    root: &Dirtree,
) -> Result<DirectoryTask, String> {
    commit.metadata.require_exact_ref(exact_ref)?;
    if commit.content_checksum() != content {
        return Err(format!(
            "OSTree commit content checksum {} does not match reviewed checksum {}",
            commit.content_checksum().to_hex(),
            content.to_hex()
        ));
    }
    let mut selected = None;
    for directory in &root.directories {
        if directory.name != "files" {
            continue;
        }
        if selected.is_some() {
            return Err("reviewed OSTree commit has more than one files/ directory".into());
        }
        selected = Some(DirectoryTask {
            path: String::new(),
            tree: directory.tree,
            meta: directory.meta,
            depth: 0,
        });
    }
    selected.ok_or_else(|| "reviewed OSTree commit has no files/ directory".to_string())
}

fn collect_deploy(
    cache: &Path,
    exact_ref: &str,
    commit_checksum: Checksum,
    content_checksum: Checksum,
    budget: &mut Budget,
) -> Result<(Vec<String>, Vec<FileTask>), String> {
    let commit_bytes = read_object(
        cache,
        ObjectKey::new(ObjectKind::Commit, commit_checksum),
        budget,
    )?;
    let commit = ostree::parse_commit_verified(commit_checksum, &commit_bytes)?;
    let root_tree_bytes = read_object(
        cache,
        ObjectKey::new(ObjectKind::Dirtree, commit.root_tree),
        budget,
    )?;
    let root_tree = ostree::parse_dirtree_verified(commit.root_tree, &root_tree_bytes)?;
    let root_meta_bytes = read_object(
        cache,
        ObjectKey::new(ObjectKind::Dirmeta, commit.root_meta),
        budget,
    )?;
    let _ = ostree::parse_dirmeta_verified(commit.root_meta, &root_meta_bytes)?;

    let mut frontier = VecDeque::from([select_files_root(
        exact_ref,
        content_checksum,
        &commit,
        &root_tree,
    )?]);
    let mut parsed_trees = BTreeMap::<Checksum, Dirtree>::new();
    let mut parsed_metas = BTreeSet::from([commit.root_meta]);
    let mut directories = vec![String::new()];
    let mut files = Vec::new();
    while let Some(task) = frontier.pop_front() {
        admit_depth(task.depth, budget.limits)?;
        if parsed_metas.insert(task.meta) {
            let meta_bytes = read_object(
                cache,
                ObjectKey::new(ObjectKind::Dirmeta, task.meta),
                budget,
            )?;
            let _ = ostree::parse_dirmeta_verified(task.meta, &meta_bytes)?;
        }
        if let Entry::Vacant(entry) = parsed_trees.entry(task.tree) {
            let tree_bytes = read_object(
                cache,
                ObjectKey::new(ObjectKind::Dirtree, task.tree),
                budget,
            )?;
            entry.insert(ostree::parse_dirtree_verified(task.tree, &tree_bytes)?);
        }
        let tree = parsed_trees
            .get(&task.tree)
            .ok_or_else(|| "OSTree parsed-tree cache lost an entry".to_string())?;
        for file in &tree.files {
            let path = child_path(&task.path, &file.name, budget.limits)?;
            budget.admit_path(&path)?;
            files.push(FileTask {
                path,
                checksum: file.checksum,
            });
        }
        for directory in &tree.directories {
            let path = child_path(&task.path, &directory.name, budget.limits)?;
            budget.admit_path(&path)?;
            directories.push(path.clone());
            frontier.push_back(DirectoryTask {
                path,
                tree: directory.tree,
                meta: directory.meta,
                depth: task
                    .depth
                    .checked_add(1)
                    .ok_or_else(|| "OSTree deploy depth overflow".to_string())?,
            });
        }
    }
    Ok((directories, files))
}

fn normalize_absolute(target: &Path) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    for component in target.components() {
        match component {
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = parts.pop();
            }
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| "OSTree symlink target is not UTF-8".to_string())?;
                parts.push(value.to_string());
            }
            Component::Prefix(_) => {
                return Err("OSTree symlink target has a non-Unix prefix".into());
            }
        }
    }
    Ok(parts)
}

fn validate_absolute_target(target: &str) -> Result<(), String> {
    let parts = normalize_absolute(Path::new(target))?;
    if parts.first().map(String::as_str) == Some("run")
        && parts.get(1).map(String::as_str) == Some("host")
    {
        return Err("OSTree symlink targets Flatpak's refused /run/host view".into());
    }
    let first = parts.first().map(String::as_str);
    if matches!(first, Some("app" | "usr"))
        || first.is_some_and(|value| CURRENT_JAIL_ROOTS.contains(&value))
        || first.is_some_and(|value| REQUIRED_USR_ALIASES.contains(&value))
    {
        return Ok(());
    }
    Err(format!(
        "OSTree symlink target {target:?} is outside /app, /usr and synthesized jail paths"
    ))
}

fn validate_relative_target(root: DeployRoot, link_path: &str, target: &str) -> Result<(), String> {
    let mut parts = vec![root.component().to_string()];
    if let Some(parent) = Path::new(link_path).parent() {
        for component in parent.components() {
            if let Component::Normal(value) = component {
                let value = value
                    .to_str()
                    .ok_or_else(|| "OSTree symlink path is not UTF-8".to_string())?;
                parts.push(value.to_string());
            }
        }
    }
    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = parts.pop();
            }
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| "OSTree symlink target is not UTF-8".to_string())?;
                parts.push(value.to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("relative OSTree symlink target changed path roots".into());
            }
        }
    }
    let absolute = format!("/{}", parts.join("/"));
    validate_absolute_target(&absolute).map_err(|reason| {
        format!("OSTree symlink {link_path:?} target {target:?} is refused: {reason}")
    })
}

fn validate_symlink_target(root: DeployRoot, link_path: &str, target: &str) -> Result<(), String> {
    if target.starts_with('/') {
        validate_absolute_target(target)
    } else {
        validate_relative_target(root, link_path, target)
    }
}

fn set_epoch(file: &File, path: &Path) -> Result<(), String> {
    file.set_times(
        fs::FileTimes::new()
            .set_accessed(SystemTime::UNIX_EPOCH)
            .set_modified(SystemTime::UNIX_EPOCH),
    )
    .map_err(|error| format!("normalize timestamps on {}: {error}", path.display()))
}

fn make_directories(root: &Path, paths: &[String]) -> Result<(), String> {
    for path in paths.iter().filter(|path| !path.is_empty()) {
        let destination = root.join(path);
        let mut builder = fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&destination).map_err(|error| {
            format!("create deploy directory {}: {error}", destination.display())
        })?;
    }
    Ok(())
}

fn write_regular_blob(path: &Path, contents: &[u8], executable: bool) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("create deploy file {}: {error}", path.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("write deploy file {}: {error}", path.display()))?;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )
    .map_err(|error| format!("chmod deploy file {}: {error}", path.display()))?;
    set_epoch(&file, path)?;
    file.sync_all()
        .map_err(|error| format!("sync deploy file {}: {error}", path.display()))
}

fn link_regular_blob_with<Link>(
    blob: &Path,
    destination: &Path,
    contents: &[u8],
    executable: bool,
    link: Link,
) -> Result<(), String>
where
    Link: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    match link(blob, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(EMLINK) => {
            write_regular_blob(destination, contents, executable)
        }
        Err(error) => Err(format!(
            "link deploy file {} to {}: {error}",
            blob.display(),
            destination.display()
        )),
    }
}

fn link_regular_blob(
    blob: &Path,
    destination: &Path,
    contents: &[u8],
    executable: bool,
) -> Result<(), String> {
    link_regular_blob_with(blob, destination, contents, executable, |source, target| {
        fs::hard_link(source, target)
    })
}

fn materialize_files(
    cache: &Path,
    staging: &StagingTree,
    files: &[FileTask],
    budget: &mut Budget,
    root: DeployRoot,
) -> Result<(usize, usize), String> {
    let mut references = BTreeMap::<Checksum, Vec<&FileTask>>::new();
    for file in files {
        references.entry(file.checksum).or_default().push(file);
    }
    let mut regular_files = 0usize;
    let mut symlinks = 0usize;
    for (checksum, paths) in references {
        let key = ObjectKey::new(ObjectKind::File, checksum);
        let bytes = read_object(cache, key, budget)?;
        let logical = ostree::archive_file_logical_size(&bytes)?;
        budget.admit_decoded(logical, paths.len())?;
        let archive = ostree::decode_archive_file_verified(checksum, &bytes)?;
        match archive.kind {
            ArchiveFileKind::Regular(contents) => {
                if archive.mode & FILE_TYPE_MASK != REGULAR_TYPE {
                    return Err("authenticated OSTree regular file has inconsistent mode".into());
                }
                let blob = staging.blobs.join(checksum.to_hex());
                let executable = archive.mode & 0o111 != 0;
                write_regular_blob(&blob, &contents, executable)?;
                for task in paths {
                    let destination = staging.tree.join(&task.path);
                    link_regular_blob(&blob, &destination, &contents, executable)?;
                    regular_files = regular_files
                        .checked_add(1)
                        .ok_or_else(|| "OSTree regular-file count overflow".to_string())?;
                }
                fs::remove_file(&blob)
                    .map_err(|error| format!("remove deploy blob {}: {error}", blob.display()))?;
            }
            ArchiveFileKind::Symlink(target) => {
                if archive.mode & FILE_TYPE_MASK != SYMLINK_TYPE {
                    return Err("authenticated OSTree symlink has inconsistent mode".into());
                }
                for task in paths {
                    validate_symlink_target(root, &task.path, &target)?;
                    let destination = staging.tree.join(&task.path);
                    std::os::unix::fs::symlink(&target, &destination).map_err(|error| {
                        format!(
                            "create deploy symlink {} -> {:?}: {error}",
                            destination.display(),
                            target
                        )
                    })?;
                    symlinks = symlinks
                        .checked_add(1)
                        .ok_or_else(|| "OSTree symlink count overflow".to_string())?;
                }
            }
        }
    }
    Ok((regular_files, symlinks))
}

fn seal_directory(directory: &Path) -> Result<(), String> {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod deploy directory {}: {error}", directory.display()))?;
    let file = File::open(directory)
        .map_err(|error| format!("open deploy directory {}: {error}", directory.display()))?;
    set_epoch(&file, directory)?;
    file.sync_all()
        .map_err(|error| format!("sync deploy directory {}: {error}", directory.display()))
}

fn seal_directories_with<Seal>(root: &Path, paths: &[String], mut seal: Seal) -> Result<(), String>
where
    Seal: FnMut(&Path) -> Result<(), String>,
{
    for path in paths.iter().rev() {
        seal(&root.join(path))?;
    }
    Ok(())
}

fn seal_directories(root: &Path, paths: &[String]) -> Result<(), String> {
    seal_directories_with(root, paths, seal_directory)
}

fn syscall_path(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path {} contains a NUL byte", path.display()))
}

fn publication_error(staging: &StagingTree, error: std::io::Error) -> String {
    let unsupported = matches!(error.raw_os_error(), Some(EINVAL | ENOSYS | EOPNOTSUPP));
    if unsupported {
        return format!(
            "the kernel or destination filesystem for {} does not support required atomic no-replace publication: {error}",
            staging.requested_destination.display()
        );
    }
    format!(
        "publish OSTree deploy to {} without replacement: {error}",
        staging.requested_destination.display()
    )
}

fn publish_tree_with<SyncParent, RemoveWork>(
    staging: &StagingTree,
    sync_parent: SyncParent,
    remove_work: RemoveWork,
) -> Result<Vec<String>, String>
where
    SyncParent: FnOnce(&File) -> std::io::Result<()>,
    RemoveWork: FnOnce(&Path) -> std::io::Result<()>,
{
    fs::remove_dir(&staging.blobs)
        .map_err(|error| format!("remove {}: {error}", staging.blobs.display()))?;
    let source = syscall_path(&staging.tree)?;
    let destination_path = syscall_path(&staging.destination)?;
    sys::rename_noreplace(&source, &destination_path)
        .map_err(|error| publication_error(staging, error))?;

    // The rename is the commit point. Nothing after it may turn a complete,
    // visible destination into a reported pre-publication failure.
    let mut warnings = Vec::new();
    if let Err(error) = sync_parent(&staging.parent) {
        warnings.push(format!(
            "destination {} was committed but syncing its parent failed: {error}",
            staging.requested_destination.display()
        ));
    }
    if let Err(error) = remove_work(&staging.work) {
        warnings.push(format!(
            "destination {} was committed but removing empty staging {} failed: {error}",
            staging.requested_destination.display(),
            staging.work.display()
        ));
    }
    Ok(warnings)
}

fn publish_tree(staging: &StagingTree) -> Result<Vec<String>, String> {
    publish_tree_with(
        staging,
        |parent| parent.sync_all(),
        |work| fs::remove_dir(work),
    )
}

fn failed_with_staging_cleanup(staging: &StagingTree, error: String) -> String {
    let requested = staging.requested_destination.display();
    match fs::remove_dir_all(&staging.work) {
        Ok(()) => format!("materialize OSTree deploy {requested}: {error}"),
        Err(cleanup) => format!(
            "materialize OSTree deploy {requested}: {error}; additionally could not remove private staging {}: {cleanup}",
            staging.work.display()
        ),
    }
}

pub(crate) fn materialize(
    cache: &Path,
    exact_ref: &str,
    commit: &str,
    content: &str,
    destination: &Path,
) -> Result<MaterializedStats, String> {
    let root = DeployRoot::from_exact_ref(exact_ref)?;
    let commit = Checksum::from_hex(commit)?;
    let content = Checksum::from_hex(content)?;
    let cache_metadata = fs::symlink_metadata(cache)
        .map_err(|error| format!("stat OSTree cache {}: {error}", cache.display()))?;
    if !cache_metadata.file_type().is_dir() {
        return Err(format!(
            "OSTree cache {} is not a directory",
            cache.display()
        ));
    }
    let staging = create_staging(destination)?;
    let result = (|| {
        let mut budget = Budget::new(GRAPH_LIMITS);
        let (directories, files) = collect_deploy(cache, exact_ref, commit, content, &mut budget)?;
        make_directories(&staging.tree, &directories)?;
        let (regular_files, symlinks) =
            materialize_files(cache, &staging, &files, &mut budget, root)?;
        seal_directories(&staging.tree, &directories)?;
        let directories_count = directories
            .len()
            .checked_sub(1)
            .ok_or_else(|| "OSTree deploy lost its root directory".to_string())?;
        let mut stats = MaterializedStats {
            objects: budget.objects.len(),
            paths: budget.paths,
            directories: directories_count,
            regular_files,
            symlinks,
            decoded_bytes: budget.decoded_bytes,
            transfer_bytes: budget.transfer_bytes,
            warnings: Vec::new(),
        };
        stats.warnings = publish_tree(&staging)?;
        Ok(stats)
    })();
    result.map_err(|error| failed_with_staging_cleanup(&staging, error))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )]

    use super::*;
    use std::os::unix::fs::MetadataExt;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        commit: Checksum,
        content: Checksum,
        objects: Vec<(ObjectKey, Vec<u8>)>,
    }

    fn digest(bytes: &[u8]) -> Checksum {
        let mut hasher = td_engine::sha256::Sha256::new();
        hasher.update(bytes);
        Checksum::from_hex(&td_engine::sha256::to_base16(&hasher.finalize())).unwrap()
    }

    fn offset(value: usize, width: usize) -> Vec<u8> {
        let bytes = u64::try_from(value).unwrap().to_le_bytes();
        bytes[..width].to_vec()
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
            let alignment = alignments[index];
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
        let mut bytes = 1000u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&1000u32.to_be_bytes());
        bytes.extend_from_slice(&0o040700u32.to_be_bytes());
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

    fn archive_file(mode: u32, target: &str, contents: &[u8]) -> (Checksum, Vec<u8>) {
        let header = tuple(
            vec![
                u64::try_from(contents.len())
                    .unwrap()
                    .to_be_bytes()
                    .to_vec(),
                1000u32.to_be_bytes().to_vec(),
                1000u32.to_be_bytes().to_vec(),
                mode.to_be_bytes().to_vec(),
                0u32.to_be_bytes().to_vec(),
                text(target),
                Vec::new(),
            ],
            &[5],
            &[8, 4, 4, 4, 4, 1, 1],
        );
        let mut archive = u32::try_from(header.len()).unwrap().to_be_bytes().to_vec();
        archive.extend_from_slice(&[0; 4]);
        archive.extend_from_slice(&header);
        if mode & FILE_TYPE_MASK == REGULAR_TYPE {
            archive.extend_from_slice(&raw_stored(contents));
        }

        let data_len = 16usize.checked_add(target.len() + 1).unwrap();
        let width = serialized_width(data_len, 1);
        let variant_len = data_len.checked_add(width).unwrap();
        let mut canonical = u32::try_from(variant_len).unwrap().to_be_bytes().to_vec();
        canonical.extend_from_slice(&[0; 4]);
        canonical.extend_from_slice(&1000u32.to_be_bytes());
        canonical.extend_from_slice(&1000u32.to_be_bytes());
        canonical.extend_from_slice(&mode.to_be_bytes());
        canonical.extend_from_slice(&0u32.to_be_bytes());
        canonical.extend_from_slice(target.as_bytes());
        canonical.push(0);
        canonical.extend(offset(data_len, width));
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

    fn fixture(exact_ref: &str, files: Vec<(String, Checksum, Vec<u8>)>) -> Fixture {
        let metadata_bytes = dirmeta();
        let metadata = digest(&metadata_bytes);
        let mut file_entries: Vec<Vec<u8>> = files
            .iter()
            .map(|(name, checksum, _)| dirtree_file(name, *checksum))
            .collect();
        file_entries.sort();
        let files_tree_bytes = dirtree(&file_entries, &[]);
        let files_tree = digest(&files_tree_bytes);
        let root_tree_bytes = dirtree(&[], &[dirtree_directory("files", files_tree, metadata)]);
        let root_tree = digest(&root_tree_bytes);
        let commit_bytes = commit(exact_ref, root_tree, metadata);
        let commit_checksum = digest(&commit_bytes);
        let parsed = ostree::parse_commit_verified(commit_checksum, &commit_bytes).unwrap();
        let mut objects = vec![
            (
                ObjectKey::new(ObjectKind::Commit, commit_checksum),
                commit_bytes,
            ),
            (
                ObjectKey::new(ObjectKind::Dirtree, root_tree),
                root_tree_bytes,
            ),
            (
                ObjectKey::new(ObjectKind::Dirmeta, metadata),
                metadata_bytes,
            ),
            (
                ObjectKey::new(ObjectKind::Dirtree, files_tree),
                files_tree_bytes,
            ),
        ];
        for (_, checksum, bytes) in files {
            if !objects
                .iter()
                .any(|(key, _)| key.kind == ObjectKind::File && key.checksum == checksum)
            {
                objects.push((ObjectKey::new(ObjectKind::File, checksum), bytes));
            }
        }
        Fixture {
            commit: commit_checksum,
            content: parsed.content_checksum(),
            objects,
        }
    }

    fn nested_fixture(exact_ref: &str) -> Fixture {
        let metadata_bytes = dirmeta();
        let metadata = digest(&metadata_bytes);
        let (file_checksum, file_bytes) = archive_file(0o100644, "", b"nested");
        let b_tree_bytes = dirtree(&[dirtree_file("leaf", file_checksum)], &[]);
        let b_tree = digest(&b_tree_bytes);
        let a_tree_bytes = dirtree(&[], &[dirtree_directory("b", b_tree, metadata)]);
        let a_tree = digest(&a_tree_bytes);
        let share_tree_bytes = dirtree(&[], &[dirtree_directory("a", a_tree, metadata)]);
        let share_tree = digest(&share_tree_bytes);
        let files_tree_bytes = dirtree(&[], &[dirtree_directory("share", share_tree, metadata)]);
        let files_tree = digest(&files_tree_bytes);
        let root_tree_bytes = dirtree(&[], &[dirtree_directory("files", files_tree, metadata)]);
        let root_tree = digest(&root_tree_bytes);
        let commit_bytes = commit(exact_ref, root_tree, metadata);
        let commit_checksum = digest(&commit_bytes);
        let parsed = ostree::parse_commit_verified(commit_checksum, &commit_bytes).unwrap();
        Fixture {
            commit: commit_checksum,
            content: parsed.content_checksum(),
            objects: vec![
                (
                    ObjectKey::new(ObjectKind::Commit, commit_checksum),
                    commit_bytes,
                ),
                (
                    ObjectKey::new(ObjectKind::Dirtree, root_tree),
                    root_tree_bytes,
                ),
                (
                    ObjectKey::new(ObjectKind::Dirmeta, metadata),
                    metadata_bytes,
                ),
                (
                    ObjectKey::new(ObjectKind::Dirtree, files_tree),
                    files_tree_bytes,
                ),
                (
                    ObjectKey::new(ObjectKind::Dirtree, share_tree),
                    share_tree_bytes,
                ),
                (ObjectKey::new(ObjectKind::Dirtree, a_tree), a_tree_bytes),
                (ObjectKey::new(ObjectKind::Dirtree, b_tree), b_tree_bytes),
                (ObjectKey::new(ObjectKind::File, file_checksum), file_bytes),
            ],
        }
    }

    fn test_root(label: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "td-builder-ostree-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    fn write_fixture(root: &Path, fixture: &Fixture) -> PathBuf {
        let cache = root.join("cache");
        fs::create_dir(&cache).unwrap();
        for (key, bytes) in &fixture.objects {
            let path = object_path(&cache, *key).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        cache
    }

    #[test]
    fn symlink_policy_separates_current_roots_and_required_aliases() {
        assert_eq!(
            CURRENT_JAIL_ROOTS,
            ["dev", "etc", "home", "proc", "run", "tmp", "var"]
        );
        assert_eq!(REQUIRED_USR_ALIASES, ["bin", "lib", "lib64", "sbin"]);
        assert!(validate_symlink_target(DeployRoot::Runtime, "lib/a", "../share/a").is_ok());
        assert!(validate_symlink_target(
            DeployRoot::Runtime,
            "bin/ld.so",
            "../../lib64/ld-linux-x86-64.so.2"
        )
        .is_ok());
        assert!(validate_symlink_target(DeployRoot::App, "a", "../escape").is_err());
        for target in [
            "/app/lib/a",
            "/usr/lib/a",
            "/lib64/ld-linux-x86-64.so.2",
            "/etc/ssl/certs/ca-certificates.crt",
        ] {
            assert!(
                validate_symlink_target(DeployRoot::App, "a", target).is_ok(),
                "{target}"
            );
        }
        for target in [
            "/run/host/etc/passwd",
            "/mnt/ambient",
            "/opt/ambient",
            "/sys/devices",
        ] {
            assert!(
                validate_symlink_target(DeployRoot::Runtime, "a", target).is_err(),
                "{target}"
            );
        }
        let relative_host =
            validate_symlink_target(DeployRoot::Runtime, "a", "../run/host/etc/passwd")
                .unwrap_err();
        assert!(relative_host.contains("/run/host"), "{relative_host}");
    }

    #[test]
    fn deploy_root_is_derived_only_from_the_exact_ref() {
        assert_eq!(
            DeployRoot::from_exact_ref("app/example.App/x86_64/stable").unwrap(),
            DeployRoot::App
        );
        assert_eq!(
            DeployRoot::from_exact_ref("runtime/example.Platform/x86_64/stable").unwrap(),
            DeployRoot::Runtime
        );
        assert!(DeployRoot::from_exact_ref("usr/example.Platform/x86_64/stable").is_err());
    }

    #[test]
    fn materialization_graph_limits_fail_at_small_boundaries() {
        let one = GraphLimits {
            objects: 1,
            paths: 1,
            path_bytes: 3,
            total_path_bytes: 3,
            transfer_bytes: 1,
            decoded_bytes: 1,
            depth: 1,
        };
        let first = ObjectKey::new(
            ObjectKind::Commit,
            Checksum::from_hex("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
        );
        let second = ObjectKey::new(
            ObjectKind::Dirtree,
            Checksum::from_hex("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap(),
        );

        let mut objects = Budget::new(one);
        objects.admit_object(first, 0).unwrap();
        assert!(objects
            .admit_object(second, 0)
            .unwrap_err()
            .contains("more than 1 unique objects"));

        let mut transfer = Budget::new(GraphLimits { objects: 2, ..one });
        transfer.admit_object(first, 1).unwrap();
        assert!(transfer
            .admit_object(second, 1)
            .unwrap_err()
            .contains("transfer exceeds 1 bytes"));

        let mut count = Budget::new(GraphLimits {
            path_bytes: 32,
            total_path_bytes: 32,
            ..one
        });
        count.admit_path("a").unwrap();
        assert!(count
            .admit_path("b")
            .unwrap_err()
            .contains("more than 1 paths"));

        let mut length = Budget::new(GraphLimits {
            paths: 8,
            total_path_bytes: 32,
            ..one
        });
        assert!(length
            .admit_path("four")
            .unwrap_err()
            .contains("1..=3 bytes"));

        let mut aggregate = Budget::new(GraphLimits {
            paths: 8,
            path_bytes: 32,
            ..one
        });
        assert!(aggregate
            .admit_path("four")
            .unwrap_err()
            .contains("path text exceeds 3 bytes"));

        let mut decoded = Budget::new(one);
        decoded.admit_decoded(1, 1).unwrap();
        assert!(decoded
            .admit_decoded(1, 1)
            .unwrap_err()
            .contains("decodes beyond 1 bytes"));
        assert!(admit_depth(0, one).is_ok());
        assert!(admit_depth(1, one)
            .unwrap_err()
            .contains("exceeds 1 directory levels"));
    }

    #[test]
    fn destination_parent_accepts_a_bare_relative_name() {
        assert_eq!(
            destination_parent(Path::new("deploy")).unwrap(),
            Path::new(".")
        );
    }

    #[test]
    fn hard_link_ceiling_falls_back_to_an_independent_canonical_file() {
        let root = test_root("hard-link-fallback");
        let blob = root.join("blob");
        let destination = root.join("copy");
        write_regular_blob(&blob, b"fallback", true).unwrap();

        link_regular_blob_with(&blob, &destination, b"fallback", true, |_, _| {
            Err(std::io::Error::from_raw_os_error(EMLINK))
        })
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"fallback");
        let metadata = fs::metadata(&destination).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o755);
        assert_eq!(metadata.modified().unwrap(), SystemTime::UNIX_EPOCH);
        assert_ne!(metadata.ino(), fs::metadata(&blob).unwrap().ino());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_never_replaces_an_intervening_destination() {
        let root = test_root("intervening");
        let destination = root.join("deploy");
        let staging = create_staging(&destination).unwrap();
        fs::create_dir(&destination).unwrap();

        let error = publish_tree(&staging).unwrap_err();

        assert!(error.contains("without replacement"), "{error}");
        assert!(error.contains(destination.to_str().unwrap()), "{error}");
        assert!(destination.is_dir());
        assert!(staging.tree.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_no_replace_filesystems_are_diagnosed() {
        let root = test_root("unsupported-publication");
        let destination = root.join("deploy");
        let staging = create_staging(&destination).unwrap();

        for errno in [EINVAL, ENOSYS, EOPNOTSUPP] {
            let error = publication_error(&staging, std::io::Error::from_raw_os_error(errno));
            assert!(error.contains("does not support required"), "{error}");
            assert!(error.contains(destination.to_str().unwrap()), "{error}");
        }
        let ordinary = publication_error(&staging, std::io::Error::from_raw_os_error(17));
        assert!(ordinary.contains("without replacement"), "{ordinary}");
        assert!(!ordinary.contains("does not support"), "{ordinary}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staging_and_publication_stay_on_the_open_parent_after_a_retarget() {
        let root = test_root("parent-retarget");
        let first_parent = root.join("first");
        let second_parent = root.join("second");
        fs::create_dir(&first_parent).unwrap();
        fs::create_dir(&second_parent).unwrap();
        let selected_parent = root.join("selected");
        std::os::unix::fs::symlink(&first_parent, &selected_parent).unwrap();
        let requested_destination = selected_parent.join("deploy");
        let staging = create_staging(&requested_destination).unwrap();
        fs::write(staging.tree.join("anchored"), b"first").unwrap();

        fs::remove_file(&selected_parent).unwrap();
        std::os::unix::fs::symlink(&second_parent, &selected_parent).unwrap();
        let warnings = publish_tree(&staging).unwrap();

        assert!(warnings.is_empty());
        assert_eq!(
            fs::read(first_parent.join("deploy/anchored")).unwrap(),
            b"first"
        );
        assert!(!second_parent.join("deploy").exists());
        assert!(!requested_destination.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_one_materializer_can_publish_and_post_commit_errors_are_warnings() {
        let root = test_root("two-writers");
        let destination = root.join("deploy");
        let first = create_staging(&destination).unwrap();
        let second = create_staging(&destination).unwrap();
        fs::write(first.tree.join("winner"), b"first").unwrap();
        fs::write(second.tree.join("loser"), b"second").unwrap();

        assert!(publish_tree(&first).unwrap().is_empty());
        let error = publish_tree(&second).unwrap_err();
        assert!(error.contains("without replacement"), "{error}");
        assert_eq!(fs::read(destination.join("winner")).unwrap(), b"first");
        assert!(!destination.join("loser").exists());

        let warning_destination = root.join("warning-deploy");
        let warning_staging = create_staging(&warning_destination).unwrap();
        fs::write(warning_staging.tree.join("complete"), b"tree").unwrap();
        let warnings = publish_tree_with(
            &warning_staging,
            |_| Err(std::io::Error::other("injected parent sync failure")),
            |_| Err(std::io::Error::other("injected cleanup failure")),
        )
        .unwrap();
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            fs::read(warning_destination.join("complete")).unwrap(),
            b"tree"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_graph_materializes_transactionally_with_canonical_store_properties() {
        let exact_ref = "app/example.App/x86_64/stable";
        let (regular, regular_bytes) = archive_file(0o100711, "", b"hello");
        let (link, link_bytes) = archive_file(0o120777, "a", b"");
        let fixture = fixture(
            exact_ref,
            vec![
                ("a".into(), regular, regular_bytes.clone()),
                ("b".into(), regular, regular_bytes),
                ("link".into(), link, link_bytes),
            ],
        );
        let root = test_root("complete");
        let cache = write_fixture(&root, &fixture);
        let destination = root.join("deploy");
        let stats = materialize(
            &cache,
            exact_ref,
            &fixture.commit.to_hex(),
            &fixture.content.to_hex(),
            &destination,
        )
        .unwrap();

        assert_eq!(stats.objects, 6);
        assert_eq!(stats.paths, 3);
        assert_eq!(stats.directories, 0);
        assert_eq!(stats.regular_files, 2);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.decoded_bytes, 11);
        assert!(stats.warnings.is_empty());
        assert_eq!(fs::read(destination.join("a")).unwrap(), b"hello");
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("a")
        );
        let a = fs::metadata(destination.join("a")).unwrap();
        let b = fs::metadata(destination.join("b")).unwrap();
        assert_eq!(a.ino(), b.ino(), "one authenticated object is decoded once");
        assert_eq!(a.mode() & 0o7777, 0o755);
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(a.modified().unwrap(), SystemTime::UNIX_EPOCH);
        assert_eq!(
            fs::metadata(&destination).unwrap().modified().unwrap(),
            SystemTime::UNIX_EPOCH
        );
        assert!(fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".td-ostree-")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_graph_creates_and_seals_every_directory_in_parent_first_order() {
        let exact_ref = "runtime/example.Platform/x86_64/stable";
        let fixture = nested_fixture(exact_ref);
        let root = test_root("nested");
        let cache = write_fixture(&root, &fixture);
        let destination = root.join("deploy");

        let stats = materialize(
            &cache,
            exact_ref,
            &fixture.commit.to_hex(),
            &fixture.content.to_hex(),
            &destination,
        )
        .unwrap();

        assert_eq!(stats.objects, 8);
        assert_eq!(stats.paths, 4);
        assert_eq!(stats.directories, 3);
        assert_eq!(stats.regular_files, 1);
        assert_eq!(
            fs::read(destination.join("share/a/b/leaf")).unwrap(),
            b"nested"
        );
        for path in ["share", "share/a", "share/a/b"] {
            let metadata = fs::metadata(destination.join(path)).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o755);
            assert_eq!(metadata.modified().unwrap(), SystemTime::UNIX_EPOCH);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_sealing_visits_children_before_their_parents() {
        let paths = vec![
            String::new(),
            "share".to_string(),
            "share/a".to_string(),
            "share/a/b".to_string(),
        ];
        let mut observed = Vec::new();

        seal_directories_with(Path::new("/deploy"), &paths, |path| {
            observed.push(path.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(
            observed,
            [
                PathBuf::from("/deploy/share/a/b"),
                PathBuf::from("/deploy/share/a"),
                PathBuf::from("/deploy/share"),
                PathBuf::from("/deploy"),
            ]
        );
    }

    #[test]
    fn authentication_or_symlink_failure_never_publishes_a_destination() {
        let exact_ref = "runtime/example.Platform/x86_64/stable";
        let (link, link_bytes) = archive_file(0o120777, "/run/host/etc/passwd", b"");
        let fixture = fixture(exact_ref, vec![("bad-link".into(), link, link_bytes)]);
        let root = test_root("refusal");
        let cache = write_fixture(&root, &fixture);
        let destination = root.join("deploy");
        let error = materialize(
            &cache,
            exact_ref,
            &fixture.commit.to_hex(),
            &fixture.content.to_hex(),
            &destination,
        )
        .unwrap_err();
        assert!(error.contains("/run/host"), "{error}");
        assert!(!destination.exists());
        assert!(fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".td-ostree-materialize-")));

        let commit_path =
            object_path(&cache, ObjectKey::new(ObjectKind::Commit, fixture.commit)).unwrap();
        let mut corrupt = fs::read(&commit_path).unwrap();
        corrupt[0] ^= 1;
        fs::write(&commit_path, corrupt).unwrap();
        let second = root.join("corrupt");
        let error = materialize(
            &cache,
            exact_ref,
            &fixture.commit.to_hex(),
            &fixture.content.to_hex(),
            &second,
        )
        .unwrap_err();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(!second.exists());
        assert!(fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".td-ostree-materialize-")));
        fs::remove_dir_all(root).unwrap();
    }
}
