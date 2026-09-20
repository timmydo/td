//! Bounded filesystem model and software pixels for the FileChooser dialog,
//! the navigation, filter, selection and scroll window being td-ui's shared
//! directory finder's (td-ui/DESIGN.md, "Shared directory finder") over a
//! listing this model reads under its own descriptors and bounds.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use td_ui::chrome::{Status, ROW};
use td_ui::finder;
use td_ui::font::Font;
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Surface, BORDER,
    CHROME, INK, LINE_NUMBER,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 432;
pub const BYTES_PER_PIXEL: usize = 4;
pub const MAX_DIRECTORY_ENTRIES: usize = 512;
pub const MAX_DIRECTORY_NAME_BYTES: usize = 64 * 1024;
pub const MAX_SELECTIONS: usize = 32;
pub const MAX_DIRECTORY_DEPTH: usize = 64;
pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_RESULT_URI_BYTES: usize = 512 * 1024;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RENDERED_TITLE_BYTES: usize = 320;
pub const MAX_ACCEPT_LABEL_BYTES: usize = 64;
// A display name fills the room the finder's row has, less the folder's
// slash and the ellipsis a cut name ends in, so the filter sees as much of
// a name as the widget holds; the list clips the row at its edge.
const MAX_DISPLAY_NAME_CHARS: usize = finder::NAME_BYTES - 1 - '…'.len_utf8();

// The file chooser renders as a `Composition` over the shared raster at
// scale 1: a chrome ground, a title heading over a hairline rule, the
// selection facts line, td-ui's finder (its path row, filter field, entry
// list and status row) and a `Status` footer with the control legend. These
// are the band tops in font pixels; the finder fills from `FINDER_TOP` to
// one `ROW` above the bottom (the footer).
const INSET_X: usize = CELL_WIDTH;
const TITLE_Y: usize = 4;
const RULE_Y: usize = ROW;
const FACTS_Y: usize = ROW + 4;
const FINDER_TOP: usize = 2 * ROW;
/// The smallest surface the finder lays out on: its `MIN_COLUMNS` inside the
/// inset, and its four rows between `FINDER_TOP` and the footer. A viewport
/// smaller than this is rendered on a surface padded to it and clipped back,
/// so the request keeps a one-row list instead of failing.
const MIN_WIDTH: usize = (finder::MIN_COLUMNS + 2) * CELL_WIDTH;
const MIN_HEIGHT: usize = FINDER_TOP + 5 * ROW;

const O_DIRECTORY: i32 = 0x0001_0000;
const O_NOFOLLOW: i32 = 0x0002_0000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileFilter {
    label: String,
    pattern: String,
}

