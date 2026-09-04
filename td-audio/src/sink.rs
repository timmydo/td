//! The `AudioSink` boundary, and the in-memory sink that stands in for a card.
//!
//! §K.1 asks for this seam explicitly, and for the reason to be recorded: td
//! serves the PulseAudio protocol first because that is the only interface the
//! target application speaks, not because PipeWire was voted down. A future
//! PipeWire service would sit on the other side of this trait, so the mixer
//! above it must not know what a `snd_pcm_hw_params` is — and, symmetrically,
//! nothing below it may know what a Pulse stream is.
//!
//! The second reason is testing. §K's four levels start with "an in-memory
//! `AudioSink` replacing ALSA, asserting exact decoded PCM, drain timing,
//! underflow and mixing". `MemorySink` is that sink: it models a bounded device
//! ring with a clock the test advances by hand, so backpressure, underrun and
//! the latency sum are all reproducible without a card, a kernel or a wait.

use std::io;
use std::os::fd::RawFd;

/// The rate the device is fixed at (§K.5). The protocol admits only this rate
/// and channel count; its one Firefox float format is converted without
/// resampling before this boundary.
pub const RATE: u32 = 48000;
/// Stereo.
pub const CHANNELS: u32 = 2;
/// `S16_LE`: two bytes per sample.
pub const SAMPLE_BYTES: usize = 2;
/// Four bytes per frame at the fixed spec.
pub const FRAME_BYTES: usize = CHANNELS as usize * SAMPLE_BYTES;

/// What a sink is actually running at, which is what the kernel chose rather
/// than what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub rate: u32,
    pub channels: u32,
    /// Bytes per frame — `channels * 2` for `S16_LE`, and checked against what
    /// the device reported rather than assumed.
    pub frame_bytes: usize,
}

impl Spec {
    pub const fn fixed() -> Self {
        Self {
            rate: RATE,
            channels: CHANNELS,
            frame_bytes: FRAME_BYTES,
        }
    }

    /// Frames as microseconds at this rate. Every latency figure the daemon
    /// reports goes through here, so that "converted at the actually-negotiated
    /// rate" (§K.3) is a property of one function rather than of every caller.
    pub fn frames_to_usec(&self, frames: u64) -> u64 {
        if self.rate == 0 {
            return 0;
        }
        frames.saturating_mul(1_000_000) / u64::from(self.rate)
    }

    pub fn usec_to_frames(&self, usec: u64) -> u64 {
        usec.saturating_mul(u64::from(self.rate)) / 1_000_000
    }
}

/// The outcome of waiting for the device to make room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Nothing happened before the timeout.
    Timeout,
    /// There is room for at least one period.
    Writable,
    /// The stream underran. The caller re-prepares and re-primes; this is
    /// recoverable and expected under load, not a failure.
    Underrun,
    /// The device is gone.
    Gone,
}

/// One mixed, interleaved `S16_LE` output stream reaching hardware.
///
/// Every method that can block takes an explicit timeout, because the daemon's
/// one thread also has clients to serve.
pub trait AudioSink {
    /// What the device actually negotiated.
    fn spec(&self) -> Spec;

    /// Frames still ahead of the last accepted one, in the kernel and the
    /// device. This is the hardware term of §K.3's latency sum and the only
    /// term this layer can answer.
    fn device_delay(&mut self) -> io::Result<u64>;

    /// Wait for room, at most `timeout_ms`.
    fn wait(&mut self, timeout_ms: i32) -> io::Result<Wait>;

    /// Offer interleaved `S16_LE` bytes. Returns the FRAMES accepted, which may
    /// be fewer than offered, and may be zero.
    fn write(&mut self, pcm: &[u8]) -> io::Result<usize>;

    /// Begin playback. Called once the ring has been primed.
    fn start(&mut self) -> io::Result<()>;

    /// Stop now, discarding what is queued.
    fn stop(&mut self) -> io::Result<()>;

    /// Play out what is queued, then stop. DEVICE shutdown: §K.3 is explicit
    /// that a per-client Pulse `DRAIN` must never reach here, because it would
    /// silence every other stream.
    fn drain(&mut self) -> io::Result<()>;

    /// Return an underrun stream to a state that accepts frames again.
    fn recover(&mut self) -> io::Result<()>;

    /// The ring's total size in frames, which is the most that can be in flight.
    fn buffer_frames(&self) -> u64;

    /// The largest transfer the mixer prepares in one pass, in frames.
    /// Short real tails remain short rather than being padded to this size.
    fn period_frames(&self) -> u64;

    /// The descriptor to poll for writability, if this sink has one.
    ///
    /// The daemon waits on its clients and its device in a single `poll(2)`, so
    /// it needs the device's descriptor rather than a blocking `wait`. A sink
    /// with no descriptor answers `None` and the daemon polls its clients
    /// alone, which is correct rather than degraded: a sink whose readiness is
    /// not a descriptor is one whose `wait` answers immediately.
    fn raw_fd(&self) -> Option<RawFd> {
        None
    }

