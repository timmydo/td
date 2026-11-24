//! What the panel shows, read from the daemon in one pass: every output
//! and which plays, the playing output's level, and each application's,
//! grouped and named as `td-audio apps` does (td-pulse's `apps`), and the
//! writes each of the panel's actions asks for.

use td_pulse::apps::{self, shown, App};
use td_pulse::client::{
    self, level, levels_like, percent, volume_of, Client, Transport, DEFAULT_SINK, MAX_PERCENT,
};
use td_pulse::proto::error;
use td_pulse::tag;

/// The most outputs and applications a snapshot keeps: the daemon offers
/// at most 64 outputs, and 128 streams at most could each be another
/// application's.
pub const MAX_OUTPUTS: usize = 64;
pub const MAX_APPS: usize = 128;

/// The most characters of a name the panel keeps; a row shows fewer.
pub const MAX_LABEL_CHARS: usize = 96;

/// A level and its mute, as a whole percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub percent: u32,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The sink name the daemon is told to switch to.
    pub name: String,
    /// What the row says: the output's description, else its name.
    pub label: String,
    pub playing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppLevel {
    /// The application as `td-audio app` takes it: its token, or
    /// `client-N`.
    pub name: String,
    /// What the row says: the name with what a display must not be handed
    /// dropped, its spaces kept.
    pub label: String,
    pub level: Level,
}

/// Everything the panel shows. `master` is the playing output's level;
/// `None` when no output plays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub outputs: Vec<Output>,
    pub master: Option<Level>,
    pub apps: Vec<AppLevel>,
}

/// What an action sets the level or mute of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The output that plays.
    Master,
    /// Every stream of one application, by the name `AppLevel::name` holds.
    App(String),
}

/// One of the panel's actions, as the daemon is asked it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    /// Make the output of this sink name the one that plays.
    Choose(String),
    /// Set a level, in whole percent.
    Level(Target, u32),
    /// Set a mute.
    Mute(Target, bool),
}

/// Why a read or a write did not complete.
#[derive(Debug)]
pub enum Error<E> {
    Daemon(client::Error<E>),
    /// The panel could not hold what the daemon sent.
    Memory,
}

impl<E> From<client::Error<E>> for Error<E> {
    fn from(error: client::Error<E>) -> Self {
        Self::Daemon(error)
    }
}

impl<E> From<td_alloc::Error> for Error<E> {
    fn from(_: td_alloc::Error) -> Self {
        Self::Memory
    }
}

impl<E> From<td_alloc::FormatError> for Error<E> {
    fn from(_: td_alloc::FormatError) -> Self {
        Self::Memory
    }
}

/// What a refusal's code says, as the panel tells it.
pub fn refusal(code: u32) -> &'static str {
    match code {
        error::ACCESS => "no access",
        error::NOENTITY => "no longer there",
        error::NOTSUPPORTED => "that output would not open",
        error::NOTIMPLEMENTED => "not for this account",
        _ => "an error the panel does not name",
    }
}

/// `text` as a row shows it: what `shown` refuses dropped, at most
/// `MAX_LABEL_CHARS` characters.
pub fn label(text: &str) -> Result<String, td_alloc::Error> {
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, text.len().min(MAX_LABEL_CHARS * 4))?;
    let mut buffer = [0; 4];
    for character in text
        .chars()
        .filter(|character| shown(*character))
        .take(MAX_LABEL_CHARS)
    {
        td_alloc::push_str(&mut out, character.encode_utf8(&mut buffer))?;
    }
    Ok(out)
}

/// Every entry of a listing, or the first that does not parse.
fn entries<T, E>(
    listing: &client::Listing<T>,
    mut each: impl FnMut(T) -> Result<(), Error<E>>,
) -> Result<(), Error<E>> {
    for entry in listing.iter() {
        each(entry.map_err(|error: tag::Error| client::Error::Malformed(error))?)?;
    }
    Ok(())
}

