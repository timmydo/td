//! Which terminal `/dev/console` is (td-install/ENCRYPTION.md "Keyboard
//! console").
//!
//! It cannot be learned from a descriptor: `fstat` on any open of
//! `/dev/console` reports the console device, 5:1, never the terminal behind
//! it. The kernel lists its enabled consoles in `ACTIVE`, `/dev/console`'s
//! last (Linux 7.1.4's `show_cons_active`), so the last entry names it. No
//! ioctl is needed for this, and none is used.
//!
//! secret-line reads this to choose its lines, and td-boot, which includes
//! this one file by `#[path]` as it includes td-fs's real-file rule, to choose
//! whether it mirrors its console lines to the VT. Std file I/O only.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// The kernel's list of enabled consoles.
pub const ACTIVE: &str = "/sys/class/tty/console/active";

/// One sysfs page: `ACTIVE` is read up to this many bytes, and a longer one
/// is not understood.
pub const ACTIVE_MAX: usize = 4096;

/// What `ACTIVE` says `/dev/console` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// `tty1`, or `tty0`, which names the foreground VT and so tty1 here:
    /// `/dev/console` and `/dev/tty1` are one terminal.
    Vt,
    /// Any other terminal, such as the serial console the built-in command
    /// line names last: the VT is a second terminal.
    Other,
    /// `ACTIVE` could not be read or parsed. Callers treat this as `Vt`: one
    /// terminal, so nothing is written to one terminal twice.
    Unknown,
}

impl Console {
    /// Whether `/dev/tty1` is a terminal beside `/dev/console`.
    pub fn vt_is_second(self) -> bool {
        self == Console::Other
    }
}

/// `ACTIVE`'s content: names of printable ASCII without spaces, separated
/// by single spaces and ended by one newline. Anything else is `Unknown`.
pub fn parse(bytes: &[u8]) -> Console {
    if bytes.len() > ACTIVE_MAX {
        return Console::Unknown;
    }
    let Some(list) = bytes.strip_suffix(b"\n") else {
        return Console::Unknown;
    };
    let mut last: Option<&[u8]> = None;
    for name in list.split(|b| *b == b' ') {
        if name.is_empty() || !name.iter().all(|b| (0x21..=0x7e).contains(b)) {
            return Console::Unknown;
        }
        last = Some(name);
    }
    match last {
        Some(b"tty0" | b"tty1") => Console::Vt,
        Some(_) => Console::Other,
        None => Console::Unknown,
    }
}

/// `path`, normally `ACTIVE`, read up to `ACTIVE_MAX` bytes and parsed.
pub fn read(path: &Path) -> Console {
    let Ok(file) = File::open(path) else {
        return Console::Unknown;
    };
    let mut bytes = Vec::with_capacity(ACTIVE_MAX);
    // One byte past the page, so an over-long file is seen as one.
    match file.take(ACTIVE_MAX as u64 + 1).read_to_end(&mut bytes) {
        Ok(_) => parse(&bytes),
        Err(_) => Console::Unknown,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn the_last_entry_names_the_console() {
        assert_eq!(parse(b"tty0 ttyS0\n"), Console::Other);
        assert_eq!(parse(b"ttyS0 tty0\n"), Console::Vt);
        assert_eq!(parse(b"ttyS0 tty1\n"), Console::Vt);
        assert_eq!(parse(b"tty1\n"), Console::Vt);
        assert_eq!(parse(b"ttyS0\n"), Console::Other);
        assert_eq!(parse(b"tty2\n"), Console::Other);
        assert_eq!(parse(b"tty10\n"), Console::Other);
        assert!(parse(b"tty0 ttyS0\n").vt_is_second());
        assert!(!parse(b"ttyS0 tty0\n").vt_is_second());
        assert!(!Console::Unknown.vt_is_second());
    }

    #[test]
    fn anything_but_the_kernels_grammar_is_unknown() {
        for bad in [
            &b""[..],
            b"\n",
            b"ttyS0",
            b"tty0  ttyS0\n",
            b" tty0\n",
            b"tty0 \n",
            b"tty0\tttyS0\n",
            b"tty0 ttyS0\n\n",
            b"tty\x7f0\n",
            "tty0 ttyé\n".as_bytes(),
        ] {
            assert_eq!(parse(bad), Console::Unknown, "{bad:?}");
        }
        let mut long = b"ttyS0 ".repeat(ACTIVE_MAX / 6 + 1);
        long.push(b'\n');
        assert!(long.len() > ACTIVE_MAX);
        assert_eq!(parse(&long), Console::Unknown);
    }

    #[test]
    fn a_file_is_read_up_to_one_page() {
        let dir = std::env::temp_dir().join(format!("td-init-console-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let active = dir.join("active");
        std::fs::write(&active, b"tty0 ttyS0\n").unwrap();
        assert_eq!(read(&active), Console::Other);
        std::fs::write(&active, b"ttyS0 tty0\n").unwrap();
        assert_eq!(read(&active), Console::Vt);
        let mut page = vec![b'x'; ACTIVE_MAX - 1];
        page.push(b'\n');
        std::fs::write(&active, &page).unwrap();
        assert_eq!(read(&active), Console::Other);
        page.insert(0, b'x');
        std::fs::write(&active, &page).unwrap();
        assert_eq!(read(&active), Console::Unknown);
        assert_eq!(read(&dir.join("missing")), Console::Unknown);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
