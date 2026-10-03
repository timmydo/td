//! The test vault: a synthetic notebook served on the vault thread in
//! place of td-secret's, built only with the `test-vault` feature, which
//! the native compositor cases enable and no recipe does. It holds two
//! entries in memory under one primary key whose PIN is 1234, keeps no
//! file but its journal, and watches no host. The case directory named
//! by `TD_PASS_TEST_VAULT` holds the journal, one line per thing the
//! vault was asked or did, and the case's one-shot controls: `swap`
//! answers the first open with swap on storage the window must accept,
//! `fail` refuses the next save, `elsewhere` saves the entry from another
//! device first, so the next save is stale, and `hold` keeps the next
//! save in flight after its PIN, as a token at work would, until the
//! window cancels it. Unlocking and saving ask the key's presence and
//! then its PIN, with td-secret's words for each.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use super::{closed, pass, refused, Job};
use crate::plain::{Bytes, Text};
use crate::protocol::{
    Answer, Ask, Change, Command, EntryId, Failure, Item, KeyLabel, Keys, Op, PinUse, Reply, Role,
};

/// Where the case's journal and controls are.
const DIRECTORY: &str = "TD_PASS_TEST_VAULT";

/// td-secret's names for the operations the test vault authorizes.
const UNLOCK: &str = "unlock the portable vault for browsing";
const SAVE: &str = "save a new portable vault revision";

/// How long a held save waits for the window before failing.
const HOLD: Duration = Duration::from_secs(10);

struct Entry {
    id: EntryId,
    revision: u64,
    title: String,
    body: Vec<u8>,
}

/// The host's lock is not watched here; the window says so.
pub(super) fn watch() -> Result<pass::HostEvents, String> {
    Err("the test vault watches no host".to_owned())
}

pub(super) fn serve(
    jobs: &Receiver<Job>,
    answers: &Receiver<(Op, Answer)>,
    replies: &Sender<Reply>,
) {
    let directory = std::env::var_os(DIRECTORY).map(PathBuf::from);
    let mut entries = vec![
        entry(1, "Alpha", "alpha one\nalpha two\n"),
        entry(2, "Beta", "beta\n"),
    ];
    let mut next = 3;
    let mut unlocked = false;
    // Nothing here is a token operation, so a job's own cancel goes
    // unused: a held save watches for the window's decline instead.
    while let Ok(Job { command, .. }) = jobs.recv() {
        let Some(directory) = directory.as_deref() else {
            let text = format!("{DIRECTORY} names no case directory");
            if replies.send(Reply::Refused { text }).is_err() {
                break;
            }
            continue;
        };
        let vault = Vault {
            directory,
            replies,
            answers,
        };
        let reply = match command {
            Command::Open if vault.take("swap") => {
                vault.note("open: swap");
                Reply::Swap {
                    devices: vec![SWAP.to_owned()],
                }
            }
            Command::Open => {
                vault.note("open");
                Reply::Opened { keys: Some(keys()) }
            }
            Command::AcceptSwap => {
                vault.note("open: swap accepted");
                Reply::Opened { keys: Some(keys()) }
            }
            Command::Unlock { op, key: 0 } => match vault.authorize(op, "unlock", UNLOCK) {
                Ok(()) => {
                    unlocked = true;
                    vault.note("unlocked");
                    Reply::Unlocked {
                        op,
                        entries: entries.iter().map(item).collect(),
                        keys: Keys {
                            labels: keys(),
                            using: Some(0),
                        },
                    }
                }
                Err(failure) => {
                    vault.note(&failure.text);
                    Reply::Failed { op, failure }
                }
            },
            Command::Read { id } => match entries.iter().find(|entry| entry.id == id) {
                Some(entry) if unlocked => {
                    vault.note(&format!("read {:?}", entry.title));
                    Reply::Entry {
                        id,
                        revision: entry.revision,
                        title: Text::new(entry.title.clone()),
                        body: Bytes::copy(&entry.body),
                    }
                }
                _ => Reply::Missing { id },
            },
            Command::Apply { op, change } if unlocked => {
                match vault.apply(op, change, &mut entries, &mut next) {
                    Ok((id, revision)) => {
                        vault.note(&format!("committed {} {revision:?}", number(&id)));
                        Reply::Committed { op, id, revision }
                    }
                    Err(failure) => {
                        vault.note(&failure.text);
                        Reply::Failed { op, failure }
                    }
                }
            }
            Command::Lock => {
                unlocked = false;
                vault.note("lock");
                Reply::Locked { keys: Some(keys()) }
            }
            Command::Apply { op, .. } => closed(op),
            Command::Create { op, .. }
            | Command::Unlock { op, .. }
            | Command::UseKey { op, .. }
            | Command::AddKey { op }
            | Command::ReplaceKeys { op, .. }
            | Command::Export { op, .. }
            | Command::ReadCopy { op, .. }
            | Command::Import { op, .. } => refused(op, "the test vault does not do this"),
        };
        while answers.try_recv().is_ok() {}
        if replies.send(reply).is_err() {
            break;
        }
    }
}

