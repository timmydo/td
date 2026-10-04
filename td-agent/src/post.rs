//! Messages between conversations, the window process's side (DESIGN.md
//! §3, §6): it routes them. A conversation's `send_message`, which the
//! human allowed on a card in its own process, reaches the window as a
//! `Send`, is checked again here (both ends exist, the sender is not the
//! receiver, the bounds) and queued in the state
//! directory's `outbox`, one file per message under its receiver, written
//! whole before the sender hears it was queued. Each poll hands what is
//! queued to its receiver's process, starting one in the background for a
//! conversation that has none, and the file goes when the receiver
//! acknowledges the delivery id, which it logs once. So a restart of
//! either process, or of the window, neither loses nor repeats a message.
//!
//! The window also answers a conversation's `Query` for the states it
//! knows (`conversations`).

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use crate::protocol::{Down, Up};
use crate::store::{self, Id, Role, StateDir};
use crate::supervisor::{Supervisor, Update};
use crate::tools::{self, Op};
use td_json::Json;

/// The longest message file read back: a message at its longest, escaped.
const MAX_FILE: u64 = 256 * 1024;
/// The most conversations a `States` answer names.
const MAX_STATES: usize = 1000;

/// A message queued for its receiver. Its `role` and `status` are a
/// sender's from before there were only conversations, which an outbox
/// may still hold: the orchestrator, or a report's status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Queued {
    pub to: Id,
    pub delivery: String,
    pub from: Id,
    pub role: Role,
    pub text: String,
    pub status: Option<String>,
    /// Its place in the outbox's order.
    order: u64,
}

impl Queued {
    fn file(&self) -> String {
        format!("{:020}-{}", self.order, self.delivery)
    }

    fn down(&self) -> Down {
        Down::Message {
            delivery: self.delivery.clone(),
            from: self.from.clone(),
            role: self.role,
            text: self.text.clone(),
            status: self.status.clone(),
        }
    }
}

/// The messages not yet delivered, as the state directory keeps them.
pub struct Outbox {
    dir: PathBuf,
    queued: Vec<Queued>,
    next: u64,
}

impl Outbox {
    /// The outbox of `state`, made when missing, with what could not be
    /// read named.
    pub fn load(state: &StateDir) -> (Self, Vec<String>) {
        let dir = state.root().join("outbox");
        let mut problems = Vec::new();
        if let Err(e) = DirBuilder::new().recursive(true).mode(0o700).create(&dir) {
            problems.push(format!("{}: {e}", dir.display()));
        }
        let mut queued = Vec::new();
        for receiver in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Some(to) = receiver.file_name().to_str().and_then(Id::parse) else {
                continue;
            };
            // Messages a failed removal left for a receiver since deleted
            // (DESIGN.md §4) go now.
            if let Err(e) = std::fs::symlink_metadata(state.conversation(&to)) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    if let Err(e) = std::fs::remove_dir_all(receiver.path()) {
                        problems.push(format!("{}: {e}", receiver.path().display()));
                    }
                    continue;
                }
            }
            for entry in std::fs::read_dir(receiver.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = entry.file_name();
                let Some(name) = name.to_str().filter(|n| !n.starts_with('.')) else {
                    continue;
                };
                match read(&to, name, &entry.path()) {
                    Ok(message) => queued.push(message),
                    Err(e) => problems.push(format!("{}: {e}", entry.path().display())),
                }
            }
        }
        queued.sort_by_key(|q| q.order);
        let next = queued.last().map_or(1, |q| q.order.saturating_add(1));
        (Self { dir, queued, next }, problems)
    }

    /// The messages waiting, oldest first.
    pub fn queued(&self) -> &[Queued] {
        &self.queued
    }

    /// The messages waiting for `to`.
    pub fn count(&self, to: &Id) -> usize {
        self.queued.iter().filter(|q| &q.to == to).count()
    }

    /// Queues a message, written whole before this returns.
    pub fn post(&mut self, to: Id, from: Id, text: String) -> Result<(), String> {
        let message = Queued {
            delivery: store::random_hex(16)?,
            to,
            from,
            role: Role::Conversation,
            text,
            status: None,
            order: self.next,
        };
        let dir = self.dir.join(message.to.as_str());
        // The outbox is synced before each message is written, so a
        // receiver's directory made for it is durable first: `replace`
        // syncs only the directory the message is written in.
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)
            .and_then(|()| std::fs::File::open(&self.dir)?.sync_all())
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut pairs = vec![
            ("from".into(), Json::Str(message.from.to_string())),
            ("role".into(), Json::Str(message.role.word().into())),
            ("text".into(), Json::Str(message.text.clone())),
        ];
        if let Some(status) = &message.status {
            pairs.push(("status".into(), Json::Str(status.clone())));
        }
        store::replace(
            &dir,
            &message.file(),
            Json::Obj(pairs).to_string().as_bytes(),
        )?;
        self.next = self.next.saturating_add(1);
        self.queued.push(message);
        Ok(())
    }

    /// Conversation `to` is deleted: what was queued for it goes, from
    /// memory and from the state directory.
    pub fn forget(&mut self, to: &Id) -> Result<(), String> {
        self.queued.retain(|q| &q.to != to);
        let dir = self.dir.join(to.as_str());
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", dir.display())),
        }
    }

    /// `to` logged delivery `delivery`, or refused it: it is queued no
    /// more. A delivery id that is not this outbox's (the human's) is
    /// nothing.
    pub fn delivered(&mut self, to: &Id, delivery: &str) -> Result<(), String> {
        let Some(at) = self
            .queued
            .iter()
            .position(|q| &q.to == to && q.delivery == delivery)
        else {
            return Ok(());
        };
        let message = self.queued.remove(at);
        let path = self.dir.join(to.as_str()).join(message.file());
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// A queued message's file read back.
fn read(to: &Id, name: &str, path: &std::path::Path) -> Result<Queued, String> {
    let (order, delivery) = name.split_once('-').ok_or("not a message's name")?;
    let order = order.parse().map_err(|_| "not a message's name")?;
    if !crate::protocol::delivery_ok(delivery) {
        return Err("not a message's name".into());
    }
    let bytes = store::read_bounded(path, MAX_FILE).map_err(|e| e.to_string())?;
    let value = td_json::parse_slice(&bytes).map_err(|e| e.to_string())?;
    let text = |name: &str| value.get(name).and_then(Json::as_str);
    Ok(Queued {
        to: to.clone(),
        delivery: delivery.to_string(),
        from: text("from").and_then(Id::parse).ok_or("no sender")?,
        role: text("role").and_then(Role::parse).ok_or("no role")?,
        text: text("text")
            .filter(|t| t.len() <= tools::MAX_MESSAGE)
            .ok_or("no text, or one past a message's bound")?
            .to_string(),
        status: text("status").map(str::to_string),
        order,
    })
}

/// A conversation as the window knows it, for routing and `conversations`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub id: Id,
    /// Its state as `conversations` names it.
    pub state: String,
    /// Its process failed: nothing wakes it until the human opens it.
    pub failed: bool,
    /// The human archived it: it takes no message, and what is queued
    /// for it waits until it is unarchived.
    pub archived: bool,
}

