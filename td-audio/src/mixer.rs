//! Many streams, one device.
//!
//! §K.5 refuses a single-stream v1: "a daily driver wants browser plus
//! notification, so mix from the start". Everything here is protocol-free on
//! purpose — a stream is an id, a queue of interleaved `S16_LE` bytes and a
//! volume, and nothing in this module knows what a tagstruct is. Rung 26 adds
//! the Pulse server above it; the fixture in `main.rs` drives it today.
//!
//! # The three accounting traps, and how each is avoided
//!
//! §K.3 names them, and all three are places where a plausible implementation is
//! wrong in a way that only shows up as bad lip-sync.
//!
//! **Do not double-count.** Bytes already accepted that have been handed to the
//! mixer are represented in the mixer queue, so adding the accept counter to the
//! queue counts them twice and the clock runs ahead of the sound by exactly the
//! overlap. So a stream tracks ONE position — `frames_accepted` — and everything
//! else is derived: what is still in its own queue is `accepted - mixed`, and
//! what is in the shared output path is a property of the output, not of the
//! stream.
//!
//! **A timing reply is not a scalar.** A Pulse client computes latency from
//! timestamps plus read and write indexes, so `Timing` below reports a
//! consistent SET rather than one number squeezed into a field.
//!
//! **Per-stream drain is not ALSA `DRAIN`.** That ioctl drains and stops the
//! shared mixed PCM, which would silence every other application. A stream is
//! drained when its own last output frame has been consumed by the device, which
//! is bookkeeping against `out_frames_mixed` and the device delay — see
//! `drain_mark`.

use crate::sink::{is_underrun, AudioSink, Spec, Wait, SAMPLE_BYTES};
use std::collections::VecDeque;
use std::fmt;
use std::io;

/// `PA_VOLUME_NORM`: unity gain. Volumes are the protocol's own fixed-point
/// scale, so rung 26 needs no conversion and this module needs no floats in its
/// interface.
pub const VOLUME_NORM: u32 = 0x1_0000;
/// The loudest a stream may be set to — 0 dB. td does not offer software gain
/// above unity, because summing two amplified streams clips the mix rather than
/// making either louder.
pub const VOLUME_MAX: u32 = VOLUME_NORM;

/// The most this mixer will hold queued across ALL streams.
///
/// The per-stream ceiling, the streams-per-client cap and the client cap are
/// each bounded and each justified where it is declared. Their PRODUCT is not:
/// thirty-two clients times thirty-two streams times four mebibytes is four
/// gibibytes of queued PCM, and a client reaches its share by ignoring the byte
/// grants it is sent — nothing about a grant stops the write that follows it.
/// A budget over the mixer as a whole is the only place that product can be
/// bounded, because it is the only place that sees all of it.
///
/// Sixty-four mebibytes is roughly six minutes of audio at the fixed spec, and
/// more than a second apiece for a thousand streams. A client past it is short-
/// written and told so, which is the same answer its own ceiling already gives.
pub const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

/// The most streams the shared mixer will admit across every client.
///
/// The per-client ceiling alone composes to 1,024 streams. Even with a byte
/// ceiling, walking that many empty or corked streams on every device period
/// gives a client a cheap way to spend the daemon's single audio thread. The
/// reservation check in [`Mixer::can_open`] also makes the byte ceiling a
/// promise made at admission rather than a short write after a grant.
pub const MAX_STREAMS: usize = 128;

/// The largest period this mixer will size a pass from.
///
/// One second at 192 kHz, which is far past anything a card asks for and still
/// a number rather than whatever the kernel said. The accumulator is sized from
/// it, and an allocation that fails aborts instead of returning an error.
const MAX_PERIOD_FRAMES: usize = 192_000;

/// Underflow records retained for one stream.
///
/// A live run reserves one record until it either plays continuously or turns
/// into an exact wire event. The event replaces, rather than adds to, that
/// reservation. Ordinary hardware holds only a few periods; this larger
/// ceiling makes a non-reading client backpressure its own next discontinuous
/// run instead of growing either queue on the audio thread.
const MAX_UNDERFLOW_RECORDS: usize = 1024;

/// A stream's handle. Opaque so that the Pulse channel number, which is a
/// protocol detail, never becomes the mixer's identity for it.
///
/// The field is private and `Mixer::open` is the only way to obtain one, which
/// is what makes that sentence true. It was not: sessions numbered their Pulse
/// channels from zero independently of each other and handed those numbers
/// straight to the mixer, so the second client to connect collided with the
/// first on channel 0 and was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u32);

impl StreamId {
    /// A process-global Pulse sink-input index for this admission.
    pub(crate) fn sink_input_index(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy)]
struct Run {
    output_start: u64,
    output_end: u64,
}

#[derive(Clone, Copy)]
struct UnderflowCandidate {
    /// The discontinuous run this accepted prefix belongs to.
    run_start: u64,
    /// Where the stream exhausted its private queue in that run.
    run_end: u64,
    /// How far the sink has accepted. This can precede `run_end` after a
    /// short write, and is already a real playback endpoint.
    accepted_end: u64,
    /// The same accepted endpoint on this stream's own frame clock.
    stream_end: u64,
    /// Pulse DRAIN deliberately owns this endpoint.
    suppress: bool,
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What one client is playing.
struct Stream {
    id: StreamId,
    /// Interleaved `S16_LE` at the sink's spec, as accepted from the client.
    queue: VecDeque<u8>,
    /// The most this stream may keep queued. Backpressure, and the number the
    /// Pulse `REQUEST` grant is computed from.
    limit_frames: u64,
    /// The negotiated watermark that drives Pulse byte grants.
    ///
    /// A watermark below one device period must itself release a transfer: a
    /// conforming client cannot grow the queue beyond the bytes we granted.
    target_frames: u64,
    volume: u32,
    /// THE position. Frames this stream has handed the mixer, ever.
    frames_accepted: u64,
    /// Frames of this stream's audio that have been mixed into the output.
    frames_mixed: u64,
    /// The output position at which this stream's queued audio runs out, set
    /// whenever its queue empties. `None` while it still has audio queued.
    drain_mark: Option<u64>,
    /// The output position after the last frame this stream contributed.
    ///
    /// THE per-stream anchor on the shared output timeline. Without it a
    /// stream's played count has to be derived from the GLOBAL backlog, and
    /// then one stream's audio in flight makes every other stream's position
    /// regress: a stream that finished playing minutes ago reports a
    /// `read_index` of zero and a latency equal to somebody else's buffer,
    /// while `is_drained` says it is done. The two answers contradicting each
    /// other is the bug; this field is what makes them agree.
    out_end: u64,
    /// Paused by the client. A corked stream keeps its queue and contributes
    /// nothing, which is the difference between pausing and muting: muting
    /// plays silence over the audio, pausing leaves the audio where it is.
    corked: bool,
    /// Do not consume this stream until its queue reaches `prebuffer_frames`.
    prebuffering: bool,
    /// The bounded Pulse threshold selected for this stream.
    prebuffer_frames: u64,
    /// `TRIGGER` or `DRAIN` explicitly released this stream's short tail.
    ///
    /// An empty queue is not evidence that a continuous client is finished:
    /// it can merely be between writes. Only an explicit release lets such a
    /// tail start a prepared device before its ring is otherwise primed.
    short_start_released: bool,
    /// This run crossed its non-zero prebuffer threshold while the device was
    /// stopped. It may finish priming that stopped ring, but START consumes the
    /// permission so it cannot eagerly drain later continuous refills.
    threshold_start_released: bool,
    /// This stream contributed to the device since its last START/PREPARE.
    priming_contributed: bool,
    /// At least one such contribution was not explicitly released.
    priming_blocks_short_start: bool,
    /// End frame of this stream's contribution to the current pending buffer.
    ///
    /// Every contribution begins at frame zero. Keeping the end, rather than
    /// a boolean, lets a partial sink write retire a shorter stream's
    /// provenance before a surviving longer tail crosses PREPARE.
    pending_contribution_end: usize,
    /// That pending contribution was not explicitly released.
    pending_blocks_short_start: bool,
    /// The exhausted run represented by this stream's contribution to the
    /// current pending transfer. It is captured at mix time so a refill or
    /// DRAIN arriving between short writes cannot rewrite its history.
    pending_underflow_run: Option<Run>,
    /// Discontinuous contributions that have not wholly passed the device.
    /// One current-run anchor is insufficient: a stream can resume while its
    /// earlier run is still in the hardware backlog.
    runs: VecDeque<Run>,
    /// This stream contributed to the current render run and has not yet been
    /// observed empty. It drives Pulse STARTED/UNDERFLOW transitions.
    running: bool,
    /// An explicit Pulse DRAIN owns the current exhausted endpoint. Reaching
    /// it completes the request rather than reporting client starvation.
    draining: bool,
    /// It ran dry while the shared playhead crossed its accepted endpoint.
    underflows: u32,
    /// Exact stream-frame positions owed in Pulse UNDERFLOW events.
    underflow_events: VecDeque<u64>,
    /// Exhausted accepted runs on the shared output axis.
    ///
    /// Emptying the private queue into a still-playing device is not itself
    /// an underflow. A contiguous refill accepted before playback reaches the
    /// endpoint extends the same candidate. A refill after peer-only output is
    /// a new candidate and cannot erase the earlier gap.
    underflow_candidates: VecDeque<UnderflowCandidate>,
    /// A write was refused because the queue was full.
    overflows: u32,
}

impl Stream {
    fn queued_frames(&self, frame_bytes: usize) -> u64 {
        (self.queue.len() / frame_bytes.max(1)) as u64
    }

    fn short_transfer_released(&self, frame_bytes: usize, allow_threshold_start: bool) -> bool {
        let queued = self.queued_frames(frame_bytes);
        self.short_start_released
            || (allow_threshold_start && self.threshold_start_released)
            // A negotiated target below one device period cannot grow into a
            // full transfer: the byte-grant loop holds it at this watermark.
            // Direct Mixer users default the target to the queue limit.
            || queued >= self.target_frames
    }

    fn can_contribute_at(&self, output_start: u64) -> bool {
        self.runs
            .back()
            .is_some_and(|run| run.output_end == output_start)
            || self.runs.len().saturating_add(self.underflow_events.len()) < MAX_UNDERFLOW_RECORDS
    }
}

/// A consistent set of positions for one stream, as §K.3 requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Bytes the client has written, ever. Pulse's `write_index`.
    pub write_index: u64,
    /// Bytes of that stream the device has actually played. Pulse's
    /// `read_index`, and never ahead of `write_index`.
    pub read_index: u64,
    /// Frames still in this stream's own queue.
    pub queued_frames: u64,
    /// Frames of mixed output not yet played — the mixer's staging buffer plus
    /// whatever the kernel and device still hold.
    pub output_backlog_frames: u64,
    /// Frames still inside the kernel and the device alone.
    pub device_delay_frames: u64,
    /// The whole sum, converted at the actually-negotiated rate.
    pub latency_usec: u64,
}

/// What one `pump` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pumped {
    /// Frames of mixed output the device accepted.
    pub frames_written: u64,
    /// Frames mixed but not yet accepted, still held for the next pump.
    pub frames_pending: u64,
}

/// One bounded transfer from the stream queues into the shared output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct MixPlan {
    frames: usize,
    /// A sub-period transfer must leave continuous contributors queued.
    released_only: bool,
    /// Include a prebuffer-complete run which is priming a stopped device.
    allow_threshold_start: bool,
}

/// Every stream, summed into one device.
pub struct Mixer {
    spec: Spec,
    /// Smallest working queue the server negotiates for a new stream.
    ///
    /// Production sets this to the selected device ring plus one transfer
    /// period. A client may ask for more latency, but accepting less can leave
    /// no software refill reserve behind the initially primed ring.
    target_floor_frames: u64,
    streams: Vec<Stream>,
    /// `channels * period` accumulators, allocated once (AGENTS.md: outside hot
    /// loops) and cleared per pass.
    accumulator: Vec<f32>,
    /// Mixed output the sink has not accepted yet. Never dropped: dropping it
    /// would put a gap in the middle of every client's audio.
    pending: Vec<u8>,
    /// Where `pending` starts, so a partially-accepted buffer is not re-copied.
    pending_at: usize,
    /// Frames mixed into the output path, ever.
    out_frames_mixed: u64,
    /// Frames the sink has accepted, ever.
    out_frames_written: u64,
    /// The device's own delay, as of the last pump.
    device_delay: u64,
    /// A removed stream contributed frames that remain in the stopped ring.
    ///
    /// Removing the stream releases those unavoidable shared-ring frames: the
    /// daemon cannot discard them without also discarding every peer's mix,
    /// and retaining its vanished start blocker would strand the ring forever.
    retired_priming_contributed: bool,
    /// End frame of a removed stream's contribution still in `pending`.
    retired_pending_contribution_end: usize,
    /// The next id `open` will issue. Monotonic and never reused, so a stream
    /// that has been removed cannot be confused with a later one.
    next_id: u64,
}

impl Mixer {
    pub fn new(spec: Spec) -> Self {
        Self::with_target_floor(spec, 0)
    }

