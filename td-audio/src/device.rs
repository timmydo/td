//! Playback-device discovery: `/proc/asound/pcm`, and each card's id beside it.
//!
//! §K.4 names the wrong file and says exactly why it is wrong. `/proc/asound/
//! cards` lists CARDS — one entry per adapter, with no playback device or
//! subdevice number — while the node this daemon must open is
//! `/dev/snd/pcmC<card>D<device>p`, whose `<device>` appears only in `pcm`. A
//! card with no playback PCM at all (an HDMI-capture-only adapter, a card whose
//! only stream is capture) is indistinguishable from a usable one in `cards`, so
//! a daemon that guessed `D0p` would open the wrong node or fail on a real
//! machine while passing every test on a single-device fixture.
//!
//! Both are ordinary file reads, so this costs no syscall surface. The format is
//! stable and line-oriented, written by `snd_pcm_proc_read` as
//!
//! ```text
//! %02i-%02i: <id> : <name>[ : playback N][ : capture M]
//! ```
//!
//! and a line that does not parse is REFUSED with a diagnostic naming it. Never
//! guessed: guessing a device number is how a daemon ends up feeding audio to an
//! HDMI port nobody plugged anything into while reporting success.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the kernel publishes the PCM table.
pub const PROC_ASOUND_PCM: &str = "/proc/asound/pcm";

/// The exact id/name pair emitted by the built-in `snd-aloop` test device.
/// It remains explicitly selectable, but it must not become the default sink
/// merely because kernel registration assigned it the lowest card number.
const TEST_LOOPBACK_PCM: &str = "Loopback PCM";

/// One playback-capable PCM, as `/proc/asound/pcm` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    pub card: u32,
    pub device: u32,
    /// The card's short id, e.g. `ALC1220 Analog`.
    pub id: String,
    /// The card's long name.
    pub name: String,
    /// How many playback substreams the device has. At least one, or this is
    /// not a `Playback`.
    pub subdevices: u32,
    /// How many capture substreams it has, if any. Recorded but unused: §K.5
    /// ships no microphone in v1, and this is what makes "the hardware has one
    /// and td declines to open it" a visible fact rather than an absence.
    pub capture_subdevices: u32,
}

impl Playback {
    /// `/dev/snd/pcmC<card>D<device>p`.
    ///
    /// The trailing `p` is the kernel's own playback suffix, which is why the
    /// stream direction never has to be requested: it is in the node name, and
    /// `SNDRV_PCM_IOCTL_INFO` is what confirms the kernel agrees.
    pub fn node(&self) -> PathBuf {
        PathBuf::from(format!("/dev/snd/pcmC{}D{}p", self.card, self.device))
    }
}

impl fmt::Display for Playback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "card {} device {} ({}) — {} playback subdevice(s)",
            self.card, self.device, self.name, self.subdevices
        )
    }
}

/// Parse the whole table, refusing anything that is not the documented shape.
pub fn parse(text: &str) -> io::Result<Vec<Playback>> {
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok(Some(playback)) => found.push(playback),
            Ok(None) => {}
            Err(what) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{PROC_ASOUND_PCM}:{}: {what}: {line:?}", number + 1),
                ));
            }
        }
    }
    Ok(found)
}

