//! The pstream framing layer: a 20-byte descriptor and a body.
//!
//! §K.3: "Frames are a 20-byte descriptor of five big-endian `u32` — `length`,
//! `channel`, `offset_hi`, `offset_lo`, `flags`. `channel == 0xFFFFFFFF` marks
//! a control frame carrying a **tagstruct**; any other channel is stream data
//! for that playback stream, with `flags` as the seek mode."
//!
//! That is the whole of it, because `enable-shm=no` deleted the rest. §K.2: the
//! flatpak client config upstream already generates forces sandboxed clients
//! onto plain socket data, so a server that declines both the SHM and memfd
//! feature bits in the handshake matches what every flatpak app on every distro
//! does today — and that removes `SCM_RIGHTS`, pool and block identifiers,
//! release/revoke synchronisation, seal policy, and recovery from a client
//! dying with blocks in flight from v1 entirely. This module has no ancillary
//! data path at all, which is why `td-audio` needs no descriptor-adoption
//! allowance in `UNSAFE.md` §13.

use crate::tag;
use std::fmt;

/// Five big-endian `u32`.
pub const DESCRIPTOR_LEN: usize = 20;

/// `channel` for a control frame.
pub const CHANNEL_CONTROL: u32 = 0xFFFF_FFFF;

/// The largest control frame this server will accept.
///
/// A control packet is a tagstruct, and the biggest legitimate one is an
/// authentication packet or a proplist-carrying stream creation — both well
/// under this. The bound exists because `length` arrives from a jailed client
/// before any of its bytes do.
pub const CONTROL_MAX: usize = 64 * 1024;

/// The largest stream-data frame.
///
/// Audio, so larger, but still finite: at 48 kHz stereo `S16_LE` this is about
/// five seconds, which is far more than any client's buffer attributes ask for.
pub const DATA_MAX: usize = 1024 * 1024;

/// `PA_FLAG_SHMDATA`. Never set by a client this server has answered, because
/// the handshake clears the SHM feature bit — so seeing it is a protocol error
/// rather than a case to handle.
pub const FLAG_SHMDATA: u32 = 0x8000_0000;

/// The seek modes a data frame's `flags` may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seek {
    /// Relative to the write index.
    Relative,
    /// Absolute from the start of the stream.
    Absolute,
    /// Relative to the read index.
    RelativeOnRead,
    /// Relative to the end of the buffer.
    RelativeEnd,
}

impl Seek {
    pub fn from_flags(flags: u32) -> Option<Self> {
        match flags {
            0 => Some(Seek::Relative),
            1 => Some(Seek::Absolute),
            2 => Some(Seek::RelativeOnRead),
            3 => Some(Seek::RelativeEnd),
            _ => None,
        }
    }
}

/// One frame off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A control packet: a tagstruct whose first two values are the command
    /// number and the tag.
    Control(Vec<u8>),
    /// Audio for one playback stream.
    Data {
        channel: u32,
        seek: Seek,
        offset: i64,
        pcm: Vec<u8>,
    },
}

/// Why a frame could not be read.
///
/// `Copy` because the decoder remembers one: a framing error is terminal and is
/// reported again rather than re-parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A control frame longer than `CONTROL_MAX`.
    ControlTooLarge(usize),
    /// A data frame longer than `DATA_MAX`.
    DataTooLarge(usize),
    /// `PA_FLAG_SHMDATA` on a connection whose handshake cleared the SHM bit.
    SharedMemoryRefused,
    /// A data frame whose `flags` are not a seek mode.
    BadSeekMode(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::ControlTooLarge(len) => write!(
                f,
                "a control frame declares {len} bytes, over the {CONTROL_MAX}-byte bound"
            ),
            Error::DataTooLarge(len) => write!(
                f,
                "a data frame declares {len} bytes, over the {DATA_MAX}-byte bound"
            ),
            Error::SharedMemoryRefused => write!(
                f,
                "a frame carries PA_FLAG_SHMDATA, but this server cleared the SHM \
                 feature bit in the handshake and has no shared-memory path"
            ),
            Error::BadSeekMode(flags) => {
                write!(f, "a data frame's flags {flags:#x} are not a seek mode")
            }
        }
    }
}

