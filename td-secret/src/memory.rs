//! The memory a process holding a key, a PIN or a passphrase requires: no
//! active swap that can put its pages on storage, and a zero core-dump
//! soft limit. A zram device without a writeback device keeps what it
//! swaps in memory, compressed, so it is admitted; every other device is
//! storage. Pure over `/proc/swaps`, `/proc/self/limits` and the
//! `/sys/block` attributes as the caller reads them, so each consumer reads
//! them under its own file rules: td-secret's store and workers, with every
//! crate compiling the store, td-install's storage worker
//! (td-install/DESIGN.md, "Storage operation worker"), td-authd's
//! credential intake, the compositor's PIN field and td-pass's portable
//! host, which lets its account accept named storage devices.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Room for `/sys/block/zram` and its digits, `/` and the longest
/// attribute read; a longer path is never a zram device's.
const ATTRIBUTE_PATH: usize = 64;

/// Refuses active swap that can put memory on storage. A missing
/// `/proc/swaps` is no swap: Linux registers it only when CONFIG_SWAP is
/// enabled. `sysfs(path)` reads one `/sys/block` attribute.
pub(crate) fn check_swap_state(
    swaps: io::Result<String>,
    sysfs: &dyn Fn(&Path) -> io::Result<String>,
) -> Result<(), String> {
    let swaps = match swaps {
        Ok(swaps) => swaps,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let mut storage = false;
    storage_swap(&swaps, sysfs, &mut |_| storage = true)?;
    if storage {
        return Err("secret memory requires no swap on storage".into());
    }
    Ok(())
}

/// Calls `found` with each active swap device in `/proc/swaps`'s `swaps`
/// that can put memory on storage, by the name the table gives. A zram
/// device keeps its compressed pages in memory unless it has a writeback
/// device: `backing_dev` reads `none`, or is absent in a kernel built
/// without writeback, which `sysfs` shows by the zram attribute
/// `comp_algorithm`. zram refuses a writeback device once it is
/// initialized, so a device that is swapping cannot gain one. Every other
/// device, a swap file, or a zram device whose attributes cannot be read
/// is storage.
pub(crate) fn storage_swap(
    swaps: &str,
    sysfs: &dyn Fn(&Path) -> io::Result<String>,
    found: &mut dyn FnMut(&str),
) -> Result<(), &'static str> {
    let mut lines = swaps.lines();
    if lines.next().is_none() {
        return Err("the host's swap table is unreadable");
    }
    for line in lines {
        // The kernel escapes space, tab, newline and backslash in a name and
        // nothing else, so only those separate columns: a name holding any
        // other whitespace stays whole and is never another device's.
        let mut fields = line.split([' ', '\t']).filter(|field| !field.is_empty());
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(kind) = fields.next() else {
            return Err("the host's swap table is unreadable");
        };
        if !in_memory(name, kind, sysfs) {
            found(name);
        }
    }
    Ok(())
}

fn in_memory(name: &str, kind: &str, sysfs: &dyn Fn(&Path) -> io::Result<String>) -> bool {
    let Some(device) = name.strip_prefix("/dev/") else {
        return false;
    };
    let Some(number) = device.strip_prefix("zram") else {
        return false;
    };
    if kind != "partition" || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let read = |attribute: &str| {
        let mut path = [0; ATTRIBUTE_PATH];
        match block_attribute(device, attribute, &mut path) {
            Some(path) => sysfs(path),
            None => Err(io::Error::from(io::ErrorKind::InvalidInput)),
        }
    };
    match read("backing_dev") {
        Ok(backing) => backing.trim_end_matches('\n') == "none",
        Err(error) if error.kind() == io::ErrorKind::NotFound => read("comp_algorithm").is_ok(),
        Err(_) => false,
    }
}