    /// Whether the device has been started.
    ///
    /// `SwParams::set_playback` puts `start_threshold` at the boundary so the
    /// device never starts itself; the daemon starts it once the ring has
    /// audio, and needs to know whether it already did.
    fn is_running(&self) -> bool;
}

/// Whether an error is an ALSA underrun.
///
/// One definition, because three callers ask: the fixture's play loop, the
/// daemon's device loop, and the mixer's drain. Three copies of a two-constant
/// predicate is three places for one of them to forget `ESTRPIPE`, which is the
/// suspend case and looks like a hang rather than like a wrong answer.
pub fn is_underrun(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == crate::sys::EPIPE || code == crate::sys::ESTRPIPE
    )
}

/// An `AudioSink` that keeps every frame it is given, with a device clock the
/// test advances by hand.
///
/// Test-only, and deliberately so: it is a stand-in for hardware, and a build
/// that could reach it at runtime would be a build that could serve silence
/// while reporting success.
///
/// The ring is bounded exactly as a real one is, so a writer that ignores
/// backpressure fills it and gets short writes here just as it would on a card.
/// Nothing here sleeps: `advance` is how time passes, which is what makes drain
/// timing and underrun assertions exact instead of flaky.
#[cfg(test)]
pub struct MemorySink {
    spec: Spec,
    buffer_frames: u64,
    period_frames: u64,
    /// Every frame ever accepted, in order. The oracle for "exact decoded PCM".
    played: Vec<u8>,
    /// Frames accepted but not yet consumed by the modelled device.
    queued: u64,
    running: bool,
    underran: bool,
    /// How many times the ring emptied while running.
    pub underruns: u32,
    gone: bool,
    write_limit: Option<usize>,
    delay_error: Option<i32>,
}

#[cfg(test)]
impl MemorySink {
    pub fn new(spec: Spec, buffer_frames: u64, period_frames: u64) -> Self {
        Self {
            spec,
            buffer_frames,
            period_frames,
            played: Vec::new(),
            queued: 0,
            running: false,
            underran: false,
            underruns: 0,
            gone: false,
            write_limit: None,
            delay_error: None,
        }
    }

    /// A sink at the fixed spec with a quarter-second ring.
    pub fn fixed() -> Self {
        Self::new(Spec::fixed(), 12000, 1024)
    }

    /// Advance the modelled device clock by `frames`.
    ///
    /// If the ring runs dry while running, that is an underrun: the device
    /// stops, exactly as a real one does with `stop_threshold == buffer_size`.
    pub fn advance(&mut self, frames: u64) {
        if !self.running {
            return;
        }
        if frames > self.queued {
            self.queued = 0;
            self.underran = true;
            self.running = false;
            self.underruns = self.underruns.saturating_add(1);
        } else {
            self.queued -= frames;
        }
    }

    /// Every frame decoded to per-channel samples, which is what a test asserts
    /// against a generated waveform.
    pub fn samples(&self) -> Vec<i16> {
        self.played
            .as_chunks::<SAMPLE_BYTES>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect()
    }

    /// Frames accepted in total.
    pub fn frames_written(&self) -> u64 {
        (self.played.len() / self.spec.frame_bytes.max(1)) as u64
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Make the device disappear, so callers can be tested against `Wait::Gone`.
    pub fn unplug(&mut self) {
        self.gone = true;
    }

    /// Restrict one test device's accepted frames per transfer.
    pub fn limit_writes_to(&mut self, frames: Option<usize>) {
        self.write_limit = frames;
    }

    /// Make DELAY return one exact device error.
    pub fn fail_delay_with(&mut self, error: Option<i32>) {
        self.delay_error = error;
    }
}

#[cfg(test)]
impl AudioSink for MemorySink {
    fn spec(&self) -> Spec {
        self.spec
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn device_delay(&mut self) -> io::Result<u64> {
        if self.gone {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "device unplugged",
            ));
        }
        if let Some(error) = self.delay_error {
            return Err(io::Error::from_raw_os_error(error));
        }
        Ok(self.queued)
    }

    fn wait(&mut self, _timeout_ms: i32) -> io::Result<Wait> {
        if self.gone {
            return Ok(Wait::Gone);
        }
        if self.underran {
            return Ok(Wait::Underrun);
        }
        if self.buffer_frames.saturating_sub(self.queued) >= self.period_frames {
            Ok(Wait::Writable)
        } else {
            Ok(Wait::Timeout)
        }
    }

