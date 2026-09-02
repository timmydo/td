//! The ALSA PCM back end: one device, opened, configured, and written to.
//!
//! §K.4's shape, in order. Discovery has already turned `/proc/asound/pcm` into
//! a card and device number; this module opens the node those name, asks
//! `SNDRV_PCM_IOCTL_INFO` whether the kernel agrees about what was opened,
//! refines a hardware configuration instead of asserting one, commits it, READS
//! BACK what was actually chosen, and refuses to serve a stream the device did
//! not take.
//!
//! The last step is the one that is easy to skip and expensive to skip: nothing
//! observable distinguishes a mask the kernel narrowed from one it honoured
//! until the pitch is wrong. A card that quietly gives 44100 where 48000 was
//! asked plays everything 8.8% sharp and reports success.

use crate::device::Playback;
use crate::pcm::{
    boundary_for, HwParamsExt, Identity, Interval, Mask, PcmInfoExt, Range, SwParamsExt,
    ACCESS_RW_INTERLEAVED, FORMAT_S16_LE, STREAM_PLAYBACK, SUBFORMAT_STD,
};
use crate::sink::{AudioSink, Spec, Wait};
use crate::sys::{self, HwParams, PcmInfo, Ready, SwParams};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

/// `O_NONBLOCK`. The daemon has clients to serve, so it never parks inside a
/// transfer: `poll(2)` is the wait, and `WRITEI_FRAMES` returns what fit.
const O_NONBLOCK: i32 = 0o4000;

/// `SNDRV_PROTOCOL_MAJOR` of the PCM interface this crate is written against.
///
/// The minor and subminor are deliberately NOT pinned: the kernel bumps them for
/// additive changes, and refusing those would make every kernel bump an audio
/// outage. A major bump is a different interface, and this daemon is not written
/// for it.
const PCM_PROTOCOL_MAJOR: u32 = 2;

/// What to ask the device for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub rate: u32,
    pub channels: u32,
    /// The transfer granularity to aim for. The device gets the final say and
    /// the result is read back.
    pub period_frames: u32,
    /// How many periods the ring should hold. Two is the minimum that can play
    /// while the writer fills; four is the default, which at 1024-frame periods
    /// is about 85 ms of slack at 48 kHz.
    pub periods: u32,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            rate: crate::sink::RATE,
            channels: crate::sink::CHANNELS,
            period_frames: 1024,
            periods: 4,
        }
    }
}

/// A configured playback PCM.
pub struct AlsaSink {
    file: File,
    spec: Spec,
    identity: Identity,
    period_frames: u64,
    buffer_frames: u64,
    boundary: u64,
    fifo_frames: u64,
    started: bool,
    /// The frame size the DEVICE confirmed, not the one the spec repeats. This
    /// is what bounds every transfer.
    frame_bytes: sys::FrameBytes,
}

impl AlsaSink {
    /// Open, verify, configure and prepare the device `playback` names.
    pub fn open(playback: &Playback, request: Request) -> io::Result<Self> {
        let node = playback.node();
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(&node)
            .map_err(|e| {
                    io::Error::new(e.kind(), format!("{}: {e}", node.display()))
            })?;
        let fd = file.as_raw_fd();

        let version = sys::pversion(fd)?;
        let major = version >> 16;
        if major != PCM_PROTOCOL_MAJOR {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{}: PCM protocol {}.{}.{} — this daemon is written for major {PCM_PROTOCOL_MAJOR}",
                    node.display(),
                    major,
                    (version >> 8) & 0xff,
                    version & 0xff
                ),
            ));
        }

        let identity = confirm_identity(fd, playback)?;
        let chosen = configure(fd, request)?;
        let spec = Spec {
            rate: chosen.rate,
            channels: chosen.channels,
            frame_bytes: chosen.frame_bytes.get(),
        };

        let boundary = boundary_for(chosen.buffer_frames);
        let mut sw = SwParams::zeroed();
        sw.set_playback(version, chosen.period_frames, chosen.buffer_frames, boundary)?;
        sys::sw_params(fd, &mut sw)?;
        let kernel_boundary = sw.boundary()?;
        if kernel_boundary != boundary {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: the kernel's ring boundary is {kernel_boundary}, not the {boundary} \
                     this daemon computed for a {}-frame buffer — start_threshold would not \
                     mean 'never start by yourself'",
                    node.display(),
                    chosen.buffer_frames
                ),
            ));
        }

        sys::prepare(fd)?;
        Ok(Self {
            file,
            spec,
            identity,
            period_frames: chosen.period_frames,
            buffer_frames: chosen.buffer_frames,
            boundary,
            fifo_frames: chosen.fifo_frames,
            started: false,
            frame_bytes: chosen.frame_bytes,
        })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// What the kernel said this node is.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The ring wrap point the kernel computed.
    ///
    /// Kept because `SwParams::set_playback` puts `start_threshold` here, which
    /// is what stops the device starting itself. Nothing in this daemon wraps
    /// an index at it: the mixer's positions are `u64` frame counts that never
    /// reach a wrap, and the Pulse indexes are derived from those.
    pub fn boundary(&self) -> u64 {
        self.boundary
    }

    /// The device's own FIFO depth in frames, as `hw_params` reported it.
    ///
    /// Zero on most cards. Where it is not, it is frames of latency that
    /// `SNDRV_PCM_IOCTL_DELAY` may not include.
    ///
    /// Reported, not yet summed: it reaches the `devices` output and no
    /// further, so a card with a non-zero FIFO under-reports its latency by
    /// that many frames. Folding it into `Mixer::timing` needs the mixer to
    /// know a device property it is otherwise independent of, and no card on
    /// hand reports a non-zero one to test the change against.
    pub fn fifo_frames(&self) -> u64 {
        self.fifo_frames
    }

}

