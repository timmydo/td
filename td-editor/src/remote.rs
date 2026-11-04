//! Editing another machine's files (DESIGN.md, "Editing another machine's
//! files"). The window, its text and its undo stay here. `td-editor
//! --file-host` runs there, started over ssh for this connection, and runs
//! the same file worker a local window does, so a save is judged and published
//! beside the file by the same code. Its jobs and completions are what crosses,
//! on a td-remote link.
//!
//! The host's file IDs and retained baselines end with the connection. So this
//! side holds the IDs the window sees, and each file's baseline, and after a
//! reconnect opens each file again and compares: the same bytes carry on, and
//! any other meet the conflict dialog a changed file always does.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender};

use td_remote::codec::{Reader, Writer};
use td_remote::{
    Client, Connector, Error as LinkError, Hello, Host, Link, Options, RemoteProgram, RequestError,
};
use td_ui::editor_text::MAX_FILE_BYTES;

use crate::files::{DeletePlan, DirectorySource, FileId, RenameSource};
use crate::path_completion::Matches;
use crate::session::{Completion, Job, Loaded, Operation};

type Result<T> = std::result::Result<T, String>;

const FILES: u32 = 1;
const COMPLETION: u32 = 2;
/// The argument a window starts the far side with.
pub const HOST_ARGUMENT: &str = "--file-host";
/// A file's bytes, with room for its path, the other tabs' it reserves (at most
/// 64 of 4 KiB) and the reply around them. A listing is smaller: 4096 entries and
/// 1 MiB of names.
const MAX_MESSAGE: usize = MAX_FILE_BYTES + (1 << 20);
pub const OPTIONS: Options = Options {
    hello: Hello {
        program: "td-editor-files",
        version: 1,
    },
    streams: &[FILES, COMPLETION],
    max_message: MAX_MESSAGE,
    unread: 1,
};
const PATH_BYTES: usize = 4096;
/// The most files one keep set names: the adapter's 64 and a candidate.
const KEEP: usize = 128;
/// The paths a job reserves: one for each open directory tab and each file tab
/// its host has no association for.
const RESERVED: usize = 1024;
/// A far side's sentence for a notice.
const TEXT_BYTES: usize = 64 * 1024;

/// A machine's files, reached through `ssh`.
pub struct Remote {
    label: String,
    client: Client,
}

impl Remote {
    pub fn ssh(host: Host, program: RemoteProgram) -> Self {
        let label = host.as_str().to_string();
        let connector: Connector =
            Box::new(move || Link::ssh(&host, &program, &[HOST_ARGUMENT], OPTIONS));
        Self::over(label, connector)
    }

    /// Any connector: a test's, for a host on a thread or a child process.
    pub fn over(label: String, connector: Connector) -> Self {
        Self {
            label,
            client: Client::new(OPTIONS.streams, connector),
        }
    }

    /// The machine's name, for the window to say.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn connect(&self) -> Result<()> {
        self.client
            .connected(FILES)
            .map(|_| ())
            .map_err(|error| format!("Cannot reach {}: {error}", self.label))
    }

    /// Path completion there, for a worker of its own.
    pub(crate) fn complete(&self, text: &str) -> Result<Matches> {
        let mut w = Writer::new();
        w.bytes(text.as_bytes());
        let reply =
            self.client
                .request(COMPLETION, &w.into_bytes())
                .map_err(|error| match error {
                    RequestError::Unsent(error) | RequestError::Lost(error) => {
                        format!("{}: {error}", self.label)
                    }
                    RequestError::Stale => format!("{}: the connection changed; retry", self.label),
                })?;
        let matches =
            decode_matches(&reply).map_err(|error| format!("{}: {error}", self.label))??;
        if !matches.complete(text) {
            return Err(format!(
                "{}: an answer that does not complete the path",
                self.label
            ));
        }
        Ok(matches)
    }
}

pub(crate) fn put_path(w: &mut Writer, path: &Path) {
    w.bytes(path.as_os_str().as_bytes());
}

/// A path from the other machine: its bytes are its own, but it fits the same
/// bounds a path here must.
pub(crate) fn get_path(r: &mut Reader<'_>) -> std::result::Result<PathBuf, LinkError> {
    let bytes = r.bytes(PATH_BYTES)?;
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(LinkError::Protocol("an empty or NUL path".into()));
    }
    Ok(PathBuf::from(OsStr::from_bytes(bytes)))
}

fn get_name(r: &mut Reader<'_>) -> std::result::Result<std::ffi::OsString, LinkError> {
    let bytes = r.bytes(PATH_BYTES)?;
    if bytes.contains(&0) {
        return Err(LinkError::Protocol("a NUL in a name".into()));
    }
    Ok(OsStr::from_bytes(bytes).to_os_string())
}

/// A far side's sentence, as a notice may show it.
fn get_text(r: &mut Reader<'_>) -> std::result::Result<String, LinkError> {
    Ok(clean(r.bytes(TEXT_BYTES)?))
}

fn clean(bytes: &[u8]) -> String {
    td_remote::printable(&String::from_utf8_lossy(bytes))
}

fn put_paths(w: &mut Writer, paths: &[PathBuf]) {
    w.count(paths.len());
    for path in paths {
        put_path(w, path);
    }
}

fn get_paths(r: &mut Reader<'_>) -> std::result::Result<Vec<PathBuf>, LinkError> {
    let count = r.count(RESERVED)?;
    (0..count).map(|_| get_path(r)).collect()
}

fn put_option_text(w: &mut Writer, text: Option<&str>) {
    match text {
        None => w.u8(0),
        Some(text) => {
            w.u8(1);
            w.bytes(text.as_bytes());
        }
    }
}

fn get_option_text(r: &mut Reader<'_>) -> std::result::Result<Option<String>, LinkError> {
    match r.u8()? {
        0 => Ok(None),
        1 => get_text(r).map(Some),
        tag => Err(LinkError::Protocol(format!("an optional text's tag {tag}"))),
    }
}

const JOB_COPY: u8 = 1;
const JOB_MKDIR: u8 = 2;
const JOB_DELETE: u8 = 3;
const JOB_OPEN: u8 = 4;
const JOB_RENAME: u8 = 5;
const JOB_RELOAD: u8 = 6;
const JOB_SAVE: u8 = 7;

/// A job as the host hears it. A dictionary is never one: its path names a
/// file on this machine, and no far side reads it.
fn encode_job(keep: &BTreeSet<FileId>, operation: &Operation) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    w.count(keep.len());
    for file in keep {
        w.u64(*file);
    }
    match operation {
        Operation::Copy {
            source,
            name,
            reserved,
        } => {
            w.u8(JOB_COPY);
            source.encode(&mut w);
            w.bytes(name.as_bytes());
            put_paths(&mut w, reserved);
        }
        Operation::Mkdir { source, name } => {
            w.u8(JOB_MKDIR);
            source.encode(&mut w);
            w.bytes(name.as_bytes());
        }
        Operation::Delete(plan) => {
            w.u8(JOB_DELETE);
            plan.encode(&mut w);
        }
        Operation::Open(path) => {
            w.u8(JOB_OPEN);
            put_path(&mut w, path);
        }
        Operation::Rename {
            source,
            name,
            reserved,
        } => {
            w.u8(JOB_RENAME);
            source.encode(&mut w);
            w.bytes(name.as_bytes());
            put_paths(&mut w, reserved);
        }
        Operation::Reload(file) => {
            w.u8(JOB_RELOAD);
            w.u64(*file);
        }
        Operation::Save {
            file,
            path,
            bytes,
            reserved,
        } => {
            w.u8(JOB_SAVE);
            put_paths(&mut w, reserved);
            match file {
                None => w.u8(0),
                Some(file) => {
                    w.u8(1);
                    w.u64(*file);
                }
            }
            match path {
                None => w.u8(0),
                Some(path) => {
                    w.u8(1);
                    put_path(&mut w, path);
                }
            }
            w.bytes(bytes);
        }
        Operation::Dictionary(_) => return Err("A dictionary is read on this machine".into()),
    }
    Ok(w.into_bytes())
}

fn decode_job(bytes: &[u8]) -> std::result::Result<Job, LinkError> {
    let mut r = Reader::new(bytes);
    let count = r.count(KEEP)?;
    let mut keep = BTreeSet::new();
    for _ in 0..count {
        keep.insert(r.u64()?);
    }
    let operation = match r.u8()? {
        JOB_COPY => Operation::Copy {
            source: RenameSource::decode(&mut r)?,
            name: get_name(&mut r)?,
            reserved: get_paths(&mut r)?,
        },
        JOB_MKDIR => Operation::Mkdir {
            source: DirectorySource::decode(&mut r)?,
            name: get_name(&mut r)?,
        },
        JOB_DELETE => Operation::Delete(DeletePlan::decode(&mut r)?),
        JOB_OPEN => Operation::Open(get_path(&mut r)?),
        JOB_RENAME => Operation::Rename {
            source: RenameSource::decode(&mut r)?,
            name: get_name(&mut r)?,
            reserved: get_paths(&mut r)?,
        },
        JOB_RELOAD => Operation::Reload(r.u64()?),
        JOB_SAVE => {
            let reserved = get_paths(&mut r)?;
            let file = match r.u8()? {
                0 => None,
                1 => Some(r.u64()?),
                tag => return Err(LinkError::Protocol(format!("a save's file tag {tag}"))),
            };
            let path = match r.u8()? {
                0 => None,
                1 => Some(get_path(&mut r)?),
                tag => return Err(LinkError::Protocol(format!("a save's path tag {tag}"))),
            };
            Operation::Save {
                file,
                path,
                bytes: r.bytes(MAX_FILE_BYTES)?.to_vec(),
                reserved,
            }
        }
        tag => return Err(LinkError::Protocol(format!("a job's tag {tag}"))),
    };
    r.finish()?;
    Ok(Job { keep, operation })
}

const DONE_FAILED: u8 = 0;
const DONE_CREATED: u8 = 1;
const DONE_DELETED: u8 = 2;
const DONE_OPEN: u8 = 3;
const DONE_DIRECTORY: u8 = 4;
const DONE_RENAMED: u8 = 5;
const DONE_RELOAD: u8 = 6;
const DONE_CONFLICT: u8 = 7;
const DONE_SAVED: u8 = 8;
const DONE_UNCONFIRMED: u8 = 9;

