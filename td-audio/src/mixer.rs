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
///
/// The budget is NOT divided between peers, so one of them can occupy it — with
/// corked streams, which keep their queues and are never drained — and short-
/// write the rest until it resumes, which they hear as gaps. Peers here CAN
/// distrust each other, and `MAX_PENDING` is bounded per client for exactly
/// that reason. What differs is the stake: `pending` is the daemon's own
/// memory, so exhausting it ends the service for everyone and does not come
/// back, where this is a playback backlog whose cost is interrupted audio among
/// one seat's own applications, returned by an uncork, a disconnect or a kill.
/// A per-client share is the answer if that is ever judged too weak.
pub const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

/// The largest period this mixer will size a pass from.
///
/// One second at 192 kHz, which is far past anything a card asks for and still
/// a number rather than whatever the kernel said. The accumulator is sized from
/// it, and an allocation that fails aborts instead of returning an error.
const MAX_PERIOD_FRAMES: usize = 192_000;

/// A stream's handle. Opaque so that the Pulse channel number, which is a
/// protocol detail, never becomes the mixer's identity for it.
///
/// The field is private and `Mixer::open` is the only way to obtain one, which
/// is what makes that sentence true. It was not: sessions numbered their Pulse
/// channels from zero independently of each other and handed those numbers
/// straight to the mixer, so the second client to connect collided with the
/// first on channel 0 and was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u64);

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
    /// `frames_mixed` as it stood when the stream's CURRENT run of contiguous
    /// output began.
    ///
    /// `out_end` anchors only the end of that run. Everything mixed before it
    /// belongs to an earlier run that the device has long since played, so
    /// without this the gap gets charged to the stream as unplayed audio and
    /// `read_index` walks backwards the moment a paused or starved stream
    /// resumes.
    segment_base: u64,
    /// It ran dry while the mixer wanted frames from it.
    underflows: u32,
    /// A write was refused because the queue was full.
    overflows: u32,
}

