//! A password store read for import into the open notebook: the `pass`
//! layout's `*.gpg` files under a chosen folder, each decrypted by the
//! account's own `gpg`, which its gpg-agent may ask for a passphrase or
//! a card. Only the vault's thread calls it. Each entry's name, its path
//! under the store less `.gpg`, is its title; its decrypted text is its
//! body, held in clearing owners. Nothing is written, and no plaintext
//! reaches a file, argv or the environment.

use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::plain::{self, Text};
use crate::protocol::{MAX_BODY, MAX_ENTRIES, MAX_TITLE};

/// The folders `pass` keeps beside its entries, never walked.
const PRUNED: [&str; 2] = [".git", ".extensions"];

/// The deepest folder walked below the store.
const DEPTH: usize = 32;

/// The most directory entries one walk examines.
const EXAMINED: usize = 64 * 1024;

/// The largest encrypted entry read: a body's bound with room for
/// OpenPGP's packets and armor.
const CIPHER: u64 = 4 * MAX_BODY as u64;

/// How often a decryption looks for a cancel.
const TICK: Duration = Duration::from_millis(20);

/// How long a decryption waits for its output's end once gpg has exited:
/// a process gpg started and left holding the pipe is not waited for.
const GRACE: Duration = Duration::from_secs(2);

/// gpg's arguments: decrypt standard input to standard output without
/// asking on a terminal; its agent asks for what a key needs.
const GPG: &[&str] = &["--quiet", "--batch", "--decrypt"];

/// Decryptions that fail before any succeeds stop the import: the key,
/// not the entry, is then what is wrong.
const FIRST_FAILURES: usize = 2;

/// An entry read, to import.
#[derive(Debug)]
pub struct Found {
    pub title: Text,
    pub body: Text,
}

