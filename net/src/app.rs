//! td-app: fetch, admit and select one account's Flathub application
//! (APPLICATIONS.md §W.9).
//!
//! `td-app update NAME [--force]` runs as the application's account and owns
//! `$HOME/.td/flathub`. It follows the declared refs as hints, admits a
//! candidate only under rules 1–5, and publishes a generation by replacing
//! `current` with one rename. A refusal or failure leaves `current` as it was.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

use std::ffi::{CStr, OsStr};
use std::fs::{self, File, OpenOptions};
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
const REGISTRY: &str = "/etc/td-applications.tsv";
const MAX_REGISTRY_BYTES: u64 = 64 * 1024;
const MAX_REGISTRY_ROWS: usize = 256;
/// Part of every checks digest: raise it when rule 5's validator changes
/// what it admits, so selections it never saw are validated again.
const CHECKS_VERSION: &str = "td-app-checks-1";
/// Every directory td-app lists holds a handful of entries; more is damage.
const MAX_LISTED_ENTRIES: usize = 4096;

/// Checks the signature and ref bindings of one fetched commit (rules 1–2).
type Verify = fn(&str, Checksum, &[u8], &[u8], u64) -> Result<Commit, String>;
/// The dynamic validator over an application and runtime tree (rule 5).
type Validate = fn(&Path, &str, &Path, &[String], &[String], usize) -> Result<(), String>;

struct Admission<'a> {
    repository: &'a str,
    verify: Verify,
    validate: Validate,
    registry: &'a Path,
}

#[derive(Debug, Eq, PartialEq)]
enum Outcome {
    Throttled,
    Unchanged,
    Selected(u64),
}

/// What the declaration asks for before anything is fetched.
struct Expected {
    package: Option<PathBuf>,
    checks: String,
    library_paths: Vec<String>,
    optional_targets: Vec<String>,
    optional_links: usize,
}

struct Candidate {
    admitted: Admitted,
    content: Checksum,
    metadata: Option<String>,
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
    let head = fetch_small(
        &format!("{}/refs/heads/{exact_ref}", admission.repository),
        &work.join("ref"),
        MAX_REF_BYTES,
        deadline,
    )?;
    let hex = std::str::from_utf8(&head)
        .ok()
        .map(|text| text.strip_suffix('\n').unwrap_or(text))
        .ok_or_else(|| format!("{exact_ref} does not name a commit"))?;
    let commit = Checksum::from_hex(hex).map_err(|error| format!("{exact_ref}: {error}"))?;
    let commit_bytes = fetch_small(
        &object_url(
            admission.repository,
            ObjectKey::new(ObjectKind::Commit, commit),
        )?,
        &work.join("commit"),
        ObjectKind::Commit.max_transfer()?,
        deadline,
    )?;
    let commitmeta_limit = u64::try_from(ostree::MAX_COMMITMETA_BYTES)
        .map_err(|_| "commitmeta limit does not fit u64".to_string())?;
    let commitmeta = fetch_small(
        &commitmeta_url(admission.repository, commit)?,
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
    crate::ostree::acquire(&spec, &objects)?;
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

/// The application tree of a runtime-only source: its `/td/store` package,
/// read under td-jail's registry rules.
fn registry_package(registry: &Path, name: &str) -> Result<PathBuf, String> {
    let bytes = read_bounded(registry, MAX_REGISTRY_BYTES)?
        .ok_or_else(|| format!("{} is missing", registry.display()))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8", registry.display()))?;
    let mut previous: Option<&str> = None;
    let mut found = None;
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_REGISTRY_ROWS {
            return Err(format!("{} has too many rows", registry.display()));
        }
        let mut fields = line.split('\t');
        let (Some(row), Some(path), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!("{} row {line:?} is malformed", registry.display()));
        };
        if previous.is_some_and(|prior| prior >= row) {
            return Err(format!("{} is not strictly sorted", registry.display()));
        }
        previous = Some(row);
        if !flathub_app::valid_package(path) {
            return Err(format!(
                "{} row {line:?} names no store package",
                registry.display()
            ));
        }
        if row == name {
            found = Some(PathBuf::from(path));
        }
    }
    found.ok_or_else(|| format!("{} has no {name}", registry.display()))
}

