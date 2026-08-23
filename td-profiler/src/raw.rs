use crate::event::{Event, Kind, StartIdentity};
use std::io::{self, Read, Write};
use std::mem::size_of;

const MAGIC: &[u8; 8] = b"TDPRFRAW";
const ENDIAN: u32 = 0x0102_0304;
pub const VERSION: u32 = 1;
pub const FILE_HEADER_BYTES: usize = 16;
const RECORD_HEADER_BYTES: usize = 40;
const MAX_RECORD_BYTES: usize = 1 << 20;
const MAX_BYTE_FIELD: usize = 1 << 16;
const MAX_CALLCHAIN: usize = 4096;
pub const MAX_RAW_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_DECODED_EVENT_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_DECODED_EVENTS: usize = 524_288;

const TASK: u16 = 1;
const FORK: u16 = 2;
const EXIT: u16 = 3;
const COMM: u16 = 4;
const MMAP: u16 = 5;
const SAMPLE: u16 = 6;
const SWITCH: u16 = 7;
const LOST: u16 = 8;
const ERROR: u16 = 9;
const IGNORED: u16 = 10;

#[derive(Debug, Default)]
pub struct Decoded {
    pub events: Vec<Event>,
    pub unknown_records: u64,
    event_payload_bytes: usize,
}

pub struct Writer<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Writer<W> {
    pub fn new(mut inner: W) -> io::Result<Self> {
        inner.write_all(MAGIC)?;
        inner.write_all(&ENDIAN.to_le_bytes())?;
        inner.write_all(&VERSION.to_le_bytes())?;
        Ok(Self {
            inner,
            bytes: FILE_HEADER_BYTES as u64,
        })
    }