/// Ask the kernel what it just opened, and refuse if it is not what discovery
/// promised.
///
/// §K.4: `INFO` is not optional once discovery reads `/proc/asound/pcm`, because
/// a daemon that skips it is trusting a path string on a real machine.
fn confirm_identity(fd: RawFd, playback: &Playback) -> io::Result<Identity> {
    let mut info = PcmInfo::zeroed();
    sys::info(fd, &mut info)?;
    let identity = info.identity()?;
    if identity.stream != STREAM_PLAYBACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "card {} device {} opened as stream {} — not playback",
                playback.card, playback.device, identity.stream
            ),
        ));
    }
    if identity.card != i32::try_from(playback.card).unwrap_or(i32::MAX) || identity.device != playback.device {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is card {} device {}, but /proc/asound/pcm named card {} device {}",
                playback.node().display(),
                identity.card,
                identity.device,
                playback.card,
                playback.device
            ),
        ));
    }
    Ok(identity)
}

/// What the device settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chosen {
    rate: u32,
    channels: u32,
    frame_bytes: sys::FrameBytes,
    period_frames: u64,
    buffer_frames: u64,
    fifo_frames: u64,
}

/// Fill a parameter block with the constraints that are not negotiable.
///
/// Access, format, subformat, channels and rate: a device that cannot do these
/// is a device this daemon declines, because resampling and format conversion
/// belong in the mixer where every stream shares one code path, not in a
/// per-card special case.
fn constrain(params: &mut HwParams, request: Request) -> io::Result<()> {
    params.set_any()?;
    params.set_mask(Mask::Access, ACCESS_RW_INTERLEAVED)?;
    params.set_mask(Mask::Format, FORMAT_S16_LE)?;
    params.set_mask(Mask::Subformat, SUBFORMAT_STD)?;
    params.set_interval(Interval::Channels, request.channels)?;
    params.set_interval(Interval::Rate, request.rate)?;
    Ok(())
}

