//! `zram-swap` — compressed swap in RAM, the only swap td enables.
//!
//! The desktop's td-svc table runs this once at boot. It sizes `/dev/zram0`
//! at half of `MemTotal`, checks it compresses with zstd, writes the swap
//! signature and turns it on at priority 100. A zram device's size caps the
//! data it may hold before compression; it takes memory for what it holds,
//! compressed, and up front only for its slot table, which the kernel sizes
//! at a few bytes per page of `disksize` (about 0.2% of RAM).
//!
//! What it never does is give the device a writeback device: td's kernel is
//! built without `CONFIG_ZRAM_WRITEBACK`, and this refuses a device whose
//! `backing_dev` reads anything but `none`. Every page swapped stays in
//! memory, which is what td-secret's memory rule (td-secret/DESIGN.md)
//! admits beside a process holding a key or a PIN.
//!
//! Each step is read back, because none of the writes says what the kernel
//! took: `comp_algorithm` names the selection in brackets, `disksize` the
//! size, and `/proc/swaps` lists the device and its priority once
//! `swapon(2)` succeeded. The node the header is written to must be the
//! block device sysfs describes, by device number. A td-svc respawned after
//! the swap is on finds it listed at priority 100 and succeeds without
//! writing anything.

use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use crate::{devt, sys};

const DEVICE: &str = "/dev/zram0";
const DEVICE_C: &CStr = c"/dev/zram0";
const BACKING_DEV: &str = "/sys/block/zram0/backing_dev";
const COMP_ALGORITHM: &str = "/sys/block/zram0/comp_algorithm";
const DEV: &str = "/sys/block/zram0/dev";
const DISKSIZE: &str = "/sys/block/zram0/disksize";
const INITSTATE: &str = "/sys/block/zram0/initstate";
const ALGORITHM: &str = "zstd";
/// The priority `sys::swapon`'s fixed flags carry; a confinement test in
/// `main.rs` holds the two equal.
const PRIORITY: i32 = 100;
/// x86_64's page, the unit of the swap header and of `last_page`.
const PAGE: u64 = 4096;
const PAGE_BYTES: usize = 4096;
/// A device smaller than this holds too little to be worth a swap area,
/// and is what a misread `/proc/meminfo` would most likely yield.
const MIN_PAGES: u64 = 256;

pub fn run(args: &[String]) -> Result<u8, String> {
    if !args.is_empty() {
        return Err(usage());
    }
    match swapping(&read("/proc/swaps")?) {
        Some(PRIORITY) => {
            crate::emit("zram-swap: /dev/zram0 is already swap\n")?;
            return Ok(0);
        }
        Some(other) => return Err(wrong_priority(other)),
        None => {}
    }
    let size = disksize(&read("/proc/meminfo")?)?;
    refuse_writeback()?;
    if read(INITSTATE)?.trim() != "1" {
        if !selected(&read(COMP_ALGORITHM)?) {
            write(COMP_ALGORITHM, ALGORITHM)?;
        }
        write(DISKSIZE, &size.to_string())?;
    }
    if !selected(&read(COMP_ALGORITHM)?) {
        return Err(format!("{COMP_ALGORITHM} did not take {ALGORITHM}"));
    }
    let taken = number(read(DISKSIZE)?.trim(), "disksize")?;
    if taken != size {
        return Err(format!(
            "{DISKSIZE} reads {taken}, not the {size} asked for"
        ));
    }
    let mut device = open_device()?;
    write_header(&mut device, size)?;
    drop(device);
    sys::swapon(DEVICE_C).map_err(|e| format!("swapon {DEVICE}: {e}"))?;
    match swapping(&read("/proc/swaps")?) {
        Some(PRIORITY) => {
            crate::emit(&format!(
                "zram-swap: {DEVICE} is swap, {size} bytes, {ALGORITHM}, priority {PRIORITY}\n"
            ))?;
            Ok(0)
        }
        Some(other) => Err(wrong_priority(other)),
        None => Err(format!("swapon succeeded but /proc/swaps lacks {DEVICE}")),
    }
}

