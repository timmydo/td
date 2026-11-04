//! The directory chooser (DESIGN.md §7): td-ui's finder over the human's
//! folders, modal over the window's body, as the model picker is, for a
//! conversation in a directory. Return or a second press enters the
//! selected folder; Backspace with no filter, or Alt+Up, goes up;
//! Control+Return chooses the folder listed; Escape closes it with nothing
//! chosen. Only folders are listed, a link to one marked `link` and a
//! repository's top marked `git`, which admission refuses; hidden names
//! are left out. What is chosen is admitted by the broker
//! (`workspace::admit_directory`), not here.
//!
//! The folders are the broker's, on its machine: `FolderReader` reads them
//! there and the `Chooser`, in the window, reads no disk. It shows the
//! `Folders` it is told, which may have come from far away, so `listing` holds
//! them to the finder's bounds and takes nothing on trust.

use std::collections::BinaryHeap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use td_ui::chrome::List;
use td_ui::finder::{self, Choice, Choose, Controller, Entry, Kind, Listing, Outcome};
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::window::{Input, PointerPhase};

/// The status row's standing note.
pub const NOTE: &str = "Return enters, Backspace goes up, Ctrl+Return works here, Escape cancels";
/// How soon a second press on a row enters it, as the picker's does.
const DOUBLE_PRESS: Duration = Duration::from_millis(400);
/// The most directory entries one listing reads, as td-mail's finder.
const EXAMINED: usize = 16 * finder::ENTRIES;
/// The most folders one request may have the broker try.
const CANDIDATES: usize = 16;

/// One folder as the broker read it, for a window to list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Folders {
    /// The path it read.
    pub folder: PathBuf,
    /// Each folder in it, by name and what marks it, in order.
    pub entries: Vec<(String, String)>,
    /// The read stopped short of the whole folder.
    pub truncated: bool,
}

impl Folders {
    /// The finder's listing of them, or what is wrong with them: nothing here
    /// is taken on trust, a name least, which what is chosen joins to the folder.
    pub fn listing(&self) -> Result<Listing, String> {
        let shown = self.folder.display();
        let named = |e: finder::Error| format!("{shown}: {e}");
        if !self.folder.is_absolute() {
            return Err(format!("{shown} is not an absolute path"));
        }
        let text = self
            .folder
            .to_str()
            .ok_or_else(|| format!("{shown} is not text"))?;
        let mut entries = Vec::new();
        for (name, meta) in &self.entries {
            if name.contains('/') || name == "." || name == ".." {
                return Err(format!("{shown}: {name:?} is not a folder's name"));
            }
            entries.push(Entry::new(name, meta, Kind::Folder, true).map_err(named)?);
        }
        Listing::new(text, entries, self.truncated).map_err(named)
    }
}

/// What the broker answers to a `Request::ListFolders`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldersAnswer {
    /// The folders it was asked to try, said back: what the folder read may
    /// not say, as it may be another and a refusal has none, is which
    /// question this answers.
    pub asked: Vec<PathBuf>,
    /// The entry to select: the folder an ascent came from.
    pub from: Option<String>,
    pub read: Result<Folders, String>,
}

/// Reads for the chooser on a thread, one read at a time: a folder can hang, as
/// over a dead mount, and the broker's loop must not, nor must threads pile up on
/// it. One that never answers goes when the process does, or when the mount does
/// should the kernel hold it past that.
#[derive(Default)]
pub struct FolderReader {
    reading: Option<Reading>,
}

struct Reading {
    asked: Vec<PathBuf>,
    from: Option<String>,
    answer: std::sync::mpsc::Receiver<Result<Folders, String>>,
}

impl FolderReader {
    /// Starts on what was asked, or answers at once if it cannot.
    pub fn ask(
        &mut self,
        candidates: Vec<PathBuf>,
        or_home: bool,
        from: Option<String>,
        home: Option<PathBuf>,
    ) -> Option<FoldersAnswer> {
        let asked = candidates.clone();
        self.start(asked, from, move || {
            first_readable(&candidates, or_home, home)
        })
    }

