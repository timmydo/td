//! Bounded filesystem model and software pixels for the FileChooser dialog.

use crate::list_filter;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use td_ui::chrome::{Field, Item, List, Status, TextEntry, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Surface, BORDER,
    CHROME, INK, LINE_NUMBER,
};
use td_ui::font::Font;
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
const MAX_DISPLAY_NAME_CHARS: usize = 64;

// The file chooser renders as a `Composition` over the shared raster at
// scale 1: a chrome ground, a title heading over a hairline rule, the guest
// path and selection-status lines, the filter as a `TextEntry`, the entries
// as a `List`, and a `Status` footer. These are the band tops in font pixels;
// the list fills from `LIST_TOP` to one `ROW` above the bottom (the footer).
const INSET_X: usize = CELL_WIDTH;
const TITLE_Y: usize = 4;
const RULE_Y: usize = ROW;
const PATH_Y: usize = ROW + 4;
const STATUS_Y: usize = 2 * ROW + 4;
const FILTER_TOP: usize = 3 * ROW;
const LIST_TOP: usize = 4 * ROW;

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
    search: String,
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
    matches: Vec<usize>,
    selected: usize,
    scroll_start: usize,
    visible_rows: usize,
    chosen: BTreeSet<Vec<u8>>,
    query: String,
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
        let row_capacity = visible_rows(HEIGHT);
        let (entries, directory_truncated) = read_entries(&root)?;
        let mut chooser = Self {
            title: title.to_string(),
            guest_root: guest_root.to_path_buf(),
            relative: PathBuf::new(),
            directories: vec![root],
            mode,
            accept_label,
            filter,
            entries,
            matches: Vec::new(),
            selected: 0,
            scroll_start: 0,
            visible_rows: row_capacity,
            chosen: BTreeSet::new(),
            query: String::with_capacity(list_filter::MAX_QUERY_BYTES),
            directory_truncated,
            selection_limit_hit: false,
            finished: false,
            font: td_ui::font::pinned()?,
        };
        chooser.refresh_matches();
        Ok(chooser)
    }

    pub fn apply(&mut self, action: Action) -> Result<Outcome, String> {
        if self.finished {
            return Err("file chooser request is already complete".into());
        }
        let outcome = match action {
            Action::Next if !self.matches.is_empty() => {
                self.selected = self.selected.saturating_add(1) % self.matches.len();
                self.keep_selected_visible();
                Outcome::Pending
            }
            Action::Previous if !self.matches.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.matches.len().saturating_sub(1)
                } else {
                    self.selected.saturating_sub(1)
                };
                self.keep_selected_visible();
                Outcome::Pending
            }
            Action::Insert(character) if list_filter::insert(&mut self.query, character) => {
                self.refresh_matches();
                Outcome::Pending
            }
            Action::Backspace if !self.query.is_empty() => {
                self.query.pop();
                self.refresh_matches();
                Outcome::Pending
            }
            Action::Activate => self.activate()?,
            Action::Toggle => {
                self.toggle()?;
                Outcome::Pending
            }
            Action::Accept => self.accept()?,
            Action::Parent => {
                self.parent()?;
                Outcome::Pending
            }
            Action::Cancel => Outcome::Cancelled,
            Action::Next | Action::Previous | Action::Insert(_) | Action::Backspace => {
                Outcome::Pending
            }
        };
        if outcome != Outcome::Pending {
            self.finished = true;
        }
        Ok(outcome)
    }

    pub fn render(&self) -> Result<Vec<u8>, String> {
        self.render_sized(WIDTH, HEIGHT)
    }

    pub fn set_viewport(&mut self, width: usize, height: usize) -> Result<(), String> {
        frame_bytes(width, height)?;
        self.visible_rows = visible_rows(height);
        self.keep_selected_visible();
        Ok(())
    }

    pub fn render_sized(&self, width: usize, height: usize) -> Result<Vec<u8>, String> {
        let scale = Scale::new(1).map_err(|error| format!("file chooser scale: {error}"))?;
        let surface = Surface::new(width, height, scale)
            .map_err(|error| format!("file chooser surface {width}x{height}: {error}"))?;
        let rows = visible_rows(height);
        let mut items = Vec::with_capacity(rows);
        for entry_index in self.matches.iter().skip(self.scroll_start).take(rows) {
            let Some(entry) = self.entries.get(*entry_index) else {
                continue;
            };
            let suffix = if entry.kind == EntryKind::Directory {
                "/"
            } else {
                ""
            };
            let marked = self
                .chosen
                .contains(&selection_key(&self.relative, &entry.name));
            items.push(ListRow {
                label: item_label(*entry_index, entry, suffix),
                marked,
            });
        }
        let view = ChooserView {
            surface,
            rows,
            title: self.title.clone(),
            path: format!("PATH  {}", file_uri(&self.guest_directory())?),
            status: self.status_line(),
            query: self.query.clone(),
            help: self.help_line(),
            items,
            first: self.scroll_start,
            selected: self.selected,
            total: self.matches.len(),
        };
        let mut pixels = vec![0u8; frame_bytes(width, height)?];
        Raster::new(
            &mut pixels,
            &self.font,
            surface,
            width.saturating_mul(BYTES_PER_PIXEL),
        )
        .map_err(|error| format!("file chooser raster: {error}"))?
        .paint(&view, surface.bounds())
        .map_err(|error| format!("file chooser paint: {error}"))?;
        Ok(pixels)
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn matched_names(&self) -> Vec<&OsStr> {
        self.matches
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.name.as_os_str())
            .collect()
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

    fn activate(&mut self) -> Result<Outcome, String> {
        let Some(entry_index) = self.matches.get(self.selected).copied() else {
            return Ok(Outcome::Pending);
        };
        let Some(entry) = self.entries.get(entry_index) else {
            return Err("file chooser selection escaped its entry table".into());
        };
        if entry.kind == EntryKind::Directory {
            let name = entry.name.clone();
            let device = entry.device;
            let inode = entry.inode;
            self.enter_directory(&name, device, inode)?;
            return Ok(Outcome::Pending);
        }
        match self.mode {
            Mode::OpenFile { multiple: false } => {
                let uri = self.entry_uri(entry)?;
                accepted_uris(vec![uri])
            }
            Mode::OpenFile { multiple: true } => {
                self.toggle()?;
                Ok(Outcome::Pending)
            }
            Mode::OpenDirectory => Ok(Outcome::Pending),
        }
    }

    fn toggle(&mut self) -> Result<(), String> {
        let Mode::OpenFile { multiple: true } = self.mode else {
            return Ok(());
        };
        let Some(entry_index) = self.matches.get(self.selected).copied() else {
            return Ok(());
        };
        let Some(entry) = self.entries.get(entry_index) else {
            return Err("file chooser selection escaped its entry table".into());
        };
        if entry.kind != EntryKind::File {
            return Ok(());
        }
        let key = selection_key(&self.relative, &entry.name);
        if !self.chosen.remove(&key) {
            if self.chosen.len() >= MAX_SELECTIONS {
                self.selection_limit_hit = true;
                return Ok(());
            }
            self.chosen.insert(key);
        }
        self.selection_limit_hit = false;
        Ok(())
    }

    fn accept(&self) -> Result<Outcome, String> {
        match self.mode {
            Mode::OpenDirectory => accepted_uris(vec![file_uri(&self.guest_directory())?]),
            Mode::OpenFile { multiple: true } if !self.chosen.is_empty() => {
                let mut uris = Vec::with_capacity(self.chosen.len());
                for relative in &self.chosen {
                    let path = self.guest_root.join(OsString::from_vec(relative.clone()));
                    uris.push(file_uri(&path)?);
                }
                accepted_uris(uris)
            }
            Mode::OpenFile { multiple: false } | Mode::OpenFile { multiple: true } => {
                Ok(Outcome::Pending)
            }
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
        if !relative.pop() {
            return Err("file chooser descriptor stack escaped its relative path".into());
        }
        self.entries = entries;
        self.relative = relative;
        self.directories.pop();
        self.directory_truncated = truncated;
        self.selection_limit_hit = false;
        self.query.clear();
        self.refresh_matches();
        Ok(())
    }

    fn enter_directory(&mut self, name: &OsStr, device: u64, inode: u64) -> Result<(), String> {
        if self.directories.len() >= MAX_DIRECTORY_DEPTH {
            return Err(format!(
                "file chooser directory depth exceeds {MAX_DIRECTORY_DEPTH}"
            ));
        }
        let relative = self.relative.join(name);
        require_path_bound(&relative, "file chooser relative path")?;
        require_path_bound(
            &self.guest_root.join(&relative),
            "file chooser guest directory",
        )?;
        let current = self
            .directories
            .last()
            .ok_or_else(|| "file chooser current descriptor is absent".to_string())?;
        let directory = open_child_directory(current, name, device, inode)?;
        let (entries, truncated) = read_entries(&directory)?;

        self.relative = relative;
        self.directories.push(directory);
        self.entries = entries;
        self.directory_truncated = truncated;
        self.selection_limit_hit = false;
        self.query.clear();
        self.refresh_matches();
        Ok(())
    }

    fn refresh_matches(&mut self) {
        self.matches.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            if list_filter::matches(&entry.search, &self.query) {
                self.matches.push(index);
            }
        }
        self.selected = 0;
        self.scroll_start = 0;
    }

    fn guest_directory(&self) -> PathBuf {
        if self.relative.as_os_str().is_empty() {
            self.guest_root.clone()
        } else {
            self.guest_root.join(&self.relative)
        }
    }

    fn keep_selected_visible(&mut self) {
        if self.selected < self.scroll_start {
            self.scroll_start = self.selected;
        } else if self.selected >= self.scroll_start.saturating_add(self.visible_rows) {
            self.scroll_start = self
                .selected
                .saturating_add(1)
                .saturating_sub(self.visible_rows);
        }
    }

    fn entry_uri(&self, entry: &Entry) -> Result<String, String> {
        file_uri(&self.guest_directory().join(&entry.name))
    }
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
            search: name.to_string_lossy().to_ascii_lowercase(),
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

fn item_label(index: usize, entry: &Entry, suffix: &str) -> String {
    format!("{index:03} {}{suffix}", entry.display)
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

/// The `List` rows the surface shows: the height less the header bands and
/// the footer row, divided by the chrome row, at least one. `render_sized`
/// lays the `List` out at exactly this many rows, so the caller-owned scroll
/// window and the list's own row count agree.
fn visible_rows(height: usize) -> usize {
    (height.saturating_sub(LIST_TOP.saturating_add(ROW)) / ROW).max(1)
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

/// One `List` row built from an entry: its label and whether it carries the
/// multi-select mark. The list paints the mark prefix and the selection.
struct ListRow {
    label: String,
    marked: bool,
}

/// The file chooser laid out over one surface as a `Composition`: a chrome
/// ground, the title heading over a hairline rule, the guest path and
/// selection-status lines, the filter field, the entry list and the status
/// footer. It owns the strings and the visible window `render_sized` built
/// from the model, and reads nothing but its own fields.
struct ChooserView {
    surface: Surface,
    rows: usize,
    title: String,
    path: String,
    status: String,
    query: String,
    help: String,
    items: Vec<ListRow>,
    first: usize,
    selected: usize,
    total: usize,
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

impl Composition for ChooserView {
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
            self.title.chars(),
            (title.x, title.y),
            title,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
        // A hairline rule separates the heading from the path and status.
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
        for (line, top) in [(&self.path, PATH_Y), (&self.status, STATUS_Y)] {
            let at = band(top);
            text_run(
                self.surface.scale,
                line.chars(),
                (at.x, at.y),
                at,
                GlyphStyle::medium(LINE_NUMBER, CHROME),
                damage,
                sink,
            );
        }
        let filter = Rect {
            x: inset,
            y: (FILTER_TOP * s) as i64,
            width: content,
            height: (ROW * s) as u32,
        };
        if let Some(entry) = TextEntry::new(self.surface, filter) {
            let caret = self.query.chars().count();
            let first = entry.reveal(caret, caret, 0);
            entry.emit(
                Field {
                    text: &self.query,
                    placeholder: "Filter",
                    caret,
                    anchor: None,
                    first,
                    masked: false,
                    focused: true,
                    caret_visible: true,
                },
                damage,
                sink,
            );
        }
        let list_rect = Rect {
            x: inset,
            y: (LIST_TOP * s) as i64,
            width: content,
            height: (self.rows * ROW * s) as u32,
        };
        if let Some(list) = List::new(self.surface, list_rect) {
            list.emit(
                self.items.iter().map(|row| Item {
                    label: row.label.as_str(),
                    meta: "",
                    enabled: true,
                    marked: row.marked,
                }),
                self.first,
                self.selected,
                self.total,
                damage,
                sink,
            );
            if self.total == 0 {
                let body = list.body();
                text_run(
                    self.surface.scale,
                    "No matches".chars(),
                    (body.x + (INSET_X * s) as i64, body.y + (TITLE_Y * s) as i64),
                    body,
                    GlyphStyle::medium(LINE_NUMBER, CHROME),
                    damage,
                    sink,
                );
            }
        }
        Status::new(self.surface).emit(self.help.chars(), damage, sink);
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
        assert_eq!(
            chooser.apply(Action::Accept).unwrap(),
            Outcome::Accepted(vec!["file:///home/td/Downloads".into()])
        );
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
        let rows = chooser.visible_rows;
        for _ in 0..rows.saturating_add(3) {
            chooser.apply(Action::Next).unwrap();
        }
        assert_eq!(chooser.selected, rows.saturating_add(3));
        assert_eq!(chooser.scroll_start, 4);
        chooser.apply(Action::Previous).unwrap();
        assert_eq!(chooser.selected, rows.saturating_add(2));
        assert_eq!(chooser.scroll_start, 4);
        assert!(contains_rgb(&chooser.render().unwrap(), SELECTED_ROW));
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
            search: String::new(),
            kind: EntryKind::File,
            device: 1,
            inode: 1,
        };
        let left = entry(&long_left);
        let right = entry(&long_right);
        assert_eq!(left.display, right.display);
        assert_ne!(item_label(1, &left, ""), item_label(2, &right, ""));
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
        let large_rows = chooser.visible_rows;
        chooser.set_viewport(1, 1).unwrap();
        assert_eq!(chooser.visible_rows, 1);
        assert!(large_rows > chooser.visible_rows);
        assert_eq!(chooser.render_sized(1, 1).unwrap().len(), 4);
        assert!(chooser.set_viewport(0, 432).is_err());
        assert!(chooser.render_sized(4096, 2161).is_err());
    }

    #[test]
    fn empty_title_uses_the_same_bounded_model() {
        let root = Temp::new("empty-title");
        let chooser = Chooser::open(
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
