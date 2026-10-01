//! The confined raw-syscall layer: the whole `unsafe` surface of td-install,
//! recorded as UNSAFE.md §21.
//!
//! The crate root denies the unsafe lint and exactly one item here carries a
//! scoped allow: `syscall3`, the `syscall`-instruction body td-util and
//! td-init use. The surface is ONE syscall, `ioctl(2)`, with exactly TWO
//! value-pinned requests, both about loop devices: `LOOP_CTL_GET_FREE` on
//! `/dev/loop-control`, which returns the index of an unbound loop device
//! (adding one if none is free), and `LOOP_CONFIGURE` on that device, which
//! binds a backing file at an offset and size limit in one call. A third
//! request or a second syscall is a reviewed amendment; `main.rs`'s
//! `confinement` tests pin the roster, the values, the body and the callers.
//!
//! Deliberately NOT here: `LOOP_SET_FD` followed by `LOOP_SET_STATUS64`, which
//! would expose the whole backing disk at offset zero between the two calls;
//! `LOOP_CLR_FD`, since the device is configured to clear itself when its last
//! opener closes; and `LOOP_CTL_REMOVE`, which nothing here needs.
//!
//! The wrappers take typed values, never bytes: this module alone lays out
//! `struct loop_config`, with the flags fixed to autoclear, and
//! `loop_device.rs` reads back from sysfs what the kernel made of it.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-install's loop layer is x86_64-linux only (raw syscall ABI)");

const SYS_IOCTL: usize = 16;

/// `struct loop_config` (`linux/loop.h`): the backing descriptor and block
/// size as two `__u32`, a 232-byte `struct loop_info64`, then eight reserved
/// `__u64`. The kernel copies exactly this many bytes through the pointer.
const LOOP_CONFIG_LEN: usize = 304;

/// Byte offsets of the fields set: `fd` and `block_size`, then
/// `struct loop_info64` at 8, whose three read-only `__u64` identities come
/// before `lo_offset`, and whose `lo_number`, `lo_encrypt_type` and
/// `lo_encrypt_key_size` come before `lo_flags`. Every other byte is zero.
const CONFIG_FD: usize = 0;
const CONFIG_BLOCK_SIZE: usize = 4;
const INFO_OFFSET: usize = 8 + 24;
const INFO_SIZELIMIT: usize = 8 + 32;
const INFO_FLAGS: usize = 8 + 52;

/// `LO_FLAGS_AUTOCLEAR`, the only flag: not read-only, no partition scan,
/// no direct I/O.
const LO_FLAGS_AUTOCLEAR: u32 = 4;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI). Its body is
/// the ONLY `unsafe` in the crate. The scoped allow covers where `unsafe` may
/// appear, not what may be passed: this fn is safe to CALL, so its confinement
/// is module privacy plus the two typed wrappers below being its only callers.
#[inline]
#[allow(unsafe_code)]
fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax;
    // the arguments are integers or a pointer-as-usize whose pointee the
    // caller keeps live and correctly sized across the call. `options(nomem)`
    // is deliberately ABSENT: LOOP_CONFIGURE has the kernel READ through the
    // pointer, and promising the compiler this asm touches no memory would
    // let it defer the writes that fill the buffer past the call.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// A raw return as a `Result`: nonnegative is the value, negative an errno.
fn check(ret: isize) -> io::Result<usize> {
    usize::try_from(ret).map_err(|_| {
        io::Error::from_raw_os_error(
            ret.checked_neg()
                .and_then(|errno| i32::try_from(errno).ok())
                .unwrap_or(i32::MAX),
        )
    })
}

/// `ioctl(control, LOOP_CTL_GET_FREE)`: the index of an unbound loop device.
///
/// The index is a hint, not a reservation: another process can bind that
/// device before this one configures it, which `LOOP_CONFIGURE` then refuses.
pub fn free_loop(control: &File) -> io::Result<usize> {
    const LOOP_CTL_GET_FREE: usize = 0x4c82;
    check(syscall3(
        SYS_IOCTL,
        control.as_raw_fd() as usize,
        LOOP_CTL_GET_FREE,
        0,
    ))
}

