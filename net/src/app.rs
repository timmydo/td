//! td-app: fetch, admit and select one account's Flathub application
//! (APPLICATIONS.md §W.9).
//!
//! `td-app update NAME [--force]` runs as the application's account and owns
//! `$HOME/.td/flathub`. It follows the declared refs as hints, admits a
//! candidate only under rules 1–5, and publishes a generation by replacing
//! `current` with one rename. A refusal or failure leaves `current` as it was.
//! `td-app ready NAME` names the generation td-jail may launch.
//!
//! A provisioned feed (td-vm/DESIGN.md) is tried first for every ref and
//! is transport only: whatever it serves passes the same rules. The host
//! fills it with `td-feed export flathub`, which runs [`export`].
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use td_engine::application_spec::dynamic_application_policy;
use td_engine::flathub_app::{
    self, Admitted, FlathubSource, Selection, ADMITTED, CACHE_DIR, CACHE_ENTRIES, CHECKED, CURRENT,
    DEPLOY, FILES, GENERATIONS, LOCK, OBJECTS, PREVIOUS, SELECTION, WORK,
};
use td_engine::ostree::{self, Checksum, Commit, ObjectKey, ObjectKind};

use crate::ostree::AcquireSpec;

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const RESOLVE_TIME: Duration = Duration::from_secs(10 * 60);
/// A ref file is 64 hex digits and a newline.
const MAX_REF_BYTES: u64 = 65;
/// Part of every checks digest: raise it when rule 5's validator changes
/// what it admits, so selections it never saw are validated again.
const CHECKS_VERSION: &str = "td-app-checks-1";
/// Every directory td-app lists holds a handful of entries; more is damage.
const MAX_LISTED_ENTRIES: usize = 4096;
const ENOTDIR: i32 = 20;
const ELOOP: i32 = 40;
/// A deploy or object graph being removed is renamed to this prefix
/// first, so an interrupted removal never leaves a `files/` that reads as
/// complete; the next collection finishes it.
const REMOVING: &str = ".removing-";

/// Checks the signature and ref bindings of one fetched commit (rules 1–2).
type Verify = fn(&str, Checksum, &[u8], &[u8], u64) -> Result<Commit, String>;
/// The dynamic validator over an application and runtime tree (rule 5).
type Validate = fn(&Path, &str, &Path, &[String], &[String], usize) -> Result<(), String>;

struct Admission<'a> {
    repository: &'a str,
    /// A td-feed base serving refs under FEED_REFS and graphs under
    /// `ostree/COMMIT/`.
    feed: Option<&'a str>,
    verify: Verify,
    validate: Validate,
}

/// Where a feed names the commit it holds for a ref.
const FEED_REFS: &str = "flathub/refs/heads";
/// In the host export cache: the commits whose graphs the last export
/// published, so the next one removes those no ref names.
const EXPORTED: &str = "exported";

#[derive(Debug, Eq, PartialEq)]
enum Outcome {
    Ready(Option<u64>),
    Throttled,
    Unchanged,
    Selected(u64),
}

/// What the declaration asks for before anything is fetched.
struct Expected {
    checks: String,
    library_paths: Vec<String>,
    optional_targets: Vec<String>,
    optional_links: usize,
}

struct Candidate {
    admitted: Admitted,
    content: Checksum,
    metadata: Option<String>,
    /// The detached signature metadata, which a feed export republishes.
    commitmeta: Vec<u8>,
    /// Whether the feed served this ref, so its graph comes from there too.
    from_feed: bool,
}

fn verify_flathub(
    exact_ref: &str,
    checksum: Checksum,
    commit: &[u8],
    commitmeta: &[u8],
    now: u64,
) -> Result<Commit, String> {
    crate::flathub::verify_commit(exact_ref, checksum, commit, commitmeta, now)
        .map(|signed| signed.commit)
}

fn validate_dynamic(
    out: &Path,
    entry: &str,
    runtime: &Path,
    library_paths: &[String],
    optional_targets: &[String],
    optional_links: usize,
) -> Result<(), String> {
    td_engine::dynamic_application::validate_dynamic_application(
        out,
        entry,
        runtime,
        library_paths,
        optional_targets,
        optional_links,
    )
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|error| format!("system clock is before the epoch: {error}"))
}

fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("stat {}: {error}", path.display()))?;
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(format!("{} is not a directory", path.display()))
            }
        }
        Err(error) => Err(format!("mkdir {}: {error}", path.display())),
    }
}

fn prepare_cache(cache: &Path) -> Result<(), String> {
    let parent = cache
        .parent()
        .ok_or_else(|| format!("cache {} has no parent", cache.display()))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
    private_dir(cache)?;
    fs::set_permissions(cache, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod {}: {error}", cache.display()))?;
    for name in [OBJECTS, DEPLOY, GENERATIONS] {
        private_dir(&cache.join(name))?;
    }
    Ok(())
}

/// Open a regular file without following a final symlink, or `None`.
fn open_regular(path: &Path, options: &mut OpenOptions) -> Result<Option<File>, String> {
    let file = match options
        .custom_flags(crate::feed::OPEN_REGULAR_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(Some(file))
}

fn lock(cache: &Path) -> Result<File, String> {
    let path = cache.join(LOCK);
    let file = open_regular(
        &path,
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600),
    )?
    .ok_or_else(|| format!("{} vanished", path.display()))?;
    file.lock()
        .map_err(|error| format!("lock {}: {error}", path.display()))?;
    Ok(file)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let Some(file) = open_regular(path, OpenOptions::new().read(true))? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > limit) {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    Ok(Some(bytes))
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    let limit = u64::try_from(flathub_app::MAX_STATE_BYTES)
        .map_err(|_| "state limit does not fit u64".to_string())?;
    read_bounded(path, limit)?
        .map(|bytes| {
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))
        })
        .transpose()
}

fn write_state(path: &Path, text: &str) -> Result<(), String> {
    crate::td_fs_replace::replace(path, text.as_bytes(), 0o600)
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// The generation a pointer names, and its selection.
fn read_pointer(cache: &Path, name: &str) -> Result<Option<(u64, Selection)>, String> {
    let link = cache.join(name);
    let target = match fs::read_link(&link) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", link.display())),
    };
    let target = target
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", link.display()))?;
    let generation = flathub_app::parse_pointer(target)
        .map_err(|error| format!("{}: {error}", link.display()))?;
    let path = cache
        .join(GENERATIONS)
        .join(generation.to_string())
        .join(SELECTION);
    let text = read_text(&path)?.ok_or_else(|| format!("{} is missing", path.display()))?;
    Ok(Some((generation, Selection::parse(&text)?)))
}

/// `.work` is recreated under the lock just before these fetches, and only
/// td-app writes the cache outside the jail, so the download path is fresh.
fn fetch_small(url: &str, path: &Path, limit: u64, deadline: Instant) -> Result<Vec<u8>, String> {
    let budget = AtomicU64::new(0);
    let fetched = crate::http::get_to_file_accounted_no_redirects_before(
        url, path, limit, &budget, limit, deadline,
    )
    .and_then(|_| read_bounded(path, limit))
    .and_then(|bytes| bytes.ok_or_else(|| format!("{} vanished", path.display())));
    let _ = fs::remove_file(path);
    fetched
}

fn object_url(repository: &str, key: ObjectKey) -> Result<String, String> {
    Ok(format!("{repository}/{}", key.relative_path()?))
}

fn commitmeta_url(repository: &str, commit: Checksum) -> Result<String, String> {
    let hex = commit.to_hex();
    let (Some(fanout), Some(rest)) = (hex.get(..2), hex.get(2..)) else {
        return Err("commit checksum is too short".into());
    };
    Ok(format!("{repository}/objects/{fanout}/{rest}.commitmeta"))
}

