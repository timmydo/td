//! The one scoped Linux/x86-64 surface for perf events, mapped rings, and the
//! permanent credential drop. `main.rs` carries the sole unsafe-code allowance.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-profiler supports the Linux x86-64 perf ABI only");

const SYS_CLOSE: usize = 3;
const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_IOCTL: usize = 16;
const SYS_SETGID: usize = 106;
const SYS_SETUID: usize = 105;
const SYS_SETGROUPS: usize = 116;
const SYS_CLOCK_GETTIME: usize = 228;
const SYS_PERF_EVENT_OPEN: usize = 298;

const PROT_READ_WRITE: usize = 0x1 | 0x2;
const MAP_SHARED: usize = 0x1;
const MAP_FAILED: isize = -1;
const PERF_FLAG_FD_CLOEXEC: usize = 8;
const PERF_EVENT_IOC_ENABLE: usize = 0x2400;
const PERF_EVENT_IOC_DISABLE: usize = 0x2401;
const PERF_EVENT_IOC_SET_OUTPUT: usize = 0x2405;
const PERF_EVENT_IOC_ID: usize = 0x8008_2407;
const CLOCK_MONOTONIC: usize = 1;
const PAGE_BYTES: usize = 4096;
const DATA_HEAD_OFFSET: usize = 1024;
const DATA_TAIL_OFFSET: usize = 1032;
const DATA_OFFSET_OFFSET: usize = 1040;
const DATA_SIZE_OFFSET: usize = 1048;
const ATTR_BYTES: usize = 128;

const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
const PERF_COUNT_SW_DUMMY: u64 = 9;
const SAMPLE_IP: u64 = 1 << 0;
const SAMPLE_TID: u64 = 1 << 1;
const SAMPLE_TIME: u64 = 1 << 2;
const SAMPLE_CALLCHAIN: u64 = 1 << 5;
const SAMPLE_CPU: u64 = 1 << 7;
const SAMPLE_IDENTIFIER: u64 = 1 << 16;
const ATTR_DISABLED: u64 = 1 << 0;
const ATTR_EXCLUDE_KERNEL: u64 = 1 << 5;
const ATTR_EXCLUDE_HV: u64 = 1 << 6;
const ATTR_MMAP: u64 = 1 << 8;
const ATTR_COMM: u64 = 1 << 9;
const ATTR_FREQ: u64 = 1 << 10;
const ATTR_TASK: u64 = 1 << 13;
const ATTR_SAMPLE_ID_ALL: u64 = 1 << 18;
const ATTR_MMAP2: u64 = 1 << 23;
const ATTR_COMM_EXEC: u64 = 1 << 24;
const ATTR_USE_CLOCKID: u64 = 1 << 25;

pub struct CpuEvents {
    cpu: u32,
    metadata_fd: i32,
    sample_fd: i32,
    pub metadata_id: u64,
    pub sample_id: u64,
    ring: Ring,
    sequence: u64,
}

impl CpuEvents {
    pub fn open(cpu: u32, rate_hz: u32, ring_pages: usize) -> io::Result<Self> {
        if rate_hz == 0 || ring_pages == 0 || !ring_pages.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sample rate and power-of-two ring pages must be nonzero",
            ));
        }
        let metadata_attr = metadata_attr()?;
        let metadata_fd = perf_event_open(&metadata_attr, cpu)?;
        let sample_attr = sample_attr(rate_hz)?;
        let sample_fd = match perf_event_open(&sample_attr, cpu) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = close(metadata_fd);
                return Err(error);
            }
        };
        let ring = match Ring::map(metadata_fd, ring_pages) {
            Ok(ring) => ring,
            Err(error) => {
                let _ = close(sample_fd);
                let _ = close(metadata_fd);
                return Err(error);
            }
        };
        if let Err(error) = ioctl_value(sample_fd, PERF_EVENT_IOC_SET_OUTPUT, metadata_fd as usize)
        {
            drop(ring);
            let _ = close(sample_fd);
            let _ = close(metadata_fd);
            return Err(error);
        }
        let metadata_id = match ioctl_id(metadata_fd) {
            Ok(id) => id,
            Err(error) => {
                drop(ring);
                let _ = close(sample_fd);
                let _ = close(metadata_fd);
                return Err(error);
            }
        };
        let sample_id = match ioctl_id(sample_fd) {
            Ok(id) => id,
            Err(error) => {
                drop(ring);
                let _ = close(sample_fd);
                let _ = close(metadata_fd);
                return Err(error);
            }
        };
        Ok(Self {
            cpu,
            metadata_fd,
            sample_fd,
            metadata_id,
            sample_id,
            ring,
            sequence: 0,
        })
    }

    pub fn cpu(&self) -> u32 {
        self.cpu
    }

    pub fn enable_metadata(&self) -> io::Result<()> {
        ioctl_value(self.metadata_fd, PERF_EVENT_IOC_ENABLE, 0)
    }

    pub fn enable_samples(&self) -> io::Result<()> {
        ioctl_value(self.sample_fd, PERF_EVENT_IOC_ENABLE, 0)
    }

    pub fn disable_samples(&self) -> io::Result<()> {
        ioctl_value(self.sample_fd, PERF_EVENT_IOC_DISABLE, 0)
    }

    pub fn drain(&mut self) -> io::Result<Vec<(u64, Vec<u8>)>> {
        self.ring.drain(&mut self.sequence)
    }
}

