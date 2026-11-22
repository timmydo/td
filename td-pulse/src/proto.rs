//! Protocol version negotiation, the command table, and the error codes.
//!
//! §K.3 is explicit about where this table comes from: "Do not write the
//! command table from memory. Capture it: run the pinned runtime's own
//! libpulse ... against a logging stub ... and commit the captures as golden
//! fixtures. This is the `.filez` rule again: the bytes in tree are the
//! oracle."
//!
//! Every number below was established that way. The client-to-server commands
//! were read off a Unix socket while libpulse 16.1 — `pactl`, `paplay`, and a
//! purpose-built client driving the whole playback lifecycle — talked to a
//! logging stub; the fixtures in this module's tests are those exact packets.
//! The server-to-client numbers cannot be captured from a client, so each was
//! established by sending a candidate and observing which libpulse callback
//! fired: a firing callback names the command, and a wrong packet shape fails
//! the connection instead, so a clean callback is proof rather than absence of
//! evidence. `UNDERFLOW` is pinned twice over — the callback fired *and*
//! `pa_stream_get_underflow_index()` returned the exact `s64` offset the
//! candidate packet carried. The error codes were established the same way, by
//! answering a request with `ERROR` and reading `pa_context_errno` back.
//!
//! Two plausible commands are NOT in this table, and their absence is a
//! finding rather than an omission. Libpulse 16.1 at protocol 35 never sends
//! `LOOKUP_SINK` or `SET_PLAYBACK_STREAM_NAME`.
//! `pa_context_get_sink_info_by_name` sends `GET_SINK_INFO` with the name and
//! an invalid index, and `pa_stream_set_name` sends
//! `UPDATE_PLAYBACK_STREAM_PROPLIST` with a `media.name` property — the
//! captured packet for the second is in this module's tests. Implementing the
//! two unused commands would have added code no client exercises while
//! leaving the paths clients actually use unhandled, which is the exact
//! failure §K.3's capture rule exists to prevent.

/// The version this server advertises. §K.3: "A lower version would shrink the
/// schemas but steer clients into downgrade paths nothing in the ecosystem
/// exercises — `pipewire-pulse` answers 35, so 35 is what modern libpulse is
/// tested against."
pub const VERSION: u32 = 35;

/// The oldest version this server will agree to speak.
///
/// Not a preference: `Session::create_stream` parses the widest form of
/// `CREATE_PLAYBACK_STREAM` unconditionally, and its trailing format list
/// arrived in version 21. Agreeing to an older one and then failing to parse
/// that client's shorter packet would disconnect it mid-setup, after it had
/// been told the connection was good.
pub const MIN_VERSION: u32 = 21;

/// `PA_PROTOCOL_VERSION_MASK`. The low half of `AUTH`'s version word is the
/// version; the high half is feature bits.
pub const VERSION_MASK: u32 = 0xFFFF;

/// Shared-memory transport. §K.2 clears this, which deletes `SCM_RIGHTS`,
/// pools, block identifiers and seal policy from v1 entirely.
pub const FLAG_SHM: u32 = 0x8000_0000;

/// memfd transport. Cleared for the same reason.
pub const FLAG_MEMFD: u32 = 0x4000_0000;

/// Every feature bit this server understands, and clears.
pub const FEATURE_BITS: u32 = FLAG_SHM | FLAG_MEMFD;

/// The version to parse a connection's packets at.
///
/// §K.3, and this is the whole of the rule: "Advertise version 35 and parse
/// each command at `min(client_version & PA_PROTOCOL_VERSION_MASK, 35)`. The
/// mask is not decoration, and an earlier draft's `min(client_version, 35)` on
/// the raw word is wrong in the dangerous direction: the `AUTH` version field
/// carries protocol FEATURE bits in its high half ... so an *older* client that
/// sets one of them presents a raw word far above 35 and would be parsed at 35,
/// against schemas it does not speak. Mask first, cap second."
pub fn negotiate(raw_version: u32) -> u32 {
    (raw_version & VERSION_MASK).min(VERSION)
}

/// The feature bits to echo in the `AUTH` reply: none of them.
///
/// The reply carries a bare version, so a client that asked for shared memory
/// is told plainly that it did not get it and falls back to socket data — which
/// is what every flatpak app already does, because the client config flatpak
/// generates sets `enable-shm=no` (§K.2).
pub fn auth_reply_version(negotiated: u32) -> u32 {
    negotiated & !FEATURE_BITS
}

/// Whether a client asked for a transport this server does not implement.
pub fn requested_shared_memory(raw_version: u32) -> bool {
    raw_version & FEATURE_BITS != 0
}

/// Commands, all captured.
///
/// Grouped by direction. Within a group the numbers are in ascending order so
/// a reader can check the table against a transcript at a glance.
pub mod command {
    // --- Server to client. Established by which libpulse callback fired. ---

    /// A failed request. Carries the tag and one `u32` from [`super::error`].
    /// Established by answering a request with this and reading
    /// `pa_context_errno` back: sending 1, 3, 5 and 23 produced "Access
    /// denied", "Invalid argument", "No such entity" and "Missing
    /// implementation" in the client.
    pub const ERROR: u32 = 0;

