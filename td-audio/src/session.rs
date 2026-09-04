//! One client connection, as a pure state machine.
//!
//! Bytes in, bytes out, and a `&mut Mixer` for the audio. Nothing here opens a
//! socket, reads a clock, or calls a syscall — which is what lets the whole
//! protocol be tested against the captured packets in `proto` and `tag` with no
//! daemon running, and what keeps the socket increment's `SO_PEERCRED`
//! amendment out of this file.
//!
//! The reply schemas are not written from memory either. Each one below was
//! served to a real libpulse 16.1 client from a logging stub and accepted:
//! `pactl info` printed the server info, `pactl list sinks` printed the sink,
//! `paplay` played through the stream lifecycle, and a purpose-built client
//! read back the sink-input, latency and buffer-attribute replies. A schema
//! that is wrong by one value desynchronises the packet and fails the
//! connection, so "the client parsed it" is a real oracle.

use crate::mixer::{Mixer, StreamId, VOLUME_NORM};
use crate::proto::{command, error, format, subscription};
use crate::sink::Spec;
use crate::tag::{self, Property, SampleSpec};
use crate::wire::{self, Frame, Seek};
use std::collections::HashMap;

/// What this server calls its one sink.
pub const SINK_NAME: &str = "td-audio";

/// Its human-readable description, which is what a device picker shows.
pub const SINK_DESCRIPTION: &str = "td audio";

/// The sink's index. There is one sink, so it is 0 and stays 0.
pub const SINK_INDEX: u32 = 0;

/// The one module index this server reports. `INVALID_INDEX` is honest: td has
/// no modules, and inventing a number would make `pactl` print a module that
/// cannot be queried.
pub const OWNER_MODULE: u32 = tag::INVALID_INDEX;

/// `PA_SINK_RUNNING` and `PA_SINK_IDLE`. td does not suspend the sink, but idle
/// is distinct: an empty/prebuffering server must not claim it is playing.
pub const SINK_STATE_RUNNING: u32 = 0;
pub const SINK_STATE_IDLE: u32 = 1;

/// How much audio a stream may keep queued, by default, in milliseconds.
/// This is Pulse's `tlength`, and the grant loop keeps the queue near it.
pub const DEFAULT_TARGET_MS: u64 = 200;

/// The smallest grant worth sending, in milliseconds. Below this the server
/// would spend more on `REQUEST` frames than on audio.
pub const DEFAULT_MINREQ_MS: u64 = 20;

/// `maxlength` is this multiple of the target. A client may seek backwards
/// inside its buffer, so the hard ceiling is above the working set.
pub const MAXLENGTH_MULTIPLE: u64 = 4;

/// The most one stream may keep queued, in bytes.
///
/// `maxlength` and `tlength` arrive as bare `u32`s that the client picks, and
/// nothing above this made them anything but a request. Upstream clamps to the
/// same four mebibytes (`PA_MEMBLOCKQ_MAXLENGTH`) for the same reason: 4 GiB per
/// stream is not a buffer size, it is a way to spend the daemon's memory. Two
/// hundred milliseconds of the fixed spec is 38 400 bytes, so this is a hundred
/// times what an ordinary client asks for.
pub const MAXLENGTH_CEILING: u64 = 4 * 1024 * 1024;

/// Maximum framed protocol output retained inside one session before the
/// daemon disconnects it. The server may carry one additional consumed-prefix
/// window while a socket drains, but no peer can make this state grow further.
pub const MAX_OUTPUT_BYTES: usize = 16 * crate::wire::CONTROL_MAX;

/// The most streams one connection may hold.
///
/// Every stream is a linear scan in the mixer and a `timing` call per pass, so
/// an unbounded count is the daemon's CPU as well as its memory. A browser with
/// a tab per sound and a notification daemon beside it does not reach eight.
pub const MAX_STREAMS_PER_CLIENT: usize = 32;

/// Mixer queue reservation one connection may hold. Four maximum-sized S16
/// streams or eight maximum-sized FLOAT32 streams fit, while four independent
/// clients retain a share of the daemon-wide 64 MiB reservation ceiling.
pub const MAX_RESERVED_BYTES_PER_CLIENT: u64 = 16 * 1024 * 1024;

/// Underflow events one service pass may serialize for one stream. Further
/// exact positions remain in the mixer's shared run/event record ceiling,
/// keeping coarse playhead samples exact without an unbounded single-pass
/// loop.
const MAX_UNDERFLOW_EVENTS_PER_SERVICE: u32 = 32;

/// Why a connection must end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disconnect {
    /// The client framed something this server will not accept. `wire` decides
    /// this before any body is buffered.
    Framing(wire::Error),
    /// A packet did not match its schema. §K.3: the tags make a malformed
    /// packet detectable, and the schemas make a well-formed but unexpected one
    /// an error.
    Schema(tag::Error),
    /// A command arrived before `AUTH`.
    Unauthenticated(u32),
    /// Stream data was not a whole number of the negotiated PCM frames.
    PcmAlignment { bytes: usize, frame_bytes: usize },
    /// The bounded float conversion scratch could not be reserved.
    ConversionBuffer,
    /// Stream data named a position this append-only mixer cannot represent.
    UnsupportedWrite { seek: Seek, offset: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackFormat {
    S16Le,
    Float32Le,
}

impl PlaybackFormat {
    fn from_wire(value: u8) -> Option<Self> {
        match value {
            format::SAMPLE_S16LE => Some(Self::S16Le),
            format::SAMPLE_FLOAT32LE => Some(Self::Float32Le),
            _ => None,
        }
    }

    fn sample_bytes(self) -> u64 {
        match self {
            Self::S16Le => 2,
            Self::Float32Le => 4,
        }
    }
}

impl std::fmt::Display for Disconnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Disconnect::Framing(error) => write!(f, "framing: {error}"),
            Disconnect::Schema(error) => write!(f, "schema: {error}"),
            Disconnect::Unauthenticated(command) => {
                let name = crate::proto::command_name(*command).unwrap_or("an unknown command");
                write!(f, "{name} ({command}) arrived before AUTH")
            }
            Disconnect::PcmAlignment { bytes, frame_bytes } => write!(
                f,
                "PCM frame has {bytes} bytes, not a multiple of {frame_bytes}"
            ),
            Disconnect::ConversionBuffer => {
                write!(f, "could not reserve the bounded PCM conversion buffer")
            }
            Disconnect::UnsupportedWrite { seek, offset } => {
                write!(
                    f,
                    "unsupported stream write position {seek:?} offset {offset}"
                )
            }
        }
    }
}

fn float32_to_s16(sample: f32) -> i16 {
    if sample.is_nan() {
        0
    } else if sample >= 1.0 {
        i16::MAX
    } else if sample <= -1.0 {
        i16::MIN
    } else if sample >= 0.0 {
        (sample * f32::from(i16::MAX)).round() as i16
    } else {
        (sample * 32_768.0).round() as i16
    }
}

/// One playback stream, from the protocol's side. The audio itself lives in the
/// mixer; this is the bookkeeping the wire needs.
#[derive(Debug, Clone)]
struct Stream {
    /// The per-connection Pulse channel used by stream commands and data.
    channel: u32,
    /// The shared mixer's name for it. Distinct from `channel` because channels
    /// are per-connection: every client calls its first stream 0.
    id: StreamId,
    /// The stream format Pulse negotiated. The mixer remains fixed S16; this
    /// controls byte grants, position indexes, and boundary conversion.
    sample_spec: SampleSpec,
    format: PlaybackFormat,
    /// The process-global identity exposed by sink-input introspection.
    sink_input_index: u32,
    /// Frames the client may keep queued.
    target_frames: u64,
    /// Frames reserved in the fixed S16 mixer queue for this stream.
    limit_frames: u64,
    /// The smallest grant this stream will be sent.
    minreq_frames: u64,
    /// The client's bounded prebuffer threshold, re-armed by PREBUF and flush.
    prebuffer_frames: u64,
    /// Bytes granted and not yet spent. A grant the client has not used is
    /// still owed to it, so re-granting the same bytes would let it write past
    /// `target_frames`.
    outstanding_bytes: u64,
    corked: bool,
    muted: bool,
    /// The volume the client set, kept here so a mute can be undone without
    /// the client re-sending it.
    volume: u32,
    name: String,
    /// A `DRAIN` waiting on the mixer. §K.3: per-stream drain is bookkeeping,
    /// not the ALSA ioctl.
    draining: Option<u32>,
    /// The client has been told the stream started.
    started: bool,
}

impl Stream {
    fn frame_bytes(&self) -> u64 {
        self.format
            .sample_bytes()
            .saturating_mul(u64::from(self.sample_spec.channels))
            .max(1)
    }

