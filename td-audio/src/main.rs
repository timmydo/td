//! td-audio — td's audio daemon.
//!
//! The normative design is `APPLICATIONS.md` §K, and the confined surface is
//! `UNSAFE.md` §13. This landing is §I's rungs 25 AND 26: the ALSA PCM back end
//! and the mixer that sits on it, driven by a fixture that writes a tone; and
//! the PulseAudio protocol, the per-connection session, and the `serve`
//! personality that binds §K.5's socket and drives them.
//!
//! Rung 25 is the half that is testable with no browser, no jail and no
//! protocol — which is why the ladder puts it first: it is the cheapest place
//! to find out that the pinned kernel's sound pins are wrong. Rung 26's own
//! gate is a client playing audio through the daemon, and the `status` and
//! `volume` personalities §K.5 describes are not here: they are Pulse clients
//! of this socket, and a public entry point that cannot do what its name says
//! is worse than one that is absent.
#![deny(unsafe_code)]

mod alsa;
mod device;
mod mixer;
mod pcm;
mod proto;
mod serve;
mod session;
mod sink;
mod sys;
mod tag;
mod tone;
mod wav;
mod wire;

use crate::alsa::{AlsaSink, Request};
use crate::mixer::{Mixer, StreamId};
use crate::sink::{is_underrun, AudioSink, Spec, Wait};
use crate::tone::{Generator, Tone};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "td-audio: {error}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage: td-audio devices [--proc PATH]
       td-audio tone [--card N] [--device N] [--hz HZ] [--voices N]
                     [--amplitude N] [--ms MILLISECONDS] [--wav PATH]
                     [--proc PATH]
       td-audio verify-tone [--wav PATH] [--hz HZ] [--voices N] [--ms MS]
       td-audio serve [--socket PATH] [--card N] [--device N] [--proc PATH]
                      [--passes N]
       td-audio probe [--socket PATH]

  devices      list the playback PCMs /proc/asound/pcm reports
  tone         play a deterministic tone through the ALSA back end, or render
               it to a WAVE file with --wav and touch no device at all
  verify-tone  check a recorded WAVE against the tone that should be in it:
               rate, duration, non-silence, and correlation with the waveform
  serve        run the PulseAudio-protocol daemon: bind the socket, mix every
               client, and play the sum through the ALSA back end
  probe        connect to the supervised daemon's socket
";

fn run(arguments: &[OsString]) -> io::Result<()> {
    let (personality, rest) = arguments
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?;
    match personality.to_str() {
        Some("devices") => devices(rest),
        Some("tone") => tone(rest),
        Some("verify-tone") => verify_tone(rest),
        Some("serve") => serve_daemon(rest),
        Some("probe") => probe(rest),
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown personality {personality:?}\n{USAGE}"),
        )),
    }
}

fn probe(arguments: &[OsString]) -> io::Result<()> {
    let socket = parse_probe(arguments)?;
    drop(UnixStream::connect(&socket).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("connect {}: {error}", socket.display()),
        )
    })?);
    Ok(())
}

fn parse_probe(arguments: &[OsString]) -> io::Result<PathBuf> {
    Ok(match arguments {
        [] => PathBuf::from(serve::SOCKET_PATH),
        [flag, path] if flag == "--socket" && !path.is_empty() => PathBuf::from(path),
        _ => return Err(bad("probe accepts only [--socket PATH]".into())),
    })
}

/// Run the daemon.
///
/// §K.5 puts the socket at `/run/td-audio/native` in a directory `td-seatd`
/// creates, and authorizes on `SO_PEERCRED` rather than on mode bits. The
/// image runs it as the dedicated `audio` account; the daemon resolves that
/// identity at run time and admits its own uid plus the seat user rather than
/// compiling an account number into this protocol boundary.
fn serve_daemon(arguments: &[OsString]) -> io::Result<()> {
    let options = parse(arguments)?;
    let found = device::read(&options.proc_pcm)?;
    let wanted = options.card.zip(options.device);
    let playback = device::select(&found, wanted)?;
    let sink = AlsaSink::open(playback, Request::default())?;
    let spec = sink.spec();
    let policy = serve::Policy::for_uid(current_uid()?);
    let mut server = serve::Server::bind(&options.socket, sink, policy)?;
    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "{}: serving {} at {} Hz, {} channels, for uid(s) {:?}",
        options.socket.display(),
        playback.node().display(),
        spec.rate,
        spec.channels,
        server.policy_uids()
    )?;
    let stopped = server.run(options.passes);
    server.shutdown();
    match stopped? {
        serve::Stopped::DeviceGone => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{} went away", playback.node().display()),
        )),
        serve::Stopped::Finished => Ok(()),
    }
}

/// This process's real uid, from `/proc/self/status`.
///
/// Read rather than asked for: `getuid(2)` would be a fourth syscall on the
/// surface `UNSAFE.md` §13 records, and the answer is already a file this
/// daemon can read. §K.5's policy needs the number, not the syscall.
fn current_uid() -> io::Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "/proc/self/status carries no real uid, so the peer policy has no self to admit",
            )
        })
}

/// The parsed `tone`/`devices` options.
struct Options {
    proc_pcm: PathBuf,
    card: Option<u32>,
    device: Option<u32>,
    hertz: u32,
    voices: u32,
    amplitude: i32,
    milliseconds: u64,
    wav: Option<PathBuf>,
    socket: PathBuf,
    /// A bound on `serve`'s event loop, for a test or a smoke run. `None` is
    /// the real daemon, which runs until the device goes away.
    passes: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            proc_pcm: PathBuf::from(device::PROC_ASOUND_PCM),
            card: None,
            device: None,
            hertz: Tone::fixture().hertz,
            voices: 1,
            amplitude: Tone::fixture().amplitude,
            milliseconds: 2000,
            wav: None,
            socket: PathBuf::from(serve::SOCKET_PATH),
            passes: None,
        }
    }
}

impl Options {
    fn voices(&self) -> Vec<tone::Voice> {
        tone::plan(
            Tone {
                hertz: self.hertz,
                amplitude: self.amplitude,
            },
            self.voices,
            mixer::VOLUME_NORM,
        )
    }
}

fn bad(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

fn number<T: std::str::FromStr>(flag: &str, value: Option<&OsString>) -> io::Result<T> {
    let value = value.ok_or_else(|| bad(format!("{flag} needs a value")))?;
    value
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| bad(format!("{flag} needs a number, not {value:?}")))
}

