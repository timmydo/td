//! Window file coordinator. The worker alone owns filesystem associations;
//! completions enter the same controller as ordinary editing commands.

use crate::files::{FileId, Session as Files};
use crate::model::{SavePoint, TabId};
use crate::ui::{Controller, Event, Outcome};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

type Result<T> = std::result::Result<T, String>;

#[derive(Debug)]
pub(crate) struct PollFailure {
    pub(crate) code: crate::Error,
    pub(crate) detail: String,
}
impl PollFailure {
    fn unavailable(detail: String) -> Self {
        Self {
            code: crate::Error::Unavailable,
            detail,
        }
    }
}

enum Operation {
    Copy {
        source: crate::files::RenameSource,
        name: std::ffi::OsString,
        reserved: Vec<PathBuf>,
    },
    Mkdir {
        source: crate::files::DirectorySource,
        name: std::ffi::OsString,
    },
    Delete(crate::files::DeletePlan),
    Dictionary(PathBuf),
    Open(PathBuf),
    Rename {
        source: crate::files::RenameSource,
        name: std::ffi::OsString,
        reserved: Vec<PathBuf>,
    },
    Reload(FileId),
    Save {
        file: Option<FileId>,
        path: Option<PathBuf>,
        bytes: Vec<u8>,
    },
}
struct Job {
    keep: BTreeSet<FileId>,
    operation: Operation,
}
struct Loaded {
    file: FileId,
    path: PathBuf,
    bytes: Vec<u8>,
    missing: bool,
}
enum Completion {
    CreatedPath {
        path: PathBuf,
        notice: String,
        listing: Result<Option<crate::directory::Snapshot>>,
    },
    Deleted {
        result: crate::files::Deleted,
        listing: Result<Option<crate::directory::Snapshot>>,
    },
    Dictionary(crate::spelling::Dictionary),
    Open(Loaded),
    Directory(crate::directory::Snapshot),
    Renamed {
        result: crate::files::Renamed,
        listings: Vec<Result<Option<crate::directory::Snapshot>>>,
    },
    Reload(Loaded),
    Conflict(String),
    Saved {
        file: FileId,
        path: PathBuf,
    },
}
enum Pending {
    Copy(TabId),
    Mkdir(TabId),
    Delete,
    Dictionary,
    Open,
    Rename,
    Browse(crate::model::RevisionPoint),
    Reload {
        permit: Option<crate::Reload>,
    },
    Save {
        tab: TabId,
        point: SavePoint,
    },
    QueuedSave {
        point: crate::model::RevisionPoint,
        path: Option<PathBuf>,
    },
}
struct Association {
    file: FileId,
    title: String,
    path: PathBuf,
}

pub(crate) struct Session {
    #[cfg(feature = "test-file-barrier")]
    queue_gate: Option<crate::test_file_barrier::QueueGate>,
    sender: SyncSender<Job>,
    receiver: Receiver<Result<Completion>>,
    pending: Option<Pending>,
    associations: BTreeMap<TabId, Association>,
    directories: BTreeMap<TabId, crate::directory::Snapshot>,
    failed: bool,
    conflict: Option<crate::dialog::Target>,
    dictionary: Option<crate::spelling::Dictionary>,
}