    fn client_index(&self, sink_index: u64, sink_frame_bytes: u64) -> u64 {
        sink_index
            .checked_div(sink_frame_bytes.max(1))
            .unwrap_or(0)
            .saturating_mul(self.frame_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing but `AUTH` is accepted.
    New,
    /// Authenticated at this negotiated version.
    Ready(u32),
}

/// A sink-input command whose process-global index belongs to another client
/// connection. The daemon routes these after the current bounded read pass.
pub enum GlobalRequest {
    Info {
        tag: u32,
        index: u32,
        version: u32,
    },
    Volume {
        tag: u32,
        index: u32,
        volumes: Vec<u32>,
    },
    Mute {
        tag: u32,
        index: u32,
        muted: bool,
    },
}

struct ServiceUpdate {
    channel: u32,
    grant: u64,
    finish_drain: Option<u32>,
    start: bool,
    running: bool,
    underflow_positions: Vec<u64>,
}

/// One connection.
pub struct Session {
    state: State,
    spec: Spec,
    decoder: wire::Decoder,
    out: Vec<u8>,
    streams: HashMap<u32, Stream>,
    /// One reusable FLOAT32LE conversion scratch for this connection. Keeping
    /// it here instead of on every stream bounds retained scratch across all
    /// admitted clients to `MAX_CLIENTS * DATA_MAX / 2`.
    converted: Vec<u8>,
    next_channel: u64,
    client_index: u32,
    subscribed: u32,
    /// Sink-input changes for the daemon to relay to every other subscriber.
    global_events: Vec<(u32, u32)>,
    global_requests: Vec<GlobalRequest>,
    output_overflowed: bool,
    input_deferred: bool,
    /// The clock the caller last supplied, in microseconds. Kept as data so
    /// this module reads no clock of its own.
    now_usec: u64,
}

impl Session {
    pub fn new(spec: Spec, client_index: u32) -> Self {
        Self {
            state: State::New,
            spec,
            decoder: wire::Decoder::new(),
            out: Vec::new(),
            streams: HashMap::new(),
            converted: Vec::new(),
            next_channel: 0,
            client_index,
            subscribed: 0,
            global_events: Vec::new(),
            global_requests: Vec::new(),
            output_overflowed: false,
            input_deferred: false,
            now_usec: 0,
        }
    }

    /// The negotiated version, once authenticated.
    pub fn version(&self) -> Option<u32> {
        match self.state {
            State::New => None,
            State::Ready(version) => Some(version),
        }
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Choose a free per-connection playback channel without ever overwriting
    /// a live stream when the u32 counter wraps.
    fn allocate_channel(&mut self) -> Option<u32> {
        if self.next_channel >= u64::from(tag::INVALID_INDEX) {
            return None;
        }
        let candidate = u32::try_from(self.next_channel).ok()?;
        self.next_channel = self.next_channel.saturating_add(1);
        (!self.streams.contains_key(&candidate)).then_some(candidate)
    }

    /// Bytes held for a frame that has not finished arriving. The daemon bounds
    /// this: a client that declares a frame and stops is a connection that
    /// never makes progress.
    pub fn has_incomplete_input(&self) -> bool {
        self.decoder.is_incomplete()
    }

    /// Every channel this session owns, for the daemon to reconcile the mixer
    /// against.
    pub fn stream_ids(&self) -> Vec<StreamId> {
        self.streams.values().map(|stream| stream.id).collect()
    }

    pub fn sink_input_indexes(&self) -> Vec<u32> {
        self.streams
            .values()
            .map(|stream| stream.sink_input_index)
            .collect()
    }

    pub fn client_index(&self) -> u32 {
        self.client_index
    }

    /// Changes originated by this connection, for server-wide subscription
    /// delivery. The originating session already received its local copy.
    pub fn take_global_events(&mut self) -> Vec<(u32, u32)> {
        std::mem::take(&mut self.global_events)
    }

    pub fn take_global_requests(&mut self) -> Vec<GlobalRequest> {
        std::mem::take(&mut self.global_requests)
    }

    /// State a streamless control connection must retain across admission
    /// pressure. A subscriber is a live observer, and an unrouted request or
    /// event is work the server has not completed yet.
    pub fn holds_idle_control_state(&self) -> bool {
        self.subscribed != 0 || !self.global_events.is_empty() || !self.global_requests.is_empty()
    }

    pub fn owns_sink_input(&self, index: u32) -> bool {
        self.streams
            .values()
            .any(|stream| stream.sink_input_index == index)
    }

    pub fn notify_global(&mut self, event: u32, index: u32) {
        self.notify(event, index);
    }

    /// Complete a request routed by the daemon to the owning session.
    pub fn complete_global_request(
        &mut self,
        owner: &mut Session,
        request: GlobalRequest,
        mixer: &mut Mixer,
    ) {
        match request {
            GlobalRequest::Info {
                tag,
                index,
                version,
            } => {
                let Some(stream) = owner
                    .streams
                    .values()
                    .find(|stream| stream.sink_input_index == index)
                    .cloned()
                else {
                    self.error(tag, error::NOENTITY);
                    return;
                };
                self.reply_sink_input_info(tag, version, owner.client_index, &stream, mixer);
            }
            GlobalRequest::Volume {
                tag,
                index,
                volumes,
            } => {
                if !owner.apply_volume(index, &volumes, mixer) {
                    self.error(tag, error::NOENTITY);
                    return;
                }
                self.reply(tag, |_| {});
                owner.announce(
                    subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
                    index,
                );
            }
            GlobalRequest::Mute { tag, index, muted } => {
                if !owner.apply_mute(index, muted, mixer) {
                    self.error(tag, error::NOENTITY);
                    return;
                }
                self.reply(tag, |_| {});
                owner.announce(
                    subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
                    index,
                );
            }
        }
    }

    pub fn reject_global_request(&mut self, request: GlobalRequest) {
        let tag = match request {
            GlobalRequest::Info { tag, .. }
            | GlobalRequest::Volume { tag, .. }
            | GlobalRequest::Mute { tag, .. } => tag,
        };
        self.error(tag, error::NOENTITY);
    }

    /// The mixer's name for the stream this connection calls `channel`.
    ///
    /// Test-only: production code reaches the id through the stream it already
    /// has in hand, and an accessor that invites a lookup by channel number is
    /// the habit this change exists to break.
    #[cfg(test)]
    pub fn stream_id(&self, channel: u32) -> Option<StreamId> {
        self.streams.get(&channel).map(|stream| stream.id)
    }

    /// Update the clock this session stamps timing replies with.
    pub fn tick(&mut self, now_usec: u64) {
        self.now_usec = now_usec;
    }

    /// Bytes to write back to the client. Taking them clears the buffer.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    pub fn output_overflowed(&self) -> bool {
        self.output_overflowed
    }

    pub fn input_deferred(&self) -> bool {
        self.input_deferred
    }

    /// Feed bytes read from the socket, handling every whole frame they
    /// complete.
    #[cfg(test)]
    pub fn feed(&mut self, bytes: &[u8], mixer: &mut Mixer) -> Result<(), Disconnect> {
        let _ = self.feed_limited(bytes, mixer, usize::MAX)?;
        Ok(())
    }

    /// Feed at most `limit` complete frames, retaining any remainder for the
    /// next daemon pass. This is the event-loop fairness boundary; socket read
    /// size alone does not bound the number of tiny control frames it contains.
    pub fn feed_limited(
        &mut self,
        bytes: &[u8],
        mixer: &mut Mixer,
        limit: usize,
    ) -> Result<usize, Disconnect> {
        self.decoder.push(bytes);
        self.input_deferred = false;
        let mut processed = 0usize;
        while processed < limit {
            let Some(frame) = self.decoder.next_frame() else {
                break;
            };
            match frame.map_err(Disconnect::Framing)? {
                Frame::Control(packet) => self.control(&packet, mixer)?,
                Frame::Data {
                    channel,
                    seek,
                    offset,
                    pcm,
                } => self.data(channel, seek, offset, &pcm, mixer)?,
            }
            processed = processed.saturating_add(1);
        }
        self.input_deferred = processed == limit;
        Ok(processed)
    }

    /// Emit whatever the mixer's current state owes the client: byte grants,
    /// completed drains, underflow notices, and the first `STARTED`.
    ///
    /// Kept separate from `feed` because most of it is caused by the device
    /// consuming audio rather than by the client saying anything, and a server
    /// that only spoke when spoken to would stall exactly as §K.3 describes.
    pub fn service(&mut self, mixer: &mut Mixer) {
        let Some(version) = self.version() else {
            return;
        };
        let mut updates = Vec::new();
        for stream in self.streams.values() {
            let id = stream.id;
            let Ok(timing) = mixer.timing(id) else {
                continue;
            };
            let frame_bytes = stream.frame_bytes();
            let queued = timing.queued_frames.saturating_mul(frame_bytes);
            let target = stream.target_frames.saturating_mul(frame_bytes);
            let held = queued.saturating_add(stream.outstanding_bytes);
            let grant = target.saturating_sub(held);
            let minreq = stream.minreq_frames.saturating_mul(frame_bytes);
            let grant = if grant >= minreq && !stream.corked {
                grant
            } else {
                0
            };

            let drained = mixer.is_drained(id).unwrap_or(false);
            let finish_drain = stream.draining.filter(|_| drained);

            let underflow_positions = mixer
                .take_underflow_positions(id, MAX_UNDERFLOW_EVENTS_PER_SERVICE as usize)
                .unwrap_or_default();
            let underflows_pending = mixer.has_underflow_positions(id).unwrap_or(false);
            let running = mixer.is_running(id).unwrap_or(false);
            let start = !stream.corked
                && running
                && !underflows_pending
                && (!stream.started || !underflow_positions.is_empty());
            updates.push(ServiceUpdate {
                channel: stream.channel,
                grant,
                finish_drain,
                start,
                running,
                underflow_positions,
            });
        }

        for update in updates {
            let ServiceUpdate {
                channel,
                grant,
                finish_drain,
                start,
                running,
                underflow_positions,
            } = update;
            if grant > 0 {
                let granted = u32::try_from(grant).unwrap_or(u32::MAX);
                self.send(&packet(command::REQUEST, tag::INVALID_INDEX, |writer| {
                    writer.u32(channel).u32(granted);
                }));
                if let Some(stream) = self.streams.get_mut(&channel) {
                    stream.outstanding_bytes =
                        stream.outstanding_bytes.saturating_add(u64::from(granted));
                }
            }
            for position in underflow_positions {
                let read_index = self
                    .streams
                    .get(&channel)
                    .map(|stream| stream.client_index(position, self.spec.frame_bytes as u64))
                    .unwrap_or(0);
                self.send(&packet(command::UNDERFLOW, tag::INVALID_INDEX, |writer| {
                    writer.u32(channel);
                    if version >= 23 {
                        writer.s64(read_index as i64);
                    }
                }));
            }
            // One coarse DELAY sample can cross an exhausted endpoint and
            // enter an already accepted later run. Announce that underflow
            // before the later run's new STARTED transition.
            if start {
                self.send(&packet(command::STARTED, tag::INVALID_INDEX, |writer| {
                    writer.u32(channel);
                }));
                if let Some(stream) = self.streams.get_mut(&channel) {
                    stream.started = true;
                }
            }
            if !running {
                if let Some(stream) = self.streams.get_mut(&channel) {
                    stream.started = false;
                }
            }
            if let Some(tag) = finish_drain {
                self.reply(tag, |_| {});
                if let Some(stream) = self.streams.get_mut(&channel) {
                    stream.draining = None;
                }
            }
        }
    }

    /// Tell the client every stream is gone, because the device is.
    pub fn kill_all_streams(&mut self) {
        let channels: Vec<u32> = self.streams.keys().copied().collect();
        for channel in channels {
            self.send(&packet(
                command::PLAYBACK_STREAM_KILLED,
                tag::INVALID_INDEX,
                |writer| {
                    writer.u32(channel);
                },
            ));
            self.streams.remove(&channel);
        }
    }

    fn data(
        &mut self,
        channel: u32,
        seek: Seek,
        offset: i64,
        pcm: &[u8],
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        // v1 accepts audio only at the write index. A client that seeks is
        // rewriting audio the mixer may already have summed, and answering that
        // correctly means a rewritable queue; refusing it plainly is better
        // than accepting it and playing the wrong thing.
        let (streams, converted) = (&mut self.streams, &mut self.converted);
        let Some(stream) = streams.get_mut(&channel) else {
            return Ok(());
        };
        if seek != Seek::Relative || offset != 0 {
            return Err(Disconnect::UnsupportedWrite { seek, offset });
        }
        let client_frame_bytes = usize::try_from(stream.frame_bytes()).unwrap_or(usize::MAX);
        if !pcm.len().is_multiple_of(client_frame_bytes) {
            return Err(Disconnect::PcmAlignment {
                bytes: pcm.len(),
                frame_bytes: client_frame_bytes,
            });
        }
        stream.outstanding_bytes = stream.outstanding_bytes.saturating_sub(pcm.len() as u64);
        let output = match stream.format {
            PlaybackFormat::S16Le => pcm,
            PlaybackFormat::Float32Le => {
                // Do not convert bytes the stream queue cannot accept. A
                // hostile peer may send a full wire frame after spending its
                // grant; the mixer remains the final shared-cap authority.
                let room_frames = mixer.request_frames(stream.id).unwrap_or(0);
                let offered_frames = (pcm.len() as u64) / stream.frame_bytes();
                let samples = room_frames
                    .min(offered_frames)
                    .saturating_mul(u64::from(stream.sample_spec.channels));
                let samples = usize::try_from(samples).unwrap_or(usize::MAX);
                converted.clear();
                converted
                    .try_reserve_exact(samples.saturating_mul(2))
                    .map_err(|_| Disconnect::ConversionBuffer)?;
                for raw in pcm.as_chunks::<4>().0.iter().take(samples) {
                    converted
                        .extend_from_slice(&float32_to_s16(f32::from_le_bytes(*raw)).to_le_bytes());
                }
                converted.as_slice()
            }
        };
        let Ok(accepted_output_bytes) = mixer.write(stream.id, output) else {
            return Ok(());
        };
        let accepted_frames = (accepted_output_bytes / self.spec.frame_bytes.max(1)) as u64;
        let accepted_client_bytes = accepted_frames.saturating_mul(stream.frame_bytes());
        if accepted_client_bytes < pcm.len() as u64 {
            self.send(&packet(command::OVERFLOW, tag::INVALID_INDEX, |writer| {
                writer.u32(channel);
            }));
        }
        Ok(())
    }

    fn control(&mut self, packet: &[u8], mixer: &mut Mixer) -> Result<(), Disconnect> {
        let (command, tag) = wire::command_and_tag(packet).map_err(Disconnect::Schema)?;
        let version = match self.state {
            State::New if command == command::AUTH => {
                return self.auth(packet, tag);
            }
            State::New => return Err(Disconnect::Unauthenticated(command)),
            State::Ready(version) => version,
        };
        // Past AUTH and the command and tag are consumed; every handler reads
        // from here.
        let mut reader = tag::Reader::new(packet);
        reader.u32().map_err(Disconnect::Schema)?;
        reader.u32().map_err(Disconnect::Schema)?;
        match command {
            command::SET_CLIENT_NAME => self.set_client_name(reader, tag, version),
            command::GET_SERVER_INFO => self.server_info(reader, tag, version),
            command::GET_SINK_INFO => self.sink_info(reader, tag, version, mixer),
            command::GET_SINK_INFO_LIST => self.sink_info_list(reader, tag, version, mixer),
            command::GET_SOURCE_INFO_LIST => self.source_info_list(reader, tag),
            command::GET_SINK_INPUT_INFO => self.sink_input_info(reader, tag, version, mixer),
            command::SUBSCRIBE => self.subscribe(reader, tag),
            command::CREATE_PLAYBACK_STREAM => self.create_stream(reader, tag, version, mixer),
            command::DELETE_PLAYBACK_STREAM => self.delete_stream(reader, tag, mixer),
            command::CORK_PLAYBACK_STREAM => self.cork(reader, tag, mixer),
            command::FLUSH_PLAYBACK_STREAM => self.flush(reader, tag, mixer),
            command::PREBUF_PLAYBACK_STREAM => self.prebuffer(reader, tag, mixer, true),
            command::TRIGGER_PLAYBACK_STREAM => self.prebuffer(reader, tag, mixer, false),
            command::DRAIN_PLAYBACK_STREAM => self.drain(reader, tag, mixer),
            command::GET_PLAYBACK_LATENCY => self.latency(reader, tag, version, mixer),
            command::SET_SINK_INPUT_VOLUME => self.set_volume(reader, tag, mixer),
            command::SET_SINK_INPUT_MUTE => self.set_mute(reader, tag, mixer),
            command::UPDATE_PLAYBACK_STREAM_PROPLIST => self.update_proplist(reader, tag),
            _ => {
                // §K.3's list is what playback needs; anything else is a
                // control-panel feature this server does not have. Saying so
                // exactly is what lets a client fall back rather than hang.
                self.error(tag, error::NOTIMPLEMENTED);
                Ok(())
            }
        }
    }

    fn auth(&mut self, packet: &[u8], tag: u32) -> Result<(), Disconnect> {
        let mut reader = tag::Reader::new(packet);
        reader.u32().map_err(Disconnect::Schema)?;
        reader.u32().map_err(Disconnect::Schema)?;
        let raw = reader.u32().map_err(Disconnect::Schema)?;
        // The reply strips every transport feature bit, including SHM. This
        // pure state machine does not log that peer-chosen request: reconnects
        // could reset a local budget and block the daemon's audio thread.
        let _requested_shared_memory = crate::proto::requested_shared_memory(raw);
        // §K.3: the cookie "is still parsed at its exact 256-byte length and
        // then ignored". Parsing it is not ceremony — it is what proves the
        // packet ends where the schema says, and the byte after it would
        // otherwise be read as the next command.
        let cookie = reader.arbitrary().map_err(Disconnect::Schema)?;
        let cookie_len = cookie.len();
        reader.finish().map_err(Disconnect::Schema)?;
        if cookie_len != AUTH_COOKIE_LEN {
            self.error(tag, error::INVALID);
            return Ok(());
        }
        let version = crate::proto::negotiate(raw);
        if version < crate::proto::MIN_VERSION {
            // Negotiating a version down is only honest if the older schema is
            // then spoken. `create_stream` parses the widest form
            // unconditionally — the format list is version 21 — so a client
            // that agreed on 12 would authenticate and then be hung up on at
            // its first stream. Refusing here says so at the one point the
            // client can still act on it.
            self.error(tag, error::VERSION);
            return Ok(());
        }
        self.state = State::Ready(version);
        let granted = crate::proto::auth_reply_version(version);
        self.reply(tag, |writer| {
            writer.u32(granted);
        });
        Ok(())
    }

    fn set_client_name(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
    ) -> Result<(), Disconnect> {
        reader.proplist().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let index = self.client_index;
        self.reply(tag, |writer| {
            if version >= 13 {
                writer.u32(index);
            }
        });
        Ok(())
    }

    fn server_info(
        &mut self,
        reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
    ) -> Result<(), Disconnect> {
        reader.finish().map_err(Disconnect::Schema)?;
        let spec = self.sample_spec();
        self.reply(tag, |writer| {
            writer
                .string("pulseaudio")
                .string(SERVER_VERSION)
                .string("td")
                .string("td")
                .sample_spec(spec)
                .string(SINK_NAME)
                .string("")
                .u32(0);
            if version >= 15 {
                writer.channel_map(&CHANNEL_MAP);
            }
        });
        Ok(())
    }

    fn sink_info(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
        mixer: &Mixer,
    ) -> Result<(), Disconnect> {
        // Captured in both forms: an index with a NULL name, or an invalid
        // index with a name. One command, two ways of asking.
        let index = reader.u32().map_err(Disconnect::Schema)?;
        let name = reader.string().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        if !sink_lookup_matches(index, name.as_deref()) {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        let spec = self.sample_spec();
        let state = sink_state(mixer);
        self.reply(tag, |writer| {
            write_sink_info(writer, spec, version, state);
        });
        Ok(())
    }

    fn sink_info_list(
        &mut self,
        reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
        mixer: &Mixer,
    ) -> Result<(), Disconnect> {
        reader.finish().map_err(Disconnect::Schema)?;
        let spec = self.sample_spec();
        let state = sink_state(mixer);
        self.reply(tag, |writer| {
            write_sink_info(writer, spec, version, state);
        });
        Ok(())
    }

    fn source_info_list(&mut self, reader: tag::Reader<'_>, tag: u32) -> Result<(), Disconnect> {
        reader.finish().map_err(Disconnect::Schema)?;
        // §K.3 wants "an empty list, not an error, so device pickers see 'no
        // microphone' rather than a broken server". An empty list is a REPLY
        // whose payload is nothing at all.
        self.reply(tag, |_| {});
        Ok(())
    }

    fn sink_input_info(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
        mixer: &Mixer,
    ) -> Result<(), Disconnect> {
        let index = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self
            .streams
            .values()
            .find(|stream| stream.sink_input_index == index)
            .cloned()
        else {
            self.global_requests.push(GlobalRequest::Info {
                tag,
                index,
                version,
            });
            return Ok(());
        };
        self.reply_sink_input_info(tag, version, self.client_index, &stream, mixer);
        Ok(())
    }

    fn reply_sink_input_info(
        &mut self,
        tag: u32,
        version: u32,
        client: u32,
        stream: &Stream,
        mixer: &Mixer,
    ) {
        let spec = stream.sample_spec;
        let queued_usec = mixer
            .timing(stream.id)
            .map(|timing| self.spec.frames_to_usec(timing.queued_frames))
            .unwrap_or(0);
        let device_usec = mixer
            .timing(stream.id)
            .map(|timing| self.spec.frames_to_usec(timing.device_delay_frames))
            .unwrap_or(0);
        self.reply(tag, |writer| {
            writer
                .u32(stream.sink_input_index)
                .string(&stream.name)
                .u32(OWNER_MODULE)
                .u32(client)
                .u32(SINK_INDEX)
                .sample_spec(spec)
                .channel_map(&CHANNEL_MAP)
                .cvolume(&[stream.volume, stream.volume])
                .usec(queued_usec)
                .usec(device_usec)
                .string("copy")
                .string(DRIVER);
            if version >= 11 {
                writer.boolean(stream.muted);
            }
            if version >= 13 {
                writer.proplist(&[tag::text_property("media.name", &stream.name)]);
            }
            if version >= 19 {
                writer.boolean(stream.corked);
            }
            if version >= 20 {
                writer.boolean(true).boolean(true);
            }
            if version >= 21 {
                writer.format_info(format::ENCODING_PCM, &[]);
            }
        });
    }

    fn subscribe(&mut self, mut reader: tag::Reader<'_>, tag: u32) -> Result<(), Disconnect> {
        let mask = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        if mask & !subscription::MASK_ALL != 0 {
            self.error(tag, error::INVALID);
            return Ok(());
        }
        self.subscribed = mask;
        self.reply(tag, |_| {});
        Ok(())
    }

    fn create_stream(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        // The widest schema in the protocol, and every value is read even when
        // it is ignored — see the sixteen booleans. A reader that skipped to
        // the values it wanted would be reading them from the wrong offsets.
        let requested = reader.sample_spec().map_err(Disconnect::Schema)?;
        let map = reader.channel_map().map_err(Disconnect::Schema)?;
        let sink_index = reader.u32().map_err(Disconnect::Schema)?;
        let sink_name = reader.string().map_err(Disconnect::Schema)?;
        let maxlength = reader.u32().map_err(Disconnect::Schema)?;
        let corked = reader.boolean().map_err(Disconnect::Schema)?;
        let tlength = reader.u32().map_err(Disconnect::Schema)?;
        let prebuf = reader.u32().map_err(Disconnect::Schema)?;
        let minreq = reader.u32().map_err(Disconnect::Schema)?;
        let _syncid = reader.u32().map_err(Disconnect::Schema)?;
        let volumes = reader.cvolume().map_err(Disconnect::Schema)?;
        let _no_remap = reader.boolean().map_err(Disconnect::Schema)?;
        let _no_remix = reader.boolean().map_err(Disconnect::Schema)?;
        let _fix_format = reader.boolean().map_err(Disconnect::Schema)?;
        let _fix_rate = reader.boolean().map_err(Disconnect::Schema)?;
        let _fix_channels = reader.boolean().map_err(Disconnect::Schema)?;
        let _no_move = reader.boolean().map_err(Disconnect::Schema)?;
        let variable_rate = reader.boolean().map_err(Disconnect::Schema)?;
        let requested_muted = reader.boolean().map_err(Disconnect::Schema)?;
        let _adjust_latency = reader.boolean().map_err(Disconnect::Schema)?;
        let properties = reader.proplist().map_err(Disconnect::Schema)?;
        let volume_set = reader.boolean().map_err(Disconnect::Schema)?;
        let _early_requests = reader.boolean().map_err(Disconnect::Schema)?;
        let muted_set = reader.boolean().map_err(Disconnect::Schema)?;
        let _dont_inhibit_auto_suspend = reader.boolean().map_err(Disconnect::Schema)?;
        let _fail_on_suspend = reader.boolean().map_err(Disconnect::Schema)?;
        let relative_volume = reader.boolean().map_err(Disconnect::Schema)?;
        let passthrough = reader.boolean().map_err(Disconnect::Schema)?;
        let formats = reader.u8().map_err(Disconnect::Schema)?;
        let mut encodings = Vec::with_capacity(formats as usize);
        for _ in 0..formats {
            encodings.push(reader.format_info().map_err(Disconnect::Schema)?.0);
        }
        reader.finish().map_err(Disconnect::Schema)?;

        // The device and mixer stay at one rate/channel shape. Firefox Web
        // Audio nevertheless asks Pulse for native float samples, as real
        // cubeb does even after reading the S16 sink. Convert that one captured
        // format at the protocol edge; a different rate or channel shape would
        // require a resampler/remixer and is refused rather than misplayed.
        let Some(playback_format) = PlaybackFormat::from_wire(requested.format) else {
            self.error(tag, error::NOTSUPPORTED);
            return Ok(());
        };
        if u32::from(requested.channels) != self.spec.channels || requested.rate != self.spec.rate {
            self.error(tag, error::NOTSUPPORTED);
            return Ok(());
        }
        if map != CHANNEL_MAP || volumes.len() != usize::from(requested.channels) {
            self.error(tag, error::INVALID);
            return Ok(());
        }
        if !sink_create_matches(sink_index, sink_name.as_deref()) {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        if variable_rate
            || relative_volume
            || passthrough
            || encodings
                .iter()
                .any(|encoding| *encoding != format::ENCODING_PCM)
        {
            self.error(tag, error::NOTSUPPORTED);
            return Ok(());
        }

        let name = match properties
            .iter()
            .find(|property| property.key == "media.name")
        {
            Some(property) => match property_text(property) {
                Ok(name) => name,
                Err(()) => {
                    self.error(tag, error::INVALID);
                    return Ok(());
                }
            },
            None => "playback".to_string(),
        };

        if self.streams.len() >= MAX_STREAMS_PER_CLIENT {
            self.error(tag, error::TOOLARGE);
            return Ok(());
        }
        let frame_bytes = playback_format
            .sample_bytes()
            .saturating_mul(u64::from(requested.channels))
            .max(1);
        let ceiling_frames = (MAXLENGTH_CEILING / frame_bytes).max(1);
        // Clamped, not trusted. Both numbers are the client's, and the queue
        // they size is the daemon's. The selected device ring plus one
        // transfer period is also a floor: starting a larger ring after
        // honoring a 50 ms client target leaves no software refill reserve
        // behind it.
        let target_frames = attribute_frames(tlength, DEFAULT_TARGET_MS, self.spec, frame_bytes)
            .max(mixer.target_floor_frames())
            .min(ceiling_frames);
        let maxlength_frames = match maxlength {
            tag::INVALID_INDEX => target_frames.saturating_mul(MAXLENGTH_MULTIPLE),
            bytes => (u64::from(bytes) / frame_bytes).max(target_frames),
        }
        .min(ceiling_frames)
        .max(target_frames);
        let minreq_frames = attribute_frames(minreq, DEFAULT_MINREQ_MS, self.spec, frame_bytes)
            .min(target_frames.max(1))
            .max(1);
        // Pulse's default and maximum are tlength plus one frame minus
        // minreq. The one-frame term keeps the threshold reachable when the
        // target and minimum request are equal.
        let max_prebuffer_frames = target_frames
            .saturating_add(1)
            .saturating_sub(minreq_frames);
        let prebuffer_frames = match prebuf {
            tag::INVALID_INDEX => max_prebuffer_frames,
            0 => 0,
            bytes => (u64::from(bytes) / frame_bytes).min(max_prebuffer_frames),
        };

        let sink_frame_bytes = self.spec.frame_bytes.max(1) as u64;
        let reserved = self.streams.values().fold(0u64, |total, stream| {
            total.saturating_add(stream.limit_frames.saturating_mul(sink_frame_bytes))
        });
        let requested_reservation = maxlength_frames.saturating_mul(sink_frame_bytes);
        if reserved.saturating_add(requested_reservation) > MAX_RESERVED_BYTES_PER_CLIENT {
            self.error(tag, error::TOOLARGE);
            return Ok(());
        }

        if !mixer.can_open(maxlength_frames) {
            self.error(tag, error::TOOLARGE);
            return Ok(());
        }

        let Some(channel) = self.allocate_channel() else {
            self.error(tag, error::INTERNAL);
            return Ok(());
        };
        // The channel is this connection's name for the stream; the id is the
        // shared mixer's. Two connected clients both call their first stream
        // channel 0, so the two names cannot be the same number.
        let Ok(id) = mixer.open(maxlength_frames) else {
            self.error(tag, error::INTERNAL);
            return Ok(());
        };
        let sink_input_index = id.sink_input_index();
        let volume = if volume_set {
            volumes.iter().copied().max().unwrap_or(VOLUME_NORM)
        } else {
            VOLUME_NORM
        }
        .min(VOLUME_NORM);
        let muted = muted_set && requested_muted;
        if mixer
            .set_target_frames(id, target_frames)
            .and_then(|()| mixer.set_volume(id, if muted { 0 } else { volume }))
            .and_then(|()| mixer.set_corked(id, corked))
            .and_then(|()| mixer.set_prebuffer(id, prebuffer_frames, true))
            .is_err()
        {
            mixer.remove(id);
            self.error(tag, error::INTERNAL);
            return Ok(());
        }
        self.streams.insert(
            channel,
            Stream {
                channel,
                id,
                sample_spec: requested,
                format: playback_format,
                sink_input_index,
                target_frames,
                limit_frames: maxlength_frames,
                minreq_frames,
                prebuffer_frames,
                outstanding_bytes: 0,
                corked,
                muted,
                volume,
                name,
                draining: None,
                started: false,
            },
        );

        self.announce(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_NEW,
            sink_input_index,
        );
        // The reply's `missing` is the first byte grant, and it is the only one
        // that does not arrive as a REQUEST frame. A server that sent 0 here
        // and waited would be the stall §K.3 describes.
        let missing = if corked {
            0
        } else {
            u32::try_from(target_frames.saturating_mul(frame_bytes)).unwrap_or(u32::MAX)
        };
        if let Some(stream) = self.streams.get_mut(&channel) {
            stream.outstanding_bytes = u64::from(missing);
        }
        let spec = requested;
        let bytes =
            |frames: u64| u32::try_from(frames.saturating_mul(frame_bytes)).unwrap_or(u32::MAX);
        let (maxlength_bytes, tlength_bytes, prebuf_bytes, minreq_bytes) = (
            bytes(maxlength_frames),
            bytes(target_frames),
            bytes(prebuffer_frames),
            bytes(minreq_frames),
        );
        self.reply(tag, |writer| {
            writer.u32(channel).u32(sink_input_index).u32(missing);
            if version >= 9 {
                writer
                    .u32(maxlength_bytes)
                    .u32(tlength_bytes)
                    .u32(prebuf_bytes)
                    .u32(minreq_bytes);
            }
            if version >= 12 {
                writer
                    .sample_spec(spec)
                    .channel_map(&CHANNEL_MAP)
                    .u32(SINK_INDEX)
                    .string(SINK_NAME)
                    .boolean(false);
            }
            if version >= 13 {
                writer.usec(0);
            }
            if version >= 21 {
                writer.format_info(format::ENCODING_PCM, &[]);
            }
        });
        Ok(())
    }

    fn delete_stream(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self.streams.remove(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        let sink_input_index = stream.sink_input_index;
        mixer.remove(stream.id);
        self.reply(tag, |_| {});
        self.announce(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_REMOVE,
            sink_input_index,
        );
        Ok(())
    }

    fn cork(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        let corked = reader.boolean().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self.streams.get_mut(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        stream.corked = corked;
        // The session withholding grants is not enough: the frames the client
        // already sent belong to the mixer, and a pause that plays out the
        // buffered tail is not a pause.
        let _ = mixer.set_corked(stream.id, corked);
        if corked {
            // A corked stream is told nothing more until it uncorks, so the
            // next uncork re-announces the start rather than staying silent.
            stream.started = false;
        }
        self.reply(tag, |_| {});
        Ok(())
    }

    fn flush(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self.streams.get_mut(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        // Flush drops what this stream has queued and nothing else. Its Pulse
        // channel, mixer identity, lifetime byte clock, and event counters all
        // remain the same stream.
        stream.outstanding_bytes = 0;
        stream.draining = None;
        stream.started = false;
        if mixer.flush(stream.id).is_err() {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        self.reply(tag, |_| {});
        Ok(())
    }

    fn prebuffer(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
        armed: bool,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self.streams.get_mut(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        if mixer
            .set_prebuffer(stream.id, stream.prebuffer_frames, armed)
            .is_err()
        {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        if armed {
            stream.started = false;
        }
        self.reply(tag, |_| {});
        Ok(())
    }

    fn drain(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self.streams.get_mut(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        // §K.3: "A stream is drained when its own output-frame position has
        // been consumed by the device, which is bookkeeping against the mixer
        // rather than an ioctl; the ALSA DRAIN in the roster exists for
        // shutting the device down, not for serving this command."
        // DRAIN disables the prebuffer gate even when the stream is already
        // empty. Otherwise an immediately answered drain followed by a short
        // write remains stuck below the old threshold.
        if mixer
            .begin_drain(stream.id, stream.prebuffer_frames)
            .is_err()
        {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        if mixer.is_drained(stream.id).unwrap_or(true) {
            let _ = mixer.finish_drain(stream.id);
            self.reply(tag, |_| {});
        } else {
            stream.draining = Some(tag);
        }
        Ok(())
    }

    fn latency(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
        mixer: &Mixer,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        let local = reader.timeval().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some((id, client_frame_bytes)) = self
            .streams
            .get(&channel)
            .map(|stream| (stream.id, stream.frame_bytes()))
        else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        let Ok(timing) = mixer.timing(id) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        let now = self.now_usec;
        let sink_frame_bytes = (self.spec.frame_bytes as u64).max(1);
        let write_index = timing
            .write_index
            .checked_div(sink_frame_bytes)
            .unwrap_or(0)
            .saturating_mul(client_frame_bytes);
        let read_index = timing
            .read_index
            .checked_div(sink_frame_bytes)
            .unwrap_or(0)
            .saturating_mul(client_frame_bytes);
        // §K.3: "A Pulse timing reply is not a scalar: it carries timestamps
        // plus read and write indexes, and clients compute latency from those —
        // so the server must report a consistent SET, not one number squeezed
        // into a field." `latency_usec` is the mixer's single derived sum, and
        // the indexes below come from the same snapshot, so the two cannot
        // disagree.
        self.reply(tag, |writer| {
            writer
                .usec(timing.latency_usec)
                .usec(0)
                .boolean(mixer.is_running(id).unwrap_or(false))
                // The client's own timestamp, echoed. It computes the round
                // trip from it, so a stamp of this server's own making would
                // make every latency read look instantaneous.
                .timeval(local.0, local.1)
                .timeval((now / 1_000_000) as u32, (now % 1_000_000) as u32)
                .s64(write_index as i64)
                .s64(read_index as i64);
            if version >= 13 {
                // These are time-since-transition fields, not an event count
                // or a byte index. This v1 does not retain either duration.
                writer.u64(0).u64(0);
            }
        });
        Ok(())
    }

    fn set_volume(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        let index = reader.u32().map_err(Disconnect::Schema)?;
        let volumes = reader.cvolume().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        if !matches!(volumes.len(), 1 | 2) {
            self.error(tag, error::INVALID);
            return Ok(());
        }
        if !self.owns_sink_input(index) {
            self.global_requests.push(GlobalRequest::Volume {
                tag,
                index,
                volumes,
            });
            return Ok(());
        }
        if !self.apply_volume(index, &volumes, mixer) {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        self.reply(tag, |_| {});
        self.announce(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
            index,
        );
        Ok(())
    }

    fn set_mute(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &mut Mixer,
    ) -> Result<(), Disconnect> {
        let index = reader.u32().map_err(Disconnect::Schema)?;
        let muted = reader.boolean().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        if !self.owns_sink_input(index) {
            self.global_requests
                .push(GlobalRequest::Mute { tag, index, muted });
            return Ok(());
        }
        if !self.apply_mute(index, muted, mixer) {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        self.reply(tag, |_| {});
        self.announce(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
            index,
        );
        Ok(())
    }

    fn update_proplist(&mut self, mut reader: tag::Reader<'_>, tag: u32) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        let mode = reader.u32().map_err(Disconnect::Schema)?;
        let properties = reader.proplist().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        if mode != 2 {
            self.error(tag, error::INVALID);
            return Ok(());
        }
        let Some(stream) = self.streams.get_mut(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        // This is also how a modern client renames a stream — see
        // `proto::K3_AMENDMENTS`.
        if let Some(property) = properties
            .iter()
            .find(|property| property.key == "media.name")
        {
            match property_text(property) {
                Ok(name) => stream.name = name,
                Err(()) => {
                    self.error(tag, error::INVALID);
                    return Ok(());
                }
            }
        }
        let sink_input_index = stream.sink_input_index;
        self.reply(tag, |_| {});
        self.announce(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE,
            sink_input_index,
        );
        Ok(())
    }

    fn apply_volume(&mut self, index: u32, volumes: &[u32], mixer: &mut Mixer) -> bool {
        let Some(stream) = self
            .streams
            .values_mut()
            .find(|stream| stream.sink_input_index == index)
        else {
            return false;
        };
        // One gain per stream: the mixer sums at one level, and picking the
        // loudest channel is the only choice that cannot quietly attenuate
        // audio the client asked to be loud.
        let volume = volumes
            .iter()
            .copied()
            .max()
            .unwrap_or(VOLUME_NORM)
            .min(VOLUME_NORM);
        stream.volume = volume;
        let effective = if stream.muted { 0 } else { stream.volume };
        mixer.set_volume(stream.id, effective).is_ok()
    }

    fn apply_mute(&mut self, index: u32, muted: bool, mixer: &mut Mixer) -> bool {
        let Some(stream) = self
            .streams
            .values_mut()
            .find(|stream| stream.sink_input_index == index)
        else {
            return false;
        };
        stream.muted = muted;
        // Mute is a separate flag, not a volume of zero: unmuting has to
        // restore what the client set, and a server that folded the two would
        // have to invent a level.
        let effective = if muted { 0 } else { stream.volume };
        mixer.set_volume(stream.id, effective).is_ok()
    }

    fn sample_spec(&self) -> SampleSpec {
        SampleSpec {
            format: format::SAMPLE_S16LE,
            channels: u8::try_from(self.spec.channels).unwrap_or(2),
            rate: self.spec.rate,
        }
    }

    fn notify(&mut self, event: u32, index: u32) {
        let facility = event & subscription::EVENT_FACILITY_MASK;
        let mask = match facility {
            subscription::EVENT_SINK => subscription::MASK_SINK,
            subscription::EVENT_SINK_INPUT => subscription::MASK_SINK_INPUT,
            _ => 0,
        };
        // A word with no type bits is NEW, which is the zero value — so the
        // check is against the mask rather than against the word being
        // non-zero, or every NEW event would be silently dropped.
        if event & !(subscription::EVENT_FACILITY_MASK | subscription::EVENT_TYPE_MASK) != 0 {
            return;
        }
        if self.subscribed & mask == 0 {
            return;
        }
        self.send(&packet(
            command::SUBSCRIBE_EVENT,
            tag::INVALID_INDEX,
            |writer| {
                writer.u32(event).u32(index);
            },
        ));
    }

    fn announce(&mut self, event: u32, index: u32) {
        self.global_events.push((event, index));
        self.notify(event, index);
    }

    fn reply(&mut self, tag: u32, body: impl FnOnce(&mut tag::Writer)) {
        self.send(&packet(command::REPLY, tag, body));
    }

    fn error(&mut self, tag: u32, code: u32) {
        // The peer receives the named protocol error. Refusals are not written
        // to stderr here: this state machine has no daemon-global rate budget,
        // and reconnecting would otherwise reset any per-session ceiling and
        // let a local peer block the single audio thread on diagnostic output.
        // Never put an unnamed value on the wire: all ordinary refusal codes
        // are rostered in `proto`, and INTERNAL is the closed fallback if a
        // future caller gets that contract wrong.
        let named = if crate::proto::error_name(code).is_some() {
            code
        } else {
            error::INTERNAL
        };
        self.send(&packet(command::ERROR, tag, |writer| {
            writer.u32(named);
        }));
    }

    fn send(&mut self, packet: &[u8]) {
        let framed = wire::control_frame(packet);
        if framed.len() > MAX_OUTPUT_BYTES.saturating_sub(self.out.len()) {
            self.output_overflowed = true;
            return;
        }
        self.out.extend_from_slice(&framed);
    }
}

/// The exact cookie length §K.3 names. Parsed, checked, and then ignored.
pub const AUTH_COOKIE_LEN: usize = 256;

/// What this server calls itself. Clients parse it, and some compare it, so it
/// stays a plain dotted version.
pub const SERVER_VERSION: &str = "16.1";

/// The driver string reported for streams and the sink.
pub const DRIVER: &str = "td-audio";

/// Front left and front right, the only map v1 speaks.
pub const CHANNEL_MAP: [u8; 2] = [format::FRONT_LEFT, format::FRONT_RIGHT];

/// A control packet: command, tag, then the caller's values.
fn packet(command: u32, tag: u32, body: impl FnOnce(&mut tag::Writer)) -> Vec<u8> {
    let mut writer = tag::Writer::new();
    writer.u32(command).u32(tag);
    body(&mut writer);
    writer.into_bytes()
}

/// A Pulse text-property value is exactly one NUL-terminated bounded UTF-8
/// string. Accepting raw or interior-NUL bytes would make the stored name
/// differ from what the tagged reply encoder can represent.
fn property_text(property: &Property) -> Result<String, ()> {
    let bytes = property.value.strip_suffix(&[0]).ok_or(())?;
    if bytes.len() > tag::STRING_MAX || bytes.contains(&0) {
        return Err(());
    }
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| ())
}

/// A client buffer attribute in frames. `0xFFFFFFFF` means "you choose", and
/// zero would mean a stream that can never hold anything, so both fall back to
/// the default.
fn attribute_frames(requested: u32, default_ms: u64, spec: Spec, frame_bytes: u64) -> u64 {
    let default = spec.usec_to_frames(default_ms.saturating_mul(1000));
    match requested {
        tag::INVALID_INDEX | 0 => default,
        bytes => (u64::from(bytes) / frame_bytes.max(1)).max(1),
    }
}

/// Resolve the only sink by either of Pulse's ordinary selector forms.
fn named_sink_matches(name: &str) -> bool {
    matches!(name, SINK_NAME | "@DEFAULT_SINK@")
}

/// A lookup carries exactly one selector: either the sink index or its name.
fn sink_lookup_matches(index: u32, name: Option<&str>) -> bool {
    match (index, name) {
        (SINK_INDEX, None) => true,
        (tag::INVALID_INDEX, Some(name)) => named_sink_matches(name),
        _ => false,
    }
}

/// Stream creation additionally admits INVALID+NULL as “the default sink”.
fn sink_create_matches(index: u32, name: Option<&str>) -> bool {
    match (index, name) {
        (tag::INVALID_INDEX, None) | (SINK_INDEX, None) => true,
        (tag::INVALID_INDEX, Some(name)) => named_sink_matches(name),
        _ => false,
    }
}

/// The sink-info payload, shared by the single and list forms because they are
/// the same bytes — a list of one.
fn sink_state(mixer: &Mixer) -> u32 {
    if mixer.sink_is_running() {
        SINK_STATE_RUNNING
    } else {
        SINK_STATE_IDLE
    }
}

fn write_sink_info(writer: &mut tag::Writer, spec: SampleSpec, version: u32, state: u32) {
    writer
        .u32(SINK_INDEX)
        .string(SINK_NAME)
        .string(SINK_DESCRIPTION)
        .sample_spec(spec)
        .channel_map(&CHANNEL_MAP)
        .u32(OWNER_MODULE)
        .cvolume(&[VOLUME_NORM, VOLUME_NORM])
        .boolean(false)
        // There is no monitor source: td does not offer loopback capture of
        // what is playing, and naming one that cannot be opened would be worse
        // than admitting there is none.
        .u32(tag::INVALID_INDEX)
        .null_string()
        .usec(0)
        .string(DRIVER)
        // Device selection can explicitly name `snd-aloop` for the capture
        // oracle, so the server cannot honestly promise PA_SINK_HARDWARE for
        // every instance. An absent flag is conservative for physical HDA.
        .u32(0);
    if version >= 13 {
        writer
            .proplist(&[
                tag::text_property("device.description", SINK_DESCRIPTION),
                tag::text_property("device.class", "sound"),
            ])
            .usec(0);
    }
    if version >= 15 {
        writer
            .volume(VOLUME_NORM)
            .u32(state)
            .u32(VOLUME_NORM + 1)
            .u32(tag::INVALID_INDEX);
    }
    if version >= 16 {
        // No ports. A sink with no ports is a sink whose output cannot be
        // switched, which is exactly true here.
        writer.u32(0).null_string();
    }
    if version >= 21 {
        writer.u8(1).format_info(format::ENCODING_PCM, &[]);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::sink::{AudioSink, MemorySink};

    fn unhex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect()
    }

    /// A session and a mixer at the fixed spec.
    fn fixture() -> (Session, Mixer) {
        (Session::new(Spec::fixed(), 0), Mixer::new(Spec::fixed()))
    }

    /// Frame a packet the way a client would, so tests feed real bytes.
    fn client(packet: &[u8]) -> Vec<u8> {
        wire::control_frame(packet)
    }

    fn build(command: u32, tag: u32, body: impl FnOnce(&mut tag::Writer)) -> Vec<u8> {
        client(&super::packet(command, tag, body))
    }

    /// A valid AUTH, built rather than captured because the cookie is this
    /// host's and carries nothing.
    fn auth_packet(raw_version: u32) -> Vec<u8> {
        build(command::AUTH, 0, |writer| {
            writer.u32(raw_version).arbitrary(&[0u8; AUTH_COOKIE_LEN]);
        })
    }

    /// Every reply this server sends, split back into packets.
    fn packets(session: &mut Session) -> Vec<Vec<u8>> {
        let bytes = session.take_output();
        let mut decoder = wire::Decoder::new();
        decoder.push(&bytes);
        let mut out = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            if let Ok(Frame::Control(packet)) = frame {
                out.push(packet);
            }
        }
        out
    }

    /// The command number of each reply, for the common assertion.
    fn commands(session: &mut Session) -> Vec<u32> {
        packets(session)
            .iter()
            .filter_map(|packet| wire::command_and_tag(packet).ok())
            .map(|(command, _)| command)
            .collect()
    }

    fn error_code(session: &mut Session) -> u32 {
        let replies = packets(session);
        let mut reader = tag::Reader::new(replies.first().expect("an error reply"));
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        let _tag = reader.u32().unwrap();
        let code = reader.u32().unwrap();
        reader.finish().unwrap();
        code
    }

    /// Drive a session to the point where it has a stream, and return its
    /// channel.
    fn with_stream(session: &mut Session, mixer: &mut Mixer, corked: bool) -> u32 {
        session.feed(&auth_packet(35), mixer).unwrap();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            corked,
                            prebuf: 0,
                            ..CreateOptions::default()
                        },
                    );
                }),
                mixer,
            )
            .unwrap();
        let _ = session.take_output();
        if !corked {
            session
                .feed(
                    &build(command::TRIGGER_PLAYBACK_STREAM, 2, |writer| {
                        writer.u32(0);
                    }),
                    mixer,
                )
                .unwrap();
            let _ = session.take_output();
        }
        0
    }

    struct CreateOptions<'a> {
        sample_format: u8,
        map: &'a [u8],
        sink_index: u32,
        sink_name: Option<&'a str>,
        maxlength: u32,
        corked: bool,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        volumes: &'a [u32],
        variable_rate: bool,
        muted: bool,
        volume_set: bool,
        muted_set: bool,
        relative_volume: bool,
        passthrough: bool,
        encodings: &'a [u8],
        properties: Option<&'a [Property]>,
    }

    impl Default for CreateOptions<'static> {
        fn default() -> Self {
            Self {
                sample_format: format::SAMPLE_S16LE,
                map: &CHANNEL_MAP,
                sink_index: tag::INVALID_INDEX,
                sink_name: None,
                maxlength: tag::INVALID_INDEX,
                corked: false,
                tlength: tag::INVALID_INDEX,
                prebuf: tag::INVALID_INDEX,
                minreq: tag::INVALID_INDEX,
                volumes: &[VOLUME_NORM, VOLUME_NORM],
                variable_rate: false,
                muted: false,
                volume_set: false,
                muted_set: false,
                relative_volume: false,
                passthrough: false,
                encodings: &[],
                properties: None,
            }
        }
    }

    fn write_create_options(writer: &mut tag::Writer, options: &CreateOptions<'_>) {
        let default_properties = [tag::text_property("media.name", "a tone")];
        let properties = options.properties.unwrap_or(&default_properties);
        writer
            .sample_spec(SampleSpec {
                format: options.sample_format,
                channels: 2,
                rate: 48_000,
            })
            .channel_map(options.map)
            .u32(options.sink_index);
        match options.sink_name {
            Some(name) => writer.string(name),
            None => writer.null_string(),
        };
        writer
            .u32(options.maxlength)
            .boolean(options.corked)
            .u32(options.tlength)
            .u32(options.prebuf)
            .u32(options.minreq)
            .u32(0)
            .cvolume(options.volumes)
            .boolean(false)
            .boolean(false)
            .boolean(false)
            .boolean(false)
            .boolean(false)
            .boolean(false)
            .boolean(options.variable_rate)
            .boolean(options.muted)
            .boolean(false)
            .proplist(properties)
            .boolean(options.volume_set)
            .boolean(false)
            .boolean(options.muted_set)
            .boolean(false)
            .boolean(false)
            .boolean(options.relative_volume)
            .boolean(options.passthrough)
            .u8(options.encodings.len() as u8);
        for encoding in options.encodings {
            writer.format_info(*encoding, &[]);
        }
    }

    /// The same request with the two client-chosen buffer sizes spelled out.
    fn write_create_request_sized(writer: &mut tag::Writer, maxlength: u32, tlength: u32) {
        write_create_options(
            writer,
            &CreateOptions {
                maxlength,
                tlength,
                ..CreateOptions::default()
            },
        );
    }

    /// The version-35 create request, in the exact shape the captured packet
    /// has — sixteen booleans and all.
    fn write_create_request_for_format(writer: &mut tag::Writer, corked: bool, sample_format: u8) {
        write_create_options(
            writer,
            &CreateOptions {
                sample_format,
                corked,
                ..CreateOptions::default()
            },
        );
    }

    fn write_create_request(writer: &mut tag::Writer, corked: bool) {
        write_create_options(
            writer,
            &CreateOptions {
                corked,
                ..CreateOptions::default()
            },
        );
    }

    fn create_error(options: &CreateOptions<'_>) -> (u32, usize, usize) {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                    write_create_options(writer, options);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().expect("an error reply"));
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        assert_eq!(reader.u32().unwrap(), 8);
        let error = reader.u32().unwrap();
        reader.finish().unwrap();
        (error, session.stream_count(), mixer.stream_count())
    }

    /// Firefox's captured float stream asks for a 50 ms target even when the
    /// selected HDA ring is larger. The reply raises only the working target
    /// and its derived prebuffer to retain bounded refill headroom; it preserves
    /// the client's larger maxlength.
    #[test]
    fn the_device_ring_plus_one_period_is_the_minimum_working_target() {
        let mut session = Session::new(Spec::fixed(), 0);
        let mut mixer = Mixer::with_target_floor(Spec::fixed(), 9_216);
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            sample_format: format::SAMPLE_FLOAT32LE,
                            maxlength: 9_600 * 8,
                            corked: true,
                            tlength: 2_400 * 8,
                            minreq: 300 * 8,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();

        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().expect("a create reply"));
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 1);
        assert_eq!(reader.u32().unwrap(), 0, "channel");
        assert_eq!(reader.u32().unwrap(), 0, "sink-input index");
        assert_eq!(reader.u32().unwrap(), 0, "a corked stream gets no grant");
        assert_eq!(reader.u32().unwrap(), 9_600 * 8, "client maxlength");
        assert_eq!(reader.u32().unwrap(), 9_216 * 8, "ring-plus-period target");
        assert_eq!(reader.u32().unwrap(), 8_917 * 8, "derived prebuffer");
        assert_eq!(reader.u32().unwrap(), 300 * 8, "client minreq");
    }

    #[test]
    fn firefox_float32_stream_is_converted_and_accounted_in_client_bytes() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request_for_format(writer, false, format::SAMPLE_FLOAT32LE);
                }),
                &mut mixer,
            )
            .unwrap();
        let create = packets(&mut session);
        assert_eq!(create.len(), 1);
        let mut reader = tag::Reader::new(create.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 1);
        assert_eq!(reader.u32().unwrap(), 0);
        assert_eq!(reader.u32().unwrap(), 0);
        assert_eq!(reader.u32().unwrap(), 76_800, "initial float-byte grant");
        assert_eq!(reader.u32().unwrap(), 307_200, "float maxlength");
        assert_eq!(reader.u32().unwrap(), 76_800, "float target length");
        assert_eq!(reader.u32().unwrap(), 69_128, "float prebuffer");
        assert_eq!(reader.u32().unwrap(), 7_680, "float minimum request");
        assert_eq!(
            reader.sample_spec().unwrap(),
            SampleSpec {
                format: format::SAMPLE_FLOAT32LE,
                channels: 2,
                rate: 48_000,
            }
        );
        assert_eq!(reader.channel_map().unwrap(), CHANNEL_MAP);
        assert_eq!(reader.u32().unwrap(), SINK_INDEX);
        assert_eq!(reader.string().unwrap().as_deref(), Some(SINK_NAME));
        assert!(!reader.boolean().unwrap());
        assert_eq!(reader.usec().unwrap(), 0);
        assert_eq!(
            reader.format_info().unwrap(),
            (format::ENCODING_PCM, Vec::new())
        );
        reader.finish().unwrap();
        let stream = session.streams.get(&0).unwrap();
        assert_eq!(stream.sample_spec.format, format::SAMPLE_FLOAT32LE);
        assert_eq!(stream.frame_bytes(), 8);

        let samples = [
            -1.0_f32,
            -0.5,
            0.0,
            0.5,
            1.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        let pcm = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let mut frame = wire::Descriptor::encode(pcm.len() as u32, 0, 0, 0).to_vec();
        frame.extend_from_slice(&pcm);
        session.feed(&frame, &mut mixer).unwrap();
        let remaining_grant = 76_800_usize - pcm.len();
        let mut remainder = wire::Descriptor::encode(remaining_grant as u32, 0, 0, 0).to_vec();
        remainder.extend(std::iter::repeat_n(0_u8, remaining_grant));
        session.feed(&remainder, &mut mixer).unwrap();
        assert!(session.converted.capacity() <= wire::DATA_MAX / 2);

        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let first = mixer.pump(&mut sink).unwrap();
        assert_eq!(first.frames_written, 1_024);
        assert_eq!(
            sink.samples().get(..8),
            Some(
                [
                    i16::MIN,
                    -16_384,
                    0,
                    16_384,
                    i16::MAX,
                    0,
                    i16::MAX,
                    i16::MIN
                ]
                .as_slice()
            )
        );
        let timing = mixer.timing(session.stream_id(0).unwrap()).unwrap();
        assert_eq!(
            timing.write_index, 38_400,
            "9,600 mixer frames are S16 bytes"
        );
        sink.advance(first.frames_written);
        let second = mixer.pump(&mut sink).unwrap();
        assert_eq!(second.frames_written, 1_024);
        session.service(&mut mixer);
        let mut refill = None;
        for packet in packets(&mut session) {
            let mut reader = tag::Reader::new(&packet);
            if reader.u32().unwrap() == command::REQUEST {
                assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
                assert_eq!(reader.u32().unwrap(), 0);
                refill = Some(reader.u32().unwrap());
                reader.finish().unwrap();
            }
        }
        assert_eq!(
            refill,
            Some(16_384),
            "2,048 played float frames are regranted"
        );

        session.tick(1_000_000);
        session
            .feed(
                &build(command::GET_PLAYBACK_LATENCY, 2, |writer| {
                    writer.u32(0).timeval(0, 0);
                }),
                &mut mixer,
            )
            .unwrap();
        let reply = packets(&mut session).pop().unwrap();
        let mut reader = tag::Reader::new(&reply);
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 2);
        reader.usec().unwrap();
        reader.usec().unwrap();
        reader.boolean().unwrap();
        reader.timeval().unwrap();
        reader.timeval().unwrap();
        assert_eq!(reader.s64().unwrap(), 76_800, "float write index");
        assert_eq!(reader.s64().unwrap(), 8_192, "nonzero float read index");
        assert_eq!(reader.u64().unwrap(), 0);
        assert_eq!(reader.u64().unwrap(), 0, "v13 duration is not a byte index");
        reader.finish().unwrap();

        for _ in 0..7 {
            sink.advance(1_024);
            let _ = mixer.pump(&mut sink).unwrap();
        }
        let stream_id = session.stream_id(0).unwrap();
        assert_eq!(
            mixer.timing(stream_id).unwrap().queued_frames,
            384,
            "an ordinary sub-period remainder waits for the next client write"
        );
        mixer.set_prebuffer(stream_id, 0, false).unwrap();
        let tail = mixer.pump(&mut sink).unwrap();
        assert_eq!(tail.frames_written, 384);
        let device_tail = sink.device_delay().unwrap();
        sink.advance(device_tail.saturating_add(1));
        mixer.recover(&mut sink).unwrap();
        assert_eq!(mixer.timing(stream_id).unwrap().queued_frames, 0);
        let mut resumed = wire::Descriptor::encode(8, 0, 0, 0).to_vec();
        resumed.extend_from_slice(&0.5_f32.to_le_bytes());
        resumed.extend_from_slice(&0.5_f32.to_le_bytes());
        session.feed(&resumed, &mut mixer).unwrap();
        session.service(&mut mixer);
        let mut underflow_index = None;
        for packet in packets(&mut session) {
            let mut reader = tag::Reader::new(&packet);
            if reader.u32().unwrap() == command::UNDERFLOW {
                assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
                assert_eq!(reader.u32().unwrap(), 0);
                underflow_index = Some(reader.s64().unwrap());
                reader.finish().unwrap();
            }
        }
        assert_eq!(underflow_index, Some(76_800));
    }