    fn start(
        &mut self,
        asked: Vec<PathBuf>,
        from: Option<String>,
        read: impl FnOnce() -> Result<Folders, String> + Send + 'static,
    ) -> Option<FoldersAnswer> {
        if let Some(reading) = &self.reading {
            let shown = reading.asked.first().map_or_else(
                || "the home folder".to_string(),
                |at| at.display().to_string(),
            );
            return Some(FoldersAnswer {
                asked,
                from,
                read: Err(format!(
                    "{shown} is still being read and has not answered, so nothing else can be listed yet"
                )),
            });
        }
        let (tell, answer) = std::sync::mpsc::channel();
        let started = std::thread::Builder::new()
            .name("td-agent-folders".into())
            .spawn(move || {
                let _ = tell.send(read());
            });
        match started {
            Ok(_) => {
                self.reading = Some(Reading {
                    asked,
                    from,
                    answer,
                });
                None
            }
            Err(e) => Some(FoldersAnswer {
                asked,
                from,
                read: Err(format!("a thread to read folders on: {e}")),
            }),
        }
    }

    /// What has been read since last asked, if anything.
    pub fn answered(&mut self) -> Option<FoldersAnswer> {
        let read = match self.reading.as_ref()?.answer.try_recv() {
            Ok(read) => read,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("the read of those folders ended without an answer".to_string())
            }
        };
        let Reading { asked, from, .. } = self.reading.take()?;
        Some(FoldersAnswer { asked, from, read })
    }
}

/// The broker's answer: the first of `candidates` it can read, then, when
/// `or_home` says so, `home` when absolute and then the root.
pub(crate) fn first_readable(
    candidates: &[PathBuf],
    or_home: bool,
    home: Option<PathBuf>,
) -> Result<Folders, String> {
    if candidates.len() > CANDIDATES {
        return Err(format!(
            "{} folders were asked for at once, past the {CANDIDATES} tried",
            candidates.len()
        ));
    }
    if let Some(relative) = candidates.iter().find(|folder| !folder.is_absolute()) {
        return Err(format!("{} is not an absolute path", relative.display()));
    }
    // A folder gone since is not a dead end, when opening.
    let onward = or_home.then(|| {
        home.into_iter()
            .filter(|home| home.is_absolute())
            .chain([PathBuf::from("/")])
    });
    let mut why = "no folder was asked for".to_string();
    for folder in candidates
        .iter()
        .cloned()
        .chain(onward.into_iter().flatten())
    {
        match list_folders(&folder) {
            Ok(folders) => return Ok(folders),
            Err(e) => why = e,
        }
    }
    Err(why)
}

/// The folders in `path`, sorted by name with case aside: at most
/// `finder::ENTRIES` of the first `EXAMINED` entries read, cut short, and
/// said to be, past either or past `finder::LISTING_BYTES`. A link is
/// followed to learn whether it is a folder.
pub fn list_folders(path: &Path) -> Result<Folders, String> {
    let named = |e: std::io::Error| format!("{}: {e}", path.display());
    // (the name with case aside, the name, a link, a repository): a
    // max-heap keeps the first ENTRIES in that order.
    let mut kept: BinaryHeap<(String, String, bool, bool)> = BinaryHeap::new();
    let mut truncated = false;
    for (seen, entry) in fs::read_dir(path).map_err(named)?.enumerate() {
        if seen >= EXAMINED {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.')
            || name.len() > finder::NAME_BYTES
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let folder = if kind.is_symlink() {
            fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
        } else {
            kind.is_dir()
        };
        if !folder {
            continue;
        }
        let repository = crate::workspace::is_repository(&entry.path());
        kept.push((name.to_lowercase(), name, kind.is_symlink(), repository));
        if kept.len() > finder::ENTRIES {
            kept.pop();
            truncated = true;
        }
    }
    let mut entries = Vec::with_capacity(kept.len());
    let mut bytes = 0usize;
    for (_, name, link, repository) in kept.into_sorted_vec() {
        let meta = match (link, repository) {
            (true, true) => "git, link",
            (true, false) => "link",
            (false, true) => "git",
            (false, false) => "",
        };
        if Entry::new(&name, meta, Kind::Folder, true).is_err() {
            continue;
        }
        let next = bytes.saturating_add(name.len() + meta.len());
        if next > finder::LISTING_BYTES {
            truncated = true;
            break;
        }
        bytes = next;
        entries.push((name, meta.to_string()));
    }
    let folders = Folders {
        folder: path.to_path_buf(),
        entries,
        truncated,
    };
    // Said now rather than by the window that could not list it.
    folders.listing()?;
    Ok(folders)
}

/// What the chooser made of an input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    /// Still open; whether it needs a paint.
    Stay(bool),
    /// Closed with nothing chosen, or unable to go on.
    Closed,
    /// Closed on this folder.
    Chosen(PathBuf),
    /// Still open, showing what it had, and wanting this folder listed by
    /// the broker, selecting `from`: `answered` says what came of it.
    List {
        folder: PathBuf,
        from: Option<String>,
    },
}

