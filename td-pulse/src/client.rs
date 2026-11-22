//! A blocking control client: connect, list, set and subscribe.
//!
//! td's controllers — the `td-audio` CLI personalities, the compositor's
//! volume keys and bar, the mixer panel — are ordinary Pulse clients of the
//! daemon's socket (`APPLICATIONS.md` §K.5). This is the one client they
//! share. It owns no socket: the caller hands it a [`Transport`], td's
//! controllers `socket::UnixSocket`, so this module stays bytes in and bytes
//! out and the client can be driven by a scripted server in tests.
//!
//! The server is not trusted. The client runs inside the compositor, so a
//! reply is parsed against the version-35 schema the request asked for and
//! anything else is an error, never a panic: a frame over the control bound
//! is refused by the decoder, audio data once it outgrows any control frame,
//! and a wait that consumes `MAX_WAIT_BYTES` without its answer; a list is
//! parsed one entry at a time from the one packet it arrived in, and every
//! count it carries is bounded by the bytes behind it. A wait's time is the
//! transport's to bound. After any failure but a refusal the connection is
//! broken, since its framing or its tags are no longer known.
//!
//! Requests are the packets libpulse sends for the same questions, and the
//! tests pin each one against a capture (§K.3): 16.1's where libpulse 17.0
//! sends the same bytes, 17.0's own otherwise.

use crate::proto::{self, command};
use crate::tag::{self, Property, Reader, Writer};
use crate::wire::{self, Decoder, Frame};

/// The bytes between the client and the server.
pub trait Transport {
    type Error;
    /// Send all of `bytes`.
    fn send(&mut self, bytes: &[u8]) -> std::result::Result<(), Self::Error>;
    /// Read what has arrived into `buffer`; `0` means the server closed.
    fn receive(&mut self, buffer: &mut [u8]) -> std::result::Result<usize, Self::Error>;
}

/// Why a request did not complete.
#[derive(Debug)]
pub enum Error<E> {
    /// The transport failed.
    Transport(E),
    /// The server closed the connection.
    Closed,
    /// The server sent a frame the decoder refuses.
    Framing(wire::Error),
    /// A packet did not match the schema it had to have.
    Malformed(tag::Error),
    /// The server answered with `ERROR` and this code (`proto::error`).
    Refused(u32),
    /// A command no server sends a control client, or a reply to a tag that
    /// was not asked for.
    Unexpected { command: u32, tag: u32 },
    /// Audio data, to a client that plays nothing.
    UnexpectedData,
    /// The server speaks an older protocol than the one this client parses.
    Unsupported(u32),
    /// The server sent more than `MAX_WAIT_BYTES` without answering.
    Flooded,
    /// An earlier failure left the connection's framing or tags unknown;
    /// reconnect.
    Broken,
}

impl<E> From<tag::Error> for Error<E> {
    fn from(error: tag::Error) -> Self {
        Self::Malformed(error)
    }
}

pub type Result<T, E> = std::result::Result<T, Error<E>>;

/// How much one read asks the transport for.
const READ_CHUNK: usize = 4096;

/// The most bytes one wait — for a reply, or for an event — may consume
/// without producing what it waits for: room for the largest reply and as
/// many events again. Past it the server is flooding, whether with events
/// or with the empty control frames the decoder skips, and a client that
/// kept reading would never return.
const MAX_WAIT_BYTES: usize = 2 * wire::CONTROL_MAX;

/// The most a decoder holding only an unfinished frame can hold when that
/// frame is a control frame. Anything larger is audio data on its way,
/// refused here rather than buffered up to the data bound.
const MAX_PENDING_BYTES: usize = wire::DESCRIPTOR_LEN + wire::CONTROL_MAX;

/// `PA_NATIVE_COOKIE_LENGTH`. td's server authorizes on the peer's kernel
/// credentials, so the cookie is sent as zeros.
const COOKIE_LEN: usize = 256;

/// The tag events arrive with, which a request never uses.
const EVENT_TAG: u32 = tag::INVALID_INDEX;

/// The default sink, as the server resolves it.
pub const DEFAULT_SINK: &str = "@DEFAULT_SINK@";

/// The server's own description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub package: Option<String>,
    pub version: Option<String>,
    pub default_sink: Option<String>,
}

/// One sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    pub index: u32,
    pub name: Option<String>,
    pub description: Option<String>,
    pub volume: Vec<u32>,
    pub muted: bool,
}

/// One playback stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInput {
    pub index: u32,
    pub name: Option<String>,
    pub client: u32,
    pub sink: u32,
    pub volume: Vec<u32>,
    pub muted: bool,
    pub properties: Vec<Property>,
}

/// One connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub index: u32,
    pub name: Option<String>,
    pub properties: Vec<Property>,
}

/// One subscription event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// `proto::subscription::EVENT_SINK` and the like.
    pub facility: u32,
    /// `EVENT_NEW`, `EVENT_CHANGE` or `EVENT_REMOVE`.
    pub kind: u32,
    pub index: u32,
}

impl SinkInput {
    /// A text property's value, as `pa_proplist_gets` would return it.
    pub fn property(&self, key: &str) -> Option<&str> {
        text_property(&self.properties, key)
    }
}

impl ClientInfo {
    pub fn property(&self, key: &str) -> Option<&str> {
        text_property(&self.properties, key)
    }
}

