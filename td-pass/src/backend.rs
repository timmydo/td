//! The vault's thread: it alone holds td-secret's standalone `Host`, the
//! unlocked `Vault` and the enrolled keys' credentials, and serves the
//! window's commands in order. `Host` here (td-secret's `HostEvents`)
//! watches the host's lock and sleep from a thread of its own and keeps
//! the sleep delays for the window, which holds no vault. A token operation blocks this thread, not
//! the window; its prompts travel to the window and wait for the answer
//! with the operation's number. The window abandons an operation through
//! `Client::cancel`, which cancels its token session and declines the
//! prompt it may be waiting on.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use td_secret::pass;

use crate::plain::{Bytes, Text};
use crate::protocol::{
    Answer, Ask, Change, Command, Failure, HostEvent, Item, KeyLabel, Keys, Op, PinUse, Reply, Role,
};

#[cfg(feature = "test-vault")]
mod fixture;

// The window offers no copy larger than td-secret reads.
const _: () = assert!(crate::protocol::MAX_COPY == pass::MAX_COPY);

struct Job {
    command: Command,
    cancel: pass::Cancel,
}

/// The window's end of the vault's thread. Dropping it ends the thread,
/// and the vault with it.
pub struct Client {
    jobs: Sender<Job>,
    answers: Sender<(Op, Answer)>,
    replies: Receiver<Reply>,
    current: Option<(Op, pass::Cancel)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub fn start() -> Result<Client, String> {
    let (jobs, job_rx) = mpsc::channel();
    let (answers, answer_rx) = mpsc::channel();
    let (reply_tx, replies) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("vault".to_owned())
        .spawn(move || vault(&job_rx, &answer_rx, &reply_tx))
        .map_err(|error| format!("td-pass cannot start its vault thread: {error}"))?;
    Ok(Client {
        jobs,
        answers,
        replies,
        current: None,
        thread: Some(thread),
    })
}

/// The keys td-secret creates a notebook with, as the person chose.
fn creation(backup: bool) -> pass::Creation {
    if backup {
        pass::Creation::PrimaryAndBackup
    } else {
        pass::Creation::PrimaryOnly
    }
}

impl Client {
    pub fn send(&mut self, command: Command) {
        let cancel = pass::Cancel::new();
        if let Command::Create { op, .. }
        | Command::Unlock { op, .. }
        | Command::Apply { op, .. }
        | Command::AddKey { op }
        | Command::ReplaceKeys { op, .. }
        | Command::Export { op, .. }
        | Command::ReadCopy { op, .. }
        | Command::Import { op, .. } = command
        {
            self.current = Some((op, cancel.clone()));
        }
        // A thread that is gone answers nothing; the window stays on what
        // it shows.
        let _ = self.jobs.send(Job { command, cancel });
    }

    pub fn answer(&self, op: Op, answer: Answer) {
        let _ = self.answers.send((op, answer));
    }

    /// Abandons the operation in flight, if any.
    pub fn cancel(&mut self) {
        if let Some((op, cancel)) = self.current.take() {
            cancel.cancel();
            self.answer(op, Answer::Decline);
        }
    }