impl FileFilter {
    pub fn all_files(label: &str, pattern: &str) -> Result<Self, String> {
        if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
            return Err("file filter label is outside the 256-byte text bound".into());
        }
        if !matches!(pattern, "*" | "*.*") {
            return Err("file filter is not the supported all-files glob".into());
        }
        Ok(Self {
            label: label.to_string(),
            pattern: pattern.to_string(),
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    OpenFile { multiple: bool },
    OpenDirectory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Next,
    Previous,
    Insert(char),
    Backspace,
    Activate,
    Toggle,
    Accept,
    Parent,
    Cancel,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Pending,
    Accepted(Vec<String>),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum EntryKind {
    Directory,
    File,
}

#[derive(Debug)]
struct Entry {
    name: OsString,
    display: String,
    kind: EntryKind,
    device: u64,
    inode: u64,
}

pub struct Chooser {
    title: String,
    guest_root: PathBuf,
    relative: PathBuf,
    directories: Vec<File>,
    mode: Mode,
    accept_label: Option<String>,
    filter: Option<FileFilter>,
    entries: Vec<Entry>,
    /// The shared finder over the current directory's listing: the filter,
    /// the shown rows, the selection and the scroll window are its.
    finder: finder::Controller,
    /// The configured viewport; the finder lays out on it padded to the
    /// minimum and the frame is clipped back to it.
    viewport: (usize, usize),
    chosen: BTreeSet<Vec<u8>>,
    directory_truncated: bool,
    selection_limit_hit: bool,
    finished: bool,
    // The pinned face is built once here, not per repaint: `pinned()` parses
    // the whole glyph table, so rebuilding it on every keystroke-driven frame
    // is wasted work. td-editor and td-setup cache it the same way.
    font: Font,
}

impl Chooser {
    pub fn open(
        title: &str,
        host_root: &Path,
        guest_root: &Path,
        mode: Mode,
    ) -> Result<Self, String> {
        Self::open_with_options(title, host_root, guest_root, mode, None, None)
    }

    pub fn open_with_options(
        title: &str,
        host_root: &Path,
        guest_root: &Path,
        mode: Mode,
        accept_label: Option<String>,
        filter: Option<FileFilter>,
    ) -> Result<Self, String> {
        if title.len() > MAX_RENDERED_TITLE_BYTES || title.chars().any(char::is_control) {
            return Err(format!(
                "file chooser title is outside the {MAX_RENDERED_TITLE_BYTES}-byte text bound"
            ));
        }
        if accept_label.as_ref().is_some_and(|label| {
            label.is_empty()
                || label.len() > MAX_ACCEPT_LABEL_BYTES
                || label.chars().any(char::is_control)
        }) {
            return Err(format!(
                "file chooser accept label is outside the {MAX_ACCEPT_LABEL_BYTES}-byte text bound"
            ));
        }
        require_absolute_clean(guest_root, "file chooser guest root")?;
        require_absolute_clean(host_root, "file chooser host root")?;
        let root = open_directory(host_root).map_err(|error| {
            format!(
                "open file chooser root {} as a direct directory: {error}",
                host_root.display()
            )
        })?;
        let (entries, directory_truncated) = read_entries(&root)?;
        let chosen = BTreeSet::new();
        let listing = listing_of(
            guest_root,
            Path::new(""),
            &entries,
            &chosen,
            directory_truncated,
            mode,
        )?;
        let surface = padded_surface(WIDTH, HEIGHT)?;
        let finder = finder::Controller::new(
            listing,
            choose_of(mode),
            surface,
            finder_rect(surface),
            None,
        )
        .map_err(|error| format!("file chooser finder: {error}"))?;
        Ok(Self {
            title: title.to_string(),
            guest_root: guest_root.to_path_buf(),
            relative: PathBuf::new(),
            directories: vec![root],
            mode,
            accept_label,
            filter,
            entries,
            finder,
            viewport: (WIDTH, HEIGHT),
            chosen,
            directory_truncated,
            selection_limit_hit: false,
            finished: false,
            font: td_ui::font::pinned()?,
        })
    }

    /// One action: the finder's keys for the moves, the filter, a descent,
    /// an ascent (Parent, or Backspace on an empty filter) and the cancel;
    /// the model's own for the marks of the multiple-file mode, whose
    /// Activate on a file toggles it and whose Accept takes the marked set.
    pub fn apply(&mut self, action: Action) -> Result<Outcome, String> {
        if self.finished {
            return Err("file chooser request is already complete".into());
        }
        let outcome = match action {
            Action::Next => self.key(finder::Key::Down)?,
            Action::Previous => self.key(finder::Key::Up)?,
            Action::Insert(character) => self.finder_event(finder::Event::Insert(character))?,
            Action::Backspace => self.key(finder::Key::Backspace)?,
            Action::Activate => self.activate()?,
            Action::Toggle => {
                self.toggle()?;
                Outcome::Pending
            }
            Action::Accept => self.accept()?,
            Action::Parent => self.key(finder::Key::Parent)?,
            Action::Cancel => self.key(finder::Key::Escape)?,
        };
        if outcome != Outcome::Pending {
            self.finished = true;
        }
        Ok(outcome)
    }

    fn key(&mut self, key: finder::Key) -> Result<Outcome, String> {
        // The dialog sees one press per key and repeats nothing itself;
        // a held Backspace ascending is what the flag would guard against.
        self.finder_event(finder::Event::Key {
            key,
            repeated: false,
        })
    }

    /// One finder event and what it asks of the model: a descent enters the
    /// folder under the cursor (refused when it changed since it was
    /// listed, which fails the request with the widget untouched), an
    /// ascent the parent, a choice the accepted result. A choice closes the
    /// finder, so a result that cannot be made completes the request
    /// refused rather than leaving a closed finder behind a pending one.
    fn finder_event(&mut self, event: finder::Event) -> Result<Outcome, String> {
        match self.finder.event(event) {
            finder::Outcome::Descend(index) => {
                self.enter_directory(index)?;
                Ok(Outcome::Pending)
            }
            finder::Outcome::Ascend => {
                self.parent()?;
                Ok(Outcome::Pending)
            }
            finder::Outcome::Closed(choice) => {
                self.finished = true;
                match choice {
                    finder::Choice::Entry(index) => {
                        let entry = self.entries.get(index).ok_or_else(|| {
                            "file chooser choice escaped its entry table".to_string()
                        })?;
                        // The result invariant is the portal's, not the
                        // widget's: only a file is a chosen entry.
                        if entry.kind != EntryKind::File {
                            return Err("file chooser choice of a folder".into());
                        }
                        let uri = self.entry_uri(entry)?;
                        accepted_uris(vec![uri])
                    }
                    finder::Choice::Here => accepted_uris(vec![file_uri(&self.guest_directory())?]),
                    finder::Choice::Cancelled => Ok(Outcome::Cancelled),
                    finder::Choice::Unavailable(error) => {
                        Err(format!("file chooser finder closed: {error}"))
                    }
                }
            }
            finder::Outcome::Ignored | finder::Outcome::Consumed | finder::Outcome::Changed => {
                Ok(Outcome::Pending)
            }
        }
    }

    pub fn render(&mut self) -> Result<Vec<u8>, String> {
        let (width, height) = self.viewport;
        self.render_sized(width, height)
    }

    /// Lays the finder out for the viewport, padded to the minimum it needs;
    /// the selection stays shown.
    pub fn set_viewport(&mut self, width: usize, height: usize) -> Result<(), String> {
        frame_bytes(width, height)?;
        let surface = padded_surface(width, height)?;
        if let finder::Outcome::Closed(finder::Choice::Unavailable(error)) =
            self.finder.event(finder::Event::Resize {
                surface,
                rect: finder_rect(surface),
            })
        {
            // The widget has closed; the request ends with it.
            self.finished = true;
            return Err(format!("file chooser finder cannot lay out: {error}"));
        }
        self.viewport = (width, height);
        Ok(())
    }

    /// The frame for a `width` by `height` viewport, laid out for it first
    /// when it is not the configured one; a viewport under the minimum is
    /// painted on the padded surface and clipped to its top-left corner.
    pub fn render_sized(&mut self, width: usize, height: usize) -> Result<Vec<u8>, String> {
        if (width, height) != self.viewport {
            self.set_viewport(width, height)?;
        }
        let surface = padded_surface(width, height)?;
        let view = View {
            chooser: self,
            surface,
        };
        let mut pixels = vec![0u8; frame_bytes(width, height)?];
        if surface.width == width && surface.height == height {
            Raster::new(
                &mut pixels,
                &self.font,
                surface,
                width.saturating_mul(BYTES_PER_PIXEL),
            )
            .map_err(|error| format!("file chooser raster: {error}"))?
            .paint(&view, surface.bounds())
            .map_err(|error| format!("file chooser paint: {error}"))?;
            return Ok(pixels);
        }
        let padded_stride = surface.width.saturating_mul(BYTES_PER_PIXEL);
        let mut padded = vec![0u8; padded_stride.saturating_mul(surface.height)];
        Raster::new(&mut padded, &self.font, surface, padded_stride)
            .map_err(|error| format!("file chooser raster: {error}"))?
            .paint(&view, surface.bounds())
            .map_err(|error| format!("file chooser paint: {error}"))?;
        let stride = width.saturating_mul(BYTES_PER_PIXEL);
        for (row, target) in pixels.chunks_mut(stride).enumerate() {
            let start = row.saturating_mul(padded_stride);
            if let Some(source) = padded.get(start..start.saturating_add(stride)) {
                target.copy_from_slice(source);
            }
        }
        Ok(pixels)
    }

    pub fn query(&self) -> &str {
        self.finder.query()
    }

    pub fn matched_names(&self) -> Vec<&OsStr> {
        self.finder
            .shown()
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.name.as_os_str())
            .collect()
    }

    /// The rows the finder's list shows at the configured viewport, at
    /// least one.
    pub fn visible_rows(&self) -> usize {
        (self.finder.list_rect().height as usize / ROW).max(1)
    }

    fn status_line(&self) -> String {
        let mut line = format!("SELECTED {}", self.chosen.len());
        if self.directory_truncated {
            line.push_str("  DIRECTORY TRUNCATED");
        }
        if self.selection_limit_hit {
            line.push_str(&format!("  LIMIT {MAX_SELECTIONS}"));
        }
        if let Some(filter) = &self.filter {
            line.push_str("  APP FILTER=");
            line.push_str(&format!("{:?}", filter.label()));
        }
        line
    }

    fn help_line(&self) -> String {
        let mut line = match self.mode {
            Mode::OpenFile { multiple: false } => {
                "MOVE  FILTER  OPEN FILE  PARENT  CANCEL".to_string()
            }
            Mode::OpenFile { multiple: true } => {
                "MOVE  FILTER  TOGGLE FILES  ACCEPT  PARENT  CANCEL".to_string()
            }
            Mode::OpenDirectory => {
                "MOVE  FILTER  ENTER FOLDER  ACCEPT HERE  PARENT  CANCEL".to_string()
            }
        };
        if let Some(label) = &self.accept_label {
            line.push_str("  APP ACTION=");
            line.push_str(&format!("{label:?}"));
        }
        line
    }

    /// The entry under the finder's cursor when it is a file.
    fn selected_file(&self) -> Option<usize> {
        let index = self.finder.selected()?;
        self.entries
            .get(index)
            .filter(|entry| entry.kind == EntryKind::File)
            .map(|_| index)
    }

    fn activate(&mut self) -> Result<Outcome, String> {
        if let Mode::OpenFile { multiple: true } = self.mode {
            if self.selected_file().is_some() {
                self.toggle()?;
                return Ok(Outcome::Pending);
            }
        }
        self.key(finder::Key::Activate)
    }

    fn toggle(&mut self) -> Result<(), String> {
        let Mode::OpenFile { multiple: true } = self.mode else {
            return Ok(());
        };
        let Some(index) = self.selected_file() else {
            return Ok(());
        };
        let Some(entry) = self.entries.get(index) else {
            return Err("file chooser selection escaped its entry table".into());
        };
        let key = selection_key(&self.relative, &entry.name);
        let marked = if self.chosen.remove(&key) {
            false
        } else {
            if self.chosen.len() >= MAX_SELECTIONS {
                self.selection_limit_hit = true;
                return Ok(());
            }
            self.chosen.insert(key);
            true
        };
        self.selection_limit_hit = false;
        self.finder
            .set_marked(index, marked)
            .map_err(|error| format!("file chooser mark: {error}"))
    }

    fn accept(&mut self) -> Result<Outcome, String> {
        match self.mode {
            Mode::OpenFile { multiple: true } if !self.chosen.is_empty() => {
                let mut uris = Vec::with_capacity(self.chosen.len());
                for relative in &self.chosen {
                    let path = self.guest_root.join(OsString::from_vec(relative.clone()));
                    uris.push(file_uri(&path)?);
                }
                accepted_uris(uris)
            }
            // Accept is the multiple-file mode's and the directory mode's
            // key; a single file is opened by Activate alone, as it was.
            Mode::OpenFile { .. } => Ok(Outcome::Pending),
            Mode::OpenDirectory => self.key(finder::Key::Accept),
        }
    }

    fn parent(&mut self) -> Result<(), String> {
        if self.directories.len() <= 1 {
            return Ok(());
        }
        let target = self
            .directories
            .get(self.directories.len().saturating_sub(2))
            .ok_or_else(|| "file chooser parent descriptor is absent".to_string())?;
        let (entries, truncated) = read_entries(target)?;
        let mut relative = self.relative.clone();
        let from = relative
            .file_name()
            .map(|name| folder_label(&display_name(name)));
        if !relative.pop() {
            return Err("file chooser descriptor stack escaped its relative path".into());
        }
        let listing = listing_of(
            &self.guest_root,
            &relative,
            &entries,
            &self.chosen,
            truncated,
            self.mode,
        )?;
        self.finder
            .set_listing(listing, from.as_deref())
            .map_err(|error| format!("file chooser listing: {error}"))?;
        self.entries = entries;
        self.relative = relative;
        self.directories.pop();
        self.directory_truncated = truncated;
        self.selection_limit_hit = false;
        Ok(())
    }

    fn enter_directory(&mut self, index: usize) -> Result<(), String> {
        let Some(entry) = self.entries.get(index) else {
            return Err("file chooser descent escaped its entry table".into());
        };
        if entry.kind != EntryKind::Directory {
            return Err("file chooser descent into a file".into());
        }
        if self.directories.len() >= MAX_DIRECTORY_DEPTH {
            return Err(format!(
                "file chooser directory depth exceeds {MAX_DIRECTORY_DEPTH}"
            ));
        }
        let relative = self.relative.join(&entry.name);
        require_path_bound(&relative, "file chooser relative path")?;
        require_path_bound(
            &self.guest_root.join(&relative),
            "file chooser guest directory",
        )?;
        let current = self
            .directories
            .last()
            .ok_or_else(|| "file chooser current descriptor is absent".to_string())?;
        let directory = open_child_directory(current, &entry.name, entry.device, entry.inode)?;
        let (entries, truncated) = read_entries(&directory)?;
        let listing = listing_of(
            &self.guest_root,
            &relative,
            &entries,
            &self.chosen,
            truncated,
            self.mode,
        )?;
        self.finder
            .set_listing(listing, None)
            .map_err(|error| format!("file chooser listing: {error}"))?;
        self.relative = relative;
        self.directories.push(directory);
        self.entries = entries;
        self.directory_truncated = truncated;
        self.selection_limit_hit = false;
        Ok(())
    }

    fn guest_directory(&self) -> PathBuf {
        if self.relative.as_os_str().is_empty() {
            self.guest_root.clone()
        } else {
            self.guest_root.join(&self.relative)
        }
    }

    fn entry_uri(&self, entry: &Entry) -> Result<String, String> {
        file_uri(&self.guest_directory().join(&entry.name))
    }
}

/// What the finder chooses in each mode: a folder (`Here`) when a
/// directory is asked for, else a file.
fn choose_of(mode: Mode) -> finder::Choose {
    match mode {
        Mode::OpenDirectory => finder::Choose::Folder,
        Mode::OpenFile { .. } => finder::Choose::File,
    }
}

/// The scale-1 surface the finder lays out on for a viewport: the viewport
/// itself, or the minimum where the viewport is under it.
fn padded_surface(width: usize, height: usize) -> Result<Surface, String> {
    let scale = Scale::new(1).map_err(|error| format!("file chooser scale: {error}"))?;
    let (width, height) = (width.max(MIN_WIDTH), height.max(MIN_HEIGHT));
    Surface::new(width, height, scale)
        .map_err(|error| format!("file chooser surface {width}x{height}: {error}"))
}

/// The finder's rectangle on a surface: inside the inset, from `FINDER_TOP`
/// to the footer.
fn finder_rect(surface: Surface) -> Rect {
    Rect {
        x: INSET_X as i64,
        y: FINDER_TOP as i64,
        width: surface.width.saturating_sub(2 * INSET_X) as u32,
        height: surface.height.saturating_sub(FINDER_TOP + ROW) as u32,
    }
}

/// A directory's row label: its display name and a slash.
fn folder_label(display: &str) -> String {
    let mut label = String::with_capacity(display.len() + 1);
    label.push_str(display);
    label.push('/');
    label
}

/// One entry as the finder lists it: the display name (a folder with its
/// slash), the row ordinal as the meta, so two names cut to the same
/// display stay distinct rows (an ascent selecting by name lands on the
/// first of them), a file chosen only where files are, and the mark when
/// the entry is among the chosen.
fn row_of(index: usize, entry: &Entry, mode: Mode, marked: bool) -> Result<finder::Entry, String> {
    let (name, kind, enabled) = match entry.kind {
        EntryKind::Directory => (folder_label(&entry.display), finder::Kind::Folder, true),
        EntryKind::File => (
            entry.display.clone(),
            finder::Kind::File,
            mode != Mode::OpenDirectory,
        ),
    };
    finder::Entry::new(&name, &format!("{index:03}"), kind, enabled)
        .map(|row| row.with_marked(marked))
        .map_err(|error| format!("file chooser row {index}: {error}"))
}

/// A directory's listing for the finder: the guest path as its label, the
/// entries in their read order and whether the read was cut short.
fn listing_of(
    guest_root: &Path,
    relative: &Path,
    entries: &[Entry],
    chosen: &BTreeSet<Vec<u8>>,
    truncated: bool,
    mode: Mode,
) -> Result<finder::Listing, String> {
    let guest = if relative.as_os_str().is_empty() {
        guest_root.to_path_buf()
    } else {
        guest_root.join(relative)
    };
    let mut rows = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let marked = chosen.contains(&selection_key(relative, &entry.name));
        rows.push(row_of(index, entry, mode, marked)?);
    }
    finder::Listing::new(&path_label(&guest), rows, truncated)
        .map_err(|error| format!("file chooser listing: {error}"))
}

/// A path as the finder's path row shows it: its bytes escaped as a display
/// name's are, and its tail when the escaped form is longer than the finder
/// holds.
fn path_label(path: &Path) -> String {
    let mut label = String::with_capacity(path.as_os_str().len());
    for byte in path.as_os_str().as_bytes() {
        if matches!(*byte, b' '..=b'~') && *byte != b'%' {
            label.push(char::from(*byte));
        } else {
            push_hex_escape(&mut label, *byte);
        }
    }
    if label.len() <= finder::PATH_BYTES {
        return label;
    }
    let mut keep = label.len() - (finder::PATH_BYTES - '…'.len_utf8());
    // The label is ASCII, so any index is a char boundary; the cut is moved
    // past a `%XX` escape it would split so the tail begins whole.
    let bytes = label.as_bytes();
    if keep >= 1 && bytes.get(keep - 1) == Some(&b'%') {
        keep += 2;
    } else if keep >= 2 && bytes.get(keep - 2) == Some(&b'%') {
        keep += 1;
    }
    let mut tail = String::with_capacity(finder::PATH_BYTES);
    tail.push('…');
    tail.push_str(label.get(keep..).unwrap_or_default());
    tail
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
}

fn descriptor_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn open_child_directory(
    parent: &File,
    name: &OsStr,
    expected_device: u64,
    expected_inode: u64,
) -> Result<File, String> {
    let path = descriptor_path(parent).join(name);
    let directory =
        open_directory(&path).map_err(|error| format!("open chooser child {:?}: {error}", name))?;
    let metadata = directory
        .metadata()
        .map_err(|error| format!("inspect chooser child {:?}: {error}", name))?;
    if metadata.dev() != expected_device || metadata.ino() != expected_inode {
        return Err(format!(
            "chooser child {:?} changed after it was listed",
            name
        ));
    }
    Ok(directory)
}

fn read_entries(directory: &File) -> Result<(Vec<Entry>, bool), String> {
    let path = descriptor_path(directory);
    let iterator = fs::read_dir(&path)
        .map_err(|error| format!("read chooser directory descriptor: {error}"))?;
    let mut entries = Vec::new();
    let mut seen = 0usize;
    let mut names = 0usize;
    let mut truncated = false;
    for result in iterator {
        seen = seen.saturating_add(1);
        if seen > MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let item = result.map_err(|error| format!("read chooser entry: {error}"))?;
        let name = item.file_name();
        let next_names = names
            .checked_add(name.as_bytes().len())
            .ok_or_else(|| "file chooser directory name accounting overflow".to_string())?;
        if next_names > MAX_DIRECTORY_NAME_BYTES {
            truncated = true;
            break;
        }
        names = next_names;
        let file_type = item
            .file_type()
            .map_err(|error| format!("inspect chooser entry {:?}: {error}", name))?;
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            continue;
        };
        let metadata = item
            .metadata()
            .map_err(|error| format!("inspect chooser entry {:?}: {error}", name))?;
        entries.push(Entry {
            display: display_name(&name),
            name,
            kind,
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.as_bytes().cmp(right.name.as_bytes()))
    });
    Ok((entries, truncated))
}