fn text_property<'a>(properties: &'a [Property], key: &str) -> Option<&'a str> {
    let property = properties.iter().find(|property| property.key == key)?;
    let value = property.value.strip_suffix(&[0]).unwrap_or(&property.value);
    std::str::from_utf8(value).ok()
}

/// `PA_VOLUME_NORM`: unity, and the loudest td's mixer plays anything.
pub const VOLUME_NORM: u32 = 0x10000;

/// The most a whole-percent level offers: unity, since the mixer caps there
/// and there is no boost to offer.
pub const MAX_PERCENT: u32 = 100;

/// A volume as a whole percentage of `VOLUME_NORM`, rounded and clamped:
/// td's mixer plays nothing louder, whatever a server reports.
pub fn percent(volume: u32) -> u32 {
    let norm = u64::from(VOLUME_NORM);
    let rounded = (u64::from(volume) * 100 + norm / 2) / norm;
    u32::try_from(rounded.min(u64::from(MAX_PERCENT))).unwrap_or(MAX_PERCENT)
}

/// A whole percentage as a volume, rounded: the inverse of `percent` on
/// every value `percent` returns.
pub fn volume_of(percent: u32) -> u32 {
    let norm = u64::from(VOLUME_NORM);
    let volume = (u64::from(percent.min(MAX_PERCENT)) * norm + 50) / 100;
    u32::try_from(volume).unwrap_or(VOLUME_NORM)
}

/// `levels` set to `volume` on as many channels as `like` has, at least one:
/// the cvolume a setter sends to put a stream or the output at one level.
pub fn levels_like<'a>(
    levels: &'a mut [u32; tag::CHANNELS_MAX],
    like: &[u32],
    volume: u32,
) -> &'a [u32] {
    *levels = [volume; tag::CHANNELS_MAX];
    let channels = like.len().clamp(1, tag::CHANNELS_MAX);
    levels.get(..channels).unwrap_or(levels.as_slice())
}

/// The loudest channel of a cvolume: the one level td's mixer plays a stream
/// or the output at.
pub fn level(volume: &[u32]) -> u32 {
    volume.iter().copied().max().unwrap_or(0)
}

/// A list reply, parsed one entry at a time from the packet it arrived in.
pub struct Listing<T> {
    packet: Vec<u8>,
    parse: fn(&mut Reader<'_>) -> tag::Result<T>,
}

impl<T> Listing<T> {
    /// The entries in the order the server sent them. A malformed entry ends
    /// the walk with its error.
    pub fn iter(&self) -> Entries<'_, T> {
        let mut reader = Reader::new(&self.packet);
        let start = reader.u32().and_then(|_| reader.u32());
        Entries {
            reader,
            parse: self.parse,
            failed: start.err(),
            done: false,
        }
    }
}

pub struct Entries<'a, T> {
    reader: Reader<'a>,
    parse: fn(&mut Reader<'_>) -> tag::Result<T>,
    failed: Option<tag::Error>,
    done: bool,
}

impl<T> Iterator for Entries<'_, T> {
    type Item = tag::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(error) = self.failed.take() {
            self.done = true;
            return Some(Err(error));
        }
        if self.reader.finish().is_ok() {
            self.done = true;
            return None;
        }
        let entry = (self.parse)(&mut self.reader);
        self.done = entry.is_err();
        Some(entry)
    }
}

/// A connection to a Pulse server, authenticated and named.
pub struct Client<T: Transport> {
    transport: T,
    decoder: Decoder,
    next_tag: u32,
    index: u32,
    /// Whether events may arrive: only after a SUBSCRIBE was sent.
    subscribed: bool,
    missed_events: bool,
    broken: bool,
}

impl<T: Transport> Client<T> {
    /// Authenticate over `transport` and name this client `application`.
    pub fn connect(transport: T, application: &str) -> Result<Self, T::Error> {
        let mut client = Self {
            transport,
            decoder: Decoder::new(),
            next_tag: 0,
            index: tag::INVALID_INDEX,
            subscribed: false,
            missed_events: false,
            broken: false,
        };
        let reply = client.call(command::AUTH, |writer| {
            writer.u32(proto::VERSION).arbitrary(&[0; COOKIE_LEN]);
        })?;
        let mut reader = body(&reply)?;
        let version = reader.u32()? & proto::VERSION_MASK;
        reader.finish()?;
        if version < proto::VERSION {
            return Err(Error::Unsupported(version));
        }
        let reply = client.call(command::SET_CLIENT_NAME, |writer| {
            writer.proplist(&[tag::text_property("application.name", application)]);
        })?;
        let mut reader = body(&reply)?;
        client.index = reader.u32()?;
        reader.finish()?;
        Ok(client)
    }