/// Whether a message from `from` to `to` may be queued (DESIGN.md §3),
/// checked again in the window whatever the sender checked. The crossing
/// itself is the human's, decided on a card in the sender's process.
fn check(
    outbox: &Outbox,
    directory: &[Entry],
    from: &Id,
    to: &Id,
    text: &str,
) -> Result<(), String> {
    let known = |id: &Id| directory.iter().any(|e| &e.id == id);
    if !known(from) {
        return Err("the sending conversation is unknown to the window".into());
    }
    if !known(to) {
        return Err(format!("there is no conversation {to}"));
    }
    if directory.iter().any(|e| &e.id == to && e.archived) {
        return Err(format!(
            "conversation {to} is archived; it takes messages again once the human unarchives it"
        ));
    }
    tools::crossing(from, to, Op::Message)?;
    if text.trim().is_empty() || text.len() > tools::MAX_MESSAGE {
        return Err(format!(
            "a message is between 1 and {} bytes",
            tools::MAX_MESSAGE
        ));
    }
    if outbox.count(to) >= tools::MAX_UNDELIVERED {
        return Err(format!(
            "conversation {to} already holds {} messages undelivered; it takes them between its turns, so send again later",
            tools::MAX_UNDELIVERED
        ));
    }
    Ok(())
}

/// The window's post office: the outbox and the routing of what it holds.
pub struct Post {
    outbox: Outbox,
    /// Receivers that could not be started for their messages, left until
    /// the human opens them.
    stuck: Vec<Id>,
}

impl Post {
    pub fn new(outbox: Outbox) -> Self {
        Self {
            outbox,
            stuck: Vec::new(),
        }
    }

