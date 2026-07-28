//! `losetup` — attach a verified image to a loop device, read-only.
//!
//! The last job td-boot reached busybox for. It was not a small dependency: the
//! ability to BOOT rested on a third-party multicall existing at an absolute
//! path and parsing `-r <device> <file>` the way td expected, with nothing
//! tying the two together — drop `losetup` from busybox's applet list and every
//! boot stops at the root loop, with no build-time complaint. That is the same
//! argument that moved `kill(2)` into td-svc, and it ends the same way: one
//! confined `ioctl(2)` request instead of a `fork`+`exec` of somebody else's
//! program.
//!
//! ## Read-only is a property of the descriptor, not a flag
//!
//! td-boot hands this applet the file it has already verified, as an open
//! descriptor, via `/proc/self/fd/0` — so the bytes attached are the bytes
//! whose SHA-256 it checked, not whatever the path resolves to now. That
//! descriptor is read-only, and `LOOP_SET_FD` marks the loop read-only when the
//! backing file has no write access. So `-r` cannot be forgotten here the way a
//! command-line flag can: it follows from what was handed over.
//!
//! Which is exactly why it is READ BACK. Nothing observable distinguishes a
//! read-only loop from a writable one at attach time — the ioctl succeeds
//! either way — and a writable loop over the verified root is a root filesystem
//! whose contents no longer match the hash that admitted it.
//! `/sys/dev/block/<major>:<minor>/ro` is the kernel's own answer, asked by the
//! number off the OPENED device rather than by the path string, and this refuses
//! unless it reads 1.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use crate::sys;

/// `losetup -r <loop-device> <backing-file>`.
///
/// `-r` is REQUIRED rather than accepted: this crate attaches verified,
/// read-only images and nothing else, so a caller that omitted it would be
/// asking for something td-boot never wants, and silently getting read-only
/// anyway would be an accepted-and-ignored flag.
pub fn run(args: &[String]) -> Result<u8, String> {
    let (device, backing) = parse(args)?;
    attach(device, backing).map_err(|e| format!("{device}: {e}"))?;
    Ok(0)
}

fn usage() -> String {
    "usage: losetup -r <loop-device> <backing-file>".to_string()
}

fn parse(args: &[String]) -> Result<(&str, &str), String> {
    let flag = args.first().map(String::as_str);
    let device = args.get(1).map(String::as_str);
    let backing = args.get(2).map(String::as_str);
    match (flag, device, backing, args.len()) {
        (Some("-r"), Some(device), Some(backing), 3) => {
            if !device.starts_with('/') || !backing.starts_with('/') {
                return Err(format!("both paths must be absolute\n{}", usage()));
            }
            Ok((device, backing))
        }
        _ => Err(usage()),
    }
}

fn attach(device: &str, backing: &str) -> io::Result<()> {
    // Read-only, and load-bearing: the kernel marks the loop read-only when the
    // backing file has no write access, so this is what `-r` actually means.
    let image = File::open(backing)?;
    // The DEVICE is opened read-only too. `LOOP_SET_FD` does not require write
    // access to it, the kernel treats a device opened without `FMODE_WRITE` as
    // another reason to mark the loop read-only, and it is what busybox's
    // `losetup -r` did — so this asks for no privilege the replaced call had.
    let loop_device = File::open(device)?;
    // Both descriptors stay open across the ioctl: the kernel takes its own
    // reference to the backing file, but dropping either before the call
    // returns would hand it a descriptor that is already closed.
    sys::attach_loop(loop_device.as_raw_fd(), image.as_raw_fd())?;
    // Asked about the device the ioctl actually went to, by its device NUMBER
    // rather than by the path string: a symlink, or a node whose minor does not
    // match its name, would otherwise have the authority checking one device
    // while having bound another.
    let readonly = read_only(&loop_device)?;
    drop(loop_device);
    drop(image);
    refuse_if_writable(device, readonly)
}

/// The whole point of the read-back, as its own decision so it can be tested.
///
/// Nothing observable distinguishes a read-only loop from a writable one at
/// attach time — the ioctl succeeds either way — so without this the read is
/// performed and discarded.
///
/// The device is deliberately NOT detached on refusal. `LOOP_SET_FD` sets no
/// `LO_FLAGS_AUTOCLEAR`, so the binding outlives the descriptors, and undoing it
/// would need `LOOP_CLR_FD` — a THIRD permitted ioctl request, widening the
/// confined surface to tidy up a device on a machine that is deliberately
/// refusing to boot. Nothing retries within a boot, and the next boot starts
/// from a fresh initramfs.
fn refuse_if_writable(device: &str, readonly: bool) -> io::Result<()> {
    if readonly {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "the kernel reports {device} as writable after attaching a read-only image; \
         refusing, because a writable loop over a verified root is a root whose \
         contents no longer match the hash that admitted it"
    )))
}

/// The kernel's own answer for the device this descriptor names.
///
/// `/sys/dev/block/<major>:<minor>/ro` rather than `/sys/block/<name>/ro`: the
/// number comes off the OPENED device, so the answer cannot be about a
/// different device than the one the ioctl was issued to.
fn read_only(loop_device: &File) -> io::Result<bool> {
    use std::os::linux::fs::MetadataExt;
    let rdev = loop_device.metadata()?.st_rdev();
    let path = format!("/sys/dev/block/{}:{}/ro", major(rdev), minor(rdev));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
    Ok(ro_flag(&text))
}

