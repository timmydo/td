//! What the human set on the outputs, kept across restarts (APPLICATIONS.md
//! §K.5): the preference list and each output's level and mute, by key, in
//! `/var/lib/td-audio/state`.
//!
//! The file is the audio account's own, in a directory `td-seatd
//! exec-audio` makes for it as root. It is read once, at start; a file that
//! is not exactly this version's shape is ignored, said once, and the
//! defaults apply. It is written whole by a helper thread, never the mixing
//! loop, about two seconds after the last change, or ten after the first
//! change still unwritten: to a temporary file, synced, renamed over the
//! old one, and the directory synced, so a crash leaves the old file or the
//! new. td-svc stops the daemon with `SIGTERM`, which ends it at once as a
//! crash does, so a stop or a crash loses at most that window.
//!
//! ```text
//! td-audio-state 1
//! prefer<TAB>Device:0        the human's choices, most recent first
//! level<TAB>PCH:0<TAB>65536<TAB>0     a level and a mute, by key
//! ```

use crate::mixer::VOLUME_MAX;
use crate::output::MAX_OUTPUTS;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Where the daemon keeps it.
pub const STATE_PATH: &str = "/var/lib/td-audio/state";

/// The first line. A later format is a later version: the current and the
/// previous deployment share `/var`, so each reads only its own.
const VERSION_LINE: &str = "td-audio-state 1";

/// The most of the file read: 64 preferences and 64 levels at their longest
/// fit well within it.
const MAX_STATE_BYTES: u64 = 16 * 1024;

/// The longest key discovery can make: a card id as long as it reads, `:`,
/// and a device number, so the daemon reads back whatever it wrote.
const MAX_KEY_BYTES: usize = crate::device::CARD_ID_BYTES + 1 + 10;

/// Open flags `std` does not name: no symlink, and no wait on a FIFO.
const O_NOFOLLOW: i32 = 0o400_000;
const O_NONBLOCK: i32 = 0o4000;
/// `ELOOP`, what `O_NOFOLLOW` answers a symlink with.
const ELOOP: i32 = 40;

/// How long after the last change the state is written, so a slider being
/// dragged is one write.
pub const SAVE_AFTER: Duration = Duration::from_secs(2);

/// The longest a change waits, however often others follow it.
pub const SAVE_AT_MOST: Duration = Duration::from_secs(10);

/// One output's level and mute, by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    pub key: String,
    pub volume: u32,
    pub muted: bool,
}

/// What was kept.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Saved {
    /// The human's choices, most recent first.
    pub preference: Vec<String>,
    pub levels: Vec<Level>,
}

/// The kept state, `None` when there is none yet. A file that cannot be
/// read, or is not exactly this version's shape, is an error.
pub fn load(path: &Path) -> io::Result<Option<Saved>> {
    let not_plain = || io::Error::new(io::ErrorKind::InvalidData, "it is not a plain file");
    // Opened without following a symlink or waiting on a FIFO, then
    // checked as opened.
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(ELOOP) => return Err(not_plain()),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(not_plain());
    }
    let mut text = String::new();
    let refused = |_| io::Error::from(io::ErrorKind::OutOfMemory);
    crate::allocation::reserve_string(&mut text, 4096).map_err(refused)?;
    let read = file.take(MAX_STATE_BYTES + 1).read_to_string(&mut text)?;
    if u64::try_from(read).map_or(true, |read| read > MAX_STATE_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "it is longer than any this daemon writes",
        ));
    }
    parse(&text)
        .map(Some)
        .map_err(|what| io::Error::new(io::ErrorKind::InvalidData, what))
}