/// `ioctl(device, LOOP_CONFIGURE, &config)`: bind `backing`'s bytes
/// `offset..offset + len` to `device` with `block_size`-byte logical blocks,
/// clearing itself when its last opener closes.
///
/// Both files are borrowed across the call; the kernel takes its own
/// reference to `backing`.
pub fn configure(
    device: &File,
    backing: &File,
    offset: u64,
    len: u64,
    block_size: u32,
) -> io::Result<()> {
    const LOOP_CONFIGURE: usize = 0x4c0a;
    let config = config(backing, offset, len, block_size)?;
    check(syscall3(
        SYS_IOCTL,
        device.as_raw_fd() as usize,
        LOOP_CONFIGURE,
        config.as_ptr() as usize,
    ))
    .map(drop)
}

/// The configuration bytes, every unset field zero.
fn config(
    backing: &File,
    offset: u64,
    len: u64,
    block_size: u32,
) -> io::Result<[u8; LOOP_CONFIG_LEN]> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message);
    let fd = u32::try_from(backing.as_raw_fd())
        .map_err(|_| invalid("the backing descriptor is not a descriptor"))?;
    if len == 0 {
        return Err(invalid("a loop over no bytes"));
    }
    let mut config = [0; LOOP_CONFIG_LEN];
    for (at, bytes) in [
        (CONFIG_FD, &fd.to_le_bytes()[..]),
        (CONFIG_BLOCK_SIZE, &block_size.to_le_bytes()[..]),
        (INFO_OFFSET, &offset.to_le_bytes()[..]),
        (INFO_SIZELIMIT, &len.to_le_bytes()[..]),
        (INFO_FLAGS, &LO_FLAGS_AUTOCLEAR.to_le_bytes()[..]),
    ] {
        at.checked_add(bytes.len())
            .and_then(|end| config.get_mut(at..end))
            .ok_or_else(|| invalid("loop configuration field out of range"))?
            .copy_from_slice(bytes);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both requests are really ISSUED: a regular file is not a loop device,
    /// so the kernel answers ENOTTY (25). Every other assertion about this
    /// module reads source text, which a wrapper that returned `Ok` without
    /// issuing anything would satisfy.
    #[test]
    fn both_requests_reach_the_kernel() {
        let dir = std::env::temp_dir().join(format!("td-install-loop-sys-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let file = std::fs::File::create_new(dir.join("file")).unwrap();
        let free = free_loop(&file).unwrap_err();
        let configured = configure(&file, &file, 0, 512, 512).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(free.raw_os_error(), Some(25), "{free}");
        assert_eq!(configured.raw_os_error(), Some(25), "{configured}");
    }

    #[test]
    fn a_negative_return_is_its_errno() {
        assert_eq!(check(-16).unwrap_err().raw_os_error(), Some(16));
        assert_eq!(check(3).unwrap(), 3);
        assert_eq!(
            check(isize::MIN).unwrap_err().raw_os_error(),
            Some(i32::MAX)
        );
    }

    /// The layout `linux/loop.h` gives, written out rather than derived from
    /// the constants above so the test does not agree with them by
    /// construction.
    #[test]
    fn the_configuration_puts_each_field_where_the_kernel_reads_it() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd = u32::try_from(file.as_raw_fd()).unwrap();
        let config = config(&file, 0x0102_0304_0506_0708, 0x1112_1314_1516_1718, 4096).unwrap();
        let mut expected = [0u8; 304];
        expected[0..4].copy_from_slice(&fd.to_le_bytes());
        expected[4..8].copy_from_slice(&4096u32.to_le_bytes());
        // loop_info64: lo_device, lo_inode, lo_rdevice at 8, 16, 24.
        expected[32..40].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        expected[40..48].copy_from_slice(&0x1112_1314_1516_1718u64.to_le_bytes());
        // lo_number, lo_encrypt_type, lo_encrypt_key_size at 48, 52, 56.
        expected[60..64].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(config, expected);
    }

    #[test]
    fn an_empty_range_refuses() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(config(&file, 0, 0, 512).is_err());
    }
}