    fn write(&mut self, pcm: &[u8]) -> io::Result<usize> {
        if self.gone {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "device unplugged",
            ));
        }
        if self.underran {
            return Err(io::Error::from_raw_os_error(crate::sys::EPIPE));
        }
        let frame_bytes = self.spec.frame_bytes.max(1);
        let offered = pcm.len() / frame_bytes;
        let room = self.buffer_frames.saturating_sub(self.queued);
        let accepted = offered
            .min(usize::try_from(room).unwrap_or(usize::MAX))
            .min(self.write_limit.unwrap_or(usize::MAX));
        let taken = accepted.saturating_mul(frame_bytes);
        self.played
            .extend_from_slice(pcm.get(..taken).unwrap_or(pcm));
        self.queued = self.queued.saturating_add(accepted as u64);
        Ok(accepted)
    }

    fn start(&mut self) -> io::Result<()> {
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        self.running = false;
        self.queued = 0;
        Ok(())
    }

    fn drain(&mut self) -> io::Result<()> {
        self.queued = 0;
        self.running = false;
        Ok(())
    }

    fn recover(&mut self) -> io::Result<()> {
        self.underran = false;
        self.queued = 0;
        self.running = false;
        Ok(())
    }

    fn buffer_frames(&self) -> u64 {
        self.buffer_frames
    }

    fn period_frames(&self) -> u64 {
        self.period_frames
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_fixed_spec_is_the_one_section_k_pins() {
        let spec = Spec::fixed();
        assert_eq!(spec.rate, 48000);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.frame_bytes, 4);
        assert_eq!(spec.frames_to_usec(48000), 1_000_000);
        assert_eq!(spec.frames_to_usec(24000), 500_000);
        assert_eq!(spec.usec_to_frames(1_000_000), 48000);
        // A zero rate cannot divide, and answering 0 is better than dividing.
        let broken = Spec {
            rate: 0,
            channels: 2,
            frame_bytes: 4,
        };
        assert_eq!(broken.frames_to_usec(48000), 0);
    }

    #[test]
    fn a_memory_sink_keeps_every_byte_in_order() {
        let mut sink = MemorySink::fixed();
        let pcm: Vec<u8> = (0..8u16).flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(sink.write(&pcm).unwrap(), 4);
        assert_eq!(sink.samples(), vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(sink.frames_written(), 4);
        assert_eq!(sink.device_delay().unwrap(), 4);
    }

    /// Backpressure is real: a full ring accepts a short write, not an error.
    #[test]
    fn a_full_ring_short_writes() {
        let mut sink = MemorySink::new(Spec::fixed(), 4, 2);
        let pcm = [0u8; 6 * FRAME_BYTES];
        assert_eq!(sink.write(&pcm).unwrap(), 4);
        assert_eq!(sink.write(&pcm).unwrap(), 0);
        assert_eq!(sink.wait(0).unwrap(), Wait::Timeout);
        sink.start().unwrap();
        sink.advance(2);
        assert_eq!(sink.wait(0).unwrap(), Wait::Writable);
        assert_eq!(sink.write(&pcm).unwrap(), 2);
    }

    /// A ring that empties while running underruns, and says so — which is the
    /// event the mixer has to notice to produce a Pulse `UNDERFLOW`.
    #[test]
    fn an_empty_ring_underruns_while_running() {
        let mut sink = MemorySink::new(Spec::fixed(), 8, 2);
        sink.write(&[0u8; 4 * FRAME_BYTES]).unwrap();
        sink.start().unwrap();
        sink.advance(2);
        assert_eq!(sink.underruns, 0);
        sink.advance(4);
        assert_eq!(sink.underruns, 1);
        assert_eq!(sink.wait(0).unwrap(), Wait::Underrun);
        assert_eq!(
            sink.write(&[0u8; FRAME_BYTES]).unwrap_err().raw_os_error(),
            Some(crate::sys::EPIPE)
        );
        sink.recover().unwrap();
        assert_eq!(sink.wait(0).unwrap(), Wait::Writable);
        assert_eq!(sink.write(&[0u8; FRAME_BYTES]).unwrap(), 1);
    }

    /// The clock does not run before `start`, so priming never underruns.
    #[test]
    fn a_stopped_sink_does_not_consume() {
        let mut sink = MemorySink::fixed();
        sink.write(&[0u8; 100 * FRAME_BYTES]).unwrap();
        sink.advance(1000);
        assert_eq!(sink.device_delay().unwrap(), 100);
        assert_eq!(sink.underruns, 0);
    }

    #[test]
    fn an_unplugged_sink_reports_gone() {
        let mut sink = MemorySink::fixed();
        sink.unplug();
        assert_eq!(sink.wait(0).unwrap(), Wait::Gone);
        assert!(sink.device_delay().is_err());
        assert!(sink.write(&[0u8; FRAME_BYTES]).is_err());
    }
}
