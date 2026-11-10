//! A spec whose halves come from Flathub (APPLICATIONS.md §W.9): the
//! application tree from the account's cache, the runtime's from the
//! runtime store. td-app, run as this same account and as the runtime
//! account when the owner asked td-software to install or update it,
//! fetched and admitted the trees; td-jail asks it which generation serves
//! the declaration, holds that generation and the runtime store's `current`
//! against collection for the life of the launch, and names their trees. A
//! workspace naming a runtime holds that runtime's `current` alone. A
//! launch fetches nothing.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::flathub_app::{
    self, FlathubRuntime, FlathubSource, RuntimeSelection, Selection, CACHE_DIR, CURRENT, DEPLOY,
    FILES, FLATHUB_SOURCES, GENERATIONS, MAX_STATE_BYTES, RUNTIME_STORE, RUNTIME_UID, SELECTION,
};

/// td-net's applet, linked beside td-jail in the image.
const TD_APP: &str = "/bin/td-app";
const MAX_READY_BYTES: usize = 32;
/// A generation td-app named can go before td-jail holds it when two
/// newer ones were published meanwhile, or a removal collected it, and
/// the runtime's `current` can move between td-app's answer and the hold;
/// asking again settles it, revalidating against the new runtime, or
/// answers `absent`.
const ATTEMPTS: usize = 3;
const O_NOFOLLOW: i32 = 0o400_000;
const O_NONBLOCK: i32 = 0o4000;

fn refused(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(crate) struct Generation {
    pub(crate) selection: Selection,
    /// A shared lock on the selection, held for the launch: td-app removes
    /// no generation it cannot lock exclusively.
    pub(crate) hold: File,
    /// The canonical application cache the generation was held in.
    pub(crate) cache: PathBuf,
    /// The runtime store's `current`, held alike; its row is the
    /// selection's `runtime=`.
    pub(crate) runtime: RuntimeGeneration,
}

impl Generation {
    /// The two leases stage 1 keeps for the launch.
    pub(crate) fn holds(self) -> Vec<File> {
        vec![self.hold, self.runtime.hold]
    }
}

pub(crate) struct RuntimeGeneration {
    pub(crate) selection: RuntimeSelection,
    pub(crate) hold: File,
    /// The canonical runtime store, the runtime account's.
    pub(crate) store: PathBuf,
}

/// A deploy tree a selection names: a direct directory of the canonical
/// `cache`, owned by `uid` and writable by nobody else.
pub(crate) fn tree(cache: &Path, commit: &str, uid: u32) -> io::Result<PathBuf> {
    let path = cache.join(DEPLOY).join(commit).join(FILES);
    let canonical = fs::canonicalize(&path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    let metadata = fs::symlink_metadata(&path)?;
    if canonical != path
        || !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
    {
        return Err(refused(format!(
            "{} is not a direct directory only this account writes",
            path.display()
        )));
    }
    Ok(path)
}

/// The generation to launch, held. A launch never fetches
/// (td-software/DESIGN.md §4.1): with no generation serving the declaration
/// it revalidates `current` offline, which a system update that changed
/// the policy needs, and with no `current` it refuses. Either refusal sends
/// the owner to td-software, the one place an application installs or
/// updates.
pub(crate) fn hold_generation(
    source: FlathubSource,
    home: &Path,
    uid: u32,
) -> io::Result<Generation> {
    let runtime = RuntimeStore {
        declared: source.runtime,
        root: Path::new(RUNTIME_STORE).join(source.runtime.name),
        owner: RUNTIME_UID,
    };
    hold_with(source.name, home, uid, &runtime, |ask| {
        answer(ask, source.name, home)
    })
}

/// Where a declared runtime's store is and whose it must be.
struct RuntimeStore {
    declared: FlathubRuntime,
    root: PathBuf,
    owner: u32,
}

/// What td-jail asks td-app; both answer a generation or `absent`, and
/// `revalidate` `busy` while an update or removal holds the cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ask {
    /// `ready`: the generation that serves now, taking no lock.
    Ready,
    /// `revalidate`: rule 5 again over `current`'s deploys, offline.
    Revalidate,
}

impl Ask {
    fn verb(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Revalidate => "revalidate",
        }
    }
}

/// The refusal for an account whose cache has no `current`.
fn not_installed(name: &str) -> io::Error {
    refused(format!(
        "{name} is not installed; install it in td-software"
    ))
}

