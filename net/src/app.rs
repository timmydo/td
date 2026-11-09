//! td-app: fetch, admit and select Flathub runtimes and applications
//! (APPLICATIONS.md §W.9).
//!
//! `td-app runtime update NAME --force` runs as the runtime account and owns
//! one store below its home, `/var/lib/td/runtimes/NAME`, which every
//! account may read. `td-app update NAME --force` runs as an application's
//! account and owns `$HOME/.td/flathub`, which holds the application's tree
//! alone: it is validated against the runtime store's `current`, and
//! follows it. Every update asks: there is no throttle, since only the
//! owner's request in td-software runs one (td-software/DESIGN.md §7). Each
//! follows its declared ref as a hint, admits a candidate only under rules
//! 1–5, and publishes a generation by replacing `current` with one rename.
//! A refusal or failure leaves `current` as it was. `td-app ready NAME`
//! names the generation td-jail may launch.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use td_engine::application_spec::dynamic_application_policy;
use td_engine::flathub_app::{
    self, Admitted, CacheStatus, DeployStatus, FlathubRuntime, FlathubSource, GenerationStatus,
    Part, Progress, Refusal, RuntimeGenerationStatus, RuntimeSelection, RuntimeStatus, Selection,
    Stage, Status, ADMITTED, CACHE_DIR, CACHE_ENTRIES, CHECKED, CURRENT, DEPLOY, FILES,
    GENERATIONS, LOCK, OBJECTS, PREVIOUS, PROGRESS, REFUSED, RUNTIME_ACCOUNT, RUNTIME_STORE,
    RUNTIME_UID, SELECTION, WORK,
};
use td_engine::ostree::{self, Checksum, Commit, ObjectKey, ObjectKind};

use crate::ostree::AcquireSpec;

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
    /// `revalidate` found a writer that fetches or removes holding the
    /// cache's lock, and did not wait for it.
    Busy,
    Status(Box<Status>),
    RuntimeStatus(Box<RuntimeStatus>),
    Removed,
    /// Whether there was state to delete.
    Deleted(bool),
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

/// What a store holds: an application's cache, private to its account, or
/// one runtime's store, which every account reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Application,
    Runtime,
}

/// A cache td-app writes, which its verbs take rather than each deriving
/// it from the account's home.
struct Store {
    root: PathBuf,
    kind: Kind,
}

impl Store {
    fn application(root: PathBuf) -> Store {
        Store {
            root,
            kind: Kind::Application,
        }
    }

    fn runtime(root: PathBuf) -> Store {
        Store {
            root,
            kind: Kind::Runtime,
        }
    }

    /// The mode of the root, `deploy/` and `generations/`: a runtime store
    /// is read by every application account that validates or launches on
    /// it, so those three are 0755 there; `objects/`, `.work`, the lock and
    /// the state files stay private.
    fn reader_mode(&self) -> u32 {
        match self.kind {
            Kind::Application => 0o700,
            Kind::Runtime => 0o755,
        }
    }

    /// The commits a generation keeps here. An application's keeps only
    /// its own: its runtime's tree is the runtime store's, so a runtime
    /// deploy an older cache holds goes at the next collection.
    fn kept(&self, text: &str) -> Result<String, String> {
        Ok(match self.kind {
            Kind::Application => Selection::parse(text)?.application.commit,
            Kind::Runtime => RuntimeSelection::parse(text)?.runtime.commit,
        })
    }
}

/// A runtime store as an application's verbs see it: read, never written,
/// and trusted only as `owner`'s.
struct RuntimeView {
    root: PathBuf,
    owner: u32,
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

fn prepare_cache(store: &Store) -> Result<(), String> {
    let cache = &store.root;
    let parent = cache
        .parent()
        .ok_or_else(|| format!("cache {} has no parent", cache.display()))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
    private_dir(cache)?;
    // Set rather than created: the umask clips a mkdir mode, and an
    // existing directory keeps whatever it had.
    let mode = store.reader_mode();
    fs::set_permissions(cache, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", cache.display()))?;
    private_dir(&cache.join(OBJECTS))?;
    for name in [DEPLOY, GENERATIONS] {
        let path = cache.join(name);
        private_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
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
    let (path, file) = lock_file(cache)?;
    file.lock()
        .map_err(|error| format!("lock {}: {error}", path.display()))?;
    forget_progress(cache);
    Ok(file)
}

fn lock_file(cache: &Path) -> Result<(PathBuf, File), String> {
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
    Ok((path, file))
}

/// A record a killed writer left would read as this writer's until it
/// writes its own. Best effort: debris here must not block the run that
/// clears `.work`.
fn forget_progress(cache: &Path) {
    let _ = remove_entry(&cache.join(WORK).join(PROGRESS));
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

/// The generation a pointer names, without reading it.
fn pointer(cache: &Path, name: &str) -> Result<Option<u64>, String> {
    let link = cache.join(name);
    let target = match fs::read_link(&link) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", link.display())),
    };
    let target = target
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", link.display()))?;
    flathub_app::parse_pointer(target)
        .map(Some)
        .map_err(|error| format!("{}: {error}", link.display()))
}

fn selection_path(cache: &Path, generation: u64) -> PathBuf {
    cache
        .join(GENERATIONS)
        .join(generation.to_string())
        .join(SELECTION)
}

/// The generation a pointer names, and its selection's text.
fn read_pointer_text(cache: &Path, name: &str) -> Result<Option<(u64, String)>, String> {
    let Some(generation) = pointer(cache, name)? else {
        return Ok(None);
    };
    let path = selection_path(cache, generation);
    let text = read_text(&path)?.ok_or_else(|| format!("{} is missing", path.display()))?;
    Ok(Some((generation, text)))
}

/// The generation a pointer of an application's cache names, and its
/// selection.
fn read_pointer(cache: &Path, name: &str) -> Result<Option<(u64, Selection)>, String> {
    read_pointer_text(cache, name)?
        .map(|(generation, text)| Ok((generation, Selection::parse(&text)?)))
        .transpose()
}

/// The generation a pointer of a runtime store names, and its selection.
fn read_runtime_pointer(
    cache: &Path,
    name: &str,
) -> Result<Option<(u64, RuntimeSelection)>, String> {
    read_pointer_text(cache, name)?
        .map(|(generation, text)| Ok((generation, RuntimeSelection::parse(&text)?)))
        .transpose()
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

/// What a check holds while it takes each declared ref through a store:
/// `admit`, then `deploy`.
struct RefCheck<'a> {
    admission: &'a Admission<'a>,
    store: &'a Store,
    tracker: &'a Tracker,
    work: PathBuf,
    now: u64,
    deadline: Instant,
}

impl RefCheck<'_> {
    /// Rules 1–3 for whatever commit `exact_ref` names now, above the
    /// floor `known` sets.
    fn admit(&self, exact_ref: &str, part: Part, known: &[&Admitted]) -> Result<Candidate, String> {
        self.tracker.set(Stage::Resolve, Some(part));
        resolve(
            self.admission,
            &self.work,
            exact_ref,
            known,
            self.now,
            self.deadline,
        )
    }

    /// Acquire and materialize one admitted commit unless its deploy
    /// exists. Publication is one rename, so an existing `files/` is
    /// complete; a deploy without one holds only what an interrupted run
    /// left.
    fn deploy(&self, candidate: &Candidate, part: Part) -> Result<PathBuf, String> {
        let (admission, cache, tracker) = (self.admission, &self.store.root, self.tracker);
        let hex = &candidate.admitted.commit;
        let out = deploy_path(cache, hex);
        if deployed(cache, hex) {
            return Ok(out);
        }
        if fs::symlink_metadata(&out).is_ok() {
            remove_tree(&set_aside(cache, DEPLOY, hex)?)?;
        }
        let objects = cache.join(OBJECTS).join(hex);
        let spec = AcquireSpec::parse(
            admission.repository,
            &candidate.admitted.exact_ref,
            hex,
            &candidate.content.to_hex(),
        )?;
        // A graph already complete is never fetched, so its counters stay
        // at nothing rather than the last graph's.
        tracker.graph.reset();
        tracker.set(Stage::Fetch, Some(part));
        let progress = Some(&tracker.graph);
        match admission.feed {
            Some(base) if candidate.from_feed => {
                crate::ostree::acquire_from_feed(&spec, &objects, base, |_| Ok(()), progress)?;
            }
            _ => {
                crate::ostree::acquire(&spec, &objects, progress)?;
            }
        }
        tracker.set(Stage::Materialize, Some(part));
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
}

/// The rule-5 inputs no commit carries, and their digest, recorded in the
/// selection so a change to any of them forces a fresh validation.
fn expect(source: FlathubSource) -> Result<Expected, String> {
    let policy = dynamic_application_policy(source.name, source.runtime.name)
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
/// checks match, its application deploy is complete, and it was validated
/// against `runtime`, the runtime store's serving `current`, which every
/// application follows.
fn serves(
    cache: &Path,
    selection: &Selection,
    source: FlathubSource,
    expected: &Expected,
    runtime: Option<&Admitted>,
) -> bool {
    selection.name == source.name
        && selection.checks == expected.checks
        && selection.application.exact_ref == source.application
        && selection.runtime.exact_ref == source.runtime.exact_ref
        && runtime == Some(&selection.runtime)
        && deployed(cache, &selection.application.commit)
}

/// A runtime store's generation still serves its declaration.
fn runtime_serves(cache: &Path, selection: &RuntimeSelection, runtime: FlathubRuntime) -> bool {
    selection.name == runtime.name
        && selection.runtime.exact_ref == runtime.exact_ref
        && deployed(cache, &selection.runtime.commit)
}

/// The runtime store's `current` row while it serves, read without a lock
/// as `ready` and `status` read; a store that cannot be read serves
/// nothing, and the revalidation that follows says why.
fn runtime_serving(view: &RuntimeView, runtime: FlathubRuntime) -> Option<Admitted> {
    read_pointer_settled(&view.root, CURRENT, RuntimeSelection::parse)
        .ok()
        .flatten()
        .filter(|(_, selection)| runtime_serves(&view.root, selection, runtime))
        .map(|(_, selection)| selection.runtime)
}

/// A runtime store's `current`, held: the shared lock on its selection
/// keeps the runtime account's collection from removing it while an
/// application is validated against it.
struct HeldRuntime {
    selection: RuntimeSelection,
    /// Its `deploy/<commit>`, which the validator reads.
    deploy: PathBuf,
    _lease: File,
}

/// Hold the runtime store's `current` and check it is the owner's: its
/// selection and its deploy's `files/` owned by `view.owner` and writable
/// by no one else. One collected between the link and the lock is read
/// again, as td-jail does.
fn hold_runtime(view: &RuntimeView, runtime: FlathubRuntime) -> Result<HeldRuntime, String> {
    for _ in 0..POINTER_ATTEMPTS {
        let Some(generation) = pointer(&view.root, CURRENT)? else {
            return Err(format!(
                "runtime {} is not installed; update the application to install it",
                runtime.name
            ));
        };
        let path = selection_path(&view.root, generation);
        let lease = match OpenOptions::new()
            .read(true)
            .custom_flags(crate::feed::OPEN_REGULAR_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("open {}: {error}", path.display())),
        };
        lease
            .lock_shared()
            .map_err(|error| format!("lock {}: {error}", path.display()))?;
        let metadata = lease
            .metadata()
            .map_err(|error| format!("stat {}: {error}", path.display()))?;
        if metadata.nlink() == 0 {
            continue;
        }
        if !metadata.is_file() || metadata.uid() != view.owner || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "{} is not a regular file only the runtime account writes",
                path.display()
            ));
        }
        let limit = u64::try_from(flathub_app::MAX_STATE_BYTES)
            .map_err(|_| "state limit does not fit u64".to_string())?;
        let mut bytes = Vec::new();
        (&lease)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        let selection = RuntimeSelection::parse(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if selection.name != runtime.name || selection.runtime.exact_ref != runtime.exact_ref {
            return Err(format!(
                "runtime store {}'s current names another runtime",
                view.root.display()
            ));
        }
        let deploy = deploy_path(&view.root, &selection.runtime.commit);
        let files = deploy.join(FILES);
        let tree = fs::symlink_metadata(&files)
            .map_err(|error| format!("stat {}: {error}", files.display()))?;
        if !tree.is_dir() || tree.uid() != view.owner || tree.mode() & 0o022 != 0 {
            return Err(format!(
                "{} is not a directory only the runtime account writes",
                files.display()
            ));
        }
        return Ok(HeldRuntime {
            selection,
            deploy,
            _lease: lease,
        });
    }
    Err(format!(
        "runtime {}'s current generation was collected each time td-app tried to hold it",
        runtime.name
    ))
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
/// `previous` becomes the `previous` pointer: an update passes the
/// generation it replaces, a revalidation the rollback it keeps.
/// `selection` is the rendered selection, an application's or a runtime's.
fn publish(cache: &Path, selection: &str, previous: Option<u64>) -> Result<u64, String> {
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
    file.write_all(selection.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    sync_dir(&directory)?;
    for area in [GENERATIONS, DEPLOY, OBJECTS] {
        sync_dir(&cache.join(area))?;
    }
    if let Some(previous) = previous {
        point(cache, PREVIOUS, previous)?;
    }
    point(cache, CURRENT, next)?;
    sync_dir(cache)?;
    Ok(next)
}

/// Remove a tree the account owns, opening any directory a reader sealed.
fn remove_tree(path: &Path) -> Result<(), String> {
    remove_tree_on(path, None)
}

/// `remove_tree`, refusing before it removes anything when a directory in
/// the tree is on another filesystem than `device`.
fn remove_tree_on(path: &Path, device: Option<u64>) -> Result<(), String> {
    if open_tree(path, device)? {
        fs::remove_dir_all(path).map_err(|error| format!("remove {}: {error}", path.display()))
    } else {
        fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))
    }
}

/// Refuse a tree holding a directory on another filesystem than `device`,
/// changing nothing; a directory it cannot read is passed over.
fn foreign_directory(path: &Path, device: u64) -> Result<(), String> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.file_type().is_dir() {
                if metadata.dev() != device {
                    return Err(format!(
                        "{} is another filesystem's; nothing was removed",
                        entry.path().display()
                    ));
                }
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Open every directory of a tree to its owner, refusing a directory on
/// another filesystem than `device` before anything is removed; whether
/// `path` is a directory.
fn open_tree(path: &Path, device: Option<u64>) -> Result<bool, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Ok(false);
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
                if let Some(device) = device {
                    let found = fs::symlink_metadata(entry.path())
                        .map_err(|error| format!("stat {}: {error}", entry.path().display()))?
                        .dev();
                    if found != device {
                        return Err(format!(
                            "{} is another filesystem's; nothing was removed",
                            entry.path().display()
                        ));
                    }
                }
                pending.push(entry.path());
            }
        }
    }
    Ok(true)
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

