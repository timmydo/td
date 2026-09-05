//! Editor-owned transactions. Validation finishes before document/history
//! mutation, including global budgets and undo replay admission.

use crate::{fill, text, Error, Result};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;

pub type TabId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub file_bytes: usize,
    pub text_bytes: usize,
    pub tabs: usize,
    pub history_bytes: usize,
    pub transactions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            file_bytes: text::MAX_FILE_BYTES,
            text_bytes: 64 * 1024 * 1024,
            tabs: 64,
            history_bytes: 64 * 1024 * 1024,
            transactions: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    pub anchor: usize,
    pub caret: usize,
}

impl Selection {
    pub fn range(self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }
}

#[derive(Debug)]
struct Transaction {
    serial: u64,
    at: usize,
    removed: String,
    inserted: String,
    before: Selection,
    after: Selection,
    old_state: u64,
    new_state: u64,
}

impl Transaction {
    fn bytes(&self) -> usize {
        self.removed.len() + self.inserted.len()
    }
}

#[derive(Debug)]
pub struct Document {
    text: String,
    format: text::Format,
    newlines: usize,
    selection: Selection,
    revision: u64,
    state: u64,
    saved: Option<u64>,
    undo: VecDeque<Transaction>,
    redo: VecDeque<Transaction>,
    auto_fill: bool,
    fill_column: usize,
}

impl Document {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn format(&self) -> text::Format {
        self.format
    }
    pub fn selection(&self) -> Selection {
        self.selection
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn dirty(&self) -> bool {
        self.saved != Some(self.state)
    }
    pub fn auto_fill(&self) -> bool {
        self.auto_fill
    }
    pub fn fill_column(&self) -> usize {
        self.fill_column
    }
    pub fn history_depth(&self) -> (usize, usize) {
        (self.undo.len(), self.redo.len())
    }
}

/// A save adapter captures this alongside bytes, then acknowledges only
/// after writing those bytes. Fields are private to prevent invented states.
#[derive(Clone, Debug)]
pub struct SavePoint {
    owner: Arc<()>,
    tab: TabId,
    state: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Motion {
    Left,
    Right,
    Home,
    End,
    WordLeft,
    WordRight,
    DocumentStart,
    DocumentEnd,
}

#[derive(Debug)]
pub enum Command {
    Select(Selection),
    Insert(String),
    Type(char),
    Backspace,
    Delete,
    Undo,
    Redo,
    Move {
        motion: Motion,
        extend: bool,
    },
    FillParagraph,
    AutoFill(bool),
    FillColumn(usize),
    Find {
        needle: String,
        backward: bool,
        wrap: bool,
    },
    ReplaceAll {
        needle: String,
        replacement: String,
    },
}

#[derive(Debug)]
pub struct Editor {
    identity: Arc<()>,
    tabs: BTreeMap<TabId, Document>,
    active: Option<TabId>,
    next_tab: u64,
    next_state: u64,
    next_transaction: u64,
    limits: Limits,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            identity: Arc::new(()),
            tabs: BTreeMap::new(),
            active: None,
            next_tab: 1,
            next_state: 1,
            next_transaction: 1,
            limits: Limits::default(),
        }
    }
}