fn hold_with(
    name: &str,
    home: &Path,
    uid: u32,
    runtime: &RuntimeStore,
    mut ask: impl FnMut(Ask) -> io::Result<Option<u64>>,
) -> io::Result<Generation> {
    for _ in 0..ATTEMPTS {
        let generation = match ask(Ask::Ready)? {
            Some(generation) => generation,
            None if !installed(home)? => return Err(not_installed(name)),
            None => match ask(Ask::Revalidate)? {
                Some(generation) => generation,
                None if !installed(home)? => return Err(not_installed(name)),
                None => {
                    return Err(refused(format!(
                        "{name} did not revalidate offline against this system and its \
                         runtime; update it in td-software"
                    )))
                }
            },
        };
        let cache = account_cache(home)?;
        let Some((selection, hold)) = hold(&cache, generation, uid)? else {
            continue;
        };
        let Some(held_runtime) = hold_runtime(runtime, || {
            application_runtime_absent(name, runtime.declared)
        })?
        else {
            continue;
        };
        // The runtime moved after td-app answered: the next `ready` answers
        // `absent`, and the revalidation follows the new `current`.
        if held_runtime.selection.runtime != selection.runtime {
            continue;
        }
        return Ok(Generation {
            selection,
            hold,
            cache,
            runtime: held_runtime,
        });
    }
    Err(refused(format!(
        "{name}'s generation or its runtime's moved each time td-jail tried to hold them"
    )))
}

/// Whether the account's cache has a `current` generation, which is what
/// installed means; the link itself, not what it names.
fn installed(home: &Path) -> io::Result<bool> {
    let current = home.join(CACHE_DIR).join(CURRENT);
    match fs::symlink_metadata(&current) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("stat {}: {error}", current.display()),
        )),
    }
}

/// The refusal while td-app's revalidation found the cache's writer
/// fetching or removing: a launch does not wait for an update.
fn busy(name: &str) -> io::Error {
    refused(format!(
        "an update of {name} is running; launch it when it finishes"
    ))
}

/// td-app with only the account's home: it needs nothing else from td-jail.
fn td_app(home: &Path) -> Command {
    let mut command = Command::new(TD_APP);
    command
        .env_clear()
        .env("HOME", home)
        .current_dir("/")
        .stdin(Stdio::null());
    command
}

fn answer(ask: Ask, name: &str, home: &Path) -> io::Result<Option<u64>> {
    let verb = ask.verb();
    let output = td_app(home)
        .args([verb, name])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| io::Error::other(format!("run {TD_APP} {verb} {name}: {error}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{TD_APP} {verb} {name}: {}",
            output.status
        )));
    }
    parse_answer(ask, name, &output.stdout)
}

/// td-app's one line: a generation, `absent`, or, from `revalidate` only,
/// `busy`.
fn parse_answer(ask: Ask, name: &str, stdout: &[u8]) -> io::Result<Option<u64>> {
    let verb = ask.verb();
    let answer = (stdout.len() <= MAX_READY_BYTES)
        .then(|| std::str::from_utf8(stdout).ok())
        .flatten()
        .and_then(|text| text.strip_suffix('\n'));
    match answer {
        Some("absent") => Ok(None),
        // `revalidate` found an update or removal holding the cache and
        // did not wait for it.
        Some("busy") if ask == Ask::Revalidate => Err(busy(name)),
        Some(generation) => flathub_app::generation_number(generation)
            .map(Some)
            .ok_or_else(|| refused(format!("{TD_APP} {verb} {name} answered {generation:?}"))),
        None => Err(refused(format!("{TD_APP} {verb} {name} answered no line"))),
    }
}

/// The account's cache, canonical and reached without a link.
fn account_cache(home: &Path) -> io::Result<PathBuf> {
    let cache = fs::canonicalize(home)?.join(CACHE_DIR);
    if fs::canonicalize(&cache)? != cache {
        return Err(refused(format!(
            "{} is not a direct directory",
            cache.display()
        )));
    }
    Ok(cache)
}