    /// The index the server gave this connection.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The transport, for settings the caller owns, such as a deadline.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn server_info(&mut self) -> Result<ServerInfo, T::Error> {
        let reply = self.call(command::GET_SERVER_INFO, |_| {})?;
        let mut reader = body(&reply)?;
        let package = reader.string()?;
        let version = reader.string()?;
        reader.string()?;
        reader.string()?;
        reader.sample_spec()?;
        let default_sink = reader.string()?;
        reader.string()?;
        reader.u32()?;
        reader.channel_map()?;
        reader.finish()?;
        Ok(ServerInfo {
            package,
            version,
            default_sink,
        })
    }

    /// The sink named `name`; [`DEFAULT_SINK`] is the active one.
    pub fn sink(&mut self, name: &str) -> Result<Sink, T::Error> {
        let reply = self.call(command::GET_SINK_INFO, |writer| {
            writer.u32(tag::INVALID_INDEX).string(name);
        })?;
        let mut reader = body(&reply)?;
        let sink = parse_sink(&mut reader)?;
        reader.finish()?;
        Ok(sink)
    }

    pub fn sinks(&mut self) -> Result<Listing<Sink>, T::Error> {
        let packet = self.call(command::GET_SINK_INFO_LIST, |_| {})?;
        Ok(Listing {
            packet,
            parse: parse_sink,
        })
    }

    /// Set a sink's volume, one level per channel.
    pub fn set_sink_volume(&mut self, index: u32, volume: &[u32]) -> Result<(), T::Error> {
        self.acknowledged(command::SET_SINK_VOLUME, |writer| {
            writer.u32(index).null_string().cvolume(volume);
        })
    }

    pub fn set_sink_mute(&mut self, index: u32, muted: bool) -> Result<(), T::Error> {
        self.acknowledged(command::SET_SINK_MUTE, |writer| {
            writer.u32(index).null_string().boolean(muted);
        })
    }

    pub fn sink_inputs(&mut self) -> Result<Listing<SinkInput>, T::Error> {
        let packet = self.call(command::GET_SINK_INPUT_INFO_LIST, |_| {})?;
        Ok(Listing {
            packet,
            parse: parse_sink_input,
        })
    }

    pub fn clients(&mut self) -> Result<Listing<ClientInfo>, T::Error> {
        let packet = self.call(command::GET_CLIENT_INFO_LIST, |_| {})?;
        Ok(Listing {
            packet,
            parse: parse_client,
        })
    }

    /// Set a stream's volume, one level per channel. On td's server a
    /// controller's write to an application's stream sets the human's level
    /// for the whole application.
    pub fn set_sink_input_volume(&mut self, index: u32, volume: &[u32]) -> Result<(), T::Error> {
        self.acknowledged(command::SET_SINK_INPUT_VOLUME, |writer| {
            writer.u32(index).cvolume(volume);
        })
    }

    pub fn set_sink_input_mute(&mut self, index: u32, muted: bool) -> Result<(), T::Error> {
        self.acknowledged(command::SET_SINK_INPUT_MUTE, |writer| {
            writer.u32(index).boolean(muted);
        })
    }

    /// Ask for the events of every facility in `mask`
    /// (`proto::subscription::MASK_*`).
    pub fn subscribe(&mut self, mask: u32) -> Result<(), T::Error> {
        // From the request on: the server may announce before it answers.
        self.subscribed |= mask != 0;
        self.acknowledged(command::SUBSCRIBE, |writer| {
            writer.u32(mask);
        })
    }

    /// Wait for the next event of a subscribed connection.
    pub fn next_event(&mut self) -> Result<Event, T::Error> {
        let outcome = self.await_event();
        self.note(&outcome);
        outcome
    }

    fn await_event(&mut self) -> Result<Event, T::Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let packet = self.next_packet()?;
        let mut reader = Reader::new(&packet);
        let command = reader.u32()?;
        let tag = reader.u32()?;
        if command != command::SUBSCRIBE_EVENT || tag != EVENT_TAG {
            return Err(Error::Unexpected { command, tag });
        }
        Ok(parse_event(&mut reader)?)
    }

    /// Whether events arrived while a request waited for its reply, and were
    /// dropped. They name what changed, not how, so a caller that re-reads
    /// the state it shows has lost nothing; this says when to.
    pub fn take_missed_events(&mut self) -> bool {
        std::mem::take(&mut self.missed_events)
    }

    #[cfg(test)]
    fn set_next_tag(&mut self, tag: u32) {
        self.next_tag = tag;
    }

    fn acknowledged(
        &mut self,
        command: u32,
        body_of: impl FnOnce(&mut Writer),
    ) -> Result<(), T::Error> {
        let reply = self.call(command, body_of)?;
        body(&reply)?.finish()?;
        Ok(())
    }

    /// Send one request and wait for its reply packet. Any failure but the
    /// server's refusal leaves the connection broken.
    fn call(
        &mut self,
        command: u32,
        body_of: impl FnOnce(&mut Writer),
    ) -> Result<Vec<u8>, T::Error> {
        let outcome = self.exchange(command, body_of);
        self.note(&outcome);
        outcome
    }

    fn note<V>(&mut self, outcome: &Result<V, T::Error>) {
        if let Err(error) = outcome {
            self.broken |= !matches!(error, Error::Refused(_));
        }
    }

    fn exchange(
        &mut self,
        command: u32,
        body_of: impl FnOnce(&mut Writer),
    ) -> Result<Vec<u8>, T::Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let tag = self.next_tag;
        self.next_tag = match tag.wrapping_add(1) {
            EVENT_TAG => 0,
            next => next,
        };
        let mut writer = Writer::new();
        writer.u32(command).u32(tag);
        body_of(&mut writer);
        self.transport
            .send(&wire::control_frame(writer.as_bytes()))
            .map_err(Error::Transport)?;
        let mut waited = 0;
        loop {
            let packet = self.next_packet_within(&mut waited)?;
            let mut reader = Reader::new(&packet);
            let (answer, answered) = (reader.u32()?, reader.u32()?);
            match answer {
                command::SUBSCRIBE_EVENT if answered == EVENT_TAG && self.subscribed => {
                    parse_event(&mut reader)?;
                    self.missed_events = true;
                }
                command::REPLY if answered == tag => return Ok(packet),
                command::ERROR if answered == tag => {
                    let code = reader.u32()?;
                    reader.finish()?;
                    return Err(Error::Refused(code));
                }
                _ => {
                    return Err(Error::Unexpected {
                        command: answer,
                        tag: answered,
                    })
                }
            }
        }
    }

    fn next_packet(&mut self) -> Result<Vec<u8>, T::Error> {
        self.next_packet_within(&mut 0)
    }

    /// The next control packet, counting what it took into `waited`.
    fn next_packet_within(&mut self, waited: &mut usize) -> Result<Vec<u8>, T::Error> {
        loop {
            match self.decoder.next_frame() {
                Some(Ok(Frame::Control(packet))) => return Ok(packet),
                Some(Ok(_)) => return Err(Error::UnexpectedData),
                Some(Err(error)) => return Err(Error::Framing(error)),
                None => {}
            }
            if self.decoder.buffered() > MAX_PENDING_BYTES {
                return Err(Error::UnexpectedData);
            }
            if *waited > MAX_WAIT_BYTES {
                return Err(Error::Flooded);
            }
            let mut buffer = [0u8; READ_CHUNK];
            let read = self
                .transport
                .receive(&mut buffer)
                .map_err(Error::Transport)?;
            match buffer.get(..read) {
                Some([]) | None => return Err(Error::Closed),
                Some(bytes) => {
                    *waited = waited.saturating_add(bytes.len());
                    self.decoder.push(bytes);
                }
            }
        }
    }
}