impl Session {
    pub(crate) fn start() -> Result<Self> {
        #[cfg(feature = "test-file-barrier")]
        let queue_gate = crate::test_file_barrier::QueueGate::start()?;
        let (sender, jobs) = mpsc::sync_channel(1);
        let (results, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("td-editor-files".into())
            .spawn(move || worker(jobs, results))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            #[cfg(feature = "test-file-barrier")]
            queue_gate,
            sender,
            receiver,
            pending: None,
            associations: BTreeMap::new(),
            directories: BTreeMap::new(),
            failed: false,
            conflict: None,
            dictionary: None,
        })
    }

    pub(crate) fn busy(&self) -> bool {
        self.pending.is_some()
    }

    fn available(&self) -> Result<()> {
        if self.failed {
            Err("File worker disconnected; restart the editor".into())
        } else if self.busy() {
            Err("File operation pending; wait before trying again".into())
        } else {
            Ok(())
        }
    }

    fn submit(&mut self, operation: Operation, pending: Pending) -> Result<()> {
        self.available()?;
        let keep = self.associations.values().map(|a| a.file).collect();
        self.sender
            .try_send(Job { keep, operation })
            .map_err(|e| e.to_string())?;
        self.pending = Some(pending);
        Ok(())
    }

    pub(crate) fn open(&mut self, path: PathBuf) -> Result<()> {
        if path.as_os_str().as_bytes().len() > 4096 {
            return Err("Path exceeds 4096 bytes".into());
        }
        self.submit(Operation::Open(path), Pending::Open)
    }

    pub(crate) fn browse(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        path: PathBuf,
        new_tab: bool,
    ) -> Result<()> {
        let point = ui
            .editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        if !ui
            .editor()
            .document(tab)
            .map_err(|e| e.to_string())?
            .directory()
        {
            return Err("Navigation needs a directory tab".into());
        }
        if path.as_os_str().as_bytes().len() > 4096 {
            return Err("Path exceeds 4096 bytes".into());
        }
        self.submit(
            Operation::Open(path),
            if new_tab {
                Pending::Open
            } else {
                Pending::Browse(point)
            },
        )
    }

    pub(crate) fn directory(&self, tab: TabId) -> Option<&crate::directory::Snapshot> {
        self.directories.get(&tab)
    }

    pub(crate) fn mark_directory(
        &mut self,
        ui: &mut Controller,
        tab: TabId,
        revision: u64,
        delete: bool,
    ) -> Result<()> {
        self.available()?;
        let point = ui
            .editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        let doc = ui.editor().document(tab).map_err(|e| e.to_string())?;
        let row = doc
            .text()
            .get(..doc.selection().caret)
            .ok_or("Invalid selection")?
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        let mut next = self.directory(tab).ok_or("Not a directory tab")?.clone();
        next.mark(row, delete)?;
        // Like dired, marking advances by one entry, but never beyond EOF.
        let selected = next.entry((row + 1).min(next.len().saturating_sub(1)));
        let caret = selected.as_ref().map_or(0, |path| next.offset(path));
        ui.refresh_directory(point, next.text.as_bytes(), caret)
            .map_err(|e| e.to_string())?;
        next.text = String::new();
        self.directories.insert(tab, next);
        Ok(())
    }

    pub(crate) fn copy(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        source: crate::files::RenameSource,
        name: std::ffi::OsString,
    ) -> Result<()> {
        ui.editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        let directory = self.directory(tab).ok_or("Copy needs a directory tab")?;
        if source.path().parent() != Some(directory.path.as_path()) {
            return Err("Copy source belongs to a different directory".into());
        }
        let path = source.copy_destination(&name).map_err(|e| e.to_string())?;
        if self
            .directories
            .values()
            .any(|snapshot| snapshot.path.starts_with(&path))
        {
            return Err("Copy destination belongs to an open directory tab".into());
        }
        let reserved = self
            .directories
            .values()
            .map(|snapshot| snapshot.path.clone())
            .collect();
        self.submit(
            Operation::Copy {
                source,
                name,
                reserved,
            },
            Pending::Copy(tab),
        )
    }

    pub(crate) fn create_directory(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        source: crate::files::DirectorySource,
        name: std::ffi::OsString,
    ) -> Result<()> {
        ui.editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        let directory = self
            .directory(tab)
            .ok_or("Creation needs a directory tab")?;
        if source.path() != directory.path {
            return Err("Creation belongs to a different directory".into());
        }
        let path = source.destination(&name).map_err(|e| e.to_string())?;
        if self
            .directories
            .values()
            .any(|snapshot| snapshot.path.starts_with(&path))
        {
            return Err("Destination belongs to an open directory tab".into());
        }
        self.submit(Operation::Mkdir { source, name }, Pending::Mkdir(tab))
    }

    pub(crate) fn delete(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        plan: crate::files::DeletePlan,
    ) -> Result<()> {
        ui.editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        let directory = self
            .directory(tab)
            .ok_or("Deletion needs a directory tab")?;
        if plan
            .paths()
            .any(|path| path.parent() != Some(directory.path.as_path()))
        {
            return Err("Deletion plan belongs to a different directory".into());
        }
        for path in plan.paths() {
            if self
                .directories
                .values()
                .any(|snapshot| snapshot.path.starts_with(path))
            {
                return Err("Deletion includes an open directory; close that tab first".into());
            }
        }
        self.submit(Operation::Delete(plan), Pending::Delete)
    }

    pub(crate) fn rename(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        source: crate::files::RenameSource,
        name: std::ffi::OsString,
    ) -> Result<()> {
        ui.editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        let directory = self.directory(tab).ok_or("Rename needs a directory tab")?;
        if source.path().parent() != Some(directory.path.as_path()) {
            return Err("Rename source belongs to a different directory".into());
        }
        let to = source.copy_destination(&name).map_err(|e| e.to_string())?;
        // Cached directory paths are not owned by the file worker. Admit
        // their rebasing before it can publish any filesystem change.
        for snapshot in self.directories.values() {
            if let Ok(suffix) = snapshot.path.strip_prefix(source.path()) {
                if to.join(suffix).as_os_str().as_bytes().len() > 4096 {
                    return Err("Renamed directory-tab path exceeds 4096 bytes".into());
                }
            }
        }
        let reserved = self
            .directories
            .values()
            .map(|snapshot| snapshot.path.clone())
            .collect();
        self.submit(
            Operation::Rename {
                source,
                name,
                reserved,
            },
            Pending::Rename,
        )
    }

    pub(crate) fn sort_directory(
        &mut self,
        ui: &mut Controller,
        tab: TabId,
        revision: u64,
        sort: crate::directory::Sort,
        reverse: bool,
    ) -> Result<()> {
        self.available()?;
        let point = ui
            .editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        let doc = ui.editor().document(tab).map_err(|e| e.to_string())?;
        let row = doc
            .text()
            .get(..doc.selection().caret)
            .ok_or("Invalid directory selection")?
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        let old = self.directory(tab).ok_or("Not a directory tab")?;
        if old.sort == sort && old.reverse == reverse {
            return Ok(());
        }
        ui.generation()
            .checked_add(2)
            .ok_or("Directory layout counter exhausted")?;
        let next = revision
            .checked_add(1)
            .ok_or("Directory revision exhausted")?;
        let selected = old.entry(row);
        let mut snapshot = old.clone();
        snapshot.arrange(sort, reverse);
        let caret = selected.as_ref().map_or(0, |path| snapshot.offset(path));
        ui.dispatch(Event::Open(crate::model::Open {
            source: Some(point),
            bytes: snapshot.text.as_bytes(),
            missing: false,
            directory: true,
            existing: None,
        }))
        .map_err(|e| e.to_string())?;
        // Both generations were reserved; caret is a boundary in this exact
        // admitted ASCII listing, so selection cannot fail after replacement.
        ui.dispatch(Event::Edit {
            tab,
            revision: next,
            command: crate::model::Command::Select(crate::model::Selection {
                anchor: caret,
                caret,
            }),
        })
        .map_err(|e| e.to_string())?;
        snapshot.text = String::new();
        self.directories.insert(tab, snapshot);
        Ok(())
    }

    pub(crate) fn dictionary(&mut self, path: PathBuf) -> Result<()> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.len() > 4096 || bytes.contains(&0) {
            return Err("Dictionary path must contain 1..=4096 non-NUL bytes".into());
        }
        if self.dictionary.is_some() {
            return Err("Dictionary completion must be consumed first".into());
        }
        self.submit(Operation::Dictionary(path), Pending::Dictionary)
    }

    pub(crate) fn take_dictionary(&mut self) -> Option<crate::spelling::Dictionary> {
        self.dictionary.take()
    }

    /// Startup only, before connecting the display.
    pub(crate) fn initial_dictionary(&mut self, ui: &mut Controller, path: PathBuf) -> Result<()> {
        self.dictionary(path)?;
        let result = self.receiver.recv().map_err(|e| e.to_string())?;
        let pending = self.pending.take();
        self.finish(ui, pending, result).map(|_| ())
    }

    /// Only before connecting the display; the event loop uses poll instead.
    pub(crate) fn initial_open(&mut self, ui: &mut Controller, path: PathBuf) -> Result<()> {
        self.open(path)?;
        let result = self.receiver.recv().map_err(|e| e.to_string())?;
        let pending = self.pending.take();
        self.finish(ui, pending, result).map(|_| ())
    }

    pub(crate) fn associated(&self, tab: TabId) -> bool {
        self.associations.contains_key(&tab)
    }
    pub(crate) fn path(&self, tab: TabId) -> Option<&std::path::Path> {
        self.associations
            .get(&tab)
            .map(|a| a.path.as_path())
            .or_else(|| self.directories.get(&tab).map(|d| d.path.as_path()))
    }
    pub(crate) fn take_conflict(&mut self) -> Option<crate::dialog::Target> {
        self.conflict.take()
    }

    pub(crate) fn reload(&mut self, ui: &Controller, permit: crate::Reload) -> Result<()> {
        self.available()?;
        permit.check(ui.editor()).map_err(|e| e.to_string())?;
        let file = self
            .associations
            .get(&permit.tab())
            .ok_or("Reload needs an associated file")?
            .file;
        self.submit(
            Operation::Reload(file),
            Pending::Reload {
                permit: Some(permit),
            },
        )
    }

    pub(crate) fn cancel_reload(&mut self) {
        if let Some(Pending::Reload { permit }) = self.pending.as_mut() {
            *permit = None;
        }
    }
    pub(crate) fn forget(&mut self, tab: TabId) {
        self.associations.remove(&tab);
        self.directories.remove(&tab);
    }
    pub(crate) fn labels(&self) -> impl Iterator<Item = (TabId, &str)> {
        self.associations
            .iter()
            .map(|(id, a)| (*id, a.title.as_str()))
            .chain(
                self.directories
                    .iter()
                    .map(|(id, d)| (*id, d.title.as_str())),
            )
    }

    pub(crate) fn save(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        path: Option<PathBuf>,
    ) -> Result<()> {
        let file = self.save_arguments(ui, tab, revision, &path)?;
        let (point, bytes) = ui.editor().save_snapshot(tab).map_err(|e| e.to_string())?;
        self.submit(
            Operation::Save { file, path, bytes },
            Pending::Save { tab, point },
        )
    }

    /// Remote admission reserves the single file slot, but no bytes yet.
    pub(crate) fn queue_save(
        &mut self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        path: Option<PathBuf>,
    ) -> Result<()> {
        self.save_arguments(ui, tab, revision, &path)?;
        let point = ui
            .editor()
            .revision_point(tab, revision)
            .map_err(|e| e.to_string())?;
        #[cfg(feature = "test-file-barrier")]
        if let Some(gate) = self.queue_gate.as_mut() {
            gate.begin()?;
        }
        self.pending = Some(Pending::QueuedSave { point, path });
        Ok(())
    }

    fn save_arguments(
        &self,
        ui: &Controller,
        tab: TabId,
        revision: u64,
        path: &Option<PathBuf>,
    ) -> Result<Option<FileId>> {
        self.available()?;
        if path
            .as_ref()
            .is_some_and(|p| p.as_os_str().as_bytes().len() > 4096)
        {
            return Err("Path exceeds 4096 bytes".into());
        }
        let doc = ui.editor().document(tab).map_err(|e| e.to_string())?;
        if doc.directory() {
            return Err("Directory listings are read-only; nothing written".into());
        }
        if doc.revision() != revision {
            return Err(
                "Save refused: tab changed since the request; retry Save (stale-revision)".into(),
            );
        }
        let file = self.associations.get(&tab).map(|a| a.file);
        if file.is_none() && path.is_none() {
            return Err("Save As needs a path".into());
        }
        Ok(file)
    }

    /// Nonblocking; at most one completion and one encoded snapshot can exist.
    pub(crate) fn poll(
        &mut self,
        ui: &mut Controller,
    ) -> Option<std::result::Result<String, PollFailure>> {
        #[cfg(feature = "test-file-barrier")]
        if matches!(self.pending, Some(Pending::QueuedSave { .. })) {
            if let Some(gate) = self.queue_gate.as_mut() {
                // No reply returns immediately without consuming the queued Save.
                if let Err(detail) = gate.poll()? {
                    self.pending.take();
                    return Some(Err(PollFailure::unavailable(detail)));
                }
            }
        }
        if let Some(Pending::QueuedSave { point, path }) = self
            .pending
            .take_if(|pending| matches!(pending, Pending::QueuedSave { .. }))
        {
            if let Err(code) = ui.editor().check_revision(&point) {
                return Some(Err(PollFailure {
                    code,
                    detail: format!("Queued Save refused ({code}); no write submitted"),
                }));
            }
            // This is the handoff boundary: validate before capturing bytes.
            // Later edits cannot change the immutable ordinary Save snapshot.
            return self
                .save(ui, point.tab, point.revision, path)
                .err()
                .map(|detail| Err(PollFailure::unavailable(detail)));
        }
        let result = match self.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) if self.failed => return None,
            Err(TryRecvError::Disconnected) => {
                self.failed = true;
                Err("File worker disconnected; a pending write may have reached disk. Verify the destination; text remains unsaved.".into())
            }
        };
        let pending = self.pending.take();
        Some(
            self.finish(ui, pending, result)
                .map_err(PollFailure::unavailable),
        )
    }

    fn finish(
        &mut self,
        ui: &mut Controller,
        pending: Option<Pending>,
        result: Result<Completion>,
    ) -> Result<String> {
        match (pending, result?) {
            (
                Some(Pending::Mkdir(origin) | Pending::Copy(origin)),
                Completion::CreatedPath {
                    path,
                    mut notice,
                    listing,
                },
            ) => {
                let mut stale = false;
                match listing {
                    Ok(Some(fresh)) => {
                        for (&tab, old) in &mut self.directories {
                            if old.path != fresh.path {
                                continue;
                            }
                            let refreshed = (|| -> Result<crate::directory::Snapshot> {
                                let doc = ui.editor().document(tab).map_err(|e| e.to_string())?;
                                let point = ui
                                    .editor()
                                    .revision_point(tab, doc.revision())
                                    .map_err(|e| e.to_string())?;
                                let row = doc
                                    .text()
                                    .get(..doc.selection().caret)
                                    .ok_or("Invalid selection")?
                                    .bytes()
                                    .filter(|b| *b == b'\n')
                                    .count();
                                let selected = if tab == origin {
                                    Some(path.clone())
                                } else {
                                    old.entry(row)
                                };
                                let mut next = fresh.clone();
                                next.arrange(old.sort, old.reverse);
                                let caret = selected.as_ref().map_or(0, |path| next.offset(path));
                                ui.refresh_directory(point, next.text.as_bytes(), caret)
                                    .map_err(|e| e.to_string())?;
                                next.text = String::new();
                                Ok(next)
                            })();
                            match refreshed {
                                Ok(next) => *old = next,
                                Err(_) => stale = true,
                            }
                        }
                    }
                    _ => stale = true,
                }
                if stale {
                    notice.push_str("; listing stale: use g to refresh");
                }
                Ok(notice)
            }
            (Some(Pending::Delete), Completion::Deleted { result, listing }) => {
                let mut notice = format!(
                    "Removed {}/{} entries permanently (not undoable)",
                    result.removed, result.requested
                );
                if let Some(failure) = &result.failure {
                    notice.push_str(&format!("; {failure}"));
                }
                match listing {
                    Ok(Some(fresh)) => {
                        for (&tab, old) in &mut self.directories {
                            if old.path != fresh.path {
                                continue;
                            }
                            let refreshed = (|| -> Result<crate::directory::Snapshot> {
                                let doc = ui.editor().document(tab).map_err(|e| e.to_string())?;
                                let point = ui
                                    .editor()
                                    .revision_point(tab, doc.revision())
                                    .map_err(|e| e.to_string())?;
                                let row = doc
                                    .text()
                                    .get(..doc.selection().caret)
                                    .ok_or("Invalid selection")?
                                    .bytes()
                                    .filter(|b| *b == b'\n')
                                    .count();
                                let selected = old.entry(row);
                                let mut next = fresh.clone();
                                next.arrange(old.sort, old.reverse);
                                let caret = selected.as_ref().map_or(0, |path| next.offset(path));
                                ui.refresh_directory(point, next.text.as_bytes(), caret)
                                    .map_err(|e| e.to_string())?;
                                next.text = String::new();
                                Ok(next)
                            })();
                            match refreshed {
                                Ok(next) => *old = next,
                                Err(_) => notice.push_str("; listing stale: use g to refresh"),
                            }
                        }
                    }
                    _ => notice.push_str("; listing stale: use g to refresh"),
                }
                if result.failure.is_some() {
                    Err(notice)
                } else {
                    Ok(notice)
                }
            }
            (Some(Pending::Rename), Completion::Renamed { result, listings }) => {
                for association in self.associations.values_mut() {
                    if let Some(path) = result.relocated(&association.path) {
                        association.path = path;
                        association.title = format!(
                            "{:?}",
                            association
                                .path
                                .file_name()
                                .unwrap_or(association.path.as_os_str())
                        )
                        .chars()
                        .take(512)
                        .collect();
                    }
                }
                for snapshot in self.directories.values_mut() {
                    if let Some(path) = result.relocated(&snapshot.path) {
                        snapshot.relocate(path);
                    }
                }
                let mut notice = result.warning.unwrap_or_else(|| {
                    format!(
                        "Renamed to {:?}; open tabs followed the new path",
                        result.to
                    )
                });
                for listing in listings {
                    match listing {
                        Ok(Some(fresh)) => {
                            for (&tab, old) in &mut self.directories {
                                if old.path != fresh.path {
                                    continue;
                                }
                                let replaced = (|| -> Result<crate::directory::Snapshot> {
                                    let doc =
                                        ui.editor().document(tab).map_err(|e| e.to_string())?;
                                    let revision = doc.revision();
                                    let point = ui
                                        .editor()
                                        .revision_point(tab, revision)
                                        .map_err(|e| e.to_string())?;
                                    let row = doc
                                        .text()
                                        .get(..doc.selection().caret)
                                        .ok_or("Invalid selection")?
                                        .bytes()
                                        .filter(|b| *b == b'\n')
                                        .count();
                                    let mut selected = old.entry(row);
                                    let mut next = fresh.clone();
                                    next.arrange(old.sort, old.reverse);
                                    if selected.as_ref() == Some(&result.from) {
                                        selected = if result.to.parent() == Some(next.path.as_path()) {
                                            Some(result.to.clone())
                                        } else {
                                            next.entry(row.min(next.len().saturating_sub(1)))
                                        };
                                    }
                                    let caret =
                                        selected.as_ref().map_or(0, |path| next.offset(path));
                                    ui.refresh_directory(point, next.text.as_bytes(), caret)
                                        .map_err(|e| e.to_string())?;
                                    next.text = String::new();
                                    Ok(next)
                                })();
                                match replaced {
                                    Ok(next) => *old = next,
                                    Err(_) => notice.push_str("; listing not refreshed: use g"),
                                }
                            }
                        }
                        _ => notice.push_str("; directory read failed: use g to refresh"),
                    }
                }
                Ok(notice)
            }
            (Some(Pending::Dictionary), Completion::Dictionary(dictionary)) => {
                let count = dictionary.entry_count();
                self.dictionary = Some(dictionary);
                Ok(format!(
                    "Dictionary loaded: {count} entries. F7 checks the whole document."
                ))
            }
            (Some(Pending::Save { tab, .. }), Completion::Conflict(detail)) => {
                if let Ok(doc) = ui.editor().document(tab) {
                    self.conflict = Some(crate::dialog::Target {
                        tab,
                        revision: doc.revision(),
                    });
                }
                Err(detail)
            }
            (Some(Pending::Reload { permit }), Completion::Reload(loaded)) => {
                let Some(permit) = permit else {
                    return Ok("Reload cancelled; text and old baseline retained".into());
                };
                if loaded.missing {
                    return Err("Reload refused: destination is missing. Text and old baseline retained; use Save As to a new path.".into());
                }
                let tab = permit.tab();
                ui.dispatch(Event::Reload {
                    permit,
                    bytes: &loaded.bytes,
                    missing: loaded.missing,
                })
                .map_err(|e| {
                    format!("Reload not admitted ({e}); text and old baseline retained")
                })?;
                self.associate(tab, loaded.file, &loaded.path);
                Ok("Reloaded disk snapshot; undo history cleared".into())
            }
            (
                Some(pending @ (Pending::Open | Pending::Browse(_))),
                mut completion @ (Completion::Open(_) | Completion::Directory(_)),
            ) => {
                let source = match pending {
                    Pending::Browse(point) => {
                        ui.editor().check_revision(&point).map_err(|e| e.to_string())?;
                        matches!(completion, Completion::Directory(_)).then_some(point)
                    }
                    _ => None,
                };
                let replaced = source.as_ref().map(|point| point.tab);
                if let (Some(tab), Completion::Directory(snapshot)) = (replaced, &mut completion) {
                    if let Some(old) = self.directory(tab) { snapshot.arrange(old.sort, old.reverse); }
                }
                let (bytes, missing, directory, existing) = match &completion {
                    Completion::Directory(snapshot) => {
                        (snapshot.text.as_bytes(), false, true, None)
                    }
                    Completion::Open(loaded) => (
                        loaded.bytes.as_slice(),
                        loaded.missing,
                        false,
                        self.associations
                            .iter()
                            .find(|(_, a)| a.file == loaded.file)
                            .map(|(&tab, _)| tab),
                    ),
                    _ => return Err("Invalid Open completion".into()),
                };
                let Outcome::Created(tab) = ui
                    .dispatch(Event::Open(crate::model::Open {
                        source,
                        bytes,
                        missing,
                        directory,
                        existing,
                    }))
                    .map_err(|e| match e {
                        crate::Error::Limit => {
                            "Open refused: tab or text budget exhausted; existing tabs unchanged"
                                .to_string()
                        }
                        _ => format!("Open was not admitted ({e}); existing tabs unchanged"),
                    })?
                else {
                    return Err("file admission did not create a tab".into());
                };
                if let Some(tab) = replaced {
                    self.forget(tab);
                }
                let loaded = match completion {
                    Completion::Open(loaded) => loaded,
                    Completion::Directory(mut snapshot) => {
                        snapshot.text = String::new();
                        self.directories.insert(tab, snapshot);
                        return Ok("Directory: Enter/click opens; files keep this tab; Shift new directory tab; q close; ^ parent; g refresh".into());
                    }
                    _ => return Err("Invalid Open completion".into()),
                };
                if existing.is_some() {
                    return Ok("Selected already-open file; edits and baseline retained".into());
                }
                self.associate(tab, loaded.file, &loaded.path);
                Ok(format!(
                    "Opened{}: {:?}",
                    if loaded.missing {
                        " (new file, not saved)"
                    } else {
                        ""
                    },
                    loaded.path,
                ))
            }
            (Some(Pending::Save { tab, point }), Completion::Saved { file, path }) => {
                ui.dispatch(Event::Saved(point))
                    .map_err(|e| e.to_string())?;
                self.associate(tab, file, &path);
                let dirty = ui
                    .editor()
                    .document(tab)
                    .map_err(|e| e.to_string())?
                    .dirty();
                Ok(format!(
                    "Saved snapshot{}: {:?}",
                    if dirty {
                        "; newer edits remain unsaved"
                    } else {
                        ""
                    },
                    path,
                ))
            }
            _ => Err("unexpected file completion".into()),
        }
    }

    fn associate(&mut self, tab: TabId, file: FileId, path: &std::path::Path) {
        let title = format!("{:?}", path.file_name().unwrap_or(path.as_os_str()))
            .chars()
            .take(512)
            .collect();
        self.associations.insert(
            tab,
            Association {
                file,
                title,
                path: path.to_path_buf(),
            },
        );
    }
}

