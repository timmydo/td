//! Playback-device discovery: `/proc/asound/pcm`, and nothing else.
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
        .filter(|playback| playback.id != TEST_LOOPBACK_PCM || playback.name != TEST_LOOPBACK_PCM)
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
}
