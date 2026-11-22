//! The volume keys and the bar's audio field, over td-audio's socket
//! (td-compositor/DESIGN.md, "Volume"; APPLICATIONS.md §K.5).
//!
//! The compositor is an ordinary Pulse client of the daemon, admitted in
//! the output role: it sets and reads the output's level and mute and hears
//! the output change, and nothing else. Two threads on two connections,
//! because the client blocks and the input path must never wait on it:
//!
//! - `td-audio-control` takes keys from a short queue the input path only
//!   ever `try_send`s to. Each key is one read, the writes it asks for and
//!   one read back, under a deadline, on a connection kept between keys;
//!   then a notice.
//! - `td-audio-watch` holds a subscription and re-reads the output on each
//!   sink event, so a change made elsewhere, by the CLI or the mixer,
//!   reaches the bar. It reconnects with backoff while the daemon is away.
//!
//! Both publish what they read to the runtime, whose state the bar's tick
//! shows.

use crate::runtime::Runtime;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use td_pulse::client::{
    self, level, levels_like, percent, volume_of, Client, Transport, DEFAULT_SINK, MAX_PERCENT,
};
use td_pulse::proto::subscription;
use td_pulse::socket::UnixSocket;
use td_pulse::tag;

/// The daemon's socket (`td_engine::permissions::TD_AUDIO_SOCKET_PATH`).
pub const SOCKET: &str = "/run/td-audio/native";

/// What the daemon is told this client is called.
const CLIENT_NAME: &str = "td-compositor";

/// One key's step, in whole percent.
pub const STEP: u32 = 5;

/// How long one key, or one re-read after an event, may take. A daemon
/// that does not answer in time is shown as no audio, never waited on.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// The watcher's first wait before reconnecting, doubled up to
/// `BACKOFF_MAX` while the daemon stays away and reset once one connects.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Keys queued for the control thread. A held key repeats faster than a
/// round trip may take; a repeat that finds the queue full is dropped, so
/// the input path never waits and the level stops soon after the key does.
const QUEUE: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Down,
    Up,
    Mute,
}

/// The output as last read, or that it could not be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    Level { percent: u32, muted: bool },
    Unavailable,
}

impl Reading {
    /// The bar's field: `VOL 45%`, `MUTE` or `NO AUDIO`.
    pub fn field(self) -> Field {
        Field(self)
    }

    /// The notice a key leaves: `VOLUME 45%`, `MUTED` or `NO AUDIO`.
    pub fn notice(self) -> Notice {
        Notice(self)
    }
}

pub struct Field(Reading);

impl fmt::Display for Field {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Reading::Level { muted: true, .. } => out.write_str("MUTE"),
            Reading::Level { percent, .. } => write!(out, "VOL {percent}%"),
            Reading::Unavailable => out.write_str("NO AUDIO"),
        }
    }
}

/// The console line each key's outcome is also logged as, which the boot
/// oracle waits on: `TD-VOLUME level=45 muted=no`, `TD-VOLUME unavailable`.
pub struct Marker(Reading);

impl fmt::Display for Marker {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Reading::Level { percent, muted } => {
                let muted = if muted { "yes" } else { "no" };
                write!(out, "TD-VOLUME level={percent} muted={muted}")
            }
            Reading::Unavailable => out.write_str("TD-VOLUME unavailable"),
        }
    }
}

/// Log a key's outcome as one write beginning a line of its own, as the
/// bar's zone report is: the console is shared with other writers'
/// unterminated output. Advisory; a console that refuses it costs nothing.
fn report(reading: Reading) {
    let mut line = [0u8; 64];
    let mut cursor = io::Cursor::new(line.as_mut_slice());
    if write!(cursor, "\n{}\n", Marker(reading)).is_err() {
        return;
    }
    let written = usize::try_from(cursor.position()).unwrap_or_default();
    let mut out = io::stdout().lock();
    let _ = out
        .write_all(line.get(..written).unwrap_or_default())
        .and_then(|()| out.flush());
}