    pub fn try_recv(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }
}

/// The host's lock and sleep, as the window sees them. Watching starts
/// on a thread of its own, so a slow system bus never holds the window;
/// the delays sleep waits on are kept here until the window has locked.
pub struct Host {
    starting: Option<Receiver<Result<pass::HostEvents, String>>>,
    events: Option<pass::HostEvents>,
    /// Each delay and when it came: none is held past `DELAY_HOLD`, so a
    /// vault thread that never answers holds no later sleep.
    delays: Vec<(Instant, pass::SleepDelay)>,
    /// Told once: sleep went on without a delay.
    undelayed: bool,
    note: Option<HostEvent>,
}

/// Longer than logind lets a delay hold sleep by default (five
/// seconds); a host that allows more gets thirty.
const DELAY_HOLD: Duration = Duration::from_secs(30);

pub fn watch_host() -> Host {
    let (started, starting) = mpsc::channel();
    // A thread that cannot start drops its sender, which reads as
    // unwatched on the first poll.
    let _ = std::thread::Builder::new()
        .name("host-watch".to_owned())
        .spawn(move || {
            let _ = started.send(watch());
        });
    Host {
        starting: Some(starting),
        events: None,
        delays: Vec::new(),
        undelayed: false,
        note: None,
    }
}

impl Host {
    /// The next thing the host did, without waiting.
    pub fn try_next(&mut self) -> Option<HostEvent> {
        self.delays.retain(|(at, _)| at.elapsed() < DELAY_HOLD);
        if let Some(note) = self.note.take() {
            return Some(note);
        }
        if let Some(starting) = &self.starting {
            let started = match starting.try_recv() {
                Ok(started) => started,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    Err("td-pass could not start watching them".to_owned())
                }
            };
            self.starting = None;
            match started {
                Ok(events) => {
                    self.undelayed = !events.delays_sleep();
                    self.events = Some(events);
                    return Some(if self.undelayed {
                        HostEvent::Undelayed
                    } else {
                        HostEvent::Watched
                    });
                }
                Err(reason) => return Some(HostEvent::Unwatched(reason)),
            }
        }
        match self.events.as_mut()?.try_next()? {
            pass::HostEvent::Lock => Some(HostEvent::Lock),
            pass::HostEvent::Suspend(delay) => {
                if !delay.held() && !self.undelayed {
                    self.undelayed = true;
                    self.note = Some(HostEvent::Undelayed);
                }
                self.delays.push((Instant::now(), delay));
                Some(HostEvent::Suspend)
            }
            pass::HostEvent::Lost(reason) => {
                self.events = None;
                Some(HostEvent::Lost(reason))
            }
        }
    }

    /// The window has locked: sleep may go on.
    pub fn release(&mut self) {
        self.delays.clear();
    }
}

/// Closing the window ends the thread: the operation in flight is
/// cancelled, the channels close, and the thread drops the vault and the
/// host, whose token worker it stops, before the process exits.
impl Drop for Client {
    fn drop(&mut self) {
        self.cancel();
        let (jobs, _) = mpsc::channel();
        let (answers, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.jobs, jobs));
        drop(std::mem::replace(&mut self.answers, answers));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "test-vault")]
use fixture::serve as vault;
#[cfg(not(feature = "test-vault"))]
use serve as vault;

#[cfg(not(feature = "test-vault"))]
fn watch() -> Result<pass::HostEvents, String> {
    pass::HostEvents::watch()
}
#[cfg(feature = "test-vault")]
use fixture::watch;

