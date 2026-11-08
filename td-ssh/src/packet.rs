//! Version exchange and binary packet framing (RFC 4253 sections 4 and 6),
//! with `chacha20-poly1305@openssh.com` once keys are installed.
//!
//! Reads are exact, length then remainder, so a process that stops reading
//! leaves no buffered bytes behind when transport state moves on to another
//! process (DESIGN.md section 2).

use crate::error::{Error, Result};
use std::io::{Read, Write};
use td_primitives::chacha20_poly1305::{OpenSshAead, TAG_LEN};

pub const IDENTIFICATION: &str = "SSH-2.0-tdssh_1.0";

/// The largest `packet_length` accepted, which bounds any one allocation.
pub const MAX_PACKET: usize = 64 * 1024;
const MAX_LINE: usize = 255;
const MAX_PREAMBLE_LINES: usize = 64;
const BLOCK: usize = 8;
const MIN_PADDING: usize = 4;

/// Sends our identification, then reads the peer's. A server's peer must lead
/// with it; a client tolerates a bounded preamble (RFC 4253 section 4.2).
pub fn exchange_versions<S: Read + Write>(stream: &mut S, is_client: bool) -> Result<String> {
    stream.write_all(format!("{IDENTIFICATION}\r\n").as_bytes())?;
    stream.flush()?;
    let allowed_lines = if is_client { MAX_PREAMBLE_LINES } else { 0 };
    for _ in 0..=allowed_lines {
        let line = read_line(stream)?;
        if line.starts_with(b"SSH-") {
            let text = String::from_utf8(line)
                .map_err(|_| Error::protocol("identification is not UTF-8"))?;
            if !(text.starts_with("SSH-2.0-") || text.starts_with("SSH-1.99-")) {
                return Err(Error::protocol("peer does not speak SSH 2.0"));
            }
            return Ok(text);
        }
    }
    Err(Error::protocol("no SSH identification from peer"))
}

/// One byte at a time: nothing past the line may leave the kernel's buffer.
fn read_line<R: Read>(stream: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::with_capacity(64);
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte)?;
        let [value] = byte;
        match value {
            b'\n' => {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            0 => return Err(Error::protocol("NUL in identification")),
            _ if line.len() >= MAX_LINE => {
                return Err(Error::protocol("identification line is too long"))
            }
            _ => line.push(value),
        }
    }
}

/// Traffic counters that decide when to rekey (DESIGN.md section 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub seq: u32,
    pub bytes: u64,
    pub packets: u64,
}

const REKEY_BYTES: u64 = 1 << 30;
const REKEY_PACKETS: u64 = 1 << 28;

impl Counters {
    pub fn wants_rekey(&self) -> bool {
        self.bytes >= REKEY_BYTES || self.packets >= REKEY_PACKETS
    }

    /// Strict KEX: the sequence number resets at every `NEWKEYS`.
    pub fn on_new_keys(&mut self) {
        *self = Self::default();
    }
}

/// The sequence number after `seq`. It is the AEAD's nonce, and strict KEX resets
/// it at every `NEWKEYS`, so only a peer that keeps a connection on one key comes
/// near a wrap: one never finishing a rekey, or never ending authentication, which
/// rekeys nothing. The connection ends instead.
fn next_seq(seq: u32) -> Result<u32> {
    seq.checked_add(1)
        .ok_or_else(|| Error::protocol("sequence number exhausted without a rekey"))
}

/// A transport key, boxed so that moving it copies only a pointer, and wiped when
/// dropped: through the exchange's output, the reader thread's channel and the
/// session's handoff these bytes stay in one place. The cipher built from it holds
/// its own expanded copy inline, which moves with its owner like any value
/// (DESIGN.md section 7).
pub struct TransportKey(Box<[u8; 64]>);

impl TransportKey {
    /// For filling in place.
    pub fn zeroed() -> Self {
        Self(Box::new([0; 64]))
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let mut key = Self::zeroed();
        if bytes.len() != key.0.len() {
            return Err(Error::protocol("a transport key is 64 bytes"));
        }
        key.0.copy_from_slice(bytes);
        Ok(key)
    }

    pub fn bytes(&self) -> &[u8; 64] {
        &self.0
    }

    pub fn bytes_mut(&mut self) -> &mut [u8; 64] {
        &mut self.0
    }
}

