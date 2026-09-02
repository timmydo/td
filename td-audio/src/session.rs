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

/// `PA_SINK_RUNNING`. Reported unconditionally: td's sink does not suspend, so
/// a client that watches for suspension never sees one.
pub const SINK_STATE_RUNNING: u32 = 0;

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

/// The most streams one connection may hold.
///
/// Every stream is a linear scan in the mixer and a `timing` call per pass, so
/// an unbounded count is the daemon's CPU as well as its memory. A browser with
/// a tab per sound and a notification daemon beside it does not reach eight.
pub const MAX_STREAMS_PER_CLIENT: usize = 32;

/// How many refusals one connection may log before it stops being interesting.
///
/// The client chooses how often a refusal happens — a peer that has not
/// authenticated can loop a malformed `AUTH` — and a line apiece turns one bad
/// client into unbounded writes to a pipe whose reader can stall the whole
/// single-threaded daemon.
const REFUSAL_LOG_LIMIT: u32 = 8;

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
        }
    }
}

/// One playback stream, from the protocol's side. The audio itself lives in the
/// mixer; this is the bookkeeping the wire needs.
#[derive(Debug, Clone)]
struct Stream {
    /// The Pulse channel, which is also this server's sink-input index. It is
    /// this CONNECTION's name for the stream, and the only one the client sees.
    channel: u32,
    /// The shared mixer's name for it. Distinct from `channel` because channels
    /// are per-connection: every client calls its first stream 0.
    id: StreamId,
    /// Frames the client may keep queued.
    target_frames: u64,
    /// The hard ceiling.
    maxlength_frames: u64,
    /// The smallest grant this stream will be sent.
    minreq_frames: u64,
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
    /// Underflows already reported, so each is announced once.
    reported_underflows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing but `AUTH` is accepted.
    New,
    /// Authenticated at this negotiated version.
    Ready(u32),
}

/// One connection.
pub struct Session {
    state: State,
    spec: Spec,
    decoder: wire::Decoder,
    out: Vec<u8>,
    streams: HashMap<u32, Stream>,
    next_channel: u32,
    client_index: u32,
    subscribed: u32,
    /// Refusals already logged. See `REFUSAL_LOG_LIMIT`.
    refusals_logged: u32,
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
            next_channel: 0,
            client_index,
            subscribed: 0,
            refusals_logged: 0,
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

    /// Bytes held for a frame that has not finished arriving. The daemon bounds
    /// this: a client that declares a frame and stops is a connection that
    /// never makes progress.
    pub fn buffered(&self) -> usize {
        self.decoder.buffered()
    }