impl Drop for CpuEvents {
    fn drop(&mut self) {
        let _ = ioctl_value(self.sample_fd, PERF_EVENT_IOC_DISABLE, 0);
        let _ = ioctl_value(self.metadata_fd, PERF_EVENT_IOC_DISABLE, 0);
        let _ = close(self.sample_fd);
        let _ = close(self.metadata_fd);
    }
}

struct Ring {
    base: *mut u8,
    mapped_bytes: usize,
    data_offset: usize,
    data_size: usize,
}

impl Ring {
    fn map(fd: i32, pages: usize) -> io::Result<Self> {
        let mapped_bytes = pages
            .checked_add(1)
            .and_then(|count| count.checked_mul(PAGE_BYTES))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ring size overflow"))?;
        let pointer = syscall6(
            SYS_MMAP,
            0,
            mapped_bytes,
            PROT_READ_WRITE,
            MAP_SHARED,
            fd as usize,
            0,
        );
        if pointer == MAP_FAILED || pointer < 0 {
            return Err(last(pointer));
        }
        let base = pointer as *mut u8;
        let data_offset = unsafe { read_u64(base, DATA_OFFSET_OFFSET) }?;
        let data_size = unsafe { read_u64(base, DATA_SIZE_OFFSET) }?;
        let data_offset = usize::try_from(data_offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ring offset overflow"))?;
        let data_size = usize::try_from(data_size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ring size overflow"))?;
        if data_offset < PAGE_BYTES
            || data_size != pages.saturating_mul(PAGE_BYTES)
            || data_offset.saturating_add(data_size) > mapped_bytes
        {
            let _ = syscall6(SYS_MUNMAP, base as usize, mapped_bytes, 0, 0, 0, 0);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "kernel returned an invalid perf ring layout",
            ));
        }
        Ok(Self {
            base,
            mapped_bytes,
            data_offset,
            data_size,
        })
    }

    fn drain(&mut self, sequence: &mut u64) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let head = unsafe { load_atomic_u64(self.base, DATA_HEAD_OFFSET, Ordering::Acquire) };
        let mut tail = unsafe { load_atomic_u64(self.base, DATA_TAIL_OFFSET, Ordering::Relaxed) };
        if head.saturating_sub(tail) > self.data_size as u64 {
            unsafe { store_atomic_u64(self.base, DATA_TAIL_OFFSET, head, Ordering::Release) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "perf ring overrun",
            ));
        }
        let mut records = Vec::new();
        while tail < head {
            let mut header = [0u8; 8];
            unsafe { self.copy_into(tail, &mut header) }?;
            let size = usize::from(u16::from_le_bytes([
                *header.get(6).ok_or_else(truncated_ring)?,
                *header.get(7).ok_or_else(truncated_ring)?,
            ]));
            if !(8..=crate::perf::MAX_KERNEL_RECORD).contains(&size)
                || size % 8 != 0
                || tail.saturating_add(size as u64) > head
            {
                unsafe { store_atomic_u64(self.base, DATA_TAIL_OFFSET, head, Ordering::Release) };
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed perf record in ring",
                ));
            }
            let record = unsafe { self.copy(tail, size) }?;
            records.push((*sequence, record));
            *sequence = sequence.saturating_add(1);
            tail = tail.saturating_add(size as u64);
        }
        unsafe { store_atomic_u64(self.base, DATA_TAIL_OFFSET, tail, Ordering::Release) };
        Ok(records)
    }

    unsafe fn copy(&self, absolute: u64, length: usize) -> io::Result<Vec<u8>> {
        if length > self.data_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "perf record is larger than its ring",
            ));
        }
        let mut out = vec![0; length];
        self.copy_into(absolute, &mut out)?;
        Ok(out)
    }

    unsafe fn copy_into(&self, absolute: u64, out: &mut [u8]) -> io::Result<()> {
        if out.len() > self.data_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "perf record is larger than its ring",
            ));
        }
        let mask = u64::try_from(self.data_size.saturating_sub(1))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ring mask overflow"))?;
        let ring_at = usize::try_from(absolute & mask)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ring offset overflow"))?;
        let first = out.len().min(self.data_size.saturating_sub(ring_at));
        std::ptr::copy_nonoverlapping(
            self.base.add(self.data_offset.saturating_add(ring_at)),
            out.as_mut_ptr(),
            first,
        );
        if first < out.len() {
            std::ptr::copy_nonoverlapping(
                self.base.add(self.data_offset),
                out.as_mut_ptr().add(first),
                out.len().saturating_sub(first),
            );
        }
        Ok(())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let _ = syscall6(
            SYS_MUNMAP,
            self.base as usize,
            self.mapped_bytes,
            0,
            0,
            0,
            0,
        );
    }
}