fn accepted_uris(uris: Vec<String>) -> Result<Outcome, String> {
    let mut bytes = 0usize;
    for uri in &uris {
        bytes = bytes
            .checked_add(uri.len())
            .ok_or_else(|| "file chooser result URI accounting overflow".to_string())?;
        if bytes > MAX_RESULT_URI_BYTES {
            return Err(format!(
                "file chooser result URIs exceed {MAX_RESULT_URI_BYTES} bytes"
            ));
        }
    }
    Ok(Outcome::Accepted(uris))
}

fn require_absolute_clean(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(format!("{label} is not a clean absolute path"));
    }
    require_path_bound(path, label)
}

fn require_path_bound(path: &Path, label: &str) -> Result<(), String> {
    if path.as_os_str().as_bytes().len() > MAX_PATH_BYTES {
        return Err(format!("{label} exceeds {MAX_PATH_BYTES} bytes"));
    }
    Ok(())
}

fn display_name(name: &OsStr) -> String {
    let mut display = String::with_capacity(MAX_DISPLAY_NAME_CHARS.saturating_add(1));
    let mut truncated = false;
    for byte in name.as_bytes() {
        let width = if matches!(*byte, b' '..=b'~') && *byte != b'%' {
            1
        } else {
            3
        };
        if display.len().saturating_add(width) > MAX_DISPLAY_NAME_CHARS {
            truncated = true;
            break;
        }
        if width == 1 {
            display.push(char::from(*byte));
        } else {
            push_hex_escape(&mut display, *byte);
        }
    }
    if truncated {
        display.push('…');
    }
    display
}