    /// Every channel this session owns, for the daemon to reconcile the mixer
    /// against.
    pub fn stream_ids(&self) -> Vec<StreamId> {
        self.streams.values().map(|stream| stream.id).collect()
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

    /// Feed bytes read from the socket, handling every whole frame they
    /// complete.
    pub fn feed(&mut self, bytes: &[u8], mixer: &mut Mixer) -> Result<(), Disconnect> {
        self.decoder.push(bytes);
        while let Some(frame) = self.decoder.next_frame() {
            match frame.map_err(Disconnect::Framing)? {
                Frame::Control(packet) => self.control(&packet, mixer)?,
                Frame::Data {
                    channel,
                    seek,
                    offset,
                    pcm,
                } => self.data(channel, seek, offset, &pcm, mixer),
            }
        }
        Ok(())
    }

    /// Emit whatever the mixer's current state owes the client: byte grants,
    /// completed drains, underflow notices, and the first `STARTED`.
    ///
    /// Kept separate from `feed` because most of it is caused by the device
    /// consuming audio rather than by the client saying anything, and a server
    /// that only spoke when spoken to would stall exactly as §K.3 describes.
    pub fn service(&mut self, mixer: &Mixer) {
        let frame_bytes = self.spec.frame_bytes as u64;
        let mut updates: Vec<(u32, StreamId, u64, Option<u32>, bool, u32)> = Vec::new();
        for stream in self.streams.values() {
            let id = stream.id;
            let Ok(timing) = mixer.timing(id) else {
                continue;
            };
            let queued = timing.queued_frames.saturating_mul(frame_bytes);
            let target = stream.target_frames.saturating_mul(frame_bytes);
            let held = queued.saturating_add(stream.outstanding_bytes);
            let grant = target.saturating_sub(held);
            let minreq = stream.minreq_frames.saturating_mul(frame_bytes);
            let grant = if grant >= minreq && !stream.corked { grant } else { 0 };

            let drained = mixer.is_drained(id).unwrap_or(false);
            let finish_drain = stream.draining.filter(|_| drained);

            let start = !stream.started && !stream.corked && timing.read_index > 0;
            let underflows = mixer.underflows(id).unwrap_or(0);
            updates.push((stream.channel, id, grant, finish_drain, start, underflows));
        }

        for (channel, id, grant, finish_drain, start, underflows) in updates {
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
            if start {
                self.send(&packet(command::STARTED, tag::INVALID_INDEX, |writer| {
                    writer.u32(channel);
                }));
                if let Some(stream) = self.streams.get_mut(&channel) {
                    stream.started = true;
                }
            }
            let already = self
                .streams
                .get(&channel)
                .map(|stream| stream.reported_underflows)
                .unwrap_or(0);
            if underflows > already {
                let read_index = mixer
                    .timing(id)
                    .map(|timing| timing.read_index)
                    .unwrap_or(0);
                self.send(&packet(command::UNDERFLOW, tag::INVALID_INDEX, |writer| {
                    writer.u32(channel).s64(read_index as i64);
                }));
                if let Some(stream) = self.streams.get_mut(&channel) {
                    stream.reported_underflows = underflows;
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
        _offset: i64,
        pcm: &[u8],
        mixer: &mut Mixer,
    ) {
        // v1 accepts audio only at the write index. A client that seeks is
        // rewriting audio the mixer may already have summed, and answering that
        // correctly means a rewritable queue; refusing it plainly is better
        // than accepting it and playing the wrong thing.
        if seek != Seek::Relative {
            return;
        }
        let Some(stream) = self.streams.get_mut(&channel) else {
            return;
        };
        stream.outstanding_bytes = stream.outstanding_bytes.saturating_sub(pcm.len() as u64);
        let Ok(accepted) = mixer.write(stream.id, pcm) else {
            return;
        };
        if accepted < pcm.len() {
            self.send(&packet(command::OVERFLOW, tag::INVALID_INDEX, |writer| {
                writer.u32(channel);
            }));
        }
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
            command::GET_SINK_INFO => self.sink_info(reader, tag, version),
            command::GET_SINK_INFO_LIST => self.sink_info_list(reader, tag, version),
            command::GET_SOURCE_INFO_LIST => self.source_info_list(reader, tag),
            command::GET_SINK_INPUT_INFO => self.sink_input_info(reader, tag, version, mixer),
            command::SUBSCRIBE => self.subscribe(reader, tag),
            command::CREATE_PLAYBACK_STREAM => self.create_stream(reader, tag, version, mixer),
            command::DELETE_PLAYBACK_STREAM => self.delete_stream(reader, tag, mixer),
            command::CORK_PLAYBACK_STREAM => self.cork(reader, tag, mixer),
            command::FLUSH_PLAYBACK_STREAM => self.flush(reader, tag, mixer),
            command::PREBUF_PLAYBACK_STREAM | command::TRIGGER_PLAYBACK_STREAM => {
                self.acknowledge_channel(reader, tag)
            }
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
        if crate::proto::requested_shared_memory(raw) {
            // Worth a line: a client that asked for SHM and silently got socket
            // data is a client whose performance question has an answer nobody
            // wrote down. §K.2 is why the answer is no.
            eprintln!(
                "td-audio: a client asked for a shared-memory transport; \
                 serving it over the socket instead (APPLICATIONS.md K.2)"
            );
        }
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
    ) -> Result<(), Disconnect> {
        // Captured in both forms: an index with a NULL name, or an invalid
        // index with a name. One command, two ways of asking.
        let index = reader.u32().map_err(Disconnect::Schema)?;
        let name = reader.string().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let matched = match (index, name.as_deref()) {
            (tag::INVALID_INDEX, Some(asked)) => asked == SINK_NAME || asked == "@DEFAULT_SINK@",
            (tag::INVALID_INDEX, None) => true,
            (SINK_INDEX, _) => true,
            _ => false,
        };
        if !matched {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        let spec = self.sample_spec();
        self.reply(tag, |writer| {
            write_sink_info(writer, spec, version);
        });
        Ok(())
    }

    fn sink_info_list(
        &mut self,
        reader: tag::Reader<'_>,
        tag: u32,
        version: u32,
    ) -> Result<(), Disconnect> {
        reader.finish().map_err(Disconnect::Schema)?;
        let spec = self.sample_spec();
        self.reply(tag, |writer| {
            write_sink_info(writer, spec, version);
        });
        Ok(())
    }

    fn source_info_list(
        &mut self,
        reader: tag::Reader<'_>,
        tag: u32,
    ) -> Result<(), Disconnect> {
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
        let Some(stream) = self.streams.get(&index).cloned() else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        let spec = self.sample_spec();
        let queued_usec = mixer
            .timing(stream.id)
            .map(|timing| self.spec.frames_to_usec(timing.queued_frames))
            .unwrap_or(0);
        let device_usec = mixer
            .timing(stream.id)
            .map(|timing| self.spec.frames_to_usec(timing.device_delay_frames))
            .unwrap_or(0);
        let client = self.client_index;
        self.reply(tag, |writer| {
            writer
                .u32(index)
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
        Ok(())
    }

    fn subscribe(&mut self, mut reader: tag::Reader<'_>, tag: u32) -> Result<(), Disconnect> {
        // Masked to the facilities that exist. A bit outside `MASK_ALL` is a
        // client asking about something this protocol has no facility for, and
        // keeping it would mean matching against it forever.
        self.subscribed =
            reader.u32().map_err(Disconnect::Schema)? & subscription::MASK_ALL;
        reader.finish().map_err(Disconnect::Schema)?;
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
        let _map = reader.channel_map().map_err(Disconnect::Schema)?;
        let _sink_index = reader.u32().map_err(Disconnect::Schema)?;
        let _sink_name = reader.string().map_err(Disconnect::Schema)?;
        let maxlength = reader.u32().map_err(Disconnect::Schema)?;
        let corked = reader.boolean().map_err(Disconnect::Schema)?;
        let tlength = reader.u32().map_err(Disconnect::Schema)?;
        let _prebuf = reader.u32().map_err(Disconnect::Schema)?;
        let minreq = reader.u32().map_err(Disconnect::Schema)?;
        let _syncid = reader.u32().map_err(Disconnect::Schema)?;
        let volumes = reader.cvolume().map_err(Disconnect::Schema)?;
        for _ in 0..9 {
            reader.boolean().map_err(Disconnect::Schema)?;
        }
        let properties = reader.proplist().map_err(Disconnect::Schema)?;
        for _ in 0..7 {
            reader.boolean().map_err(Disconnect::Schema)?;
        }
        let formats = reader.u8().map_err(Disconnect::Schema)?;
        for _ in 0..formats {
            reader.format_info().map_err(Disconnect::Schema)?;
        }
        reader.finish().map_err(Disconnect::Schema)?;

        // v1 converts nothing. §K.4 fixes the device at one spec and the mixer
        // sums at that spec, so a stream in another format has to be refused
        // rather than silently played at the wrong speed. cubeb asks for the
        // server's own default, which is this.
        if requested.format != format::SAMPLE_S16LE
            || u32::from(requested.channels) != self.spec.channels
            || requested.rate != self.spec.rate
        {
            self.error(tag, error::NOTSUPPORTED);
            return Ok(());
        }

        if self.streams.len() >= MAX_STREAMS_PER_CLIENT {
            self.error(tag, error::TOOLARGE);
            return Ok(());
        }
        let frame_bytes = (self.spec.frame_bytes as u64).max(1);
        let ceiling_frames = (MAXLENGTH_CEILING / frame_bytes).max(1);
        // Clamped, not trusted. Both numbers are the client's, and the queue
        // they size is the daemon's.
        let target_frames = attribute_frames(tlength, DEFAULT_TARGET_MS, self.spec, frame_bytes)
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

        let channel = self.next_channel;
        self.next_channel = self.next_channel.saturating_add(1);
        // The channel is this connection's name for the stream; the id is the
        // shared mixer's. Two connected clients both call their first stream
        // channel 0, so the two names cannot be the same number.
        let Ok(id) = mixer.open(maxlength_frames) else {
            self.error(tag, error::INTERNAL);
            return Ok(());
        };
        let name = properties
            .iter()
            .find(|property| property.key == "media.name")
            .and_then(property_text)
            .unwrap_or_else(|| "playback".to_string());
        let volume = volumes.first().copied().unwrap_or(VOLUME_NORM).min(VOLUME_NORM);
        let _ = mixer.set_volume(id, volume);
        self.streams.insert(
            channel,
            Stream {
                channel,
                id,
                target_frames,
                maxlength_frames,
                minreq_frames,
                outstanding_bytes: 0,
                corked,
                muted: false,
                volume,
                name,
                draining: None,
                started: false,
                reported_underflows: 0,
            },
        );

        self.notify(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_NEW,
            channel,
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
        let spec = self.sample_spec();
        let bytes = |frames: u64| u32::try_from(frames.saturating_mul(frame_bytes)).unwrap_or(u32::MAX);
        let (maxlength_bytes, tlength_bytes, minreq_bytes) = (
            bytes(maxlength_frames),
            bytes(target_frames),
            bytes(minreq_frames),
        );
        self.reply(tag, |writer| {
            writer.u32(channel).u32(channel).u32(missing);
            if version >= 9 {
                writer
                    .u32(maxlength_bytes)
                    .u32(tlength_bytes)
                    .u32(tlength_bytes)
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
        mixer.remove(stream.id);
        self.reply(tag, |_| {});
        self.notify(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_REMOVE,
            channel,
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
        // Flush drops what this stream has queued and nothing else. Re-creating
        // it in the mixer is how that is expressed without a discard path that
        // no other caller would use.
        let limit = stream.maxlength_frames;
        stream.outstanding_bytes = 0;
        stream.draining = None;
        let volume = stream.volume;
        let muted = stream.muted;
        let corked = stream.corked;
        let old = stream.id;
        mixer.remove(old);
        let Ok(id) = mixer.open(limit) else {
            self.error(tag, error::INTERNAL);
            return Ok(());
        };
        // Re-admission issues a new id, so the stream has to be told its new
        // name or every later lookup would use the one just removed.
        if let Some(stream) = self.streams.get_mut(&channel) {
            stream.id = id;
        }
        let _ = mixer.set_volume(id, if muted { 0 } else { volume });
        let _ = mixer.set_corked(id, corked);
        self.reply(tag, |_| {});
        Ok(())
    }

    fn acknowledge_channel(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        if !self.streams.contains_key(&channel) {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        // PREBUF and TRIGGER are prebuffering controls, and this mixer starts
        // as soon as it has audio: there is no prebuffer to arm or release, so
        // the honest answer is success rather than an error that would make a
        // client think the stream is broken.
        self.reply(tag, |_| {});
        Ok(())
    }

    fn drain(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
        mixer: &Mixer,
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
        if mixer.is_drained(stream.id).unwrap_or(true) {
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
        let Some((id, corked)) = self
            .streams
            .get(&channel)
            .map(|stream| (stream.id, stream.corked))
        else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        let Ok(timing) = mixer.timing(id) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        let now = self.now_usec;
        let underruns = mixer.underflows(id).unwrap_or(0);
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
                .boolean(!corked)
                // The client's own timestamp, echoed. It computes the round
                // trip from it, so a stamp of this server's own making would
                // make every latency read look instantaneous.
                .timeval(local.0, local.1)
                .timeval((now / 1_000_000) as u32, (now % 1_000_000) as u32)
                .s64(timing.write_index as i64)
                .s64(timing.read_index as i64);
            if version >= 13 {
                writer
                    .u64(u64::from(underruns))
                    .u64(timing.read_index);
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
        let Some(stream) = self.streams.get_mut(&index) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        // One gain per stream: the mixer sums at one level, and picking the
        // loudest channel is the only choice that cannot quietly attenuate
        // audio the client asked to be loud.
        let volume = volumes.iter().copied().max().unwrap_or(VOLUME_NORM);
        stream.volume = volume.min(VOLUME_NORM);
        let effective = if stream.muted { 0 } else { stream.volume };
        if mixer.set_volume(stream.id, effective).is_err() {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        self.reply(tag, |_| {});
        self.notify(subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE, index);
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
        let Some(stream) = self.streams.get_mut(&index) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        stream.muted = muted;
        // Mute is a separate flag, not a volume of zero: unmuting has to
        // restore what the client set, and a server that folded the two would
        // have to invent a level.
        let effective = if muted { 0 } else { stream.volume };
        if mixer.set_volume(stream.id, effective).is_err() {
            self.error(tag, error::NOENTITY);
            return Ok(());
        }
        self.reply(tag, |_| {});
        self.notify(subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE, index);
        Ok(())
    }

    fn update_proplist(
        &mut self,
        mut reader: tag::Reader<'_>,
        tag: u32,
    ) -> Result<(), Disconnect> {
        let channel = reader.u32().map_err(Disconnect::Schema)?;
        let _mode = reader.u32().map_err(Disconnect::Schema)?;
        let properties = reader.proplist().map_err(Disconnect::Schema)?;
        reader.finish().map_err(Disconnect::Schema)?;
        let Some(stream) = self.streams.get_mut(&channel) else {
            self.error(tag, error::NOENTITY);
            return Ok(());
        };
        // This is also how a modern client renames a stream — see
        // `proto::K3_AMENDMENTS`.
        if let Some(name) = properties
            .iter()
            .find(|property| property.key == "media.name")
            .and_then(property_text)
        {
            stream.name = name;
        }
        self.reply(tag, |_| {});
        self.notify(subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE, channel);
        Ok(())
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
        debug_assert!(event & !(subscription::EVENT_FACILITY_MASK | subscription::EVENT_TYPE_MASK) == 0);
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

    fn reply(&mut self, tag: u32, body: impl FnOnce(&mut tag::Writer)) {
        self.send(&packet(command::REPLY, tag, body));
    }

    fn error(&mut self, tag: u32, code: u32) {
        // Named, not numbered. A refusal a reader has to look up in a header is
        // a refusal nobody reads. Bounded, because the client chooses how many
        // there are: see `REFUSAL_LOG_LIMIT`.
        if self.refusals_logged < REFUSAL_LOG_LIMIT {
            self.refusals_logged = self.refusals_logged.saturating_add(1);
            eprintln!(
                "td-audio: refusing request {tag}: {} ({code})",
                crate::proto::error_name(code).unwrap_or("an unnamed code")
            );
            if self.refusals_logged == REFUSAL_LOG_LIMIT {
                eprintln!(
                    "td-audio: this connection has been refused {REFUSAL_LOG_LIMIT} \
                     times; further refusals are answered but not logged"
                );
            }
        }
        self.send(&packet(command::ERROR, tag, |writer| {
            writer.u32(code);
        }));
    }

    fn send(&mut self, packet: &[u8]) {
        self.out.extend_from_slice(&wire::control_frame(packet));
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

/// A proplist value is NUL-terminated bytes; this is its text without the NUL.
fn property_text(property: &Property) -> Option<String> {
    let bytes = property.value.strip_suffix(&[0]).unwrap_or(&property.value);
    std::str::from_utf8(bytes).ok().map(|text| text.to_string())
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

/// The sink-info payload, shared by the single and list forms because they are
/// the same bytes — a list of one.
fn write_sink_info(writer: &mut tag::Writer, spec: SampleSpec, version: u32) {
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
        // PA_SINK_HARDWARE. The audio really does reach hardware.
        .u32(0x0001);
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
            .u32(SINK_STATE_RUNNING)
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

    /// Drive a session to the point where it has a stream, and return its
    /// channel.
    fn with_stream(session: &mut Session, mixer: &mut Mixer, corked: bool) -> u32 {
        session.feed(&auth_packet(35), mixer).unwrap();
        session
            .feed(
                &build(command::CREATE_PLAYBACK_STREAM, 1, |writer| {
                    write_create_request(writer, corked);
                }),
                mixer,
            )
            .unwrap();
        let _ = session.take_output();
        0
    }

    /// The same request with the two client-chosen buffer sizes spelled out.
    fn write_create_request_sized(writer: &mut tag::Writer, maxlength: u32, tlength: u32) {
        writer
            .sample_spec(SampleSpec {
                format: format::SAMPLE_S16LE,
                channels: 2,
                rate: 48_000,
            })
            .channel_map(&CHANNEL_MAP)
            .u32(tag::INVALID_INDEX)
            .null_string()
            .u32(maxlength)
            .boolean(false)
            .u32(tlength)
            .u32(tag::INVALID_INDEX)
            .u32(tag::INVALID_INDEX)
            .u32(0)
            .cvolume(&[VOLUME_NORM, VOLUME_NORM]);
        for _ in 0..9 {
            writer.boolean(false);
        }
        writer.proplist(&[tag::text_property("media.name", "a tone")]);
        for _ in 0..7 {
            writer.boolean(false);
        }
        writer.u8(0);
    }

    /// The version-35 create request, in the exact shape the captured packet
    /// has — sixteen booleans and all.
    fn write_create_request(writer: &mut tag::Writer, corked: bool) {
        writer
            .sample_spec(SampleSpec {
                format: format::SAMPLE_S16LE,
                channels: 2,
                rate: 48_000,
            })
            .channel_map(&CHANNEL_MAP)
            .u32(tag::INVALID_INDEX)
            .null_string()
            .u32(tag::INVALID_INDEX)
            .boolean(corked)
            .u32(tag::INVALID_INDEX)
            .u32(tag::INVALID_INDEX)
            .u32(tag::INVALID_INDEX)
            .u32(0)
            .cvolume(&[VOLUME_NORM, VOLUME_NORM]);
        for _ in 0..9 {
            writer.boolean(false);
        }
        writer.proplist(&[tag::text_property("media.name", "a tone")]);
        for _ in 0..7 {
            writer.boolean(false);
        }
        writer.u8(0);
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
        let granted = mixer.request_frames(id).unwrap().saturating_mul(frame_bytes);
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
        assert_eq!(reader.u32().unwrap(), command::REPLY, "a reply, not an error");
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
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX, "no monitor source");
        assert_eq!(reader.string().unwrap(), None);
        assert_eq!(reader.usec().unwrap(), 0);
        assert_eq!(reader.string().unwrap().as_deref(), Some(DRIVER));
        assert_eq!(reader.u32().unwrap(), 1, "PA_SINK_HARDWARE");
        assert!(reader.proplist().unwrap().iter().any(|p| p.key == "device.description"));
        assert_eq!(reader.usec().unwrap(), 0);
        assert_eq!(reader.volume().unwrap(), VOLUME_NORM);
        assert_eq!(reader.u32().unwrap(), SINK_STATE_RUNNING);
        assert_eq!(reader.u32().unwrap(), VOLUME_NORM + 1);
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX, "no card");
        assert_eq!(reader.u32().unwrap(), 0, "no ports");
        assert_eq!(reader.string().unwrap(), None, "no active port");
        assert_eq!(reader.u8().unwrap(), 1, "one format");
        assert_eq!(reader.format_info().unwrap().0, format::ENCODING_PCM);
        reader.finish().unwrap();
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
        assert_eq!(reader.u32().unwrap(), expected as u32, "prebuf");
        let minreq = reader.u32().unwrap();
        assert_eq!(
            u64::from(minreq),
            Spec::fixed().usec_to_frames(DEFAULT_MINREQ_MS * 1000)
                * Spec::fixed().frame_bytes as u64
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
        session.service(&mixer);
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
        session.service(&mixer);
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
            session.service(&mixer);
            assert!(commands(&mut session).is_empty(), "re-granted unspent bytes");
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
        session.service(&mixer);
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
        session.service(&mixer);
        let replies = packets(&mut session);
        let mut reader = tag::Reader::new(replies.first().expect("a grant for the played audio"));
        assert_eq!(reader.u32().unwrap(), command::REQUEST);
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX, "an event, not a reply");
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
        assert!(sink.is_running() || !sink.is_running(), "the sink is untouched");

        // Play it out. Advancing by exactly what the device took is what a
        // real device does; advancing past it would be an underrun.
        for _ in 0..8 {
            let pumped = mixer.pump(&mut sink).unwrap();
            sink.advance(pumped.frames_written);
        }
        session.service(&mixer);
        let replies = packets(&mut session);
        let drained = replies
            .iter()
            .filter_map(|packet| wire::command_and_tag(packet).ok())
            .find(|(command, _)| *command == command::REPLY);
        assert_eq!(drained, Some((command::REPLY, 20)), "the drain reply, tagged 20");
    }

    /// A drain on an empty stream is answered at once rather than left hanging.
    #[test]
    fn draining_an_empty_stream_replies_immediately() {
        let (mut session, mut mixer) = fixture();
        let channel = with_stream(&mut session, &mut mixer, false);
        session
            .feed(
                &build(command::DRAIN_PLAYBACK_STREAM, 21, |writer| {
                    writer.u32(channel);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
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
        assert_eq!(reader.timeval().unwrap(), (111, 222), "the client's own stamp");
        assert_eq!(reader.timeval().unwrap(), (1234, 567_890), "the server's");
        let write_index = reader.s64().unwrap();
        let read_index = reader.s64().unwrap();
        let _underrun_for = reader.u64().unwrap();
        let _playing_for = reader.u64().unwrap();
        reader.finish().unwrap();

        assert_eq!(write_index, pcm.len() as i64, "every byte accepted");
        assert!(read_index >= 0);
        assert!(
            read_index <= write_index,
            "read {read_index} ran past write {write_index}: the clock is ahead of the sound"
        );
        assert!(sink_usec > 0, "there is audio in flight, so there is latency");
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
        assert!(reader.proplist().unwrap().iter().any(|p| p.key == "media.name"));
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
        assert_eq!(commands(&mut session), vec![command::REPLY], "no subscription");

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
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX, "events carry no tag");
        let word = reader.u32().unwrap();
        assert_eq!(
            word & subscription::EVENT_FACILITY_MASK,
            subscription::EVENT_SINK_INPUT
        );
        assert_eq!(word & subscription::EVENT_TYPE_MASK, subscription::EVENT_CHANGE);
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
        let _ = session.take_output();
        session
            .feed(
                &build(command::FLUSH_PLAYBACK_STREAM, 60, |writer| {
                    writer.u32(0);
                }),
                &mut mixer,
            )
            .unwrap();
        assert_eq!(commands(&mut session), vec![command::REPLY]);
        // By CHANNEL, not by position: the ids are the mixer's and flushing
        // re-admitted channel 0 under a new one.
        let flushed = session.stream_id(0).unwrap();
        let other = session.stream_id(1).unwrap();
        assert_ne!(flushed, other, "two clients' streams are two mixer streams");
        assert_eq!(mixer.timing(flushed).unwrap().queued_frames, 0, "flushed");
        assert_eq!(
            mixer.timing(other).unwrap().queued_frames,
            480,
            "the other stream is untouched"
        );
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
        assert_eq!(mixer.stream_count(), 0, "and no mixer stream was left behind");
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
        assert_eq!(reader.u32().unwrap(), 70, "answered on the client's own tag");
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
        assert!(matches!(error, Disconnect::Schema(tag::Error::Trailing { .. })));
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
            kinds.iter().filter(|c| **c == command::SUBSCRIBE_EVENT).count(),
            1
        );
        assert_eq!(bulk.stream_count(), drip.stream_count());
    }
}
