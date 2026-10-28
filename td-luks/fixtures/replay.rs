//! Bounded, test-only container for the cryptsetup reencryption write trace.
use std::io::Read;

pub const DATA_OFFSET: usize = 16 << 20;
pub const DATA_LEN: usize = 2 << 20;
pub const DISK_LEN: usize = DATA_OFFSET + DATA_LEN;
pub const PREFIX_LEN: usize = 548864;
const MAGIC: &[u8] = b"TDLUKSR1";
const MAX_TRACE: usize = 8 << 20;
const MAX_RECORDS: usize = 4096;

pub struct Write {
    pub offset: usize,
    pub data: Vec<u8>,
}

fn integer(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let end = offset.checked_add(8).ok_or("integer offset overflow")?;
    let field = bytes.get(offset..end).ok_or("truncated integer")?;
    Ok(u64::from_le_bytes(
        field.try_into().map_err(|_| "short integer")?,
    ))
}

pub fn initial(base: &[u8]) -> Result<Vec<u8>, String> {
    if base.len() != PREFIX_LEN + DATA_LEN {
        return Err("wrong initial fixture length".into());
    }
    let mut image = vec![0; DISK_LEN];
    image
        .get_mut(..PREFIX_LEN)
        .ok_or("prefix bounds")?
        .copy_from_slice(base.get(..PREFIX_LEN).ok_or("short prefix")?);
    image
        .get_mut(DATA_OFFSET..)
        .ok_or("data bounds")?
        .copy_from_slice(base.get(PREFIX_LEN..).ok_or("short data")?);
    Ok(image)
}

pub fn decode(trace: &[u8]) -> Result<Vec<Write>, String> {
    if trace.len() > MAX_TRACE || trace.get(..8) != Some(MAGIC) {
        return Err("invalid reencryption trace".into());
    }
    let mut at = 8usize;
    let mut writes = Vec::new();
    while at < trace.len() {
        if writes.len() == MAX_RECORDS {
            return Err("too many writes".into());
        }
        let offset = usize::try_from(integer(trace, at)?).map_err(|_| "offset overflow")?;
        let len = usize::try_from(integer(trace, at + 8)?).map_err(|_| "length overflow")?;
        at += 16;
        let end = offset.checked_add(len).ok_or("write overflow")?;
        if end > DISK_LEN
            || offset % 512 != 0
            || len % 512 != 0
            || (len != 0 && offset < PREFIX_LEN && end > 32768)
        {
            return Err("write outside captured metadata/resilience/data ranges".into());
        }
        let next = at.checked_add(len).ok_or("trace overflow")?;
        let data = trace.get(at..next).ok_or("truncated write")?.to_vec();
        writes.push(Write { offset, data });
        at = next;
    }
    if writes.is_empty() {
        return Err("empty trace".into());
    }
    Ok(writes)
}

pub fn apply(image: &mut [u8], write: &Write) -> Result<(), String> {
    let end = write
        .offset
        .checked_add(write.data.len())
        .ok_or("write overflow")?;
    image
        .get_mut(write.offset..end)
        .ok_or("write bounds")?
        .copy_from_slice(&write.data);
    Ok(())
}

/// Convert QEMU blklogwrites v1 records, stopping before the first old-slot
/// erasure. The logged guest completes normally; this retains every prefix
/// while both volume keys still exist. Flush records are empty writes.
pub fn capture<R: Read>(log: &mut R) -> Result<Vec<u8>, String> {
    fn sector<R: Read>(log: &mut R) -> Result<[u8; 512], String> {
        let mut bytes = [0; 512];
        log.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        Ok(bytes)
    }
    let header = sector(log)?;
    if integer(&header, 0)? != 0x6a736677736872
        || integer(&header, 8)? != 1
        || header.get(24..28) != Some(512u32.to_le_bytes().as_slice())
    {
        return Err("unsupported QEMU write log".into());
    }
    let count = usize::try_from(integer(&header, 16)?).map_err(|_| "count overflow")?;
    if count == 0 || count > MAX_RECORDS {
        return Err("write log count out of bounds".into());
    }
    let mut trace = MAGIC.to_vec();
    for _ in 0..count {
        let entry = sector(log)?;
        let offset = integer(&entry, 0)?
            .checked_mul(512)
            .ok_or("offset overflow")?;
        let len = integer(&entry, 8)?
            .checked_mul(512)
            .ok_or("length overflow")?;
        let flags = integer(&entry, 16)?;
        if flags > 1 || integer(&entry, 24)? != 0 || (flags == 1 && len != 0) {
            return Err("unsupported QEMU record flags/data".into());
        }
        let end = offset.checked_add(len).ok_or("write overflow")?;
        if end > DISK_LEN as u64 {
            return Err("QEMU write past disk".into());
        }
        if len != 0 && offset < PREFIX_LEN as u64 && end > 32768 {
            decode(&trace)?;
            return Ok(trace);
        }
        let len = usize::try_from(len).map_err(|_| "write too large")?;
        if trace.len() + 16 + len > MAX_TRACE {
            return Err("trace too large".into());
        }
        trace.extend_from_slice(&offset.to_le_bytes());
        trace.extend_from_slice(&(len as u64).to_le_bytes());
        let at = trace.len();
        trace.resize(at + len, 0);
        log.read_exact(trace.get_mut(at..).ok_or("trace bounds")?)
            .map_err(|e| e.to_string())?;
    }
    Err("log did not reach old-keyslot erasure".into())
}