    pub fn write_event(&mut self, event: &Event) -> io::Result<bool> {
        let record = encode(event)?;
        let next = self.bytes.saturating_add(record.len() as u64);
        if next > MAX_RAW_FILE_BYTES {
            return Ok(false);
        }
        self.inner.write_all(&record)?;
        self.bytes = next;
        Ok(true)
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

pub fn read(mut input: impl Read) -> Result<Decoded, String> {
    let mut header = [0u8; FILE_HEADER_BYTES];
    input
        .read_exact(&mut header)
        .map_err(|e| format!("read raw capture header: {e}"))?;
    validate_header(&header)?;
    let mut total = FILE_HEADER_BYTES as u64;
    let mut out = Decoded::default();
    loop {
        let mut record_header = [0u8; 8];
        let read = input
            .read(&mut record_header[..1])
            .map_err(|e| format!("read raw record header: {e}"))?;
        if read == 0 {
            break;
        }
        input
            .read_exact(&mut record_header[1..])
            .map_err(|e| format!("read raw record header: {e}"))?;
        let kind = u16::from_le_bytes([record_header[0], record_header[1]]);
        let flags = u16::from_le_bytes([record_header[2], record_header[3]]);
        let length = usize::try_from(u32::from_le_bytes([
            record_header[4],
            record_header[5],
            record_header[6],
            record_header[7],
        ]))
        .map_err(|_| "raw record length does not fit usize".to_string())?;
        if !(RECORD_HEADER_BYTES..=MAX_RECORD_BYTES).contains(&length) {
            return Err(format!("raw record at {total} has invalid length {length}"));
        }
        let next = total
            .checked_add(length as u64)
            .ok_or("raw capture length overflow")?;
        if next > MAX_RAW_FILE_BYTES {
            return Err(format!("raw capture exceeds {MAX_RAW_FILE_BYTES} bytes"));
        }
        let mut body = vec![0; length.saturating_sub(8)];
        input
            .read_exact(&mut body)
            .map_err(|e| format!("read raw record at {total}: {e}"))?;
        if kind > IGNORED || kind == 0 {
            out.unknown_records = out.unknown_records.saturating_add(1);
        } else {
            let event = decode_known(kind, flags, &body, total as usize)?;
            push_decoded(&mut out, event)?;
        }
        total = next;
    }
    Ok(out)
}

#[cfg(test)]
pub fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    let mut cursor = Cursor::new(bytes);
    if cursor.take(8)? != MAGIC {
        return Err("raw capture magic mismatch".into());
    }
    if cursor.u32()? != ENDIAN {
        return Err("raw capture endian marker mismatch".into());
    }
    let version = cursor.u32()?;
    if version != VERSION {
        return Err(format!("unsupported raw capture version {version}"));
    }
    let mut out = Decoded::default();
    while !cursor.is_empty() {
        let start = cursor.position();
        let kind = cursor.u16()?;
        let flags = cursor.u16()?;
        let length = usize::try_from(cursor.u32()?)
            .map_err(|_| "raw record length does not fit usize".to_string())?;
        if !(RECORD_HEADER_BYTES..=MAX_RECORD_BYTES).contains(&length) {
            return Err(format!("raw record at {start} has invalid length {length}"));
        }
        let body_len = length.saturating_sub(8);
        let body = cursor.take(body_len)?;
        if kind > IGNORED || kind == 0 {
            out.unknown_records = out.unknown_records.saturating_add(1);
            continue;
        }
        let event = decode_known(kind, flags, body, start)?;
        push_decoded(&mut out, event)?;
    }
    Ok(out)
}

fn validate_header(bytes: &[u8]) -> Result<(), String> {
    if bytes.get(..8) != Some(MAGIC) {
        return Err("raw capture magic mismatch".into());
    }
    if bytes.get(8..12) != Some(ENDIAN.to_le_bytes().as_slice()) {
        return Err("raw capture endian marker mismatch".into());
    }
    let version = bytes
        .get(12..16)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or("raw capture has a truncated version")?;
    if version != VERSION {
        return Err(format!("unsupported raw capture version {version}"));
    }
    Ok(())
}

fn push_decoded(out: &mut Decoded, event: Event) -> Result<(), String> {
    if out.events.len() >= MAX_DECODED_EVENTS {
        return Err(format!(
            "raw capture exceeds {MAX_DECODED_EVENTS} decoded events"
        ));
    }
    out.events
        .try_reserve(1)
        .map_err(|_| "decoded event allocation failed".to_string())?;
    let payload = event_payload_bytes(&event);
    let next_payload = out
        .event_payload_bytes
        .checked_add(payload)
        .ok_or("decoded event heap overflow")?;
    let fixed = out
        .events
        .capacity()
        .checked_mul(size_of::<Event>())
        .ok_or("decoded event heap overflow")?;
    if fixed.saturating_add(next_payload) > MAX_DECODED_EVENT_BYTES {
        return Err(format!(
            "raw events expand beyond {MAX_DECODED_EVENT_BYTES} decoded bytes"
        ));
    }
    out.event_payload_bytes = next_payload;
    out.events.push(event);
    Ok(())
}

fn event_payload_bytes(event: &Event) -> usize {
    match &event.kind {
        Kind::Task { comm, .. } => comm.capacity(),
        Kind::Comm { name, .. } => name.capacity(),
        Kind::Mmap { path, .. } => path.capacity(),
        Kind::Sample { callchain, .. } => callchain.capacity().saturating_mul(size_of::<u64>()),
        Kind::Lost { reason, .. } => reason.capacity(),
        Kind::Error { message } => message.capacity(),
        Kind::Fork { .. } | Kind::Exit | Kind::Switch { .. } | Kind::Ignored { .. } => 0,
    }
}

fn encode(event: &Event) -> io::Result<Vec<u8>> {
    let expected = encoded_len(event)?;
    let mut body = Vec::with_capacity(expected.saturating_sub(8));
    body.extend_from_slice(&event.time_ns.to_le_bytes());
    body.extend_from_slice(&event.cpu.to_le_bytes());
    body.extend_from_slice(&event.pid.to_le_bytes());
    body.extend_from_slice(&event.tid.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&event.sequence.to_le_bytes());
    let (kind, flags) = match &event.kind {
        Kind::Task {
            start,
            generation,
            comm,
            valid,
        } => {
            let (start_kind, start_value) = match start {
                StartIdentity::Unknown => (0u32, 0),
                StartIdentity::ProcTicks(value) => (1, *value),
                StartIdentity::PerfTimeNs(value) => (2, *value),
            };
            body.extend_from_slice(&start_kind.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&start_value.to_le_bytes());
            body.extend_from_slice(&generation.to_le_bytes());
            put_bytes(&mut body, comm)?;
            (TASK, u16::from(*valid))
        }
        Kind::Fork {
            parent_pid,
            parent_tid,
        } => {
            body.extend_from_slice(&parent_pid.to_le_bytes());
            body.extend_from_slice(&parent_tid.to_le_bytes());
            (FORK, 0)
        }
        Kind::Exit => (EXIT, 0),
        Kind::Comm { name, exec } => {
            put_bytes(&mut body, name)?;
            (COMM, u16::from(*exec))
        }
        Kind::Mmap {
            address,
            length,
            page_offset,
            major,
            minor,
            inode,
            inode_generation,
            path,
            synthetic,
        } => {
            body.extend_from_slice(&address.to_le_bytes());
            body.extend_from_slice(&length.to_le_bytes());
            body.extend_from_slice(&page_offset.to_le_bytes());
            body.extend_from_slice(&major.to_le_bytes());
            body.extend_from_slice(&minor.to_le_bytes());
            body.extend_from_slice(&inode.to_le_bytes());
            body.extend_from_slice(&inode_generation.to_le_bytes());
            put_bytes(&mut body, path)?;
            (MMAP, u16::from(*synthetic))
        }
        Kind::Sample { ip, callchain } => {
            if callchain.len() > MAX_CALLCHAIN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sample callchain is too long",
                ));
            }
            body.extend_from_slice(&ip.to_le_bytes());
            body.extend_from_slice(&(callchain.len() as u32).to_le_bytes());
            for address in callchain {
                body.extend_from_slice(&address.to_le_bytes());
            }
            (SAMPLE, 0)
        }
        Kind::Switch { out, preempt } => (SWITCH, u16::from(*out) | (u16::from(*preempt) << 1)),
        Kind::Lost { count, reason } => {
            body.extend_from_slice(&count.to_le_bytes());
            put_bytes(&mut body, reason)?;
            (LOST, 0)
        }
        Kind::Error { message } => {
            put_bytes(&mut body, message)?;
            (ERROR, 0)
        }
        Kind::Ignored { perf_kind } => {
            body.extend_from_slice(&perf_kind.to_le_bytes());
            (IGNORED, 0)
        }
    };
    let total = body.len().saturating_add(8);
    if total > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw record is too large",
        ));
    }
    let total = u32::try_from(total)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "raw record length overflow"))?;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&body);
    if out.len() != expected {
        return Err(io::Error::other("internal raw record length mismatch"));
    }
    Ok(out)
}