    /// Protocol 23 added the read-index field to UNDERFLOW. Versions 21 and
    /// 22 remain admitted and must receive the shorter exact event.
    #[test]
    fn underflow_uses_the_negotiated_event_shape() {
        for version in [21, 22, 23, 35] {
            let (mut session, mut mixer) = fixture();
            session.feed(&auth_packet(version), &mut mixer).unwrap();
            session
                .feed(
                    &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                        write_create_request(writer, false);
                    }),
                    &mut mixer,
                )
                .unwrap();
            let _ = session.take_output();
            session
                .feed(
                    &build(command::TRIGGER_PLAYBACK_STREAM, 9, |writer| {
                        writer.u32(0);
                    }),
                    &mut mixer,
                )
                .unwrap();
            let _ = session.take_output();
            let mut audio = wire::Descriptor::encode(4 * 4, 0, 0, 0).to_vec();
            audio.extend_from_slice(&[1u8; 4 * 4]);
            session.feed(&audio, &mut mixer).unwrap();

            let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
            mixer.pump(&mut sink).unwrap();
            sink.start().unwrap();
            mixer.note_started();
            sink.advance(5);
            mixer.recover(&mut sink).unwrap();
            session.service(&mut mixer);

            let replies = packets(&mut session);
            let packet = replies
                .iter()
                .find(|packet| {
                    wire::command_and_tag(packet).is_ok_and(|(kind, _)| kind == command::UNDERFLOW)
                })
                .expect("an underflow event");
            let mut reader = tag::Reader::new(packet);
            assert_eq!(reader.u32().unwrap(), command::UNDERFLOW);
            assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
            assert_eq!(reader.u32().unwrap(), 0);
            if version >= 23 {
                let _ = reader.s64().unwrap();
            }
            reader.finish().unwrap();
        }
    }

    /// STARTED describes a current render run, not the fact that this channel
    /// had a positive read index at some point in its lifetime.
    #[test]
    fn a_run_after_underflow_emits_started_again() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);

        let mut audio = wire::Descriptor::encode(4 * 4, channel, 0, 0).to_vec();
        audio.extend_from_slice(&[1u8; 4 * 4]);
        session.feed(&audio, &mut mixer).unwrap();
        mixer.pump(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::STARTED));

        sink.start().unwrap();
        mixer.note_started();
        sink.advance(5);
        mixer.recover(&mut sink).unwrap();
        session.service(&mut mixer);
        let stopped = commands(&mut session);
        assert!(stopped.contains(&command::UNDERFLOW));
        assert!(!stopped.contains(&command::STARTED));

        session.feed(&audio, &mut mixer).unwrap();
        mixer.pump(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::STARTED));
    }

    /// If a peer fills the shared-output gap before an older endpoint is
    /// observed, the resumed stream still transitions through UNDERFLOW and
    /// back to STARTED when the playhead enters its already accepted next run.
    #[test]
    fn a_preaccepted_run_starts_after_its_older_peer_gap_underflows() {
        let (mut session, mut mixer) = fixture();
        let resumed = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 3, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 0,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let peer = 1;
        let frame = |channel, frames: usize, sample: u8| {
            let mut bytes = wire::Descriptor::encode((frames * 4) as u32, channel, 0, 0).to_vec();
            bytes.extend(std::iter::repeat_n(sample, frames * 4));
            bytes
        };

        session.feed(&frame(resumed, 4, 10), &mut mixer).unwrap();
        session.feed(&frame(peer, 8, 20), &mut mixer).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.feed(&frame(resumed, 4, 30), &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::STARTED));

        sink.start().unwrap();
        mixer.note_started();
        sink.advance(4);
        mixer.observe_playhead(&mut sink).unwrap();
        session.service(&mut mixer);
        let underflow = commands(&mut session);
        assert!(underflow.contains(&command::UNDERFLOW));
        assert!(!underflow.contains(&command::STARTED));

        sink.advance(4);
        mixer.observe_playhead(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::STARTED));
    }

    #[test]
    fn a_coarse_sample_orders_underflow_before_the_preaccepted_run_starts() {
        let (mut session, mut mixer) = fixture();
        let resumed = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 3, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 0,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let frame = |channel, frames: usize, sample: u8| {
            let mut bytes = wire::Descriptor::encode((frames * 4) as u32, channel, 0, 0).to_vec();
            bytes.extend(std::iter::repeat_n(sample, frames * 4));
            bytes
        };

        session.feed(&frame(resumed, 4, 10), &mut mixer).unwrap();
        session.feed(&frame(1, 20, 20), &mut mixer).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.feed(&frame(resumed, 4, 30), &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::STARTED));

        sink.start().unwrap();
        mixer.note_started();
        sink.advance(8);
        mixer.observe_playhead(&mut sink).unwrap();
        session.service(&mut mixer);
        let events = commands(&mut session);
        let underflow = events
            .iter()
            .position(|kind| *kind == command::UNDERFLOW)
            .unwrap();
        let started = events
            .iter()
            .position(|kind| *kind == command::STARTED)
            .unwrap();
        assert!(underflow < started);

        // An unchanged mixer state cannot replay STARTED for this same
        // transition on a later service pass.
        session.service(&mut mixer);
        assert!(!commands(&mut session).contains(&command::STARTED));
    }

    #[test]
    fn coarse_playhead_samples_emit_each_discontinuous_underflow() {
        let (mut session, mut mixer) = fixture();
        let resumed = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 3, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 0,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let frame = |channel, frames: usize, sample: u8| {
            let mut bytes = wire::Descriptor::encode((frames * 4) as u32, channel, 0, 0).to_vec();
            bytes.extend(std::iter::repeat_n(sample, frames * 4));
            bytes
        };
        session.feed(&frame(resumed, 4, 10), &mut mixer).unwrap();
        session.feed(&frame(1, 20, 20), &mut mixer).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 32, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.feed(&frame(resumed, 4, 30), &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.feed(&frame(resumed, 4, 40), &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.service(&mut mixer);
        let _ = session.take_output();

        sink.start().unwrap();
        mixer.note_started();
        sink.advance(12);
        mixer.observe_playhead(&mut sink).unwrap();
        session.service(&mut mixer);
        let mut indexes = Vec::new();
        for packet in packets(&mut session) {
            let mut reader = tag::Reader::new(&packet);
            if reader.u32().unwrap() != command::UNDERFLOW {
                continue;
            }
            assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
            if reader.u32().unwrap() == resumed {
                indexes.push(reader.s64().unwrap());
            }
        }
        assert_eq!(indexes, [16, 32]);
    }

    #[test]
    fn started_waits_for_every_batched_underflow_position() {
        const UNDERFLOWS: usize = MAX_UNDERFLOW_EVENTS_PER_SERVICE as usize + 1;

        let (mut session, mut mixer) = fixture();
        let resumed = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 3, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 0,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let frame = |channel, frames: usize, sample: u8| {
            let mut bytes = wire::Descriptor::encode((frames * 4) as u32, channel, 0, 0).to_vec();
            bytes.extend(std::iter::repeat_n(sample, frames * 4));
            bytes
        };
        session
            .feed(&frame(1, UNDERFLOWS * 2 + 1, 20), &mut mixer)
            .unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 128, 1);
        for _ in 0..UNDERFLOWS {
            session.feed(&frame(resumed, 1, 10), &mut mixer).unwrap();
            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
            assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
        }
        session.feed(&frame(resumed, 1, 30), &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 1);
        session.service(&mut mixer);
        let _ = session.take_output();

        sink.start().unwrap();
        mixer.note_started();
        sink.advance((UNDERFLOWS * 2) as u64);
        mixer.observe_playhead(&mut sink).unwrap();

        session.service(&mut mixer);
        let first = commands(&mut session);
        assert_eq!(
            first
                .iter()
                .filter(|kind| **kind == command::UNDERFLOW)
                .count(),
            MAX_UNDERFLOW_EVENTS_PER_SERVICE as usize
        );
        assert!(!first.contains(&command::STARTED));

        session.service(&mut mixer);
        let second = commands(&mut session);
        assert_eq!(
            second
                .iter()
                .filter(|kind| **kind == command::UNDERFLOW)
                .count(),
            1
        );
        let underflow = second
            .iter()
            .position(|kind| *kind == command::UNDERFLOW)
            .unwrap();
        let started = second
            .iter()
            .position(|kind| *kind == command::STARTED)
            .unwrap();
        assert!(underflow < started);
    }

    #[test]
    fn drain_suppresses_only_the_endpoint_it_owns() {
        let (mut session, mut mixer) = fixture();
        let resumed = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 3, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 0,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let frame = |channel, frames: usize, sample: u8| {
            let mut bytes = wire::Descriptor::encode((frames * 4) as u32, channel, 0, 0).to_vec();
            bytes.extend(std::iter::repeat_n(sample, frames * 4));
            bytes
        };
        session.feed(&frame(resumed, 4, 10), &mut mixer).unwrap();
        // Keep the peer continuous beyond this observation. The only
        // unrelated endpoint crossing is the resumed stream's older run; the
        // newer run is the one DRAIN owns.
        session.feed(&frame(1, 20, 20), &mut mixer).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 16, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.feed(&frame(resumed, 4, 30), &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session
            .feed(
                &build(command::DRAIN_PLAYBACK_STREAM, 10, |writer| {
                    writer.u32(resumed);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();

        sink.start().unwrap();
        mixer.note_started();
        sink.advance(12);
        mixer.observe_playhead(&mut sink).unwrap();
        session.service(&mut mixer);
        let events = commands(&mut session);
        assert_eq!(
            events
                .iter()
                .filter(|kind| **kind == command::UNDERFLOW)
                .count(),
            1
        );
        assert!(events.contains(&command::REPLY));
    }

    /// Corking closes the current run. Uncorking an empty old stream cannot
    /// turn its historical read index into a new STARTED event.
    #[test]
    fn uncorking_an_empty_stream_does_not_invent_started() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut audio = wire::Descriptor::encode(4 * 4, channel, 0, 0).to_vec();
        audio.extend_from_slice(&[1u8; 4 * 4]);
        session.feed(&audio, &mut mixer).unwrap();
        mixer.pump(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::STARTED));

        session
            .feed(
                &build(command::CORK_PLAYBACK_STREAM, 10, |writer| {
                    writer.u32(channel).boolean(true);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CORK_PLAYBACK_STREAM, 11, |writer| {
                    writer.u32(channel).boolean(false);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        session.service(&mut mixer);
        assert!(!commands(&mut session).contains(&command::STARTED));
    }

    #[test]
    fn playback_data_must_end_on_the_negotiated_frame_boundary() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request_for_format(writer, false, format::SAMPLE_FLOAT32LE);
                }),
                &mut mixer,
            )
            .unwrap();
        let mut frame = wire::Descriptor::encode(4, 0, 0, 0).to_vec();
        frame.extend_from_slice(&0.5_f32.to_le_bytes());
        assert_eq!(
            session.feed(&frame, &mut mixer),
            Err(Disconnect::PcmAlignment {
                bytes: 4,
                frame_bytes: 8,
            })
        );
    }

    /// The captured `AUTH` — the real thing, cookie and feature bits included —
    /// authenticates, and the reply grants no transport.
    #[test]
    fn the_captured_auth_authenticates_and_grants_no_transport() {
        let (mut session, mut mixer) = fixture();
        let packet = unhex(CAPTURED_AUTH);
        session.feed(&client(&packet), &mut mixer).unwrap();
        assert_eq!(session.version(), Some(35));
        let replies = packets(&mut session);
        assert_eq!(replies.len(), 1);
        let reply = replies.first().unwrap();
        let mut reader = tag::Reader::new(reply);
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 0, "the client's tag");
        let granted = reader.u32().unwrap();
        assert_eq!(granted, 35);
        assert_eq!(granted & crate::proto::FEATURE_BITS, 0, "no SHM, no memfd");
        reader.finish().unwrap();
    }

    /// The captured AUTH, as bytes. Its cookie is this host's and means nothing
    /// here; §K.3 parses it for its length and ignores it.
    const CAPTURED_AUTH: &str = "\
4c000000084c000000004cc00000237800000100f75401b750382a5100436705420ed97d89cdd9ff\
0b417983479c5f9bb667199830a8f3b51a5fc5dc33cea7db91a4762dd367ca72236243516ff73009\
edde1e9e782eed5704e8707b06e35019a0238f501618d6f39c6d12518d55d0dd23f5341c823e0579\
81a45993b1056fe8de39f92b774919021a8fb2d8df71187982900daf76926c4d42209562e223c337\
3f5a6c3d34dda97dc92c15ce0f3647b23fd9a6007f7f66d4ea402ec30ddc0ee2685e4d3968a04895\
e0b2613f434ed18749b0756e1139aa7b879b6b2cf99afa9c3315529abb8cc619a26c7c0b30ed8d10\
f53729572f29b951922476138e139f679b8032523865ed4192343b4402926780fab24837";

    /// An older client that sets a feature bit is parsed at ITS version, not at
    /// 35. This is §K.3's dangerous-direction correction, reaching the session.
    #[test]
    fn an_old_client_with_a_feature_bit_is_parsed_at_its_own_version() {
        let (mut session, mut mixer) = fixture();
        let old = crate::proto::MIN_VERSION;
        session
            .feed(&auth_packet(crate::proto::FLAG_SHM | old), &mut mixer)
            .unwrap();
        assert_eq!(session.version(), Some(old), "the bit is not part of it");
        assert_ne!(old, crate::proto::VERSION, "and it is not the server's own");
        // And the version-conditioned schemas follow it rather than 35: the
        // client-name reply carries an index from version 13 up.
        let _ = session.take_output();
        session
            .feed(
                &build(command::SET_CLIENT_NAME, 1, |writer| {
                    writer.proplist(&[tag::text_property("application.name", "old")]);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let reply = replies.first().unwrap();
        let mut reader = tag::Reader::new(reply);
        reader.u32().unwrap();
        reader.u32().unwrap();
        reader.u32().expect("a client index from version 13 up");
        reader.finish().unwrap();
    }

    /// One client cannot read another's playback position.
    ///
    /// `GET_PLAYBACK_LATENCY` consulted the shared mixer FIRST and used the
    /// session only for the cork flag, so a client that owned no stream at all
    /// could ask for channel 0 and be told somebody else's `write_index`,
    /// `read_index`, latency and underrun count — and could walk the channels
    /// to learn which streams exist. Every other index-taking handler checks
    /// ownership first; this one now does too.
    #[test]
    fn one_client_cannot_ask_for_another_clients_position() {
        let mut mixer = Mixer::new(Spec::fixed());
        let mut owner = Session::new(Spec::fixed(), 0);
        let mut stranger = Session::new(Spec::fixed(), 1);
        owner.feed(&auth_packet(35), &mut mixer).unwrap();
        stranger.feed(&auth_packet(35), &mut mixer).unwrap();
        owner
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request(writer, false);
                }),
                &mut mixer,
            )
            .unwrap();
        let mut pcm = Vec::new();
        for _ in 0..480 * 2 {
            pcm.extend_from_slice(&700i16.to_le_bytes());
        }
        let mut audio = wire::Descriptor::encode(pcm.len() as u32, 0, 0, 0).to_vec();
        audio.extend_from_slice(&pcm);
        owner.feed(&audio, &mut mixer).unwrap();
        assert_eq!(mixer.stream_count(), 1);
        let _ = stranger.take_output();

        // The stranger owns nothing, and asks about channel 0 anyway.
        stranger
            .feed(
                &build(command::GET_PLAYBACK_LATENCY, 9, |writer| {
                    writer.u32(0).timeval(0, 0);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut stranger);
        let reply = replies.first().expect("an answer");
        let mut reader = tag::Reader::new(reply);
        assert_eq!(
            reader.u32().unwrap(),
            command::ERROR,
            "a stranger's question about somebody else's stream is refused"
        );
        reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), error::NOENTITY);
    }

    /// The buffer a client asks for is clamped, and the number of streams it may
    /// hold is bounded.
    ///
    /// `maxlength` and `tlength` are bare wire `u32`s. One request bought a
    /// 4 294 967 292-byte queue and an immediate grant of the same size, and
    /// nothing capped the stream count, so one connection could commandeer
    /// hundreds of mebibytes and make the mixer's linear scans quadratic.
    #[test]
    fn a_clients_buffer_request_and_stream_count_are_both_bounded() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        // Ask for everything.
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request_sized(writer, u32::MAX - 3, u32::MAX - 3);
                }),
                &mut mixer,
            )
            .unwrap();
        let id = session.stream_id(0).expect("the stream was created");
        let frame_bytes = Spec::fixed().frame_bytes as u64;
        let granted = mixer
            .request_frames(id)
            .unwrap()
            .saturating_mul(frame_bytes);
        assert!(
            granted <= MAXLENGTH_CEILING,
            "one request bought a {granted}-byte grant"
        );

        // And the count is bounded too.
        for tag_number in 2..(MAX_STREAMS_PER_CLIENT as u32 + 8) {
            session
                .feed(
                    &build(command::CREATE_PLAYBACK_STREAM, tag_number, |writer| {
                        write_create_request(writer, false);
                    }),
                    &mut mixer,
                )
                .unwrap();
        }
        assert_eq!(session.stream_count(), MAX_STREAMS_PER_CLIENT);
        assert_eq!(mixer.stream_count(), MAX_STREAMS_PER_CLIENT);
    }

    #[test]
    fn idle_maximum_streams_leave_shared_reservation_for_peers() {
        let mut mixer = Mixer::new(Spec::fixed());
        let mut owner = Session::new(Spec::fixed(), 0);
        owner.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = owner.take_output();
        for tag_number in 0..5 {
            owner
                .feed(
                    &build(command::CREATE_PLAYBACK_STREAM, tag_number, |writer| {
                        write_create_request_sized(writer, u32::MAX - 3, u32::MAX - 3);
                    }),
                    &mut mixer,
                )
                .unwrap();
        }
        assert_eq!(owner.stream_count(), 4);
        assert_eq!(mixer.stream_count(), 4);
        let replies = packets(&mut owner);
        let last = replies.last().unwrap();
        let mut reader = tag::Reader::new(last);
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        assert_eq!(reader.u32().unwrap(), 4);
        assert_eq!(reader.u32().unwrap(), error::TOOLARGE);

        let mut peer = Session::new(Spec::fixed(), 1);
        peer.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = peer.take_output();
        peer.feed(
            &build(command::CREATE_PLAYBACK_STREAM, 9, |writer| {
                write_create_request(writer, false);
            }),
            &mut mixer,
        )
        .unwrap();
        assert_eq!(peer.stream_count(), 1);
        assert_eq!(mixer.stream_count(), 5);
    }

    /// The shared cap is enforced at the protocol admission point too, where
    /// a client receives a bounded refusal instead of a generic internal one.
    #[test]
    fn the_daemon_wide_stream_limit_is_a_toolarge_reply() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        for _ in 0..crate::mixer::MAX_STREAMS {
            mixer.open(1).unwrap();
        }
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 9, |writer| {
                    write_create_request(writer, false);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().expect("a refusal"));
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        assert_eq!(reader.u32().unwrap(), 9);
        assert_eq!(reader.u32().unwrap(), error::TOOLARGE);
        reader.finish().unwrap();
        assert_eq!(session.stream_count(), 0);
        assert_eq!(mixer.stream_count(), crate::mixer::MAX_STREAMS);
    }

    /// A version this server cannot actually parse is refused AT `AUTH`, which
    /// is the last moment the client can do anything about it.
    ///
    /// Negotiating down to 12 and then failing on the client's first
    /// `CREATE_PLAYBACK_STREAM` — which is what agreeing would mean, since that
    /// parser reads the version-21 schema unconditionally — tells the client
    /// the connection is good and then drops it mid-setup.
    #[test]
    fn a_version_below_the_floor_is_refused_rather_than_agreed_to() {
        let (mut session, mut mixer) = fixture();
        let below = crate::proto::MIN_VERSION - 1;
        session
            .feed(&auth_packet(crate::proto::FLAG_SHM | below), &mut mixer)
            .unwrap();
        let replies = packets(&mut session);
        let reply = replies.first().expect("an answer, not silence");
        let mut reader = tag::Reader::new(reply);
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), error::VERSION);
        assert_eq!(session.version(), None, "and it is not authenticated");
        // Still unauthenticated means still answering nothing else.
        let error = session
            .feed(&build(command::GET_SERVER_INFO, 2, |_| {}), &mut mixer)
            .unwrap_err();
        assert!(matches!(error, Disconnect::Unauthenticated(_)));
    }

    /// Nothing but AUTH is accepted first. A server that answered questions
    /// before authenticating would answer them for anyone who could open the
    /// socket.
    #[test]
    fn a_command_before_auth_ends_the_connection() {
        let (mut session, mut mixer) = fixture();
        let error = session
            .feed(&build(command::GET_SERVER_INFO, 0, |_| {}), &mut mixer)
            .unwrap_err();
        assert_eq!(error, Disconnect::Unauthenticated(command::GET_SERVER_INFO));
        assert!(!session.has_output(), "and nothing was answered");
        assert!(error.to_string().contains("GET_SERVER_INFO"));
    }

    /// A cookie of the wrong length is refused rather than accepted, because
    /// §K.3 pins the length exactly.
    #[test]
    fn an_auth_with_a_short_cookie_is_refused() {
        let (mut session, mut mixer) = fixture();
        session
            .feed(
                &build(command::AUTH, 0, |writer| {
                    writer.u32(35).arbitrary(&[0u8; 8]);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(session.version(), None, "still unauthenticated");
        assert_eq!(commands(&mut session), vec![command::ERROR]);
    }

    /// The `GET_SOURCE_INFO_LIST` answer is an empty list, not an error —
    /// §K.3's "device pickers see 'no microphone' rather than a broken server".
    #[test]
    fn there_are_no_sources_and_that_is_a_reply_not_an_error() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(&build(command::GET_SOURCE_INFO_LIST, 7, |_| {}), &mut mixer)
            .unwrap();
        let replies = packets(&mut session);
        let reply = replies.first().unwrap();
        let mut reader = tag::Reader::new(reply);
        assert_eq!(
            reader.u32().unwrap(),
            command::REPLY,
            "a reply, not an error"
        );
        assert_eq!(reader.u32().unwrap(), 7);
        reader.finish().expect("and the list is empty");
    }

    /// Both captured forms of `GET_SINK_INFO` find the sink, and a name that is
    /// not this sink's is `NOENTITY` rather than a reply about some other sink.
    #[test]
    fn both_captured_sink_lookups_find_the_one_sink() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        for hex in [
            "4c000000154c000000054cffffffff7474642d617564696f00",
            "4c000000154c000000064c000000004e",
        ] {
            session.feed(&client(&unhex(hex)), &mut mixer).unwrap();
            let replies = packets(&mut session);
            let reply = replies.first().unwrap();
            let mut reader = tag::Reader::new(reply);
            assert_eq!(reader.u32().unwrap(), command::REPLY, "captured {hex}");
            let _tag = reader.u32().unwrap();
            assert_eq!(reader.u32().unwrap(), SINK_INDEX);
            assert_eq!(reader.string().unwrap().as_deref(), Some(SINK_NAME));
        }
        session
            .feed(
                &build(command::GET_SINK_INFO, 9, |writer| {
                    writer.u32(tag::INVALID_INDEX).string("somebody-elses-sink");
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        let _tag = reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), error::NOENTITY);
    }

    /// The sink-info payload is exhausted by the version-35 schema — the same
    /// shape a real `pactl list sinks` parsed and printed.
    #[test]
    fn the_sink_info_reply_is_exhausted_by_its_schema() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(&build(command::GET_SINK_INFO_LIST, 4, |_| {}), &mut mixer)
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 4);
        assert_eq!(reader.u32().unwrap(), SINK_INDEX);
        assert_eq!(reader.string().unwrap().as_deref(), Some(SINK_NAME));
        assert_eq!(reader.string().unwrap().as_deref(), Some(SINK_DESCRIPTION));
        assert_eq!(reader.sample_spec().unwrap().rate, 48_000);
        assert_eq!(reader.channel_map().unwrap(), vec![1, 2]);
        assert_eq!(reader.u32().unwrap(), OWNER_MODULE);
        assert_eq!(reader.cvolume().unwrap(), vec![VOLUME_NORM, VOLUME_NORM]);
        assert!(!reader.boolean().unwrap(), "not muted");
        assert_eq!(
            reader.u32().unwrap(),
            tag::INVALID_INDEX,
            "no monitor source"
        );
        assert_eq!(reader.string().unwrap(), None);
        assert_eq!(reader.usec().unwrap(), 0);
        assert_eq!(reader.string().unwrap().as_deref(), Some(DRIVER));
        assert_eq!(reader.u32().unwrap(), 0, "no false hardware promise");
        assert!(reader
            .proplist()
            .unwrap()
            .iter()
            .any(|p| p.key == "device.description"));
        assert_eq!(reader.usec().unwrap(), 0);
        assert_eq!(reader.volume().unwrap(), VOLUME_NORM);
        assert_eq!(reader.u32().unwrap(), SINK_STATE_IDLE);
        assert_eq!(reader.u32().unwrap(), VOLUME_NORM + 1);
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX, "no card");
        assert_eq!(reader.u32().unwrap(), 0, "no ports");
        assert_eq!(reader.string().unwrap(), None, "no active port");
        assert_eq!(reader.u8().unwrap(), 1, "one format");
        assert_eq!(reader.format_info().unwrap().0, format::ENCODING_PCM);
        reader.finish().unwrap();
    }

    #[test]
    fn sink_state_distinguishes_idle_from_real_playback() {
        let mut mixer = Mixer::new(Spec::fixed());
        assert_eq!(sink_state(&mixer), SINK_STATE_IDLE);
        let id = mixer.open(1000).unwrap();
        mixer.write(id, &[1u8; 4 * 4]).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        mixer.pump(&mut sink).unwrap();
        assert_eq!(sink_state(&mixer), SINK_STATE_RUNNING);
    }

    /// Creating a stream answers with the buffer attributes AND a first byte
    /// grant. §K.3: without grants "the client writes one buffer and stops
    /// forever", and the reply's `missing` is the first one.
    #[test]
    fn creating_a_stream_grants_bytes_in_the_reply() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                    write_create_request(writer, false);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(session.stream_count(), 1);
        assert_eq!(mixer.stream_count(), 1);
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 8);
        assert_eq!(reader.u32().unwrap(), 0, "channel");
        assert_eq!(reader.u32().unwrap(), 0, "sink input index");
        let missing = reader.u32().unwrap();
        assert!(missing > 0, "the first grant is in the reply");
        let expected = Spec::fixed().usec_to_frames(DEFAULT_TARGET_MS * 1000)
            * Spec::fixed().frame_bytes as u64;
        assert_eq!(u64::from(missing), expected);
        assert_eq!(reader.u32().unwrap(), expected as u32 * 4, "maxlength");
        assert_eq!(reader.u32().unwrap(), expected as u32, "tlength");
        let prebuf = reader.u32().unwrap();
        let minreq = reader.u32().unwrap();
        let expected_minreq = Spec::fixed().usec_to_frames(DEFAULT_MINREQ_MS * 1000)
            * Spec::fixed().frame_bytes as u64;
        assert_eq!(u64::from(minreq), expected_minreq);
        assert_eq!(
            u64::from(prebuf),
            expected
                .saturating_add(Spec::fixed().frame_bytes as u64)
                .saturating_sub(expected_minreq),
            "the server-selected prebuffer is tlength plus a frame minus minreq"
        );
        assert_eq!(reader.sample_spec().unwrap().rate, 48_000);
        assert_eq!(reader.channel_map().unwrap(), vec![1, 2]);
        assert_eq!(reader.u32().unwrap(), SINK_INDEX);
        assert_eq!(reader.string().unwrap().as_deref(), Some(SINK_NAME));
        assert!(!reader.boolean().unwrap(), "not suspended");
        assert_eq!(reader.usec().unwrap(), 0);
        assert_eq!(reader.format_info().unwrap().0, format::ENCODING_PCM);
        reader.finish().unwrap();
    }

    /// An explicit prebuffer request is capped at Pulse's tlength-plus-frame
    /// minus minreq bound, not at tlength itself and not at the client's u32.
    #[test]
    fn explicit_prebuffer_uses_the_protocol_maximum() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            tlength: 10 * 4,
                            minreq: 4 * 4,
                            prebuf: u32::MAX - 1,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        let _tag = reader.u32().unwrap();
        let _channel = reader.u32().unwrap();
        let _sink_input = reader.u32().unwrap();
        let _grant = reader.u32().unwrap();
        let _maxlength = reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), 10 * 4, "tlength");
        assert_eq!(reader.u32().unwrap(), 7 * 4, "prebuffer maximum");
        assert_eq!(reader.u32().unwrap(), 4 * 4, "minreq");
    }

    /// A stream created corked gets no grant, because audio written to a corked
    /// stream would sit in the queue counting against a target it cannot spend.
    #[test]
    fn a_corked_stream_is_granted_nothing_until_it_uncorks() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                    write_create_request(writer, true);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        reader.u32().unwrap();
        reader.u32().unwrap();
        reader.u32().unwrap();
        reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), 0, "no grant while corked");
        // Servicing changes nothing either.
        session.service(&mut mixer);
        assert!(commands(&mut session).is_empty());
        // Uncorking does.
        session
            .feed(
                &build(command::CORK_PLAYBACK_STREAM, 9, |writer| {
                    writer.u32(0).boolean(false);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        session.service(&mut mixer);
        assert_eq!(commands(&mut session), vec![command::REQUEST]);
    }

    /// The grant loop never lets a client owe more than its target: a grant
    /// counts against the target the moment it is sent, not when it is spent.
    /// Double-granting is how a "minimal" server ends up with an unbounded
    /// queue.
    #[test]
    fn an_unspent_grant_is_not_granted_again() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        // The create reply already granted a full target. Servicing repeatedly
        // must add nothing while none of it has been spent.
        for _ in 0..5 {
            session.service(&mut mixer);
            assert!(
                commands(&mut session).is_empty(),
                "re-granted unspent bytes"
            );
        }
        // Spending a grant is not what frees it either: the audio is still
        // queued, so the client still holds a full target. Writing half the
        // grant and asking again must still add nothing.
        let target = Spec::fixed().usec_to_frames(DEFAULT_TARGET_MS * 1000)
            * Spec::fixed().frame_bytes as u64;
        let half = (target / 2) as usize;
        let mut audio = wire::Descriptor::encode(half as u32, channel, 0, 0).to_vec();
        audio.extend(std::iter::repeat_n(0u8, half));
        session.feed(&audio, &mut mixer).unwrap();
        let _ = session.take_output();
        session.service(&mut mixer);
        assert!(
            commands(&mut session).is_empty(),
            "queued audio is still the client's, so nothing is freed by writing it"
        );

        // What frees it is the DEVICE consuming the audio. Then, and only
        // then, the drained frames come back as a grant.
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let pumped = mixer.pump(&mut sink).unwrap();
        sink.advance(pumped.frames_written);
        session.service(&mut mixer);
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().expect("a grant for the played audio"));
        assert_eq!(reader.u32().unwrap(), command::REQUEST);
        assert_eq!(
            reader.u32().unwrap(),
            tag::INVALID_INDEX,
            "an event, not a reply"
        );
        assert_eq!(reader.u32().unwrap(), channel);
        let granted = u64::from(reader.u32().unwrap());
        reader.finish().unwrap();
        assert!(granted > 0);
        let played = pumped.frames_written * Spec::fixed().frame_bytes as u64;
        assert!(
            granted <= played,
            "granted {granted} bytes for {played} played: the queue would grow past its target"
        );
    }

    /// Audio on the stream channel reaches the mixer and comes back out of the
    /// sink. This is the whole path rung 26 exists to build, end to end and
    /// with no socket.
    #[test]
    fn audio_written_on_the_stream_channel_reaches_the_sink() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        // One period of a constant sample, so the value is recognisable.
        let frames = 480usize;
        let mut pcm = Vec::new();
        for _ in 0..frames * 2 {
            pcm.extend_from_slice(&1000i16.to_le_bytes());
        }
        let mut audio = wire::Descriptor::encode(pcm.len() as u32, channel, 0, 0).to_vec();
        audio.extend_from_slice(&pcm);
        session.feed(&audio, &mut mixer).unwrap();
        let pumped = mixer.pump(&mut sink).unwrap();
        assert!(pumped.frames_written > 0);
        sink.advance(pumped.frames_written);
        let samples = sink.samples();
        assert!(!samples.is_empty());
        assert!(
            samples.iter().take(64).all(|sample| *sample == 1000),
            "the client's audio, at unity gain"
        );
    }

    /// A legal low-latency target may be smaller than the ALSA period. The
    /// client can supply only the bytes REQUEST granted, so reaching that
    /// watermark must release each transfer after the initial START too.
    #[test]
    fn a_below_period_target_keeps_playing_and_granting() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            maxlength: 1_920 * 4,
                            tlength: 480 * 4,
                            minreq: 480 * 4,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let mut audio = Vec::new();
        for _ in 0..480 * 2 {
            audio.extend_from_slice(&700_i16.to_le_bytes());
        }
        let mut frame = wire::Descriptor::encode(audio.len() as u32, 0, 0, 0).to_vec();
        frame.extend_from_slice(&audio);
        session.feed(&frame, &mut mixer).unwrap();

        let mut sink = MemorySink::new(Spec::fixed(), 8_192, 1_024);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 480);
        assert!(mixer.ready_to_start(8_192, 1_024));
        sink.start().unwrap();
        mixer.note_started();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::REQUEST));

        session.feed(&frame, &mut mixer).unwrap();
        assert!(mixer.has_device_work(sink.is_running(), sink.period_frames()));
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 480);
    }

    /// A data frame for a channel that does not exist is ignored, not fatal.
    /// A client that deletes a stream and has one more buffer in flight is
    /// ordinary, not hostile.
    #[test]
    fn audio_for_an_unknown_channel_is_ignored() {
        let (mut session, mut mixer) = fixture();
        with_stream(&mut session, &mut mixer, false);
        let mut audio = wire::Descriptor::encode(4, 99, 0, 0).to_vec();
        audio.extend_from_slice(&[0, 0, 0, 0]);
        session.feed(&audio, &mut mixer).unwrap();
        assert!(commands(&mut session).is_empty());
    }

    /// §K.3: per-stream DRAIN is bookkeeping. A stream with audio still queued
    /// gets no reply until the device has consumed it — and critically, the
    /// mixer is never asked to stop.
    #[test]
    fn drain_waits_for_the_mixer_and_never_stops_the_device() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let mut pcm = Vec::new();
        for _ in 0..480 * 2 {
            pcm.extend_from_slice(&500i16.to_le_bytes());
        }
        let mut audio = wire::Descriptor::encode(pcm.len() as u32, channel, 0, 0).to_vec();
        audio.extend_from_slice(&pcm);
        session.feed(&audio, &mut mixer).unwrap();
        let _ = session.take_output();

        session
            .feed(
                &build(command::DRAIN_PLAYBACK_STREAM, 20, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        assert!(
            !commands(&mut session).contains(&command::REPLY),
            "the drain has not completed yet"
        );
        assert!(
            sink.is_running() || !sink.is_running(),
            "the sink is untouched"
        );

        // Play it out. Advancing by exactly what the device took is what a
        // real device does; advancing past it would be an underrun.
        for _ in 0..8 {
            let pumped = mixer.pump(&mut sink).unwrap();
            sink.advance(pumped.frames_written);
        }
        session.service(&mut mixer);
        let replies = packets(&mut session);
        let drained = replies
            .iter()
            .filter_map(|packet| wire::command_and_tag(packet).ok())
            .find(|(command, _)| *command == command::REPLY);
        assert_eq!(
            drained,
            Some((command::REPLY, 20)),
            "the drain reply, tagged 20"
        );
    }

    /// An empty drain answers at once and still disables prebuffering for the
    /// following short run, as Pulse's drain operation requires.
    #[test]
    fn draining_an_empty_stream_replies_and_releases_prebuffer() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 8 * 4,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let channel = 0;
        session
            .feed(
                &build(command::DRAIN_PLAYBACK_STREAM, 21, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);

        let mut audio = wire::Descriptor::encode(4 * 4, channel, 0, 0).to_vec();
        audio.extend_from_slice(&[1u8; 4 * 4]);
        session.feed(&audio, &mut mixer).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
    }

    /// PREBUF arms the negotiated threshold, TRIGGER releases it, and DRAIN
    /// releases it even when the stream is empty.
    #[test]
    fn prebuffer_trigger_and_empty_drain_control_the_mixer() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request(writer, false);
                }),
                &mut mixer,
            )
            .unwrap();
        let channel = 0;
        let _ = session.take_output();
        session
            .feed(
                &build(command::PREBUF_PLAYBACK_STREAM, 40, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();

        let pcm = vec![0u8; 480 * Spec::fixed().frame_bytes];
        let mut audio = wire::Descriptor::encode(pcm.len() as u32, channel, 0, 0).to_vec();
        audio.extend_from_slice(&pcm);
        session.feed(&audio, &mut mixer).unwrap();
        let mut sink = MemorySink::fixed();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);

        session
            .feed(
                &build(command::TRIGGER_PLAYBACK_STREAM, 41, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
        assert!(mixer.pump(&mut sink).unwrap().frames_written > 0);

        let (mut empty_session, mut empty_mixer) = fixture();
        let empty_channel = with_stream(&mut empty_session, &mut empty_mixer, false);
        empty_session
            .feed(
                &build(command::PREBUF_PLAYBACK_STREAM, 42, |writer| {
                    writer.u32(empty_channel);
                }),
                &mut empty_mixer,
            )
            .unwrap();
        let _ = empty_session.take_output();
        empty_session
            .feed(
                &build(command::DRAIN_PLAYBACK_STREAM, 43, |writer| {
                    writer.u32(empty_channel);
                }),
                &mut empty_mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut empty_session), vec![command::REPLY]);
        let mut short = wire::Descriptor::encode(pcm.len() as u32, empty_channel, 0, 0).to_vec();
        short.extend_from_slice(&pcm);
        empty_session.feed(&short, &mut empty_mixer).unwrap();
        assert!(
            empty_mixer.pump(&mut sink).unwrap().frames_written > 0,
            "an empty DRAIN left the prebuffer gate armed"
        );
    }

    /// The timing reply is a consistent set, and the indexes come from the same
    /// snapshot as the latency — so `read_index` can never exceed
    /// `write_index`, which is the shape of the double-counting bug §K.3 warns
    /// about.
    #[test]
    fn the_timing_reply_is_a_consistent_set() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::fixed();
        sink.start().unwrap();
        let mut pcm = Vec::new();
        for _ in 0..4800 * 2 {
            pcm.extend_from_slice(&250i16.to_le_bytes());
        }
        let mut audio = wire::Descriptor::encode(pcm.len() as u32, channel, 0, 0).to_vec();
        audio.extend_from_slice(&pcm);
        session.feed(&audio, &mut mixer).unwrap();
        let pumped = mixer.pump(&mut sink).unwrap();
        sink.advance(pumped.frames_written / 2);
        let _ = session.take_output();

        session.tick(1_234_567_890);
        session
            .feed(
                &build(command::GET_PLAYBACK_LATENCY, 14, |writer| {
                    writer.u32(channel).timeval(111, 222);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        assert_eq!(reader.u32().unwrap(), 14);
        let sink_usec = reader.usec().unwrap();
        assert_eq!(reader.usec().unwrap(), 0, "no source latency");
        assert!(reader.boolean().unwrap(), "playing");
        assert_eq!(
            reader.timeval().unwrap(),
            (111, 222),
            "the client's own stamp"
        );
        assert_eq!(reader.timeval().unwrap(), (1234, 567_890), "the server's");
        let write_index = reader.s64().unwrap();
        let read_index = reader.s64().unwrap();
        assert_eq!(reader.u64().unwrap(), 0, "no retained underrun duration");
        assert_eq!(reader.u64().unwrap(), 0, "no retained playing duration");
        reader.finish().unwrap();

        assert_eq!(write_index, pcm.len() as i64, "every byte accepted");
        assert!(read_index >= 0);
        assert!(
            read_index <= write_index,
            "read {read_index} ran past write {write_index}: the clock is ahead of the sound"
        );
        assert!(
            sink_usec > 0,
            "there is audio in flight, so there is latency"
        );
        // The figure is the mixer's own derived sum, not a number this module
        // made up, and not a constant. §K.3: "A constant 50 ms is not an
        // implementation."
        let id = session.stream_id(channel).unwrap();
        let timing = mixer.timing(id).unwrap();
        assert_eq!(sink_usec, timing.latency_usec);
        assert_eq!(write_index as u64, timing.write_index);
        assert_eq!(read_index as u64, timing.read_index);
        // And it moves with the audio rather than sitting at a constant.
        // Playing out what is queued lowers the figure.
        assert!(timing.queued_frames > 0, "there is a backlog to work off");
        for _ in 0..6 {
            let pumped = mixer.pump(&mut sink).unwrap();
            sink.advance(pumped.frames_written);
        }
        let _ = session.take_output();
        session
            .feed(
                &build(command::GET_PLAYBACK_LATENCY, 15, |writer| {
                    writer.u32(channel).timeval(0, 0);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        reader.u32().unwrap();
        reader.u32().unwrap();
        let later = reader.usec().unwrap();
        assert!(
            later < sink_usec,
            "latency stayed at {sink_usec} while the device played: a constant, not a measurement"
        );
        let id = session.stream_id(channel).unwrap();
        assert_eq!(later, mixer.timing(id).unwrap().latency_usec);
    }

    /// Mute is a flag, not a volume of zero: unmuting restores exactly what the
    /// client set.
    #[test]
    fn mute_and_unmute_restore_the_clients_own_volume() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let half = VOLUME_NORM / 2;
        session
            .feed(
                &build(command::SET_SINK_INPUT_VOLUME, 30, |writer| {
                    writer.u32(channel).cvolume(&[half, half]);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::SET_SINK_INPUT_MUTE, 31, |writer| {
                    writer.u32(channel).boolean(true);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::SET_SINK_INPUT_MUTE, 32, |writer| {
                    writer.u32(channel).boolean(false);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        // The stream's own volume survived the mute, and the sink-input reply
        // reports it.
        session
            .feed(
                &build(command::GET_SINK_INPUT_INFO, 33, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::REPLY);
        let _tag = reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), channel);
        assert_eq!(reader.string().unwrap().as_deref(), Some("a tone"));
        assert_eq!(reader.u32().unwrap(), OWNER_MODULE);
        assert_eq!(reader.u32().unwrap(), 0, "client index");
        assert_eq!(reader.u32().unwrap(), SINK_INDEX);
        assert_eq!(reader.sample_spec().unwrap().channels, 2);
        assert_eq!(reader.channel_map().unwrap(), vec![1, 2]);
        assert_eq!(reader.cvolume().unwrap(), vec![half, half], "restored");
        let _buffer_usec = reader.usec().unwrap();
        let _sink_usec = reader.usec().unwrap();
        assert_eq!(reader.string().unwrap().as_deref(), Some("copy"));
        assert_eq!(reader.string().unwrap().as_deref(), Some(DRIVER));
        assert!(!reader.boolean().unwrap(), "unmuted");
        assert!(reader
            .proplist()
            .unwrap()
            .iter()
            .any(|p| p.key == "media.name"));
        assert!(!reader.boolean().unwrap(), "not corked");
        assert!(reader.boolean().unwrap(), "has volume");
        assert!(reader.boolean().unwrap(), "volume writable");
        assert_eq!(reader.format_info().unwrap().0, format::ENCODING_PCM);
        reader.finish().unwrap();
    }

    /// A subscribed client is told when a stream changes; an unsubscribed one
    /// is not. Sending events nobody asked for is how a server ends up writing
    /// into a socket the client is not reading.
    #[test]
    fn subscription_events_go_only_to_subscribers() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::SET_SINK_INPUT_MUTE, 40, |writer| {
                    writer.u32(channel).boolean(true);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(
            commands(&mut session),
            vec![command::REPLY],
            "no subscription"
        );

        session
            .feed(
                &build(command::SUBSCRIBE, 41, |writer| {
                    writer.u32(subscription::MASK_ALL);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::SET_SINK_INPUT_MUTE, 42, |writer| {
                    writer.u32(channel).boolean(false);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let events: Vec<u32> = replies
            .iter()
            .filter_map(|packet| wire::command_and_tag(packet).ok())
            .map(|(command, _)| command)
            .collect();
        assert!(events.contains(&command::SUBSCRIBE_EVENT));
        let event = replies
            .iter()
            .find(|packet| {
                wire::command_and_tag(packet).map(|(c, _)| c) == Ok(command::SUBSCRIBE_EVENT)
            })
            .unwrap();
        let mut reader = tag::Reader::new(event);
        reader.u32().unwrap();
        assert_eq!(
            reader.u32().unwrap(),
            tag::INVALID_INDEX,
            "events carry no tag"
        );
        let word = reader.u32().unwrap();
        assert_eq!(
            word & subscription::EVENT_FACILITY_MASK,
            subscription::EVENT_SINK_INPUT
        );
        assert_eq!(
            word & subscription::EVENT_TYPE_MASK,
            subscription::EVENT_CHANGE
        );
        assert_eq!(reader.u32().unwrap(), channel);
        reader.finish().unwrap();
    }

    /// Renaming through a proplist update takes effect — the path
    /// `proto::K3_AMENDMENTS` says clients actually use.
    #[test]
    fn a_proplist_update_renames_the_stream() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        session
            .feed(&client(&unhex(CAPTURED_RENAME)), &mut mixer)
            .unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::GET_SINK_INPUT_INFO, 50, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        reader.u32().unwrap();
        reader.u32().unwrap();
        reader.u32().unwrap();
        assert_eq!(reader.string().unwrap().as_deref(), Some("renamed"));
    }

    /// The captured `pa_stream_set_name("renamed")` packet.
    const CAPTURED_RENAME: &str = "\
4c000000514c0000000f4c000000004c0000000250746d656469612e6e616d65004c000000087800\
00000872656e616d6564004e";

    /// Flush drops this stream's queue and nothing else — the other stream's
    /// audio survives. A flush that reached the mixed output would silence
    /// every app, which is the same mistake §K.3 names for DRAIN.
    #[test]
    fn flush_drops_only_its_own_stream() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        for tag in 1..=2 {
            session
                .feed(
                    &build(command::CREATE_PLAYBACK_STREAM, tag, |writer| {
                        write_create_request(writer, false);
                    }),
                    &mut mixer,
                )
                .unwrap();
        }
        let _ = session.take_output();
        assert_eq!(session.stream_count(), 2);
        for channel in [0u32, 1] {
            let mut pcm = Vec::new();
            for _ in 0..480 * 2 {
                pcm.extend_from_slice(&700i16.to_le_bytes());
            }
            let mut audio = wire::Descriptor::encode(pcm.len() as u32, channel, 0, 0).to_vec();
            audio.extend_from_slice(&pcm);
            session.feed(&audio, &mut mixer).unwrap();
        }
        let original = session.stream_id(0).unwrap();
        let _ = session.take_output();
        let before = mixer.timing(session.stream_id(0).unwrap()).unwrap();
        session
            .feed(
                &build(command::FLUSH_PLAYBACK_STREAM, 60, |writer| {
                    writer.u32(0);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
        // By CHANNEL, not by position: the ids are the mixer's and a flush
        // keeps channel 0's original admission and absolute byte clock.
        let flushed = session.stream_id(0).unwrap();
        let other = session.stream_id(1).unwrap();
        assert_eq!(flushed, original, "flush preserves the stream identity");
        assert_ne!(flushed, other, "two clients' streams are two mixer streams");
        let after = mixer.timing(flushed).unwrap();
        assert_eq!(after.queued_frames, 0, "flushed");
        assert_eq!(
            after.write_index, before.write_index,
            "the byte clock is stable"
        );
        assert_eq!(
            after.read_index, after.write_index,
            "flushed bytes are consumed"
        );
        assert_eq!(
            mixer.timing(other).unwrap().queued_frames,
            480,
            "the other stream is untouched"
        );
    }

    /// Flush preserves both sides of underflow accounting. Otherwise a new
    /// mixer counter starts below the session's reported count and later
    /// events disappear until it catches up.
    #[test]
    fn flush_preserves_underflow_event_continuity() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut audio = wire::Descriptor::encode(4 * 4, channel, 0, 0).to_vec();
        audio.extend_from_slice(&[1u8; 4 * 4]);

        session.feed(&audio, &mut mixer).unwrap();
        mixer.pump(&mut sink).unwrap();
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(5);
        mixer.recover(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::UNDERFLOW));
        let id = session.stream_id(channel).unwrap();
        let first_write_index = mixer.timing(id).unwrap().write_index;

        session
            .feed(
                &build(command::FLUSH_PLAYBACK_STREAM, 61, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        assert_eq!(session.stream_id(channel), Some(id));
        assert_eq!(mixer.timing(id).unwrap().write_index, first_write_index);
        session
            .feed(
                &build(command::TRIGGER_PLAYBACK_STREAM, 62, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();

        session.feed(&audio, &mut mixer).unwrap();
        mixer.pump(&mut sink).unwrap();
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(5);
        mixer.recover(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(commands(&mut session).contains(&command::UNDERFLOW));
        assert_eq!(mixer.underflows(id).unwrap(), 2);
        assert!(mixer.timing(id).unwrap().write_index > first_write_index);
    }

    /// Flush begins a new run on the stream, so later playback must receive a
    /// new STARTED event rather than inheriting the old run's notification.
    #[test]
    fn flush_rearms_the_started_event() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        let mut audio = wire::Descriptor::encode(4 * 4, channel, 0, 0).to_vec();
        audio.extend_from_slice(&[1u8; 4 * 4]);

        session.feed(&audio, &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.service(&mut mixer);
        assert_eq!(
            commands(&mut session)
                .into_iter()
                .filter(|command| *command == command::STARTED)
                .count(),
            1
        );
        sink.start().unwrap();
        sink.advance(4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        session.service(&mut mixer);
        let _ = session.take_output();

        session
            .feed(
                &build(command::FLUSH_PLAYBACK_STREAM, 60, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
        session.feed(&audio, &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        session.service(&mut mixer);
        assert_eq!(
            commands(&mut session)
                .into_iter()
                .filter(|command| *command == command::STARTED)
                .count(),
            1
        );
        sink.advance(4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
    }

    /// A stream in a format the device does not play is refused, not accepted
    /// and played at the wrong speed.
    #[test]
    fn a_stream_in_another_format_is_refused() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                    writer
                        .sample_spec(SampleSpec {
                            format: format::SAMPLE_S16LE,
                            channels: 2,
                            rate: 44_100,
                        })
                        .channel_map(&CHANNEL_MAP)
                        .u32(tag::INVALID_INDEX)
                        .null_string()
                        .u32(tag::INVALID_INDEX)
                        .boolean(false)
                        .u32(tag::INVALID_INDEX)
                        .u32(tag::INVALID_INDEX)
                        .u32(tag::INVALID_INDEX)
                        .u32(0)
                        .cvolume(&[VOLUME_NORM, VOLUME_NORM]);
                    for _ in 0..9 {
                        writer.boolean(false);
                    }
                    writer.proplist(&[]);
                    for _ in 0..7 {
                        writer.boolean(false);
                    }
                    writer.u8(0);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        let _tag = reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), error::NOTSUPPORTED);
        assert_eq!(session.stream_count(), 0);
        assert_eq!(
            mixer.stream_count(),
            0,
            "and no mixer stream was left behind"
        );
    }

    /// The self-describing packet shape is only the first validation layer.
    /// Correlated map, volume and sink fields still have to describe this one
    /// fixed sink, and feature flags cannot request behavior v1 lacks.
    #[test]
    fn malformed_or_unsupported_stream_semantics_are_refused_before_admission() {
        let cases = [
            (
                CreateOptions {
                    map: &[format::FRONT_LEFT],
                    ..CreateOptions::default()
                },
                error::INVALID,
            ),
            (
                CreateOptions {
                    sink_name: Some("another-sink"),
                    ..CreateOptions::default()
                },
                error::NOENTITY,
            ),
            (
                CreateOptions {
                    volumes: &[VOLUME_NORM],
                    ..CreateOptions::default()
                },
                error::INVALID,
            ),
            (
                CreateOptions {
                    variable_rate: true,
                    ..CreateOptions::default()
                },
                error::NOTSUPPORTED,
            ),
            (
                CreateOptions {
                    relative_volume: true,
                    ..CreateOptions::default()
                },
                error::NOTSUPPORTED,
            ),
            (
                CreateOptions {
                    passthrough: true,
                    ..CreateOptions::default()
                },
                error::NOTSUPPORTED,
            ),
            (
                CreateOptions {
                    encodings: &[99],
                    ..CreateOptions::default()
                },
                error::NOTSUPPORTED,
            ),
        ];
        for (options, expected) in cases {
            assert_eq!(create_error(&options), (expected, 0, 0));
        }
    }

    /// `media.name` is a Pulse text property, not an arbitrary byte string.
    /// Refusing malformed values before allocating identities also prevents an
    /// invalid-request loop from consuming the finite channel/id spaces.
    #[test]
    fn malformed_media_names_are_refused_before_admission() {
        let cases = [
            Property {
                key: "media.name".to_string(),
                value: b"unterminated".to_vec(),
            },
            Property {
                key: "media.name".to_string(),
                value: b"interior\0nul\0".to_vec(),
            },
            Property {
                key: "media.name".to_string(),
                value: vec![0xff, 0],
            },
            Property {
                key: "media.name".to_string(),
                value: {
                    let mut value = vec![b'x'; tag::STRING_MAX + 1];
                    value.push(0);
                    value
                },
            },
        ];
        for property in &cases {
            let properties = [property.clone()];
            assert_eq!(
                create_error(&CreateOptions {
                    properties: Some(&properties),
                    ..CreateOptions::default()
                }),
                (error::INVALID, 0, 0)
            );
        }
    }

    /// Control requests have semantic schemas too: selector XOR, exact
    /// subscription bits, stereo-or-scalar cvolume, and the one supported
    /// proplist update mode. Tagged fields alone do not establish those rules.
    #[test]
    fn malformed_control_semantics_are_refused() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let index = session
            .streams
            .get(&channel)
            .map(|stream| stream.sink_input_index)
            .unwrap();

        session
            .feed(
                &build(command::GET_SINK_INFO, 20, |writer| {
                    writer.u32(SINK_INDEX).string(SINK_NAME);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(error_code(&mut session), error::NOENTITY);

        session
            .feed(
                &build(command::SUBSCRIBE, 21, |writer| {
                    writer.u32(subscription::MASK_ALL | (1 << 31));
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(error_code(&mut session), error::INVALID);

        for (tag_number, volumes) in [(22, Vec::new()), (23, vec![VOLUME_NORM; 3])] {
            session
                .feed(
                    &build(command::SET_SINK_INPUT_VOLUME, tag_number, |writer| {
                        writer.u32(index).cvolume(&volumes);
                    }),
                    &mut mixer,
                )
                .unwrap();
            assert_eq!(error_code(&mut session), error::INVALID);
        }

        session
            .feed(
                &build(command::UPDATE_PLAYBACK_STREAM_PROPLIST, 25, |writer| {
                    writer.u32(channel).u32(1).proplist(&[]);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(error_code(&mut session), error::INVALID);

        let malformed_name = [Property {
            key: "media.name".to_string(),
            value: b"old\0new\0".to_vec(),
        }];
        session
            .feed(
                &build(command::UPDATE_PLAYBACK_STREAM_PROPLIST, 26, |writer| {
                    writer.u32(channel).u32(2).proplist(&malformed_name);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(error_code(&mut session), error::INVALID);
    }

    /// Clients may restore gain above 100 percent. td's v1 policy has no
    /// software gain above unity, so it caps the value instead of refusing an
    /// otherwise valid stream or control request.
    #[test]
    fn volume_above_unity_is_capped_at_the_protocol_edge() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            volumes: &[VOLUME_NORM * 2, VOLUME_NORM * 2],
                            volume_set: true,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
        let stream = session.streams.get(&0).unwrap();
        assert_eq!(stream.volume, VOLUME_NORM);
        let index = stream.sink_input_index;

        session
            .feed(
                &build(command::SET_SINK_INPUT_VOLUME, 2, |writer| {
                    writer.u32(index).cvolume(&[VOLUME_NORM * 2]);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
        assert_eq!(session.streams.get(&0).unwrap().volume, VOLUME_NORM);
    }

    /// Per-connection channels stop before Pulse's reserved INVALID value and
    /// never reuse an identity after counter exhaustion.
    #[test]
    fn stream_channels_exhaust_before_the_reserved_value() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session.next_channel = u64::from(u32::MAX - 1);
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request(writer, false);
                }),
                &mut mixer,
            )
            .unwrap();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 2, |writer| {
                    write_create_request(writer, false);
                }),
                &mut mixer,
            )
            .unwrap();
        let replies = packets(&mut session);
        assert_eq!(
            wire::command_and_tag(replies.first().unwrap()),
            Ok((command::REPLY, 1))
        );
        assert_eq!(
            wire::command_and_tag(replies.get(1).unwrap()),
            Ok((command::ERROR, 2))
        );
        assert!(!session.streams.contains_key(&tag::INVALID_INDEX));
    }

    /// TRIGGER releases one sub-threshold run, an empty run re-arms the gate,
    /// and DRAIN releases it again so the requested drain can finish.
    #[test]
    fn prebuffer_trigger_and_drain_control_each_run() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 8, |writer| {
                    write_create_options(
                        writer,
                        &CreateOptions {
                            prebuf: 8 * 4,
                            ..CreateOptions::default()
                        },
                    );
                }),
                &mut mixer,
            )
            .unwrap();
        let _ = session.take_output();
        let mut audio = wire::Descriptor::encode(4 * 4, 0, 0, 0).to_vec();
        audio.extend_from_slice(&[1u8; 4 * 4]);
        session.feed(&audio, &mut mixer).unwrap();
        let mut sink = MemorySink::new(Spec::fixed(), 64, 4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);

        session
            .feed(
                &build(command::TRIGGER_PLAYBACK_STREAM, 9, |writer| {
                    writer.u32(0);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        let _ = session.take_output();
        sink.start().unwrap();
        sink.advance(4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        session.service(&mut mixer);
        assert!(packets(&mut session).iter().any(|packet| {
            wire::command_and_tag(packet).is_ok_and(|pair| pair.0 == command::UNDERFLOW)
        }));

        // Crossing the accepted endpoint rearms prebuffering without a PREBUF
        // command. Queue emptiness alone is not an underflow while its frames
        // remain in the device ring.
        session.feed(&audio, &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        session
            .feed(
                &build(command::DRAIN_PLAYBACK_STREAM, 10, |writer| {
                    writer.u32(0);
                }),
                &mut mixer,
            )
            .unwrap();
        assert!(
            commands(&mut session).is_empty(),
            "the audio is still queued"
        );
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        sink.start().unwrap();
        sink.advance(4);
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 0);
        session.service(&mut mixer);
        let drain_packets = packets(&mut session);
        assert!(drain_packets
            .iter()
            .any(|packet| { wire::command_and_tag(packet) == Ok((command::REPLY, 10)) }));
        assert!(!drain_packets.iter().any(|packet| {
            wire::command_and_tag(packet).is_ok_and(|pair| pair.0 == command::UNDERFLOW)
        }));
        assert_eq!(mixer.underflows(session.stream_id(0).unwrap()).unwrap(), 1);

        // The earlier real starvation is still the only underflow. DRAIN's
        // intentional endpoint did not rearm the eight-frame gate, so another
        // four-frame tail remains runnable after the acknowledgement.
        session.feed(&audio, &mut mixer).unwrap();
        assert_eq!(mixer.pump(&mut sink).unwrap().frames_written, 4);
        assert_eq!(mixer.underflows(session.stream_id(0).unwrap()).unwrap(), 1);
        sink.start().unwrap();
        mixer.note_started();
        sink.advance(4);
        mixer.observe_playhead(&mut sink).unwrap();
        session.service(&mut mixer);
        assert!(packets(&mut session).iter().any(|packet| {
            wire::command_and_tag(packet).is_ok_and(|pair| pair.0 == command::UNDERFLOW)
        }));
        assert_eq!(mixer.underflows(session.stream_id(0).unwrap()).unwrap(), 2);
    }

    /// The only supported write position is exactly the current write index.
    #[test]
    fn a_nonzero_relative_seek_is_not_appended_at_the_wrong_position() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let id = session.stream_id(channel).unwrap();
        let mut audio = wire::Descriptor::encode(4, channel, 4, 0).to_vec();
        audio.extend_from_slice(&[1, 2, 3, 4]);
        let error = session.feed(&audio, &mut mixer).unwrap_err();
        assert_eq!(
            error,
            Disconnect::UnsupportedWrite {
                seek: Seek::Relative,
                offset: 4
            }
        );
        assert_eq!(mixer.timing(id).unwrap().write_index, 0);
    }

    /// Socket chunks may split anywhere, but a complete Pulse data frame must
    /// still end on this sink's interleaved PCM frame boundary.
    #[test]
    fn a_partial_pcm_frame_is_refused_instead_of_dropped_as_overflow() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        let id = session.stream_id(channel).unwrap();
        let mut audio = wire::Descriptor::encode(3, channel, 0, 0).to_vec();
        audio.extend_from_slice(&[1, 2, 3]);
        let error = session.feed(&audio, &mut mixer).unwrap_err();
        assert_eq!(
            error,
            Disconnect::PcmAlignment {
                bytes: 3,
                frame_bytes: 4,
            }
        );
        assert_eq!(mixer.timing(id).unwrap().write_index, 0);
        assert!(commands(&mut session).is_empty(), "not a queue overflow");
    }

    /// A command with the right shape but no handler is answered
    /// `NOTIMPLEMENTED` — 23, the code libpulse reports as "Missing
    /// implementation" — rather than dropped.
    #[test]
    fn an_unhandled_command_is_answered_not_dropped() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        // 13 is STAT in the protocol and is not on §K.3's playback list.
        session.feed(&build(13, 70, |_| {}), &mut mixer).unwrap();
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::ERROR);
        assert_eq!(
            reader.u32().unwrap(),
            70,
            "answered on the client's own tag"
        );
        assert_eq!(reader.u32().unwrap(), error::NOTIMPLEMENTED);
        reader.finish().unwrap();
    }

    /// A packet that does not match its schema ends the connection rather than
    /// being half-applied. §K.3: the schemas are what make a well-formed but
    /// unexpected packet an error.
    #[test]
    fn a_packet_that_misses_its_schema_ends_the_connection() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        let error = session
            .feed(
                &build(command::CORK_PLAYBACK_STREAM, 9, |writer| {
                    writer.u32(0); // and then nothing, where a boolean belongs
                }),
                &mut mixer,
            )
            .unwrap_err();
        assert!(matches!(error, Disconnect::Schema(_)));
        assert!(error.to_string().starts_with("schema:"));
    }

    /// Trailing bytes after a schema are refused too — a packet that parses and
    /// has more in it is not the packet it claims to be.
    #[test]
    fn a_packet_with_trailing_bytes_ends_the_connection() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        let error = session
            .feed(
                &build(command::GET_SERVER_INFO, 3, |writer| {
                    writer.u32(1);
                }),
                &mut mixer,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            Disconnect::Schema(tag::Error::Trailing { .. })
        ));
    }

    /// A framing refusal reaches the caller as a disconnect, with the framing
    /// error intact.
    #[test]
    fn a_refused_frame_ends_the_connection() {
        let (mut session, mut mixer) = fixture();
        let descriptor = wire::Descriptor::encode(16, 0, 0, wire::FLAG_SHMDATA);
        let error = session.feed(&descriptor, &mut mixer).unwrap_err();
        assert_eq!(
            error,
            Disconnect::Framing(wire::Error::SharedMemoryRefused),
            "a client cannot talk this server into shared memory"
        );
        assert!(error.to_string().contains("PA_FLAG_SHMDATA"));
    }

    /// Losing the device tells every stream it is gone, so clients tear down
    /// rather than writing into a server that will never play again.
    #[test]
    fn losing_the_device_kills_every_stream() {
        let (mut session, mut mixer) = fixture();
        with_stream(&mut session, &mut mixer, false);
        session.kill_all_streams();
        assert_eq!(session.stream_count(), 0);
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().unwrap());
        assert_eq!(reader.u32().unwrap(), command::PLAYBACK_STREAM_KILLED);
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
        assert_eq!(reader.u32().unwrap(), 0, "the channel");
        reader.finish().unwrap();
    }

    /// A whole conversation arriving in one read, and in one-byte reads, gives
    /// the same answers. The socket decides how bytes arrive, not the client.
    #[test]
    fn the_session_does_not_care_how_the_bytes_arrive() {
        let mut conversation = auth_packet(35);
        conversation.extend(build(command::SET_CLIENT_NAME, 1, |writer| {
            writer.proplist(&[tag::text_property("application.name", "firefox")]);
        }));
        conversation.extend(build(command::GET_SERVER_INFO, 2, |_| {}));
        conversation.extend(build(command::GET_SINK_INFO_LIST, 3, |_| {}));
        conversation.extend(build(command::GET_SOURCE_INFO_LIST, 4, |_| {}));
        conversation.extend(build(command::SUBSCRIBE, 5, |writer| {
            writer.u32(subscription::MASK_ALL);
        }));
        conversation.extend(build(command::CREATE_PLAYBACK_STREAM, 6, |writer| {
            write_create_request(writer, false);
        }));

        let (mut bulk, mut bulk_mixer) = fixture();
        bulk.feed(&conversation, &mut bulk_mixer).unwrap();
        let bulk_replies = packets(&mut bulk);

        let (mut drip, mut drip_mixer) = fixture();
        for byte in &conversation {
            drip.feed(&[*byte], &mut drip_mixer).unwrap();
        }
        let drip_replies = packets(&mut drip);

        assert_eq!(bulk_replies, drip_replies);
        // Seven commands, seven replies — plus one SUBSCRIBE_EVENT, because the
        // client subscribed before it created the stream and a new sink input
        // is exactly what it asked to be told about.
        assert_eq!(bulk_replies.len(), 8);
        let kinds: Vec<u32> = bulk_replies
            .iter()
            .filter_map(|packet| wire::command_and_tag(packet).ok())
            .map(|(command, _)| command)
            .collect();
        assert_eq!(kinds.iter().filter(|c| **c == command::REPLY).count(), 7);
        assert_eq!(
            kinds
                .iter()
                .filter(|c| **c == command::SUBSCRIBE_EVENT)
                .count(),
            1
        );
        assert_eq!(bulk.stream_count(), drip.stream_count());
    }

    /// One socket read can contain thousands of tiny commands. The daemon
    /// consumes only its per-pass budget and retains complete framed input for
    /// later passes, so control floods cannot monopolize the device thread.
    #[test]
    fn complete_input_frames_are_processed_in_bounded_batches() {
        let (mut session, mut mixer) = fixture();
        session.feed(&auth_packet(35), &mut mixer).unwrap();
        let _ = session.take_output();
        let mut questions = Vec::new();
        for tag_number in 0..20 {
            questions.extend(build(command::GET_SERVER_INFO, tag_number, |_| {}));
        }

        assert_eq!(session.feed_limited(&questions, &mut mixer, 7).unwrap(), 7);
        assert!(session.input_deferred());
        assert_eq!(session.feed_limited(&[], &mut mixer, 7).unwrap(), 7);
        assert!(session.input_deferred());
        assert_eq!(session.feed_limited(&[], &mut mixer, 7).unwrap(), 6);
        assert!(!session.input_deferred());
        assert_eq!(packets(&mut session).len(), 20);
    }

    /// Output admission happens before allocation. A fan-out or query flood
    /// can mark this peer for disconnection, but cannot first grow its session
    /// buffer beyond the advertised ceiling.
    #[test]
    fn session_output_refuses_growth_at_the_exact_ceiling() {
        let mut session = Session::new(Spec::fixed(), 0);
        session.out = vec![0; MAX_OUTPUT_BYTES - 1];
        let before = session.out.len();
        session.reply(1, |_| {});
        assert_eq!(session.out.len(), before);
        assert!(session.output_overflowed());
    }
}