fn parse(arguments: &[OsString]) -> io::Result<Options> {
    let mut options = Options::default();
    let mut index = 0usize;
    while let Some(argument) = arguments.get(index) {
        let next = arguments.get(index + 1);
        match argument.to_str() {
            Some("--proc") => {
                options.proc_pcm =
                    PathBuf::from(next.ok_or_else(|| bad("--proc needs a path".into()))?)
            }
            Some("--card") => options.card = Some(number("--card", next)?),
            Some("--device") => options.device = Some(number("--device", next)?),
            Some("--hz") => options.hertz = number("--hz", next)?,
            Some("--voices") => options.voices = number("--voices", next)?,
            Some("--amplitude") => options.amplitude = number("--amplitude", next)?,
            Some("--ms") => options.milliseconds = number("--ms", next)?,
            Some("--wav") => {
                options.wav = Some(PathBuf::from(
                    next.ok_or_else(|| bad("--wav needs a path".into()))?,
                ))
            }
            Some("--socket") => {
                options.socket =
                    PathBuf::from(next.ok_or_else(|| bad("--socket needs a path".into()))?)
            }
            Some("--passes") => options.passes = Some(number("--passes", next)?),
            _ => {
                return Err(bad(format!("unexpected argument {argument:?}\n{USAGE}")));
            }
        }
        // Every flag here takes a value, so the step is fixed. A flag that did
        // not would need its own step, which is why this is a constant rather
        // than an `if` nobody would revisit.
        index = index.saturating_add(2);
    }
    if options.voices == 0 || options.voices > 8 {
        return Err(bad(format!("--voices {} is outside 1..=8", options.voices)));
    }
    // The TOP harmonic has to stay audible and below Nyquist, so the ceiling is
    // on the highest voice rather than on the base: `--hz 6000 --voices 8` would
    // otherwise ask for 48 kHz out of a 48 kHz device, which is a constant.
    let highest = options.hertz.saturating_mul(options.voices);
    if options.hertz == 0 || highest >= sink::RATE / 2 {
        return Err(bad(format!(
            "--hz {} with {} voice(s) reaches {highest} Hz, which is not below \
             half the {} Hz the device runs at",
            options.hertz,
            options.voices,
            sink::RATE
        )));
    }
    if options.amplitude <= 0 || options.amplitude > i32::from(i16::MAX) {
        return Err(bad(format!(
            "--amplitude {} is outside 1..={}",
            options.amplitude,
            i16::MAX
        )));
    }
    if options.milliseconds == 0 || options.milliseconds > 600_000 {
        return Err(bad(format!(
            "--ms {} is outside 1..=600000",
            options.milliseconds
        )));
    }
    match (options.card, options.device) {
        (Some(_), None) | (None, Some(_)) => Err(bad(
            "--card and --device select one PCM and are given together".into(),
        )),
        _ => Ok(options),
    }
}

fn devices(arguments: &[OsString]) -> io::Result<()> {
    let options = parse(arguments)?;
    let found = device::read(&options.proc_pcm)?;
    let mut stdout = io::stdout();
    if found.is_empty() {
        writeln!(
            stdout,
            "no playback device in {} — is CONFIG_SND_HDA_INTEL enabled?",
            options.proc_pcm.display()
        )?;
        return Ok(());
    }
    for playback in &found {
        writeln!(stdout, "{}\t{playback}", playback.node().display())?;
    }
    Ok(())
}

fn tone(arguments: &[OsString]) -> io::Result<()> {
    let options = parse(arguments)?;
    match &options.wav {
        Some(path) => render(path, &options),
        None => play(&options),
    }
}

/// The frames a run of `milliseconds` produces.
fn frame_count(spec: Spec, milliseconds: u64) -> io::Result<u64> {
    Ok(spec.usec_to_frames(milliseconds.saturating_mul(1000)))
}

/// Render the fixture to a WAVE file, touching no device.
///
/// The same voices, volumes and summation as the playback path, so a machine
/// with no sound card can still check that the audio itself is right — and so
/// `verify-tone` has one expected waveform rather than two.
fn render(path: &Path, options: &Options) -> io::Result<()> {
    let spec = Spec::fixed();
    let voices = options.voices();
    let frames = frame_count(spec, options.milliseconds)?;
    let channels = spec.channels.max(1);
    let mut pcm = Vec::with_capacity(
        usize::try_from(frames.saturating_mul(spec.frame_bytes as u64)).unwrap_or(0),
    );
    for frame in 0..frames {
        let sample = tone::expected(spec, &voices, frame, mixer::VOLUME_NORM).to_le_bytes();
        for _ in 0..channels {
            pcm.extend_from_slice(&sample);
        }
    }
    let length = u32::try_from(pcm.len())
        .map_err(|_| bad("a WAVE data chunk cannot exceed 4 GiB".into()))?;
    let mut file = wav::header(spec, length)?;
    file.extend_from_slice(&pcm);
    fs::write(path, &file)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    writeln!(
        io::stdout(),
        "{}: {frames} frames, {} voice(s) from {} Hz, at {} Hz stereo S16_LE",
        path.display(),
        voices.len(),
        options.hertz,
        spec.rate
    )
}

