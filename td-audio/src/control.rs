//! The control personalities: `APPLICATIONS.md` §K.5's CLI.
//!
//! Each is an ordinary Pulse client of the daemon's own socket, through
//! td-pulse's client, so it is a controller only when it runs as a
//! controller uid: the seat user or audio. Output is one line per fact,
//! `key value`, with free text last on its line, for scripts and the bar.
//! Exit codes: 0 success, 1 the daemon refused or could not be reached, 2 a
//! usage error.
//!
//! `play` is the one that is no controller: it plays the fixture tone as a
//! stream, as any application would, so a test can hear the daemon's whole
//! path, switches included, from a client's side.

use crate::proto::{self, error, subscription};
use crate::session::{shown, CHANNEL_MAP};
use crate::sink::Spec;
use crate::tone::{Generator, Tone};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use td_pulse::client::{
    self, level, levels_like, percent, volume_of, Client, Listing, SinkInput, DEFAULT_SINK,
    MAX_PERCENT,
};
use td_pulse::socket::UnixSocket;
use td_pulse::tag;

/// The personalities this module answers for.
pub const PERSONALITIES: &[&str] = &[
    "status", "outputs", "output", "volume", "mute", "apps", "app", "watch", "play",
];

const SUCCESS: u8 = 0;
const FAILED: u8 = 1;
const USAGE_ERROR: u8 = 2;

/// How long a whole command may take, `watch` until it has subscribed. A
/// daemon that is restarting answers in milliseconds or not at all; the bar
/// and a script both want to hear which.
const DEADLINE: Duration = Duration::from_secs(5);

/// What the daemon is told this client is called.
const CLIENT_NAME: &str = "td-audio";

/// The longest tone `play` plays: a test fixture, not a music player.
const MAX_PLAY_MS: u64 = 60_000;

/// The frames `play` generates and sends at a time: one of the daemon's
/// periods.
const PLAY_CHUNK_FRAMES: usize = 1024;

/// Run one control personality and return its exit code.
pub fn main(
    personality: &str,
    arguments: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    run(personality, arguments, out, err, DEADLINE, &mut || {})
}

/// `main`, with the deadline, and a hook `watch` calls once its
/// subscription is in place.
fn run(
    personality: &str,
    arguments: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    deadline: Duration,
    subscribed: &mut dyn FnMut(),
) -> u8 {
    let (socket, words) = match arguments {
        [flag, path, rest @ ..] if flag == "--socket" && !path.is_empty() => {
            (Path::new(path.as_os_str()), rest)
        }
        _ => (Path::new(crate::serve::SOCKET_PATH), arguments),
    };
    let Some(command) = parse(personality, words) else {
        let _ = writeln!(err, "td-audio: usage:");
        for line in crate::USAGE.lines().map(str::trim_start) {
            if line.split(' ').nth(1) == Some(personality) {
                let _ = writeln!(err, "  {line}");
            }
        }
        return USAGE_ERROR;
    };
    let deadline = match command {
        // Played in real time, then drained: twice the tone bounds both.
        Command::Play(milliseconds) => {
            deadline.saturating_add(Duration::from_millis(milliseconds.saturating_mul(2)))
        }
        _ => deadline,
    };
    let outcome = connect(socket, deadline)
        .and_then(|mut client| execute(&mut client, &command, out, subscribed));
    match outcome {
        Ok(()) => SUCCESS,
        Err(failure) => {
            let _ = writeln!(err, "td-audio: {failure}");
            FAILED
        }
    }
}

enum Command<'a> {
    Status,
    Outputs,
    Output(&'a str),
    Volume(Option<Level>),
    Mute(Option<Switch>),
    Apps,
    AppVolume(&'a str, Level),
    AppMute(&'a str, Switch),
    Watch,
    Play(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    To(u32),
    Up(u32),
    Down(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Switch {
    On,
    Off,
    Toggle,
}

fn parse<'a>(personality: &str, words: &'a [OsString]) -> Option<Command<'a>> {
    let text = |word: &'a OsString| word.to_str();
    Some(match (personality, words) {
        ("status", []) => Command::Status,
        ("outputs", []) => Command::Outputs,
        ("output", [name]) => Command::Output(text(name).filter(|name| is_sink_name(name))?),
        ("volume", []) => Command::Volume(None),
        ("volume", [value]) => Command::Volume(Some(parse_level(text(value)?)?)),
        ("mute", []) => Command::Mute(None),
        ("mute", [value]) => Command::Mute(Some(parse_switch(text(value)?)?)),
        ("apps", []) => Command::Apps,
        ("app", [name, what, value]) => {
            let name = text(name)?;
            if name.is_empty() {
                return None;
            }
            match text(what)? {
                "volume" => Command::AppVolume(name, parse_level(text(value)?)?),
                "mute" => Command::AppMute(name, parse_switch(text(value)?)?),
                _ => return None,
            }
        }
        ("watch", []) => Command::Watch,
        ("play", [milliseconds]) => Command::Play(
            text(milliseconds)
                .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))?
                .parse()
                .ok()
                .filter(|milliseconds| (1..=MAX_PLAY_MS).contains(milliseconds))?,
        ),
        _ => return None,
    })
}

/// What `output` may send as a sink's name. A flag is a usage error, not
/// a name; an output whose name starts with `-` is named by its number.
/// Nor is a name longer than the protocol carries, which the daemon would
/// hang up on.
fn is_sink_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && name.len() <= tag::STRING_MAX
}

/// `40%` or `40` sets a level; `+5%` and `-5%` step from the one read.
fn parse_level(text: &str) -> Option<Level> {
    let number = text.strip_suffix('%').unwrap_or(text);
    let digits = |text: &str| {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse::<u32>().ok()
    };
    if let Some(step) = number.strip_prefix('+') {
        return digits(step).map(Level::Up);
    }
    if let Some(step) = number.strip_prefix('-') {
        return digits(step).map(Level::Down);
    }
    digits(number).map(Level::To)
}