fn push_hex_escape(text: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    text.push('%');
    if let Some(high) = HEX.get(usize::from(byte >> 4)).copied() {
        text.push(char::from(high));
    }
    if let Some(low) = HEX.get(usize::from(byte & 0x0f)).copied() {
        text.push(char::from(low));
    }
}

fn selection_key(relative: &Path, name: &OsStr) -> Vec<u8> {
    relative.join(name).as_os_str().as_bytes().to_vec()
}

pub fn file_uri(path: &Path) -> Result<String, String> {
    require_absolute_clean(path, "file chooser result")?;
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(*byte));
        } else {
            push_hex_escape(&mut uri, *byte);
        }
    }
    Ok(uri)
}

fn frame_bytes(width: usize, height: usize) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "file chooser surface {width}x{height} has an empty viewport"
        ));
    }
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| "file chooser frame size overflow".to_string())?;
    if bytes > MAX_FRAME_BYTES {
        return Err(format!(
            "file chooser surface {width}x{height} exceeds {MAX_FRAME_BYTES} frame bytes"
        ));
    }
    Ok(bytes)
}

/// The file chooser laid out over one surface as a `Composition`: a chrome
/// ground, the title heading over a hairline rule, the selection facts line,
/// the finder and the footer with the control legend. It reads the model.
struct View<'a> {
    chooser: &'a Chooser,
    surface: Surface,
}