/// Check a recorded WAVE against the tone that should be in it.
///
/// §K's fourth test level: rate, duration, non-silence, and correlation with
/// the expected waveform. The correlation is what makes this an oracle rather
/// than a smoke test — a check for "not all zeroes" passes on noise, and an
/// exact byte comparison fails on any resampling the capture path did.
fn verify_tone(arguments: &[OsString]) -> io::Result<()> {
    let options = parse(arguments)?;
    let path = options
        .wav
        .as_ref()
        .ok_or_else(|| bad("verify-tone needs --wav PATH".into()))?;
    let spec = Spec::fixed();
    let voices = options.voices();
    let bytes =
        fs::read(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let wave = wav::parse(&bytes)?;

    let mut complaints = Vec::new();
    if wave.rate != spec.rate {
        complaints.push(format!("rate is {} Hz, not {}", wave.rate, spec.rate));
    }
    // The channel layout is checked, not assumed. A mono capture correlates
    // perfectly against the expected waveform while being the wrong artifact
    // entirely, and a stereo one whose second channel is noise would too if
    // only channel zero were compared.
    if u32::from(wave.channels) != spec.channels {
        complaints.push(format!(
            "the recording has {} channel(s), not the {} played",
            wave.channels, spec.channels
        ));
    }
    // Both bounds. Four fifths is the floor because a capture that lost the
    // tail is still a capture; the ceiling is there because a recording twice
    // as long as the tone is a different artifact, and a lower bound alone
    // accepts it.
    let wanted_ms = options.milliseconds;
    if wave.duration_ms().saturating_mul(5) < wanted_ms.saturating_mul(4) {
        complaints.push(format!(
            "duration is {} ms, shorter than four fifths of the {wanted_ms} ms played",
            wave.duration_ms()
        ));
    }
    if wave.duration_ms() > wanted_ms.saturating_mul(2) {
        complaints.push(format!(
            "duration is {} ms, over twice the {wanted_ms} ms played",
            wave.duration_ms()
        ));
    }
    if wave.peak() == 0 {
        complaints.push("the recording is silent".into());
    }
    // EVERY channel, not just the first. The mixer writes the same sum to both,
    // so a capture whose channels disagree is a capture of something else.
    let mut correlations = Vec::new();
    for channel in 0..wave.channels {
        let recorded = wave.channel(channel);
        let expected: Vec<i16> = (0..recorded.len() as u64)
            .map(|frame| tone::expected(spec, &voices, frame, mixer::VOLUME_NORM))
            .collect();
        let correlation = wav::correlation(&recorded, &expected);
        if correlation < 0.9 {
            complaints.push(format!(
                "channel {channel} correlates {correlation:.4} with the expected waveform, \
                 not above 0.9"
            ));
        }
        correlations.push(correlation);
    }
    let correlation = correlations
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        .min(1.0);
    if !complaints.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {}", path.display(), complaints.join("; ")),
        ));
    }
    writeln!(
        io::stdout(),
        "{}: {} Hz, {} channel(s), {} ms, peak {}, correlation {correlation:.4} — OK",
        path.display(),
        wave.rate,
        wave.channels,
        wave.duration_ms(),
        wave.peak()
    )
}