fn parse_switch(text: &str) -> Option<Switch> {
    match text {
        "on" => Some(Switch::On),
        "off" => Some(Switch::Off),
        "toggle" => Some(Switch::Toggle),
        _ => None,
    }
}

/// The level `level` asks for, from `current`, in percent and clamped.
fn resolve(level: Level, current: u32) -> u32 {
    match level {
        Level::To(target) => target,
        Level::Up(step) => current.saturating_add(step),
        Level::Down(step) => current.saturating_sub(step),
    }
    .min(MAX_PERCENT)
}

fn switched(switch: Switch, muted: bool) -> bool {
    match switch {
        Switch::On => true,
        Switch::Off => false,
        Switch::Toggle => !muted,
    }
}

/// Connect with one deadline for the whole command, so a daemon that answers
/// a byte at a time cannot stretch a request past it.
fn connect(socket: &Path, deadline: Duration) -> Result<Client<UnixSocket>, Failure<'_>> {
    let mut transport =
        UnixSocket::connect(socket).map_err(|error| Failure::Connect(socket, error))?;
    transport.deadline_in(deadline);
    Client::connect(transport, CLIENT_NAME).map_err(Failure::Daemon)
}

enum Failure<'a> {
    Connect(&'a Path, io::Error),
    Daemon(client::Error<io::Error>),
    NoApp(&'a str),
    Output(io::Error),
    Memory,
}

impl From<client::Error<io::Error>> for Failure<'_> {
    fn from(error: client::Error<io::Error>) -> Self {
        Self::Daemon(error)
    }
}

impl From<tag::Error> for Failure<'_> {
    fn from(error: tag::Error) -> Self {
        Self::Daemon(client::Error::Malformed(error))
    }
}

impl fmt::Display for Failure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(path, error) => write!(f, "connect {}: {error}", path.display()),
            Self::Daemon(client::Error::Transport(error))
                if error.kind() == io::ErrorKind::TimedOut =>
            {
                f.write_str("the daemon did not answer in time")
            }
            Self::Daemon(client::Error::Transport(error)) => {
                write!(f, "the daemon's socket: {error}")
            }
            Self::Daemon(client::Error::Closed) => f.write_str("the daemon closed the connection"),
            Self::Daemon(client::Error::Framing(error)) => {
                write!(f, "the daemon sent a frame td-pulse refuses: {error:?}")
            }
            Self::Daemon(client::Error::Malformed(error)) => {
                write!(f, "the daemon sent a malformed reply: {error:?}")
            }
            Self::Daemon(client::Error::Refused(code)) => {
                write!(f, "the daemon refused: {}", refusal(*code))
            }
            Self::Daemon(client::Error::Unexpected { command, tag }) => {
                write!(
                    f,
                    "the daemon sent command {command} for tag {tag}, unasked"
                )
            }
            Self::Daemon(client::Error::UnexpectedData) => {
                f.write_str("the daemon sent audio to a control client")
            }
            Self::Daemon(client::Error::Unsupported(version)) => write!(
                f,
                "the daemon speaks protocol {version}; this client needs {}",
                proto::VERSION
            ),
            Self::Daemon(client::Error::Flooded) => {
                f.write_str("the daemon kept sending without answering")
            }
            Self::Daemon(client::Error::Broken) => f.write_str("the connection is broken"),
            Self::Daemon(client::Error::AlreadyPlaying | client::Error::NotPlaying) => {
                f.write_str("td-audio asked for a stream its connection does not play")
            }
            Self::Daemon(client::Error::Killed) => {
                f.write_str("the daemon ended the stream: no output is left to play it")
            }
            Self::NoApp(name) => write!(f, "no stream of app {}", Token(name)),
            Self::Output(error) => write!(f, "write: {error}"),
            Self::Memory => f.write_str("out of memory"),
        }
    }
}

fn refusal(code: u32) -> &'static str {
    match code {
        error::ACCESS => "access denied",
        error::INVALID => "invalid argument",
        error::NOENTITY => "no such entity",
        error::NOTSUPPORTED => "not supported",
        error::TOOLARGE => "too large",
        error::NOTIMPLEMENTED => "not implemented, or not for this account",
        _ => "an error td-audio does not name",
    }
}

/// Text from the daemon as one token: what a terminal must not be handed
/// dropped, and whitespace made `_`, so a line splits on spaces; `-` when
/// nothing is left, so no field goes missing.
struct Token<'a>(&'a str);

/// Text from the daemon to the end of a line: what a terminal must not be
/// handed dropped.
struct Text<'a>(&'a str);

fn token_chars(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .filter(|character| shown(*character))
        .map(|character| {
            if character.is_whitespace() {
                '_'
            } else {
                character
            }
        })
}

impl fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut empty = true;
        for character in token_chars(self.0) {
            empty = false;
            fmt::Write::write_char(f, character)?;
        }
        if empty {
            f.write_str("-")?;
        }
        Ok(())
    }
}

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .chars()
            .filter(|character| shown(*character))
            .try_for_each(|character| fmt::Write::write_char(f, character))
    }
}

fn on_off(muted: bool) -> &'static str {
    if muted {
        "on"
    } else {
        "off"
    }
}

/// The application a stream is grouped and addressed under: its
/// `application.name` as a token, which td's daemon sets from the uid for a
/// confined application, or `client-N` for a stream whose client never
/// named itself, or named itself `client-` anything, which would pass for
/// another client's.
#[derive(Clone, Copy)]
enum App<'a> {
    Named(&'a str),
    Client(u32),
}

impl<'a> App<'a> {
    fn of(stream: &'a SinkInput) -> Self {
        match stream.property("application.name") {
            Some(name)
                if token_chars(name).next().is_some()
                    && !token_chars(name)
                        .take(CLIENT_PREFIX.len())
                        .eq(CLIENT_PREFIX.chars()) =>
            {
                Self::Named(name)
            }
            _ => Self::Client(stream.client),
        }
    }