// The test vault serves in its place; kept compiled, with all it uses,
// so its build checks the same thread.
#[cfg_attr(feature = "test-vault", allow(dead_code))]
fn serve(jobs: &Receiver<Job>, answers: &Receiver<(Op, Answer)>, replies: &Sender<Reply>) {
    let mut host: Option<pass::Host> = None;
    let mut vault: Option<pass::Vault> = None;
    // The keys the locked view lists, and the unlocked notebook's: the
    // window names either by its place in the list it was given.
    let mut keys: Vec<pass::Key> = Vec::new();
    let mut enrolled: Vec<pass::Key> = Vec::new();
    // A copy read for import, and the keys it opens with.
    let mut copy: Option<(Vec<u8>, Vec<pass::Key>)> = None;
    // The swap the window last asked about, accepted only by `AcceptSwap`.
    let mut risk: Option<pass::SwapRisk> = None;
    while let Ok(Job { command, cancel }) = jobs.recv() {
        let reply = match command {
            Command::Open => open(None, &mut host, &mut keys, &mut risk),
            Command::AcceptSwap => {
                let accepted = risk.take();
                open(accepted, &mut host, &mut keys, &mut risk)
            }
            Command::Create { op, backup } => {
                let mut asker = Asker {
                    op,
                    replies,
                    answers,
                };
                match host.as_mut() {
                    Some(host) => match host.create(creation(backup), &mut asker, &cancel) {
                        Ok(created) => {
                            let entries = items(&created);
                            let keys = listing(&created, &mut enrolled);
                            vault = Some(created);
                            Reply::Unlocked { op, entries, keys }
                        }
                        Err(failure) => failed(op, &failure),
                    },
                    None => closed(op),
                }
            }
            Command::Unlock { op, key } => {
                let mut asker = Asker {
                    op,
                    replies,
                    answers,
                };
                match (host.as_mut(), keys.get(key)) {
                    (Some(host), Some(key)) => match host.unlock(key, &mut asker, &cancel) {
                        Ok(unlocked) => {
                            let entries = items(&unlocked);
                            let keys = listing(&unlocked, &mut enrolled);
                            vault = Some(unlocked);
                            Reply::Unlocked { op, entries, keys }
                        }
                        Err(failure) => failed(op, &failure),
                    },
                    _ => closed(op),
                }
            }
            Command::Read { id } => match vault.as_ref().and_then(|vault| vault.entry(&id)) {
                Some(entry) => Reply::Entry {
                    id,
                    revision: entry.revision,
                    title: Text::new(entry.title.to_owned()),
                    body: Bytes::copy(entry.body),
                },
                None => Reply::Missing { id },
            },
            // A save asks no token: the unlock authorized the session.
            Command::Apply { op, change } => match (host.as_mut(), vault.as_mut()) {
                (Some(host), Some(vault)) => match host.apply(vault, convert(change), &cancel) {
                    Ok(committed) => Reply::Committed {
                        op,
                        id: committed.id,
                        revision: committed.revision,
                    },
                    Err(failure) => failed(op, &failure),
                },
                _ => closed(op),
            },
            Command::AddKey { op } => {
                let mut asker = Asker {
                    op,
                    replies,
                    answers,
                };
                match (host.as_mut(), vault.as_mut()) {
                    (Some(host), Some(vault)) => match host.add_key(vault, &mut asker, &cancel) {
                        Ok(()) => Reply::Keys {
                            op,
                            keys: listing(vault, &mut enrolled),
                        },
                        Err(failure) => failed(op, &failure),
                    },
                    _ => closed(op),
                }
            }
            Command::ReplaceKeys { op, revoked } => {
                let mut asker = Asker {
                    op,
                    replies,
                    answers,
                };
                let revoked: Option<Vec<pass::Key>> = revoked
                    .iter()
                    .map(|&index| enrolled.get(index).cloned())
                    .collect();
                match (host.as_mut(), vault.as_mut(), revoked) {
                    (Some(host), Some(vault), Some(revoked)) => {
                        match host.replace_keys(vault, &revoked, &mut asker, &cancel) {
                            Ok(()) => Reply::Keys {
                                op,
                                keys: listing(vault, &mut enrolled),
                            },
                            Err(failure) => failed(op, &failure),
                        }
                    }
                    (Some(_), Some(_), None) => refused(op, "that key is no longer listed"),
                    _ => closed(op),
                }
            }
            Command::Export { op, folder } => match vault.as_ref() {
                Some(vault) => match vault.export() {
                    Ok(bytes) => {
                        let name = format!("td-pass-notebook-r{}.tdpass", vault.revision());
                        match crate::files::write_copy(&folder, &name, &bytes) {
                            Ok(path) => Reply::Exported {
                                op,
                                path: path.display().to_string(),
                            },
                            Err(text) => refused(op, &text),
                        }
                    }
                    Err(failure) => failed(op, &failure),
                },
                None => closed(op),
            },
            Command::ReadCopy { op, path } => {
                copy = None;
                match crate::files::read_copy(&path, pass::MAX_COPY) {
                    Ok(bytes) => match pass::keys_of(&bytes) {
                        Ok(opens) => {
                            let keys = opens.iter().map(label).collect();
                            copy = Some((bytes, opens));
                            Reply::Copy { op, keys }
                        }
                        Err(failure) => failed(op, &failure),
                    },
                    Err(text) => refused(op, &text),
                }
            }
            Command::Import { op, key } => {
                let mut asker = Asker {
                    op,
                    replies,
                    answers,
                };
                let chosen = copy.as_ref().map(|(bytes, opens)| (bytes, opens.get(key)));
                match (host.as_mut(), chosen) {
                    (Some(host), Some((bytes, Some(key)))) => {
                        match host.import(bytes, key, &mut asker, &cancel) {
                            Ok(imported) => {
                                let entries = items(&imported);
                                let keys = listing(&imported, &mut enrolled);
                                vault = Some(imported);
                                copy = None;
                                Reply::Unlocked { op, entries, keys }
                            }
                            Err(failure) => failed(op, &failure),
                        }
                    }
                    (Some(_), Some((_, None))) => refused(op, "that key is no longer listed"),
                    (Some(_), None) => refused(op, "no copy is read for import"),
                    (None, _) => closed(op),
                }
            }
            Command::Lock => {
                vault = None;
                enrolled.clear();
                copy = None;
                match host.as_ref().map(pass::Host::keys) {
                    Some(Ok(listed)) => Reply::Locked {
                        keys: labels(&mut keys, listed),
                    },
                    Some(Err(failure)) => Reply::Refused {
                        text: failure.to_string(),
                    },
                    None => Reply::Locked { keys: None },
                }
            }
        };
        // An answer that arrived after its operation ended is dropped
        // now, clearing any PIN it carried, rather than at the next prompt.
        while answers.try_recv().is_ok() {}
        if replies.send(reply).is_err() {
            break;
        }
    }
}