/// An entry the store holds but the notebook cannot take, and why.
#[derive(Debug)]
pub struct Skipped {
    pub title: Text,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct Store {
    pub found: Vec<Found>,
    pub skipped: Vec<Skipped>,
}

/// Why nothing was read.
#[derive(Debug, Eq, PartialEq)]
pub enum Stop {
    Cancelled,
    Failed(String),
}

/// One file to decrypt, by its title.
struct Listed {
    title: String,
    path: PathBuf,
}

/// Reads the store at `root`: its entries in title order, each decrypted
/// by `gpg`, with `progress` told how many of how many are done. `admit`
/// is asked about the titles found before anything is decrypted, so a
/// store the notebook has no room for asks for no key. `cancel` ends it,
/// killing the decryption in flight.
pub fn read(
    root: &Path,
    admit: &dyn Fn(&[&str]) -> Result<(), String>,
    cancel: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Store, Stop> {
    read_with(root, admit, cancel, progress, &|| {
        let mut command = Command::new("gpg");
        command.args(GPG);
        command
    })
}

fn read_with(
    root: &Path,
    admit: &dyn Fn(&[&str]) -> Result<(), String>,
    cancel: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, usize),
    gpg: &dyn Fn() -> Command,
) -> Result<Store, Stop> {
    let (listed, mut skipped) = walk(root, cancel)?;
    let titles: Vec<&str> = listed.iter().map(|entry| entry.title.as_str()).collect();
    admit(&titles).map_err(Stop::Failed)?;
    let total = listed.len();
    let mut store = Store {
        found: Vec::with_capacity(total),
        skipped: Vec::new(),
    };
    let mut failures = 0;
    for (done, entry) in listed.into_iter().enumerate() {
        progress(done, total);
        if cancel() {
            return Err(Stop::Cancelled);
        }
        match decrypt(&entry.path, cancel, gpg) {
            Ok(body) => store.found.push(Found {
                title: Text::new(entry.title),
                body,
            }),
            Err(Refusal::Cancelled) => return Err(Stop::Cancelled),
            Err(Refusal::Gpg(text)) => return Err(Stop::Failed(text)),
            Err(Refusal::Entry(reason)) => {
                if reason == UNDECRYPTED && store.found.is_empty() {
                    failures += 1;
                    if failures >= FIRST_FAILURES {
                        return Err(Stop::Failed(format!(
                            "gpg failed on {FIRST_FAILURES} entries before decrypting any, \
                             {} the last: is their key available to gpg-agent?",
                            entry.title
                        )));
                    }
                }
                skipped.push(Skipped {
                    title: Text::new(entry.title),
                    reason,
                });
            }
        }
    }
    progress(total, total);
    store.skipped = skipped;
    Ok(store)
}

/// The store's entries in title order, and those whose name the notebook
/// cannot take. The store's own folder may be a link, as one kept in a
/// synchronized folder often is; below it no link is followed, so the
/// walk stays in the store. A folder below it that cannot be read is
/// skipped with why. `cancel` ends it between names, so a slow folder
/// holds no lock.
fn walk(root: &Path, cancel: &dyn Fn() -> bool) -> Result<(Vec<Listed>, Vec<Skipped>), Stop> {
    let named =
        |path: &Path, error: std::io::Error| Stop::Failed(format!("{}: {error}", path.display()));
    if !fs::metadata(root)
        .map_err(|error| named(root, error))?
        .is_dir()
    {
        return Err(Stop::Failed(format!("{} is not a folder", root.display())));
    }
    let mut listed = Vec::new();
    let mut skipped = Vec::new();
    let mut examined = 0usize;
    // (folder, its path under the store, its depth)
    let mut folders = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((folder, under, depth)) = folders.pop() {
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) if under.is_empty() => return Err(named(&folder, error)),
            Err(_) => {
                skipped.push(skip(&under, "its folder cannot be read"));
                continue;
            }
        };
        for entry in entries {
            if cancel() {
                return Err(Stop::Cancelled);
            }
            examined += 1;
            if examined > EXAMINED {
                return Err(Stop::Failed(format!(
                    "{} holds more than {EXAMINED} files and folders",
                    root.display()
                )));
            }
            let here = if under.is_empty() {
                "."
            } else {
                under.as_str()
            };
            let examine = |error| skip(here, error);
            let Ok(entry) = entry else {
                skipped.push(examine("a name in its folder cannot be read"));
                continue;
            };
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else {
                skipped.push(examine("a name in its folder cannot be examined"));
                continue;
            };
            let lossy = name.to_string_lossy();
            let path = if under.is_empty() {
                lossy.to_string()
            } else {
                format!("{under}/{lossy}")
            };
            if kind.is_dir() {
                if PRUNED.contains(&lossy.as_ref()) {
                    continue;
                }
                if depth + 1 > DEPTH {
                    return Err(Stop::Failed(format!(
                        "{} is more than {DEPTH} folders deep",
                        root.join(&path).display()
                    )));
                }
                if name.to_str().is_none() {
                    skipped.push(skip(&path, "its folder's name is not UTF-8"));
                    continue;
                }
                folders.push((entry.path(), path, depth + 1));
                continue;
            }
            let Some(title) = path.strip_suffix(".gpg") else {
                continue;
            };
            if kind.is_symlink() {
                skipped.push(skip(title, "it is a link, which is not followed"));
            } else if !kind.is_file() {
                skipped.push(skip(title, "it is not a file"));
            } else if name.to_str().is_none() {
                skipped.push(skip(title, "its name is not UTF-8"));
            } else if title.is_empty()
                || title.len() > MAX_TITLE
                || title.chars().any(char::is_control)
            {
                skipped.push(skip(title, "its name cannot be a title"));
            } else {
                if listed.len() >= MAX_ENTRIES {
                    return Err(Stop::Failed(format!(
                        "{} holds more than the notebook's {MAX_ENTRIES} entries",
                        root.display()
                    )));
                }
                listed.push(Listed {
                    title: title.to_owned(),
                    path: entry.path(),
                });
            }
        }
    }
    listed.sort_by(|a, b| a.title.cmp(&b.title));
    skipped.sort_by(|a, b| a.title.as_str().cmp(b.title.as_str()));
    Ok((listed, skipped))
}