/// Pick a period and buffer size out of what the device said it can do.
///
/// Separated from the ioctls because this is where an arithmetic mistake hides:
/// a buffer that is not a whole number of periods gives a short final transfer
/// every wrap, and a buffer of one period cannot play while the writer fills.
fn choose_sizes(
    period_range: Range,
    buffer_range: Range,
    request: Request,
) -> io::Result<(u32, u32)> {
    let unsatisfiable = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the device admits no {what} at all"),
        )
    };
    let period_low = period_range.lowest().ok_or_else(|| unsatisfiable("period size"))?;
    let period_high = period_range.highest().ok_or_else(|| unsatisfiable("period size"))?;
    let buffer_low = buffer_range.lowest().ok_or_else(|| unsatisfiable("buffer size"))?;
    let buffer_high = buffer_range.highest().ok_or_else(|| unsatisfiable("buffer size"))?;

    let periods = request.periods.max(2);
    // The period must also be small enough that `periods` of them fit.
    let period_ceiling = period_high.min(buffer_high / periods.max(1));

    if period_ceiling < period_low {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "no period size in [{period_low}, {period_high}] leaves room for {periods} \
                 periods inside a buffer of at most {buffer_high} frames"
            ),
        ));
    }
    let period = request.period_frames.clamp(period_low, period_ceiling);
    // A ZERO period is not caught by the range check above: a refined interval
    // whose minimum is 0 makes `period_low` 0 too, so `period_ceiling <
    // period_low` is `0 < 0` and false, and `clamp` then answers 0. Both bounds
    // are ioctl readback, which is exactly the untrusted input AGENTS.md means,
    // and the division below is where it lands.
    if period == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the device refined the period size to a range starting at zero                  ([{period_low}, {period_high}] against a buffer of at most                  {buffer_high} frames), and a zero-frame period is not a transfer size"
            ),
        ));
    }

    // Whole periods only, never fewer than two, and inside what the device
    // admits at both ends.
    let wanted = period.saturating_mul(periods);
    let capped = wanted.min(buffer_high);
    let whole = (capped / period).max(2);
    let buffer = period.saturating_mul(whole);
    if buffer < buffer_low || buffer > buffer_high {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a whole number of {period}-frame periods cannot land inside the device's \
                 buffer range [{buffer_low}, {buffer_high}]"
            ),
        ));
    }
    Ok((period, buffer))
}

/// Refine, commit, and read back.
fn configure(fd: RawFd, request: Request) -> io::Result<Chosen> {
    let mut params = HwParams::zeroed();
    constrain(&mut params, request)?;
    sys::hw_refine(fd, &mut params).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "the device cannot do {} Hz {}-channel S16_LE interleaved: {e}",
                request.rate, request.channels
            ),
        )
    })?;
    let sizes = choose_sizes(
        params.interval(Interval::PeriodSize)?,
        params.interval(Interval::BufferSize)?,
        request,
    );

    // Two attempts, each from a fresh block: the sizes this daemon chose, then
    // the period alone. The refined ranges are a per-parameter answer, so a pair
    // drawn from them can still be jointly unsatisfiable on a card with a
    // coupled constraint — which is a reason to fall back, not a reason to fail.
    // When `sizes` could not be chosen at all there is nothing to drop, so the
    // second attempt repeats the first; the retry costs one ioctl and keeps the
    // no-sizes path from needing a shape of its own.
    //
    // There is deliberately no third rung leaving the sizes entirely open.
    // `snd_pcm_hw_params_choose` narrows ACCESS, FORMAT, SUBFORMAT, CHANNELS,
    // RATE, PERIOD_TIME, BUFFER_SIZE and TICK_TIME to single values in place —
    // but NOT PERIOD_SIZE. A rung that left it a two-value interval would have
    // its configuration COMMITTED by the kernel and then rejected by
    // `require_exact` here, which is the worst of both: the device configured,
    // and this daemon saying it is not. So the period is pinned on every rung
    // that has one, and a rung that pinned nothing else would be a repeat of
    // the one above it.
    let mut last = None;
    for attempt in 0..2 {
        let mut params = HwParams::zeroed();
        constrain(&mut params, request)?;
        match (attempt, &sizes) {
            (0, Ok((period, buffer))) => {
                params.set_interval(Interval::PeriodSize, *period)?;
                params.set_interval(Interval::BufferSize, *buffer)?;
            }
            (1, Ok((period, _))) => {
                params.set_interval(Interval::PeriodSize, *period)?;
            }
            _ => {}
        }
        match sys::hw_params(fd, &mut params) {
            Ok(()) => return readback(&params, request),
            Err(e) => last = Some(e),
        }
    }
    let reason = match last {
        Some(e) => e,
        None => io::Error::other("no configuration was attempted"),
    };
    Err(io::Error::new(
        reason.kind(),
        match sizes {
            Ok((period, buffer)) => format!(
                "the device refused every configuration, last {period}-frame periods in a \
                 {buffer}-frame buffer: {reason}"
            ),
            Err(why) => format!("the device refused every configuration ({why}): {reason}"),
        },
    ))
}