/// Opens the host, with the swap the person accepted, and lists its keys;
/// swap not yet accepted is kept here and named to the window.
fn open(
    accepted: Option<pass::SwapRisk>,
    host: &mut Option<pass::Host>,
    keys: &mut Vec<pass::Key>,
    risk: &mut Option<pass::SwapRisk>,
) -> Reply {
    let opened = pass::Host::open(accepted).and_then(|opening| match opening {
        pass::Opening::Opened(opened) => {
            let listed = opened.keys()?;
            Ok(Ok((opened, listed)))
        }
        pass::Opening::Swap(swap) => Ok(Err(swap)),
    });
    match opened {
        Ok(Ok((opened, listed))) => {
            *risk = None;
            *host = Some(opened);
            Reply::Opened {
                keys: labels(keys, listed),
            }
        }
        Ok(Err(swap)) => {
            let devices = swap.devices().to_vec();
            *risk = Some(swap);
            Reply::Swap { devices }
        }
        Err(failure) => Reply::Refused {
            text: failure.to_string(),
        },
    }
}

/// Keeps the credentials here and gives the window their labels.
fn labels(keys: &mut Vec<pass::Key>, listed: Option<Vec<pass::Key>>) -> Option<Vec<KeyLabel>> {
    *keys = listed?;
    Some(keys.iter().map(label).collect())
}

/// Keeps the unlocked notebook's credentials here and gives the window
/// their labels, with the one that authorizes adding a key.
fn listing(vault: &pass::Vault, enrolled: &mut Vec<pass::Key>) -> Keys {
    *enrolled = vault.keys();
    let using = vault.key();
    Keys {
        labels: enrolled.iter().map(label).collect(),
        using: enrolled.iter().position(|key| Some(key) == using.as_ref()),
    }
}

fn label(key: &pass::Key) -> KeyLabel {
    KeyLabel {
        role: role(key.role()),
        fingerprint: key.fingerprint().to_string(),
    }
}

fn role(role: pass::KeyRole) -> Role {
    match role {
        pass::KeyRole::Primary => Role::Primary,
        pass::KeyRole::Backup => Role::Backup,
    }
}

fn items(vault: &pass::Vault) -> Vec<Item> {
    vault
        .entries()
        .map(|summary| Item {
            id: summary.id,
            revision: summary.revision,
            title: Text::new(summary.title.to_owned()),
        })
        .collect()
}