/// Rename `area`'s entry `name` to `.removing-<name>` beside it, removing
/// one an earlier removal cut short, and return where it went. Removal
/// opens a tree's directories to their owner only, so in a runtime store,
/// which other accounts read, a tree must leave its name first: a reader
/// looking it up then finds it absent, which it reads as collected, never
/// a directory it may not enter.
fn set_aside(cache: &Path, area: &str, name: &str) -> Result<PathBuf, String> {
    let path = cache.join(area).join(name);
    let doomed = cache.join(area).join(format!("{REMOVING}{name}"));
    if fs::symlink_metadata(&doomed).is_ok() {
        remove_tree(&doomed)?;
    }
    fs::rename(&path, &doomed)
        .map_err(|error| format!("rename {} for removal: {error}", path.display()))?;
    Ok(doomed)
}

/// Keep `current`, `previous`, every generation a running td-jail or an
/// application's validation holds, the commits they keep here
/// ([`Store::kept`]) and `also`; nothing else in the cache survives. A
/// generation goes only under an exclusive lock on its selection, so a
/// holder that locks it afterwards finds it unlinked and asks again.
fn collect(store: &Store, also: &[String]) -> Result<(), String> {
    let cache = &store.root;
    let mut keep_generations = Vec::new();
    let mut keep_commits: Vec<String> = also.to_vec();
    for name in [CURRENT, PREVIOUS] {
        if let Some((generation, text)) = read_pointer_text(cache, name)? {
            keep_generations.push(generation.to_string());
            keep_commits.push(store.kept(&text)?);
        }
    }
    for (name, path) in list(&cache.join(GENERATIONS))? {
        if keep_generations.contains(&name) {
            continue;
        }
        let selection = path.join(SELECTION);
        // A generation a reader may look up by number goes aside first,
        // as a deploy does.
        let numbered = flathub_app::generation_number(&name).is_some();
        match hold(&selection)? {
            Hold::Debris if numbered => remove_tree(&set_aside(cache, GENERATIONS, &name)?)?,
            Hold::Debris => remove_tree(&path)?,
            Hold::Free(proof) => {
                if numbered {
                    remove_tree(&set_aside(cache, GENERATIONS, &name)?)?;
                } else {
                    remove_tree(&path)?;
                }
                drop(proof);
            }
            Hold::Held => {
                // A read that fails ends collection, as an open does; one
                // that does not parse names nothing td-jail could launch.
                if let Some(text) = read_text(&selection)? {
                    if let Ok(commit) = store.kept(&text) {
                        keep_commits.push(commit);
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
                    set_aside(cache, area, &name)?
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

fn read_checked(cache: &Path) -> Result<Option<u64>, String> {
    Ok(read_text(&cache.join(CHECKED))?.and_then(|text| {
        text.strip_suffix('\n')
            .and_then(|text| text.parse::<u64>().ok())
    }))
}

/// `.work` afresh, under the lock: nothing a reader trusts is left in it.
fn fresh_work(cache: &Path) -> Result<PathBuf, String> {
    let work = cache.join(WORK);
    if fs::symlink_metadata(&work).is_ok() {
        remove_tree(&work)?;
    }
    private_dir(&work)?;
    Ok(work)
}

/// What a check that did not complete reports through `status` until one
/// does. A failure to record it is reported, never in place of the cause.
fn record_refusal(cache: &Path, now: u64, error: &str) {
    let mut reason = flathub_app::reason_text(error);
    if reason.is_empty() {
        reason.push_str("unknown");
    }
    let refusal = Refusal { time: now, reason };
    if let Err(failure) = write_state(&cache.join(REFUSED), &format!("{}\n", refusal.render())) {
        eprintln!("td-app: {failure}");
    }
}

fn clear_refusal(cache: &Path) -> Result<(), String> {
    let path = cache.join(REFUSED);
    if fs::symlink_metadata(&path).is_ok() {
        remove_tree(&path)?;
    }
    Ok(())
}

/// How often a running check refreshes its progress record.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_TICK: Duration = Duration::from_millis(50);

/// What the writer holding the lock is doing, published as one line in
/// `.work/progress` for `status`, which takes no lock.
struct Tracker {
    path: PathBuf,
    staged: PathBuf,
    stage: Mutex<(Stage, Option<Part>)>,
    /// Serializes the ticker's writes with `set`'s.
    writing: Mutex<()>,
    graph: crate::ostree::Progress,
    done: AtomicBool,
}

/// Ends the ticker however `run`'s body ends, a panic included, so the
/// scope that joins it always finishes.
struct Finish<'a>(&'a AtomicBool);
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Tracker {
    fn new(work: &Path, stage: Stage) -> Tracker {
        let tracker = Tracker {
            path: work.join(PROGRESS),
            staged: work.join(".progress.new"),
            stage: Mutex::new((stage, None)),
            writing: Mutex::new(()),
            graph: crate::ostree::Progress::default(),
            done: AtomicBool::new(false),
        };
        tracker.write();
        tracker
    }

    fn set(&self, stage: Stage, part: Option<Part>) {
        if let Ok(mut current) = self.stage.lock() {
            *current = (stage, part);
        }
        self.write();
    }

    fn record(&self) -> Progress {
        let (stage, part) = self
            .stage
            .lock()
            .map(|current| *current)
            .unwrap_or((Stage::Busy, None));
        let mut progress = Progress::idle(stage);
        progress.part = part;
        if stage == Stage::Fetch {
            let (objects_bound, bytes_bound) = crate::ostree::Progress::bounds();
            // Done before known: a batch is known before any of it is done.
            let objects = self.graph.objects.load(Ordering::Acquire);
            let known = self.graph.known.load(Ordering::Acquire).min(objects_bound);
            progress.objects = objects.min(known);
            progress.objects_known = known;
            progress.objects_bound = objects_bound;
            progress.bytes = self.graph.bytes.load(Ordering::Acquire).min(bytes_bound);
            progress.bytes_bound = bytes_bound;
        }
        progress
    }

    /// Best effort and not durable: one rename publishes the line whole,
    /// and nothing needs it after a crash, since `status` reads it only
    /// while the lock's holder lives. A record that cannot be written
    /// leaves `status` saying only that a writer holds the lock.
    fn write(&self) {
        let Ok(_writing) = self.writing.lock() else {
            return;
        };
        let line = format!("{}\n", self.record().render());
        if fs::write(&self.staged, line).is_ok() {
            let _ = fs::rename(&self.staged, &self.path);
        }
    }

    /// Run `body`, refreshing the record beside it until it returns. A
    /// ticker that cannot start leaves the records `set` writes.
    fn run<T>(&self, body: impl FnOnce() -> T) -> T {
        self.done.store(false, Ordering::Release);
        std::thread::scope(|scope| {
            let finish = Finish(&self.done);
            let ticker = std::thread::Builder::new().spawn_scoped(scope, || {
                let mut last = Instant::now();
                while !self.done.load(Ordering::Acquire) {
                    std::thread::sleep(PROGRESS_TICK);
                    if last.elapsed() >= PROGRESS_INTERVAL {
                        last = Instant::now();
                        self.write();
                    }
                }
            });
            if let Err(error) = ticker {
                eprintln!("td-app: progress is written at each stage only: {error}");
            }
            let result = body();
            drop(finish);
            result
        })
    }
}

/// `.work` goes once a run that holds the lock is over. Failing to remove
/// it does not undo what the run did, and the next run clears it.
fn clear_work(work: &Path) {
    if let Err(error) = remove_tree(work) {
        eprintln!("td-app: {error}");
    }
}

/// One check of a store under its lock, beside a progress record: whatever
/// fails is the last refusal, and one that completes writes `checked`.
fn checked_run(
    store: &Store,
    now: u64,
    body: impl FnOnce(&Tracker) -> Result<Outcome, String>,
) -> Result<Outcome, String> {
    prepare_cache(store)?;
    let cache = &store.root;
    let _lock = lock(cache)?;
    let outcome = (|| -> Result<Outcome, String> {
        let work = fresh_work(cache)?;
        let tracker = Tracker::new(&work, Stage::Resolve);
        let outcome = tracker.run(|| body(&tracker));
        clear_work(&work);
        let outcome = outcome?;
        write_state(&cache.join(CHECKED), &format!("{now}\n"))?;
        clear_refusal(cache)?;
        Ok(outcome)
    })();
    if let Err(error) = &outcome {
        record_refusal(cache, now, error);
    }
    outcome
}

/// `td-app update`: the application against the runtime store's `current`.
fn update(
    admission: &Admission<'_>,
    store: &Store,
    view: &RuntimeView,
    source: FlathubSource,
    now: u64,
) -> Result<Outcome, String> {
    checked_run(store, now, |tracker| {
        let current = read_pointer(&store.root, CURRENT)?;
        let previous = read_pointer(&store.root, PREVIOUS)?;
        check(
            admission,
            store,
            view,
            source,
            (current, previous),
            now,
            tracker,
        )
    })
}

/// `td-app runtime update`: the runtime account's store for one runtime.
fn runtime_update(
    admission: &Admission<'_>,
    store: &Store,
    runtime: FlathubRuntime,
    now: u64,
) -> Result<Outcome, String> {
    checked_run(store, now, |tracker| {
        let current = read_runtime_pointer(&store.root, CURRENT)?;
        let previous = read_runtime_pointer(&store.root, PREVIOUS)?;
        runtime_check(admission, store, runtime, (current, previous), now, tracker)
    })
}

/// A check's outcome after the collection that follows it either way.
fn collected(
    outcome: Result<Outcome, String>,
    collection: Result<(), String>,
) -> Result<Outcome, String> {
    match (outcome, collection) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(collection)) => Err(format!(
            "{error}; collecting the cache also failed: {collection}"
        )),
    }
}

/// The rows a store already holds for its floor (rule 3), and its
/// `admitted` rows for `exact_ref` alone, which bounds the file.
fn admitted_rows(cache: &Path, exact_ref: &str) -> Result<Vec<Admitted>, String> {
    let mut admitted: Vec<Admitted> = match read_text(&cache.join(ADMITTED))? {
        Some(text) => flathub_app::parse_admitted(&text)?,
        None => Vec::new(),
    };
    admitted.retain(|row| row.exact_ref == exact_ref);
    Ok(admitted)
}

fn deadline() -> Result<Instant, String> {
    Instant::now()
        .checked_add(RESOLVE_TIME)
        .ok_or_else(|| "resolve deadline overflows".to_string())
}

type RuntimePointers = (
    Option<(u64, RuntimeSelection)>,
    Option<(u64, RuntimeSelection)>,
);

/// Rules 1–3 for what the runtime's ref names now, its deploy, then
/// publication and collection, under the store's lock. The runtime has no
/// validator of its own: rule 5's runs per application, against whatever
/// `current` it follows.
fn runtime_check(
    admission: &Admission<'_>,
    store: &Store,
    runtime: FlathubRuntime,
    (current, previous): RuntimePointers,
    now: u64,
    tracker: &Tracker,
) -> Result<Outcome, String> {
    let cache = &store.root;
    let mut admitted = admitted_rows(cache, runtime.exact_ref)?;
    let known: Vec<&Admitted> = admitted
        .iter()
        .chain(current.iter().map(|(_, selection)| &selection.runtime))
        .chain(previous.iter().map(|(_, selection)| &selection.runtime))
        .collect();
    let refs = RefCheck {
        admission,
        store,
        tracker,
        work: cache.join(WORK),
        now,
        deadline: deadline()?,
    };
    let candidate = refs.admit(runtime.exact_ref, Part::Runtime, &known)?;
    drop(known);
    let selection = RuntimeSelection {
        name: runtime.name.to_string(),
        runtime: candidate.admitted.clone(),
    };
    admitted.retain(|row| row.exact_ref != runtime.exact_ref);
    admitted.push(candidate.admitted.clone());
    let admitted_text = flathub_app::render_admitted(&admitted);
    let outcome = (|| {
        let same = current
            .as_ref()
            .is_some_and(|(_, selected)| *selected == selection);
        if same && runtime_serves(cache, &selection, runtime) {
            write_state(&cache.join(ADMITTED), &admitted_text)?;
            return Ok(Outcome::Unchanged);
        }
        refs.deploy(&candidate, Part::Runtime)?;
        write_state(&cache.join(ADMITTED), &admitted_text)?;
        if same {
            // Restored in place.
            return Ok(Outcome::Unchanged);
        }
        tracker.set(Stage::Publish, None);
        let generation = publish(
            cache,
            &selection.render(),
            current.as_ref().map(|(number, _)| *number),
        )?;
        Ok(Outcome::Selected(generation))
    })();
    tracker.set(Stage::Collect, None);
    let collection = collect(store, std::slice::from_ref(&candidate.admitted.commit));
    collected(outcome, collection)
}

type Pointers = (Option<(u64, Selection)>, Option<(u64, Selection)>);

/// Rules 1–5 for what the application's ref names now, against the
/// runtime store's `current`, then publication and collection, under the
/// lock and beside `tracker`'s record. The ref is admitted and rule 4
/// checked before its tree is fetched, so an unchanged selection is
/// settled first. The runtime is the runtime account's to fetch: this
/// holds its `current` while the application is validated against it, and
/// records that commit as the selection's `runtime=` row.
fn check(
    admission: &Admission<'_>,
    store: &Store,
    view: &RuntimeView,
    source: FlathubSource,
    (current, previous): Pointers,
    now: u64,
    tracker: &Tracker,
) -> Result<Outcome, String> {
    let cache = &store.root;
    let expected = expect(source)?;
    let mut admitted = admitted_rows(cache, source.application)?;
    let known: Vec<&Admitted> = admitted
        .iter()
        .chain(current.iter().flat_map(|(_, selection)| selection.rows()))
        .chain(previous.iter().flat_map(|(_, selection)| selection.rows()))
        .collect();
    let refs = RefCheck {
        admission,
        store,
        tracker,
        work: cache.join(WORK),
        now,
        deadline: deadline()?,
    };
    let application = refs.admit(source.application, Part::Application, &known)?;
    let metadata = application
        .metadata
        .as_deref()
        .ok_or_else(|| format!("{} commit carries no xa.metadata", source.application))?;
    flathub_app::require_runtime(metadata, source.runtime.exact_ref)?;
    drop(known);
    let runtime = hold_runtime(view, source.runtime)?;

    let selection = Selection {
        name: source.name.to_string(),
        checks: expected.checks.clone(),
        application: application.admitted.clone(),
        runtime: runtime.selection.runtime.clone(),
    };
    admitted.retain(|row| row.exact_ref != source.application);
    admitted.push(application.admitted.clone());
    let admitted_text = flathub_app::render_admitted(&admitted);

    let outcome = (|| {
        let same = current
            .as_ref()
            .is_some_and(|(_, selected)| *selected == selection);
        if same
            && serves(
                cache,
                &selection,
                source,
                &expected,
                Some(&runtime.selection.runtime),
            )
        {
            // Heals an `admitted` that an interrupted run left behind.
            write_state(&cache.join(ADMITTED), &admitted_text)?;
            return Ok(Outcome::Unchanged);
        }
        let application_out = refs.deploy(&application, Part::Application)?;
        tracker.set(Stage::Validate, None);
        (admission.validate)(
            &application_out,
            source.entry,
            &runtime.deploy,
            &expected.library_paths,
            &expected.optional_targets,
            expected.optional_links,
        )?;
        // Written before the flip, so the floor is never below `current`.
        write_state(&cache.join(ADMITTED), &admitted_text)?;
        if same {
            // Current's deploy was restored in place; a new generation
            // would only push the real rollback out.
            return Ok(Outcome::Unchanged);
        }
        tracker.set(Stage::Publish, None);
        let generation = publish(
            cache,
            &selection.render(),
            current.as_ref().map(|(number, _)| *number),
        )?;
        Ok(Outcome::Selected(generation))
    })();
    drop(runtime);
    // A refused candidate keeps its deploy for the next attempt, and only it.
    tracker.set(Stage::Collect, None);
    let collection = collect(store, std::slice::from_ref(&application.admitted.commit));
    collected(outcome, collection)
}

/// The generation td-jail may launch: `current`, while it still serves the
/// declaration against the runtime store's `current`. It takes no lock,
/// and `revalidate` answers `busy` rather than wait behind a writer that
/// fetches or removes, so a launch never waits behind a fetch; collection
/// never removes what `current` names.
fn ready(store: &Store, view: &RuntimeView, source: FlathubSource) -> Result<Option<u64>, String> {
    let cache = &store.root;
    let expected = expect(source)?;
    let runtime = runtime_serving(view, source.runtime);
    Ok(match read_pointer(cache, CURRENT)? {
        Some((generation, selection))
            if serves(cache, &selection, source, &expected, runtime.as_ref()) =>
        {
            Some(generation)
        }
        _ => None,
    })
}

/// The regular-file bytes below a deploy's `files/`, walked without
/// following links; an entry that goes during the walk is not counted.
fn deploy_bytes(files: &Path) -> Result<u64, String> {
    let bound = u64::try_from(ostree::MAX_GRAPH_PATHS).unwrap_or(u64::MAX);
    let mut entries = 0u64;
    let mut bytes = 0u64;
    let mut pending = vec![files.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let listing = match fs::read_dir(&directory) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read {}: {error}", directory.display())),
        };
        for entry in listing {
            let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
            entries += 1;
            if entries > bound {
                return Err(format!("{} has more than {bound} entries", files.display()));
            }
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("stat {}: {error}", entry.path().display())),
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok(bytes)
}

/// A pointer as `status` reads it without the lock: a check can move it
/// and collect the generation it named between the link and the
/// selection, so a missing selection is read again from the link.
fn read_pointer_settled<T>(
    cache: &Path,
    name: &str,
    parse: fn(&str) -> Result<T, String>,
) -> Result<Option<(u64, T)>, String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let read = read_pointer_text(cache, name).and_then(|found| {
            found
                .map(|(generation, text)| Ok((generation, parse(&text)?)))
                .transpose()
        });
        match read {
            Err(_) if attempts < POINTER_ATTEMPTS => {}
            settled => return settled,
        }
    }
}

const POINTER_ATTEMPTS: usize = 3;

fn deploy_status(cache: &Path, row: &Admitted) -> DeployStatus {
    // Deployed before and after the walk: a tree collected meanwhile has
    // no size rather than a partial one.
    let bytes = deployed(cache, &row.commit)
        .then(|| deploy_bytes(&deploy_path(cache, &row.commit).join(FILES)).ok())
        .flatten()
        .filter(|_| deployed(cache, &row.commit));
    DeployStatus {
        row: row.clone(),
        bytes,
    }
}

/// What the writer holding the cache's lock is doing, or `None` when none
/// does. It never waits for or keeps the lock: a shared try-lock answers
/// whether a writer holds it and is released at once, so a record a killed
/// check left behind never reads as running.
fn writer_progress(cache: &Path) -> Result<Option<Progress>, String> {
    let path = cache.join(LOCK);
    let Some(file) = open_regular(&path, OpenOptions::new().read(true))? else {
        return Ok(None);
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(None),
        Err(TryLockError::WouldBlock) => {
            let record = read_text(&cache.join(WORK).join(PROGRESS))
                .ok()
                .flatten()
                .and_then(|text| {
                    text.strip_suffix('\n')
                        .and_then(|line| Progress::parse(line).ok())
                });
            Ok(Some(record.unwrap_or(Progress::idle(Stage::Busy))))
        }
        Err(TryLockError::Error(error)) => Err(format!("lock {}: {error}", path.display())),
    }
}

/// The last refusal a store records. A damaged record is no refusal; it is
/// rewritten or cleared by the next check.
fn read_refusal(cache: &Path) -> Option<Refusal> {
    read_text(&cache.join(REFUSED))
        .ok()
        .flatten()
        .and_then(|text| {
            text.strip_suffix('\n')
                .and_then(|line| Refusal::parse(line).ok())
        })
}

/// `td-app status`: the cache as it reads now, taking no lock, as `ready`
/// takes none. It writes nothing, and a cache that does not exist reads as
/// one with nothing installed. A generation's runtime row is measured in
/// the runtime store, where its tree is.
fn status(store: &Store, view: &RuntimeView, source: FlathubSource) -> Result<Status, String> {
    let cache = &store.root;
    let expected = expect(source)?;
    let runtime = runtime_serving(view, source.runtime);
    let generation = |pointer| -> Result<Option<GenerationStatus>, String> {
        Ok(read_pointer_settled(cache, pointer, Selection::parse)?.map(
            |(generation, selection)| GenerationStatus {
                generation,
                serves: serves(cache, &selection, source, &expected, runtime.as_ref()),
                application: deploy_status(cache, &selection.application),
                runtime: deploy_status(&view.root, &selection.runtime),
            },
        ))
    };
    let current = generation(CURRENT)?;
    let previous = generation(PREVIOUS)?;
    Ok(Status {
        name: source.name.to_string(),
        cache: Some(CacheStatus {
            current,
            previous,
            checked: read_checked(cache)?,
            refused: read_refusal(cache),
            progress: writer_progress(cache)?,
        }),
    })
}

/// `td-app runtime status`: one runtime store as `status` reads a cache.
fn runtime_status(store: &Store, runtime: FlathubRuntime) -> Result<RuntimeStatus, String> {
    let cache = &store.root;
    let generation = |pointer| -> Result<Option<RuntimeGenerationStatus>, String> {
        Ok(
            read_pointer_settled(cache, pointer, RuntimeSelection::parse)?.map(
                |(generation, selection)| RuntimeGenerationStatus {
                    generation,
                    serves: runtime_serves(cache, &selection, runtime),
                    runtime: deploy_status(cache, &selection.runtime),
                },
            ),
        )
    };
    Ok(RuntimeStatus {
        name: runtime.name.to_string(),
        current: generation(CURRENT)?,
        previous: generation(PREVIOUS)?,
        checked: read_checked(cache)?,
        refused: read_refusal(cache),
        progress: writer_progress(cache)?,
    })
}

fn remove_entry(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok() {
        remove_tree(path)?;
    }
    Ok(())
}

/// `td-app remove`: clear `previous`, then `current`, and collect every
/// generation, deploy and object graph no launch holds. A held generation
/// stays until a later collection finds it free. `admitted` stays, so a
/// reinstall admits no commit older than one already admitted, and so does
/// the jail state, which only `delete-data` removes.
fn remove(store: &Store) -> Result<Outcome, String> {
    let cache = &store.root;
    if fs::symlink_metadata(cache).is_err() {
        return Ok(Outcome::Removed);
    }
    prepare_cache(store)?;
    let _lock = lock(cache)?;
    let work = fresh_work(cache)?;
    let _tracker = Tracker::new(&work, Stage::Remove);
    // Previous first: an interrupted removal leaves `current` alone, still
    // installed, rather than a `previous` no `current` stands before.
    let removed = (|| {
        for pointer in [PREVIOUS, CURRENT] {
            remove_entry(&cache.join(pointer))?;
        }
        sync_dir(cache)?;
        remove_entry(&cache.join(REFUSED))?;
        collect(store, &[])
    })();
    clear_work(&work);
    removed?;
    Ok(Outcome::Removed)
}

/// Where td-jail keeps an application's state, relative to its account's
/// home (td-jail's compiled `state-root`).
const STATE_ROOT: &str = ".td/app";

/// A directory of the account's own, reached without a link.
fn own_directory(path: &Path, uid: u32) -> Result<Option<fs::Metadata>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("stat {}: {error}", path.display())),
    };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(format!(
            "{} is not a directory of this account's",
            path.display()
        ));
    }
    Ok(Some(metadata))
}