/// Play the fixture through a real device.
///
/// One stream per voice, so this exercises the mixer as well as the back end —
/// which is the point of playing more than one: a summation fault is inaudible
/// in a single-stream fixture and obvious in a harmonic stack.
fn play(options: &Options) -> io::Result<()> {
    let found = device::read(&options.proc_pcm)?;
    let wanted = options.card.zip(options.device);
    let playback = device::select(&found, wanted)?;
    let mut sink = AlsaSink::open(playback, Request::default())?;
    let spec = sink.spec();
    let identity = sink.identity().clone();
    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "{}: {} — {} Hz, {} channels, {}-frame periods in a {}-frame buffer \
         (boundary {}, device FIFO {} frames)",
        playback.node().display(),
        identity.name,
        spec.rate,
        spec.channels,
        sink.period_frames(),
        sink.buffer_frames(),
        sink.boundary(),
        sink.fifo_frames()
    )?;

    let target = frame_count(spec, options.milliseconds)?;
    let voices = options.voices();
    let mut mixer = Mixer::new(spec);
    let mut running: Vec<(StreamId, Generator)> = Vec::new();
    for voice in voices.iter() {
        let id = mixer.open(sink.buffer_frames().saturating_mul(2))?;
        mixer.set_volume(id, voice.volume)?;
        running.push((id, Generator::new(spec, voice.tone)));
    }
    let mut scratch = Vec::new();

    // A wall-clock bound so a wedged device ends the fixture rather than the
    // fixture ending the boot: four times the audio, plus two seconds of slack.
    let deadline = Instant::now()
        + Duration::from_millis(options.milliseconds.saturating_mul(4).saturating_add(2000));
    let mut written = 0u64;
    let mut started = false;
    let mut underruns = 0u32;
    let mut timed_out = true;

    while Instant::now() < deadline {
        for (id, generator) in &mut running {
            let produced = generator.frames_produced();
            if produced >= target {
                continue;
            }
            let grant = mixer
                .request_frames(*id)?
                .min(sink.period_frames())
                .min(target - produced);
            if grant > 0 {
                generator.fill(&mut scratch, usize::try_from(grant).unwrap_or(0));
                mixer.write(*id, &scratch)?;
            }
        }

        let pumped = match mixer.pump(&mut sink) {
            Ok(pumped) => pumped,
            // An underrun found by the transfer rather than by `poll`: the ring
            // emptied between the two. Recoverable, and NOT a reason to stop —
            // this is the ordinary consequence of a busy machine.
            Err(e) if is_underrun(&e) => {
                underruns = underruns.saturating_add(1);
                mixer.recover(&mut sink)?;
                started = false;
                // The ring is empty again, so the priming count restarts with
                // it. Leaving the lifetime total here would satisfy the
                // threshold below on the very next pass and start a device
                // with nothing queued — an immediate second underrun.
                written = 0;
                continue;
            }
            Err(e) => {
                let _ = sink.stop();
                return Err(e);
            }
        };
        written = written.saturating_add(pumped.frames_written);

        // Start once the ring holds a period, so playback begins from a primed
        // buffer rather than from whatever happened to be written first.
        // `written` counts frames since the last prepare, not since the start
        // of the run: a recovery empties the ring, and priming has to happen
        // again.
        if !started && written >= sink.period_frames() {
            sink.start()?;
            started = true;
        }

        if running
            .iter()
            .all(|(_, generator)| generator.frames_produced() >= target)
        {
            timed_out = false;
            break;
        }

        match sink.wait(100)? {
            Wait::Gone => {
                let _ = sink.stop();
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("{} went away mid-tone", playback.node().display()),
                ));
            }
            Wait::Underrun => {
                underruns = underruns.saturating_add(1);
                mixer.recover(&mut sink)?;
                started = false;
                written = 0;
            }
            Wait::Writable | Wait::Timeout => {}
        }
    }

    // Everything the fixture meant to play has been handed to the mixer. Run
    // the device until it has all reached the speakers, with a bound: a card
    // that stops consuming must end the fixture rather than hang the boot.
    let passes = if timed_out {
        0
    } else {
        let allowed = u32::try_from(target / sink.period_frames().max(1))
            .unwrap_or(u32::MAX)
            .saturating_add(64);
        let used = mixer.drain_all(&mut sink, allowed)?;
        if used >= allowed {
            timed_out = true;
        }
        used
    };

    if timed_out {
        // Whatever is queued is audio nobody is waiting for, so DROP it rather
        // than draining it: a fixture that overran its own deadline should stop
        // making noise, not finish the tune.
        let _ = sink.stop();
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{} did not play {target} frames within the deadline ({written} written, \
                 {underruns} underrun(s))",
                playback.node().display()
            ),
        ));
    }

    // Everything queued has reached the speakers, so bring the last partial
    // period out too. The DEVICE drain, which is what this ioctl is for; a
    // per-stream drain never reaches here (§K.3).
    sink.drain()?;
    for (id, _) in &running {
        let timing = mixer.timing(*id)?;
        writeln!(
            stdout,
            "stream {}: {} of {} bytes played, {} underflow(s), {} overflow(s), \
             {} us still in flight, drained {}",
            id,
            timing.read_index,
            timing.write_index,
            mixer.underflows(*id)?,
            mixer.overflows(*id)?,
            timing.latency_usec,
            mixer.is_drained(*id)?
        )?;
    }
    writeln!(
        stdout,
        "played {written} frames ({} ms) from {} stream(s), {underruns} device \
         underrun(s), drained in {passes} pass(es)",
        spec.frames_to_usec(written) / 1000,
        mixer.stream_count()
    )?;
    for (id, _) in &running {
        mixer.remove(*id);
    }
    if written == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the device accepted no frames at all",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn probe_accepts_only_its_fixed_socket_option() {
        assert_eq!(parse_probe(&[]).unwrap(), PathBuf::from(serve::SOCKET_PATH));
        assert_eq!(
            parse_probe(&[OsString::from("--socket"), OsString::from("/tmp/audio")]).unwrap(),
            PathBuf::from("/tmp/audio")
        );
        for bad in [
            vec![OsString::from("--socket")],
            vec![OsString::from("--socket"), OsString::new()],
            vec![OsString::from("--hz"), OsString::from("440")],
        ] {
            assert!(parse_probe(&bad).is_err());
        }
    }

    /// Every source file of this crate, by name. Read at COMPILE time, so the
    /// assertions below do not depend on the working directory a test runs in.
    fn sources() -> Vec<(&'static str, &'static str)> {
        vec![
            ("main.rs", include_str!("main.rs")),
            ("alsa.rs", include_str!("alsa.rs")),
            ("device.rs", include_str!("device.rs")),
            ("mixer.rs", include_str!("mixer.rs")),
            ("pcm.rs", include_str!("pcm.rs")),
            ("proto.rs", include_str!("proto.rs")),
            ("serve.rs", include_str!("serve.rs")),
            ("session.rs", include_str!("session.rs")),
            ("sink.rs", include_str!("sink.rs")),
            ("sys.rs", include_str!("sys.rs")),
            ("tag.rs", include_str!("tag.rs")),
            ("tone.rs", include_str!("tone.rs")),
            ("wav.rs", include_str!("wav.rs")),
            ("wire.rs", include_str!("wire.rs")),
        ]
    }

    fn source(name: &str) -> &'static str {
        sources()
            .into_iter()
            .find(|(file, _)| *file == name)
            .map(|(_, text)| text)
            .unwrap_or("")
    }

    /// Whitespace squeezed out, so a pin is immune to reformatting.
    fn squeeze(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The same text with line comments removed.
    ///
    /// Needed for exactly one claim: that the raw entry point is NAMED only
    /// where it is defined and called. The module documentation names it on
    /// purpose — that is where the confinement is argued — and prose is not a
    /// way to reach the kernel, so the claim is about code and the scan should
    /// be too. No string literal in `sys.rs` contains `//`, which is what makes
    /// this crude strip exact rather than approximate.
    fn code_only(text: &str) -> String {
        text.lines()
            .map(|line| match line.find("//") {
                Some(at) => line.get(..at).unwrap_or(""),
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn squeezed() -> String {
        sources()
            .into_iter()
            .map(|(_, text)| squeeze(text))
            .collect()
    }

    /// The scoped allowance, assembled rather than written out: every string in
    /// this module is itself scanned, so a literal here would be a match the
    /// crate does not contain.
    const ALLOW: &str = concat!("#[allow(un", "safe_code)]");

    /// Every module a source text declares.
    ///
    /// Over the WHOLE text, not line by line. `pub(crate) mod` is a declaration
    /// and matching the prefix `mod ` does not see it; and Rust does not care
    /// where the newlines are, so `mod` on one line with `extra;` on the next
    /// is a declaration a per-line scan cannot see either. A confirmation pass
    /// used the first to get a module past this, and the next pass used the
    /// second to get it past the fix. An inline `mod tests {` has no semicolon
    /// and its body is already inside a file this scan reads.
    fn declared_in(text: &str) -> Vec<String> {
        let code: String = text
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let mut found = Vec::new();
        let mut words = code.split_whitespace().peekable();
        while let Some(word) = words.next() {
            if word != "mod" {
                continue;
            }
            if let Some(next) = words.peek() {
                if let Some(name) = next.strip_suffix(';') {
                    found.push(name.to_string());
                }
            }
        }
        found
    }

    /// Reading a list of files only equals reading the crate if the two agree.
    /// A module redirected by a path attribute, or a file nobody declares,
    /// would make every assertion below vacuous for exactly the file that
    /// needed checking.
    #[test]
    fn the_scan_covers_every_module_the_crate_declares() {
        let mut declared = vec!["main.rs".to_string()];
        for name in declared_in(source("main.rs")) {
            declared.push(format!("{name}.rs"));
        }
        let scanned: Vec<String> = sources().into_iter().map(|(f, _)| f.to_string()).collect();
        let mut sorted_declared = declared.clone();
        sorted_declared.sort();
        let mut sorted_scanned = scanned.clone();
        sorted_scanned.sort();
        assert_eq!(
            sorted_declared, sorted_scanned,
            "the module list and the scanned file list must be the same set"
        );
        // And NO other file declares one. A module declared in `sys.rs`
        // resolves to `sys/extra.rs`, which is in neither this list nor the
        // recipe's staged one, so nothing would read it at all: a confirmation
        // pass put a pointer-dereferencing back door there and every assertion
        // in this file still passed. The crate is flat and the recipe stages it
        // flat, so a nested module is refused rather than followed.
        for (file, text) in sources() {
            if file == "main.rs" {
                continue;
            }
            assert!(
                declared_in(text).is_empty(),
                "{file} declares a submodule, which no scan here reads"
            );
        }
        // No path-attribute redirection anywhere: it would point a module at a
        // file this scan never reads.
        assert!(!squeezed().contains(concat!("#[pa", "th")));
    }

    /// The list this module scans is every `.rs` file on disk, and there are
    /// no others.
    ///
    /// Everything else here reads a roster somebody typed, and a scan of a
    /// roster is a scan of whatever was remembered. An include-by-path needs no
    /// `mod`, so a file named by no list is read by the compiler and by
    /// nothing here:
    /// a confirmation pass put a dereference in one, staged it, compiled it,
    /// and watched every assertion in this file and in the recipe pass. The
    /// directory is the only list that cannot be out of date.
    ///
    /// Subdirectories are refused outright rather than walked. The recipe
    /// stages this crate flat, so a nested file could not ship even if it
    /// compiled here, and a scan that followed one would be describing a tree
    /// the build does not have.
    #[test]
    fn the_scanned_list_is_every_rust_file_on_disk() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut on_disk: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                entry.file_type().unwrap().is_file(),
                "{name} is not a file; this crate is flat and the recipe stages it flat"
            );
            if name.ends_with(".rs") {
                on_disk.push(name);
            }
        }
        let mut scanned: Vec<String> = sources()
            .into_iter()
            .map(|(file, _)| file.to_string())
            .collect();
        on_disk.sort();
        scanned.sort();
        assert_eq!(
            on_disk, scanned,
            "the files on disk and the files this module scans must be the same set"
        );
    }

    /// Nothing pulls in a file by path.
    ///
    /// The path-including macro takes a path and no `mod`, so it is how a file
    /// gets compiled without appearing in any list of modules. The test above
    /// makes such a
    /// file visible; this refuses the construct that reaches one outside `src`
    /// entirely.
    #[test]
    fn no_source_is_pulled_in_by_path() {
        for (file, text) in sources() {
            assert!(
                !squeeze(text).contains(concat!("incl", "ude!")),
                "{file} pulls in a file by path"
            );
        }
    }

    /// The two-character comment opener appears nowhere in the crate, in any
    /// context.
    ///
    /// A block comment is the reason the shape assertions above could be walked
    /// past: it is not whitespace, so it splits the keyword from its brace and
    /// an attribute from its contents while the code still compiles. The
    /// per-file counts catch that, but a count can be edited in the same diff
    /// as the region it counts, which makes it one bound rather than two.
    /// Refusing the construct restores the second: with none in the crate, the
    /// shape scans see what the compiler sees.
    ///
    /// The scan is over raw text, so it also refuses the sequence inside a
    /// string or a comment — a path glob, say. That is a false alarm and not a
    /// hole, and the message says which it is, because a message claiming a
    /// block comment sends the reader looking for one that is not there. The
    /// crate contains none of either today.
    #[test]
    fn the_crate_writes_no_block_comments() {
        for (file, text) in sources() {
            assert!(
                !text.contains(BLOCK_OPEN),
                "{file} contains the comment opener. If it is a comment, the \
                 shape scans cannot see through it; if it is inside a string, \
                 spell it some other way"
            );
        }
    }

    /// Assembled, so this scan does not match its own source.
    const BLOCK_OPEN: &str = concat!("/", "*");

    /// The crate denies the keyword, and exactly ONE annotation relaxes it:
    /// `syscall5` itself.
    ///
    /// There is deliberately no allowance on `mod sys;`. A module-level one
    /// exempts the whole module, and a review demonstrated the consequence by
    /// appending a second, arbitrary region to `sys.rs` and watching
    /// every test in this file still pass: the new block needed no attribute,
    /// so nothing counted it. The function-level allowance is sufficient on its
    /// own, so the module-level one is gone and the count below is the real
    /// bound rather than a formality.
    #[test]
    fn only_the_syscall_layer_may_contain_a_confined_region() {
        assert!(source("main.rs").contains(concat!("#![deny(un", "safe_code)]")));
        assert_eq!(
            squeezed().matches(ALLOW).count(),
            1,
            "exactly one scoped allowance: the raw entry point, and nothing else"
        );
        assert_eq!(squeeze(source("main.rs")).matches(ALLOW).count(), 0);
        assert_eq!(squeeze(source("sys.rs")).matches(ALLOW).count(), 1);
        // The INNER form is a different token and exempts everything below it
        // to the end of its block. A review demonstrated that too: a nested
        // module carrying an inner allowance plus a hand-written Send impl
        // passed every test here, because neither the attribute nor the item
        // contains the strings this scan looked for.
        assert!(
            !squeezed().contains(INNER_ALLOW),
            "an inner allowance exempts everything below it, including code this scan cannot see"
        );
        // Every form of the keyword, not just a block. Its impl, fn, trait
        // and extern forms carry no `{` immediately after it, and would
        // otherwise be invisible here.
        for (file, text) in sources() {
            let squeezed_file = squeeze(&code_only(text));
            let mut regions = 0usize;
            for form in FORMS {
                let uses = squeezed_file.matches(&format!("{KEYWORD}{form}")).count();
                if file == "sys.rs" && *form == "{" {
                    // The one confined block, pinned whole below.
                    regions += uses;
                    continue;
                }
                assert_eq!(
                    uses, 0,
                    "{file} contains `{KEYWORD} {form}`, which is outside the confined block"
                );
            }
            if file == "sys.rs" {
                assert_eq!(
                    regions, 1,
                    "sys.rs contains {regions} such blocks; exactly one is confined"
                );
            }
        }
    }

    /// THE bound: the keyword itself is counted, wherever it appears.
    ///
    /// Every other assertion in this module matches a SHAPE — the keyword followed
    /// by a brace, an attribute spelled exactly one way — over text with line
    /// comments stripped and whitespace squeezed. A review broke all of them at
    /// once, three ways, and each escape compiled and ran:
    ///
    /// * a block comment between the keyword and its form, which is not
    ///   whitespace and was not stripped, so the keyword never met its brace;
    /// * a `cfg_attr` wrapper around the allowance, which is not the literal
    ///   the allowance counter looked for;
    /// * `//` inside an ordinary string literal, which blinded the line-comment
    ///   strip for the whole line that followed it.
    ///
    /// Counting is immune to all three, and to the ones nobody has thought of
    /// yet. Rust has exactly one spelling of the keyword: no escape, no macro
    /// that pastes it without the text being present, no way to hide it in a
    /// comment or a string.
    ///
    /// The counts below are the LOAD-BEARING part, and only because there is no
    /// slack in them. A confirmation pass broke an earlier version of this test
    /// that pinned 17 here and 7 in `sys.rs`, most of them prose: a count with
    /// room in it is a budget, and an attacker spends it by deleting a sentence
    /// that mentions the keyword and adding a region that uses it, for a net
    /// change of zero. It compiled, it read arbitrary memory, and it passed
    /// every test in this file and the recipe's staged scan.
    ///
    /// So the budget is spent to nothing. Each pin equals exactly the tokens
    /// that MUST be there — the denial here, the allowance and its block in
    /// `sys.rs` — and no comment in either file may name the keyword; they say
    /// "the keyword" instead, which is why the prose above reads as it does.
    /// With no slack, every added region moves a count, and the direction it
    /// moves cannot be cancelled. A reworded sentence costs a number here; a
    /// miss costs the confinement.
    #[test]
    fn the_keyword_is_counted_wherever_it_appears() {
        for (file, text) in sources() {
            let uses = text.matches(KEYWORD).count();
            let expected = match file {
                "sys.rs" => KEYWORD_USES_IN_SYS,
                "main.rs" => KEYWORD_USES_IN_MAIN,
                _ => 0,
            };
            assert_eq!(
                uses, expected,
                "{file} names the keyword {uses} times and the roster says \
                 {expected}. If this crate gained a region, that is the finding. \
                 If a comment was reworded, update the number here."
            );
        }
    }

    /// A conditional attribute can spell any other attribute, which makes every
    /// assertion about a literal attribute conditional too.
    ///
    /// A `cfg_attr` wrapping the allowance is one the counter
    /// above cannot see, and one wrapping a `path` redirects a module past the
    /// file list. This crate has no use for either, so the form is refused
    /// outright rather than modelled.
    #[test]
    fn no_attribute_in_this_crate_is_written_conditionally() {
        for (file, text) in sources() {
            assert!(
                !squeeze(text).contains(CFG_ATTR),
                "{file} writes an attribute conditionally, which puts every \
                 attribute assertion in this module out of reach"
            );
        }
    }

    /// `sys.rs`: the scoped allowance, and the block it scopes. Nothing else.
    ///
    /// NOT named `SYS_`-something: that prefix is what the syscall-roster scan
    /// below counts declarations with.
    const KEYWORD_USES_IN_SYS: usize = 2;
    /// This file: the crate-level denial, and nothing else.
    const KEYWORD_USES_IN_MAIN: usize = 1;
    /// The conditional-attribute form, assembled so this scan does not match
    /// its own source.
    const CFG_ATTR: &str = concat!("#[cfg_", "attr(");

    /// The inner attribute form, assembled so this scan does not match itself.
    const INNER_ALLOW: &str = concat!("#![allow(un", "safe_code)]");

    /// The keyword, and every syntactic form that can follow it. A confined
    /// region is not only a block.
    const KEYWORD: &str = concat!("un", "safe");
    const FORMS: &[&str] = &["{", "fn", "impl", "trait", "extern"];

    /// How a syscall number is declared, assembled rather than written out, so
    /// the scan does not match its own source.
    const DECL: &str = concat!("const", "SYS_");
    /// The three-argument forwarder, which the ioctls and `poll` use.
    const CALL: &str = concat!("sys", "call3");
    /// The raw entry point, which owns the only inline assembly. `syscall3`
    /// forwards to it and `peer_credentials` calls it directly.
    const RAW: &str = concat!("sys", "call5");

    /// The syscalls `UNSAFE.md` records for this crate, with the x86-64 number
    /// each must carry. A FOURTH is a reviewed amendment.
    ///
    /// The NUMBER is pinned, not just the name: renumbering `SYS_IOCTL` to a
    /// neighbour would change which kernel call this crate makes while every
    /// name-based assertion still passed. 16 is `ioctl` (15 is `rt_sigreturn`,
    /// 17 is `pread64`); 7 is `poll` (6 is `lstat`, 8 is `lseek`); 55 is
    /// `getsockopt` (54 is `setsockopt`, which WRITES an option and is the
    /// neighbour a slip would reach, and 56 is `clone`).
    const AMENDED: &[(&str, &str)] = &[
        ("SYS_IOCTL", "16"),
        ("SYS_POLL", "7"),
        ("SYS_GETSOCKOPT", "55"),
    ];

    #[test]
    fn the_syscall_roster_is_the_amended_three() {
        let sys = squeeze(source("sys.rs"));
        for (name, value) in AMENDED {
            assert_eq!(
                sys.matches(&format!("const{name}:usize={value};")).count(),
                1,
                "{name} must be declared exactly once in sys.rs as {value}"
            );
        }
        // Nothing else declares one, anywhere.
        assert_eq!(
            squeezed().matches(DECL).count(),
            AMENDED.len(),
            "a syscall constant is declared somewhere other than the two amended ones"
        );
    }

    /// The eleven `ioctl` requests §K.4 pins, each composed from a length.
    ///
    /// `ioctl(2)` is one syscall onto an unbounded space of operations, so the
    /// number in `rax` is not the surface — the request in `rsi` is, and a
    /// twelfth entry here is what would widen it.
    ///
    /// The composer's name is assembled, because this table is itself scanned:
    /// a literal here would make "no other file composes a request" trivially
    /// false for this one.
    const COMPOSE: &str = concat!("i", "oc(");

    /// `(name, direction, request number, length constant)`.
    const REQUESTS: &[(&str, &str, &str, &str)] = &[
        ("PVERSION", "IOC_READ", "0x00", "INT_LEN"),
        ("INFO", "IOC_READ", "0x01", "PCM_INFO_LEN"),
        ("HW_REFINE", "IOC_READ|IOC_WRITE", "0x10", "HW_PARAMS_LEN"),
        ("HW_PARAMS", "IOC_READ|IOC_WRITE", "0x11", "HW_PARAMS_LEN"),
        ("SW_PARAMS", "IOC_READ|IOC_WRITE", "0x13", "SW_PARAMS_LEN"),
        ("DELAY", "IOC_READ", "0x21", "SFRAMES_LEN"),
        ("PREPARE", "0", "0x40", "0"),
        ("START", "0", "0x42", "0"),
        ("DROP", "0", "0x43", "0"),
        ("DRAIN", "0", "0x44", "0"),
        ("WRITEI_FRAMES", "IOC_WRITE", "0x50", "XFERI_LEN"),
    ];

    #[test]
    fn the_ioctl_requests_are_exactly_eleven_and_composed_from_a_length() {
        let sys = squeeze(source("sys.rs"));
        for (name, direction, number, length) in REQUESTS {
            let composition = format!("const{name}:usize={COMPOSE}{direction},{number},{length});");
            assert_eq!(
                sys.matches(&composition).count(),
                1,
                "{name} must be declared exactly once, composed as {composition}"
            );
        }
        // One composer, defined once, and used only where a request is
        // declared: composing a request number at a CALL site would put an
        // operation outside the roster while every name-based scan stayed green.
        assert_eq!(
            sys.matches(&format!("constfn{COMPOSE}")).count(),
            1,
            "one composer, defined once"
        );
        for (file, text) in sources() {
            if file == "sys.rs" {
                continue;
            }
            assert!(
                !squeeze(text).contains(COMPOSE),
                "{file} composes an ioctl request; they belong in sys.rs"
            );
        }
    }

    /// `getsockopt(2)` is, like `ioctl(2)`, one syscall onto a wide space of
    /// operations, so the surface is the (level, option) PAIR and not the
    /// number in `rax`. §K.5 pins it: "`getsockopt(2)` restricted to
    /// `SOL_SOCKET`/`SO_PEERCRED` with a pinned 12-byte `[i32; 3]`."
    #[test]
    fn the_socket_option_is_exactly_one_pinned_pair() {
        let sys = squeeze(source("sys.rs"));
        for (name, value) in [
            ("SOL_SOCKET", "1"),
            ("SO_PEERCRED", "17"),
            ("UCRED_LEN", "12"),
        ] {
            // One count, not two: `pubconst…` CONTAINS `const…`, so adding the
            // two would report a public declaration as a pair of them.
            assert_eq!(
                sys.matches(&format!("const{name}:usize={value};")).count(),
                1,
                "{name} must be declared exactly once as {value}"
            );
        }
        // Exactly one call, and it names both halves of the pair rather than
        // taking them from a variable — a level or option computed at the call
        // site is a different surface that every name-based scan would pass.
        assert_eq!(
            sys.matches("SYS_GETSOCKOPT,fdasusize,SOL_SOCKET,SO_PEERCRED,")
                .count(),
            1,
            "one getsockopt call site, with the pinned level and option"
        );
        // And no other socket option is DECLARED anywhere in the crate. The
        // names are assembled because `sys.rs` argues about two of them by
        // name — naming the neighbour a slip would reach is the point of the
        // refusal being recorded — so a literal here would match that prose.
        for refused in [
            concat!("SO_PASS", "CRED"),
            concat!("SO_PEER", "SEC"),
            concat!("SO_PEER", "GROUPS"),
            concat!("SOL", "_IP"),
            concat!("SOL", "_TCP"),
            concat!("SYS_SET", "SOCKOPT"),
        ] {
            assert!(
                !squeezed().contains(&format!("const{refused}")),
                "{refused} is declared, and is outside this crate's socket surface"
            );
            assert!(
                !squeezed().contains(&format!(",{refused},")),
                "{refused} is passed to a syscall, and is outside this crate's surface"
            );
        }
    }

    /// Requests deliberately OUTSIDE the surface, each one this daemon could
    /// plausibly have reached for and does not.
    ///
    /// `SYNC_PTR` and the `MMAP` family are §K.4's central refusal: RW mode
    /// needs no mapped ring, and taking one would add shared-control-page
    /// correctness as a dependency to save under 200 KiB/s of copying.
    /// `READI_FRAMES` is capture, which §K.5 does not ship in v1.
    /// `SNDRV_CTL_*` and `controlC` are the control device, which is never
    /// opened because volume is multiplication in the mixer.
    const REFUSED: &[&str] = &[
        "SYNC_PTR",
        "STATUS_EXT",
        "CHANNEL_INFO",
        "HW_FREE",
        "RESET",
        "PAUSE",
        "REWIND",
        "RESUME",
        "FORWARD",
        "XRUN",
        "READI_FRAMES",
        "READN_FRAMES",
        "WRITEN_FRAMES",
        "HWSYNC",
        "SNDRV_CTL",
        "controlC",
        "SYS_MMAP",
        "SYS_OPENAT",
        "SYS_READ",
        "SYS_WRITE",
    ];

    #[test]
    fn the_refused_requests_and_devices_appear_nowhere_in_the_crate() {
        for (file, text) in sources() {
            let squeezed_file = squeeze(text);
            for refused in REFUSED {
                // The doc comments in sys.rs argue about these by name, which is
                // the point of the refusal being recorded; what must not appear
                // is a declaration or a call.
                assert!(
                    !squeezed_file.contains(&format!("const{refused}")),
                    "{file} declares {refused}, which is outside this crate's surface"
                );
                assert!(
                    !squeezed_file.contains(&format!("{CALL}({refused}")),
                    "{file} calls the raw entry point with {refused}"
                );
            }
        }
        // The control device is never named as a path anywhere.
        assert!(
            !squeezed().contains(concat!("/dev/snd/cont", "rolC")),
            "the control device is never opened (§K.4)"
        );
    }

    /// The one permitted block is a REGION, and counting tokens inside
    /// it never bounds it: a second inline-assembly invocation, a second
    /// instruction inside the SAME one, or a raw pointer dereference all fit
    /// without changing any count. So the block is pinned WHOLE, with
    /// whitespace squeezed out.
    ///
    /// This is what pins the REGISTERS. Everything else pins what is handed to
    /// `syscall5`; only this pins where those arguments land, and `in("rsi")`
    /// changed to `in("rdx")` compiles, passes every other assertion, and hands
    /// the kernel a request number where the buffer pointer belongs.
    ///
    /// The block takes five argument registers because `getsockopt(2)` takes
    /// five; `syscall3` forwards with zeros. That is deliberately ONE mapping
    /// rather than two: a second inline-assembly block would be a second place
    /// for an argument to land in the wrong register, and `r10` — not `rcx` —
    /// is the fourth syscall argument, which is the mistake a second block
    /// invites.
    ///
    /// `options(nomem)` being ABSENT is part of the pin, not an omission: seven
    /// of the eleven ioctl requests, `poll` and `getsockopt` all have the
    /// kernel write through one of those pointers.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        const BLOCK: &str = concat!(
            "un",
            "safe{core",
            "::arch::a",
            "sm!(\"syscall\",inlateout(\"rax\")nasisize=>ret,",
            "in(\"rdi\")a1,in(\"rsi\")a2,in(\"rdx\")a3,",
            "in(\"r10\")a4,in(\"r8\")a5,",
            "out(\"rcx\")_,out(\"r11\")_,options(nostack),);}"
        );
        let squeezed = squeezed();
        assert_eq!(
            squeezed.matches(BLOCK).count(),
            1,
            "the confined block's body changed; re-audit it and update this pin"
        );
        assert_eq!(
            squeezed.matches(concat!("a", "sm!")).count(),
            1,
            "exactly one inline-assembly invocation may exist in this crate"
        );
    }

    /// The call sites select their syscall through a named constant, the
    /// constant IS the argument rather than the start of an expression, and the
    /// entry point is named nowhere but at its definition and those calls.
    ///
    /// Without the first the roster is only a claim about declarations: a bare
    /// `(999, ...)` would reach an unaudited kernel call. Without the last,
    /// `let f = syscall3;` binds the entry point to a name this scan does not
    /// know and every later call goes through it.
    #[test]
    fn every_syscall_call_site_uses_a_named_constant() {
        let text = code_only(source("sys.rs"));
        let text = text.as_str();
        let mut sites = 0usize;
        let mut mentions = 0usize;
        for (offset, _) in text.match_indices(CALL) {
            mentions += 1;
            let Some(after) = text.get(offset + CALL.len()..) else {
                continue;
            };
            let Some(arguments) = after.trim_start().strip_prefix('(') else {
                continue;
            };
            let arguments = arguments.trim_start();
            // The definition itself takes `(n: usize, ...)`.
            if arguments.starts_with("n: usize") {
                continue;
            }
            sites += 1;
            let selector: String = arguments
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                AMENDED.iter().any(|(name, _)| *name == selector),
                "called with '{selector}', which is not one of the amended syscalls"
            );
            let rest = arguments.get(selector.len()..).unwrap_or("").trim_start();
            assert!(
                rest.starts_with(','),
                "the selector must be '{selector}' itself, not an expression built from it"
            );
        }
        // One per typed wrapper that goes through the forwarder: eleven ioctls
        // and the two polls (the single-descriptor wait and the daemon's set).
        assert_eq!(sites, 13, "expected one call site per typed wrapper");
        assert_eq!(
            mentions,
            sites + 1,
            "the raw entry point is named somewhere that is neither a call nor its definition"
        );
        // The whole crate, not just sys.rs: no other file may reach it.
        let crate_code: String = sources()
            .into_iter()
            .map(|(_, text)| squeeze(&code_only(text)))
            .collect();
        assert_eq!(
            crate_code.matches(&format!("{CALL}(")).count(),
            sites + 1,
            "the raw entry point escaped its module"
        );
    }

    /// Every `ioctl` call site, pinned WHOLE — every register, not just the
    /// selector. A descriptor and a request swapped past each other would still
    /// be `ioctl(2)`, still use named constants, and would ask file descriptor
    /// 0x4140 for something.
    #[test]
    fn every_call_site_is_pinned_whole() {
        const ARGUMENTS: &[&str] = &[
            "(SYS_IOCTL,fdasusize,PVERSION,out.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,INFO,out.0.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,HW_REFINE,params.0.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,HW_PARAMS,params.0.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,SW_PARAMS,params.0.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,PREPARE,0)",
            "(SYS_IOCTL,fdasusize,START,0)",
            "(SYS_IOCTL,fdasusize,DROP,0)",
            "(SYS_IOCTL,fdasusize,DRAIN,0)",
            "(SYS_IOCTL,fdasusize,DELAY,out.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,WRITEI_FRAMES,xferi.as_mut_ptr()asusize,)",
            "(SYS_POLL,pollfd.as_mut_ptr()asusize,1,timeout_msasusize,)",
        ];
        assert_eq!(
            ARGUMENTS.len(),
            REQUESTS.len() + 1,
            "one pin per request, plus poll"
        );
        let sys = squeeze(source("sys.rs"));
        for arguments in ARGUMENTS {
            assert_eq!(
                sys.matches(&format!("{CALL}{arguments}")).count(),
                1,
                "the {CALL} call site is not the pinned one: {arguments}"
            );
        }
        const GETSOCKOPT: &str = concat!(
            "(SYS_GETSOCKOPT,fdasusize,SOL_SOCKET,SO_PEERCRED,",
            "cred.as_mut_ptr()asusize,len.as_mut_ptr()asusize,)"
        );
        assert_eq!(
            sys.matches(&format!("{RAW}{GETSOCKOPT}")).count(),
            1,
            "the {RAW} getsockopt call site is not the pinned one"
        );
    }

    /// The raw entry point is private to its module, and annotated.
    #[test]
    fn the_raw_entry_point_is_private_to_its_module() {
        let sys = squeeze(source("sys.rs"));
        // Defined exactly once, and the allowance is part of that definition —
        // a SECOND, unannotated `fn syscall5` would otherwise be a raw entry
        // point nobody counted.
        assert_eq!(sys.matches(&format!("fn{RAW}(")).count(), 1);
        assert_eq!(
            sys.matches(&format!(
                "{}#[inline]{}fn{RAW}(",
                "",
                concat!("#[allow(un", "safe_code)]")
            ))
            .count(),
            1,
            "the definition must carry the scoped allowance directly"
        );
        // The forwarder is defined once too, and carries NO allowance of its
        // own: it carries none of the keyword, and one that did would be a second
        // region this scan does not pin.
        assert_eq!(sys.matches(&format!("fn{CALL}(")).count(), 1);
        assert_eq!(
            sys.matches(&format!(
                "{}fn{CALL}(",
                concat!("#[allow(un", "safe_code)]")
            ))
            .count(),
            0,
            "the forwarder needs no allowance and must not carry one"
        );
        for name in [CALL, RAW] {
            assert!(
                !sys.contains(&format!("pubfn{name}")),
                "{name} must not be public"
            );
            assert!(
                !sys.contains(&format!("pub(crate)fn{name}")),
                "{name} must not escape its module"
            );
        }
        // The raw entry point is named exactly three times in code: its own
        // definition, the forwarder's body, and `peer_credentials`. A fourth
        // is a call this roster has not audited.
        let crate_code: String = sources()
            .into_iter()
            .map(|(_, text)| squeeze(&code_only(text)))
            .collect();
        assert_eq!(
            crate_code.matches(RAW).count(),
            3,
            "the five-argument entry point is reached from somewhere unaudited"
        );
    }

    #[test]
    fn the_architecture_guard_is_present_in_the_layers_that_need_it() {
        assert!(
            squeeze(source("sys.rs")).contains("#[cfg(not(all(target_arch=\"x86_64\","),
            "sys.rs must refuse to build for another architecture: every struct \
             length and request number there is an x86-64 fact"
        );
    }
}