/// One job's view of the case and the window.
struct Vault<'a> {
    directory: &'a Path,
    replies: &'a Sender<Reply>,
    answers: &'a Receiver<(Op, Answer)>,
}

impl Vault<'_> {
    /// Appends `line` to the journal; a case that cannot be told fails
    /// on what it waits for.
    fn note(&self, line: &str) {
        if let Ok(mut journal) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("journal"))
        {
            let _ = journal.write_all(format!("{line}\n").as_bytes());
        }
    }

    /// Takes the one-shot control `name`, if the case set it.
    fn take(&self, name: &str) -> bool {
        std::fs::remove_file(self.directory.join(name)).is_ok()
    }

    /// The window's answer to `ask`, journaled under `tag`.
    fn ask(&self, op: Op, tag: &str, ask: Ask) -> Result<Answer, Failure> {
        self.note(&format!("ask {tag} {:?}", ask.pin));
        self.replies
            .send(Reply::Ask { op, ask })
            .map_err(|_| failure("the window is gone"))?;
        loop {
            match self.answers.recv() {
                Ok((o, answer)) if o == op => return Ok(answer),
                Ok(_) => {}
                Err(_) => return Err(failure("the window is gone")),
            }
        }
    }

    /// The key's presence and then its PIN, as td-secret asks them for
    /// `operation`; `tag` names the asks in the journal.
    fn authorize(&self, op: Op, tag: &str, operation: &'static str) -> Result<(), Failure> {
        let mut ask = Ask {
            operation,
            role: Role::Primary,
            key: Some(FINGERPRINT.to_owned()),
            pin: None,
        };
        let Answer::Proceed = self.ask(op, tag, ask.clone())? else {
            return Err(cancelled());
        };
        ask.pin = Some(PinUse::Authorize);
        match self.ask(op, tag, ask)? {
            Answer::Pin(pin) if pin.as_slice() == b"1234" => Ok(()),
            Answer::Pin(_) => Err(failure("the PIN is wrong")),
            _ => Err(cancelled()),
        }
    }

    /// Saves `change` against the revision it names: the case's controls,
    /// then the stale check td-secret makes before any token, then the
    /// key's authorization: the entry and its new revision, `None` once
    /// deleted.
    fn apply(
        &self,
        op: Op,
        change: Change,
        entries: &mut Vec<Entry>,
        next: &mut u8,
    ) -> Result<(EntryId, Option<u64>), Failure> {
        let (id, base) = match &change {
            Change::Create { title, body } => {
                self.note(&format!("create {:?} {:?}", title.as_str(), body.as_str()));
                (None, 0)
            }
            Change::Edit {
                id,
                base,
                title,
                body,
            } => {
                self.note(&format!(
                    "edit {} {base} {:?} {:?}",
                    number(id),
                    title.as_str(),
                    body.as_str()
                ));
                (Some(*id), *base)
            }
            Change::Rename { id, base, title } => {
                self.note(&format!(
                    "rename {} {base} {:?}",
                    number(id),
                    title.as_str()
                ));
                (Some(*id), *base)
            }
            Change::Delete { id, base } => {
                self.note(&format!("delete {} {base}", number(id)));
                (Some(*id), *base)
            }
        };
        if self.take("fail") {
            return Err(failure("the test vault refused this save"));
        }
        if self.take("elsewhere") {
            if let Some(entry) = entries.iter_mut().find(|entry| Some(entry.id) == id) {
                entry.revision = next_revision(entry.revision)?;
                self.note(&format!("saved elsewhere {}", entry.revision));
            }
        }
        let position = id.map(|id| entries.iter().position(|entry| entry.id == id));
        if let Some(found) = position {
            let entry = found
                .and_then(|at| entries.get(at))
                .ok_or_else(|| failure("the entry is gone"))?;
            if entry.revision != base {
                return Err(Failure {
                    text: "the entry changed since it was opened; nothing was saved".to_owned(),
                    stale: true,
                    uncertain: false,
                    cancelled: false,
                });
            }
        }
        self.authorize(op, "save", SAVE)?;
        if self.take("hold") {
            self.note("held");
            self.hold(op)?;
        }
        match (change, position.flatten()) {
            (Change::Create { title, body }, _) => {
                let id = [*next; 16];
                *next = next
                    .checked_add(1)
                    .ok_or_else(|| failure("the test vault is full"))?;
                entries.push(Entry {
                    id,
                    revision: 1,
                    title: title.take(),
                    body: body.take().into_bytes(),
                });
                Ok((id, Some(1)))
            }
            (Change::Edit { title, body, .. }, Some(at)) => {
                let entry = entries
                    .get_mut(at)
                    .ok_or_else(|| failure("the entry is gone"))?;
                entry.title = title.take();
                entry.body = body.take().into_bytes();
                entry.revision = next_revision(entry.revision)?;
                Ok((entry.id, Some(entry.revision)))
            }
            (Change::Rename { title, .. }, Some(at)) => {
                let entry = entries
                    .get_mut(at)
                    .ok_or_else(|| failure("the entry is gone"))?;
                entry.title = title.take();
                entry.revision = next_revision(entry.revision)?;
                Ok((entry.id, Some(entry.revision)))
            }
            (Change::Delete { id, .. }, Some(_)) => {
                entries.retain(|entry| entry.id != id);
                Ok((id, None))
            }
            _ => Err(failure("the entry is gone")),
        }
    }

    /// Keeps the save in flight until the window cancels it, which
    /// declines its operation, or the hold runs out.
    fn hold(&self, op: Op) -> Result<(), Failure> {
        let deadline = Instant::now() + HOLD;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| failure("the hold ran out"))?;
            match self.answers.recv_timeout(left) {
                Ok((o, Answer::Decline)) if o == op => return Err(cancelled()),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return Err(failure("the hold ran out")),
                Err(RecvTimeoutError::Disconnected) => return Err(cancelled()),
            }
        }
    }
}

