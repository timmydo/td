//! Bounded data work through already-open O_DIRECT kernel crypt mappings.
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use td_luks::reencrypt::{Area, Hotzone, Resilience, HOTZONE_SIZE};
use td_luks::write::{DATA_OFFSET, KEYSLOTS_OFFSET};
#[repr(align(4096))]
struct Block {
    bytes: [u8; 4096],
}
impl Drop for Block {
    fn drop(&mut self) {
        self.bytes.fill(0);
        std::hint::black_box(&mut self.bytes);
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub struct Data {
    pub raw: File,
    pub old: File,
    pub new: File,
    pub size: u64,
}
impl Data {
    fn bounds(&self, zone: Hotzone) -> io::Result<()> {
        if zone.size == 0
            || !zone.size.is_multiple_of(4096)
            || !zone.offset.is_multiple_of(4096)
            || zone
                .offset
                .checked_add(zone.size)
                .is_none_or(|end| end > self.size)
            || DATA_OFFSET.checked_add(self.size).is_none()
        {
            return Err(invalid("data hotzone bounds"));
        }
        Ok(())
    }
    fn capacity(&self, zone: Hotzone, area: Area) -> io::Result<()> {
        self.bounds(zone)?;
        let needed = match area.resilience {
            Resilience::Checksum => zone.size / 4096 * 32,
            Resilience::Journal => zone.size,
        };
        if needed > area.size
            || area.offset < KEYSLOTS_OFFSET
            || !area.offset.is_multiple_of(4096)
            || !area.size.is_multiple_of(4096)
            || area
                .offset
                .checked_add(area.size)
                .is_none_or(|end| end > DATA_OFFSET)
        {
            return Err(invalid("resilience capacity or bounds"));
        }
        Ok(())
    }
    /// Snapshot only raw ciphertext or its hashes, while callers exclude writers.
    pub fn protect(&self, zone: Hotzone, area: Area) -> io::Result<Vec<u8>> {
        self.capacity(zone, area)?;
        if zone.size > HOTZONE_SIZE {
            return Err(invalid("new hotzone exceeds memory bound"));
        }
        let size = match area.resilience {
            Resilience::Checksum => zone.size / 4096 * 32,
            Resilience::Journal => zone.size,
        };
        let mut out = Vec::with_capacity(size as usize);
        let mut raw = Box::new(Block { bytes: [0; 4096] });
        for delta in (0..zone.size).step_by(4096) {
            self.raw
                .read_exact_at(&mut raw.bytes, DATA_OFFSET + zone.offset + delta)?;
            match area.resilience {
                Resilience::Checksum => out.extend_from_slice(&td_fido::digest(&raw.bytes)),
                Resilience::Journal => out.extend_from_slice(&raw.bytes),
            }
        }
        Ok(out)
    }
    /// The caller has committed the dirty header and retained resilience.
    pub fn rewrite(&self, zone: Hotzone) -> io::Result<()> {
        self.bounds(zone)?;
        let mut plain = Box::new(Block { bytes: [0; 4096] });
        for delta in (0..zone.size).step_by(4096) {
            let at = zone.offset + delta;
            self.old.read_exact_at(&mut plain.bytes, at)?;
            self.new.write_all_at(&plain.bytes, at)?;
        }
        self.new.sync_all()
    }
    /// A dirty journal is always replayed from its old ciphertext. A checksum
    /// block already differing from its old checksum is treated as rewritten.
    pub fn recover(&self, metadata: &File, zone: Hotzone, area: Area) -> io::Result<()> {
        self.capacity(zone, area)?;
        let mut raw = Box::new(Block { bytes: [0; 4096] });
        let mut plain = Box::new(Block { bytes: [0; 4096] });
        for delta in (0..zone.size).step_by(4096) {
            let at = zone.offset + delta;
            match area.resilience {
                Resilience::Journal => {
                    metadata.read_exact_at(&mut raw.bytes, area.offset + delta)?;
                    self.raw.write_all_at(&raw.bytes, DATA_OFFSET + at)?;
                }
                Resilience::Checksum => {
                    let mut old = [0; 32];
                    metadata.read_exact_at(&mut old, area.offset + delta / 4096 * 32)?;
                    self.raw.read_exact_at(&mut raw.bytes, DATA_OFFSET + at)?;
                    if !td_fido::same(&old, &td_fido::digest(&raw.bytes)) {
                        continue;
                    }
                }
            }
            self.old.read_exact_at(&mut plain.bytes, at)?;
            self.new.write_all_at(&plain.bytes, at)?;
        }
        self.new.sync_all()
    }
}