    /// Whether `text`, as typed, names this application.
    fn is(self, text: &str) -> bool {
        match self {
            Self::Named(name) => token_chars(name).eq(text.chars()),
            Self::Client(index) => text.strip_prefix(CLIENT_PREFIX).is_some_and(|digits| {
                let decimal = digits.bytes().all(|byte| byte.is_ascii_digit())
                    && (digits == "0" || !digits.starts_with('0'));
                decimal && digits.parse::<u32>().ok() == Some(index)
            }),
        }
    }

    fn same(self, other: App<'_>) -> bool {
        match (self, other) {
            (Self::Named(one), App::Named(two)) => token_chars(one).eq(token_chars(two)),
            (Self::Client(one), App::Client(two)) => one == two,
            _ => false,
        }
    }
}

impl fmt::Display for App<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => Token(name).fmt(f),
            Self::Client(index) => write!(f, "{CLIENT_PREFIX}{index}"),
        }
    }
}

const CLIENT_PREFIX: &str = "client-";

/// An application's level is its loudest stream's, and it is muted when
/// every stream is: on td's daemon a confined application's streams all
/// show the one level the human set for it.
struct Summary {
    volume: u32,
    muted: bool,
    streams: usize,
}

fn summary(streams: &Listing<SinkInput>, belongs: impl Fn(&SinkInput) -> bool) -> Summary {
    let mut summary = Summary {
        volume: 0,
        muted: true,
        streams: 0,
    };
    for stream in streams.iter().flatten().filter(|stream| belongs(stream)) {
        summary.volume = summary.volume.max(level(&stream.volume));
        summary.muted &= stream.muted;
        summary.streams += 1;
    }
    summary
}

/// Every entry of a listing parses, so later walks may skip none.
fn whole<T>(listing: &Listing<T>) -> Result<(), Failure<'static>> {
    for entry in listing.iter() {
        entry?;
    }
    Ok(())
}

/// Write every stream of the application `name`. A stream that ended since
/// the list was read answers `NOENTITY` and is passed over; the command
/// fails only when no write landed.
fn each_stream<'a>(
    streams: impl Iterator<Item = SinkInput>,
    name: &'a str,
    mut write: impl FnMut(&SinkInput) -> Result<(), client::Error<io::Error>>,
) -> Result<(), Failure<'a>> {
    let mut landed = false;
    for stream in streams {
        if !App::of(&stream).is(name) {
            continue;
        }
        match write(&stream) {
            Ok(()) => landed = true,
            Err(client::Error::Refused(error::NOENTITY)) => {}
            Err(other) => return Err(Failure::Daemon(other)),
        }
    }
    if landed {
        Ok(())
    } else {
        Err(Failure::NoApp(name))
    }
}

fn title(stream: &SinkInput) -> &str {
    stream
        .property("media.name")
        .or(stream.name.as_deref())
        .unwrap_or("")
}