/// Rules 1–3 for whatever commit `exact_ref` names now. `known` is every
/// row the cache holds, so the floor is never below a selected commit.
fn resolve(
    admission: &Admission<'_>,
    work: &Path,
    exact_ref: &str,
    known: &[&Admitted],
    now: u64,
    deadline: Instant,
) -> Result<Candidate, String> {
    let from_feed = match admission.feed {
        Some(base) => fetch_small(
            &format!("{base}/{FEED_REFS}/{exact_ref}"),
            &work.join("ref"),
            MAX_REF_BYTES,
            deadline,
        )
        .map_err(|error| eprintln!("td-app: the feed has no {exact_ref} ({error}); asking Flathub"))
        .ok(),
        None => None,
    };
    let (head, from_feed) = match from_feed {
        Some(head) => (head, true),
        None => (
            fetch_small(
                &format!("{}/refs/heads/{exact_ref}", admission.repository),
                &work.join("ref"),
                MAX_REF_BYTES,
                deadline,
            )?,
            false,
        ),
    };
    let hex = std::str::from_utf8(&head)
        .ok()
        .map(|text| text.strip_suffix('\n').unwrap_or(text))
        .ok_or_else(|| format!("{exact_ref} does not name a commit"))?;
    let commit = Checksum::from_hex(hex).map_err(|error| format!("{exact_ref}: {error}"))?;
    let repository = match admission.feed {
        Some(base) if from_feed => crate::ostree::feed_graph_base(base, commit),
        _ => admission.repository.to_string(),
    };
    let commit_bytes = fetch_small(
        &object_url(&repository, ObjectKey::new(ObjectKind::Commit, commit))?,
        &work.join("commit"),
        ObjectKind::Commit.max_transfer()?,
        deadline,
    )?;
    let commitmeta_limit = u64::try_from(ostree::MAX_COMMITMETA_BYTES)
        .map_err(|_| "commitmeta limit does not fit u64".to_string())?;
    let commitmeta = fetch_small(
        &commitmeta_url(&repository, commit)?,
        &work.join("commitmeta"),
        commitmeta_limit,
        deadline,
    )?;
    let parsed = (admission.verify)(exact_ref, commit, &commit_bytes, &commitmeta, now)
        .map_err(|error| format!("{exact_ref} commit {}: {error}", commit.to_hex()))?;
    let commit_hex = commit.to_hex();
    flathub_app::admits_after(
        flathub_app::floor(known.iter().copied(), exact_ref),
        &commit_hex,
        parsed.timestamp,
    )?;
    Ok(Candidate {
        admitted: Admitted {
            exact_ref: exact_ref.to_string(),
            commit: commit_hex,
            timestamp: parsed.timestamp,
        },
        content: parsed.content_checksum(),
        metadata: parsed.metadata.flatpak_metadata,
        commitmeta,
        from_feed,
    })
}

/// The cache lock is held, so an absent destination stays absent.
fn rename_absent(old: &CStr, new: &CStr) -> std::io::Result<()> {
    let old = Path::new(OsStr::from_bytes(old.to_bytes()));
    let new = Path::new(OsStr::from_bytes(new.to_bytes()));
    match fs::symlink_metadata(new) {
        Ok(_) => Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::rename(old, new),
        Err(error) => Err(error),
    }
}

fn deploy_path(cache: &Path, commit: &str) -> PathBuf {
    cache.join(DEPLOY).join(commit)
}

fn deployed(cache: &Path, commit: &str) -> bool {
    fs::symlink_metadata(deploy_path(cache, commit).join(FILES))
        .is_ok_and(|metadata| metadata.is_dir())
}

/// Acquire and materialize one admitted commit unless its deploy exists.
/// Publication is one rename, so an existing `files/` is complete; a deploy
/// without one holds only what an interrupted run left.
fn deploy(
    admission: &Admission<'_>,
    cache: &Path,
    candidate: &Candidate,
) -> Result<PathBuf, String> {
    let hex = &candidate.admitted.commit;
    let out = deploy_path(cache, hex);
    if deployed(cache, hex) {
        return Ok(out);
    }
    if fs::symlink_metadata(&out).is_ok() {
        remove_tree(&out)?;
    }
    let objects = cache.join(OBJECTS).join(hex);
    let spec = AcquireSpec::parse(
        admission.repository,
        &candidate.admitted.exact_ref,
        hex,
        &candidate.content.to_hex(),
    )?;
    match admission.feed {
        Some(base) if candidate.from_feed => {
            crate::ostree::acquire_from_feed(&spec, &objects, base, |_| Ok(()))?;
        }
        _ => {
            crate::ostree::acquire(&spec, &objects)?;
        }
    }
    private_dir(&out)?;
    fs::set_permissions(&out, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod {}: {error}", out.display()))?;
    let stats = td_engine::ostree_deploy::materialize(
        &objects,
        &candidate.admitted.exact_ref,
        hex,
        &candidate.content.to_hex(),
        &out.join(FILES),
        rename_absent,
    )?;
    for warning in stats.warnings {
        eprintln!("td-app: {warning}");
    }
    Ok(out)
}

/// The rule-5 inputs no commit carries, and their digest, recorded in the
/// selection so a change to any of them forces a fresh validation.
fn expect(source: FlathubSource) -> Result<Expected, String> {
    let policy = dynamic_application_policy(source.name, source.policy_runtime)
        .ok_or_else(|| format!("{} has no dynamic-library policy", source.name))?;
    let mut hasher = td_engine::sha256::Sha256::new();
    let mut record = |key: &str, value: &[u8]| {
        hasher.update(key.as_bytes());
        hasher.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value);
    };
    record("version", CHECKS_VERSION.as_bytes());
    record("entry", source.entry.as_bytes());
    // The retired runtime-only form's store package; always empty now, so
    // every selection's digest is what it was.
    record("package", &[]);
    for path in policy.library_paths() {
        record("library", path.as_bytes());
    }
    for target in policy.optional_targets() {
        record("optional", target.as_bytes());
    }
    record("links", &policy.optional_links().to_be_bytes());
    let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
    Ok(Expected {
        checks: td_engine::sha256::to_base16(&hasher.finalize()),
        library_paths: strings(policy.library_paths()),
        optional_targets: strings(policy.optional_targets()),
        optional_links: policy.optional_links(),
    })
}

/// A selection still serves the declaration: its name, refs and recorded
/// checks match, and every deploy it names is complete.
fn serves(cache: &Path, selection: &Selection, source: FlathubSource, expected: &Expected) -> bool {
    selection.name == source.name
        && selection.checks == expected.checks
        && selection.application.exact_ref == source.application
        && selection.runtime.exact_ref == source.runtime
        && selection.commits().all(|commit| deployed(cache, commit))
}

fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn point(cache: &Path, name: &str, generation: u64) -> Result<(), String> {
    let staged = cache.join(format!("{name}.new"));
    let _ = fs::remove_file(&staged);
    std::os::unix::fs::symlink(flathub_app::pointer_target(generation), &staged)
        .map_err(|error| format!("link {}: {error}", staged.display()))?;
    fs::rename(&staged, cache.join(name))
        .map_err(|error| format!("replace {}: {error}", cache.join(name).display()))
}

fn list(directory: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut found = Vec::new();
    for entry in
        fs::read_dir(directory).map_err(|error| format!("read {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
        if found.len() >= MAX_LISTED_ENTRIES {
            return Err(format!("{} has too many entries", directory.display()));
        }
        // A name that is not UTF-8 is nothing td-app wrote; it reads as debris.
        let name = entry.file_name().to_str().unwrap_or("").to_string();
        found.push((name, entry.path()));
    }
    Ok(found)
}

/// Write the generation completely, then make it `current`; the generation
/// it replaces becomes `previous`. A crash between the two renames leaves
/// both naming the old generation, which costs only the older rollback.
fn publish(cache: &Path, selection: &Selection, current: Option<u64>) -> Result<u64, String> {
    let next = list(&cache.join(GENERATIONS))?
        .iter()
        .filter_map(|(name, _)| flathub_app::generation_number(name))
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("generation numbers are exhausted")?;
    let directory = cache.join(GENERATIONS).join(next.to_string());
    private_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod {}: {error}", directory.display()))?;
    let path = directory.join(SELECTION);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o644)
        .open(&path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(selection.render().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    sync_dir(&directory)?;
    for area in [GENERATIONS, DEPLOY, OBJECTS] {
        sync_dir(&cache.join(area))?;
    }
    if let Some(current) = current {
        point(cache, PREVIOUS, current)?;
    }
    point(cache, CURRENT, next)?;
    sync_dir(cache)?;
    Ok(next)
}

/// Remove a tree the account owns, opening any directory a reader sealed.
fn remove_tree(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return fs::remove_file(path)
            .map_err(|error| format!("remove {}: {error}", path.display()));
    }
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("chmod {}: {error}", directory.display()))?;
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
            if entry
                .file_type()
                .map_err(|error| format!("stat {}: {error}", entry.path().display()))?
                .is_dir()
            {
                pending.push(entry.path());
            }
        }
    }
    fs::remove_dir_all(path).map_err(|error| format!("remove {}: {error}", path.display()))
}