fn worker(jobs: Receiver<Job>, results: SyncSender<Result<Completion>>) {
    #[cfg(feature = "test-file-barrier")]
    let mut barrier = match crate::test_file_barrier::Barrier::connect() {
        Ok(barrier) => barrier,
        Err(detail) => {
            let _ = results.send(Err(detail));
            return;
        }
    };
    let mut files = Files::default();
    let mut known = BTreeSet::new();
    let mut incoming = jobs.recv();
    while let Ok(job) = incoming {
        #[cfg(feature = "test-file-barrier")]
        if let Some(barrier) = barrier.as_mut() {
            let kind = match &job.operation {
                Operation::Dictionary(_) => "dictionary",
                Operation::Open(_) => "open",
                Operation::Reload(_) => "reload",
                Operation::Save { .. } => "save",
                Operation::Rename { .. } => "rename",
                Operation::Delete(_) => "delete",
                Operation::Mkdir { .. } => "mkdir",
                Operation::Copy { .. } => "copy",
            };
            if let Err(detail) = barrier.checkpoint(kind) {
                let _ = results.send(Err(detail));
                return;
            }
        }
        let result = if let Operation::Reload(original) = job.operation {
            retain_files(&mut files, &mut known, &job.keep);
            match files.prepare_reload(original) {
                Ok(candidate) => {
                    let replacement = candidate.file_id();
                    let loaded = Loaded {
                        file: replacement,
                        path: candidate.path().to_owned(),
                        bytes: candidate.bytes().to_vec(),
                        missing: candidate.missing(),
                    };
                    if results.send(Ok(Completion::Reload(loaded))).is_err() {
                        break;
                    }
                    incoming = jobs.recv();
                    if incoming.as_ref().is_ok_and(|next| {
                        next.keep.contains(&replacement) && !next.keep.contains(&original)
                    }) {
                        candidate.commit();
                        known.remove(&original);
                        known.insert(replacement);
                    }
                    // Otherwise dropping the borrowed candidate cancels it.
                    continue;
                }
                Err(detail) => Err(file_error(detail)),
            }
        } else {
            execute(&mut files, &mut known, job)
        };
        if results.send(result).is_err() {
            break;
        }
        incoming = jobs.recv();
    }
}