fn put_loaded(w: &mut Writer, loaded: &Loaded) {
    w.u64(loaded.file);
    put_path(w, &loaded.path);
    w.bytes(&loaded.bytes);
    w.u8(u8::from(loaded.missing));
}

fn get_loaded(r: &mut Reader<'_>) -> std::result::Result<Loaded, LinkError> {
    Ok(Loaded {
        file: r.u64()?,
        path: get_path(r)?,
        bytes: r.bytes(MAX_FILE_BYTES)?.to_vec(),
        missing: match r.u8()? {
            0 => false,
            1 => true,
            tag => return Err(LinkError::Protocol(format!("a load's missing tag {tag}"))),
        },
    })
}

fn put_listing(w: &mut Writer, listing: &Result<Option<crate::directory::Snapshot>>) {
    match listing {
        Err(detail) => {
            w.u8(0);
            w.bytes(detail.as_bytes());
        }
        Ok(None) => w.u8(1),
        Ok(Some(snapshot)) => {
            w.u8(2);
            snapshot.encode(w);
        }
    }
}

fn get_listing(
    r: &mut Reader<'_>,
) -> std::result::Result<Result<Option<crate::directory::Snapshot>>, LinkError> {
    match r.u8()? {
        0 => Ok(Err(get_text(r)?)),
        1 => Ok(Ok(None)),
        2 => Ok(Ok(Some(crate::directory::Snapshot::decode(r)?))),
        tag => Err(LinkError::Protocol(format!("a listing's tag {tag}"))),
    }
}

fn encode_completion(result: &Result<Completion>) -> Vec<u8> {
    let mut w = Writer::new();
    match result {
        Err(detail) => {
            w.u8(DONE_FAILED);
            w.bytes(detail.as_bytes());
        }
        Ok(Completion::CreatedPath {
            path,
            notice,
            listing,
        }) => {
            w.u8(DONE_CREATED);
            put_path(&mut w, path);
            w.bytes(notice.as_bytes());
            put_listing(&mut w, listing);
        }
        Ok(Completion::Deleted { result, listing }) => {
            w.u8(DONE_DELETED);
            w.u64(u64::try_from(result.removed).unwrap_or(u64::MAX));
            w.u64(u64::try_from(result.requested).unwrap_or(u64::MAX));
            put_path(&mut w, &result.parent);
            put_option_text(&mut w, result.failure.as_deref());
            put_listing(&mut w, listing);
        }
        Ok(Completion::Open(loaded)) => {
            w.u8(DONE_OPEN);
            put_loaded(&mut w, loaded);
        }
        Ok(Completion::Directory(snapshot)) => {
            w.u8(DONE_DIRECTORY);
            snapshot.encode(&mut w);
        }
        Ok(Completion::Renamed { result, listings }) => {
            w.u8(DONE_RENAMED);
            put_path(&mut w, &result.from);
            put_path(&mut w, &result.to);
            put_option_text(&mut w, result.warning.as_deref());
            w.count(listings.len());
            for listing in listings {
                put_listing(&mut w, listing);
            }
        }
        Ok(Completion::Reload(loaded)) => {
            w.u8(DONE_RELOAD);
            put_loaded(&mut w, loaded);
        }
        Ok(Completion::Conflict(detail)) => {
            w.u8(DONE_CONFLICT);
            w.bytes(detail.as_bytes());
        }
        Ok(Completion::Unconfirmed(detail)) => {
            w.u8(DONE_UNCONFIRMED);
            w.bytes(detail.as_bytes());
        }
        Ok(Completion::Saved { file, path }) => {
            w.u8(DONE_SAVED);
            w.u64(*file);
            put_path(&mut w, path);
        }
        // No job that crosses makes one; say so rather than send nothing.
        Ok(Completion::Dictionary(_)) => {
            w.u8(DONE_FAILED);
            w.bytes(b"A dictionary is read on the window's machine");
        }
    }
    w.into_bytes()
}

fn decode_completion(bytes: &[u8]) -> std::result::Result<Result<Completion>, LinkError> {
    let mut r = Reader::new(bytes);
    let done = match r.u8()? {
        DONE_FAILED => Err(get_text(&mut r)?),
        DONE_CREATED => Ok(Completion::CreatedPath {
            path: get_path(&mut r)?,
            notice: get_text(&mut r)?,
            listing: get_listing(&mut r)?,
        }),
        DONE_DELETED => Ok(Completion::Deleted {
            result: crate::files::Deleted {
                removed: usize::try_from(r.u64()?).unwrap_or(usize::MAX),
                requested: usize::try_from(r.u64()?).unwrap_or(usize::MAX),
                parent: get_path(&mut r)?,
                failure: get_option_text(&mut r)?,
            },
            listing: get_listing(&mut r)?,
        }),
        DONE_OPEN => Ok(Completion::Open(get_loaded(&mut r)?)),
        DONE_DIRECTORY => Ok(Completion::Directory(crate::directory::Snapshot::decode(
            &mut r,
        )?)),
        DONE_RENAMED => {
            let result = crate::files::Renamed {
                from: get_path(&mut r)?,
                to: get_path(&mut r)?,
                warning: get_option_text(&mut r)?,
            };
            let count = r.count(2)?;
            let mut listings = Vec::with_capacity(count);
            for _ in 0..count {
                listings.push(get_listing(&mut r)?);
            }
            Ok(Completion::Renamed { result, listings })
        }
        DONE_RELOAD => Ok(Completion::Reload(get_loaded(&mut r)?)),
        DONE_CONFLICT => Ok(Completion::Conflict(get_text(&mut r)?)),
        DONE_UNCONFIRMED => Ok(Completion::Unconfirmed(get_text(&mut r)?)),
        DONE_SAVED => Ok(Completion::Saved {
            file: r.u64()?,
            path: get_path(&mut r)?,
        }),
        tag => return Err(LinkError::Protocol(format!("a completion's tag {tag}"))),
    };
    r.finish()?;
    Ok(done)
}

fn decode_matches(bytes: &[u8]) -> std::result::Result<Result<Matches>, LinkError> {
    let mut r = Reader::new(bytes);
    let done = match r.u8()? {
        0 => Err(get_text(&mut r)?),
        1 => Ok(Matches::decode(&mut r)?),
        tag => return Err(LinkError::Protocol(format!("a completion's tag {tag}"))),
    };
    r.finish()?;
    Ok(done)
}

fn encode_matches(result: &Result<Matches>) -> Vec<u8> {
    let mut w = Writer::new();
    match result {
        Err(detail) => {
            w.u8(0);
            w.bytes(detail.as_bytes());
        }
        Ok(matches) => {
            w.u8(1);
            matches.encode(&mut w);
        }
    }
    w.into_bytes()
}

/// `td-editor --file-host`: this machine's files for a window on another,
/// over this process's standard streams. It runs until the window goes away,
/// and a job in hand when that happens runs to its end: a save publishes
/// whole or not at all, and nobody is left to tell.
pub fn file_host() -> io::Result<()> {
    serve(io::stdin(), io::stdout()).map_err(io::Error::other)
}

fn serve(
    reader: impl io::Read + Send + 'static,
    writer: impl io::Write + Send + 'static,
) -> std::result::Result<(), LinkError> {
    let mut link = Link::over(reader, writer, OPTIONS)?;
    let missing = || LinkError::Protocol("a stream is missing".into());
    let files = link.take_inbox(FILES).ok_or_else(missing)?;
    let completion = link.take_inbox(COMPLETION).ok_or_else(missing)?;
    let (jobs, incoming) = std::sync::mpsc::sync_channel(1);
    let (results, outgoing) = std::sync::mpsc::sync_channel(1);
    let thread = |name: &str| std::thread::Builder::new().name(name.into());
    let worker = thread("td-editor-files")
        .spawn(move || crate::session::worker(incoming, results))
        .map_err(LinkError::from)?;
    let sender = link.sender();
    let replier = thread("td-editor-replies")
        .spawn(move || {
            while let Ok(result) = outgoing.recv() {
                // A window gone hears nothing; the worker still ends its job.
                let _ = sender.send(FILES, &encode_completion(&result));
            }
        })
        .map_err(LinkError::from)?;
    let sender = link.sender();
    // Not joined: a slow directory may hold it, and it has nothing to finish.
    thread("td-editor-completion")
        .spawn(move || {
            while let Ok(message) = completion.recv() {
                let mut request = Reader::new(&message);
                let result = request
                    .text(PATH_BYTES)
                    .and_then(|text| request.finish().map(|()| text))
                    .map_err(|error| error.to_string())
                    .and_then(crate::path_completion::scan);
                if sender.send(COMPLETION, &encode_matches(&result)).is_err() {
                    return;
                }
            }
        })
        .map_err(LinkError::from)?;
    let sender = link.sender();
    while let Ok(message) = files.recv() {
        match decode_job(&message) {
            Ok(job) => {
                if jobs.send(job).is_err() {
                    break;
                }
            }
            // Unread, so unrun: the window hears why, and may ask again.
            Err(error) => {
                let refused: Result<Completion> = Err(format!("The host refused a job: {error}"));
                let _ = sender.send(FILES, &encode_completion(&refused));
            }
        }
    }
    drop(jobs);
    let _ = worker.join();
    let _ = replier.join();
    Ok(())
}

/// One file as the window knows it, on any connection.
struct Known {
    /// What the current connection's host calls it, so that the host's guards
    /// for an open file cover it. None when the connection is new, or the host
    /// could not associate this path with this file alone.
    host: Option<FileId>,
    path: PathBuf,
    /// What the window believes is on disk: what the connection's host would
    /// compare a save against, had it not gone. Means nothing while `stale`.
    baseline: Vec<u8>,
    missing: bool,
    /// The file differed from `baseline` at some reconnect. Only a Reload or a
    /// Save As, each of which makes a new `Known`, ends this; what the file
    /// later comes to hold does not.
    stale: bool,
    /// A plain save whose connection went before its answer came.
    attempt: Option<Vec<u8>>,
}