/// One line. `Ok(None)` is a well-formed capture-only device.
fn parse_line(line: &str) -> Result<Option<Playback>, &'static str> {
    let (address, rest) = line.split_once(": ").ok_or("no 'CC-DD: ' prefix")?;
    let (card, device) = address.split_once('-').ok_or("no card-device address")?;
    let card: u32 = card.parse().map_err(|_| "card number is not a number")?;
    let device: u32 = device
        .parse()
        .map_err(|_| "device number is not a number")?;

    // `id` and `name` are card-supplied strings that may themselves contain
    // ` : `, so the stream counts are found from the END rather than by
    // splitting the line into fields.
    // The kernel emits one closed suffix chain: `capture N` at the end,
    // optionally preceded immediately by `playback N`, or `playback N` at the
    // end. A digit-looking field inside card text must not become a stream
    // merely because some unrelated ` : ` field follows it.
    let capture = terminal_field(rest, " : capture ")?;
    let playback = match capture {
        // Card text immediately before a real capture suffix may itself end in
        // playback-like nonnumeric text. That is capture-only, not a malformed
        // playback field and certainly not a playback device.
        Some((_, at)) => {
            terminal_field(rest.get(..at).unwrap_or(""), " : playback ").unwrap_or(None)
        }
        None => terminal_field(rest, " : playback ")?,
    };
    // Where the real fields begin, not where the first text that looks like one
    // does: `find` here truncated the name of a card whose own name contained
    // the marker, which is the case the parse below exists to survive.
    let head_end = [playback, capture]
        .iter()
        .filter_map(|field| field.map(|(_, at)| at))
        .min()
        .unwrap_or(rest.len());
    let head = rest.get(..head_end).unwrap_or(rest);
    let (id, name) = match head.split_once(" : ") {
        Some((id, name)) => (id.trim(), name.trim()),
        None => (head.trim(), head.trim()),
    };

    let Some((subdevices, _)) = playback else {
        return Ok(None);
    };
    if subdevices == 0 {
        return Err("a playback device with no subdevices");
    }
    Ok(Some(Playback {
        card,
        device,
        id: sanitise(id),
        name: sanitise(name),
        subdevices,
        capture_subdevices: capture.map(|(count, _)| count).unwrap_or(0),
    }))
}

/// The number in an exact terminal field and where that field starts.
fn terminal_field(rest: &str, marker: &str) -> Result<Option<(u32, usize)>, &'static str> {
    let Some(at) = rest.rfind(marker) else {
        return Ok(None);
    };
    let tail = rest.get(at + marker.len()..).unwrap_or("");
    if tail.contains(" : ") {
        return Ok(None);
    }
    if tail.is_empty() || !tail.chars().all(|character| character.is_ascii_digit()) {
        return Err("a stream count that is not a number");
    }
    tail.parse()
        .map(|count| Some((count, at)))
        .map_err(|_| "a stream count that does not fit")
}

/// Card-supplied text reaches diagnostics and, once the protocol lands, sink
/// descriptions sent to clients. Sanitised here, at the boundary.
fn sanitise(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// One output: a playback PCM the daemon may drive, under a key that follows
/// the card rather than the number plug order gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub playback: Playback,
    /// `<card id>:<device>`, e.g. `PCH:0`, from `/proc/asound/card<N>/id`.
    /// Two identical cards are told apart only by the ids the kernel gives
    /// them in plug order (`Device`, `Device_1`); td reads no USB topology.
    pub key: String,
    /// The Pulse sink name the key gives, e.g. `pch-0`: the id lowercased,
    /// `_` as `-`, then the device number.
    pub name: String,
}

/// Every output, preferred first: the one `wanted` names, else the one
/// `select` would choose, then the rest by card and device. The test
/// loopback is an output only when it is the one named. Card ids are read
/// from `card<N>/id` beside `pcm`; a card with none is keyed `card<N>`. A
/// sink name an earlier output already took, which folding case and `_`
/// can make, gains `.<card>`: no derived name holds a `.`, and a card's
/// number is its own.
pub fn outputs(pcm: &Path, wanted: Option<(u32, u32)>) -> io::Result<Vec<Output>> {
    listed(read(pcm)?, pcm, wanted, false)
}

/// Every output as a running daemon rescans them: as `outputs` finds them,
/// except that a table with no ordinary playback PCM left is an empty list,
/// every output gone, and `wanted` leads only while it is listed. A table
/// that cannot be read or parsed is an error, which the caller answers by
/// keeping what it had, and so is a card id that cannot be read: keyed
/// `card<N>` instead, a playing output would seem gone.
pub fn rescan(pcm: &Path, wanted: Option<(u32, u32)>) -> io::Result<Vec<Output>> {
    let devices = read(pcm)?;
    let wanted = wanted.filter(|(card, device)| {
        devices
            .iter()
            .any(|playback| (playback.card, playback.device) == (*card, *device))
    });
    if wanted.is_none() && !devices.iter().any(|playback| !is_test_loopback(playback)) {
        return Ok(Vec::new());
    }
    listed(devices, pcm, wanted, true)
}