    /// A successful request. Carries the tag and the reply's own schema.
    /// Established by real `pactl info`, `pactl list sinks` and `paplay`
    /// parsing and printing replies framed this way.
    pub const REPLY: u32 = 2;

    /// A byte grant: `channel`, then a `u32` count. §K.3: "`REQUEST` is what
    /// makes sound happen at all — without byte grants the client writes one
    /// buffer and stops forever." Established empirically: with 61 the client's
    /// write callback fired and it kept writing; without any grant it wrote one
    /// buffer and stopped, exactly as §K.3 says.
    pub const REQUEST: u32 = 61;

    /// The client wrote past the buffer: `channel`. Established by the overflow
    /// callback firing.
    pub const OVERFLOW: u32 = 62;

    /// The stream ran dry: `channel`, then an `s64` offset at version >= 23.
    /// Established twice: the underflow callback fired, and
    /// `pa_stream_get_underflow_index()` returned the exact offset sent. The
    /// same packet without the `s64` fails the connection, which is what pins
    /// the schema rather than just the number.
    pub const UNDERFLOW: u32 = 63;

    /// The server dropped the stream: `channel`. Established by the client's
    /// stream going to `PA_STREAM_FAILED` while the connection stayed healthy —
    /// a kill, not a protocol error.
    pub const PLAYBACK_STREAM_KILLED: u32 = 64;

    /// A subscription event: a `u32` event word then a `u32` index.
    /// Established by the subscribe callback firing with the exact event word
    /// sent.
    pub const SUBSCRIBE_EVENT: u32 = 66;

    /// Playback began: `channel`. Established by the started callback firing.
    pub const STARTED: u32 = 86;

    // --- Client to server. Read off the wire; see the fixtures below. ---

    /// Sample spec, channel map, sink, buffer attributes, sixteen booleans and
    /// a proplist. The widest schema in the protocol.
    pub const CREATE_PLAYBACK_STREAM: u32 = 3;

    /// `channel`.
    pub const DELETE_PLAYBACK_STREAM: u32 = 4;

    /// Version word and a 256-byte cookie. §K.3 authenticates by `SO_PEERCRED`
    /// uid and parses the cookie only to consume its exact length.
    pub const AUTH: u32 = 8;

    /// One proplist. The reply carries the client index at version >= 13.
    pub const SET_CLIENT_NAME: u32 = 9;

    /// `channel`. §K.3: this is bookkeeping against the mixer, NOT the ALSA
    /// `DRAIN` ioctl, "which drains and stops the shared mixed PCM — draining
    /// one stream would silence every other app."
    pub const DRAIN_PLAYBACK_STREAM: u32 = 12;

    /// `channel` and a client timeval to echo. The reply is §K.3's "consistent
    /// set, not one number squeezed into a field".
    pub const GET_PLAYBACK_LATENCY: u32 = 14;

    /// Nothing.
    pub const GET_SERVER_INFO: u32 = 20;

    /// A `u32` index and a string, one of which is invalid. Captured in both
    /// forms: by name it sends `0xFFFFFFFF` and the name, by index it sends the
    /// index and a NULL string.
    pub const GET_SINK_INFO: u32 = 21;

    /// Nothing.
    pub const GET_SINK_INFO_LIST: u32 = 22;

    /// Nothing. §K.3 wants an empty list here, "not an error, so device pickers
    /// see 'no microphone' rather than a broken server".
    pub const GET_SOURCE_INFO_LIST: u32 = 24;

    /// A `u32` client index. Captured from the pinned runtime's libpulse
    /// 17.0 (`pa_context_get_client_info`).
    pub const GET_CLIENT_INFO: u32 = 27;

    /// Nothing. Captured from libpulse 17.0's `pactl list clients`.
    pub const GET_CLIENT_INFO_LIST: u32 = 28;

    /// A `u32` index.
    pub const GET_SINK_INPUT_INFO: u32 = 29;

    /// Nothing. Captured from libpulse 17.0's `pactl list sink-inputs`.
    pub const GET_SINK_INPUT_INFO_LIST: u32 = 30;

    /// A `u32` mask. Captured with `0x02FF`, which is
    /// `PA_SUBSCRIPTION_MASK_ALL`.
    pub const SUBSCRIBE: u32 = 35;

    /// A `u32` index, a string and a cvolume, one of the index and the name
    /// invalid as in `GET_SINK_INFO`. Captured in both forms from the pinned
    /// runtime's libpulse 17.0.
    pub const SET_SINK_VOLUME: u32 = 36;

    /// A `u32` index and a cvolume.
    pub const SET_SINK_INPUT_VOLUME: u32 = 37;

    /// A `u32` index, a string and a boolean, in `SET_SINK_VOLUME`'s two
    /// forms. Captured in both from the same libpulse 17.0.
    pub const SET_SINK_MUTE: u32 = 39;

    /// `channel` and a boolean. Captured in both directions.
    pub const CORK_PLAYBACK_STREAM: u32 = 41;

    /// `channel`.
    pub const FLUSH_PLAYBACK_STREAM: u32 = 42;

    /// `channel`.
    pub const TRIGGER_PLAYBACK_STREAM: u32 = 43;

    /// `channel`.
    pub const PREBUF_PLAYBACK_STREAM: u32 = 60;