/// A reply's reader, past its command and tag.
fn body<E>(packet: &[u8]) -> Result<Reader<'_>, E> {
    let mut reader = Reader::new(packet);
    reader.u32()?;
    reader.u32()?;
    Ok(reader)
}

fn parse_event(reader: &mut Reader<'_>) -> tag::Result<Event> {
    let word = reader.u32()?;
    let index = reader.u32()?;
    reader.finish()?;
    Ok(Event {
        facility: word & proto::subscription::EVENT_FACILITY_MASK,
        kind: word & proto::subscription::EVENT_TYPE_MASK,
        index,
    })
}

/// A version-35 sink entry: what td-audio's server writes and libpulse's
/// `pactl list sinks` prints.
fn parse_sink(reader: &mut Reader<'_>) -> tag::Result<Sink> {
    let index = reader.u32()?;
    let name = reader.string()?;
    let description = reader.string()?;
    reader.sample_spec()?;
    reader.channel_map()?;
    reader.u32()?;
    let volume = reader.cvolume()?;
    let muted = reader.boolean()?;
    // Monitor index and name, latency, driver, flags.
    reader.u32()?;
    reader.string()?;
    reader.usec()?;
    reader.string()?;
    reader.u32()?;
    // Version 13: properties and configured latency.
    reader.proplist()?;
    reader.usec()?;
    // Version 15: base volume, state, volume steps, card.
    reader.volume()?;
    reader.u32()?;
    reader.u32()?;
    reader.u32()?;
    // Version 16: ports, each a name, a description and a priority, then
    // its availability (24) and availability group and type (34); then the
    // active port. Each port consumes bytes, so the count bounds nothing.
    let ports = reader.u32()?;
    for _ in 0..ports {
        reader.string()?;
        reader.string()?;
        reader.u32()?;
        reader.u32()?;
        reader.string()?;
        reader.u32()?;
    }
    reader.string()?;
    // Version 21: formats.
    let formats = reader.u8()?;
    for _ in 0..formats {
        reader.format_info()?;
    }
    Ok(Sink {
        index,
        name,
        description,
        volume,
        muted,
    })
}

/// A version-35 sink-input entry, the shape `pactl list sink-inputs` prints.
fn parse_sink_input(reader: &mut Reader<'_>) -> tag::Result<SinkInput> {
    let index = reader.u32()?;
    let name = reader.string()?;
    reader.u32()?;
    let client = reader.u32()?;
    let sink = reader.u32()?;
    reader.sample_spec()?;
    reader.channel_map()?;
    let volume = reader.cvolume()?;
    // Buffer and sink latency, resample method, driver.
    reader.usec()?;
    reader.usec()?;
    reader.string()?;
    reader.string()?;
    let muted = reader.boolean()?;
    let properties = reader.proplist()?;
    // Corked, has volume, volume writable, format.
    reader.boolean()?;
    reader.boolean()?;
    reader.boolean()?;
    reader.format_info()?;
    Ok(SinkInput {
        index,
        name,
        client,
        sink,
        volume,
        muted,
        properties,
    })
}