fn retain_files(files: &mut Files, known: &mut BTreeSet<FileId>, keep: &BTreeSet<FileId>) {
    known.retain(|id| {
        if keep.contains(id) {
            true
        } else {
            files.forget(*id);
            false
        }
    });
}

fn execute(files: &mut Files, known: &mut BTreeSet<FileId>, job: Job) -> Result<Completion> {
    // Release closed tabs and rejected Open admissions before the next job.
    retain_files(files, known, &job.keep);
    match job.operation {
        Operation::Copy {
            source,
            name,
            reserved,
        } => {
            let result = files
                .copy_reserved(source, &name, &reserved)
                .map_err(|e| format!("Copy failed: {e}"))?;
            let listing = result
                .path
                .parent()
                .ok_or_else(|| "Copy has no parent".to_string())
                .and_then(crate::directory::read);
            Ok(Completion::CreatedPath {
                path: result.path,
                notice: result.warning.unwrap_or_else(|| "File copied".into()),
                listing,
            })
        }
        Operation::Mkdir { source, name } => {
            let parent = source.path().to_path_buf();
            let result = files
                .create_directory(source, &name)
                .map_err(|e| format!("Directory creation failed: {e}"))?;
            let listing = crate::directory::read(&parent);
            Ok(Completion::CreatedPath {
                path: result.path,
                notice: result.warning.unwrap_or_else(|| "Directory created".into()),
                listing,
            })
        }
        Operation::Delete(plan) => {
            let result = files
                .delete(plan)
                .map_err(|e| format!("Deletion refused: {e}"))?;
            let listing = crate::directory::read(&result.parent);
            Ok(Completion::Deleted { result, listing })
        }
        Operation::Rename {
            source,
            name,
            reserved,
        } => {
            let result = files
                .rename_reserved(source, &name, &reserved)
                .map_err(|e| format!("Rename failed: {e}"))?;
            let mut listings = vec![result
                .to
                .parent()
                .ok_or_else(|| "Rename path has no parent".to_string())
                .and_then(crate::directory::read)];
            if result.from.parent() != result.to.parent() {
                listings.push(
                    result
                        .from
                        .parent()
                        .ok_or_else(|| "Rename source has no parent".to_string())
                        .and_then(crate::directory::read),
                );
            }
            Ok(Completion::Renamed { result, listings })
        }
        Operation::Dictionary(path) => crate::files::read_dictionary(&path)
            .map(Completion::Dictionary)
            .map_err(|detail| {
                format!("Dictionary load refused; dictionary selection unchanged: {detail}")
            }),
        // The outer worker owns Reload's borrow across jobs; never adopt here.
        Operation::Reload(_) => Err("Reload requires the worker's prepared handoff".into()),
        Operation::Open(path) => {
            if let Some(snapshot) = crate::directory::read(&path)? {
                return Ok(Completion::Directory(snapshot));
            }
            let file = files.open(&path).map_err(file_error)?;
            known.insert(file);
            Ok(Completion::Open(Loaded {
                file,
                path: files.path(file).map_err(|e| e.to_string())?.to_owned(),
                bytes: if job.keep.contains(&file) {
                    Vec::new()
                } else {
                    files.bytes(file).map_err(|e| e.to_string())?.to_vec()
                },
                missing: files.missing(file).map_err(|e| e.to_string())?,
            }))
        }
        Operation::Save { file, path, bytes } => {
            let file = match (file, path) {
                (Some(file), Some(path)) => {
                    files.save_as(file, &path, bytes).map_err(file_error)?;
                    file
                }
                (Some(file), None) => {
                    if let Err(detail) = files.save(file, bytes) {
                        let conflict = !detail.published
                            && !detail.publication_attempted
                            && detail.residual.is_none()
                            && matches!(
                                detail.kind,
                                crate::files::Kind::Conflict | crate::files::Kind::Exists
                            );
                        let detail = file_error(detail);
                        return if conflict {
                            Ok(Completion::Conflict(detail))
                        } else {
                            Err(detail)
                        };
                    }
                    file
                }
                (None, Some(path)) => {
                    // A refused Save As should not read an existing destination.
                    // Reservation still rechecks races through the file adapter.
                    match std::fs::symlink_metadata(&path) {
                        Ok(_) => return Err(
                            "Save As requires a new path; destination exists and was not modified"
                                .into(),
                        ),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(format!(
                                "Cannot inspect Save As destination; nothing written: {e}"
                            ));
                        }
                    }
                    let file = files.open(&path).map_err(|e| {
                        format!(
                            "Cannot reserve Save As destination; nothing written: {}",
                            file_error(e)
                        )
                    })?;
                    let reserved = known.contains(&file);
                    known.insert(file);
                    if reserved || !files.missing(file).map_err(|e| e.to_string())? {
                        return Err("Save As requires a new, unassociated path; destination was not modified".into());
                    }
                    files.save(file, bytes).map_err(file_error)?;
                    file
                }
                (None, None) => return Err("Save As needs a path".into()),
            };
            Ok(Completion::Saved {
                file,
                path: files.path(file).map_err(|e| e.to_string())?.to_owned(),
            })
        }
    }
}

fn file_error(error: crate::files::Failure) -> String {
    // Put consequences before possibly long path diagnostics so the bounded
    // window notice cannot truncate the publication/cleanup warning away.
    format!(
        "{}{}{}{}",
        if error.published || error.publication_attempted {
            "Destination may contain the snapshot; save is UNCONFIRMED. Verify disk contents. "
        } else {
            ""
        },
        if error.residual.is_some() {
            "Temporary cleanup is unconfirmed. "
        } else {
            ""
        },
        if error.kind == crate::files::Kind::Conflict {
            "Disk conflict; retry Save or use Save As to a new path. "
        } else {
            ""
        },
        error
    )
}

#[cfg(test)]
impl Session {
    pub(crate) fn disconnected_for_test() -> Self {
        let (sender, _jobs) = mpsc::sync_channel(1);
        let (_results, receiver) = mpsc::sync_channel(1);
        Self {
            #[cfg(feature = "test-file-barrier")]
            queue_gate: None,
            sender,
            receiver,
            pending: None,
            associations: BTreeMap::new(),
            directories: BTreeMap::new(),
            failed: false,
            conflict: None,
            dictionary: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::model::Command;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-editor-session-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn finish_worker(session: &mut Session, ui: &mut Controller) -> Result<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = session.poll(ui) {
                return result.map_err(|error| error.detail);
            }
            assert!(std::time::Instant::now() < deadline, "file worker timeout");
            std::thread::yield_now();
        }
    }

    #[test]
    fn dictionary_worker_is_bounded_and_never_creates_document_associations() {
        let directory = Directory::new();
        let path = directory.path("words");
        fs::write(&path, b"known\nknown\r\nother").unwrap();
        let mut session = Session::start().unwrap();
        let mut ui = Controller::default();
        ui.dispatch(Event::New).unwrap();
        let before = format!("{:?}", ui.editor());
        for invalid in [
            PathBuf::new(),
            PathBuf::from("a\0b"),
            PathBuf::from("x".repeat(4097)),
        ] {
            assert!(session.dictionary(invalid).is_err());
            assert!(!session.busy());
        }
        session.dictionary(path.clone()).unwrap();
        assert!(session.busy());
        assert!(session.open(directory.path("text")).is_err());
        assert!(session.dictionary(path.clone()).is_err());
        assert!(finish_worker(&mut session, &mut ui)
            .unwrap()
            .contains("2 entries"));
        assert!(!session.busy());
        assert!(session.dictionary(path.clone()).is_err());
        assert_eq!(session.take_dictionary().unwrap().entry_count(), 2);
        assert!(session.take_dictionary().is_none());
        assert_eq!(format!("{:?}", ui.editor()), before);
        assert!(session.associations.is_empty());
        fs::write(&path, b"not a word").unwrap();
        session.dictionary(path).unwrap();
        assert!(finish_worker(&mut session, &mut ui)
            .unwrap_err()
            .contains("dictionary selection unchanged"));
        assert!(session.take_dictionary().is_none());
        assert_eq!(format!("{:?}", ui.editor()), before);
        assert!(session.associations.is_empty());
    }