fn usage() -> String {
    "usage: zram-swap (takes no arguments)".to_string()
}

fn wrong_priority(other: i32) -> String {
    format!("/proc/swaps lists {DEVICE} at priority {other}, not {PRIORITY}")
}

fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn write(path: &str, value: &str) -> Result<(), String> {
    fs::write(path, value).map_err(|e| format!("{path}: {e}"))
}

/// A device with a writeback device can put swapped pages on storage. The
/// attribute is absent from a kernel built without writeback.
fn refuse_writeback() -> Result<(), String> {
    match fs::read_to_string(BACKING_DEV) {
        Ok(backing) if backing.trim_end_matches('\n') == "none" => Ok(()),
        Ok(backing) => Err(format!(
            "{BACKING_DEV} reads {:?}: a writeback device would put swap on storage",
            backing.trim_end()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{BACKING_DEV}: {e}")),
    }
}

/// `/dev/zram0`, opened for the header, once it is known to be the block
/// device whose number sysfs gives zram0.
fn open_device() -> Result<File, String> {
    let device = OpenOptions::new()
        .write(true)
        .open(DEVICE)
        .map_err(|e| format!("{DEVICE}: {e}"))?;
    let metadata = device.metadata().map_err(|e| format!("{DEVICE}: {e}"))?;
    let expected = device_number(&read(DEV)?)?;
    let rdev = metadata.rdev();
    let found = (devt::major(rdev), devt::minor(rdev));
    if !metadata.file_type().is_block_device() || found != expected {
        return Err(format!(
            "{DEVICE} is not the block device {}:{} that {DEV} names",
            expected.0, expected.1
        ));
    }
    Ok(device)
}

/// sysfs's `MAJOR:MINOR`.
fn device_number(text: &str) -> Result<(u64, u64), String> {
    let (major, minor) = text
        .trim()
        .split_once(':')
        .ok_or(format!("{DEV} is not MAJOR:MINOR"))?;
    Ok((number(major, "major")?, number(minor, "minor")?))
}

/// Half of `MemTotal`, whole pages, in bytes.
fn disksize(meminfo: &str) -> Result<u64, String> {
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .ok_or("/proc/meminfo has no MemTotal in kB")?;
    let bytes = number(kib.trim(), "MemTotal")?
        .checked_mul(1024)
        .ok_or("MemTotal overflows")?;
    let pages = bytes / 2 / PAGE;
    if pages < MIN_PAGES {
        return Err(format!(
            "half of MemTotal is {pages} pages, under {MIN_PAGES}"
        ));
    }
    Ok(pages * PAGE)
}

fn number(text: &str, what: &str) -> Result<u64, String> {
    text.parse()
        .map_err(|_| format!("{what} '{text}' is not a number"))
}

/// zram lists every algorithm and brackets the selected one.
fn selected(comp_algorithm: &str) -> bool {
    comp_algorithm.split_whitespace().any(|name| {
        name.strip_prefix('[')
            .and_then(|name| name.strip_suffix(']'))
            == Some(ALGORITHM)
    })
}

/// `/dev/zram0`'s priority, when `/proc/swaps` lists it as a partition.
fn swapping(swaps: &str) -> Option<i32> {
    swaps.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(DEVICE) || fields.next() != Some("partition") {
            return None;
        }
        // Size and used come before the priority.
        fields.nth(2).and_then(|priority| priority.parse().ok())
    })
}

/// The first page of a version-1 swap area, as mkswap writes it: version
/// and last page after the 1024 boot bytes, the signature in the page's
/// last ten. No bad pages, no UUID, no label.
fn header(size: u64) -> Result<[u8; PAGE_BYTES], String> {
    let last = (size / PAGE)
        .checked_sub(1)
        .and_then(|last| u32::try_from(last).ok())
        .ok_or("the device's page count does not fit a swap header")?;
    let mut page = [0u8; PAGE_BYTES];
    let mut put = |at: usize, bytes: &[u8]| -> Result<(), String> {
        page.get_mut(at..at + bytes.len())
            .ok_or("swap header field outside the page")?
            .copy_from_slice(bytes);
        Ok(())
    };
    put(1024, &1u32.to_le_bytes())?;
    put(1028, &last.to_le_bytes())?;
    put(PAGE_BYTES - 10, b"SWAPSPACE2")?;
    Ok(page)
}