/// Lock the generation's selection in the canonical `cache` shared and
/// read it; it must be `uid`'s. `None` when td-app collected it first: it
/// unlinks a generation only under the exclusive lock, so one held here
/// with a link left stays until the lock goes.
fn hold(cache: &Path, generation: u64, uid: u32) -> io::Result<Option<(Selection, File)>> {
    let Some((text, file)) = lock_selection(cache, generation, uid)? else {
        return Ok(None);
    };
    let selection = Selection::parse(&text).map_err(|error| {
        refused(format!(
            "{}: {error}",
            cache
                .join(GENERATIONS)
                .join(generation.to_string())
                .display()
        ))
    })?;
    Ok(Some((selection, file)))
}

/// The refusal for an application whose runtime store has no `current`.
fn application_runtime_absent(name: &str, runtime: FlathubRuntime) -> io::Error {
    refused(format!(
        "{name}'s runtime {} is not installed; update {name} in td-software",
        runtime.name
    ))
}

/// The refusal for a workspace whose runtime store has no `current`: a
/// workspace installs nothing, and an application that names the runtime
/// installs and keeps it.
fn workspace_runtime_absent(runtime: FlathubRuntime) -> io::Error {
    let installers: Vec<&str> = FLATHUB_SOURCES
        .iter()
        .filter(|source| source.runtime == runtime)
        .map(|source| source.name)
        .collect();
    if installers.is_empty() {
        return refused(format!(
            "the runtime {} is not installed, and no application installs it yet",
            runtime.name
        ));
    }
    refused(format!(
        "the runtime {} is not installed; install an application that uses it ({}) in \
         td-software",
        runtime.name,
        installers.join(", ")
    ))
}

/// A workspace's runtime: the store's `current`, held for the launch, and
/// its tree.
pub(crate) struct HeldRuntime {
    pub(crate) generation: RuntimeGeneration,
    pub(crate) files: PathBuf,
}

/// The runtime store a workspace launch holds from, compiled.
pub(crate) const WORKSPACE_RUNTIME_STORE: (&str, u32) = (RUNTIME_STORE, RUNTIME_UID);

/// The declared `runtime`'s `current` for a workspace launch (WORKLOADS.md,
/// "Runtimes"), held as an application's is, from the store below `root`
/// that `owner` keeps. A workspace follows `current` and asks td-app
/// nothing.
pub(crate) fn hold_workspace_runtime(
    runtime: FlathubRuntime,
    root: &Path,
    owner: u32,
) -> io::Result<HeldRuntime> {
    hold_store_current(&RuntimeStore {
        declared: runtime,
        root: root.join(runtime.name),
        owner,
    })
}

fn hold_store_current(store: &RuntimeStore) -> io::Result<HeldRuntime> {
    for _ in 0..ATTEMPTS {
        // Collected between reading `current` and the lock: read it again.
        let Some(generation) = hold_runtime(store, || workspace_runtime_absent(store.declared))?
        else {
            continue;
        };
        let files = tree(
            &generation.store,
            &generation.selection.runtime.commit,
            store.owner,
        )?;
        return Ok(HeldRuntime { generation, files });
    }
    Err(refused(format!(
        "the runtime {}'s current named a generation td-jail could not hold each time it \
         tried: it moved, or its selection is missing",
        store.declared.name
    )))
}

/// The runtime store's `current`, held as [`hold`] holds a generation: the
/// store canonical, its selection and deploy the runtime account's and
/// writable by no one else, naming the declared runtime. `None` when it
/// was collected before the lock; `absent`'s refusal when there is none.
fn hold_runtime(
    store: &RuntimeStore,
    absent: impl FnOnce() -> io::Error,
) -> io::Result<Option<RuntimeGeneration>> {
    let root = &store.root;
    let link = root.join(CURRENT);
    let target = match fs::read_link(&link) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(absent()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("read {}: {error}", link.display()),
            ))
        }
    };
    if fs::canonicalize(root)? != *root {
        return Err(refused(format!(
            "{} is not a direct directory",
            root.display()
        )));
    }
    let generation = target
        .to_str()
        .ok_or_else(|| refused(format!("{} is not UTF-8", link.display())))
        .and_then(|target| flathub_app::parse_pointer(target).map_err(refused))?;
    let Some((text, hold)) = lock_selection(root, generation, store.owner)? else {
        return Ok(None);
    };
    let selection = RuntimeSelection::parse(&text)
        .map_err(|error| refused(format!("{}: {error}", root.display())))?;
    if selection.name != store.declared.name
        || selection.runtime.exact_ref != store.declared.exact_ref
    {
        return Err(refused(format!(
            "{}'s current names another runtime",
            root.display()
        )));
    }
    Ok(Some(RuntimeGeneration {
        selection,
        hold,
        store: root.clone(),
    }))
}