enum Hold {
    /// No selection td-jail could have opened.
    Debris,
    /// The exclusive lock proving no td-jail holds it, kept until it is gone.
    Free(File),
    /// A running td-jail holds a shared lock for as long as it runs.
    Held,
}

fn hold(selection: &Path) -> Result<Hold, String> {
    // Only what td-jail could not have opened is debris: absent, a link,
    // under a non-directory, or not a regular file. Any other failure ends
    // collection rather than guess.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(crate::feed::OPEN_REGULAR_NOFOLLOW)
        .open(selection)
    {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || matches!(error.raw_os_error(), Some(ELOOP | ENOTDIR)) =>
        {
            return Ok(Hold::Debris);
        }
        Err(error) => return Err(format!("open {}: {error}", selection.display())),
    };
    if !file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", selection.display()))?
        .is_file()
    {
        return Ok(Hold::Debris);
    }
    match file.try_lock() {
        Ok(()) => Ok(Hold::Free(file)),
        Err(TryLockError::WouldBlock) => Ok(Hold::Held),
        Err(TryLockError::Error(error)) => Err(format!("lock {}: {error}", selection.display())),
    }
}

/// Keep `current`, `previous`, every generation a running td-jail holds,
/// the commits they name and `also`; nothing else in the cache survives. A
/// generation goes only under an exclusive lock on its selection, so a
/// td-jail that locks it afterwards finds it unlinked and asks again.
fn collect(cache: &Path, also: &[String]) -> Result<(), String> {
    let mut keep_generations = Vec::new();
    let mut keep_commits: Vec<String> = also.to_vec();
    for pointer in [CURRENT, PREVIOUS] {
        if let Some((generation, selection)) = read_pointer(cache, pointer)? {
            keep_generations.push(generation.to_string());
            keep_commits.extend(selection.commits().map(str::to_string));
        }
    }
    for (name, path) in list(&cache.join(GENERATIONS))? {
        if keep_generations.contains(&name) {
            continue;
        }
        let selection = path.join(SELECTION);
        match hold(&selection)? {
            Hold::Debris => remove_tree(&path)?,
            Hold::Free(proof) => {
                remove_tree(&path)?;
                drop(proof);
            }
            Hold::Held => {
                // A read that fails ends collection, as an open does; one
                // that does not parse names nothing td-jail could launch.
                if let Some(text) = read_text(&selection)? {
                    if let Ok(selection) = Selection::parse(&text) {
                        keep_commits.extend(selection.commits().map(str::to_string));
                    }
                }
            }
        }
    }
    for area in [DEPLOY, OBJECTS] {
        for (name, path) in list(&cache.join(area))? {
            if !keep_commits.contains(&name) {
                let doomed = if name.starts_with(REMOVING) {
                    // Already cleared if its commit was listed first.
                    if fs::symlink_metadata(&path).is_err() {
                        continue;
                    }
                    path
                } else if !flathub_app::valid_hex64(&name) {
                    // Not a commit, so nothing reads it as complete.
                    path
                } else {
                    let doomed = cache.join(area).join(format!("{REMOVING}{name}"));
                    // An earlier removal of the same commit cut short.
                    if fs::symlink_metadata(&doomed).is_ok() {
                        remove_tree(&doomed)?;
                    }
                    fs::rename(&path, &doomed).map_err(|error| {
                        format!("rename {} for removal: {error}", path.display())
                    })?;
                    doomed
                };
                remove_tree(&doomed)?;
            }
        }
    }
    for (name, path) in list(cache)? {
        if !CACHE_ENTRIES.contains(&name.as_str()) {
            remove_tree(&path)?;
        }
    }
    Ok(())
}

fn update(
    admission: &Admission<'_>,
    cache: &Path,
    source: FlathubSource,
    force: bool,
    now: u64,
) -> Result<Outcome, String> {
    prepare_cache(cache)?;
    let _lock = lock(cache)?;
    let expected = expect(source)?;
    let current = read_pointer(cache, CURRENT)?;
    let previous = read_pointer(cache, PREVIOUS)?;
    if !force {
        if let Some((_, selection)) = &current {
            let checked = read_text(&cache.join(CHECKED))?.and_then(|text| {
                text.strip_suffix('\n')
                    .and_then(|text| text.parse::<u64>().ok())
            });
            if checked.is_some_and(|checked| checked <= now && now - checked < CHECK_INTERVAL_SECS)
                && serves(cache, selection, source, &expected)
            {
                return Ok(Outcome::Throttled);
            }
        }
    }
    // Rows for refs no longer declared are dropped, which bounds the file.
    let mut admitted: Vec<Admitted> = match read_text(&cache.join(ADMITTED))? {
        Some(text) => flathub_app::parse_admitted(&text)?,
        None => Vec::new(),
    };
    admitted.retain(|row| row.exact_ref == source.runtime || row.exact_ref == source.application);
    let known: Vec<&Admitted> = admitted
        .iter()
        .chain(current.iter().flat_map(|(_, selection)| selection.rows()))
        .chain(previous.iter().flat_map(|(_, selection)| selection.rows()))
        .collect();

    let work = cache.join(WORK);
    if fs::symlink_metadata(&work).is_ok() {
        remove_tree(&work)?;
    }
    private_dir(&work)?;
    let deadline = Instant::now()
        .checked_add(RESOLVE_TIME)
        .ok_or("resolve deadline overflows")?;
    let runtime = resolve(admission, &work, source.runtime, &known, now, deadline)?;
    let application = resolve(admission, &work, source.application, &known, now, deadline)?;
    let metadata = application
        .metadata
        .as_deref()
        .ok_or_else(|| format!("{} commit carries no xa.metadata", source.application))?;
    flathub_app::require_runtime(metadata, source.runtime)?;
    remove_tree(&work)?;
    drop(known);

    let selection = Selection {
        name: source.name.to_string(),
        checks: expected.checks.clone(),
        application: application.admitted.clone(),
        runtime: runtime.admitted.clone(),
    };
    let candidates = [&application, &runtime];
    for candidate in &candidates {
        admitted.retain(|row| row.exact_ref != candidate.admitted.exact_ref);
        admitted.push(candidate.admitted.clone());
    }
    let admitted_text = flathub_app::render_admitted(&admitted);

    let outcome = (|| {
        let same = current
            .as_ref()
            .is_some_and(|(_, selected)| *selected == selection);
        if same && serves(cache, &selection, source, &expected) {
            // Heals an `admitted` that an interrupted run left behind.
            write_state(&cache.join(ADMITTED), &admitted_text)?;
            return Ok(Outcome::Unchanged);
        }
        let runtime_out = deploy(admission, cache, &runtime)?;
        let application_out = deploy(admission, cache, &application)?;
        (admission.validate)(
            &application_out,
            source.entry,
            &runtime_out,
            &expected.library_paths,
            &expected.optional_targets,
            expected.optional_links,
        )?;
        // Written before the flip, so the floor is never below `current`.
        write_state(&cache.join(ADMITTED), &admitted_text)?;
        if same {
            // Current's deploys were restored in place; a new generation
            // would only push the real rollback out.
            return Ok(Outcome::Unchanged);
        }
        let generation = publish(
            cache,
            &selection,
            current.as_ref().map(|(number, _)| *number),
        )?;
        Ok(Outcome::Selected(generation))
    })();
    // A refused candidate keeps its deploy for the next attempt, and only it.
    let attempted: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.admitted.commit.clone())
        .collect();
    let collected = collect(cache, &attempted);
    let outcome = match (outcome, collected) {
        (Ok(outcome), Ok(())) => outcome,
        (Ok(_), Err(error)) => return Err(error),
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(collection)) => {
            return Err(format!(
                "{error}; collecting the cache also failed: {collection}"
            ))
        }
    };
    write_state(&cache.join(CHECKED), &format!("{now}\n"))?;
    Ok(outcome)
}

