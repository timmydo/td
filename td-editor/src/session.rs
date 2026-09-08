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
    Dictionary(PathBuf),
    Open(PathBuf),
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
    Dictionary(crate::spelling::Dictionary),
    Open(Loaded),
    Reload(Loaded),
    Conflict(String),
    Saved { file: FileId, path: PathBuf },
}
enum Pending {
    Dictionary,
    Open,
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
}

pub(crate) struct Session {
    sender: SyncSender<Job>,
    receiver: Receiver<Result<Completion>>,
    pending: Option<Pending>,
    associations: BTreeMap<TabId, Association>,
    failed: bool,
    conflict: Option<crate::dialog::Target>,
    dictionary: Option<crate::spelling::Dictionary>,
}

impl Session {
    pub(crate) fn start() -> Result<Self> {
        let (sender, jobs) = mpsc::sync_channel(1);
        let (results, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("td-editor-files".into())
            .spawn(move || worker(jobs, results))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            sender,
            receiver,
            pending: None,
            associations: BTreeMap::new(),
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
    }
    pub(crate) fn labels(&self) -> impl Iterator<Item = (TabId, &str)> {
        self.associations
            .iter()
            .map(|(id, a)| (*id, a.title.as_str()))
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
            (Some(Pending::Open), Completion::Open(loaded)) => {
                if let Some((&tab, _)) = self
                    .associations
                    .iter()
                    .find(|(_, a)| a.file == loaded.file)
                {
                    ui.dispatch(Event::SelectTab(tab))
                        .map_err(|e| e.to_string())?;
                    return Ok("Selected already-open file; edits and baseline retained".into());
                }
                let event = if loaded.missing {
                    Event::MissingFile
                } else {
                    Event::Load(&loaded.bytes)
                };
                let Outcome::Created(tab) = ui.dispatch(event).map_err(|e| match e {
                    crate::Error::Limit => {
                        "Open refused: tab or text budget exhausted; existing tabs unchanged"
                            .to_string()
                    }
                    _ => format!("Open was not admitted ({e}); existing tabs unchanged"),
                })?
                else {
                    return Err("file admission did not create a tab".into());
                };
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
        self.associations.insert(tab, Association { file, title });
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
        Operation::Dictionary(path) => crate::files::read_dictionary(&path)
            .map(Completion::Dictionary)
            .map_err(|detail| {
                format!("Dictionary load refused; dictionary selection unchanged: {detail}")
            }),
        // The outer worker owns Reload's borrow across jobs; never adopt here.
        Operation::Reload(_) => Err("Reload requires the worker's prepared handoff".into()),
        Operation::Open(path) => {
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
                            ))
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
            sender,
            receiver,
            pending: None,
            associations: BTreeMap::new(),
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
                    sender,
                    receiver,
                    pending: None,
                    associations: BTreeMap::new(),
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