/// `td-app delete-data`: the application's jail state, as its account,
/// only once it is removed and no instance runs. Every running launch holds
/// a generation's selection (§W.9's lease), so a generation left after
/// collection under the lock is a running instance. A launch that starts
/// meanwhile finds no `current` and waits for this lock before it fetches,
/// so it starts on the state as this leaves it.
fn delete_data(home: &Path, store: &Store, source: FlathubSource) -> Result<Outcome, String> {
    let cache = &store.root;
    // Never installed and no state: nothing to lock against.
    if fs::symlink_metadata(cache).is_err()
        && fs::symlink_metadata(home.join(STATE_ROOT).join(source.name)).is_err()
    {
        return Ok(Outcome::Deleted(false));
    }
    prepare_cache(store)?;
    let _lock = lock(cache)?;
    let work = fresh_work(cache)?;
    let _tracker = Tracker::new(&work, Stage::DeleteData);
    let deleted = (|| {
        for pointer in [CURRENT, PREVIOUS] {
            if fs::symlink_metadata(cache.join(pointer)).is_ok() {
                return Err(format!(
                    "{} is installed; remove it before deleting its data",
                    source.name
                ));
            }
        }
        collect(store, &[])?;
        if !list(&cache.join(GENERATIONS))?.is_empty() {
            return Err(format!(
                "{} is running; its data stays until every instance exits",
                source.name
            ));
        }
        delete_state(home, source.name)
    })();
    clear_work(&work);
    Ok(Outcome::Deleted(deleted?))
}