/// The generation td-jail may launch: `current`, while it still serves the
/// declaration. It takes no lock, so a launch never waits behind a fetch;
/// collection never removes what `current` names.
fn ready(cache: &Path, source: FlathubSource) -> Result<Option<u64>, String> {
    let expected = expect(source)?;
    Ok(match read_pointer(cache, CURRENT)? {
        Some((generation, selection)) if serves(cache, &selection, source, &expected) => {
            Some(generation)
        }
        _ => None,
    })
}

/// The cache must belong to the account td-app runs as: as root it would
/// leave entries the account cannot remove and follow links the account
/// planted.
fn require_account(home: &Path) -> Result<(), String> {
    let uid = fs::metadata("/proc/self")
        .map_err(|error| format!("stat /proc/self: {error}"))?
        .uid();
    if uid == 0 {
        return Err("td-app runs as an application account, not root".into());
    }
    let owner = fs::metadata(home)
        .map_err(|error| format!("stat {}: {error}", home.display()))?
        .uid();
    if owner != uid {
        return Err(format!(
            "HOME {} belongs to uid {owner}, not this account's {uid}",
            home.display()
        ));
    }
    Ok(())
}

/// The host half of the feed transport: resolve every declared ref
/// against Flathub under rules 1–2, fetch its graph into `cache`, and
/// publish the graph, its commitmeta and last the ref into a td-feed
/// store, so a ref the feed names always has its graph. Graphs no
/// declared ref names any more leave the cache.
fn export(
    admission: &Admission<'_>,
    refs: &[&str],
    cache: &Path,
    store: &Path,
    now: u64,
) -> Result<Vec<String>, String> {
    prepare_cache(cache)?;
    let _lock = lock(cache)?;
    let work = cache.join(WORK);
    if fs::symlink_metadata(&work).is_ok() {
        remove_tree(&work)?;
    }
    private_dir(&work)?;
    let deadline = Instant::now()
        .checked_add(RESOLVE_TIME)
        .ok_or("resolve deadline overflows")?;
    // The graphs this export may remove later: those it recorded before
    // and those it creates now. A graph already in the store, such as one
    // another export placed, is never its own.
    let exported_path = cache.join(EXPORTED);
    // Only a record that is not a file starts again; any other failure to
    // read it is reported, since starting again abandons what it owned.
    if fs::symlink_metadata(&exported_path).is_ok_and(|metadata| !metadata.is_file()) {
        eprintln!(
            "td-feed: {} is not a file; the export's record starts again",
            exported_path.display()
        );
        remove_tree(&exported_path)?;
    }
    let mut owned: BTreeSet<String> = read_text(&exported_path)?
        .unwrap_or_default()
        .lines()
        .filter(|commit| flathub_app::valid_hex64(commit))
        .map(str::to_string)
        .collect();
    let store_graph = |commit: Checksum| {
        store.join(crate::ostree::feed_graph_base("", commit).trim_start_matches('/'))
    };
    let mut resolved = Vec::with_capacity(refs.len());
    for &exact_ref in refs {
        let candidate = resolve(admission, &work, exact_ref, &[], now, deadline)?;
        let commit = Checksum::from_hex(&candidate.admitted.commit)?;
        let spec = AcquireSpec::parse(
            admission.repository,
            exact_ref,
            &candidate.admitted.commit,
            &candidate.content.to_hex(),
        )?;
        let objects = cache.join(OBJECTS).join(&candidate.admitted.commit);
        crate::ostree::acquire(&spec, &objects)?;
        // Recorded before any of it is published, so no failure leaks it.
        if fs::symlink_metadata(store_graph(commit)).is_err()
            && owned.insert(candidate.admitted.commit.clone())
        {
            write_state(&exported_path, &render_commits(&owned))?;
        }
        crate::ostree::export_feed(&spec, &objects, store, |_| Ok(()))?;
        let graph = crate::ostree::feed_graph_base("", commit);
        let graph = graph.trim_start_matches('/');
        publish_to_feed(
            &work,
            &candidate.commitmeta,
            &store.join(commitmeta_url(graph, commit)?),
        )?;
        resolved.push((exact_ref, candidate.admitted.commit));
    }
    // The refs move only once every graph is in place, so the feed never
    // names a new application beside a runtime graph it still lacks.
    for (exact_ref, commit) in &resolved {
        publish_to_feed(
            &work,
            format!("{commit}\n").as_bytes(),
            &store.join(FEED_REFS).join(exact_ref),
        )?;
    }
    remove_tree(&work)?;
    // An owned graph leaves the store once no ref file there names it, a
    // ref no longer declared included, so the feed never names a graph it
    // lacks.
    let named = feed_ref_commits(&store.join(FEED_REFS))?;
    let mut removed = Vec::new();
    for commit in owned.iter().filter(|commit| !named.contains(*commit)) {
        let graph = store_graph(Checksum::from_hex(commit)?);
        if fs::symlink_metadata(&graph).is_ok() {
            remove_tree(&graph)?;
        }
        removed.push(commit.clone());
    }
    for commit in removed {
        owned.remove(&commit);
    }
    write_state(&exported_path, &render_commits(&owned))?;
    // The host cache keeps the graphs exported now and their ostree
    // siblings, which the client names by its hash of the graph's name.
    let kept: BTreeSet<&str> = resolved.iter().map(|(_, commit)| commit.as_str()).collect();
    let siblings: BTreeSet<String> = kept
        .iter()
        .map(|commit| crate::feed::hex_sha256(commit.as_bytes()))
        .collect();
    for (name, path) in list(&cache.join(OBJECTS))? {
        let stale = if flathub_app::valid_hex64(&name) {
            !kept.contains(name.as_str())
        } else {
            name.strip_prefix('.')
                .and_then(|rest| {
                    rest.strip_suffix(".td-ostree.lock")
                        .or_else(|| rest.strip_suffix(".td-ostree-transaction"))
                })
                .is_some_and(|identity| {
                    flathub_app::valid_hex64(identity) && !siblings.contains(identity)
                })
        };
        if stale {
            remove_tree(&path)?;
        }
    }
    Ok(resolved
        .iter()
        .map(|(exact_ref, commit)| format!("{exact_ref} {commit}"))
        .collect())
}

fn render_commits(commits: &BTreeSet<String>) -> String {
    commits.iter().map(|commit| format!("{commit}\n")).collect()
}

/// Every commit a ref file under `refs` names; refs are four segments deep.
fn feed_ref_commits(refs: &Path) -> Result<BTreeSet<String>, String> {
    let mut named = BTreeSet::new();
    if fs::symlink_metadata(refs).is_err() {
        return Ok(named);
    }
    let mut pending = vec![(refs.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        for (name, path) in list(&directory)? {
            if name.starts_with('.') || name.ends_with(".sha256") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("stat {}: {error}", path.display()))?;
            if metadata.is_dir() && depth < 3 {
                pending.push((path, depth + 1));
            } else if metadata.is_file() && depth == 3 {
                if let Some(commit) = read_text(&path)?
                    .as_deref()
                    .and_then(|text| text.strip_suffix('\n'))
                    .filter(|commit| flathub_app::valid_hex64(commit))
                {
                    named.insert(commit.to_string());
                }
            }
        }
    }
    Ok(named)
}

/// Publish `bytes` at `destination` through td-feed's checked writer.
fn publish_to_feed(work: &Path, bytes: &[u8], destination: &Path) -> Result<(), String> {
    let staged = work.join("publish");
    fs::write(&staged, bytes).map_err(|error| format!("write {}: {error}", staged.display()))?;
    let published =
        crate::feed::export_verified_file(&staged, destination, &crate::feed::hex_sha256(bytes));
    let _ = fs::remove_file(&staged);
    published
}

/// `td-feed export flathub`: the host account's own cache, the feed's store.
pub(crate) fn export_to(store: &Path) -> Result<Vec<String>, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let admission = Admission {
        repository: flathub_app::REPOSITORY,
        feed: None,
        verify: verify_flathub,
        validate: validate_dynamic,
    };
    let mut refs: Vec<&str> = flathub_app::FLATHUB_SOURCES
        .iter()
        .flat_map(|source| [source.application, source.runtime])
        .collect();
    refs.sort_unstable();
    refs.dedup();
    export(
        &admission,
        &refs,
        &Path::new(&home).join(".td/flathub-export"),
        store,
        unix_now()?,
    )
}