impl Drop for TransportKey {
    fn drop(&mut self) {
        td_primitives::ct::zero(self.0.as_mut_slice());
    }
}

impl PartialEq for TransportKey {
    fn eq(&self, other: &Self) -> bool {
        td_primitives::ct::eq(self.0.as_slice(), other.0.as_slice())
    }
}

impl Eq for TransportKey {}

impl std::fmt::Debug for TransportKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransportKey(..)")
    }
}

pub struct PacketReader<R> {
    inner: R,
    aead: Option<OpenSshAead>,
    key: Option<TransportKey>,
    counters: Counters,
    buffer: Vec<u8>,
}

impl<R: Read> PacketReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            aead: None,
            key: None,
            counters: Counters::default(),
            buffer: Vec::with_capacity(4096),
        }
    }

    /// Resumes a connection whose keys another process negotiated.
    pub fn resume(inner: R, key: TransportKey, counters: Counters) -> Self {
        let mut reader = Self::new(inner);
        reader.aead = Some(OpenSshAead::new(key.bytes()));
        reader.key = Some(key);
        reader.counters = counters;
        reader
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The key in use, for handing transport state to another process.
    /// For a handoff: the key leaves, and the cipher built from it stays.
    pub fn take_key(&mut self) -> Option<TransportKey> {
        self.key.take()
    }

    pub fn install_key(&mut self, key: TransportKey) {
        self.aead = Some(OpenSshAead::new(key.bytes()));
        self.key = Some(key);
        self.counters.on_new_keys();
    }

    /// One message's payload. A bad length or tag ends the connection without
    /// reporting which: RFC 4253 section 6 parsing errors are not an oracle.
    pub fn read_packet(&mut self) -> Result<Vec<u8>> {
        let mut encrypted_length = [0u8; 4];
        self.inner.read_exact(&mut encrypted_length)?;
        let seq = self.counters.seq;
        let length_bytes = match &self.aead {
            Some(aead) => aead.decrypt_length(seq, &encrypted_length),
            None => encrypted_length,
        };
        let length = u32::from_be_bytes(length_bytes) as usize;
        if !(1 + MIN_PADDING..=MAX_PACKET).contains(&length) {
            return Err(Error::protocol("bad packet length"));
        }
        let wire_length = if self.aead.is_some() {
            // The length field is not part of the block-aligned count here.
            if !length.is_multiple_of(BLOCK) {
                return Err(Error::protocol("bad packet length"));
            }
            length + TAG_LEN
        } else {
            if !(length + 4).is_multiple_of(BLOCK) {
                return Err(Error::protocol("bad packet length"));
            }
            length
        };
        self.buffer.clear();
        self.buffer.extend_from_slice(&encrypted_length);
        self.buffer.resize(4 + wire_length, 0);
        let body = self
            .buffer
            .get_mut(4..)
            .ok_or_else(|| Error::protocol("bad packet length"))?;
        self.inner.read_exact(body)?;
        if let Some(aead) = &self.aead {
            let split = 4 + length;
            let (packet, tag) = self
                .buffer
                .split_at_mut_checked(split)
                .ok_or_else(|| Error::protocol("bad packet length"))?;
            let tag: &[u8; TAG_LEN] = (&*tag)
                .try_into()
                .map_err(|_| Error::protocol("bad packet length"))?;
            aead.open(seq, packet, tag)
                .map_err(|_| Error::protocol("message authentication failed"))?;
        }
        let padding = *self
            .buffer
            .get(4)
            .ok_or_else(|| Error::protocol("bad packet length"))? as usize;
        if padding < MIN_PADDING || padding + 1 > length {
            return Err(Error::protocol("bad padding"));
        }
        let payload = self
            .buffer
            .get(5..4 + length - padding)
            .ok_or_else(|| Error::protocol("bad padding"))?
            .to_vec();
        self.counters.seq = next_seq(seq)?;
        self.counters.packets = self.counters.packets.saturating_add(1);
        self.counters.bytes = self.counters.bytes.saturating_add(length as u64 + 4);
        Ok(payload)
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }
}

pub struct PacketWriter<W> {
    inner: W,
    aead: Option<OpenSshAead>,
    key: Option<TransportKey>,
    counters: Counters,
    buffer: Vec<u8>,
}

