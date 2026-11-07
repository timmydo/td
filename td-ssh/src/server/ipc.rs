//! Framed messages between the stages, and the transport state that moves from
//! the pre-auth worker to the session. Peers here may be compromised, so reads
//! are bounded and every message is parsed as hostile.

use crate::error::{Error, Result};
use crate::packet::Counters;
use crate::wire::{Reader, Writer};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

pub const MAX_FRAME: usize = 64 * 1024;

/// Reads from a stream under one absolute deadline. `SO_RCVTIMEO` bounds a single
/// read and `read_exact` loops, so re-arming it per frame would let a peer that
/// sends one byte at a time multiply the time left by the frame's length. This
/// re-arms it from the deadline before every read instead.
pub struct UntilDeadline<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl<'a> UntilDeadline<'a> {
    pub fn new(stream: &'a UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }
}

impl Read for UntilDeadline<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        self.stream.set_read_timeout(Some(remaining))?;
        (&mut &*self.stream).read(out)
    }
}

/// Whether a read's error is a deadline passing rather than the stream failing.
pub fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

pub fn write_frame<W: Write>(stream: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(Error::protocol("frame is too long"));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::protocol("frame is too long"))?
        .to_be_bytes();
    let mut message = Vec::with_capacity(4 + payload.len());
    message.extend_from_slice(&length);
    message.extend_from_slice(payload);
    stream.write_all(&message)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(stream: &mut R) -> Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(Error::protocol("frame is too long"));
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

pub const WORKER_SIGN: u8 = 1;
pub const WORKER_KEY_ACCEPTABLE: u8 = 2;
pub const WORKER_AUTHENTICATE: u8 = 3;
pub const WORKER_AUTHENTICATED: u8 = 4;

pub const MONITOR_INIT: u8 = 11;
pub const MONITOR_SIGNATURE: u8 = 12;
pub const MONITOR_ANSWER: u8 = 13;
pub const MONITOR_TOO_MANY: u8 = 14;

/// What moves from the worker to the session once authentication succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportState {
    pub client_version: String,
    pub session_id: [u8; 32],
    pub tx_key: [u8; 64],
    pub rx_key: [u8; 64],
    pub tx: Counters,
    pub rx: Counters,
}

fn put_counters(writer: &mut Writer, counters: Counters) {
    writer
        .u32(counters.seq)
        .u64(counters.bytes)
        .u64(counters.packets);
}

fn get_counters(reader: &mut Reader<'_>) -> Result<Counters> {
    Ok(Counters {
        seq: reader.u32()?,
        bytes: reader.u64()?,
        packets: reader.u64()?,
    })
}

impl TransportState {
    pub fn encode(&self, writer: &mut Writer) {
        writer.str(&self.client_version);
        writer.string(&self.session_id);
        writer.string(&self.tx_key);
        writer.string(&self.rx_key);
        put_counters(writer, self.tx);
        put_counters(writer, self.rx);
    }

    pub fn decode(reader: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            client_version: reader.str()?.to_string(),
            session_id: reader.array()?,
            tx_key: reader.array()?,
            rx_key: reader.array()?,
            tx: get_counters(reader)?,
            rx: get_counters(reader)?,
        })
    }
}

/// The resolved account and policy handed to a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGrant {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: String,
    pub shell: String,
    pub force_command: Option<String>,
    pub permit_tty: bool,
    pub max_sessions: u32,
    pub sftp: bool,
    pub peer_address: String,
    pub peer_port: u16,
    pub local_address: String,
    pub local_port: u16,
}

impl SessionGrant {
    pub fn encode(&self, writer: &mut Writer) {
        writer.str(&self.user).u32(self.uid).u32(self.gid);
        writer.u32(u32::try_from(self.groups.len()).unwrap_or(u32::MAX));
        for group in &self.groups {
            writer.u32(*group);
        }
        writer.str(&self.home).str(&self.shell);
        writer.bool(self.force_command.is_some());
        writer.str(self.force_command.as_deref().unwrap_or(""));
        writer
            .bool(self.permit_tty)
            .u32(self.max_sessions)
            .bool(self.sftp);
        writer
            .str(&self.peer_address)
            .u32(u32::from(self.peer_port))
            .str(&self.local_address)
            .u32(u32::from(self.local_port));
    }