/// One snapshot: the server's default, its outputs, its streams.
pub fn read<T: Transport>(client: &mut Client<T>) -> Result<Snapshot, Error<T::Error>> {
    let server = client.server_info()?;
    let sinks = client.sinks()?;
    let mut snapshot = Snapshot::default();
    entries(&sinks, |sink| {
        let playing = sink.name.is_some() && sink.name == server.default_sink;
        if playing {
            snapshot.master = Some(Level {
                percent: percent(level(&sink.volume)),
                muted: sink.muted,
            });
        }
        if snapshot.outputs.len() < MAX_OUTPUTS {
            let name = td_alloc::string(sink.name.as_deref().unwrap_or(""))?;
            let label = label(
                sink.description
                    .as_deref()
                    .filter(|text| text.chars().any(shown))
                    .unwrap_or(&name),
            )?;
            td_alloc::push(
                &mut snapshot.outputs,
                Output {
                    name,
                    label,
                    playing,
                },
            )?;
        }
        Ok(())
    })?;
    let listing = client.sink_inputs()?;
    let mut streams = Vec::new();
    entries(&listing, |stream| Ok(td_alloc::push(&mut streams, stream)?))?;
    for (at, stream) in streams.iter().enumerate() {
        let app = App::of(stream);
        let seen = streams
            .iter()
            .take(at)
            .any(|earlier| App::of(earlier).same(app));
        if seen || snapshot.apps.len() >= MAX_APPS {
            continue;
        }
        let summary = apps::summary(&streams, |other| App::of(other).same(app));
        let label = match app {
            App::Named(name) => label(name)?,
            App::Client(_) => td_alloc::format(format_args!("{app}"))?,
        };
        td_alloc::push(
            &mut snapshot.apps,
            AppLevel {
                name: td_alloc::format(format_args!("{app}"))?,
                label,
                level: Level {
                    percent: percent(summary.volume),
                    muted: summary.muted,
                },
            },
        )?;
    }
    Ok(snapshot)
}

/// Ask the daemon for `job`. A stream that ended between the list and its
/// write is passed over; an application with none left is no error, the
/// read that follows shows it gone.
pub fn apply<T: Transport>(client: &mut Client<T>, job: &Job) -> Result<(), Error<T::Error>> {
    let mut levels = [0; tag::CHANNELS_MAX];
    match job {
        Job::Choose(name) => client.set_default_sink(name)?,
        Job::Level(Target::Master, wanted) => {
            let sink = client.sink(DEFAULT_SINK)?;
            let levels = levels_like(&mut levels, &sink.volume, volume_of(*wanted));
            client.set_sink_volume(sink.index, levels)?;
        }
        Job::Mute(Target::Master, muted) => {
            let sink = client.sink(DEFAULT_SINK)?;
            client.set_sink_mute(sink.index, *muted)?;
        }
        Job::Level(Target::App(name), wanted) => {
            each_stream(client, name, |client, stream| {
                let levels = levels_like(&mut levels, &stream.volume, volume_of(*wanted));
                client.set_sink_input_volume(stream.index, levels)
            })?;
        }
        Job::Mute(Target::App(name), muted) => {
            each_stream(client, name, |client, stream| {
                client.set_sink_input_mute(stream.index, *muted)
            })?;
        }
    }
    Ok(())
}

fn each_stream<T: Transport>(
    client: &mut Client<T>,
    name: &str,
    mut write: impl FnMut(&mut Client<T>, &client::SinkInput) -> client::Result<(), T::Error>,
) -> Result<(), Error<T::Error>> {
    let listing = client.sink_inputs()?;
    let mut streams = Vec::new();
    entries(&listing, |stream| Ok(td_alloc::push(&mut streams, stream)?))?;
    for stream in streams.iter().filter(|stream| App::of(stream).is(name)) {
        match write(client, stream) {
            Ok(()) | Err(client::Error::Refused(error::NOENTITY)) => {}
            Err(other) => return Err(other.into()),
        }
    }
    Ok(())
}

/// `from` one `grid` step up or down, clamped, as the volume keys step: a
/// level off the grid lands on the next mark in the step's direction.
pub fn step(from: u32, up: bool, grid: u32) -> u32 {
    let grid = grid.max(1);
    let from = from.min(MAX_PERCENT);
    let to = if up {
        (from / grid + 1) * grid
    } else if from.is_multiple_of(grid) {
        from.saturating_sub(grid)
    } else {
        from / grid * grid
    };
    to.min(MAX_PERCENT)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn a_label_drops_what_a_display_acts_on_and_is_bounded() {
        assert_eq!(
            label("Built-in\u{202e} Audio\u{1b}").unwrap(),
            "Built-in Audio"
        );
        assert_eq!(
            label(&"é".repeat(MAX_LABEL_CHARS + 5))
                .unwrap()
                .chars()
                .count(),
            MAX_LABEL_CHARS
        );
    }

    #[test]
    fn a_step_lands_on_the_grid_and_clamps() {
        for (from, up, to) in [
            (45, true, 50),
            (47, true, 50),
            (98, true, 100),
            (100, true, 100),
            (45, false, 40),
            (47, false, 45),
            (3, false, 0),
            (0, false, 0),
            (150, true, 100),
        ] {
            assert_eq!(step(from, up, 5), to, "{from} {up}");
        }
    }
}