pub fn monotonic_ns() -> io::Result<u64> {
    let mut time = Timespec { sec: 0, nsec: 0 };
    check(syscall6(
        SYS_CLOCK_GETTIME,
        CLOCK_MONOTONIC,
        std::ptr::from_mut(&mut time) as usize,
        0,
        0,
        0,
        0,
    ))?;
    if time.sec < 0 || !(0..1_000_000_000).contains(&time.nsec) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "clock_gettime returned an invalid value",
        ));
    }
    (time.sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(time.nsec as u64))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "monotonic clock overflow"))
}

pub fn drop_credentials(uid: u32, gid: u32) -> io::Result<()> {
    check(syscall6(SYS_SETGROUPS, 0, 0, 0, 0, 0, 0))?;
    check(syscall6(SYS_SETGID, gid as usize, 0, 0, 0, 0, 0))?;
    check(syscall6(SYS_SETUID, uid as usize, 0, 0, 0, 0, 0))
}

fn metadata_attr() -> io::Result<[u8; ATTR_BYTES]> {
    let mut attr = [0; ATTR_BYTES];
    put_u32(&mut attr, 0, PERF_TYPE_SOFTWARE)?;
    put_u32(&mut attr, 4, ATTR_BYTES as u32)?;
    put_u64(&mut attr, 8, PERF_COUNT_SW_DUMMY)?;
    put_u64(
        &mut attr,
        24,
        SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_IDENTIFIER,
    )?;
    put_u64(
        &mut attr,
        40,
        ATTR_DISABLED
            | ATTR_MMAP
            | ATTR_COMM
            | ATTR_TASK
            | ATTR_SAMPLE_ID_ALL
            | ATTR_MMAP2
            | ATTR_COMM_EXEC
            | ATTR_USE_CLOCKID,
    )?;
    put_u32(&mut attr, 48, 1)?;
    put_u32(&mut attr, 92, CLOCK_MONOTONIC as u32)?;
    Ok(attr)
}

fn sample_attr(rate_hz: u32) -> io::Result<[u8; ATTR_BYTES]> {
    let mut attr = [0; ATTR_BYTES];
    put_u32(&mut attr, 0, PERF_TYPE_SOFTWARE)?;
    put_u32(&mut attr, 4, ATTR_BYTES as u32)?;
    put_u64(&mut attr, 8, PERF_COUNT_SW_CPU_CLOCK)?;
    put_u64(&mut attr, 16, u64::from(rate_hz))?;
    put_u64(
        &mut attr,
        24,
        SAMPLE_IDENTIFIER | SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CALLCHAIN | SAMPLE_CPU,
    )?;
    put_u64(
        &mut attr,
        40,
        ATTR_DISABLED
            | ATTR_EXCLUDE_KERNEL
            | ATTR_EXCLUDE_HV
            | ATTR_FREQ
            | ATTR_SAMPLE_ID_ALL
            | ATTR_USE_CLOCKID,
    )?;
    put_u32(&mut attr, 48, 1)?;
    put_u32(&mut attr, 92, CLOCK_MONOTONIC as u32)?;
    Ok(attr)
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(4))
        .ok_or_else(truncated_attr)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(8))
        .ok_or_else(truncated_attr)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn truncated_attr() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "perf attr offset is out of bounds",
    )
}