/// Rename the state aside, then remove it, so an interrupted deletion
/// never leaves half a profile for td-jail to launch on; the next one
/// finishes it. Nothing outside the state's own filesystem is touched.
fn delete_state(home: &Path, name: &str) -> Result<bool, String> {
    let uid = fs::metadata(home)
        .map_err(|error| format!("stat {}: {error}", home.display()))?
        .uid();
    let td = home.join(".td");
    let applications = home.join(STATE_ROOT);
    if own_directory(&td, uid)?.is_none() || own_directory(&applications, uid)?.is_none() {
        return Ok(false);
    }
    let doomed = applications.join(format!("{REMOVING}{name}"));
    if let Some(metadata) = own_directory(&doomed, uid)? {
        remove_tree_on(&doomed, Some(metadata.dev()))?;
    }
    let state = applications.join(name);
    let Some(metadata) = own_directory(&state, uid)? else {
        return Ok(false);
    };
    // Refuse before the state moves or a mode changes; a directory too
    // sealed to read here is checked again once it is aside.
    foreign_directory(&state, metadata.dev())?;
    fs::rename(&state, &doomed)
        .map_err(|error| format!("rename {} for removal: {error}", state.display()))?;
    sync_dir(&applications)?;
    remove_tree_on(&doomed, Some(metadata.dev()))?;
    Ok(true)
}

/// `td-app revalidate`: rule 5 again over the application deploy `current`
/// already has, fetching nothing, when what it was validated against
/// changed: what no commit carries (a system update's entry or loader
/// policy, `checks=`) or the runtime store's `current`, which every
/// application follows. It answers as `ready` does: the generation that
/// serves now, or `absent` with the reason recorded as the last refusal;
/// or `busy`, changing nothing, while an update or removal holds the
/// cache, which a launch does not wait for.
fn revalidate(
    admission: &Admission<'_>,
    store: &Store,
    view: &RuntimeView,
    source: FlathubSource,
    now: u64,
) -> Result<Outcome, String> {
    let cache = &store.root;
    if fs::symlink_metadata(cache).is_err() {
        return Ok(Outcome::Ready(None));
    }
    prepare_cache(store)?;
    let Some(_lock) = launch_lock(cache)? else {
        return Ok(Outcome::Busy);
    };
    revalidate_held(admission, store, view, source, now).map(Outcome::Ready)
}

/// How long a launch's revalidation waits behind another launch's, which
/// fetches nothing; and how long behind a writer that has not yet said
/// what it is doing.
const REVALIDATION_WAIT: Duration = Duration::from_secs(120);
const UNANNOUNCED_WAIT: Duration = Duration::from_secs(2);

/// The cache's lock for a launch, or `None` while a writer that fetches
/// or removes holds it: a launch never waits behind an update, whose
/// bound is an hour. It waits only behind another launch's revalidation,
/// within its own bound, and a writer's first moments.
fn launch_lock(cache: &Path) -> Result<Option<File>, String> {
    let (path, file) = lock_file(cache)?;
    let begin = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                forget_progress(cache);
                return Ok(Some(file));
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(format!("lock {}: {error}", path.display()))
            }
        }
        let waited = begin.elapsed();
        let stage = writer_progress(cache)?.map(|progress| progress.stage);
        let wait = match stage {
            // Released between the two looks: try again.
            None | Some(Stage::Revalidate) => waited < REVALIDATION_WAIT,
            Some(Stage::Busy) => waited < UNANNOUNCED_WAIT,
            Some(_) => false,
        };
        if !wait {
            return Ok(None);
        }
        std::thread::sleep(PROGRESS_TICK);
    }
}

/// `revalidate` under the cache's lock.
fn revalidate_held(
    admission: &Admission<'_>,
    store: &Store,
    view: &RuntimeView,
    source: FlathubSource,
    now: u64,
) -> Result<Option<u64>, String> {
    let cache = &store.root;
    let expected = expect(source)?;
    let Some((number, selection)) = read_pointer(cache, CURRENT)? else {
        return Ok(None);
    };
    let serving = runtime_serving(view, source.runtime);
    if serves(cache, &selection, source, &expected, serving.as_ref()) {
        return Ok(Some(number));
    }
    let work = fresh_work(cache)?;
    let _tracker = Tracker::new(&work, Stage::Revalidate);
    let revalidated = (|| {
        if selection.name != source.name
            || selection.application.exact_ref != source.application
            || selection.runtime.exact_ref != source.runtime.exact_ref
        {
            return Err(format!(
                "{}'s declaration names other refs; it needs an update",
                source.name
            ));
        }
        if !deployed(cache, &selection.application.commit) {
            return Err(format!(
                "the deploy of {}'s current generation is missing; it needs an update",
                source.name
            ));
        }
        let runtime = hold_runtime(view, source.runtime)?;
        (admission.validate)(
            &deploy_path(cache, &selection.application.commit),
            source.entry,
            &runtime.deploy,
            &expected.library_paths,
            &expected.optional_targets,
            expected.optional_links,
        )?;
        let next = Selection {
            checks: expected.checks.clone(),
            runtime: runtime.selection.runtime.clone(),
            ..selection.clone()
        };
        drop(runtime);
        // The stale copy is no rollback: `previous` stays what it was.
        let previous = read_pointer(cache, PREVIOUS)?.map(|(generation, _)| generation);
        let generation = publish(cache, &next.render(), previous)?;
        collect(store, &[])?;
        Ok(generation)
    })();
    clear_work(&work);
    match revalidated {
        Ok(generation) => {
            clear_refusal(cache)?;
            Ok(Some(generation))
        }
        Err(error) => {
            record_refusal(cache, now, &format!("offline revalidation: {error}"));
            Ok(None)
        }
    }
}

fn process_uid() -> Result<u32, String> {
    Ok(fs::metadata("/proc/self")
        .map_err(|error| format!("stat /proc/self: {error}"))?
        .uid())
}

fn home_owner(home: &Path) -> Result<u32, String> {
    Ok(fs::metadata(home)
        .map_err(|error| format!("stat {}: {error}", home.display()))?
        .uid())
}

/// The cache must belong to the account td-app runs as: as root it would
/// leave entries the account cannot remove and follow links the account
/// planted. The runtime account has no application cache.
fn require_account(home: &Path) -> Result<(), String> {
    application_account(process_uid()?, home, home_owner(home)?)
}

fn application_account(uid: u32, home: &Path, owner: u32) -> Result<(), String> {
    if uid == 0 {
        return Err("td-app runs as an application account, not root".into());
    }
    if uid == RUNTIME_UID {
        return Err(format!(
            "{RUNTIME_ACCOUNT} owns runtimes, not an application; use td-app runtime"
        ));
    }
    if owner != uid {
        return Err(format!(
            "HOME {} belongs to uid {owner}, not this account's {uid}",
            home.display()
        ));
    }
    Ok(())
}