const FINGERPRINT: &str = "0a0b0c0d";

/// The storage swap device the `swap` control reports.
const SWAP: &str = "/dev/test-swap";

fn keys() -> Vec<KeyLabel> {
    vec![KeyLabel {
        role: Role::Primary,
        fingerprint: FINGERPRINT.to_owned(),
    }]
}

fn entry(id: u8, title: &str, body: &str) -> Entry {
    Entry {
        id: [id; 16],
        revision: 1,
        title: title.to_owned(),
        body: body.as_bytes().to_vec(),
    }
}

fn item(entry: &Entry) -> Item {
    Item {
        id: entry.id,
        revision: entry.revision,
        title: Text::new(entry.title.clone()),
    }
}

/// An entry's number in the journal: the byte its id repeats.
fn number(id: &EntryId) -> u8 {
    id.first().copied().unwrap_or_default()
}

fn next_revision(revision: u64) -> Result<u64, Failure> {
    revision
        .checked_add(1)
        .ok_or_else(|| failure("the revision cannot advance"))
}

fn failure(text: &str) -> Failure {
    Failure {
        text: text.to_owned(),
        stale: false,
        uncertain: false,
        cancelled: false,
    }
}

fn cancelled() -> Failure {
    Failure {
        text: "cancelled".to_owned(),
        stale: false,
        uncertain: false,
        cancelled: true,
    }
}