/// A version-35 client entry, the shape `pactl list clients` prints.
fn parse_client(reader: &mut Reader<'_>) -> tag::Result<ClientInfo> {
    let index = reader.u32()?;
    let name = reader.string()?;
    reader.u32()?;
    reader.string()?;
    let properties = reader.proplist()?;
    Ok(ClientInfo {
        index,
        name,
        properties,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::proto::tests::{captured, captured_17, unhex};
    use crate::proto::{error, subscription};
    use crate::tag::tests::captured as captured_session;

    /// A server that answers from a script and records what it was sent.
    struct Scripted {
        incoming: Vec<u8>,
        at: usize,
        /// The most one `receive` returns, so frames arrive in pieces.
        chunk: usize,
        sent: Vec<u8>,
    }

    impl Scripted {
        fn new(incoming: Vec<u8>) -> Self {
            Self {
                incoming,
                at: 0,
                chunk: usize::MAX,
                sent: Vec::new(),
            }
        }
    }

    impl Transport for Scripted {
        type Error = ();
        fn send(&mut self, bytes: &[u8]) -> std::result::Result<(), ()> {
            self.sent.extend_from_slice(bytes);
            Ok(())
        }
        fn receive(&mut self, buffer: &mut [u8]) -> std::result::Result<usize, ()> {
            let left = &self.incoming[self.at..];
            let count = left.len().min(buffer.len()).min(self.chunk);
            buffer[..count].copy_from_slice(&left[..count]);
            self.at += count;
            Ok(count)
        }
    }

    fn packet(command: u32, tag: u32, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u32(command).u32(tag);
        body(&mut writer);
        wire::control_frame(writer.as_bytes())
    }

    fn reply(tag: u32, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
        packet(command::REPLY, tag, body)
    }

    /// What td-audio's server answers AUTH and SET_CLIENT_NAME with.
    fn handshake() -> Vec<u8> {
        let mut bytes = reply(0, |writer| {
            writer.u32(proto::VERSION);
        });
        bytes.extend(reply(1, |writer| {
            writer.u32(7);
        }));
        bytes
    }

    fn connected(script: Vec<u8>) -> Client<Scripted> {
        let mut incoming = handshake();
        incoming.extend(script);
        Client::connect(Scripted::new(incoming), "td-audio").unwrap()
    }

    /// Every control packet the client sent, after the handshake.
    fn sent_after_handshake(client: &Client<Scripted>) -> Vec<Vec<u8>> {
        let mut decoder = Decoder::new();
        decoder.push(&client.transport.sent);
        let mut packets = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            match frame.unwrap() {
                Frame::Control(packet) => packets.push(packet),
                other => panic!("the client sent {other:?}"),
            }
        }
        packets.split_off(2)
    }

    fn stereo() -> tag::SampleSpec {
        tag::SampleSpec {
            format: tag::SAMPLE_S16LE,
            channels: 2,
            rate: 48_000,
        }
    }

    /// A version-35 sink entry in the shape td-audio writes.
    fn write_sink(writer: &mut Writer, index: u32, name: &str, volume: u32, muted: bool) {
        writer
            .u32(index)
            .string(name)
            .string("td audio")
            .sample_spec(stereo())
            .channel_map(&[1, 2])
            .u32(tag::INVALID_INDEX)
            .cvolume(&[volume, volume])
            .boolean(muted)
            .u32(tag::INVALID_INDEX)
            .null_string()
            .usec(0)
            .string("td-audio")
            .u32(0)
            .proplist(&[tag::text_property("device.class", "sound")])
            .usec(0)
            .volume(0x10000)
            .u32(1)
            .u32(0x10001)
            .u32(tag::INVALID_INDEX)
            .u32(0)
            .null_string()
            .u8(1)
            .format_info(1, &[]);
    }

    /// A version-35 sink-input entry in the shape td-audio writes.
    fn write_sink_input(writer: &mut Writer, index: u32, name: &str, app: &str) {
        writer
            .u32(index)
            .string(name)
            .u32(tag::INVALID_INDEX)
            .u32(3)
            .u32(0)
            .sample_spec(stereo())
            .channel_map(&[1, 2])
            .cvolume(&[0x4000, 0x8000])
            .usec(200_000)
            .usec(0)
            .string("copy")
            .string("td-audio")
            .boolean(true)
            .proplist(&[
                tag::text_property("media.name", name),
                tag::text_property("application.name", app),
            ])
            .boolean(false)
            .boolean(true)
            .boolean(true)
            .format_info(1, &[]);
    }

    /// The handshake is libpulse's captured AUTH with two differences, both
    /// deliberate: the version word asks for no shared memory or memfd
    /// (`0x23`, not `0xc0000023`), which td's server would clear anyway, and
    /// the cookie is zeros, which it never reads. SET_CLIENT_NAME is
    /// libpulse's shape, the command, the tag and one proplist, carrying
    /// only the application's name.
    #[test]
    fn the_handshake_is_libpulses_minus_shared_memory_and_cookie() {
        let client = connected(Vec::new());
        assert_eq!(client.index(), 7);
        let mut decoder = Decoder::new();
        decoder.push(&client.transport.sent);
        let Some(Ok(Frame::Control(auth))) = decoder.next_frame() else {
            panic!("no AUTH");
        };
        let mut expected = unhex(captured_session::AUTH);
        // 'L' command, 'L' tag, 'L' then the version word's four bytes.
        expected[11..15].copy_from_slice(&proto::VERSION.to_be_bytes());
        // 'x' and a four-byte length, then the cookie.
        expected[20..].fill(0);
        assert_eq!(auth, expected);
        let Some(Ok(Frame::Control(name))) = decoder.next_frame() else {
            panic!("no SET_CLIENT_NAME");
        };
        let captured_name = unhex(captured_session::SET_CLIENT_NAME);
        assert_eq!(name[..11], captured_name[..11], "command, tag, proplist");
        let mut reader = Reader::new(&name);
        reader.u32().unwrap();
        reader.u32().unwrap();
        let properties = reader.proplist().unwrap();
        reader.finish().unwrap();
        assert_eq!(
            properties,
            vec![tag::text_property("application.name", "td-audio")]
        );
    }

    /// Each request is byte for byte the packet libpulse sends for the same
    /// question, at the tag it was captured with.
    #[test]
    fn every_request_is_the_packet_libpulse_sends() {
        type Call = fn(&mut Client<Scripted>);
        let cases: &[(&str, u32, Call)] = &[
            (captured_session::GET_SERVER_INFO, 2, |client| {
                client.server_info().map(drop).unwrap_err();
            }),
            (captured_session::GET_SINK_INFO_LIST, 2, |client| {
                client.sinks().map(drop).unwrap();
            }),
            (captured_17::GET_SINK_INPUT_INFO_LIST, 2, |client| {
                client.sink_inputs().map(drop).unwrap();
            }),
            (captured_17::GET_CLIENT_INFO_LIST, 2, |client| {
                client.clients().map(drop).unwrap();
            }),
            (captured_session::SUBSCRIBE, 2, |client| {
                client.subscribe(subscription::MASK_ALL).unwrap();
            }),
            (captured::GET_SINK_INFO_BY_NAME, 5, |client| {
                client.sink("td-audio").map(drop).unwrap_err();
            }),
            (captured_17::GET_SINK_INFO_BY_NUMBER_NAME, 2, |client| {
                client.sink("0").map(drop).unwrap_err();
            }),
            (captured_17::SET_SINK_VOLUME_BY_INDEX, 2, |client| {
                client.set_sink_volume(0, &[0x8000, 0x4000]).unwrap();
            }),
            (captured_17::SET_SINK_MUTE_BY_INDEX, 3, |client| {
                client.set_sink_mute(0, true).unwrap();
            }),
            (captured_17::SET_SINK_UNMUTE_BY_INDEX, 4, |client| {
                client.set_sink_mute(0, false).unwrap();
            }),
            (captured::SET_SINK_INPUT_VOLUME, 16, |client| {
                client.set_sink_input_volume(0, &[0x8000, 0x8000]).unwrap();
            }),
            (captured::SET_SINK_INPUT_MUTE, 17, |client| {
                client.set_sink_input_mute(0, true).unwrap();
            }),
        ];
        for (capture, tag, call) in cases {
            // An empty reply: the setters and lists accept it; the lookups
            // refuse it, which is beside the point here.
            let mut client = connected(reply(*tag, |_| {}));
            client.set_next_tag(*tag);
            call(&mut client);
            assert_eq!(
                sent_after_handshake(&client),
                vec![unhex(capture)],
                "{capture}"
            );
        }
    }

    #[test]
    fn replies_parse_into_what_the_server_described() {
        let mut script = reply(2, |writer| {
            write_sink(writer, 0, "td-audio", 0x8000, true);
        });
        script.extend(reply(3, |writer| {
            write_sink_input(writer, 4, "a tab", "firefox");
            write_sink_input(writer, 9, "a bell", "td-mixer");
        }));
        script.extend(reply(4, |writer| {
            writer
                .string("pulseaudio")
                .string("16.1")
                .string("td")
                .string("td")
                .sample_spec(stereo())
                .string("td-audio")
                .string("")
                .u32(0)
                .channel_map(&[1, 2]);
        }));
        let mut client = connected(script);
        let sink = client.sink(DEFAULT_SINK).unwrap();
        assert_eq!(
            (
                sink.index,
                sink.name.as_deref(),
                sink.volume.as_slice(),
                sink.muted
            ),
            (0, Some("td-audio"), &[0x8000, 0x8000][..], true)
        );
        let streams = client.sink_inputs().unwrap();
        let streams: Vec<SinkInput> = streams.iter().map(|entry| entry.unwrap()).collect();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].index, 4);
        assert_eq!(streams[0].client, 3);
        assert_eq!(streams[0].property("application.name"), Some("firefox"));
        assert_eq!(streams[1].property("media.name"), Some("a bell"));
        assert_eq!(level(&streams[0].volume), 0x8000);
        assert!(streams[0].muted);
        let server = client.server_info().unwrap();
        assert_eq!(server.default_sink.as_deref(), Some("td-audio"));
    }

    /// A reply that arrives one byte per read is the same reply.
    #[test]
    fn a_reply_in_one_byte_pieces_is_whole() {
        let mut incoming = handshake();
        incoming.extend(reply(2, |writer| {
            write_sink(writer, 0, "td-audio", 0x10000, false);
        }));
        let mut transport = Scripted::new(incoming);
        transport.chunk = 1;
        let mut client = Client::connect(transport, "td-audio").unwrap();
        assert_eq!(client.sink(DEFAULT_SINK).unwrap().volume, vec![0x10000; 2]);
    }

    /// The server closing mid-frame, or before answering, is `Closed`, and
    /// the connection is broken from then on.
    #[test]
    fn a_truncated_reply_is_a_closed_connection() {
        let mut whole = reply(2, |writer| {
            write_sink(writer, 0, "td-audio", 0x10000, false);
        });
        whole.truncate(whole.len() - 3);
        let mut client = connected(whole);
        assert!(matches!(client.sink(DEFAULT_SINK), Err(Error::Closed)));
        assert!(matches!(client.sinks(), Err(Error::Broken)));
        assert!(matches!(client.next_event(), Err(Error::Broken)));
        let mut client = connected(Vec::new());
        assert!(matches!(client.next_event(), Err(Error::Closed)));
    }

    /// A frame over the control bound is refused before its body is read.
    #[test]
    fn an_oversized_frame_is_refused() {
        let length = u32::try_from(wire::CONTROL_MAX + 1).unwrap();
        let oversized = wire::Descriptor::encode(length, wire::CHANNEL_CONTROL, 0, 0).to_vec();
        let mut client = connected(oversized);
        assert!(matches!(
            client.sinks(),
            Err(Error::Framing(wire::Error::ControlTooLarge(_)))
        ));
    }

    /// An entry that breaks the schema ends the walk with its error; the
    /// entry before it is still there, and nothing after is guessed at.
    #[test]
    fn a_malformed_entry_ends_the_list() {
        let mut bytes = reply(2, |writer| {
            write_sink_input(writer, 4, "a tab", "firefox");
            write_sink_input(writer, 5, "b tab", "firefox");
            write_sink_input(writer, 6, "c tab", "firefox");
        });
        // The second entry's name tag: a byte no tagstruct value starts with.
        let at = bytes
            .windows(6)
            .position(|window| window == b"tb tab")
            .unwrap();
        bytes[at] = b'!';
        let mut client = connected(bytes);
        let streams = client.sink_inputs().unwrap();
        let walk: Vec<bool> = streams.iter().map(|entry| entry.is_ok()).collect();
        assert_eq!(walk, vec![true, false]);
    }

    /// A count the bytes cannot back is a truncation, not a long loop.
    #[test]
    fn a_count_the_packet_cannot_back_fails_at_the_end_of_the_packet() {
        let script = reply(2, |writer| {
            writer
                .u32(0)
                .string("td-audio")
                .string("td audio")
                .sample_spec(stereo())
                .channel_map(&[1, 2])
                .u32(0)
                .cvolume(&[0x10000, 0x10000])
                .boolean(false)
                .u32(0)
                .null_string()
                .usec(0)
                .string("td-audio")
                .u32(0)
                .proplist(&[])
                .usec(0)
                .volume(0x10000)
                .u32(0)
                .u32(0)
                .u32(0)
                .u32(u32::MAX);
        });
        let mut client = connected(script);
        let sinks = client.sinks().unwrap();
        let walk: Vec<bool> = sinks.iter().map(|entry| entry.is_ok()).collect();
        assert_eq!(walk, vec![false]);
    }

    /// A refusal carries its code and leaves the connection usable: the
    /// server answered, so the framing and the tags are still known.
    #[test]
    fn a_refusal_carries_its_code() {
        let mut script = packet(command::ERROR, 2, |writer| {
            writer.u32(error::NOENTITY);
        });
        script.extend(reply(3, |_| {}));
        let mut client = connected(script);
        assert!(matches!(
            client.sink("nobody"),
            Err(Error::Refused(error::NOENTITY))
        ));
        client.set_sink_mute(0, true).unwrap();
    }

    /// Only an answer to the request in flight answers it. A stale tag, a
    /// stream command a control client never asked for, audio data and a
    /// reply where an event belongs are each an error.
    #[test]
    fn what_was_not_asked_for_is_refused() {
        let mut client = connected(reply(9, |_| {}));
        assert!(matches!(
            client.subscribe(subscription::MASK_ALL),
            Err(Error::Unexpected {
                command: command::REPLY,
                tag: 9
            })
        ));
        let mut client = connected(packet(command::REQUEST, EVENT_TAG, |writer| {
            writer.u32(0).u32(4096);
        }));
        assert!(matches!(
            client.subscribe(subscription::MASK_ALL),
            Err(Error::Unexpected {
                command: command::REQUEST,
                ..
            })
        ));
        let mut data = wire::Descriptor::encode(4, 0, 0, 0).to_vec();
        data.extend([0; 4]);
        let mut client = connected(data);
        assert!(matches!(client.sinks(), Err(Error::UnexpectedData)));
        let mut client = connected(reply(2, |_| {}));
        assert!(matches!(
            client.next_event(),
            Err(Error::Unexpected {
                command: command::REPLY,
                tag: 2
            })
        ));
    }

    fn event(word: u32, index: u32) -> Vec<u8> {
        packet(command::SUBSCRIBE_EVENT, EVENT_TAG, |writer| {
            writer.u32(word).u32(index);
        })
    }

    /// An event that lands while a request waits is dropped and remembered,
    /// and the reply behind it still answers the request.
    #[test]
    fn an_event_during_a_request_is_noted_and_the_reply_still_lands() {
        let change = subscription::EVENT_SINK | subscription::EVENT_CHANGE;
        let mut script = reply(2, |_| {});
        script.extend(event(change, 0));
        script.extend(reply(3, |_| {}));
        script.extend(event(
            subscription::EVENT_SINK_INPUT | subscription::EVENT_REMOVE,
            5,
        ));
        let mut client = connected(script);
        client.subscribe(subscription::MASK_ALL).unwrap();
        assert!(!client.take_missed_events());
        client.set_sink_mute(0, true).unwrap();
        assert!(client.take_missed_events());
        assert!(!client.take_missed_events());
        assert_eq!(
            client.next_event().unwrap(),
            Event {
                facility: subscription::EVENT_SINK_INPUT,
                kind: subscription::EVENT_REMOVE,
                index: 5
            }
        );
    }

    /// Events are for a connection that asked for them. Before SUBSCRIBE,
    /// the handshake's included, one is a protocol violation.
    #[test]
    fn an_event_nobody_subscribed_to_is_refused() {
        let mut client = connected(event(subscription::EVENT_SINK, 0));
        assert!(matches!(
            client.sinks(),
            Err(Error::Unexpected {
                command: command::SUBSCRIBE_EVENT,
                ..
            })
        ));
        let mut incoming = event(subscription::EVENT_SINK, 0);
        incoming.extend(handshake());
        assert!(matches!(
            Client::connect(Scripted::new(incoming), "td-audio"),
            Err(Error::Unexpected { .. })
        ));
    }

    /// A server that keeps sending without answering — events, or the empty
    /// control frames the decoder skips — cannot hold a request or an event
    /// wait forever: past `MAX_WAIT_BYTES` the wait fails.
    #[test]
    fn a_flood_cannot_hold_a_wait() {
        let one = event(subscription::EVENT_SINK, 0);
        let mut script = reply(2, |_| {});
        for _ in 0..(2 * MAX_WAIT_BYTES / one.len()) {
            script.extend(&one);
        }
        script.extend(reply(3, |_| {}));
        let mut client = connected(script);
        client.subscribe(subscription::MASK_ALL).unwrap();
        assert!(matches!(client.set_sink_mute(0, true), Err(Error::Flooded)));

        let empty = wire::control_frame(&[]);
        let mut flood = Vec::new();
        for _ in 0..(2 * MAX_WAIT_BYTES / empty.len()) {
            flood.extend(&empty);
        }
        let mut client = connected(flood.clone());
        assert!(matches!(client.sinks(), Err(Error::Flooded)));
        let mut subscribed = reply(2, |_| {});
        subscribed.extend(flood);
        let mut client = connected(subscribed);
        client.subscribe(subscription::MASK_ALL).unwrap();
        assert!(matches!(client.next_event(), Err(Error::Flooded)));
    }

    /// Audio data is refused once more of it has arrived than any control
    /// frame could hold, not buffered to the data bound first.
    #[test]
    fn audio_data_is_refused_before_it_is_buffered() {
        let length = u32::try_from(wire::DATA_MAX).unwrap();
        let mut data = wire::Descriptor::encode(length, 0, 0, 0).to_vec();
        data.resize(data.len() + wire::DATA_MAX, 0);
        let incoming_len = handshake().len() + data.len();
        let mut client = connected(data);
        assert!(matches!(client.sinks(), Err(Error::UnexpectedData)));
        let read = client.transport.at;
        assert!(
            read <= handshake().len() + MAX_PENDING_BYTES + READ_CHUNK,
            "{read} of {incoming_len} bytes read"
        );
    }

    /// Tags count up from the handshake's and skip the one events carry.
    #[test]
    fn the_tag_wraps_past_the_event_tag() {
        let mut script = reply(EVENT_TAG - 1, |_| {});
        script.extend(reply(0, |_| {}));
        let mut client = connected(script);
        client.set_next_tag(EVENT_TAG - 1);
        client.set_sink_mute(0, true).unwrap();
        client.set_sink_mute(0, false).unwrap();
        let tags: Vec<u32> = sent_after_handshake(&client)
            .iter()
            .map(|packet| wire::command_and_tag(packet).unwrap().1)
            .collect();
        assert_eq!(tags, vec![EVENT_TAG - 1, 0]);
    }

    /// Every whole percent survives the trip to a volume and back, so a step
    /// from a level that was read lands where it says; a reported level over
    /// the norm shows as the most there is.
    /// One level on as many channels as the cvolume it copies, at least one
    /// and at most `CHANNELS_MAX`.
    #[test]
    fn levels_like_copies_the_channel_count() {
        let mut levels = [0; tag::CHANNELS_MAX];
        assert_eq!(levels_like(&mut levels, &[1, 2], 7), [7, 7]);
        assert_eq!(levels_like(&mut levels, &[], 7), [7]);
        let wide = [0; tag::CHANNELS_MAX + 3];
        assert_eq!(levels_like(&mut levels, &wide, 7).len(), tag::CHANNELS_MAX);
    }

    #[test]
    fn a_percent_and_its_volume_round_trip() {
        for value in 0..=MAX_PERCENT {
            assert_eq!(percent(volume_of(value)), value);
        }
        assert_eq!(volume_of(100), VOLUME_NORM);
        assert_eq!(volume_of(1_000), VOLUME_NORM);
        assert_eq!(percent(VOLUME_NORM / 2), 50);
        assert_eq!(percent(u32::MAX), MAX_PERCENT);
    }

    #[test]
    fn an_older_server_is_refused() {
        let incoming = reply(0, |writer| {
            writer.u32(34);
        });
        assert!(matches!(
            Client::connect(Scripted::new(incoming), "td-audio"),
            Err(Error::Unsupported(34))
        ));
    }
}