/// The runtime verbs run only as the runtime account, on its own home,
/// which is the store's root.
fn runtime_account(uid: u32, home: &Path, owner: u32) -> Result<(), String> {
    if uid != RUNTIME_UID {
        return Err(format!(
            "td-app runtime runs as {RUNTIME_ACCOUNT} (uid {RUNTIME_UID}), not uid {uid}"
        ));
    }
    if home != Path::new(RUNTIME_STORE) {
        return Err(format!(
            "HOME {} is not the runtime store {RUNTIME_STORE}",
            home.display()
        ));
    }
    if owner != uid {
        return Err(format!(
            "HOME {} belongs to uid {owner}, not {RUNTIME_ACCOUNT}'s",
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
    feed_store: &Path,
    now: u64,
) -> Result<Vec<String>, String> {
    prepare_cache(&Store::application(cache.to_path_buf()))?;
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
        feed_store.join(crate::ostree::feed_graph_base("", commit).trim_start_matches('/'))
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
        crate::ostree::acquire(&spec, &objects, None)?;
        // Recorded before any of it is published, so no failure leaks it.
        if fs::symlink_metadata(store_graph(commit)).is_err()
            && owned.insert(candidate.admitted.commit.clone())
        {
            write_state(&exported_path, &render_commits(&owned))?;
        }
        crate::ostree::export_feed(&spec, &objects, feed_store, |_| Ok(()))?;
        let graph = crate::ostree::feed_graph_base("", commit);
        let graph = graph.trim_start_matches('/');
        publish_to_feed(
            &work,
            &candidate.commitmeta,
            &feed_store.join(commitmeta_url(graph, commit)?),
        )?;
        resolved.push((exact_ref, candidate.admitted.commit));
    }
    // The refs move only once every graph is in place, so the feed never
    // names a new application beside a runtime graph it still lacks.
    for (exact_ref, commit) in &resolved {
        publish_to_feed(
            &work,
            format!("{commit}\n").as_bytes(),
            &feed_store.join(FEED_REFS).join(exact_ref),
        )?;
    }
    remove_tree(&work)?;
    // An owned graph leaves the store once no ref file there names it, a
    // ref no longer declared included, so the feed never names a graph it
    // lacks.
    let named = feed_ref_commits(&feed_store.join(FEED_REFS))?;
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
        .flat_map(|source| [source.application, source.runtime.exact_ref])
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
    eprintln!(
        "usage: td-app update NAME --force | td-app ready NAME | td-app status NAME | \
         td-app remove NAME | td-app delete-data NAME | td-app revalidate NAME | \
         td-app runtime update RUNTIME --force | td-app runtime status RUNTIME | \
         td-app runtime remove RUNTIME"
    );
    std::process::exit(2);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verb {
    Update,
    Ready,
    Status,
    Remove,
    DeleteData,
    Revalidate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeVerb {
    Update,
    Status,
    Remove,
}

#[derive(Debug, Eq, PartialEq)]
enum Request<'a> {
    Application(Verb, &'a str),
    Runtime(RuntimeVerb, &'a str),
}

/// Every update is the owner's forced check; the unforced, throttled form
/// is gone with the check beside a launch.
fn parse_request(args: &[String]) -> Option<Request<'_>> {
    Some(match args.get(1..)? {
        [scope, verb, name, flag]
            if scope == "runtime" && verb == "update" && flag == "--force" =>
        {
            Request::Runtime(RuntimeVerb::Update, name)
        }
        [scope, verb, name] if scope == "runtime" => match verb.as_str() {
            "status" => Request::Runtime(RuntimeVerb::Status, name),
            "remove" => Request::Runtime(RuntimeVerb::Remove, name),
            _ => return None,
        },
        [verb, name, flag] if verb == "update" && flag == "--force" => {
            Request::Application(Verb::Update, name)
        }
        [verb, name] => Request::Application(
            match verb.as_str() {
                "ready" => Verb::Ready,
                "status" => Verb::Status,
                "remove" => Verb::Remove,
                "delete-data" => Verb::DeleteData,
                "revalidate" => Verb::Revalidate,
                _ => return None,
            },
            name,
        ),
        _ => return None,
    })
}

fn admission(feed: Option<&str>) -> Admission<'_> {
    Admission {
        repository: flathub_app::REPOSITORY,
        feed,
        verify: verify_flathub,
        validate: validate_dynamic,
    }
}

/// An application's verbs, as its account, on its cache and the runtime
/// store it reads.
fn run_application(home: &Path, verb: Verb, source: FlathubSource) -> Result<Outcome, String> {
    require_account(home)?;
    // Only an update fetches; nothing else asks for a feed.
    let feed = match verb {
        Verb::Update => crate::feed::provisioned_feed_base()?,
        _ => None,
    };
    let admission = admission(feed.as_deref());
    let cache = Store::application(home.join(CACHE_DIR));
    let view = RuntimeView {
        root: Path::new(RUNTIME_STORE).join(source.runtime.name),
        owner: RUNTIME_UID,
    };
    match verb {
        Verb::Update => update(&admission, &cache, &view, source, unix_now()?),
        Verb::Ready => ready(&cache, &view, source).map(Outcome::Ready),
        Verb::Status => {
            status(&cache, &view, source).map(|status| Outcome::Status(Box::new(status)))
        }
        Verb::Remove => remove(&cache),
        Verb::DeleteData => delete_data(home, &cache, source),
        Verb::Revalidate => revalidate(&admission, &cache, &view, source, unix_now()?),
    }
}

/// A runtime's verbs, as the runtime account, on its store.
fn run_runtime(home: &Path, verb: RuntimeVerb, runtime: FlathubRuntime) -> Result<Outcome, String> {
    runtime_account(process_uid()?, home, home_owner(home)?)?;
    let store = Store::runtime(home.join(runtime.name));
    match verb {
        RuntimeVerb::Update => {
            let feed = crate::feed::provisioned_feed_base()?;
            runtime_update(&admission(feed.as_deref()), &store, runtime, unix_now()?)
        }
        RuntimeVerb::Status => {
            runtime_status(&store, runtime).map(|status| Outcome::RuntimeStatus(Box::new(status)))
        }
        RuntimeVerb::Remove => remove(&store),
    }
}

fn account_home() -> Result<PathBuf, String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    if !home.is_absolute() {
        return Err(format!("HOME {} is not absolute", home.display()));
    }
    Ok(home)
}

pub fn run(args: &[String]) {
    let Some(request) = parse_request(args) else {
        usage();
    };
    let (name, result) = match request {
        Request::Application(verb, name) => {
            let Some(source) = flathub_app::flathub_source(name) else {
                eprintln!("td-app: {name} is not a Flathub application");
                std::process::exit(2);
            };
            (
                name,
                account_home().and_then(|home| run_application(&home, verb, source)),
            )
        }
        Request::Runtime(verb, name) => {
            let Some(runtime) = flathub_app::flathub_runtime(name) else {
                eprintln!("td-app: {name} is not a declared Flathub runtime");
                std::process::exit(2);
            };
            (
                name,
                account_home().and_then(|home| run_runtime(&home, verb, runtime)),
            )
        }
    };
    match result {
        // td-jail's answer: one line, a generation, `absent` or `busy`.
        Ok(Outcome::Ready(Some(generation))) => println!("{generation}"),
        Ok(Outcome::Ready(None)) => println!("absent"),
        // A launch's revalidation found an update or removal running.
        Ok(Outcome::Busy) => println!("busy"),
        // The status record, which the root operation relays.
        Ok(Outcome::Status(status)) => print!("{}", status.render()),
        Ok(Outcome::RuntimeStatus(status)) => print!("{}", status.render()),
        Ok(Outcome::Removed) => println!("td-app: {name} is removed"),
        Ok(Outcome::Deleted(true)) => println!("td-app: {name}'s data is deleted"),
        Ok(Outcome::Deleted(false)) => println!("td-app: {name} has no data to delete"),
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

    /// `revalidate`'s answer to a launch with no writer in its way.
    fn revalidate(
        admission: &Admission<'_>,
        store: &Store,
        view: &RuntimeView,
        source: FlathubSource,
        now: u64,
    ) -> Result<Option<u64>, String> {
        match super::revalidate(admission, store, view, source, now)? {
            Outcome::Ready(answer) => Ok(answer),
            other => Err(format!("{other:?}")),
        }
    }

    const APP: &str = "app/org.example.App/x86_64/stable";
    const RUNTIME: &str = "runtime/org.example.Platform/x86_64/1";
    const APP_METADATA: &str =
        "[Application]\nname=org.example.App\nruntime=org.example.Platform/x86_64/1\n";
    const NOW: u64 = 1_800_000_000;

    type Repository = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

    thread_local! {
        static VALIDATED: RefCell<Vec<(PathBuf, PathBuf)>> = const { RefCell::new(Vec::new()) };
        static REFUSE: RefCell<bool> = const { RefCell::new(false) };
        /// A runtime store whose `current` the validator finds held, or
        /// not, by a shared lease: what it saw is pushed to `LEASED`.
        static PROBE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static LEASED: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
    }

    fn source() -> FlathubSource {
        FlathubSource {
            name: "firefox",
            application: APP,
            // Synthetic refs, but the real runtime name: `expect` looks the
            // dynamic-library policy up by it.
            runtime: flathub_app::FlathubRuntime {
                name: "freedesktop-platform-25-08",
                exact_ref: RUNTIME,
            },
            entry: "/app/bin/firefox",
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
        if let Some(store) = PROBE.with(|probe| probe.borrow().clone()) {
            let generation = pointer(&store, CURRENT).unwrap().unwrap();
            let probe = File::open(selection_path(&store, generation)).unwrap();
            let held = matches!(probe.try_lock(), Err(TryLockError::WouldBlock));
            LEASED.with(|leased| leased.borrow_mut().push(held));
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
        /// The runtime account's home: the store root.
        runtimes: PathBuf,
        repository: Repository,
        url: String,
        feed: Option<String>,
    }

    fn own_uid() -> u32 {
        process_uid().unwrap()
    }

    impl Fixture {
        fn new(label: &str) -> Fixture {
            let root = crate::test_temp::TempDir::new(&format!("td-app-{label}")).unwrap();
            let home = root.join("home");
            fs::create_dir(&home).unwrap();
            let runtimes = root.join("runtimes");
            fs::create_dir(&runtimes).unwrap();
            let repository = Repository::default();
            let url = serve(repository.clone());
            Fixture {
                _root: root,
                home,
                runtimes,
                repository,
                url,
                feed: None,
            }
        }

        fn cache(&self) -> PathBuf {
            self.home.join(CACHE_DIR)
        }

        fn store(&self) -> Store {
            Store::application(self.cache())
        }

        /// The runtime store `source()` names.
        fn runtime_cache(&self) -> PathBuf {
            self.runtimes.join(source().runtime.name)
        }

        fn runtime_store(&self) -> Store {
            Store::runtime(self.runtime_cache())
        }

        /// The store as the application's verbs see it; the test's own
        /// uid stands for the runtime account's.
        fn view(&self) -> RuntimeView {
            RuntimeView {
                root: self.runtime_cache(),
                owner: own_uid(),
            }
        }

        /// What root's install and update run: the runtime's check as the
        /// runtime account, then the application's.
        fn update(&self, source: FlathubSource, now: u64) -> Result<Outcome, String> {
            self.update_runtime(source.runtime, now)?;
            self.update_application(source, now)
        }

        fn update_runtime(&self, runtime: FlathubRuntime, now: u64) -> Result<Outcome, String> {
            runtime_update(&self.admission(), &self.runtime_store(), runtime, now)
        }

        fn update_application(&self, source: FlathubSource, now: u64) -> Result<Outcome, String> {
            update(&self.admission(), &self.store(), &self.view(), source, now)
        }

        fn current(&self) -> Option<(u64, Selection)> {
            read_pointer(&self.cache(), CURRENT).unwrap()
        }

        fn runtime_current(&self) -> Option<(u64, RuntimeSelection)> {
            read_runtime_pointer(&self.runtime_cache(), CURRENT).unwrap()
        }

        fn names(&self, area: &str) -> Vec<String> {
            names_in(&self.cache().join(area))
        }

        fn runtime_names(&self, area: &str) -> Vec<String> {
            names_in(&self.runtime_cache().join(area))
        }
    }

    fn names_in(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
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
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(1)));

        let (generation, selection) = fixture.current().unwrap();
        assert_eq!(generation, 1);
        assert_eq!(selection.runtime.commit, runtime);
        assert_eq!(selection.runtime.timestamp, 10);
        assert_eq!(selection.application.commit, app);
        assert!(read_pointer(&fixture.cache(), PREVIOUS).unwrap().is_none());
        // The application's tree is its cache's; the runtime's is the
        // runtime store's alone.
        let deploy = fixture.cache().join(DEPLOY);
        let runtime_deploy = fixture.runtime_cache().join(DEPLOY);
        assert_eq!(
            fs::read(deploy.join(&app).join("files/payload")).unwrap(),
            b"app"
        );
        assert_eq!(fixture.names(DEPLOY), vec![app.clone()]);
        assert_eq!(fixture.names(OBJECTS), vec![app.clone()]);
        assert_eq!(
            fs::read(runtime_deploy.join(&runtime).join("files/payload")).unwrap(),
            b"usr"
        );
        let (number, held) = fixture.runtime_current().unwrap();
        assert_eq!((number, held.runtime.commit), (1, runtime.clone()));
        VALIDATED.with(|seen| {
            assert_eq!(
                *seen.borrow(),
                vec![(deploy.join(&app), runtime_deploy.join(&runtime))]
            );
        });
        for (cache, exact_ref) in [(fixture.cache(), APP), (fixture.runtime_cache(), RUNTIME)] {
            let admitted = fs::read_to_string(cache.join(ADMITTED)).unwrap();
            let rows = flathub_app::parse_admitted(&admitted).unwrap();
            let [row] = rows.as_slice() else {
                panic!("{rows:?}")
            };
            assert_eq!(row.exact_ref, exact_ref);
            assert_eq!(
                fs::read_to_string(cache.join(CHECKED)).unwrap(),
                format!("{NOW}\n")
            );
            assert!(!cache.join(WORK).exists());
        }
        assert_eq!(mode(&fixture.cache()), 0o700);
    }

    /// The runtime store is the runtime account's and every account reads
    /// it: the root, `deploy/` and `generations/` are 0755 whatever the
    /// umask or an earlier run left, and what only the writer needs stays
    /// private.
    #[test]
    fn the_runtime_store_is_readable_and_its_writer_state_private() {
        let fixture = Fixture::new("runtime-modes");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let cache = fixture.runtime_cache();
        // A store an earlier run or another umask left closed.
        prepare_cache(&fixture.runtime_store()).unwrap();
        for area in ["", DEPLOY, GENERATIONS] {
            fs::set_permissions(cache.join(area), fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            fixture.update_runtime(source().runtime, NOW),
            Ok(Outcome::Selected(1))
        );
        for path in [
            cache.clone(),
            cache.join(DEPLOY),
            cache.join(GENERATIONS),
            cache.join(GENERATIONS).join("1"),
            cache.join(DEPLOY).join(&runtime),
            cache.join(DEPLOY).join(&runtime).join(FILES),
        ] {
            assert_eq!(mode(&path), 0o755, "{}", path.display());
        }
        assert_eq!(mode(&selection_path(&cache, 1)), 0o644);
        assert_eq!(mode(&cache.join(OBJECTS)), 0o700);
        assert_eq!(mode(&cache.join(LOCK)), 0o600);
        for state in [ADMITTED, CHECKED] {
            assert_eq!(mode(&cache.join(state)) & 0o077, 0, "{state}");
        }
        let (_, selection) = fixture.runtime_current().unwrap();
        assert_eq!(
            selection,
            RuntimeSelection {
                name: source().runtime.name.into(),
                runtime: Admitted {
                    exact_ref: RUNTIME.into(),
                    commit: runtime.clone(),
                    timestamp: 10,
                },
            }
        );
        // An unchanged check publishes nothing, and its status reads it.
        assert_eq!(
            fixture.update_runtime(source().runtime, NOW + 1),
            Ok(Outcome::Unchanged)
        );
        let status = runtime_status(&fixture.runtime_store(), source().runtime).unwrap();
        assert_eq!(RuntimeStatus::parse(&status.render()).unwrap(), status);
        let current = status.current.unwrap();
        assert_eq!(current.generation, 1);
        assert!(current.serves);
        assert_eq!(current.runtime.bytes, Some(3));
        assert_eq!(status.previous, None);
        assert_eq!(status.checked, Some(NOW + 1));
        // Its rules 1–3 are the store's own.
        publish_commit(&fixture.repository, RUNTIME, b"old", 9, None, b"signed");
        let error = fixture
            .update_runtime(source().runtime, NOW + 2)
            .unwrap_err();
        assert!(error.contains("does not supersede"), "{error}");
        let refused = runtime_status(&fixture.runtime_store(), source().runtime)
            .unwrap()
            .refused
            .unwrap();
        assert!(refused.reason.contains("does not supersede"), "{refused:?}");
    }

    /// A runtime's collection keeps `current`, `previous` and a generation
    /// a launch or an application's validation holds, and `remove` clears
    /// the pointers and collects what no one holds.
    #[test]
    fn runtime_collection_spares_held_generations_and_remove_clears_the_store() {
        let fixture = Fixture::new("runtime-held");
        let first = publish_commit(&fixture.repository, RUNTIME, b"one", 10, None, b"signed");
        fixture.update_runtime(source().runtime, NOW).unwrap();
        let launch = File::open(selection_path(&fixture.runtime_cache(), 1)).unwrap();
        launch.lock_shared().unwrap();
        publish_commit(&fixture.repository, RUNTIME, b"two", 20, None, b"signed");
        fixture.update_runtime(source().runtime, NOW).unwrap();
        let third = publish_commit(&fixture.repository, RUNTIME, b"three", 30, None, b"signed");
        assert_eq!(
            fixture.update_runtime(source().runtime, NOW),
            Ok(Outcome::Selected(3))
        );
        assert_eq!(fixture.runtime_names(GENERATIONS), vec!["1", "2", "3"]);
        assert!(deployed(&fixture.runtime_cache(), &first));
        drop(launch);
        publish_commit(&fixture.repository, RUNTIME, b"four", 40, None, b"signed");
        fixture.update_runtime(source().runtime, NOW).unwrap();
        assert_eq!(fixture.runtime_names(GENERATIONS), vec!["3", "4"]);
        assert!(!deployed(&fixture.runtime_cache(), &first));
        assert!(deployed(&fixture.runtime_cache(), &third));

        let launch = File::open(selection_path(&fixture.runtime_cache(), 4)).unwrap();
        launch.lock_shared().unwrap();
        assert_eq!(remove(&fixture.runtime_store()), Ok(Outcome::Removed));
        assert!(fixture.runtime_current().is_none());
        assert_eq!(fixture.runtime_names(GENERATIONS), vec!["4"]);
        drop(launch);
        remove(&fixture.runtime_store()).unwrap();
        assert!(fixture.runtime_names(GENERATIONS).is_empty());
        assert!(fixture.runtime_names(DEPLOY).is_empty());
        assert!(fixture.runtime_cache().join(ADMITTED).exists());
    }

    /// A launch's revalidation does not wait behind a writer that fetches
    /// or removes: it answers `busy`, which td-jail refuses with. It waits
    /// only behind another launch's revalidation, and a writer that has
    /// not yet said what it does only briefly.
    #[test]
    fn a_launch_does_not_wait_behind_an_update() {
        let fixture = Fixture::new("busy");
        let admission = fixture.admission();
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        let cache = fixture.cache();
        let announce = |stage| {
            fs::create_dir_all(cache.join(WORK)).unwrap();
            let line = Progress::idle(stage).render();
            fs::write(cache.join(WORK).join(PROGRESS), format!("{line}\n")).unwrap();
        };
        let ask =
            || super::revalidate(&admission, &fixture.store(), &fixture.view(), source(), NOW);
        for stage in [Stage::Resolve, Stage::Fetch, Stage::Validate, Stage::Remove] {
            let writer = lock(&cache).unwrap();
            announce(stage);
            let begin = Instant::now();
            assert_eq!(ask(), Ok(Outcome::Busy), "{stage:?}");
            assert!(begin.elapsed() < Duration::from_secs(1));
            drop(writer);
        }
        // A writer that never says what it does is waited for briefly.
        let writer = lock(&cache).unwrap();
        let begin = Instant::now();
        assert_eq!(ask(), Ok(Outcome::Busy));
        assert!(begin.elapsed() >= UNANNOUNCED_WAIT);
        drop(writer);
        // Another launch's revalidation is waited for.
        let writer = lock(&cache).unwrap();
        announce(Stage::Revalidate);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(writer);
        });
        assert_eq!(ask(), Ok(Outcome::Ready(Some(1))));
        release.join().unwrap();
        assert_eq!(ask(), Ok(Outcome::Ready(Some(1))));
    }

    /// A generation or deploy leaves its name before removal opens its
    /// directories to the runtime account alone: another account looking
    /// it up meanwhile finds it absent, as collected, never a directory it
    /// may not enter. What a cut-short removal left aside goes next time.
    #[test]
    fn a_collected_tree_leaves_its_name_before_it_is_opened() {
        let fixture = Fixture::new("aside");
        publish_commit(&fixture.repository, RUNTIME, b"one", 10, None, b"signed");
        fixture.update_runtime(source().runtime, NOW).unwrap();
        let store = fixture.runtime_cache();
        let aside = set_aside(&store, GENERATIONS, "1").unwrap();
        assert_eq!(aside, store.join(GENERATIONS).join(".removing-1"));
        assert_eq!(
            File::open(selection_path(&store, 1)).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        // Still as readable as it was: nothing was opened yet.
        assert_eq!(mode(&aside), 0o755);
        // The pointer's generation is gone, so a holder reads it as
        // collected, and the store holds no runtime.
        let Err(error) = hold_runtime(&fixture.view(), source().runtime) else {
            panic!("held a generation set aside");
        };
        assert!(error.contains("was collected each time"), "{error}");
        // Set aside again over a removal cut short: the newer one replaces it.
        fs::create_dir(store.join(GENERATIONS).join("1")).unwrap();
        set_aside(&store, GENERATIONS, "1").unwrap();
        assert!(!aside.join(SELECTION).exists());
        remove_entry(&store.join(CURRENT)).unwrap();
        collect(&fixture.runtime_store(), &[]).unwrap();
        assert!(fixture.runtime_names(GENERATIONS).is_empty());
        assert!(fixture.runtime_names(DEPLOY).is_empty());
    }

    /// An application follows the runtime store's `current` (D2 in
    /// WORKLOADS.md's landing): once the runtime moves, the generation no
    /// longer serves, and an offline revalidation against the new
    /// `current`, holding it while it validates, publishes the same
    /// application on it; a refusal is recorded and the launch has nothing.
    #[test]
    fn an_application_follows_the_runtime_current_and_revalidates_offline() {
        let fixture = Fixture::new("follows");
        let admission = fixture.admission();
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        assert_eq!(fixture.ready(source()), Ok(Some(1)));

        let moved = publish_commit(&fixture.repository, RUNTIME, b"usr2", 30, None, b"signed");
        fixture.update_runtime(source().runtime, NOW + 1).unwrap();
        assert_eq!(fixture.ready(source()), Ok(None));
        assert!(!fixture.status(source()).current.unwrap().serves);

        fixture.repository.lock().unwrap().clear();
        REFUSE.with(|refuse| *refuse.borrow_mut() = true);
        assert_eq!(
            revalidate(
                &admission,
                &fixture.store(),
                &fixture.view(),
                source(),
                NOW + 2
            ),
            Ok(None)
        );
        REFUSE.with(|refuse| *refuse.borrow_mut() = false);
        assert!(fixture
            .status(source())
            .refused
            .unwrap()
            .reason
            .contains("fixture validator refuses"));
        assert_eq!(fixture.current().unwrap().0, 1);

        VALIDATED.with(|seen| seen.borrow_mut().clear());
        PROBE.with(|probe| *probe.borrow_mut() = Some(fixture.runtime_cache()));
        LEASED.with(|leased| leased.borrow_mut().clear());
        assert_eq!(
            revalidate(
                &admission,
                &fixture.store(),
                &fixture.view(),
                source(),
                NOW + 3
            ),
            Ok(Some(2))
        );
        PROBE.with(|probe| *probe.borrow_mut() = None);
        LEASED.with(|leased| assert_eq!(*leased.borrow(), vec![true]));
        VALIDATED.with(|seen| {
            assert_eq!(
                *seen.borrow(),
                vec![(
                    fixture.cache().join(DEPLOY).join(&app),
                    fixture.runtime_cache().join(DEPLOY).join(&moved)
                )]
            );
        });
        let (_, selection) = fixture.current().unwrap();
        assert_eq!(selection.application.commit, app);
        assert_eq!(selection.runtime.commit, moved);
        assert_eq!(fixture.ready(source()), Ok(Some(2)));
        assert_eq!(fixture.status(source()).refused, None);
        // Nothing held the old runtime generation: the runtime's next
        // collection removes it.
    }

    /// The same move found by the owner's update rather than a launch: the
    /// application's commit is unchanged, so it is validated again against
    /// the new `current` and published as a new generation.
    #[test]
    fn an_update_after_the_runtime_moved_publishes_on_the_new_runtime() {
        let fixture = Fixture::new("follows-update");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        let moved = publish_commit(&fixture.repository, RUNTIME, b"usr2", 30, None, b"signed");
        assert_eq!(fixture.update(source(), NOW + 1), Ok(Outcome::Selected(2)));
        assert_eq!(fixture.current().unwrap().1.runtime.commit, moved);
        assert_eq!(fixture.update(source(), NOW + 2), Ok(Outcome::Unchanged));
    }

    /// With no runtime installed, an application neither installs nor
    /// revalidates: a machine whose cache predates the runtime store
    /// launches nothing until its owner updates it.
    #[test]
    fn an_application_without_its_runtime_installed_is_refused() {
        let fixture = Fixture::new("no-runtime");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        let error = fixture.update_application(source(), NOW).unwrap_err();
        assert!(error.contains("is not installed"), "{error}");
        assert!(fixture.current().is_none());
        fixture.update(source(), NOW).unwrap();
        remove(&fixture.runtime_store()).unwrap();
        assert_eq!(fixture.ready(source()), Ok(None));
        assert_eq!(
            revalidate(
                &fixture.admission(),
                &fixture.store(),
                &fixture.view(),
                source(),
                NOW
            ),
            Ok(None)
        );
        assert!(fixture
            .status(source())
            .refused
            .unwrap()
            .reason
            .contains("freedesktop-platform-25-08 is not installed"));
        // A store whose `current` is not the runtime account's is refused.
        fixture.update_runtime(source().runtime, NOW).unwrap();
        let foreign = RuntimeView {
            root: fixture.runtime_cache(),
            owner: own_uid().wrapping_add(1),
        };
        assert!(hold_runtime(&foreign, source().runtime)
            .err()
            .unwrap()
            .contains("only the runtime account writes"));
        assert!(hold_runtime(&fixture.view(), source().runtime).is_ok());
    }

    /// The migration: a cache from before the runtime store held its
    /// runtime's deploy and graph beside the application's. Its next write
    /// collects them, keeping only the application's commit.
    #[test]
    fn an_application_collection_removes_a_runtime_deploy_left_in_its_cache() {
        let fixture = Fixture::new("legacy");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        for area in [DEPLOY, OBJECTS] {
            fs::create_dir_all(fixture.cache().join(area).join(&runtime).join(FILES)).unwrap();
        }
        assert_eq!(fixture.update(source(), NOW + 1), Ok(Outcome::Unchanged));
        assert_eq!(fixture.names(DEPLOY), vec![app.clone()]);
        assert_eq!(fixture.names(OBJECTS), vec![app]);
        assert!(deployed(&fixture.runtime_cache(), &runtime));
    }

    #[test]
    fn requests_name_an_application_or_a_runtime_and_each_its_own_account() {
        let args = |words: &[&str]| -> Vec<String> {
            std::iter::once("td-app")
                .chain(words.iter().copied())
                .map(str::to_string)
                .collect()
        };
        for (words, expected) in [
            (
                vec!["update", "firefox", "--force"],
                Request::Application(Verb::Update, "firefox"),
            ),
            (
                vec!["ready", "firefox"],
                Request::Application(Verb::Ready, "firefox"),
            ),
            (
                vec!["runtime", "update", "platform", "--force"],
                Request::Runtime(RuntimeVerb::Update, "platform"),
            ),
            (
                vec!["runtime", "status", "platform"],
                Request::Runtime(RuntimeVerb::Status, "platform"),
            ),
            (
                vec!["runtime", "remove", "platform"],
                Request::Runtime(RuntimeVerb::Remove, "platform"),
            ),
        ] {
            assert_eq!(parse_request(&args(&words)), Some(expected), "{words:?}");
        }
        for words in [
            vec!["update", "firefox"],
            vec!["runtime", "update", "platform"],
            vec!["runtime", "ready", "platform"],
            vec!["runtime", "revalidate", "platform"],
            vec!["runtime", "delete-data", "platform"],
            vec!["runtime", "status"],
            vec!["runtime", "status", "platform", "extra"],
        ] {
            assert_eq!(parse_request(&args(&words)), None, "{words:?}");
        }

        let store = Path::new(RUNTIME_STORE);
        let home = Path::new("/var/lib/td/applications/65536");
        runtime_account(RUNTIME_UID, store, RUNTIME_UID).unwrap();
        for (uid, home, owner) in [
            (65536, store, RUNTIME_UID),
            (0, store, 0),
            (
                RUNTIME_UID,
                Path::new("/var/lib/td/runtimes/x"),
                RUNTIME_UID,
            ),
            (RUNTIME_UID, store, 0),
        ] {
            assert!(runtime_account(uid, home, owner).is_err(), "{uid} {home:?}");
        }
        application_account(65536, home, 65536).unwrap();
        for (uid, owner) in [(0, 0), (RUNTIME_UID, RUNTIME_UID), (65536, 65537)] {
            assert!(application_account(uid, home, owner).is_err(), "{uid}");
        }
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
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(1)));
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
        let error = fixture.update(source(), NOW).unwrap_err();
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
        assert!(fixture.update(source(), NOW).is_err());
        assert!(fixture.current().is_none());
        rewrite_feed_file(&object, Some(&lost));
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(1)));

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
        let error = fixture.update(source(), NOW).unwrap_err();
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
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(1)));
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
    fn every_update_asks_and_an_unchanged_one_publishes_nothing() {
        let fixture = Fixture::new("asks");
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        fixture.update(source(), NOW).unwrap();

        // An empty repository shows that a check a second later still
        // asks: there is no throttle, and its failure keeps `current` and
        // `checked`.
        fixture.repository.lock().unwrap().clear();
        assert!(fixture.update(source(), NOW + 1).is_err());
        assert_eq!(fixture.current().unwrap().0, 1);
        assert_eq!(
            fs::read_to_string(fixture.cache().join(CHECKED)).unwrap(),
            format!("{NOW}\n")
        );

        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        publish_commit(
            &fixture.repository,
            APP,
            b"app",
            20,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(fixture.update(source(), NOW + 2), Ok(Outcome::Unchanged));
        assert_eq!(
            fs::read_to_string(fixture.cache().join(CHECKED)).unwrap(),
            format!("{}\n", NOW + 2)
        );
        // A clock that went backwards asks too.
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Unchanged));
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
        fixture.update(source(), NOW).unwrap();
        let second = publish_commit(
            &fixture.repository,
            APP,
            b"two",
            30,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(2)));
        assert_eq!(
            read_pointer(&fixture.cache(), PREVIOUS).unwrap().unwrap().0,
            1
        );
        assert_eq!(
            fixture.names(DEPLOY),
            sorted(vec![first.clone(), second.clone()])
        );

        let third = publish_commit(
            &fixture.repository,
            APP,
            b"three",
            40,
            Some(APP_METADATA),
            b"signed",
        );
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(3)));
        assert_eq!(fixture.names(GENERATIONS), vec!["2", "3"]);
        let kept = sorted(vec![second, third]);
        assert_eq!(fixture.names(DEPLOY), kept);
        assert_eq!(fixture.names(OBJECTS), kept);
        // The runtime never moved: one generation, one deploy.
        assert_eq!(fixture.runtime_names(GENERATIONS), vec!["1"]);
        assert_eq!(fixture.runtime_names(DEPLOY), vec![runtime]);
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
        fixture.update(source(), NOW).unwrap();

        let refused = |expected: &str| {
            let error = fixture.update(source(), NOW).unwrap_err();
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
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(2)));
    }

    #[test]
    fn pointers_name_only_a_generation() {
        let fixture = Fixture::new("pointers");
        prepare_cache(&fixture.store()).unwrap();
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
        fixture.update(source(), NOW).unwrap();
        let objects = fixture.names(OBJECTS);
        let deploys = fixture.names(DEPLOY);
        let runtime_objects = fixture.runtime_names(OBJECTS);
        let runtime_deploys = fixture.runtime_names(DEPLOY);
        let refuse = |expected: &str| {
            let error = fixture.update(source(), NOW).unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert_eq!(fixture.names(OBJECTS), objects);
            assert_eq!(fixture.names(DEPLOY), deploys);
            assert_eq!(fixture.runtime_names(OBJECTS), runtime_objects);
            assert_eq!(fixture.runtime_names(DEPLOY), runtime_deploys);
            assert_eq!(fixture.current().unwrap().0, 1);
            assert_eq!(fixture.runtime_current().unwrap().0, 1);
        };
        // Rule 2: the app ref names a validly signed commit of another ref.
        fixture.repository.lock().unwrap().insert(
            format!("/refs/heads/{APP}"),
            format!("{runtime}\n").into_bytes(),
        );
        refuse("does not match exact ref");
        // Rule 3 on the runtime ref, refused in the runtime's store.
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
        fixture.update(source(), NOW).unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(), NOW).unwrap();
        fs::remove_file(fixture.cache().join(ADMITTED)).unwrap();
        // Generation 1's commit, still validly signed, served again.
        app_commit(&fixture, b"one", 20);
        let error = fixture.update(source(), NOW).unwrap_err();
        assert!(error.contains("does not supersede"), "{error}");
        // An unchanged check rewrites the floor it lost.
        app_commit(&fixture, b"two", 30);
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Unchanged));
        let admitted = fs::read_to_string(fixture.cache().join(ADMITTED)).unwrap();
        assert_eq!(flathub_app::parse_admitted(&admitted).unwrap().len(), 1);
    }

    #[test]
    fn a_changed_declaration_or_a_missing_deploy_is_checked_again() {
        let fixture = Fixture::new("declaration");
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let app = app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();

        // Another entry is another rule-5 input: the same commits are
        // validated again and selected as a new generation.
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source()
        };
        assert_eq!(fixture.update(moved, NOW + 1), Ok(Outcome::Selected(2)));
        let previous = read_pointer(&fixture.cache(), PREVIOUS).unwrap().unwrap();
        assert_ne!(fixture.current().unwrap().1.checks, previous.1.checks);
        assert_eq!(fixture.update(moved, NOW + 2), Ok(Outcome::Unchanged));

        // A deploy current names has gone, the application's or the
        // runtime's: each check restores its own in place, keeping
        // generation 1 as the rollback.
        remove_tree(&fixture.cache().join(DEPLOY).join(&app)).unwrap();
        remove_tree(&fixture.runtime_cache().join(DEPLOY).join(&runtime)).unwrap();
        assert_eq!(fixture.update(moved, NOW + 3), Ok(Outcome::Unchanged));
        assert!(deployed(&fixture.cache(), &app));
        assert!(deployed(&fixture.runtime_cache(), &runtime));
        assert_eq!(fixture.current().unwrap().0, 2);
        assert_eq!(fixture.runtime_current().unwrap().0, 1);
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
        prepare_cache(&fixture.store()).unwrap();
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

        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(10)));
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
        fixture.update(source(), NOW).unwrap();
        REFUSE.with(|refuse| *refuse.borrow_mut() = true);
        let first = app_commit(&fixture, b"first", 30);
        assert!(fixture.update(source(), NOW + 1).is_err());
        assert!(deployed(&fixture.cache(), &first));
        let second = app_commit(&fixture, b"second", 40);
        assert!(fixture.update(source(), NOW + 2).is_err());
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
        prepare_cache(&fixture.store()).unwrap();
        fs::create_dir(cache.join(ADMITTED)).unwrap();
        let error = fixture.update(source(), NOW).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        fs::remove_dir(cache.join(ADMITTED)).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
        fixture.update(source(), NOW).unwrap();
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
        require_account(&fixture.home).unwrap();
    }

    impl Fixture {
        fn ready(&self, source: FlathubSource) -> Result<Option<u64>, String> {
            ready(&self.store(), &self.view(), source)
        }
    }

    #[test]
    fn ready_names_current_only_while_it_serves_the_declaration() {
        let fixture = Fixture::new("ready");
        assert_eq!(fixture.ready(source()), Ok(None));
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        assert_eq!(fixture.ready(source()), Ok(Some(1)));
        // Another entry is another declaration the generation was not
        // validated for.
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source()
        };
        assert_eq!(fixture.ready(moved), Ok(None));
        // A runtime store whose `current` lost its deploy serves nothing.
        remove_tree(&fixture.runtime_cache().join(DEPLOY).join(&runtime)).unwrap();
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
        fixture.update(source(), NOW).unwrap();
        let selection = fixture.cache().join(GENERATIONS).join("1").join(SELECTION);
        let launch = File::open(&selection).unwrap();
        launch.lock_shared().unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(), NOW).unwrap();
        app_commit(&fixture, b"three", 40);
        fixture.update(source(), NOW).unwrap();
        // Neither current nor previous, and still running.
        assert_eq!(fixture.names(GENERATIONS), vec!["1", "2", "3"]);
        assert!(deployed(&fixture.cache(), &first));
        drop(launch);
        app_commit(&fixture, b"four", 50);
        fixture.update(source(), NOW).unwrap();
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
        fixture.update(source(), NOW).unwrap();
        app_commit(&fixture, b"two", 30);
        fixture.update(source(), NOW).unwrap();
        let selection = fixture.cache().join(GENERATIONS).join("1").join(SELECTION);
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o000)).unwrap();
        app_commit(&fixture, b"three", 40);
        let error = fixture.update(source(), NOW).unwrap_err();
        assert!(error.contains("open"), "{error}");
        assert!(deployed(&fixture.cache(), &first));
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o644)).unwrap();

        let stale = fixture
            .cache()
            .join(DEPLOY)
            .join(format!("{REMOVING}{first}"));
        fs::create_dir_all(stale.join(FILES)).unwrap();
        fixture.update(source(), NOW).unwrap();
        assert!(!stale.exists());
        assert!(!deployed(&fixture.cache(), &first));
        assert!(!fixture
            .names(DEPLOY)
            .iter()
            .any(|name| name.starts_with(REMOVING)));
    }

    impl Fixture {
        fn admission(&self) -> Admission<'_> {
            Admission {
                repository: &self.url,
                feed: self.feed.as_deref(),
                verify: verify_fixture,
                validate: validate_fixture,
            }
        }

        fn status(&self, source: FlathubSource) -> CacheStatus {
            let status = status(&self.store(), &self.view(), source).unwrap();
            assert_eq!(status.name, "firefox");
            // What root relays is the canonical rendering.
            assert_eq!(Status::parse(&status.render()).unwrap(), status);
            status.cache.unwrap()
        }

        fn state(&self) -> PathBuf {
            self.home.join(STATE_ROOT).join("firefox")
        }

        fn hold(&self, generation: u64) -> File {
            let launch = File::open(
                self.cache()
                    .join(GENERATIONS)
                    .join(generation.to_string())
                    .join(SELECTION),
            )
            .unwrap();
            launch.lock_shared().unwrap();
            launch
        }
    }

    #[test]
    fn status_reads_the_cache_without_writing_or_locking_it() {
        let fixture = Fixture::new("status");
        let empty = fixture.status(source());
        assert_eq!(
            empty,
            CacheStatus {
                current: None,
                previous: None,
                checked: None,
                refused: None,
                progress: None,
            }
        );
        assert!(!fixture.cache().exists());

        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr!", 10, None, b"signed");
        let app = app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        let installed = fixture.status(source());
        let current = installed.current.unwrap();
        assert_eq!(current.generation, 1);
        assert!(current.serves);
        assert_eq!(current.application.row.commit, app);
        assert_eq!(current.application.bytes, Some(3));
        assert_eq!(current.runtime.row.commit, runtime);
        assert_eq!(current.runtime.bytes, Some(4));
        assert_eq!(installed.previous, None);
        assert_eq!(installed.checked, Some(NOW));
        assert_eq!(installed.refused, None);
        assert_eq!(installed.progress, None);

        // A check that does not complete is the last refusal until one does.
        app_commit(&fixture, b"older", 19);
        assert!(fixture.update(source(), NOW + 5).is_err());
        let refused = fixture.status(source()).refused.unwrap();
        assert_eq!(refused.time, NOW + 5);
        assert!(refused.reason.contains("does not supersede"), "{refused:?}");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW + 6).unwrap();
        assert_eq!(fixture.status(source()).refused, None);

        // A changed declaration reads as installed but stale, and a missing
        // deploy has no size.
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source()
        };
        assert!(!fixture.status(moved).current.unwrap().serves);
        // The runtime's size is its store's deploy, which it no longer has.
        remove_tree(&fixture.runtime_cache().join(DEPLOY).join(&runtime)).unwrap();
        let damaged = fixture.status(source()).current.unwrap();
        assert!(!damaged.serves);
        assert_eq!(damaged.runtime.bytes, None);
        assert_eq!(damaged.application.bytes, Some(3));
    }

    /// Progress is what the lock's holder last recorded, and only while it
    /// holds the lock: a record a killed check left is no progress.
    #[test]
    fn progress_is_read_only_while_a_writer_holds_the_lock() {
        let fixture = Fixture::new("progress");
        prepare_cache(&fixture.store()).unwrap();
        let writer = lock(&fixture.cache()).unwrap();
        assert_eq!(
            fixture.status(source()).progress,
            Some(Progress::idle(Stage::Busy))
        );
        let work = fresh_work(&fixture.cache()).unwrap();
        let tracker = Tracker::new(&work, Stage::Resolve);
        tracker.set(Stage::Fetch, Some(Part::Runtime));
        tracker.graph.known.store(7, Ordering::Release);
        tracker.graph.objects.store(3, Ordering::Release);
        tracker.graph.bytes.store(1024, Ordering::Release);
        tracker.write();
        let progress = fixture.status(source()).progress.unwrap();
        let (objects_bound, bytes_bound) = crate::ostree::Progress::bounds();
        assert_eq!(
            progress,
            Progress {
                stage: Stage::Fetch,
                part: Some(Part::Runtime),
                objects: 3,
                objects_known: 7,
                objects_bound,
                bytes: 1024,
                bytes_bound,
            }
        );
        // The ticker refreshes the record while the body runs; only the
        // ticker writes the new count, however long it takes to.
        let refreshed = tracker.run(|| {
            tracker.graph.objects.store(6, Ordering::Release);
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(60) {
                let seen = fixture.status(source()).progress.map(|seen| seen.objects);
                if seen == Some(6) {
                    return seen;
                }
                std::thread::sleep(PROGRESS_TICK);
            }
            None
        });
        assert_eq!(refreshed, Some(6));
        // A body that panics still ends the ticker, so `run` returns.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracker.run(|| panic!("body"));
        }));
        assert!(panicked.is_err());
        fs::write(work.join(PROGRESS), "garbage\n").unwrap();
        assert_eq!(
            fixture.status(source()).progress,
            Some(Progress::idle(Stage::Busy))
        );
        drop(writer);
        assert!(work.join(PROGRESS).exists());
        assert_eq!(fixture.status(source()).progress, None);
        // The next writer does not inherit the dead one's record.
        let writer = lock(&fixture.cache()).unwrap();
        assert!(!work.join(PROGRESS).exists());
        assert_eq!(
            fixture.status(source()).progress,
            Some(Progress::idle(Stage::Busy))
        );
        drop(writer);
    }

    #[test]
    fn remove_clears_the_pointers_keeps_the_floor_and_spares_a_held_generation() {
        let fixture = Fixture::new("remove");
        assert_eq!(remove(&fixture.store()), Ok(Outcome::Removed));
        assert!(!fixture.cache().exists());
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let first = app_commit(&fixture, b"one", 20);
        fixture.update(source(), NOW).unwrap();
        let second = app_commit(&fixture, b"two", 30);
        fixture.update(source(), NOW).unwrap();
        fs::create_dir_all(fixture.state()).unwrap();
        let launch = fixture.hold(2);

        assert_eq!(remove(&fixture.store()), Ok(Outcome::Removed));
        assert_eq!(fixture.ready(source()), Ok(None));
        let removed = fixture.status(source());
        assert_eq!((removed.current, removed.previous), (None, None));
        // The running launch keeps its generation and trees; generation 1
        // and its own commit go.
        assert_eq!(fixture.names(GENERATIONS), vec!["2"]);
        assert!(deployed(&fixture.cache(), &second));
        assert!(!deployed(&fixture.cache(), &first));
        assert!(fixture.cache().join(ADMITTED).exists());
        assert!(fixture.state().exists());
        drop(launch);
        remove(&fixture.store()).unwrap();
        assert!(fixture.names(GENERATIONS).is_empty());
        assert!(fixture.names(DEPLOY).is_empty());
        assert!(fixture.names(OBJECTS).is_empty());

        // A reinstall admits nothing older than what was admitted.
        app_commit(&fixture, b"one", 20);
        let error = fixture.update(source(), NOW).unwrap_err();
        assert!(error.contains("does not supersede"), "{error}");
        // Numbering starts again in an empty generations directory.
        app_commit(&fixture, b"three", 40);
        assert_eq!(fixture.update(source(), NOW), Ok(Outcome::Selected(1)));
    }

    #[test]
    fn delete_data_waits_for_remove_and_for_every_instance() {
        let fixture = Fixture::new("delete-data");
        assert_eq!(
            delete_data(&fixture.home, &fixture.store(), source()),
            Ok(Outcome::Deleted(false))
        );
        assert!(!fixture.cache().exists());
        publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        let profile = fixture.state().join("data/profile");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("prefs.js"), b"x").unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o500)).unwrap();

        let error = delete_data(&fixture.home, &fixture.store(), source()).unwrap_err();
        assert!(error.contains("remove it before"), "{error}");
        let launch = fixture.hold(1);
        remove(&fixture.store()).unwrap();
        let error = delete_data(&fixture.home, &fixture.store(), source()).unwrap_err();
        assert!(error.contains("is running"), "{error}");
        assert!(profile.join("prefs.js").exists());
        drop(launch);
        assert_eq!(
            delete_data(&fixture.home, &fixture.store(), source()),
            Ok(Outcome::Deleted(true))
        );
        assert!(!fixture.state().exists());
        assert!(fixture.names(GENERATIONS).is_empty());
        let applications = fixture.home.join(STATE_ROOT);
        assert_eq!(fs::read_dir(&applications).unwrap().count(), 0);

        // An interrupted deletion is finished by the next one.
        let doomed = applications.join(format!("{REMOVING}firefox"));
        fs::create_dir_all(doomed.join("half")).unwrap();
        assert_eq!(
            delete_data(&fixture.home, &fixture.store(), source()),
            Ok(Outcome::Deleted(false))
        );
        assert!(!doomed.exists());

        // State reached through a link is nothing td-jail made.
        let elsewhere = fixture.home.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, fixture.state()).unwrap();
        assert!(delete_data(&fixture.home, &fixture.store(), source()).is_err());
        assert!(elsewhere.exists());
    }

    #[test]
    fn revalidation_reruns_rule_five_offline_over_current_deploys() {
        let fixture = Fixture::new("revalidate");
        let admission = fixture.admission();
        assert_eq!(
            revalidate(&admission, &fixture.store(), &fixture.view(), source(), NOW),
            Ok(None)
        );
        let runtime = publish_commit(&fixture.repository, RUNTIME, b"usr", 10, None, b"signed");
        let old = app_commit(&fixture, b"old", 15);
        fixture.update(source(), NOW).unwrap();
        let app = app_commit(&fixture, b"app", 20);
        fixture.update(source(), NOW).unwrap();
        assert_eq!(
            revalidate(&admission, &fixture.store(), &fixture.view(), source(), NOW),
            Ok(Some(2))
        );
        let (_, stale) = fixture.current().unwrap();
        // Nothing upstream answers from here on.
        fixture.repository.lock().unwrap().clear();
        let moved = FlathubSource {
            entry: "/app/bin/firefox-bin",
            ..source()
        };
        assert_eq!(fixture.ready(moved), Ok(None));

        REFUSE.with(|refuse| *refuse.borrow_mut() = true);
        assert_eq!(
            revalidate(
                &admission,
                &fixture.store(),
                &fixture.view(),
                moved,
                NOW + 1
            ),
            Ok(None)
        );
        REFUSE.with(|refuse| *refuse.borrow_mut() = false);
        let refused = fixture.status(moved).refused.unwrap();
        assert!(refused
            .reason
            .starts_with("offline revalidation: fixture validator"));

        VALIDATED.with(|seen| seen.borrow_mut().clear());
        assert_eq!(
            revalidate(
                &admission,
                &fixture.store(),
                &fixture.view(),
                moved,
                NOW + 2
            ),
            Ok(Some(3))
        );
        let deploy = fixture.cache().join(DEPLOY);
        VALIDATED.with(|seen| {
            assert_eq!(
                *seen.borrow(),
                vec![(
                    deploy.join(&app),
                    fixture.runtime_cache().join(DEPLOY).join(&runtime)
                )]
            );
        });
        assert_eq!(fixture.ready(moved), Ok(Some(3)));
        let (_, current) = fixture.current().unwrap();
        assert_eq!(current.application, stale.application);
        assert_eq!(current.runtime, stale.runtime);
        assert_ne!(current.checks, stale.checks);
        // The stale copy is collected and the real rollback kept.
        let (number, previous) = read_pointer(&fixture.cache(), PREVIOUS).unwrap().unwrap();
        assert_eq!((number, previous.application.commit), (1, old));
        assert_eq!(fixture.names(GENERATIONS), ["1", "3"]);
        assert_eq!(fixture.status(moved).refused, None);
        // `checked` says when upstream was last asked, which this did not.
        assert_eq!(fixture.status(moved).checked, Some(NOW));

        // A missing deploy or other refs need a fetch.
        remove_tree(&deploy.join(&app)).unwrap();
        let back = source();
        assert_eq!(
            revalidate(&admission, &fixture.store(), &fixture.view(), back, NOW + 3),
            Ok(None)
        );
        assert!(fixture
            .status(back)
            .refused
            .unwrap()
            .reason
            .contains("missing"));
        let other = FlathubSource {
            runtime: flathub_app::FlathubRuntime {
                exact_ref: "runtime/org.example.Platform/x86_64/2",
                ..source().runtime
            },
            ..source()
        };
        assert_eq!(
            revalidate(
                &admission,
                &fixture.store(),
                &fixture.view(),
                other,
                NOW + 4
            ),
            Ok(None)
        );
        assert!(fixture
            .status(other)
            .refused
            .unwrap()
            .reason
            .contains("other refs"));
    }
}