/// Streams a filled rectangle clipped to the damage, like the chrome bands.
fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

impl Composition for View<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let s = self.surface.scale.value();
        let inset = (INSET_X * s) as i64;
        let content = self.surface.width.saturating_sub(2 * INSET_X * s) as u32;
        let band = |top: usize| Rect {
            x: inset,
            y: (top * s) as i64,
            width: content,
            height: (CELL_HEIGHT * s) as u32,
        };
        // The chrome ground under everything, so no pixel is left unpainted.
        fill(self.surface.bounds(), CHROME, damage, sink);
        let title = band(TITLE_Y);
        text_run(
            self.surface.scale,
            self.chooser.title.chars(),
            (title.x, title.y),
            title,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
        // A hairline rule separates the heading from the facts.
        fill(
            Rect {
                x: inset,
                y: (RULE_Y * s) as i64,
                width: content,
                height: s as u32,
            },
            BORDER,
            damage,
            sink,
        );
        let facts = band(FACTS_Y);
        text_run(
            self.surface.scale,
            self.chooser.status_line().chars(),
            (facts.x, facts.y),
            facts,
            GlyphStyle::medium(LINE_NUMBER, CHROME),
            damage,
            sink,
        );
        self.chooser.finder.emit(damage, sink);
        Status::new(self.surface).emit(self.chooser.help_line().chars(), damage, sink);
    }
}

pub fn selftest() -> Result<(), String> {
    let directory = create_selftest_directory()?;
    let result = (|| {
        fs::write(directory.join("report.txt"), b"td")
            .map_err(|error| format!("write chooser selftest file: {error}"))?;
        fs::create_dir(directory.join("nested"))
            .map_err(|error| format!("create chooser selftest child: {error}"))?;
        let mut chooser = Chooser::open(
            "Open — firefox",
            &directory,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )?;
        if chooser.apply(Action::Previous)? != Outcome::Pending {
            return Err("file chooser selftest navigation outcome differed".into());
        }
        chooser.apply(Action::Insert('R'))?;
        chooser.apply(Action::Backspace)?;
        chooser.apply(Action::Insert('R'))?;
        if chooser.query() != "r" || chooser.matched_names() != [OsStr::new("report.txt")] {
            return Err("file chooser selftest filter differed".into());
        }
        let Outcome::Accepted(uris) = chooser.apply(Action::Activate)? else {
            return Err("file chooser selftest did not accept its file".into());
        };
        if uris != ["file:///home/td/Downloads/report.txt"] {
            return Err(format!("file chooser selftest returned {uris:?}"));
        }
        let mut cancelled = Chooser::open(
            "Open — firefox",
            &directory,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )?;
        if cancelled.apply(Action::Cancel)? != Outcome::Cancelled
            || cancelled.apply(Action::Activate).is_ok()
        {
            return Err("file chooser selftest cancellation was not terminal".into());
        }
        let mut multiple = Chooser::open(
            "Open — firefox",
            &directory,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: true },
        )?;
        multiple.apply(Action::Next)?;
        multiple.apply(Action::Toggle)?;
        if multiple.apply(Action::Accept)?
            != Outcome::Accepted(vec!["file:///home/td/Downloads/report.txt".into()])
        {
            return Err("file chooser selftest multiple selection differed".into());
        }
        let mut directory_mode = Chooser::open(
            "Open folder — firefox",
            &directory,
            Path::new("/home/td/Downloads"),
            Mode::OpenDirectory,
        )?;
        directory_mode.apply(Action::Activate)?;
        directory_mode.apply(Action::Parent)?;
        if directory_mode.apply(Action::Accept)?
            != Outcome::Accepted(vec!["file:///home/td/Downloads".into()])
        {
            return Err("file chooser selftest directory selection differed".into());
        }
        let frame = chooser.render()?;
        let ink = (INK | 0xff00_0000).to_le_bytes();
        if frame.len() != WIDTH * HEIGHT * BYTES_PER_PIXEL
            || !frame.as_chunks::<BYTES_PER_PIXEL>().0.contains(&ink)
        {
            return Err("file chooser selftest rendered no bounded text frame".into());
        }
        Ok(())
    })();
    let cleanup = fs::remove_dir_all(&directory)
        .map_err(|error| format!("remove chooser selftest directory: {error}"));
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(cleanup)) => Err(format!("{first}; {cleanup}")),
    }
}

