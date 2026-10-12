//! td-kexec — the guest-side kexec helper for td's image-based boot.
//!
//! It performs exactly four raw Linux x86_64 syscalls and nothing else:
//!   * `kexec_file_load(2)` (#320) — stage the selected kernel + initramfs
//!   * `reboot(2)` (#169) with `LINUX_REBOOT_CMD_KEXEC` — jump into it
//!   * `memfd_create(2)` (#319) with `MFD_CLOEXEC | MFD_ALLOW_SEALING |
//!     MFD_NOEXEC_SEAL`, and
//!   * `fcntl(2)` (#72) with `F_ADD_SEALS` and the four seals below — the
//!     sealed, unlinked initramfs copy of the volume-key handoff (`--fds-key`)
//!
//! Payload authentication is td-boot's job, NOT this program's: `--fds-key`
//! only re-hashes its own copy against the digest td-boot already verified.
//! UNSAFE.md §1 records the surface. Unlike builder's crate-level
//! `#![allow(unsafe_code)]`, the confinement here is compiler-enforced: the
//! crate `#![deny(unsafe_code)]`s and only `syscall5` carries a scoped
//! `#[allow]`, so any other `unsafe` reds; the `confinement` tests pin which
//! requests reach it.
//!
//! A `kexec_file_load` that fails with ENOMEM is retried once after writing
//! 1 to `/proc/sys/vm/drop_caches` through std; that write is no new request.
//!
//! Usage: `td-kexec <kernel> <initramfs|-> <cmdline>`
//!        `td-kexec --fds <cmdline>` (kernel on fd 0, initramfs on fd 1)
//!        `td-kexec --fds-key <initramfs sha256> <key pipe> <cmdline>`
//!        `td-kexec --fds-key-admit <initramfs sha256> <key pipe> <record pipe> <cmdline>`
//!   `<initramfs>` == "-" boots with no initramfs (`KEXEC_FILE_NO_INITRAMFS`).
//!   `--fds-key` takes the kernel and initramfs as `--fds` does and the volume
//!   key from a pipe named `/proc/<pid>/fd/<n>`, never from argv or the
//!   environment (td-install/ENCRYPTION.md "Boot and authority boundaries").
//!   `--fds-key-admit` also takes the protected tier's admission record from
//!   a second pipe named the same way, under the key's rules, and appends it
//!   as the archive's second member ("Verified account handoff").
#![deny(unsafe_code)]

// The handoff's archive member and key length, which the deployment
// initramfs reads back: one statement of the v1 contract for both sides.
#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code, reason = "shared newc appendix writer")]
mod cpio;
#[path = "../../td-boot/src/protocol.rs"]
#[allow(dead_code, reason = "shared boot deployment contract")]
mod protocol;
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code, reason = "shared streaming SHA-256 implementation")]
mod sha256;

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-kexec is x86_64-linux only (raw syscall ABI)");

const SYS_FCNTL: usize = 72;
const SYS_REBOOT: usize = 169;
const SYS_MEMFD_CREATE: usize = 319;
const SYS_KEXEC_FILE_LOAD: usize = 320;

// reboot(2) magics + the kexec command (linux/reboot.h).
const LINUX_REBOOT_MAGIC1: usize = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: usize = 0x2812_1969;
const LINUX_REBOOT_CMD_KEXEC: usize = 0x4558_4543;

// kexec_file_load(2) flags (linux/kexec.h).
const KEXEC_FILE_NO_INITRAMFS: usize = 0x0000_0004;

// memfd_create(2) flags (linux/memfd.h): MFD_CLOEXEC | MFD_ALLOW_SEALING |
// MFD_NOEXEC_SEAL, so the copy can never be executed and vm.memfd_noexec=2
// cannot refuse the call.
const MFD_FLAGS: usize = 0x0001 | 0x0002 | 0x0008;
// open(2) O_NONBLOCK (asm-generic/fcntl.h on x86_64), for the key pipe.
const O_NONBLOCK: i32 = 0o4000;
// Kernel parameters that keep the initramfs readable after boot
// (/sys/firmware/initrd), which would expose the key archive.
const INITRD_RETAINING_PARAMS: &[&str] = &["retain_initrd", "keepinitrd"];
// fcntl(2) F_ADD_SEALS, F_LINUX_SPECIFIC_BASE + 9 (linux/fcntl.h).
const F_ADD_SEALS: usize = 1024 + 9;
// F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE (linux/fcntl.h).
const SEALS: usize = 0x0001 | 0x0002 | 0x0004 | 0x0008;
// The installed selector hashes root.erofs, filling the page cache with
// clean pages; kexec_file_load's __GFP_NORETRY allocations (the control
// page, x86's transition page tables) may fail rather than reclaim enough
// of them. On its ENOMEM td-kexec drops them (1: page cache only), which
// the kernel logs as a rate-limited `drop_caches: 1` info line, and stages
// once more. The live selector hashes no root image; the retry serves it too.
const DROP_CACHES: &str = "/proc/sys/vm/drop_caches";
const DROP_CLEAN_PAGE_CACHE: &[u8] = b"1\n";
// ENOMEM (asm-generic/errno-base.h).
const ENOMEM: i32 = 12;
// Visible only as the `/proc/<pid>/fd` link text; the memfd has no path.
const MEMFD_NAME: &CStr = c"td-kexec-handoff";

// kexec_file_load reads an initramfs of at most KEXEC_FILE_SIZE_MAX, 4 GiB
// (kernel/kexec_file.c); the copy and its appended archive must fit.
const MAX_HANDOFF_BYTES: u64 = 4 << 30;
const COPY_CHUNK: usize = 64 * 1024;
// Owner-read only; the deployment initramfs removes it before starting the
// system.
const VOLUME_KEY_MODE: u32 = 0o400;
// The admission record's member has the key's mode; the deployment
// initramfs's `td-boot admit` moves it to the system's /run.
const ADMIT_RECORD_MODE: u32 = 0o400;
// newc's fixed header length (engine/src/cpio.rs).
const CPIO_HEADER_BYTES: usize = 110;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `builder/src/sys.rs`. This function's body is the ONLY `unsafe` in the crate;
/// the scoped `#[allow]` (under the crate `#![deny(unsafe_code)]`) is the
/// compiler-enforced confinement — an `unsafe` anywhere else fails the build.
#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax;
    // the args are plain integers or a pointer-as-usize whose pointee the caller
    // keeps live across the call. No memory is aliased beyond the kernel's read.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Turn a raw syscall return into a `Result`, mirroring `sys.rs::check`.
