//! A single legacy virtio-blk PCI function with one split virtqueue.
//! Guest addresses are offsets in the VM's memory file, never host pointers.
use crate::invalid;
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::Path,
};

const QUEUE: u16 = 128;
const BAR_SIZE: u16 = 64;
const IRQ: u8 = 11;
const FLUSH: u32 = 1 << 9;
const READ_ONLY: u32 = 1 << 5;

pub(super) struct Block {
    disk: File,
    size: u64,
    writable: bool,
    config_address: u32,
    command: u16,
    bar: u32,
    queue_select: u16,
    pfn: u32,
    avail: u16,
    used: u16,
    negotiated: u32,
    status: u8,
    irq: bool,
}

impl Block {
    pub(super) fn open(path: &Path, writable: bool) -> io::Result<Self> {
        let disk = OpenOptions::new()
            .read(true)
            .write(writable)
            .custom_flags(0o400000 | 0o4000)
            .open(path)?;
        if writable {
            disk.try_lock()
        } else {
            disk.try_lock_shared()
        }
        .map_err(io::Error::from)?;
        let meta = disk.metadata()?;
        if !meta.is_file() || meta.len() == 0 || !meta.len().is_multiple_of(512) {
            return Err(invalid(
                "raw disk must be a nonempty sector-aligned regular file",
            ));
        }
        Ok(Self {
            disk,
            size: meta.len(),
            writable,
            config_address: 0,
            command: 0,
            bar: 0xc001,
            queue_select: 0,
            pfn: 0,
            avail: 0,
            used: 0,
            negotiated: 0,
            status: 0,
            irq: false,
        })
    }

    pub(super) fn irq(&self) -> [u32; 2] {
        [
            u32::from(IRQ),
            u32::from(self.irq && self.command & 0x400 == 0),
        ]
    }

    fn config(&mut self, port: u16, word: &mut [u8], read: bool) -> io::Result<()> {
        let address = self.config_address;
        let offset = ((address & 0xfc) + u32::from(port - 0xcfc)) as usize;
        if address & 0x80ffff00 != 0x80000800 && address & 0x80ffff00 != 0x80000000 {
            return Ok(());
        }
        let bridge = address & 0xf800 == 0;
        if !read {
            if bridge {
                return Ok(());
            }
            match (offset, word.len()) {
                (4, 2 | 4) => self.command = (value(word)? as u16) & 0x405,
                (0x10, 4) => self.bar = (value(word)? & !u32::from(BAR_SIZE - 1)) | 1,
                _ => {}
            }
            return Ok(());
        }
        let mut config = [0; 256];
        if bridge {
            store(&mut config, 0, &0x12378086u32.to_le_bytes())?;
            store(&mut config, 8, &0x06000000u32.to_le_bytes())?;
        } else {
            store(&mut config, 0, &0x10011af4u32.to_le_bytes())?;
            store(&mut config, 4, &self.command.to_le_bytes())?;
            store(&mut config, 8, &0x01000000u32.to_le_bytes())?;
            store(&mut config, 0x10, &self.bar.to_le_bytes())?;
            store(&mut config, 0x2c, &0x00021af4u32.to_le_bytes())?;
            store(&mut config, 0x3c, &[IRQ, 1])?;
        }
        if let Some(part) = config.get(offset..).and_then(|b| b.get(..word.len())) {
            word.copy_from_slice(part);
        }
        Ok(())
    }

    pub(super) fn port(
        &mut self,
        port: u16,
        word: &mut [u8],
        read: bool,
        mem: &File,
    ) -> io::Result<()> {
        if port == 0xcf8 && word.len() == 4 {
            if read {
                word.copy_from_slice(&self.config_address.to_le_bytes());
            } else {
                self.config_address = value(word)?;
            }
        } else if (0xcfc..=0xcff).contains(&port) {
            self.config(port, word, read)?;
        } else if self.command & 1 != 0 {
            let base = self.bar & !u32::from(BAR_SIZE - 1);
            if let Some(offset) = u32::from(port)
                .checked_sub(base)
                .filter(|n| *n < u32::from(BAR_SIZE))
            {
                self.register(offset as usize, word, read, mem)?;
            }
        }
        Ok(())
    }