/// `/sys/block/DEVICE/ATTRIBUTE`, composed in `buffer` rather than
/// allocated; `None` when it does not fit.
fn block_attribute<'b>(
    device: &str,
    attribute: &str,
    buffer: &'b mut [u8; ATTRIBUTE_PATH],
) -> Option<&'b Path> {
    let mut length = 0usize;
    for part in ["/sys/block/", device, "/", attribute] {
        let end = length.checked_add(part.len())?;
        buffer
            .get_mut(length..end)?
            .copy_from_slice(part.as_bytes());
        length = end;
    }
    Some(Path::new(OsStr::from_bytes(buffer.get(..length)?)))
}

/// `operation`, once the swap and core-dump state allow it.
pub(crate) fn with_memory_state<T>(
    swaps: io::Result<String>,
    limits: io::Result<String>,
    sysfs: &dyn Fn(&Path) -> io::Result<String>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    check_swap_state(swaps, sysfs)?;
    check_core_limit(&limits.map_err(|e| e.to_string())?)?;
    operation()
}

pub(crate) fn check_core_limit(limits: &str) -> Result<(), String> {
    let core = limits
        .lines()
        .find_map(|line| line.strip_prefix("Max core file size"));
    if core.and_then(|line| line.split_whitespace().next()) != Some("0") {
        return Err("secret memory requires a zero core-dump soft limit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";

    /// A sysfs where zram0 has no writeback device, zram1 writes back to
    /// sda3, zram2 is from a kernel without writeback, and zram3's
    /// attributes cannot be read.
    fn sysfs(path: &Path) -> io::Result<String> {
        let path = path.to_str().unwrap_or_default();
        let (device, attribute) = path
            .strip_prefix("/sys/block/")
            .and_then(|rest| rest.split_once('/'))
            .unwrap_or_default();
        match (device, attribute) {
            ("zram0", "backing_dev") => Ok("none\n".into()),
            ("zram1", "backing_dev") => Ok("/dev/sda3\n".into()),
            ("zram2", "comp_algorithm") => Ok("lzo [zstd]\n".into()),
            ("zram3", _) => Err(io::ErrorKind::PermissionDenied.into()),
            _ => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn storage(swaps: &str) -> Vec<String> {
        let mut storage = Vec::new();
        match storage_swap(swaps, &sysfs, &mut |name| storage.push(name.to_owned())) {
            Ok(()) => storage,
            Err(_) => vec!["refused".to_owned()],
        }
    }

    fn line(name: &str, kind: &str) -> String {
        format!("{name} {kind} 16777212 0 100\n")
    }

    #[test]
    fn the_attribute_path_is_composed_in_place_and_bounded() {
        let mut buffer = [0; ATTRIBUTE_PATH];
        assert_eq!(
            block_attribute("zram12", "backing_dev", &mut buffer),
            Some(Path::new("/sys/block/zram12/backing_dev"))
        );
        let long = "9".repeat(ATTRIBUTE_PATH);
        assert_eq!(block_attribute(&long, "backing_dev", &mut buffer), None);
        let name = format!("/dev/zram{long}");
        assert_eq!(
            storage(&format!("{HEADER}{}", line(&name, "partition"))),
            [name]
        );
    }

    #[test]
    fn zram_without_writeback_is_memory_and_everything_else_storage() {
        assert!(storage(HEADER).is_empty());
        assert!(storage(&format!("{HEADER}\n")).is_empty());
        for kept in [
            line("/dev/zram0", "partition"),
            line("/dev/zram2", "partition"),
        ] {
            assert!(storage(&format!("{HEADER}{kept}")).is_empty(), "{kept}");
        }
        for (name, kind) in [
            ("/dev/zram1", "partition"),
            ("/dev/zram3", "partition"),
            ("/dev/zram4", "partition"),
            ("/dev/zram0", "file"),
            ("/dev/zramx", "partition"),
            ("/dev/zram", "partition"),
            ("/dev/zram0/../sda2", "partition"),
            ("/dev/sda2", "partition"),
            ("/swapfile", "file"),
            ("/tmp/dev/zram0", "partition"),
        ] {
            assert_eq!(
                storage(&format!("{HEADER}{}", line(name, kind))),
                [name],
                "{name} {kind}"
            );
        }
        assert_eq!(
            storage(&format!(
                "{HEADER}{}{}{}",
                line("/dev/zram0", "partition"),
                line("/dev/sda2", "partition"),
                line("/swapfile", "file")
            )),
            ["/dev/sda2", "/swapfile"]
        );
        assert_eq!(storage(""), ["refused"]);
        assert_eq!(storage(&format!("{HEADER}/dev/sda2\n")), ["refused"]);
    }

    #[test]
    fn a_name_keeps_whitespace_the_kernel_does_not_escape() {
        let table =
            format!("{HEADER}/swap\u{a0}one file 1 0 -2\n/swap\u{a0}two\u{2003}x file 1 0 -3\n");
        assert_eq!(
            storage(&table),
            ["/swap\u{a0}one", "/swap\u{a0}two\u{2003}x"]
        );
        assert_eq!(
            storage(&format!("{HEADER}/swap\\040one\tfile\t1\t0\t-2\n")),
            ["/swap\\040one"]
        );
    }

    #[test]
    fn swap_compiled_out_or_in_memory_is_safe_and_storage_or_unreadable_swap_is_refused() {
        let check = |swaps: io::Result<String>| check_swap_state(swaps, &sysfs);
        assert!(check(Err(io::ErrorKind::NotFound.into())).is_ok());
        assert!(check(Ok(HEADER.into())).is_ok());
        assert!(check(Ok(format!("{HEADER}{}", line("/dev/zram0", "partition")))).is_ok());
        for refused in [
            Ok(format!("{HEADER}{}", line("/dev/zram1", "partition"))),
            Ok(format!(
                "{HEADER}{}{}",
                line("/dev/zram0", "partition"),
                line("/swapfile", "file")
            )),
            Ok(String::new()),
            Err(io::ErrorKind::PermissionDenied.into()),
        ] {
            assert!(check(refused).is_err());
        }
    }

    #[test]
    fn protected_memory_refusal_prevents_the_operation() {
        // An operation that ran would answer `Ok`.
        let never = || Ok(());
        let no_swap = || Ok(HEADER.to_owned());
        let no_core = || Ok("Max core file size 0 unlimited bytes\n".to_owned());
        for (swaps, limits) in [
            (Ok(format!("{HEADER}/swap file 1 1 0\n")), no_core()),
            (Err(io::ErrorKind::PermissionDenied.into()), no_core()),
            (
                no_swap(),
                Ok("Max core file size unlimited unlimited bytes\n".into()),
            ),
            (no_swap(), Err(io::ErrorKind::PermissionDenied.into())),
            (no_swap(), Ok(String::new())),
        ] {
            assert!(with_memory_state(swaps, limits, &sysfs, never).is_err());
        }
        assert_eq!(
            with_memory_state(no_swap(), no_core(), &sysfs, || Ok(42)),
            Ok(42)
        );
        let zram = Ok(format!("{HEADER}{}", line("/dev/zram0", "partition")));
        assert_eq!(
            with_memory_state(zram, no_core(), &sysfs, || Ok(43)),
            Ok(43)
        );
        assert_eq!(
            with_memory_state(
                Err(io::ErrorKind::NotFound.into()),
                no_core(),
                &sysfs,
                || Ok(44)
            ),
            Ok(44)
        );
    }

    #[test]
    fn only_a_zero_core_limit_is_admitted() {
        let limits = |soft: &str| {
            format!(
                "Limit Soft Limit Hard Limit Units\nMax core file size {soft} unlimited bytes\n"
            )
        };
        assert!(check_core_limit(&limits("0")).is_ok());
        for soft in ["unlimited", "1", "00 x"] {
            assert!(check_core_limit(&limits(soft)).is_err(), "{soft}");
        }
        assert!(check_core_limit("").is_err());
        assert!(check_core_limit("Max open files 0 0 files\n").is_err());
    }
}