fn truncated_ring() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "truncated perf ring header")
}

fn perf_event_open(attr: &[u8; ATTR_BYTES], cpu: u32) -> io::Result<i32> {
    let value = value(syscall6(
        SYS_PERF_EVENT_OPEN,
        attr.as_ptr() as usize,
        usize::MAX,
        cpu as usize,
        usize::MAX,
        PERF_FLAG_FD_CLOEXEC,
        0,
    ))?;
    i32::try_from(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "perf fd overflow"))
}

fn ioctl_value(fd: i32, request: usize, argument: usize) -> io::Result<()> {
    check(syscall6(SYS_IOCTL, fd as usize, request, argument, 0, 0, 0))
}

fn ioctl_id(fd: i32) -> io::Result<u64> {
    let mut id = 0u64;
    ioctl_value(fd, PERF_EVENT_IOC_ID, std::ptr::from_mut(&mut id) as usize)?;
    Ok(id)
}

fn close(fd: i32) -> io::Result<()> {
    check(syscall6(SYS_CLOSE, fd as usize, 0, 0, 0, 0, 0))
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

unsafe fn read_u64(base: *mut u8, at: usize) -> io::Result<u64> {
    let value = std::ptr::read_unaligned(base.add(at) as *const u64);
    Ok(value)
}

unsafe fn load_atomic_u64(base: *mut u8, at: usize, ordering: Ordering) -> u64 {
    (*base.add(at).cast::<AtomicU64>()).load(ordering)
}

unsafe fn store_atomic_u64(base: *mut u8, at: usize, value: u64, ordering: Ordering) {
    (*base.add(at).cast::<AtomicU64>()).store(value, ordering);
}

fn value(returned: isize) -> io::Result<usize> {
    if returned < 0 {
        Err(last(returned))
    } else {
        Ok(returned as usize)
    }
}

fn check(returned: isize) -> io::Result<()> {
    value(returned).map(|_| ())
}

fn last(returned: isize) -> io::Error {
    io::Error::from_raw_os_error((-returned) as i32)
}

/// The only syscall instruction. The encompassing module allowance also covers
/// the mapped-ring pointer reads whose lifetime `Ring` owns and bounds.
fn syscall6(
    number: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let returned: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => returned,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    returned
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        metadata_attr, sample_attr, AtomicU64, ATTR_BYTES, ATTR_SAMPLE_ID_ALL, DATA_HEAD_OFFSET,
        DATA_TAIL_OFFSET,
    };

    #[test]
    fn attributes_pin_size_clock_and_sample_contract() {
        let metadata = metadata_attr().unwrap();
        let sample = sample_attr(99).unwrap();
        assert_eq!(metadata.len(), ATTR_BYTES);
        assert_eq!(sample.len(), ATTR_BYTES);
        assert_eq!(
            u32::from_le_bytes(metadata.get(4..8).unwrap().try_into().unwrap()),
            128
        );
        assert_eq!(
            u64::from_le_bytes(sample.get(16..24).unwrap().try_into().unwrap()),
            99
        );
        for attr in [&metadata, &sample] {
            let flags = u64::from_le_bytes(attr.get(40..48).unwrap().try_into().unwrap());
            assert_ne!(flags & ATTR_SAMPLE_ID_ALL, 0);
        }
        let metadata_flags = u64::from_le_bytes(metadata.get(40..48).unwrap().try_into().unwrap());
        assert_eq!(metadata_flags & (1 << 26), 0);
        assert_eq!(DATA_HEAD_OFFSET % std::mem::align_of::<AtomicU64>(), 0);
        assert_eq!(DATA_TAIL_OFFSET % std::mem::align_of::<AtomicU64>(), 0);
        assert_eq!(
            u32::from_le_bytes(sample.get(92..96).unwrap().try_into().unwrap()),
            1
        );
    }

    #[test]
    fn the_output_target_has_a_ring_before_redirect() {
        let source = include_str!("sys.rs");
        let open = source
            .split("pub fn open")
            .nth(1)
            .and_then(|body| body.split("pub fn cpu").next())
            .unwrap();
        assert!(
            open.find("Ring::map").unwrap() < open.find("PERF_EVENT_IOC_SET_OUTPUT").unwrap(),
            "SET_OUTPUT requires the target event's ring_buffer to exist"
        );
    }
}