pub struct Notice(Reading);

impl fmt::Display for Notice {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Reading::Level { muted: true, .. } => out.write_str("MUTED"),
            Reading::Level { percent, .. } => write!(out, "VOLUME {percent}%"),
            Reading::Unavailable => out.write_str("NO AUDIO"),
        }
    }
}

/// The level and mute a key asks for, from those read. A step lands on a
/// multiple of `STEP`, so a level set elsewhere rejoins the keys' grid at
/// the first press, and clamps to 0–100. Either step unmutes, so the
/// person hears where it went; mute toggles and keeps the level.
pub fn step(key: Key, percent: u32, muted: bool) -> (u32, bool) {
    let percent = percent.min(MAX_PERCENT);
    match key {
        Key::Up => (
            (percent / STEP + 1).saturating_mul(STEP).min(MAX_PERCENT),
            false,
        ),
        Key::Down => (
            percent
                .div_ceil(STEP)
                .saturating_sub(1)
                .saturating_mul(STEP),
            false,
        ),
        Key::Mute => (percent, !muted),
    }
}

/// The output's level and mute, read on `client`.
fn read<T: Transport>(client: &mut Client<T>) -> client::Result<Reading, T::Error> {
    let sink = client.sink(DEFAULT_SINK)?;
    Ok(Reading::Level {
        percent: percent(level(&sink.volume)),
        muted: sink.muted,
    })
}

/// Why a key failed, and whether anything had been sent to change the
/// output by then: a key that may have landed is never retried.
#[derive(Debug)]
struct KeyError<E> {
    written: bool,
    error: client::Error<E>,
}

/// One key on `client`: read the output, write what the key changes to the
/// index that read named, and read back what the daemon now holds.
fn press<T: Transport>(client: &mut Client<T>, key: Key) -> Result<Reading, KeyError<T::Error>> {
    let sink = client.sink(DEFAULT_SINK).map_err(|error| KeyError {
        written: false,
        error,
    })?;
    let written = |error| KeyError {
        written: true,
        error,
    };
    let (wanted, muted) = step(key, percent(level(&sink.volume)), sink.muted);
    if key != Key::Mute {
        let mut levels = [0; tag::CHANNELS_MAX];
        let levels = levels_like(&mut levels, &sink.volume, volume_of(wanted));
        client
            .set_sink_volume(sink.index, levels)
            .map_err(written)?;
    }
    if muted != sink.muted {
        client.set_sink_mute(sink.index, muted).map_err(written)?;
    }
    read(client).map_err(written)
}

/// A connection, authenticated and named within `within`.
fn connect(socket: &Path, within: Duration) -> Option<Client<UnixSocket>> {
    let mut transport = UnixSocket::connect(socket).ok()?;
    transport.deadline_in(within);
    Client::connect(transport, CLIENT_NAME).ok()
}

/// One key, on the kept connection or a fresh one from `connect`, each
/// request armed by `arm` with what is left of the key's deadline. A
/// connection that fails is dropped, so the next key starts afresh. A kept
/// connection that fails before anything was written is retried once on a
/// fresh one — a daemon restarted since the last key leaves a connection
/// that fails at first use — and nothing else is, so a key never lands
/// twice.
fn handle<T: Transport>(
    connection: &mut Option<Client<T>>,
    mut connect: impl FnMut() -> Option<Client<T>>,
    arm: impl Fn(&mut T),
    key: Key,
) -> Reading
where
    T::Error: fmt::Debug,
{
    let mut retry = connection.is_some();
    loop {
        if connection.is_none() {
            *connection = connect();
        }
        let Some(client) = connection.as_mut() else {
            return Reading::Unavailable;
        };
        arm(client.transport_mut());
        match press(client, key) {
            Ok(reading) => return reading,
            Err(failure) => {
                *connection = None;
                if !retry || failure.written {
                    // Logged as the brightness keys' failures are.
                    let _ = writeln!(
                        io::stderr(),
                        "td-compositor: volume key failed: {:?}",
                        failure.error
                    );
                    return Reading::Unavailable;
                }
                retry = false;
            }
        }
    }
}