    fn register(
        &mut self,
        offset: usize,
        word: &mut [u8],
        read: bool,
        mem: &File,
    ) -> io::Result<()> {
        if read {
            let mut regs = [0; 64];
            let features = FLUSH | if self.writable { 0 } else { READ_ONLY };
            store(&mut regs, 0, &features.to_le_bytes())?;
            store(&mut regs, 4, &self.negotiated.to_le_bytes())?;
            if self.queue_select == 0 {
                store(&mut regs, 8, &self.pfn.to_le_bytes())?;
                store(&mut regs, 12, &QUEUE.to_le_bytes())?;
            }
            store(&mut regs, 14, &self.queue_select.to_le_bytes())?;
            store(&mut regs, 18, &[self.status, u8::from(self.irq)])?;
            store(&mut regs, 20, &(self.size / 512).to_le_bytes())?;
            if let Some(b) = regs.get(offset..).and_then(|s| s.get(..word.len())) {
                word.copy_from_slice(b);
            }
            if offset == 19 {
                self.irq = false;
            }
        } else {
            match (offset, word.len()) {
                (4, 4) => {
                    self.negotiated =
                        value(word)? & (FLUSH | if self.writable { 0 } else { READ_ONLY })
                }
                (8, 4) if self.queue_select == 0 => {
                    self.pfn = value(word)?;
                    self.avail = 0;
                    self.used = 0;
                }
                (14, 2) => self.queue_select = value(word)? as u16,
                (16, 2)
                    if value(word)? == 0
                        && self.status & 0x84 == 4
                        && self.command & 4 != 0
                        && self.pfn != 0 =>
                {
                    self.drain(mem)?
                }
                (18, 1) => {
                    self.status = value(word)? as u8;
                    if self.status == 0 {
                        self.negotiated = 0;
                        self.irq = false;
                        self.pfn = 0;
                        self.avail = 0;
                        self.used = 0;
                        self.queue_select = 0;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn drain(&mut self, mem: &File) -> io::Result<()> {
        let ram_size = mem.metadata()?.len();
        let table = u64::from(self.pfn) << 12;
        let avail = table + u64::from(QUEUE) * 16;
        let used = (avail + 6 + u64::from(QUEUE) * 2 + 4095) & !4095;
        checked(
            ram_size,
            table,
            (used - table) as usize + 4 + usize::from(QUEUE) * 8,
        )?;
        let notify = u16::from_le_bytes(read_at(mem, avail)?) & 1 == 0;
        let end = u16::from_le_bytes(read_at(mem, avail + 2)?);
        if end.wrapping_sub(self.avail) > QUEUE {
            return Err(invalid("virtio queue overrun"));
        }
        while self.avail != end {
            let head =
                u16::from_le_bytes(read_at(mem, avail + 4 + u64::from(self.avail % QUEUE) * 2)?);
            let mut chain = [Descriptor::default(); QUEUE as usize];
            let mut index = head;
            let mut n = 0usize;
            loop {
                if index >= QUEUE {
                    return Err(invalid("virtio descriptor index outside queue"));
                }
                let raw: [u8; 16] = read_at(mem, table + u64::from(index) * 16)?;
                let desc = Descriptor {
                    addr: u64::from_le_bytes(part(&raw, 0)?),
                    len: u32::from_le_bytes(part(&raw, 8)?),
                    flags: u16::from_le_bytes(part(&raw, 12)?),
                };
                checked(ram_size, desc.addr, desc.len as usize)?;
                if desc.flags & !3 != 0 {
                    return Err(invalid("unnegotiated descriptor flag"));
                }
                *chain
                    .get_mut(n)
                    .ok_or_else(|| invalid("virtio descriptor chain too long"))? = desc;
                n += 1;
                if desc.flags & 1 == 0 {
                    break;
                }
                index = u16::from_le_bytes(part(&raw, 14)?);
            }
            let chain = chain
                .get(..n)
                .ok_or_else(|| invalid("invalid chain size"))?;
            let [header, buffers @ .., status] = chain else {
                return Err(invalid("incomplete block request"));
            };
            if header.flags & 2 != 0 || header.len != 16 || status.flags & 2 == 0 || status.len != 1
            {
                return Err(invalid("invalid block request framing"));
            }
            let request: [u8; 16] = read_at(mem, header.addr)?;
            let kind = u32::from_le_bytes(part(&request, 0)?);
            let sector = u64::from_le_bytes(part(&request, 8)?);
            let (status_byte, len) = self.transfer(mem, kind, sector, buffers)?;
            mem.write_all_at(&[status_byte], status.addr)?;
            let item = used + 4 + u64::from(self.used % QUEUE) * 8;

            mem.write_all_at(&u32::from(head).to_le_bytes(), item)?;
            mem.write_all_at(&len.to_le_bytes(), item + 4)?;
            self.avail = self.avail.wrapping_add(1);
            self.used = self.used.wrapping_add(1);
            mem.write_all_at(&self.used.to_le_bytes(), used + 2)?;
            self.irq |= notify;
        }
        Ok(())
    }

    fn transfer(
        &self,
        mem: &File,
        kind: u32,
        sector: u64,
        buffers: &[Descriptor],
    ) -> io::Result<(u8, u32)> {
        if kind == 4 {
            return Ok(
                if buffers.is_empty() && (!self.writable || self.disk.sync_data().is_ok()) {
                    (0, 1)
                } else {
                    (1, 1)
                },
            );
        }
        if kind == 8 {
            const ID: &[u8; 20] = b"td-vmm-disk000000000";
            let capacity: u64 = buffers.iter().map(|d| u64::from(d.len)).sum();
            if capacity < ID.len() as u64 || buffers.iter().any(|d| d.flags & 2 == 0) {
                return Ok((1, 1));
            }
            let mut remaining = ID.as_slice();
            for desc in buffers {
                if remaining.is_empty() {
                    break;
                }
                let (bytes, rest) = remaining.split_at(remaining.len().min(desc.len as usize));
                mem.write_all_at(bytes, desc.addr)?;
                remaining = rest;
            }
            return Ok((0, 21));
        }
        if !matches!(kind, 0 | 1) {
            return Ok((2, 1));
        }
        let read = kind == 0;
        let size: u64 = buffers.iter().map(|b| u64::from(b.len)).sum();
        let Some(mut offset) = sector.checked_mul(512) else {
            return Ok((1, 1));
        };
        if (!read && !self.writable)
            || size >= u64::from(u32::MAX)
            || !size.is_multiple_of(512)
            || offset.checked_add(size).is_none_or(|end| end > self.size)
            || buffers.iter().any(|b| (b.flags & 2 != 0) != read)
        {
            return Ok((1, 1));
        }
        let mut chunk = [0; 8192];
        let mut produced = 0u32;
        for buffer in buffers {
            let mut done = 0usize;
            while done < (buffer.len as usize) {
                let n = chunk.len().min(buffer.len as usize - done);
                let chunk = chunk
                    .get_mut(..n)
                    .ok_or_else(|| invalid("invalid transfer range"))?;
                let addr = buffer.addr + done as u64;
                if read {
                    if self.disk.read_exact_at(chunk, offset).is_err() {
                        return Ok((1, produced + 1));
                    }
                    mem.write_all_at(chunk, addr)?;
                    produced += n as u32;
                } else {
                    mem.read_exact_at(chunk, addr)?;
                    if self.disk.write_all_at(chunk, offset).is_err() {
                        return Ok((1, 1));
                    }
                }
                offset += n as u64;
                done += n;
            }
        }
        if !read && self.negotiated & FLUSH == 0 && self.disk.sync_data().is_err() {
            return Ok((1, 1));
        }
        Ok((0, produced + 1))
    }
}

#[derive(Clone, Copy, Default)]
struct Descriptor {
    addr: u64,
    len: u32,
    flags: u16,
}

fn checked(ram_size: u64, addr: u64, n: usize) -> io::Result<()> {
    if addr.checked_add(n as u64).is_none_or(|end| end > ram_size) {
        Err(invalid("virtio address outside guest RAM"))
    } else {
        Ok(())
    }
}
fn read_at<const N: usize>(mem: &File, addr: u64) -> io::Result<[u8; N]> {
    let mut b = [0; N];
    mem.read_exact_at(&mut b, addr)?;
    Ok(b)
}
fn store(b: &mut [u8], start: usize, data: &[u8]) -> io::Result<()> {
    b.get_mut(start..)
        .and_then(|b| b.get_mut(..data.len()))
        .ok_or_else(|| invalid("PCI register out of range"))?
        .copy_from_slice(data);
    Ok(())
}
fn part<const N: usize>(b: &[u8], start: usize) -> io::Result<[u8; N]> {
    b.get(start..)
        .and_then(|r| r.get(..N))
        .and_then(|r| r.try_into().ok())
        .ok_or_else(|| invalid("short device register"))
}
fn value(b: &[u8]) -> io::Result<u32> {
    let mut v = [0; 4];
    store(&mut v, 0, b)?;
    Ok(u32::from_le_bytes(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn descriptor(addr: u64, len: u32, flags: u16) -> Descriptor {
        Descriptor { addr, len, flags }
    }
    #[test]
    fn transfers_respect_disk_access_and_report_completed_bytes() -> io::Result<()> {
        let dir = td_fs::TempDir::new("td-vmm-block")?;
        let disk = dir.path().join("image");
        let memory = dir.path().join("ram");
        fs::write(&disk, [0; 1024])?;
        let mem = File::create_new(memory)?;
        mem.set_len(8192)?;
        mem.write_all_at(&[0x5a; 256], 512)?;
        mem.write_all_at(&[0xa5; 256], 2048)?;
        let sources = [descriptor(512, 256, 0), descriptor(2048, 256, 0)];
        let device = Block::open(&disk, true)?;
        assert_eq!(device.transfer(&mem, 1, 1, &sources)?, (0, 1));
        assert!(Block::open(&disk, false).is_err());
        assert_eq!(device.transfer(&mem, 1, 2, &sources)?, (1, 1));
        assert_eq!(device.transfer(&mem, 4, 0, &[])?, (0, 1));
        drop(device);
        let device = Block::open(&disk, false)?;
        let id_segments = [descriptor(0, 7, 2), descriptor(3000, 13, 2)];
        assert_eq!(device.transfer(&mem, 8, 0, &id_segments)?, (0, 21));
        assert_eq!(read_at::<7>(&mem, 0)?, *b"td-vmm-");
        assert_eq!(read_at::<13>(&mem, 3000)?, *b"disk000000000");
        assert_eq!(device.transfer(&mem, 1, 0, &sources)?, (1, 1));
        assert_eq!(
            device.transfer(&mem, 0, 1, &[descriptor(4096, 512, 2)])?,
            (0, 513)
        );
        assert_eq!(read_at::<256>(&mem, 4096)?, [0x5a; 256]);
        assert_eq!(read_at::<256>(&mem, 4352)?, [0xa5; 256]);
        assert_eq!(device.transfer(&mem, 0, u64::MAX, &sources)?, (1, 1));
        Ok(())
    }

    #[test]
    fn large_queue_request_can_target_ram_above_the_default_machine_size() -> io::Result<()> {
        let dir = td_fs::TempDir::new("td-vmm-large-io")?;
        let disk = dir.path().join("disk");
        let size = (8 << 20) + 512u32;
        let image = File::create_new(&disk)?;
        image.set_len(u64::from(size))?;
        image.write_all_at(b"last sector", u64::from(size) - 512)?;
        let mem = File::create_new(dir.path().join("ram"))?;
        let data = 300u64 << 20;
        mem.set_len(data + u64::from(size))?;
        let mut device = Block::open(&disk, false)?;
        device.pfn = 1;
        let table = 4096u64;
        let status = 0x4010;
        for (index, (addr, len, flags, next)) in [
            (0x4000u64, 16u32, 1u16, 1u16),
            (data, size, 3, 2),
            (status, 1, 2, 0),
        ]
        .into_iter()
        .enumerate()
        {
            let entry = table + index as u64 * 16;
            mem.write_all_at(&addr.to_le_bytes(), entry)?;
            mem.write_all_at(&len.to_le_bytes(), entry + 8)?;
            mem.write_all_at(&flags.to_le_bytes(), entry + 12)?;
            mem.write_all_at(&next.to_le_bytes(), entry + 14)?;
        }
        mem.write_all_at(&[255], status)?;
        mem.write_all_at(&1u16.to_le_bytes(), table + u64::from(QUEUE) * 16 + 2)?;
        device.drain(&mem)?;
        assert_eq!(read_at::<1>(&mem, status)?, [0]);
        assert_eq!(
            read_at::<11>(&mem, data + u64::from(size) - 512)?,
            *b"last sector"
        );
        assert_eq!(read_at::<4>(&mem, 8192 + 8)?, (size + 1).to_le_bytes());
        assert!(device.irq);
        Ok(())
    }

    #[test]
    fn malformed_guest_rings_fail_before_disk_io() -> io::Result<()> {
        let dir = td_fs::TempDir::new("td-vmm-ring")?;
        let disk = dir.path().join("image");
        fs::write(&disk, [0xaa; 512])?;
        let mem = File::create_new(dir.path().join("ram"))?;
        mem.set_len(65536)?;
        let mut device = Block::open(&disk, true)?;
        device.pfn = 1;
        let table = 4096u64;
        let avail = table + u64::from(QUEUE) * 16;
        mem.write_all_at(&1u16.to_le_bytes(), avail + 2)?;
        // Descriptor zero links back to itself.
        mem.write_all_at(&1u16.to_le_bytes(), table + 12)?;
        assert!(device.drain(&mem).is_err());
        mem.write_all_at(&0u16.to_le_bytes(), table + 12)?;
        mem.write_all_at(&u64::MAX.to_le_bytes(), table)?;
        assert!(device.drain(&mem).is_err());
        mem.write_all_at(&(QUEUE + 1).to_le_bytes(), avail + 2)?;
        assert!(device.drain(&mem).is_err());
        assert_eq!(fs::read(&disk)?, [0xaa; 512]);
        Ok(())
    }
}
