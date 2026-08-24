use crate::event::{Event, Kind};

const PERF_RECORD_MMAP: u32 = 1;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_COMM: u32 = 3;
const PERF_RECORD_EXIT: u32 = 4;
const PERF_RECORD_FORK: u32 = 7;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MMAP2: u32 = 10;
const PERF_RECORD_SWITCH: u32 = 14;
const PERF_RECORD_SWITCH_CPU_WIDE: u32 = 15;

const PERF_RECORD_MISC_COMM_EXEC: u16 = 1 << 13;
const PERF_RECORD_MISC_SWITCH_OUT: u16 = 1 << 13;
const PERF_RECORD_MISC_SWITCH_OUT_PREEMPT: u16 = 1 << 14;
const TRAILER_BYTES: usize = 32;
pub const MAX_KERNEL_RECORD: usize = u16::MAX as usize;
pub const MAX_KERNEL_CALLCHAIN: usize = 4096;

pub fn decode(record: &[u8], ring_cpu: u32, sequence: u64) -> Result<Event, String> {
    if record.len() < 8 {
        return Err("perf record is shorter than its header".into());
    }
    let kind = le32(record, 0)?;
    let misc = le16(record, 4)?;
    let size = usize::from(le16(record, 6)?);
    if size != record.len() || size > MAX_KERNEL_RECORD {
        return Err(format!(
            "perf record length mismatch: header {size}, ring {}",
            record.len()
        ));
    }
    if kind == PERF_RECORD_SAMPLE {
        return sample(
            record.get(8..).ok_or("truncated sample")?,
            ring_cpu,
            sequence,
        );
    }
    if record.len() < 8 + TRAILER_BYTES {
        return Err("perf metadata record lacks the fixed sample-id trailer".into());
    }
    let trailer_at = record.len().saturating_sub(TRAILER_BYTES);
    let body = record
        .get(8..trailer_at)
        .ok_or("invalid perf metadata body")?;
    if body.len() % 8 != 0 {
        return Err("perf metadata body is not 64-bit aligned".into());
    }
    let trailer = record
        .get(trailer_at..)
        .ok_or("invalid perf metadata trailer")?;
    let trailer_pid = le32(trailer, 0)?;
    let trailer_tid = le32(trailer, 4)?;
    let time_ns = le64(trailer, 8)?;
    let cpu = le32(trailer, 16)?;
    if le32(trailer, 20)? != 0 {
        return Err("perf CPU trailer reserved word is nonzero".into());
    }
    let _identifier = le64(trailer, 24)?;
    if cpu != ring_cpu {
        return Err(format!(
            "perf record says CPU {cpu} but came from ring {ring_cpu}"
        ));
    }
    let (pid, tid, time_ns) = match kind {
        PERF_RECORD_FORK | PERF_RECORD_EXIT => {
            require_min_len(body, 24, "task")?;
            // FORK/EXIT have their own event timestamp before the sample-id
            // trailer. Linux populates the trailer first and the task body
            // later, so the two clock reads are not an equality invariant.
            // The fixed task field is the event's semantic timestamp.
            let body_time_ns = le64(body, 16)?;
            if body_time_ns < time_ns {
                return Err("perf task body time precedes trailer time".into());
            }
            (le32(body, 0)?, le32(body, 8)?, body_time_ns)
        }
        PERF_RECORD_COMM | PERF_RECORD_MMAP | PERF_RECORD_MMAP2 => {
            if body.len() < 8 {
                return Err("perf process record has no pid/tid fields".into());
            }
            (le32(body, 0)?, le32(body, 4)?, time_ns)
        }
        _ => (trailer_pid, trailer_tid, time_ns),
    };
    let event_kind = match kind {
        PERF_RECORD_FORK => Kind::Fork {
            parent_pid: le32(body, 4)?,
            parent_tid: le32(body, 12)?,
        },
        PERF_RECORD_EXIT => Kind::Exit,
        PERF_RECORD_COMM => {
            if body.len() < 16 {
                return Err("perf comm record is too short".into());
            }
            let name = nul_bytes(body.get(8..).ok_or("truncated comm")?)?;
            Kind::Comm {
                name,
                exec: misc & PERF_RECORD_MISC_COMM_EXEC != 0,
            }
        }
        PERF_RECORD_MMAP2 => mmap2(body, false)?,
        PERF_RECORD_MMAP => mmap(body)?,
        PERF_RECORD_LOST => {
            require_min_len(body, 16, "lost")?;
            Kind::Lost {
                count: le64(body, 8)?,
                reason: b"kernel-perf-lost".to_vec(),
            }
        }
        PERF_RECORD_SWITCH => {
            require_min_len(body, 0, "switch")?;
            Kind::Switch {
                out: misc & PERF_RECORD_MISC_SWITCH_OUT != 0,
                preempt: misc & PERF_RECORD_MISC_SWITCH_OUT_PREEMPT != 0,
            }
        }
        PERF_RECORD_SWITCH_CPU_WIDE => {
            require_min_len(body, 8, "switch-cpu-wide")?;
            Kind::Switch {
                out: misc & PERF_RECORD_MISC_SWITCH_OUT != 0,
                preempt: misc & PERF_RECORD_MISC_SWITCH_OUT_PREEMPT != 0,
            }
        }
        other => Kind::Ignored { perf_kind: other },
    };
    Ok(Event {
        time_ns,
        cpu,
        sequence,
        pid,
        tid,
        kind: event_kind,
    })
}