pub fn encoded_len(event: &Event) -> io::Result<usize> {
    let extra = match &event.kind {
        Kind::Task { comm, .. } => 28usize.checked_add(byte_len(comm)?),
        Kind::Fork { .. } => Some(8),
        Kind::Exit | Kind::Switch { .. } => Some(0),
        Kind::Ignored { .. } => Some(4),
        Kind::Comm { name, .. } | Kind::Error { message: name } => {
            4usize.checked_add(byte_len(name)?)
        }
        Kind::Mmap { path, .. } => 52usize.checked_add(byte_len(path)?),
        Kind::Sample { callchain, .. } => {
            if callchain.len() > MAX_CALLCHAIN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sample callchain is too long",
                ));
            }
            callchain
                .len()
                .checked_mul(8)
                .and_then(|bytes| 12usize.checked_add(bytes))
        }
        Kind::Lost { reason, .. } => 12usize.checked_add(byte_len(reason)?),
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raw record length overflow"))?;
    let total = RECORD_HEADER_BYTES
        .checked_add(extra)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raw record length overflow"))?;
    if total > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw record is too large",
        ));
    }
    Ok(total)
}

fn byte_len(bytes: &[u8]) -> io::Result<usize> {
    if bytes.len() > MAX_BYTE_FIELD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw byte field is too long",
        ));
    }
    Ok(bytes.len())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_BYTE_FIELD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw byte field is too long",
        ));
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "byte field length overflow"))?;
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn decode_known(kind: u16, flags: u16, body: &[u8], offset: usize) -> Result<Event, String> {
    let allowed_flags = match kind {
        TASK | COMM | MMAP => 1,
        SWITCH => 3,
        _ => 0,
    };
    if flags & !allowed_flags != 0 {
        return Err(format!("raw record at {offset} has unknown flag bits"));
    }
    let mut cursor = Cursor::new(body);
    let time_ns = cursor.u64()?;
    let cpu = cursor.u32()?;
    let pid = cursor.u32()?;
    let tid = cursor.u32()?;
    if cursor.u32()? != 0 {
        return Err(format!("raw record at {offset} has nonzero reserved field"));
    }
    let sequence = cursor.u64()?;
    let event_kind = match kind {
        TASK => {
            let start_kind = cursor.u32()?;
            if cursor.u32()? != 0 {
                return Err(format!(
                    "raw task record at {offset} has nonzero reserved field"
                ));
            }
            let start_value = cursor.u64()?;
            let start = match (start_kind, start_value) {
                (0, 0) => StartIdentity::Unknown,
                (0, _) => {
                    return Err(format!(
                        "raw task record at {offset} has a value for unknown identity"
                    ));
                }
                (1, value) => StartIdentity::ProcTicks(value),
                (2, value) => StartIdentity::PerfTimeNs(value),
                _ => {
                    return Err(format!(
                        "raw task record at {offset} has unknown start identity"
                    ));
                }
            };
            Kind::Task {
                start,
                generation: cursor.u64()?,
                comm: cursor.bytes()?,
                valid: flags & 1 != 0,
            }
        }
        FORK => Kind::Fork {
            parent_pid: cursor.u32()?,
            parent_tid: cursor.u32()?,
        },
        EXIT => Kind::Exit,
        COMM => Kind::Comm {
            name: cursor.bytes()?,
            exec: flags & 1 != 0,
        },
        MMAP => Kind::Mmap {
            address: cursor.u64()?,
            length: cursor.u64()?,
            page_offset: cursor.u64()?,
            major: cursor.u32()?,
            minor: cursor.u32()?,
            inode: cursor.u64()?,
            inode_generation: cursor.u64()?,
            path: cursor.bytes()?,
            synthetic: flags & 1 != 0,
        },
        SAMPLE => {
            let ip = cursor.u64()?;
            let count = usize::try_from(cursor.u32()?)
                .map_err(|_| "callchain length does not fit usize".to_string())?;
            if count > MAX_CALLCHAIN {
                return Err(format!("raw record at {offset} has oversized callchain"));
            }
            let mut callchain = Vec::with_capacity(count);
            for _ in 0..count {
                callchain.push(cursor.u64()?);
            }
            Kind::Sample { ip, callchain }
        }
        SWITCH => Kind::Switch {
            out: flags & 1 != 0,
            preempt: flags & 2 != 0,
        },
        LOST => Kind::Lost {
            count: cursor.u64()?,
            reason: cursor.bytes()?,
        },
        ERROR => Kind::Error {
            message: cursor.bytes()?,
        },
        IGNORED => Kind::Ignored {
            perf_kind: cursor.u32()?,
        },
        _ => return Err("internal unknown raw record".into()),
    };
    if !cursor.is_empty() {
        return Err(format!("raw record at {offset} has trailing bytes"));
    }
    Ok(Event {
        time_ns,
        cpu,
        sequence,
        pid,
        tid,
        kind: event_kind,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    #[cfg(test)]
    fn position(&self) -> usize {
        self.at
    }

    fn is_empty(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(length)
            .ok_or_else(|| "raw record offset overflow".to_string())?;
        let value = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| format!("truncated raw record at byte {}", self.at))?;
        self.at = end;
        Ok(value)
    }

    #[cfg(test)]
    fn u16(&mut self) -> Result<u16, String> {
        let bytes: [u8; 2] = self.take(2)?.try_into().map_err(|_| "truncated u16")?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| "truncated u32")?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| "truncated u64")?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| "byte-field length does not fit usize".to_string())?;
        if length > MAX_BYTE_FIELD {
            return Err(format!("raw byte field exceeds {MAX_BYTE_FIELD} bytes"));
        }
        Ok(self.take(length)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        decode, read, Writer, ENDIAN, MAGIC, MAX_DECODED_EVENTS, MAX_DECODED_EVENT_BYTES,
        MAX_RAW_FILE_BYTES, VERSION,
    };
    use crate::event::{Event, Kind, StartIdentity};

    fn sample() -> Event {
        Event {
            time_ns: 99,
            cpu: 2,
            sequence: 7,
            pid: 10,
            tid: 11,
            kind: Kind::Sample {
                ip: 0x1234,
                callchain: vec![0x1234, 0x5678],
            },
        }
    }

    #[test]
    fn version_one_round_trips_and_unknown_kinds_are_skipped() {
        let mut bytes = Vec::new();
        {
            let mut writer = Writer::new(&mut bytes).unwrap();
            assert!(writer.write_event(&sample()).unwrap());
        }
        bytes.extend_from_slice(&77u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 32]);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.events, vec![sample()]);
        assert_eq!(decoded.unknown_records, 1);
        let streamed = read(bytes.as_slice()).unwrap();
        assert_eq!(streamed.events, vec![sample()]);
        assert_eq!(streamed.unknown_records, 1);
    }

    #[test]
    fn decoded_event_roster_and_payload_fit_the_heap_ceiling() {
        let fixed = MAX_DECODED_EVENTS
            .checked_mul(std::mem::size_of::<Event>())
            .unwrap();
        assert!(fixed + MAX_RAW_FILE_BYTES as usize <= MAX_DECODED_EVENT_BYTES);
    }

    #[test]
    fn task_snapshots_round_trip_exact_identity_and_generation() {
        let event = Event {
            time_ns: 101,
            cpu: 0,
            sequence: 9,
            pid: 42,
            tid: 42,
            kind: Kind::Task {
                start: StartIdentity::PerfTimeNs(88),
                generation: 7,
                comm: b"worker".to_vec(),
                valid: false,
            },
        };
        let mut bytes = Vec::new();
        Writer::new(&mut bytes)
            .unwrap()
            .write_event(&event)
            .unwrap();
        assert_eq!(decode(&bytes).unwrap().events, vec![event]);
    }

    #[test]
    fn ignored_kernel_records_round_trip_as_known_evidence() {
        let event = Event {
            time_ns: 101,
            cpu: 3,
            sequence: 9,
            pid: 42,
            tid: 43,
            kind: Kind::Ignored { perf_kind: 6 },
        };
        let mut bytes = Vec::new();
        Writer::new(&mut bytes)
            .unwrap()
            .write_event(&event)
            .unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.events, vec![event]);
        assert_eq!(decoded.unknown_records, 0);
    }

    #[test]
    fn rejects_hostile_lengths_reserved_fields_and_versions() {
        let mut short = Vec::new();
        short.extend_from_slice(MAGIC);
        short.extend_from_slice(&ENDIAN.to_le_bytes());
        short.extend_from_slice(&VERSION.to_le_bytes());
        short.extend_from_slice(&1u16.to_le_bytes());
        short.extend_from_slice(&0u16.to_le_bytes());
        short.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&short).unwrap_err().contains("invalid length"));

        let mut bytes = Vec::new();
        {
            let mut writer = Writer::new(&mut bytes).unwrap();
            writer.write_event(&sample()).unwrap();
        }
        *bytes.get_mut(44).unwrap() = 1;
        assert!(decode(&bytes).unwrap_err().contains("reserved"));
        *bytes.get_mut(44).unwrap() = 0;

        bytes
            .get_mut(18..20)
            .unwrap()
            .copy_from_slice(&0x8000u16.to_le_bytes());
        assert!(decode(&bytes).unwrap_err().contains("flag bits"));
        bytes
            .get_mut(18..20)
            .unwrap()
            .copy_from_slice(&0u16.to_le_bytes());

        let version = bytes.get_mut(12).unwrap();
        *version = 2;
        assert!(decode(&bytes).unwrap_err().contains("unsupported"));
    }
}
