//! Linux x86 boot protocol, 32-bit protected-mode entry.
use crate::invalid;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

pub(super) const PARAMS: usize = 0x7000;
pub(super) const GDT: usize = 0x500;
const CMDLINE: usize = 0x20000;
const HIGH_RAM: usize = 0x100000;

fn region(bytes: &mut [u8], start: usize, size: usize) -> io::Result<&mut [u8]> {
    bytes
        .get_mut(start..)
        .and_then(|rest| rest.get_mut(..size))
        .ok_or_else(|| invalid("boot field outside guest memory"))
}

fn put(bytes: &mut [u8], start: usize, value: &[u8]) -> io::Result<()> {
    region(bytes, start, value.len())?.copy_from_slice(value);
    Ok(())
}

fn field<const N: usize>(head: &[u8], offset: usize) -> io::Result<[u8; N]> {
    head.get(offset..)
        .and_then(|b| b.get(..N))
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("truncated kernel header"))
}

fn size(file: &File) -> io::Result<usize> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(invalid("expected a nonempty regular guest image"));
    }
    usize::try_from(metadata.len()).map_err(|_| invalid("image too large"))
}

pub(super) fn load(
    ram: &mut [u8],
    mut kernel: File,
    mut initrd: File,
    cmdline: &[u8],
) -> io::Result<usize> {
    let image_size = size(&kernel)?;
    let initrd_size = size(&initrd)?;
    let mut header = [0; 4096];
    kernel.read_exact(&mut header)?;
    if field::<2>(&header, 0x1fe)? != [0x55, 0xaa]
        || field::<4>(&header, 0x202)? != *b"HdrS"
        || u16::from_le_bytes(field(&header, 0x206)?) < 0x20c
        || field::<1>(&header, 0x211)?[0] & 1 == 0
        || u16::from_le_bytes(field(&header, 0x236)?) & 1 == 0
    {
        return Err(invalid(
            "expected a 64-bit Linux bzImage with boot protocol 2.12 or newer",
        ));
    }
    let setup = field::<1>(&header, 0x1f1)?[0];
    let payload_offset = (1 + usize::from(if setup == 0 { 4 } else { setup })) * 512;
    let image_bytes = image_size
        .checked_sub(payload_offset)
        .filter(|n| *n != 0)
        .ok_or_else(|| invalid("empty kernel payload"))?;
    let load = if field::<1>(&header, 0x234)? == [0] {
        u32::from_le_bytes(field(&header, 0x214)?) as usize
    } else {
        let align = u32::from_le_bytes(field(&header, 0x230)?) as usize;
        let preferred = usize::try_from(u64::from_le_bytes(field(&header, 0x258)?))
            .map_err(|_| invalid("invalid preferred kernel address"))?;
        if !align.is_power_of_two() || preferred % align != 0 {
            return Err(invalid("invalid preferred kernel alignment"));
        }
        preferred
    };
    if load < HIGH_RAM {
        return Err(invalid("kernel load address overlaps low memory"));
    }
    let footprint = u32::from_le_bytes(field(&header, 0x260)?) as usize;
    let initrd_max = u32::from_le_bytes(field(&header, 0x22c)?) as usize;
    let ram_len = ram.len();
    let initrd_addr = ram_len
        .min(initrd_max + 1)
        .checked_sub(initrd_size)
        .ok_or_else(|| invalid("initramfs exceeds memory"))?
        & !4095;
    if load
        .checked_add(image_bytes.max(footprint))
        .is_none_or(|end| end > initrd_addr)
    {
        return Err(invalid("kernel and initramfs overlap"));
    }
    if cmdline.len() > 4095
        || cmdline.len() > u32::from_le_bytes(field(&header, 0x238)?) as usize
        || cmdline.contains(&0)
    {
        return Err(invalid("invalid kernel command line"));
    }
    let header_end = 0x202 + usize::from(field::<1>(&header, 0x201)?[0]);
    let setup_header = header
        .get(0x1f1..header_end)
        .filter(|h| h.len() >= 0x264 - 0x1f1)
        .ok_or_else(|| invalid("short setup header"))?;
    put(ram, PARAMS + 0x1f1, setup_header)?;
    put(ram, PARAMS + 0x210, &[0xff])?;
    put(ram, PARAMS + 0x214, &(load as u32).to_le_bytes())?;
    put(ram, PARAMS + 0x218, &(initrd_addr as u32).to_le_bytes())?;
    put(ram, PARAMS + 0x21c, &(initrd_size as u32).to_le_bytes())?;
    put(ram, PARAMS + 0x228, &(CMDLINE as u32).to_le_bytes())?;
    put(ram, CMDLINE, cmdline)?;
    put(ram, PARAMS + 0x1e8, &[3])?;
    for (i, (base, len, kind)) in [
        (0u64, 0x9fc00u64, 1u32),
        (0x9fc00, 0x60400, 2),
        (
            HIGH_RAM as u64,
            ram_len
                .checked_sub(HIGH_RAM)
                .ok_or_else(|| invalid("missing high RAM"))? as u64,
            1,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let p = PARAMS + 0x2d0 + i * 20;
        put(ram, p, &base.to_le_bytes())?;
        put(ram, p + 8, &len.to_le_bytes())?;
        put(ram, p + 16, &kind.to_le_bytes())?;
    }
    put(ram, GDT + 16, &0x00cf9b000000ffffu64.to_le_bytes())?;
    put(ram, GDT + 24, &0x00cf93000000ffffu64.to_le_bytes())?;
    kernel.seek(SeekFrom::Start(payload_offset as u64))?;
    kernel.read_exact(region(ram, load, image_bytes)?)?;
    initrd.read_exact(region(ram, initrd_addr, initrd_size)?)?;
    Ok(load)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn kernel_and_archive_must_fit_at_distinct_guest_addresses() -> io::Result<()> {
        let dir = td_fs::TempDir::new("td-vmm-loader")?;
        let kernel = dir.path().join("kernel");
        let initrd = dir.path().join("archive");
        fs::write(&initrd, b"archive")?;
        let mut header = [0; 8192];
        put(&mut header, 0x1f1, &[7])?;
        put(&mut header, 0x1fe, &[0x55, 0xaa])?;
        put(&mut header, 0x201, &[0x6a])?;
        put(&mut header, 0x202, b"HdrS")?;
        put(&mut header, 0x206, &0x20cu16.to_le_bytes())?;
        put(&mut header, 0x211, &[1])?;
        put(&mut header, 0x214, &0x100000u32.to_le_bytes())?;
        put(&mut header, 0x22c, &u32::MAX.to_le_bytes())?;
        put(&mut header, 0x236, &1u16.to_le_bytes())?;
        put(&mut header, 0x238, &32u32.to_le_bytes())?;
        put(&mut header, 0x260, &4096u32.to_le_bytes())?;
        put(&mut header, 4096, b"payload")?;
        fs::write(&kernel, header)?;
        let mut memory = vec![0; 2 << 20];
        let run = |memory: &mut [u8]| {
            load(
                memory,
                File::open(&kernel)?,
                File::open(&initrd)?,
                b"console=ttyS0",
            )
        };
        assert_eq!(run(&mut memory)?, HIGH_RAM);
        let high_length = PARAMS + 0x2d0 + 2 * 20 + 8;
        assert_eq!(
            u64::from_le_bytes(field(&memory, high_length)?),
            (memory.len() - HIGH_RAM) as u64
        );
        assert_eq!(
            memory.get(HIGH_RAM..HIGH_RAM + 7),
            Some(b"payload".as_slice())
        );
        assert_eq!(memory.get(0x1ff000..0x1ff007), Some(b"archive".as_slice()));
        assert_eq!(
            memory.get(CMDLINE..CMDLINE + 14),
            Some(b"console=ttyS0\0".as_slice())
        );
        put(&mut header, 0x260, &0x100000u32.to_le_bytes())?;
        fs::write(&kernel, header)?;
        assert!(run(&mut memory).is_err());
        Ok(())
    }
}
