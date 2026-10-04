//! The directory chooser (DESIGN.md §7): td-ui's finder over the human's
//! folders, modal over the window's body, as the model picker is, for a
//! conversation in a directory. Return or a second press enters the
//! selected folder; Backspace with no filter, or Alt+Up, goes up;
//! Control+Return chooses the folder listed; Escape closes it with nothing
//! chosen. Only folders are listed, a link to one marked `link` and a
//! repository's top marked `git`, which admission refuses; hidden names
//! are left out. What is chosen is admitted by the window
//! (`workspace::admit_directory`), not here.

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

/// The folders in `path`, sorted by name with case aside: at most
/// `finder::ENTRIES` of the first `EXAMINED` entries read, cut short, and
/// said to be, past either or past `finder::LISTING_BYTES`. A link is
/// followed to learn whether it is a folder.
pub fn list_folders(path: &Path) -> Result<Listing, String> {
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
        let Ok(entry) = Entry::new(&name, meta, Kind::Folder, true) else {
            continue;
        };
        let next = bytes.saturating_add(name.len() + meta.len());
        if next > finder::LISTING_BYTES {
            truncated = true;
            break;
        }
        bytes = next;
        entries.push(entry);
    }
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not text", path.display()))?;
    Listing::new(text, entries, truncated).map_err(|e| format!("{}: {e}", path.display()))
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
    /// The chooser inside `rect`, listing `start`.
    pub fn open(surface: Surface, rect: Rect, start: &Path) -> Result<Self, String> {
        let listing = list_folders(start)?;
        let mut finder = Controller::new(listing, Choose::Folder, surface, rect, None)
            .map_err(|e| format!("the directory chooser: {e}"))?;
        let _ = finder.set_note(NOTE);
        Ok(Self {
            finder,
            surface,
            press: None,
            folder: start.to_path_buf(),
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

    /// Lists `folder`, selecting `select`; a folder that cannot be listed
    /// stays unlisted, its reason on the status row.
    fn list(&mut self, folder: PathBuf, select: Option<&str>) {
        let installed = list_folders(&folder).and_then(|listing| {
            self.finder
                .set_listing(listing, select)
                .map_err(|e| format!("{}: {e}", folder.display()))
        });
        match installed {
            Ok(()) => {
                self.folder = folder;
                let _ = self.finder.set_note(NOTE);
            }
            Err(e) => {
                let mut cut = e.len().min(finder::NOTE_BYTES);
                while !e.is_char_boundary(cut) {
                    cut -= 1;
                }
                let _ = self.finder.set_note(e.get(..cut).unwrap_or_default());
            }
        }
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
                self.list(self.folder.join(name), None);
                Reply::Stay(true)
            }
            Outcome::Ascend => {
                let Some((parent, from)) = self.folder.parent().and_then(|parent| {
                    let from = self.folder.file_name()?.to_str()?.to_string();
                    Some((parent.to_path_buf(), from))
                }) else {
                    return Reply::Stay(false);
                };
                self.list(parent, Some(&from));
                Reply::Stay(true)
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

        assert_eq!(
            names(&list_folders(&base).unwrap()),
            [
                ("alpha", ""),
                ("Beta", ""),
                ("repo", "git"),
                ("to-alpha", "link")
            ]
        );
        let mut chooser = Chooser::open(surface(), rect(), &base).unwrap();
        // Into Beta, by filter and Return; its one folder; back up with
        // Beta selected.
        for c in ["b", "e"] {
            key(&mut chooser, c);
        }
        assert_eq!(key(&mut chooser, "Return"), Reply::Stay(true));
        assert_eq!(chooser.folder(), base.join("Beta"));
        assert_eq!(names(chooser.listing()), [("inner", "")]);
        assert_eq!(key(&mut chooser, "Backspace"), Reply::Stay(true));
        assert_eq!(chooser.folder(), base);
        // Ctrl+Return chooses the folder listed.
        assert_eq!(key(&mut chooser, "C-Return"), Reply::Chosen(base.clone()));

        let mut chooser = Chooser::open(surface(), rect(), &base).unwrap();
        assert_eq!(key(&mut chooser, "Escape"), Reply::Closed);
        assert!(Chooser::open(surface(), rect(), &base.join("gone")).is_err());
        fs::remove_dir_all(&base).unwrap();
    }
}