/// The window's side of the file worker, over a connection that may be
/// replaced between any two jobs.
struct FileClient<'a> {
    remote: &'a Remote,
    generation: Option<u64>,
    known: BTreeMap<FileId, Known>,
    next: FileId,
}

/// How a lost request is told to the person: what may have happened.
#[derive(Clone, Copy)]
enum Effect {
    Read,
    Restoring,
    Wrote,
    WroteNew,
    Changed,
}

impl Effect {
    fn consequence(self) -> &'static str {
        match self {
            Self::Read => "Try again.",
            Self::Restoring => "Nothing was sent.",
            Self::Wrote => {
                "The write may have reached disk; text remains unsaved. Save again to check."
            }
            Self::WroteNew => {
                "The new file may have been written; text remains unsaved. Look there before \
                 saving to that name again."
            }
            Self::Changed => "It may have taken effect; use g to refresh.",
        }
    }
}

/// Why a job did not get an answer, which a save must act on.
enum Failed {
    /// The host heard nothing of it.
    Unsent(String),
    /// It may have run.
    Unconfirmed(String),
}

impl From<Failed> for String {
    fn from(failed: Failed) -> Self {
        match failed {
            Failed::Unsent(detail) | Failed::Unconfirmed(detail) => detail,
        }
    }
}

impl FileClient<'_> {
    fn id(&mut self) -> Result<FileId> {
        let id = self.next;
        self.next = id.checked_add(1).ok_or("File IDs exhausted")?;
        Ok(id)
    }

    fn keep(&self) -> BTreeSet<FileId> {
        self.known.values().filter_map(|file| file.host).collect()
    }

    fn unreached(&self, error: &LinkError) -> String {
        format!(
            "Cannot reach {}: {error}. Nothing was sent.",
            self.remote.label
        )
    }

    /// One job over the connection `generation`, and its answer.
    fn exchange(
        &self,
        generation: u64,
        effect: Effect,
        operation: &Operation,
    ) -> std::result::Result<Result<Completion>, Failed> {
        let label = &self.remote.label;
        let message = encode_job(&self.keep(), operation).map_err(Failed::Unsent)?;
        let reply = self
            .remote
            .client
            .request_on(generation, FILES, &message)
            .map_err(|error| match error {
                RequestError::Unsent(error) => Failed::Unsent(self.unreached(&error)),
                RequestError::Stale => Failed::Unsent(format!(
                    "The connection to {label} changed; nothing was sent. Try again."
                )),
                RequestError::Lost(error) => Failed::Unconfirmed(format!(
                    "Connection to {label} lost: {error}. {}",
                    effect.consequence()
                )),
            })?;
        decode_completion(&reply)
            .map_err(|error| Failed::Unconfirmed(self.confused(generation, effect, &error)))
    }

    /// An answer that says nothing of what became of a job. What the host now
    /// holds is unknown, so it is given up, and the next job judges every file
    /// again on a new one.
    fn confused(&self, generation: u64, effect: Effect, why: &dyn std::fmt::Display) -> String {
        self.remote.client.abandon(generation);
        format!(
            "{} sent an answer this td-editor cannot read: {why}. {}",
            self.remote.label,
            effect.consequence()
        )
    }

    /// A connection to send on, with this window's files known to its host.
    fn ready(&mut self) -> Result<u64> {
        let generation = self
            .remote
            .client
            .connected(FILES)
            .map_err(|error| self.unreached(&error))?;
        if self.generation != Some(generation) {
            self.generation = None;
            self.restore(generation)?;
            self.generation = Some(generation);
        }
        Ok(generation)
    }

    /// Opens each file on a new host, which takes up the guards for an open
    /// file. Only the same file, at the same path, holding what the window
    /// believes is there carries on; a save that was left unanswered and turns
    /// out to have landed counts as what is there. Nothing here ends staleness:
    /// a file once different is not vouched for by what it holds later.
    fn restore(&mut self, generation: u64) -> Result<()> {
        for file in self.known.values_mut() {
            file.host = None;
        }
        let ids: Vec<FileId> = self.known.keys().copied().collect();
        for id in ids {
            let path = match self.known.get(&id) {
                Some(file) => file.path.clone(),
                None => continue,
            };
            let done = self.exchange(generation, Effect::Restoring, &Operation::Open(path))?;
            let Some(file) = self.known.get_mut(&id) else {
                continue;
            };
            match done {
                // A file another tab holds answers with that tab's path, and no
                // bytes: it is not this file.
                Ok(Completion::Open(loaded)) if loaded.path == file.path => {
                    let same = loaded.missing == file.missing && loaded.bytes == file.baseline;
                    // A stale file never has a save outstanding.
                    let landed = !loaded.missing && file.attempt.as_deref() == Some(&loaded.bytes);
                    file.host = Some(loaded.file);
                    if landed {
                        file.baseline = loaded.bytes;
                        file.missing = false;
                    } else if !same {
                        file.stale = true;
                    }
                }
                // Gone from the host's view altogether. A save meets the
                // conflict dialog, and `guard` stands in for the host's guards.
                _ => file.stale = true,
            }
            file.attempt = None;
        }
        Ok(())
    }

    /// What the host would refuse of an open file it has no association for.
    fn guard(&self, operation: &Operation) -> Result<()> {
        let unheld = self
            .known
            .values()
            .filter(|file| file.host.is_none())
            .map(|file| file.path.as_path());
        match operation {
            Operation::Delete(plan) => {
                if plan
                    .paths()
                    .any(|path| unheld.clone().any(|open| open.starts_with(path)))
                {
                    return Err(
                        "Deletion includes an open file or its parent; close that tab first".into(),
                    );
                }
            }
            Operation::Rename { source, .. }
                if unheld.clone().any(|open| open.starts_with(source.path())) =>
            {
                return Err(format!(
                    "Rename refused: an open file there could not be confirmed unchanged on {}",
                    self.remote.label
                ));
            }
            Operation::Mkdir { source, name }
                if unheld
                    .clone()
                    .any(|open| open.starts_with(source.path().join(name))) =>
            {
                return Err("That name belongs to an open file; close its tab first".into());
            }
            _ => {}
        }
        Ok(())
    }

    fn run(&mut self, job: Job) -> Result<Completion> {
        let Job { keep, operation } = job;
        // Closed tabs and refused admissions, as the local worker's retain.
        self.known.retain(|id, _| keep.contains(id));
        match operation {
            Operation::Dictionary(path) => crate::files::read_dictionary(&path)
                .map(Completion::Dictionary)
                .map_err(|detail| {
                    format!("Dictionary load refused; dictionary selection unchanged: {detail}")
                }),
            Operation::Open(path) => self.open(path),
            Operation::Reload(id) => self.reload(id),
            Operation::Save {
                file, path, bytes, ..
            } => self.save(file, path, bytes),
            mut changing => {
                let generation = self.ready()?;
                self.guard(&changing)?;
                // The host's reserved list names tabs it knows of; these it may not.
                if let Operation::Copy { reserved, .. } | Operation::Rename { reserved, .. } =
                    &mut changing
                {
                    let unheld = self.known.values().filter(|file| file.host.is_none());
                    reserved.extend(unheld.map(|file| file.path.clone()));
                }
                let done = self.exchange(generation, Effect::Changed, &changing)??;
                if let Completion::Renamed { result, .. } = &done {
                    // What it says moved decides what every open path becomes.
                    let asked = match &changing {
                        Operation::Rename { source, .. } => Some(source.path()),
                        _ => None,
                    };
                    if asked != Some(result.from.as_path()) {
                        let why = "it renamed something else";
                        return Err(self.confused(generation, Effect::Changed, &why));
                    }
                    for file in self.known.values_mut() {
                        if let Some(path) = result.relocated(&file.path) {
                            file.path = path;
                        }
                    }
                }
                Ok(done)
            }
        }
    }

    fn adopt(&mut self, loaded: Loaded) -> Result<Loaded> {
        let id = self.id()?;
        self.known.insert(
            id,
            Known {
                host: Some(loaded.file),
                path: loaded.path.clone(),
                baseline: loaded.bytes.clone(),
                missing: loaded.missing,
                stale: false,
                attempt: None,
            },
        );
        Ok(Loaded { file: id, ..loaded })
    }

    fn open(&mut self, path: PathBuf) -> Result<Completion> {
        let generation = self.ready()?;
        // A file this window has open and the host could not associate: the
        // tab is the person's, whether or not the file can be read now.
        let unheld = |file: &Known| file.host.is_none() && file.path == path;
        if let Some((id, file)) = self.known.iter().find(|(_, file)| unheld(file)) {
            return Ok(Completion::Open(Loaded {
                file: *id,
                path: file.path.clone(),
                bytes: Vec::new(),
                missing: file.missing,
            }));
        }
        match self.exchange(generation, Effect::Read, &Operation::Open(path))?? {
            Completion::Open(loaded) => {
                // The host's own association: this tab's text is not replaced
                // behind the person.
                let existing = self.known.iter().find_map(|(id, file)| {
                    (file.host == Some(loaded.file)
                        || (file.host.is_none() && file.path == loaded.path))
                        .then_some(*id)
                });
                if let Some(id) = existing {
                    return Ok(Completion::Open(Loaded {
                        file: id,
                        bytes: Vec::new(),
                        ..loaded
                    }));
                }
                Ok(Completion::Open(self.adopt(loaded)?))
            }
            directory @ Completion::Directory(_) => Ok(directory),
            _ => Err("The host answered an Open with something else".into()),
        }
    }

    fn reload(&mut self, id: FileId) -> Result<Completion> {
        let generation = self.ready()?;
        let file = self
            .known
            .get(&id)
            .ok_or("Reload needs an associated file")?;
        let path = file.path.clone();
        let done = match file.host {
            Some(host) => self.exchange(generation, Effect::Read, &Operation::Reload(host))??,
            // The host has no association to reload from: what is there now,
            // which replaces this one when the window accepts it.
            None => {
                let opened = Operation::Open(path.clone());
                self.exchange(generation, Effect::Read, &opened)??
            }
        };
        match done {
            Completion::Reload(loaded) | Completion::Open(loaded) => {
                if self
                    .known
                    .values()
                    .any(|file| file.host == Some(loaded.file))
                {
                    return Err(
                        "Reload refused: that file is open in another tab; text retained".into(),
                    );
                }
                if loaded.path != path {
                    return Err(format!(
                        "Reload refused: that path now leads to {:?}; text retained",
                        loaded.path
                    ));
                }
                Ok(Completion::Reload(self.adopt(loaded)?))
            }
            Completion::Directory(_) => {
                Err("Reload refused: that path is now a directory; text retained".into())
            }
            _ => Err("The host answered a Reload with something else".into()),
        }
    }

    fn save(
        &mut self,
        file: Option<FileId>,
        path: Option<PathBuf>,
        bytes: Vec<u8>,
    ) -> Result<Completion> {
        let unsaved = |detail: String| format!("{detail} Text remains unsaved.");
        let generation = self.ready().map_err(unsaved)?;
        let host = match file.map(|id| self.known.get(&id)) {
            None => None,
            Some(None) => return Err("Save refused: the file is no longer open".into()),
            Some(Some(known)) if known.stale && path.is_none() => {
                return Ok(Completion::Conflict(format!(
                    "{:?} could not be confirmed unchanged on {}; nothing was written.",
                    known.path, self.remote.label
                )))
            }
            Some(Some(known)) => known.host,
        };
        // The host's guards name tabs it knows of; these it may not. A plain save
        // goes where its own association is.
        let reserved = (self.known.iter())
            .filter(|(id, other)| path.is_some() && Some(**id) != file && other.host.is_none())
            .map(|(_, other)| other.path.clone())
            .collect();
        let effect = if path.is_some() {
            Effect::WroteNew
        } else {
            Effect::Wrote
        };
        let plain = path.is_none();
        let operation = Operation::Save {
            file: host,
            path,
            bytes,
            reserved,
        };
        let done = self.exchange(generation, effect, &operation);
        let Operation::Save { bytes, .. } = operation else {
            return Err("A save became something else".into());
        };
        let done = match done {
            Ok(done) => done?,
            Err(Failed::Unsent(detail)) => return Err(unsaved(detail)),
            Err(Failed::Unconfirmed(detail)) if plain => {
                if let Some(known) = file.and_then(|id| self.known.get_mut(&id)) {
                    known.attempt = Some(bytes);
                }
                return Ok(Completion::Unconfirmed(detail));
            }
            Err(Failed::Unconfirmed(detail)) => return Err(detail),
        };
        let as_asked = |answered: FileId, saved: &Path, known: &BTreeMap<FileId, Known>| {
            !plain
                || (Some(answered) == host
                    && file
                        .and_then(|id| known.get(&id))
                        .is_some_and(|known| known.path == saved))
        };
        match done {
            Completion::Saved { file: host, path } if as_asked(host, &path, &self.known) => {
                let known = Known {
                    host: Some(host),
                    path: path.clone(),
                    baseline: bytes,
                    missing: false,
                    stale: false,
                    attempt: None,
                };
                let id = match file {
                    Some(id) => id,
                    None => self.id()?,
                };
                self.known.insert(id, known);
                Ok(Completion::Saved { file: id, path })
            }
            reply @ (Completion::Conflict(_) | Completion::Unconfirmed(_)) => Ok(reply),
            _ => {
                let why = "it answered something other than a save";
                let detail = self.confused(generation, effect, &why);
                if !plain {
                    return Err(detail);
                }
                if let Some(known) = file.and_then(|id| self.known.get_mut(&id)) {
                    known.attempt = Some(bytes);
                }
                Ok(Completion::Unconfirmed(detail))
            }
        }
    }
}