fn skip(title: &str, reason: &'static str) -> Skipped {
    Skipped {
        title: Text::new(title.to_owned()),
        reason,
    }
}

const UNDECRYPTED: &str = "gpg could not decrypt it";

/// Why one entry was not read.
#[derive(Debug, Eq, PartialEq)]
enum Refusal {
    Cancelled,
    /// gpg could not run at all: nothing more can be read.
    Gpg(String),
    Entry(&'static str),
}

/// `O_NOFOLLOW | O_NONBLOCK` on Linux x86-64, td's target (AArch64's
/// `O_NOFOLLOW` differs): a link put in an entry's place is not
/// followed, and a FIFO does not wait.
const NOFOLLOW_NONBLOCK: i32 = 0o400000 | 0o4000;

/// Decrypts the entry at `path`: gpg reads the file, opened here, on its
/// standard input and writes the text to a pipe read to the body's bound.
fn decrypt(
    path: &Path,
    cancel: &dyn Fn() -> bool,
    gpg: &dyn Fn() -> Command,
) -> Result<Text, Refusal> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(path)
        .map_err(|_| Refusal::Entry("it cannot be opened"))?;
    let metadata = file
        .metadata()
        .map_err(|_| Refusal::Entry("it cannot be opened"))?;
    if !metadata.is_file() {
        return Err(Refusal::Entry("it is not a file"));
    }
    if metadata.len() > CIPHER {
        return Err(Refusal::Entry("it is larger than an entry can be"));
    }
    let mut command = gpg();
    command
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        Refusal::Gpg(match error.kind() {
            std::io::ErrorKind::NotFound => {
                "gpg is not installed or not on PATH: the password store is decrypted with it"
                    .to_owned()
            }
            _ => format!("gpg cannot start: {error}"),
        })
    })?;
    let Some(stdout) = child.stdout.take() else {
        stop(&mut child);
        return Err(Refusal::Gpg("gpg's output cannot be read".to_owned()));
    };
    let bytes = collect(&mut child, stdout, cancel)?;
    if bytes.len() > MAX_BODY {
        plain::wipe(bytes);
        return Err(Refusal::Entry("its text is larger than an entry can be"));
    }
    match String::from_utf8(bytes) {
        // Kept at its own size: the read's buffer, a body's bound, is
        // cleared rather than held for every entry.
        Ok(body) => {
            let exact = body.as_str().to_owned();
            plain::wipe(body.into_bytes());
            Ok(Text::new(exact))
        }
        Err(error) => {
            plain::wipe(error.into_bytes());
            Err(Refusal::Entry("its text is not UTF-8"))
        }
    }
}

/// Plaintext on its way from the reading thread: cleared when dropped
/// unread, as when the decryption it came from was cancelled.
struct Buffer(Vec<u8>);

impl Buffer {
    fn take(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        plain::wipe(std::mem::take(&mut self.0));
    }
}