fn sample(body: &[u8], ring_cpu: u32, sequence: u64) -> Result<Event, String> {
    let mut cursor = Cursor::new(body);
    let _identifier = cursor.u64()?;
    let ip = cursor.u64()?;
    let pid = cursor.u32()?;
    let tid = cursor.u32()?;
    let time_ns = cursor.u64()?;
    let cpu = cursor.u32()?;
    if cursor.u32()? != 0 {
        return Err("perf sample CPU reserved word is nonzero".into());
    }
    let count = usize::try_from(cursor.u64()?)
        .map_err(|_| "perf callchain count does not fit usize".to_string())?;
    if count > MAX_KERNEL_CALLCHAIN {
        return Err(format!(
            "perf callchain exceeds {MAX_KERNEL_CALLCHAIN} frames"
        ));
    }
    let mut callchain = Vec::with_capacity(count);
    for _ in 0..count {
        callchain.push(cursor.u64()?);
    }
    if !cursor.is_empty() {
        return Err("perf sample has unexpected trailing fields".into());
    }
    if cpu != ring_cpu {
        return Err(format!(
            "perf sample says CPU {cpu} but came from ring {ring_cpu}"
        ));
    }
    Ok(Event {
        time_ns,
        cpu,
        sequence,
        pid,
        tid,
        kind: Kind::Sample { ip, callchain },
    })
}

fn mmap2(body: &[u8], synthetic: bool) -> Result<Kind, String> {
    if body.len() < 72 {
        return Err("perf mmap2 record is too short".into());
    }
    Ok(Kind::Mmap {
        address: le64(body, 8)?,
        length: le64(body, 16)?,
        page_offset: le64(body, 24)?,
        major: le32(body, 32)?,
        minor: le32(body, 36)?,
        inode: le64(body, 40)?,
        inode_generation: le64(body, 48)?,
        path: nul_bytes(body.get(64..).ok_or("truncated mmap2 filename")?)?,
        synthetic,
    })
}

fn mmap(body: &[u8]) -> Result<Kind, String> {
    if body.len() < 40 {
        return Err("perf mmap record is too short".into());
    }
    Ok(Kind::Mmap {
        address: le64(body, 8)?,
        length: le64(body, 16)?,
        page_offset: le64(body, 24)?,
        major: 0,
        minor: 0,
        inode: 0,
        inode_generation: 0,
        path: nul_bytes(body.get(32..).ok_or("truncated mmap filename")?)?,
        synthetic: false,
    })
}

fn nul_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("perf string is not NUL terminated")?;
    let trailing = bytes
        .get(end.saturating_add(1)..)
        .ok_or("perf string overflow")?;
    if trailing.iter().any(|byte| *byte != 0) {
        return Err("perf string padding is nonzero".into());
    }
    Ok(bytes.get(..end).ok_or("perf string overflow")?.to_vec())
}

fn require_min_len(bytes: &[u8], expected: usize, label: &str) -> Result<(), String> {
    if bytes.len() >= expected {
        Ok(())
    } else {
        Err(format!(
            "perf {label} body has {} bytes, expected at least {expected}",
            bytes.len()
        ))
    }
}

fn le16(bytes: &[u8], at: usize) -> Result<u16, String> {
    let value: [u8; 2] = bytes
        .get(at..at.saturating_add(2))
        .ok_or("truncated perf u16")?
        .try_into()
        .map_err(|_| "truncated perf u16")?;
    Ok(u16::from_le_bytes(value))
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let value: [u8; 4] = bytes
        .get(at..at.saturating_add(4))
        .ok_or("truncated perf u32")?
        .try_into()
        .map_err(|_| "truncated perf u32")?;
    Ok(u32::from_le_bytes(value))
}