impl Editor {
    /// Callers may lower budgets for tests or a constrained embedding.
    pub fn with_limits(limits: Limits) -> Result<Self> {
        let cap = Limits::default();
        if limits.file_bytes == 0
            || limits.file_bytes > cap.file_bytes
            || limits.text_bytes == 0
            || limits.text_bytes > cap.text_bytes
            || limits.tabs == 0
            || limits.tabs > cap.tabs
            || limits.history_bytes > cap.history_bytes
            || limits.transactions > cap.transactions
        {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            limits,
            ..Self::default()
        })
    }

    pub fn active(&self) -> Option<TabId> {
        self.active
    }
    pub fn tabs(&self) -> impl Iterator<Item = (TabId, &Document)> {
        self.tabs.iter().map(|(&id, doc)| (id, doc))
    }
    pub fn document(&self, id: TabId) -> Result<&Document> {
        self.tabs.get(&id).ok_or(Error::MissingTab)
    }
    fn document_mut(&mut self, id: TabId) -> Result<&mut Document> {
        self.tabs.get_mut(&id).ok_or(Error::MissingTab)
    }
    pub fn total_text_bytes(&self) -> usize {
        self.tabs.values().map(|doc| doc.text.len()).sum()
    }
    pub fn history_usage(&self) -> (usize, usize) {
        self.tabs
            .values()
            .flat_map(|doc| doc.undo.iter().chain(doc.redo.iter()))
            .fold((0, 0), |(bytes, count), tx| (bytes + tx.bytes(), count + 1))
    }

    pub fn new_tab(&mut self) -> Result<TabId> {
        self.add(
            text::Decoded {
                text: String::new(),
                format: text::Format::default(),
            },
            true,
        )
    }
    pub fn load_bytes(&mut self, bytes: &[u8]) -> Result<TabId> {
        if bytes.len() > self.limits.file_bytes {
            return Err(Error::Limit);
        }
        self.add(text::decode(bytes)?, true)
    }
    fn add(&mut self, decoded: text::Decoded, saved: bool) -> Result<TabId> {
        if self.tabs.len() >= self.limits.tabs
            || self.total_text_bytes() + decoded.text.len() > self.limits.text_bytes
        {
            return Err(Error::Limit);
        }
        let next_tab = self.next_tab.checked_add(1).ok_or(Error::Exhausted)?;
        let next_state = self.next_state.checked_add(1).ok_or(Error::Exhausted)?;
        let id = self.next_tab;
        let state = self.next_state;
        let newlines = decoded.text.bytes().filter(|&b| b == b'\n').count();
        self.tabs.insert(
            id,
            Document {
                text: decoded.text,
                format: decoded.format,
                newlines,
                selection: Selection::default(),
                revision: 0,
                state,
                saved: saved.then_some(state),
                undo: VecDeque::new(),
                redo: VecDeque::new(),
                auto_fill: false,
                fill_column: 72,
            },
        );
        self.next_tab = next_tab;
        self.next_state = next_state;
        self.active = Some(id);
        Ok(id)
    }

    pub fn select_tab(&mut self, id: TabId) -> Result<()> {
        self.document(id)?;
        self.active = Some(id);
        Ok(())
    }
    pub fn next_tab(&mut self, backward: bool) -> Result<()> {
        let active = self.active.ok_or(Error::MissingTab)?;
        let id = if backward {
            self.tabs
                .range(..active)
                .next_back()
                .or_else(|| self.tabs.iter().next_back())
        } else {
            self.tabs
                .range((
                    std::ops::Bound::Excluded(active),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .or_else(|| self.tabs.iter().next())
        }
        .map(|(&id, _)| id)
        .ok_or(Error::MissingTab)?;
        self.select_tab(id)
    }

    /// Dirty close is refused. A later dialog adapter owns explicit discard;
    /// there is deliberately no bool that lets a replay caller skip consent.
    pub fn close_tab(&mut self, id: TabId, revision: u64) -> Result<()> {
        let doc = self.checked(id, revision)?;
        if doc.dirty() {
            return Err(Error::Dirty);
        }
        self.tabs.remove(&id);
        if self.active == Some(id) {
            self.active = self.tabs.keys().next().copied();
        }
        Ok(())
    }

    pub fn save_snapshot(&self, id: TabId) -> Result<(SavePoint, Vec<u8>)> {
        let doc = self.document(id)?;
        Ok((
            SavePoint {
                owner: self.identity.clone(),
                tab: id,
                state: doc.state,
            },
            text::encode(&doc.text, doc.format)?,
        ))
    }
    pub fn acknowledge_saved(&mut self, point: SavePoint) -> Result<()> {
        if !Arc::ptr_eq(&self.identity, &point.owner) {
            return Err(Error::InvalidArgument);
        }
        self.document_mut(point.tab)?.saved = Some(point.state);
        Ok(())
    }

    fn checked(&self, id: TabId, revision: u64) -> Result<&Document> {
        let doc = self.document(id)?;
        if doc.revision != revision {
            return Err(Error::StaleRevision);
        }
        Ok(doc)
    }

    pub fn dispatch(&mut self, id: TabId, revision: u64, command: Command) -> Result<()> {
        let doc = self.checked(id, revision)?;
        match command {
            Command::Select(selection) => {
                if !doc.text.is_char_boundary(selection.anchor)
                    || !doc.text.is_char_boundary(selection.caret)
                {
                    return Err(Error::InvalidPosition);
                }
                self.document_mut(id)?.selection = selection;
            }
            Command::Insert(value) => {
                let insert = text::insertion(&value)?;
                let range = doc.selection.range();
                let at = range.start + insert.len();
                self.edit(
                    id,
                    fill::Edit {
                        range,
                        insert,
                        anchor: at,
                        caret: at,
                    },
                )?;
            }
            Command::Type(scalar) => {
                if doc.auto_fill && matches!(scalar, ' ' | '\t') {
                    let edit =
                        fill::auto_fill(&doc.text, doc.selection.range(), scalar, doc.fill_column)?;
                    self.edit(id, edit)?;
                } else {
                    self.dispatch(id, revision, Command::Insert(scalar.to_string()))?;
                }
            }
            Command::Backspace | Command::Delete => {
                let mut range = doc.selection.range();
                if range.is_empty() {
                    if matches!(command, Command::Backspace) {
                        range.start = previous(&doc.text, range.start)?;
                    } else {
                        range.end = following(&doc.text, range.end)?;
                    }
                }
                let at = range.start;
                self.edit(
                    id,
                    fill::Edit {
                        range,
                        insert: String::new(),
                        anchor: at,
                        caret: at,
                    },
                )?;
            }
            Command::Undo => self.replay_history(id, false)?,
            Command::Redo => self.replay_history(id, true)?,
            Command::Move { motion, extend } => {
                let caret = if !extend && !doc.selection.range().is_empty() {
                    match motion {
                        Motion::Left => doc.selection.range().start,
                        Motion::Right => doc.selection.range().end,
                        _ => destination(&doc.text, doc.selection.caret, motion)?,
                    }
                } else {
                    destination(&doc.text, doc.selection.caret, motion)?
                };
                let anchor = if extend { doc.selection.anchor } else { caret };
                self.document_mut(id)?.selection = Selection { anchor, caret };
            }
            Command::FillParagraph => {
                let edit = fill::paragraph(
                    &doc.text,
                    doc.selection.anchor,
                    doc.selection.caret,
                    doc.fill_column,
                )?;
                self.edit(id, edit)?;
            }
            Command::AutoFill(enabled) => self.document_mut(id)?.auto_fill = enabled,
            Command::FillColumn(column) => {
                if !(20..=240).contains(&column) {
                    return Err(Error::InvalidArgument);
                }
                self.document_mut(id)?.fill_column = column;
            }
            Command::Find {
                needle,
                backward,
                wrap,
            } => {
                if needle.is_empty() || needle.len() > self.limits.file_bytes {
                    return Err(Error::InvalidArgument);
                }
                let range = doc.selection.range();
                let matched = if backward {
                    doc.text
                        .get(..range.start)
                        .and_then(|s| s.rfind(&needle))
                        .or_else(|| wrap.then(|| doc.text.rfind(&needle)).flatten())
                } else {
                    doc.text
                        .get(range.end..)
                        .and_then(|s| s.find(&needle))
                        .map(|n| range.end + n)
                        .or_else(|| wrap.then(|| doc.text.find(&needle)).flatten())
                };
                let at = matched.ok_or(Error::Unavailable)?;
                self.document_mut(id)?.selection = Selection {
                    anchor: at,
                    caret: at + needle.len(),
                };
            }
            Command::ReplaceAll {
                needle,
                replacement,
            } => {
                if needle.is_empty() || needle.len() > self.limits.file_bytes {
                    return Err(Error::InvalidArgument);
                }
                let replacement = text::insertion(&replacement)?;
                let count = doc.text.matches(&needle).count();
                if count == 0 {
                    return Ok(());
                }
                let size = doc
                    .text
                    .len()
                    .checked_sub(count * needle.len())
                    .and_then(|n| {
                        count
                            .checked_mul(replacement.len())
                            .and_then(|extra| n.checked_add(extra))
                    })
                    .ok_or(Error::Limit)?;
                if size > self.limits.file_bytes {
                    return Err(Error::Limit);
                }
                let insert = doc.text.replace(&needle, &replacement);
                let caret = insert.len();
                self.edit(
                    id,
                    fill::Edit {
                        range: 0..doc.text.len(),
                        insert,
                        anchor: caret,
                        caret,
                    },
                )?;
            }
        }
        Ok(())
    }

    fn admission(&self, id: TabId, range: Range<usize>, insert: &str) -> Result<usize> {
        let doc = self.document(id)?;
        let removed = doc.text.get(range.clone()).ok_or(Error::InvalidPosition)?;
        let len = doc.text.len() - removed.len() + insert.len();
        let newlines = doc.newlines - removed.bytes().filter(|&b| b == b'\n').count()
            + insert.bytes().filter(|&b| b == b'\n').count();
        let size = text::encoded_len(len, newlines, doc.format)?;
        if size > self.limits.file_bytes
            || self.total_text_bytes() - doc.text.len() + len > self.limits.text_bytes
        {
            return Err(Error::Limit);
        }
        let first = if range.start != 0 {
            doc.text.chars().next()
        } else {
            insert
                .chars()
                .next()
                .or_else(|| doc.text.get(range.end..).and_then(|s| s.chars().next()))
        };
        if first == Some('\u{feff}') {
            return Err(Error::InvalidText);
        }
        Ok(newlines)
    }

    fn edit(&mut self, id: TabId, edit: fill::Edit) -> Result<()> {
        let newlines = self.admission(id, edit.range.clone(), &edit.insert)?;
        let doc = self.document(id)?;
        let removed = doc
            .text
            .get(edit.range.clone())
            .ok_or(Error::InvalidPosition)?;
        for point in [edit.anchor, edit.caret] {
            if !result_boundary(&doc.text, edit.range.clone(), &edit.insert, point) {
                return Err(Error::InvalidPosition);
            }
        }
        if removed == edit.insert {
            self.document_mut(id)?.selection = Selection {
                anchor: edit.anchor,
                caret: edit.caret,
            };
            return Ok(());
        }
        // Keep a single replacement span, but do not charge unchanged ends
        // of a paragraph or Replace All result to the undo budget.
        let prefix: usize = removed
            .chars()
            .zip(edit.insert.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        let old_tail = removed.get(prefix..).ok_or(Error::InvalidPosition)?;
        let new_tail = edit.insert.get(prefix..).ok_or(Error::InvalidPosition)?;
        let suffix: usize = old_tail
            .chars()
            .rev()
            .zip(new_tail.chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        let removed = removed
            .get(prefix..removed.len() - suffix)
            .ok_or(Error::InvalidPosition)?;
        let inserted = edit
            .insert
            .get(prefix..edit.insert.len() - suffix)
            .ok_or(Error::InvalidPosition)?
            .to_string();
        let range = (edit.range.start + prefix)..(edit.range.end - suffix);
        let revision = doc.revision.checked_add(1).ok_or(Error::Exhausted)?;
        let next_state = self.next_state.checked_add(1).ok_or(Error::Exhausted)?;
        let next_transaction = self
            .next_transaction
            .checked_add(1)
            .ok_or(Error::Exhausted)?;
        let tx = Transaction {
            serial: self.next_transaction,
            at: range.start,
            removed: removed.to_string(),
            inserted,
            before: doc.selection,
            after: Selection {
                anchor: edit.anchor,
                caret: edit.caret,
            },
            old_state: doc.state,
            new_state: self.next_state,
        };
        let doc = self.document_mut(id)?;
        // Only the validated replacement range reaches String::replace_range.
        doc.text.replace_range(range, &tx.inserted);
        doc.newlines = newlines;
        doc.selection = tx.after;
        doc.revision = revision;
        doc.state = tx.new_state;
        doc.redo.clear();
        doc.undo.push_back(tx);
        self.next_state = next_state;
        self.next_transaction = next_transaction;
        self.trim_history();
        Ok(())
    }

    fn trim_history(&mut self) {
        loop {
            let (bytes, count) = self.history_usage();
            if bytes <= self.limits.history_bytes && count <= self.limits.transactions {
                break;
            }
            let oldest = self
                .tabs
                .iter()
                .filter_map(|(&id, doc)| {
                    doc.undo
                        .front()
                        .into_iter()
                        .chain(doc.redo.back())
                        .map(|tx| (tx.serial, id))
                        .min()
                })
                .min();
            let Some((serial, id)) = oldest else {
                break;
            };
            let Some(doc) = self.tabs.get_mut(&id) else {
                break;
            };
            if doc.undo.front().is_some_and(|tx| tx.serial == serial) {
                doc.undo.pop_front();
            } else {
                // Removing the next redo step invalidates its later steps.
                doc.redo.clear();
            }
        }
    }

    fn replay_history(&mut self, id: TabId, redo: bool) -> Result<()> {
        let doc = self.document(id)?;
        let Some(tx) = (if redo { &doc.redo } else { &doc.undo }).back() else {
            return Ok(());
        };
        let (remove, insert, selection, state) = if redo {
            (&tx.removed, &tx.inserted, tx.after, tx.new_state)
        } else {
            (&tx.inserted, &tx.removed, tx.before, tx.old_state)
        };
        let range = tx.at..(tx.at + remove.len());
        if doc.text.get(range.clone()) != Some(remove.as_str()) {
            return Err(Error::InvalidPosition);
        }
        let newlines = self.admission(id, range.clone(), insert)?;
        let revision = doc.revision.checked_add(1).ok_or(Error::Exhausted)?;
        let insert = insert.clone();
        let doc = self.document_mut(id)?;
        doc.text.replace_range(range, &insert);
        doc.selection = selection;
        doc.state = state;
        doc.newlines = newlines;
        doc.revision = revision;
        if redo {
            if let Some(tx) = doc.redo.pop_back() {
                doc.undo.push_back(tx);
            }
        } else if let Some(tx) = doc.undo.pop_back() {
            doc.redo.push_back(tx);
        }
        Ok(())
    }
}

fn result_boundary(text: &str, range: Range<usize>, insert: &str, point: usize) -> bool {
    if point <= range.start {
        return text.is_char_boundary(point);
    }
    let new_end = range.start + insert.len();
    if point < new_end {
        return insert.is_char_boundary(point - range.start);
    }
    range
        .end
        .checked_add(point - new_end)
        .is_some_and(|old| text.is_char_boundary(old))
}

fn previous(text: &str, at: usize) -> Result<usize> {
    Ok(text
        .get(..at)
        .ok_or(Error::InvalidPosition)?
        .char_indices()
        .next_back()
        .map_or(0, |(at, _)| at))
}
fn following(text: &str, at: usize) -> Result<usize> {
    Ok(text
        .get(at..)
        .ok_or(Error::InvalidPosition)?
        .chars()
        .next()
        .map_or(at, |c| at + c.len_utf8()))
}
fn destination(text: &str, at: usize, motion: Motion) -> Result<usize> {
    match motion {
        Motion::Left => previous(text, at),
        Motion::Right => following(text, at),
        Motion::Home => Ok(text::line(text, at)?.start),
        Motion::End => Ok(text::line(text, at)?.end),
        Motion::DocumentStart => Ok(0),
        Motion::DocumentEnd => Ok(text.len()),
        Motion::WordLeft => {
            let mut pos = at;
            let mut word = false;
            for (index, c) in text
                .get(..at)
                .ok_or(Error::InvalidPosition)?
                .char_indices()
                .rev()
            {
                if word && !c.is_alphanumeric() {
                    break;
                }
                word |= c.is_alphanumeric();
                pos = index;
            }
            Ok(pos)
        }
        Motion::WordRight => {
            let mut pos = at;
            let mut word = false;
            for (index, c) in text.get(at..).ok_or(Error::InvalidPosition)?.char_indices() {
                if word && !c.is_alphanumeric() {
                    break;
                }
                word |= c.is_alphanumeric();
                pos = at + index + c.len_utf8();
            }
            Ok(pos)
        }
    }
}