/// `strict`: a card id that cannot be read is an error, not `card<N>`.
fn listed(
    mut devices: Vec<Playback>,
    pcm: &Path,
    wanted: Option<(u32, u32)>,
    strict: bool,
) -> io::Result<Vec<Output>> {
    let first = select(&devices, wanted).map(|first| (first.card, first.device))?;
    devices.sort_unstable_by_key(|playback| {
        (
            (playback.card, playback.device) != first,
            playback.card,
            playback.device,
        )
    });
    let root = pcm.parent().unwrap_or(Path::new("/"));
    let mut found = crate::allocation::with_capacity(devices.len()).map_err(refused)?;
    for playback in devices {
        let chosen = (playback.card, playback.device) == first;
        if !chosen && is_test_loopback(&playback) {
            continue;
        }
        let id = card_id(root, playback.card, strict)?;
        let key =
            crate::allocation::format(format_args!("{id}:{}", playback.device)).map_err(refused)?;
        let mut name =
            crate::allocation::format(format_args!("{}-{}", SinkId(&id), playback.device))
                .map_err(refused)?;
        if found.iter().any(|earlier: &Output| earlier.name == name) {
            name = crate::allocation::format(format_args!("{name}.{}", playback.card))
                .map_err(refused)?;
        }
        crate::allocation::push(
            &mut found,
            Output {
                playback,
                key,
                name,
            },
        )
        .map_err(refused)?;
    }
    Ok(found)
}

/// A card's id as the kernel keeps it, or `card<N>` when it keeps none or,
/// unless `strict`, it cannot be read. Only `[A-Za-z0-9_-]`, the characters
/// the kernel itself allows in an id, survive, so a key can carry nothing
/// else.
fn card_id(root: &Path, card: u32, strict: bool) -> io::Result<String> {
    // One formatted path rather than a join: a root that is not UTF-8,
    // which only a diagnostic `--proc` can give, finds no id and is keyed
    // `card<N>`.
    let file = crate::allocation::format(format_args!("{}/card{card}/id", root.display()))
        .map_err(refused)?;
    // The kernel's id is at most 15 bytes and a newline; a longer file is
    // not one, and only its first bytes are read.
    let mut text = [0u8; CARD_ID_BYTES];
    let length =
        match fs::File::open(&file).and_then(|mut opened| io::Read::read(&mut opened, &mut text)) {
            Ok(length) => length,
            Err(error) if strict => {
                let what =
                    crate::allocation::format(format_args!("{file}: {error}")).map_err(refused)?;
                return Err(io::Error::new(error.kind(), what));
            }
            Err(_) => 0,
        };
    let mut id = String::new();
    crate::allocation::reserve_string(&mut id, length).map_err(refused)?;
    let mut encoded = [0u8; 4];
    for byte in text.get(..length).unwrap_or_default() {
        let character = char::from(*byte);
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            crate::allocation::push_str(&mut id, character.encode_utf8(&mut encoded))
                .map_err(refused)?;
        }
    }
    if id.is_empty() {
        return crate::allocation::format(format_args!("card{card}")).map_err(refused);
    }
    Ok(id)
}

/// The most of `card<N>/id` that is read.
const CARD_ID_BYTES: usize = 64;

/// A card id as a sink name's first part: lowercase, `_` as `-`.
struct SinkId<'a>(&'a str);

impl fmt::Display for SinkId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for character in self.0.chars() {
            let character = match character {
                '_' => '-',
                other => other.to_ascii_lowercase(),
            };
            fmt::Write::write_char(f, character)?;
        }
        Ok(())
    }
}

/// An allocation td-alloc refused, as the I/O error discovery reports.
fn refused<E>(_: E) -> io::Error {
    io::Error::from(io::ErrorKind::OutOfMemory)
}

fn is_test_loopback(playback: &Playback) -> bool {
    playback.id == TEST_LOOPBACK_PCM && playback.name == TEST_LOOPBACK_PCM
}

/// Read and parse the kernel's table.
pub fn read(path: &Path) -> io::Result<Vec<Playback>> {
    let text = fs::read_to_string(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "{}: {e} — is CONFIG_SND enabled in the running kernel?",
                path.display()
            ),
        )
    })?;
    parse(&text)
}