#[derive(Debug)]
pub struct Chooser {
    finder: Controller,
    surface: Surface,
    press: Option<(Instant, usize)>,
    /// The folder listed.
    folder: PathBuf,
}

impl Chooser {
    /// The chooser inside `rect`, listing `folders`, `select` selected.
    pub fn open(
        surface: Surface,
        rect: Rect,
        folders: &Folders,
        select: Option<&str>,
    ) -> Result<Self, String> {
        let listing = folders.listing()?;
        let mut finder = Controller::new(listing, Choose::Folder, surface, rect, select)
            .map_err(|e| e.to_string())?;
        let _ = finder.set_note(NOTE);
        Ok(Self {
            finder,
            surface,
            press: None,
            folder: folders.folder.clone(),
        })
    }

    /// The folder listed.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Its folders, in their order.
    pub fn listing(&self) -> &Listing {
        self.finder.listing()
    }

    /// Lays the chooser out in `rect` of `surface`; false when the finder
    /// cannot fit there, and it is to be closed.
    pub fn resize(&mut self, surface: Surface, rect: Rect) -> bool {
        self.surface = surface;
        !matches!(
            self.finder.event(finder::Event::Resize { surface, rect }),
            Outcome::Closed(_)
        )
    }

    /// What came of a `Reply::List`: the folder listed, selecting `select`; a
    /// folder that cannot be listed stays unlisted, its reason on the status
    /// row.
    pub fn answered(&mut self, read: Result<Folders, String>, select: Option<&str>) {
        // What was pressed was in another listing.
        self.press = None;
        let installed = read.and_then(|folders| {
            let listing = folders.listing()?;
            self.finder
                .set_listing(listing, select)
                .map_err(|e| format!("{}: {e}", folders.folder.display()))?;
            Ok(folders.folder)
        });
        match installed {
            Ok(folder) => {
                self.folder = folder;
                let _ = self.finder.set_note(NOTE);
            }
            Err(e) => self.say(&e),
        }
    }

    /// The list's place and which row is first in it, for a test.
    #[cfg(test)]
    pub(crate) fn list_rect(&self) -> Rect {
        self.finder.list_rect()
    }

    #[cfg(test)]
    pub(crate) fn first(&self) -> usize {
        self.finder.first()
    }

    /// `folder` has been asked for, which may be a while.
    pub fn reading(&mut self, folder: &Path) {
        self.say(&format!("reading {}", folder.display()));
    }

    /// Says `text` on the status row, which refuses some of what it may hold.
    fn say(&mut self, text: &str) {
        let text = crate::tools::visible(text);
        let mut cut = text.len().min(finder::NOTE_BYTES);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let _ = self.finder.set_note(text.get(..cut).unwrap_or_default());
    }