/// What §K.4 calls "then read back".
fn readback(params: &HwParams, request: Request) -> io::Result<Chosen> {
    let refused = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);

    let access = params.mask_exact(Mask::Access)?;
    if access != Some(ACCESS_RW_INTERLEAVED) {
        return Err(refused(format!(
            "the device chose access mode {access:?}, not RW_INTERLEAVED — every transfer \
             below assumes interleaved write ioctls"
        )));
    }
    let format = params.mask_exact(Mask::Format)?;
    if format != Some(FORMAT_S16_LE) {
        return Err(refused(format!(
            "the device chose sample format {format:?}, not S16_LE"
        )));
    }
    let channels = params.require_exact(Interval::Channels)?;
    if channels != request.channels {
        return Err(refused(format!(
            "the device chose {channels} channels, not the {} asked for",
            request.channels
        )));
    }
    let rate = params.require_exact(Interval::Rate)?;
    if rate != request.rate {
        return Err(refused(format!(
            "the device chose {rate} Hz, not the {} Hz asked for — everything would play at \
             the wrong pitch",
            request.rate
        )));
    }
    let frame_bits = params.require_exact(Interval::FrameBits)?;
    let sample_bits = params.require_exact(Interval::SampleBits)?;
    if sample_bits != 16 || frame_bits != channels.saturating_mul(16) {
        return Err(refused(format!(
            "the device chose {sample_bits}-bit samples in {frame_bits}-bit frames, which is \
             not {channels}-channel S16_LE"
        )));
    }
    let frame_bytes = sys::FrameBytes::from_frame_bits(frame_bits).ok_or_else(|| {
        refused(format!(
            "the device chose {frame_bits}-bit frames, which is not a whole number of bytes"
        ))
    })?;
    let period_frames = u64::from(params.require_exact(Interval::PeriodSize)?);
    let buffer_frames = u64::from(params.require_exact(Interval::BufferSize)?);
    if period_frames == 0 || buffer_frames < period_frames.saturating_mul(2) {
        return Err(refused(format!(
            "the device chose a {buffer_frames}-frame buffer for {period_frames}-frame \
             periods, which cannot play one period while the writer fills another"
        )));
    }
    Ok(Chosen {
        rate,
        channels,
        frame_bytes,
        period_frames,
        buffer_frames,
        fifo_frames: params.fifo_size()?,
    })
}

/// How many waits a device drain may take before it is stopped anyway. At
/// `DRAIN_WAIT_MS` each this is a little over a second, which is longer than
/// any buffer this daemon configures.
const DRAIN_PASSES: u32 = 64;

/// How long one of those waits blocks.
const DRAIN_WAIT_MS: i32 = 20;

impl AudioSink for AlsaSink {
    fn spec(&self) -> Spec {
        self.spec
    }

    fn device_delay(&mut self) -> io::Result<u64> {
        let frames = sys::delay(self.fd())?;
        // A negative delay is what some drivers report once a stream has run
        // past its own data. It is not a count of anything that is still to be
        // heard, so it contributes zero to the latency sum rather than being
        // reinterpreted as a large unsigned number.
        Ok(u64::try_from(frames).unwrap_or(0))
    }

    fn wait(&mut self, timeout_ms: i32) -> io::Result<Wait> {
        Ok(match sys::poll_writable(self.fd(), timeout_ms)? {
            Ready::Timeout => Wait::Timeout,
            Ready::Writable => Wait::Writable,
            Ready::Broken => Wait::Underrun,
            Ready::Gone => Wait::Gone,
        })
    }