fn control(runtime: &Mutex<Runtime>, socket: &Path, keys: &Receiver<Key>) {
    let mut connection = None;
    while let Ok(key) = keys.recv() {
        // One deadline for the whole key, retry and reconnect included.
        let now = Instant::now();
        let until = now.checked_add(REQUEST_DEADLINE).unwrap_or(now);
        let left = || until.saturating_duration_since(Instant::now());
        let reading = handle(
            &mut connection,
            || connect(socket, left()),
            |transport: &mut UnixSocket| transport.deadline_in(left()),
            key,
        );
        report(reading);
        // Logged once the lock is released, as the bar's tick does: a slow
        // console must not stall every thread waiting on the runtime.
        let noticed = match runtime.lock() {
            Ok(mut runtime) => runtime.audio_notice(reading),
            Err(_) => return,
        };
        if let Err(error) = noticed {
            let _ = writeln!(io::stderr(), "td-compositor: volume notice: {error}");
        }
    }
}

/// Give the runtime the watcher's reading. Answers false once the runtime
/// is poisoned, which ends the thread.
fn publish(runtime: &Mutex<Runtime>, reading: Reading) -> bool {
    let Ok(mut runtime) = runtime.lock() else {
        return false;
    };
    runtime.audio_reading(reading);
    true
}

/// Show the output on connecting and again after each sink event, until
/// the connection fails (`Err`) or the runtime is gone (`Ok`). Events that
/// arrive during a read are dropped: the read that follows them is what
/// they would have asked for. `published` says whether any reading was.
fn follow<T: Transport>(
    runtime: &Mutex<Runtime>,
    client: &mut Client<T>,
    deadline: impl Fn(&mut T, bool),
    published: &mut bool,
) -> client::Result<(), T::Error> {
    client.subscribe(subscription::MASK_SINK)?;
    loop {
        deadline(client.transport_mut(), true);
        let reading = read(client)?;
        client.take_missed_events();
        if !publish(runtime, reading) {
            return Ok(());
        }
        *published = true;
        deadline(client.transport_mut(), false);
        while client.next_event()?.facility != subscription::EVENT_SINK {}
    }
}

/// The wait before reconnecting and the one after it: from the first again
/// once a connection published a reading, else doubled up to the most.
fn backoff(previous: Duration, published: bool) -> (Duration, Duration) {
    let wait = if published { BACKOFF_FIRST } else { previous };
    (wait, wait.saturating_mul(2).min(BACKOFF_MAX))
}

fn watch(runtime: &Mutex<Runtime>, socket: &Path) {
    let armed = |transport: &mut UnixSocket, armed: bool| {
        if armed {
            transport.deadline_in(REQUEST_DEADLINE);
        } else {
            transport.no_deadline();
        }
    };
    let mut next = BACKOFF_FIRST;
    loop {
        let mut published = false;
        if let Some(mut client) = connect(socket, REQUEST_DEADLINE) {
            if follow(runtime, &mut client, armed, &mut published).is_ok() {
                return;
            }
        }
        if !publish(runtime, Reading::Unavailable) {
            return;
        }
        let (wait, after) = backoff(next, published);
        thread::sleep(wait);
        next = after;
    }
}

/// Give the runtime the keys' queue, then start both threads against
/// `socket`; the queue first, so neither thread's first reading arrives
/// before the runtime keeps one. A thread that cannot start withdraws the
/// queue, and the caller reports it and carries on: a session without
/// audio control is one whose volume keys say `NO AUDIO`.
pub fn start(runtime: &Arc<Mutex<Runtime>>, socket: &'static Path) -> io::Result<()> {
    let (keys, queued) = mpsc::sync_channel(QUEUE);
    runtime
        .lock()
        .map_err(|_| io::Error::other("runtime lock poisoned"))?
        .enable_audio(keys);
    let started = spawn(runtime, socket, queued);
    if started.is_err() {
        if let Ok(mut runtime) = runtime.lock() {
            runtime.disable_audio();
        }
    }
    started
}

