//! The panel's two threads on two connections to td-audio's socket, as
//! the compositor's volume control has (td-compositor/src/audio.rs): the
//! client blocks, and the window must never wait on it.
//!
//! - `td-mixer-control` takes the panel's actions from a short queue the
//!   window only ever `try_send`s to, applies those waiting in order on a
//!   connection kept between them, and reads one snapshot back for the
//!   window, all under one deadline.
//! - `td-mixer-watch` holds a subscription to outputs, streams and the
//!   server, and on each event asks the control thread to read again, so a
//!   change made elsewhere (a volume key, the CLI) reaches the panel. It
//!   reconnects with backoff while the daemon is away.

use crate::model::{self, Job, Snapshot};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};
use td_pulse::client::{self, Client, Transport};
use td_pulse::proto::subscription;
use td_pulse::socket::UnixSocket;

/// What the daemon is told this client is called.
pub const CLIENT_NAME: &str = "td-mixer";

/// How long one batch of actions and the read after it may take.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// While the daemon was last unavailable, the control thread reads again
/// this often though nothing asked: the watch connection may stand when
/// only the control one failed, or a read overran its deadline.
const RETRY: Duration = Duration::from_secs(2);

/// The watcher's first wait before reconnecting, doubled up to
/// `BACKOFF_MAX` while the daemon stays away and reset once one connects.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Messages waiting for the control thread. A drag sends faster than a
/// round trip; the window keeps the latest action that found the queue
/// full and offers it again.
const QUEUE: usize = 8;

/// What the subscription asks to hear: outputs, streams, the default.
pub const MASK: u32 =
    subscription::MASK_SINK | subscription::MASK_SINK_INPUT | subscription::MASK_SERVER;

/// What the window hears. `answered` counts the actions, oldest first,
/// whose batch this ends, applied or not: the window shows those still
/// unanswered over each snapshot.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// What the daemon says now.
    Snapshot { snapshot: Snapshot, answered: usize },
    /// The daemon refused an action, said in full; the snapshot after it
    /// follows.
    Refused(String),
    /// The daemon is not there, did not answer in time, or refused to say
    /// what it has, which `why` then says.
    Unavailable {
        answered: usize,
        why: Option<String>,
    },
}

#[derive(Debug)]
enum Message {
    Job(Job),
    Changed,
}

pub struct Worker {
    messages: SyncSender<Message>,
    replies: Receiver<Reply>,
}

impl Worker {
    /// Start both threads against `socket`.
    pub fn start(socket: &'static Path) -> io::Result<Self> {
        let (messages, queued) = mpsc::sync_channel(QUEUE);
        let (replies, received) = mpsc::sync_channel(QUEUE);
        spawn("td-mixer-control", move || {
            control_loop(socket, &queued, &replies)
        })?;
        let changed = SyncSender::clone(&messages);
        spawn("td-mixer-watch", move || watch_loop(socket, &changed))?;
        Ok(Self {
            messages,
            replies: received,
        })
    }

    /// Queue `job` without waiting. It comes back when the queue is full,
    /// for the window to offer again; `Err` once the control thread is
    /// gone.
    pub fn send(&self, job: Job) -> Result<Option<Job>, Gone> {
        match self.messages.try_send(Message::Job(job)) {
            Ok(()) => Ok(None),
            Err(TrySendError::Full(Message::Job(job))) => Ok(Some(job)),
            Err(TrySendError::Full(Message::Changed)) => Ok(None),
            Err(TrySendError::Disconnected(_)) => Err(Gone),
        }
    }

    /// A reply that arrived, if any; `Err` once the control thread is gone.
    pub fn try_recv(&self) -> Result<Option<Reply>, Gone> {
        match self.replies.try_recv() {
            Ok(reply) => Ok(Some(reply)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Gone),
        }
    }
}

/// The control thread ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone;

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the panel's audio thread stopped")
    }
}

fn spawn(name: &str, run: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let name = td_alloc::string(name).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    thread::Builder::new().name(name).spawn(run).map(|_| ())
}

/// A connection, authenticated and named within `within`.
fn connect(socket: &Path, within: Duration) -> Option<Client<UnixSocket>> {
    let mut transport = UnixSocket::connect(socket).ok()?;
    transport.deadline_in(within);
    Client::connect(transport, CLIENT_NAME).ok()
}