fn write_header(device: &mut File, size: u64) -> Result<(), String> {
    let page = header(size)?;
    device
        .write_all(&page)
        .and_then(|()| device.sync_all())
        .map_err(|e| format!("write the swap header to {DEVICE}: {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn half_of_memtotal_in_whole_pages() {
        let meminfo = "MemTotal:       65012345 kB\nMemFree:          100 kB\n";
        let size = disksize(meminfo).unwrap();
        assert_eq!(size % PAGE, 0);
        assert_eq!(size, 65012345 * 1024 / 2 / PAGE * PAGE);
        assert_eq!(
            disksize("MemTotal: 1024 kB\n").unwrap_err(),
            "half of MemTotal is 128 pages, under 256"
        );
        for bad in [
            "",
            "MemFree: 1 kB\n",
            "MemTotal: 12 MB\n",
            "MemTotal: x kB\n",
        ] {
            assert!(disksize(bad).is_err(), "{bad}");
        }
        assert!(disksize("MemTotal: 18446744073709551615 kB\n").is_err());
    }

    #[test]
    fn only_a_bracketed_zstd_is_selected() {
        assert!(selected("lzo lzo-rle lz4 [zstd] deflate\n"));
        assert!(!selected("lzo [lzo-rle] lz4 zstd\n"));
        assert!(!selected("[zstd-x]\n"));
        assert!(!selected("[zstd\n"));
        assert!(!selected(""));
    }

    #[test]
    fn the_device_priority_is_read_from_the_swap_table() {
        let header = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";
        assert_eq!(swapping(header), None);
        assert_eq!(
            swapping(&format!("{header}/dev/zram0 partition 16777212 0 100\n")),
            Some(100)
        );
        assert_eq!(
            swapping(&format!("{header}/dev/zram0\tpartition\t1\t0\t-2\n")),
            Some(-2)
        );
        assert_eq!(
            swapping(&format!(
                "{header}/dev/zram1 partition 1 0 100\n/swap file 1 0 5\n"
            )),
            None
        );
        // A regular file planted at the device's path is not the device.
        assert_eq!(
            swapping(&format!("{header}/dev/zram0 file 1 0 100\n")),
            None
        );
        // The header line is never a device.
        assert_eq!(swapping("/dev/zram0 partition 1 0 100\n"), None);
    }

    #[test]
    fn the_device_number_is_sysfs_major_colon_minor() {
        assert_eq!(device_number("252:0\n"), Ok((252, 0)));
        assert!(device_number("252\n").is_err());
        assert!(device_number("x:0\n").is_err());
    }

    #[test]
    fn the_header_is_a_version_one_swap_area() {
        let size = 1024 * PAGE;
        let page = header(size).unwrap();
        assert_eq!(page[1024..1028], 1u32.to_le_bytes());
        assert_eq!(page[1028..1032], 1023u32.to_le_bytes());
        assert_eq!(&page[PAGE_BYTES - 10..], b"SWAPSPACE2");
        assert!(page[..1024].iter().all(|b| *b == 0));
        assert!(page[1032..PAGE_BYTES - 10].iter().all(|b| *b == 0));
        assert!(header((u64::from(u32::MAX) + 2) * PAGE).is_err());
        assert!(header(0).is_err());
        assert!(header(PAGE - 1).is_err());
    }

    #[test]
    fn any_argument_is_refused_before_anything_is_read() {
        let refused = run(&["--not-an-option".to_string()]).unwrap_err();
        assert!(refused.starts_with("usage: zram-swap"), "{refused}");
    }
}