/// Open a generation's selection as td-app does, lock it shared and read
/// it; it must be `owner`'s regular file, writable by no one else.
fn lock_selection(cache: &Path, generation: u64, owner: u32) -> io::Result<Option<(String, File)>> {
    let path = cache
        .join(GENERATIONS)
        .join(generation.to_string())
        .join(SELECTION);
    // As td-app opens it: a link here is nothing it published.
    let file = match File::options()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("open {}: {error}", path.display()),
            ))
        }
    };
    file.lock_shared()?;
    let metadata = file.metadata()?;
    if metadata.nlink() == 0 {
        return Ok(None);
    }
    if !metadata.is_file() || metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
        return Err(refused(format!(
            "{} is not a regular file only its account writes",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(
            u64::try_from(MAX_STATE_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(refused(format!("{} is oversized", path.display())));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| refused(format!("{} is not UTF-8", path.display())))?;
    Ok(Some((text, file)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const APP: &str = "app/org.example.Fixture/x86_64/stable";
    const APP_COMMIT: &str = "bd44a62300000000000000000000000000000000000000000000000000000000";
    const RUNTIME_NAME: &str = "freedesktop-platform-25-08";
    const RUNTIME: &str = "runtime/org.freedesktop.Platform/x86_64/25.08";
    const COMMIT: &str = "7985364b35b83cfe5aa17a00932df1c2d1dae69e5c592aa637a53ff1d53fdabc";
    const MOVED: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn uid() -> u32 {
        fs::metadata("/proc/self").unwrap().uid()
    }

    fn admitted(exact_ref: &str, commit: &str, timestamp: u64) -> flathub_app::Admitted {
        flathub_app::Admitted {
            exact_ref: exact_ref.into(),
            commit: commit.into(),
            timestamp,
        }
    }

    /// A deploy's `files/` below `cache`, as td-app publishes one.
    fn deploy(cache: &Path, commit: &str) -> PathBuf {
        let files = cache.join(DEPLOY).join(commit).join(FILES);
        fs::create_dir_all(&files).unwrap();
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
        files
    }

    fn write_selection(cache: &Path, number: u64, text: &str) {
        let directory = cache.join(GENERATIONS).join(number.to_string());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(SELECTION), text).unwrap();
        fs::set_permissions(directory.join(SELECTION), fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// An application generation validated against runtime `commit`.
    fn generation(home: &Path, number: u64, commit: &str) -> PathBuf {
        let selection = Selection {
            name: "fixture".into(),
            checks: "0".repeat(64),
            application: admitted(APP, APP_COMMIT, 20),
            runtime: admitted(RUNTIME, commit, 10),
        };
        let cache = home.join(CACHE_DIR);
        write_selection(&cache, number, &selection.render());
        deploy(&cache, APP_COMMIT)
    }

    /// The runtime store's generation `number` on `commit`, made `current`.
    fn runtime_generation(store: &Path, number: u64, commit: &str) -> PathBuf {
        let selection = RuntimeSelection {
            name: RUNTIME_NAME.into(),
            runtime: admitted(RUNTIME, commit, 10),
        };
        write_selection(store, number, &selection.render());
        let current = store.join(CURRENT);
        let _ = fs::remove_file(&current);
        std::os::unix::fs::symlink(format!("generations/{number}"), &current).unwrap();
        deploy(store, commit)
    }

    /// The fixture's runtime store, the test's own uid standing for the
    /// runtime account's.
    fn runtime_store(root: &Path) -> RuntimeStore {
        RuntimeStore {
            declared: FlathubRuntime {
                name: RUNTIME_NAME,
                exact_ref: RUNTIME,
            },
            root: fs::canonicalize(root)
                .unwrap()
                .join("runtimes")
                .join(RUNTIME_NAME),
            owner: uid(),
        }
    }

    fn hold_for_fixture(store: &RuntimeStore) -> io::Result<Option<RuntimeGeneration>> {
        hold_runtime(store, || {
            application_runtime_absent("fixture", store.declared)
        })
    }

    /// A workspace holds the runtime store's `current` alone, as an
    /// application's launch holds it; with none it names who installs the
    /// runtime, and it asks again only while `current` names a generation
    /// collected under it.
    #[test]
    fn a_workspace_holds_the_runtime_current_or_says_who_installs_it() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-workspace").unwrap();
        let store = runtime_store(root.path());
        assert_eq!(
            hold_store_current(&store).err().unwrap().to_string(),
            "the runtime freedesktop-platform-25-08 is not installed; install an application \
             that uses it (firefox) in td-software"
        );
        let sdk = flathub_app::flathub_runtime("freedesktop-sdk-25-08").unwrap();
        assert_eq!(
            workspace_runtime_absent(sdk).to_string(),
            "the runtime freedesktop-sdk-25-08 is not installed, and no application installs it yet"
        );

        let files = runtime_generation(&store.root, 1, COMMIT);
        let held = hold_store_current(&store).unwrap();
        assert_eq!(held.files, files);
        assert_eq!(held.generation.selection.runtime.commit, COMMIT);
        let selection = store.root.join(GENERATIONS).join("1").join(SELECTION);
        let probe = File::open(&selection).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(held);
        drop(probe);

        // Another account's store, a selection or tree another account
        // could write: nothing to bind.
        let foreign = RuntimeStore {
            owner: uid().wrapping_add(1),
            ..runtime_store(root.path())
        };
        assert!(hold_store_current(&foreign).is_err());
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o664)).unwrap();
        assert!(hold_store_current(&store).is_err());
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&files, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(hold_store_current(&store).is_err());
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(hold_store_current(&store).is_ok());

        // Through the workspace's entry, from the store root it is given.
        let held = hold_workspace_runtime(
            store.declared,
            &fs::canonicalize(root.path()).unwrap().join("runtimes"),
            uid(),
        );
        assert_eq!(held.unwrap().files, files);
        assert!(hold_workspace_runtime(
            store.declared,
            &fs::canonicalize(root.path()).unwrap().join("runtimes"),
            uid().wrapping_add(1)
        )
        .is_err());

        // `current` naming a generation collected before the lock, every
        // time it is read, or one whose selection is missing.
        runtime_generation(&store.root, 2, MOVED);
        fs::remove_file(store.root.join(GENERATIONS).join("2").join(SELECTION)).unwrap();
        assert_eq!(
            hold_store_current(&store).err().unwrap().to_string(),
            "the runtime freedesktop-platform-25-08's current named a generation td-jail \
             could not hold each time it tried: it moved, or its selection is missing"
        );
    }

    #[test]
    fn a_held_generation_names_only_direct_private_trees() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-hold").unwrap();
        let home = root.path().join("home");
        let files = generation(&home, 1, COMMIT);
        let cache = account_cache(&home).unwrap();
        assert!(hold(&cache, 2, uid()).unwrap().is_none());
        let (selection, held) = hold(&cache, 1, uid()).unwrap().unwrap();
        assert_eq!(selection.application.commit, APP_COMMIT);
        assert_eq!(tree(&cache, APP_COMMIT, uid()).unwrap(), files);
        // A deploy the selection names but the cache lacks: the runtime's
        // is the runtime store's, never the application cache's.
        assert!(tree(&cache, COMMIT, uid()).is_err());
        assert!(tree(&cache, APP_COMMIT, uid().wrapping_add(1)).is_err());
        fs::set_permissions(&files, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(tree(&cache, APP_COMMIT, uid()).is_err());
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
        // td-app's collection finds the hold and leaves the generation.
        let probe = File::open(cache.join(GENERATIONS).join("1").join(SELECTION)).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(held);
        probe.try_lock().unwrap();
        drop(probe);

        // A selection another account could write is nothing to launch.
        let selection = cache.join(GENERATIONS).join("1").join(SELECTION);
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o664)).unwrap();
        assert!(hold(&cache, 1, uid()).is_err());
        fs::set_permissions(&selection, fs::Permissions::from_mode(0o644)).unwrap();

        // A deploy reached through a link is not the cache's own tree.
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::remove_dir(&files).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &files).unwrap();
        assert!(hold(&cache, 1, uid()).unwrap().is_some());
        assert!(tree(&cache, APP_COMMIT, uid()).is_err());
    }

    /// td-app answers a generation or `absent`; `revalidate` also `busy`,
    /// while an update or removal holds the cache, which refuses the
    /// launch rather than wait for it. Anything else is refused as itself.
    #[test]
    fn td_apps_answers_are_one_line_and_busy_refuses_the_launch() {
        for ask in [Ask::Ready, Ask::Revalidate] {
            assert_eq!(parse_answer(ask, "fixture", b"absent\n").unwrap(), None);
            assert_eq!(parse_answer(ask, "fixture", b"7\n").unwrap(), Some(7));
            for bad in [&b"07\n"[..], b"7", b"", b"7\n8\n", b"\xff\n"] {
                assert!(parse_answer(ask, "fixture", bad).is_err(), "{bad:?}");
            }
        }
        assert_eq!(
            parse_answer(Ask::Revalidate, "fixture", b"busy\n")
                .unwrap_err()
                .to_string(),
            "an update of fixture is running; launch it when it finishes"
        );
        assert!(parse_answer(Ask::Ready, "fixture", b"busy\n")
            .unwrap_err()
            .to_string()
            .contains("answered \"busy\""));
    }

    #[test]
    fn a_generation_unlinked_under_the_lock_is_not_held() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-unlinked").unwrap();
        let home = root.path().join("home");
        generation(&home, 1, COMMIT);
        let selection = home
            .join(CACHE_DIR)
            .join(GENERATIONS)
            .join("1")
            .join(SELECTION);
        let collector = File::open(&selection).unwrap();
        collector.lock().unwrap();
        fs::remove_file(&selection).unwrap();
        drop(collector);
        assert!(hold(&account_cache(&home).unwrap(), 1, uid())
            .unwrap()
            .is_none());
    }

    /// The runtime store's `current` is held as a generation is, and only
    /// while it is the runtime account's, canonical and the declared
    /// runtime; its tree is a direct directory only that account writes.
    #[test]
    fn the_runtime_current_is_held_only_as_the_runtime_accounts() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-runtime").unwrap();
        let store = runtime_store(root.path());
        let error = hold_for_fixture(&store).err().unwrap();
        assert_eq!(
            error.to_string(),
            "fixture's runtime freedesktop-platform-25-08 is not installed; \
             update fixture in td-software"
        );
        let files = runtime_generation(&store.root, 1, COMMIT);
        let held = hold_for_fixture(&store).unwrap().unwrap();
        assert_eq!(held.selection.runtime.commit, COMMIT);
        assert_eq!(tree(&held.store, COMMIT, store.owner).unwrap(), files);
        assert!(tree(&held.store, COMMIT, uid().wrapping_add(1)).is_err());
        // The runtime account's collection finds the hold.
        let probe = File::open(store.root.join(GENERATIONS).join("1").join(SELECTION)).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(held);
        drop(probe);

        let foreign = RuntimeStore {
            owner: uid().wrapping_add(1),
            ..runtime_store(root.path())
        };
        assert!(hold_for_fixture(&foreign).is_err());
        let other = RuntimeStore {
            declared: FlathubRuntime {
                name: RUNTIME_NAME,
                exact_ref: "runtime/org.freedesktop.Platform/x86_64/26.08",
            },
            ..runtime_store(root.path())
        };
        assert!(hold_for_fixture(&other)
            .err()
            .unwrap()
            .to_string()
            .contains("another runtime"));
        // A store reached through a link is not the runtime account's.
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&store.root, &linked).unwrap();
        let through = RuntimeStore {
            root: linked,
            ..runtime_store(root.path())
        };
        assert!(hold_for_fixture(&through).is_err());
    }

    /// A launch never fetches (td-software/DESIGN.md §4.1, §6): it asks
    /// `ready`, then, only for an installed account, `revalidate`; never
    /// `update`, and nothing beside the launch. `answers` are td-app's, in
    /// order; `moves` runs after each answer, as a runtime update could.
    fn launch(
        root: &Path,
        home: &Path,
        answers: &[Option<u64>],
        mut moves: impl FnMut(usize),
    ) -> (io::Result<Generation>, Vec<Ask>) {
        let mut asked = Vec::new();
        let mut answers = answers.iter();
        let store = runtime_store(root);
        let held = hold_with("fixture", home, uid(), &store, |ask| {
            asked.push(ask);
            let answer = answers.next().copied().flatten();
            moves(asked.len());
            Ok(answer)
        });
        (held, asked)
    }

    #[test]
    fn an_account_without_current_is_not_installed_and_fetches_nothing() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-absent").unwrap();
        let home = root.path().join("home");
        let (held, asked) = launch(root.path(), &home, &[None], |_| {});
        assert_eq!(
            held.err().unwrap().to_string(),
            "fixture is not installed; install it in td-software"
        );
        assert_eq!(asked, [Ask::Ready]);
        // A generation left behind without `current` is not installed
        // either: a removal clears `current` first.
        generation(&home, 1, COMMIT);
        let (held, asked) = launch(root.path(), &home, &[None], |_| {});
        assert!(held.is_err());
        assert_eq!(asked, [Ask::Ready]);
    }

    /// An installed account launches its generation on the runtime it was
    /// validated against, holding both, with no other question.
    #[test]
    fn an_existing_current_launches_without_a_fetch() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-existing").unwrap();
        let home = root.path().join("home");
        generation(&home, 1, COMMIT);
        std::os::unix::fs::symlink("generations/1", home.join(CACHE_DIR).join(CURRENT)).unwrap();
        runtime_generation(&runtime_store(root.path()).root, 1, COMMIT);
        let (held, asked) = launch(root.path(), &home, &[Some(1)], |_| {});
        let held = held.unwrap();
        assert_eq!(held.selection.runtime.commit, COMMIT);
        assert_eq!(held.runtime.selection.runtime.commit, COMMIT);
        assert_eq!(held.holds().len(), 2);
        assert_eq!(asked, [Ask::Ready]);
    }

    /// After a system update changed the policy, or the runtime moved,
    /// `current` no longer serves: the launch revalidates it offline and
    /// launches what that publishes, or refuses and sends the owner to
    /// td-software.
    #[test]
    fn a_stale_current_is_revalidated_offline_or_refused() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-stale").unwrap();
        let home = root.path().join("home");
        generation(&home, 1, COMMIT);
        generation(&home, 2, COMMIT);
        std::os::unix::fs::symlink("generations/1", home.join(CACHE_DIR).join(CURRENT)).unwrap();
        runtime_generation(&runtime_store(root.path()).root, 1, COMMIT);
        let (held, asked) = launch(root.path(), &home, &[None, Some(2)], |_| {});
        assert!(held.is_ok());
        assert_eq!(asked, [Ask::Ready, Ask::Revalidate]);
        let (held, asked) = launch(root.path(), &home, &[None, None], |_| {});
        assert_eq!(
            held.err().unwrap().to_string(),
            "fixture did not revalidate offline against this system and its runtime; \
             update it in td-software"
        );
        assert_eq!(asked, [Ask::Ready, Ask::Revalidate]);
        assert_eq!(Ask::Ready.verb(), "ready");
        assert_eq!(Ask::Revalidate.verb(), "revalidate");
    }

    /// The runtime's `current` moved between td-app's answer and the hold:
    /// the generation td-app named was validated against the old one, so
    /// td-jail asks again, and the revalidation td-app runs on that
    /// `absent` names a generation on the new runtime.
    #[test]
    fn a_runtime_that_moved_under_the_launch_is_followed() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-moved").unwrap();
        let home = root.path().join("home");
        let store = runtime_store(root.path()).root;
        generation(&home, 1, COMMIT);
        generation(&home, 2, MOVED);
        std::os::unix::fs::symlink("generations/1", home.join(CACHE_DIR).join(CURRENT)).unwrap();
        runtime_generation(&store, 1, COMMIT);
        let (held, asked) = launch(root.path(), &home, &[Some(1), None, Some(2)], |asked| {
            if asked == 1 {
                runtime_generation(&store, 2, MOVED);
            }
        });
        let held = held.unwrap();
        assert_eq!(held.selection.runtime.commit, MOVED);
        assert_eq!(held.runtime.selection.runtime.commit, MOVED);
        assert_eq!(asked, [Ask::Ready, Ask::Ready, Ask::Revalidate]);

        // A runtime that keeps moving ends the launch rather than run an
        // application on a runtime it was not validated against.
        let (held, asked) = launch(root.path(), &home, &[Some(1); 3], |_| {});
        assert!(held.err().unwrap().to_string().contains("moved each time"));
        assert_eq!(asked.len(), ATTEMPTS);
    }

    #[test]
    fn a_linked_cache_is_refused() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-linked").unwrap();
        let home = root.path().join("home");
        let other = root.path().join("other");
        generation(&other, 1, COMMIT);
        fs::create_dir_all(home.join(".td")).unwrap();
        std::os::unix::fs::symlink(other.join(CACHE_DIR), home.join(CACHE_DIR)).unwrap();
        assert!(account_cache(&home).is_err());
    }
}