/// The device this daemon will use when nobody named one.
///
/// The lowest non-test card, then the lowest device on it. The image builds
/// `snd-aloop` for an explicit in-guest oracle; selecting it by default would
/// silently discard ordinary Firefox output instead of reaching HDA. An
/// explicit card/device selection may still choose it. Deliberately not a
/// guess when no ordinary playback PCM exists.
pub fn select(devices: &[Playback], wanted: Option<(u32, u32)>) -> io::Result<&Playback> {
    if let Some((card, device)) = wanted {
        return devices
            .iter()
            .find(|p| p.card == card && p.device == device)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "card {card} device {device} is not a playback PCM in {PROC_ASOUND_PCM}"
                    ),
                )
            });
    }
    devices
        .iter()
        .filter(|playback| !is_test_loopback(playback))
        .min_by_key(|p| (p.card, p.device))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{PROC_ASOUND_PCM} lists no non-test playback device; select the loopback \
                     explicitly with --card and --device"
                ),
            )
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Captured verbatim from a real multi-card x86-64 machine: an HD-audio
    /// controller whose only PCMs are two HDMI outputs, plus a codec card with
    /// an analog duplex device, a digital output, and — the case §K.4 is about —
    /// a THIRD device that is capture-only.
    const REAL: &str = "\
00-03: HDMI 0 : HDMI 0 : playback 1
00-07: HDMI 1 : HDMI 1 : playback 1
01-00: ALC1220 Analog : ALC1220 Analog : playback 1 : capture 1
01-01: ALC1220 Digital : ALC1220 Digital : playback 1
01-02: ALC1220 Alt Analog : ALC1220 Alt Analog : capture 1
";

    /// QEMU's `intel-hda` with `hda-duplex`, the §K.5 first target.
    const QEMU_HDA: &str = "00-00: ALC262 Analog : ALC262 Analog : playback 1 : capture 1\n";

    /// `CONFIG_SND_ALOOP`, the in-guest test oracle: two devices, each with
    /// eight substreams, and both directions on both.
    const ALOOP: &str = "\
00-00: Loopback PCM : Loopback PCM : playback 8 : capture 8
00-01: Loopback PCM : Loopback PCM : playback 8 : capture 8
";

    /// A `/proc/asound` for `outputs`: the PCM table, and each card's id.
    fn proc_tree(name: &str, pcm: &str, ids: &[(u32, &str)]) -> td_fs::TempDir {
        let dir = td_fs::TempDir::new(name).unwrap();
        fs::write(dir.path().join("pcm"), pcm).unwrap();
        for (card, id) in ids {
            let card = dir.path().join(format!("card{card}"));
            fs::create_dir(&card).unwrap();
            fs::write(card.join("id"), id).unwrap();
        }
        dir
    }

    fn shown(outputs: &[Output]) -> Vec<(u32, u32, &str, &str)> {
        outputs
            .iter()
            .map(|output| {
                (
                    output.playback.card,
                    output.playback.device,
                    output.key.as_str(),
                    output.name.as_str(),
                )
            })
            .collect()
    }

    /// Every playback PCM is an output under its card's id, the one
    /// `select` picks first and the rest by card and device; naming one
    /// puts it first.
    #[test]
    fn outputs_are_keyed_by_card_id_and_the_chosen_one_is_first() {
        let dir = proc_tree("outputs-real", REAL, &[(0, "HDMI\n"), (1, "PCH\n")]);
        let pcm = dir.path().join("pcm");
        assert_eq!(
            shown(&outputs(&pcm, None).unwrap()),
            [
                (0, 3, "HDMI:3", "hdmi-3"),
                (0, 7, "HDMI:7", "hdmi-7"),
                (1, 0, "PCH:0", "pch-0"),
                (1, 1, "PCH:1", "pch-1"),
            ]
        );
        assert_eq!(
            shown(&outputs(&pcm, Some((1, 0))).unwrap()),
            [
                (1, 0, "PCH:0", "pch-0"),
                (0, 3, "HDMI:3", "hdmi-3"),
                (0, 7, "HDMI:7", "hdmi-7"),
                (1, 1, "PCH:1", "pch-1"),
            ]
        );
        assert!(outputs(&pcm, Some((1, 2))).is_err(), "capture-only");
    }

    /// The test loopback is an output only when named; an id the kernel
    /// would not write is cut to the characters it would, the second of two
    /// identical cards keeps its plug-order suffix, and a card with no id, or
    /// one that cannot be read as a file, is `card<N>`.
    #[test]
    fn the_loopback_stays_out_and_card_ids_are_cut_to_the_kernels_alphabet() {
        let table = format!(
            "{ALOOP}01-00: USB Audio : USB Audio : playback 1\n\
             02-00: USB Audio : USB Audio : playback 1\n\
             03-00: ALC262 Analog : ALC262 Analog : playback 1\n\
             04-00: ALC262 Analog : ALC262 Analog : playback 1\n"
        );
        let dir = proc_tree(
            "outputs-ids",
            &table,
            &[
                (0, "Loopback\n"),
                (1, "Device\n"),
                (2, "Dev ice\u{1b}[2J_1\n"),
            ],
        );
        // A directory where the id would be: not readable as one.
        fs::create_dir_all(dir.path().join("card4/id")).unwrap();
        let pcm = dir.path().join("pcm");
        assert_eq!(
            shown(&outputs(&pcm, None).unwrap()),
            [
                (1, 0, "Device:0", "device-0"),
                (2, 0, "Device2J_1:0", "device2j-1-0"),
                (3, 0, "card3:0", "card3-0"),
                (4, 0, "card4:0", "card4-0"),
            ]
        );
        let named = outputs(&pcm, Some((0, 1))).unwrap();
        assert_eq!(
            shown(&named).first(),
            Some(&(0, 1, "Loopback:1", "loopback-1")),
            "the loopback, named, is the one output it is"
        );
        assert_eq!(named.len(), 5, "and the other loopback device is not one");
    }

    /// Ids the kernel keeps apart can fold to one sink name, by case or by
    /// `_` and `-`; the later output's name gains its card, so every output
    /// can be named. The kernel's own `-` survives into the key.
    #[test]
    fn folded_sink_names_stay_distinct() {
        let table = "00-00: A : A : playback 1\n\
                     01-00: B : B : playback 1\n\
                     02-00: C : C : playback 1\n\
                     03-00: D : D : playback 1\n";
        let dir = proc_tree(
            "outputs-folded",
            table,
            &[
                (0, "PCH\n"),
                (1, "pch\n"),
                (2, "USB_Dac\n"),
                (3, "USB-Dac\n"),
            ],
        );
        assert_eq!(
            shown(&outputs(&dir.path().join("pcm"), None).unwrap()),
            [
                (0, 0, "PCH:0", "pch-0"),
                (1, 0, "pch:0", "pch-0.1"),
                (2, 0, "USB_Dac:0", "usb-dac-0"),
                (3, 0, "USB-Dac:0", "usb-dac-0.3"),
            ]
        );
    }

    #[test]
    fn a_real_table_yields_only_the_playback_devices() {
        let devices = parse(REAL).unwrap();
        assert_eq!(devices.len(), 4, "the capture-only device must not appear");
        assert!(!devices.iter().any(|p| p.card == 1 && p.device == 2));
        let analog = devices
            .iter()
            .find(|p| p.card == 1 && p.device == 0)
            .unwrap();
        assert_eq!(analog.name, "ALC1220 Analog");
        assert_eq!(analog.subdevices, 1);
        assert_eq!(analog.capture_subdevices, 1);
        assert_eq!(
            analog.node().to_string_lossy(),
            "/dev/snd/pcmC1D0p",
            "the node name is built from BOTH numbers"
        );
    }

    /// The bug §K.4 exists to prevent: guessing `D0p` on this machine opens
    /// nothing at all, because card 0's devices are 3 and 7.
    #[test]
    fn the_device_number_is_not_zero_on_a_real_card() {
        let devices = parse(REAL).unwrap();
        let card0: Vec<u32> = devices
            .iter()
            .filter(|p| p.card == 0)
            .map(|p| p.device)
            .collect();
        assert_eq!(card0, vec![3, 7]);
        assert!(!card0.contains(&0));
    }

    #[test]
    fn selection_is_the_lowest_card_then_the_lowest_device() {
        let devices = parse(REAL).unwrap();
        let chosen = select(&devices, None).unwrap();
        assert_eq!((chosen.card, chosen.device), (0, 3));
        let named = select(&devices, Some((1, 1))).unwrap();
        assert_eq!(named.name, "ALC1220 Digital");
        // A capture-only device cannot be selected even by name, because it is
        // not in the list at all.
        let err = select(&devices, Some((1, 2))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("card 1 device 2"));
    }

    #[test]
    fn qemu_hda_wins_over_the_lower_numbered_test_loopback() {
        let devices = parse(
            "00-00: Loopback PCM : Loopback PCM : playback 8 : capture 8\n\
             00-01: Loopback PCM : Loopback PCM : playback 8 : capture 8\n\
             01-00: Generic : Generic Analog : playback 1\n",
        )
        .unwrap();
        let chosen = select(&devices, None).unwrap();
        assert_eq!((chosen.card, chosen.device), (1, 0));
        assert_eq!(chosen.name, "Generic Analog");

        let explicit = select(&devices, Some((0, 0))).unwrap();
        assert_eq!(explicit.name, TEST_LOOPBACK_PCM);
    }

    #[test]
    fn a_test_loopback_only_machine_needs_an_explicit_selection() {
        let devices = parse(ALOOP).unwrap();
        let error = select(&devices, None).unwrap_err();
        assert!(error.to_string().contains("no non-test playback device"));
        assert_eq!(select(&devices, Some((0, 1))).unwrap().device, 1);
    }

    #[test]
    fn the_qemu_and_loopback_fixtures_parse() {
        let hda = parse(QEMU_HDA).unwrap();
        assert_eq!(hda.len(), 1);
        assert_eq!(
            hda.first().unwrap().node().to_string_lossy(),
            "/dev/snd/pcmC0D0p"
        );
        let loop_devices = parse(ALOOP).unwrap();
        assert_eq!(loop_devices.len(), 2);
        assert_eq!(loop_devices.first().unwrap().subdevices, 8);
    }

    /// `snd-aloop` is linked before HDA in the image kernel and can therefore
    /// claim card zero. It remains explicitly selectable for capture tests,
    /// but ordinary playback must reach the audible HDA device.
    #[test]
    fn the_test_loopback_is_not_the_default_sink() {
        let devices = parse(
            "00-00: Loopback PCM : Loopback PCM : playback 8 : capture 8\n\
             00-01: Loopback PCM : Loopback PCM : playback 8 : capture 8\n\
             01-00: ALC262 Analog : ALC262 Analog : playback 1 : capture 1\n",
        )
        .unwrap();
        let default = select(&devices, None).unwrap();
        assert_eq!((default.card, default.device), (1, 0));
        let oracle = select(&devices, Some((0, 0))).unwrap();
        assert_eq!(oracle.id, "Loopback PCM");
    }

    #[test]
    fn a_loopback_only_machine_requires_explicit_selection() {
        let devices = parse(ALOOP).unwrap();
        let error = select(&devices, None).unwrap_err();
        assert!(error.to_string().contains("no non-test playback device"));
        assert_eq!(select(&devices, Some((0, 1))).unwrap().device, 1);
    }

    #[test]
    fn an_empty_table_refuses_rather_than_inventing_a_device() {
        assert!(parse("").unwrap().is_empty());
        let err = select(&[], None).unwrap_err();
        assert!(err
            .to_string()
            .contains("lists no non-test playback device"));
    }

    /// §K.4's rule, tested: an unparseable line is refused with the line in the
    /// diagnostic. Each of these is a plausible corruption rather than noise.
    #[test]
    fn a_line_that_does_not_parse_is_refused_by_name() {
        for (bad, why) in [
            ("00: HDMI : HDMI : playback 1", "no card-device address"),
            (
                "xx-03: HDMI : HDMI : playback 1",
                "card number is not a number",
            ),
            (
                "00-yy: HDMI : HDMI : playback 1",
                "device number is not a number",
            ),
            (
                "00-03: HDMI : HDMI : playback many",
                "a stream count that is not a number",
            ),
            (
                "00-03: HDMI : HDMI : playback 0",
                "a playback device with no subdevices",
            ),
            ("garbage", "no 'CC-DD: ' prefix"),
        ] {
            let err = parse(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains(why), "{bad:?} -> {err}");
            assert!(
                err.to_string().contains(bad),
                "the line must be quoted: {err}"
            );
        }
    }

    /// A card whose NAME looks like a playback field is still capture-only.
    ///
    /// The count has to end the field. Taking the digits after any occurrence of
    /// the marker read `" : playback 3 monitor"` inside a card-supplied name as
    /// a real field, so a capture-only device was selected for playback and the
    /// daemon went looking for a `...p` node that does not exist. And the head
    /// was cut at the FIRST marker rather than at the real field, so the name
    /// was truncated even when a genuine field followed it.
    #[test]
    fn a_card_named_like_a_playback_field_is_not_one() {
        // Capture-only: the only occurrence of the marker is inside the name.
        let line = "01-00: USBmic : Cheap USB : playback 3 monitor : capture 1";
        assert_eq!(parse_line(line).unwrap(), None, "it plays nothing");

        // And the same name on a card that DOES have a playback field keeps
        // both the real count and the whole name.
        let line = "01-00: USBmic : Cheap USB : playback 3 monitor : playback 2 : capture 1";
        let found = parse_line(line).unwrap().expect("a playback device");
        assert_eq!(found.subdevices, 2, "the real field wins");
        assert_eq!(found.capture_subdevices, 1);
        assert!(
            found.name.contains("monitor"),
            "the name was cut at the first marker: {:?}",
            found.name
        );
    }

    #[test]
    fn a_digit_field_inside_capture_only_card_text_is_not_playback() {
        let line = "01-00: USBmic : Cheap : playback 3 : monitor : capture 1";
        assert!(parse(line).unwrap().is_empty());
    }

    /// A card name containing the field separator does not shift the counts:
    /// they are found from the end.
    #[test]
    fn a_colon_in_a_card_name_does_not_move_the_stream_counts() {
        let devices = parse("00-00: A : B : C : playback 2 : capture 1\n").unwrap();
        let only = devices.first().unwrap();
        assert_eq!(only.subdevices, 2);
        assert_eq!(only.capture_subdevices, 1);
        assert_eq!(only.card, 0);
        assert_eq!(only.device, 0);
    }

    #[test]
    fn card_supplied_text_is_sanitised() {
        let devices = parse("00-00: x\u{1b}[2J : y\u{7f}z : playback 1\n").unwrap();
        let only = devices.first().unwrap();
        assert!(!only.id.contains('\u{1b}'));
        assert!(!only.name.contains('\u{7f}'));
        assert_eq!(only.name, "y?z");
    }

    /// The daemon's real table, when the build host has one. This is the only
    /// assertion here that reads the running kernel, and it is deliberately
    /// tolerant of a host with no sound card — what it proves is that the parser
    /// accepts whatever a real `/proc/asound/pcm` contains, which is a different
    /// claim from the fixtures above.
    #[test]
    fn the_hosts_own_table_parses_if_it_has_one() {
        let path = Path::new(PROC_ASOUND_PCM);
        if !path.exists() {
            return;
        }
        let devices = read(path).unwrap();
        for playback in &devices {
            assert!(playback.subdevices >= 1);
            assert!(playback.node().starts_with("/dev/snd/"));
        }
    }

    /// A rescan lists what `outputs` would, but no ordinary PCM left is an
    /// empty list rather than an error, and a named PCM that went leads no
    /// longer; an unparsable table stays an error.
    #[test]
    fn a_rescan_reads_no_pcm_as_no_outputs() {
        let dir = proc_tree("rescan", REAL, &[(0, "HDMI\n"), (1, "PCH\n")]);
        let pcm = dir.path().join("pcm");
        assert_eq!(rescan(&pcm, None).unwrap(), outputs(&pcm, None).unwrap());
        assert_eq!(
            rescan(&pcm, Some((1, 1))).unwrap(),
            outputs(&pcm, Some((1, 1))).unwrap()
        );
        assert_eq!(
            rescan(&pcm, Some((5, 0))).unwrap(),
            outputs(&pcm, None).unwrap(),
            "a named PCM gone leads no longer"
        );
        fs::write(&pcm, ALOOP).unwrap();
        assert!(outputs(&pcm, None).is_err());
        assert!(rescan(&pcm, None).unwrap().is_empty());
        assert_eq!(
            rescan(&pcm, Some((0, 1))).unwrap().len(),
            1,
            "the named loopback"
        );
        fs::write(&pcm, "").unwrap();
        assert!(rescan(&pcm, None).unwrap().is_empty());
        fs::write(&pcm, "garbage\n").unwrap();
        assert!(rescan(&pcm, None).is_err());
        // A card id that cannot be read keys the card `card<N>` at start,
        // but fails a rescan, which keeps the outputs it had.
        fs::write(&pcm, REAL).unwrap();
        fs::remove_dir_all(dir.path().join("card1")).unwrap();
        assert!(outputs(&pcm, None)
            .unwrap()
            .iter()
            .any(|o| o.key == "card1:0"));
        let refused = rescan(&pcm, None).unwrap_err();
        assert!(refused.to_string().contains("card1/id"), "{refused}");
    }
}