fn spawn(
    runtime: &Arc<Mutex<Runtime>>,
    socket: &'static Path,
    queued: Receiver<Key>,
) -> io::Result<()> {
    let name = |name| {
        crate::allocation::string(name).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    };
    let control_runtime = Arc::clone(runtime);
    thread::Builder::new()
        .name(name("td-audio-control")?)
        .spawn(move || control(&control_runtime, socket, &queued))?;
    let watch_runtime = Arc::clone(runtime);
    thread::Builder::new()
        .name(name("td-audio-watch")?)
        .spawn(move || watch(&watch_runtime, socket))?;
    Ok(())
}

/// Hand a key to the control thread without waiting. `Some(false)` is a
/// repeat dropped because the queue is full; `None`, a thread that is gone.
pub fn queue(keys: &SyncSender<Key>, key: Key) -> Option<bool> {
    match keys.try_send(key) {
        Ok(()) => Some(true),
        Err(TrySendError::Full(_)) => Some(false),
        Err(TrySendError::Disconnected(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use td_pulse::proto::{self, command};
    use td_pulse::tag::{Reader, Writer};
    use td_pulse::wire::{self, Decoder, Frame};

    #[test]
    fn a_step_lands_on_the_grid_and_clamps() {
        for (key, from, muted, to) in [
            (Key::Up, 45, false, (50, false)),
            (Key::Up, 47, false, (50, false)),
            (Key::Up, 98, false, (100, false)),
            (Key::Up, 100, false, (100, false)),
            (Key::Up, 30, true, (35, false)),
            (Key::Down, 45, false, (40, false)),
            (Key::Down, 47, false, (45, false)),
            (Key::Down, 3, false, (0, false)),
            (Key::Down, 0, false, (0, false)),
            (Key::Down, 30, true, (25, false)),
            (Key::Mute, 45, false, (45, true)),
            (Key::Mute, 45, true, (45, false)),
            (Key::Up, 150, false, (100, false)),
        ] {
            assert_eq!(step(key, from, muted), to, "{key:?} from {from} {muted}");
        }
    }

    #[test]
    fn the_field_and_the_notice_read_as_designed() {
        let level = Reading::Level {
            percent: 45,
            muted: false,
        };
        let muted = Reading::Level {
            percent: 45,
            muted: true,
        };
        let text = |shown: &dyn fmt::Display| crate::allocation::format(format_args!("{shown}"));
        assert_eq!(text(&level.field()).unwrap(), "VOL 45%");
        assert_eq!(text(&muted.field()).unwrap(), "MUTE");
        assert_eq!(text(&Reading::Unavailable.field()).unwrap(), "NO AUDIO");
        assert_eq!(text(&level.notice()).unwrap(), "VOLUME 45%");
        assert_eq!(text(&muted.notice()).unwrap(), "MUTED");
        assert_eq!(text(&Reading::Unavailable.notice()).unwrap(), "NO AUDIO");
        assert_eq!(text(&Marker(level)).unwrap(), "TD-VOLUME level=45 muted=no");
        assert_eq!(
            text(&Marker(muted)).unwrap(),
            "TD-VOLUME level=45 muted=yes"
        );
        assert_eq!(
            text(&Marker(Reading::Unavailable)).unwrap(),
            "TD-VOLUME unavailable"
        );
        // The longest marker fits the one write `report` makes.
        let longest = Marker(Reading::Level {
            percent: MAX_PERCENT,
            muted: true,
        });
        assert!(text(&longest).unwrap().len() + 2 <= 64);
    }

    #[test]
    fn a_full_queue_drops_a_repeat_and_a_gone_thread_says_so() {
        let (keys, queued) = mpsc::sync_channel(1);
        assert_eq!(queue(&keys, Key::Up), Some(true));
        assert_eq!(queue(&keys, Key::Up), Some(false));
        drop(queued);
        assert_eq!(queue(&keys, Key::Up), None);
    }

    /// A server that answers from a script and records what it was sent.
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

    /// A version-35 sink entry in the shape td-audio writes, at sink 3.
    fn sink(tag: u32, volume: u32, muted: bool) -> Vec<u8> {
        reply(tag, |writer| {
            writer
                .u32(3)
                .string("td-audio")
                .string("td audio")
                .sample_spec(tag::SampleSpec {
                    format: tag::SAMPLE_S16LE,
                    channels: 2,
                    rate: 48_000,
                })
                .channel_map(&[1, 2])
                .u32(tag::INVALID_INDEX)
                .cvolume(&[volume, volume])
                .boolean(muted)
                .u32(tag::INVALID_INDEX)
                .null_string()
                .usec(0)
                .string("td-audio")
                .u32(0)
                .proplist(&[tag::text_property("device.class", "sound")])
                .usec(0)
                .volume(client::VOLUME_NORM)
                .u32(1)
                .u32(0x10001)
                .u32(tag::INVALID_INDEX)
                .u32(0)
                .null_string()
                .u8(1)
                .format_info(1, &[]);
        })
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
        let transport = Scripted {
            incoming,
            at: 0,
            sent: Vec::new(),
        };
        Client::connect(transport, CLIENT_NAME).unwrap()
    }

    /// Each request after the handshake: its command, and the sink index and
    /// value a setter named.
    /// A request's command, and the sink index and value a setter named.
    type Request = (u32, Option<(u32, Vec<u32>)>);

    fn requests(client: &mut Client<Scripted>) -> Vec<Request> {
        let mut decoder = Decoder::new();
        decoder.push(&client.transport_mut().sent);
        let mut found = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            let Frame::Control(packet) = frame.unwrap() else {
                panic!("the client sent data");
            };
            let mut reader = Reader::new(&packet);
            let command = reader.u32().unwrap();
            reader.u32().unwrap();
            let named = match command {
                command::SET_SINK_VOLUME => {
                    let index = reader.u32().unwrap();
                    reader.string().unwrap();
                    Some((index, reader.cvolume().unwrap()))
                }
                command::SET_SINK_MUTE => {
                    let index = reader.u32().unwrap();
                    reader.string().unwrap();
                    Some((index, vec![u32::from(reader.boolean().unwrap())]))
                }
                _ => None,
            };
            found.push((command, named));
        }
        found.split_off(2)
    }

    /// Up from a muted 47%: the read, the level to 50% on every channel of
    /// the sink the read named, the unmute, the read back.
    #[test]
    fn a_key_writes_what_it_changes_to_the_sink_it_read() {
        let at = |value: u32| volume_of(value);
        let mut client = connected(&[
            sink(2, at(47), true),
            reply(3, |_| {}),
            reply(4, |_| {}),
            sink(5, at(50), false),
        ]);
        assert_eq!(
            press(&mut client, Key::Up).unwrap(),
            Reading::Level {
                percent: 50,
                muted: false
            }
        );
        assert_eq!(
            requests(&mut client),
            [
                (command::GET_SINK_INFO, None),
                (command::SET_SINK_VOLUME, Some((3, vec![at(50), at(50)]))),
                (command::SET_SINK_MUTE, Some((3, vec![0]))),
                (command::GET_SINK_INFO, None),
            ]
        );
    }

    /// Mute writes the mute alone, and a step on an unmuted output writes
    /// the level alone.
    #[test]
    fn mute_and_an_unmuted_step_write_one_thing_each() {
        let at = |value: u32| volume_of(value);
        let mut client = connected(&[
            sink(2, at(60), false),
            reply(3, |_| {}),
            sink(4, at(60), true),
        ]);
        assert_eq!(
            press(&mut client, Key::Mute).unwrap(),
            Reading::Level {
                percent: 60,
                muted: true
            }
        );
        assert_eq!(
            requests(&mut client),
            [
                (command::GET_SINK_INFO, None),
                (command::SET_SINK_MUTE, Some((3, vec![1]))),
                (command::GET_SINK_INFO, None),
            ]
        );
        let mut client = connected(&[
            sink(2, at(60), false),
            reply(3, |_| {}),
            sink(4, at(55), false),
        ]);
        press(&mut client, Key::Down).unwrap();
        assert_eq!(
            requests(&mut client)[1],
            (command::SET_SINK_VOLUME, Some((3, vec![at(55), at(55)])))
        );
    }

    /// A refusal is a failed key, not a guess: the refusal is what fails it,
    /// before the read back a swallowed refusal would have gone on to.
    #[test]
    fn a_refused_write_fails_the_key() {
        let mut refused = Writer::new();
        refused
            .u32(command::ERROR)
            .u32(3)
            .u32(proto::error::NOTIMPLEMENTED);
        let mut client = connected(&[
            sink(2, volume_of(40), false),
            wire::control_frame(refused.as_bytes()),
            sink(4, volume_of(45), false),
        ]);
        let failure = press(&mut client, Key::Up).unwrap_err();
        assert!(failure.written);
        assert!(
            matches!(failure.error, client::Error::Refused(code) if code == proto::error::NOTIMPLEMENTED),
            "{failure:?}"
        );
    }

    /// A kept connection that fails before writing is retried once, fresh:
    /// the restarted daemon. One that fails after a write is not, so the key
    /// is not applied twice. No connection to be had is no audio, asked once.
    #[test]
    fn a_key_is_retried_fresh_only_when_nothing_was_written() {
        let fresh = || {
            connected(&[
                sink(2, volume_of(60), false),
                reply(3, |_| {}),
                sink(4, volume_of(65), false),
            ])
        };
        let mut kept = Some(connected(&[]));
        let mut asked = 0;
        let reading = handle(
            &mut kept,
            || {
                asked += 1;
                Some(fresh())
            },
            |_| {},
            Key::Up,
        );
        assert_eq!(
            reading,
            Reading::Level {
                percent: 65,
                muted: false
            }
        );
        assert_eq!(asked, 1);
        assert!(
            kept.is_some(),
            "the fresh connection is kept for the next key"
        );

        // The write was sent and its answer lost: no second step.
        let mut kept = Some(connected(&[sink(2, volume_of(60), false)]));
        let mut asked = 0;
        let reading = handle(
            &mut kept,
            || {
                asked += 1;
                Some(fresh())
            },
            |_| {},
            Key::Up,
        );
        assert_eq!(reading, Reading::Unavailable);
        assert_eq!(asked, 0);
        assert!(kept.is_none(), "a failed connection is dropped");

        // A fresh connection that fails is not retried either.
        let mut none: Option<Client<Scripted>> = None;
        let mut asked = 0;
        let reading = handle(
            &mut none,
            || {
                asked += 1;
                Some(connected(&[]))
            },
            |_| {},
            Key::Up,
        );
        assert_eq!(reading, Reading::Unavailable);
        assert_eq!(asked, 1);
        let mut asked = 0;
        let reading = handle(
            &mut none,
            || {
                asked += 1;
                None
            },
            |_| {},
            Key::Mute,
        );
        assert_eq!((reading, asked), (Reading::Unavailable, 1));
    }

    fn event(facility: u32) -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .u32(command::SUBSCRIBE_EVENT)
            .u32(tag::INVALID_INDEX)
            .u32(facility | subscription::EVENT_CHANGE)
            .u32(0);
        wire::control_frame(writer.as_bytes())
    }

    /// The watcher subscribes to the sink alone, shows the output, re-reads
    /// it after a sink event and only then, and ends when the daemon goes,
    /// saying it had published; the deadline is armed for each read and
    /// lifted for each wait.
    #[test]
    fn the_watcher_shows_the_output_and_each_sink_change() {
        let dir = crate::temp_dir::TempDir::new("td-audio-watch").unwrap();
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&dir.path().join("fb"), 320, 200, 320 * 4)
                .unwrap();
        let runtime = Mutex::new(Runtime::new(framebuffer));
        let (keys, _queued) = mpsc::sync_channel(1);
        runtime.lock().unwrap().enable_audio(keys);
        let mut client = connected(&[
            reply(2, |_| {}),
            sink(3, volume_of(40), false),
            event(subscription::EVENT_SINK_INPUT),
            event(subscription::EVENT_SINK),
            sink(4, volume_of(55), true),
        ]);
        let arms = std::cell::RefCell::new(Vec::new());
        let mut published = false;
        let ended = follow(
            &runtime,
            &mut client,
            |_, armed| arms.borrow_mut().push(armed),
            &mut published,
        );
        assert!(matches!(ended, Err(client::Error::Closed)), "{ended:?}");
        assert!(published);
        assert_eq!(
            runtime.lock().unwrap().audio_shown(),
            Some(Reading::Level {
                percent: 55,
                muted: true
            })
        );
        assert_eq!(*arms.borrow(), [true, false, true, false]);
        let mut decoder = Decoder::new();
        decoder.push(&client.transport_mut().sent);
        let mut sent = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            let Frame::Control(packet) = frame.unwrap() else {
                panic!("data");
            };
            let mut reader = Reader::new(&packet);
            let command = reader.u32().unwrap();
            reader.u32().unwrap();
            let mask = (command == command::SUBSCRIBE).then(|| reader.u32().unwrap());
            sent.push((command, mask));
        }
        assert_eq!(
            sent.split_off(2),
            [
                (command::SUBSCRIBE, Some(subscription::MASK_SINK)),
                (command::GET_SINK_INFO, None),
                (command::GET_SINK_INFO, None),
            ]
        );
    }

    /// Another facility's event is no reason to read the output again.
    #[test]
    fn the_watcher_reads_nothing_for_a_stream_event() {
        let dir = crate::temp_dir::TempDir::new("td-audio-watch-stream").unwrap();
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&dir.path().join("fb"), 320, 200, 320 * 4)
                .unwrap();
        let runtime = Mutex::new(Runtime::new(framebuffer));
        let mut client = connected(&[
            reply(2, |_| {}),
            sink(3, volume_of(40), false),
            event(subscription::EVENT_SINK_INPUT),
        ]);
        let mut published = false;
        let ended = follow(&runtime, &mut client, |_, _| {}, &mut published);
        assert!(matches!(ended, Err(client::Error::Closed)), "{ended:?}");
        let mut decoder = Decoder::new();
        decoder.push(&client.transport_mut().sent);
        let mut reads = 0;
        while let Some(frame) = decoder.next_frame() {
            if let Frame::Control(packet) = frame.unwrap() {
                let mut reader = Reader::new(&packet);
                reads += usize::from(reader.u32().unwrap() == command::GET_SINK_INFO);
            }
        }
        assert_eq!(reads, 1);
    }

    /// The wait doubles to the most while the daemon stays away, and starts
    /// over only after a connection showed a reading.
    #[test]
    fn the_backoff_resets_only_after_a_reading() {
        let mut next = BACKOFF_FIRST;
        let mut waits = Vec::new();
        for _ in 0..7 {
            let (wait, after) = backoff(next, false);
            waits.push(wait.as_secs());
            next = after;
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff(next, true), (BACKOFF_FIRST, BACKOFF_FIRST * 2));
    }
}