fn execute<'a>(
    client: &mut Client<UnixSocket>,
    command: &Command<'a>,
    out: &mut dyn Write,
    subscribed: &mut dyn FnMut(),
) -> Result<(), Failure<'a>> {
    let output = Failure::Output;
    match *command {
        Command::Status => {
            let sink = client.sink(DEFAULT_SINK)?;
            let streams = client.sink_inputs()?;
            whole(&streams)?;
            let name = sink.name.as_deref().unwrap_or("");
            writeln!(out, "output {}", Token(name)).map_err(output)?;
            writeln!(out, "volume {}%", percent(level(&sink.volume))).map_err(output)?;
            writeln!(out, "mute {}", on_off(sink.muted)).map_err(output)?;
            writeln!(out, "streams {}", streams.iter().count()).map_err(output)?;
        }
        Command::Outputs => {
            let server = client.server_info()?;
            let sinks = client.sinks()?;
            whole(&sinks)?;
            for sink in sinks.iter().flatten() {
                let active = sink.name.is_some() && sink.name == server.default_sink;
                writeln!(
                    out,
                    "output {} {} {} {}",
                    sink.index,
                    Token(sink.name.as_deref().unwrap_or("")),
                    if active { "active" } else { "-" },
                    Text(sink.description.as_deref().unwrap_or("")),
                )
                .map_err(output)?;
            }
        }
        Command::Output(name) => {
            client.set_default_sink(name)?;
            // What the daemon made default, by its name: `0` or
            // `@DEFAULT_SINK@` resolve to one.
            let server = client.server_info()?;
            writeln!(
                out,
                "output {}",
                Token(server.default_sink.as_deref().unwrap_or(""))
            )
            .map_err(output)?;
        }
        Command::Volume(wanted) => {
            let sink = client.sink(DEFAULT_SINK)?;
            let mut shown = percent(level(&sink.volume));
            if let Some(wanted) = wanted {
                shown = resolve(wanted, shown);
                let mut levels = [0; tag::CHANNELS_MAX];
                let levels = levels_like(&mut levels, &sink.volume, volume_of(shown));
                client.set_sink_volume(sink.index, levels)?;
            }
            writeln!(out, "volume {shown}%").map_err(output)?;
        }
        Command::Mute(wanted) => {
            let sink = client.sink(DEFAULT_SINK)?;
            let mut muted = sink.muted;
            if let Some(wanted) = wanted {
                muted = switched(wanted, muted);
                client.set_sink_mute(sink.index, muted)?;
            }
            writeln!(out, "mute {}", on_off(muted)).map_err(output)?;
        }
        Command::Apps => {
            let streams = client.sink_inputs()?;
            whole(&streams)?;
            for (at, stream) in streams.iter().flatten().enumerate() {
                let app = App::of(&stream);
                let seen = streams
                    .iter()
                    .flatten()
                    .take(at)
                    .any(|earlier| App::of(&earlier).same(app));
                if seen {
                    continue;
                }
                let summary = summary(&streams, |other| App::of(other).same(app));
                writeln!(
                    out,
                    "app {app} volume {}% mute {} streams {}",
                    percent(summary.volume),
                    on_off(summary.muted),
                    summary.streams
                )
                .map_err(output)?;
                for member in streams.iter().flatten() {
                    if !App::of(&member).same(app) {
                        continue;
                    }
                    writeln!(
                        out,
                        "stream {} client {} app {app} volume {}% mute {} title {}",
                        member.index,
                        member.client,
                        percent(level(&member.volume)),
                        on_off(member.muted),
                        Text(title(&member)),
                    )
                    .map_err(output)?;
                }
            }
        }
        Command::AppVolume(name, wanted) => {
            let streams = client.sink_inputs()?;
            whole(&streams)?;
            let current = summary(&streams, |stream| App::of(stream).is(name));
            if current.streams == 0 {
                return Err(Failure::NoApp(name));
            }
            let target = resolve(wanted, percent(current.volume));
            each_stream(streams.iter().flatten(), name, |stream| {
                let mut levels = [0; tag::CHANNELS_MAX];
                let levels = levels_like(&mut levels, &stream.volume, volume_of(target));
                client.set_sink_input_volume(stream.index, levels)
            })?;
            writeln!(out, "app {} volume {target}%", Token(name)).map_err(output)?;
        }
        Command::AppMute(name, wanted) => {
            let streams = client.sink_inputs()?;
            whole(&streams)?;
            let current = summary(&streams, |stream| App::of(stream).is(name));
            if current.streams == 0 {
                return Err(Failure::NoApp(name));
            }
            let muted = switched(wanted, current.muted);
            each_stream(streams.iter().flatten(), name, |stream| {
                client.set_sink_input_mute(stream.index, muted)
            })?;
            writeln!(out, "app {} mute {}", Token(name), on_off(muted)).map_err(output)?;
        }
        Command::Play(milliseconds) => {
            let spec = Spec::fixed();
            let frames = spec.usec_to_frames(milliseconds.saturating_mul(1000));
            let playback = client.create_playback(
                tag::SampleSpec {
                    format: proto::format::SAMPLE_S16LE,
                    channels: u8::try_from(spec.channels).unwrap_or(2),
                    rate: spec.rate,
                },
                &CHANNEL_MAP,
                &[tag::text_property("media.name", "tone")],
            )?;
            let mut generator = Generator::new(spec, Tone::fixture());
            // Reserved once: `fill` then never grows it.
            let mut pcm = crate::allocation::with_capacity(
                PLAY_CHUNK_FRAMES.saturating_mul(spec.frame_bytes),
            )
            .map_err(|_| Failure::Memory)?;
            while generator.frames_produced() < frames {
                let left = frames.saturating_sub(generator.frames_produced());
                let chunk = usize::try_from(left)
                    .map_or(PLAY_CHUNK_FRAMES, |left| left.min(PLAY_CHUNK_FRAMES));
                generator.fill(&mut pcm, chunk);
                client.write_all(&playback, &pcm)?;
            }
            client.drain(&playback)?;
            let report = client.playback_report();
            let stream = playback.index();
            client.delete_playback(playback)?;
            writeln!(out, "stream {stream}").map_err(output)?;
            writeln!(out, "frames {frames}").map_err(output)?;
            writeln!(out, "started {}", report.started).map_err(output)?;
            writeln!(out, "underflows {}", report.underflows).map_err(output)?;
            writeln!(out, "overflows {}", report.overflows).map_err(output)?;
        }
        Command::Watch => {
            client.subscribe(subscription::MASK_SINK | subscription::MASK_SINK_INPUT)?;
            // Events come when something changes, which may be never.
            client.transport_mut().no_deadline();
            subscribed();
            loop {
                let event = client.next_event()?;
                writeln!(
                    out,
                    "{} {} {}",
                    Kind(event.kind),
                    Facility(event.facility),
                    event.index
                )
                .map_err(output)?;
                out.flush().map_err(output)?;
            }
        }
    }
    Ok(())
}

struct Kind(u32);
struct Facility(u32);

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            subscription::EVENT_NEW => f.write_str("new"),
            subscription::EVENT_CHANGE => f.write_str("change"),
            subscription::EVENT_REMOVE => f.write_str("remove"),
            other => write!(f, "kind-{other}"),
        }
    }
}