/// Reads `child`'s output to one byte past the body's bound on a thread
/// of its own, while this one watches `cancel` and the child's exit. The
/// buffer is allocated whole first, so no plaintext is left behind in a
/// buffer outgrown.
fn collect(
    child: &mut Child,
    stdout: ChildStdout,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<u8>, Refusal> {
    let (sent, output) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("store-read".to_owned())
        .spawn(move || {
            let mut bytes = Vec::with_capacity(MAX_BODY + 1);
            let read = stdout
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut bytes)
                .map(|_| ());
            // A receiver gone, or one that drops it unread, has given the
            // import up: the buffer clears what came.
            let _ = sent.send((Buffer(bytes), read));
        });
    if reader.is_err() {
        stop(child);
        return Err(Refusal::Gpg("td-pass cannot start reading gpg".to_owned()));
    }
    let mut exited: Option<(Instant, bool)> = None;
    loop {
        if cancel() {
            stop(child);
            return Err(Refusal::Cancelled);
        }
        match output.recv_timeout(TICK) {
            Ok((bytes, read)) => {
                let bytes = bytes.take();
                if read.is_err() {
                    stop(child);
                    plain::wipe(bytes);
                    return Err(Refusal::Entry(UNDECRYPTED));
                }
                // Past the bound gpg has nothing more to say that is
                // kept; the caller refuses the text by its length.
                if bytes.len() > MAX_BODY {
                    stop(child);
                    return Ok(bytes);
                }
                // The text counts only once gpg says it decrypted it.
                return match wait(child, cancel) {
                    Ok(true) => Ok(bytes),
                    Ok(false) => {
                        plain::wipe(bytes);
                        Err(Refusal::Entry(UNDECRYPTED))
                    }
                    Err(refusal) => {
                        plain::wipe(bytes);
                        Err(refusal)
                    }
                };
            }
            Err(RecvTimeoutError::Disconnected) => {
                stop(child);
                return Err(Refusal::Entry(UNDECRYPTED));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if exited.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                exited = Some((Instant::now(), status.success()));
            }
        }
        if let Some((at, _)) = exited {
            if at.elapsed() > GRACE {
                // Something gpg started holds its output: the reader is
                // left to the pipe's end, its text, if any, cleared, and
                // the import stops rather than leave one per entry.
                return Err(Refusal::Gpg(
                    "gpg left its output open after it exited; the import stopped".to_owned(),
                ));
            }
        }
    }
}

/// Waits for `child` to exit, watching `cancel`; whether it succeeded.
fn wait(child: &mut Child, cancel: &dyn Fn() -> bool) -> Result<bool, Refusal> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) => {}
            Err(_) => {
                stop(child);
                return Ok(false);
            }
        }
        if cancel() {
            stop(child);
            return Err(Refusal::Cancelled);
        }
        std::thread::sleep(TICK);
    }
}