impl<W: Write> PacketWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            aead: None,
            key: None,
            counters: Counters::default(),
            buffer: Vec::with_capacity(4096),
        }
    }

    pub fn resume(inner: W, key: TransportKey, counters: Counters) -> Self {
        let mut writer = Self::new(inner);
        writer.aead = Some(OpenSshAead::new(key.bytes()));
        writer.key = Some(key);
        writer.counters = counters;
        writer
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The key in use, for handing transport state to another process.
    /// For a handoff: the key leaves, and the cipher built from it stays.
    pub fn take_key(&mut self) -> Option<TransportKey> {
        self.key.take()
    }

    pub fn install_key(&mut self, key: TransportKey) {
        self.aead = Some(OpenSshAead::new(key.bytes()));
        self.key = Some(key);
        self.counters.on_new_keys();
    }

    /// Padding is zeros. Under the AEAD it is encrypted anyway, and before keys
    /// exist every KEX message is public.
    pub fn write_packet(&mut self, payload: &[u8]) -> Result<()> {
        let encrypted = self.aead.is_some();
        // The AEAD's length field is excluded from alignment (OpenSSH's aadlen).
        let aligned = if encrypted {
            1 + payload.len()
        } else {
            5 + payload.len()
        };
        let mut padding = BLOCK - aligned % BLOCK;
        if padding < MIN_PADDING {
            padding += BLOCK;
        }
        let length = 1 + payload.len() + padding;
        if length > MAX_PACKET {
            return Err(Error::protocol("outgoing packet is too long"));
        }
        let seq = self.counters.seq;
        let next = next_seq(seq)?;
        self.buffer.clear();
        self.buffer
            .extend_from_slice(&u32::try_from(length).unwrap_or(u32::MAX).to_be_bytes());
        self.buffer.push(u8::try_from(padding).unwrap_or(u8::MAX));
        self.buffer.extend_from_slice(payload);
        self.buffer.resize(4 + length, 0);
        if let Some(aead) = &self.aead {
            let tag = aead
                .seal(seq, &mut self.buffer)
                .map_err(|_| Error::protocol("outgoing packet cannot be sealed"))?;
            self.buffer.extend_from_slice(&tag);
        }
        self.inner.write_all(&self.buffer)?;
        self.inner.flush()?;
        self.counters.seq = next;
        self.counters.packets = self.counters.packets.saturating_add(1);
        self.counters.bytes = self.counters.bytes.saturating_add(length as u64 + 4);
        Ok(())
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::io::Cursor;

    fn frame(payload: &[u8], key: Option<&[u8; 64]>) -> Vec<u8> {
        let mut writer = PacketWriter::new(Vec::new());
        if let Some(key) = key {
            writer.aead = Some(OpenSshAead::new(key));
        }
        writer.write_packet(payload).unwrap();
        writer.inner
    }

    #[test]
    fn plaintext_packets_round_trip_at_every_alignment() {
        for size in 0..40 {
            let payload = vec![0x5a; size];
            let wire = frame(&payload, None);
            assert_eq!(wire.len() % BLOCK, 0, "size {size}");
            let mut reader = PacketReader::new(Cursor::new(wire));
            assert_eq!(reader.read_packet().unwrap(), payload);
        }
    }

    #[test]
    fn encrypted_packets_round_trip_and_keep_alignment_without_the_length() {
        let key = [7u8; 64];
        for size in 0..40 {
            let payload = vec![0xa5; size];
            let wire = frame(&payload, Some(&key));
            assert_eq!((wire.len() - 4 - TAG_LEN) % BLOCK, 0, "size {size}");
            let mut reader = PacketReader::new(Cursor::new(wire));
            reader.aead = Some(OpenSshAead::new(&key));
            assert_eq!(reader.read_packet().unwrap(), payload);
        }
    }

    #[test]
    fn a_flipped_byte_anywhere_is_refused() {
        let key = [9u8; 64];
        let wire = frame(b"hello", Some(&key));
        for index in 4..wire.len() {
            let mut bad = wire.clone();
            bad[index] ^= 1;
            let mut reader = PacketReader::new(Cursor::new(bad));
            reader.aead = Some(OpenSshAead::new(&key));
            assert!(reader.read_packet().is_err(), "byte {index}");
        }
    }

    #[test]
    fn hostile_lengths_are_refused_before_any_large_allocation() {
        for length in [0u32, 1, 4, u32::MAX, (MAX_PACKET + 4) as u32] {
            let mut wire = length.to_be_bytes().to_vec();
            wire.extend_from_slice(&[0; 64]);
            let mut reader = PacketReader::new(Cursor::new(wire));
            assert!(reader.read_packet().is_err(), "length {length}");
            assert!(reader.buffer.capacity() < MAX_PACKET);
        }
    }

    #[test]
    fn padding_is_validated() {
        // length 12, so padding of 2 is below the minimum and of 200 overruns.
        for padding in [2u8, 200] {
            let mut wire = 12u32.to_be_bytes().to_vec();
            wire.push(padding);
            wire.extend_from_slice(&[0; 11]);
            let mut reader = PacketReader::new(Cursor::new(wire));
            assert!(reader.read_packet().is_err(), "padding {padding}");
        }
    }

    #[test]
    fn sequence_numbers_differ_so_a_replayed_packet_fails() {
        let key = [3u8; 64];
        let mut writer = PacketWriter::new(Vec::new());
        writer.aead = Some(OpenSshAead::new(&key));
        writer.write_packet(b"one").unwrap();
        let first_len = writer.inner.len();
        let mut wire = writer.inner.clone();
        wire.extend_from_slice(&writer.inner[..first_len]);
        let mut reader = PacketReader::new(Cursor::new(wire));
        reader.aead = Some(OpenSshAead::new(&key));
        assert_eq!(reader.read_packet().unwrap(), b"one");
        assert!(reader.read_packet().is_err());
    }

    /// Under one key the nonce never repeats: the packet that would wrap the
    /// sequence number is refused, whichever way it goes.
    #[test]
    fn the_sequence_number_never_wraps() {
        let key = [3u8; 64];
        let mut writer = PacketWriter::new(Vec::new());
        writer.aead = Some(OpenSshAead::new(&key));
        writer.counters.seq = u32::MAX - 1;
        writer.write_packet(b"last").unwrap();
        let sent = writer.inner.len();
        let error = writer.write_packet(b"wraps").err().unwrap().to_string();
        assert!(error.contains("sequence number"), "{error}");
        assert_eq!(writer.inner.len(), sent, "nothing went out");

        // A peer's packet sealed at the last number, which is good in itself:
        // only the number after it is refused.
        let mut wire = writer.inner.clone();
        let mut packet = vec![0, 0, 0, 16, 10];
        packet.extend_from_slice(b"wraps");
        packet.resize(4 + 16, 0);
        let tag = OpenSshAead::new(&key).seal(u32::MAX, &mut packet).unwrap();
        wire.extend_from_slice(&packet);
        wire.extend_from_slice(&tag);
        let mut reader = PacketReader::new(Cursor::new(wire));
        reader.aead = Some(OpenSshAead::new(&key));
        reader.counters.seq = u32::MAX - 1;
        assert_eq!(reader.read_packet().unwrap(), b"last");
        assert_eq!(reader.counters.seq, u32::MAX);
        let error = reader.read_packet().err().unwrap().to_string();
        assert!(error.contains("sequence number"), "{error}");
    }

    #[test]
    fn version_exchange_server_refuses_a_preamble_client_tolerates_one() {
        struct Duplex(Cursor<Vec<u8>>, Vec<u8>);
        impl Read for Duplex {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Write for Duplex {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.1.write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let text = b"hello there\r\nSSH-2.0-peer\r\nleft for KEX".to_vec();
        let mut client = Duplex(Cursor::new(text.clone()), Vec::new());
        assert_eq!(
            exchange_versions(&mut client, true).unwrap(),
            "SSH-2.0-peer"
        );
        // Nothing past the identification line was consumed.
        let mut rest = Vec::new();
        client.0.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"left for KEX");
        assert_eq!(client.1, b"SSH-2.0-tdssh_1.0\r\n");

        let mut server = Duplex(Cursor::new(text), Vec::new());
        assert!(exchange_versions(&mut server, false).is_err());

        for bad in [&b"SSH-1.5-old\r\n"[..], b"SSH-2.0-\0x\r\n"] {
            let mut peer = Duplex(Cursor::new(bad.to_vec()), Vec::new());
            assert!(exchange_versions(&mut peer, true).is_err());
        }
        let long = [b"SSH-2.0-".as_slice(), &[b'x'; 300], b"\r\n"].concat();
        let mut peer = Duplex(Cursor::new(long), Vec::new());
        assert!(exchange_versions(&mut peer, true).is_err());
    }
}
