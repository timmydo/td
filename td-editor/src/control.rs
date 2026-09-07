//! Safe control framing, queries and revision-checked edits. No listener or I/O.

use crate::model::{Command, Selection, TabId};
use crate::ui::{Controller, Event};
use crate::{Error, Result};
use std::fmt::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

pub const MAX_FRAME: usize = 1024 * 1024;
pub const PAGE_BYTES: usize = 256 * 1024;
pub const INSERT_BYTES: usize = 256 * 1024;
pub const SEARCH_BYTES: usize = crate::search::QUERY_BYTES;
pub const SPELLING_RANGES: usize = 256;
pub const KEY_BYTES: usize = 32;

/// One length-prefixed frame. Any refusal poisons it and drops partial text.
#[derive(Default)]
pub struct Decoder {
    header: [u8; 4],
    header_used: usize,
    payload: Vec<u8>,
    used: usize,
    failed: bool,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let result = self.append(bytes);
        if result.is_err() {
            self.failed = true;
            self.payload = Vec::new();
        }
        result
    }

    fn append(&mut self, mut bytes: &[u8]) -> Result<()> {
        if self.failed {
            return Err(Error::Protocol);
        }
        if self.header_used < 4 {
            let take = bytes.len().min(4 - self.header_used);
            self.header
                .get_mut(self.header_used..self.header_used + take)
                .ok_or(Error::Protocol)?
                .copy_from_slice(bytes.get(..take).ok_or(Error::Protocol)?);
            self.header_used += take;
            bytes = bytes.get(take..).ok_or(Error::Protocol)?;
            if self.header_used < 4 {
                return Ok(());
            }
            let size =
                usize::try_from(u32::from_be_bytes(self.header)).map_err(|_| Error::Limit)?;
            if size == 0 {
                return Err(Error::Protocol);
            }
            if size > MAX_FRAME {
                return Err(Error::Limit);
            }
            self.payload = vec![0; size];
        }
        let end = self.used.checked_add(bytes.len()).ok_or(Error::Limit)?;
        self.payload
            .get_mut(self.used..end)
            .ok_or(Error::Protocol)?
            .copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    pub fn payload(&self) -> Option<&[u8]> {
        (!self.failed && self.header_used == 4 && self.used == self.payload.len())
            .then_some(self.payload.as_slice())
    }

    /// EOF before a complete payload is an error, not a shorter request.
    pub fn finish(self) -> Result<Vec<u8>> {
        if self.payload().is_none() {
            return Err(Error::Protocol);
        }
        Ok(self.payload)
    }
}

pub fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() {
        return Err(Error::Protocol);
    }
    if payload.len() > MAX_FRAME {
        return Err(Error::Limit);
    }
    let length = u32::try_from(payload.len()).map_err(|_| Error::Limit)?;
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(payload);
    Ok(framed)
}

#[derive(Clone, Eq, PartialEq)]
pub enum Operation {
    State,
    New,
    Open(PathBuf),
    Save {
        tab: TabId,
        revision: u64,
        path: Option<PathBuf>,
    },
    Quit,
    CloseTab {
        tab: TabId,
        revision: u64,
    },
    DialogAnswer {
        dialog: u64,
        tab: TabId,
        revision: u64,
        answer: DialogAnswer,
    },
    Key {
        tab: TabId,
        revision: u64,
        generation: u64,
        chord: String,
    },
    Pointer {
        tab: TabId,
        revision: u64,
        generation: u64,
        phase: crate::ui::PointerPhase,
        x: u32,
        y: u32,
        extend: bool,
    },
    WaitFrame(u64),
    CheckSpelling {
        tab: TabId,
        revision: u64,
    },
    SpellingResults {
        tab: TabId,
        revision: u64,
        scan: u64,
        offset: usize,
        limit: usize,
    },
    Text {
        tab: TabId,
        revision: u64,
        offset: usize,
        limit: usize,
    },
    Edit {
        tab: TabId,
        revision: u64,
        edit: Edit,
    },
}

impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State => f.write_str("State"),
            Self::New => f.write_str("New"),
            Self::Open(path) => f
                .debug_struct("Open")
                .field("path_bytes", &path.as_os_str().len())
                .finish(),
            Self::Save {
                tab,
                revision,
                path,
            } => f
                .debug_struct("Save")
                .field("tab", tab)
                .field("revision", revision)
                .field(
                    "path_bytes",
                    &path.as_ref().map(|path| path.as_os_str().len()),
                )
                .finish(),
            Self::Quit => f.write_str("Quit"),
            Self::CloseTab { tab, revision } => f
                .debug_struct("CloseTab")
                .field("tab", tab)
                .field("revision", revision)
                .finish(),
            Self::DialogAnswer {
                dialog,
                tab,
                revision,
                answer,
            } => f
                .debug_struct("DialogAnswer")
                .field("dialog", dialog)
                .field("tab", tab)
                .field("revision", revision)
                .field("answer", answer)
                .finish(),
            Self::Key {
                tab,
                revision,
                generation,
                chord,
            } => f
                .debug_struct("Key")
                .field("tab", tab)
                .field("revision", revision)
                .field("generation", generation)
                .field("chord_bytes", &chord.len())
                .finish(),
            Self::Pointer {
                tab,
                revision,
                generation,
                phase,
                x,
                y,
                extend,
            } => f
                .debug_struct("Pointer")
                .field("tab", tab)
                .field("revision", revision)
                .field("generation", generation)
                .field("phase", phase)
                .field("x", x)
                .field("y", y)
                .field("extend", extend)
                .finish(),
            Self::WaitFrame(generation) => f.debug_tuple("WaitFrame").field(generation).finish(),
            Self::CheckSpelling { tab, revision } => f
                .debug_struct("CheckSpelling")
                .field("tab", tab)
                .field("revision", revision)
                .finish(),
            Self::SpellingResults {
                tab,
                revision,
                scan,
                offset,
                limit,
            } => f
                .debug_struct("SpellingResults")
                .field("tab", tab)
                .field("revision", revision)
                .field("scan", scan)
                .field("offset", offset)
                .field("limit", limit)
                .finish(),
            Self::Text {
                tab,
                revision,
                offset,
                limit,
            } => f
                .debug_struct("Text")
                .field("tab", tab)
                .field("revision", revision)
                .field("offset", offset)
                .field("limit", limit)
                .finish(),
            Self::Edit {
                tab,
                revision,
                edit,
            } => f
                .debug_struct("Edit")
                .field("tab", tab)
                .field("revision", revision)
                .field("edit", edit)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum DialogAnswer {
    Cancel,
    Discard,
    Save,
    Path(PathBuf),
    Reload,
    DiscardReload,
    SaveAs(PathBuf),
}

impl std::fmt::Debug for DialogAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancel => f.write_str("Cancel"),
            Self::Discard => f.write_str("Discard"),
            Self::Save => f.write_str("Save"),
            Self::Path(path) => f
                .debug_struct("Path")
                .field("path_bytes", &path.as_os_str().len())
                .finish(),
            Self::Reload => f.write_str("Reload"),
            Self::DiscardReload => f.write_str("DiscardReload"),
            Self::SaveAs(path) => f
                .debug_struct("SaveAs")
                .field("path_bytes", &path.as_os_str().len())
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum Edit {
    SelectTab,
    SelectRange(Selection),
    Insert {
        expected: Selection,
        text: String,
    },
    Delete {
        expected: Selection,
    },
    Undo,
    Redo,
    FillParagraph {
        expected: Selection,
    },
    AutoFill(bool),
    FillColumn(usize),
    GoToLine(usize),
    Profile(crate::keys::Profile),
    Find {
        expected: Selection,
        needle: String,
        backward: bool,
        wrap: bool,
    },
    ReplaceAll {
        needle: String,
        replacement: String,
    },
}

impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debugging transport jobs must not disclose document text.
        match self {
            Self::Insert { expected, text } => f
                .debug_struct("Insert")
                .field("expected", expected)
                .field("bytes", &text.len())
                .finish(),
            Self::SelectTab => f.write_str("SelectTab"),
            Self::SelectRange(selection) => f.debug_tuple("SelectRange").field(selection).finish(),
            Self::Delete { expected } => f.debug_tuple("Delete").field(expected).finish(),
            Self::Undo => f.write_str("Undo"),
            Self::Redo => f.write_str("Redo"),
            Self::FillParagraph { expected } => {
                f.debug_tuple("FillParagraph").field(expected).finish()
            }
            Self::AutoFill(value) => f.debug_tuple("AutoFill").field(value).finish(),
            Self::FillColumn(value) => f.debug_tuple("FillColumn").field(value).finish(),
            Self::GoToLine(value) => f.debug_tuple("GoToLine").field(value).finish(),
            Self::Profile(value) => f.debug_tuple("Profile").field(value).finish(),
            Self::Find {
                expected,
                needle,
                backward,
                wrap,
            } => f
                .debug_struct("Find")
                .field("expected", expected)
                .field("needle_bytes", &needle.len())
                .field("backward", backward)
                .field("wrap", wrap)
                .finish(),
            Self::ReplaceAll {
                needle,
                replacement,
            } => f
                .debug_struct("ReplaceAll")
                .field("needle_bytes", &needle.len())
                .field("replacement_bytes", &replacement.len())
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub id: u64,
    pub operation: Operation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub id: u64,
    pub error: Error,
}

impl Refusal {
    pub fn response(self) -> String {
        format!(
            "1\t{}\terror\t{}\t{}",
            self.id,
            self.error.code(),
            hex(self.error.code().as_bytes())
        )
    }
}