/// The rule-5 inputs no commit carries, and their digest, recorded in the
/// selection so a change to any of them forces a fresh validation.
fn expect(admission: &Admission<'_>, source: FlathubSource) -> Result<Expected, String> {
    let policy = dynamic_application_policy(source.name, source.policy_runtime)
        .ok_or_else(|| format!("{} has no dynamic-library policy", source.name))?;
    let package = match source.application {
        Some(_) => None,
        None => Some(registry_package(admission.registry, source.name)?),
    };
    let mut hasher = td_engine::sha256::Sha256::new();
    let mut record = |key: &str, value: &[u8]| {
        hasher.update(key.as_bytes());
        hasher.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value);
    };
    record("version", CHECKS_VERSION.as_bytes());
    record("entry", source.entry.as_bytes());
    record(
        "package",
        package
            .as_deref()
            .map_or(&[][..], |path| path.as_os_str().as_bytes()),
    );
    for path in policy.library_paths() {
        record("library", path.as_bytes());
    }
    for target in policy.optional_targets() {
        record("optional", target.as_bytes());
    }
    record("links", &policy.optional_links().to_be_bytes());
    let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
    Ok(Expected {
        package,
        checks: td_engine::sha256::to_base16(&hasher.finalize()),
        library_paths: strings(policy.library_paths()),
        optional_targets: strings(policy.optional_targets()),
        optional_links: policy.optional_links(),
    })
}

fn package_text(expected: &Expected) -> Result<Option<String>, String> {
    expected
        .package
        .as_deref()
        .map(|path| {
            path.to_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{} is not UTF-8", path.display()))
        })
        .transpose()
}