    /// Conversation `id` is deleted: nothing is queued or stuck for it.
    pub fn forget(&mut self, id: &Id) -> Result<(), String> {
        self.stuck.retain(|stuck| stuck != id);
        self.outbox.forget(id)
    }

    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// What conversation `from`'s process said, as far as it is the
    /// post's: a message to queue and answer, the states asked for, or a
    /// delivery acknowledged or refused. A note for the human, when there
    /// is one to give.
    pub fn hear(
        &mut self,
        from: &Id,
        update: &Update,
        supervisor: &mut Supervisor,
        directory: &[Entry],
    ) -> Option<String> {
        match update {
            Update::Up(Up::Send { id, to, text }) => {
                let queued = check(&self.outbox, directory, from, to, text)
                    .and_then(|()| self.outbox.post(to.clone(), from.clone(), text.clone()));
                supervisor.answer(
                    from,
                    &Down::Sent {
                        id: *id,
                        refusal: queued.err(),
                    },
                );
                None
            }
            Update::Up(Up::Query { id }) => {
                let states = directory
                    .iter()
                    .take(MAX_STATES)
                    .map(|e| (e.id.clone(), e.state.clone()))
                    .collect();
                supervisor.answer(from, &Down::States { id: *id, states });
                None
            }
            Update::Up(Up::Delivered { delivery }) => self.outbox.delivered(from, delivery).err(),
            Update::Undeliverable { delivery, reason } => {
                let refused = format!(
                    "conversation {from} refused a message from another conversation: {reason}"
                );
                Some(match self.outbox.delivered(from, delivery) {
                    Ok(()) => refused,
                    Err(e) => format!("{refused}; removing it from the outbox: {e}"),
                })
            }
            _ => None,
        }
    }