    pub fn decode(reader: &mut Reader<'_>) -> Result<Self> {
        let user = reader.str()?.to_string();
        let uid = reader.u32()?;
        let gid = reader.u32()?;
        let count = reader.u32()? as usize;
        if count > 65536 {
            return Err(Error::protocol("too many groups"));
        }
        let mut groups = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            groups.push(reader.u32()?);
        }
        let home = reader.str()?.to_string();
        let shell = reader.str()?.to_string();
        let has_force = reader.bool()?;
        let force = reader.str()?.to_string();
        let port =
            |value: u32| u16::try_from(value).map_err(|_| Error::protocol("port out of range"));
        Ok(Self {
            user,
            uid,
            gid,
            groups,
            home,
            shell,
            force_command: has_force.then_some(force),
            permit_tty: reader.bool()?,
            max_sessions: reader.u32()?,
            sftp: reader.bool()?,
            peer_address: reader.str()?.to_string(),
            peer_port: port(reader.u32()?)?,
            local_address: reader.str()?.to_string(),
            local_port: port(reader.u32()?)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    /// One byte per step would have taken the full frame's worth of timeouts
    /// when the timeout was armed once per frame.
    #[test]
    fn a_dribbled_frame_cannot_outlast_the_deadline() {
        let (reader, writer) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let deadline = start + Duration::from_millis(300);
        let dribbler = std::thread::spawn(move || {
            let mut writer = writer;
            // Stops at the far end's close, or by itself well past the deadline.
            for byte in [0u8, 0, 0, 200].into_iter().chain([1; 195]) {
                if writer.write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let error = read_frame(&mut UntilDeadline::new(&reader, deadline)).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(&error, Error::Io(io) if is_timeout(io)),
            "{error:?}"
        );
        // The whole frame would take about 10 s: a busy host must not make this flake.
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        drop(reader);
        dribbler.join().unwrap();
    }

    #[test]
    fn a_passed_deadline_reads_nothing_and_a_frame_in_time_arrives() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        write_frame(&mut writer, b"hello").unwrap();
        let past = Instant::now();
        std::thread::sleep(Duration::from_millis(2));
        let mut adapter = UntilDeadline::new(&reader, past);
        assert!(matches!(
            read_frame(&mut adapter),
            Err(Error::Io(io)) if is_timeout(&io)
        ));
        let mut adapter = UntilDeadline::new(&reader, Instant::now() + Duration::from_secs(5));
        assert_eq!(read_frame(&mut adapter).unwrap(), b"hello");
    }

    #[test]
    fn frames_round_trip_and_oversize_is_refused_before_allocating() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"hello").unwrap();
        assert_eq!(read_frame(&mut Cursor::new(&wire)).unwrap(), b"hello");
        assert!(write_frame(&mut wire, &vec![0; MAX_FRAME + 1]).is_err());
        let hostile = (u32::MAX).to_be_bytes().to_vec();
        assert!(read_frame(&mut Cursor::new(hostile)).is_err());
        assert!(read_frame(&mut Cursor::new(vec![0, 0, 0, 9, 1])).is_err());
    }

    #[test]
    fn transport_state_and_grant_round_trip() {
        let state = TransportState {
            client_version: "SSH-2.0-x".into(),
            session_id: [1; 32],
            tx_key: [2; 64],
            rx_key: [3; 64],
            tx: Counters {
                seq: 4,
                bytes: 5,
                packets: 6,
            },
            rx: Counters {
                seq: 7,
                bytes: 8,
                packets: 9,
            },
        };
        let grant = SessionGrant {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            groups: vec![1000, 5],
            home: "/home/alice".into(),
            shell: "/bin/sh".into(),
            force_command: Some("cmd".into()),
            permit_tty: false,
            max_sessions: 1,
            sftp: true,
            peer_address: "10.0.0.1".into(),
            peer_port: 1234,
            local_address: "10.0.0.2".into(),
            local_port: 22,
        };
        let mut writer = Writer::new();
        state.encode(&mut writer);
        grant.encode(&mut writer);
        let bytes = writer.into_vec();
        let mut reader = Reader::new(&bytes);
        assert_eq!(TransportState::decode(&mut reader).unwrap(), state);
        assert_eq!(SessionGrant::decode(&mut reader).unwrap(), grant);
        assert!(reader.finish().is_ok());
        // Truncation at every length fails without panicking.
        for cut in 0..bytes.len() {
            let mut reader = Reader::new(bytes.get(..cut).unwrap());
            assert!(
                TransportState::decode(&mut reader).is_err()
                    || SessionGrant::decode(&mut reader).is_err()
            );
        }
    }
}