fn usage() -> ! {
    eprintln!("usage: td-app update NAME [--force] | td-app ready NAME");
    std::process::exit(2);
}

enum Verb {
    Update { force: bool },
    Ready,
}

pub fn run(args: &[String]) {
    let (name, verb) = match args.get(1..) {
        Some([verb, name]) if verb == "update" => (name, Verb::Update { force: false }),
        Some([verb, name, flag]) if verb == "update" && flag == "--force" => {
            (name, Verb::Update { force: true })
        }
        Some([verb, name]) if verb == "ready" => (name, Verb::Ready),
        _ => usage(),
    };
    let Some(source) = flathub_app::flathub_source(name) else {
        eprintln!("td-app: {name} is not a Flathub application");
        std::process::exit(2);
    };
    let result = (|| {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let home = Path::new(&home);
        if !home.is_absolute() {
            return Err(format!("HOME {} is not absolute", home.display()));
        }
        require_account(home)?;
        // Only an update fetches; `ready` answers td-jail without it.
        let feed = match verb {
            Verb::Update { .. } => crate::feed::provisioned_feed_base()?,
            Verb::Ready => None,
        };
        let admission = Admission {
            repository: flathub_app::REPOSITORY,
            feed: feed.as_deref(),
            verify: verify_flathub,
            validate: validate_dynamic,
        };
        let cache = home.join(CACHE_DIR);
        match verb {
            Verb::Update { force } => update(&admission, &cache, source, force, unix_now()?),
            Verb::Ready => ready(&cache, source).map(Outcome::Ready),
        }
    })();
    match result {
        // td-jail's answer: one line, a generation or `absent`.
        Ok(Outcome::Ready(Some(generation))) => println!("{generation}"),
        Ok(Outcome::Ready(None)) => println!("absent"),
        Ok(Outcome::Throttled) => println!("td-app: {name} was checked within a day"),
        Ok(Outcome::Unchanged) => println!("td-app: {name} is current"),
        Ok(Outcome::Selected(generation)) => {
            println!("td-app: {name} selected generation {generation}")
        }
        Err(error) => {
            eprintln!("td-app: {name}: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::*;
    use crate::ostree::tests::{archive_regular, synthetic_graph_at};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const APP: &str = "app/org.example.App/x86_64/stable";
    const RUNTIME: &str = "runtime/org.example.Platform/x86_64/1";
    const APP_METADATA: &str =
        "[Application]\nname=org.example.App\nruntime=org.example.Platform/x86_64/1\n";
    const NOW: u64 = 1_800_000_000;

    type Repository = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

    thread_local! {
        static VALIDATED: RefCell<Vec<(PathBuf, PathBuf)>> = const { RefCell::new(Vec::new()) };
        static REFUSE: RefCell<bool> = const { RefCell::new(false) };
    }

    fn source() -> FlathubSource {
        FlathubSource {
            name: "firefox",
            application: APP,
            runtime: RUNTIME,
            entry: "/app/bin/firefox",
            policy_runtime: "freedesktop-platform-25-08",
        }
    }

    /// Stands in for Flathub's key: the commit must parse under its
    /// checksum and ref, and its commitmeta must be the word `signed`.
    fn verify_fixture(
        exact_ref: &str,
        checksum: Checksum,
        commit: &[u8],
        commitmeta: &[u8],
        _now: u64,
    ) -> Result<Commit, String> {
        let parsed = ostree::parse_commit_verified(checksum, commit)?;
        parsed.metadata.require_exact_ref(exact_ref)?;
        if commitmeta != b"signed" {
            return Err("fixture signature does not verify".into());
        }
        Ok(parsed)
    }

    fn validate_fixture(
        out: &Path,
        _entry: &str,
        runtime: &Path,
        _library_paths: &[String],
        _optional_targets: &[String],
        _optional_links: usize,
    ) -> Result<(), String> {
        if REFUSE.with(|refuse| *refuse.borrow()) {
            return Err("fixture validator refuses".into());
        }
        VALIDATED.with(|seen| {
            seen.borrow_mut()
                .push((out.to_path_buf(), runtime.to_path_buf()))
        });
        Ok(())
    }

    fn serve(repository: Repository) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut connection) = connection else {
                    return;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while request.len() < 8_192 && !request.ends_with(b"\r\n\r\n") {
                    match connection.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let body = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|path| path.strip_prefix("/repo"))
                    .and_then(|path| repository.lock().unwrap().get(path).cloned());
                let _ = match body {
                    Some(body) => connection
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .and_then(|()| connection.write_all(&body)),
                    None => connection.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ),
                };
            }
        });
        format!("http://{address}/repo")
    }

    /// Publish one commit of `exact_ref` with `signature` as its
    /// commitmeta, and point the ref at it.
    fn publish_commit(
        repository: &Repository,
        exact_ref: &str,
        contents: &[u8],
        timestamp: u64,
        metadata: Option<&str>,
        signature: &[u8],
    ) -> String {
        let (checksum, bytes) = archive_regular(contents);
        let (spec, objects) = synthetic_graph_at(
            exact_ref,
            vec![("payload".into(), checksum, bytes)],
            timestamp,
            metadata,
        );
        let commit = spec.commit_hex();
        let mut map = repository.lock().unwrap();
        map.extend(objects);
        map.insert(
            format!("/objects/{}/{}.commitmeta", &commit[..2], &commit[2..]),
            signature.to_vec(),
        );
        map.insert(
            format!("/refs/heads/{exact_ref}"),
            format!("{commit}\n").into_bytes(),
        );
        commit
    }

    struct Fixture {
        _root: crate::test_temp::TempDir,
        home: PathBuf,
        repository: Repository,
        url: String,
        feed: Option<String>,
    }

    impl Fixture {
        fn new(label: &str) -> Fixture {
            let root = crate::test_temp::TempDir::new(&format!("td-app-{label}")).unwrap();
            let home = root.join("home");
            fs::create_dir(&home).unwrap();
            let repository = Repository::default();
            let url = serve(repository.clone());
            Fixture {
                _root: root,
                home,
                repository,
                url,
                feed: None,
            }
        }

        fn cache(&self) -> PathBuf {
            self.home.join(CACHE_DIR)
        }

        fn update(&self, source: FlathubSource, force: bool, now: u64) -> Result<Outcome, String> {
            let admission = Admission {
                repository: &self.url,
                feed: self.feed.as_deref(),
                verify: verify_fixture,
                validate: validate_fixture,
            };
            update(&admission, &self.cache(), source, force, now)
        }

        fn current(&self) -> Option<(u64, Selection)> {
            read_pointer(&self.cache(), CURRENT).unwrap()
        }

        fn names(&self, area: &str) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.cache().join(area))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    #[test]
    fn first_update_admits_both_refs_and_publishes_generation_one() {
        let fixture = Fixture::new("first");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );

        VALIDATED.with(|seen| seen.borrow_mut().clear());
        assert_eq!(
            fixture.update(source(), false, NOW),
            Ok(Outcome::Selected(1))
        );

        let (generation, selection) = fixture.current().unwrap();
        assert_eq!(generation, 1);
        assert_eq!(selection.runtime.commit, runtime);
        assert_eq!(selection.runtime.timestamp, 10);
        assert_eq!(selection.application.commit, app);
        assert!(read_pointer(&fixture.cache(), PREVIOUS).unwrap().is_none());
        let deploy = fixture.cache().join(DEPLOY);
        assert_eq!(
            fs::read(deploy.join(&app).join("files/payload")).unwrap(),
            b"app"
        );
        assert_eq!(
            fs::read(deploy.join(&runtime).join("files/payload")).unwrap(),
            b"usr"
        );
        VALIDATED.with(|seen| {
            assert_eq!(
                *seen.borrow(),
                vec![(deploy.join(&app), deploy.join(&runtime))]
            );
        });
        let admitted = fs::read_to_string(fixture.cache().join(ADMITTED)).unwrap();
        assert_eq!(flathub_app::parse_admitted(&admitted).unwrap().len(), 2);
        assert_eq!(
            fs::read_to_string(fixture.cache().join(CHECKED)).unwrap(),
            format!("{NOW}\n")
        );
        assert!(!fixture.cache().join(WORK).exists());
        let mode = fs::metadata(fixture.cache()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    impl Fixture {
        /// Export both refs from this fixture's Flathub into a fresh feed
        /// store, as `td-feed export flathub` does on the host.
        fn export(&self, host: &Path) -> Result<Vec<String>, String> {
            let admission = Admission {
                repository: &self.url,
                feed: None,
                verify: verify_fixture,
                validate: validate_fixture,
            };
            export(
                &admission,
                &[APP, RUNTIME],
                &host.join("cache"),
                &host.join("store"),
                NOW,
            )
        }
    }

    /// The feed is transport only: an update takes each ref, its commit,
    /// commitmeta and graph from an exported feed and never asks Flathub,
    /// and still verifies them as it would Flathub's.
    #[test]
    fn an_exported_feed_serves_an_update_without_flathub() {
        let mut fixture = Fixture::new("feed");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        let host = crate::test_temp::TempDir::new("td-app-feed-host").unwrap();
        assert_eq!(
            fixture.export(&host).unwrap(),
            vec![format!("{APP} {app}"), format!("{RUNTIME} {runtime}")]
        );
        let store = host.join("store");
        assert_eq!(
            fs::read_to_string(store.join(FEED_REFS).join(RUNTIME)).unwrap(),
            format!("{runtime}\n")
        );
        fixture.repository.lock().unwrap().clear();
        let feed = crate::feed::tests::ConsumerServer::start(store.clone(), None);
        fixture.feed = Some(feed.base.clone());
        assert_eq!(
            fixture.update(source(), false, NOW),
            Ok(Outcome::Selected(1))
        );
        let (_, selection) = fixture.current().unwrap();
        assert_eq!(selection.runtime.commit, runtime);
        assert_eq!(selection.application.commit, app);
        assert_eq!(
            fs::read(
                fixture
                    .cache()
                    .join(DEPLOY)
                    .join(&app)
                    .join("files/payload")
            )
            .unwrap(),
            b"app"
        );

        // A feed whose signature does not verify is refused like Flathub.
        let forged = Fixture::new("feed-forged");
        publish_commit(&forged.repository, RUNTIME, b"usr", 10, None, b"forged");
        publish_commit(
            &forged.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"forged",
        );
        let host = crate::test_temp::TempDir::new("td-app-feed-forged").unwrap();
        assert!(forged.export(&host).is_err());
    }

    /// Replace one file of a feed store, sidecar included, as a hostile or
    /// broken host could.
    fn rewrite_feed_file(path: &Path, bytes: Option<&[u8]>) {
        let sidecar = PathBuf::from(format!("{}.sha256", path.display()));
        match bytes {
            Some(bytes) => {
                fs::write(path, bytes).unwrap();
                fs::write(&sidecar, format!("{}\n", crate::feed::hex_sha256(bytes))).unwrap();
            }
            None => {
                fs::remove_file(path).unwrap();
                fs::remove_file(&sidecar).unwrap();
            }
        }
    }

    /// A ref the feed answers is the feed's alone, and still held to rules
    /// 1–3: a forged signature, a commit below the floor and a graph the
    /// feed cannot complete each refuse the update, with no fallback to the
    /// Flathub that would have answered.
    #[test]
    fn a_feed_answer_is_held_to_the_rules_without_fallback() {
        let mut fixture = Fixture::new("feed-rules");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        let host = crate::test_temp::TempDir::new("td-app-feed-rules").unwrap();
        fixture.export(&host).unwrap();
        let store = host.join("store");
        let commit = Checksum::from_hex(&runtime).unwrap();
        let graph = store.join(crate::ostree::feed_graph_base("", commit).trim_start_matches('/'));
        let commitmeta = graph.join(commitmeta_url("", commit).unwrap().trim_start_matches('/'));
        let feed = crate::feed::tests::ConsumerServer::start(store.clone(), None);
        fixture.feed = Some(feed.base.clone());

        rewrite_feed_file(&commitmeta, Some(b"forged"));
        let error = fixture.update(source(), false, NOW).unwrap_err();
        assert!(error.contains("does not verify"), "{error}");
        assert!(fixture.current().is_none());
        rewrite_feed_file(&commitmeta, Some(b"signed"));

        // A graph object the feed lost.
        let object = fs::read_dir(graph.join("objects"))
            .unwrap()
            .flat_map(|fanout| fs::read_dir(fanout.unwrap().path()).unwrap())
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|kind| kind == "filez"))
            .unwrap();
        let lost = fs::read(&object).unwrap();
        rewrite_feed_file(&object, None);
        assert!(fixture.update(source(), false, NOW).is_err());
        assert!(fixture.current().is_none());
        rewrite_feed_file(&object, Some(&lost));
        assert_eq!(
            fixture.update(source(), false, NOW),
            Ok(Outcome::Selected(1))
        );

        // An older signed commit than the account admitted.
        let older = Fixture::new("feed-older");
        publish_commit(&older.repository, RUNTIME, b"old", 5, None, b"signed");
        publish_commit(
            &older.repository,
            APP,
            b"old",
            6,
            Some(APP_METADATA),
            b"signed",
        );
        let stale = crate::test_temp::TempDir::new("td-app-feed-stale").unwrap();
        older.export(&stale).unwrap();
        let stale_feed = crate::feed::tests::ConsumerServer::start(stale.join("store"), None);
        fixture.feed = Some(stale_feed.base.clone());
        let error = fixture.update(source(), true, NOW).unwrap_err();
        assert!(error.contains("does not supersede"), "{error}");
        assert_eq!(fixture.current().unwrap().0, 1);
    }

    /// A feed that lacks a ref is no answer: td-app asks Flathub.
    #[test]
    fn a_feed_without_the_ref_falls_back_to_flathub() {
        let mut fixture = Fixture::new("feed-empty");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        let empty = crate::test_temp::TempDir::new("td-app-feed-empty").unwrap();
        let store = empty.join("store");
        fs::create_dir(&store).unwrap();
        let feed = crate::feed::tests::ConsumerServer::start(store, None);
        fixture.feed = Some(feed.base.clone());
        assert_eq!(
            fixture.update(source(), false, NOW),
            Ok(Outcome::Selected(1))
        );
        // It was asked for both refs first.
        assert_eq!(feed.count(), 2);
    }

    /// The host cache keeps only the graphs the declared refs name now.
    #[test]
    fn export_keeps_only_the_graphs_it_exported() {
        let fixture = Fixture::new("feed-prune");
        let old = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        let host = crate::test_temp::TempDir::new("td-app-feed-prune").unwrap();
        fixture.export(&host).unwrap();
        let new = publish_commit(&fixture.repository, RUNTIME, b"usr2", 30, None, b"signed");
        fixture.export(&host).unwrap();
        let names: Vec<String> = fs::read_dir(host.join("cache").join(OBJECTS))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let kept: Vec<String> = names
            .iter()
            .filter(|name| !name.starts_with('.'))
            .cloned()
            .collect();
        assert_eq!(sorted(kept), sorted(vec![app.clone(), new.clone()]));
        let lock = |commit: &str| {
            format!(
                ".{}.td-ostree.lock",
                crate::feed::hex_sha256(commit.as_bytes())
            )
        };
        assert!(
            names.contains(&lock(&new)) && names.contains(&lock(&app)),
            "{names:?}"
        );
        assert!(!names.contains(&lock(&old)), "{names:?}");
        assert_eq!(
            fs::read_to_string(host.join("store").join(FEED_REFS).join(RUNTIME)).unwrap(),
            format!("{new}\n")
        );
        // The store loses the graph the moved ref named, and nothing else.
        let graph = |commit: &str| {
            let base = crate::ostree::feed_graph_base("", Checksum::from_hex(commit).unwrap());
            host.join("store").join(base.trim_start_matches('/'))
        };
        assert!(!graph(&old).exists());
        assert!(graph(&new).exists() && graph(&app).exists());
    }

    /// The store keeps a graph the export did not create, such as one
    /// another export placed, and a graph any ref file there still names,
    /// such as a ref no longer declared.
    #[test]
    fn export_removes_only_its_own_graphs_no_ref_names() {
        let fixture = Fixture::new("feed-owned");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        let host = crate::test_temp::TempDir::new("td-app-feed-owned").unwrap();
        let store = host.join("store");
        let graph = |commit: &str| {
            let base = crate::ostree::feed_graph_base("", Checksum::from_hex(commit).unwrap());
            store.join(base.trim_start_matches('/'))
        };
        // Another export placed the runtime's graph first.
        fs::create_dir_all(graph(&runtime)).unwrap();
        fixture.export(&host).unwrap();
        // A ref the declaration has dropped still names the application.
        let dropped = store
            .join(FEED_REFS)
            .join("app/org.example.Old/x86_64/stable");
        fs::create_dir_all(dropped.parent().unwrap()).unwrap();
        fs::write(&dropped, format!("{app}\n")).unwrap();
        publish_commit(&fixture.repository, RUNTIME, b"usr2", 30, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app2",
            40,
            Some(APP_METADATA),
            b"signed",
        );
        fixture.export(&host).unwrap();
        assert!(graph(&runtime).exists(), "not this export's to remove");
        assert!(graph(&app).exists(), "a ref still names it");
        fs::remove_file(&dropped).unwrap();
        fixture.export(&host).unwrap();
        assert!(!graph(&app).exists());
        assert!(graph(&runtime).exists());

        // An export that fails part way has recorded the graph it published.
        let admission = Admission {
            repository: &fixture.url,
            feed: None,
            verify: verify_fixture,
            validate: validate_fixture,
        };
        let moved = publish_commit(
            &fixture.repository,
            APP,
            b"app3",
            50,
            Some(APP_METADATA),
            b"signed",
        );
        let partial = export(
            &admission,
            &[APP, "runtime/org.example.Absent/x86_64/1"],
            &host.join("cache"),
            &store,
            NOW,
        );
        assert!(partial.is_err());
        assert!(graph(&moved).exists());
        let record = host.join("cache").join(EXPORTED);
        assert!(fs::read_to_string(&record).unwrap().contains(&moved));

        // A damaged record starts again instead of failing every export.
        fs::remove_file(&record).unwrap();
        fs::create_dir(&record).unwrap();
        fixture.export(&host).unwrap();
        assert!(record.is_file());
    }

    #[test]
    fn a_daily_check_is_throttled_and_force_rechecks_without_a_new_generation() {
        let fixture = Fixture::new("throttle");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        fixture.update(source(), false, NOW).unwrap();

        // An empty repository shows that the throttled check asks nothing.
        fixture.repository.lock().unwrap().clear();
        assert_eq!(
            fixture.update(source(), false, NOW + CHECK_INTERVAL_SECS - 1),
            Ok(Outcome::Throttled)
        );
        assert!(fixture
            .update(source(), false, NOW + CHECK_INTERVAL_SECS)
            .is_err());
        assert!(fixture.update(source(), true, NOW + 1).is_err());
        assert_eq!(fixture.current().unwrap().0, 1);

        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(
            fixture.update(source(), true, NOW + 2),
            Ok(Outcome::Unchanged)
        );
        assert_eq!(
            fs::read_to_string(fixture.cache().join(CHECKED)).unwrap(),
            format!("{}\n", NOW + 2)
        );
        // A clock that went backwards does not suppress the check.
        assert_eq!(fixture.update(source(), false, NOW), Ok(Outcome::Unchanged));
    }

    #[test]
    fn updates_flip_current_keep_previous_and_collect_the_rest() {
        let fixture = Fixture::new("generations");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let first = publish_commit(
            &fixture.repository,
            APP,
            b"one",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        fixture.update(source(), true, NOW).unwrap();
        let second = publish_commit(
            &fixture.repository,
            APP,
            b"two",
            30,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(
            fixture.update(source(), true, NOW),
            Ok(Outcome::Selected(2))
        );
        assert_eq!(
            read_pointer(&fixture.cache(), PREVIOUS).unwrap().unwrap().0,
            1
        );
        assert_eq!(
            fixture.names(DEPLOY),
            sorted(vec![first.clone(), second.clone(), runtime.clone()])
        );

        let third = publish_commit(
            &fixture.repository,
            APP,
            b"three",
            40,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(
            fixture.update(source(), true, NOW),
            Ok(Outcome::Selected(3))
        );
        assert_eq!(fixture.names(GENERATIONS), vec!["2", "3"]);
        let kept = sorted(vec![second, third, runtime]);
        assert_eq!(fixture.names(DEPLOY), kept);
        assert_eq!(fixture.names(OBJECTS), kept);
    }

    #[test]
    fn replayed_unsigned_and_mismatched_candidates_leave_current_in_place() {
        let fixture = Fixture::new("refusals");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let admitted = publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        fixture.update(source(), true, NOW).unwrap();

        let refused = |expected: &str| {
            let error = fixture.update(source(), true, NOW).unwrap_err();
            assert!(error.contains(expected), "{error}");
            let (generation, selection) = fixture.current().unwrap();
            assert_eq!(generation, 1);
            assert_eq!(selection.application.commit, admitted);
        };
        let candidate = |contents: &[u8], timestamp, metadata, signature: &[u8]| {
            publish_commit(
                &fixture.repository,
                APP,
                contents,
                timestamp,
                metadata,
                signature,
            )
        };
        // Rule 3: older, and the same time with another commit.
        candidate(b"older", 19, Some(APP_METADATA), b"signed");
        refused("does not supersede");
        candidate(b"same-time", 20, Some(APP_METADATA), b"signed");
        refused("does not supersede");
        // Rules 1 and 2.
        candidate(b"forged", 30, Some(APP_METADATA), b"forged");
        refused("does not verify");
        // Rule 4.
        candidate(
            b"other",
            30,
            Some("[Application]\nruntime=org.example.Platform/x86_64/2\n"),
            b"signed",
        );
        refused("only \"org.example.Platform/x86_64/1\" is accepted");
        candidate(b"bare", 30, None, b"signed");
        refused("carries no xa.metadata");
        // Rule 5.
        candidate(b"invalid", 30, Some(APP_METADATA), b"signed");
        REFUSE.with(|refuse| *refuse.borrow_mut() = true);
        refused("fixture validator refuses");
        REFUSE.with(|refuse| *refuse.borrow_mut() = false);
        // The same candidate is admitted once it validates.
        assert_eq!(
            fixture.update(source(), true, NOW),
            Ok(Outcome::Selected(2))
        );
    }

    #[test]
    fn pointers_name_only_a_generation() {
        let fixture = Fixture::new("pointers");
        prepare_cache(&fixture.cache()).unwrap();
        let link = fixture.cache().join(CURRENT);
        for target in [
            "../x",
            "generations",
            "generations/01",
            "elsewhere/1",
            "/generations/1",
        ] {
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(target, &link).unwrap();
            assert!(read_pointer(&fixture.cache(), CURRENT).is_err(), "{target}");
        }
    }

    fn app_commit(fixture: &Fixture, contents: &[u8], timestamp: u64) -> String {
        publish_commit(
            &fixture.repository,
            APP,
            contents,
            timestamp,
            Some(APP_METADATA),
            b"signed",
        )
    }

    #[test]
    fn rules_one_to_four_refuse_before_any_tree_is_fetched() {
        let fixture = Fixture::new("before-trees");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), true, NOW).unwrap();
        let objects = fixture.names(OBJECTS);
        let deploys = fixture.names(DEPLOY);
        let refuse = |expected: &str| {
            let error = fixture.update(source(), true, NOW).unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert_eq!(fixture.names(OBJECTS), objects);
            assert_eq!(fixture.names(DEPLOY), deploys);
            assert_eq!(fixture.current().unwrap().0, 1);
        };
        // Rule 2: the app ref names a validly signed commit of another ref.
        fixture.repository.lock().unwrap().insert(
            format!("/refs/heads/{APP}"),
            format!("{runtime}\n").into_bytes(),
        );
        refuse("does not match exact ref");
        // Rule 3 on the runtime ref.
        app_commit(&fixture, b"app", 20);
        publish_commit(
            &fixture.repository,
            RUNTIME,
            b"older-usr",
            9,
            None,
            b"signed",
        );
        refuse("does not supersede");
        // Rule 1 on the runtime ref.
        publish_commit(
            &fixture.repository,
            RUNTIME,
            b"new-usr",
            11,
            None,
            b"unsigned",
        );
        refuse("does not verify");
    }

    #[test]
    fn a_lost_admitted_file_never_lowers_the_floor_below_the_selection() {
        let fixture = Fixture::new("floor");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"one", 20);
        fixture.update(source(), true, NOW).unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(), true, NOW).unwrap();
        fs::remove_file(fixture.cache().join(ADMITTED)).unwrap();
        // Generation 1's commit, still validly signed, served again.
        app_commit(&fixture, b"one", 20);
        let error = fixture.update(source(), true, NOW).unwrap_err();
        assert!(error.contains("does not supersede"), "{error}");
        // An unchanged check rewrites the floor it lost.
        app_commit(&fixture, b"two", 30);
        assert_eq!(fixture.update(source(), true, NOW), Ok(Outcome::Unchanged));
        let admitted = fs::read_to_string(fixture.cache().join(ADMITTED)).unwrap();
        assert_eq!(flathub_app::parse_admitted(&admitted).unwrap().len(), 2);
    }

    #[test]
    fn a_changed_declaration_or_a_missing_deploy_is_never_throttled() {
        let fixture = Fixture::new("declaration");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), false, NOW).unwrap();

        // Another entry is another rule-5 input: the same commits are
        // validated again and selected as a new generation.
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source()
        };
        assert_eq!(
            fixture.update(moved, false, NOW + 1),
            Ok(Outcome::Selected(2))
        );
        let previous = read_pointer(&fixture.cache(), PREVIOUS).unwrap().unwrap();
        assert_ne!(fixture.current().unwrap().1.checks, previous.1.checks);
        assert_eq!(
            fixture.update(moved, false, NOW + 2),
            Ok(Outcome::Throttled)
        );

        // A deploy current names has gone: the check runs and restores it
        // in place, keeping generation 1 as the rollback.
        remove_tree(&fixture.cache().join(DEPLOY).join(&runtime)).unwrap();
        assert_eq!(
            fixture.update(moved, false, NOW + 3),
            Ok(Outcome::Unchanged)
        );
        assert!(deployed(&fixture.cache(), &runtime));
        assert_eq!(fixture.current().unwrap().0, 2);
        assert_eq!(
            read_pointer(&fixture.cache(), PREVIOUS).unwrap().unwrap().0,
            1
        );
    }

    #[test]
    fn an_interrupted_run_leaves_nothing_the_next_one_trips_on() {
        let fixture = Fixture::new("debris");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = app_commit(&fixture, b"app", 20);
        let cache = fixture.cache();
        prepare_cache(&cache).unwrap();
        fs::create_dir_all(cache.join(WORK).join("ref")).unwrap();
        fs::create_dir_all(cache.join(GENERATIONS).join("9")).unwrap();
        let partial = cache
            .join(DEPLOY)
            .join(&app)
            .join(".td-ostree-materialize-1-0");
        fs::create_dir_all(&partial).unwrap();
        fs::write(partial.join("half"), b"x").unwrap();
        fs::write(cache.join(".admitted.1.tmp"), b"x").unwrap();
        std::os::unix::fs::symlink("generations/9", cache.join("current.new")).unwrap();

        assert_eq!(
            fixture.update(source(), false, NOW),
            Ok(Outcome::Selected(10))
        );
        assert!(!partial.exists());
        assert_eq!(
            fs::read(cache.join(DEPLOY).join(&app).join("files/payload")).unwrap(),
            b"app"
        );
        assert_eq!(fixture.names(GENERATIONS), vec!["10"]);
        let mut root: Vec<String> = list(&cache)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        root.sort();
        assert_eq!(
            root,
            vec![
                ADMITTED,
                CHECKED,
                CURRENT,
                DEPLOY,
                GENERATIONS,
                LOCK,
                OBJECTS
            ]
        );
    }

    #[test]
    fn refused_candidates_keep_only_the_latest_attempt() {
        let fixture = Fixture::new("refused-collection");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let admitted = app_commit(&fixture, b"app", 20);
        fixture.update(source(), true, NOW).unwrap();
        REFUSE.with(|refuse| *refuse.borrow_mut() = true);
        let first = app_commit(&fixture, b"first", 30);
        assert!(fixture.update(source(), true, NOW + 1).is_err());
        assert!(deployed(&fixture.cache(), &first));
        let second = app_commit(&fixture, b"second", 40);
        assert!(fixture.update(source(), true, NOW + 2).is_err());
        REFUSE.with(|refuse| *refuse.borrow_mut() = false);
        assert!(!deployed(&fixture.cache(), &first));
        assert!(!fixture.names(OBJECTS).contains(&first));
        assert!(deployed(&fixture.cache(), &second));
        assert!(deployed(&fixture.cache(), &admitted));
        // A refusal is not a completed check.
        assert_eq!(
            fs::read_to_string(fixture.cache().join(CHECKED)).unwrap(),
            format!("{NOW}\n")
        );
    }

    #[test]
    fn state_files_must_be_regular_and_the_cache_private() {
        let fixture = Fixture::new("state-shapes");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        let cache = fixture.cache();
        prepare_cache(&cache).unwrap();
        fs::create_dir(cache.join(ADMITTED)).unwrap();
        let error = fixture.update(source(), true, NOW).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        fs::remove_dir(cache.join(ADMITTED)).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
        fixture.update(source(), true, NOW).unwrap();
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
        require_account(&fixture.home).unwrap();
    }

    impl Fixture {
        fn ready(&self, source: FlathubSource) -> Result<Option<u64>, String> {
            ready(&self.cache(), source)
        }
    }

    #[test]
    fn ready_names_current_only_while_it_serves_the_declaration() {
        let fixture = Fixture::new("ready");
        assert_eq!(fixture.ready(source()), Ok(None));
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), false, NOW).unwrap();
        assert_eq!(fixture.ready(source()), Ok(Some(1)));
        // Another entry is another declaration the generation was not
        // validated for.
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source()
        };
        assert_eq!(fixture.ready(moved), Ok(None));
        remove_tree(&fixture.cache().join(DEPLOY).join(&runtime)).unwrap();
        assert_eq!(fixture.ready(source()), Ok(None));
        // It reads and writes nothing it does not need: no lock file appears
        // in a cache that has none.
        let empty = Fixture::new("ready-empty");
        assert_eq!(empty.ready(source()), Ok(None));
        assert!(!empty.cache().exists());
    }

    #[test]
    fn collection_spares_a_generation_a_launch_holds() {
        let fixture = Fixture::new("held");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let first = app_commit(&fixture, b"one", 20);
        fixture.update(source(), true, NOW).unwrap();
        let selection = fixture.cache().join(GENERATIONS).join("1").join(SELECTION);
        let launch = File::open(&selection).unwrap();
        launch.lock_shared().unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(), true, NOW).unwrap();
        app_commit(&fixture, b"three", 40);
        fixture.update(source(), true, NOW).unwrap();
        // Neither current nor previous, and still running.
        assert_eq!(fixture.names(GENERATIONS), vec!["1", "2", "3"]);
        assert!(deployed(&fixture.cache(), &first));
        drop(launch);
        app_commit(&fixture, b"four", 50);
        fixture.update(source(), true, NOW).unwrap();
        assert_eq!(fixture.names(GENERATIONS), vec!["3", "4"]);
        assert!(!deployed(&fixture.cache(), &first));
        // A launch that opened generation 1 before it went finds it unlinked.
        assert!(!selection.exists());
    }

    /// A selection that cannot be opened for any reason but absence may
    /// still be held: collection stops rather than remove it, and a removal
    /// cut short is finished by the next one.
    #[test]
    fn collection_removes_only_what_it_proved_free() {
        let fixture = Fixture::new("unproved");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let first = app_commit(&fixture, b"one", 20);
        fixture.update(source(), true, NOW).unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(), true, NOW).unwrap();
        let selection = fixture.cache().join(GENERATIONS).join("1").join(SELECTION);
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o000)).unwrap();
        app_commit(&fixture, b"three", 40);
        let error = fixture.update(source(), true, NOW).unwrap_err();
        assert!(error.contains("open"), "{error}");
        assert!(deployed(&fixture.cache(), &first));
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o644)).unwrap();

        let stale = fixture
            .cache()
            .join(DEPLOY)
            .join(format!("{REMOVING}{first}"));
        fs::create_dir_all(stale.join(FILES)).unwrap();
        fixture.update(source(), true, NOW).unwrap();
        assert!(!stale.exists());
        assert!(!deployed(&fixture.cache(), &first));
        assert!(!fixture
            .names(DEPLOY)
            .iter()
            .any(|name| name.starts_with(REMOVING)));
    }
}