fn check(ret: isize) -> std::io::Result<isize> {
    if ret < 0 {
        Err(std::io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

fn usage_err() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "usage: td-kexec <kernel> <initramfs|-> <cmdline>\n       td-kexec --fds <cmdline>\n       \
         td-kexec --fds-key <initramfs sha256> </proc/PID/fd/N key pipe> <cmdline>\n       \
         td-kexec --fds-key-admit <initramfs sha256> </proc/PID/fd/N key pipe> \
         </proc/PID/fd/N record pipe> <cmdline>",
    )
}

fn invalid(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

enum Inputs {
    Paths(OsString, OsString, OsString),
    Fds(OsString),
    FdsKey {
        expected: [u8; 64],
        key_pipe: PathBuf,
        /// `--fds-key-admit`'s record pipe; none for `--fds-key`.
        record_pipe: Option<PathBuf>,
        cmdline: OsString,
    },
}

fn parse_args<I: Iterator<Item = OsString>>(mut args: I) -> std::io::Result<Inputs> {
    let first = args.next().ok_or_else(usage_err)?;
    if first == OsStr::new("--fds") {
        let cmdline = args.next().ok_or_else(usage_err)?;
        if args.next().is_some() {
            return Err(usage_err());
        }
        return Ok(Inputs::Fds(cmdline));
    }
    let admits = first == OsStr::new("--fds-key-admit");
    if admits || first == OsStr::new("--fds-key") {
        let digest = args.next().ok_or_else(usage_err)?;
        let key_pipe = args.next().ok_or_else(usage_err)?;
        let record_pipe = if admits {
            Some(args.next().ok_or_else(usage_err)?)
        } else {
            None
        };
        let cmdline = args.next().ok_or_else(usage_err)?;
        if args.next().is_some() {
            return Err(usage_err());
        }
        return Ok(Inputs::FdsKey {
            expected: expected_digest(&digest)?,
            key_pipe: pipe_path(&key_pipe, "volume key")?,
            record_pipe: record_pipe
                .map(|record| pipe_path(&record, "admission record"))
                .transpose()?,
            cmdline,
        });
    }
    let initramfs = args.next().ok_or_else(usage_err)?;
    let cmdline = args.next().ok_or_else(usage_err)?;
    if args.next().is_some() {
        return Err(usage_err());
    }
    Ok(Inputs::Paths(first, initramfs, cmdline))
}

/// The manifest's initramfs digest td-boot verified: 64 lowercase hex digits,
/// td-boot's one statement of a digest's shape.
fn expected_digest(arg: &OsStr) -> std::io::Result<[u8; 64]> {
    let bytes = arg.as_bytes();
    if !protocol::valid_digest(bytes) {
        return Err(invalid(
            "the expected initramfs digest is not 64 lowercase hex digits".to_string(),
        ));
    }
    let mut digest = [0u8; 64];
    digest.copy_from_slice(bytes);
    Ok(digest)
}

/// `/proc/<pid>/fd/<n>`, decimal and nothing else: the name of a descriptor
/// the caller holds, as td-install names one to cryptsetup. The shape carries
/// no secret and admits no other file; `open_pipe` then requires a pipe.
/// `what` names the payload: the volume key or the admission record.
fn pipe_path(arg: &OsStr, what: &str) -> std::io::Result<PathBuf> {
    let refuse = || invalid(format!("the {what} must be named as /proc/<pid>/fd/<n>"));
    let rest = arg.as_bytes().strip_prefix(b"/proc/").ok_or_else(refuse)?;
    let mut parts = rest.split(|byte| *byte == b'/');
    let (Some(pid), Some(b"fd"), Some(fd), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(refuse());
    };
    if !canonical_decimal(pid) || !canonical_decimal(fd) {
        return Err(refuse());
    }
    Ok(PathBuf::from(arg))
}

/// 1 to 10 ASCII digits without a leading zero, except `0` itself.
fn canonical_decimal(text: &[u8]) -> bool {
    !text.is_empty()
        && text.len() <= 10
        && text.iter().all(u8::is_ascii_digit)
        && (text.len() == 1 || text.first() != Some(&b'0'))
}

/// Opens the named pipe end anew and refuses anything but a pipe, so the key
/// or the record never comes from a file that persists. Non-blocking: the
/// caller writes the whole payload and closes every write end first, so a
/// read that would wait is a refusal rather than a selector hung on its own
/// pipe.
fn open_pipe(path: &Path, what: &str) -> std::io::Result<File> {
    let pipe = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{what} pipe: {e}")))?;
    if !pipe.metadata()?.file_type().is_fifo() {
        return Err(invalid(format!("the {what} descriptor is not a pipe")));
    }
    Ok(pipe)
}

/// Writes 1 to `vm.drop_caches`: the kernel drops clean, unmapped page cache
/// and skips dirty pages and shmem, the handoff memfd included, so nothing
/// unwritten is lost. Through std's open and write; no request of its own.
fn drop_clean_page_cache() -> std::io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(DROP_CACHES)?
        .write_all(DROP_CLEAN_PAGE_CACHE)?;
    let _ = writeln!(
        std::io::stderr(),
        "td-kexec: kexec_file_load: out of memory; dropped the clean page cache, retrying once"
    );
    Ok(())
}

/// Stages the image, and on ENOMEM alone drops the clean page cache and
/// stages once more, returning that attempt's result. A failed drop reports
/// the ENOMEM with the drop's error beside it.
fn stage_with_reclaim(
    mut stage: impl FnMut() -> std::io::Result<()>,
    drop_cache: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let enomem = match stage() {
        Err(e) if e.raw_os_error() == Some(ENOMEM) => e,
        staged => return staged,
    };
    drop_cache().map_err(|drop_error| {
        std::io::Error::new(
            enomem.kind(),
            format!("{enomem}; dropping the page cache to retry failed: {drop_error}"),
        )
    })?;
    stage()
}

fn load(kernel_fd: i32, initrd_fd: i32, flags: usize, cmdline: &OsStr) -> std::io::Result<()> {
    // The kernel copies `cmdline_len` bytes and requires the last be NUL, so pass
    // the length WITH the terminator.
    let cmdline_c = CString::new(cmdline.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cmdline contains an interior NUL byte",
        )
    })?;
    let cmdline_bytes = cmdline_c.as_bytes_with_nul();

    // kexec_file_load(kernel_fd, initrd_fd, cmdline_len, cmdline_ptr, flags)
    let stage = || {
        check(syscall5(
            SYS_KEXEC_FILE_LOAD,
            kernel_fd as usize,
            initrd_fd as usize,
            cmdline_bytes.len(),
            cmdline_bytes.as_ptr() as usize,
            flags,
        ))
        .map(|_| ())
    };
    stage_with_reclaim(stage, drop_clean_page_cache)?;

    // reboot(magic1, magic2, LINUX_REBOOT_CMD_KEXEC, NULL) — jumps into the
    // staged image and does not return on success.
    check(syscall5(
        SYS_REBOOT,
        LINUX_REBOOT_MAGIC1,
        LINUX_REBOOT_MAGIC2,
        LINUX_REBOOT_CMD_KEXEC,
        0,
        0,
    ))?;

    Err(std::io::Error::other(
        "reboot(LINUX_REBOOT_CMD_KEXEC) returned without booting the staged image",
    ))
}

fn run_paths(kernel: OsString, initramfs: OsString, cmdline: OsString) -> std::io::Result<()> {
    let kernel_file = File::open(&kernel)?;
    let initrd_file;
    let (initrd_fd, flags): (i32, usize) = if initramfs.as_os_str() == OsStr::new("-") {
        (-1, KEXEC_FILE_NO_INITRAMFS)
    } else {
        initrd_file = File::open(&initramfs)?;
        (initrd_file.as_raw_fd(), 0)
    };
    load(
        kernel_file.as_raw_fd(),
        initrd_fd,
        flags,
        cmdline.as_os_str(),
    )
}

/// Best-effort zeroing that the optimizer cannot drop as a dead store, as
/// td-tpm's `zero` does.
fn zero(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

/// Fills `buffer` from `source` and then requires end of file: exactly
/// `buffer.len()` bytes, the key's rule and the record's. A short or long
/// payload is refused without naming a byte of it; `what` names it.
fn read_exactly(source: &mut dyn Read, buffer: &mut [u8], what: &str) -> std::io::Result<()> {
    let length = buffer.len();
    source.read_exact(buffer).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            invalid(format!("the {what} ended before {length} bytes"))
        }
        std::io::ErrorKind::WouldBlock => invalid(format!(
            "the {what} pipe held fewer than {length} bytes with a write end open"
        )),
        _ => e,
    })?;
    let mut extra = [0u8; 1];
    loop {
        match source.read(&mut extra) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                zero(&mut extra);
                return Err(invalid(format!("the {what} is longer than {length} bytes")));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(invalid(format!(
                    "the {what} pipe still has a write end open"
                )));
            }
            Err(e) => return Err(e),
        }
    }
}

/// The volume key, in one heap allocation of its exact length that is never
/// moved or grown, zeroed on drop. Neither `Debug` nor `Clone`.
struct VolumeKey(Box<[u8]>);