    pub fn with_target_floor(spec: Spec, target_floor_frames: u64) -> Self {
        Self {
            spec,
            target_floor_frames,
            streams: Vec::new(),
            accumulator: Vec::new(),
            pending: Vec::new(),
            pending_at: 0,
            out_frames_mixed: 0,
            out_frames_written: 0,
            device_delay: 0,
            retired_priming_contributed: false,
            retired_pending_contribution_end: 0,
            next_id: 0,
        }
    }

    pub fn target_floor_frames(&self) -> u64 {
        self.target_floor_frames
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Whether one more stream and its whole queue reservation fit.
    pub fn can_open(&self, limit_frames: u64) -> bool {
        if self.streams.len() >= MAX_STREAMS {
            return false;
        }
        let frame_bytes = self.spec.frame_bytes.max(1) as u64;
        let reserved = self.streams.iter().fold(0u64, |total, stream| {
            total.saturating_add(stream.limit_frames.saturating_mul(frame_bytes))
        });
        let requested = limit_frames.max(1).saturating_mul(frame_bytes);
        reserved.saturating_add(requested) <= MAX_QUEUED_BYTES as u64
    }

    /// Admit a stream and name it, with a queue limit in frames.
    ///
    /// The id is issued here rather than chosen by the caller. A caller that
    /// picks its own has to know what every other caller picked, and the
    /// sessions cannot: each numbers its Pulse channels from zero.
    pub fn open(&mut self, limit_frames: u64) -> io::Result<StreamId> {
        if !self.can_open(limit_frames) {
            return Err(io::Error::other(
                "the shared mixer stream or queue reservation limit is full",
            ));
        }
        if self.next_id >= u64::from(u32::MAX) {
            return Err(io::Error::other(
                "the process-global stream id space is exhausted",
            ));
        }
        let raw = u32::try_from(self.next_id)
            .map_err(|_| io::Error::other("the process-global stream id does not fit u32"))?;
        self.next_id = self.next_id.saturating_add(1);
        let id = StreamId(raw);
        if self.find(id).is_some() {
            return Err(io::Error::other(
                "the next process-global stream id is live",
            ));
        }
        self.create(id, limit_frames)?;
        Ok(id)
    }

    /// Admit a stream under an id the caller names. Private: `open` is the
    /// public door, and this exists so the tests below can pin behaviour to a
    /// known id.
    fn create(&mut self, id: StreamId, limit_frames: u64) -> io::Result<()> {
        if self.streams.iter().any(|s| s.id == id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("stream {} already exists", id.0),
            ));
        }
        self.streams.push(Stream {
            id,
            queue: VecDeque::new(),
            limit_frames: limit_frames.max(1),
            target_frames: limit_frames.max(1),
            volume: VOLUME_NORM,
            frames_accepted: 0,
            frames_mixed: 0,
            drain_mark: Some(0),
            out_end: 0,
            corked: false,
            prebuffering: false,
            prebuffer_frames: 0,
            // `open` has not selected a non-zero threshold yet. Treat that
            // state like Pulse's explicit zero prebuffer; Session immediately
            // replaces it with the client's negotiated value.
            short_start_released: true,
            threshold_start_released: false,
            priming_contributed: false,
            priming_blocks_short_start: false,
            pending_contribution_end: 0,
            pending_blocks_short_start: false,
            pending_underflow_run: None,
            runs: VecDeque::new(),
            running: false,
            draining: false,
            underflows: 0,
            underflow_events: VecDeque::new(),
            underflow_candidates: VecDeque::new(),
            overflows: 0,
        });
        Ok(())
    }

    pub fn remove(&mut self, id: StreamId) {
        self.retain_streams(|stream| stream.id != id);
    }

    /// Drop one stream's private queue without resetting its lifetime Pulse
    /// clock or disturbing output already mixed into the shared path.
    pub fn flush(&mut self, id: StreamId) -> io::Result<()> {
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.queue.clear();
        // Flushed client bytes are consumed from the protocol clock even
        // though they deliberately never reach the device.
        stream.frames_mixed = stream.frames_accepted;
        stream.drain_mark = Some(stream.out_end);
        stream.prebuffering = stream.prebuffer_frames > 0;
        stream.short_start_released = stream.prebuffer_frames == 0;
        stream.threshold_start_released = false;
        stream.running = false;
        stream.draining = false;
        stream.pending_underflow_run = None;
        stream.underflow_candidates.clear();
        Ok(())
    }

    fn find(&self, id: StreamId) -> Option<&Stream> {
        self.streams.iter().find(|s| s.id == id)
    }

    fn find_mut(&mut self, id: StreamId) -> Option<&mut Stream> {
        self.streams.iter_mut().find(|s| s.id == id)
    }

    fn missing(id: StreamId) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, format!("no stream {}", id.0))
    }

    /// Frames this stream may still send before its queue is full.
    ///
    /// This is what a Pulse `REQUEST` grant is computed from, and §K.3 is blunt
    /// about why it matters: without byte grants the client writes one buffer
    /// and stops forever.
    pub fn request_frames(&self, id: StreamId) -> io::Result<u64> {
        let stream = self.find(id).ok_or_else(|| Self::missing(id))?;
        Ok(stream
            .limit_frames
            .saturating_sub(stream.queued_frames(self.spec.frame_bytes)))
    }

    /// Accept interleaved `S16_LE` from a client. Returns the BYTES taken,
    /// which is short when the queue fills.
    pub fn write(&mut self, id: StreamId, pcm: &[u8]) -> io::Result<usize> {
        let frame_bytes = self.spec.frame_bytes.max(1);
        // Summed rather than carried: every other lookup here is a scan of the
        // same short list, and a running total is a number that can drift from
        // the queues it claims to describe.
        let held: usize = self.streams.iter().map(|s| s.queue.len()).sum();
        let shared_room_frames = (MAX_QUEUED_BYTES.saturating_sub(held) / frame_bytes) as u64;
        let stream = self
            .streams
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| Self::missing(id))?;
        let room_frames = stream
            .limit_frames
            .saturating_sub((stream.queue.len() / frame_bytes) as u64)
            .min(shared_room_frames);
        let offered_frames = (pcm.len() / frame_bytes) as u64;
        let taken_frames = offered_frames.min(room_frames);
        if taken_frames < offered_frames {
            stream.overflows = stream.overflows.saturating_add(1);
        }
        let taken = usize::try_from(taken_frames.saturating_mul(frame_bytes as u64))
            .unwrap_or(0)
            .min(pcm.len());
        stream.queue.extend(pcm.get(..taken).unwrap_or(&[]));
        stream.frames_accepted = stream.frames_accepted.saturating_add(taken_frames);
        if taken_frames > 0 {
            // A refill after the private queue reached zero is still
            // contiguous while earlier frames remain in the device ring.
            // The shared playhead, not queue emptiness or a later write,
            // decides whether the accepted run underflowed.
            stream.drain_mark = None;
        }
        Ok(taken)
    }

    /// Pause or resume a stream.
    ///
    /// The session cannot express this on its own: it can stop granting the
    /// client more bytes, but the frames already queued are the mixer's, and
    /// without this the mixer plays out a paused stream's whole buffered tail —
    /// up to `maxlength`, which is four times the target buffer.
    pub fn set_corked(&mut self, id: StreamId, corked: bool) -> io::Result<()> {
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.corked = corked;
        if corked {
            stream.running = false;
        }
        Ok(())
    }

    /// Select and arm or release this stream's prebuffer threshold.
    pub fn set_prebuffer(&mut self, id: StreamId, frames: u64, armed: bool) -> io::Result<()> {
        let pending_frame = self.pending_at / self.spec.frame_bytes.max(1);
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.prebuffer_frames = frames.min(stream.limit_frames);
        stream.prebuffering = armed && stream.prebuffer_frames > 0;
        if stream.prebuffering {
            stream.running = false;
        }
        // An explicit zero threshold is Pulse's request to start immediately;
        // it is a release even though there is no threshold to arm.
        let blocks_short_start = armed && stream.prebuffer_frames > 0;
        stream.short_start_released = !blocks_short_start;
        stream.threshold_start_released = false;
        // Arming prepares the NEXT client run; it must not revoke a release
        // already granted to frames inside the shared ring or `pending`.
        if !blocks_short_start && stream.priming_contributed {
            stream.priming_blocks_short_start = false;
        }
        if !blocks_short_start && stream.pending_contribution_end > pending_frame {
            stream.pending_blocks_short_start = false;
        }
        Ok(())
    }

    /// Bind short-transfer release to the queue watermark granted on the wire.
    pub fn set_target_frames(&mut self, id: StreamId, frames: u64) -> io::Result<()> {
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.target_frames = frames.max(1).min(stream.limit_frames);
        Ok(())
    }

    /// Release a Pulse DRAIN without turning its intentional endpoint into an
    /// UNDERFLOW. The session clears an immediately completed drain; a live
    /// endpoint clears itself when the shared playhead reaches it.
    pub fn begin_drain(&mut self, id: StreamId, frames: u64) -> io::Result<()> {
        self.set_prebuffer(id, frames, false)?;
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.draining = true;
        if let Some(mark) = stream.drain_mark {
            if let Some(candidate) = stream
                .underflow_candidates
                .iter_mut()
                .rev()
                .find(|candidate| candidate.run_end == mark)
            {
                candidate.suppress = true;
            }
        }
        Ok(())
    }

    pub fn finish_drain(&mut self, id: StreamId) -> io::Result<()> {
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.draining = false;
        Ok(())
    }

    pub fn set_volume(&mut self, id: StreamId, volume: u32) -> io::Result<()> {
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        stream.volume = volume.min(VOLUME_MAX);
        Ok(())
    }

    /// Has everything this stream queued reached the speakers?
    ///
    /// Not an ioctl. The mark is the output position at which this stream's
    /// audio ended; the device has played `out_frames_written - device_delay`.
    pub fn is_drained(&self, id: StreamId) -> io::Result<bool> {
        let stream = self.find(id).ok_or_else(|| Self::missing(id))?;
        let Some(mark) = stream.drain_mark else {
            return Ok(false);
        };
        Ok(self.frames_played() >= mark)
    }

    /// Output frames the device has actually played.
    fn frames_played(&self) -> u64 {
        self.out_frames_written.saturating_sub(self.device_delay)
    }

    /// The §K.3 position set for one stream.
    pub fn timing(&self, id: StreamId) -> io::Result<Timing> {
        let stream = self.find(id).ok_or_else(|| Self::missing(id))?;
        let frame_bytes = self.spec.frame_bytes.max(1) as u64;
        let queued = stream.frames_accepted.saturating_sub(stream.frames_mixed);
        let backlog = self.out_frames_mixed.saturating_sub(self.frames_played());
        // This stream's OWN unplayed share, measured from where its audio ends
        // on the shared output timeline rather than from the global backlog.
        // The global figure is right only for whichever stream is currently
        // feeding the mixer: for any other one it charges somebody else's
        // buffer to this stream, which drives `read_index` backwards and
        // reports latency for a stream that finished long ago.
        // Every outstanding run contributes only its own frames. Keeping all
        // runs that overlap the device backlog handles a resume before an
        // earlier run has played without charging another stream's gap.
        let played_output = self.frames_played();
        let unplayed_share = stream.runs.iter().fold(0u64, |total, run| {
            let run_frames = run.output_end.saturating_sub(run.output_start);
            let played_in_run = played_output
                .saturating_sub(run.output_start)
                .min(run_frames);
            total.saturating_add(run_frames.saturating_sub(played_in_run))
        });
        let played = stream.frames_mixed.saturating_sub(unplayed_share);
        let latency_frames = queued.saturating_add(unplayed_share);
        Ok(Timing {
            write_index: stream.frames_accepted.saturating_mul(frame_bytes),
            read_index: played.saturating_mul(frame_bytes),
            queued_frames: queued,
            output_backlog_frames: backlog,
            device_delay_frames: self.device_delay,
            latency_usec: self.spec.frames_to_usec(latency_frames),
        })
    }

    /// Keep only the streams whose ids are listed, dropping the rest.
    ///
    /// The daemon reconciles the mixer against the sessions that are still
    /// connected rather than trusting a disconnect to have told it. A client
    /// that vanishes mid-stream leaves audio queued, and audio nobody owns
    /// would be summed into the output forever.
    pub fn retain(&mut self, live: &[StreamId]) {
        self.retain_streams(|stream| live.contains(&stream.id));
    }

    fn retain_streams(&mut self, mut keep: impl FnMut(&Stream) -> bool) {
        let pending_frame = self.pending_at / self.spec.frame_bytes.max(1);
        let mut retired_priming = false;
        let mut retired_pending_end = 0;
        self.streams.retain(|stream| {
            if keep(stream) {
                return true;
            }
            retired_priming |= stream.priming_contributed;
            if stream.pending_contribution_end > pending_frame {
                retired_pending_end = retired_pending_end.max(stream.pending_contribution_end);
            }
            false
        });
        self.retired_priming_contributed |= retired_priming;
        self.retired_pending_contribution_end = self
            .retired_pending_contribution_end
            .max(retired_pending_end);
    }

    /// How many times this stream ran dry with the client still writing.
    pub fn underflows(&self, id: StreamId) -> io::Result<u32> {
        Ok(self.find(id).ok_or_else(|| Self::missing(id))?.underflows)
    }

    /// Consume a bounded prefix of exact per-stream underflow positions.
    pub fn take_underflow_positions(&mut self, id: StreamId, limit: usize) -> io::Result<Vec<u64>> {
        let frame_bytes = self.spec.frame_bytes.max(1) as u64;
        let stream = self.find_mut(id).ok_or_else(|| Self::missing(id))?;
        let count = limit.min(stream.underflow_events.len());
        Ok(stream
            .underflow_events
            .drain(..count)
            .map(|frames| frames.saturating_mul(frame_bytes))
            .collect())
    }

    pub fn has_underflow_positions(&self, id: StreamId) -> io::Result<bool> {
        Ok(!self
            .find(id)
            .ok_or_else(|| Self::missing(id))?
            .underflow_events
            .is_empty())
    }

    /// Whether this stream has accepted audio in its current render run.
    pub fn is_running(&self, id: StreamId) -> io::Result<bool> {
        Ok(self.find(id).ok_or_else(|| Self::missing(id))?.running)
    }

    /// How many client writes were shortened because the queue was full.
    pub fn overflows(&self, id: StreamId) -> io::Result<u32> {
        Ok(self.find(id).ok_or_else(|| Self::missing(id))?.overflows)
    }

    /// Whether the shared sink is carrying or preparing real stream audio.
    pub fn sink_is_running(&self) -> bool {
        self.pending_at < self.pending.len()
            || self.out_frames_mixed > self.frames_played()
            || self.streams.iter().any(|stream| stream.running)
    }

    /// Mix at most one period of real client audio and give it to the device.
    ///
    /// Flushes what the sink would not take last time BEFORE mixing anything
    /// new: a short write is backpressure, and audio already mixed must reach
    /// the device in order or there is a gap in every stream at once.
    pub fn pump<S: AudioSink>(&mut self, sink: &mut S) -> io::Result<Pumped> {
        let mut pumped = Pumped::default();
        if self.pending_at >= self.pending.len() {
            // Refresh the shared playhead before a refill can extend an old
            // run. Otherwise a refill that arrives after the device crossed
            // the prior endpoint can hide the underflow it follows.
            self.observe_playhead(sink)?;
            // An empty or threshold-gated mixer has no output clock to advance.
            // Writing a zero period here starts the PCM before the client has
            // buffered audio and inserts a device-sized gap at every refill.
            let device_running = sink.is_running();
            self.arm_ready_streams(device_running);
            let reported_period = sink.period_frames();
            if reported_period > MAX_PERIOD_FRAMES as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the device reported a {reported_period}-frame period"),
                ));
            }
            let period = reported_period as usize;
            let plan = self.mix_plan(period, !device_running);
            if plan.frames == 0 {
                return Ok(pumped);
            }
            self.mix(plan)?;
        }
        let played_before_transfer = self.frames_played();
        pumped.frames_written = self.push_pending(sink)?;
        pumped.frames_pending = self.pending_frames();
        // AFTER the transfer, not before. The delay is how much the device
        // still holds, and frames handed over in this very pump are part of
        // that — reading it first would make `frames_played` count them as
        // already heard, which reports a stream drained while its last period
        // is still in the ring and makes every latency figure short by one
        // transfer.
        // DELAY is the evidence that accepted frames really remain in the
        // device. An XRUN is recoverable by the daemon and a lost device is a
        // terminal condition, but converting either (or an arbitrary EIO) to
        // zero would instead claim that every accepted frame was heard.
        // Until DELAY proves otherwise, newly accepted frames are all still
        // pending. If the read fails, timing remains conservative instead of
        // advancing past audio whose device state is unknown.
        self.device_delay = self
            .out_frames_written
            .saturating_sub(played_before_transfer);
        self.observe_playhead(sink)?;
        Ok(pumped)
    }

    /// Sample and settle the device timeline without consuming new client
    /// audio. The server does this before applying commands from the same poll
    /// wake so CORK and DRAIN cannot retroactively change a crossed endpoint.
    pub fn observe_playhead<S: AudioSink>(&mut self, sink: &mut S) -> io::Result<()> {
        self.device_delay = sink.device_delay()?;
        // Settle first: a due candidate retains its run reservation until its
        // exact event replaces that record. Retiring first would let the
        // candidate escape the shared run/event ceiling.
        self.settle_played_underflows();
        self.retire_played_runs();
        Ok(())
    }

    fn retire_played_runs(&mut self) {
        let played = self.frames_played();
        for stream in &mut self.streams {
            while stream
                .runs
                .front()
                .is_some_and(|run| run.output_end <= played)
            {
                stream.runs.pop_front();
            }
        }
    }

    /// Whether the event loop must poll or drive the device.
    ///
    /// A stopped PCM with only an unreleased short tail already in its ring has
    /// nothing the device can make progress on. Residual device delay is
    /// refreshed by the bounded timer rather than a level-writable PCM fd.
    pub fn has_device_work(&self, device_running: bool, period_frames: u64) -> bool {
        if self.pending_at < self.pending.len() {
            return true;
        }
        let Ok(period) = usize::try_from(period_frames) else {
            return true;
        };
        if period > MAX_PERIOD_FRAMES || self.has_threshold_ready_audio() {
            return true;
        }
        self.mix_plan(period, !device_running).frames > 0
    }

    /// The PCM may start once its ring is full, or it has accepted only a tail
    /// released by an explicit command, a completed initial prebuffer gate, or
    /// a queue ceiling smaller than one device period.
    pub fn ready_to_start(&self, buffer_frames: u64, period_frames: u64) -> bool {
        let released_finite_tail = (self.retired_priming_contributed
            || self.streams.iter().any(|stream| stream.priming_contributed))
            && self
                .streams
                .iter()
                .all(|stream| !stream.priming_blocks_short_start);
        let no_mixable_audio = !self.has_threshold_ready_audio()
            && usize::try_from(period_frames)
                .ok()
                .filter(|period| *period > 0 && *period <= MAX_PERIOD_FRAMES)
                .is_some_and(|period| self.mix_plan(period, true).frames == 0);
        self.device_delay > 0
            && (self.device_delay.saturating_add(period_frames) > buffer_frames
                || (released_finite_tail && no_mixable_audio && self.pending_frames() == 0))
    }

    /// Forget the priming interval after a successful device START.
    pub fn note_started(&mut self) {
        self.reset_priming_contributions();
        for stream in &mut self.streams {
            stream.threshold_start_released = false;
        }
    }

    fn reset_priming_contributions(&mut self) {
        self.retired_priming_contributed = false;
        for stream in &mut self.streams {
            stream.priming_contributed = false;
            stream.priming_blocks_short_start = false;
        }
    }

    fn pending_frames(&self) -> u64 {
        ((self.pending.len().saturating_sub(self.pending_at)) / self.spec.frame_bytes.max(1)) as u64
    }

    /// Release every stream whose negotiated threshold is complete.
    fn arm_ready_streams(&mut self, device_running: bool) {
        let frame_bytes = self.spec.frame_bytes.max(1);
        for stream in &mut self.streams {
            if stream.prebuffering && stream.queued_frames(frame_bytes) >= stream.prebuffer_frames {
                stream.prebuffering = false;
                stream.threshold_start_released = !device_running;
            }
        }
    }

    /// Whether the next pump will open a prebuffer gate and find audio.
    fn has_threshold_ready_audio(&self) -> bool {
        let frame_bytes = self.spec.frame_bytes.max(1);
        self.streams.iter().any(|stream| {
            !stream.corked
                && stream.prebuffering
                && stream.queued_frames(frame_bytes) >= stream.prebuffer_frames
        })
    }

    /// The largest real contribution available for the next device write.
    ///
    /// A period is the device's maximum transfer granularity, not a license to
    /// invent the rest of one. A continuous stream's sub-period remainder is
    /// not a finite tail either: hold it for the next client write instead of
    /// draining the queue and risking an XRUN between two pieces of one signal.
    /// `TRIGGER`, `DRAIN`, zero prebuffer, a completed stopped-device prebuffer
    /// gate, and a full below-period queue release a real short tail. When
    /// another stream has a whole period, its output advances the shared clock
    /// and silence from a shorter stream is the correct mix for the missing
    /// interval.
    fn mix_plan(&self, limit: usize, allow_threshold_start: bool) -> MixPlan {
        let frame_bytes = self.spec.frame_bytes.max(1);
        let mut largest_released = 0;
        for stream in self.streams.iter().filter(|stream| {
            !stream.corked
                && !stream.prebuffering
                && stream.can_contribute_at(self.out_frames_mixed)
        }) {
            let queued = (stream.queue.len() / frame_bytes).min(limit);
            if queued == limit {
                return MixPlan {
                    frames: limit,
                    released_only: false,
                    allow_threshold_start,
                };
            }
            if stream.short_transfer_released(frame_bytes, allow_threshold_start) {
                largest_released = largest_released.max(queued);
            }
        }
        MixPlan {
            frames: largest_released,
            released_only: largest_released > 0,
            allow_threshold_start,
        }
    }

    /// Sum every stream into `pending`.
    fn mix(&mut self, plan: MixPlan) -> io::Result<()> {
        let frames = plan.frames;
        let channels = self.spec.channels.max(1) as usize;
        let frame_bytes = self.spec.frame_bytes.max(1);
        // The transfer count is capped by the already-validated device period.
        // Keep the local bound too: it pins the allocation below even if a
        // future caller derives a transfer without going through `pump`.
        if frames > MAX_PERIOD_FRAMES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the mixer derived a {frames}-frame transfer"),
            ));
        }
        let samples = frames.saturating_mul(channels);
        self.accumulator.clear();
        self.accumulator.resize(samples, 0.0);
        for stream in &mut self.streams {
            stream.pending_contribution_end = 0;
            stream.pending_blocks_short_start = false;
            stream.pending_underflow_run = None;
        }
        self.retired_pending_contribution_end = 0;

        let out_base = self.out_frames_mixed;
        for stream in &mut self.streams {
            if stream.corked || stream.prebuffering || !stream.can_contribute_at(out_base) {
                // Gated from this output interval: contributes nothing and
                // keeps its queue. Its `out_end` stays where it is, so the
                // run ledger records a later contribution after the gap.
                continue;
            }
            let released = stream.short_transfer_released(frame_bytes, plan.allow_threshold_start);
            let queued = stream.queue.len() / frame_bytes;
            let mut available = queued.min(frames);
            if plan.released_only && !released {
                // A released peer may advance the shared output, but it must
                // not silence a continuous stream that can span that whole
                // interval. Emptying the queue is the operation that needs a
                // release, not contribution to a peer-sized transfer.
                available = available.min(queued.saturating_sub(1));
            }
            if available == 0 {
                // A stream with nothing left is only underflowing if it had not
                // already been marked drained: a client that finished playing
                // and is waiting is not starving.
                if stream.drain_mark.is_none() {
                    // This path records where the private queue ended. The
                    // accepted endpoint below decides whether a refill arrived
                    // before the shared playhead crossed the run.
                    stream.drain_mark = Some(out_base);
                    stream.out_end = out_base;
                }
                continue;
            }
            stream.pending_contribution_end = available;
            stream.pending_blocks_short_start = !released;
            stream.running = true;
            let gain = stream.volume as f32 / VOLUME_NORM as f32;
            for index in 0..available.saturating_mul(channels) {
                let low = stream.queue.pop_front().unwrap_or(0);
                let high = stream.queue.pop_front().unwrap_or(0);
                let sample = f32::from(i16::from_le_bytes([low, high]));
                if let Some(slot) = self.accumulator.get_mut(index) {
                    *slot += sample * gain;
                }
            }
            stream.frames_mixed = stream.frames_mixed.saturating_add(available as u64);
            stream.out_end = out_base.saturating_add(available as u64);
            if let Some(run) = stream
                .runs
                .back_mut()
                .filter(|run| run.output_end == out_base)
            {
                run.output_end = stream.out_end;
            } else {
                stream.runs.push_back(Run {
                    output_start: out_base,
                    output_end: stream.out_end,
                });
            }
            if stream.queue.len() < frame_bytes {
                // Its audio ends here, at this output position.
                stream.drain_mark = Some(out_base.saturating_add(available as u64));
                stream.pending_underflow_run = stream.runs.back().copied();
            }
        }

        self.pending.clear();
        self.pending_at = 0;
        self.pending.reserve(samples.saturating_mul(SAMPLE_BYTES));
        for sample in &self.accumulator {
            // Saturating summation: a mix that would exceed full scale clips
            // rather than wrapping, because a wrap is a click and a clip is
            // loudness.
            let clamped = sample.clamp(f32::from(i16::MIN), f32::from(i16::MAX));
            self.pending
                .extend_from_slice(&(clamped as i16).to_le_bytes());
        }
        self.out_frames_mixed = out_base.saturating_add(frames as u64);
        Ok(())
    }

    /// Offer `pending` to the sink, keeping whatever it would not take.
    fn push_pending<S: AudioSink>(&mut self, sink: &mut S) -> io::Result<u64> {
        let frame_bytes = self.spec.frame_bytes.max(1);
        let tail = self.pending.get(self.pending_at..).unwrap_or(&[]);
        if tail.is_empty() {
            return Ok(0);
        }
        let old_pending_frame = self.pending_at / frame_bytes;
        let accepted = sink.write(tail)?;
        if accepted > 0 {
            let accepted_end = old_pending_frame.saturating_add(accepted);
            let mix_base = self
                .out_frames_written
                .saturating_sub(old_pending_frame as u64);
            if self.retired_pending_contribution_end > old_pending_frame {
                self.retired_priming_contributed = true;
            }
            for stream in &mut self.streams {
                if stream.pending_contribution_end > old_pending_frame {
                    stream.priming_contributed = true;
                    // Only the accepted prefix can become audible. A first
                    // transfer that itself gets EPIPE must not blame the new
                    // stream, and a shorter contributor must not inherit a
                    // longer peer's accepted endpoint.
                    let contribution_end = stream.pending_contribution_end.min(accepted_end) as u64;
                    // Every accepted prefix of an exhausted contribution can
                    // become an audible endpoint. A later short write extends
                    // it; an XRUN before that write must still account for the
                    // prefix the device did accept. Distinct run starts prove
                    // a peer-only gap and therefore retain distinct endpoints.
                    if let Some(run) = stream.pending_underflow_run {
                        let candidate_end = mix_base.saturating_add(contribution_end);
                        let run_frames = run.output_end.saturating_sub(run.output_start);
                        let stream_start = stream.frames_mixed.saturating_sub(run_frames);
                        let stream_end = stream_start
                            .saturating_add(candidate_end.saturating_sub(run.output_start));
                        if let Some(candidate) = stream
                            .underflow_candidates
                            .back_mut()
                            .filter(|candidate| candidate.run_start == run.output_start)
                        {
                            candidate.run_end = run.output_end;
                            candidate.accepted_end = candidate.accepted_end.max(candidate_end);
                            candidate.stream_end = candidate.stream_end.max(stream_end);
                            candidate.suppress |= stream.draining;
                        } else {
                            stream.underflow_candidates.push_back(UnderflowCandidate {
                                run_start: run.output_start,
                                run_end: run.output_end,
                                accepted_end: candidate_end,
                                stream_end,
                                suppress: stream.draining,
                            });
                        }
                    }
                    stream.running = true;
                    if stream.pending_blocks_short_start {
                        stream.priming_blocks_short_start = true;
                    }
                }
            }
        }
        self.pending_at = self
            .pending_at
            .saturating_add(accepted.saturating_mul(frame_bytes));
        self.out_frames_written = self.out_frames_written.saturating_add(accepted as u64);
        Ok(accepted as u64)
    }

    /// Close a stream's run only when the shared playback clock reaches its
    /// accepted endpoint. A refill accepted first extends that endpoint; a
    /// peer can advance the clock across it without a hardware XRUN.
    fn settle_played_underflows(&mut self) {
        let played = self.frames_played();
        let accepted = self.out_frames_written;
        for stream in &mut self.streams {
            while let Some(candidate) = stream.underflow_candidates.front().copied() {
                if played < candidate.accepted_end {
                    break;
                }
                if !candidate.suppress
                    && !stream.corked
                    && stream.underflow_events.len() >= MAX_UNDERFLOW_RECORDS
                {
                    break;
                }
                stream.underflow_candidates.pop_front();
                if candidate.suppress {
                    // A partial write can make the first accepted prefix reach
                    // the playhead before the rest of the drained run enters
                    // the ring. Keep DRAIN ownership until its full mixed
                    // endpoint is resolved, then return later starvation to
                    // ordinary UNDERFLOW handling.
                    if played >= candidate.run_end {
                        stream.draining = false;
                    }
                } else if !stream.corked {
                    stream.underflows = stream.underflows.saturating_add(1);
                    stream.underflow_events.push_back(candidate.stream_end);
                    if stream.prebuffer_frames > 0 {
                        stream.prebuffering = true;
                        stream.threshold_start_released = false;
                        stream.short_start_released = false;
                    }
                }
                stream.running = false;
            }
            // A later discontinuous run can already be accepted behind the
            // peer-only gap that just underflowed. It is not subject to the
            // newly rearmed private-queue gate: transition back to STARTED
            // when the shared playhead actually enters those accepted frames.
            if !stream.corked
                && stream.runs.iter().any(|run| {
                    let accepted_end = run.output_end.min(accepted);
                    run.output_start <= played
                        && run.output_start < accepted_end
                        && played < accepted_end
                })
            {
                stream.running = true;
            }
        }
    }

    /// Recover the device after an underrun and re-prime it.
    ///
    /// The device's own frame counters restart, so the mixer's output axis is
    /// rebased: `out_frames_written` is the position the device is at, and
    /// everything mixed but unaccepted is still ahead of it.
    pub fn recover<S: AudioSink>(&mut self, sink: &mut S) -> io::Result<()> {
        sink.recover()?;
        self.device_delay = 0;
        self.reset_priming_contributions();
        self.settle_played_underflows();
        // The ring's contents are GONE — `PREPARE` discards them — so frames
        // this mixer had handed over and not yet counted as played were never
        // heard. `frames_played()` is `out_frames_written - device_delay`, and
        // zeroing the delay credits exactly those lost frames as played. That
        // is deliberate and it is the only self-consistent choice: the device's
        // own counters restart at the prepare, so the mixer's output axis has
        // to restart with them or every later position is offset by the gap.
        // The cost is that a stream whose audio was in the discarded window
        // reports drained without having been heard. An underrun already means
        // the listener missed that audio; reporting it as still pending would
        // hold a `DRAIN` open for frames no device will ever play.
        self.retire_played_runs();
        Ok(())
    }

    /// Run the device until every stream has drained, or `max_passes` passes
    /// have gone by. Returns the passes used.
    ///
    /// The bound is not decoration: a device that stops accepting frames would
    /// otherwise make this loop forever, and a daemon that hangs on shutdown is
    /// worse than one that reports it could not drain.
    pub fn drain_all<S: AudioSink>(&mut self, sink: &mut S, max_passes: u32) -> io::Result<u32> {
        // Device shutdown is an implicit drain of every client. A negotiated
        // prebuffer threshold must not strand a final short tail here any more
        // than an explicit Pulse DRAIN may strand it.
        let pending_frame = self.pending_at / self.spec.frame_bytes.max(1);
        for stream in &mut self.streams {
            stream.prebuffering = false;
            stream.short_start_released = true;
            if stream.priming_contributed {
                stream.priming_blocks_short_start = false;
            }
            if stream.pending_contribution_end > pending_frame {
                stream.pending_blocks_short_start = false;
            }
        }
        for pass in 0..max_passes {
            let all_drained = self.streams.iter().all(|s| {
                s.drain_mark
                    .is_some_and(|mark| self.frames_played() >= mark)
            });
            if all_drained && self.pending_at >= self.pending.len() {
                return Ok(pass);
            }
            match sink.wait(100)? {
                Wait::Gone => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the device went away while draining",
                    ));
                }
                Wait::Underrun => self.recover(sink)?,
                Wait::Writable | Wait::Timeout => {
                    // The transfer-side underrun, not just the poll-side one.
                    // `poll` saying writable and the ring emptying before the
                    // write lands is an ordinary race on a busy machine, and
                    // treating it as fatal ends a shutdown that was working.
                    match self.pump(sink) {
                        Ok(_) => {}
                        Err(error) if is_underrun(&error) => {
                            self.recover(sink)?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    // `recover` leaves the PCM PREPARED but stopped, and
                    // `SwParams::set_playback` puts `start_threshold` at the
                    // boundary so it never starts itself. Without this the
                    // drain fills a stationary ring and spends every pass
                    // waiting for a device that was never told to play.
                    if !sink.is_running()
                        && self.ready_to_start(sink.buffer_frames(), sink.period_frames())
                    {
                        sink.start()?;
                        self.note_started();
                    }
                }
            }
        }
        Ok(max_passes)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::sink::MemorySink;
    use crate::tone::{Generator, Tone};

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn an_idle_mixer_does_not_start_the_device_with_silence() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        assert_eq!(mixer.pump(&mut sink).unwrap(), Pumped::default());
        assert_eq!(sink.frames_written(), 0);
        assert!(!sink.is_running());

        mixer.open(1000).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap(), Pumped::default());
        assert_eq!(sink.frames_written(), 0, "an empty stream is not audio");
        assert!(!sink.is_running());
    }

    #[test]
    fn a_failed_delay_read_is_not_reported_as_played_audio() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(1000).unwrap();
        mixer.write(id, &stereo(&[1, 2, 3, 4])).unwrap();
        sink.fail_delay_with(Some(32));
        assert_eq!(
            mixer.pump(&mut sink).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(mixer.timing(id).unwrap().read_index, 0);
    }

    fn stereo(frames: &[i16]) -> Vec<u8> {
        frames
            .iter()
            .flat_map(|s| [*s, *s])
            .flat_map(|s| s.to_le_bytes())
            .collect()
    }

    /// Two admissions are two streams, whatever the callers would have called
    /// them.
    ///
    /// Sessions number their Pulse channels from zero and do not know about
    /// each other, so when the channel WAS the mixer's key the second client's
    /// first stream collided with the first client's and was refused.
    #[test]
    fn every_admission_gets_an_id_of_its_own() {
        let mut mixer = Mixer::new(Spec::fixed());
        let first = mixer.open(1000).unwrap();
        let second = mixer.open(1000).unwrap();
        assert_ne!(first, second);
        assert_eq!(mixer.stream_count(), 2);
        // And no id is handed out twice while it is live.
        mixer.remove(first);
        let third = mixer.open(1000).unwrap();
        assert_ne!(third, first);
        assert_ne!(third, second);

        let mut exhausted = Mixer::new(Spec::fixed());
        exhausted.next_id = u64::from(u32::MAX - 1);
        assert_eq!(
            exhausted.open(1000).unwrap().sink_input_index(),
            u32::MAX - 1
        );
        assert!(exhausted.open(1000).is_err(), "INVALID is never issued");
    }

    /// Stream count and reserved queue bytes are shared-daemon limits, not
    /// values each one of 32 clients gets independently.
    #[test]
    fn admissions_reserve_one_bounded_shared_budget() {
        let mut mixer = Mixer::new(Spec::fixed());
        let frame_bytes = Spec::fixed().frame_bytes as u64;
        let huge = (MAX_QUEUED_BYTES as u64 / frame_bytes / 2).max(1);
        mixer.open(huge).unwrap();
        mixer.open(huge).unwrap();
        assert!(!mixer.can_open(1), "all queue bytes were already reserved");
        assert!(mixer.open(1).is_err());

        let mut mixer = Mixer::new(Spec::fixed());
        for _ in 0..MAX_STREAMS {
            mixer.open(1).unwrap();
        }
        assert_eq!(mixer.stream_count(), MAX_STREAMS);
        assert!(!mixer.can_open(1), "the daemon-wide stream cap is exact");
        assert!(mixer.open(1).is_err());
    }

    /// No client audio means no synthetic output, and a stream does not enter
    /// the device until its negotiated prebuffer threshold is complete.
    #[test]
    fn idle_output_is_suppressed_and_initial_prebuffering_is_enforced() {
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        assert_eq!(sink.frames_written(), 0);

        let id = mixer.open(16).unwrap();
        mixer.set_prebuffer(id, 4, true).unwrap();
        mixer.write(id, &stereo(&[700, 700, 700])).unwrap();
        assert!(
            !mixer.has_device_work(sink.is_running(), sink.period_frames()),
            "three frames cannot cross a four-frame gate"
        );
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);

        mixer.write(id, &stereo(&[700])).unwrap();
        assert!(mixer.has_device_work(sink.is_running(), sink.period_frames()));
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert!(sink
            .samples()
            .get(..8)
            .is_some_and(|samples| { samples.iter().all(|sample| *sample == 700) }));

        mixer.write(id, &stereo(&[800, 800, 800])).unwrap();
        assert_eq!(
            mixer.pump(&mut sink).unwrap().frames_written,
            3,
            "copying a run into the device must not invent an underflow"
        );
        assert_eq!(mixer.underflows(id).unwrap(), 0);
    }

    /// A client refill that arrives while its preceding frames remain in the
    /// device is one continuous run. Queue emptiness is an implementation
    /// boundary between the private and device buffers, not a Pulse underflow.
    #[test]
    fn a_refill_before_the_device_xrun_stays_contiguous() {
        let mut sink = MemorySink::new(Spec::fixed(), 8, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(16).unwrap();
        mixer.set_prebuffer(id, 4, true).unwrap();
        mixer.write(id, &stereo(&[1, 2, 3, 4, 5, 6, 7, 8])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert!(mixer.ready_to_start(8, 4));
        sink.start().unwrap();
        mixer.note_started();

        mixer.write(id, &stereo(&[9, 10, 11, 12])).unwrap();
        assert_eq!(mixer.underflows(id).unwrap(), 0);
        sink.advance(4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.underflows(id).unwrap(), 0);
        assert_eq!(
            sink.samples(),
            (1i16..=12)
                .flat_map(|sample| [sample, sample])
                .collect::<Vec<_>>()
        );
    }

    /// An XRUN left by an older device epoch is not an underflow in a stream
    /// whose first transfer the device never accepted.
    #[test]
    fn a_first_transfer_that_finds_an_old_xrun_is_not_blamed_for_it() {
        let mut sink = MemorySink::new(Spec::fixed(), 8, 4);
        sink.start().unwrap();
        sink.advance(1);

        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(8).unwrap();
        mixer.set_prebuffer(id, 0, false).unwrap();
        mixer.write(id, &stereo(&[1, 2, 3, 4])).unwrap();
        assert!(is_underrun(&mixer.pump(&mut sink).unwrap_err()));
        mixer.recover(&mut sink).unwrap();
        assert_eq!(mixer.underflows(id).unwrap(), 0);
    }

    /// A peer can carry the device across one stream's endpoint without an
    /// ALSA XRUN. That stream underflows once at its own endpoint and does not
    /// inherit the peer's later hardware failure.
    #[test]
    fn a_peer_can_expose_one_stream_underflow_without_recharging_it() {
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let early = mixer.open(8).unwrap();
        let tail = mixer.open(16).unwrap();
        mixer.set_prebuffer(early, 1, true).unwrap();
        mixer.set_prebuffer(tail, 4, true).unwrap();
        mixer.write(early, &stereo(&[10])).unwrap();
        mixer
            .write(tail, &stereo(&[20, 20, 20, 20, 30, 30, 30, 30]))
            .unwrap();

        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(2);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.underflows(early).unwrap(), 1);
        assert_eq!(mixer.underflows(tail).unwrap(), 0);
        sink.advance(7);
        mixer.recover(&mut sink).unwrap();

        assert_eq!(mixer.underflows(early).unwrap(), 1);
        assert_eq!(mixer.underflows(tail).unwrap(), 1);
    }

    #[test]
    fn underflow_records_share_one_backpressure_ceiling() {
        let mut sink = MemorySink::new(Spec::fixed(), 4096, 1);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let stream = mixer.open(2).unwrap();
        let peer = mixer.open(4096).unwrap();
        mixer.set_prebuffer(stream, 0, false).unwrap();
        mixer.set_prebuffer(peer, 0, false).unwrap();
        mixer
            .write(peer, &stereo(&vec![0; MAX_UNDERFLOW_RECORDS * 2 + 32]))
            .unwrap();

        for sample in 0..MAX_UNDERFLOW_RECORDS {
            mixer.write(stream, &stereo(&[sample as i16])).unwrap();
            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
            sink.advance(2);
            mixer.observe_playhead(&mut sink).unwrap();
        }

        let state = mixer.find(stream).unwrap();
        assert_eq!(state.underflow_events.len(), MAX_UNDERFLOW_RECORDS);
        assert!(state.underflow_candidates.is_empty());
        assert!(state.runs.is_empty());

        mixer.write(stream, &stereo(&[123])).unwrap();
        for _ in 0..16 {
            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
            sink.advance(1);
            mixer.observe_playhead(&mut sink).unwrap();
        }
        let state = mixer.find(stream).unwrap();
        assert_eq!(state.queued_frames(Spec::fixed().frame_bytes), 1);
        assert_eq!(state.underflow_events.len(), MAX_UNDERFLOW_RECORDS);
        assert!(state.underflow_candidates.is_empty());
        assert!(state.runs.is_empty());

        assert_eq!(mixer.take_underflow_positions(stream, 1).unwrap().len(), 1);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
        sink.advance(2);
        mixer.observe_playhead(&mut sink).unwrap();
        let state = mixer.find(stream).unwrap();
        assert_eq!(state.underflow_events.len(), MAX_UNDERFLOW_RECORDS);
        assert!(state.underflow_candidates.is_empty());
        assert!(state.runs.is_empty());
    }

    /// Corking does not erase accepted output. If the client resumes before
    /// that same device epoch underruns, its surviving tail still identifies
    /// the stream whose continuity was lost.
    #[test]
    fn cork_then_resume_preserves_the_accepted_xrun_endpoint() {
        let mut sink = MemorySink::new(Spec::fixed(), 8, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(8).unwrap();
        mixer.set_prebuffer(id, 4, true).unwrap();
        mixer.write(id, &stereo(&[1, 2, 3, 4])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        sink.start().unwrap();
        mixer.note_started();

        mixer.set_corked(id, true).unwrap();
        mixer.set_corked(id, false).unwrap();
        sink.advance(5);
        mixer.recover(&mut sink).unwrap();

        assert_eq!(mixer.underflows(id).unwrap(), 1);
    }

    /// A continuous sub-period remainder waits for the client's next write,
    /// while an explicitly released finite tail crosses without zero padding.
    #[test]
    fn a_continuous_remainder_waits_but_a_released_tail_does_not() {
        let mut sink = MemorySink::new(Spec::fixed(), 8, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(16).unwrap();
        mixer.set_prebuffer(id, 4, true).unwrap();
        mixer.write(id, &stereo(&[50; 8])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert!(mixer.ready_to_start(8, 4));
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(8);

        mixer
            .write(id, &stereo(&[100, 200, 300, 400, 500]))
            .unwrap();

        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        assert!(
            !mixer.has_device_work(sink.is_running(), sink.period_frames()),
            "the writable PCM cannot complete a continuous remainder"
        );
        mixer.write(id, &stereo(&[600, 700, 800])).unwrap();
        assert!(mixer.has_device_work(sink.is_running(), sink.period_frames()));
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(
            sink.samples().get(16..),
            Some(
                [100, 100, 200, 200, 300, 300, 400, 400, 500, 500, 600, 600, 700, 700, 800, 800]
                    .as_slice()
            )
        );

        let mut tail_sink = MemorySink::new(Spec::fixed(), 16, 4);
        let mut tail_mixer = Mixer::new(Spec::fixed());
        let tail = tail_mixer.open(16).unwrap();
        tail_mixer.set_prebuffer(tail, 4, true).unwrap();
        tail_mixer.write(tail, &stereo(&[900])).unwrap();
        assert_eq!(tail_mixer.pump(&mut tail_sink).unwrap().frames_written, 0);
        assert!(!tail_mixer.has_device_work(tail_sink.is_running(), tail_sink.period_frames()));
        tail_mixer.set_prebuffer(tail, 4, false).unwrap();
        assert!(tail_mixer.has_device_work(tail_sink.is_running(), tail_sink.period_frames()));
        assert_eq!(tail_mixer.pump(&mut tail_sink).unwrap().frames_written, 1);
        assert_eq!(tail_sink.samples(), vec![900, 900]);
    }

    /// Crossing the negotiated gate is sufficient to play a complete short
    /// sound even when neither that sound nor its queue can fill the ALSA ring.
    #[test]
    fn a_prebuffer_complete_short_sound_primes_and_starts() {
        let mut sink = MemorySink::new(Spec::fixed(), 4096, 1024);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(2400).unwrap();
        mixer.set_prebuffer(id, 1441, true).unwrap();
        mixer.write(id, &stereo(&[700; 1500])).unwrap();

        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1024);
        assert!(!mixer.ready_to_start(4096, 1024));
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 476);
        assert!(mixer.ready_to_start(4096, 1024));
        sink.start().unwrap();
        mixer.note_started();
        assert!(sink.is_running());
        assert_eq!(sink.frames_written(), 1500);
        assert!(sink.samples().iter().all(|sample| *sample == 700));
    }

    /// A legal queue smaller than the device period cannot grow into a whole
    /// transfer. Its exact ceiling is therefore a bounded release condition.
    #[test]
    fn a_below_period_queue_ceiling_releases_and_grants_again() {
        let mut sink = MemorySink::new(Spec::fixed(), 4096, 1024);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(512).unwrap();
        mixer.set_prebuffer(id, 512, true).unwrap();
        mixer.write(id, &stereo(&[800; 512])).unwrap();

        assert_eq!(mixer.request_frames(id).unwrap(), 0);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 512);
        assert_eq!(mixer.request_frames(id).unwrap(), 512);
        assert!(sink.samples().iter().all(|sample| *sample == 800));
    }

    /// The device's readback is bounded before a short client tail can make
    /// the transfer itself look harmless.
    #[test]
    fn a_short_tail_does_not_hide_an_oversized_device_period() {
        let mut sink = MemorySink::new(
            Spec::fixed(),
            MAX_PERIOD_FRAMES as u64 * 2,
            MAX_PERIOD_FRAMES as u64 + 1,
        );
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(4).unwrap();
        mixer.write(id, &stereo(&[100])).unwrap();
        let error = mixer.pump(&mut sink).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("192001-frame period"));

        let mut boundary_sink = MemorySink::new(
            Spec::fixed(),
            MAX_PERIOD_FRAMES as u64 * 2,
            MAX_PERIOD_FRAMES as u64,
        );
        let mut boundary_mixer = Mixer::new(Spec::fixed());
        let boundary = boundary_mixer.open(4).unwrap();
        boundary_mixer.write(boundary, &stereo(&[200])).unwrap();
        assert_eq!(
            boundary_mixer
                .pump(&mut boundary_sink)
                .unwrap()
                .frames_written,
            1
        );
    }

    /// The start predicate fills every whole-period slot before releasing a
    /// continuous stream, while still allowing a finite short tail to start.
    #[test]
    fn the_device_start_predicate_primes_its_ring() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(32).unwrap();
        mixer.write(id, &stereo(&[500; 16])).unwrap();
        mixer.pump(&mut sink).unwrap();
        assert!(!mixer.ready_to_start(12, 4));
        mixer.pump(&mut sink).unwrap();
        assert!(!mixer.ready_to_start(12, 4));
        mixer.pump(&mut sink).unwrap();
        assert!(mixer.ready_to_start(12, 4));

        let mut short_sink = MemorySink::new(Spec::fixed(), 12, 4);
        let mut short_mixer = Mixer::new(Spec::fixed());
        let short = short_mixer.open(8).unwrap();
        short_mixer.set_prebuffer(short, 2, true).unwrap();
        short_mixer.write(short, &stereo(&[600, 600])).unwrap();
        assert_eq!(short_mixer.pump(&mut short_sink).unwrap().frames_written, 2);
        assert!(
            short_mixer.ready_to_start(12, 4),
            "crossing the initial threshold must start a complete short sound"
        );

        let mut zero_sink = MemorySink::new(Spec::fixed(), 12, 4);
        let mut zero_mixer = Mixer::new(Spec::fixed());
        let zero = zero_mixer.open(8).unwrap();
        zero_mixer.set_prebuffer(zero, 0, true).unwrap();
        zero_mixer.write(zero, &stereo(&[650, 650])).unwrap();
        zero_mixer.pump(&mut zero_sink).unwrap();
        assert!(
            zero_mixer.ready_to_start(12, 4),
            "an explicit zero prebuffer is an immediate-start request"
        );

        let mut capped_sink = MemorySink::new(Spec::fixed(), 12, 4);
        capped_sink.start().unwrap();
        let mut capped_mixer = Mixer::new(Spec::fixed());
        let capped = capped_mixer.open(2).unwrap();
        capped_mixer.set_prebuffer(capped, 2, true).unwrap();
        capped_mixer.write(capped, &stereo(&[660, 660])).unwrap();
        assert_eq!(capped_mixer.request_frames(capped).unwrap(), 0);
        assert_eq!(
            capped_mixer.pump(&mut capped_sink).unwrap().frames_written,
            2,
            "a below-period queue at capacity must not deadlock its grants"
        );
        assert_eq!(capped_mixer.request_frames(capped).unwrap(), 2);

        let mut waiting_sink = MemorySink::new(Spec::fixed(), 12, 4);
        let mut waiting_mixer = Mixer::new(Spec::fixed());
        let waiting = waiting_mixer.open(8).unwrap();
        waiting_mixer.set_prebuffer(waiting, 3, true).unwrap();
        waiting_mixer.write(waiting, &stereo(&[675, 675])).unwrap();
        waiting_mixer.pump(&mut waiting_sink).unwrap();
        assert!(!waiting_mixer.ready_to_start(12, 4));
        assert!(
            !waiting_mixer.has_device_work(waiting_sink.is_running(), waiting_sink.period_frames()),
            "a stopped PCM cannot advance a below-threshold queue"
        );
        assert!(
            !waiting_mixer.has_device_work(true, waiting_sink.period_frames()),
            "a writable PCM cannot complete a below-threshold queue"
        );

        let mut limited_sink = MemorySink::new(Spec::fixed(), 12, 4);
        limited_sink.limit_writes_to(Some(0));
        let mut limited_mixer = Mixer::new(Spec::fixed());
        let limited = limited_mixer.open(8).unwrap();
        limited_mixer.write(limited, &stereo(&[900, 900])).unwrap();
        limited_mixer.set_prebuffer(limited, 0, false).unwrap();
        assert_eq!(
            limited_mixer
                .pump(&mut limited_sink)
                .unwrap()
                .frames_written,
            0
        );
        assert!(!limited_mixer.ready_to_start(12, 4));
        limited_sink.limit_writes_to(Some(1));
        assert_eq!(
            limited_mixer
                .pump(&mut limited_sink)
                .unwrap()
                .frames_written,
            1
        );
        assert!(
            !limited_mixer.ready_to_start(12, 4),
            "a finite tail is not staged while a mixed frame remains unaccepted"
        );
        assert_eq!(
            limited_mixer
                .pump(&mut limited_sink)
                .unwrap()
                .frames_written,
            1
        );
        assert!(limited_mixer.ready_to_start(12, 4));
    }

    /// One client's partial refill cannot hold another client's DRAIN open.
    #[test]
    fn a_continuous_remainder_does_not_block_a_peers_finite_tail() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let released = mixer.open(8).unwrap();
        let continuous = mixer.open(8).unwrap();
        mixer.write(released, &stereo(&[100])).unwrap();
        mixer.write(continuous, &stereo(&[200])).unwrap();
        mixer.set_prebuffer(released, 0, false).unwrap();
        mixer.set_prebuffer(continuous, 1, true).unwrap();

        assert!(mixer.has_device_work(sink.is_running(), sink.period_frames()));
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
        assert_eq!(mixer.timing(continuous).unwrap().queued_frames, 1);
        assert_eq!(mixer.timing(released).unwrap().queued_frames, 0);
        assert!(
            mixer.ready_to_start(12, 4),
            "the released tail may start while the continuous remainder waits"
        );

        mixer.set_prebuffer(continuous, 0, false).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
        assert!(mixer.ready_to_start(12, 4));
        assert_eq!(sink.samples(), vec![100, 100, 200, 200]);
    }

    /// A finite peer tail may advance the output clock without muting a
    /// continuous stream that already holds enough frames for that interval.
    #[test]
    fn a_released_peer_does_not_silence_a_continuous_contributor() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let continuous = mixer.open(8).unwrap();
        let released = mixer.open(8).unwrap();
        mixer.set_prebuffer(continuous, 4, true).unwrap();
        mixer.write(continuous, &stereo(&[11, 12, 13, 14])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);

        mixer.write(continuous, &stereo(&[15, 16, 17])).unwrap();
        mixer.write(released, &stereo(&[90, 91])).unwrap();
        mixer.set_prebuffer(released, 0, false).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 2);
        assert_eq!(mixer.timing(continuous).unwrap().queued_frames, 1);
        assert_eq!(mixer.timing(released).unwrap().queued_frames, 0);

        mixer.write(continuous, &stereo(&[18, 19, 20])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(
            sink.samples(),
            vec![
                11, 11, 12, 12, 13, 13, 14, 14, 105, 105, 107, 107, 17, 17, 18, 18, 19, 19, 20, 20,
            ]
        );
    }

    /// Flushing older pending audio must not hide a newly ready prebuffer.
    #[test]
    fn a_threshold_ready_stream_blocks_an_underfilled_start_after_a_pending_flush() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.limit_writes_to(Some(0));
        let mut mixer = Mixer::new(Spec::fixed());
        let finite = mixer.open(8).unwrap();
        let ready = mixer.open(8).unwrap();
        mixer.set_prebuffer(finite, 0, false).unwrap();
        mixer.set_prebuffer(ready, 4, true).unwrap();
        mixer.write(finite, &stereo(&[100, 100])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_pending, 2);

        mixer.write(ready, &stereo(&[200, 200, 200, 200])).unwrap();
        sink.limit_writes_to(None);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 2);
        assert!(
            !mixer.ready_to_start(12, 4),
            "flushing pending audio hid the threshold-ready full period"
        );
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
    }

    /// A removed stream cannot take its contribution back out of the shared
    /// ring. Its vanished gate must therefore release that contribution while
    /// every still-live contributor retains its own decision.
    #[test]
    fn removing_a_stream_releases_only_its_unavoidable_ring_tail() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let removed = mixer.open(8).unwrap();
        let driver = mixer.open(8).unwrap();
        mixer.set_prebuffer(removed, 2, true).unwrap();
        mixer.write(removed, &stereo(&[100, 100])).unwrap();
        mixer.write(driver, &stereo(&[10, 10, 10, 10])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert!(!mixer.ready_to_start(12, 4));
        mixer.remove(removed);
        assert!(mixer.ready_to_start(12, 4));

        let mut pending_sink = MemorySink::new(Spec::fixed(), 12, 4);
        pending_sink.start().unwrap();
        pending_sink.limit_writes_to(Some(0));
        let mut pending_mixer = Mixer::new(Spec::fixed());
        let pending = pending_mixer.open(8).unwrap();
        let pending_driver = pending_mixer.open(8).unwrap();
        pending_mixer.set_prebuffer(pending, 2, true).unwrap();
        pending_mixer.write(pending, &stereo(&[200, 200])).unwrap();
        pending_mixer
            .write(pending_driver, &stereo(&[20, 20, 20, 20]))
            .unwrap();
        assert_eq!(
            pending_mixer
                .pump(&mut pending_sink)
                .unwrap()
                .frames_pending,
            4
        );
        pending_mixer.remove(pending);
        pending_sink.limit_writes_to(None);
        assert_eq!(
            pending_mixer
                .pump(&mut pending_sink)
                .unwrap()
                .frames_written,
            4
        );
        assert!(pending_mixer.ready_to_start(12, 4));

        let mut shared_sink = MemorySink::new(Spec::fixed(), 12, 4);
        shared_sink.start().unwrap();
        let mut shared = Mixer::new(Spec::fixed());
        let gone = shared.open(8).unwrap();
        let live = shared.open(8).unwrap();
        let shared_driver = shared.open(8).unwrap();
        shared.set_prebuffer(gone, 1, true).unwrap();
        shared.set_prebuffer(live, 1, true).unwrap();
        shared.write(gone, &stereo(&[300])).unwrap();
        shared.write(live, &stereo(&[400])).unwrap();
        shared
            .write(shared_driver, &stereo(&[10, 10, 10, 10]))
            .unwrap();
        assert_eq!(shared.pump(&mut shared_sink).unwrap().frames_written, 4);
        shared.retain(&[live]);
        assert!(
            !shared.ready_to_start(12, 4),
            "a removed peer must not release a live peer's contribution"
        );
        shared.set_prebuffer(live, 0, false).unwrap();
        assert!(shared.ready_to_start(12, 4));
    }

    /// PREBUF arms the next run. It cannot retroactively revoke a release for
    /// audio already accepted by the ring or already mixed into `pending`.
    #[test]
    fn rearming_prebuffer_preserves_an_existing_tail_release() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let accepted = mixer.open(8).unwrap();
        mixer.set_prebuffer(accepted, 0, false).unwrap();
        mixer.write(accepted, &stereo(&[500, 500])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 2);
        assert!(mixer.ready_to_start(12, 4));
        mixer.set_prebuffer(accepted, 2, true).unwrap();
        assert!(mixer.ready_to_start(12, 4));

        let mut pending_sink = MemorySink::new(Spec::fixed(), 12, 4);
        pending_sink.limit_writes_to(Some(0));
        let mut pending_mixer = Mixer::new(Spec::fixed());
        let pending = pending_mixer.open(8).unwrap();
        pending_mixer.set_prebuffer(pending, 0, false).unwrap();
        pending_mixer.write(pending, &stereo(&[600, 600])).unwrap();
        assert_eq!(
            pending_mixer
                .pump(&mut pending_sink)
                .unwrap()
                .frames_pending,
            2
        );
        pending_mixer.set_prebuffer(pending, 2, true).unwrap();
        pending_sink.limit_writes_to(None);
        assert_eq!(
            pending_mixer
                .pump(&mut pending_sink)
                .unwrap()
                .frames_written,
            2
        );
        assert!(pending_mixer.ready_to_start(12, 4));
    }

    /// PREPARE discards accepted ring frames, but the mixed tail that the sink
    /// has not accepted survives. Its explicit release must survive with it.
    #[test]
    fn a_released_pending_tail_keeps_its_start_provenance_across_recovery() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.limit_writes_to(Some(0));
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(8).unwrap();
        mixer.write(id, &stereo(&[300, 400])).unwrap();
        mixer.set_prebuffer(id, 0, false).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_pending, 2);
        assert!(!mixer.ready_to_start(12, 4));

        mixer.recover(&mut sink).unwrap();
        sink.limit_writes_to(None);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 2);
        assert!(
            mixer.ready_to_start(12, 4),
            "the released pending tail was stranded after PREPARE"
        );
    }

    /// Once a shorter stream's whole contribution was accepted, PREPARE must
    /// not carry its blocker into another stream's surviving pending suffix.
    #[test]
    fn a_partial_prefix_retires_shorter_pending_provenance_before_recovery() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.limit_writes_to(Some(1));
        let mut mixer = Mixer::new(Spec::fixed());
        let one_unreleased = mixer.open(8).unwrap();
        let three_released = mixer.open(8).unwrap();
        mixer.write(one_unreleased, &stereo(&[100])).unwrap();
        mixer
            .write(three_released, &stereo(&[200, 300, 400, 500]))
            .unwrap();
        mixer.set_prebuffer(one_unreleased, 1, true).unwrap();
        mixer.set_prebuffer(three_released, 0, false).unwrap();
        let first = mixer.pump(&mut sink).unwrap();
        assert_eq!(first.frames_written, 1);
        assert_eq!(first.frames_pending, 3);

        mixer.recover(&mut sink).unwrap();
        sink.limit_writes_to(None);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 3);
        assert!(
            mixer.ready_to_start(12, 4),
            "the retired one-frame blocker followed the released suffix"
        );
    }

    #[test]
    fn idle_delay_is_timer_driven_and_errors_are_not_playback() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.write(&stereo(&[100, 100])).unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        assert!(
            !mixer.has_device_work(true, sink.period_frames()),
            "residual delay must not poll a level-writable PCM"
        );
        sink.fail_delay_with(Some(77));
        assert!(
            mixer.pump(&mut sink).is_err(),
            "a failed DELAY must not be credited as played audio"
        );
    }

    /// A corked stream keeps its audio instead of playing it out.
    ///
    /// Corking was session state alone, so the mixer went on consuming the
    /// queue: pausing played the whole buffered tail — up to `maxlength`, four
    /// times the target buffer — before it went quiet.
    #[test]
    fn a_corked_stream_contributes_nothing_and_keeps_its_queue() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(1000).unwrap();
        let audio = stereo(&[100, 200, 300, 400]);
        assert_eq!(mixer.write(id, &audio).unwrap(), audio.len());
        mixer.set_corked(id, true).unwrap();
        let paused = mixer.pump(&mut sink).unwrap();
        assert_eq!(paused.frames_written, 0, "no synthetic silent period");
        assert!(
            sink.samples().iter().all(|s| *s == 0),
            "a paused stream is silent, not merely ungranted"
        );
        assert_eq!(
            mixer.timing(id).unwrap().queued_frames,
            4,
            "and its audio is still there to resume into"
        );
        // Uncorking plays exactly what was held.
        mixer.set_corked(id, false).unwrap();
        mixer.pump(&mut sink).unwrap();
        assert_eq!(mixer.timing(id).unwrap().queued_frames, 0);
        assert!(sink.samples().contains(&100));
    }

    /// An armed prebuffer does not start the device before its threshold, and
    /// TRIGGER's state transition releases the audio already queued.
    #[test]
    fn prebuffering_waits_for_its_threshold_or_a_trigger() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(1000).unwrap();
        mixer.set_prebuffer(id, 8, true).unwrap();
        let audio = stereo(&[100, 200, 300, 400]);
        mixer.write(id, &audio).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap(), Pumped::default());
        assert_eq!(sink.frames_written(), 0);

        mixer.set_prebuffer(id, 8, false).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert!(sink.samples().contains(&100));
    }

    /// A stream that resumes after a gap does not report audio it already
    /// played as unplayed.
    ///
    /// `out_end` anchors only the END of a stream's current run. Measuring the
    /// unplayed share against everything ever mixed charged the whole earlier
    /// run to the new one, so `read_index` — which Pulse clients require to be
    /// monotonic — walked backwards the moment a paused stream resumed. The
    /// device has to be BEHIND for it to show: the wrong figure is bounded by
    /// the backlog, so a device that keeps up hides it.
    #[test]
    fn read_index_does_not_regress_when_a_stream_resumes_after_a_gap() {
        let mut sink = MemorySink::new(Spec::fixed(), 8192, 256);
        sink.start().unwrap();
        let mut mixer = Mixer::new(Spec::fixed());
        let quiet = mixer.open(100_000).unwrap();
        let busy = mixer.open(100_000).unwrap();

        // The stream under test plays a period, and the device plays it out.
        mixer.write(quiet, &stereo(&[500; 256])).unwrap();
        let pumped = mixer.pump(&mut sink).unwrap();
        sink.advance(pumped.frames_written);
        mixer.pump(&mut sink).unwrap();
        let before = mixer.timing(quiet).unwrap().read_index;
        assert!(before > 0, "it played something");

        // Somebody else occupies the output timeline, and the device falls
        // behind by exactly that much.
        for _ in 0..4 {
            mixer.write(busy, &stereo(&[100; 256])).unwrap();
            mixer.pump(&mut sink).unwrap();
        }
        assert_eq!(
            mixer.timing(quiet).unwrap().read_index,
            before,
            "a silent stream's position does not move on its own"
        );

        // And now it resumes, far past where its last run ended.
        mixer.write(quiet, &stereo(&[500; 256])).unwrap();
        mixer.pump(&mut sink).unwrap();
        let after = mixer.timing(quiet).unwrap().read_index;
        assert!(
            after >= before,
            "read_index went backwards: {before} then {after}"
        );
    }

    /// Resuming before an older run leaves the device keeps both pieces in the
    /// stream's latency. One current-segment marker credited the older run as
    /// played merely because another stream created an output-timeline gap.
    #[test]
    fn separate_unplayed_runs_are_both_kept_in_stream_timing() {
        let mut sink = MemorySink::new(Spec::fixed(), 8192, 256);
        let mut mixer = Mixer::new(Spec::fixed());
        let quiet = mixer.open(100_000).unwrap();
        let busy = mixer.open(100_000).unwrap();

        mixer.write(quiet, &stereo(&[500; 256])).unwrap();
        mixer.pump(&mut sink).unwrap();
        mixer.write(busy, &stereo(&[100; 256])).unwrap();
        mixer.pump(&mut sink).unwrap();
        mixer.write(quiet, &stereo(&[500; 256])).unwrap();
        mixer.pump(&mut sink).unwrap();

        let timing = mixer.timing(quiet).unwrap();
        assert_eq!(timing.write_index, 512 * 4);
        assert_eq!(timing.read_index, 0, "neither quiet run has played");
        assert_eq!(timing.latency_usec, Spec::fixed().frames_to_usec(512));
    }

    /// A paused stream cannot exhaust the live run-record ceiling for a
    /// different stream that is actually ready to mix. Preflight must apply
    /// the same cork/prebuffer selection as the mixing pass itself.
    #[test]
    fn paused_stream_run_history_does_not_refuse_runnable_audio() {
        for (corked, prebuffering) in [(true, false), (false, true)] {
            let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
            let mut mixer = Mixer::new(Spec::fixed());
            let paused = mixer.open(1000).unwrap();
            let runnable = mixer.open(1000).unwrap();
            mixer.write(paused, &stereo(&[100; 4])).unwrap();
            mixer.write(runnable, &stereo(&[200; 4])).unwrap();
            let stream = mixer.find_mut(paused).unwrap();
            stream.corked = corked;
            stream.prebuffering = prebuffering;
            stream.prebuffer_frames = 8;
            let base = u64::MAX - (MAX_UNDERFLOW_RECORDS as u64 * 2);
            for offset in 0..MAX_UNDERFLOW_RECORDS as u64 {
                let output_start = base + offset * 2;
                stream.runs.push_back(Run {
                    output_start,
                    output_end: output_start + 1,
                });
            }

            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
            assert!(sink.samples().iter().all(|sample| *sample == 200));
        }

        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let held = mixer.open(1000).unwrap();
        let released = mixer.open(1000).unwrap();
        mixer.write(held, &stereo(&[100])).unwrap();
        mixer.write(released, &stereo(&[200, 200])).unwrap();
        mixer.set_prebuffer(released, 0, false).unwrap();
        let held_stream = mixer.find_mut(held).unwrap();
        held_stream.prebuffer_frames = 4;
        held_stream.short_start_released = false;
        let base = u64::MAX - (MAX_UNDERFLOW_RECORDS as u64 * 2);
        for offset in 0..MAX_UNDERFLOW_RECORDS as u64 {
            let output_start = base + offset * 2;
            held_stream.runs.push_back(Run {
                output_start,
                output_end: output_start + 1,
            });
        }

        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 2);
        assert_eq!(sink.samples(), vec![200i16, 200, 200, 200]);
    }

    /// A short sink acceptance is already an audible run. If PREPARE drops
    /// that prefix before the pending suffix is accepted, the one stream
    /// underflows and its negotiated gate is rearmed.
    #[test]
    fn a_partially_accepted_exhausted_tail_is_accounted_on_recovery() {
        let mut sink = MemorySink::new(Spec::fixed(), 12, 4);
        sink.limit_writes_to(Some(1));
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(8).unwrap();
        mixer.set_prebuffer(id, 4, true).unwrap();
        mixer.write(id, &stereo(&[100, 200, 300, 400])).unwrap();

        let first = mixer.pump(&mut sink).unwrap();
        assert_eq!(first.frames_written, 1);
        assert_eq!(first.frames_pending, 3);
        assert_eq!(mixer.underflows(id).unwrap(), 0);
        assert_eq!(mixer.find(id).unwrap().underflow_candidates.len(), 1);

        mixer.recover(&mut sink).unwrap();
        assert_eq!(mixer.underflows(id).unwrap(), 1);
        sink.limit_writes_to(None);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 3);

        mixer.write(id, &stereo(&[500, 600, 700])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        mixer.write(id, &stereo(&[800])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
    }

    /// A peer-only interval makes the refill a new run even when the device
    /// has not yet reached the older endpoint. The new candidate must not
    /// replace the earlier gap whose evidence is still in the ring.
    #[test]
    fn a_peer_gap_retains_the_older_underflow_candidate() {
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let resumed = mixer.open(16).unwrap();
        let peer = mixer.open(16).unwrap();
        mixer.set_prebuffer(resumed, 0, false).unwrap();
        mixer.set_prebuffer(peer, 0, false).unwrap();
        mixer.write(resumed, &stereo(&[10; 4])).unwrap();
        mixer.write(peer, &stereo(&[20; 8])).unwrap();

        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        mixer.write(resumed, &stereo(&[30; 4])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        let candidates = &mixer.find(resumed).unwrap().underflow_candidates;
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates.front().map(|candidate| candidate.run_start),
            Some(0)
        );
        assert_eq!(
            candidates.back().map(|candidate| candidate.run_start),
            Some(8)
        );

        sink.start().unwrap();
        mixer.note_started();
        sink.advance(4);
        mixer.observe_playhead(&mut sink).unwrap();
        assert_eq!(mixer.underflows(resumed).unwrap(), 1);
        assert_eq!(mixer.find(resumed).unwrap().underflow_candidates.len(), 1);
    }

    #[test]
    fn a_contiguous_refill_extends_one_underflow_candidate() {
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        let id = mixer.open(16).unwrap();
        mixer.set_prebuffer(id, 0, false).unwrap();
        mixer.write(id, &stereo(&[10; 4])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        mixer.write(id, &stereo(&[20; 4])).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);

        let candidates = &mixer.find(id).unwrap().underflow_candidates;
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates.front().map(|candidate| candidate.run_start),
            Some(0)
        );
        assert_eq!(
            candidates.front().map(|candidate| candidate.run_end),
            Some(8)
        );
        assert_eq!(
            candidates.front().map(|candidate| candidate.accepted_end),
            Some(8)
        );
    }

    /// The server observes the playhead before commands from the same wake.
    /// A crossing while corked is consumed without blame; a later CORK cannot
    /// erase a crossing already observed while the stream was active.
    #[test]
    fn playhead_observation_orders_cork_against_an_endpoint() {
        let setup = || {
            let mut sink = MemorySink::new(Spec::fixed(), 8, 4);
            let mut mixer = Mixer::new(Spec::fixed());
            let id = mixer.open(8).unwrap();
            mixer.set_prebuffer(id, 4, true).unwrap();
            mixer.write(id, &stereo(&[10; 4])).unwrap();
            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
            sink.start().unwrap();
            mixer.note_started();
            (mixer, sink, id)
        };

        let (mut corked, mut corked_sink, corked_id) = setup();
        corked.set_corked(corked_id, true).unwrap();
        corked_sink.advance(4);
        corked.observe_playhead(&mut corked_sink).unwrap();
        corked.set_corked(corked_id, false).unwrap();
        assert_eq!(corked.underflows(corked_id).unwrap(), 0);
        assert!(!corked.find(corked_id).unwrap().prebuffering);

        let (mut active, mut active_sink, active_id) = setup();
        active_sink.advance(4);
        active.observe_playhead(&mut active_sink).unwrap();
        active.set_corked(active_id, true).unwrap();
        assert_eq!(active.underflows(active_id).unwrap(), 1);
        assert!(active.find(active_id).unwrap().prebuffering);
    }

    #[test]
    fn one_stream_reaches_the_device_unchanged() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 1000).unwrap();
        let audio = stereo(&[100, -200, 300, -400]);
        assert_eq!(mixer.write(StreamId(1), &audio).unwrap(), audio.len());
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        assert_eq!(
            sink.samples(),
            vec![100, 100, -200, -200, 300, 300, -400, -400]
        );
    }

    /// Two streams sum. This is the assertion §K.5 asks for by name: the answer
    /// is written down in advance and the mixer has to produce it.
    #[test]
    fn two_streams_are_summed_sample_by_sample() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 1000).unwrap();
        mixer.create(StreamId(2), 1000).unwrap();
        mixer
            .write(StreamId(1), &stereo(&[1000, 2000, 3000, 4000]))
            .unwrap();
        mixer
            .write(StreamId(2), &stereo(&[10, 20, 30, 40]))
            .unwrap();
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        let played = sink.samples();
        assert_eq!(played.first().copied(), Some(1010));
        assert_eq!(played.get(2).copied(), Some(2020));
        assert_eq!(played.get(4).copied(), Some(3030));
        assert_eq!(played.get(6).copied(), Some(4040));
    }

    /// Unequal streams select the longest real contribution, capped by one
    /// period. A shorter stream is silent only after its own tail ends.
    #[test]
    fn unequal_stream_tails_advance_by_the_longest_real_contribution() {
        let mut short_sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut short_mixer = Mixer::new(Spec::fixed());
        let two = short_mixer.open(8).unwrap();
        let three = short_mixer.open(8).unwrap();
        short_mixer.write(two, &stereo(&[10, 20])).unwrap();
        short_mixer.write(three, &stereo(&[100, 200, 300])).unwrap();
        assert_eq!(short_mixer.pump(&mut short_sink).unwrap().frames_written, 3);
        assert_eq!(short_sink.samples(), vec![110, 110, 220, 220, 300, 300]);

        let mut full_sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut full_mixer = Mixer::new(Spec::fixed());
        let short = full_mixer.open(8).unwrap();
        let full = full_mixer.open(8).unwrap();
        full_mixer.write(short, &stereo(&[10, 20])).unwrap();
        full_mixer
            .write(full, &stereo(&[100, 200, 300, 400]))
            .unwrap();
        assert_eq!(full_mixer.pump(&mut full_sink).unwrap().frames_written, 4);
        assert_eq!(
            full_sink.samples(),
            vec![110, 110, 220, 220, 300, 300, 400, 400]
        );
    }

    /// A sum past full scale clips instead of wrapping. Wrapping is the loud
    /// click that makes a mixer sound broken; clipping is merely loud.
    #[test]
    fn a_loud_sum_saturates_rather_than_wrapping() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 2);
        let mut mixer = Mixer::new(Spec::fixed());
        for id in [1, 2, 3] {
            mixer.create(StreamId(id), 1000).unwrap();
            mixer
                .write(StreamId(id), &stereo(&[30000, -30000]))
                .unwrap();
        }
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        let played = sink.samples();
        assert_eq!(played.first().copied(), Some(i16::MAX));
        assert_eq!(played.get(2).copied(), Some(i16::MIN));
    }

    #[test]
    fn volume_is_multiplication_in_the_mixer() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 2);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 1000).unwrap();
        mixer.create(StreamId(2), 1000).unwrap();
        mixer.set_volume(StreamId(1), VOLUME_NORM / 2).unwrap();
        mixer.set_volume(StreamId(2), 0).unwrap();
        mixer.write(StreamId(1), &stereo(&[1000, 1000])).unwrap();
        mixer.write(StreamId(2), &stereo(&[5000, 5000])).unwrap();
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        assert_eq!(
            sink.samples().first().copied(),
            Some(500),
            "half of one stream, and none of a stream at zero"
        );
        // Volume above unity is refused rather than applied.
        mixer.set_volume(StreamId(1), VOLUME_NORM * 4).unwrap();
        assert_eq!(mixer.find(StreamId(1)).unwrap().volume, VOLUME_NORM);
    }

    /// Backpressure: a full stream queue shortens the client's write and counts
    /// an overflow, and the grant is what the client should have asked for.
    #[test]
    fn a_full_stream_queue_short_writes_and_grants_what_is_left() {
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 4).unwrap();
        assert_eq!(mixer.request_frames(StreamId(1)).unwrap(), 4);
        let audio = stereo(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(mixer.write(StreamId(1), &audio).unwrap(), 4 * 4);
        assert_eq!(mixer.request_frames(StreamId(1)).unwrap(), 0);
        assert_eq!(mixer.overflows(StreamId(1)).unwrap(), 1);
        assert_eq!(mixer.write(StreamId(1), &audio).unwrap(), 0);
    }

    /// A short write from the device is held, not dropped: the next pump
    /// delivers exactly the frames the sink refused, in order.
    #[test]
    fn a_short_device_write_is_retried_rather_than_dropped() {
        // Room for 3 frames, a 4-frame period: the sink can never take a whole
        // period at once.
        let mut sink = MemorySink::new(Spec::fixed(), 3, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 1000).unwrap();
        mixer
            .write(StreamId(1), &stereo(&[11, 22, 33, 44]))
            .unwrap();
        sink.start().unwrap();
        let first = mixer.pump(&mut sink).unwrap();
        assert_eq!(first.frames_written, 3);
        assert_eq!(first.frames_pending, 1);
        sink.advance(3);
        let second = mixer.pump(&mut sink).unwrap();
        assert_eq!(second.frames_written, 1);
        assert_eq!(second.frames_pending, 0);
        // Every frame, once, in order.
        let played: Vec<i16> = sink
            .samples()
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|frame| frame.first().copied())
            .collect();
        assert_eq!(played, vec![11, 22, 33, 44]);
    }

    /// §K.3's first trap: the reported latency must not count a frame twice.
    #[test]
    fn latency_counts_each_frame_exactly_once() {
        let mut sink = MemorySink::new(Spec::fixed(), 4800, 480);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        // One second of audio queued, nothing mixed yet.
        mixer.write(StreamId(1), &vec![0u8; 48000 * 4]).unwrap();
        let before = mixer.timing(StreamId(1)).unwrap();
        assert_eq!(before.queued_frames, 48000);
        assert_eq!(before.output_backlog_frames, 0);
        assert_eq!(before.latency_usec, 1_000_000);

        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        let after = mixer.timing(StreamId(1)).unwrap();
        // 480 frames moved from the stream queue into the output path, all of
        // them still inside the device. The SUM is unchanged: adding the accept
        // counter to the queue instead would report 1.01 seconds here, and
        // reading the device delay BEFORE the transfer would report 0.99.
        assert_eq!(after.queued_frames, 47_520);
        assert_eq!(after.output_backlog_frames, 480);
        assert_eq!(after.device_delay_frames, 480);
        assert_eq!(after.latency_usec, 1_000_000);

        // The device plays half a period; the sum drops by exactly that, and by
        // nothing else, however many periods have been handed over since.
        sink.advance(240);
        mixer.pump(&mut sink).unwrap();
        let played = mixer.timing(StreamId(1)).unwrap();
        assert_eq!(played.device_delay_frames, 720);
        assert_eq!(
            played.queued_frames + played.output_backlog_frames,
            48000 - 240
        );
        assert_eq!(played.latency_usec, 995_000);
    }

    /// A timing reply is a consistent SET: the read index never overtakes the
    /// write index, and both advance with the sound.
    #[test]
    fn the_indexes_stay_consistent_with_each_other() {
        let mut sink = MemorySink::new(Spec::fixed(), 480, 240);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 2400 * 4]).unwrap();
        sink.start().unwrap();
        for _ in 0..6 {
            mixer.pump(&mut sink).unwrap();
            sink.advance(120);
            let timing = mixer.timing(StreamId(1)).unwrap();
            assert!(
                timing.read_index <= timing.write_index,
                "read {} overtook write {}",
                timing.read_index,
                timing.write_index
            );
            assert_eq!(timing.write_index % 4, 0);
        }
        assert!(mixer.timing(StreamId(1)).unwrap().read_index > 0);
    }

    /// §K.3's third trap: draining one stream is bookkeeping, and must not stop
    /// the shared device or silence anybody else.
    #[test]
    fn a_stream_drains_without_touching_the_device() {
        let mut sink = MemorySink::new(Spec::fixed(), 480, 240);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.create(StreamId(2), 48000).unwrap();
        // Stream 1 has a little; stream 2 has plenty.
        mixer.write(StreamId(1), &vec![0u8; 240 * 4]).unwrap();
        mixer.write(StreamId(2), &vec![0u8; 4800 * 4]).unwrap();
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        assert!(
            !mixer.is_drained(StreamId(1)).unwrap(),
            "still in the device"
        );
        // Once the device has played those frames, and only then.
        sink.advance(240);
        mixer.pump(&mut sink).unwrap();
        assert!(mixer.is_drained(StreamId(1)).unwrap());
        assert!(!mixer.is_drained(StreamId(2)).unwrap());
        // The device was never stopped, and stream 2 is still playing.
        assert!(
            sink.is_running(),
            "a per-stream drain must not stop the PCM"
        );
    }

    /// Crossing one exhausted accepted endpoint rearms prebuffering and
    /// reports exactly one underflow, rather than treating every ordinary
    /// queue refill as one.
    #[test]
    fn a_starved_stream_underflows_once() {
        let mut sink = MemorySink::new(Spec::fixed(), 4800, 240);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.set_prebuffer(StreamId(1), 100, true).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 100 * 4]).unwrap();
        let first = mixer.pump(&mut sink).unwrap();
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(first.frames_written.saturating_add(1));
        mixer.recover(&mut sink).unwrap();
        assert_eq!(
            mixer.underflows(StreamId(1)).unwrap(),
            1,
            "the device consumed the whole run and then underran"
        );
        mixer.write(StreamId(1), &[0u8; 10 * 4]).unwrap();
        mixer.pump(&mut sink).unwrap();
        assert_eq!(
            mixer.underflows(StreamId(1)).unwrap(),
            1,
            "refilling after the recorded XRUN must not count it twice"
        );
        assert_eq!(
            mixer.pump(&mut sink).unwrap().frames_written,
            0,
            "an XRUN must rearm the negotiated prebuffer"
        );
        mixer.write(StreamId(1), &[0u8; 90 * 4]).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 100);
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(101);
        mixer.recover(&mut sink).unwrap();
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 2);
        // More idle pumps after the one recovery do not create more events.
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 2);
    }

    #[test]
    fn contiguous_period_replenishment_is_not_an_underflow() {
        let mut sink = MemorySink::new(Spec::fixed(), 4800, 240);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 240 * 4]).unwrap();
        mixer.pump(&mut sink).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 240 * 4]).unwrap();
        mixer.pump(&mut sink).unwrap();
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 0);
        assert!(mixer.is_running(StreamId(1)).unwrap());
    }

    /// A stream's first write is not an underflow. A new stream starts marked
    /// drained — it has no audio — and counting that would report every client
    /// as having fallen behind before it ever played a frame.
    #[test]
    fn a_streams_first_write_is_not_an_underflow() {
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 240 * 4]).unwrap();
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 0);
        // Nor is a second write that arrives before the queue ever emptied.
        mixer.write(StreamId(1), &vec![0u8; 240 * 4]).unwrap();
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 0);
    }

    /// One stream's audio in flight must not move another stream's position.
    ///
    /// Stream 1 finishes and is played out; stream 2 keeps the device busy.
    /// Deriving stream 1's played count from the GLOBAL backlog charges stream
    /// 2's buffer to it, so its `read_index` falls back to zero and it reports
    /// latency for audio that finished long ago — while `is_drained` says it is
    /// done. The two answers must agree.
    #[test]
    fn a_finished_streams_position_is_not_moved_by_another_streams_backlog() {
        let mut sink = MemorySink::new(Spec::fixed(), 4800, 240);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.create(StreamId(2), 48000).unwrap();
        // Stream 1 plays a little; stream 2 has plenty and keeps going.
        mixer.write(StreamId(1), &vec![0u8; 240 * 4]).unwrap();
        mixer.write(StreamId(2), &vec![0u8; 4800 * 4]).unwrap();
        sink.start().unwrap();
        for _ in 0..6 {
            let pumped = mixer.pump(&mut sink).unwrap();
            sink.advance(pumped.frames_written);
        }
        assert!(
            mixer.is_drained(StreamId(1)).unwrap(),
            "stream 1 has played out"
        );
        let one = mixer.timing(StreamId(1)).unwrap();
        assert_eq!(
            one.read_index, one.write_index,
            "a drained stream has heard everything it wrote"
        );
        assert_eq!(
            one.latency_usec, 0,
            "a drained stream has no latency, whatever else is in the device"
        );
        // And stream 2, which really does have audio in flight, still reports
        // it.
        assert!(!mixer.is_drained(StreamId(2)).unwrap());
        let two = mixer.timing(StreamId(2)).unwrap();
        assert!(two.read_index < two.write_index);
        assert!(two.latency_usec > 0);
    }

    /// Real audio through the whole path: a generated tone, mixed, played, and
    /// compared frame by frame with the closed form.
    #[test]
    fn a_generated_tone_survives_the_mixer_bit_for_bit() {
        let spec = Spec::fixed();
        let tone = Tone::fixture();
        let mut generator = Generator::new(spec, tone);
        let mut audio = Vec::new();
        generator.fill(&mut audio, 4800);

        let mut sink = MemorySink::new(spec, 9600, 480);
        let mut mixer = Mixer::new(spec);
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &audio).unwrap();
        sink.start().unwrap();
        for _ in 0..12 {
            mixer.pump(&mut sink).unwrap();
            sink.advance(480);
        }
        let played = sink.samples();
        assert!(played.len() >= 4800 * 2);
        for frame in 0..4800u64 {
            let expected = Generator::sample_at(spec, tone, frame);
            let got = played.get(frame as usize * 2).copied().unwrap();
            assert_eq!(got, expected, "frame {frame}");
        }
    }

    #[test]
    fn drain_all_stops_when_everything_has_played() {
        let spec = Spec::fixed();
        let mut sink = MemorySink::new(spec, 4800, 240);
        let mut mixer = Mixer::new(spec);
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &[7u8; 480 * 4]).unwrap();
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        mixer.pump(&mut sink).unwrap();
        sink.advance(480);
        let passes = mixer.drain_all(&mut sink, 100).unwrap();
        assert!(passes < 100, "drained in {passes} passes");
        assert!(mixer.is_drained(StreamId(1)).unwrap());
    }

    #[test]
    fn drain_all_releases_prebuffer_and_primes_a_stopped_device() {
        let spec = Spec::fixed();
        let mut short_sink = MemorySink::new(spec, 12, 4);
        let mut short_mixer = Mixer::new(spec);
        let short = short_mixer.open(16).unwrap();
        short_mixer.set_prebuffer(short, 8, true).unwrap();
        short_mixer.write(short, &stereo(&[700, 700, 700])).unwrap();
        assert_eq!(short_mixer.drain_all(&mut short_sink, 1).unwrap(), 1);
        assert_eq!(short_sink.frames_written(), 3);
        assert!(short_sink.is_running(), "the released finite tail starts");

        let mut sink = MemorySink::new(spec, 12, 4);
        let mut mixer = Mixer::new(spec);
        let id = mixer.open(32).unwrap();
        mixer.write(id, &stereo(&[500; 16])).unwrap();
        assert_eq!(mixer.drain_all(&mut sink, 1).unwrap(), 1);
        assert!(
            !sink.is_running(),
            "one period cannot start a continuous tail"
        );
        assert_eq!(mixer.drain_all(&mut sink, 1).unwrap(), 1);
        assert!(
            !sink.is_running(),
            "two periods cannot start a three-period ring"
        );
        assert_eq!(mixer.drain_all(&mut sink, 1).unwrap(), 1);
        assert!(
            sink.is_running(),
            "a fully primed ring starts during shutdown"
        );
    }

    #[test]
    fn drain_all_reprimes_after_recovery_before_starting() {
        let spec = Spec::fixed();
        let mut sink = MemorySink::new(spec, 12, 4);
        sink.write(&stereo(&[1])).unwrap();
        sink.start().unwrap();
        sink.advance(2);

        let mut mixer = Mixer::new(spec);
        let id = mixer.open(32).unwrap();
        mixer.write(id, &stereo(&[500; 16])).unwrap();
        assert_eq!(mixer.drain_all(&mut sink, 2).unwrap(), 2);
        assert!(
            !sink.is_running(),
            "the recovery pass plus one period cannot start the empty ring"
        );
    }

    /// The bound is real: a device that never consumes does not hang the daemon.
    #[test]
    fn drain_all_gives_up_rather_than_looping_forever() {
        let spec = Spec::fixed();
        let mut sink = MemorySink::new(spec, 480, 240);
        let mut mixer = Mixer::new(spec);
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 48000 * 4]).unwrap();
        // Started, but the clock never advances: nothing is ever played.
        sink.start().unwrap();
        assert_eq!(mixer.drain_all(&mut sink, 8).unwrap(), 8);
    }

    #[test]
    fn a_device_that_disappears_while_draining_is_an_error() {
        let spec = Spec::fixed();
        let mut sink = MemorySink::new(spec, 480, 240);
        let mut mixer = Mixer::new(spec);
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 4800 * 4]).unwrap();
        sink.start().unwrap();
        sink.unplug();
        let err = mixer.drain_all(&mut sink, 8).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn an_unknown_stream_is_an_error_rather_than_a_silent_no_op() {
        let mut mixer = Mixer::new(Spec::fixed());
        assert!(mixer.write(StreamId(9), &pcm(&[0])).is_err());
        assert!(mixer.timing(StreamId(9)).is_err());
        assert!(mixer.set_volume(StreamId(9), 0).is_err());
        assert!(mixer.is_drained(StreamId(9)).is_err());
        mixer.create(StreamId(9), 100).unwrap();
        assert!(mixer.create(StreamId(9), 100).is_err(), "no duplicate ids");
        mixer.remove(StreamId(9));
        assert_eq!(mixer.stream_count(), 0);
    }
}