    /// Hands every queued message to its receiver's process, starting one
    /// in the background for a receiver that has none, unless it failed.
    /// Notes for the human, when a process could not be started.
    pub fn deliver(&mut self, supervisor: &mut Supervisor, directory: &[Entry]) -> Vec<String> {
        let mut notes = Vec::new();
        // One the human has opened since is tried again.
        self.stuck.retain(|id| !supervisor.running(id));
        for message in &self.outbox.queued {
            let to = &message.to;
            let known = directory.iter().find(|e| &e.id == to);
            if known.is_some_and(|e| e.failed || e.archived)
                || self.stuck.contains(to)
                || supervisor.holds(to, &message.delivery)
            {
                continue;
            }
            // Said once, and not tried again until the human opens it:
            // a receiver the store no longer lists, or whose process
            // cannot be started, is not started again every poll.
            let woken = match known {
                None => Err("the store does not list it".to_string()),
                Some(_) if supervisor.running(to) => Ok(()),
                Some(_) => supervisor.wake(to),
            };
            if let Err(e) = woken {
                notes.push(format!(
                    "conversation {to} could not be started for a message, which stays queued: {e}"
                ));
                self.stuck.push(to.clone());
                continue;
            }
            supervisor.deliver(to, message.delivery.clone(), message.down());
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::store::tests::Scratch;

    fn id(n: u8) -> Id {
        Id::parse(&format!("{n:032x}")).unwrap()
    }

    /// The store holds these conversations, so the outbox keeps their
    /// messages across a load.
    fn receivers(state: &StateDir, ns: &[u8]) {
        for &n in ns {
            std::fs::create_dir_all(state.conversation(&id(n))).unwrap();
        }
    }

    fn entry(n: u8) -> Entry {
        Entry {
            id: id(n),
            state: "idle".into(),
            failed: false,
            archived: false,
        }
    }

    #[test]
    fn the_outbox_keeps_messages_in_order_across_a_restart() {
        let scratch = Scratch::new("outbox");
        let state = scratch.state();
        receivers(&state, &[1, 2]);
        let (mut outbox, problems) = Outbox::load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        outbox.post(id(1), id(2), "first".into()).unwrap();
        outbox
            .post(id(1), id(3), "second \"quoted\"".into())
            .unwrap();
        outbox.post(id(2), id(1), "third".into()).unwrap();
        let (mut again, problems) = Outbox::load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(again.queued(), outbox.queued());
        assert!(again
            .queued()
            .iter()
            .all(|q| q.role == Role::Conversation && q.status.is_none()));
        assert_eq!(again.count(&id(1)), 2);
        let first = again.queued()[0].delivery.clone();
        again.delivered(&id(1), &first).unwrap();
        // Delivered twice, or another's id, is nothing.
        again.delivered(&id(1), &first).unwrap();
        again.delivered(&id(1), "0".repeat(32).as_str()).unwrap();
        let (third, _) = Outbox::load(&state);
        let texts: Vec<&str> = third.queued().iter().map(|q| q.text.as_str()).collect();
        assert_eq!(texts, ["second \"quoted\"", "third"]);
        assert_eq!(third.next, 4, "orders keep rising");
    }

    /// A message an outbox from before peers holds, from the orchestrator
    /// or a report, is read back with its role and status.
    #[test]
    fn an_older_outboxs_report_is_read_back_whole() {
        let scratch = Scratch::new("older");
        let state = scratch.state();
        receivers(&state, &[1]);
        let dir = state.root().join("outbox").join(id(1).as_str());
        std::fs::create_dir_all(&dir).unwrap();
        let file = format!("{:020}-{}", 7, "d".repeat(32));
        let text = format!(
            r#"{{"from":"{}","role":"orchestrator","text":"done here","status":"done"}}"#,
            id(2)
        );
        std::fs::write(dir.join(file), text).unwrap();
        let (outbox, problems) = Outbox::load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        let queued = &outbox.queued()[0];
        assert_eq!(queued.role, Role::Orchestrator);
        assert_eq!(queued.status.as_deref(), Some("done"));
        assert_eq!(outbox.next, 8);
    }

    /// A deleted receiver's messages go, from memory and from disk; the
    /// others' stay.
    #[test]
    fn a_deleted_receiver_is_forgotten() {
        let scratch = Scratch::new("forget");
        let state = scratch.state();
        receivers(&state, &[1, 2]);
        let (outbox, _) = Outbox::load(&state);
        let mut post = Post::new(outbox);
        for (to, text) in [(1, "kept"), (2, "gone"), (2, "gone too")] {
            post.outbox.post(id(to), id(3), text.into()).unwrap();
        }
        post.forget(&id(2)).unwrap();
        assert_eq!(post.outbox().count(&id(2)), 0);
        assert!(!state.root().join("outbox").join(id(2).as_str()).exists());
        let (again, problems) = Outbox::load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        let texts: Vec<&str> = again.queued().iter().map(|q| q.text.as_str()).collect();
        assert_eq!(texts, ["kept"]);
        // Forgetting one with nothing queued is nothing.
        post.forget(&id(4)).unwrap();
        // What a failed removal left for a receiver since deleted goes at
        // the next load.
        post.outbox.post(id(1), id(3), "late".into()).unwrap();
        std::fs::remove_dir_all(state.conversation(&id(1))).unwrap();
        let (again, problems) = Outbox::load(&state);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(again.queued().is_empty());
        assert!(!state.root().join("outbox").join(id(1).as_str()).exists());
    }

    /// What is queued for an archived receiver waits: no process is
    /// started for it, nor said to have failed to start.
    #[test]
    fn nothing_is_delivered_to_an_archived_receiver() {
        let scratch = Scratch::new("archived");
        let state = scratch.state();
        let (mut outbox, _) = Outbox::load(&state);
        outbox.post(id(2), id(1), "hi".into()).unwrap();
        let mut post = Post::new(outbox);
        // A program that cannot start, so a wake would be said.
        let setup = Down::Setup {
            key: Err("no key".into()),
            client: crate::config::Client::default(),
        };
        let mut supervisor = Supervisor::new(
            state.root().join("no-such-program"),
            state.root().to_path_buf(),
            setup,
        );
        let archived = [
            entry(1),
            Entry {
                archived: true,
                ..entry(2)
            },
        ];
        assert!(post.deliver(&mut supervisor, &archived).is_empty());
        assert!(!supervisor.running(&id(2)));
        assert_eq!(post.outbox().count(&id(2)), 1, "it waits");
        // Unarchived, it is woken for it, here in vain.
        let notes = post.deliver(&mut supervisor, &[entry(1), entry(2)]);
        assert!(
            notes.iter().any(|n| n.contains("could not be started")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_message_is_checked_again_by_the_window() {
        let scratch = Scratch::new("check");
        let (mut outbox, _) = Outbox::load(&scratch.state());
        let directory = [entry(1), entry(2), entry(3)];
        let ok =
            |outbox: &Outbox, from: u8, to: u8| check(outbox, &directory, &id(from), &id(to), "hi");
        assert!(ok(&outbox, 1, 2).is_ok());
        assert!(ok(&outbox, 2, 1).is_ok());
        assert!(ok(&outbox, 2, 3).is_ok());
        assert!(ok(&outbox, 2, 2).unwrap_err().contains("itself"));
        assert!(ok(&outbox, 2, 9).unwrap_err().contains("no conversation"));
        assert!(ok(&outbox, 9, 1).unwrap_err().contains("unknown"));
        let long = "x".repeat(tools::MAX_MESSAGE + 1);
        assert!(check(&outbox, &directory, &id(2), &id(1), &long).is_err());
        assert!(check(&outbox, &directory, &id(2), &id(1), " \n").is_err());
        for _ in 0..tools::MAX_UNDELIVERED {
            outbox.post(id(1), id(2), "x".into()).unwrap();
        }
        let full = ok(&outbox, 2, 1).unwrap_err();
        assert!(full.contains("16 messages undelivered"), "{full}");
        // An archived receiver takes none.
        let directory = [
            entry(1),
            Entry {
                archived: true,
                ..entry(2)
            },
        ];
        let refused = check(&outbox, &directory, &id(1), &id(2), "hi").unwrap_err();
        assert!(refused.contains("is archived"), "{refused}");
    }
}