    /// An input, which is the chooser's while it is open: its keys, every
    /// other chord consumed, the pointer and the wheel over its list. A
    /// resize, a focus change and a paste are the window's.
    pub fn input(&mut self, input: &Input<'_>) -> Reply {
        let key = |key, repeated| finder::Event::Key { key, repeated };
        let event = match *input {
            Input::Key { chord, repeat } => match chord {
                "Up" => key(finder::Key::Up, repeat),
                "Down" => key(finder::Key::Down, repeat),
                "PageUp" => key(finder::Key::PageUp, repeat),
                "PageDown" => key(finder::Key::PageDown, repeat),
                "Home" => key(finder::Key::Home, repeat),
                "End" => key(finder::Key::End, repeat),
                "Return" => key(finder::Key::Activate, repeat),
                "C-Return" => key(finder::Key::Accept, repeat),
                "Backspace" => key(finder::Key::Backspace, repeat),
                "M-Up" => key(finder::Key::Parent, repeat),
                "Escape" => key(finder::Key::Escape, repeat),
                "Space" => finder::Event::Insert(' '),
                _ => {
                    let mut chars = chord.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if !c.is_control() => finder::Event::Insert(c),
                        _ => finder::Event::Other,
                    }
                }
            },
            Input::Pointer { phase, x, y, .. } => match phase {
                PointerPhase::Press => finder::Event::Press { x, y },
                PointerPhase::Move => finder::Event::Move { x, y },
                PointerPhase::Release => finder::Event::Release { x, y },
            },
            Input::Wheel { rows, .. } => {
                let list = self.finder.list_rect();
                finder::Event::Wheel {
                    x: list.x,
                    y: list.y,
                    rows,
                }
            }
            Input::Resize(_)
            | Input::Focus(_)
            | Input::Paste(_)
            | Input::PasteImage { .. }
            | Input::CancelPointer
            | Input::Hover(_)
            | Input::Context { .. }
            | Input::Close => return Reply::Stay(false),
        };
        let hit = match event {
            finder::Event::Press { x, y } => List::new(self.surface, self.finder.list_rect())
                .and_then(|rows| rows.hit(x, y))
                .and_then(|row| self.finder.first().checked_add(row))
                .and_then(|position| self.finder.shown().get(position).copied()),
            _ => None,
        };
        let mut outcome = self.finder.event(event);
        match event {
            finder::Event::Press { .. } => {
                let now = Instant::now();
                let again = hit.is_some()
                    && self.press.is_some_and(|(at, entry)| {
                        Some(entry) == hit && now.duration_since(at) <= DOUBLE_PRESS
                    });
                self.press = if again {
                    None
                } else {
                    hit.map(|entry| (now, entry))
                };
                if again {
                    outcome = self.finder.event(key(finder::Key::Activate, false));
                }
            }
            finder::Event::Move { .. } | finder::Event::Release { .. } => {}
            _ => self.press = None,
        }
        match outcome {
            Outcome::Ignored | Outcome::Consumed => Reply::Stay(false),
            Outcome::Changed => Reply::Stay(true),
            Outcome::Descend(index) => {
                let Some(name) = self
                    .finder
                    .listing()
                    .entries()
                    .get(index)
                    .map(|entry| entry.name().to_string())
                else {
                    return Reply::Stay(false);
                };
                Reply::List {
                    folder: self.folder.join(name),
                    from: None,
                }
            }
            Outcome::Ascend => {
                let Some((parent, from)) = self.folder.parent().and_then(|parent| {
                    let from = self.folder.file_name()?.to_str()?.to_string();
                    Some((parent.to_path_buf(), from))
                }) else {
                    return Reply::Stay(false);
                };
                Reply::List {
                    folder: parent,
                    from: Some(from),
                }
            }
            Outcome::Closed(Choice::Here) => Reply::Chosen(self.folder.clone()),
            Outcome::Closed(Choice::Entry(index)) => {
                match self.finder.listing().entries().get(index) {
                    Some(entry) => Reply::Chosen(self.folder.join(entry.name())),
                    None => Reply::Closed,
                }
            }
            Outcome::Closed(Choice::Cancelled | Choice::Unavailable(_)) => Reply::Closed,
        }
    }

    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        self.finder.emit(damage, sink);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use td_ui::raster::Scale;

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: 640,
        }
    }

    fn key(chooser: &mut Chooser, chord: &str) -> Reply {
        chooser.input(&Input::Key {
            chord,
            repeat: false,
        })
    }

    fn names(listing: &Listing) -> Vec<(&str, &str)> {
        listing
            .entries()
            .iter()
            .map(|entry| (entry.name(), entry.meta()))
            .collect()
    }

    /// The broker's part of `reply`, which is to list a folder, done.
    fn followed(chooser: &mut Chooser, reply: Reply) {
        let Reply::List { folder, from } = reply else {
            panic!("asked for no folder: {reply:?}");
        };
        chooser.answered(list_folders(&folder), from.as_deref());
    }

    fn folders(folder: &str, entries: &[&str]) -> Folders {
        Folders {
            folder: folder.into(),
            entries: entries
                .iter()
                .map(|name| (name.to_string(), String::new()))
                .collect(),
            truncated: false,
        }
    }

    /// It is told what to list, and asks for what is next; what it had stays
    /// until the answer, and stays when the answer is that it cannot be read.
    #[test]
    fn it_lists_what_it_is_told_and_stays_until_told_more() {
        // Folders of no disk here.
        let there = folders("/far/away", &["one", "two"]);
        let mut chooser = Chooser::open(surface(), rect(), &there, Some("two")).unwrap();
        assert_eq!(chooser.folder(), Path::new("/far/away"));
        assert_eq!(chooser.finder.selected_entry().unwrap().name(), "two");
        let reply = key(&mut chooser, "Return");
        assert_eq!(
            reply,
            Reply::List {
                folder: "/far/away/two".into(),
                from: None
            }
        );
        assert_eq!(chooser.folder(), Path::new("/far/away"));
        assert_eq!(names(chooser.listing()).len(), 2);

        chooser.answered(Err("/far/away/two: no\nway".into()), None);
        assert_eq!(chooser.folder(), Path::new("/far/away"));
        assert_eq!(names(chooser.listing()).len(), 2);
        assert!(chooser.finder.note().contains("/far/away/two: no"));

        chooser.answered(Ok(folders("/far/away/two", &["in"])), None);
        assert_eq!(chooser.folder(), Path::new("/far/away/two"));
        assert_eq!(names(chooser.listing()), [("in", "")]);
        assert_eq!(chooser.finder.note(), NOTE);

        assert_eq!(
            key(&mut chooser, "M-Up"),
            Reply::List {
                folder: "/far/away".into(),
                from: Some("two".into())
            }
        );
        chooser.answered(Ok(there), Some("two"));
        assert_eq!(chooser.finder.selected_entry().unwrap().name(), "two");
    }

    /// What may have come from far away is a note if it is wrong, never a
    /// folder to join a path to.
    #[test]
    fn what_a_far_broker_says_of_folders_is_not_taken_on_trust() {
        let long = "x".repeat(finder::NAME_BYTES + 1);
        let bad = [
            folders("relative", &["a"]),
            folders("", &["a"]),
            folders("/a\nb", &["a"]),
            folders("/a", &["../up"]),
            folders("/a", &["in/side"]),
            folders("/a", &["."]),
            folders("/a", &[".."]),
            folders("/a", &[""]),
            folders("/a", &["a\nb"]),
            folders("/a", &[&long]),
            Folders {
                folder: PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff])),
                entries: Vec::new(),
                truncated: false,
            },
            Folders {
                entries: vec![("a".into(), "m".repeat(finder::META_BYTES + 1))],
                ..folders("/a", &[])
            },
            Folders {
                entries: vec![("a".into(), String::new()); finder::ENTRIES + 1],
                ..folders("/a", &[])
            },
        ];
        let good = folders("/good", &["a"]);
        let mut chooser = Chooser::open(surface(), rect(), &good, None).unwrap();
        for folders in bad {
            assert!(
                Chooser::open(surface(), rect(), &folders, None).is_err(),
                "{folders:?}"
            );
            chooser.answered(Ok(folders.clone()), None);
            assert_eq!(chooser.folder(), Path::new("/good"), "{folders:?}");
            assert_ne!(chooser.finder.note(), NOTE, "{folders:?}");
            chooser.answered(Ok(good.clone()), None);
        }
    }

    /// The candidates in order; home and the root only when asked for; what
    /// is refused is said, and what it was asked is said back.
    #[test]
    fn the_broker_answers_the_first_it_can_read_or_why_not() {
        let owned = td_fs::TempDir::new("chooser-answer").unwrap();
        let base = fs::canonicalize(owned.path()).unwrap();
        let (home, there) = (base.join("home"), base.join("there"));
        for dir in [&home, &there] {
            fs::create_dir(dir).unwrap();
        }
        let gone = base.join("gone");
        let read_of = |candidates: &[&Path], or_home, home: Option<&Path>| {
            let candidates: Vec<PathBuf> = candidates.iter().map(|c| c.to_path_buf()).collect();
            let answer = FoldersAnswer {
                read: first_readable(&candidates, or_home, home.map(Path::to_path_buf)),
                asked: candidates.clone(),
                from: Some("from".into()),
            };
            assert_eq!(answer.asked, candidates);
            assert_eq!(answer.from.as_deref(), Some("from"));
            answer.read
        };
        let folder_of = |read: Result<Folders, String>| read.unwrap().folder;
        assert_eq!(folder_of(read_of(&[&there], false, None)), there);
        assert_eq!(folder_of(read_of(&[&gone, &there], false, None)), there);
        assert_eq!(folder_of(read_of(&[&gone], true, Some(&home))), home);
        assert_eq!(folder_of(read_of(&[], true, Some(&home))), home);
        // A home that is not absolute is passed over for the root.
        assert_eq!(
            folder_of(read_of(&[&gone], true, Some(Path::new("home")))),
            Path::new("/")
        );
        assert_eq!(folder_of(read_of(&[&gone], true, None)), Path::new("/"));
        // Not asked to go on, it says why not.
        let why = read_of(&[&gone], false, Some(&home)).unwrap_err();
        assert!(why.contains(gone.to_str().unwrap()), "{why}");
        assert!(read_of(&[], false, Some(&home)).is_err());
        // A file is no folder; a relative path is refused however it is asked.
        let file = base.join("file");
        fs::write(&file, "").unwrap();
        assert!(read_of(&[&file], false, None).is_err());
        for or_home in [false, true] {
            let why = read_of(&[Path::new("here")], or_home, Some(&home)).unwrap_err();
            assert!(why.contains("here is not an absolute path"), "{why}");
            let why = read_of(&[&there, Path::new("here")], or_home, None).unwrap_err();
            assert!(why.contains("not an absolute path"), "{why}");
        }
        // However many are asked for, few are tried.
        let many = vec![gone.as_path(); CANDIDATES + 1];
        let why = read_of(&many, true, Some(&home)).unwrap_err();
        assert!(why.contains("at once"), "{why}");
        let mut many = vec![gone.as_path(); CANDIDATES - 1];
        many.push(&there);
        assert_eq!(folder_of(read_of(&many, false, None)), there);
    }

    #[test]
    fn folders_are_listed_entered_left_and_chosen() {
        let base = std::env::temp_dir().join(format!(
            "td-agent-chooser-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        for dir in ["Beta/inner", "alpha", ".hidden", "repo/.git"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::write(base.join("file"), "").unwrap();
        std::os::unix::fs::symlink(base.join("alpha"), base.join("to-alpha")).unwrap();
        std::os::unix::fs::symlink(base.join("file"), base.join("to-file")).unwrap();
        let base = fs::canonicalize(&base).unwrap();

        let listed = list_folders(&base).unwrap();
        assert!(!listed.truncated);
        assert_eq!(
            names(&listed.listing().unwrap()),
            [
                ("alpha", ""),
                ("Beta", ""),
                ("repo", "git"),
                ("to-alpha", "link")
            ]
        );
        let mut chooser = Chooser::open(surface(), rect(), &listed, None).unwrap();
        // Into Beta, by filter and Return; its one folder; back up with
        // Beta selected.
        for c in ["b", "e"] {
            key(&mut chooser, c);
        }
        let reply = key(&mut chooser, "Return");
        followed(&mut chooser, reply);
        assert_eq!(chooser.folder(), base.join("Beta"));
        assert_eq!(names(chooser.listing()), [("inner", "")]);
        let reply = key(&mut chooser, "Backspace");
        followed(&mut chooser, reply);
        assert_eq!(chooser.folder(), base);
        assert_eq!(chooser.finder.selected_entry().unwrap().name(), "Beta");
        // Ctrl+Return chooses the folder listed; the root has no parent.
        assert_eq!(key(&mut chooser, "C-Return"), Reply::Chosen(base.clone()));

        let mut chooser = Chooser::open(surface(), rect(), &listed, None).unwrap();
        assert_eq!(key(&mut chooser, "Escape"), Reply::Closed);
        assert!(list_folders(&base.join("gone")).is_err());
        let root = folders("/", &["a"]);
        let mut chooser = Chooser::open(surface(), rect(), &root, None).unwrap();
        assert_eq!(key(&mut chooser, "M-Up"), Reply::Stay(false));
        fs::remove_dir_all(&base).unwrap();
    }

    /// A folder that hangs holds up its own thread and nothing else, and no more
    /// threads join it.
    #[test]
    fn a_read_that_hangs_stops_nothing_and_is_the_only_one() {
        let mut reader = FolderReader::default();
        assert!(reader.answered().is_none());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let dead = vec![PathBuf::from("/dead")];
        let started = reader.start(dead.clone(), Some("x".into()), move || {
            let _ = wait.recv();
            Ok(folders("/dead", &[]))
        });
        assert!(started.is_none());
        assert!(reader.answered().is_none());
        let refused = reader
            .ask(vec![PathBuf::from("/other")], false, None, None)
            .unwrap();
        assert_eq!(refused.asked, [PathBuf::from("/other")]);
        assert!(refused.read.unwrap_err().contains("/dead"));
        release.send(()).unwrap();
        let answer = loop {
            match reader.answered() {
                Some(answer) => break answer,
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        };
        assert_eq!(answer.asked, dead);
        assert_eq!(answer.from.as_deref(), Some("x"));
        assert_eq!(answer.read.unwrap().folder, PathBuf::from("/dead"));
        assert!(reader.answered().is_none());
    }

    #[test]
    fn a_read_that_ends_without_an_answer_is_said_and_another_can_start() {
        let mut reader = FolderReader::default();
        let dead = vec![PathBuf::from("/dead")];
        reader.start(dead.clone(), None, || {
            std::panic::resume_unwind(Box::new("fixture"))
        });
        let answer = loop {
            match reader.answered() {
                Some(answer) => break answer,
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        };
        assert_eq!(answer.asked, dead);
        assert!(answer.read.is_err());
        assert!(reader
            .ask(vec![PathBuf::from("/")], false, None, None)
            .is_none());
    }

    /// Two presses, each in its own listing, are not a double press.
    #[test]
    fn a_press_does_not_carry_over_to_the_next_listing() {
        let mut chooser = Chooser::open(surface(), rect(), &folders("/a", &["b"]), None).unwrap();
        chooser.press = Some((Instant::now(), 0));
        chooser.answered(Ok(folders("/a/b", &["c"])), None);
        assert!(chooser.press.is_none());
    }
}