impl Request {
    /// Parse only the implemented operations. Recoverable IDs echo on errors;
    /// errors before a recoverable ID use zero, matching replay.
    pub fn parse(input: &[u8]) -> std::result::Result<Self, Refusal> {
        let Envelope { id, name, mut args } = envelope(input)?;
        let result = (|| {
            let operation = match name {
                "state" => Operation::State,
                "new" => Operation::New,
                "open" => Operation::Open(os_path(args.next().ok_or(Error::Protocol)?)?),
                "save" | "save-as" => Operation::Save {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                    path: if name == "save-as" {
                        Some(os_path(args.next().ok_or(Error::Protocol)?)?)
                    } else {
                        None
                    },
                },
                "quit" => Operation::Quit,
                "close-tab" => Operation::CloseTab {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                },
                "dialog-answer" => Operation::DialogAnswer {
                    dialog: decimal(args.next().ok_or(Error::Protocol)?)?,
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                    answer: match args.next().ok_or(Error::Protocol)? {
                        "cancel" => DialogAnswer::Cancel,
                        "discard" => DialogAnswer::Discard,
                        "save" => DialogAnswer::Save,
                        "reload" => DialogAnswer::Reload,
                        "discard-reload" => DialogAnswer::DiscardReload,
                        "save-as" => {
                            DialogAnswer::SaveAs(os_path(args.next().ok_or(Error::Protocol)?)?)
                        }
                        "path" => DialogAnswer::Path(os_path(args.next().ok_or(Error::Protocol)?)?),
                        _ => return Err(Error::InvalidArgument),
                    },
                },
                "key" => {
                    let tab = decimal(args.next().ok_or(Error::Protocol)?)?;
                    let revision = decimal(args.next().ok_or(Error::Protocol)?)?;
                    let generation = decimal(args.next().ok_or(Error::Protocol)?)?;
                    let chord = bounded_text(args.next().ok_or(Error::Protocol)?, KEY_BYTES)?;
                    validate_chord(&chord)?;
                    Operation::Key {
                        tab,
                        revision,
                        generation,
                        chord,
                    }
                }
                "pointer" => Operation::Pointer {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                    generation: decimal(args.next().ok_or(Error::Protocol)?)?,
                    phase: match args.next().ok_or(Error::Protocol)? {
                        "press" => crate::ui::PointerPhase::Press,
                        "move" => crate::ui::PointerPhase::Move,
                        "release" => crate::ui::PointerPhase::Release,
                        _ => return Err(Error::InvalidArgument),
                    },
                    x: pointer_pixel(args.next().ok_or(Error::Protocol)?)?,
                    y: pointer_pixel(args.next().ok_or(Error::Protocol)?)?,
                    extend: boolean(args.next().ok_or(Error::Protocol)?)?,
                },
                "wait-frame" => Operation::WaitFrame(decimal(args.next().ok_or(Error::Protocol)?)?),
                "check-spelling" => Operation::CheckSpelling {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                },
                "spelling-results" => Operation::SpellingResults {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                    scan: decimal(args.next().ok_or(Error::Protocol)?)?,
                    offset: size(args.next().ok_or(Error::Protocol)?)?,
                    limit: size(args.next().ok_or(Error::Protocol)?)?,
                },
                "text" => Operation::Text {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                    offset: size(args.next().ok_or(Error::Protocol)?)?,
                    limit: size(args.next().ok_or(Error::Protocol)?)?,
                },
                "select-tab" | "select-range" | "insert" | "delete" | "undo" | "redo"
                | "fill-paragraph" | "set-auto-fill" | "set-fill-column" | "go-to-line"
                | "set-key-profile" | "find" | "replace" => {
                    let tab = decimal(args.next().ok_or(Error::Protocol)?)?;
                    let revision = decimal(args.next().ok_or(Error::Protocol)?)?;
                    let mut selection = || -> Result<Selection> {
                        Ok(Selection {
                            anchor: size(args.next().ok_or(Error::Protocol)?)?,
                            caret: size(args.next().ok_or(Error::Protocol)?)?,
                        })
                    };
                    let edit = match name {
                        "select-tab" => Edit::SelectTab,
                        "select-range" => Edit::SelectRange(selection()?),
                        "insert" => {
                            let expected = selection()?;
                            let text =
                                bounded_text(args.next().ok_or(Error::Protocol)?, INSERT_BYTES)?;
                            Edit::Insert { expected, text }
                        }
                        "delete" => Edit::Delete {
                            expected: selection()?,
                        },
                        "undo" => Edit::Undo,
                        "redo" => Edit::Redo,
                        "fill-paragraph" => Edit::FillParagraph {
                            expected: selection()?,
                        },
                        "set-auto-fill" => {
                            Edit::AutoFill(boolean(args.next().ok_or(Error::Protocol)?)?)
                        }
                        "set-fill-column" => {
                            Edit::FillColumn(size(args.next().ok_or(Error::Protocol)?)?)
                        }
                        "go-to-line" => Edit::GoToLine(size(args.next().ok_or(Error::Protocol)?)?),
                        "set-key-profile" => {
                            Edit::Profile(match args.next().ok_or(Error::Protocol)? {
                                "windows" => crate::keys::Profile::Windows,
                                "emacs" => crate::keys::Profile::Emacs,
                                _ => return Err(Error::InvalidArgument),
                            })
                        }
                        "find" => {
                            let expected = selection()?;
                            let needle =
                                bounded_text(args.next().ok_or(Error::Protocol)?, SEARCH_BYTES)?;
                            let backward = boolean(args.next().ok_or(Error::Protocol)?)?;
                            let wrap = boolean(args.next().ok_or(Error::Protocol)?)?;
                            Edit::Find {
                                expected,
                                needle,
                                backward,
                                wrap,
                            }
                        }
                        "replace" => Edit::ReplaceAll {
                            needle: bounded_text(
                                args.next().ok_or(Error::Protocol)?,
                                SEARCH_BYTES,
                            )?,
                            replacement: bounded_text(
                                args.next().ok_or(Error::Protocol)?,
                                INSERT_BYTES,
                            )?,
                        },
                        _ => return Err(Error::Protocol),
                    };
                    Operation::Edit {
                        tab,
                        revision,
                        edit,
                    }
                }
                _ => return Err(Error::Protocol),
            };
            if args.next().is_some() {
                return Err(Error::Protocol);
            }
            Ok(operation)
        })();
        result
            .map(|operation| Self { id, operation })
            .map_err(|error| Refusal { id, error })
    }

    /// Controller snapshot only. Native state adds its own flags and frames.
    pub fn response(&self, ui: &Controller) -> String {
        let result = match &self.operation {
            Operation::State => state(ui),
            Operation::Text {
                tab,
                revision,
                offset,
                limit,
            } => page(ui, *tab, *revision, *offset, *limit),
            Operation::Edit { .. }
            | Operation::New
            | Operation::Open(_)
            | Operation::Save { .. }
            | Operation::Quit
            | Operation::CloseTab { .. }
            | Operation::DialogAnswer { .. }
            | Operation::Key { .. }
            | Operation::Pointer { .. }
            | Operation::CheckSpelling { .. }
            | Operation::SpellingResults { .. }
            | Operation::WaitFrame(_) => Err(Error::Unavailable),
        };
        match result {
            Ok(body) => format!("1\t{}\tok\t{body}", self.id),
            Err(error) => Refusal { id: self.id, error }.response(),
        }
    }

    pub fn is_edit(&self) -> bool {
        matches!(self.operation, Operation::Edit { .. })
    }

    /// Only after native admission: do not classify modal refusal as no match.
    pub(crate) fn admitted_edit_refusal(&self, error: Error) -> String {
        if error == Error::Unavailable
            && matches!(
                self.operation,
                Operation::Edit {
                    edit: Edit::Find { .. },
                    ..
                }
            )
        {
            format!("1\t{}\terror\tno-match\t{}", self.id, hex(b"no-match"))
        } else {
            Refusal { id: self.id, error }.response()
        }
    }

    pub(crate) fn is_mutating(&self) -> bool {
        self.is_edit()
            || matches!(
                self.operation,
                Operation::New
                    | Operation::Open(_)
                    | Operation::Save { .. }
                    | Operation::Quit
                    | Operation::CheckSpelling { .. }
                    | Operation::CloseTab { .. }
                    | Operation::DialogAnswer { .. }
                    | Operation::Key { .. }
                    | Operation::Pointer { .. }
            )
    }

    pub(crate) fn spelling_response(
        &self,
        ui: &Controller,
        spelling: &crate::spelling::WindowState,
    ) -> String {
        let result = (|| {
            let Operation::SpellingResults {
                tab,
                revision,
                scan,
                offset,
                limit,
            } = self.operation
            else {
                return Err(Error::InvalidArgument);
            };
            let snapshot = spelling.snapshot(ui.editor(), tab, revision)?;
            if !(1..=SPELLING_RANGES).contains(&limit) || (scan == 0 && offset != 0) {
                return Err(Error::InvalidArgument);
            }
            if scan != 0 && scan != snapshot.scan {
                return Err(Error::StaleRevision);
            }
            let remaining = snapshot.marks.get(offset..).ok_or(Error::InvalidPosition)?;
            let page = remaining
                .get(..remaining.len().min(limit))
                .ok_or(Error::InvalidPosition)?;
            let counts = snapshot.counts.map_or_else(
                || "-\t-\t-\t-".into(),
                |counts| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        counts.checked,
                        counts.unknown,
                        counts.skipped,
                        u8::from(counts.truncated)
                    )
                },
            );
            let mut body = format!(
                "{tab}\t{revision}\t{}\t{}\t{}\t{}\t{counts}",
                snapshot.scan,
                snapshot.status.code(),
                offset + page.len(),
                snapshot.marks.len()
            );
            if page.is_empty() {
                body.push_str("\t-");
            } else {
                body.reserve(page.len() * 42); // Tab plus two maximum-width u64 offsets.
                for range in page {
                    write!(body, "\t{},{}", range.start, range.end).map_err(|_| Error::Protocol)?;
                }
            }
            Ok(body)
        })();
        match result {
            Ok(body) => format!("1\t{}\tok\t{body}", self.id),
            Err(error) => Refusal { id: self.id, error }.response(),
        }
    }

    /// Native modal/liveness admission belongs to the adapter. No file or
    /// clipboard authority is available here. Refusals preserve all UI state.
    pub fn execute(&self, ui: &mut Controller) -> Result<()> {
        let Operation::Edit {
            tab,
            revision,
            edit,
        } = &self.operation
        else {
            return Err(Error::InvalidArgument);
        };
        let doc = ui.editor().document(*tab)?;
        if doc.revision() != *revision {
            return Err(Error::StaleRevision);
        }
        if matches!(edit, Edit::SelectTab) {
            ui.dispatch(Event::SelectTab(*tab))?;
            return Ok(());
        }
        if ui.editor().active() != Some(*tab) {
            return Err(Error::InvalidArgument);
        }
        if let Edit::Profile(profile) = edit {
            ui.dispatch(Event::Profile(*profile))?;
            return Ok(());
        }
        if let Edit::Insert { expected, .. }
        | Edit::Delete { expected }
        | Edit::FillParagraph { expected }
        | Edit::Find { expected, .. } = edit
        {
            if doc.selection() != *expected {
                return Err(Error::InvalidArgument);
            }
        }
        let command = match edit {
            // Exhaustiveness for variants already dispatched above; never panic.
            Edit::SelectTab | Edit::Profile(_) => return Err(Error::InvalidArgument),
            Edit::SelectRange(selection) => Command::Select(*selection),
            Edit::Insert { text, .. } => {
                // Public requests can also be constructed without the parser.
                if text.len() > INSERT_BYTES {
                    return Err(Error::Limit);
                }
                Command::Insert(text.clone())
            }
            Edit::Delete { .. } => Command::Delete,
            Edit::Undo => Command::Undo,
            Edit::Redo => Command::Redo,
            Edit::FillParagraph { .. } => Command::FillParagraph,
            Edit::AutoFill(enabled) => Command::AutoFill(*enabled),
            Edit::FillColumn(column) => Command::FillColumn(*column),
            Edit::GoToLine(line) => Command::GoToLine(*line),
            Edit::Find {
                needle,
                backward,
                wrap,
                ..
            } => {
                if needle.len() > SEARCH_BYTES {
                    return Err(Error::Limit);
                }
                Command::Find {
                    needle: needle.clone(),
                    backward: *backward,
                    wrap: *wrap,
                }
            }
            Edit::ReplaceAll {
                needle,
                replacement,
            } => {
                if needle.len() > SEARCH_BYTES || replacement.len() > INSERT_BYTES {
                    return Err(Error::Limit);
                }
                Command::ReplaceAll {
                    needle: needle.clone(),
                    replacement: replacement.clone(),
                }
            }
        };
        ui.dispatch(Event::Edit {
            tab: *tab,
            revision: *revision,
            command,
        })?;
        Ok(())
    }
}