/// A selection still serves the declaration: its name, refs and recorded
/// checks match, and every deploy it names is complete.
fn serves(
    cache: &Path,
    selection: &Selection,
    source: FlathubSource,
    expected: &Expected,
) -> Result<bool, String> {
    Ok(selection.name == source.name
        && selection.checks == expected.checks
        && selection.package == package_text(expected)?
        && selection
            .application
            .as_ref()
            .map(|row| row.exact_ref.as_str())
            == source.application
        && selection.runtime.exact_ref == source.runtime
        && selection.commits().all(|commit| deployed(cache, commit)))
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

/// Keep `current`, `previous`, the commits they name and `also`; nothing
/// else in the cache survives.
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
        if !keep_generations.contains(&name) {
            remove_tree(&path)?;
        }
    }
    for area in [DEPLOY, OBJECTS] {
        for (name, path) in list(&cache.join(area))? {
            if !keep_commits.contains(&name) {
                remove_tree(&path)?;
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
    let expected = expect(admission, source)?;
    let current = read_pointer(cache, CURRENT)?;
    let previous = read_pointer(cache, PREVIOUS)?;
    if !force {
        if let Some((_, selection)) = &current {
            let checked = read_text(&cache.join(CHECKED))?.and_then(|text| {
                text.strip_suffix('\n')
                    .and_then(|text| text.parse::<u64>().ok())
            });
            if checked.is_some_and(|checked| checked <= now && now - checked < CHECK_INTERVAL_SECS)
                && serves(cache, selection, source, &expected)?
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
    admitted.retain(|row| {
        row.exact_ref == source.runtime || Some(row.exact_ref.as_str()) == source.application
    });
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
    let application = match source.application {
        Some(exact_ref) => {
            let candidate = resolve(admission, &work, exact_ref, &known, now, deadline)?;
            let metadata = candidate
                .metadata
                .as_deref()
                .ok_or_else(|| format!("{exact_ref} commit carries no xa.metadata"))?;
            flathub_app::require_runtime(metadata, source.runtime)?;
            Some(candidate)
        }
        None => None,
    };
    remove_tree(&work)?;
    drop(known);

    let selection = Selection {
        name: source.name.to_string(),
        checks: expected.checks.clone(),
        application: application
            .as_ref()
            .map(|candidate| candidate.admitted.clone()),
        package: package_text(&expected)?,
        runtime: runtime.admitted.clone(),
    };
    let candidates: Vec<&Candidate> = application
        .iter()
        .chain(std::iter::once(&runtime))
        .collect();
    for candidate in &candidates {
        admitted.retain(|row| row.exact_ref != candidate.admitted.exact_ref);
        admitted.push(candidate.admitted.clone());
    }
    let admitted_text = flathub_app::render_admitted(&admitted);

    let outcome = (|| {
        let same = current
            .as_ref()
            .is_some_and(|(_, selected)| *selected == selection);
        if same && serves(cache, &selection, source, &expected)? {
            // Heals an `admitted` that an interrupted run left behind.
            write_state(&cache.join(ADMITTED), &admitted_text)?;
            return Ok(Outcome::Unchanged);
        }
        let runtime_out = deploy(admission, cache, &runtime)?;
        let application_out = match &application {
            Some(candidate) => deploy(admission, cache, candidate)?,
            None => expected
                .package
                .clone()
                .ok_or("a runtime-only source has no package")?,
        };
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

fn usage() -> ! {
    eprintln!("usage: td-app update NAME [--force]");
    std::process::exit(2);
}

pub fn run(args: &[String]) {
    let (name, force) = match args.get(1..) {
        Some([verb, name]) if verb == "update" => (name, false),
        Some([verb, name, flag]) if verb == "update" && flag == "--force" => (name, true),
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
        let admission = Admission {
            repository: flathub_app::REPOSITORY,
            verify: verify_flathub,
            validate: validate_dynamic,
            registry: Path::new(REGISTRY),
        };
        update(
            &admission,
            &home.join(CACHE_DIR),
            source,
            force,
            unix_now()?,
        )
    })();
    match result {
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
    const PACKAGE: &str = "/td/store/0123456789abcdefghijklmnopqrstuv-firefox-1";

    type Repository = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

    thread_local! {
        static VALIDATED: RefCell<Vec<(PathBuf, PathBuf)>> = const { RefCell::new(Vec::new()) };
        static REFUSE: RefCell<bool> = const { RefCell::new(false) };
    }

    fn source(application: Option<&'static str>) -> FlathubSource {
        FlathubSource {
            name: "firefox",
            application,
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
        registry: PathBuf,
        repository: Repository,
        url: String,
    }

    impl Fixture {
        fn new(label: &str) -> Fixture {
            let root = crate::test_temp::TempDir::new(&format!("td-app-{label}")).unwrap();
            let home = root.join("home");
            fs::create_dir(&home).unwrap();
            let registry = root.join("td-applications.tsv");
            fs::write(&registry, format!("firefox\t{PACKAGE}\n")).unwrap();
            let repository = Repository::default();
            let url = serve(repository.clone());
            Fixture {
                _root: root,
                home,
                registry,
                repository,
                url,
            }
        }

        fn cache(&self) -> PathBuf {
            self.home.join(CACHE_DIR)
        }

        fn update(&self, source: FlathubSource, force: bool, now: u64) -> Result<Outcome, String> {
            let admission = Admission {
                repository: &self.url,
                verify: verify_fixture,
                validate: validate_fixture,
                registry: &self.registry,
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
            fixture.update(source(Some(APP)), false, NOW),
            Ok(Outcome::Selected(1))
        );

        let (generation, selection) = fixture.current().unwrap();
        assert_eq!(generation, 1);
        assert_eq!(selection.runtime.commit, runtime);
        assert_eq!(selection.runtime.timestamp, 10);
        assert_eq!(selection.application.as_ref().unwrap().commit, app);
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
        fixture.update(source(Some(APP)), false, NOW).unwrap();

        // An empty repository shows that the throttled check asks nothing.
        fixture.repository.lock().unwrap().clear();
        assert_eq!(
            fixture.update(source(Some(APP)), false, NOW + CHECK_INTERVAL_SECS - 1),
            Ok(Outcome::Throttled)
        );
        assert!(fixture
            .update(source(Some(APP)), false, NOW + CHECK_INTERVAL_SECS)
            .is_err());
        assert!(fixture.update(source(Some(APP)), true, NOW + 1).is_err());
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
            fixture.update(source(Some(APP)), true, NOW + 2),
            Ok(Outcome::Unchanged)
        );
        assert_eq!(
            fs::read_to_string(fixture.cache().join(CHECKED)).unwrap(),
            format!("{}\n", NOW + 2)
        );
        // A clock that went backwards does not suppress the check.
        assert_eq!(
            fixture.update(source(Some(APP)), false, NOW),
            Ok(Outcome::Unchanged)
        );
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
        fixture.update(source(Some(APP)), true, NOW).unwrap();
        let second = publish_commit(
            &fixture.repository,
            APP,
            b"two",
            30,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(
            fixture.update(source(Some(APP)), true, NOW),
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
            fixture.update(source(Some(APP)), true, NOW),
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
        fixture.update(source(Some(APP)), true, NOW).unwrap();

        let refused = |expected: &str| {
            let error = fixture.update(source(Some(APP)), true, NOW).unwrap_err();
            assert!(error.contains(expected), "{error}");
            let (generation, selection) = fixture.current().unwrap();
            assert_eq!(generation, 1);
            assert_eq!(selection.application.unwrap().commit, admitted);
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
            fixture.update(source(Some(APP)), true, NOW),
            Ok(Outcome::Selected(2))
        );
    }

    #[test]
    fn a_runtime_only_source_validates_against_its_store_package() {
        let fixture = Fixture::new("runtime-only");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        VALIDATED.with(|seen| seen.borrow_mut().clear());
        assert_eq!(
            fixture.update(source(None), false, NOW),
            Ok(Outcome::Selected(1))
        );
        assert!(fixture.current().unwrap().1.application.is_none());
        VALIDATED.with(|seen| {
            assert_eq!(
                *seen.borrow(),
                vec![(
                    PathBuf::from(PACKAGE),
                    fixture.cache().join(DEPLOY).join(&runtime),
                )]
            );
        });
    }

    #[test]
    fn registry_rows_name_only_direct_store_children() {
        let fixture = Fixture::new("registry");
        for row in [
            "firefox\t/td/store/../etc\n",
            "firefox\t/td/store/a/b\n",
            "firefox\t/var/lib/x\n",
            "firefox\t/td/store/\n",
            "firefox /td/store/x\n",
            "firefox\t/td/store/x\textra\n",
            "zeta\t/td/store/z\nfirefox\t/td/store/x\n",
        ] {
            fs::write(&fixture.registry, row).unwrap();
            assert!(
                registry_package(&fixture.registry, "firefox").is_err(),
                "{row:?}"
            );
        }
        fs::write(&fixture.registry, "claude\t/td/store/x-claude\n").unwrap();
        assert!(registry_package(&fixture.registry, "firefox")
            .unwrap_err()
            .contains("has no firefox"));
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
        fixture.update(source(Some(APP)), true, NOW).unwrap();
        let objects = fixture.names(OBJECTS);
        let deploys = fixture.names(DEPLOY);
        let refuse = |expected: &str| {
            let error = fixture.update(source(Some(APP)), true, NOW).unwrap_err();
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
        fixture.update(source(Some(APP)), true, NOW).unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(Some(APP)), true, NOW).unwrap();
        fs::remove_file(fixture.cache().join(ADMITTED)).unwrap();
        // Generation 1's commit, still validly signed, served again.
        app_commit(&fixture, b"one", 20);
        let error = fixture.update(source(Some(APP)), true, NOW).unwrap_err();
        assert!(error.contains("does not supersede"), "{error}");
        // An unchanged check rewrites the floor it lost.
        app_commit(&fixture, b"two", 30);
        assert_eq!(
            fixture.update(source(Some(APP)), true, NOW),
            Ok(Outcome::Unchanged)
        );
        let admitted = fs::read_to_string(fixture.cache().join(ADMITTED)).unwrap();
        assert_eq!(flathub_app::parse_admitted(&admitted).unwrap().len(), 2);
    }

    #[test]
    fn a_changed_declaration_or_a_missing_deploy_is_never_throttled() {
        let fixture = Fixture::new("declaration");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(Some(APP)), false, NOW).unwrap();

        // Another entry is another rule-5 input: the same commits are
        // validated again and selected as a new generation.
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source(Some(APP))
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
            fixture.update(source(Some(APP)), false, NOW),
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
        fixture.update(source(Some(APP)), true, NOW).unwrap();
        REFUSE.with(|refuse| *refuse.borrow_mut() = true);
        let first = app_commit(&fixture, b"first", 30);
        assert!(fixture.update(source(Some(APP)), true, NOW + 1).is_err());
        assert!(deployed(&fixture.cache(), &first));
        let second = app_commit(&fixture, b"second", 40);
        assert!(fixture.update(source(Some(APP)), true, NOW + 2).is_err());
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
        let error = fixture.update(source(Some(APP)), true, NOW).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        fs::remove_dir(cache.join(ADMITTED)).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
        fixture.update(source(Some(APP)), true, NOW).unwrap();
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
        require_account(&fixture.home).unwrap();
    }
}