/// The parsed descriptor, before its body has arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub length: u32,
    pub channel: u32,
    pub offset: i64,
    pub flags: u32,
}

impl Descriptor {
    /// Read the fixed 20 bytes.
    ///
    /// Returns `None` only when fewer than 20 bytes are present; a descriptor
    /// that is present but nonsensical is an `Err`, because the difference
    /// between "wait for more" and "hang up" is the whole job of this function.
    pub fn parse(bytes: &[u8]) -> Option<Result<Self, Error>> {
        let head = bytes.get(..DESCRIPTOR_LEN)?;
        let word = |index: usize| -> u32 {
            head.get(index * 4..index * 4 + 4)
                .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
                .map(u32::from_be_bytes)
                .unwrap_or(0)
        };
        let length = word(0);
        let channel = word(1);
        let offset = ((u64::from(word(2)) << 32) | u64::from(word(3))) as i64;
        let flags = word(4);
        if flags & FLAG_SHMDATA != 0 {
            return Some(Err(Error::SharedMemoryRefused));
        }
        let len = length as usize;
        if channel == CHANNEL_CONTROL {
            if len > CONTROL_MAX {
                return Some(Err(Error::ControlTooLarge(len)));
            }
        } else if len > DATA_MAX {
            return Some(Err(Error::DataTooLarge(len)));
        }
        Some(Ok(Self {
            length,
            channel,
            offset,
            flags,
        }))
    }

    pub fn is_control(&self) -> bool {
        self.channel == CHANNEL_CONTROL
    }

    /// Turn this descriptor and its body into a frame.
    pub fn with_body(&self, body: &[u8]) -> Result<Frame, Error> {
        if self.is_control() {
            return Ok(Frame::Control(body.to_vec()));
        }
        let seek = Seek::from_flags(self.flags).ok_or(Error::BadSeekMode(self.flags))?;
        Ok(Frame::Data {
            channel: self.channel,
            seek,
            offset: self.offset,
            pcm: body.to_vec(),
        })
    }

    /// The 20 bytes, for a frame this server sends.
    pub fn encode(length: u32, channel: u32, offset: i64, flags: u32) -> [u8; DESCRIPTOR_LEN] {
        let mut out = [0u8; DESCRIPTOR_LEN];
        let offset = offset as u64;
        for (index, word) in [
            length,
            channel,
            (offset >> 32) as u32,
            (offset & 0xffff_ffff) as u32,
            flags,
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(slot) = out.get_mut(index * 4..index * 4 + 4) {
                slot.copy_from_slice(&word.to_be_bytes());
            }
        }
        out
    }
}