/// The file worker of a window whose files are on `remote`.
pub(crate) fn client_worker(
    remote: &Remote,
    jobs: Receiver<Job>,
    results: SyncSender<Result<Completion>>,
) {
    let mut client = FileClient {
        remote,
        generation: None,
        known: BTreeMap::new(),
        next: 1,
    };
    while let Ok(job) = jobs.recv() {
        if results.send(client.run(job)).is_err() {
            break;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::session::Session;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use td_ui::editor::{Controller, Event};

    /// The far side's end, when a test wants it gone.
    #[derive(Clone, Default)]
    struct Ends(Arc<Mutex<Option<UnixStream>>>);

    impl Ends {
        fn hang_up(&self) {
            let end = self.0.lock().unwrap().take().unwrap();
            end.shutdown(Shutdown::Both).unwrap();
        }
    }

    /// A writer that drops the connection instead of a reply, once armed: the
    /// job ran, and the window never hears.
    struct Silenced {
        stream: UnixStream,
        armed: Arc<AtomicBool>,
        began: bool,
    }

    impl io::Write for Silenced {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.began && self.armed.swap(false, Ordering::Relaxed) {
                let _ = self.stream.shutdown(Shutdown::Both);
            }
            self.began = true;
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    /// A window's remote whose every connection is a real host on threads.
    fn remote() -> (Remote, Ends, Arc<AtomicBool>, Arc<AtomicU64>) {
        let ends = Ends::default();
        let silence = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicU64::new(0));
        let (kept, armed, counted) = (ends.clone(), silence.clone(), connections.clone());
        let connector: Connector = Box::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            let (near, far) = UnixStream::pair().unwrap();
            *kept.0.lock().unwrap() = Some(near.try_clone().unwrap());
            let armed = armed.clone();
            std::thread::spawn(move || {
                let writer = Silenced {
                    stream: far.try_clone().unwrap(),
                    armed,
                    began: false,
                };
                let _ = serve(far, writer);
            });
            Link::over(near.try_clone().unwrap(), near, OPTIONS)
        });
        (
            Remote::over("testhost".into(), connector),
            ends,
            silence,
            connections,
        )
    }

    fn poll(files: &mut Session, ui: &mut Controller) -> std::result::Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(done) = files.poll(ui) {
                return done.map_err(|failure| failure.detail);
            }
            assert!(Instant::now() < deadline, "no completion came");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn opened(files: &mut Session, ui: &mut Controller, path: &Path) -> td_ui::editor_model::TabId {
        files.open(path.to_path_buf()).unwrap();
        poll(files, ui).unwrap();
        ui.editor().active().unwrap()
    }

    fn type_text(ui: &mut Controller, tab: td_ui::editor_model::TabId, text: &str) -> u64 {
        let revision = ui.editor().document(tab).unwrap().revision();
        ui.dispatch(Event::Edit {
            tab,
            revision,
            command: td_ui::editor_model::Command::Insert(text.to_string()),
        })
        .unwrap();
        ui.editor().document(tab).unwrap().revision()
    }

    fn save(
        files: &mut Session,
        ui: &mut Controller,
        tab: td_ui::editor_model::TabId,
    ) -> std::result::Result<String, String> {
        let revision = ui.editor().document(tab).unwrap().revision();
        files.save(ui, tab, revision, None).unwrap();
        poll(files, ui)
    }

    fn scratch() -> td_fs::TempDir {
        td_fs::TempDir::new("td-editor-remote").unwrap()
    }

    #[test]
    fn a_file_on_the_far_side_opens_edits_and_saves_there() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"there\n").unwrap();
        let (remote, _ends, _silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        assert_eq!(ui.editor().document(tab).unwrap().text(), "there\n");
        assert_eq!(files.path(tab).unwrap(), path);
        type_text(&mut ui, tab, "here, ");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"here, there\n");
        assert!(!ui.editor().document(tab).unwrap().dirty());
        // And again: its baseline moved with the save.
        type_text(&mut ui, tab, "and ");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(connections.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_file_changed_there_meets_the_conflict_dialog() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, _ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        std::fs::write(&path, b"someone else's\n").unwrap();
        type_text(&mut ui, tab, "x");
        let failed = save(&mut files, &mut ui, tab).unwrap_err();
        assert!(failed.contains("conflict"), "{failed}");
        assert!(files.take_conflict().is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else's\n");
    }

    #[test]
    fn after_a_lost_connection_an_unchanged_file_saves_with_no_conflict() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, _silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        ends.hang_up();
        type_text(&mut ui, tab, "x");
        // A link that died idle, or one found dead as this save went out; either
        // way the next save lands on the new host.
        if save(&mut files, &mut ui, tab).is_err() {
            save(&mut files, &mut ui, tab).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"xone\n");
        assert_eq!(connections.load(Ordering::Relaxed), 2);
        assert!(files.take_conflict().is_none());
    }

    #[test]
    fn after_a_lost_connection_a_file_changed_there_is_a_conflict_not_an_overwrite() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        ends.hang_up();
        std::fs::write(&path, b"someone else's\n").unwrap();
        type_text(&mut ui, tab, "x");
        let mut failed = save(&mut files, &mut ui, tab).unwrap_err();
        if files.take_conflict().is_none() {
            failed = save(&mut files, &mut ui, tab).unwrap_err();
            assert!(files.take_conflict().is_some(), "{failed}");
        }
        assert!(failed.contains("changed"), "{failed}");
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else's\n");
        // Opening it again selects the tab, and replaces none of its text.
        files.open(path.clone()).unwrap();
        let said = poll(&mut files, &mut ui).unwrap();
        assert!(said.contains("already-open"), "{said}");
        assert!(ui.editor().document(tab).unwrap().dirty());
    }

    #[test]
    fn a_save_that_landed_unanswered_is_said_to_be_unconfirmed_then_recognised() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, _ends, silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        silence.store(true, Ordering::Relaxed);
        let failed = save(&mut files, &mut ui, tab).unwrap_err();
        assert!(failed.contains("may have reached disk"), "{failed}");
        assert!(ui.editor().document(tab).unwrap().dirty(), "not told saved");
        // It did land. Saving again finds that, not someone else's change.
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::read(&path).unwrap() != b"xone\n" {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        save(&mut files, &mut ui, tab).unwrap();
        assert!(files.take_conflict().is_none());
        assert!(!ui.editor().document(tab).unwrap().dirty());
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    /// Saves until the conflict dialog is up: the first may find the old link
    /// dead on its way. It must not write.
    fn conflict(files: &mut Session, ui: &mut Controller, tab: td_ui::editor_model::TabId) {
        for _ in 0..2 {
            let failed = save(files, ui, tab).unwrap_err();
            if files.take_conflict().is_some() {
                return;
            }
            assert!(!failed.contains("Saved"), "{failed}");
        }
        panic!("no conflict");
    }

    /// Saves until there has been a connection since `before` and the conflict
    /// dialog is up on it: a stale tab sends nothing that would find a link dead.
    fn conflict_on_a_new_connection(
        files: &mut Session,
        ui: &mut Controller,
        tab: td_ui::editor_model::TabId,
        connections: &AtomicU64,
        before: u64,
    ) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            save(files, ui, tab).unwrap_err();
            let conflicted = files.take_conflict().is_some();
            if conflicted && connections.load(Ordering::Relaxed) > before {
                return;
            }
            assert!(Instant::now() < deadline, "no new connection");
            std::thread::yield_now();
        }
    }

    /// Hangs up, then runs `change` before anything can reconnect.
    fn drop_link(ends: &Ends, change: impl FnOnce()) {
        ends.hang_up();
        change();
    }

    #[test]
    fn a_file_found_changed_stays_unknown_whatever_it_holds_after_another_reconnect() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, _silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "mine ");
        drop_link(&ends, || std::fs::write(&path, b"two\n").unwrap());
        conflict(&mut files, &mut ui, tab);
        // Emptied, which a cleared baseline would match; then the bytes the
        // window first read, which a kept one would.
        for held in [&b""[..], &b"one\n"[..]] {
            let before = connections.load(Ordering::Relaxed);
            drop_link(&ends, || std::fs::write(&path, held).unwrap());
            conflict_on_a_new_connection(&mut files, &mut ui, tab, &connections, before);
            assert_eq!(std::fs::read(&path).unwrap(), held);
        }
        // A Reload ends it.
        let permit = {
            save(&mut files, &mut ui, tab).unwrap_err();
            reload_permit(&mut files, &ui)
        };
        files.reload(&ui, permit).unwrap();
        poll(&mut files, &mut ui).unwrap();
        type_text(&mut ui, tab, "y");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"yone\n");
    }

    #[test]
    fn an_empty_file_gone_while_disconnected_is_not_written_back() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        drop_link(&ends, || std::fs::remove_file(&path).unwrap());
        conflict(&mut files, &mut ui, tab);
        assert!(!path.exists());
    }

    #[test]
    fn a_save_left_unanswered_is_judged_once_and_not_at_some_later_connection() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        silence.store(true, Ordering::Relaxed);
        save(&mut files, &mut ui, tab).unwrap_err();
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::read(&path).unwrap() != b"xone\n" {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // Someone put it back, and the next connection finds what it knew. Later
        // someone else writes those same bytes: that is not this save landing.
        std::fs::write(&path, b"one\n").unwrap();
        files.open(dir.path().to_path_buf()).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(connections.load(Ordering::Relaxed), 2);
        drop_link(&ends, || std::fs::write(&path, b"xone\n").unwrap());
        // A job that records no attempt reaches the new connection: a save could
        // find the old link dead on its way, and leave an attempt that is due.
        while connections.load(Ordering::Relaxed) < 3 {
            assert!(Instant::now() < deadline);
            files.open(dir.path().to_path_buf()).unwrap();
            let _ = poll(&mut files, &mut ui);
        }
        conflict(&mut files, &mut ui, tab);
        assert_eq!(std::fs::read(&path).unwrap(), b"xone\n");
    }

    #[test]
    fn a_tab_whose_path_now_names_another_tabs_file_cannot_write_it() {
        let dir = scratch();
        let (q, p) = (dir.path().join("q"), dir.path().join("p"));
        std::fs::write(&q, b"theirs\n").unwrap();
        std::fs::write(&p, b"mine\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        opened(&mut files, &mut ui, &q);
        let second = opened(&mut files, &mut ui, &p);
        type_text(&mut ui, second, "edit ");
        drop_link(&ends, || {
            std::fs::remove_file(&p).unwrap();
            std::fs::hard_link(&q, &p).unwrap();
        });
        conflict(&mut files, &mut ui, second);
        assert_eq!(std::fs::read(&q).unwrap(), b"theirs\n");
        // Nor does a Reload hand it the other tab's file, or empty its text.
        save(&mut files, &mut ui, second).unwrap_err();
        let permit = reload_permit(&mut files, &ui);
        files.reload(&ui, permit).unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("another tab"), "{refused}");
        let text = ui.editor().document(second).unwrap().text().to_string();
        assert_eq!(text, "edit mine\n");
        assert_eq!(std::fs::read(&q).unwrap(), b"theirs\n");
    }

    #[test]
    fn a_save_as_cannot_take_a_path_another_tab_reserved_while_disconnected() {
        let dir = scratch();
        let (q, p) = (dir.path().join("q"), dir.path().join("p"));
        std::fs::write(&q, b"theirs\n").unwrap();
        std::fs::write(&p, b"mine\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let first = opened(&mut files, &mut ui, &q);
        let second = opened(&mut files, &mut ui, &p);
        type_text(&mut ui, second, "edit ");
        drop_link(&ends, || std::fs::remove_file(&p).unwrap());
        conflict(&mut files, &mut ui, second);
        let revision = ui.editor().document(first).unwrap().revision();
        files.save(&ui, first, revision, Some(p.clone())).unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("nothing written") || refused.contains("association"));
        assert!(!p.exists());
    }

    #[test]
    fn a_tab_with_no_association_there_keeps_its_path_and_can_still_be_recovered() {
        let dir = scratch();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"a\n").unwrap();
        std::fs::write(&b, b"b\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let first = opened(&mut files, &mut ui, &a);
        let second = opened(&mut files, &mut ui, &b);
        type_text(&mut ui, second, "x");
        // A directory there at the reconnect, so the host associates nothing.
        drop_link(&ends, || {
            std::fs::remove_file(&b).unwrap();
            std::fs::create_dir(&b).unwrap();
        });
        conflict(&mut files, &mut ui, second);
        std::fs::remove_dir(&b).unwrap();

        // No other tab can take the path by that name, though nothing is there now.
        let revision = ui.editor().document(first).unwrap().revision();
        files.save(&ui, first, revision, Some(b.clone())).unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("another tab"), "{refused}");
        assert!(!b.exists());
        // Nor can a copy, a rename or a new directory, from a listing.
        let listing = opened(&mut files, &mut ui, dir.path());
        let row = {
            let snapshot = files.directory(listing).unwrap();
            (0..snapshot.len())
                .find(|row| snapshot.entry(*row).as_deref() == Some(a.as_path()))
                .unwrap()
        };
        let revision = ui.editor().document(listing).unwrap().revision();
        let source = files.directory(listing).unwrap().copy_source(row).unwrap();
        files
            .copy(&ui, listing, revision, source, "b".into())
            .unwrap();
        poll(&mut files, &mut ui).unwrap_err();
        assert!(!b.exists());
        let revision = ui.editor().document(listing).unwrap().revision();
        let source = files
            .directory(listing)
            .unwrap()
            .rename_source(row)
            .unwrap();
        files
            .rename(&ui, listing, revision, source, "b".into())
            .unwrap();
        poll(&mut files, &mut ui).unwrap_err();
        assert!(!b.exists() && a.exists());
        let revision = ui.editor().document(listing).unwrap().revision();
        let source = files.directory(listing).unwrap().creation_source();
        files
            .create_directory(&ui, listing, revision, source, "b".into())
            .unwrap();
        poll(&mut files, &mut ui).unwrap_err();
        assert!(!b.exists());

        // Another spelling of it selects the tab, once it can be read.
        std::fs::write(&b, b"later\n").unwrap();
        files.open(dir.path().join("sub/../b")).unwrap();
        let said = poll(&mut files, &mut ui).unwrap();
        assert!(said.contains("already-open"), "{said}");
        assert_eq!(ui.editor().document(second).unwrap().text(), "xb\n");

        // Reload adopts what is there, and saves carry on from it.
        save(&mut files, &mut ui, second).unwrap_err();
        let permit = reload_permit(&mut files, &ui);
        files.reload(&ui, permit).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(ui.editor().document(second).unwrap().text(), "later\n");
        type_text(&mut ui, second, "y");
        save(&mut files, &mut ui, second).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"ylater\n");
    }

    #[test]
    fn a_new_directory_cannot_be_made_above_a_tab_with_no_association_there() {
        let dir = scratch();
        let (sub, note) = (dir.path().join("sub"), dir.path().join("sub/note"));
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(&note, b"one\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let listing = opened(&mut files, &mut ui, dir.path());
        let tab = opened(&mut files, &mut ui, &note);
        type_text(&mut ui, tab, "x");
        drop_link(&ends, || std::fs::remove_dir_all(&sub).unwrap());
        conflict(&mut files, &mut ui, tab);
        let revision = ui.editor().document(listing).unwrap().revision();
        let source = files.directory(listing).unwrap().creation_source();
        files
            .create_directory(&ui, listing, revision, source, "sub".into())
            .unwrap();
        poll(&mut files, &mut ui).unwrap_err();
        assert!(!sub.exists());
    }

    #[test]
    fn a_save_as_spelled_another_way_cannot_take_the_path_of_a_tab_with_no_association() {
        let dir = scratch();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"a\n").unwrap();
        std::fs::write(&b, b"b\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let writer = opened(&mut files, &mut ui, &a);
        let stale = opened(&mut files, &mut ui, &b);
        type_text(&mut ui, stale, "x");
        // A directory cannot be opened as its file, so the host holds nothing
        // there; with it gone, the path is free but for the tab.
        drop_link(&ends, || {
            std::fs::remove_file(&b).unwrap();
            std::fs::create_dir(&b).unwrap();
        });
        conflict(&mut files, &mut ui, stale);
        std::fs::remove_dir(&b).unwrap();
        let revision = ui.editor().document(writer).unwrap().revision();
        let spelled = dir.path().join("sub/../b");
        files.save(&ui, writer, revision, Some(spelled)).unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("another tab"), "{refused}");
        assert!(!b.exists());
        // Both go on as they were: the writer on its own file, the stale tab
        // able to put its text where it belongs.
        type_text(&mut ui, writer, "y");
        save(&mut files, &mut ui, writer).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"ya\n");
        let revision = ui.editor().document(stale).unwrap().revision();
        files.save(&ui, stale, revision, Some(b.clone())).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"xb\n");
    }

    #[test]
    fn a_save_as_cannot_take_a_tabs_path_whose_parent_became_a_link() {
        let dir = scratch();
        let (a, d, real) = (
            dir.path().join("a"),
            dir.path().join("d"),
            dir.path().join("real"),
        );
        std::fs::create_dir(&d).unwrap();
        let b = d.join("b");
        std::fs::write(&a, b"a\n").unwrap();
        std::fs::write(&b, b"b\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let writer = opened(&mut files, &mut ui, &a);
        let stale = opened(&mut files, &mut ui, &b);
        type_text(&mut ui, stale, "x");
        drop_link(&ends, || {
            std::fs::remove_dir_all(&d).unwrap();
            std::fs::create_dir(&real).unwrap();
            std::os::unix::fs::symlink(&real, &d).unwrap();
        });
        conflict(&mut files, &mut ui, stale);
        let revision = ui.editor().document(writer).unwrap().revision();
        files.save(&ui, writer, revision, Some(b.clone())).unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("another tab"), "{refused}");
        assert!(!real.join("b").exists());
    }

    #[test]
    fn a_save_as_from_a_tab_with_no_association_there_writes_only_the_new_file() {
        let dir = scratch();
        let (path, target) = (dir.path().join("note"), dir.path().join("target"));
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        drop_link(&ends, || {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        });
        conflict(&mut files, &mut ui, tab);
        let revision = ui.editor().document(tab).unwrap().revision();
        files
            .save(&ui, tab, revision, Some(target.clone()))
            .unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"xone\n");
        assert!(path.is_dir());
        assert!(!ui.editor().document(tab).unwrap().dirty());
        // And it is that file's tab now.
        type_text(&mut ui, tab, "y");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"xyone\n");
    }

    #[test]
    fn a_lost_save_of_a_missing_file_that_landed_is_recognised() {
        let dir = scratch();
        let (path, other) = (dir.path().join("new"), dir.path().join("other"));
        std::fs::write(&other, b"other\n").unwrap();
        let (remote, ends, silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        silence.store(true, Ordering::Relaxed);
        save(&mut files, &mut ui, tab).unwrap_err();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // Another job finds it landed; a later connection still takes it for
        // what it now is, a file that exists.
        opened(&mut files, &mut ui, &other);
        let before = connections.load(Ordering::Relaxed);
        ends.hang_up();
        while connections.load(Ordering::Relaxed) == before {
            assert!(Instant::now() < deadline);
            if save(&mut files, &mut ui, tab).is_ok() {
                break;
            }
            assert!(files.take_conflict().is_none());
        }
        save(&mut files, &mut ui, tab).unwrap();
        assert!(files.take_conflict().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn a_path_that_now_leads_elsewhere_is_not_saved_through() {
        let dir = scratch();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let path = a.join("note");
        std::fs::write(&path, b"same\n").unwrap();
        std::fs::write(b.join("note"), b"same\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        drop_link(&ends, || {
            std::fs::rename(&a, dir.path().join("moved")).unwrap();
            std::os::unix::fs::symlink(&b, &a).unwrap();
        });
        conflict(&mut files, &mut ui, tab);
        assert_eq!(std::fs::read(b.join("note")).unwrap(), b"same\n");
        save(&mut files, &mut ui, tab).unwrap_err();
        let permit = reload_permit(&mut files, &ui);
        files.reload(&ui, permit).unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("Reload refused"), "{refused}");
        assert_eq!(ui.editor().document(tab).unwrap().text(), "xsame\n");
    }

    #[test]
    fn a_file_found_changed_is_still_guarded_as_open() {
        let dir = scratch();
        let (changed, unreadable) = (dir.path().join("changed"), dir.path().join("unreadable"));
        std::fs::write(&changed, b"one\n").unwrap();
        std::fs::write(&unreadable, b"one\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let listing = opened(&mut files, &mut ui, dir.path());
        let one = opened(&mut files, &mut ui, &changed);
        let other = opened(&mut files, &mut ui, &unreadable);
        type_text(&mut ui, one, "x");
        type_text(&mut ui, other, "x");
        drop_link(&ends, || {
            std::fs::write(&changed, b"two\n").unwrap();
            // Past what a tab may hold, so the new host can associate nothing.
            std::fs::write(&unreadable, b"\xff\xfe\xff").unwrap();
        });
        conflict(&mut files, &mut ui, one);
        // Marked for deletion in a listing of the directory they are in.
        for path in [&changed, &unreadable] {
            let mut snapshot = files.directory(listing).unwrap().clone();
            for at in 0..snapshot.len() {
                let hit = snapshot.entry(at).as_deref() == Some(path.as_path());
                snapshot.mark(at, hit).unwrap();
            }
            let plan = snapshot.delete_plan().unwrap();
            let revision = ui.editor().document(listing).unwrap().revision();
            files.delete(&ui, listing, revision, plan).unwrap();
            let refused = poll(&mut files, &mut ui).unwrap_err();
            assert!(refused.contains("open file"), "{refused}");
            assert!(path.exists());
        }
        // Nor renamed out from under its tab.
        let row = {
            let snapshot = files.directory(listing).unwrap();
            (0..snapshot.len())
                .find(|row| snapshot.entry(*row).as_deref() == Some(unreadable.as_path()))
                .unwrap()
        };
        let revision = ui.editor().document(listing).unwrap().revision();
        let source = files
            .directory(listing)
            .unwrap()
            .rename_source(row)
            .unwrap();
        files
            .rename(&ui, listing, revision, source, "moved".into())
            .unwrap();
        let refused = poll(&mut files, &mut ui).unwrap_err();
        assert!(refused.contains("Rename refused"), "{refused}");
        assert!(unreadable.exists());
        // Opening a path whose file cannot be read selects its tab.
        files.open(unreadable.clone()).unwrap();
        let said = poll(&mut files, &mut ui).unwrap();
        assert!(said.contains("already-open"), "{said}");
    }

    #[test]
    fn undoing_after_an_unconfirmed_save_does_not_read_as_saved() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, _ends, silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        silence.store(true, Ordering::Relaxed);
        save(&mut files, &mut ui, tab).unwrap_err();
        let revision = ui.editor().document(tab).unwrap().revision();
        ui.dispatch(Event::Edit {
            tab,
            revision,
            command: td_ui::editor_model::Command::Undo,
        })
        .unwrap();
        let document = ui.editor().document(tab).unwrap();
        assert_eq!(document.text(), "one\n");
        assert!(document.dirty(), "the disk may hold the undone text");
    }

    #[test]
    fn a_lost_save_as_says_where_to_look_and_records_no_attempt_on_the_old_file() {
        let dir = scratch();
        let (path, target) = (dir.path().join("note"), dir.path().join("target"));
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, _ends, silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        silence.store(true, Ordering::Relaxed);
        let revision = ui.editor().document(tab).unwrap().revision();
        files
            .save(&ui, tab, revision, Some(target.clone()))
            .unwrap();
        let failed = poll(&mut files, &mut ui).unwrap_err();
        assert!(failed.contains("Look there"), "{failed}");
        // The old file is as it was, so its saved state still means something,
        // and no attempt on it is left to be taken for a landed save.
        let revision = ui.editor().document(tab).unwrap().revision();
        ui.dispatch(Event::Edit {
            tab,
            revision,
            command: td_ui::editor_model::Command::Undo,
        })
        .unwrap();
        assert!(!ui.editor().document(tab).unwrap().dirty());
        type_text(&mut ui, tab, "y");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"yone\n");
    }

    #[test]
    fn a_completion_that_reconnects_does_not_put_a_save_before_a_host_that_lacks_its_file() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, _silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        ends.hang_up();
        let prefix = format!("{}/no", dir.path().display());
        let completer = files.remote().unwrap();
        // Its first try may be the one to find the link dead.
        if completer.complete(&prefix).is_err() {
            completer.complete(&prefix).unwrap();
        }
        assert_eq!(connections.load(Ordering::Relaxed), 2);
        type_text(&mut ui, tab, "x");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"xone\n");
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    /// A far side that opens anything as `bytes`, says `other` of anything else
    /// and `completions` of any completion; and how many connections it has had.
    fn a_far_side_saying(
        bytes: &'static [u8],
        other: impl Fn() -> Vec<u8> + Clone + Send + 'static,
        completions: Vec<u8>,
    ) -> (Remote, Arc<AtomicU64>) {
        let connections = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&connections);
        let connector: Connector = Box::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            let (near, far) = UnixStream::pair().unwrap();
            let (other, completions) = (other.clone(), completions.clone());
            std::thread::spawn(move || {
                let reader = far.try_clone().unwrap();
                let mut link = Link::over(reader, far, OPTIONS).unwrap();
                let jobs = link.take_inbox(FILES).unwrap();
                let asked = link.take_inbox(COMPLETION).unwrap();
                let sender = link.sender();
                let completer = link.sender();
                std::thread::spawn(move || {
                    while asked.recv().is_ok() {
                        let _ = completer.send(COMPLETION, &completions);
                    }
                });
                let mut next = 1;
                while let Ok(message) = jobs.recv() {
                    let reply = match decode_job(&message).unwrap().operation {
                        Operation::Open(path) => {
                            next += 1;
                            encode_completion(&Ok(Completion::Open(Loaded {
                                file: next,
                                path,
                                bytes: bytes.to_vec(),
                                missing: false,
                            })))
                        }
                        _ => other(),
                    };
                    if sender.send(FILES, &reply).is_err() {
                        return;
                    }
                }
            });
            Link::over(near.try_clone().unwrap(), near, OPTIONS)
        });
        (Remote::over("liar".into(), connector), connections)
    }

    #[test]
    fn a_rename_answered_with_another_source_moves_nothing_and_ends_the_connection() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, connections) = a_far_side_saying(
            b"one\n",
            || {
                encode_completion(&Ok(Completion::Renamed {
                    result: crate::files::Renamed {
                        from: "/".into(),
                        to: "/elsewhere".into(),
                        warning: None,
                    },
                    listings: Vec::new(),
                }))
            },
            Vec::new(),
        );
        remote.connect().unwrap();
        let mut client = FileClient {
            remote: &remote,
            generation: None,
            known: BTreeMap::new(),
            next: 1,
        };
        let job = |keep: BTreeSet<FileId>, operation| Job { keep, operation };
        let opened = client
            .run(job(BTreeSet::new(), Operation::Open(path.clone())))
            .unwrap();
        let Completion::Open(loaded) = opened else {
            panic!("not opened");
        };
        let source = crate::files::RenameSource::inspect(&path).unwrap();
        let refused = client.run(job(
            BTreeSet::from([loaded.file]),
            Operation::Rename {
                source,
                name: "other".into(),
                reserved: Vec::new(),
            },
        ));
        assert!(matches!(refused, Err(detail) if detail.contains("may have taken effect")));
        assert_eq!(client.known[&loaded.file].path, path);
        client.ready().unwrap();
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    /// A save `reply` answers: told as may-have-landed, and undo does not read
    /// as saved.
    fn a_save_answered(
        reply: impl Fn(&Path) -> Vec<u8> + Clone + Send + 'static,
        said: &str,
    ) -> u64 {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let there = path.clone();
        let (remote, connections) = a_far_side_saying(b"one\n", move || reply(&there), Vec::new());
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        let failed = save(&mut files, &mut ui, tab).unwrap_err();
        assert!(failed.contains(said), "{failed}");
        let revision = ui.editor().document(tab).unwrap().revision();
        ui.dispatch(Event::Edit {
            tab,
            revision,
            command: td_ui::editor_model::Command::Undo,
        })
        .unwrap();
        let document = ui.editor().document(tab).unwrap();
        assert_eq!(document.text(), "one\n");
        assert!(document.dirty());
        connections.load(Ordering::Relaxed)
    }

    #[test]
    fn a_save_answered_with_something_else_may_have_landed() {
        let reply = |_: &Path| {
            encode_completion(&Ok(Completion::Reload(Loaded {
                file: 2,
                path: "/x".into(),
                bytes: Vec::new(),
                missing: false,
            })))
        };
        a_save_answered(reply, "may have reached disk");
    }

    #[test]
    fn a_save_answered_as_saved_somewhere_else_may_have_landed() {
        // Its own file, as the far side numbered it, at another path.
        let reply = |_: &Path| {
            encode_completion(&Ok(Completion::Saved {
                file: 2,
                path: "/elsewhere".into(),
            }))
        };
        a_save_answered(reply, "may have reached disk");
    }

    #[test]
    fn a_save_answered_as_another_file_saved_at_its_path_may_have_landed() {
        let reply = |path: &Path| {
            encode_completion(&Ok(Completion::Saved {
                file: 999,
                path: path.to_path_buf(),
            }))
        };
        a_save_answered(reply, "may have reached disk");
    }

    #[test]
    fn a_save_answered_unreadably_may_have_landed() {
        a_save_answered(|_: &Path| vec![255], "cannot read");
    }

    #[test]
    fn a_host_saying_a_save_is_unconfirmed_is_believed() {
        let reply =
            |_: &Path| encode_completion(&Ok(Completion::Unconfirmed("publication went".into())));
        assert_eq!(a_save_answered(reply, "publication went"), 1);
    }

    #[test]
    fn a_completion_must_complete_what_was_typed() {
        let matches = |paths: &[&str]| {
            let mut w = Writer::new();
            w.count(paths.len());
            for path in paths {
                w.bytes(path.as_bytes());
            }
            w.u64(0);
            let mut reply = vec![1];
            reply.extend_from_slice(&w.into_bytes());
            reply
        };
        let cases: [(&[&str], bool); 3] = [
            (&["/typed/one", "/typed/two"], true),
            (&[], false),
            (&["/typed/one", "/somewhere/else"], false),
        ];
        for (paths, good) in cases {
            let (remote, _connections) = a_far_side_saying(b"", Vec::new, matches(paths));
            assert_eq!(remote.complete("/typed/").is_ok(), good, "{paths:?}");
        }
    }

    #[test]
    fn a_completion_request_with_bytes_after_it_is_refused() {
        let (near, far) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let reader = far.try_clone().unwrap();
            let _ = serve(reader, far);
        });
        let mut link = Link::over(near.try_clone().unwrap(), near, OPTIONS).unwrap();
        let replies = link.take_inbox(COMPLETION).unwrap();
        let mut w = Writer::default();
        w.bytes(b"/");
        w.u8(0);
        link.sender().send(COMPLETION, &w.into_bytes()).unwrap();
        let answer = decode_matches(&replies.recv().unwrap()).unwrap();
        assert!(answer.is_err());
    }

    #[test]
    fn a_dictionarys_path_completes_here_even_with_no_far_side_to_ask() {
        let dir = scratch();
        std::fs::write(dir.path().join("words"), b"").unwrap();
        let connector: Connector = Box::new(|| Err(LinkError::Closed("no host".into())));
        let remote = Arc::new(Remote::over("gone".into(), connector));
        let mut worker = crate::path_completion::Worker::start_for(Some(&remote)).unwrap();
        worker
            .request(1, format!("{}/wo", dir.path().display()), true)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let done = loop {
            if let Some(done) = worker.poll() {
                break done;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(done.0, 1);
        assert_eq!(done.1.unwrap().count(), 1);
    }

    #[test]
    fn a_host_that_cannot_be_reached_says_so_at_startup() {
        let connector: Connector = Box::new(|| Err(LinkError::Closed("no route to host".into())));
        let refused = Session::start_remote(Remote::over("gone".into(), connector));
        let Err(detail) = refused else {
            panic!("it started");
        };
        assert!(
            detail.contains("gone") && detail.contains("no route"),
            "{detail}"
        );
    }

    #[test]
    fn with_no_connection_a_save_says_nothing_was_sent() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let attempts = Arc::new(AtomicU64::new(0));
        let ends = Ends::default();
        let (counted, kept) = (attempts.clone(), ends.clone());
        let connector: Connector = Box::new(move || {
            if counted.fetch_add(1, Ordering::Relaxed) > 0 {
                return Err(LinkError::Closed("no route to host".into()));
            }
            let (near, far) = UnixStream::pair().unwrap();
            *kept.0.lock().unwrap() = Some(near.try_clone().unwrap());
            std::thread::spawn(move || {
                let reader = far.try_clone().unwrap();
                let _ = serve(reader, far);
            });
            Link::over(near.try_clone().unwrap(), near, OPTIONS)
        });
        let mut files = Session::start_remote(Remote::over("h".into(), connector)).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        ends.hang_up();
        type_text(&mut ui, tab, "x");
        // It may first find the link dead with the save on its way.
        let mut failed = save(&mut files, &mut ui, tab).unwrap_err();
        if !failed.contains("Nothing was sent") {
            failed = save(&mut files, &mut ui, tab).unwrap_err();
        }
        assert!(failed.contains("Nothing was sent"), "{failed}");
        assert!(ui.editor().document(tab).unwrap().dirty());
        assert_eq!(std::fs::read(&path).unwrap(), b"one\n");
    }

    #[test]
    fn a_dictionary_is_read_here_with_no_connection_at_all() {
        let dir = scratch();
        let words = dir.path().join("words");
        std::fs::write(&words, b"hello\nworld\n").unwrap();
        let (remote, ends, _silence, connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        ends.hang_up();
        let before = connections.load(Ordering::Relaxed);
        let mut ui = Controller::default();
        files.initial_dictionary(&mut ui, words).unwrap();
        assert!(files.take_dictionary().is_some());
        assert_eq!(connections.load(Ordering::Relaxed), before);
    }

    #[test]
    fn a_directory_there_browses_and_its_operations_run_there() {
        let dir = scratch();
        std::fs::write(dir.path().join("b"), b"b").unwrap();
        std::fs::write(dir.path().join("a\nodd"), b"a").unwrap();
        let (remote, _ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, dir.path());
        let listing = ui.editor().document(tab).unwrap().text().to_string();
        assert!(listing.contains("a\\nodd"), "{listing}");
        assert!(listing.lines().last().unwrap().ends_with(" b"), "{listing}");
        let revision = ui.editor().document(tab).unwrap().revision();

        // Copy, by an observation made there and judged there.
        let source = files.directory(tab).unwrap().copy_source(1).unwrap();
        files.copy(&ui, tab, revision, source, "c".into()).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(std::fs::read(dir.path().join("c")).unwrap(), b"b");

        // Make a directory.
        let revision = ui.editor().document(tab).unwrap().revision();
        let source = files.directory(tab).unwrap().creation_source();
        files
            .create_directory(&ui, tab, revision, source, "made".into())
            .unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert!(dir.path().join("made").is_dir());

        // Rename: an open tab of that file follows it.
        let b = dir.path().join("b");
        let open = opened(&mut files, &mut ui, &b);
        assert_ne!(open, tab);
        let row = {
            let snapshot = files.directory(tab).unwrap();
            (0..snapshot.len())
                .find(|row| snapshot.entry(*row).as_deref() == Some(b.as_path()))
                .unwrap()
        };
        let revision = ui.editor().document(tab).unwrap().revision();
        let source = files.directory(tab).unwrap().rename_source(row).unwrap();
        files
            .rename(&ui, tab, revision, source, "renamed".into())
            .unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(files.path(open).unwrap(), dir.path().join("renamed"));
        // And it still saves, by its new name.
        type_text(&mut ui, open, "x");
        save(&mut files, &mut ui, open).unwrap();
        assert_eq!(std::fs::read(dir.path().join("renamed")).unwrap(), b"xb");

        // Delete, by a plan of marks.
        let row = {
            let snapshot = files.directory(tab).unwrap();
            (0..snapshot.len())
                .find(|row| snapshot.entry(*row).as_deref() == Some(dir.path().join("c").as_path()))
                .unwrap()
        };
        let revision = ui.editor().document(tab).unwrap().revision();
        files.mark_directory(&mut ui, tab, revision, true).ok();
        let mut snapshot = files.directory(tab).unwrap().clone();
        for at in 0..snapshot.len() {
            snapshot.mark(at, at == row).unwrap();
        }
        let plan = snapshot.delete_plan().unwrap();
        let revision = ui.editor().document(tab).unwrap().revision();
        files.delete(&ui, tab, revision, plan).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert!(!dir.path().join("c").exists());
    }

    #[test]
    fn a_reload_replaces_the_baseline_when_accepted_and_keeps_it_when_cancelled() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, _ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        std::fs::write(&path, b"two\n").unwrap();
        save(&mut files, &mut ui, tab).unwrap_err();
        // Cancelled: the text and the old baseline stay, so a save still refuses.
        let permit = reload_permit(&mut files, &ui);
        files.reload(&ui, permit).unwrap();
        files.cancel_reload();
        let said = poll(&mut files, &mut ui).unwrap();
        assert!(said.contains("cancelled"), "{said}");
        assert_eq!(ui.editor().document(tab).unwrap().text(), "xone\n");
        save(&mut files, &mut ui, tab).unwrap_err();
        assert_eq!(std::fs::read(&path).unwrap(), b"two\n");
        // Accepted: the next job commits it there, and a save compares against
        // the new bytes.
        let permit = reload_permit(&mut files, &ui);
        files.reload(&ui, permit).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(ui.editor().document(tab).unwrap().text(), "two\n");
        type_text(&mut ui, tab, "y");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"ytwo\n");
    }

    #[test]
    fn a_file_that_changed_while_disconnected_reloads_and_then_saves() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (remote, ends, _silence, _connections) = remote();
        let mut files = Session::start_remote(remote).unwrap();
        let mut ui = Controller::default();
        let tab = opened(&mut files, &mut ui, &path);
        type_text(&mut ui, tab, "x");
        ends.hang_up();
        std::fs::write(&path, b"two\n").unwrap();
        // The first may find the link dead on its way; the second has a host.
        save(&mut files, &mut ui, tab).unwrap_err();
        save(&mut files, &mut ui, tab).unwrap_err();
        let permit = reload_permit(&mut files, &ui);
        files.reload(&ui, permit).unwrap();
        poll(&mut files, &mut ui).unwrap();
        assert_eq!(ui.editor().document(tab).unwrap().text(), "two\n");
        type_text(&mut ui, tab, "y");
        save(&mut files, &mut ui, tab).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"ytwo\n");
    }

    fn reload_permit(files: &mut Session, ui: &Controller) -> td_ui::editor_dialog::Reload {
        let target = files.take_conflict().unwrap();
        let mut conflict = td_ui::editor_dialog::Conflict::new(ui.editor(), target).unwrap();
        assert!(conflict.answer(ui.editor(), false).unwrap().is_none());
        conflict.answer(ui.editor(), true).unwrap().unwrap()
    }

    #[test]
    fn path_completion_reads_the_far_side() {
        let dir = scratch();
        std::fs::write(dir.path().join("alpha"), b"").unwrap();
        std::fs::create_dir(dir.path().join("alps")).unwrap();
        let (remote, _ends, _silence, _connections) = remote();
        let files = Session::start_remote(remote).unwrap();
        let remote = files.remote().unwrap();
        let prefix = format!("{}/al", dir.path().display());
        let mut matches = remote.complete(&prefix).unwrap();
        assert_eq!(matches.count(), 2);
        assert_eq!(matches.prefix(), format!("{}/alp", dir.path().display()));
        assert!(matches.cycle(false).unwrap().ends_with("alpha"));
        assert!(matches.cycle(false).unwrap().ends_with("alps/"));
        assert!(remote
            .complete(&format!("{}/nothing", dir.path().display()))
            .is_err());
    }

    #[test]
    fn a_job_in_hand_finishes_after_the_window_goes() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (near, far) = UnixStream::pair().unwrap();
        let host = std::thread::spawn(move || {
            let reader = far.try_clone().unwrap();
            serve(reader, far)
        });
        let mut link = Link::over(
            near.try_clone().unwrap(),
            near.try_clone().unwrap(),
            OPTIONS,
        )
        .unwrap();
        let replies = link.take_inbox(FILES).unwrap();
        let sender = link.sender();
        let open = encode_job(&BTreeSet::new(), &Operation::Open(path.clone())).unwrap();
        sender.send(FILES, &open).unwrap();
        let Ok(Completion::Open(loaded)) = decode_completion(&replies.recv().unwrap()).unwrap()
        else {
            panic!("no open");
        };
        // Large enough that the job outlasts its window's going by far.
        let saved = vec![b'x'; MAX_FILE_BYTES];
        let save = Operation::Save {
            file: Some(loaded.file),
            path: None,
            bytes: saved.clone(),
            reserved: Vec::new(),
        };
        let keep = BTreeSet::from([loaded.file]);
        sender
            .send(FILES, &encode_job(&keep, &save).unwrap())
            .unwrap();
        // The window is gone with the save asked for and unanswered.
        near.shutdown(Shutdown::Both).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !host.is_finished() {
            assert!(Instant::now() < deadline, "the host did not end");
            std::thread::sleep(Duration::from_millis(1));
        }
        host.join().unwrap().unwrap();
        assert!(std::fs::read(&path).unwrap() == saved);
    }

    #[test]
    fn a_job_that_cannot_be_read_is_refused_and_the_connection_carries_on() {
        let dir = scratch();
        let path = dir.path().join("note");
        std::fs::write(&path, b"one\n").unwrap();
        let (near, far) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let reader = far.try_clone().unwrap();
            let _ = serve(reader, far);
        });
        let mut link = Link::over(near.try_clone().unwrap(), near, OPTIONS).unwrap();
        let replies = link.take_inbox(FILES).unwrap();
        let sender = link.sender();
        for bad in [&b""[..], &[0, 0, 0, 0, 0, 0, 0, 0, 99][..], &[0xff; 9][..]] {
            sender.send(FILES, bad).unwrap();
            let done = decode_completion(&replies.recv().unwrap()).unwrap();
            assert!(matches!(done, Err(detail) if detail.contains("refused")));
        }
        let open = encode_job(&BTreeSet::new(), &Operation::Open(path)).unwrap();
        sender.send(FILES, &open).unwrap();
        assert!(matches!(
            decode_completion(&replies.recv().unwrap()).unwrap(),
            Ok(Completion::Open(_))
        ));
    }

    #[test]
    fn jobs_and_completions_round_trip_and_trailing_bytes_are_refused() {
        let keep = BTreeSet::from([3, 9]);
        let save = Operation::Save {
            file: Some(9),
            path: Some("/a/\u{e9}".into()),
            bytes: vec![0, 255, 10],
            reserved: vec!["/b".into()],
        };
        let mut bytes = encode_job(&keep, &save).unwrap();
        let job = decode_job(&bytes).unwrap();
        assert_eq!(job.keep, keep);
        let Operation::Save {
            file,
            path,
            bytes: saved,
            reserved,
        } = job.operation
        else {
            panic!("not a save");
        };
        assert_eq!(reserved, [PathBuf::from("/b")]);
        assert_eq!(
            (file, path.unwrap(), saved),
            (Some(9), PathBuf::from("/a/\u{e9}"), vec![0, 255, 10])
        );
        bytes.push(0);
        assert!(decode_job(&bytes).is_err());
        assert!(encode_job(&keep, &Operation::Dictionary("/w".into())).is_err());

        let done: Result<Completion> = Ok(Completion::Saved {
            file: 4,
            path: "/x".into(),
        });
        let mut bytes = encode_completion(&done);
        assert!(matches!(
            decode_completion(&bytes).unwrap(),
            Ok(Completion::Saved { file: 4, .. })
        ));
        bytes.push(0);
        assert!(decode_completion(&bytes).is_err());
    }

    /// A listing a far side might send, by hand.
    fn listing(name: &[u8], kind: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(DONE_DIRECTORY);
        w.bytes(b"/there");
        w.u64(1);
        w.u64(2);
        w.count(1);
        w.bytes(name);
        w.u32(kind);
        w.u32(0o644);
        w.u64(1);
        w.u32(0);
        w.u32(0);
        w.u64(0);
        w.i64(0);
        w.i64(0);
        for _ in 0..2 {
            w.u64(0);
        }
        w.u32(0);
        w.u32(0);
        w.u32(0);
        w.u64(0);
        w.u64(0);
        for _ in 0..4 {
            w.i64(0);
        }
        w.into_bytes()
    }

    #[test]
    fn a_far_sides_listing_is_held_to_what_a_directory_can_hold() {
        assert!(matches!(
            decode_completion(&listing(b"fine", u32::from('-'))).unwrap(),
            Ok(Completion::Directory(_))
        ));
        for name in [&b".."[..], b".", b"", b"a/b", b"../../etc", b"a\0b"] {
            assert!(
                decode_completion(&listing(name, u32::from('-'))).is_err(),
                "{name:?}"
            );
        }
        assert!(decode_completion(&listing(b"fine", u32::from('x'))).is_err());
        assert!(decode_completion(&listing(b"fine", 0xFFFF_FFFF)).is_err());
    }

    #[test]
    fn a_far_sides_words_reach_a_notice_with_no_control_characters() {
        let mut w = Writer::new();
        w.u8(DONE_FAILED);
        w.bytes(b"nope\x1b[31m\nsecond\x07");
        let Err(detail) = decode_completion(&w.into_bytes()).unwrap() else {
            panic!("not a failure");
        };
        assert!(!detail.chars().any(char::is_control), "{detail:?}");
        assert!(detail.contains("nope") && detail.contains("second"));
    }

    #[test]
    fn a_far_side_cannot_name_a_path_with_a_nul_or_past_the_bound() {
        for path in [vec![], b"/a\0b".to_vec(), vec![b'a'; PATH_BYTES + 1]] {
            let mut w = Writer::new();
            w.u8(DONE_SAVED);
            w.u64(1);
            w.bytes(&path);
            assert!(decode_completion(&w.into_bytes()).is_err());
        }
    }
}