impl fmt::Display for Facility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            subscription::EVENT_SINK => f.write_str("sink"),
            subscription::EVENT_SINK_INPUT => f.write_str("sink-input"),
            other => write!(f, "facility-{other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::mixer::VOLUME_NORM;
    use crate::serve::tests::{
        admit_as, auth_packet, browser_policy, client_name_packet, created_sink_input,
        drain_socket, named_create_request, offer_outputs, own_uid, BROWSER_UID,
    };
    use crate::serve::Server;
    use crate::sink::{AudioSink, MemorySink, Spec};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread::{self, JoinHandle};
    use std::time::Instant;
    use td_fs::TempDir;

    /// A daemon serving on its own thread, as the CLI meets one. Dropping it,
    /// an assertion's unwinding included, stops the thread and removes the
    /// socket's directory.
    struct Running {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<Server<MemorySink>>>,
        _dir: TempDir,
    }

    impl Running {
        /// Stop serving: every connection closes as the daemon goes away.
        fn stop(mut self) -> Server<MemorySink> {
            self.halt().unwrap()
        }

        fn halt(&mut self) -> Option<Server<MemorySink>> {
            self.stop.store(true, Ordering::SeqCst);
            let mut server = self.thread.take()?.join().ok()?;
            server.shutdown();
            Some(server)
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.halt();
        }
    }

    /// A daemon whose browser account is `BROWSER_UID`, set up by `setup`
    /// before it starts serving on its own thread.
    fn daemon(name: &str, setup: impl FnOnce(&mut Server<MemorySink>, &Path)) -> Running {
        playing_daemon(name, 0, setup)
    }

    /// `daemon`, whose device plays `frames` after each pass and a pause, so
    /// a stream's audio drains at about the rate the device's clock would
    /// drain it.
    fn playing_daemon(
        name: &str,
        frames: u64,
        setup: impl FnOnce(&mut Server<MemorySink>, &Path),
    ) -> Running {
        let dir = TempDir::new(name).unwrap();
        let path = dir.path().join("native");
        let mut sink = MemorySink::new(Spec::fixed(), 8_192, 1_024);
        sink.start().unwrap();
        let mut server = Server::bind(&path, sink, browser_policy(own_uid())).unwrap();
        setup(&mut server, &path);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                server.pass().unwrap();
                if frames > 0 {
                    server.device_mut().advance(frames);
                    thread::sleep(Duration::from_millis(20));
                }
            }
            server
        });
        Running {
            path,
            stop,
            thread: Some(thread),
            _dir: dir,
        }
    }

    /// A browser connection, admitted as `BROWSER_UID`, playing one stream
    /// per title; the connection and each stream's index.
    fn browser(server: &mut Server<MemorySink>, titles: &[&str]) -> (UnixStream, Vec<u32>) {
        let mut player = admit_as(server, BROWSER_UID, 7);
        player.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut player);
        let mut streams = Vec::new();
        for (tag, title) in (3..).zip(titles) {
            player.write_all(&named_create_request(tag, title)).unwrap();
            server.pass().unwrap();
            server.pass().unwrap();
            streams.push(created_sink_input(&drain_socket(&mut player)).unwrap());
        }
        (player, streams)
    }

    /// A controller connection named `application`, playing one stream.
    fn named_controller(
        server: &mut Server<MemorySink>,
        path: &Path,
        application: &str,
    ) -> (UnixStream, u32) {
        let mut socket = UnixStream::connect(path).unwrap();
        socket.write_all(&auth_packet()).unwrap();
        server.pass().unwrap();
        socket
            .write_all(&client_name_packet(1, application))
            .unwrap();
        server.pass().unwrap();
        let _ = drain_socket(&mut socket);
        socket
            .write_all(&named_create_request(3, "a bell"))
            .unwrap();
        server.pass().unwrap();
        server.pass().unwrap();
        let stream = created_sink_input(&drain_socket(&mut socket)).unwrap();
        (socket, stream)
    }

    fn arguments(socket: &Path, words: &[&str]) -> Vec<OsString> {
        let mut arguments = vec![OsString::from("--socket"), socket.as_os_str().to_owned()];
        arguments.extend(words.iter().map(OsString::from));
        arguments
    }

    /// Run a personality as the CLI would, against `socket`, within
    /// `deadline`.
    fn cli_within(
        deadline: Duration,
        socket: &Path,
        personality: &str,
        words: &[&str],
    ) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let arguments = arguments(socket, words);
        let code = run(
            personality,
            &arguments,
            &mut out,
            &mut err,
            deadline,
            &mut || {},
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn cli(socket: &Path, personality: &str, words: &[&str]) -> (u8, String, String) {
        cli_within(DEADLINE, socket, personality, words)
    }

    fn ok(socket: &Path, personality: &str, words: &[&str]) -> String {
        let (code, out, err) = cli(socket, personality, words);
        assert_eq!((code, err.as_str()), (0, ""), "{personality} {words:?}");
        out
    }

    /// `output NAME` makes an output the one that plays, by name or by
    /// number, and says which by name; `outputs` and `status` follow it. An
    /// output the daemon has not is refused, an output that will not open
    /// is refused with the old one still playing, and a missing or empty
    /// name is a usage error.
    #[test]
    fn output_switches_the_output_that_plays() {
        let daemon = daemon("cli-output", |server, _| {
            offer_outputs(
                server,
                &[
                    (0, "pch-0", "ALC892 Analog"),
                    (1, "device-0", "USB Audio"),
                    (7, "locked-0", "Locked"),
                    (8, "small-0", "Small Ring"),
                ],
            );
        });
        let socket = daemon.path.clone();
        let at = |personality: &str, words: &[&str]| ok(&socket, personality, words);
        assert_eq!(
            at("outputs", &[]),
            "output 0 pch-0 active ALC892 Analog\n\
             output 1 device-0 - USB Audio\n\
             output 2 locked-0 - Locked\n\
             output 3 small-0 - Small Ring\n"
        );
        assert_eq!(at("output", &["device-0"]), "output device-0\n");
        assert_eq!(
            at("outputs", &[]),
            "output 0 pch-0 - ALC892 Analog\n\
             output 1 device-0 active USB Audio\n\
             output 2 locked-0 - Locked\n\
             output 3 small-0 - Small Ring\n"
        );
        assert_eq!(at("output", &["0"]), "output pch-0\n");
        assert_eq!(
            at("status", &[]),
            "output pch-0\nvolume 100%\nmute off\nstreams 0\n"
        );
        for (name, why) in [
            ("hdmi-9", "no such entity"),
            ("locked-0", "access denied"),
            ("small-0", "not supported"),
        ] {
            let (code, out, err) = cli(&socket, "output", &[name]);
            assert_eq!(
                (code, out.as_str(), err),
                (1, "", format!("td-audio: the daemon refused: {why}\n")),
                "{name}"
            );
        }
        assert_eq!(at("output", &["@DEFAULT_SINK@"]), "output pch-0\n");
        let long = "a".repeat(td_pulse::tag::STRING_MAX + 1);
        for words in [
            &[][..],
            &[""],
            &["a", "b"],
            &["--help"],
            &["-h"],
            &["--socket"],
            &[long.as_str()],
        ] {
            let (code, out, err) = cli(&socket, "output", words);
            assert_eq!(
                (code, out.as_str(), err.as_str()),
                (
                    2,
                    "",
                    "td-audio: usage:\n  td-audio output [--socket PATH] NAME\n"
                ),
                "{words:?}"
            );
        }
    }

    /// The personalities this module answers for, the usage forms, and the
    /// commands `main` runs itself are one set: a personality dropped from
    /// the list would leave its usage line, and its tests, pointing at a
    /// command the binary no longer has.
    #[test]
    fn every_usage_form_has_a_command_and_every_personality_a_form() {
        const RUN_BY_MAIN: &[&str] = &["devices", "tone", "verify-tone", "serve", "probe"];
        let forms: Vec<&str> = crate::USAGE
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with("td-audio ") || line.starts_with("usage: td-audio "))
            .filter_map(|line| line.trim_start_matches("usage: ").split(' ').nth(1))
            .collect();
        for form in &forms {
            assert!(
                PERSONALITIES.contains(form) || RUN_BY_MAIN.contains(form),
                "{form} has a usage form and no command"
            );
        }
        for personality in PERSONALITIES {
            assert!(
                forms.contains(personality),
                "{personality} has no usage form"
            );
        }
    }

    /// The tone's left channel, with the zeros a device plays while a
    /// stream is short taken out: what is left must be the whole tone, in
    /// order, with nothing lost and nothing played twice.
    fn heard(samples: &[i16]) -> Vec<i16> {
        samples
            .iter()
            .step_by(2)
            .copied()
            .filter(|sample| *sample != 0)
            .collect()
    }

    /// `play` plays the tone through the daemon as a stream: every frame
    /// reaches the device, in order, and the stream starts once.
    #[test]
    fn play_plays_the_tone_as_a_stream() {
        let daemon = playing_daemon("cli-play", 1024, |_, _| {});
        let socket = daemon.path.clone();
        let out = ok(&socket, "play", &["200"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "{out}");
        assert!(lines[0].starts_with("stream "), "{out}");
        assert_eq!(lines[1], "frames 9600");
        assert_eq!(lines[2], "started 1");
        assert!(lines[3].starts_with("underflows "), "{out}");
        assert_eq!(lines[4], "overflows 0");
        let server = daemon.stop();
        let expected: Vec<i16> = (0..9600)
            .map(|frame| Generator::sample_at(Spec::fixed(), Tone::fixture(), frame))
            .filter(|sample| *sample != 0)
            .collect();
        assert_eq!(heard(&server_samples(server)), expected);
    }

    fn server_samples(mut server: Server<MemorySink>) -> Vec<i16> {
        server.device_mut().samples()
    }

    /// A switch while `play` plays moves its stream to the new output, and
    /// the tone plays on there to its end: the client sees no seam.
    #[test]
    fn play_plays_on_across_a_switch() {
        let daemon = playing_daemon("cli-play-switch", 1024, |server, _| {
            offer_outputs(
                server,
                &[(0, "pch-0", "ALC892 Analog"), (1, "device-0", "USB Audio")],
            );
        });
        let socket = daemon.path.clone();
        let player = {
            let socket = socket.clone();
            thread::spawn(move || cli(&socket, "play", &["1000"]))
        };
        let begun = Instant::now();
        while !ok(&socket, "status", &[]).contains("streams 1") {
            assert!(begun.elapsed() < DEADLINE, "the stream never appeared");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ok(&socket, "output", &["device-0"]), "output device-0\n");
        let (code, out, err) = player.join().unwrap();
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.contains("frames 48000\n"), "{out}");
        let server = daemon.stop();
        let tail = heard(&server_samples(server));
        assert!(!tail.is_empty(), "the new output played nothing");
        // What the new output played is the tone's end, whole: no frame of
        // it lost to the switch or sent once where it was owed twice.
        let tone: Vec<i16> = (0..48_000)
            .map(|frame| Generator::sample_at(Spec::fixed(), Tone::fixture(), frame))
            .filter(|sample| *sample != 0)
            .collect();
        assert!(
            tone.ends_with(&tail),
            "the new output played {} samples that are not the tone's last",
            tail.len()
        );
    }

    /// `play` takes one length, 1 ms to a minute; anything else is a usage
    /// error that never reaches the daemon.
    #[test]
    fn play_takes_one_length_in_bounds() {
        let socket = Path::new("/nonexistent/td-audio-play");
        for words in [
            &[][..],
            &["0"],
            &["60001"],
            &["-5"],
            &["+5"],
            &["1.5"],
            &["abc"],
            &["1", "2"],
        ] {
            let (code, out, err) = cli(socket, "play", words);
            assert_eq!(
                (code, out.as_str(), err.as_str()),
                (
                    2,
                    "",
                    "td-audio: usage:\n  td-audio play [--socket PATH] MILLISECONDS\n"
                ),
                "{words:?}"
            );
        }
        let (code, _, err) = cli(socket, "play", &["60000"]);
        assert_eq!(code, 1, "{err}");
        assert!(err.starts_with("td-audio: connect "), "{err}");
    }

    /// Lines in an order-free form: a session's streams are not ordered.
    fn sorted(text: &str) -> Vec<String> {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    }

    /// Every personality, against a daemon with a browser tab playing: the
    /// exact lines each prints, and that the app writes reached the human's
    /// level for the browser rather than its own stream.
    #[test]
    fn each_personality_prints_its_lines() {
        let mut setup = None;
        let daemon = daemon("cli-lines", |server, _| {
            setup = Some(browser(server, &["a\u{1b}[2J tab"]));
        });
        let (_player, streams) = setup.unwrap();
        let tab = streams[0];
        let socket = daemon.path.clone();
        let at = |personality: &str, words: &[&str]| ok(&socket, personality, words);
        assert_eq!(
            at("status", &[]),
            "output td-audio\nvolume 100%\nmute off\nstreams 1\n"
        );
        assert_eq!(at("outputs", &[]), "output 0 td-audio active td audio\n");
        assert_eq!(at("volume", &["40%"]), "volume 40%\n");
        assert_eq!(at("volume", &["+5%"]), "volume 45%\n");
        assert_eq!(at("volume", &[]), "volume 45%\n");
        assert_eq!(at("volume", &["-50"]), "volume 0%\n");
        assert_eq!(at("volume", &["400%"]), "volume 100%\n", "no boost");
        assert_eq!(at("volume", &["-5%"]), "volume 95%\n");
        assert_eq!(at("mute", &["toggle"]), "mute on\n");
        assert_eq!(at("mute", &[]), "mute on\n");
        assert_eq!(at("mute", &["off"]), "mute off\n");
        assert_eq!(
            at("apps", &[]),
            format!(
                "app firefox volume 100% mute off streams 1\n\
                 stream {tab} client 0 app firefox volume 100% mute off title a[2J tab\n"
            )
        );
        assert_eq!(
            at("app", &["firefox", "volume", "25%"]),
            "app firefox volume 25%\n"
        );
        assert_eq!(
            at("app", &["firefox", "volume", "+5%"]),
            "app firefox volume 30%\n"
        );
        assert_eq!(
            at("app", &["firefox", "mute", "toggle"]),
            "app firefox mute on\n"
        );
        assert_eq!(
            at("apps", &[]),
            format!(
                "app firefox volume 30% mute on streams 1\n\
                 stream {tab} client 0 app firefox volume 30% mute on title a[2J tab\n"
            )
        );
        assert_eq!(
            at("status", &[]),
            "output td-audio\nvolume 95%\nmute off\nstreams 1\n"
        );
        let (master, app) = daemon.stop().levels();
        assert_eq!(master, volume_of(95));
        assert_eq!(app, Some((volume_of(30), true)), "the browser's level");
    }

    /// Two browser streams and a named controller's: `apps` groups them by
    /// application, and an `app` write reaches every stream of the one it
    /// names and no other.
    #[test]
    fn app_writes_reach_every_stream_of_one_application() {
        let mut setup = None;
        let daemon = daemon("cli-apps", |server, path| {
            let browser = browser(server, &["a tab", "b tab"]);
            setup = Some((browser, named_controller(server, path, "td mixer")));
        });
        let ((_player, tabs), (_mixer, bell)) = setup.unwrap();
        let socket = daemon.path.clone();
        let (a, b) = (tabs[0], tabs[1]);
        let apps = ok(&socket, "apps", &[]);
        assert!(apps.starts_with("app "), "{apps}");
        assert_eq!(
            sorted(&apps),
            sorted(&format!(
                "app firefox volume 100% mute off streams 2\n\
                 stream {a} client 0 app firefox volume 100% mute off title a tab\n\
                 stream {b} client 0 app firefox volume 100% mute off title b tab\n\
                 app td_mixer volume 100% mute off streams 1\n\
                 stream {bell} client 1 app td_mixer volume 100% mute off title a bell\n"
            ))
        );
        ok(&socket, "app", &["firefox", "mute", "on"]);
        ok(&socket, "app", &["td_mixer", "volume", "50%"]);
        assert_eq!(
            sorted(&ok(&socket, "apps", &[])),
            sorted(&format!(
                "app firefox volume 100% mute on streams 2\n\
                 stream {a} client 0 app firefox volume 100% mute on title a tab\n\
                 stream {b} client 0 app firefox volume 100% mute on title b tab\n\
                 app td_mixer volume 50% mute off streams 1\n\
                 stream {bell} client 1 app td_mixer volume 50% mute off title a bell\n"
            ))
        );
        let (_, app) = daemon.stop().levels();
        assert_eq!(app, Some((VOLUME_NORM, true)));
    }

    fn stream(index: u32, client: u32, application: Option<&str>) -> SinkInput {
        SinkInput {
            index,
            name: Some("a stream".into()),
            client,
            sink: 0,
            volume: vec![VOLUME_NORM; 2],
            muted: false,
            properties: application
                .map(|name| vec![tag::text_property("application.name", name)])
                .unwrap_or_default(),
        }
    }

    /// A stream that ended between the list and its write is passed over,
    /// and the rest of the application is still written. Only when every
    /// write found its stream gone does the command fail.
    #[test]
    fn a_stream_that_ended_is_passed_over() {
        let streams = [
            stream(1, 0, Some("firefox")),
            stream(2, 0, Some("firefox")),
            stream(3, 1, Some("td-mixer")),
        ];
        let mut written = Vec::new();
        each_stream(streams.iter().cloned(), "firefox", |stream| {
            written.push(stream.index);
            match stream.index {
                1 => Err(client::Error::Refused(error::NOENTITY)),
                _ => Ok(()),
            }
        })
        .map_err(|failure| failure.to_string())
        .unwrap();
        assert_eq!(written, vec![1, 2]);
        let all_gone = each_stream(streams.iter().cloned(), "firefox", |_| {
            Err(client::Error::Refused(error::NOENTITY))
        });
        assert!(matches!(all_gone, Err(Failure::NoApp("firefox"))));
        let refused = each_stream(streams.iter().cloned(), "firefox", |_| {
            Err(client::Error::Refused(error::ACCESS))
        });
        assert!(matches!(
            refused,
            Err(Failure::Daemon(client::Error::Refused(error::ACCESS)))
        ));
    }

    /// A client cannot pass for another: a name in the `client-N` form is
    /// that client's own `client-N`, and only the canonical decimal names
    /// a client.
    #[test]
    fn an_app_name_cannot_pass_for_another_client() {
        let impostor = stream(1, 7, Some("client-4"));
        assert_eq!(App::of(&impostor).to_string(), "client-7");
        assert!(!App::of(&impostor).is("client-4"));
        let unnamed = stream(2, 4, None);
        assert!(App::of(&unnamed).is("client-4"));
        for refused in ["client-04", "client-+4", "client-4x", "client-", "4"] {
            assert!(!App::of(&unnamed).is(refused), "{refused}");
        }
        assert!(App::of(&stream(3, 0, None)).is("client-0"));
        assert!(App::of(&stream(4, 9, Some("\u{1b}"))).is("client-9"));
    }

    /// 0 for success, 1 when the daemon refuses or is not there, 2 for a
    /// usage error, which names only the personality's own forms and never
    /// reaches the daemon.
    #[test]
    fn exit_codes_say_whose_fault_it_was() {
        let mut setup = None;
        let daemon = daemon("cli-codes", |server, _| {
            setup = Some(browser(server, &["a tab"]));
        });
        let _player = setup;
        let socket = daemon.path.clone();
        assert_eq!(
            cli(&socket, "app", &["nobody", "mute", "on"]),
            (
                1,
                String::new(),
                "td-audio: no stream of app nobody\n".into()
            )
        );
        for (personality, words) in [
            ("volume", &["4x"][..]),
            ("volume", &["40%", "50%"]),
            ("mute", &["maybe"]),
            ("app", &["firefox", "volume"]),
            ("app", &["firefox", "pan", "left"]),
            ("app", &["", "mute", "on"]),
            ("status", &["now"]),
        ] {
            let (code, out, err) = cli(&socket, personality, words);
            assert_eq!((code, out.as_str()), (2, ""), "{personality} {words:?}");
            assert!(err.starts_with("td-audio: usage:\n"), "{err}");
            let forms: Vec<&str> = err.lines().skip(1).collect();
            assert!(!forms.is_empty());
            for form in forms {
                assert!(
                    form.starts_with(&format!("  td-audio {personality} ")),
                    "{err}"
                );
            }
        }
        drop(daemon);
        let (code, out, err) = cli(&socket, "status", &[]);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(
            err.starts_with(&format!("td-audio: connect {}: ", socket.display())),
            "{err}"
        );
        assert_eq!(
            Failure::Daemon(client::Error::Refused(error::ACCESS)).to_string(),
            "the daemon refused: access denied"
        );
    }

    /// The deadline is the whole command's: a daemon that never answers, and
    /// one that answers a byte at a time, both end it on time with exit 1.
    #[test]
    fn a_daemon_that_stalls_cannot_hold_a_command() {
        let dir = TempDir::new("cli-stall").unwrap();
        let silent = dir.path().join("silent");
        let _listener = UnixListener::bind(&silent).unwrap();
        let deadline = Duration::from_millis(300);
        let started = Instant::now();
        assert_eq!(
            cli_within(deadline, &silent, "status", &[]),
            (
                1,
                String::new(),
                "td-audio: the daemon did not answer in time\n".into()
            )
        );
        assert!(started.elapsed() < Duration::from_secs(3));

        let dripping = dir.path().join("dripping");
        let listener = UnixListener::bind(&dripping).unwrap();
        let drip = thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let descriptor = td_pulse::wire::Descriptor::encode(1_000, u32::MAX, 0, 0);
            let _ = peer.write_all(&descriptor);
            for _ in 0..200 {
                if peer.write_all(&[0]).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let (code, _, err) = cli_within(deadline, &dripping, "status", &[]);
        assert_eq!(
            (code, err.as_str()),
            (1, "td-audio: the daemon did not answer in time\n")
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        drip.join().unwrap();
    }

    /// `watch` prints a line per change the daemon announces, from the
    /// output and from streams, and exits 1 when the daemon goes away. Its
    /// deadline ends once it has subscribed: a quiet daemon is not a dead
    /// one.
    #[test]
    fn watch_prints_each_change_until_the_daemon_goes() {
        let mut ids = None;
        let daemon = daemon("cli-watch", |server, _| {
            ids = Some(browser(server, &["a tab"]));
        });
        let (mut player, tabs) = ids.unwrap();
        let socket = daemon.path.clone();
        let (ready, subscribed) = mpsc::channel();
        let watching = socket.clone();
        let watcher = thread::spawn(move || {
            let arguments = arguments(&watching, &[]);
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let deadline = Duration::from_millis(200);
            let code = run(
                "watch",
                &arguments,
                &mut out,
                &mut err,
                deadline,
                &mut || {
                    ready.send(()).unwrap();
                },
            );
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        });
        subscribed.recv().unwrap();
        thread::sleep(Duration::from_millis(400));
        ok(&socket, "volume", &["50%"]);
        ok(&socket, "app", &["firefox", "mute", "on"]);
        player
            .write_all(&named_create_request(4, "another"))
            .unwrap();
        // The daemon announces a new stream before the next command is read.
        ok(&socket, "status", &[]);
        drop(daemon);
        let (code, out, err) = watcher.join().unwrap();
        assert_eq!(code, 1);
        assert_eq!(err, "td-audio: the daemon closed the connection\n");
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("change sink 0"));
        assert_eq!(
            lines.next().map(str::to_owned),
            Some(format!("change sink-input {}", tabs[0]))
        );
        let created = lines.next().unwrap();
        assert!(created.starts_with("new sink-input "), "{out}");
        assert_eq!(lines.next(), None, "{out}");
    }

    #[test]
    fn levels_parse_and_clamp() {
        assert_eq!(parse_level("40%"), Some(Level::To(40)));
        assert_eq!(parse_level("40"), Some(Level::To(40)));
        assert_eq!(parse_level("+5%"), Some(Level::Up(5)));
        assert_eq!(parse_level("-5"), Some(Level::Down(5)));
        for refused in ["", "%", "+", "-%", "4x", "4.5%", "+-5", " 5", "99999999999"] {
            assert_eq!(parse_level(refused), None, "{refused:?}");
        }
        assert_eq!(resolve(Level::Up(10), 95), 100);
        assert_eq!(resolve(Level::Down(10), 5), 0);
        assert_eq!(resolve(Level::To(250), 5), 100);
    }

    /// Text from the daemon is shown without what a terminal acts on, and a
    /// token keeps one word per field, `-` when nothing is left of it.
    #[test]
    fn daemon_text_is_shown_inert() {
        assert_eq!(
            Token("Pacat Test\u{1b}[2J\u{202e}").to_string(),
            "Pacat_Test[2J"
        );
        assert_eq!(Token("\u{1b}\u{7}").to_string(), "-");
        assert_eq!(Token("").to_string(), "-");
        assert_eq!(Text("a tab\u{7}\u{2066} title").to_string(), "a tab title");
        assert!(App::Named("Pacat Test").is("Pacat_Test"));
        assert!(!App::Named("Pacat Test").is("Pacat Test"));
    }
}