impl Stream {
    fn queued_frames(&self, frame_bytes: usize) -> u64 {
        (self.queue.len() / frame_bytes.max(1)) as u64
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

/// Every stream, summed into one device.
pub struct Mixer {
    spec: Spec,
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
    /// The next id `open` will issue. Monotonic and never reused, so a stream
    /// that has been removed cannot be confused with a later one.
    next_id: u64,
}

impl Mixer {
    pub fn new(spec: Spec) -> Self {
        Self {
            spec,
            streams: Vec::new(),
            accumulator: Vec::new(),
            pending: Vec::new(),
            pending_at: 0,
            out_frames_mixed: 0,
            out_frames_written: 0,
            device_delay: 0,
            next_id: 0,
        }
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Admit a stream and name it, with a queue limit in frames.
    ///
    /// The id is issued here rather than chosen by the caller. A caller that
    /// picks its own has to know what every other caller picked, and the
    /// sessions cannot: each numbers its Pulse channels from zero.
    pub fn open(&mut self, limit_frames: u64) -> io::Result<StreamId> {
        let id = StreamId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
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
            volume: VOLUME_NORM,
            frames_accepted: 0,
            frames_mixed: 0,
            drain_mark: Some(0),
            out_end: 0,
            corked: false,
            segment_base: 0,
            underflows: 0,
            overflows: 0,
        });
        Ok(())
    }

    pub fn remove(&mut self, id: StreamId) {
        self.streams.retain(|s| s.id != id);
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
            // Audio arriving for a stream the mixer had already run out of is a
            // GAP: silence went to the device between this stream's frames, and
            // that is what a Pulse `UNDERFLOW` reports. Counting it here rather
            // than at the empty mix is what makes the count reachable at all —
            // a stream whose queue empties every period is marked drained by
            // that same mix, so the empty-mix path can never tell "the client
            // finished" from "the client fell behind". Only the client writing
            // again distinguishes them, and only afterwards.
            if stream.drain_mark.is_some() && stream.frames_mixed > 0 {
                stream.underflows = stream.underflows.saturating_add(1);
            }
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
        // Bounded by the CURRENT run, not by everything ever mixed: frames
        // from an earlier run ended at an earlier `out_end` the device has
        // already passed, and charging them here drives `read_index` backwards
        // the moment a stream resumes after a pause or a starve.
        let segment_frames = stream.frames_mixed.saturating_sub(stream.segment_base);
        let unplayed_share = stream
            .out_end
            .saturating_sub(self.frames_played())
            .min(segment_frames);
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
        self.streams.retain(|stream| live.contains(&stream.id));
    }

    /// How many times this stream ran dry with the client still writing.
    pub fn underflows(&self, id: StreamId) -> io::Result<u32> {
        Ok(self.find(id).ok_or_else(|| Self::missing(id))?.underflows)
    }

    /// How many client writes were shortened because the queue was full.
    pub fn overflows(&self, id: StreamId) -> io::Result<u32> {
        Ok(self.find(id).ok_or_else(|| Self::missing(id))?.overflows)
    }

    /// Mix one period and give it to the device.
    ///
    /// Flushes what the sink would not take last time BEFORE mixing anything
    /// new: a short write is backpressure, and audio already mixed must reach
    /// the device in order or there is a gap in every stream at once.
    pub fn pump<S: AudioSink>(&mut self, sink: &mut S) -> io::Result<Pumped> {
        let mut pumped = Pumped::default();
        if self.pending_at >= self.pending.len() {
            let frames = usize::try_from(sink.period_frames()).unwrap_or(0);
            if frames > 0 {
                self.mix(frames)?;
            }
        }
        pumped.frames_written = self.push_pending(sink)?;
        pumped.frames_pending = self.pending_frames();
        // AFTER the transfer, not before. The delay is how much the device
        // still holds, and frames handed over in this very pump are part of
        // that — reading it first would make `frames_played` count them as
        // already heard, which reports a stream drained while its last period
        // is still in the ring and makes every latency figure short by one
        // transfer.
        // A DELAY that fails answers `EPIPE` in an XRUN and `EBADFD` after a
        // DROP, and in both the device holds nothing: keeping the previous
        // value would freeze `frames_played()` at a position the device has
        // left, so the honest fallback is zero rather than the stale figure.
        self.device_delay = sink.device_delay().unwrap_or(0);
        Ok(pumped)
    }

    fn pending_frames(&self) -> u64 {
        ((self.pending.len().saturating_sub(self.pending_at)) / self.spec.frame_bytes.max(1)) as u64
    }

    /// Sum every stream into `pending`.
    fn mix(&mut self, frames: usize) -> io::Result<()> {
        let channels = self.spec.channels.max(1) as usize;
        let frame_bytes = self.spec.frame_bytes.max(1);
        // The frame count comes from the device's own readback. A readback this
        // large is a card this daemon cannot drive, and `resize` on an absurd
        // one aborts the process rather than returning — so it is refused with
        // a diagnostic instead.
        if frames > MAX_PERIOD_FRAMES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the device reported a {frames}-frame period"),
            ));
        }
        let samples = frames.saturating_mul(channels);
        self.accumulator.clear();
        self.accumulator.resize(samples, 0.0);

        let out_base = self.out_frames_mixed;
        for stream in &mut self.streams {
            if stream.corked {
                // Paused: contributes nothing, and is not starving. Its
                // `out_end` stays where it is, so resuming is a gap and the
                // segment bookkeeping below treats it as one.
                continue;
            }
            let available = (stream.queue.len() / frame_bytes).min(frames);
            if available == 0 {
                // A stream with nothing left is only underflowing if it had not
                // already been marked drained: a client that finished playing
                // and is waiting is not starving.
                if stream.drain_mark.is_none() {
                    // Marking, not counting. `write` owns the underflow count,
                    // and its comment says why: from here a stream that
                    // finished and a stream that fell behind look identical,
                    // because this same pass is what marks it drained. Only the
                    // client writing again tells them apart.
                    stream.drain_mark = Some(out_base);
                    stream.out_end = out_base;
                }
                continue;
            }
            if stream.out_end != out_base {
                // A gap: this stream contributed nothing at the output position
                // its last run ended at, so that run is over and the device has
                // played it. Everything counted so far is played; the new run
                // starts here.
                stream.segment_base = stream.frames_mixed;
            }
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
            if stream.queue.len() < frame_bytes {
                // Its audio ends here, at this output position.
                stream.drain_mark = Some(out_base.saturating_add(available as u64));
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
            self.pending.extend_from_slice(&(clamped as i16).to_le_bytes());
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
        let accepted = sink.write(tail)?;
        self.pending_at = self
            .pending_at
            .saturating_add(accepted.saturating_mul(frame_bytes));
        self.out_frames_written = self.out_frames_written.saturating_add(accepted as u64);
        Ok(accepted as u64)
    }

    /// Recover the device after an underrun and re-prime it.
    ///
    /// The device's own frame counters restart, so the mixer's output axis is
    /// rebased: `out_frames_written` is the position the device is at, and
    /// everything mixed but unaccepted is still ahead of it.
    pub fn recover<S: AudioSink>(&mut self, sink: &mut S) -> io::Result<()> {
        sink.recover()?;
        self.device_delay = 0;
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
        Ok(())
    }

    /// Run the device until every stream has drained, or `max_passes` passes
    /// have gone by. Returns the passes used.
    ///
    /// The bound is not decoration: a device that stops accepting frames would
    /// otherwise make this loop forever, and a daemon that hangs on shutdown is
    /// worse than one that reports it could not drain.
    pub fn drain_all<S: AudioSink>(&mut self, sink: &mut S, max_passes: u32) -> io::Result<u32> {
        for pass in 0..max_passes {
            let all_drained = self
                .streams
                .iter()
                .all(|s| s.drain_mark.is_some_and(|mark| self.frames_played() >= mark));
            if all_drained && self.pending_at >= self.pending.len() {
                return Ok(pass);
            }
            match sink.wait(100)? {
                Wait::Gone => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the device went away while draining",
                    ))
                }
                Wait::Underrun => self.recover(sink)?,
                Wait::Writable | Wait::Timeout => {
                    // The transfer-side underrun, not just the poll-side one.
                    // `poll` saying writable and the ring emptying before the
                    // write lands is an ordinary race on a busy machine, and
                    // treating it as fatal ends a shutdown that was working.
                    let pumped = match self.pump(sink) {
                        Ok(pumped) => pumped,
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
                    if pumped.frames_written > 0 && !sink.is_running() {
                        sink.start()?;
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

    fn stereo(frames: &[i16]) -> Vec<u8> {
        frames.iter().flat_map(|s| [*s, *s]).flat_map(|s| s.to_le_bytes()).collect()
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
        // And an id is never handed out twice, so a removed stream cannot be
        // confused with a later one that reused its number.
        mixer.remove(first);
        let third = mixer.open(1000).unwrap();
        assert_ne!(third, first);
        assert_ne!(third, second);
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
        mixer.pump(&mut sink).unwrap();
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

    /// No set of streams can queue more than the mixer's whole-mixer budget,
    /// however generous each one's own ceiling is.
    ///
    /// The per-stream ceiling, the per-client stream cap and the client cap are
    /// each bounded; their PRODUCT is four gibibytes, and a client reaches its
    /// share simply by writing past the byte grants it was sent. The budget is
    /// the only bound that sees all the queues at once, so it is the only place
    /// that product can be capped.
    #[test]
    fn the_queues_of_all_streams_together_are_bounded() {
        let mut mixer = Mixer::new(Spec::fixed());
        // Ceilings far past the budget, so nothing but the budget can stop this.
        let frame_bytes = Spec::fixed().frame_bytes as u64;
        let roomy = MAX_QUEUED_BYTES as u64 / frame_bytes;
        let ids: Vec<StreamId> = (0..8).map(|_| mixer.open(roomy).unwrap()).collect();

        // One buffer, offered many times: this is 128 MiB of writes against a
        // 64 MiB budget, without allocating 128 MiB to make them.
        let chunk = vec![0u8; 2 * 1024 * 1024];
        let mut accepted = 0usize;
        for _ in 0..8 {
            for id in &ids {
                accepted = accepted.saturating_add(mixer.write(*id, &chunk).unwrap());
            }
        }
        assert!(
            accepted <= MAX_QUEUED_BYTES,
            "the mixer accepted {accepted} bytes against a budget of {MAX_QUEUED_BYTES}"
        );
        assert!(
            accepted >= MAX_QUEUED_BYTES.saturating_sub(chunk.len()),
            "and it is a budget rather than a smaller accident: {accepted} bytes"
        );

        // A short write is the answer a client already understands: it is the
        // same one its own ceiling gives, so nothing new has to handle it.
        assert_eq!(
            mixer.write(ids[0], &chunk).unwrap(),
            0,
            "a stream with room of its own is still refused once the mixer is full"
        );

        // And the budget is a backlog, not a latch: playing frames out returns
        // room to everyone.
        let mut sink = MemorySink::new(Spec::fixed(), 8192, 256);
        sink.start().unwrap();
        let pumped = mixer.pump(&mut sink).unwrap();
        assert!(pumped.frames_written > 0);
        sink.advance(pumped.frames_written);
        assert!(
            mixer.write(ids[0], &chunk).unwrap() > 0,
            "room freed by the device is room the next write can use"
        );
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

    #[test]
    fn one_stream_reaches_the_device_unchanged() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 1000).unwrap();
        let audio = stereo(&[100, -200, 300, -400]);
        assert_eq!(mixer.write(StreamId(1), &audio).unwrap(), audio.len());
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        assert_eq!(sink.samples(), vec![100, 100, -200, -200, 300, 300, -400, -400]);
    }

    /// Two streams sum. This is the assertion §K.5 asks for by name: the answer
    /// is written down in advance and the mixer has to produce it.
    #[test]
    fn two_streams_are_summed_sample_by_sample() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 1000).unwrap();
        mixer.create(StreamId(2), 1000).unwrap();
        mixer.write(StreamId(1), &stereo(&[1000, 2000, 3000, 4000])).unwrap();
        mixer.write(StreamId(2), &stereo(&[10, 20, 30, 40])).unwrap();
        sink.start().unwrap();
        mixer.pump(&mut sink).unwrap();
        let played = sink.samples();
        assert_eq!(played.first().copied(), Some(1010));
        assert_eq!(played.get(2).copied(), Some(2020));
        assert_eq!(played.get(4).copied(), Some(3030));
        assert_eq!(played.get(6).copied(), Some(4040));
    }

    /// A sum past full scale clips instead of wrapping. Wrapping is the loud
    /// click that makes a mixer sound broken; clipping is merely loud.
    #[test]
    fn a_loud_sum_saturates_rather_than_wrapping() {
        let mut sink = MemorySink::new(Spec::fixed(), 64, 2);
        let mut mixer = Mixer::new(Spec::fixed());
        for id in [1, 2, 3] {
            mixer.create(StreamId(id), 1000).unwrap();
            mixer.write(StreamId(id), &stereo(&[30000, -30000])).unwrap();
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
        mixer.write(StreamId(1), &stereo(&[11, 22, 33, 44])).unwrap();
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
        assert!(!mixer.is_drained(StreamId(1)).unwrap(), "still in the device");
        // Once the device has played those frames, and only then.
        sink.advance(240);
        mixer.pump(&mut sink).unwrap();
        assert!(mixer.is_drained(StreamId(1)).unwrap());
        assert!(!mixer.is_drained(StreamId(2)).unwrap());
        // The device was never stopped, and stream 2 is still playing.
        assert!(sink.is_running(), "a per-stream drain must not stop the PCM");
    }

    /// A stream that runs dry underflows exactly once, not once
    /// per pump forever.
    #[test]
    fn a_starved_stream_underflows_once() {
        let mut sink = MemorySink::new(Spec::fixed(), 4800, 240);
        let mut mixer = Mixer::new(Spec::fixed());
        mixer.create(StreamId(1), 48000).unwrap();
        mixer.write(StreamId(1), &vec![0u8; 100 * 4]).unwrap();
        sink.start().unwrap();
        for _ in 0..5 {
            mixer.pump(&mut sink).unwrap();
            sink.advance(240);
        }
        assert_eq!(
            mixer.underflows(StreamId(1)).unwrap(),
            0,
            "a stream that finished is not starving"
        );
        // The client writing AGAIN is what turns that quiet into an underflow:
        // silence went to the device between two runs of this stream's audio,
        // and that gap is what a Pulse UNDERFLOW reports. Nothing at the empty
        // pump can tell the two apart — a stream that has finished and a stream
        // whose client is late look identical until the client comes back.
        mixer.write(StreamId(1), &[0u8; 10 * 4]).unwrap();
        assert_eq!(
            mixer.underflows(StreamId(1)).unwrap(),
            1,
            "the client fell behind and the mixer had to fill silence"
        );
        // Playing that out and letting it run dry again is not a second
        // underflow until the client writes again.
        for _ in 0..3 {
            mixer.pump(&mut sink).unwrap();
            sink.advance(240);
        }
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 1);
        mixer.write(StreamId(1), &[0u8; 10 * 4]).unwrap();
        assert_eq!(mixer.underflows(StreamId(1)).unwrap(), 2);
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
        assert!(mixer.is_drained(StreamId(1)).unwrap(), "stream 1 has played out");
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
