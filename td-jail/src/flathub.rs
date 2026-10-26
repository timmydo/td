//! A spec whose halves come from the account's Flathub cache
//! (APPLICATIONS.md §W.9). td-app, run as this same account when the owner
//! asked td-software to install or update it, fetched and admitted the
//! trees; td-jail asks it which generation serves the declaration, holds
//! that generation against collection for the life of the launch, and
//! names its trees. A launch fetches nothing.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::flathub_app::{
    self, FlathubSource, Selection, CACHE_DIR, CURRENT, DEPLOY, FILES, GENERATIONS,
    MAX_STATE_BYTES, SELECTION,
};

/// td-net's applet, linked beside td-jail in the image.
const TD_APP: &str = "/bin/td-app";
const MAX_READY_BYTES: usize = 32;
/// A generation td-app named can go before td-jail holds it when two
/// newer ones were published meanwhile, or a removal collected it; asking
/// again settles it or answers `absent`.
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
    cache: PathBuf,
}

impl Generation {
    /// A deploy tree the selection names: a direct directory of the cache,
    /// the account's own and writable by nobody else.
    pub(crate) fn tree(&self, commit: &str, uid: u32) -> io::Result<PathBuf> {
        let path = self.cache.join(DEPLOY).join(commit).join(FILES);
        let canonical = fs::canonicalize(&path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
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
    hold_with(source.name, home, uid, |ask| answer(ask, source.name, home))
}

/// What td-jail asks td-app; both answer a generation or `absent`.
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
                        "{name} did not revalidate offline after a system update; \
                         update it in td-software"
                    )))
                }
            },
        };
        if let Some(held) = hold(home, generation, uid)? {
            return Ok(held);
        }
    }
    Err(refused(format!(
        "{name}'s generation was collected each time td-jail tried to hold one"
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
    let answer = (output.stdout.len() <= MAX_READY_BYTES)
        .then(|| std::str::from_utf8(&output.stdout).ok())
        .flatten()
        .and_then(|text| text.strip_suffix('\n'));
    match answer {
        Some("absent") => Ok(None),
        Some(generation) => flathub_app::generation_number(generation)
            .map(Some)
            .ok_or_else(|| refused(format!("{TD_APP} {verb} {name} answered {generation:?}"))),
        None => Err(refused(format!("{TD_APP} {verb} {name} answered no line"))),
    }
}

/// Lock the generation's selection shared and read it. `None` when td-app
/// collected it first: it unlinks a generation only under the exclusive
/// lock, so one held here with a link left stays until the lock goes.
fn hold(home: &Path, generation: u64, uid: u32) -> io::Result<Option<Generation>> {
    let cache = fs::canonicalize(home)?.join(CACHE_DIR);
    if fs::canonicalize(&cache)? != cache {
        return Err(refused(format!(
            "{} is not a direct directory",
            cache.display()
        )));
    }
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
    if !metadata.is_file() || metadata.uid() != uid {
        return Err(refused(format!(
            "{} is not this account's regular file",
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
    let selection =
        Selection::parse(&text).map_err(|error| refused(format!("{}: {error}", path.display())))?;
    Ok(Some(Generation {
        selection,
        hold: file,
        cache,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const APP: &str = "app/org.example.Fixture/x86_64/stable";
    const APP_COMMIT: &str = "bd44a62300000000000000000000000000000000000000000000000000000000";
    const RUNTIME: &str = "runtime/org.freedesktop.Platform/x86_64/25.08";
    const COMMIT: &str = "7985364b35b83cfe5aa17a00932df1c2d1dae69e5c592aa637a53ff1d53fdabc";

    fn uid() -> u32 {
        fs::metadata("/proc/self").unwrap().uid()
    }

    fn generation(home: &Path, number: u64) -> PathBuf {
        let selection = Selection {
            name: "fixture".into(),
            checks: "0".repeat(64),
            application: flathub_app::Admitted {
                exact_ref: APP.into(),
                commit: APP_COMMIT.into(),
                timestamp: 20,
            },
            runtime: flathub_app::Admitted {
                exact_ref: RUNTIME.into(),
                commit: COMMIT.into(),
                timestamp: 10,
            },
        };
        let directory = home
            .join(CACHE_DIR)
            .join(GENERATIONS)
            .join(number.to_string());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(SELECTION), selection.render()).unwrap();
        let files = home.join(CACHE_DIR).join(DEPLOY).join(COMMIT).join(FILES);
        fs::create_dir_all(&files).unwrap();
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
        files
    }

    #[test]
    fn a_held_generation_names_only_direct_private_trees() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-hold").unwrap();
        let home = root.path().join("home");
        let files = generation(&home, 1);
        assert!(hold(&home, 2, uid()).unwrap().is_none());
        let held = hold(&home, 1, uid()).unwrap().unwrap();
        assert_eq!(held.selection.runtime.commit, COMMIT);
        assert_eq!(held.tree(COMMIT, uid()).unwrap(), files);
        // A deploy the selection names but the cache lacks.
        assert!(held.tree(APP_COMMIT, uid()).is_err());
        assert!(held.tree(COMMIT, uid().wrapping_add(1)).is_err());
        fs::set_permissions(&files, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(held.tree(COMMIT, uid()).is_err());
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
        // td-app's collection finds the hold and leaves the generation.
        let probe = File::open(
            home.join(CACHE_DIR)
                .join(GENERATIONS)
                .join("1")
                .join(SELECTION),
        )
        .unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(held);
        probe.try_lock().unwrap();
        drop(probe);

        // A deploy reached through a link is not the cache's own tree.
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::remove_dir(&files).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &files).unwrap();
        let held = hold(&home, 1, uid()).unwrap().unwrap();
        assert!(held.tree(COMMIT, uid()).is_err());
    }

    #[test]
    fn a_generation_unlinked_under_the_lock_is_not_held() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-unlinked").unwrap();
        let home = root.path().join("home");
        generation(&home, 1);
        let selection = home
            .join(CACHE_DIR)
            .join(GENERATIONS)
            .join("1")
            .join(SELECTION);
        let collector = File::open(&selection).unwrap();
        collector.lock().unwrap();
        fs::remove_file(&selection).unwrap();
        drop(collector);
        assert!(hold(&home, 1, uid()).unwrap().is_none());
    }

    /// A launch never fetches (td-software/DESIGN.md §4.1, §6): it asks
    /// `ready`, then, only for an installed account, `revalidate`; never
    /// `update`, and nothing beside the launch.
    fn launch(home: &Path, answers: &[Option<u64>]) -> (io::Result<Generation>, Vec<Ask>) {
        let mut asked = Vec::new();
        let mut answers = answers.iter();
        let held = hold_with("fixture", home, uid(), |ask| {
            asked.push(ask);
            Ok(answers.next().copied().flatten())
        });
        (held, asked)
    }

    #[test]
    fn an_account_without_current_is_not_installed_and_fetches_nothing() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-absent").unwrap();
        let home = root.path().join("home");
        let (held, asked) = launch(&home, &[None]);
        assert_eq!(
            held.err().unwrap().to_string(),
            "fixture is not installed; install it in td-software"
        );
        assert_eq!(asked, [Ask::Ready]);
        // A generation left behind without `current` is not installed
        // either: a removal clears `current` first.
        generation(&home, 1);
        let (held, asked) = launch(&home, &[None]);
        assert!(held.is_err());
        assert_eq!(asked, [Ask::Ready]);
    }

    /// A machine whose account already had `current` before the cutover
    /// reads as installed and launches it with no other question.
    #[test]
    fn an_existing_current_launches_without_a_fetch() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-existing").unwrap();
        let home = root.path().join("home");
        generation(&home, 1);
        std::os::unix::fs::symlink("generations/1", home.join(CACHE_DIR).join(CURRENT)).unwrap();
        let (held, asked) = launch(&home, &[Some(1)]);
        assert_eq!(held.unwrap().selection.runtime.commit, COMMIT);
        assert_eq!(asked, [Ask::Ready]);
    }

    /// After a system update changed the policy, `current` no longer
    /// serves: the launch revalidates it offline and launches what that
    /// publishes, or refuses and sends the owner to td-software.
    #[test]
    fn a_stale_current_is_revalidated_offline_or_refused() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-stale").unwrap();
        let home = root.path().join("home");
        generation(&home, 1);
        generation(&home, 2);
        std::os::unix::fs::symlink("generations/1", home.join(CACHE_DIR).join(CURRENT)).unwrap();
        let (held, asked) = launch(&home, &[None, Some(2)]);
        assert!(held.is_ok());
        assert_eq!(asked, [Ask::Ready, Ask::Revalidate]);
        let (held, asked) = launch(&home, &[None, None]);
        assert_eq!(
            held.err().unwrap().to_string(),
            "fixture did not revalidate offline after a system update; update it in td-software"
        );
        assert_eq!(asked, [Ask::Ready, Ask::Revalidate]);
        assert_eq!(Ask::Ready.verb(), "ready");
        assert_eq!(Ask::Revalidate.verb(), "revalidate");
    }

    #[test]
    fn a_linked_cache_is_refused() {
        let root = crate::test_temp::TempDir::new("td-jail-flathub-linked").unwrap();
        let home = root.path().join("home");
        let other = root.path().join("other");
        generation(&other, 1);
        fs::create_dir_all(home.join(".td")).unwrap();
        std::os::unix::fs::symlink(other.join(CACHE_DIR), home.join(CACHE_DIR)).unwrap();
        assert!(hold(&home, 1, uid()).is_err());
    }
}