    fn write(&mut self, pcm: &[u8]) -> io::Result<usize> {
        let frames = pcm.len() / self.spec.frame_bytes.max(1);
        if frames == 0 {
            return Ok(0);
        }
        match sys::writei(self.fd(), self.frame_bytes, pcm, frames) {
            Ok(accepted) => Ok(accepted),
            // A non-blocking transfer with a full ring is backpressure, not an
            // error: the caller keeps the frames and offers them again.
            Err(e) if e.raw_os_error() == Some(sys::EAGAIN) => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn is_running(&self) -> bool {
        self.started
    }

    /// The PCM's own descriptor, so the daemon can wait on the device and its
    /// clients in one `poll(2)`.
    fn raw_fd(&self) -> Option<RawFd> {
        Some(self.file.as_raw_fd())
    }

    fn start(&mut self) -> io::Result<()> {
        sys::start(self.fd())?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        sys::drop_pcm(self.fd())?;
        self.started = false;
        Ok(())
    }

    /// Play out what is queued, then stop.
    ///
    /// On a non-blocking descriptor the ioctl starts the drain and returns
    /// `EAGAIN`; the wait is ours. `poll` reports the stream stopping as
    /// `POLLERR`/`POLLHUP`, and `DELAY` reaching zero is the other end of it, so
    /// either ends the wait. The pass bound is what stops a card that has
    /// stopped consuming from hanging shutdown — §K.4's own argument for
    /// bounding `drain_all` applies to the device drain too.
    fn drain(&mut self) -> io::Result<()> {
        match sys::drain(self.fd())? {
            sys::Draining::Finished => {}
            sys::Draining::Started => {
                for _ in 0..DRAIN_PASSES {
                    // An error here is NOT "nothing left to play", and must
                    // not end the loop: reading it as zero broke out and
                    // dropped the device, truncating exactly the tail this
                    // drain exists to finish. Keep waiting instead — the poll
                    // below reports a device that has really gone, and
                    // DRAIN_PASSES bounds the wait either way.
                    if let Ok(0) = self.device_delay() {
                        break;
                    }
                    match sys::poll_writable(self.fd(), DRAIN_WAIT_MS)? {
                        sys::Ready::Broken | sys::Ready::Gone => break,
                        sys::Ready::Writable | sys::Ready::Timeout => {}
                    }
                }
                // However the wait ended — played out, stopped, or out of
                // passes — the device is left stopped rather than in DRAINING,
                // which is the state the caller asked for. The one path that
                // skips this is a `poll` that fails outright, which propagates:
                // a descriptor this daemon cannot even wait on is not a device
                // to issue a further ioctl to.
                sys::drop_pcm(self.fd())?;
            }
        }
        self.started = false;
        Ok(())
    }

    fn recover(&mut self) -> io::Result<()> {
        sys::prepare(self.fd())?;
        self.started = false;
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
    use crate::pcm::HwParamsExt;

    fn open(min: u32, max: u32) -> Range {
        Range { min, max, openmin: false, openmax: false, integer: true, empty: false }
    }

    #[test]
    fn the_constraints_narrow_exactly_the_non_negotiable_parameters() {
        let mut params = HwParams::zeroed();
        constrain(&mut params, Request::default()).unwrap();
        assert_eq!(params.mask_exact(Mask::Access).unwrap(), Some(ACCESS_RW_INTERLEAVED));
        assert_eq!(params.mask_exact(Mask::Format).unwrap(), Some(FORMAT_S16_LE));
        assert_eq!(params.mask_exact(Mask::Subformat).unwrap(), Some(SUBFORMAT_STD));
        assert_eq!(params.require_exact(Interval::Rate).unwrap(), 48000);
        assert_eq!(params.require_exact(Interval::Channels).unwrap(), 2);
        // Sizes are deliberately left open here: they come from the refinement.
        assert!(params.require_exact(Interval::PeriodSize).is_err());
        assert!(params.require_exact(Interval::BufferSize).is_err());
    }

    #[test]
    fn the_requested_sizes_survive_a_generous_device() {
        let (period, buffer) =
            choose_sizes(open(64, 8192), open(128, 65536), Request::default()).unwrap();
        assert_eq!(period, 1024);
        assert_eq!(buffer, 4096);
        assert_eq!(buffer % period, 0);
    }

    /// A card that will not go as small as the request gets the request clamped
    /// up, and the buffer follows — this is the case that used to produce a
    /// buffer that was not a whole number of periods.
    #[test]
    fn a_coarse_device_still_gets_whole_periods() {
        let (period, buffer) =
            choose_sizes(open(2048, 8192), open(4096, 65536), Request::default()).unwrap();
        assert_eq!(period, 2048);
        assert_eq!(buffer, 8192);
        assert_eq!(buffer % period, 0);
        assert!(buffer / period >= 2);
    }

    /// A card with a small maximum buffer gets fewer periods rather than a
    /// buffer it cannot hold.
    #[test]
    fn a_small_buffer_reduces_the_period_count_not_the_alignment() {
        let (period, buffer) = choose_sizes(open(64, 8192), open(64, 2048), Request::default()).unwrap();
        assert!(buffer <= 2048);
        assert_eq!(buffer % period, 0);
        assert!(buffer / period >= 2, "one period cannot play while the writer fills");
    }

    #[test]
    fn at_least_two_periods_even_when_one_was_asked_for() {
        let request = Request { periods: 1, ..Request::default() };
        let (period, buffer) = choose_sizes(open(64, 8192), open(64, 65536), request).unwrap();
        assert_eq!(buffer / period, 2);
    }

    #[test]
    fn an_unsatisfiable_device_is_refused_by_name() {
        let empty = Range { min: 0, max: 0, openmin: false, openmax: false, integer: true, empty: true };
        let err = choose_sizes(empty, open(64, 65536), Request::default()).unwrap_err();
        assert!(err.to_string().contains("no period size"), "{err}");
        let err = choose_sizes(open(64, 8192), empty, Request::default()).unwrap_err();
        assert!(err.to_string().contains("no buffer size"), "{err}");
        // A device whose smallest period cannot fit twice into its largest
        // buffer is refused rather than served with a one-period ring.
        let err = choose_sizes(open(4096, 8192), open(64, 4096), Request::default()).unwrap_err();
        assert!(err.to_string().contains("leaves room for"), "{err}");
    }

    /// The readback is what §K.4 says it is: a device that narrowed a parameter
    /// to something other than what was asked is refused, and the diagnostic
    /// says which parameter and to what.
    #[test]
    fn a_device_that_changed_the_rate_is_refused() {
        let mut params = HwParams::zeroed();
        constrain(&mut params, Request::default()).unwrap();
        params.set_interval(Interval::Rate, 44100).unwrap();
        params.set_interval(Interval::SampleBits, 16).unwrap();
        params.set_interval(Interval::FrameBits, 32).unwrap();
        params.set_interval(Interval::PeriodSize, 1024).unwrap();
        params.set_interval(Interval::BufferSize, 4096).unwrap();
        let err = readback(&params, Request::default()).unwrap_err();
        assert!(err.to_string().contains("44100 Hz, not the 48000"), "{err}");
        assert!(err.to_string().contains("wrong pitch"), "{err}");
    }

    #[test]
    fn a_device_that_changed_the_format_or_access_is_refused() {
        let base = |f: fn(&mut HwParams)| {
            let mut params = HwParams::zeroed();
            constrain(&mut params, Request::default()).unwrap();
            params.set_interval(Interval::SampleBits, 16).unwrap();
            params.set_interval(Interval::FrameBits, 32).unwrap();
            params.set_interval(Interval::PeriodSize, 1024).unwrap();
            params.set_interval(Interval::BufferSize, 4096).unwrap();
            f(&mut params);
            params
        };
        let params = base(|p| p.set_mask(Mask::Format, 10).unwrap());
        assert!(readback(&params, Request::default())
            .unwrap_err()
            .to_string()
            .contains("not S16_LE"));
        let params = base(|p| p.set_mask(Mask::Access, 0).unwrap());
        assert!(readback(&params, Request::default())
            .unwrap_err()
            .to_string()
            .contains("not RW_INTERLEAVED"));
        // A frame size that is not channels * 16 bits would make every transfer
        // length wrong, so it is caught here rather than in the mixer.
        let params = base(|p| p.set_interval(Interval::FrameBits, 48).unwrap());
        assert!(readback(&params, Request::default())
            .unwrap_err()
            .to_string()
            .contains("not 2-channel S16_LE"));
    }

    #[test]
    fn a_one_period_ring_is_refused() {
        let mut params = HwParams::zeroed();
        constrain(&mut params, Request::default()).unwrap();
        params.set_interval(Interval::SampleBits, 16).unwrap();
        params.set_interval(Interval::FrameBits, 32).unwrap();
        params.set_interval(Interval::PeriodSize, 1024).unwrap();
        params.set_interval(Interval::BufferSize, 1024).unwrap();
        let err = readback(&params, Request::default()).unwrap_err();
        assert!(err.to_string().contains("cannot play one period"), "{err}");
    }

    #[test]
    fn a_good_readback_reports_what_the_device_chose() {
        let mut params = HwParams::zeroed();
        constrain(&mut params, Request::default()).unwrap();
        params.set_interval(Interval::SampleBits, 16).unwrap();
        params.set_interval(Interval::FrameBits, 32).unwrap();
        params.set_interval(Interval::PeriodSize, 940).unwrap();
        params.set_interval(Interval::BufferSize, 3760).unwrap();
        let chosen = readback(&params, Request::default()).unwrap();
        assert_eq!(chosen.rate, 48000);
        assert_eq!(chosen.frame_bytes.get(), 4);
        // Not the requested 1024: the point of the readback is to carry what the
        // device actually took, so the mixer sizes its transfers to that.
        assert_eq!(chosen.period_frames, 940);
        assert_eq!(chosen.buffer_frames, 3760);
    }
}