fn pointer_pixel(value: &str) -> Result<u32> {
    let pixel = u32::try_from(decimal(value)?).map_err(|_| Error::InvalidArgument)?;
    pointer_fixed(pixel)?;
    Ok(pixel)
}

pub(crate) fn pointer_fixed(pixel: u32) -> Result<i32> {
    i32::try_from(pixel)
        .ok()
        .and_then(|pixel| pixel.checked_mul(256))
        .ok_or(Error::InvalidArgument)
}

pub(crate) fn validate_chord(chord: &str) -> Result<()> {
    if chord.len() > KEY_BYTES {
        return Err(Error::Limit);
    }
    if chord.is_empty() || chord.chars().any(char::is_control) {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

fn bounded_text(encoded: &str, limit: usize) -> Result<String> {
    if encoded.len() > limit.checked_mul(2).ok_or(Error::Limit)? {
        return Err(Error::Limit);
    }
    String::from_utf8(unhex(encoded)?).map_err(|_| Error::InvalidText)
}

fn os_path(encoded: &str) -> Result<PathBuf> {
    if encoded.len() > 8192 {
        return Err(Error::Limit);
    }
    let path = unhex(encoded)?;
    if path.is_empty() || path.contains(&0) {
        return Err(Error::InvalidArgument);
    }
    Ok(std::ffi::OsString::from_vec(path).into())
}

pub(crate) struct Envelope<'a> {
    pub id: u64,
    pub name: &'a str,
    pub args: std::str::Split<'a, char>,
}

pub(crate) fn envelope(input: &[u8]) -> std::result::Result<Envelope<'_>, Refusal> {
    let mut id = 0;
    let result = (|| {
        if input.len() > MAX_FRAME {
            return Err(Error::Limit);
        }
        let input = std::str::from_utf8(input).map_err(|_| Error::Protocol)?;
        if !input.is_ascii() || input.bytes().any(|b| b < b' ' && b != b'\t' || b == 127) {
            return Err(Error::Protocol);
        }
        let mut args = input.split('\t');
        if args.next() != Some("1") {
            return Err(Error::Protocol);
        }
        id = decimal(args.next().ok_or(Error::Protocol)?)?;
        let name = args.next().ok_or(Error::Protocol)?;
        Ok((name, args))
    })();
    result
        .map(|(name, args)| Envelope { id, name, args })
        .map_err(|error| Refusal { id, error })
}

pub(crate) fn decimal(text: &str) -> Result<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    text.parse().map_err(|_| Error::Protocol)
}

pub(crate) fn size(text: &str) -> Result<usize> {
    usize::try_from(decimal(text)?).map_err(|_| Error::Protocol)
}

pub(crate) fn boolean(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(Error::Protocol),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        for index in [usize::from(byte >> 4), usize::from(byte & 15)] {
            if let Some(&digit) = DIGITS.get(index) {
                out.push(char::from(digit));
            }
        }
    }
    out
}

pub fn unhex(value: &str) -> Result<Vec<u8>> {
    if value == "-" {
        return Ok(Vec::new());
    }
    if value.is_empty() || value.len() > MAX_FRAME || !value.len().is_multiple_of(2) {
        return Err(Error::Protocol);
    }
    let digit = |b: u8| -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(Error::Protocol),
        }
    };
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = digit(*pair.first().ok_or(Error::Protocol)?)?;
            let low = digit(*pair.get(1).ok_or(Error::Protocol)?)?;
            Ok(high * 16 + low)
        })
        .collect()
}

pub(crate) fn state(ui: &Controller) -> Result<String> {
    let mut out = format!(
        "active={}\tkeys={}\tprefix={}",
        ui.editor().active().unwrap_or(0),
        match ui.keys().profile() {
            crate::keys::Profile::Windows => "windows",
            crate::keys::Profile::Emacs => "emacs",
        },
        u8::from(ui.keys().pending())
    );
    for (id, doc) in ui.editor().tabs() {
        let sel = doc.selection();
        out.push_str(&format!(
            "\ttab={id},{},{},{},{},{},{},{},{},{}",
            doc.revision(),
            u8::from(doc.dirty()),
            doc.text().len(),
            sel.anchor,
            sel.caret,
            u8::from(doc.auto_fill()),
            doc.fill_column(),
            u8::from(doc.format().bom),
            match doc.format().ending {
                crate::text::LineEnding::Lf => "lf",
                crate::text::LineEnding::CrLf => "crlf",
            }
        ));
    }
    let (width, height) = ui.geometry().dimensions();
    out.push_str(&format!(
        "\tgeneration={}\twindow={width},{height},{}\tfocus={}",
        ui.generation(),
        ui.geometry().scale().value(),
        u8::from(ui.focused())
    ));
    for (id, _) in ui.editor().tabs() {
        let view = ui.tab_view(id)?;
        let origin = view.viewport.origin();
        let (columns, rows) = view.viewport.dimensions();
        out.push_str(&format!(
            "\tview={id},{},{},{columns},{rows},{},{},{}",
            origin.row,
            origin.column,
            u8::from(view.soft_wrap),
            match view.affinity {
                crate::layout::Affinity::Upstream => "upstream",
                crate::layout::Affinity::Downstream => "downstream",
            },
            view.desired_column
                .map_or_else(|| "-".into(), |value| value.to_string())
        ));
    }
    Ok(out)
}