/// The file's text, refused whole for anything but this version's exact
/// shape.
pub fn parse(text: &str) -> Result<Saved, &'static str> {
    let mut lines = text.split_terminator('\n');
    if lines.next() != Some(VERSION_LINE) {
        return Err("it is not a td-audio-state 1 file");
    }
    let refused = |_| "no memory to read it";
    let mut saved = Saved {
        preference: crate::allocation::with_capacity(MAX_OUTPUTS).map_err(refused)?,
        levels: crate::allocation::with_capacity(MAX_OUTPUTS).map_err(refused)?,
    };
    for line in lines {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some("prefer"), Some(key), None, None) => {
                let key = valid_key(key)?;
                if saved.preference.len() >= MAX_OUTPUTS {
                    return Err("it prefers more outputs than are kept");
                }
                if saved.preference.iter().any(|known| known == key) {
                    return Err("it prefers an output twice");
                }
                let key = crate::allocation::string(key).map_err(refused)?;
                crate::allocation::push(&mut saved.preference, key).map_err(refused)?;
            }
            (Some("level"), Some(key), Some(volume), Some(muted)) => {
                if fields.next().is_some() {
                    return Err("a level line has more fields");
                }
                let key = valid_key(key)?;
                if saved.levels.len() >= MAX_OUTPUTS {
                    return Err("it holds more levels than are kept");
                }
                if saved.levels.iter().any(|known| known.key == key) {
                    return Err("it holds an output's level twice");
                }
                let volume = plain_number(volume)
                    .filter(|volume| *volume <= VOLUME_MAX)
                    .ok_or("a level is not a volume")?;
                let muted = match muted {
                    "0" => false,
                    "1" => true,
                    _ => return Err("a mute is neither 0 nor 1"),
                };
                let key = crate::allocation::string(key).map_err(refused)?;
                crate::allocation::push(&mut saved.levels, Level { key, volume, muted })
                    .map_err(refused)?;
            }
            _ => return Err("a line is neither a preference nor a level"),
        }
    }
    if !text.ends_with('\n') {
        return Err("it ends mid-line");
    }
    Ok(saved)
}

/// A key as discovery makes them: an id of `[A-Za-z0-9_-]`, `:`, a device.
fn valid_key(key: &str) -> Result<&str, &'static str> {
    let valid = key.len() <= MAX_KEY_BYTES
        && key.split_once(':').is_some_and(|(id, device)| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                && plain_number(device).is_some()
        });
    if valid {
        Ok(key)
    } else {
        Err("a key is not an output's")
    }
}

/// Plain decimal digits, no sign, no padding.
fn plain_number(text: &str) -> Option<u32> {
    let plain = !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    plain.then(|| text.parse().ok()).flatten()
}

/// The file's text for `preference` and `levels`, which the caller keeps
/// within the caps and free of a key twice.
pub fn render<'a>(
    preference: impl Iterator<Item = &'a str>,
    levels: impl Iterator<Item = (&'a str, u32, bool)>,
) -> Result<String, crate::allocation::Error> {
    let mut text = String::new();
    crate::allocation::reserve_string(&mut text, 4096)?;
    crate::allocation::push_str(&mut text, VERSION_LINE)?;
    crate::allocation::push_str(&mut text, "\n")?;
    for key in preference {
        for part in ["prefer\t", key, "\n"] {
            crate::allocation::push_str(&mut text, part)?;
        }
    }
    let mut digits = [0u8; 10];
    for (key, volume, muted) in levels {
        let volume = decimal(volume, &mut digits);
        let muted = if muted { "1" } else { "0" };
        for part in ["level\t", key, "\t", volume, "\t", muted, "\n"] {
            crate::allocation::push_str(&mut text, part)?;
        }
    }
    Ok(text)
}

/// `value` in decimal, in `digits`.
fn decimal(mut value: u32, digits: &mut [u8; 10]) -> &str {
    let mut start = digits.len();
    loop {
        start -= 1;
        if let Some(digit) = digits.get_mut(start) {
            // A remainder of ten is below 10, so it is one ASCII digit.
            *digit = b'0' + (value % 10) as u8;
        }
        value /= 10;
        if value == 0 || start == 0 {
            break;
        }
    }
    digits
        .get(start..)
        .and_then(|tail| std::str::from_utf8(tail).ok())
        .unwrap_or("0")
}