impl VolumeKey {
    /// Exactly `VOLUME_KEY_BYTES` and then end of file.
    fn read(source: &mut dyn Read) -> std::io::Result<Self> {
        let mut key = VolumeKey(vec![0u8; protocol::VOLUME_KEY_BYTES].into_boxed_slice());
        read_exactly(source, &mut key.0, "volume key")?;
        Ok(key)
    }
}

/// The admission record: exactly `ADMIT_RECORD_BYTES` and then end of file.
/// td-boot writes and judges its fields; td-kexec only carries its bytes.
fn read_record(source: &mut dyn Read) -> std::io::Result<[u8; protocol::ADMIT_RECORD_BYTES]> {
    let mut record = [0u8; protocol::ADMIT_RECORD_BYTES];
    read_exactly(source, &mut record, "admission record")?;
    Ok(record)
}

impl Drop for VolumeKey {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// The handoff archive with zeros where the key goes, and where that is: the
/// key member, then `record`'s member when there is one, then the trailer.
/// The key itself never enters this buffer: it is written to the memfd
/// straight from `VolumeKey` between the archive's two halves.
fn key_archive(
    record: Option<&[u8; protocol::ADMIT_RECORD_BYTES]>,
) -> std::io::Result<(Vec<u8>, usize)> {
    let placeholder = [0u8; protocol::VOLUME_KEY_BYTES];
    let mut entries = vec![cpio::Entry {
        name: protocol::VOLUME_KEY_MEMBER,
        mode: VOLUME_KEY_MODE,
        kind: cpio::Kind::File(&placeholder),
    }];
    if let Some(record) = record {
        entries.push(cpio::Entry {
            name: protocol::ADMIT_MEMBER,
            mode: ADMIT_RECORD_MODE,
            kind: cpio::Kind::File(record),
        });
    }
    let archive = cpio::build(&entries).map_err(invalid)?;
    // newc: the header, the name and its NUL padded to 4, then the data.
    let named = CPIO_HEADER_BYTES
        .saturating_add(protocol::VOLUME_KEY_MEMBER.len())
        .saturating_add(1);
    let data_at = named.saturating_add(cpio::alignment_padding(named));
    let data_end = data_at.saturating_add(protocol::VOLUME_KEY_BYTES);
    match archive.get(data_at..data_end) {
        Some(data) if data.iter().all(|byte| *byte == 0) => Ok((archive, data_end)),
        _ => Err(std::io::Error::other(
            "the handoff archive's key field is not where newc puts it",
        )),
    }
}

/// memfd_create(name, MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL): an
/// unlinked, sealable, never-executable file. The descriptor stays a raw
/// number, released at exit or kexec, since adopting it into a `File` would
/// be a second scoped `unsafe`.
fn memfd_create() -> std::io::Result<i32> {
    let fd = check(syscall5(
        SYS_MEMFD_CREATE,
        MEMFD_NAME.as_ptr() as usize,
        MFD_FLAGS,
        0,
        0,
        0,
    ))?;
    i32::try_from(fd).map_err(|_| std::io::Error::other("memfd_create returned no descriptor"))
}

/// fcntl(fd, F_ADD_SEALS, SEAL | SHRINK | GROW | WRITE).
fn add_seals(fd: i32) -> std::io::Result<()> {
    check(syscall5(SYS_FCNTL, fd as usize, F_ADD_SEALS, SEALS, 0, 0))?;
    Ok(())
}

/// Builds the sealed initramfs `--fds-key` hands to kexec_file_load: a copy
/// of `initramfs`, refused unless it hashes to `expected`, padded to 4, then
/// one newc archive holding the volume key, read from `key` only after the
/// copy verified, and for `--fds-key-admit` the admission record from
/// `record` before it in time and after it in the archive, read only then
/// too. Returns the memfd's descriptor.
///
/// The memfd is written through a second open file of it, closed before the
/// seals: kexec_file_load refuses an initramfs open for writing (ETXTBSY),
/// which memfd_create's own descriptor does not count as.
fn prepare_handoff(
    initramfs: &File,
    expected: &[u8; 64],
    key: &mut dyn Read,
    record: Option<&mut dyn Read>,
) -> std::io::Result<i32> {
    let metadata = initramfs.metadata()?;
    if !metadata.is_file() {
        return Err(invalid(
            "the deployment initramfs is not a regular file".to_string(),
        ));
    }
    if metadata.len() > MAX_HANDOFF_BYTES {
        return Err(invalid(format!(
            "the deployment initramfs exceeds kexec's {MAX_HANDOFF_BYTES}-byte bound"
        )));
    }
    let memfd = memfd_create()?;
    let mut copy = OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/fd/{memfd}"))?;
    let mut hasher = sha256::Sha256::new();
    let mut chunk = vec![0u8; COPY_CHUNK];
    let mut length: u64 = 0;
    loop {
        // Positioned reads: the descriptor's shared offset may stand anywhere
        // after td-boot's verification.
        let read = match initramfs.read_at(&mut chunk, length) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let bytes = chunk
            .get(..read)
            .ok_or_else(|| std::io::Error::other("read past its buffer"))?;
        length = length.saturating_add(read as u64);
        if length > MAX_HANDOFF_BYTES {
            return Err(invalid(format!(
                "the deployment initramfs exceeds kexec's {MAX_HANDOFF_BYTES}-byte bound"
            )));
        }
        hasher.update(bytes);
        copy.write_all(bytes)?;
    }
    let digest = sha256::to_base16(&hasher.finalize());
    if digest.as_bytes() != expected.as_slice() {
        return Err(invalid(format!(
            "the initramfs copy hashes to {digest}, not the verified {}; \
             no volume key was read",
            String::from_utf8_lossy(expected)
        )));
    }
    // The record is read before the key, so the key lives only from its read
    // to its write; the archive puts it after the key.
    let record = record.map(read_record).transpose()?;
    let padding = cpio::alignment_padding((length % 4) as usize);
    let (archive, data_end) = key_archive(record.as_ref())?;
    let total = length
        .saturating_add(padding as u64)
        .saturating_add(archive.len() as u64);
    if total > MAX_HANDOFF_BYTES {
        return Err(invalid(format!(
            "the initramfs and its key archive exceed kexec's {MAX_HANDOFF_BYTES}-byte bound"
        )));
    }
    let data_at = data_end.saturating_sub(protocol::VOLUME_KEY_BYTES);
    let (Some(head), Some(tail)) = (archive.get(..data_at), archive.get(data_end..)) else {
        return Err(std::io::Error::other("the handoff archive is truncated"));
    };
    let zeros = [0u8; 3];
    copy.write_all(
        zeros
            .get(..padding)
            .ok_or_else(|| std::io::Error::other("padding beyond 3 bytes"))?,
    )?;
    copy.write_all(head)?;
    {
        let volume_key = VolumeKey::read(key)?;
        copy.write_all(&volume_key.0)?;
    }
    copy.write_all(tail)?;
    if copy.metadata()?.len() != total {
        return Err(std::io::Error::other(
            "the handoff memfd's length differs from what was written",
        ));
    }
    drop(copy);
    add_seals(memfd)?;
    Ok(memfd)
}

/// The kernel's `isspace` (lib/ctype.c): bytes 9 to 13, space, and 0xA0,
/// which `next_arg` splits the command line on.
fn kernel_space(byte: &u8) -> bool {
    matches!(*byte, 9..=13 | b' ' | 0xa0)
}

/// Refuses a command line that would keep the initramfs, and so the key
/// archive, readable after boot. The kernel strips quotes from a parameter
/// and treats `-` and `_` in its name alike, so the comparison does too.
fn refuse_initrd_retention(cmdline: &OsStr) -> std::io::Result<()> {
    for token in cmdline.as_bytes().split(kernel_space) {
        let unquoted: Vec<u8> = token
            .iter()
            .filter(|byte| **byte != b'"')
            .map(|byte| if *byte == b'-' { b'_' } else { *byte })
            .collect();
        let name = unquoted.split(|byte| *byte == b'=').next().unwrap_or(&[]);
        if INITRD_RETAINING_PARAMS
            .iter()
            .any(|param| param.as_bytes() == name)
        {
            return Err(invalid(format!(
                "the command line names {}, which would keep the volume key readable \
                 after boot",
                String::from_utf8_lossy(name)
            )));
        }
    }
    Ok(())
}

/// `--fds-key` and `--fds-key-admit`: the kernel on fd 0 and the verified
/// initramfs on fd 1, as `--fds`; kexec_file_load then takes the sealed copy
/// in its place.
fn run_fds_key(
    expected: &[u8; 64],
    key_pipe: &Path,
    record_pipe: Option<&Path>,
    cmdline: &OsStr,
) -> std::io::Result<()> {
    refuse_initrd_retention(cmdline)?;
    let mut key = open_pipe(key_pipe, "volume key")?;
    let mut record = record_pipe
        .map(|path| open_pipe(path, "admission record"))
        .transpose()?;
    let initramfs = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let memfd = prepare_handoff(
        &initramfs,
        expected,
        &mut key,
        record.as_mut().map(|pipe| pipe as &mut dyn Read),
    )?;
    drop(key);
    drop(record);
    load(0, memfd, 0, cmdline)
}

fn run() -> std::io::Result<()> {
    // args_os()/OsString, not args()/String: paths are arbitrary OS bytes and
    // the String iterator panics on non-UTF-8 input.
    match parse_args(std::env::args_os().skip(1))? {
        Inputs::Paths(kernel, initramfs, cmdline) => run_paths(kernel, initramfs, cmdline),
        // td-boot maps its already-open, verified files onto these descriptors
        // across exec. Stdout is reserved for the read-only initramfs; stderr
        // remains the diagnostic channel.
        Inputs::Fds(cmdline) => load(0, 1, 0, cmdline.as_os_str()),
        Inputs::FdsKey {
            expected,
            key_pipe,
            record_pipe,
            cmdline,
        } => run_fds_key(&expected, &key_pipe, record_pipe.as_deref(), &cmdline),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Fallible write, not eprintln!, which PANICS if the stderr write
            // fails (e.g. EPIPE); the error path must never panic.
            let _ = writeln!(std::io::stderr(), "td-kexec: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(xs: &[&str]) -> std::vec::IntoIter<OsString> {
        xs.iter()
            .map(|s| OsString::from(*s))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parse_requires_exactly_three_args() {
        assert!(parse_args(args(&["k"])).is_err());
        assert!(parse_args(args(&["k", "i"])).is_err());
        assert!(parse_args(args(&["k", "i", "c"])).is_ok());
    }

    #[test]
    fn parse_rejects_a_fourth_arg() {
        assert!(parse_args(args(&["k", "i", "c", "extra"])).is_err());
    }

    #[test]
    fn parse_preserves_the_three_values() {
        let parsed = parse_args(args(&["/boot/bzImage", "-", "console=ttyS0"])).unwrap();
        match parsed {
            Inputs::Paths(k, i, c) => {
                assert_eq!(k, OsString::from("/boot/bzImage"));
                assert_eq!(i, OsString::from("-"));
                assert_eq!(c, OsString::from("console=ttyS0"));
            }
            _ => panic!("expected path mode"),
        }
    }

    #[test]
    fn parse_accepts_verified_file_descriptor_mode() {
        let parsed = parse_args(args(&["--fds", "console=ttyS0"])).unwrap();
        match parsed {
            Inputs::Fds(cmdline) => assert_eq!(cmdline, OsString::from("console=ttyS0")),
            _ => panic!("expected fd mode"),
        }
        assert!(parse_args(args(&["--fds"])).is_err());
        assert!(parse_args(args(&["--fds", "cmdline", "extra"])).is_err());
    }

    #[test]
    fn check_maps_negative_to_errno() {
        assert_eq!(check(-2).unwrap_err().raw_os_error(), Some(2));
        assert_eq!(check(0).unwrap(), 0);
        assert_eq!(check(5).unwrap(), 5);
    }

    /// Drives `stage_with_reclaim` with scripted stage results, recording
    /// each call: `S` a stage, `D` a drop.
    fn reclaim(
        stages: &[std::io::Result<()>],
        drop_result: std::io::Result<()>,
    ) -> (std::io::Result<()>, String) {
        let log = std::cell::RefCell::new(String::new());
        let mut results = stages.iter();
        let result = stage_with_reclaim(
            || {
                log.borrow_mut().push('S');
                match results.next() {
                    Some(Ok(())) => Ok(()),
                    Some(Err(e)) => {
                        Err(std::io::Error::from_raw_os_error(e.raw_os_error().unwrap()))
                    }
                    None => panic!("staged more often than scripted"),
                }
            },
            || {
                log.borrow_mut().push('D');
                drop_result
            },
        );
        (result, log.into_inner())
    }

    fn errno(code: i32) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(code))
    }

    #[test]
    fn enomem_drops_the_page_cache_then_stages_again() {
        let (result, log) = reclaim(&[errno(ENOMEM), Ok(())], Ok(()));
        result.unwrap();
        assert_eq!(log, "SDS");
    }

    #[test]
    fn a_staged_image_drops_nothing() {
        let (result, log) = reclaim(&[Ok(())], Ok(()));
        result.unwrap();
        assert_eq!(log, "S");
    }

    /// EBUSY, EINVAL, ETXTBSY, EPERM, ENOEXEC, EFBIG: returned as they are.
    #[test]
    fn any_other_errno_is_returned_without_a_retry() {
        for code in [16, 22, 26, 1, 8, 27] {
            let (result, log) = reclaim(&[errno(code), Ok(())], Ok(()));
            assert_eq!(result.unwrap_err().raw_os_error(), Some(code));
            assert_eq!(log, "S", "errno {code}");
        }
    }

    /// One retry, not a loop: a second ENOMEM, or any other failure of the
    /// retry, is the result.
    #[test]
    fn the_retry_happens_once() {
        for code in [ENOMEM, 16] {
            let (result, log) = reclaim(&[errno(ENOMEM), errno(code), Ok(())], Ok(()));
            assert_eq!(result.unwrap_err().raw_os_error(), Some(code));
            assert_eq!(log, "SDS", "errno {code}");
        }
    }

    #[test]
    fn a_failed_drop_reports_the_enomem_and_the_drop_error() {
        let (result, log) = reclaim(
            &[errno(ENOMEM), Ok(())],
            Err(std::io::Error::from_raw_os_error(13)),
        );
        let err = result.unwrap_err();
        assert_eq!(log, "SD");
        assert_eq!(err.kind(), std::io::ErrorKind::OutOfMemory);
        let enomem = std::io::Error::from_raw_os_error(ENOMEM).to_string();
        let eacces = std::io::Error::from_raw_os_error(13).to_string();
        let text = err.to_string();
        assert!(text.starts_with(&enomem), "{text}");
        assert!(text.contains("dropping the page cache"), "{text}");
        assert!(text.ends_with(&eacces), "{text}");
    }

    #[test]
    fn cmdline_with_interior_nul_is_rejected() {
        assert!(CString::new("bad\0cmdline").is_err());
    }

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_accepts_the_key_handoff_mode() {
        let parsed = parse_args(args(&[
            "--fds-key",
            DIGEST,
            "/proc/42/fd/7",
            "console=ttyS0",
        ]))
        .unwrap();
        match parsed {
            Inputs::FdsKey {
                expected,
                key_pipe,
                record_pipe,
                cmdline,
            } => {
                assert_eq!(&expected, DIGEST.as_bytes());
                assert_eq!(key_pipe, PathBuf::from("/proc/42/fd/7"));
                assert_eq!(record_pipe, None, "no record without the flag");
                assert_eq!(cmdline, OsString::from("console=ttyS0"));
            }
            _ => panic!("expected the key handoff mode"),
        }
        assert!(parse_args(args(&["--fds-key", DIGEST, "/proc/42/fd/7"])).is_err());
        assert!(parse_args(args(&["--fds-key", DIGEST, "/proc/42/fd/7", "c", "extra"])).is_err());
    }

    #[test]
    fn parse_accepts_the_admitting_handoff_mode() {
        let parsed = parse_args(args(&[
            "--fds-key-admit",
            DIGEST,
            "/proc/42/fd/7",
            "/proc/42/fd/8",
            "console=ttyS0",
        ]))
        .unwrap();
        match parsed {
            Inputs::FdsKey {
                expected,
                key_pipe,
                record_pipe,
                cmdline,
            } => {
                assert_eq!(&expected, DIGEST.as_bytes());
                assert_eq!(key_pipe, PathBuf::from("/proc/42/fd/7"));
                assert_eq!(record_pipe, Some(PathBuf::from("/proc/42/fd/8")));
                assert_eq!(cmdline, OsString::from("console=ttyS0"));
            }
            _ => panic!("expected the admitting handoff mode"),
        }
        assert!(parse_args(args(&["--fds-key-admit", DIGEST, "/proc/42/fd/7", "c"])).is_err());
        assert!(parse_args(args(&[
            "--fds-key-admit",
            DIGEST,
            "/proc/42/fd/7",
            "/proc/42/fd/8",
            "c",
            "extra"
        ]))
        .is_err());
    }

    #[test]
    fn the_expected_digest_is_64_lowercase_hex() {
        let upper = DIGEST.to_uppercase();
        let short = &DIGEST[..63];
        let long = format!("{DIGEST}0");
        for bad in [upper.as_str(), short, long.as_str(), ""] {
            assert!(
                parse_args(args(&["--fds-key", bad, "/proc/1/fd/3", "c"])).is_err(),
                "{bad:?}"
            );
            assert!(
                parse_args(args(&[
                    "--fds-key-admit",
                    bad,
                    "/proc/1/fd/3",
                    "/proc/1/fd/4",
                    "c"
                ]))
                .is_err(),
                "{bad:?}"
            );
        }
    }

    /// The key and record operands are descriptors' NAMES: anything that is
    /// not `/proc/<decimal>/fd/<decimal>` — a key's own bytes included — is
    /// refused before anything is opened, in either position.
    #[test]
    fn the_key_never_rides_argv() {
        let hex_key = "ab".repeat(protocol::VOLUME_KEY_BYTES);
        let good = "/proc/42/fd/3";
        for bad in [
            hex_key.as_str(),
            "/proc/self/fd/3",
            "/proc/42/fd/",
            "/proc//fd/3",
            "/proc/42/fd/3/",
            "/proc/42/fd/03",
            "/proc/042/fd/3",
            "/proc/42/fdinfo/3",
            "/proc/42/fd/3x",
            "/proc/12345678901/fd/3",
            "proc/42/fd/3",
            "/tmp/key",
        ] {
            assert!(
                parse_args(args(&["--fds-key", DIGEST, bad, "c"])).is_err(),
                "{bad:?}"
            );
            for (key, record) in [(bad, good), (good, bad)] {
                assert!(
                    parse_args(args(&["--fds-key-admit", DIGEST, key, record, "c"])).is_err(),
                    "{key:?} {record:?}"
                );
            }
        }
        assert!(parse_args(args(&["--fds-key", DIGEST, "/proc/0/fd/0", "c"])).is_ok());
        assert!(parse_args(args(&[
            "--fds-key-admit",
            DIGEST,
            "/proc/0/fd/0",
            "/proc/0/fd/1",
            "c"
        ]))
        .is_ok());
    }

    /// `/proc/<this process>/fd/<fd>`, as td-boot names a pipe it holds.
    fn proc_name(fd: i32, what: &str) -> PathBuf {
        pipe_path(
            OsStr::new(&format!("/proc/{}/fd/{fd}", std::process::id())),
            what,
        )
        .unwrap()
    }

    const PAYLOADS: [(&str, usize); 2] = [
        ("volume key", protocol::VOLUME_KEY_BYTES),
        ("admission record", protocol::ADMIT_RECORD_BYTES),
    ];

    /// Reads one payload as its mode reads it.
    fn read_payload(what: &str, pipe: &mut File) -> std::io::Result<Vec<u8>> {
        if what == "volume key" {
            VolumeKey::read(pipe).map(|key| key.0.to_vec())
        } else {
            read_record(pipe).map(|record| record.to_vec())
        }
    }

    #[test]
    fn a_descriptor_that_is_not_a_pipe_is_refused() {
        let dir = std::env::temp_dir().join(format!("td-kexec-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (what, length) in PAYLOADS {
            let path = dir.join("payload");
            std::fs::write(&path, vec![7u8; length]).unwrap();
            let file = File::open(&path).unwrap();
            let err = open_pipe(&proc_name(file.as_raw_fd(), what), what).unwrap_err();
            assert!(err.to_string().contains("not a pipe"), "{err}");
            assert!(err.to_string().contains(what), "{err}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pipe_is_opened_anew_by_its_proc_name() {
        for (what, length) in PAYLOADS {
            let (reader, mut writer) = std::io::pipe().unwrap();
            writer.write_all(&vec![9u8; length]).unwrap();
            drop(writer);
            let mut pipe = open_pipe(&proc_name(reader.as_raw_fd(), what), what).unwrap();
            assert_eq!(read_payload(what, &mut pipe).unwrap(), vec![9u8; length]);
        }
    }

    /// A write end left open is refused, not waited on: empty, short, and
    /// whole but not yet closed, for the key and the record alike.
    #[test]
    fn a_pipe_with_a_write_end_open_is_refused_without_waiting() {
        for (what, whole) in PAYLOADS {
            for length in [0usize, 10, whole] {
                let (reader, mut writer) = std::io::pipe().unwrap();
                writer.write_all(&vec![9u8; length]).unwrap();
                let mut pipe = open_pipe(&proc_name(reader.as_raw_fd(), what), what).unwrap();
                let err = read_payload(what, &mut pipe).err().unwrap();
                assert!(
                    err.to_string().contains("write end open"),
                    "{what} {length}: {err}"
                );
                assert!(err.to_string().contains(what), "{err}");
                drop(writer);
            }
        }
    }

    #[test]
    fn a_command_line_that_retains_the_initramfs_is_refused() {
        for bad in [
            "retain_initrd",
            "console=ttyS0 retain_initrd quiet",
            "retain-initrd",
            "\"retain_initrd\"",
            "retain_initrd=1",
            "keepinitrd",
            "a\tkeepinitrd",
            "console=ttyS0\x0bretain_initrd",
            "console=ttyS0\x0cretain_initrd",
        ] {
            assert!(refuse_initrd_retention(OsStr::new(bad)).is_err(), "{bad:?}");
        }
        // 0xA0 is a kernel separator, though not UTF-8 on its own.
        let nbsp = OsStr::from_bytes(b"console=ttyS0\xa0retain_initrd");
        assert!(refuse_initrd_retention(nbsp).is_err());
        for good in [
            "",
            "console=ttyS0 td.deployment=x",
            "retain_initrdx",
            "x.retain_initrd",
            "td.note=retain_initrd",
        ] {
            assert!(
                refuse_initrd_retention(OsStr::new(good)).is_ok(),
                "{good:?}"
            );
        }
    }

    #[test]
    fn the_key_is_exactly_64_bytes_then_end_of_file() {
        assert_eq!(
            &*VolumeKey::read(&mut &[1u8; 64][..]).unwrap().0,
            &[1u8; 64][..]
        );
        for length in [0usize, 1, 63] {
            let err = VolumeKey::read(&mut &vec![1u8; length][..]).err().unwrap();
            assert!(err.to_string().contains("ended before 64"), "{err}");
        }
        for length in [65usize, 128] {
            let err = VolumeKey::read(&mut &vec![1u8; length][..]).err().unwrap();
            assert!(err.to_string().contains("longer than 64"), "{err}");
            assert!(!err.to_string().contains('1'), "{err}");
        }
    }

    #[test]
    fn zero_clears_the_key_in_place() {
        let mut key = VolumeKey::read(&mut &[0x5au8; 64][..]).unwrap();
        let at = key.0.as_ptr();
        zero(&mut key.0);
        assert!(key.0.iter().all(|byte| *byte == 0));
        assert_eq!(key.0.as_ptr(), at);
    }

    /// The v1 contract's literals, held here so that a change to td-boot's
    /// shared constants reds the side that writes them.
    #[test]
    fn the_v1_member_and_length_are_pinned() {
        assert_eq!(protocol::VOLUME_KEY_MEMBER, "td-volume-key-v1");
        assert_eq!(protocol::VOLUME_KEY_BYTES, 64);
        assert_eq!(VOLUME_KEY_MODE, 0o400);
        assert_eq!(protocol::ADMIT_MEMBER, "td-admit-v1");
        assert_eq!(protocol::ADMIT_RECORD_BYTES, 46);
        assert_eq!(ADMIT_RECORD_MODE, 0o400);
    }

    fn hex_field(archive: &[u8], header_at: usize, index: usize) -> u32 {
        let start = header_at + 6 + index * 8;
        u32::from_str_radix(std::str::from_utf8(&archive[start..start + 8]).unwrap(), 16).unwrap()
    }

    /// Parses the appended archive independently of the writer: one regular
    /// file, the v1 name, 64 bytes of key, then the trailer, each entry at a
    /// 4-aligned offset.
    fn assert_key_archive(archive: &[u8], key: &[u8]) {
        assert_eq!(&archive[..6], b"070701");
        assert_eq!(hex_field(archive, 0, 1), 0o100400, "regular file, 0400");
        assert_eq!(hex_field(archive, 0, 2), 0, "uid");
        assert_eq!(hex_field(archive, 0, 3), 0, "gid");
        assert_eq!(hex_field(archive, 0, 4), 1, "nlink");
        assert_eq!(hex_field(archive, 0, 6), 64, "filesize");
        assert_eq!(hex_field(archive, 0, 11), 17, "namesize with its NUL");
        assert_eq!(hex_field(archive, 0, 12), 0, "chksum");
        assert_eq!(&archive[110..127], b"td-volume-key-v1\0");
        // 110 + 17 = 127, padded to 128.
        assert_eq!(archive[127], 0);
        assert_eq!(&archive[128..192], key);
        // 192 is 4-aligned: the trailer follows directly.
        let trailer = 192;
        assert_eq!(&archive[trailer..trailer + 6], b"070701");
        assert_eq!(hex_field(archive, trailer, 4), 1, "trailer nlink");
        assert_eq!(hex_field(archive, trailer, 6), 0, "trailer filesize");
        assert_eq!(hex_field(archive, trailer, 11), 11, "trailer namesize");
        assert_eq!(&archive[trailer + 110..trailer + 121], b"TRAILER!!!\0");
        assert_eq!(archive.len(), trailer + 124, "trailer name padded to 4");
        assert!(archive[trailer + 121..].iter().all(|byte| *byte == 0));
    }

    /// The same independent parse of `--fds-key-admit`'s archive: the key
    /// member exactly as above, then the record's member, a regular root
    /// file of mode 0400 named `td-admit-v1` holding exactly the record's
    /// 46 bytes, then the trailer.
    fn assert_admit_archive(archive: &[u8], key: &[u8], record: &[u8]) {
        assert_eq!(record.len(), 46);
        // The key member, byte for byte as `--fds-key` writes it.
        let (key_only, _) = key_archive(None).unwrap();
        assert_eq!(&archive[..128], &key_only[..128]);
        assert_eq!(&archive[128..192], key);
        let member = 192;
        assert_eq!(&archive[member..member + 6], b"070701");
        assert_eq!(
            hex_field(archive, member, 1),
            0o100400,
            "regular file, 0400"
        );
        assert_eq!(hex_field(archive, member, 2), 0, "uid");
        assert_eq!(hex_field(archive, member, 3), 0, "gid");
        assert_eq!(hex_field(archive, member, 4), 1, "nlink");
        assert_eq!(hex_field(archive, member, 6), 46, "filesize");
        assert_eq!(hex_field(archive, member, 11), 12, "namesize with its NUL");
        assert_eq!(hex_field(archive, member, 12), 0, "chksum");
        assert_eq!(&archive[member + 110..member + 122], b"td-admit-v1\0");
        // 192 + 110 + 12 = 314, padded to 316.
        assert_eq!(&archive[314..316], &[0, 0]);
        assert_eq!(&archive[316..362], record);
        // 362 padded to 364, then the trailer.
        assert_eq!(&archive[362..364], &[0, 0]);
        let trailer = 364;
        assert_eq!(&archive[trailer..trailer + 6], b"070701");
        assert_eq!(hex_field(archive, trailer, 4), 1, "trailer nlink");
        assert_eq!(hex_field(archive, trailer, 6), 0, "trailer filesize");
        assert_eq!(hex_field(archive, trailer, 11), 11, "trailer namesize");
        assert_eq!(&archive[trailer + 110..trailer + 121], b"TRAILER!!!\0");
        assert_eq!(archive.len(), trailer + 124, "trailer name padded to 4");
        assert!(archive[trailer + 121..].iter().all(|byte| *byte == 0));
    }

    /// A 46-byte record whose every byte differs from its neighbours, as
    /// td-boot's would: `TDADMIT1`, version, UID, method, manifest ID.
    fn record_bytes() -> [u8; 46] {
        let mut record = [0u8; 46];
        record[..8].copy_from_slice(b"TDADMIT1");
        record[8] = 1;
        record[9..13].copy_from_slice(&1000u32.to_be_bytes());
        record[13] = 1;
        for (index, byte) in record[14..].iter_mut().enumerate() {
            *byte = 0xa0 ^ index as u8;
        }
        record
    }

    #[test]
    fn the_key_archive_layout_is_newc_with_the_key_at_128() {
        let (archive, data_end) = key_archive(None).unwrap();
        assert_eq!(data_end, 192);
        assert_key_archive(&archive, &[0u8; 64]);
    }

    /// The record follows the key; the key's place does not move.
    #[test]
    fn the_admitting_archive_puts_the_record_after_the_key() {
        let record = record_bytes();
        let (archive, data_end) = key_archive(Some(&record)).unwrap();
        assert_eq!(data_end, 192);
        assert_admit_archive(&archive, &[0u8; 64], &record);
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "td-kexec-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }

        fn initramfs(&self, bytes: &[u8]) -> File {
            let path = self.dir.join("initramfs.cpio");
            std::fs::write(&path, bytes).unwrap();
            File::open(&path).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn digest_of(bytes: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out.copy_from_slice(sha256::hex_digest(bytes).as_bytes());
        out
    }

    fn memfd_bytes(memfd: i32) -> Vec<u8> {
        std::fs::read(format!("/proc/self/fd/{memfd}")).unwrap()
    }

    /// The real memfd_create and F_ADD_SEALS, which need no privilege; only
    /// kexec_file_load is left to the boot oracle. Every length class of the
    /// 4-byte join is covered, including an empty initramfs.
    #[test]
    fn the_handoff_is_the_copy_padded_to_4_then_the_key_archive() {
        let key: Vec<u8> = (0u8..64).map(|byte| byte.wrapping_mul(7)).collect();
        for length in [0usize, 1, 2, 3, 4, 5, 511, 512, COPY_CHUNK + 3] {
            let fixture = Fixture::new("layout");
            let body: Vec<u8> = (0..length).map(|i| (i % 251) as u8 | 1).collect();
            let initramfs = fixture.initramfs(&body);
            let memfd =
                prepare_handoff(&initramfs, &digest_of(&body), &mut &key[..], None).unwrap();
            let bytes = memfd_bytes(memfd);
            let padded = length.div_ceil(4) * 4;
            assert_eq!(&bytes[..length], &body[..], "length {length}");
            assert!(bytes[length..padded].iter().all(|byte| *byte == 0));
            assert_eq!(padded % 4, 0);
            assert_key_archive(&bytes[padded..], &key);
        }
    }

    /// `--fds-key-admit`'s handoff: the same copy and join, then the key's
    /// member and the record's exact 46 bytes after it, sealed as the key's
    /// handoff is.
    #[test]
    fn the_admitting_handoff_carries_the_record_after_the_key() {
        let key: Vec<u8> = (0u8..64).map(|byte| byte.wrapping_mul(7)).collect();
        let record = record_bytes();
        for length in [0usize, 1, 2, 3, 4, 5, 511, COPY_CHUNK + 3] {
            let fixture = Fixture::new("admit");
            let body: Vec<u8> = (0..length).map(|i| (i % 251) as u8 | 1).collect();
            let initramfs = fixture.initramfs(&body);
            let memfd = prepare_handoff(
                &initramfs,
                &digest_of(&body),
                &mut &key[..],
                Some(&mut &record[..]),
            )
            .unwrap();
            let bytes = memfd_bytes(memfd);
            let padded = length.div_ceil(4) * 4;
            assert_eq!(&bytes[..length], &body[..], "length {length}");
            assert!(bytes[length..padded].iter().all(|byte| *byte == 0));
            assert_admit_archive(&bytes[padded..], &key, &record);
            assert_eq!(
                add_seals(memfd).unwrap_err().raw_os_error(),
                Some(1),
                "sealed"
            );
        }
    }

    /// The record is refused under the key's rules: short, long or empty.
    #[test]
    fn a_record_of_the_wrong_length_refuses_the_handoff() {
        let fixture = Fixture::new("record-length");
        let body = b"abc".to_vec();
        let initramfs = fixture.initramfs(&body);
        for length in [0usize, 45, 47, 92] {
            let record = vec![1u8; length];
            let err = prepare_handoff(
                &initramfs,
                &digest_of(&body),
                &mut &[1u8; 64][..],
                Some(&mut &record[..]),
            )
            .err()
            .unwrap();
            assert!(err.to_string().contains("admission record"), "{err}");
        }
    }

    /// F_SEAL_WRITE, F_SEAL_GROW and F_SEAL_SHRINK each refuse their
    /// operation, and F_SEAL_SEAL refuses any further seal.
    #[test]
    fn the_handoff_memfd_is_sealed_and_unlinked() {
        let fixture = Fixture::new("seals");
        let body = b"0707010000".to_vec();
        let initramfs = fixture.initramfs(&body);
        let memfd =
            prepare_handoff(&initramfs, &digest_of(&body), &mut &[3u8; 64][..], None).unwrap();
        let link = std::fs::read_link(format!("/proc/self/fd/{memfd}")).unwrap();
        assert_eq!(
            link.as_os_str().as_bytes(),
            b"/memfd:td-kexec-handoff (deleted)"
        );
        let length = memfd_bytes(memfd).len() as u64;
        let writer = OpenOptions::new()
            .write(true)
            .open(format!("/proc/self/fd/{memfd}"));
        match writer {
            Ok(mut writer) => {
                // Each operation tests one seal: an in-bounds write only
                // F_SEAL_WRITE, a truncation each size seal.
                assert_eq!(
                    writer.write_all(b"x").unwrap_err().raw_os_error(),
                    Some(1),
                    "F_SEAL_WRITE"
                );
                assert_eq!(
                    writer.set_len(length + 4096).unwrap_err().raw_os_error(),
                    Some(1),
                    "F_SEAL_GROW"
                );
                assert_eq!(
                    writer.set_len(1).unwrap_err().raw_os_error(),
                    Some(1),
                    "F_SEAL_SHRINK"
                );
            }
            Err(e) => assert_eq!(e.raw_os_error(), Some(1), "{e}"),
        }
        assert_eq!(
            add_seals(memfd).unwrap_err().raw_os_error(),
            Some(1),
            "F_SEAL_SEAL: EPERM"
        );
        assert_eq!(memfd_bytes(memfd).len() as u64, length);
    }

    // fcntl(2) F_SETLEASE and F_RDLCK, test-only (UNSAFE.md §1): a read
    // lease is refused with EAGAIN exactly when the inode's i_writecount is
    // positive (fs/locks.c `check_conflicting_open`), the counter
    // kexec_file_load's `deny_write_access` refuses on with ETXTBSY. Exec
    // cannot probe it: MFD_NOEXEC_SEAL refuses exec with EACCES first.
    const F_SETLEASE: usize = 1024;
    const F_RDLCK: usize = 0;

    /// Whether a read lease can be taken on a fresh read-only open of the
    /// memfd, released at once by closing that open.
    fn read_lease(memfd: i32) -> std::io::Result<()> {
        let reader = File::open(format!("/proc/self/fd/{memfd}"))?;
        check(syscall5(
            SYS_FCNTL,
            reader.as_raw_fd() as usize,
            F_SETLEASE,
            F_RDLCK,
            0,
            0,
        ))
        .map(|_| ())
    }

    /// False, with a note, where leases are disabled (fs.leases-enable=0
    /// makes F_SETLEASE fail with EINVAL): the probe cannot run there.
    fn leases_enabled(memfd: i32) -> bool {
        match read_lease(memfd) {
            Err(e) if e.raw_os_error() == Some(22) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "note: file leases are disabled here (EINVAL); skipping the write-count probe"
                );
                false
            }
            _ => true,
        }
    }

    /// The probe discriminates: a writer held open through /proc refuses
    /// the lease, and memfd_create's own descriptor does not.
    #[test]
    fn the_write_count_probe_sees_a_proc_writer_and_not_the_memfd() {
        let memfd = memfd_create().unwrap();
        if !leases_enabled(memfd) {
            return;
        }
        let writer = OpenOptions::new()
            .write(true)
            .open(format!("/proc/self/fd/{memfd}"))
            .unwrap();
        assert_eq!(read_lease(memfd).unwrap_err().raw_os_error(), Some(11));
        drop(writer);
        read_lease(memfd).unwrap();
    }

    /// What kexec_file_load needs: no writer counts against the sealed
    /// memfd when prepare_handoff returns, so deny_write_access admits it.
    #[test]
    fn the_handoff_memfd_has_no_writer_left_for_kexec_to_refuse() {
        let fixture = Fixture::new("writers");
        let body = b"initramfs".to_vec();
        let initramfs = fixture.initramfs(&body);
        let memfd =
            prepare_handoff(&initramfs, &digest_of(&body), &mut &[4u8; 64][..], None).unwrap();
        if !leases_enabled(memfd) {
            return;
        }
        read_lease(memfd).unwrap();
    }

    struct Untouchable;

    impl Read for Untouchable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("the key or record was read before the copy verified");
        }
    }

    /// Neither pipe is read before the copy verifies.
    #[test]
    fn a_copy_that_does_not_hash_to_the_verified_digest_is_refused_unread() {
        let fixture = Fixture::new("digest");
        let initramfs = fixture.initramfs(b"the deployment initramfs");
        let err = prepare_handoff(&initramfs, &digest_of(b"another"), &mut Untouchable, None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("no volume key was read"), "{err}");
        let err = prepare_handoff(
            &initramfs,
            &digest_of(b"another"),
            &mut Untouchable,
            Some(&mut Untouchable),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("no volume key was read"), "{err}");
    }

    #[test]
    fn a_key_of_the_wrong_length_refuses_the_handoff() {
        let fixture = Fixture::new("short");
        let body = b"abc".to_vec();
        let initramfs = fixture.initramfs(&body);
        assert!(prepare_handoff(&initramfs, &digest_of(&body), &mut &[1u8; 63][..], None).is_err());
        assert!(prepare_handoff(&initramfs, &digest_of(&body), &mut &[1u8; 65][..], None).is_err());
    }

    #[test]
    fn a_non_regular_initramfs_is_refused() {
        let fixture = Fixture::new("dir");
        let dir = File::open(&fixture.dir).unwrap();
        assert!(prepare_handoff(&dir, &digest_of(b""), &mut Untouchable, None).is_err());
    }

    /// The UNSAFE.md §1 contract the compiler cannot express: which requests
    /// reach the one instruction, with which pinned values.
    mod confinement {
        use super::super::*;

        const SOURCE: &str = include_str!("main.rs");

        /// Production source with line comments removed. No string literal in
        /// the production half spells `//`, which the first test holds.
        fn production() -> String {
            let shipped = SOURCE
                .split_once("\n#[cfg(test)]\nmod tests {")
                .map(|(head, _)| head)
                .unwrap();
            shipped
                .lines()
                .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[test]
        fn the_production_half_spells_no_double_slash_in_a_literal() {
            let shipped = SOURCE.split_once("\n#[cfg(test)]\nmod tests {").unwrap().0;
            for line in shipped.lines() {
                if let Some((code, _)) = line.split_once("//") {
                    assert_eq!(
                        code.matches('"').count() % 2,
                        0,
                        "a `//` inside a string literal: {line}"
                    );
                }
            }
        }

        #[test]
        fn one_unsafe_block_under_one_scoped_allow() {
            let code = production();
            assert_eq!(code.matches("unsafe {").count(), 1);
            assert_eq!(code.matches("unsafe").count(), 3, "{code}");
            assert_eq!(code.matches("#[allow(unsafe_code)]").count(), 1);
            assert!(code.contains("#![deny(unsafe_code)]"));
            assert!(code.contains(
                "#[allow(unsafe_code)]\nfn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {"
            ));
        }

        #[test]
        fn exactly_four_requests_reach_the_instruction() {
            let code = production();
            // Each call's argument list, up to its balancing parenthesis,
            // whitespace-normalized. The first match is the definition.
            let calls: Vec<String> = code
                .split("syscall5(")
                .skip(2)
                .map(|call| {
                    let mut depth = 1usize;
                    let end = call
                        .char_indices()
                        .find(|(_, c)| {
                            match c {
                                '(' => depth += 1,
                                ')' => depth -= 1,
                                _ => {}
                            }
                            depth == 0
                        })
                        .unwrap()
                        .0;
                    call[..end].split_whitespace().collect::<Vec<_>>().join(" ")
                })
                .collect();
            assert_eq!(
                calls,
                [
                    "SYS_KEXEC_FILE_LOAD, kernel_fd as usize, initrd_fd as usize, \
                     cmdline_bytes.len(), cmdline_bytes.as_ptr() as usize, flags,",
                    "SYS_REBOOT, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, \
                     LINUX_REBOOT_CMD_KEXEC, 0, 0,",
                    "SYS_MEMFD_CREATE, MEMFD_NAME.as_ptr() as usize, MFD_FLAGS, 0, 0, 0,",
                    "SYS_FCNTL, fd as usize, F_ADD_SEALS, SEALS, 0, 0",
                ]
            );
        }

        #[test]
        fn the_pinned_values_are_the_kernel_abi() {
            assert_eq!(SYS_FCNTL, 72);
            assert_eq!(SYS_REBOOT, 169);
            assert_eq!(SYS_MEMFD_CREATE, 319);
            assert_eq!(SYS_KEXEC_FILE_LOAD, 320);
            assert_eq!(
                MFD_FLAGS, 0xb,
                "MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL"
            );
            assert_eq!(O_NONBLOCK, 0o4000);
            assert_eq!(F_ADD_SEALS, 1033);
            assert_eq!(SEALS, 0xf, "SEAL | SHRINK | GROW | WRITE");
            assert_eq!(MEMFD_NAME.to_bytes(), b"td-kexec-handoff");
            assert_eq!(MAX_HANDOFF_BYTES, 4 * 1024 * 1024 * 1024);
            assert_eq!(ENOMEM, 12);
            assert_eq!(DROP_CACHES, "/proc/sys/vm/drop_caches");
            assert_eq!(DROP_CLEAN_PAGE_CACHE, b"1\n", "page cache only");
        }

        /// The page-cache drop is the reclaim seam's only drop, reached only
        /// through the one kexec_file_load call site, and writes nothing but
        /// the pinned value to the pinned file.
        #[test]
        fn the_one_cache_drop_serves_the_one_kexec_file_load() {
            let code = production();
            assert_eq!(code.matches("drop_clean_page_cache").count(), 2);
            assert_eq!(code.matches("stage_with_reclaim(").count(), 2);
            // `stage` is the closure around the one kexec_file_load request
            // (`exactly_four_requests_reach_the_instruction`), and nothing else.
            assert_eq!(code.matches("let stage").count(), 1);
            assert!(code.contains(
                "    let stage = || {\n        check(syscall5(\n            \
                 SYS_KEXEC_FILE_LOAD,\n            kernel_fd as usize,\n            \
                 initrd_fd as usize,\n            cmdline_bytes.len(),\n            \
                 cmdline_bytes.as_ptr() as usize,\n            flags,\n        ))\n        \
                 .map(|_| ())\n    };\n    \
                 stage_with_reclaim(stage, drop_clean_page_cache)?;\n"
            ));
            assert_eq!(code.matches("DROP_CACHES").count(), 2);
            assert_eq!(code.matches("DROP_CLEAN_PAGE_CACHE").count(), 2);
            assert!(code.contains(
                "        .open(DROP_CACHES)?\n        .write_all(DROP_CLEAN_PAGE_CACHE)?;\n"
            ));
            assert_eq!(code.matches("/proc/sys/").count(), 1);
        }

        /// The seals go on the memfd and nothing else, once, after its
        /// writer closed; the cmdline check precedes both pipes' opens, and
        /// the one non-blocking open serves the key and the record.
        #[test]
        fn seals_only_the_memfd_and_checks_the_cmdline_first() {
            let code = production();
            assert_eq!(code.matches("add_seals(").count(), 2);
            assert!(code.contains("    drop(copy);\n    add_seals(memfd)?;\n    Ok(memfd)\n"));
            assert!(code.contains(
                "    refuse_initrd_retention(cmdline)?;\n    \
                 let mut key = open_pipe(key_pipe, \"volume key\")?;\n    \
                 let mut record = record_pipe\n        \
                 .map(|path| open_pipe(path, \"admission record\"))\n"
            ));
            assert_eq!(code.matches("open_pipe(").count(), 3);
            assert_eq!(code.matches("custom_flags(").count(), 1);
            assert!(code.contains(".custom_flags(O_NONBLOCK)"));
        }

        /// The whole file, tests included, has five call sites: the four
        /// above and the test-only write-count probe, whose request is
        /// pinned here.
        #[test]
        fn the_one_test_only_request_is_the_lease_probe() {
            // Up to this module, whose own literals spell the name.
            let (code, _) = SOURCE.split_once("\n    mod confinement {").unwrap();
            let calls: Vec<&str> = code.split("syscall5(").skip(2).collect();
            assert_eq!(calls.len(), 5);
            let probe = calls
                .last()
                .unwrap()
                .split_once("))")
                .unwrap()
                .0
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                probe,
                "SYS_FCNTL, reader.as_raw_fd() as usize, F_SETLEASE, F_RDLCK, 0, 0,"
            );
            assert_eq!(super::F_SETLEASE, 1024);
            assert_eq!(super::F_RDLCK, 0);
        }

        /// No descriptor is adopted (a second scoped `unsafe`), and nothing
        /// reads the environment: the key arrives only by descriptor.
        #[test]
        fn no_adoption_and_no_environment() {
            let code = production();
            for banned in [
                "from_raw_fd",
                "borrow_raw",
                "FromRawFd",
                "env::var",
                "vars_os",
                "var_os",
            ] {
                assert!(!code.contains(banned), "{banned}");
            }
            assert_eq!(code.matches("std::env::").count(), 1);
            assert!(code.contains("std::env::args_os()"));
        }
    }
}