    #[test]
    fn reload_handoff_adopts_only_after_model_admission_and_cancellation_retains_baseline() {
        for rejection in [None, Some("cancel"), Some("stale")] {
            let directory = Directory::new();
            let path = directory.path("text");
            fs::write(&path, b"old").unwrap();
            let mut session = Session::start().unwrap();
            let mut ui = Controller::default();
            session.initial_open(&mut ui, path.clone()).unwrap();
            let tab = ui.editor().active().unwrap();
            ui.dispatch(Event::Edit {
                tab,
                revision: 0,
                command: Command::Insert("edit".into()),
            })
            .unwrap();
            fs::write(&path, b"disk").unwrap();
            session.save(&ui, tab, 1, None).unwrap();
            assert!(finish_worker(&mut session, &mut ui).is_err());
            let target = session.take_conflict().unwrap();
            assert_eq!(target, crate::dialog::Target { tab, revision: 1 });
            let mut conflict = crate::dialog::Conflict::new(ui.editor(), target).unwrap();
            assert!(conflict.answer(ui.editor(), false).unwrap().is_none());
            let permit = conflict.answer(ui.editor(), true).unwrap().unwrap();
            let original_file = session.associations.get(&tab).unwrap().file;
            session.reload(&ui, permit).unwrap();
            assert!(session.busy());
            assert!(session.open(path.clone()).is_err());
            if rejection == Some("cancel") {
                session.cancel_reload();
            }
            if rejection == Some("stale") {
                ui.dispatch(Event::Edit {
                    tab,
                    revision: 1,
                    command: Command::Insert("new".into()),
                })
                .unwrap();
            }
            let result = finish_worker(&mut session, &mut ui);
            assert_eq!(result.is_err(), rejection == Some("stale"));
            let file = session.associations.get(&tab).unwrap().file;
            if rejection.is_some() {
                assert_eq!(file, original_file);
                assert!(ui.editor().document(tab).unwrap().text().contains("edit"));
                assert!(ui.editor().document(tab).unwrap().dirty());
                let revision = ui.editor().document(tab).unwrap().revision();
                session.save(&ui, tab, revision, None).unwrap();
                assert!(finish_worker(&mut session, &mut ui).is_err());
                assert!(session.take_conflict().is_some());
                assert_eq!(fs::read(&path).unwrap(), b"disk");
            } else {
                assert_ne!(file, original_file);
                assert_eq!(ui.editor().document(tab).unwrap().text(), "disk");
                assert!(!ui.editor().document(tab).unwrap().dirty());
                ui.dispatch(Event::Edit {
                    tab,
                    revision: 2,
                    command: Command::Insert("new".into()),
                })
                .unwrap();
                session.save(&ui, tab, 3, None).unwrap();
                finish_worker(&mut session, &mut ui).unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"newdisk");
            }
        }
    }

    #[test]
    fn reload_failure_does_not_adopt_invalid_bytes_or_clear_dirty_text() {
        let directory = Directory::new();
        let path = directory.path("text");
        fs::write(&path, b"old").unwrap();
        let mut session = Session::start().unwrap();
        let mut ui = Controller::default();
        session.initial_open(&mut ui, path.clone()).unwrap();
        let tab = ui.editor().active().unwrap();
        let mut conflict =
            crate::dialog::Conflict::new(ui.editor(), crate::dialog::Target { tab, revision: 0 })
                .unwrap();
        let permit = conflict.answer(ui.editor(), false).unwrap().unwrap();
        fs::write(&path, b"\xff").unwrap();
        session.reload(&ui, permit).unwrap();
        assert!(finish_worker(&mut session, &mut ui).is_err());
        assert_eq!(ui.editor().document(tab).unwrap().text(), "old");
        session.save(&ui, tab, 0, None).unwrap();
        assert!(finish_worker(&mut session, &mut ui).is_err());
        assert!(session.take_conflict().is_some());
        assert_eq!(fs::read(path).unwrap(), b"\xff");
    }

    // Manually advance ordinary jobs; Reload tests hold the prepared borrow
    // across explicitly ordered completion delivery and admission instead.
    #[test]
    fn directory_browsing_reuses_identity_or_adds_a_tab_and_preserves_file_dedup() {
        let dir = Directory::new();
        fs::create_dir(dir.path("child")).unwrap();
        fs::write(dir.path("child/file"), "original").unwrap();
        let mut h = Harness::new();
        let tab = h.open(dir.0.clone());
        assert!(h.ui.editor().document(tab).unwrap().directory());
        assert!(h.ui.editor().document(tab).unwrap().text().ends_with(" child/"));
        assert!(!h.ui.tab_view(tab).unwrap().soft_wrap);
        assert_eq!(h.session.path(tab), Some(dir.0.as_path()));
        h.session
            .browse(&h.ui, tab, 0, dir.path("child"), false)
            .unwrap();
        h.complete().unwrap();
        assert_eq!(h.ui.editor().active(), Some(tab));
        assert_eq!(h.ui.editor().tabs().count(), 1);
        assert_eq!(h.ui.editor().document(tab).unwrap().revision(), 1);
        h.session
            .browse(&h.ui, tab, 1, dir.path("child/file"), false)
            .unwrap();
        h.complete().unwrap();
        let file = h.ui.editor().active().unwrap();
        assert_ne!(file, tab);
        h.ui.dispatch(Event::Edit {
            tab: file,
            revision: 0,
            command: Command::Insert("dirty ".into()),
        })
        .unwrap();
        h.ui.dispatch(Event::SelectTab(tab)).unwrap();
        h.session
            .browse(&h.ui, tab, 1, dir.path("child/file"), false)
            .unwrap();
        h.complete().unwrap();
        assert_eq!(h.ui.editor().active(), Some(file));
        assert_eq!(h.ui.editor().tabs().count(), 2);
        assert!(h.session.directory(tab).is_some());
        assert!(h.ui.tab_view(tab).is_ok());
        assert_eq!(h.ui.editor().document(tab).unwrap().revision(), 1);
        assert!(h.ui.editor().document(file).unwrap().dirty());
        assert_eq!(
            h.ui.editor().document(file).unwrap().text(),
            "dirty original"
        );
        h.session.save(&h.ui, file, 1, None).unwrap();
        h.complete().unwrap();
        assert_eq!(fs::read(dir.path("child/file")).unwrap(), b"dirty original");
    }

    #[test]
    fn directory_mutations_are_refused_and_navigation_failure_is_atomic() {
        let dir = Directory::new();
        fs::write(dir.path("bad"), b"\xff").unwrap();
        let mut h = Harness::new();
        let tab = h.open(dir.0.clone());
        let commands = [
            Command::Insert("x".into()),
            Command::Type('x'),
            Command::Backspace,
            Command::Delete,
            Command::Undo,
            Command::Redo,
            Command::FillParagraph,
            Command::AutoFill(true),
            Command::FillColumn(80),
            Command::ReplaceAll {
                needle: "bad".into(),
                replacement: "good".into(),
            },
        ];
        let generation = h.ui.generation();
        for command in commands {
            assert_eq!(
                h.ui.dispatch(Event::Edit {
                    tab,
                    revision: 0,
                    command
                }),
                Err(crate::Error::Unavailable)
            );
        }
        assert_eq!(h.ui.generation(), generation);
        assert!(h.ui.editor().save_snapshot(tab).is_err());
        assert!(h
            .session
            .save(&h.ui, tab, 0, Some(dir.path("output")))
            .is_err());
        assert!(h
            .session
            .queue_save(&h.ui, tab, 0, Some(dir.path("output")))
            .is_err());
        h.session
            .browse(&h.ui, tab, 0, dir.path("bad"), false)
            .unwrap();
        assert!(h.complete().is_err());
        assert_eq!(h.ui.generation(), generation);
        assert!(h.ui.editor().document(tab).unwrap().text().ends_with(" bad"));
        assert_eq!(h.session.path(tab), Some(dir.0.as_path()));
        assert!(!dir.path("output").exists());
        h.session
            .browse(&h.ui, tab, 0, dir.0.clone(), false)
            .unwrap();
        h.ui.dispatch(Event::Close { tab, revision: 0 }).unwrap();
        assert!(h.complete().is_err());
        assert_eq!(h.ui.editor().tabs().count(), 0);
    }

    #[test]
    fn directory_replacement_preserves_tab_limit_and_cannot_replace_editable_text() {
        let dir = Directory::new();
        fs::write(dir.path("file"), "body").unwrap();
        let mut h = Harness::new();
        let tab = h.open(dir.0.clone());
        for _ in 1..64 {
            h.ui.dispatch(Event::New).unwrap();
        }
        h.session
            .browse(&h.ui, tab, 0, dir.path("file"), true)
            .unwrap();
        assert!(h.complete().is_err());
        h.session
            .browse(&h.ui, tab, 0, dir.path("file"), false)
            .unwrap();
        assert!(h.complete().is_err());
        assert_eq!(h.ui.editor().tabs().count(), 64);
        assert!(h.ui.editor().document(tab).unwrap().directory());
        assert_eq!(h.ui.editor().document(tab).unwrap().revision(), 0);
        h.session.browse(&h.ui, tab, 0, dir.0.clone(), false).unwrap();
        h.complete().unwrap();
        assert_eq!(h.ui.editor().document(tab).unwrap().revision(), 1);
        let editable = h.ui.editor().tabs().map(|(id, _)| id).find(|&id| id != tab).unwrap();
        let source = Some(h.ui.editor().revision_point(editable, 0).unwrap());
        assert_eq!(
            h.ui.dispatch(Event::Open(crate::model::Open {
                source,
                bytes: b"replacement",
                directory: true,
                missing: false,
                existing: None,
            })),
            Err(crate::Error::InvalidArgument)
        );
        assert_eq!(h.ui.editor().document(editable).unwrap().text(), "");
    }

    #[test]
    fn directory_file_completion_refuses_closed_source_and_deduplicates_at_limit() {
        let dir = Directory::new();
        fs::write(dir.path("file"), "body").unwrap();
        for existing in [false, true] {
            let mut h = Harness::new();
            let source = h.open(dir.0.clone());
            if existing {
                let file = h.open(dir.path("file"));
                h.ui.dispatch(Event::Edit { tab: file, revision: 0,
                    command: Command::Insert("dirty".into()) }).unwrap();
            }
            h.session.browse(&h.ui, source, 0, dir.path("file"), false).unwrap();
            h.ui.dispatch(Event::Close { tab: source, revision: 0 }).unwrap();
            let before = format!("{:?}", h.ui.editor());
            assert!(h.complete().is_err());
            assert_eq!(format!("{:?}", h.ui.editor()), before);
        }
        let mut h = Harness::new();
        let source = h.open(dir.0.clone());
        let file = h.open(dir.path("file"));
        h.ui.dispatch(Event::Edit { tab: file, revision: 0,
            command: Command::Insert("dirty".into()) }).unwrap();
        let before = format!("{:?}", h.ui.editor().document(file).unwrap());
        for _ in 2..64 { h.ui.dispatch(Event::New).unwrap(); }
        h.session.browse(&h.ui, source, 0, dir.path("file"), false).unwrap();
        h.complete().unwrap();
        assert_eq!(h.ui.editor().active(), Some(file));
        assert_eq!(h.ui.editor().tabs().count(), 64);
        assert!(h.ui.editor().document(source).unwrap().directory());
        assert_eq!(format!("{:?}", h.ui.editor().document(file).unwrap()), before);
    }

    #[test]
    fn moving_selected_entry_keeps_adjacent_source_row_and_incidental_destination() {
        let dir = Directory::new();
        fs::create_dir(dir.path("source")).unwrap();
        fs::create_dir(dir.path("destination")).unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::write(dir.path("source").join(name), b"disk").unwrap();
        }
        fs::write(dir.path("destination/z"), b"keep").unwrap();
        let mut h = Harness::new();
        let source_tab = h.open(dir.path("source"));
        let destination_tab = h.open(dir.path("destination"));
        h.ui.dispatch(Event::SelectTab(source_tab)).unwrap();
        for (revision, row, name) in [(0, 1, "b"), (1, 2, "d")] {
            let snapshot = h.session.directory(source_tab).unwrap();
            let source = snapshot.rename_source(row).unwrap();
            let caret = h.ui.editor().document(source_tab).unwrap().text()
                .lines().take(row).map(|line| line.len() + 1).sum();
            h.ui.dispatch(Event::Edit { tab: source_tab, revision,
                command: Command::Select(crate::model::Selection { anchor: caret, caret }) }).unwrap();
            h.session.rename(&h.ui, source_tab, revision, source,
                dir.path("destination").join(name).into_os_string()).unwrap();
            h.complete().unwrap();
            assert_eq!(h.ui.editor().active(), Some(source_tab));
            for (tab, selected) in [(source_tab, "c"), (destination_tab, "z")] {
                let doc = h.ui.editor().document(tab).unwrap();
                let line = doc.text().get(doc.selection().caret..).unwrap().lines().next().unwrap();
                assert_eq!(line.split_whitespace().last(), Some(selected));
            }
        }
    }

    #[test]
    fn directory_rename_reassociates_dirty_tabs_without_touching_model_state() {
        let dir = Directory::new();
        fs::create_dir_all(dir.path("tree/sub")).unwrap();
        fs::write(dir.path("tree/sub/file"), b"disk").unwrap();
        let mut h = Harness::new();
        let file = h.open(dir.path("tree/sub/file"));
        h.ui.dispatch(Event::Edit {
            tab: file,
            revision: 0,
            command: Command::Insert("edit".into()),
        })
        .unwrap();
        let child = h.open(dir.path("tree/sub"));
        let parent = h.open(dir.0.clone());
        let duplicate = h.open(dir.0.clone());
        h.ui.dispatch(Event::SelectTab(parent)).unwrap();
        let source = h
            .session
            .directory(parent)
            .unwrap()
            .rename_source(0)
            .unwrap();
        let before = format!("{:?}", h.ui.editor().document(file).unwrap());
        h.session
            .rename(&h.ui, parent, 0, source, "forest".into())
            .unwrap();
        assert!(h.session.open(dir.0.clone()).is_err());
        h.complete().unwrap();
        assert_eq!(h.ui.editor().active(), Some(parent));
        assert!(h.ui.editor().document(duplicate).unwrap().text().ends_with("forest/"));
        assert_eq!(
            format!("{:?}", h.ui.editor().document(file).unwrap()),
            before
        );
        assert_eq!(
            h.session.path(file).unwrap().as_os_str().as_bytes(),
            dir.path("forest/sub/file").as_os_str().as_bytes()
        );
        assert_eq!(h.session.path(child).unwrap(), dir.path("forest/sub"));
        assert_eq!(h.ui.editor().document(child).unwrap().revision(), 0);
        assert!(h
            .ui
            .editor()
            .document(parent)
            .unwrap()
            .text()
            .ends_with("forest/"));
        // Rename that same dirty file, then edit again while the job is busy.
        let source = h
            .session
            .directory(child)
            .unwrap()
            .rename_source(0)
            .unwrap();
        h.session
            .rename(&h.ui, child, 0, source, "renamed".into())
            .unwrap();
        h.ui.dispatch(Event::Edit {
            tab: file,
            revision: 1,
            command: Command::Insert("later".into()),
        })
        .unwrap();
        let before = format!("{:?}", h.ui.editor().document(file).unwrap());
        h.ui.dispatch(Event::SelectTab(file)).unwrap();
        h.complete().unwrap();
        assert_eq!(h.ui.editor().active(), Some(file));
        assert_eq!(
            format!("{:?}", h.ui.editor().document(file).unwrap()),
            before
        );
        assert_eq!(
            h.session.path(file).unwrap().as_os_str().as_bytes(),
            dir.path("forest/sub/renamed").as_os_str().as_bytes()
        );
        assert_eq!(
            h.ui.editor().document(file).unwrap().history_depth(),
            (2, 0)
        );
        h.session.save(&h.ui, file, 2, None).unwrap();
        h.complete().unwrap();
        assert_eq!(
            fs::read(dir.path("forest/sub/renamed")).unwrap(),
            b"editlaterdisk"
        );
        assert!(!dir.path("tree").exists());
        assert!(!dir.path("forest/sub/file").exists());
    }

    #[test]
    fn directory_sort_preserves_selected_name_readonly_state_and_refresh_order() {
        use crate::directory::Sort;
        let dir = Directory::new();
        fs::create_dir(dir.path("child")).unwrap();
        fs::write(dir.path("a"), "a").unwrap();
        fs::write(dir.path("z"), "largest").unwrap();
        let mut h = Harness::new();
        let tab = h.open(dir.0.clone());
        let caret = h.ui.editor().document(tab).unwrap().text().lines().next().unwrap().len() + 1;
        h.ui.dispatch(Event::Edit { tab, revision: 0, command: Command::Select(
            crate::model::Selection { anchor: caret, caret }) }).unwrap();
        h.session.sort_directory(&mut h.ui, tab, 0, Sort::Size, false).unwrap();
        let doc = h.ui.editor().document(tab).unwrap();
        assert_eq!(doc.revision(), 1);
        assert!(doc.directory() && !doc.dirty());
        assert_eq!(doc.history_depth(), (0, 0));
        assert_eq!(doc.text().get(doc.selection().caret..).unwrap().split_whitespace().last(), Some("a"));
        assert_eq!(h.session.directory(tab).unwrap().entry(1), Some(dir.path("z")));
        let before = format!("{:?}", h.ui.editor());
        assert!(h.session.sort_directory(&mut h.ui, tab, 0, Sort::Name, false).is_err());
        assert_eq!(format!("{:?}", h.ui.editor()), before);
        let generation = h.ui.generation();
        h.ui.generation_for_test(u64::MAX - 1);
        assert!(h.session.sort_directory(&mut h.ui, tab, 1, Sort::Name, false).is_err());
        assert_eq!(format!("{:?}", h.ui.editor()), before);
        assert_eq!(h.session.directory(tab).unwrap().sort, Sort::Size);
        h.ui.generation_for_test(generation);
        h.session.browse(&h.ui, tab, 1, dir.0.clone(), false).unwrap();
        assert!(h.session.sort_directory(&mut h.ui, tab, 1, Sort::Name, false).is_err());
        h.complete().unwrap();
        assert_eq!(h.session.directory(tab).unwrap().sort, Sort::Size);
        assert_eq!(h.session.directory(tab).unwrap().entry(1), Some(dir.path("z")));
        h.session.sort_directory(&mut h.ui, tab, 2, Sort::Size, true).unwrap();
        assert_eq!(h.session.directory(tab).unwrap().entry(0), Some(dir.path("child")));
        assert_eq!(h.session.directory(tab).unwrap().entry(1), Some(dir.path("a")));
        assert_eq!(fs::read(dir.path("z")).unwrap(), b"largest");
    }

    #[test]
    fn directory_raw_names_bounds_empty_and_symlink_policy() {
        use std::os::unix::ffi::OsStringExt;
        let dir = Directory::new();
        let raw = std::ffi::OsString::from_vec(b"bad\xff\n\\name".to_vec());
        fs::write(dir.0.join(&raw), "body").unwrap();
        fs::create_dir(dir.path("z")).unwrap();
        std::os::unix::fs::symlink(dir.path("z"), dir.path("link")).unwrap();
        let snapshot = crate::directory::read(&dir.0).unwrap().unwrap();
        assert_eq!(snapshot.text.lines().map(|line| line.split_whitespace().last().unwrap()).collect::<Vec<_>>(),
            ["z/", "bad\\xff\\n\\\\name", "link"]);
        assert_eq!(snapshot.text.lines().map(|line| line.chars().nth(2).unwrap()).collect::<Vec<_>>(), ['d', '-', 'l']);
        assert_eq!(snapshot.entry(1), Some(dir.0.join(raw)));
        assert_eq!(snapshot.entry(3), None);
        assert!(crate::directory::read(&dir.path("link/"))
            .unwrap()
            .is_none());
        assert_eq!(
            crate::directory::read(&dir.path("z"))
                .unwrap()
                .unwrap()
                .len(),
            0
        );
        for i in 0..4094 {
            fs::write(dir.path(&format!("entry-{i}")), "").unwrap();
        }
        assert!(crate::directory::read(&dir.0)
            .err()
            .unwrap()
            .contains("4096"));
    }

    struct Harness {
        session: Session,
        jobs: Receiver<Job>,
        results: SyncSender<Result<Completion>>,
        files: Files,
        known: BTreeSet<FileId>,
        ui: Controller,
    }
    impl Harness {
        fn new() -> Self {
            let (sender, jobs) = mpsc::sync_channel(1);
            let (results, receiver) = mpsc::sync_channel(1);
            Self {
                session: Session {
                    #[cfg(feature = "test-file-barrier")]
                    queue_gate: None,
                    sender,
                    receiver,
                    pending: None,
                    associations: BTreeMap::new(),
                    directories: BTreeMap::new(),
                    failed: false,
                    conflict: None,
                    dictionary: None,
                },
                jobs,
                results,
                files: Files::default(),
                known: BTreeSet::new(),
                ui: Controller::default(),
            }
        }
        fn complete(&mut self) -> Result<String> {
            let job = self.jobs.try_recv().unwrap();
            let result = execute(&mut self.files, &mut self.known, job);
            self.results.send(result).ok().unwrap();
            self.session
                .poll(&mut self.ui)
                .unwrap()
                .map_err(|error| error.detail)
        }
        fn open(&mut self, path: PathBuf) -> TabId {
            self.session.open(path).unwrap();
            self.complete().unwrap();
            self.ui.editor().active().unwrap()
        }
        fn edit(&mut self, tab: TabId, command: Command) {
            let revision = self.ui.editor().document(tab).unwrap().revision();
            self.ui
                .dispatch(Event::Edit {
                    tab,
                    revision,
                    command,
                })
                .unwrap();
        }
        fn save(&mut self, tab: TabId, path: Option<PathBuf>) {
            let revision = self.ui.editor().document(tab).unwrap().revision();
            self.session.save(&self.ui, tab, revision, path).unwrap();
        }
    }

    #[test]
    fn queued_save_checks_revision_before_handoff_and_owns_bytes_after_it() {
        for save_as in [false, true] {
            let directory = Directory::new();
            let path = directory.path("text");
            let destination = directory.path("new-path");
            fs::write(&path, b"disk").unwrap();
            let mut h = Harness::new();
            let tab = h.open(path.clone());
            h.edit(tab, Command::Insert("a".into()));
            let target = save_as.then(|| destination.clone());
            h.session.queue_save(&h.ui, tab, 1, target.clone()).unwrap();
            assert!(h.session.busy());
            assert!(matches!(h.jobs.try_recv(), Err(TryRecvError::Empty)));
            assert!(h.session.save(&h.ui, tab, 1, target.clone()).is_err());
            assert!(h.session.open(path.clone()).is_err());
            h.edit(tab, Command::Insert("b".into()));
            h.edit(tab, Command::Undo);
            assert_eq!(h.ui.editor().document(tab).unwrap().text(), "adisk");
            let error = h.session.poll(&mut h.ui).unwrap().unwrap_err();
            assert_eq!(error.code, crate::Error::StaleRevision);
            assert!(!h.session.busy());
            assert!(matches!(h.jobs.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(fs::read(&path).unwrap(), b"disk");
            assert!(!destination.exists());
            h.session.queue_save(&h.ui, tab, 3, target).unwrap();
            assert!(h.session.poll(&mut h.ui).is_none());
            assert!(h.session.busy());
            h.edit(tab, Command::Insert("c".into()));
            h.complete().unwrap();
            assert_eq!(
                fs::read(if save_as { &destination } else { &path }).unwrap(),
                b"adisk"
            );
            assert_eq!(h.ui.editor().document(tab).unwrap().text(), "acdisk");
            assert!(h.ui.editor().document(tab).unwrap().dirty());
        }
    }

    #[cfg(feature = "test-file-barrier")]
    #[test]
    fn queued_gate_error_clears_the_slot_without_submitting_file_work() {
        for disconnect in [false, true] {
            let directory = Directory::new();
            let path = directory.path("draft");
            fs::write(&path, b"disk").unwrap();
            let mut h = Harness::new();
            let tab = h.open(path.clone());
            h.edit(tab, Command::Insert("a".into()));
            let (requests, incoming) = mpsc::sync_channel(1);
            let (outgoing, replies) = mpsc::sync_channel(1);
            h.session.queue_gate = Some(crate::test_file_barrier::QueueGate::from_channels(
                requests, replies,
            ));
            h.session.queue_save(&h.ui, tab, 1, None).unwrap();
            incoming.try_recv().unwrap();
            assert!(h.session.poll(&mut h.ui).is_none());
            assert!(h.session.busy());
            assert!(matches!(h.jobs.try_recv(), Err(TryRecvError::Empty)));
            if !disconnect {
                outgoing.send(Err("refused checkpoint".into())).unwrap();
            }
            drop(outgoing);
            let error = h.session.poll(&mut h.ui).unwrap().unwrap_err();
            assert_eq!(error.code, crate::Error::Unavailable);
            assert!(!h.session.busy());
            assert!(matches!(h.jobs.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(h.ui.editor().document(tab).unwrap().text(), "adisk");
            assert!(h.ui.editor().document(tab).unwrap().dirty());
            assert_eq!(fs::read(&path).unwrap(), b"disk");
            // The ordinary, nonqueued path still uses the unaffected file worker.
            h.session.save(&h.ui, tab, 1, None).unwrap();
            h.complete().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"adisk");
        }
    }

    #[test]
    fn queued_save_model_point_rejects_other_editor_and_missing_tab_before_io() {
        for missing in [false, true] {
            let directory = Directory::new();
            let path = directory.path("text");
            fs::write(&path, b"disk").unwrap();
            let mut h = Harness::new();
            let tab = h.open(path.clone());
            h.session.queue_save(&h.ui, tab, 0, None).unwrap();
            let code = if missing {
                h.ui.dispatch(Event::Close { tab, revision: 0 }).unwrap();
                h.session.poll(&mut h.ui).unwrap().unwrap_err().code
            } else {
                let mut other = Controller::default();
                other.dispatch(Event::Load(b"disk")).unwrap();
                h.session.poll(&mut other).unwrap().unwrap_err().code
            };
            assert_eq!(
                code,
                if missing {
                    crate::Error::MissingTab
                } else {
                    crate::Error::InvalidArgument
                }
            );
            assert!(matches!(h.jobs.try_recv(), Err(TryRecvError::Empty)));
            assert_eq!(fs::read(path).unwrap(), b"disk");
        }
    }

    #[test]
    fn reload_completion_cancel_is_deterministic_before_and_after_delivery() {
        for timing in [
            "before-read",
            "before-send",
            "after-send",
            "accept",
            "stale",
        ] {
            let directory = Directory::new();
            let path = directory.path("text");
            fs::write(&path, b"old").unwrap();
            let mut h = Harness::new();
            let tab = h.open(path.clone());
            h.edit(tab, Command::Insert("edit".into()));
            let original = h.session.associations.get(&tab).unwrap().file;
            let mut conflict = crate::dialog::Conflict::new(
                h.ui.editor(),
                crate::dialog::Target { tab, revision: 1 },
            )
            .unwrap();
            assert!(conflict.answer(h.ui.editor(), false).unwrap().is_none());
            let permit = conflict.answer(h.ui.editor(), true).unwrap().unwrap();
            h.session.reload(&h.ui, permit).unwrap();
            let job = h.jobs.try_recv().unwrap();
            assert!(matches!(job.operation, Operation::Reload(id) if id == original));
            assert!(h.session.poll(&mut h.ui).is_none());
            if timing == "before-read" {
                h.session.cancel_reload();
            }
            fs::write(&path, b"disk").unwrap();
            let candidate = h.files.prepare_reload(original).unwrap();
            let fresh = candidate.file_id();
            if timing == "before-send" {
                h.session.cancel_reload();
            }
            h.results
                .send(Ok(Completion::Reload(Loaded {
                    file: fresh,
                    path: candidate.path().to_owned(),
                    bytes: candidate.bytes().to_vec(),
                    missing: candidate.missing(),
                })))
                .ok()
                .unwrap();
            if timing == "after-send" {
                h.session.cancel_reload();
            }
            if timing == "stale" {
                h.ui.dispatch(Event::Edit {
                    tab,
                    revision: 1,
                    command: Command::Insert("new".into()),
                })
                .unwrap();
            }
            let result = h.session.poll(&mut h.ui).unwrap();
            assert_eq!(result.is_err(), timing == "stale");
            let accepted = timing == "accept";
            assert_eq!(
                h.session.associations.get(&tab).unwrap().file == fresh,
                accepted
            );
            assert_eq!(
                h.ui.editor().document(tab).unwrap().text() == "disk",
                accepted
            );
            let revision = h.ui.editor().document(tab).unwrap().revision();
            h.session.save(&h.ui, tab, revision, None).unwrap();
            let next = h.jobs.try_recv().unwrap();
            assert_eq!(next.keep.contains(&fresh), accepted);
            assert_eq!(next.keep.contains(&original), !accepted);
            if accepted {
                candidate.commit();
                h.known.remove(&original);
                h.known.insert(fresh);
            } else {
                drop(candidate);
            }
            let result = execute(&mut h.files, &mut h.known, next).unwrap();
            assert_eq!(matches!(result, Completion::Saved { .. }), accepted);
            assert_eq!(fs::read(&path).unwrap(), b"disk");
        }
    }

    #[test]
    fn reload_handles_newly_created_paths_and_refuses_deleted_destinations() {
        for initially_missing in [false, true] {
            let directory = Directory::new();
            let path = directory.path("text");
            if !initially_missing {
                fs::write(&path, b"old").unwrap();
            }
            let mut session = Session::start().unwrap();
            let mut ui = Controller::default();
            session.initial_open(&mut ui, path.clone()).unwrap();
            let tab = ui.editor().active().unwrap();
            ui.dispatch(Event::Edit {
                tab,
                revision: 0,
                command: Command::Insert("edit".into()),
            })
            .unwrap();
            let original = session.associations.get(&tab).unwrap().file;
            let before = format!("{:?}", ui.editor());
            if initially_missing {
                fs::write(&path, b"external").unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
            session.save(&ui, tab, 1, None).unwrap();
            let error = finish_worker(&mut session, &mut ui).unwrap_err();
            assert!(error.contains(if initially_missing {
                "Exists"
            } else {
                "Conflict"
            }));
            let mut conflict =
                crate::dialog::Conflict::new(ui.editor(), session.take_conflict().unwrap())
                    .unwrap();
            assert!(conflict.answer(ui.editor(), false).unwrap().is_none());
            session
                .reload(&ui, conflict.answer(ui.editor(), true).unwrap().unwrap())
                .unwrap();
            let result = finish_worker(&mut session, &mut ui);
            if initially_missing {
                result.unwrap();
                assert_eq!(ui.editor().document(tab).unwrap().text(), "external");
                assert!(!ui.editor().document(tab).unwrap().dirty());
                session.save(&ui, tab, 2, None).unwrap();
                finish_worker(&mut session, &mut ui).unwrap();
            } else {
                assert!(result.unwrap_err().contains("destination is missing"));
                assert_eq!(format!("{:?}", ui.editor()), before);
                assert_eq!(session.associations.get(&tab).unwrap().file, original);
                session.save(&ui, tab, 1, None).unwrap();
                assert!(finish_worker(&mut session, &mut ui).is_err());
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn delayed_save_acknowledges_only_snapshot_and_bounds_work() {
        let directory = Directory::new();
        let path = directory.path("draft");
        fs::write(&path, b"\xef\xbb\xbfold\r\n").unwrap();
        let mut h = Harness::new();
        let tab = h.open(path.clone());
        h.edit(tab, Command::Insert("first ".into()));
        h.save(tab, None);
        assert!(h.session.busy());
        assert!(h.session.poll(&mut h.ui).is_none());
        assert!(h.session.open(directory.path("other")).is_err());
        assert!(h.session.save(&h.ui, tab, 1, None).is_err());
        h.edit(tab, Command::Insert("second ".into()));
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfold\r\n");
        assert!(h.complete().unwrap().contains("newer edits"));
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbffirst old\r\n");
        assert!(h.ui.editor().document(tab).unwrap().dirty());
        h.edit(tab, Command::Undo);
        assert!(!h.ui.editor().document(tab).unwrap().dirty());
        assert!(!h.session.busy());
        assert!(h.session.save(&h.ui, tab, 0, None).is_err());
    }

    #[test]
    fn missing_tabs_are_dirty_and_duplicate_open_never_reloads() {
        let directory = Directory::new();
        let path = directory.path("new");
        let mut h = Harness::new();
        let tab = h.open(path.clone());
        assert!(!path.exists());
        assert!(h.ui.editor().document(tab).unwrap().dirty());
        h.edit(tab, Command::Insert("memory".into()));
        assert_eq!(h.open(path.clone()), tab);
        assert_eq!(h.ui.editor().document(tab).unwrap().text(), "memory");
        assert_eq!(h.ui.editor().tabs().count(), 1);
        h.save(tab, None);
        h.complete().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"memory");
        assert!(!h.ui.editor().document(tab).unwrap().dirty());
        fs::write(&path, b"external").unwrap();
        assert_eq!(h.open(path.clone()), tab);
        h.edit(tab, Command::Insert("later ".into()));
        h.save(tab, None);
        assert!(h.complete().unwrap_err().contains("Disk conflict"));
        assert_eq!(fs::read(path).unwrap(), b"external");
        assert!(h.ui.editor().document(tab).unwrap().dirty());
        h.save(tab, Some(directory.path("rescued")));
        h.complete().unwrap();
        assert_eq!(
            fs::read(directory.path("rescued")).unwrap(),
            b"memorylater "
        );
        assert!(!h.ui.editor().document(tab).unwrap().dirty());
    }

    #[test]
    fn untitled_save_as_refuses_existing_and_reserved_names() {
        let directory = Directory::new();
        let existing = directory.path("existing");
        fs::write(&existing, b"keep").unwrap();
        let mut h = Harness::new();
        let reserved = directory.path("reserved");
        let reserved_tab = h.open(reserved.clone());
        h.ui.dispatch(Event::New).unwrap();
        let tab = h.ui.editor().active().unwrap();
        h.edit(tab, Command::Insert("new".into()));
        assert!(h
            .session
            .save(&h.ui, tab, 1, Some(PathBuf::from("a".repeat(4097))))
            .is_err());
        assert!(!h.session.busy());
        assert!(matches!(h.jobs.try_recv(), Err(TryRecvError::Empty)));
        for path in [existing.clone(), reserved.clone()] {
            h.save(tab, Some(path));
            assert!(h.complete().is_err());
            assert!(!h.session.associated(tab));
            assert!(h.ui.editor().document(tab).unwrap().dirty());
        }
        assert_eq!(fs::read(existing).unwrap(), b"keep");
        assert!(!reserved.exists());
        assert!(h.session.associated(reserved_tab));
        let destination = directory.path("new");
        h.save(tab, Some(destination.clone()));
        h.complete().unwrap();
        assert!(h.session.associated(tab));
        assert!(!h.ui.editor().document(tab).unwrap().dirty());
        assert_eq!(fs::read(destination).unwrap(), b"new");
        assert_eq!(h.known.len(), 2);
    }

    #[test]
    fn rejected_open_and_closed_associations_are_released_before_next_job() {
        let directory = Directory::new();
        let mut h = Harness::new();
        for _ in 0..64 {
            h.ui.dispatch(Event::New).unwrap();
        }
        h.session.open(directory.path("rejected")).unwrap();
        assert!(h
            .complete()
            .unwrap_err()
            .contains("tab or text budget exhausted"));
        assert_eq!(h.known.len(), 1);
        let tab = h.ui.editor().active().unwrap();
        h.ui.dispatch(Event::Close { tab, revision: 0 }).unwrap();
        h.open(directory.path("admitted"));
        assert_eq!(h.known.len(), 1);
        assert_eq!(h.session.associations.len(), 1);
        let tab = h.ui.editor().active().unwrap();
        h.save(tab, None);
        h.complete().unwrap();
        h.ui.dispatch(Event::Close { tab, revision: 0 }).unwrap();
        h.session.forget(tab);
        h.open(directory.path("next"));
        assert_eq!(h.known.len(), 1);
    }

    #[test]
    fn real_worker_initial_open_and_disconnect_are_observable() {
        let directory = Directory::new();
        let mut session = Session::start().unwrap();
        let mut ui = Controller::default();
        session
            .initial_open(&mut ui, directory.path("missing"))
            .unwrap();
        assert!(ui
            .editor()
            .document(ui.editor().active().unwrap())
            .unwrap()
            .dirty());
        let mut h = Harness::new();
        h.session.open(directory.path("unused")).unwrap();
        drop(h.results);
        assert!(h
            .session
            .poll(&mut h.ui)
            .unwrap()
            .unwrap_err()
            .detail
            .contains("may have reached disk"));
        assert!(!h.session.busy());
        assert!(h.session.poll(&mut h.ui).is_none());
        assert!(h.session.open(directory.path("again")).is_err());
    }

    #[test]
    fn duplicate_open_transfers_no_text_and_keeps_existing_tab() {
        let directory = Directory::new();
        let path = directory.path("draft");
        fs::write(&path, b"baseline").unwrap();
        let mut h = Harness::new();
        let tab = h.open(path.clone());
        h.edit(tab, Command::Insert("edited ".into()));
        h.session.open(path).unwrap();
        let completion = execute(&mut h.files, &mut h.known, h.jobs.try_recv().unwrap()).unwrap();
        assert!(matches!(&completion, Completion::Open(loaded) if loaded.bytes.is_empty()));
        h.results.send(Ok(completion)).ok().unwrap();
        h.session.poll(&mut h.ui).unwrap().unwrap();
        assert_eq!(h.ui.editor().active(), Some(tab));
        assert_eq!(
            h.ui.editor().document(tab).unwrap().text(),
            "edited baseline"
        );
        assert!(h.ui.editor().document(tab).unwrap().dirty());
    }

    #[test]
    fn refused_save_as_does_not_read_or_admit_existing_destinations() {
        let directory = Directory::new();
        fs::write(directory.path("invalid"), b"\xff").unwrap();
        fs::File::create(directory.path("large"))
            .unwrap()
            .set_len(crate::text::MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        let mut h = Harness::new();
        h.ui.dispatch(Event::New).unwrap();
        let tab = h.ui.editor().active().unwrap();
        h.edit(tab, Command::Insert("my valid text".into()));
        for name in ["invalid", "large"] {
            h.save(tab, Some(directory.path(name)));
            assert!(h
                .complete()
                .unwrap_err()
                .contains("destination exists and was not modified"));
            assert!(h.known.is_empty());
            assert_eq!(h.files.baseline_bytes(), 0);
            assert!(!h.session.associated(tab));
            assert!(h.ui.editor().document(tab).unwrap().dirty());
        }
        assert_eq!(fs::read(directory.path("invalid")).unwrap(), b"\xff");
        assert_eq!(
            fs::metadata(directory.path("large")).unwrap().len(),
            crate::text::MAX_FILE_BYTES as u64 + 1
        );
    }
}