/// The helper thread that writes the state, and the one-slot channel the
/// audio thread hands it a whole file's text through.
pub struct Saver {
    sender: Option<SyncSender<String>>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    /// Start the writer for `path`, which writes through `path` with the
    /// extension `new` (`state.new` for the default), so `path` cannot be
    /// that name itself.
    pub fn spawn(path: PathBuf) -> io::Result<Self> {
        let temporary = path.with_extension("new");
        if temporary == path {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the state cannot be kept under its own temporary name",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel::<String>(1);
        let name = crate::allocation::string("td-audio-state")
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let thread = std::thread::Builder::new().name(name).spawn(move || {
            let mut failing = false;
            for text in receiver {
                match write(&path, &temporary, &text) {
                    Ok(()) if failing => {
                        failing = false;
                        let _ = writeln!(io::stderr(), "td-audio: the state is kept again");
                    }
                    Ok(()) => {}
                    Err(error) if !failing => {
                        failing = true;
                        let _ = writeln!(
                            io::stderr(),
                            "td-audio: the state is not kept: {}: {error}",
                            path.display()
                        );
                    }
                    Err(_) => {}
                }
            }
        })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    /// Hand the writer `text` without waiting. It comes back when the
    /// writer is still writing the last, for the caller to offer again.
    pub fn offer(&self, text: String) -> Option<String> {
        match self.sender.as_ref().map(|sender| sender.try_send(text)) {
            Some(Err(TrySendError::Full(text))) => Some(text),
            // Taken, or the writer is gone: there is nothing to wait for.
            Some(Ok(()) | Err(TrySendError::Disconnected(_))) | None => None,
        }
    }

    /// Hand the writer `text`, waiting for room, then let it finish: what
    /// the daemon does as it stops.
    pub fn finish(&mut self, text: Option<String>) {
        if let (Some(sender), Some(text)) = (self.sender.as_ref(), text) {
            let _ = sender.send(text);
        }
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            let _ = JoinHandle::join(thread);
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.finish(None);
    }
}

/// The directory `path` is in: `.` for a bare name.
pub fn directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Write `text` as the state at `path`: `temporary`, beside it, synced,
/// renamed over it, and the directory synced.
fn write(path: &Path, temporary: &Path, text: &str) -> io::Result<()> {
    let directory = directory(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    fs::File::open(directory)?.sync_all()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::mixer::VOLUME_NORM;

    fn level(key: &str, volume: u32, muted: bool) -> Level {
        Level {
            key: key.to_owned(),
            volume,
            muted,
        }
    }

    /// What is rendered parses back to the same, and the text is the
    /// documented shape.
    #[test]
    fn the_state_renders_and_parses_back() {
        let text = render(
            ["Device:0", "PCH:0"].into_iter(),
            [("PCH:0", VOLUME_NORM, false), ("Device_1:3", 0, true)].into_iter(),
        )
        .unwrap();
        assert_eq!(
            text,
            "td-audio-state 1\nprefer\tDevice:0\nprefer\tPCH:0\n\
             level\tPCH:0\t65536\t0\nlevel\tDevice_1:3\t0\t1\n"
        );
        assert_eq!(
            parse(&text).unwrap(),
            Saved {
                preference: vec!["Device:0".into(), "PCH:0".into()],
                levels: vec![
                    level("PCH:0", VOLUME_NORM, false),
                    level("Device_1:3", 0, true)
                ],
            }
        );
        assert_eq!(parse("td-audio-state 1\n").unwrap(), Saved::default());
        for value in [0, 7, 10, 65536, u32::MAX] {
            assert_eq!(decimal(value, &mut [0; 10]), value.to_string());
        }
        // The longest key discovery can make reads back.
        let longest = format!("{}:{}", "A".repeat(crate::device::CARD_ID_BYTES), u32::MAX);
        let text = render(
            [longest.as_str()].into_iter(),
            [(longest.as_str(), 1, false)].into_iter(),
        )
        .unwrap();
        assert_eq!(
            parse(&text).unwrap().preference,
            std::slice::from_ref(&longest)
        );
        assert!(parse(&format!("td-audio-state 1\nprefer\tA{longest}\n")).is_err());
    }

    /// Anything but this version's exact shape is refused whole.
    #[test]
    fn a_state_not_exactly_this_shape_is_refused() {
        let at = |body: &str| parse(&format!("td-audio-state 1\n{body}"));
        assert!(parse("").is_err());
        assert!(parse("td-audio-state 2\n").is_err(), "a later version");
        assert!(parse("td-audio-state 1").is_err(), "no newline");
        for body in [
            "prefer\tPCH:0",
            "prefer\tPCH:0\nprefer\tPCH:0\n",
            "prefer\tPCH\n",
            "prefer\t:0\n",
            "prefer\tPC H:0\n",
            "prefer\tPCH:+0\n",
            "prefer\tPCH:00\n",
            "prefer\tPCH:0\textra\n",
            "level\tPCH:0\t65536\n",
            "level\tPCH:0\t65536\t2\n",
            "level\tPCH:0\t65537\t0\n",
            "level\tPCH:0\t+1\t0\n",
            "level\tPCH:0\t1\t0\textra\n",
            "level\tPCH:0\t1\t0\nlevel\tPCH:0\t2\t0\n",
            "volume\tPCH:0\t1\t0\n",
            "\n",
        ] {
            assert!(at(body).is_err(), "{body:?}");
        }
        let many = |kind: &str, count: usize| {
            (0..count)
                .map(|card| match kind {
                    "prefer" => format!("prefer\tC{card}:0\n"),
                    _ => format!("level\tC{card}:0\t0\t0\n"),
                })
                .collect::<String>()
        };
        assert!(at(&many("prefer", MAX_OUTPUTS)).is_ok());
        assert!(at(&many("prefer", MAX_OUTPUTS + 1)).is_err());
        assert!(at(&many("level", MAX_OUTPUTS)).is_ok());
        assert!(at(&many("level", MAX_OUTPUTS + 1)).is_err());
    }

    /// Written whole and read back; none yet is no state; a file too long,
    /// or not a plain file, is refused.
    #[test]
    fn the_state_is_written_whole_and_read_back() {
        let dir = td_fs::TempDir::new("td-audio-state").unwrap();
        let path = dir.path().join("state");
        assert_eq!(load(&path).unwrap(), None);
        let text = render(["PCH:0"].into_iter(), [("PCH:0", 5, true)].into_iter()).unwrap();
        let mut saver = Saver::spawn(path.clone()).unwrap();
        assert_eq!(saver.offer(text.clone()), None);
        saver.finish(None);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(!dir.path().join("state.new").exists());
        assert_eq!(
            load(&path).unwrap().unwrap().levels,
            [level("PCH:0", 5, true)]
        );
        // A final state handed over as it stops is written before it does.
        let mut saver = Saver::spawn(path.clone()).unwrap();
        saver.finish(Some(String::from("td-audio-state 1\n")));
        assert_eq!(load(&path).unwrap(), Some(Saved::default()));
        // Refused for its length before it is parsed, so no file costs more
        // than the cap to read.
        fs::write(&path, "x".repeat(MAX_STATE_BYTES as usize + 1)).unwrap();
        let refused = load(&path).unwrap_err().to_string();
        assert!(refused.contains("longer than any"), "{refused}");
        fs::remove_file(&path).unwrap();
        // A symlink, even to a good file, is not the daemon's own file.
        fs::write(dir.path().join("elsewhere"), "td-audio-state 1\n").unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &path).unwrap();
        assert!(load(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir(&path).unwrap();
        // A FIFO is refused, not waited on.
        let made = std::process::Command::new("mkfifo").arg(&path).status();
        if made.is_ok_and(|status| status.success()) {
            assert!(load(&path).is_err());
        }
        // A state named like its own temporary file is refused.
        assert!(Saver::spawn(dir.path().join("state.new")).is_err());
    }

    /// While the writer is busy, what it could not take comes back.
    #[test]
    fn a_busy_writer_hands_the_state_back() {
        let (sender, receiver) = mpsc::sync_channel::<String>(1);
        let saver = Saver {
            sender: Some(sender),
            thread: None,
        };
        assert_eq!(saver.offer(String::from("a")), None);
        assert_eq!(saver.offer(String::from("b")), Some(String::from("b")));
        assert_eq!(receiver.recv().unwrap(), "a");
        drop(receiver);
        assert_eq!(
            saver.offer(String::from("c")),
            None,
            "no writer to wait for"
        );
    }
}