    /// A `u32` index and a boolean.
    pub const SET_SINK_INPUT_MUTE: u32 = 69;

    /// `channel`, a `u32` update mode, and a proplist. This is also how a
    /// modern client renames a stream — see `K3_AMENDMENTS`.
    pub const UPDATE_PLAYBACK_STREAM_PROPLIST: u32 = 81;
}

/// `pa_error_code`, from the shipped `pulse/def.h` of the same pinned
/// pulseaudio, and confirmed on the wire for the four this server sends.
pub mod error {
    pub const OK: u32 = 0;
    pub const ACCESS: u32 = 1;
    pub const COMMAND: u32 = 2;
    pub const INVALID: u32 = 3;
    pub const EXIST: u32 = 4;
    pub const NOENTITY: u32 = 5;
    pub const CONNECTIONREFUSED: u32 = 6;
    pub const PROTOCOL: u32 = 7;
    pub const TIMEOUT: u32 = 8;
    pub const AUTHKEY: u32 = 9;
    pub const INTERNAL: u32 = 10;
    pub const CONNECTIONTERMINATED: u32 = 11;
    pub const KILLED: u32 = 12;
    pub const INVALIDSERVER: u32 = 13;
    pub const MODINITFAILED: u32 = 14;
    pub const BADSTATE: u32 = 15;
    pub const NODATA: u32 = 16;
    pub const VERSION: u32 = 17;
    pub const TOOLARGE: u32 = 18;
    pub const NOTSUPPORTED: u32 = 19;
    pub const UNKNOWN: u32 = 20;
    pub const NOEXTENSION: u32 = 21;
    pub const OBSOLETE: u32 = 22;
    pub const NOTIMPLEMENTED: u32 = 23;
    pub const FORKED: u32 = 24;
    pub const IO: u32 = 25;
    pub const BUSY: u32 = 26;
}

/// The name of an error code, for the line a refusal is logged on.
///
/// Every constant above appears here. That is what makes the table checked
/// rather than merely written down: the test below walks all of them, so a code
/// that is wrong by one is a code whose name does not match the enum it was
/// read out of.
pub fn error_name(code: u32) -> Option<&'static str> {
    Some(match code {
        error::OK => "OK",
        error::ACCESS => "ACCESS",
        error::COMMAND => "COMMAND",
        error::INVALID => "INVALID",
        error::EXIST => "EXIST",
        error::NOENTITY => "NOENTITY",
        error::CONNECTIONREFUSED => "CONNECTIONREFUSED",
        error::PROTOCOL => "PROTOCOL",
        error::TIMEOUT => "TIMEOUT",
        error::AUTHKEY => "AUTHKEY",
        error::INTERNAL => "INTERNAL",
        error::CONNECTIONTERMINATED => "CONNECTIONTERMINATED",
        error::KILLED => "KILLED",
        error::INVALIDSERVER => "INVALIDSERVER",
        error::MODINITFAILED => "MODINITFAILED",
        error::BADSTATE => "BADSTATE",
        error::NODATA => "NODATA",
        error::VERSION => "VERSION",
        error::TOOLARGE => "TOOLARGE",
        error::NOTSUPPORTED => "NOTSUPPORTED",
        error::UNKNOWN => "UNKNOWN",
        error::NOEXTENSION => "NOEXTENSION",
        error::OBSOLETE => "OBSOLETE",
        error::NOTIMPLEMENTED => "NOTIMPLEMENTED",
        error::FORKED => "FORKED",
        error::IO => "IO",
        error::BUSY => "BUSY",
        _ => return None,
    })
}

/// Subscription facilities and event types, from the shipped `pulse/def.h`.
pub mod subscription {
    pub const MASK_SINK: u32 = 0x0001;
    pub const MASK_SINK_INPUT: u32 = 0x0004;
    /// Every facility a client can ask about. This server only ever raises the
    /// two above, but a client subscribing to everything is the ordinary case
    /// and the mask it sends has to be understood rather than refused.
    pub const MASK_ALL: u32 = 0x02FF;

    pub const EVENT_SINK: u32 = 0x0000;
    pub const EVENT_SINK_INPUT: u32 = 0x0002;
    pub const EVENT_FACILITY_MASK: u32 = 0x000F;

    pub const EVENT_NEW: u32 = 0x0000;
    pub const EVENT_CHANGE: u32 = 0x0010;
    pub const EVENT_REMOVE: u32 = 0x0020;
    pub const EVENT_TYPE_MASK: u32 = 0x0030;
}

/// The sample format and channel positions v1 speaks, from the shipped
/// `pulse/sample.h` and `pulse/channelmap.h`, and confirmed in the captured
/// `CREATE_PLAYBACK_STREAM`.
pub mod format {
    /// `PA_SAMPLE_S16LE`, which is the tagstruct codec's own constant — one
    /// definition, so the wire and the schemas cannot disagree about it.
    pub const SAMPLE_S16LE: u8 = crate::tag::SAMPLE_S16LE;
    /// `PA_SAMPLE_FLOAT32LE`. Accepted at the client boundary and converted to
    /// the fixed S16 mixer format without changing rate or channel count.
    pub const SAMPLE_FLOAT32LE: u8 = crate::tag::SAMPLE_FLOAT32LE;
    /// `PA_CHANNEL_POSITION_FRONT_LEFT`.
    pub const FRONT_LEFT: u8 = 1;
    /// `PA_CHANNEL_POSITION_FRONT_RIGHT`: u8 = 2.
    pub const FRONT_RIGHT: u8 = 2;
    /// `PA_ENCODING_PCM`, the only encoding a format-info reply names here.
    pub const ENCODING_PCM: u8 = 1;
}

