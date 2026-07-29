//! The crate's ONE `dev_t` packing, both directions.
//!
//! Two modules need it — `losetup` decodes an `rdev` to reach a device's sysfs
//! node, `mknod` encodes a pair to create one — and two copies that could
//! disagree is exactly the failure this packing invites: every wrong answer is a
//! well-formed device number naming a DIFFERENT device, which the kernel then
//! serves without complaint.
//!
//! The two directions use deliberately different formulas, which is not a
//! mismatch. `mknod(2)` takes the KERNEL's `new_encode_dev` — 32 bits, major in
//! 12, minor in 20. `rdev` read back through `std` arrives in glibc's 64-bit
//! packing, because `statx` reports major and minor separately and `std`
//! recombines them with `makedev`. The two agree on every value the 32-bit form
//! can represent, so the asymmetry only shows up at majors above 0xfff — which
//! `encode` refuses rather than truncates.

/// glibc's `gnu_dev_major`. Spelled out because getting it wrong reads a
/// DIFFERENT device and answers confidently.
pub fn major(rdev: u64) -> u64 {
    ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)
}

/// glibc's `gnu_dev_minor`. The `0xffff_ff00` is `~0xff` in THIRTY-TWO bits, which
/// is what the C macro computes: it casts `dev >> 12` to `unsigned int` first, so
/// bits from 44 up are dropped rather than folded into the minor. Writing Rust's
/// 64-bit `!0xff` here instead would leak a major's high bits into the answer.
/// Unreachable while majors are 12 bits — but this module exists to be the one
/// packing that is simply right.
pub fn minor(rdev: u64) -> u64 {
    (rdev & 0xff) | ((rdev >> 12) & 0xffff_ff00)
}

/// The kernel's `new_encode_dev`, which is what `mknod(2)` reads: minor's low 8
/// bits, then major, then minor's high bits above it.
///
/// Out of range is an ERROR rather than a truncation. A major of 0x1000 quietly
/// losing its top bit is a node pointing at driver 0, created successfully.
pub fn encode(major: u32, minor: u32) -> Result<usize, String> {
    if major > 0xfff {
        return Err(format!("major {major} does not fit the dev encoding"));
    }
    if minor > 0xf_ffff {
        return Err(format!("minor {minor} does not fit the dev encoding"));
    }
    Ok(((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A major's high bits must not leak into the minor.
    ///
    /// glibc truncates `dev >> 12` to 32 bits; Rust's `!0xff` on a `u64` does not,
    /// and the difference is a minor that silently carries someone else's major.
    #[test]
    fn a_high_major_does_not_leak_into_the_minor() {
        // Bit 44 set is a major of 0x1000 in glibc's packing — above what the
        // kernel can represent, and exactly what the wrong mask would fold in.
        let rdev: u64 = 1 << 44;
        assert_eq!(minor(rdev), 0, "the minor must not absorb the major's high bits");
    }

    /// Decoding, against the pairs these applets actually meet.
    #[test]
    fn device_numbers_are_unpacked_the_way_the_kernel_packs_them() {
        // loop0 is 7:0, loop9 is 7:9.
        assert_eq!((major(0x0700), minor(0x0700)), (7, 0));
        assert_eq!((major(0x0709), minor(0x0709)), (7, 9));
        // A minor above 255 spills into the high bits; a naive `rdev & 0xff`
        // would report 0 for 7:256 and read loop0's flag instead.
        let big = (7u64 << 8) | ((256u64 & !0xff) << 12);
        assert_eq!((major(big), minor(big)), (7, 256));
    }

    /// Encoding, against values checkable by hand.
    #[test]
    fn the_encoding_is_the_one_mknod_expects() {
        assert_eq!(encode(7, 0), Ok(0x700), "/dev/loop0, the one td creates");
        assert_eq!(encode(1, 3), Ok(0x103), "/dev/null");
        assert_eq!(encode(8, 16), Ok(0x810), "/dev/sdb");
        // The case a naive `(major << 8) | minor` gets wrong: minor's high bits
        // go ABOVE major, not into it.
        assert_eq!(encode(259, 1024), Ok(0x41_0300));
    }

    /// The two directions agree — on the values the 32-bit form can hold.
    #[test]
    fn encode_and_decode_are_inverses() {
        for (ma, mi) in [(7, 0), (1, 3), (8, 16), (259, 1024), (0xfff, 0xf_ffff)] {
            let d = encode(ma, mi).unwrap_or_default() as u64;
            assert_eq!((major(d), minor(d)), (u64::from(ma), u64::from(mi)), "{ma}:{mi}");
        }
    }

    /// Out of range is refused, not truncated.
    #[test]
    fn an_unencodable_device_number_is_an_error() {
        assert!(encode(0x1000, 0).is_err(), "major overflows its 12 bits");
        assert!(encode(0, 0x10_0000).is_err(), "minor overflows its 20 bits");
        assert!(encode(0xfff, 0xf_ffff).is_ok(), "the largest encodable pair");
    }

    /// Decoding checked against nodes the KERNEL created.
    ///
    /// Everything above is this file agreeing with itself. `/dev/null` is 1:3
    /// and `/dev/zero` 1:5 on every Linux, so reading their `rdev` back is the
    /// one check here that could catch a packing this crate is simply wrong
    /// about — and it is what makes `mknod`'s runtime readback worth anything.
    #[test]
    fn the_decoding_matches_nodes_the_kernel_created() {
        use std::os::unix::fs::MetadataExt;
        let mut checked = 0;
        for (path, want) in [("/dev/null", (1u64, 3u64)), ("/dev/zero", (1, 5))] {
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let rdev = meta.rdev();
            assert_eq!((major(rdev), minor(rdev)), want, "{path}");
            checked += 1;
        }
        assert!(checked > 0, "no /dev node available to check the decode against");
    }
}