/// Ends `child`, which this thread started, and reaps it.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A fresh store folder, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("td-pass-store-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str, text: &str) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Whether `sh` runs here; a test that needs it says so and passes.
    fn sh() -> bool {
        let found = Command::new("sh").args(["-c", "true"]).status().is_ok();
        if !found {
            eprintln!("skipped: no sh on PATH");
        }
        found
    }

    /// A stand-in gpg: `script` run by `sh` over the entry on its input.
    fn fake(script: &'static str) -> impl Fn() -> Command {
        move || {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }
    }

    fn titles(store: &Store) -> Vec<&str> {
        store.found.iter().map(|f| f.title.as_str()).collect()
    }

    #[test]
    fn titles_are_paths_under_the_store_in_order_and_links_are_not_followed() {
        let scratch = Scratch::new("walk");
        scratch.file("mail.gpg", "m");
        scratch.file("bank/checking.gpg", "c");
        scratch.file("bank/deep/card.gpg", "d");
        scratch.file(".gpg-id", "KEY");
        scratch.file("notes.txt", "not an entry");
        scratch.file(".git/objects/x.gpg", "pruned");
        scratch.file(".extensions/y.gpg", "pruned");
        std::os::unix::fs::symlink(scratch.0.join("mail.gpg"), scratch.0.join("alias.gpg"))
            .unwrap();
        std::os::unix::fs::symlink(scratch.0.join("bank"), scratch.0.join("linked")).unwrap();
        let (listed, skipped) = walk(&scratch.0, &|| false).unwrap();
        let listed: Vec<_> = listed.iter().map(|l| l.title.as_str()).collect();
        assert_eq!(listed, ["bank/checking", "bank/deep/card", "mail"]);
        let skipped: Vec<_> = skipped
            .iter()
            .map(|s| (s.title.as_str(), s.reason))
            .collect();
        assert_eq!(skipped, [("alias", "it is a link, which is not followed")]);
        assert!(walk(&scratch.0.join("mail.gpg"), &|| false).is_err());
        assert!(walk(&scratch.0.join("absent"), &|| false).is_err());
        // A cancel ends the walk between names.
        assert_eq!(walk(&scratch.0, &|| true).err(), Some(Stop::Cancelled));
    }

    #[test]
    fn names_the_notebook_cannot_take_are_skipped_and_a_store_too_large_refused() {
        let scratch = Scratch::new("names");
        // A name may be 255 bytes; a title past 512 takes folders.
        let long = ["a", "b", "c"].map(|part| part.repeat(200)).join("/");
        assert!(long.len() > MAX_TITLE);
        scratch.file(&format!("{long}.gpg"), "x");
        scratch.file("tab\there.gpg", "x");
        scratch.file(".gpg", "x");
        scratch.file("fine.gpg", "x");
        let (listed, skipped) = walk(&scratch.0, &|| false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(skipped.len(), 3);
        assert!(skipped
            .iter()
            .all(|s| s.reason == "its name cannot be a title"));
        let many = Scratch::new("many");
        for n in 0..=MAX_ENTRIES {
            many.file(&format!("{n}.gpg"), "x");
        }
        match walk(&many.0, &|| false).err() {
            Some(Stop::Failed(text)) => assert!(text.contains("1024 entries"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_entry_is_decrypted_from_its_file_to_its_body() {
        if !sh() {
            return;
        }
        let scratch = Scratch::new("read");
        scratch.file("mail.gpg", "user: a\npass: b\n");
        scratch.file("bank/pin.gpg", "1234");
        let seen = Cell::new(Vec::new());
        let mut progress = |done, total| {
            let mut all = seen.take();
            all.push((done, total));
            seen.set(all);
        };
        let store = read_with(
            &scratch.0,
            &|_| Ok(()),
            &|| false,
            &mut progress,
            &fake("cat"),
        )
        .unwrap();
        assert_eq!(titles(&store), ["bank/pin", "mail"]);
        assert_eq!(store.found[1].body.as_str(), "user: a\npass: b\n");
        assert!(store.skipped.is_empty());
        assert_eq!(seen.take(), [(0, 2), (1, 2), (2, 2)]);
    }

    #[test]
    fn text_too_large_or_not_utf8_is_skipped_and_failures_before_any_success_stop() {
        if !sh() {
            return;
        }
        let scratch = Scratch::new("refused");
        scratch.file("a.gpg", "big");
        scratch.file("b.gpg", "ok");
        scratch.file("c.gpg", "bad");
        scratch.file("d.gpg", "fail");
        // Each file's text names what the stand-in does with it.
        let script = "read what; case $what in \
            big) head -c 70000 /dev/zero | tr '\\0' x ;; \
            ok) echo fine ;; \
            bad) printf '\\377' ;; \
            *) exit 2 ;; esac";
        let store = read_with(
            &scratch.0,
            &|_| Ok(()),
            &|| false,
            &mut |_, _| {},
            &fake(script),
        )
        .unwrap();
        assert_eq!(titles(&store), ["b"]);
        assert_eq!(store.found[0].body.as_str(), "fine\n");
        let skipped: Vec<_> = store
            .skipped
            .iter()
            .map(|s| (s.title.as_str(), s.reason))
            .collect();
        assert_eq!(
            skipped,
            [
                ("a", "its text is larger than an entry can be"),
                ("c", "its text is not UTF-8"),
                ("d", UNDECRYPTED),
            ]
        );
        // Two failures before any success: the key is what is wrong.
        let none = Scratch::new("none");
        none.file("a.gpg", "x");
        none.file("b.gpg", "x");
        none.file("c.gpg", "x");
        match read_with(
            &none.0,
            &|_| Ok(()),
            &|| false,
            &mut |_, _| {},
            &fake("exit 2"),
        ) {
            Err(Stop::Failed(text)) => assert!(text.contains("gpg-agent"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_missing_gpg_stops_the_import_and_a_cancel_ends_a_decryption() {
        let scratch = Scratch::new("cancel");
        scratch.file("a.gpg", "x");
        let absent = || Command::new("/nonexistent/td-pass-gpg");
        match read_with(&scratch.0, &|_| Ok(()), &|| false, &mut |_, _| {}, &absent) {
            Err(Stop::Failed(text)) => assert!(text.contains("gpg"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            read_with(&scratch.0, &|_| Ok(()), &|| true, &mut |_, _| {}, &absent).unwrap_err(),
            Stop::Cancelled
        );
        if !sh() {
            return;
        }
        let started = Instant::now();
        let calls = Cell::new(0);
        let cancel = || {
            calls.set(calls.get() + 1);
            calls.get() > 5
        };
        assert_eq!(
            read_with(
                &scratch.0,
                &|_| Ok(()),
                &cancel,
                &mut |_, _| {},
                &fake("sleep 30")
            )
            .unwrap_err(),
            Stop::Cancelled
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_linked_store_is_read_and_folders_that_cannot_be_read_are_skipped() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = Scratch::new("linked");
        scratch.file("real/mail.gpg", "m");
        scratch.file("real/locked/x.gpg", "x");
        let link = scratch.0.join("store");
        std::os::unix::fs::symlink(scratch.0.join("real"), &link).unwrap();
        // A name that is not UTF-8, a file's and a folder's.
        let odd = std::ffi::OsStr::from_bytes(b"\xff.gpg");
        let odd_folder = std::ffi::OsStr::from_bytes(b"\xfe");
        let names = fs::write(scratch.0.join("real").join(odd), "o").is_ok()
            && fs::create_dir(scratch.0.join("real").join(odd_folder)).is_ok();
        let locked = scratch.0.join("real/locked");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // As root the folder still reads; then only the link is shown.
        let unreadable = fs::read_dir(&locked).is_err();
        let walked = walk(&link, &|| false);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let (listed, skipped) = walked.unwrap();
        let listed: Vec<_> = listed.iter().map(|l| l.title.as_str()).collect();
        let skipped: Vec<_> = skipped
            .iter()
            .map(|s| (s.title.as_str().to_owned(), s.reason))
            .collect();
        if unreadable {
            assert_eq!(listed, ["mail"]);
            assert!(skipped.contains(&("locked".to_owned(), "its folder cannot be read")));
        }
        if names {
            assert!(skipped.contains(&("\u{fffd}".to_owned(), "its name is not UTF-8")));
            assert!(skipped.contains(&("\u{fffd}".to_owned(), "its folder's name is not UTF-8")));
        }
    }

    #[test]
    fn a_store_the_notebook_has_no_room_for_decrypts_nothing() {
        let scratch = Scratch::new("room");
        scratch.file("a.gpg", "x");
        scratch.file("b.gpg", "x");
        let asked = Cell::new(Vec::new());
        let admit = |titles: &[&str]| {
            asked.set(titles.iter().map(|t| (*t).to_owned()).collect());
            Err("no room".to_owned())
        };
        let progressed = Cell::new(false);
        let absent = || Command::new("/nonexistent/td-pass-gpg");
        let read = read_with(
            &scratch.0,
            &admit,
            &|| false,
            &mut |_, _| progressed.set(true),
            &absent,
        );
        assert_eq!(read.unwrap_err(), Stop::Failed("no room".to_owned()));
        assert_eq!(asked.take(), ["a", "b"]);
        assert!(!progressed.get());
    }

    #[test]
    fn a_link_put_in_an_entrys_place_is_not_followed() {
        if !sh() {
            return;
        }
        let scratch = Scratch::new("swap");
        scratch.file("elsewhere", "outside the store");
        std::os::unix::fs::symlink(scratch.0.join("elsewhere"), scratch.0.join("entry.gpg"))
            .unwrap();
        let refused = decrypt(&scratch.0.join("entry.gpg"), &|| false, &fake("cat"));
        assert_eq!(refused.unwrap_err(), Refusal::Entry("it cannot be opened"));
    }

    #[test]
    fn a_gpg_that_leaves_its_output_open_stops_the_import() {
        if !sh() {
            return;
        }
        let scratch = Scratch::new("held");
        scratch.file("a.gpg", "x");
        let started = Instant::now();
        let read = read_with(
            &scratch.0,
            &|_| Ok(()),
            &|| false,
            &mut |_, _| {},
            &fake("cat; sleep 30 &"),
        );
        match read {
            Err(Stop::Failed(text)) => assert!(text.contains("left its output open"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