/// Frame a control packet for sending: descriptor then tagstruct.
pub fn control_frame(packet: &[u8]) -> Vec<u8> {
    let length = u32::try_from(packet.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(DESCRIPTOR_LEN.saturating_add(packet.len()));
    out.extend_from_slice(&Descriptor::encode(length, CHANNEL_CONTROL, 0, 0));
    out.extend_from_slice(packet);
    out
}

/// A reassembler: bytes in, whole frames out.
///
/// A stream socket delivers whatever it delivers, so the descriptor and its
/// body can arrive in any number of pieces. Everything held here is bounded by
/// the checks in `Descriptor::parse`, which run on the descriptor BEFORE any of
/// the body is buffered — a client cannot make this grow by declaring a large
/// frame and then sending nothing.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    /// How far into `buffer` the decoder has read.
    ///
    /// A cursor rather than a `drain` from the front. Draining moves every
    /// remaining byte on each frame, so one 64 KiB read of small frames costs
    /// time quadratic in the frame count — a client sending nothing but
    /// well-formed empty data frames could spend more than a period of the
    /// daemon's single thread per pass, and starve the device it is feeding.
    at: usize,
    pending: Option<Descriptor>,
    /// A framing error is terminal: the byte stream is no longer aligned to
    /// anything, so there is no later frame to find.
    failed: Option<Error>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes read from the socket.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.failed.is_some() {
            // Terminal means terminal: a caller that kept feeding a decoder it
            // had already been told was broken would grow this without bound.
            return;
        }
        // Compacting here rather than per frame is what keeps the cursor cheap:
        // one move of the unread remainder per socket read, not one per frame.
        if self.at > 0 {
            self.buffer.drain(..self.at);
            self.at = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// The bytes that have arrived and not yet been decoded.
    fn unread(&self) -> &[u8] {
        self.buffer.get(self.at..).unwrap_or(&[])
    }

    /// How many bytes are held, waiting for the rest of a frame.
    ///
    /// `Descriptor::parse` bounds the bytes and the daemon separately limits
    /// how long an incomplete frame may reserve them.
    #[cfg(test)]
    pub fn buffered(&self) -> usize {
        self.unread().len()
    }

    /// Whether the first unprocessed frame is waiting for more socket bytes.
    /// Complete frames retained only by the per-pass work budget are deferred,
    /// not partial, and must not start the abandoned-frame deadline.
    pub fn is_incomplete(&self) -> bool {
        if let Some(descriptor) = self.pending {
            return self.unread().len() < descriptor.length as usize;
        }
        let unread = self.unread();
        if unread.is_empty() {
            return false;
        }
        match Descriptor::parse(unread) {
            None => true,
            Some(Ok(descriptor)) => {
                unread.len() < DESCRIPTOR_LEN.saturating_add(descriptor.length as usize)
            }
            // A complete malformed descriptor is ready to be refused on the
            // next bounded decoder turn; waiting cannot repair it.
            Some(Err(_)) => false,
        }
    }

    /// The next whole frame, if one has arrived.
    pub fn next_frame(&mut self) -> Option<Result<Frame, Error>> {
        if let Some(error) = self.failed {
            return Some(Err(error));
        }
        loop {
            let descriptor = match self.pending {
                Some(descriptor) => descriptor,
                None => match Descriptor::parse(self.unread())? {
                    Ok(descriptor) => {
                        self.at = self.at.saturating_add(DESCRIPTOR_LEN);
                        self.pending = Some(descriptor);
                        descriptor
                    }
                    Err(error) => {
                        // Terminal, and remembered. Leaving the descriptor in
                        // the buffer would re-parse the same bad bytes forever;
                        // dropping them would resume mid-frame on whatever
                        // followed, which is worse.
                        self.buffer.clear();
                        self.at = 0;
                        self.pending = None;
                        self.failed = Some(error);
                        return Some(Err(error));
                    }
                },
            };
            let want = descriptor.length as usize;
            if self.unread().len() < want {
                return None;
            }
            let result = {
                let body = self.unread().get(..want).unwrap_or(&[]);
                if descriptor.is_control() && body.is_empty() {
                    None
                } else {
                    Some(descriptor.with_body(body))
                }
            };
            self.at = self.at.saturating_add(want);
            self.pending = None;
            // A zero-length control frame carries no command and is not a
            // packet; skip it rather than handing a caller an empty tagstruct
            // to misparse.
            match result {
                Some(frame) => return Some(frame),
                None => continue,
            }
        }
    }
}

/// The first two values of every control packet: the command and the tag.
pub fn command_and_tag(packet: &[u8]) -> tag::Result<(u32, u32)> {
    let mut reader = tag::Reader::new(packet);
    Ok((reader.u32()?, reader.u32()?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn control(packet: &[u8]) -> Vec<u8> {
        control_frame(packet)
    }

    #[test]
    fn a_descriptor_is_five_big_endian_words() {
        // Encoding is not bounded — the bound is a receive-side rule — so this
        // word exercises all four length bytes.
        let bytes = Descriptor::encode(0x0102_0304, CHANNEL_CONTROL, 0, 0);
        assert_eq!(bytes.len(), DESCRIPTOR_LEN);
        assert_eq!(bytes.get(..4).unwrap(), &[1, 2, 3, 4], "big-endian length");
        assert_eq!(bytes.get(4..8).unwrap(), &[0xff, 0xff, 0xff, 0xff]);
        // Parsing it back is a different question, and this length is far over
        // the control bound, so the answer is a refusal.
        assert!(matches!(
            Descriptor::parse(&bytes).unwrap().unwrap_err(),
            Error::ControlTooLarge(0x0102_0304)
        ));
        // A length within the bound round-trips.
        let bytes = Descriptor::encode(0x0304, CHANNEL_CONTROL, 0, 0);
        assert_eq!(bytes.get(..4).unwrap(), &[0, 0, 3, 4]);
        let parsed = Descriptor::parse(&bytes).unwrap().unwrap();
        assert_eq!(parsed.length, 0x0304);
        assert!(parsed.is_control());
    }

    /// The offset is a signed 64-bit value split across two words, and a
    /// negative one must survive the round trip — a client seeking backwards
    /// sends exactly that.
    #[test]
    fn a_split_offset_round_trips_including_negatives() {
        for offset in [0i64, 1, -1, i64::MAX, i64::MIN, -48000] {
            let bytes = Descriptor::encode(0, 7, offset, 0);
            assert_eq!(Descriptor::parse(&bytes).unwrap().unwrap().offset, offset);
        }
    }

    #[test]
    fn a_control_frame_round_trips_through_the_decoder() {
        let mut writer = tag::Writer::new();
        writer.u32(20).u32(3);
        let packet = writer.into_bytes();
        let mut decoder = Decoder::new();
        decoder.push(&control(&packet));
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame, Frame::Control(packet.clone()));
        assert!(decoder.next_frame().is_none());
        assert_eq!(command_and_tag(&packet).unwrap(), (20, 3));
    }

    /// A frame split across arbitrarily many reads is still one frame, and no
    /// byte is lost at a boundary.
    #[test]
    fn a_frame_split_byte_by_byte_reassembles() {
        let mut writer = tag::Writer::new();
        writer.u32(9).u32(1).string("hello");
        let packet = writer.into_bytes();
        let wire = control(&packet);
        let mut decoder = Decoder::new();
        for byte in wire.iter().take(wire.len() - 1) {
            decoder.push(&[*byte]);
            assert!(decoder.next_frame().is_none(), "not whole yet");
        }
        decoder.push(&[*wire.last().unwrap()]);
        assert_eq!(
            decoder.next_frame().unwrap().unwrap(),
            Frame::Control(packet)
        );
    }

    /// Several frames in one read all come out, in order.
    #[test]
    fn several_frames_in_one_read_come_out_in_order() {
        let mut wire = Vec::new();
        for command in [8u32, 9, 20] {
            let mut writer = tag::Writer::new();
            writer.u32(command).u32(0);
            wire.extend_from_slice(&control(writer.as_bytes()));
        }
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        let mut seen = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            if let Frame::Control(packet) = frame.unwrap() {
                seen.push(command_and_tag(&packet).unwrap().0);
            }
        }
        assert_eq!(seen, vec![8, 9, 20]);
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn complete_deferred_frames_are_not_partial_input() {
        let first = Descriptor::encode(0, 1, 0, 0);
        let second = Descriptor::encode(0, 2, 0, 0);
        let mut decoder = Decoder::new();
        decoder.push(&[first.as_slice(), second.as_slice()].concat());
        assert!(!decoder.is_incomplete());
        assert!(decoder.next_frame().is_some());
        assert!(!decoder.is_incomplete());
        assert!(decoder.next_frame().is_some());

        decoder.push(&Descriptor::encode(4, 3, 0, 0));
        assert!(decoder.next_frame().is_none());
        assert!(decoder.is_incomplete());
    }

    /// A data frame carries its channel, seek mode and audio.
    #[test]
    fn a_data_frame_carries_its_channel_and_seek_mode() {
        let pcm = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut wire = Descriptor::encode(pcm.len() as u32, 0, 0, 0).to_vec();
        wire.extend_from_slice(&pcm);
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        assert_eq!(
            decoder.next_frame().unwrap().unwrap(),
            Frame::Data {
                channel: 0,
                seek: Seek::Relative,
                offset: 0,
                pcm
            }
        );
    }

    /// This is the frame `paplay` actually sent: channel 0, seek mode 0, and
    /// 57600 bytes of the tone rendered by `td-audio tone --wav`. Captured, not
    /// invented — its descriptor is what the decoder has to accept.
    #[test]
    fn the_captured_data_frame_descriptor_parses() {
        let descriptor = Descriptor::encode(57600, 0, 0, 0);
        let parsed = Descriptor::parse(&descriptor).unwrap().unwrap();
        assert_eq!(parsed.length, 57600);
        assert_eq!(parsed.channel, 0);
        assert!(!parsed.is_control());
        assert_eq!(Seek::from_flags(parsed.flags), Some(Seek::Relative));
        // 57600 bytes is 14400 frames of 48 kHz stereo S16_LE — 300 ms.
        assert_eq!(57600 / 4, 14400);
    }

    /// §K.2's whole point: with the SHM feature bit cleared there is no shared
    /// memory path, so a frame that claims one is refused rather than handled.
    #[test]
    fn a_shared_memory_frame_is_refused_rather_than_handled() {
        let bytes = Descriptor::encode(16, 0, 0, FLAG_SHMDATA);
        assert_eq!(
            Descriptor::parse(&bytes).unwrap().unwrap_err(),
            Error::SharedMemoryRefused
        );
    }

    /// The bounds are checked on the DECLARED length, before any body is
    /// buffered — which is what stops a client reserving memory by lying.
    #[test]
    fn an_over_long_frame_is_refused_before_its_body_arrives() {
        let control = Descriptor::encode((CONTROL_MAX + 1) as u32, CHANNEL_CONTROL, 0, 0);
        assert!(matches!(
            Descriptor::parse(&control).unwrap().unwrap_err(),
            Error::ControlTooLarge(_)
        ));
        let data = Descriptor::encode((DATA_MAX + 1) as u32, 0, 0, 0);
        assert!(matches!(
            Descriptor::parse(&data).unwrap().unwrap_err(),
            Error::DataTooLarge(_)
        ));
        // And the decoder holds nothing for such a frame.
        let mut decoder = Decoder::new();
        decoder.push(&control);
        assert!(decoder.next_frame().unwrap().is_err());
        assert_eq!(
            decoder.buffered(),
            0,
            "the body was never taken, and the bad descriptor is not kept either"
        );
        // The error is terminal. Re-parsing the same bad descriptor forever, or
        // resuming mid-frame on whatever followed it, are the two ways a caller
        // that logged and continued would spin.
        assert!(
            decoder.next_frame().unwrap().is_err(),
            "and it is reported again rather than the stream resyncing"
        );
    }

    /// A data frame whose flags are not a seek mode is an error, not a guess.
    #[test]
    fn an_unknown_seek_mode_is_refused() {
        let mut wire = Descriptor::encode(4, 0, 0, 9).to_vec();
        wire.extend_from_slice(&[0, 0, 0, 0]);
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        assert_eq!(
            decoder.next_frame().unwrap().unwrap_err(),
            Error::BadSeekMode(9)
        );
        assert_eq!(Seek::from_flags(0), Some(Seek::Relative));
        assert_eq!(Seek::from_flags(3), Some(Seek::RelativeEnd));
        assert_eq!(Seek::from_flags(4), None);
    }

    /// A zero-length control frame is not a packet and must not be handed on as
    /// an empty tagstruct — a decoder that did would read a command number out
    /// of nothing.
    #[test]
    fn an_empty_control_frame_is_skipped_not_delivered() {
        let mut wire = Descriptor::encode(0, CHANNEL_CONTROL, 0, 0).to_vec();
        let mut writer = tag::Writer::new();
        writer.u32(8).u32(0);
        wire.extend_from_slice(&control(writer.as_bytes()));
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(
            command_and_tag(match &frame {
                Frame::Control(packet) => packet,
                _ => panic!("expected a control frame"),
            })
            .unwrap(),
            (8, 0)
        );
    }

    /// A zero-length DATA frame is legitimate — it is how a client signals a
    /// seek with no audio — so it is delivered rather than skipped.
    #[test]
    fn an_empty_data_frame_is_delivered() {
        let wire = Descriptor::encode(0, 3, -1000, 1).to_vec();
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        assert_eq!(
            decoder.next_frame().unwrap().unwrap(),
            Frame::Data {
                channel: 3,
                seek: Seek::Absolute,
                offset: -1000,
                pcm: Vec::new()
            }
        );
    }

    #[test]
    fn a_short_descriptor_waits_rather_than_failing() {
        let mut decoder = Decoder::new();
        decoder.push(&[0, 0, 0]);
        assert!(decoder.next_frame().is_none());
        assert!(Descriptor::parse(&[0u8; DESCRIPTOR_LEN - 1]).is_none());
    }
}