fn le64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let value: [u8; 8] = bytes
        .get(at..at.saturating_add(8))
        .ok_or("truncated perf u64")?
        .try_into()
        .map_err(|_| "truncated perf u64")?;
    Ok(u64::from_le_bytes(value))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn is_empty(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(length).ok_or("perf cursor overflow")?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or("truncated perf record")?;
        self.at = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let value: [u8; 4] = self.take(4)?.try_into().map_err(|_| "truncated perf u32")?;
        Ok(u32::from_le_bytes(value))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let value: [u8; 8] = self.take(8)?.try_into().map_err(|_| "truncated perf u64")?;
        Ok(u64::from_le_bytes(value))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{decode, PERF_RECORD_FORK, PERF_RECORD_MMAP, PERF_RECORD_SAMPLE};
    use crate::event::{Event, Kind};

    fn sample_record(callchain: &[u64]) -> Vec<u8> {
        let size = 8 + 8 + 8 + 8 + 8 + 8 + callchain.len() * 8 + 8;
        let mut out = Vec::new();
        out.extend_from_slice(&PERF_RECORD_SAMPLE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(size as u16).to_le_bytes());
        out.extend_from_slice(&55u64.to_le_bytes());
        out.extend_from_slice(&0x1234u64.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&10u32.to_le_bytes());
        out.extend_from_slice(&99u64.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(callchain.len() as u64).to_le_bytes());
        for address in callchain {
            out.extend_from_slice(&address.to_le_bytes());
        }
        out
    }

    fn mmap_record() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PERF_RECORD_MMAP.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&80u16.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&10u32.to_le_bytes());
        out.extend_from_slice(&0x4000u64.to_le_bytes());
        out.extend_from_slice(&0x1000u64.to_le_bytes());
        out.extend_from_slice(&0x2000u64.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&10u32.to_le_bytes());
        out.extend_from_slice(&99u64.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&55u64.to_le_bytes());
        out
    }

    fn fork_record() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PERF_RECORD_FORK.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&64u16.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&77u64.to_le_bytes());
        out.extend_from_slice(&999u32.to_le_bytes());
        out.extend_from_slice(&998u32.to_le_bytes());
        out.extend_from_slice(&77u64.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&55u64.to_le_bytes());
        out
    }

    fn metadata_record(kind: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&40u16.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&10u32.to_le_bytes());
        out.extend_from_slice(&99u64.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&55u64.to_le_bytes());
        out
    }

    #[test]
    fn decodes_the_pinned_sample_layout() {
        assert_eq!(
            decode(&sample_record(&[0x1234, 0x5678]), 2, 7).unwrap(),
            Event {
                time_ns: 99,
                cpu: 2,
                sequence: 7,
                pid: 9,
                tid: 10,
                kind: Kind::Sample {
                    ip: 0x1234,
                    callchain: vec![0x1234, 0x5678]
                },
            }
        );
    }

    #[test]
    fn decodes_the_kernel_order_and_minimum_padded_mmap() {
        let event = decode(&mmap_record(), 2, 8).unwrap();
        assert_eq!((event.pid, event.tid, event.time_ns), (9, 10, 99));
        assert!(matches!(
            event.kind,
            Kind::Mmap {
                address: 0x4000,
                length: 0x1000,
                page_offset: 0x2000,
                ref path,
                ..
            } if path.is_empty()
        ));
    }

    #[test]
    fn rejects_cpu_mismatch_and_hostile_callchains() {
        assert!(decode(&sample_record(&[]), 1, 0)
            .unwrap_err()
            .contains("CPU"));
        let mut record = sample_record(&[]);
        let count = record.get_mut(48..56).unwrap();
        count.copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&record, 2, 0).unwrap_err().contains("callchain"));
    }

    #[test]
    fn task_records_use_the_body_identity_and_event_time() {
        let event = decode(&fork_record(), 2, 4).unwrap();
        assert_eq!((event.pid, event.tid, event.time_ns), (9, 9, 77));
        assert!(matches!(
            event.kind,
            Kind::Fork {
                parent_pid: 1,
                parent_tid: 1
            }
        ));

        let mut mismatched = fork_record();
        mismatched
            .get_mut(24..32)
            .unwrap()
            .copy_from_slice(&78u64.to_le_bytes());
        let event = decode(&mismatched, 2, 4).unwrap();
        assert_eq!((event.pid, event.tid, event.time_ns), (9, 9, 78));

        mismatched
            .get_mut(24..32)
            .unwrap()
            .copy_from_slice(&76u64.to_le_bytes());
        assert!(decode(&mismatched, 2, 4)
            .unwrap_err()
            .contains("body time precedes trailer"));
    }

    #[test]
    fn normal_or_future_metadata_records_are_durable_but_non_corrupting() {
        for kind in [5, 6, 99] {
            assert!(matches!(
                decode(&metadata_record(kind), 2, 4).unwrap().kind,
                Kind::Ignored { perf_kind } if perf_kind == kind
            ));
        }

        let mut extended = fork_record();
        extended.splice(32..32, [0u8; 8]);
        extended
            .get_mut(6..8)
            .unwrap()
            .copy_from_slice(&72u16.to_le_bytes());
        assert!(matches!(
            decode(&extended, 2, 4).unwrap().kind,
            Kind::Fork { .. }
        ));
    }
}