fn create_selftest_directory() -> Result<PathBuf, String> {
    for attempt in 0..32u8 {
        let path = std::env::temp_dir().join(format!(
            "td-portal-file-chooser-selftest-{}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create chooser selftest: {error}")),
        }
    }
    Err("file chooser selftest exhausted its 32 directory names".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use td_ui::chrome::SELECTED_ROW;

    /// A rendered pixel's RGB, the opaque X byte the raster writes masked off.
    fn pixel_rgb(frame: &[u8], width: usize, x: usize, y: usize) -> u32 {
        let at = (y * width + x) * BYTES_PER_PIXEL;
        u32::from_le_bytes(frame[at..at + BYTES_PER_PIXEL].try_into().unwrap()) & 0xff_ffff
    }

    /// Whether any rendered pixel carries `color`, comparing RGB so the opaque
    /// alpha the raster writes (and any alpha in a chrome constant) is ignored.
    fn contains_rgb(frame: &[u8], color: u32) -> bool {
        frame
            .as_chunks::<BYTES_PER_PIXEL>()
            .0
            .iter()
            .any(|pixel| u32::from_le_bytes(*pixel) & 0xff_ffff == color & 0xff_ffff)
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-portal-file-chooser-test-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bounded_firefox_options_are_visible_and_richer_filters_are_refused() {
        let root = Temp::new("firefox-options");
        fs::write(root.0.join("report.txt"), b"x").unwrap();
        let filter = FileFilter::all_files("All Files", "*").unwrap();
        let chooser = Chooser::open_with_options(
            "File Upload — firefox",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
            Some("Select".into()),
            Some(filter),
        )
        .unwrap();
        assert!(chooser.status_line().contains("APP FILTER=\"All Files\""));
        assert!(chooser
            .help_line()
            .starts_with("MOVE  FILTER  OPEN FILE  PARENT  CANCEL"));
        assert!(chooser.help_line().contains("APP ACTION=\"Select\""));
        assert!(FileFilter::all_files("Text", "*.txt").is_err());
        assert!(Chooser::open_with_options(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
            Some("x".repeat(MAX_ACCEPT_LABEL_BYTES + 1)),
            None,
        )
        .is_err());
    }

    #[test]
    fn navigation_filter_multiple_selection_and_guest_uris_are_separate() {
        let root = Temp::new("navigation");
        fs::create_dir(root.0.join("nested")).unwrap();
        fs::write(root.0.join("Alpha report.txt"), b"a").unwrap();
        fs::write(root.0.join("Beta.txt"), b"b").unwrap();
        let mut chooser = Chooser::open(
            "Open — firefox",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: true },
        )
        .unwrap();
        assert_eq!(chooser.matched_names().first(), Some(&OsStr::new("nested")));
        chooser.apply(Action::Activate).unwrap();
        assert_eq!(
            chooser.guest_directory(),
            Path::new("/home/td/Downloads/nested")
        );
        chooser.apply(Action::Parent).unwrap();
        assert_eq!(chooser.guest_directory(), Path::new("/home/td/Downloads"));

        for character in "alpha report".chars() {
            chooser.apply(Action::Insert(character)).unwrap();
        }
        assert_eq!(chooser.matched_names(), [OsStr::new("Alpha report.txt")]);
        chooser.apply(Action::Toggle).unwrap();
        assert_eq!(
            chooser.apply(Action::Accept).unwrap(),
            Outcome::Accepted(vec!["file:///home/td/Downloads/Alpha%20report.txt".into()])
        );
        assert!(chooser.apply(Action::Backspace).is_err());
    }

    #[test]
    fn non_utf8_names_round_trip_as_percent_encoded_bytes() {
        let root = Temp::new("non-utf8");
        let name = OsString::from_vec(vec![b'a', 0xff, b' ', b'b']);
        fs::write(root.0.join(&name), b"x").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        assert_eq!(
            chooser.apply(Action::Activate).unwrap(),
            Outcome::Accepted(vec!["file:///home/td/Downloads/a%FF%20b".into()])
        );
    }

    #[test]
    fn symlinks_are_not_offered_and_roots_cannot_be_symlinks() {
        let root = Temp::new("links");
        fs::write(root.0.join("real"), b"x").unwrap();
        symlink(root.0.join("real"), root.0.join("link")).unwrap();
        let chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        assert_eq!(chooser.matched_names(), [OsStr::new("real")]);

        let alias = root.0.with_extension("alias");
        let _ = fs::remove_file(&alias);
        symlink(&root.0, &alias).unwrap();
        let error = Chooser::open(
            "Open",
            &alias,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .err()
        .unwrap();
        assert!(error.contains("direct directory"), "{error}");
        fs::remove_file(alias).unwrap();
    }

    #[test]
    fn directory_and_selection_bounds_truncate_or_refuse_without_growth() {
        let root = Temp::new("bounds");
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            fs::write(root.0.join(format!("f{index:04}")), b"x").unwrap();
        }
        let chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: true },
        )
        .unwrap();
        assert_eq!(chooser.entries.len(), MAX_DIRECTORY_ENTRIES);
        assert!(chooser.directory_truncated);
        assert!(chooser.status_line().contains("DIRECTORY TRUNCATED"));
        assert!(chooser.finder.listing().truncated());

        let names = Temp::new("name-bound");
        for index in 0..258 {
            let name = format!("{index:03}{}", "x".repeat(252));
            fs::write(names.0.join(name), b"x").unwrap();
        }
        let chooser = Chooser::open(
            "Open",
            &names.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: true },
        )
        .unwrap();
        assert_eq!(chooser.entries.len(), 257);
        assert!(chooser.directory_truncated);

        let selection = Temp::new("selection-bound");
        for index in 0..=MAX_SELECTIONS {
            fs::write(selection.0.join(format!("f{index:02}")), b"x").unwrap();
        }
        let mut chooser = Chooser::open(
            "Open",
            &selection.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: true },
        )
        .unwrap();
        for index in 0..MAX_SELECTIONS {
            chooser.apply(Action::Toggle).unwrap();
            if index + 1 < MAX_SELECTIONS {
                chooser.apply(Action::Next).unwrap();
            }
        }
        chooser.apply(Action::Next).unwrap();
        assert_eq!(chooser.apply(Action::Toggle).unwrap(), Outcome::Pending);
        assert_eq!(chooser.chosen.len(), MAX_SELECTIONS);
        assert!(chooser.selection_limit_hit);
        assert!(chooser.status_line().contains("LIMIT 32"));
    }

    #[test]
    fn directory_mode_returns_the_guest_directory_and_pixels_are_deterministic() {
        let root = Temp::new("pixels");
        fs::write(root.0.join("report.txt"), b"x").unwrap();
        let mut chooser = Chooser::open(
            "Open — firefox",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenDirectory,
        )
        .unwrap();
        let first = chooser.render().unwrap();
        let second = chooser.render().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), WIDTH * HEIGHT * BYTES_PER_PIXEL);
        // The page renders on the shared light palette: a chrome ground, the
        // selected row's highlight, ink text and the hairline rule/footer
        // border. Named colours, not a byte fingerprint, so the oracle says
        // what changed when the layout does (the td-setup welcome pattern).
        assert_eq!(pixel_rgb(&first, WIDTH, 0, 0), CHROME);
        assert!(contains_rgb(&first, SELECTED_ROW));
        assert!(contains_rgb(&first, INK));
        assert!(contains_rgb(&first, BORDER));
        // A file cannot be chosen where a directory is asked for; the
        // listed directory is, and the finished chooser still renders its
        // frame, the finder's rows gone with the choice.
        assert_eq!(chooser.apply(Action::Activate).unwrap(), Outcome::Pending);
        assert_eq!(
            chooser.apply(Action::Accept).unwrap(),
            Outcome::Accepted(vec!["file:///home/td/Downloads".into()])
        );
        let done = chooser.render().unwrap();
        assert_eq!(done.len(), WIDTH * HEIGHT * BYTES_PER_PIXEL);
        assert!(!contains_rgb(&done, SELECTED_ROW) && contains_rgb(&done, INK));
    }

    #[test]
    fn selection_scrolls_into_the_bounded_visible_window() {
        let root = Temp::new("scroll");
        for index in 0..32 {
            fs::write(root.0.join(format!("f{index:02}")), b"x").unwrap();
        }
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        let rows = chooser.visible_rows();
        for _ in 0..rows.saturating_add(3) {
            chooser.apply(Action::Next).unwrap();
        }
        assert_eq!(chooser.finder.selected(), Some(rows.saturating_add(3)));
        assert_eq!(chooser.finder.first(), 4);
        chooser.apply(Action::Previous).unwrap();
        assert_eq!(chooser.finder.selected(), Some(rows.saturating_add(2)));
        assert_eq!(chooser.finder.first(), 4);
        assert!(contains_rgb(&chooser.render().unwrap(), SELECTED_ROW));
        // The moves clamp at the ends rather than wrapping.
        for _ in 0..40 {
            chooser.apply(Action::Next).unwrap();
        }
        assert_eq!(chooser.finder.selected(), Some(31));
    }

    #[test]
    fn failed_child_identity_check_keeps_rows_and_uri_prefix_transactional() {
        let root = Temp::new("child-swap");
        fs::create_dir(root.0.join("child")).unwrap();
        fs::write(root.0.join("root.txt"), b"root").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();

        fs::rename(root.0.join("child"), root.0.join("saved-child")).unwrap();
        fs::create_dir(root.0.join("child")).unwrap();
        let error = chooser.apply(Action::Activate).unwrap_err();
        assert!(error.contains("changed after it was listed"), "{error}");
        assert_eq!(chooser.guest_directory(), Path::new("/home/td/Downloads"));
        assert_eq!(
            chooser.matched_names(),
            [OsStr::new("child"), OsStr::new("root.txt")]
        );

        chooser.apply(Action::Next).unwrap();
        assert_eq!(
            chooser.apply(Action::Activate).unwrap(),
            Outcome::Accepted(vec!["file:///home/td/Downloads/root.txt".into()])
        );
    }

    #[test]
    fn direct_child_symlink_swap_is_refused_transactionally() {
        let root = Temp::new("child-symlink-swap");
        let outside = Temp::new("child-symlink-swap-outside");
        fs::create_dir(root.0.join("child")).unwrap();
        fs::write(root.0.join("root.txt"), b"root").unwrap();
        fs::write(outside.0.join("outside.txt"), b"outside").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();

        fs::rename(root.0.join("child"), root.0.join("saved-child")).unwrap();
        symlink(&outside.0, root.0.join("child")).unwrap();
        let error = chooser.apply(Action::Activate).unwrap_err();
        assert!(error.contains("open chooser child"), "{error}");
        assert_eq!(chooser.guest_directory(), Path::new("/home/td/Downloads"));
        assert_eq!(
            chooser.matched_names(),
            [OsStr::new("child"), OsStr::new("root.txt")]
        );

        chooser.apply(Action::Next).unwrap();
        assert_eq!(
            chooser.apply(Action::Activate).unwrap(),
            Outcome::Accepted(vec!["file:///home/td/Downloads/root.txt".into()])
        );
        fs::remove_file(root.0.join("child")).unwrap();
    }

    #[test]
    fn multiple_selections_survive_navigation_and_hidden_ones_are_counted() {
        let root = Temp::new("cross-directory");
        fs::create_dir(root.0.join("nested")).unwrap();
        fs::write(root.0.join("root.txt"), b"root").unwrap();
        fs::write(root.0.join("nested/child.txt"), b"child").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: true },
        )
        .unwrap();

        chooser.apply(Action::Next).unwrap();
        chooser.apply(Action::Toggle).unwrap();
        chooser.apply(Action::Previous).unwrap();
        chooser.apply(Action::Activate).unwrap();
        chooser.apply(Action::Toggle).unwrap();
        chooser.apply(Action::Parent).unwrap();
        for character in "nested".chars() {
            chooser.apply(Action::Insert(character)).unwrap();
        }
        assert_eq!(chooser.matched_names(), [OsStr::new("nested")]);
        assert!(chooser.status_line().contains("SELECTED 2"));
        // Re-entering the folder derives the star from the chosen set again.
        chooser.apply(Action::Activate).unwrap();
        assert!(chooser
            .finder
            .listing()
            .entries()
            .iter()
            .any(|entry| entry.name() == "child.txt" && entry.marked()));
        chooser.apply(Action::Parent).unwrap();
        assert_eq!(
            chooser.apply(Action::Accept).unwrap(),
            Outcome::Accepted(vec![
                "file:///home/td/Downloads/nested/child.txt".into(),
                "file:///home/td/Downloads/root.txt".into(),
            ])
        );
    }

    #[test]
    fn display_escapes_raw_identity_and_row_ordinals_disambiguate_truncation() {
        let invalid = OsString::from_vec(vec![b'a', 0xff]);
        let replacement = OsString::from("a\u{fffd}");
        let control = OsString::from("a\n");
        assert_eq!(display_name(&invalid), "a%FF");
        assert_eq!(display_name(&replacement), "a%EF%BF%BD");
        assert_eq!(display_name(&control), "a%0A");

        let long_left = OsString::from(format!("{}a", "x".repeat(MAX_DISPLAY_NAME_CHARS)));
        let long_right = OsString::from(format!("{}b", "x".repeat(MAX_DISPLAY_NAME_CHARS)));
        let entry = |name: &OsStr| Entry {
            name: name.to_os_string(),
            display: display_name(name),
            kind: EntryKind::File,
            device: 1,
            inode: 1,
        };
        let left = entry(&long_left);
        let right = entry(&long_right);
        assert_eq!(left.display, right.display);
        let mode = Mode::OpenFile { multiple: false };
        let (left, right) = (
            row_of(1, &left, mode, false).unwrap(),
            row_of(2, &right, mode, true).unwrap(),
        );
        assert_eq!(left.name(), right.name());
        assert_ne!(left.meta(), right.meta());
        assert!(!left.marked() && right.marked());
        // A path label escapes as a display name does and keeps its tail
        // within the finder's bound.
        assert_eq!(path_label(Path::new("/home/td/a%b")), "/home/td/a%25b");
        let long = PathBuf::from(format!("/{}", "y".repeat(finder::PATH_BYTES)));
        let label = path_label(&long);
        assert!(label.len() <= finder::PATH_BYTES && label.starts_with('…'));
        assert_eq!(label.len(), finder::PATH_BYTES);
        // The tail is the suffix, so an escape near the head is where the
        // cut falls: on its `%` the tail begins with the whole escape, and on
        // either hex digit the cut moves past it and the tail is shorter.
        for (shift, head, len) in [
            (0, "…%FF/", finder::PATH_BYTES),
            (1, "…/", finder::PATH_BYTES - 2),
            (2, "…/", finder::PATH_BYTES - 1),
        ] {
            let mut raw = vec![b'/'; 10];
            raw.push(0xff);
            raw.extend(std::iter::repeat_n(b'/', 4090 + shift));
            let label = path_label(Path::new(OsStr::from_bytes(&raw)));
            assert_eq!(label.len(), len, "{shift}: {label}");
            assert!(
                label.starts_with(head) && !label.contains("…F"),
                "{shift}: {label}"
            );
        }
    }

    /// The filter sees the whole of a name the finder holds, not a short
    /// display of it: a suffix past sixty-four characters still matches,
    /// and a name past the finder's room is cut with an ellipsis.
    #[test]
    fn the_filter_matches_a_name_beyond_sixty_four_characters() {
        let root = Temp::new("long-filter");
        fs::write(root.0.join(format!("{}.txt", "x".repeat(70))), b"x").unwrap();
        fs::write(root.0.join("short.md"), b"y").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        for character in ".txt".chars() {
            chooser.apply(Action::Insert(character)).unwrap();
        }
        assert_eq!(chooser.matched_names().len(), 1);
        assert!(matches!(chooser.matched_names().first(), Some(name) if name.len() == 74));
        let cut = OsString::from("w".repeat(MAX_DISPLAY_NAME_CHARS + 1));
        let display = display_name(&cut);
        assert!(display.ends_with('…') && display.len() <= MAX_DISPLAY_NAME_CHARS + 3);
        assert!(folder_label(&display).len() <= finder::NAME_BYTES);
        // A row far wider than the list (250 raw bytes escape to 750
        // display characters) is listed and painted, clipped at its edge.
        fs::create_dir(root.0.join(OsStr::from_bytes(&[0xff; 250]))).unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        assert_eq!(chooser.finder.listing().entries().len(), 3);
        assert!(chooser
            .finder
            .listing()
            .entries()
            .iter()
            .any(|entry| entry.name().len() == 751));
        assert!(chooser.render().unwrap().iter().any(|byte| *byte != 0));
    }

    /// Backspace on an empty filter is an ascent, landing on the folder it
    /// came from; on a filter it deletes; at the root it does nothing. In
    /// the single-file mode Accept does nothing, as before: Activate opens.
    #[test]
    fn backspace_on_an_empty_filter_ascends_to_the_folder_it_left() {
        let root = Temp::new("ascend");
        fs::create_dir(root.0.join("first")).unwrap();
        fs::create_dir(root.0.join("second")).unwrap();
        fs::write(root.0.join("second").join("inner.txt"), b"x").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        chooser.apply(Action::Next).unwrap();
        chooser.apply(Action::Activate).unwrap();
        assert_eq!(
            chooser.guest_directory(),
            Path::new("/home/td/Downloads/second")
        );
        assert_eq!(chooser.apply(Action::Accept).unwrap(), Outcome::Pending);
        chooser.apply(Action::Insert('i')).unwrap();
        assert_eq!(chooser.apply(Action::Backspace).unwrap(), Outcome::Pending);
        assert_eq!(chooser.query(), "");
        assert_eq!(
            chooser.guest_directory(),
            Path::new("/home/td/Downloads/second")
        );
        assert_eq!(chooser.apply(Action::Backspace).unwrap(), Outcome::Pending);
        assert_eq!(chooser.guest_directory(), Path::new("/home/td/Downloads"));
        assert_eq!(
            chooser.finder.selected_entry().map(finder::Entry::name),
            Some("second/")
        );
        assert_eq!(chooser.apply(Action::Backspace).unwrap(), Outcome::Pending);
        assert_eq!(chooser.guest_directory(), Path::new("/home/td/Downloads"));
        assert_eq!(chooser.finder.selected(), Some(1));
    }

    #[test]
    fn path_result_and_completion_bounds_are_live() {
        assert!(require_absolute_clean(Path::new("/"), "grant")
            .unwrap_err()
            .contains("not a clean absolute path"));
        let oversized = PathBuf::from(format!("/{}", "x".repeat(MAX_PATH_BYTES)));
        assert!(file_uri(&oversized).unwrap_err().contains("exceeds 4096"));
        assert!(accepted_uris(vec!["x".repeat(MAX_RESULT_URI_BYTES + 1)])
            .unwrap_err()
            .contains("result URIs exceed"));
        assert!(matches!(
            accepted_uris(vec!["x".repeat(MAX_PATH_BYTES * 3); MAX_SELECTIONS]),
            Ok(Outcome::Accepted(_))
        ));
        let deep = Temp::new("depth");
        let mut directory = deep.0.clone();
        for index in 0..MAX_DIRECTORY_DEPTH {
            directory.push(format!("d{index:02}"));
            fs::create_dir(&directory).unwrap();
        }
        let mut depth = Chooser::open(
            "Open",
            &deep.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenDirectory,
        )
        .unwrap();
        for _ in 1..MAX_DIRECTORY_DEPTH {
            assert_eq!(depth.apply(Action::Activate).unwrap(), Outcome::Pending);
        }
        assert!(depth
            .apply(Action::Activate)
            .unwrap_err()
            .contains("depth exceeds 64"));
        assert_eq!(depth.directories.len(), MAX_DIRECTORY_DEPTH);

        let root = Temp::new("completion");
        fs::write(root.0.join("file"), b"x").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        assert!(matches!(
            chooser.apply(Action::Activate).unwrap(),
            Outcome::Accepted(_)
        ));
        assert!(chooser
            .apply(Action::Cancel)
            .unwrap_err()
            .contains("already complete"));
    }

    #[test]
    fn configured_viewport_is_exact_and_frame_bounded() {
        let root = Temp::new("viewport");
        fs::write(root.0.join("report.txt"), b"x").unwrap();
        let mut chooser = Chooser::open(
            "Open",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        let frame = chooser.render_sized(640, 432).unwrap();
        assert_eq!(frame.len(), 640 * 432 * BYTES_PER_PIXEL);
        assert!(frame.iter().any(|byte| *byte != 0));
        assert_eq!(
            chooser.render_sized(2880, 1582).unwrap().len(),
            2880 * 1582 * BYTES_PER_PIXEL
        );
        chooser.set_viewport(600, 402).unwrap();
        assert_eq!(chooser.render_sized(600, 402).unwrap().len(), 600 * 402 * 4);
        let large_rows = chooser.visible_rows();
        chooser.set_viewport(1, 1).unwrap();
        assert_eq!(chooser.visible_rows(), 1);
        assert!(large_rows > chooser.visible_rows());
        // Under the minimum the frame is the padded surface's corner: the
        // chrome ground, still one row of list to move in.
        let corner = chooser.render_sized(1, 1).unwrap();
        assert_eq!(corner.len(), 4);
        assert_eq!(pixel_rgb(&corner, 1, 0, 0), CHROME);
        // A small frame is the top-left crop of the minimum's, row by row,
        // through the title and the finder's rows.
        let whole = chooser.render_sized(MIN_WIDTH, MIN_HEIGHT).unwrap();
        let (width, height) = (100, 60);
        let crop = chooser.render_sized(width, height).unwrap();
        assert!(crop.iter().any(|byte| *byte != 0));
        for row in 0..height {
            let from = row * MIN_WIDTH * BYTES_PER_PIXEL;
            assert_eq!(
                &crop[row * width * BYTES_PER_PIXEL..(row + 1) * width * BYTES_PER_PIXEL],
                &whole[from..from + width * BYTES_PER_PIXEL],
                "row {row}"
            );
        }
        assert_eq!(chooser.apply(Action::Next).unwrap(), Outcome::Pending);
        assert!(chooser.set_viewport(0, 432).is_err());
        assert!(chooser.render_sized(4096, 2161).is_err());
    }

    #[test]
    fn empty_title_uses_the_same_bounded_model() {
        let root = Temp::new("empty-title");
        let mut chooser = Chooser::open(
            "",
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .unwrap();
        assert_eq!(chooser.title, "");
        assert_eq!(chooser.render().unwrap().len(), WIDTH * HEIGHT * 4);

        let maximum = "a".repeat(MAX_RENDERED_TITLE_BYTES);
        assert!(Chooser::open(
            &maximum,
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .is_ok());
        let oversized = "a".repeat(MAX_RENDERED_TITLE_BYTES + 1);
        assert!(Chooser::open(
            &oversized,
            &root.0,
            Path::new("/home/td/Downloads"),
            Mode::OpenFile { multiple: false },
        )
        .is_err());
    }
}