/// The sysfs `ro` attribute is exactly `0` or `1`. Anything else is not a
/// kernel saying "read-only", and this is the one place that must not be
/// generous about it.
fn ro_flag(text: &str) -> bool {
    text.trim() == "1"
}

/// glibc's `gnu_dev_major`/`gnu_dev_minor`, which is how Linux packs a `dev_t`.
/// Spelled out because getting these wrong reads a DIFFERENT device's `ro`.
fn major(rdev: u64) -> u64 {
    ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)
}

fn minor(rdev: u64) -> u64 {
    (rdev & 0xff) | ((rdev >> 12) & !0xff)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    /// The one shape td-boot uses, and nothing else.
    #[test]
    fn only_the_read_only_three_argument_form_is_accepted() {
        assert_eq!(
            parse(&args(&["-r", "/dev/loop0", "/proc/self/fd/0"])).unwrap(),
            ("/dev/loop0", "/proc/self/fd/0")
        );
        for bad in [
            &["/dev/loop0", "/img"][..],
            &["-r", "/dev/loop0"][..],
            &["-r", "/dev/loop0", "/img", "extra"][..],
            &["-d", "/dev/loop0", "/img"][..],
            &[][..],
        ] {
            assert!(
                parse(&args(bad)).is_err(),
                "{bad:?} was accepted; losetup attaches verified read-only images only"
            );
        }
    }

    /// A relative path would resolve against whatever directory td-boot left
    /// this process in, which is not something a boot may depend on.
    #[test]
    fn both_paths_must_be_absolute() {
        for bad in [
            &["-r", "dev/loop0", "/img"][..],
            &["-r", "/dev/loop0", "img"][..],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} was accepted");
        }
    }

    /// The kernel says read-only with `1`, and nothing else means it.
    ///
    /// This is the landing's whole security claim reduced to the one comparison
    /// that carries it: a `!= "0"`, or an `is_empty()`, or a bare `true` would
    /// admit a WRITABLE loop over the verified root, and the attach itself
    /// succeeds either way so nothing else would notice.
    #[test]
    fn only_a_kernel_ro_of_one_counts_as_read_only() {
        assert!(ro_flag("1"));
        assert!(ro_flag("1\n"));
        assert!(ro_flag(" 1 \n"));
        for writable in ["0", "0\n", "", "\n", "1x", "01", "yes", "true", "2"] {
            assert!(
                !ro_flag(writable),
                "'{writable}' was read as the kernel confirming read-only"
            );
        }
    }

    /// ...and the answer is ACTED on.
    ///
    /// Reading the flag and discarding it would leave every other test here
    /// green, which is exactly what made this property unasserted before.
    #[test]
    fn a_writable_loop_is_refused_and_a_read_only_one_is_not() {
        assert!(
            refuse_if_writable("/dev/loop0", true).is_ok(),
            "a read-only loop was refused"
        );
        let refused = refuse_if_writable("/dev/loop0", false);
        assert!(
            refused.is_err(),
            "a WRITABLE loop over the verified root was accepted"
        );
        assert!(
            refused
                .err()
                .is_some_and(|e| e.to_string().contains("/dev/loop0")),
            "the refusal must name the device"
        );
    }

    /// The device numbers are unpacked the way Linux packs them. Getting these
    /// wrong reads a DIFFERENT device's `ro` and still answers confidently.
    #[test]
    fn device_numbers_are_unpacked_the_way_the_kernel_packs_them() {
        // loop0 is 7:0, loop9 is 7:9 — the pairs this applet actually meets.
        assert_eq!((major(0x0700), minor(0x0700)), (7, 0));
        assert_eq!((major(0x0709), minor(0x0709)), (7, 9));
        // A minor above 255 spills into the high bits; a naive `rdev & 0xff`
        // would report 0 for 7:256 and read loop0's flag instead.
        let big = (7u64 << 8) | (256u64 & 0xff) | ((256u64 & !0xff) << 12);
        assert_eq!((major(big), minor(big)), (7, 256));
    }

    /// Something with no block sysfs node at all is an ERROR, not a silent
    /// "writable" and not a silent "read-only".
    ///
    /// A regular file, whose `st_rdev` is 0, so the path becomes
    /// `/sys/dev/block/0:0` — which no device can occupy. Deliberately not a
    /// character device: `/dev/null` is char 1:3, and block 1:3 is `/dev/ram3`,
    /// so asking about it by NUMBER would answer about a real, unrelated
    /// device. That is the confusion this read-back is built to avoid, and it
    /// is not a good test fixture for absence.
    #[test]
    fn something_with_no_block_sysfs_node_is_an_error() {
        let path = format!(
            "{}/td-init-losetup-{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::write(&path, b"");
        let Ok(file) = File::open(&path) else {
            eprintln!("note: cannot open a scratch file here; skipping");
            return;
        };
        assert!(
            read_only(&file).is_err(),
            "a thing with no block sysfs node must be an error, not a guess"
        );
        let _ = std::fs::remove_file(&path);
    }
}