fn convert(change: Change) -> pass::Change {
    match change {
        Change::Create { title, body } => pass::Change::Create {
            title: title.take(),
            body: body.take(),
        },
        Change::Edit {
            id,
            base,
            title,
            body,
        } => pass::Change::Edit {
            id,
            base,
            title: title.take(),
            body: body.take(),
        },
        Change::Rename { id, base, title } => pass::Change::Rename {
            id,
            base,
            title: title.take(),
        },
        Change::Delete { id, base } => pass::Change::Delete { id, base },
    }
}

fn failed(op: Op, failure: &pass::Failure) -> Reply {
    Reply::Failed {
        op,
        failure: Failure {
            text: failure.to_string(),
            stale: failure.stale(),
            uncertain: failure.uncertain(),
            cancelled: failure.cancelled(),
        },
    }
}

/// An operation that needs a vault this thread does not hold.
fn closed(op: Op) -> Reply {
    refused(op, "the notebook is not open")
}

fn refused(op: Op, text: &str) -> Reply {
    Reply::Failed {
        op,
        failure: Failure {
            text: text.to_owned(),
            stale: false,
            uncertain: false,
            cancelled: false,
        },
    }
}

/// The host authentication prompt over the window: each request goes to
/// the window, and the answer for this operation comes back; an answer
/// to an abandoned operation is dropped, clearing any PIN it carried.
struct Asker<'a> {
    op: Op,
    replies: &'a Sender<Reply>,
    answers: &'a Receiver<(Op, Answer)>,
}

impl Asker<'_> {
    fn ask(&mut self, request: pass::Request, pin: Option<PinUse>) -> Result<Answer, String> {
        let gone = || "the window is gone".to_owned();
        let ask = Ask {
            operation: request.operation,
            role: role(request.role),
            key: request.key.map(|key| key.to_string()),
            pin,
        };
        self.replies
            .send(Reply::Ask { op: self.op, ask })
            .map_err(|_| gone())?;
        loop {
            let (op, answer) = self.answers.recv().map_err(|_| gone())?;
            if op == self.op {
                return Ok(answer);
            }
        }
    }
}