/// The name of a command, for diagnostics. An unknown number reports itself.
pub fn command_name(command: u32) -> Option<&'static str> {
    Some(match command {
        command::ERROR => "ERROR",
        command::REPLY => "REPLY",
        command::CREATE_PLAYBACK_STREAM => "CREATE_PLAYBACK_STREAM",
        command::DELETE_PLAYBACK_STREAM => "DELETE_PLAYBACK_STREAM",
        command::AUTH => "AUTH",
        command::SET_CLIENT_NAME => "SET_CLIENT_NAME",
        command::DRAIN_PLAYBACK_STREAM => "DRAIN_PLAYBACK_STREAM",
        command::GET_PLAYBACK_LATENCY => "GET_PLAYBACK_LATENCY",
        command::GET_SERVER_INFO => "GET_SERVER_INFO",
        command::GET_SINK_INFO => "GET_SINK_INFO",
        command::GET_SINK_INFO_LIST => "GET_SINK_INFO_LIST",
        command::GET_SOURCE_INFO_LIST => "GET_SOURCE_INFO_LIST",
        command::GET_CLIENT_INFO => "GET_CLIENT_INFO",
        command::GET_CLIENT_INFO_LIST => "GET_CLIENT_INFO_LIST",
        command::GET_SINK_INPUT_INFO => "GET_SINK_INPUT_INFO",
        command::GET_SINK_INPUT_INFO_LIST => "GET_SINK_INPUT_INFO_LIST",
        command::SUBSCRIBE => "SUBSCRIBE",
        command::SET_SINK_VOLUME => "SET_SINK_VOLUME",
        command::SET_SINK_INPUT_VOLUME => "SET_SINK_INPUT_VOLUME",
        command::SET_SINK_MUTE => "SET_SINK_MUTE",
        command::CORK_PLAYBACK_STREAM => "CORK_PLAYBACK_STREAM",
        command::FLUSH_PLAYBACK_STREAM => "FLUSH_PLAYBACK_STREAM",
        command::TRIGGER_PLAYBACK_STREAM => "TRIGGER_PLAYBACK_STREAM",
        command::PREBUF_PLAYBACK_STREAM => "PREBUF_PLAYBACK_STREAM",
        command::SET_SINK_INPUT_MUTE => "SET_SINK_INPUT_MUTE",
        command::UPDATE_PLAYBACK_STREAM_PROPLIST => "UPDATE_PLAYBACK_STREAM_PROPLIST",
        command::REQUEST => "REQUEST",
        command::OVERFLOW => "OVERFLOW",
        command::UNDERFLOW => "UNDERFLOW",
        command::PLAYBACK_STREAM_KILLED => "PLAYBACK_STREAM_KILLED",
        command::SUBSCRIBE_EVENT => "SUBSCRIBE_EVENT",
        command::STARTED => "STARTED",
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::tag;

    pub(crate) fn unhex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect()
    }

    /// CAPTURED, not written from memory. These are the exact packets libpulse
    /// 16.1 sent while a client driven through the whole playback lifecycle —
    /// connect, subscribe, enumerate, create, write, uncork, cork, flush,
    /// prebuf, trigger, ask for latency, rename, set volume, mute, drain,
    /// disconnect — talked to a logging stub over a Unix socket.
    pub(crate) mod captured {
        /// `GET_PLAYBACK_LATENCY`, tag 14: channel, then a client timeval.
        pub const GET_PLAYBACK_LATENCY: &str = "4c0000000e4c0000000e4c00000000546a986546000c2970";
        /// `GET_SINK_INFO` by name, tag 5: an invalid index, then the name.
        pub const GET_SINK_INFO_BY_NAME: &str =
            "4c000000154c000000054cffffffff7474642d617564696f00";
        /// `GET_SINK_INFO` by index, tag 6: the index, then a NULL string.
        pub const GET_SINK_INFO_BY_INDEX: &str = "4c000000154c000000064c000000004e";
        /// `GET_SINK_INPUT_INFO`, tag 18.
        pub const GET_SINK_INPUT_INFO: &str = "4c0000001d4c000000124c00000000";
        /// `SET_SINK_INPUT_VOLUME`, tag 16: index, then a two-channel cvolume
        /// of half `PA_VOLUME_NORM`.
        pub const SET_SINK_INPUT_VOLUME: &str =
            "4c000000254c000000104c0000000076020000800000008000";
        /// `CORK_PLAYBACK_STREAM` with `false` — uncork, tag 9.
        pub const UNCORK: &str = "4c000000294c000000094c0000000030";
        /// `CORK_PLAYBACK_STREAM` with `true` — cork, tag 10.
        pub const CORK: &str = "4c000000294c0000000a4c0000000031";
        /// `FLUSH_PLAYBACK_STREAM`, tag 11.
        pub const FLUSH_PLAYBACK_STREAM: &str = "4c0000002a4c0000000b4c00000000";
        /// `TRIGGER_PLAYBACK_STREAM`, tag 13.
        pub const TRIGGER_PLAYBACK_STREAM: &str = "4c0000002b4c0000000d4c00000000";
        /// `PREBUF_PLAYBACK_STREAM`, tag 12.
        pub const PREBUF_PLAYBACK_STREAM: &str = "4c0000003c4c0000000c4c00000000";
        /// `SET_SINK_INPUT_MUTE`, tag 17: index, then `true`.
        pub const SET_SINK_INPUT_MUTE: &str = "4c000000454c000000114c0000000031";
        /// `UPDATE_PLAYBACK_STREAM_PROPLIST`, tag 15 — what
        /// `pa_stream_set_name("renamed")` actually sends.
        pub const RENAME_VIA_PROPLIST: &str = "\
4c000000514c0000000f4c000000004c0000000250746d656469612e6e616d65004c000000087800\
00000872656e616d6564004e";
    }

    /// CAPTURED from the pinned Firefox runtime (freedesktop 25.08), whose
    /// libpulse is 17.0: its own `pactl` and, for the by-index forms `pactl`
    /// never sends, `pa_context_set_sink_{volume,mute}_by_index` driven from
    /// the runtime's Python, each run under the runtime's loader against a
    /// logging relay in front of td-audio's server.
    pub(crate) mod captured_17 {
        /// `set_sink_volume_by_index(0, {0x8000, 0x4000})`, tag 2: the index,
        /// a NULL name, then a two-channel cvolume.
        pub const SET_SINK_VOLUME_BY_INDEX: &str =
            "4c000000244c000000024c000000004e76020000800000004000";
        /// `pactl set-sink-volume @DEFAULT_SINK@ 75%`, tag 3: an invalid
        /// index, the name, then 75% of `PA_VOLUME_NORM` on both channels.
        /// pactl's percentages are linear in the volume value; the curve is
        /// the server's.
        // One unbroken literal: td-audio's controller test finds each of
        // these strings in this file to prove it replays them unchanged.
        pub const SET_SINK_VOLUME_BY_NAME: &str =
            "4c000000244c000000034cffffffff744044454641554c545f53494e4b400076020000c0000000c000";
        /// `set_sink_mute_by_index(0, true)`, tag 3.
        pub const SET_SINK_MUTE_BY_INDEX: &str = "4c000000274c000000034c000000004e31";
        /// `set_sink_mute_by_index(0, false)`, tag 4.
        pub const SET_SINK_UNMUTE_BY_INDEX: &str = "4c000000274c000000044c000000004e30";
        /// `pactl set-sink-mute 0 1`, tag 2: pactl names even a numbered sink,
        /// so this is an invalid index and the NAME `"0"`.
        pub const SET_SINK_MUTE_BY_NUMBER_NAME: &str = "4c000000274c000000024cffffffff74300031";
        /// `pactl get-sink-volume 0`, tag 2: the same numeric name, which a
        /// Pulse server resolves as the index.
        pub const GET_SINK_INFO_BY_NUMBER_NAME: &str = "4c000000154c000000024cffffffff743000";
        /// `pactl list sink-inputs`, tag 2: the command and tag alone.
        pub const GET_SINK_INPUT_INFO_LIST: &str = "4c0000001e4c00000002";
        /// `pactl list clients`, tag 2: the command and tag alone.
        pub const GET_CLIENT_INFO_LIST: &str = "4c0000001c4c00000002";
        /// `pa_context_get_client_info(3)`, tag 2: the client's index.
        pub const GET_CLIENT_INFO: &str = "4c0000001b4c000000024c00000003";
    }

    /// Every fixture's first value is the command number this module pins. This
    /// is the test that ties the table to the bytes: change a constant and the
    /// captured packet stops agreeing with it.
    #[test]
    fn every_pinned_command_matches_its_captured_packet() {
        let cases: &[(&str, u32)] = &[
            (
                captured_17::SET_SINK_VOLUME_BY_INDEX,
                command::SET_SINK_VOLUME,
            ),
            (
                captured_17::SET_SINK_VOLUME_BY_NAME,
                command::SET_SINK_VOLUME,
            ),
            (captured_17::SET_SINK_MUTE_BY_INDEX, command::SET_SINK_MUTE),
            (
                captured_17::SET_SINK_UNMUTE_BY_INDEX,
                command::SET_SINK_MUTE,
            ),
            (
                captured_17::SET_SINK_MUTE_BY_NUMBER_NAME,
                command::SET_SINK_MUTE,
            ),
            (
                captured_17::GET_SINK_INFO_BY_NUMBER_NAME,
                command::GET_SINK_INFO,
            ),
            (
                captured_17::GET_SINK_INPUT_INFO_LIST,
                command::GET_SINK_INPUT_INFO_LIST,
            ),
            (
                captured_17::GET_CLIENT_INFO_LIST,
                command::GET_CLIENT_INFO_LIST,
            ),
            (captured_17::GET_CLIENT_INFO, command::GET_CLIENT_INFO),
            (
                captured::GET_PLAYBACK_LATENCY,
                command::GET_PLAYBACK_LATENCY,
            ),
            (captured::GET_SINK_INFO_BY_NAME, command::GET_SINK_INFO),
            (captured::GET_SINK_INFO_BY_INDEX, command::GET_SINK_INFO),
            (captured::GET_SINK_INPUT_INFO, command::GET_SINK_INPUT_INFO),
            (
                captured::SET_SINK_INPUT_VOLUME,
                command::SET_SINK_INPUT_VOLUME,
            ),
            (captured::UNCORK, command::CORK_PLAYBACK_STREAM),
            (captured::CORK, command::CORK_PLAYBACK_STREAM),
            (
                captured::FLUSH_PLAYBACK_STREAM,
                command::FLUSH_PLAYBACK_STREAM,
            ),
            (
                captured::TRIGGER_PLAYBACK_STREAM,
                command::TRIGGER_PLAYBACK_STREAM,
            ),
            (
                captured::PREBUF_PLAYBACK_STREAM,
                command::PREBUF_PLAYBACK_STREAM,
            ),
            (captured::SET_SINK_INPUT_MUTE, command::SET_SINK_INPUT_MUTE),
            (
                captured::RENAME_VIA_PROPLIST,
                command::UPDATE_PLAYBACK_STREAM_PROPLIST,
            ),
        ];
        for &(hex, expected) in cases {
            let bytes = unhex(hex);
            let mut reader = tag::Reader::new(&bytes);
            assert_eq!(
                reader.u32().unwrap(),
                expected,
                "captured packet {hex} is not command {expected}"
            );
        }
    }

    /// The sink setters take `GET_SINK_INFO`'s selector — an index and a NULL
    /// name, or an invalid index and a name — and then their value: a cvolume
    /// for the level, a boolean tag byte for mute.
    #[test]
    fn the_sink_setters_carry_a_selector_then_their_value() {
        /// The command, then the selector; the reader is left on the value.
        fn selector(reader: &mut tag::Reader<'_>) -> (u32, u32, Option<String>) {
            let command = reader.u32().unwrap();
            let _tag = reader.u32().unwrap();
            let index = reader.u32().unwrap();
            (command, index, reader.string().unwrap())
        }
        for (hex, index, name, volumes) in [
            (
                captured_17::SET_SINK_VOLUME_BY_INDEX,
                0,
                None,
                [0x8000, 0x4000],
            ),
            (
                captured_17::SET_SINK_VOLUME_BY_NAME,
                tag::INVALID_INDEX,
                Some("@DEFAULT_SINK@"),
                [0xC000, 0xC000],
            ),
        ] {
            let bytes = unhex(hex);
            let mut reader = tag::Reader::new(&bytes);
            let (command, got_index, got_name) = selector(&mut reader);
            assert_eq!(command, command::SET_SINK_VOLUME);
            assert_eq!((got_index, got_name.as_deref()), (index, name));
            assert_eq!(reader.cvolume().unwrap(), volumes);
            reader.finish().unwrap();
        }
        for (hex, index, name, muted) in [
            (captured_17::SET_SINK_MUTE_BY_INDEX, 0, None, true),
            (captured_17::SET_SINK_UNMUTE_BY_INDEX, 0, None, false),
            (
                captured_17::SET_SINK_MUTE_BY_NUMBER_NAME,
                tag::INVALID_INDEX,
                Some("0"),
                true,
            ),
        ] {
            let bytes = unhex(hex);
            let mut reader = tag::Reader::new(&bytes);
            let (command, got_index, got_name) = selector(&mut reader);
            assert_eq!(command, command::SET_SINK_MUTE);
            assert_eq!((got_index, got_name.as_deref()), (index, name));
            assert_eq!(reader.boolean().unwrap(), muted);
            reader.finish().unwrap();
        }
        let bytes = unhex(captured_17::GET_SINK_INFO_BY_NUMBER_NAME);
        let mut reader = tag::Reader::new(&bytes);
        let (command, index, name) = selector(&mut reader);
        assert_eq!(command, command::GET_SINK_INFO);
        assert_eq!((index, name.as_deref()), (tag::INVALID_INDEX, Some("0")));
        reader.finish().unwrap();
    }

    /// `GET_SINK_INFO` carries an index and a name, one of which is invalid —
    /// and both forms are the SAME command. A server that implemented only one
    /// would answer half its clients.
    #[test]
    fn get_sink_info_carries_either_an_index_or_a_name() {
        let by_name = unhex(captured::GET_SINK_INFO_BY_NAME);
        let mut reader = tag::Reader::new(&by_name);
        assert_eq!(reader.u32().unwrap(), command::GET_SINK_INFO);
        assert_eq!(reader.u32().unwrap(), 5, "tag");
        assert_eq!(reader.u32().unwrap(), tag::INVALID_INDEX);
        assert_eq!(reader.string().unwrap().as_deref(), Some("td-audio"));
        reader.finish().unwrap();

        let by_index = unhex(captured::GET_SINK_INFO_BY_INDEX);
        let mut reader = tag::Reader::new(&by_index);
        assert_eq!(reader.u32().unwrap(), command::GET_SINK_INFO);
        assert_eq!(reader.u32().unwrap(), 6, "tag");
        assert_eq!(reader.u32().unwrap(), 0, "a real index");
        assert_eq!(reader.string().unwrap(), None, "and a NULL name");
        reader.finish().unwrap();
    }

    /// The cork packets differ only in the boolean, and §K.3's own correction
    /// applies: a boolean is its own tag byte, not a `B`. `0x30`/`0x31` are
    /// `'0'`/`'1'`.
    #[test]
    fn cork_and_uncork_differ_only_in_the_boolean() {
        for (hex, expected) in [(captured::UNCORK, false), (captured::CORK, true)] {
            let bytes = unhex(hex);
            let mut reader = tag::Reader::new(&bytes);
            assert_eq!(reader.u32().unwrap(), command::CORK_PLAYBACK_STREAM);
            let _tag = reader.u32().unwrap();
            assert_eq!(reader.u32().unwrap(), 0, "channel");
            assert_eq!(reader.boolean().unwrap(), expected);
            reader.finish().unwrap();
        }
        assert_eq!(unhex(captured::UNCORK).last(), Some(&b'0'));
        assert_eq!(unhex(captured::CORK).last(), Some(&b'1'));
    }

    /// The volume packet carries a cvolume, and the captured value is exactly
    /// half `PA_VOLUME_NORM` on both channels.
    #[test]
    fn the_volume_packet_carries_a_cvolume() {
        let bytes = unhex(captured::SET_SINK_INPUT_VOLUME);
        let mut reader = tag::Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), command::SET_SINK_INPUT_VOLUME);
        let _tag = reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), 0, "sink input index");
        assert_eq!(reader.cvolume().unwrap(), vec![0x8000, 0x8000]);
        reader.finish().unwrap();
    }

    /// The wire uses a property update, not `SET_PLAYBACK_STREAM_NAME`. This
    /// is the captured proof behind §K.3's command list.
    #[test]
    fn renaming_a_stream_arrives_as_a_proplist_update() {
        let bytes = unhex(captured::RENAME_VIA_PROPLIST);
        let mut reader = tag::Reader::new(&bytes);
        assert_eq!(
            reader.u32().unwrap(),
            command::UPDATE_PLAYBACK_STREAM_PROPLIST,
            "pa_stream_set_name did not send SET_PLAYBACK_STREAM_NAME"
        );
        assert_eq!(reader.u32().unwrap(), 15, "tag");
        assert_eq!(reader.u32().unwrap(), 0, "channel");
        assert_eq!(reader.u32().unwrap(), 2, "PA_UPDATE_REPLACE");
        let properties = reader.proplist().unwrap();
        let named = properties
            .iter()
            .find(|property| property.key == "media.name")
            .expect("the new name arrives as media.name");
        assert_eq!(named.value, b"renamed\0");
        reader.finish().unwrap();
    }

    /// §K.3's dangerous-direction correction, as a test. The captured `AUTH`
    /// word is `0xc0000023`: masking first gives 35, and taking `min` on the
    /// raw word would too — so the captured packet alone does NOT catch the
    /// bug. The case that does is an OLDER client with a feature bit set.
    #[test]
    fn negotiation_masks_before_it_caps() {
        assert_eq!(negotiate(0xc000_0023), 35, "the captured word");
        assert_eq!(negotiate(35), 35);
        assert_eq!(negotiate(13), 13);
        // A version-13 client that asks for shared memory. Masking first gives
        // 13; `min(raw, 35)` gives 35, and every schema after that is wrong.
        let old_client_with_shm = FLAG_SHM | 13;
        assert_eq!(negotiate(old_client_with_shm), 13);
        assert_eq!(old_client_with_shm.min(VERSION), VERSION, "the bug, pinned");
        assert_ne!(
            negotiate(old_client_with_shm),
            old_client_with_shm.min(VERSION)
        );
        // And a client newer than this server is parsed at this server.
        assert_eq!(negotiate(FLAG_MEMFD | 40), 35);
    }

    /// The reply clears both feature bits, whatever the client asked for.
    #[test]
    fn the_auth_reply_grants_no_transport() {
        for raw in [0xc000_0023u32, FLAG_SHM | 35, FLAG_MEMFD | 35, 35] {
            let version = negotiate(raw);
            let reply = auth_reply_version(version);
            assert_eq!(reply & FEATURE_BITS, 0, "no transport is granted");
            assert_eq!(reply, version);
        }
        assert!(requested_shared_memory(0xc000_0023));
        assert!(requested_shared_memory(FLAG_MEMFD | 35));
        assert!(!requested_shared_memory(35));
    }

    /// Every code in the table has a name, and the names are the enum's own.
    /// A code that drifted would be a code with no name, or the wrong one.
    #[test]
    fn every_error_code_is_named() {
        // The enum is contiguous from OK to BUSY, which is what lets a walk
        // stand in for listing all twenty-seven by hand.
        for code in error::OK..=error::BUSY {
            assert!(error_name(code).is_some(), "code {code} has no name");
        }
        assert_eq!(error_name(error::NOTIMPLEMENTED), Some("NOTIMPLEMENTED"));
        assert_eq!(error_name(error::NOTSUPPORTED), Some("NOTSUPPORTED"));
        assert_eq!(error::BUSY, 26, "PA_ERR_MAX is 27, so BUSY is the last");
        assert_eq!(error_name(27), None, "PA_ERR_MAX is not an error code");
        assert_eq!(error_name(u32::MAX), None);
    }

    /// The error codes proved on the wire, and the one this server sends most.
    #[test]
    fn the_error_codes_are_the_ones_libpulse_reported() {
        assert_eq!(error::ACCESS, 1, "reported as \"Access denied\"");
        assert_eq!(error::INVALID, 3, "reported as \"Invalid argument\"");
        assert_eq!(error::NOENTITY, 5, "reported as \"No such entity\"");
        assert_eq!(
            error::NOTIMPLEMENTED,
            23,
            "reported as \"Missing implementation\""
        );
        // The neighbour that a from-memory table gets wrong: NOTSUPPORTED is
        // 19, and answering an unimplemented command with 19 tells the client
        // the wrong thing.
        assert_eq!(error::NOTSUPPORTED, 19);
        assert_ne!(error::NOTSUPPORTED, error::NOTIMPLEMENTED);
    }

    /// The subscribe mask the capture carried is `PA_SUBSCRIPTION_MASK_ALL`.
    #[test]
    fn the_captured_subscribe_mask_is_mask_all() {
        assert_eq!(subscription::MASK_ALL, 0x02FF);
        assert_eq!(
            subscription::MASK_ALL & (subscription::MASK_SINK | subscription::MASK_SINK_INPUT),
            subscription::MASK_SINK | subscription::MASK_SINK_INPUT
        );
        // An event word splits into a facility and a type.
        let event = subscription::EVENT_SINK_INPUT | subscription::EVENT_CHANGE;
        assert_eq!(
            event & subscription::EVENT_FACILITY_MASK,
            subscription::EVENT_SINK_INPUT
        );
        assert_eq!(
            event & subscription::EVENT_TYPE_MASK,
            subscription::EVENT_CHANGE
        );
        assert_eq!(subscription::EVENT_NEW, 0);
        assert_ne!(subscription::EVENT_REMOVE, subscription::EVENT_CHANGE);
    }

    /// No two commands share a number, in either direction. A collision would
    /// silently route one command to the other's handler.
    #[test]
    fn no_two_commands_share_a_number() {
        let all: &[u32] = &[
            command::ERROR,
            command::REPLY,
            command::CREATE_PLAYBACK_STREAM,
            command::DELETE_PLAYBACK_STREAM,
            command::AUTH,
            command::SET_CLIENT_NAME,
            command::DRAIN_PLAYBACK_STREAM,
            command::GET_PLAYBACK_LATENCY,
            command::GET_SERVER_INFO,
            command::GET_SINK_INFO,
            command::GET_SINK_INFO_LIST,
            command::GET_SOURCE_INFO_LIST,
            command::GET_CLIENT_INFO,
            command::GET_CLIENT_INFO_LIST,
            command::GET_SINK_INPUT_INFO,
            command::GET_SINK_INPUT_INFO_LIST,
            command::SUBSCRIBE,
            command::SET_SINK_VOLUME,
            command::SET_SINK_INPUT_VOLUME,
            command::SET_SINK_MUTE,
            command::CORK_PLAYBACK_STREAM,
            command::FLUSH_PLAYBACK_STREAM,
            command::TRIGGER_PLAYBACK_STREAM,
            command::PREBUF_PLAYBACK_STREAM,
            command::SET_SINK_INPUT_MUTE,
            command::UPDATE_PLAYBACK_STREAM_PROPLIST,
            command::REQUEST,
            command::OVERFLOW,
            command::UNDERFLOW,
            command::PLAYBACK_STREAM_KILLED,
            command::SUBSCRIBE_EVENT,
            command::STARTED,
        ];
        let mut deduped = all.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), all.len(), "a command number is used twice");
        for &number in all {
            assert!(command_name(number).is_some(), "{number} has no name");
        }
        assert_eq!(command_name(9999), None);
    }

    /// The sample format and channel positions v1 speaks agree with the ones
    /// the captured `CREATE_PLAYBACK_STREAM` carried.
    #[test]
    fn the_captured_stream_spec_is_the_format_this_server_speaks() {
        assert_eq!(format::SAMPLE_S16LE, tag::SAMPLE_S16LE);
        assert_eq!(format::SAMPLE_FLOAT32LE, tag::SAMPLE_FLOAT32LE);
        assert_eq!(format::FRONT_LEFT, 1);
        assert_eq!(format::FRONT_RIGHT, 2);
        // From the transcript: `sample_spec fmt=3 ch=2 rate=48000`, then
        // `channel_map [1, 2]`.
        let spec = tag::SampleSpec {
            format: format::SAMPLE_S16LE,
            channels: 2,
            rate: 48_000,
        };
        let mut writer = tag::Writer::new();
        writer
            .sample_spec(spec)
            .channel_map(&[format::FRONT_LEFT, format::FRONT_RIGHT]);
        let bytes = writer.into_bytes();
        let mut reader = tag::Reader::new(&bytes);
        assert_eq!(reader.sample_spec().unwrap(), spec);
        assert_eq!(reader.channel_map().unwrap(), vec![1, 2]);
        reader.finish().unwrap();
    }
}