/// What an action is called when the daemon refuses it.
fn action(job: &Job) -> &'static str {
    match job {
        Job::Choose(_) => "switching output",
        Job::Level(..) => "setting a level",
        Job::Mute(..) => "muting",
    }
}

fn refused(action: &str, code: u32) -> String {
    td_alloc::format(format_args!("Refused {action}: {}", model::refusal(code))).unwrap_or_default()
}

/// The actions of `batch` in order, then one read, on the kept connection
/// or a fresh one from `connect`, each request armed by `arm`; `answered`
/// is how many actions the batch ends, those dropped included. Every
/// action sets a level, a mute or the output outright, so a batch that
/// failed on a kept connection, which a daemon restarted since leaves
/// broken at first use, is run once more on a fresh one and lands the
/// same. A refused action keeps the connection, the rest still run, and
/// each refusal is said before the read; a refused read is the panel's
/// `Unavailable`, saying why.
fn run<T: Transport>(
    connection: &mut Option<Client<T>>,
    mut connect: impl FnMut() -> Option<Client<T>>,
    arm: impl Fn(&mut T),
    batch: &[Job],
    answered: usize,
    report: &mut dyn FnMut(Reply) -> bool,
) -> bool {
    let mut retry = connection.is_some();
    let mut refusals = Vec::new();
    loop {
        if connection.is_none() {
            *connection = connect();
        }
        let Some(client) = connection.as_mut() else {
            return report(Reply::Unavailable {
                answered,
                why: None,
            });
        };
        arm(client.transport_mut());
        refusals.clear();
        let mut failed = false;
        for job in batch {
            match model::apply(client, job) {
                Ok(()) => {}
                Err(model::Error::Daemon(client::Error::Refused(code))) => {
                    // A refusal that cannot be held goes unsaid; the read
                    // still shows what the daemon has.
                    let _ = td_alloc::push(&mut refusals, (action(job), code));
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        match (!failed).then(|| model::read(client)) {
            Some(Ok(snapshot)) => {
                for (action, code) in refusals.iter().copied() {
                    if !report(Reply::Refused(refused(action, code))) {
                        return false;
                    }
                }
                return report(Reply::Snapshot { snapshot, answered });
            }
            Some(Err(model::Error::Daemon(client::Error::Refused(code)))) => {
                return report(Reply::Unavailable {
                    answered,
                    why: Some(refused("listing outputs", code)),
                });
            }
            Some(Err(_)) | None => {}
        }
        *connection = None;
        if retry {
            retry = false;
            continue;
        }
        return report(Reply::Unavailable {
            answered,
            why: None,
        });
    }
}

/// The messages waiting after `first`, its jobs in order, and how many
/// jobs it took, those it could not hold included.
fn batch(first: Message, queued: &Receiver<Message>) -> (Vec<Job>, usize) {
    let mut jobs = td_alloc::with_capacity(QUEUE).unwrap_or_default();
    let mut taken = 0;
    let mut message = Some(first);
    while let Some(next) = message {
        if let Message::Job(job) = next {
            taken += 1;
            // A job that cannot be held is answered unapplied.
            let _ = td_alloc::push(&mut jobs, job);
        }
        message = queued.try_recv().ok();
    }
    (jobs, taken)
}

/// The next message; while the daemon was last unavailable, a read asked
/// for after `retry` without one. `None` once the window is gone.
fn next(queued: &Receiver<Message>, retrying: bool, retry: Duration) -> Option<Message> {
    if !retrying {
        return queued.recv().ok();
    }
    match queued.recv_timeout(retry) {
        Ok(message) => Some(message),
        Err(RecvTimeoutError::Timeout) => Some(Message::Changed),
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

fn control_loop(socket: &Path, queued: &Receiver<Message>, replies: &SyncSender<Reply>) {
    let mut connection = None;
    let mut unavailable = false;
    while let Some(first) = next(queued, unavailable, RETRY) {
        let (jobs, answered) = batch(first, queued);
        // One deadline for the whole batch, retry and reconnect included.
        let now = Instant::now();
        let until = now.checked_add(REQUEST_DEADLINE).unwrap_or(now);
        let left = || until.saturating_duration_since(Instant::now());
        let open = run(
            &mut connection,
            || connect(socket, left()),
            |transport: &mut UnixSocket| transport.deadline_in(left()),
            &jobs,
            answered,
            &mut |reply| {
                match reply {
                    Reply::Unavailable { .. } => unavailable = true,
                    Reply::Snapshot { .. } => unavailable = false,
                    Reply::Refused(_) => {}
                }
                replies.send(reply).is_ok()
            },
        );
        if !open {
            return;
        }
    }
}

/// Subscribe, then `settle` the transport, the connect deadline lifted
/// for the wait on events; ask for a read on subscribing and after each
/// event, until the connection fails (`Err`) or nobody reads (`Ok`).
fn follow<T: Transport>(
    client: &mut Client<T>,
    settle: impl FnOnce(&mut T),
    changed: &mut dyn FnMut() -> bool,
) -> client::Result<(), T::Error> {
    client.subscribe(MASK)?;
    settle(client.transport_mut());
    loop {
        client.take_missed_events();
        if !changed() {
            return Ok(());
        }
        client.next_event()?;
    }
}

/// The wait before reconnecting and the one after it: from the first again
/// once a connection subscribed, else doubled up to the most.
fn backoff(previous: Duration, subscribed: bool) -> (Duration, Duration) {
    let wait = if subscribed { BACKOFF_FIRST } else { previous };
    (wait, wait.saturating_mul(2).min(BACKOFF_MAX))
}

fn watch_loop(socket: &Path, messages: &SyncSender<Message>) {
    // A full queue already holds a message that will read again.
    let changed = || {
        !matches!(
            messages.try_send(Message::Changed),
            Err(TrySendError::Disconnected(_))
        )
    };
    let mut next = BACKOFF_FIRST;
    loop {
        let mut subscribed = false;
        if let Some(mut client) = connect(socket, REQUEST_DEADLINE) {
            let mut nudge = || {
                subscribed = true;
                changed()
            };
            let settle = |transport: &mut UnixSocket| transport.no_deadline();
            if follow(&mut client, settle, &mut nudge).is_ok() {
                return;
            }
        }
        // Gone: the control thread's read will say so.
        if !changed() {
            return;
        }
        let (wait, after) = backoff(next, subscribed);
        thread::sleep(wait);
        next = after;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::model::{AppLevel, Level, Output, Target, MAX_OUTPUTS};
    use td_pulse::client::volume_of;
    use td_pulse::proto::{self, command, error};
    use td_pulse::tag::{self, Reader, Writer};
    use td_pulse::wire::{self, Decoder, Frame};

    /// A server that answers from a script and records what it was sent;
    /// past the script it fails, as a daemon that went away.
    struct Scripted {
        incoming: Vec<u8>,
        at: usize,
        sent: Vec<u8>,
    }

    impl Transport for Scripted {
        type Error = ();
        fn send(&mut self, bytes: &[u8]) -> Result<(), ()> {
            self.sent.extend_from_slice(bytes);
            Ok(())
        }
        fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, ()> {
            let left = &self.incoming[self.at..];
            if left.is_empty() {
                return Err(());
            }
            let count = left.len().min(buffer.len());
            buffer[..count].copy_from_slice(&left[..count]);
            self.at += count;
            Ok(count)
        }
    }

    fn reply(tag: u32, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u32(command::REPLY).u32(tag);
        body(&mut writer);
        wire::control_frame(writer.as_bytes())
    }

    fn refusal(tag: u32, code: u32) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u32(command::ERROR).u32(tag).u32(code);
        wire::control_frame(writer.as_bytes())
    }

    fn event(facility: u32, kind: u32, index: u32) -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .u32(command::SUBSCRIBE_EVENT)
            .u32(u32::MAX)
            .u32(facility | kind)
            .u32(index);
        wire::control_frame(writer.as_bytes())
    }

    fn spec() -> tag::SampleSpec {
        tag::SampleSpec {
            format: tag::SAMPLE_S16LE,
            channels: 2,
            rate: 48_000,
        }
    }

    fn server_info(writer: &mut Writer, default: &str) {
        writer
            .string("pulseaudio")
            .string("16.1")
            .string("tester")
            .string("td")
            .sample_spec(spec())
            .string(default)
            .null_string()
            .u32(0)
            .channel_map(&[1, 2]);
    }

    /// A version-35 sink entry in the shape td-audio writes.
    fn sink_entry(
        writer: &mut Writer,
        index: u32,
        name: &str,
        description: &str,
        at: u32,
        muted: bool,
    ) {
        writer
            .u32(index)
            .string(name)
            .string(description)
            .sample_spec(spec())
            .channel_map(&[1, 2])
            .u32(tag::INVALID_INDEX)
            .cvolume(&[at, at])
            .boolean(muted)
            .u32(tag::INVALID_INDEX)
            .null_string()
            .usec(0)
            .string("td-audio")
            .u32(0)
            .proplist(&[])
            .usec(0)
            .volume(client::VOLUME_NORM)
            .u32(0)
            .u32(0x10001)
            .u32(tag::INVALID_INDEX)
            .u32(0)
            .null_string()
            .u8(1)
            .format_info(1, &[]);
    }

    fn input_entry(writer: &mut Writer, index: u32, client: u32, app: &str, at: u32, muted: bool) {
        writer
            .u32(index)
            .string("playback")
            .u32(tag::INVALID_INDEX)
            .u32(client)
            .u32(0)
            .sample_spec(spec())
            .channel_map(&[1, 2])
            .cvolume(&[at, at])
            .usec(0)
            .usec(0)
            .string("trivial")
            .string("td-audio")
            .boolean(muted)
            .proplist(&[tag::text_property("application.name", app)])
            .boolean(false)
            .boolean(true)
            .boolean(true)
            .format_info(1, &[]);
    }

    /// The three replies one snapshot reads, from `tag`: HDA playing at
    /// `master`, USB beside it, Firefox's one stream at `app`.
    fn snapshot_replies(tag: u32, master: u32, app: u32) -> Vec<Vec<u8>> {
        vec![
            reply(tag, |writer| server_info(writer, "intel-0")),
            reply(tag + 1, |writer| {
                sink_entry(
                    writer,
                    0,
                    "intel-0",
                    "Built-in Audio",
                    volume_of(master),
                    false,
                );
                sink_entry(
                    writer,
                    1,
                    "audio-0",
                    "USB\u{202e} Headset",
                    volume_of(50),
                    true,
                );
            }),
            reply(tag + 2, |writer| {
                input_entry(writer, 4, 9, "Firefox", volume_of(app), false);
            }),
        ]
    }

    fn expected(master: u32, app: u32) -> Snapshot {
        Snapshot {
            outputs: vec![
                Output {
                    name: "intel-0".into(),
                    label: "Built-in Audio".into(),
                    playing: true,
                },
                Output {
                    name: "audio-0".into(),
                    label: "USB Headset".into(),
                    playing: false,
                },
            ],
            master: Some(Level {
                percent: master,
                muted: false,
            }),
            apps: vec![AppLevel {
                name: "Firefox".into(),
                label: "Firefox".into(),
                level: Level {
                    percent: app,
                    muted: false,
                },
            }],
        }
    }

    /// The handshake, then `script`.
    fn connected(script: &[Vec<u8>]) -> Client<Scripted> {
        let mut incoming = reply(0, |writer| {
            writer.u32(proto::VERSION);
        });
        incoming.extend(reply(1, |writer| {
            writer.u32(7);
        }));
        for packet in script {
            incoming.extend_from_slice(packet);
        }
        Client::connect(
            Scripted {
                incoming,
                at: 0,
                sent: Vec::new(),
            },
            CLIENT_NAME,
        )
        .unwrap()
    }

    fn snapshot(snapshot: Snapshot, answered: usize) -> Reply {
        Reply::Snapshot { snapshot, answered }
    }

    /// Each request after the handshake.
    fn packets(client: &mut Client<Scripted>) -> Vec<Vec<u8>> {
        let mut decoder = Decoder::new();
        decoder.push(&client.transport_mut().sent);
        let mut found = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            let Frame::Control(packet) = frame.unwrap() else {
                panic!("the panel sent audio");
            };
            found.push(packet.to_vec());
        }
        found.split_off(2)
    }

    /// Each request after the handshake, by command.
    fn commands(client: &mut Client<Scripted>) -> Vec<u32> {
        let mut decoder = Decoder::new();
        decoder.push(&client.transport_mut().sent);
        let mut found = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            let Frame::Control(packet) = frame.unwrap() else {
                panic!("the panel sent audio");
            };
            found.push(Reader::new(&packet).u32().unwrap());
        }
        found.split_off(2)
    }

    #[test]
    fn a_snapshot_reads_outputs_the_level_and_each_application() {
        let mut client = connected(&snapshot_replies(2, 45, 30));
        assert_eq!(model::read(&mut client).unwrap(), expected(45, 30));
        assert_eq!(
            commands(&mut client),
            [
                command::GET_SERVER_INFO,
                command::GET_SINK_INFO_LIST,
                command::GET_SINK_INPUT_INFO_LIST
            ]
        );
    }

    /// The batch's actions in order, then one read; nothing reconnects.
    #[test]
    fn a_batch_applies_in_order_then_reads_once() {
        let mut script = vec![
            reply(2, |_| {}),
            reply(3, |writer| {
                sink_entry(writer, 1, "audio-0", "USB", volume_of(50), true)
            }),
            reply(4, |_| {}),
        ];
        script.extend(snapshot_replies(5, 70, 30));
        let mut connection = Some(connected(&script));
        let mut replies = Vec::new();
        let open = run(
            &mut connection,
            || panic!("the kept connection served"),
            |_| {},
            &[
                Job::Choose("audio-0".into()),
                Job::Level(Target::Master, 70),
            ],
            2,
            &mut |reply| {
                replies.push(reply);
                true
            },
        );
        assert!(open);
        assert_eq!(replies, [snapshot(expected(70, 30), 2)]);
        // The playing output's level is asked of @DEFAULT_SINK@, whichever
        // output that is when the request lands.
        let asked = packets(connection.as_mut().unwrap())
            .into_iter()
            .find(|packet| Reader::new(packet).u32().unwrap() == command::GET_SINK_INFO)
            .unwrap();
        let mut reader = Reader::new(&asked);
        reader.u32().unwrap();
        reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
        assert_eq!(reader.string().unwrap().as_deref(), Some("@DEFAULT_SINK@"));
        assert_eq!(
            commands(connection.as_mut().unwrap()),
            [
                command::SET_DEFAULT_SINK,
                command::GET_SINK_INFO,
                command::SET_SINK_VOLUME,
                command::GET_SERVER_INFO,
                command::GET_SINK_INFO_LIST,
                command::GET_SINK_INPUT_INFO_LIST
            ]
        );
    }

    /// A refused output is said, the connection kept, and the read after
    /// it still shows the panel what plays.
    #[test]
    fn a_refusal_is_said_and_the_panel_still_reads() {
        let mut script = vec![refusal(2, error::NOTSUPPORTED)];
        script.extend(snapshot_replies(3, 45, 30));
        let mut connection = Some(connected(&script));
        let mut replies = Vec::new();
        run(
            &mut connection,
            || panic!("the kept connection served"),
            |_| {},
            &[Job::Choose("audio-0".into())],
            1,
            &mut |reply| {
                replies.push(reply);
                true
            },
        );
        assert_eq!(
            replies,
            [
                Reply::Refused("Refused switching output: that output would not open".into()),
                snapshot(expected(45, 30), 1)
            ]
        );
        assert!(connection.is_some());
    }

    /// A kept connection a restarted daemon broke is replaced once; with
    /// no daemon at all the panel hears it is unavailable.
    #[test]
    fn a_broken_connection_is_replaced_once_then_unavailable() {
        let mut connection = Some(connected(&[]));
        let mut fresh = Some(connected(&snapshot_replies(2, 45, 30)));
        let mut replies = Vec::new();
        run(
            &mut connection,
            || fresh.take(),
            |_| {},
            &[],
            0,
            &mut |reply| {
                replies.push(reply);
                true
            },
        );
        assert_eq!(replies, [snapshot(expected(45, 30), 0)]);
        replies.clear();
        let mut none: Option<Client<Scripted>> = Some(connected(&[]));
        run(&mut none, || None, |_| {}, &[], 0, &mut |reply| {
            replies.push(reply);
            true
        });
        assert_eq!(
            replies,
            [Reply::Unavailable {
                answered: 0,
                why: None
            }]
        );
        assert!(none.is_none());
        // Once: a fresh connection that fails too is not replaced again.
        replies.clear();
        let mut connects = 0;
        let mut broken = Some(connected(&[]));
        run(
            &mut broken,
            || {
                connects += 1;
                Some(connected(&[]))
            },
            |_| {},
            &[Job::Mute(Target::Master, true)],
            1,
            &mut |reply| {
                replies.push(reply);
                true
            },
        );
        assert_eq!(connects, 1);
        assert_eq!(
            replies,
            [Reply::Unavailable {
                answered: 1,
                why: None
            }]
        );
    }

    /// Live: a level changed elsewhere, the CLI's or a key's, reaches the
    /// panel. The watcher asks for a read on subscribing and again on the
    /// stream's change event; the control thread's read then shows the new
    /// level.
    #[test]
    fn a_level_changed_elsewhere_reaches_the_panel() {
        let mut watcher = connected(&[
            reply(2, |_| {}),
            event(
                subscription::EVENT_SINK_INPUT,
                subscription::EVENT_CHANGE,
                4,
            ),
        ]);
        let asked = std::cell::Cell::new(0);
        // The deadline is lifted only once SUBSCRIBE is sent and answered,
        // the last request the watcher makes, and before it reads.
        let mut settled = None;
        let ended = follow(
            &mut watcher,
            |transport: &mut Scripted| settled = Some((transport.sent.len(), asked.get())),
            &mut || {
                asked.set(asked.get() + 1);
                true
            },
        );
        let everything = watcher.transport_mut().sent.len();
        assert_eq!(settled, Some((everything, 0)));
        assert!(ended.is_err(), "the script ends as a daemon going away");
        assert_eq!(asked.get(), 2, "on subscribing and on the change");
        let mut decoder = Decoder::new();
        decoder.push(&watcher.transport_mut().sent);
        let subscribed = (0..3).filter_map(|_| decoder.next_frame()).last().unwrap();
        let Frame::Control(packet) = subscribed.unwrap() else {
            panic!("audio");
        };
        let mut reader = Reader::new(&packet);
        assert_eq!(reader.u32().unwrap(), command::SUBSCRIBE);
        reader.u32().unwrap();
        // Outputs, streams and the server: a stream's level, an output
        // coming or going, the default changing.
        assert_eq!(reader.u32().unwrap(), 0x1 | 0x4 | 0x80);

        let mut script = snapshot_replies(2, 45, 30);
        script.extend(snapshot_replies(5, 45, 80));
        let mut connection = Some(connected(&script));
        let mut shown = Vec::new();
        for _ in 0..asked.get() {
            run(&mut connection, || None, |_| {}, &[], 0, &mut |reply| {
                shown.push(reply);
                true
            });
        }
        assert_eq!(
            shown,
            [snapshot(expected(45, 30), 0), snapshot(expected(45, 80), 0)]
        );
    }

    /// Streams group by application: one row each, at the loudest
    /// stream's level, muted only when every stream is; a stream with no
    /// name is its client's.
    #[test]
    fn a_snapshot_groups_streams_by_application() {
        let mut client = connected(&[
            reply(2, |writer| server_info(writer, "intel-0")),
            reply(3, |writer| {
                sink_entry(writer, 0, "intel-0", "", volume_of(45), false);
            }),
            reply(4, |writer| {
                input_entry(writer, 4, 9, "Firefox", volume_of(20), true);
                input_entry(writer, 5, 12, "", volume_of(60), false);
                input_entry(writer, 6, 9, "Firefox", volume_of(70), false);
            }),
        ]);
        let snapshot = model::read(&mut client).unwrap();
        assert_eq!(snapshot.outputs[0].label, "intel-0", "no description");
        let apps: Vec<_> = snapshot
            .apps
            .iter()
            .map(|app| (app.name.as_str(), app.label.as_str(), app.level))
            .collect();
        assert_eq!(
            apps,
            [
                (
                    "Firefox",
                    "Firefox",
                    Level {
                        percent: 70,
                        muted: false
                    }
                ),
                (
                    "client-12",
                    "client-12",
                    Level {
                        percent: 60,
                        muted: false
                    }
                )
            ]
        );
    }

    /// An application's level is written to each of its streams; one that
    /// ended between the list and its write is passed over, and another
    /// application's is left alone.
    #[test]
    fn an_applications_level_reaches_each_of_its_streams() {
        let mut client = connected(&[
            reply(2, |writer| {
                input_entry(writer, 4, 9, "Firefox", volume_of(20), false);
                input_entry(writer, 5, 12, "mpv", volume_of(60), false);
                input_entry(writer, 6, 9, "Firefox", volume_of(70), false);
            }),
            refusal(3, error::NOENTITY),
            reply(4, |_| {}),
        ]);
        model::apply(&mut client, &Job::Level(Target::App("Firefox".into()), 40)).unwrap();
        let mut decoder = Decoder::new();
        decoder.push(&client.transport_mut().sent);
        let mut written = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            let Frame::Control(packet) = frame.unwrap() else {
                panic!("audio");
            };
            let mut reader = Reader::new(&packet);
            if reader.u32().unwrap() == command::SET_SINK_INPUT_VOLUME {
                reader.u32().unwrap();
                let index = reader.u32().unwrap();
                written.push((index, reader.cvolume().unwrap()));
            }
        }
        assert_eq!(
            written,
            [(4, vec![volume_of(40); 2]), (6, vec![volume_of(40); 2])]
        );
    }

    /// Each refused action is said by name, and the read still follows;
    /// a refused read, an account that is no controller, is said after
    /// the panel hears td-audio is unavailable to it.
    #[test]
    fn every_refusal_is_said_and_a_refused_read_is_unavailable() {
        let mut script = vec![refusal(2, error::NOENTITY), refusal(3, error::NOTSUPPORTED)];
        script.extend(snapshot_replies(4, 45, 30));
        let mut connection = Some(connected(&script));
        let mut replies = Vec::new();
        run(
            &mut connection,
            || None,
            |_| {},
            &[Job::Choose("gone".into()), Job::Choose("audio-0".into())],
            2,
            &mut |reply| {
                replies.push(reply);
                true
            },
        );
        assert_eq!(
            replies,
            [
                Reply::Refused("Refused switching output: no longer there".into()),
                Reply::Refused("Refused switching output: that output would not open".into()),
                snapshot(expected(45, 30), 2)
            ]
        );
        replies.clear();
        let mut connection = Some(connected(&[refusal(2, error::NOTIMPLEMENTED)]));
        run(&mut connection, || None, |_| {}, &[], 0, &mut |reply| {
            replies.push(reply);
            true
        });
        assert_eq!(
            replies,
            [Reply::Unavailable {
                answered: 0,
                why: Some("Refused listing outputs: not for this account".into())
            }]
        );
        assert!(connection.is_some(), "a refusal keeps the connection");
    }

    /// The daemon offers at most 64 outputs; a snapshot keeps no more.
    #[test]
    fn a_snapshot_keeps_at_most_its_bound_of_outputs() {
        let mut client = connected(&[
            reply(2, |writer| server_info(writer, "out-0")),
            reply(3, |writer| {
                for index in 0..MAX_OUTPUTS as u32 + 6 {
                    let name = format!("out-{index}");
                    sink_entry(writer, index, &name, "", volume_of(45), false);
                }
            }),
            reply(4, |_| {}),
        ]);
        let snapshot = model::read(&mut client).unwrap();
        assert_eq!(snapshot.outputs.len(), MAX_OUTPUTS);
        assert!(snapshot.outputs[0].playing);
    }

    /// While the daemon was last unavailable the control thread reads again
    /// unasked; otherwise it waits for a message, and ends with the window.
    #[test]
    fn the_control_thread_retries_only_while_unavailable() {
        let (send, queued) = mpsc::sync_channel(QUEUE);
        let short = Duration::from_millis(10);
        assert!(matches!(next(&queued, true, short), Some(Message::Changed)));
        send.try_send(Message::Job(Job::Mute(Target::Master, true)))
            .unwrap();
        assert!(matches!(next(&queued, false, short), Some(Message::Job(_))));
        drop(send);
        assert!(next(&queued, true, short).is_none());
        assert!(next(&queued, false, short).is_none());
    }

    #[test]
    fn a_batch_takes_every_waiting_job_in_order() {
        let (send, queued) = mpsc::sync_channel(QUEUE);
        send.try_send(Message::Job(Job::Level(Target::Master, 10)))
            .unwrap();
        send.try_send(Message::Changed).unwrap();
        send.try_send(Message::Job(Job::Mute(Target::Master, true)))
            .unwrap();
        let (jobs, taken) = batch(Message::Changed, &queued);
        assert_eq!(taken, 2);
        assert_eq!(
            jobs,
            [
                Job::Level(Target::Master, 10),
                Job::Mute(Target::Master, true)
            ]
        );
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn the_watcher_backs_off_until_it_subscribes() {
        assert_eq!(
            backoff(BACKOFF_FIRST, false),
            (BACKOFF_FIRST, BACKOFF_FIRST * 2)
        );
        assert_eq!(backoff(Duration::from_secs(16), false).1, BACKOFF_MAX);
        assert_eq!(
            backoff(BACKOFF_MAX, true),
            (BACKOFF_FIRST, BACKOFF_FIRST * 2)
        );
    }
}