impl pass::Prompt for Asker<'_> {
    fn present(&mut self, request: pass::Request) -> Result<(), String> {
        match self.ask(request, None)? {
            Answer::Proceed => Ok(()),
            _ => Err("declined".to_owned()),
        }
    }

    fn pin(&mut self, request: pass::Request, purpose: pass::PinUse) -> Result<Box<[u8]>, String> {
        let purpose = match purpose {
            pass::PinUse::Authorize => PinUse::Authorize,
            pass::PinUse::Enroll => PinUse::Enroll,
            pass::PinUse::Proof => PinUse::Proof,
        };
        match self.ask(request, Some(purpose))? {
            Answer::Pin(pin) => Ok(pin.take()),
            _ => Err("declined".to_owned()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use pass::Prompt;

    #[test]
    fn a_backup_is_enrolled_only_when_asked_for() {
        assert_eq!(creation(true), pass::Creation::PrimaryAndBackup);
        assert_eq!(creation(false), pass::Creation::PrimaryOnly);
    }

    fn request() -> pass::Request {
        pass::Request {
            operation: "unlock",
            role: pass::KeyRole::Primary,
            key: None,
        }
    }

    #[test]
    fn a_prompt_takes_only_its_own_operations_answer() {
        let (reply_tx, reply_rx) = mpsc::channel();
        let (answer_tx, answer_rx) = mpsc::channel();
        // A late PIN for an ended operation, then this one's answers.
        answer_tx
            .send((1, Answer::Pin(Bytes::copy(b"9999"))))
            .unwrap();
        answer_tx
            .send((2, Answer::Pin(Bytes::copy(b"1234"))))
            .unwrap();
        answer_tx.send((2, Answer::Decline)).unwrap();
        let mut asker = Asker {
            op: 2,
            replies: &reply_tx,
            answers: &answer_rx,
        };
        let pin = asker.pin(request(), pass::PinUse::Authorize).unwrap();
        assert_eq!(&*pin, b"1234");
        assert!(matches!(
            reply_rx.try_recv(),
            Ok(Reply::Ask {
                op: 2,
                ask: Ask {
                    pin: Some(PinUse::Authorize),
                    ..
                }
            })
        ));
        assert!(asker.present(request()).is_err());
        drop(answer_tx);
        assert!(asker.present(request()).is_err());
    }

    #[test]
    fn answers_left_after_a_job_are_dropped_with_it() {
        let (jobs, job_rx) = mpsc::channel();
        let (answers, answer_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        answers
            .send((3, Answer::Pin(Bytes::copy(b"1234"))))
            .unwrap();
        jobs.send(Job {
            command: Command::Read { id: [0; 16] },
            cancel: pass::Cancel::new(),
        })
        .unwrap();
        drop(jobs);
        serve(&job_rx, &answer_rx, &reply_tx);
        assert!(matches!(replies.try_recv(), Ok(Reply::Missing { .. })));
        assert!(answer_rx.try_recv().is_err());
    }

    #[test]
    fn key_commands_without_an_open_notebook_are_refused() {
        let (jobs, job_rx) = mpsc::channel();
        let (_answers, answer_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        for command in [
            Command::AddKey { op: 1 },
            Command::ReplaceKeys {
                op: 2,
                revoked: vec![0],
            },
        ] {
            jobs.send(Job {
                command,
                cancel: pass::Cancel::new(),
            })
            .unwrap();
        }
        drop(jobs);
        serve(&job_rx, &answer_rx, &reply_tx);
        for expected in 1..=2 {
            match replies.try_recv() {
                Ok(Reply::Failed { op, failure }) => {
                    assert_eq!(op, expected);
                    assert_eq!(failure.text, "the notebook is not open");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn copies_that_cannot_be_read_and_writes_without_a_notebook_are_refused() {
        let (jobs, job_rx) = mpsc::channel();
        let (_answers, answer_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        // A regular file that is no vault, and one that is not there.
        let garbage = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        for command in [
            Command::ReadCopy {
                op: 1,
                path: std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/absent")),
            },
            Command::ReadCopy {
                op: 2,
                path: garbage.clone(),
            },
            Command::Export {
                op: 3,
                folder: std::env::temp_dir(),
            },
            Command::Import { op: 4, key: 0 },
        ] {
            jobs.send(Job {
                command,
                cancel: pass::Cancel::new(),
            })
            .unwrap();
        }
        drop(jobs);
        serve(&job_rx, &answer_rx, &reply_tx);
        for expected in 1..=4 {
            match replies.try_recv() {
                Ok(Reply::Failed { op, failure }) => {
                    assert_eq!(op, expected);
                    match expected {
                        1 => assert!(failure.text.contains("absent"), "{}", failure.text),
                        // Read whole, then refused as no copy.
                        2 => assert!(!failure.text.contains("Cargo.toml"), "{}", failure.text),
                        _ => assert_eq!(failure.text, "the notebook is not open"),
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn cancel_declines_the_operation_in_flight_once() {
        let (jobs, job_rx) = mpsc::channel();
        let (answers, answer_rx) = mpsc::channel();
        let (_reply_tx, replies) = mpsc::channel();
        let mut client = Client {
            jobs,
            answers,
            replies,
            current: None,
            thread: None,
        };
        client.send(Command::Read { id: [0; 16] });
        client.cancel();
        assert!(answer_rx.try_recv().is_err());
        client.send(Command::Unlock { op: 7, key: 0 });
        client.cancel();
        client.cancel();
        assert!(matches!(answer_rx.try_recv(), Ok((7, Answer::Decline))));
        assert!(answer_rx.try_recv().is_err());
        assert_eq!(job_rx.try_iter().count(), 2);
    }
}