pub(crate) fn page(
    ui: &Controller,
    tab: TabId,
    revision: u64,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let doc = ui.editor().document(tab)?;
    if doc.revision() != revision {
        return Err(Error::StaleRevision);
    }
    if !(4..=PAGE_BYTES).contains(&limit) {
        return Err(Error::InvalidArgument);
    }
    doc.text().get(offset..).ok_or(Error::InvalidPosition)?;
    let mut end = offset.saturating_add(limit).min(doc.text().len());
    while !doc.text().is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    Ok(format!(
        "{end}\t{}",
        hex(doc
            .text()
            .get(offset..end)
            .ok_or(Error::InvalidPosition)?
            .as_bytes())
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{Command, Selection};
    use crate::ui::Event;

    #[test]
    fn decoded_pointer_grammar_bounds_pixels_phases_and_native_authority() {
        let request = Request::parse(b"1\t8\tpointer\t2\t3\t4\tpress\t12\t34\t1").unwrap();
        assert_eq!(
            request.operation,
            Operation::Pointer {
                tab: 2,
                revision: 3,
                generation: 4,
                phase: crate::ui::PointerPhase::Press,
                x: 12,
                y: 34,
                extend: true,
            }
        );
        assert!(request.is_mutating() && !request.is_edit());
        let mut ui = Controller::default();
        assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        for (tail, error) in [
            ("press\t0\t0", Error::Protocol),
            ("press\t0\t0\t0\textra", Error::Protocol),
            ("press\t-1\t0\t0", Error::Protocol),
            ("press\t0\t0\t2", Error::Protocol),
            ("enter\t0\t0\t0", Error::InvalidArgument),
            ("press\t8388608\t0\t0", Error::InvalidArgument),
            ("release\t0\t4294967296\t0", Error::InvalidArgument),
        ] {
            assert_eq!(
                Request::parse(format!("1\t8\tpointer\t2\t3\t4\t{tail}").as_bytes()).unwrap_err(),
                Refusal { id: 8, error }
            );
        }
        assert_eq!(pointer_fixed(8_388_607), Ok(2_147_483_392));
        assert_eq!(pointer_fixed(8_388_608), Err(Error::InvalidArgument));
        assert!(Request::parse(b"1\t8\tpointer\t2\t3\t4\tmove\t8388607\t0\t0").is_ok());
    }

    #[test]
    fn decoded_key_grammar_is_bounded_private_and_native_only() {
        let request = Request::parse(b"1\t9\tkey\t2\t3\t4\t70726976617465").unwrap();
        assert_eq!(
            request.operation,
            Operation::Key {
                tab: 2,
                revision: 3,
                generation: 4,
                chord: "private".into(),
            }
        );
        assert!(request.is_mutating() && !request.is_edit());
        assert!(!format!("{request:?}").contains("private"));
        assert!(format!("{request:?}").contains("chord_bytes: 7"));
        let mut ui = Controller::default();
        assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        for (arguments, error) in [
            ("2\t3\t4".to_owned(), Error::Protocol),
            ("2\t3\t4\t61\textra".to_owned(), Error::Protocol),
            ("2\t3\t-1\t61".to_owned(), Error::Protocol),
            ("2\t3\t4\t6A".to_owned(), Error::Protocol),
            ("2\t3\t4\tff".to_owned(), Error::InvalidText),
            ("2\t3\t4\t-".to_owned(), Error::InvalidArgument),
            ("2\t3\t4\t0a".to_owned(), Error::InvalidArgument),
            ("2\t3\t4\tc285".to_owned(), Error::InvalidArgument),
            (format!("2\t3\t4\t{}", "61".repeat(33)), Error::Limit),
        ] {
            let refusal = Request::parse(format!("1\t9\tkey\t{arguments}").as_bytes()).unwrap_err();
            assert_eq!(refusal, Refusal { id: 9, error });
        }
        assert!(Request::parse(b"1\t0\tkey\t1\t0\t1\tf09f9880").is_ok());
        assert!(validate_chord(&"a".repeat(32)).is_ok());
        assert_eq!(validate_chord(&"a".repeat(33)), Err(Error::Limit));
    }

    #[test]
    fn save_grammar_pins_revision_and_explicit_save_as_path_without_replay_authority() {
        let secret = "private-save-path";
        let request =
            Request::parse(format!("1\t0\tsave-as\t1\t0\t{}", hex(secret.as_bytes())).as_bytes())
                .unwrap();
        let debug = format!("{request:?}");
        assert!(debug.contains("path_bytes"));
        assert!(!debug.contains(secret) && !debug.contains(&hex(secret.as_bytes())));
        for payload in ["1\t1\tsave\t2\t3", "1\t2\tsave-as\t2\t3\t2fff"] {
            let request = Request::parse(payload.as_bytes()).unwrap();
            let mut ui = Controller::default();
            assert!(request.is_mutating());
            assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
            assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        }
        for payload in [
            "save",
            "save\t1",
            "save\t1\t0\t61",
            "save-as\t1\t0",
            "save-as\t1\t0\t61\textra",
            "save\t1\t-1",
        ] {
            assert_eq!(
                Request::parse(format!("1\t3\t{payload}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::Protocol
            );
        }
        for path in ["-", "00"] {
            assert_eq!(
                Request::parse(format!("1\t4\tsave-as\t1\t0\t{path}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::InvalidArgument
            );
        }
        assert_eq!(
            Request::parse(format!("1\t5\tsave-as\t1\t0\t{}", "61".repeat(4097)).as_bytes())
                .unwrap_err()
                .error,
            Error::Limit
        );
    }

    #[test]
    fn open_accepts_only_bounded_nonempty_non_nul_os_paths() {
        use std::os::unix::ffi::OsStrExt;
        let secret = "private-draft-path";
        let request =
            Request::parse(format!("1\t8\topen\t{}", hex(secret.as_bytes())).as_bytes()).unwrap();
        let debug = format!("{request:?}");
        assert!(debug.contains("Open") && debug.contains("path_bytes: 18"));
        assert!(!debug.contains(secret) && !debug.contains(&hex(secret.as_bytes())));
        let request = Request::parse(b"1\t9\topen\t2fff").unwrap();
        let Operation::Open(path) = &request.operation else {
            panic!("not Open")
        };
        assert_eq!(path.as_os_str().as_bytes(), b"/\xff");
        assert!(request.is_mutating());
        let mut ui = Controller::default();
        assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        for (path, expected) in [
            ("-".to_owned(), Error::InvalidArgument),
            ("610062".to_owned(), Error::InvalidArgument),
            ("2F".to_owned(), Error::Protocol),
            ("6".to_owned(), Error::Protocol),
            ("61".repeat(4097), Error::Limit),
        ] {
            assert_eq!(
                Request::parse(format!("1\t9\topen\t{path}").as_bytes())
                    .unwrap_err()
                    .error,
                expected
            );
        }
        assert!(Request::parse(format!("1\t9\topen\t{}", "61".repeat(4096)).as_bytes()).is_ok());
        for payload in ["1\t9\topen", "1\t9\topen\t61\textra"] {
            assert_eq!(
                Request::parse(payload.as_bytes()).unwrap_err().error,
                Error::Protocol
            );
        }
    }

    #[test]
    fn close_requests_have_strict_native_only_grammar() {
        for payload in [
            "1\t0\tquit",
            "1\t1\tclose-tab\t2\t3",
            "1\t2\tdialog-answer\t4\t2\t3\tcancel",
            "1\t3\tdialog-answer\t4\t2\t3\tdiscard",
            "1\t4\tdialog-answer\t4\t2\t3\tsave",
            "1\t5\tdialog-answer\t4\t2\t3\tpath\t2fff",
            "1\t6\tdialog-answer\t4\t2\t3\treload",
            "1\t7\tdialog-answer\t4\t2\t3\tdiscard-reload",
            "1\t8\tdialog-answer\t4\t2\t3\tsave-as\t2fff",
        ] {
            let request = Request::parse(payload.as_bytes()).unwrap();
            assert!(request.is_mutating());
            let mut ui = Controller::default();
            assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
            assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        }
        for payload in [
            "quit\textra",
            "close-tab",
            "close-tab\t1",
            "close-tab\t1\t0\textra",
            "dialog-answer\t1\t2\t3",
            "dialog-answer\t1\t2\t3\tcancel\textra",
            "dialog-answer\t-1\t2\t3\tdiscard",
            "dialog-answer\t1\t2\t3\tsave\textra",
            "dialog-answer\t1\t2\t3\tpath",
            "dialog-answer\t1\t2\t3\tpath\t61\textra",
            "dialog-answer\t1\t2\t3\treload\textra",
            "dialog-answer\t1\t2\t3\tdiscard-reload\textra",
            "dialog-answer\t1\t2\t3\tsave-as",
            "dialog-answer\t1\t2\t3\tsave-as\t61\textra",
        ] {
            assert_eq!(
                Request::parse(format!("1\t4\t{payload}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::Protocol
            );
        }
        for answer in ["force", "Discard", ""] {
            assert_eq!(
                Request::parse(format!("1\t5\tdialog-answer\t1\t2\t3\t{answer}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::InvalidArgument
            );
        }
    }

    #[test]
    fn dialog_paths_are_bounded_os_bytes_and_redacted() {
        let secret = "private-dialog-destination";
        let request = Request::parse(
            format!(
                "1\t0\tdialog-answer\t1\t2\t3\tpath\t{}",
                hex(secret.as_bytes())
            )
            .as_bytes(),
        )
        .unwrap();
        let debug = format!("{request:?}");
        assert!(debug.contains("path_bytes: 26"));
        assert!(!debug.contains(secret));
        assert!(!debug.contains(&hex(secret.as_bytes())));
        for (path, expected) in [
            (String::new(), Error::Protocol),
            ("00".into(), Error::InvalidArgument),
            ("610062".into(), Error::InvalidArgument),
            ("x1".into(), Error::Protocol),
            ("1".into(), Error::Protocol),
            ("61".repeat(4097), Error::Limit),
        ] {
            assert_eq!(
                Request::parse(format!("1\t0\tdialog-answer\t1\t2\t3\tpath\t{path}").as_bytes())
                    .unwrap_err()
                    .error,
                expected
            );
        }
        assert!(Request::parse(
            format!("1\t0\tdialog-answer\t1\t2\t3\tpath\t{}", "61".repeat(4096)).as_bytes()
        )
        .is_ok());
    }

    #[test]
    fn conflict_save_as_paths_share_bounds_and_debug_privacy() {
        let secret = b"private-conflict-destination";
        let request = Request::parse(
            format!("1\t0\tdialog-answer\t1\t2\t3\tsave-as\t{}", hex(secret)).as_bytes(),
        )
        .unwrap();
        let debug = format!("{request:?}");
        assert!(debug.contains("SaveAs { path_bytes: 28 }"));
        assert!(!debug.contains("private-conflict-destination"));
        assert!(!debug.contains(&hex(secret)));
        for (path, error) in [
            ("".into(), Error::Protocol),
            ("00".into(), Error::InvalidArgument),
            ("x1".into(), Error::Protocol),
            ("61".repeat(4097), Error::Limit),
        ] {
            assert_eq!(
                Request::parse(format!("1\t0\tdialog-answer\t1\t2\t3\tsave-as\t{path}").as_bytes())
                    .unwrap_err()
                    .error,
                error
            );
        }
    }

    #[test]
    fn new_tab_admission_is_native_only_and_has_no_arguments() {
        let request = Request::parse(b"1\t8\tnew").unwrap();
        assert_eq!(request.operation, Operation::New);
        assert!(request.is_mutating());
        let mut ui = Controller::default();
        assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        assert_eq!(ui.editor().tabs().count(), 0);
        for extra in ["1", "extra", ""] {
            assert_eq!(
                Request::parse(format!("1\t8\tnew\t{extra}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::Protocol,
            );
        }
    }

    #[test]
    fn spelling_job_admission_is_native_only_and_strictly_framed() {
        let request = Request::parse(b"1\t1\tcheck-spelling\t2\t0").unwrap();
        assert!(request.is_mutating());
        assert!(!request.is_edit());
        let mut ui = Controller::default();
        assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        for payload in [
            "1\t1\tcheck-spelling",
            "1\t1\tcheck-spelling\t2",
            "1\t1\tcheck-spelling\t2\t0\textra",
            "1\t1\tcheck-spelling\t2\t-1",
        ] {
            assert_eq!(
                Request::parse(payload.as_bytes()).unwrap_err().error,
                Error::Protocol
            );
        }
    }

    #[test]
    fn remote_mode_and_line_commands_match_replay_without_text_history() {
        let mut ui = Controller::default();
        let mut replay = crate::replay::Session::default();
        for controller in [&mut ui, &mut replay.ui] {
            controller
                .dispatch(Event::Load("é\nsecond\n".as_bytes()))
                .unwrap();
        }
        for (remote, local) in [
            ("set-auto-fill\t1\t0\t1", "set-auto-fill\t1\t0\t1"),
            ("set-fill-column\t1\t0\t20", "set-fill-column\t1\t0\t20"),
            ("set-key-profile\t1\t0\temacs", "set-key-profile\temacs"),
            ("go-to-line\t1\t0\t2", "go-to-line\t1\t0\t2"),
            ("go-to-line\t1\t0\t3", "go-to-line\t1\t0\t3"),
            ("go-to-line\t1\t0\t1", "go-to-line\t1\t0\t1"),
            ("set-fill-column\t1\t0\t240", "set-fill-column\t1\t0\t240"),
            ("set-auto-fill\t1\t0\t0", "set-auto-fill\t1\t0\t0"),
            ("set-key-profile\t1\t0\twindows", "set-key-profile\twindows"),
        ] {
            Request::parse(format!("1\t1\t{remote}").as_bytes())
                .unwrap()
                .execute(&mut ui)
                .unwrap();
            assert!(replay
                .request(format!("1\t1\t{local}").as_bytes())
                .starts_with("1\t1\tok\t"));
            assert_eq!(state(&ui), state(&replay.ui));
            assert_eq!(
                format!("{:?}", ui.editor()),
                format!("{:?}", replay.ui.editor())
            );
            let doc = ui.editor().document(1).unwrap();
            assert_eq!(doc.text(), "é\nsecond\n");
            assert_eq!(doc.revision(), 0);
            assert!(!doc.dirty());
            assert_eq!(doc.history_depth(), (0, 0));
        }
        assert_eq!(
            ui.editor().document(1).unwrap().selection(),
            Selection {
                anchor: 0,
                caret: 0
            }
        );
    }

    #[test]
    fn remote_mode_refusals_preserve_modes_prefix_and_selection() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load("é\nsecond\n".as_bytes())).unwrap();
        ui.dispatch(Event::Profile(crate::keys::Profile::Emacs))
            .unwrap();
        ui.dispatch(Event::Key {
            tab: 1,
            revision: 0,
            chord: "C-x",
        })
        .unwrap();
        let before = state(&ui);
        for (command, error) in [
            ("set-fill-column\t1\t0\t19", Error::InvalidArgument),
            ("set-fill-column\t1\t0\t241", Error::InvalidArgument),
            ("go-to-line\t1\t0\t0", Error::InvalidArgument),
            ("go-to-line\t1\t0\t4", Error::InvalidPosition),
            ("set-auto-fill\t1\t1\t1", Error::StaleRevision),
            ("set-key-profile\t1\t1\twindows", Error::StaleRevision),
            ("set-key-profile\t99\t0\twindows", Error::MissingTab),
            ("set-fill-column\t99\t0\t999", Error::MissingTab),
            ("set-fill-column\t1\t1\t999", Error::StaleRevision),
        ] {
            let request = Request::parse(format!("1\t1\t{command}").as_bytes()).unwrap();
            assert!(request.is_edit());
            assert_eq!(request.execute(&mut ui), Err(error));
            assert_eq!(state(&ui), before);
        }
        ui.dispatch(Event::New).unwrap();
        let before = state(&ui);
        for command in [
            "set-auto-fill\t1\t0\t1",
            "set-fill-column\t1\t0\t40",
            "go-to-line\t1\t0\t2",
            "set-key-profile\t1\t0\twindows",
        ] {
            assert_eq!(
                Request::parse(format!("1\t1\t{command}").as_bytes())
                    .unwrap()
                    .execute(&mut ui),
                Err(Error::InvalidArgument)
            );
            assert_eq!(state(&ui), before);
        }
        for (command, error) in [
            ("set-auto-fill\t2\t0\t2", Error::Protocol),
            ("set-auto-fill\t2\t0\t01", Error::Protocol),
            ("set-fill-column\t2\t0", Error::Protocol),
            ("go-to-line\t2\t0\t1\t2", Error::Protocol),
            ("set-key-profile\t2\t0\tEmacs", Error::InvalidArgument),
            ("set-key-profile\t99\t0\tEmacs", Error::InvalidArgument),
            ("set-auto-fill\t2\t0", Error::Protocol),
            ("set-auto-fill\t2\t0\t1\textra", Error::Protocol),
            ("set-fill-column\t2\t0\t20\textra", Error::Protocol),
            ("go-to-line\t2\t0", Error::Protocol),
            ("set-key-profile\t2\t0", Error::Protocol),
            ("set-key-profile\t2\t0\temacs\textra", Error::Protocol),
        ] {
            assert_eq!(
                Request::parse(format!("1\t9\t{command}").as_bytes()).unwrap_err(),
                Refusal { id: 9, error }
            );
        }
    }

    #[test]
    fn wait_frame_parser_keeps_native_fences_out_of_controller_dispatch() {
        for target in [0, 1, u64::MAX] {
            let request = Request::parse(format!("1\t9\twait-frame\t{target}").as_bytes()).unwrap();
            assert_eq!(request.operation, Operation::WaitFrame(target));
            let mut ui = Controller::default();
            assert!(!request.is_edit());
            assert!(request.response(&ui).contains("\terror\tunavailable\t"));
            assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        }
        for command in [
            "wait-frame",
            "wait-frame\t",
            "wait-frame\t1\t2",
            "wait-frame\t-1",
            "wait-frame\t18446744073709551616",
        ] {
            assert_eq!(
                Request::parse(format!("1\t9\t{command}").as_bytes()).unwrap_err(),
                Refusal {
                    id: 9,
                    error: Error::Protocol
                }
            );
        }
    }

    fn spelling_page(
        ui: &Controller,
        spelling: &crate::spelling::WindowState,
        args: &str,
    ) -> String {
        Request::parse(format!("1\t7\tspelling-results\t{args}").as_bytes())
            .unwrap()
            .spelling_response(ui, spelling)
    }

    #[test]
    fn spelling_pages_pin_scan_and_revision_and_hide_partial_results() {
        use crate::spelling::{Dictionary, WindowState, STEP_SCALARS};
        let mut ui = Controller::default();
        let text = format!("naïve bad {}wrong", " ".repeat(STEP_SCALARS));
        ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        let mut spelling = WindowState::default();
        assert_eq!(
            spelling_page(&ui, &spelling, "1\t0\t0\t0\t1"),
            "1\t7\tok\t1\t0\t0\tno-dictionary\t0\t0\t-\t-\t-\t-\t-"
        );
        spelling.install(Dictionary::parse(b"known").unwrap());
        assert!(spelling_page(&ui, &spelling, "1\t0\t0\t0\t1")
            .contains("\t0\tnot-checked\t0\t0\t-\t-\t-\t-\t-"));
        spelling.start(ui.editor(), 1, 0).unwrap();
        spelling.step(ui.editor()).unwrap();
        assert_eq!(
            spelling_page(&ui, &spelling, "1\t0\t0\t0\t1"),
            "1\t7\tok\t1\t0\t1\tchecking\t0\t0\t-\t-\t-\t-\t-"
        );
        spelling.step(ui.editor()).unwrap();
        let generation = ui.generation();
        assert_eq!(
            spelling_page(&ui, &spelling, "1\t0\t1\t0\t1"),
            "1\t7\tok\t1\t0\t1\tcomplete\t1\t2\t2\t2\t1\t0\t7,10"
        );
        assert_eq!(
            spelling_page(&ui, &spelling, "1\t0\t1\t1\t256"),
            format!(
                "1\t7\tok\t1\t0\t1\tcomplete\t2\t2\t2\t2\t1\t0\t{},{}",
                text.len() - 5,
                text.len()
            )
        );
        assert_eq!(
            spelling_page(&ui, &spelling, "1\t0\t1\t2\t1"),
            "1\t7\tok\t1\t0\t1\tcomplete\t2\t2\t2\t2\t1\t0\t-"
        );
        assert_eq!(ui.generation(), generation);
        // An inactive tab is readable, with no selection or generation change.
        ui.dispatch(Event::New).unwrap();
        assert!(spelling_page(&ui, &spelling, "1\t0\t1\t0\t1").contains("\tcomplete\t"));
        assert_eq!(ui.editor().active(), Some(2));
        spelling.start(ui.editor(), 1, 0).unwrap();
        assert!(
            spelling_page(&ui, &spelling, "1\t0\t1\t1\t1").contains("\terror\tstale-revision\t")
        );
        assert!(spelling_page(&ui, &spelling, "1\t0\t0\t0\t1").contains("\t2\tchecking\t"));
        spelling.install(Dictionary::parse(b"known").unwrap());
        assert!(
            spelling_page(&ui, &spelling, "1\t0\t2\t0\t1").contains("\terror\tstale-revision\t")
        );
        spelling.start(ui.editor(), 1, 0).unwrap();
        assert!(spelling_page(&ui, &spelling, "1\t0\t3\t0\t1").contains("\t3\tchecking\t"));
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("x".into()),
        })
        .unwrap();
        // Query validates independently, even before the window observer runs.
        assert!(
            spelling_page(&ui, &spelling, "1\t0\t3\t0\t1").contains("\terror\tstale-revision\t")
        );
        assert!(spelling_page(&ui, &spelling, "1\t1\t0\t0\t1").contains("\tnot-checked\t"));
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Undo,
        })
        .unwrap();
        assert!(
            spelling_page(&ui, &spelling, "1\t2\t3\t0\t1").contains("\terror\tstale-revision\t")
        );
    }

    #[test]
    fn spelling_queries_isolate_pending_tabs_and_reject_closed_targets() {
        use crate::spelling::{Dictionary, WindowState, STEP_SCALARS};
        let mut ui = Controller::default();
        ui.dispatch(Event::Load("bad ".repeat(STEP_SCALARS).as_bytes()))
            .unwrap();
        ui.dispatch(Event::Load(b"wrong")).unwrap();
        let mut spelling = WindowState::default();
        spelling.install(Dictionary::parse(b"known").unwrap());
        spelling.start(ui.editor(), 1, 0).unwrap();
        spelling.step(ui.editor()).unwrap();
        assert_eq!(
            spelling_page(&ui, &spelling, "2\t0\t0\t0\t1"),
            "1\t7\tok\t2\t0\t0\tnot-checked\t0\t0\t-\t-\t-\t-\t-"
        );
        assert!(
            spelling_page(&ui, &spelling, "1\t0\t1\t1\t1").contains("\terror\tinvalid-position\t")
        );
        spelling.cancel();
        assert!(
            spelling_page(&ui, &spelling, "1\t0\t1\t0\t1").contains("\terror\tstale-revision\t")
        );
        spelling.start(ui.editor(), 2, 0).unwrap();
        spelling.step(ui.editor()).unwrap();
        spelling.start(ui.editor(), 1, 0).unwrap();
        let generation = ui.generation();
        assert_eq!(
            spelling_page(&ui, &spelling, "2\t0\t2\t0\t1"),
            "1\t7\tok\t2\t0\t2\tcomplete\t1\t1\t1\t1\t0\t0\t0,5"
        );
        assert!(spelling_page(&ui, &spelling, "1\t0\t3\t0\t1").contains("\t3\tchecking\t"));
        assert_eq!(ui.generation(), generation);
        ui.dispatch(Event::Close {
            tab: 2,
            revision: 0,
        })
        .unwrap();
        assert!(spelling_page(&ui, &spelling, "2\t0\t2\t0\t1").contains("\terror\tmissing-tab\t"));
        assert!(spelling_page(&ui, &spelling, "1\t0\t3\t0\t1").contains("\t3\tchecking\t"));
        ui.dispatch(Event::Close {
            tab: 1,
            revision: 0,
        })
        .unwrap();
        assert!(spelling_page(&ui, &spelling, "1\t0\t3\t0\t1").contains("\terror\tmissing-tab\t"));
        spelling.observe(ui.editor());
        assert!(!spelling.running());
    }

    #[test]
    fn spelling_page_refusals_and_range_bounds_are_explicit() {
        use crate::spelling::{Dictionary, WindowState, MARKS};
        let mut ui = Controller::default();
        ui.dispatch(Event::Load("x ".repeat(MARKS + 3).as_bytes()))
            .unwrap();
        let mut spelling = WindowState::default();
        spelling.install(Dictionary::parse(b"known").unwrap());
        spelling.start(ui.editor(), 1, 0).unwrap();
        while spelling.running() {
            spelling.step(ui.editor()).unwrap();
        }
        let response = spelling_page(&ui, &spelling, "1\t0\t1\t0\t256");
        assert!(response.starts_with(&format!(
            "1\t7\tok\t1\t0\t1\tcomplete\t256\t{MARKS}\t{}\t{}\t0\t1\t0,1",
            MARKS + 3,
            MARKS + 3
        )));
        assert_eq!(response.split('\t').skip(13).count(), SPELLING_RANGES);
        assert!(response.len() < 16 * 1024);
        for (args, code) in [
            ("99\t0\t0\t0\t1", "missing-tab"),
            ("1\t9\t0\t0\t1", "stale-revision"),
            ("1\t0\t0\t1\t1", "invalid-argument"),
            ("1\t0\t1\t0\t0", "invalid-argument"),
            ("1\t0\t1\t0\t257", "invalid-argument"),
            ("1\t0\t1\t10001\t1", "invalid-position"),
            ("1\t0\t2\t0\t1", "stale-revision"),
        ] {
            assert!(
                spelling_page(&ui, &spelling, args).contains(&format!("\terror\t{code}\t")),
                "{args}"
            );
        }
        for args in [
            "",
            "1\t0\t0\t0",
            "1\t0\t0\t0\t1\t0",
            "1\t0\t-1\t0\t1",
            "1\t0\t18446744073709551616\t0\t1",
        ] {
            assert_eq!(
                Request::parse(format!("1\t7\tspelling-results\t{args}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::Protocol
            );
        }
        let request = Request::parse(b"1\t7\tspelling-results\t1\t0\t0\t0\t1").unwrap();
        assert!(!request.is_edit());
        assert!(request.response(&ui).contains("\terror\tunavailable\t"));
        assert_eq!(request.execute(&mut ui), Err(Error::InvalidArgument));
        spelling.cancel();
        spelling.install(Dictionary::parse(b"x").unwrap());
        spelling.start(ui.editor(), 1, 0).unwrap();
        while spelling.running() {
            spelling.step(ui.editor()).unwrap();
        }
        assert_eq!(
            spelling_page(&ui, &spelling, "1\t0\t0\t0\t1"),
            format!(
                "1\t7\tok\t1\t0\t2\tcomplete\t0\t0\t{}\t0\t0\t0\t-",
                MARKS + 3
            )
        );
    }

    #[test]
    fn every_frame_split_and_single_byte_delivery_wait_for_complete_payload() {
        let payload = b"1\t17\ttext\t1\t0\t0\t4";
        let bytes = frame(payload).unwrap();
        for split in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(bytes.get(..split).unwrap()).unwrap();
            assert_eq!(
                decoder.payload(),
                (split == bytes.len()).then_some(payload.as_slice())
            );
            decoder.push(bytes.get(split..).unwrap()).unwrap();
            assert_eq!(decoder.payload(), Some(payload.as_slice()));
            assert_eq!(decoder.finish().unwrap(), payload);
        }
        let mut decoder = Decoder::default();
        for (index, byte) in bytes.iter().enumerate() {
            decoder.push(std::slice::from_ref(byte)).unwrap();
            assert_eq!(decoder.payload().is_some(), index + 1 == bytes.len());
        }
        assert_eq!(
            Request::parse(&decoder.finish().unwrap()).unwrap(),
            Request {
                id: 17,
                operation: Operation::Text {
                    tab: 1,
                    revision: 0,
                    offset: 0,
                    limit: 4
                },
            }
        );
    }

    #[test]
    fn frame_limits_truncation_and_trailing_bytes_never_publish_partial_requests() {
        let bytes = frame(b"1\t0\tstate").unwrap();
        for end in 0..bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(bytes.get(..end).unwrap()).unwrap();
            assert_eq!(decoder.finish(), Err(Error::Protocol));
        }
        for length in [0u32, MAX_FRAME as u32 + 1, u32::MAX] {
            let mut decoder = Decoder::default();
            assert!(decoder.push(&length.to_be_bytes()).is_err());
            assert!(decoder.payload.is_empty());
            assert_eq!(decoder.push(&bytes), Err(Error::Protocol));
            assert!(decoder.payload().is_none());
        }
        let mut decoder = Decoder::default();
        decoder.push(&bytes).unwrap();
        assert_eq!(decoder.push(b"x"), Err(Error::Protocol));
        assert!(decoder.payload().is_none());
        let mut joined = bytes.clone();
        joined.extend_from_slice(&bytes);
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(&joined), Err(Error::Protocol));
        assert!(decoder.payload().is_none());
        assert_eq!(frame(b""), Err(Error::Protocol));
        assert_eq!(frame(&vec![b'x'; MAX_FRAME + 1]), Err(Error::Limit));
        let limit = frame(&vec![b'x'; MAX_FRAME]).unwrap();
        let mut decoder = Decoder::default();
        decoder.push(&limit).unwrap();
        assert_eq!(decoder.finish().unwrap().len(), MAX_FRAME);
    }

    #[test]
    fn strict_request_grammar_rejects_unknown_or_incomplete_commands_and_recovers_ids() {
        for (input, id) in [
            ("", 0),
            ("2\t12\tstate", 0),
            ("1\t+1\tstate", 0),
            ("1\t18446744073709551616\tstate", 0),
            ("1\t12\tstate\n", 0),
            ("1\t12\tstáte", 0),
            ("1\t12\tstate\t", 12),
            ("1\t12\tnew\t1", 12),
            ("1\t12\tinsert\t1\t0\t61", 12),
            ("1\t12\ttext\t1\t0\t0", 12),
            ("1\t12\ttext\t1\t0\t-1\t4", 12),
            ("1\t12\ttext\t1\t0\t0\t4\textra", 12),
        ] {
            assert_eq!(
                Request::parse(input.as_bytes()),
                Err(Refusal {
                    id,
                    error: Error::Protocol
                }),
                "{input}"
            );
        }
        assert_eq!(Request::parse(b"1\t000\tstate").unwrap().id, 0);
        assert_eq!(
            Request::parse(b"1\t18446744073709551615\tstate")
                .unwrap()
                .id,
            u64::MAX
        );
        assert_eq!(
            Request::parse(&vec![b'\t'; MAX_FRAME + 1]),
            Err(Refusal {
                id: 0,
                error: Error::Limit
            })
        );
        assert!(Request::parse(&vec![b'\t'; MAX_FRAME]).is_err());
        assert_eq!(
            Request::parse(&[0xff]),
            Err(Refusal {
                id: 0,
                error: Error::Protocol
            })
        );
        assert_eq!(
            Refusal {
                id: 9,
                error: Error::StaleRevision
            }
            .response(),
            "1\t9\terror\tstale-revision\t7374616c652d7265766973696f6e"
        );
    }

    #[test]
    fn read_only_snapshots_and_scalar_pages_match_replay_without_mutating_state() {
        let mut replay = crate::replay::Session::default();
        replay.ui.dispatch(Event::Load("aλ🦀z".as_bytes())).unwrap();
        replay
            .ui
            .dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: Command::Select(Selection {
                    anchor: 7,
                    caret: 1,
                }),
            })
            .unwrap();
        for input in [
            "1\t3\tstate",
            "1\t3\ttext\t1\t0\t0\t4",
            "1\t3\ttext\t1\t0\t3\t4",
            "1\t3\ttext\t1\t0\t7\t4",
            "1\t3\ttext\t1\t0\t8\t4",
            "1\t3\ttext\t1\t0\t2\t4",
            "1\t3\ttext\t1\t1\t0\t4",
            "1\t3\ttext\t1\t0\t0\t3",
            "1\t3\ttext\t2\t0\t0\t4",
        ] {
            let before = format!("{:?}", replay.ui.editor());
            let generation = replay.ui.generation();
            let view = replay.ui.tab_view(1).unwrap();
            let response = Request::parse(input.as_bytes())
                .unwrap()
                .response(&replay.ui);
            assert_eq!(response, replay.request(input.as_bytes()));
            assert_eq!(format!("{:?}", replay.ui.editor()), before);
            assert_eq!(replay.ui.generation(), generation);
            assert_eq!(replay.ui.tab_view(1).unwrap(), view);
            assert!(response.len() <= MAX_FRAME);
            assert!(frame(response.as_bytes()).is_ok());
        }
        assert_eq!(page(&replay.ui, 1, 0, 0, 4).unwrap(), "3\t61cebb");
        assert_eq!(page(&replay.ui, 1, 0, 3, 4).unwrap(), "7\tf09fa680");
        assert_eq!(page(&replay.ui, 1, 0, 8, 4).unwrap(), "8\t-");
        assert_eq!(
            page(&replay.ui, 1, 0, usize::MAX, 4),
            Err(Error::InvalidPosition)
        );
        assert_eq!(
            page(&replay.ui, 1, 0, 0, PAGE_BYTES + 1),
            Err(Error::InvalidArgument)
        );
        let request = Request::parse(b"1\t3\ttext\t1\t0\t0\t4").unwrap();
        replay
            .ui
            .dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: Command::Insert("b".into()),
            })
            .unwrap();
        replay
            .ui
            .dispatch(Event::Edit {
                tab: 1,
                revision: 1,
                command: Command::Undo,
            })
            .unwrap();
        assert!(request
            .response(&replay.ui)
            .contains("error\tstale-revision"));
    }

    #[test]
    fn maximum_page_and_tab_snapshot_fit_the_frame_ceiling() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(&vec![b'x'; PAGE_BYTES + 1]))
            .unwrap();
        let response = Request {
            id: u64::MAX,
            operation: Operation::Text {
                tab: 1,
                revision: 0,
                offset: 0,
                limit: PAGE_BYTES,
            },
        }
        .response(&ui);
        assert!(response.ends_with(&"78".repeat(PAGE_BYTES)));
        assert!(frame(response.as_bytes()).is_ok());
        for _ in 1..64 {
            ui.dispatch(Event::New).unwrap();
        }
        let response = Request {
            id: 1,
            operation: Operation::State,
        }
        .response(&ui);
        assert_eq!(response.matches("\ttab=").count(), 64);
        assert_eq!(response.matches("\tview=").count(), 64);
        assert!(frame(response.as_bytes()).is_ok());
    }

    #[test]
    fn binary_text_encoding_and_arbitrary_framed_input_have_closed_error_paths() {
        let bytes: Vec<_> = (0..=255).collect();
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(hex(b""), "-");
        for invalid in ["", "A0", "g0", "0", "--"] {
            assert!(unhex(invalid).is_err());
        }
        let mut completed = 0;
        for seed in 0u64..1000 {
            let mut value = seed;
            let bytes: Vec<_> = (0..64)
                .map(|_| {
                    value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (value >> 32) as u8
                })
                .collect();
            let mut decoder = Decoder::default();
            let _ = decoder.push(&bytes);
            if let Some(payload) = decoder.payload() {
                let _ = Request::parse(payload);
            }
            let _ = decoder.finish();
            let _ = Request::parse(&bytes);
            if seed % 3 == 0 {
                let framed = frame(&bytes).unwrap();
                let mut decoder = Decoder::default();
                for part in framed.chunks((seed % 7 + 1) as usize) {
                    decoder.push(part).unwrap();
                }
                assert_eq!(decoder.payload(), Some(bytes.as_slice()));
                let _ = Request::parse(decoder.payload().unwrap());
                assert_eq!(decoder.finish().unwrap(), bytes);
                completed += 1;
            }
        }
        assert_eq!(completed, 334);
    }

    #[test]
    fn malformed_envelopes_and_read_only_commands_echo_identical_refusals_in_replay() {
        let mut replay = crate::replay::Session::default();
        for input in [
            b"".as_slice(),
            b"2\t12\tstate",
            b"1\t+1\tstate",
            b"1\t18446744073709551616\tstate",
            b"1\t12\tstate\n",
            "1\t12\tstáte".as_bytes(),
            b"1\t12",
            b"1\t12\tstate\t",
            b"1\t12\ttext\t1\t0\t0",
            b"1\t12\ttext\t1\t0\t-1\t4",
            b"1\t12\ttext\t1\t0\t0\t4\textra",
            &[0xff],
            &vec![b'\t'; MAX_FRAME + 1],
        ] {
            let refusal = Request::parse(input).unwrap_err();
            assert_eq!(refusal.response(), replay.request(input));
            assert_eq!(replay.ui.generation(), 0);
            assert_eq!(replay.ui.editor().tabs().count(), 0);
        }
    }

    #[test]
    fn remote_search_and_replace_match_replay_unicode_selection_and_history() {
        let mut ui = Controller::default();
        let mut replay = crate::replay::Session::default();
        for controller in [&mut ui, &mut replay.ui] {
            controller
                .dispatch(Event::Load("λ one λ\none".as_bytes()))
                .unwrap();
        }
        for (remote, local) in [
            ("find\t1\t0\t0\t0\tcebb\t0\t0", "find\t1\t0\tcebb\t0\t0"),
            ("find\t1\t0\t0\t2\tcebb\t0\t0", "find\t1\t0\tcebb\t0\t0"),
            ("find\t1\t0\t7\t9\tcebb\t0\t1", "find\t1\t0\tcebb\t0\t1"),
            ("find\t1\t0\t0\t2\tcebb\t1\t1", "find\t1\t0\tcebb\t1\t1"),
            ("find\t1\t0\t7\t9\tcebb0a\t0\t1", "find\t1\t0\tcebb0a\t0\t1"),
            (
                "replace\t1\t0\t6f6e65\t6f6e65",
                "replace\t1\t0\t6f6e65\t6f6e65",
            ),
            (
                "replace\t1\t0\t6f6e65\t74776f0d0a",
                "replace\t1\t0\t6f6e65\t74776f0d0a",
            ),
            ("undo\t1\t1", "undo\t1\t1"),
            ("redo\t1\t2", "redo\t1\t2"),
            ("replace\t1\t3\t74776f0a\t-", "replace\t1\t3\t74776f0a\t-"),
            (
                "replace\t1\t4\t6d697373696e67\t78",
                "replace\t1\t4\t6d697373696e67\t78",
            ),
        ] {
            Request::parse(format!("1\t1\t{remote}").as_bytes())
                .unwrap()
                .execute(&mut ui)
                .unwrap();
            assert!(
                replay
                    .request(format!("1\t1\t{local}").as_bytes())
                    .starts_with("1\t1\tok\t"),
                "{local}"
            );
            assert_eq!(state(&ui), state(&replay.ui), "{remote}");
            if remote == "replace\t1\t0\t6f6e65\t6f6e65" {
                let doc = ui.editor().document(1).unwrap();
                assert_eq!(
                    doc.selection(),
                    Selection {
                        anchor: 13,
                        caret: 13
                    }
                );
                assert_eq!(doc.revision(), 0);
                assert_eq!(doc.history_depth(), (0, 0));
                assert!(!doc.dirty());
            }
            assert_eq!(
                format!("{:?}", ui.editor()),
                format!("{:?}", replay.ui.editor())
            );
        }
        let doc = ui.editor().document(1).unwrap();
        assert_eq!(doc.text(), "λ  λ\n");
        assert_eq!(doc.revision(), 4); // No-match replace is an admitted no-op.
        assert_eq!(doc.history_depth(), (2, 0));
    }

    #[test]
    fn remote_search_refusals_preserve_selection_prefix_and_model_state() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load("λ λ".as_bytes())).unwrap();
        ui.dispatch(Event::New).unwrap();
        ui.dispatch(Event::SelectTab(1)).unwrap();
        ui.dispatch(Event::Profile(crate::keys::Profile::Emacs))
            .unwrap();
        ui.dispatch(Event::Key {
            tab: 1,
            revision: 0,
            chord: "C-x",
        })
        .unwrap();
        let before = state(&ui);
        let model = format!("{:?}", ui.editor());
        for (command, error) in [
            ("find\t1\t0\t2\t0\tcebb\t0\t0", Error::InvalidArgument),
            ("find\t1\t1\t0\t0\tcebb\t0\t0", Error::StaleRevision),
            ("replace\t1\t1\tcebb\t78", Error::StaleRevision),
            ("find\t2\t0\t0\t0\tcebb\t0\t0", Error::InvalidArgument),
            ("replace\t2\t0\tcebb\t78", Error::InvalidArgument),
            ("find\t9\t0\t0\t0\tcebb\t0\t0", Error::MissingTab),
            ("replace\t9\t0\tcebb\t78", Error::MissingTab),
            ("find\t1\t0\t0\t0\t-\t0\t0", Error::InvalidArgument),
            ("replace\t1\t0\t-\t78", Error::InvalidArgument),
            ("replace\t1\t0\tcebb\t00", Error::InvalidText),
            ("find\t1\t0\t0\t0\t78\t0\t1", Error::Unavailable),
            ("find\t1\t0\t0\t0\tcebb\t1\t0", Error::Unavailable),
            ("find\t1\t0\t0\t0\tcebb0d0a\t0\t1", Error::Unavailable),
            ("find\t9\t0\t0\t0\t-\t0\t0", Error::MissingTab),
            ("replace\t1\t1\t-\t78", Error::StaleRevision),
        ] {
            assert_eq!(
                Request::parse(format!("1\t1\t{command}").as_bytes())
                    .unwrap()
                    .execute(&mut ui),
                Err(error),
                "{command}"
            );
            assert_eq!(state(&ui), before);
            assert_eq!(format!("{:?}", ui.editor()), model);
        }
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 2,
                caret: 0,
            }),
        })
        .unwrap();
        let before = state(&ui);
        let swapped = Request::parse(b"1\t1\tfind\t1\t0\t0\t2\tcebb\t0\t1").unwrap();
        assert_eq!(swapped.execute(&mut ui), Err(Error::InvalidArgument));
        assert_eq!(state(&ui), before); // Same sorted range, opposite direction.
    }

    #[test]
    fn remote_replace_refuses_expansion_before_mutating_document_or_history() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(&vec![b'a'; SEARCH_BYTES])).unwrap();
        let needle = "a".repeat(SEARCH_BYTES);
        let find = Request::parse(
            format!("1\t1\tfind\t1\t0\t0\t0\t{}\t0\t0", hex(needle.as_bytes())).as_bytes(),
        )
        .unwrap();
        find.execute(&mut ui).unwrap();
        assert_eq!(
            ui.editor().document(1).unwrap().selection().range(),
            0..SEARCH_BYTES
        );
        let before = state(&ui);
        let model = format!("{:?}", ui.editor());
        let request = Request::parse(
            format!("1\t2\treplace\t1\t0\t61\t{}", "62".repeat(INSERT_BYTES)).as_bytes(),
        )
        .unwrap();
        assert_eq!(request.execute(&mut ui), Err(Error::Limit));
        assert_eq!(state(&ui), before);
        assert_eq!(format!("{:?}", ui.editor()), model);
    }

    #[test]
    fn remote_search_grammar_bounds_text_and_redacts_debug_values() {
        for command in [
            "find\t1\t0\t0\t0\t61\t0",
            "find\t1\t0\t0\t0\t61\t2\t0",
            "find\t1\t0\t0\t0\t61\t0\ttrue",
            "find\t1\t0\t0\t0\t61\t0\t0\textra",
            "replace\t1\t0\t61",
            "replace\t1\t0\t61\t-\textra",
            "replace\t1\t0\t6A\t-",
        ] {
            assert_eq!(
                Request::parse(format!("1\t7\t{command}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::Protocol
            );
        }
        for command in ["find\t1\t0\t0\t0\tff\t0\t0", "replace\t1\t0\t61\tff"] {
            assert_eq!(
                Request::parse(format!("1\t7\t{command}").as_bytes())
                    .unwrap_err()
                    .error,
                Error::InvalidText
            );
        }
        for (prefix, suffix, limit) in [
            ("find\t1\t0\t0\t0\t", "\t0\t0", SEARCH_BYTES),
            ("replace\t1\t0\t", "\t-", SEARCH_BYTES),
            ("replace\t1\t0\t61\t", "", INSERT_BYTES),
        ] {
            assert!(Request::parse(
                format!("1\t7\t{prefix}{}{suffix}", "61".repeat(limit)).as_bytes()
            )
            .is_ok());
            assert_eq!(
                Request::parse(
                    format!("1\t7\t{prefix}{}{suffix}", "61".repeat(limit + 1)).as_bytes()
                )
                .unwrap_err()
                .error,
                Error::Limit
            );
        }
        let mut ui = Controller::default();
        ui.dispatch(Event::New).unwrap();
        let before = state(&ui);
        for edit in [
            Edit::Insert {
                expected: Selection {
                    anchor: 0,
                    caret: 0,
                },
                text: "a".repeat(INSERT_BYTES + 1),
            },
            Edit::Find {
                expected: Selection {
                    anchor: 0,
                    caret: 0,
                },
                needle: "a".repeat(SEARCH_BYTES + 1),
                backward: false,
                wrap: false,
            },
            Edit::ReplaceAll {
                needle: "a".repeat(SEARCH_BYTES + 1),
                replacement: String::new(),
            },
            Edit::ReplaceAll {
                needle: "a".into(),
                replacement: "a".repeat(INSERT_BYTES + 1),
            },
        ] {
            assert_eq!(
                Request {
                    id: 1,
                    operation: Operation::Edit {
                        tab: 1,
                        revision: 0,
                        edit
                    }
                }
                .execute(&mut ui),
                Err(Error::Limit)
            );
            assert_eq!(state(&ui), before);
        }
        for command in [
            "find\t1\t0\t0\t0\t736563726574\t0\t0",
            "replace\t1\t0\t736563726574\t70726976617465",
        ] {
            let request = Request::parse(format!("1\t7\t{command}").as_bytes()).unwrap();
            let debug = format!("{request:?}");
            assert!(!debug.contains("secret") && !debug.contains("private"));
        }
    }

    #[test]
    fn remote_edits_match_replay_controller_state_views_and_undo_history() {
        let mut ui = Controller::default();
        let mut replay = crate::replay::Session::default();
        for controller in [&mut ui, &mut replay.ui] {
            controller
                .dispatch(Event::Load(b"one   two\nthree"))
                .unwrap();
            controller.dispatch(Event::New).unwrap();
        }
        for (remote, local) in [
            ("select-tab\t1\t0", "select-tab\t1"),
            ("select-range\t1\t0\t6\t3", "select-range\t1\t0\t6\t3"),
            ("insert\t1\t0\t6\t3\tcebb0d0a", "insert\t1\t0\tcebb0d0a"),
            ("undo\t1\t1", "undo\t1\t1"),
            ("redo\t1\t2", "redo\t1\t2"),
            ("delete\t1\t3\t6\t6", "delete\t1\t3"),
            ("undo\t1\t4", "undo\t1\t4"),
            ("select-range\t1\t5\t0\t0", "select-range\t1\t5\t0\t0"),
            ("fill-paragraph\t1\t5\t0\t0", "fill-paragraph\t1\t5"),
        ] {
            Request::parse(format!("1\t1\t{remote}").as_bytes())
                .unwrap()
                .execute(&mut ui)
                .unwrap();
            assert!(
                replay
                    .request(format!("1\t1\t{local}").as_bytes())
                    .starts_with("1\t1\tok\t"),
                "{local}"
            );
            assert_eq!(state(&ui), state(&replay.ui), "{remote}");
            assert_eq!(
                format!("{:?}", ui.editor()),
                format!("{:?}", replay.ui.editor()),
                "{remote}"
            );
            for (id, _) in ui.editor().tabs() {
                assert_eq!(ui.tab_view(id), replay.ui.tab_view(id));
            }
        }
    }

    #[test]
    fn remote_refusals_preserve_selection_prefix_view_text_and_history() {
        let mut ui = Controller::default();
        ui.dispatch(Event::New).unwrap();
        ui.dispatch(Event::Load("aλ".as_bytes())).unwrap();
        ui.dispatch(Event::Profile(crate::keys::Profile::Emacs))
            .unwrap();
        ui.dispatch(Event::Key {
            tab: 2,
            revision: 0,
            chord: "C-x",
        })
        .unwrap();
        for (command, expected) in [
            ("select-tab\t99\t0", Error::MissingTab),
            ("select-tab\t1\t9", Error::StaleRevision),
            ("select-range\t1\t0\t0\t0", Error::InvalidArgument),
            ("select-range\t2\t1\t0\t0", Error::StaleRevision),
            ("select-range\t2\t0\t2\t3", Error::InvalidPosition),
            ("select-range\t2\t0\t4\t0", Error::InvalidPosition),
            ("insert\t2\t0\t3\t3\t62", Error::InvalidArgument),
            ("delete\t2\t0\t3\t3", Error::InvalidArgument),
            ("fill-paragraph\t2\t0\t3\t3", Error::InvalidArgument),
            ("insert\t2\t0\t0\t0\t00", Error::InvalidText),
            ("insert\t2\t0\t0\t0\tefbbbf", Error::InvalidText),
            ("undo\t2\t1", Error::StaleRevision),
            ("redo\t2\t1", Error::StaleRevision),
        ] {
            let before = (state(&ui), format!("{:?}", ui.editor()), ui.tab_view(2));
            let request = Request::parse(format!("1\t9\t{command}").as_bytes()).unwrap();
            assert!(request.response(&ui).contains("\terror\tunavailable\t"));
            assert_eq!(request.execute(&mut ui), Err(expected), "{command}");
            assert_eq!(
                (state(&ui), format!("{:?}", ui.editor()), ui.tab_view(2)),
                before,
                "{command}"
            );
        }
    }

    #[test]
    fn mutation_grammar_has_closed_allowlist_and_bounded_private_text() {
        for command in [
            "select-tab\t1",
            "select-tab\t1\t0\textra",
            "select-range\t1\t0\t0",
            "insert\t1\t0\t0\t0",
            "insert\t1\t0\t0\t0\t",
            "insert\t1\t0\t0\t0\t6A",
            "delete\t1\t0",
            "undo\t1\t0\t0",
            "redo\t1\t-1",
            "fill-paragraph\t1\t0",
            "new\t",
            "open\t2f746d702f78\textra",
            "close-tab\t1\t0\textra",
            "quit\textra",
            "save\t1\t0\textra",
            "save-as\t1\t0\t78\textra",
            "dialog-answer\t1\t0\tdiscard",
            "key\t1\t0\tC-s",
            "load\t61",
            "pointer\t1\t0\tpress\t0\t0\t0",
            "set-auto-fill\t1\t0",
            "auto-fill\t1\t0\t1",
            "fill-column\t1\t0\t20",
            "goto-line\t1\t0\t1",
            "key-profile\t1\t0\temacs",
        ] {
            assert_eq!(
                Request::parse(format!("1\t13\t{command}").as_bytes()).unwrap_err(),
                Refusal {
                    id: 13,
                    error: Error::Protocol
                },
                "{command}"
            );
        }
        assert_eq!(
            Request::parse(b"1\t13\tinsert\t1\t0\t0\t0\tff")
                .unwrap_err()
                .error,
            Error::InvalidText
        );
        let request = Request::parse(
            format!("1\t13\tinsert\t1\t0\t0\t0\t{}", "61".repeat(INSERT_BYTES)).as_bytes(),
        )
        .unwrap();
        let mut ui = Controller::default();
        ui.dispatch(Event::New).unwrap();
        request.execute(&mut ui).unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text().len(), INSERT_BYTES);
        assert_eq!(
            Request::parse(
                format!(
                    "1\t13\tinsert\t1\t0\t0\t0\t{}",
                    "61".repeat(INSERT_BYTES + 1)
                )
                .as_bytes()
            )
            .unwrap_err()
            .error,
            Error::Limit
        );
        let private = Request::parse(b"1\t14\tinsert\t1\t0\t0\t0\t736563726574").unwrap();
        assert!(!format!("{private:?}").contains("secret"));
    }

    #[test]
    fn remote_insert_is_one_normalized_transaction_without_auto_fill() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"one two three four five six"))
            .unwrap();
        for command in [
            Command::AutoFill(true),
            Command::FillColumn(20),
            Command::Select(Selection {
                anchor: 27,
                caret: 27,
            }),
        ] {
            ui.dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command,
            })
            .unwrap();
        }
        let request = Request::parse(b"1\t1\tinsert\t1\t0\t27\t27\t200d0a").unwrap();
        request.execute(&mut ui).unwrap();
        assert_eq!(
            ui.editor().document(1).unwrap().text(),
            "one two three four five six \n"
        );
        Request::parse(b"1\t1\tundo\t1\t1")
            .unwrap()
            .execute(&mut ui)
            .unwrap();
        assert_eq!(
            ui.editor().document(1).unwrap().text(),
            "one two three four five six"
        );
        assert!(!ui.editor().document(1).unwrap().dirty());
        assert_eq!(request.execute(&mut ui), Err(Error::StaleRevision));
    }

    #[test]
    fn empty_insert_decodes_dash_and_undo_restores_the_directed_selection() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load("aλz".as_bytes())).unwrap();
        Request::parse(b"1\t1\tselect-range\t1\t0\t3\t1")
            .unwrap()
            .execute(&mut ui)
            .unwrap();
        Request::parse(b"1\t2\tinsert\t1\t0\t3\t1\t-")
            .unwrap()
            .execute(&mut ui)
            .unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "az");
        Request::parse(b"1\t3\tundo\t1\t1")
            .unwrap()
            .execute(&mut ui)
            .unwrap();
        let doc = ui.editor().document(1).unwrap();
        assert_eq!(doc.text(), "aλz");
        assert_eq!(
            doc.selection(),
            Selection {
                anchor: 3,
                caret: 1
            }
        );
        assert!(!doc.dirty());
        let oversized = Request {
            id: 4,
            operation: Operation::Edit {
                tab: 1,
                revision: 2,
                edit: Edit::Insert {
                    expected: doc.selection(),
                    text: "x".repeat(INSERT_BYTES + 1),
                },
            },
        };
        let before = state(&ui);
        assert_eq!(oversized.execute(&mut ui), Err(Error::Limit));
        assert_eq!(state(&ui), before);
        assert_eq!(ui.editor().document(1).unwrap().text(), "aλz");
    }
}
