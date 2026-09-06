use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;

const SYS_CLOSE: usize = 3;
const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_IOCTL: usize = 16;
const SYS_SENDMSG: usize = 46;
const SYS_RECVMSG: usize = 47;
const SYS_GETSOCKOPT: usize = 55;
const SYS_FCNTL: usize = 72;

/// The only `fcntl(2)` commands this crate may issue. They temporarily make
/// one clipboard destination nonblocking so a receiver cannot park td-term's
/// sole bounded writer forever.
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const O_NONBLOCK: usize = 0o4000;

/// The only `ioctl(2)` requests this crate may issue, pinned by value. A request
/// number encodes direction, argument size, and target driver, so a wrong one is
/// a different kernel operation performed on whatever argument is at hand.
const TIOCSPTLCK: usize = 0x4004_5431;
const TIOCGPTPEER: usize = 0x5441;
const TIOCSWINSZ: usize = 0x5414;
const TIOCGWINSZ: usize = 0x5413;
/// `EVIOCGABS(ABS_X)` and `EVIOCGABS(ABS_Y)`. The size field of an evdev
/// request encodes `sizeof(struct input_absinfo)`, so these two numbers assert
/// the 24-byte buffer below as much as `ABSINFO_WORDS` does.
const EVIOCGABS_X: usize = 0x8018_4540;
const EVIOCGABS_Y: usize = 0x8018_4541;

/// The DRM/KMS requests this crate may issue, from Linux 7.1.4's
/// `include/uapi/drm/drm.h`. `_IOC` packs the argument's SIZE into bits 16..30,
/// so each number states the layout of the struct it is used with — the
/// `EVIOCGABS` lesson one driver over, and `the_drm_requests_encode_the_structs_they_carry`
/// is what makes that a test rather than a remark.
///
/// These four READ. None of them modesets or allocates: discovery is allowed to
/// look at a card `fbcon` is currently driving, which is why the probe hands
/// mastership back the instant the open takes it.
const DRM_IOCTL_VERSION: usize = 0xc040_6400;
/// `DRM_IOCTL_DROP_MASTER`. Not a write to the display: it RELEASES authority
/// this process was given without asking for it.
///
/// `drm_master_open` in `drivers/gpu/drm/drm_auth.c` makes the first opener of
/// a PRIMARY node the DRM master whenever `dev->master` is NULL, and an
/// in-kernel client — fbcon, fbdev emulation — never sets it. So merely opening
/// `/dev/dri/card0` on a td image takes mastership, `SET_MASTER` or no
/// `SET_MASTER`.
///
/// What that costs is narrower than an earlier revision of this comment said,
/// and the correction is worth carrying because the wrong version is the
/// plausible one. Holding mastership does NOT stop the framebuffer console
/// painting: `drm_fb_helper_fb_dirty` asks
/// `drm_client_modeset_wait_for_vblank` for a vblank, that call answers
/// `-EBUSY` while a foreign master exists (`drm_client_modeset.c:1320`), and
/// the helper DISCARDS the result (`drm_fb_helper.c:237`) and commits the
/// damage anyway (`:249`). The console loses its vblank rate-limit and nothing
/// else.
///
/// The real costs are two. Another process's `SET_MASTER` is refused outright
/// — `if (dev->master) return -EBUSY;` (`drm_auth.c:260`) — so a genuine DRM
/// compositor cannot take the card while this descriptor holds it. And two
/// fbdev ioctls, `drm_fb_helper_setcmap` (`:863`) and
/// `drm_fb_helper_pan_display` (`:1247`), fail for as long as it is held.
///
/// Dropping it immediately is what makes "this probe does not disturb what is
/// on the screen" a property of the code rather than a hope. An earlier
/// revision asserted the absence of this request as PROOF of not being master,
/// which had it exactly backwards: it pinned that the code could not give back
/// what the open had already taken.
const DRM_IOCTL_DROP_MASTER: usize = 0x0000_641f;
const DRM_IOCTL_MODE_GETRESOURCES: usize = 0xc040_64a0;
const DRM_IOCTL_MODE_GETENCODER: usize = 0xc014_64a6;
const DRM_IOCTL_MODE_GETCONNECTOR: usize = 0xc050_64a7;

/// The dumb-buffer trio. These ALLOCATE, which the four above do not.
///
/// They are the whole of what `APPLICATIONS.md` §M calls the honest toll: a
/// dumb buffer has no `write(2)` path, so pixels reach a card only through a
/// mapping of its descriptor, and refusing that forever is the one way td could
/// paint itself out of hardware rendering.
///
/// Sizes are the kernel's own structs at the 7.1.4 pin — `drm_mode_create_dumb`
/// is 32 bytes (six `__u32` then a `__u64`), `drm_mode_map_dumb` 16 and
/// `drm_mode_destroy_dumb` 4 — and `the_drm_requests_encode_the_structs_they_carry`
/// checks each number against the Rust type actually passed to it, which is
/// what catches a hand-derived request that disagrees with its payload.
///
/// Still absent, and named absent by the confinement test: `MODE_ADDFB2`,
/// `MODE_SETCRTC`, `MODE_PAGE_FLIP` and `MODE_ATOMIC`. Allocating a buffer and
/// mapping it changes nothing on screen; those four are what put pixels on
/// glass, and they arrive with the backend that has somewhere to put them.
const DRM_IOCTL_MODE_CREATE_DUMB: usize = 0xc020_64b2;
const DRM_IOCTL_MODE_MAP_DUMB: usize = 0xc010_64b3;
const DRM_IOCTL_MODE_DESTROY_DUMB: usize = 0xc004_64b4;

/// The modeset five. These are the ones that CHANGE WHAT IS ON SCREEN, and
/// every earlier increment named them absent precisely so this one has to
/// arrive by amendment rather than by sharing a module with requests that only
/// read.
///
/// `SET_MASTER` (0x641e) is the authority to do any of it. Opening a primary
/// node already grants mastership when `dev->master` is NULL, and `open_card`
/// gives it straight back; this RE-TAKES it deliberately, for a bounded window,
/// and the difference between those two is the whole reason `DROP_MASTER`
/// landed on the roster two increments before its opposite.
///
/// `MODE_GETCRTC`/`MODE_SETCRTC` (0xc06864a1/0xc06864a2) read and write one
/// CRTC's mode, framebuffer and connector set; both carry `drm_mode_crtc`,
/// which is 104 bytes because it embeds a whole 68-byte `drm_mode_modeinfo`.
/// `MODE_ADDFB2` (0xc06864b8) registers a buffer as a scanout framebuffer and
/// `MODE_RMFB` (0xc00464af) unregisters one; `RMFB` takes a bare `unsigned int`,
/// which is why its size field is 4 and not the struct the others carry.
///
/// `MODE_PAGE_FLIP` and `MODE_ATOMIC` remain ABSENT. A modeset puts a picture
/// on a screen once; a page flip is what makes a SEQUENCE of them a display,
/// and it needs a completion path this increment does not build.
const DRM_IOCTL_SET_MASTER: usize = 0x0000_641e;
const DRM_IOCTL_MODE_GETCRTC: usize = 0xc068_64a1;
const DRM_IOCTL_MODE_SETCRTC: usize = 0xc068_64a2;
const DRM_IOCTL_MODE_RMFB: usize = 0xc004_64af;
const DRM_IOCTL_MODE_ADDFB2: usize = 0xc068_64b8;
const DRM_IOCTL_MODE_PAGE_FLIP: usize = 0xc018_64b0;

/// Ask the kernel to send a completion event when the flip takes effect.
///
/// The flip is asynchronous either way; this flag is what makes it
/// OBSERVABLE. Without it a caller has queued a frame and has no moment at
/// which it may say the frame is on glass, which is the distinction
/// `Submission` exists to keep.
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;

/// The event type a completed page flip arrives as, on the card descriptor.
///
/// `DRM_EVENT_VBLANK` (0x01) shares the same 32-byte payload and is NOT this:
/// a vblank says the scanout passed a boundary, a flip completion says the
/// frame this process queued is the one being scanned out. Matching on the
/// type is what keeps the two apart.
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

/// `DRM_FORMAT_XRGB8888`, the one pixel format this compositor scans out.
///
/// The same four characters `Fourcc` carries for the Wayland side, spelled here
/// as the `__u32` `drm_mode_fb_cmd2` wants: 'X','R','2','4' little-endian.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// `PROT_READ | PROT_WRITE` and `MAP_SHARED`, the only protection and
/// visibility a scanout mapping may ask for. Shared because the entire point is
/// that the device sees the writes; readable as well as writable because a
/// partial-damage repaint reads back the pixels it is not replacing.
const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const MAP_SHARED: usize = 0x1;

/// Bits per pixel for the one format this compositor scans out, `XRGB8888`.
const DUMB_BUFFER_BPP: u32 = 32;

/// The largest dumb buffer this compositor will map, in bytes.
///
/// The kernel reports the size and it is then handed to `mmap` as a LENGTH, so
/// an implausible one is the `MAX_DRM_OBJECTS` problem a second time: a driver
/// sizing this process's address space. 256 MiB is far past a 4K XRGB scanout
/// (about 33 MiB) and far short of anything that could pass for a leak.
const MAX_DUMB_BUFFER_BYTES: u64 = 256 * 1024 * 1024;

/// `enum drm_connector_status` from `include/drm/drm_connector.h`. The numbers
/// are the kernel's, and `unknown` is NOT a synonym for disconnected: it means
/// probing would flicker or a resource was busy, and the kernel's own advice is
/// to light such a connector only when nothing reports `connected`.
pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;
pub const DRM_MODE_UNKNOWNCONNECTION: u32 = 3;

/// `DRM_MODE_TYPE_PREFERRED`, the bit a driver sets on the mode it would rather
/// be asked for — for a virtual sink, the one the host window is sized to.
pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;

/// `DRM_DISPLAY_MODE_LEN`. The kernel writes a NUL-padded name into exactly
/// this many bytes and does not promise a terminator in the last one.
const DRM_DISPLAY_MODE_LEN: usize = 32;

/// Ceilings on what a card may claim to have, applied before anything is
/// allocated from a kernel-supplied count.
///
/// A count is read from the device and then used as an allocation length, so an
/// implausible one is a memory-exhaustion request from a driver this process
/// does not otherwise trust to size its heap. Neither bound is a hardware
/// limit: 64 objects of a kind and 256 modes are far past anything a virtio-gpu
/// or a real card reports, and a card that exceeds one gets a diagnostic rather
/// than a silently truncated view of itself.
const MAX_DRM_OBJECTS: u32 = 64;
const MAX_DRM_MODES: u32 = 256;

/// How many times a DRM ioctl is re-issued after a restart or a would-block.
///
/// `drm_ioctl` takes `mutex_lock_interruptible` on the mode-config lock, so
/// EINTR and EAGAIN are ordinary answers rather than failures — this is why
/// libdrm's own `drmIoctl` is a loop and not a call. Bounded rather than
/// `loop`: a device answering EAGAIN forever is a broken device, and a
/// compositor that hangs in discovery never gets to say so.
///
/// NOT named `DRM_IOCTL_*`: the confinement test counts that prefix to pin how
/// many DRM REQUEST NUMBERS this module carries, and a retry budget answering
/// to the same prefix would have inflated that count by one.
const DRM_RESTART_ATTEMPTS: usize = 16;

/// `O_NOCTTY` — the terminal td-term creates belongs to the child it starts, so
/// neither the peer descriptor nor its duplicate may acquire it by side effect.
const O_NOCTTY: i32 = 0o400;

/// `O_RDWR | O_NOCTTY | O_CLOEXEC`, the flags `TIOCGPTPEER` opens the slave
/// with. These are the x86-64 ABI values, as the syscall numbers above are;
/// `O_CLOEXEC` in particular differs on Alpha and SPARC.
const PTY_PEER_FLAGS: usize = 0o2 | 0o400 | 0o2_000_000;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SO_PEERCRED: i32 = 17;
const MSG_CTRUNC: i32 = 0x08;
const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;
const ERRNO_EINTR: isize = -4;
#[cfg(test)]
const ERRNO_EBADF: isize = -9;
const ERRNO_EAGAIN: isize = -11;
#[cfg(test)]
const ERRNO_ECONNABORTED: isize = -103;
#[cfg(test)]
const ERRNO_ECONNRESET: isize = -104;
const CMSG_HEADER: usize = 16;
const CMSG_ALIGN: usize = 8;
const CONTROL_CAPACITY: usize = 1024;

#[repr(align(8))]
struct ControlBuffer<const N: usize>([u8; N]);

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut u8,
    control_len: usize,
    flags: i32,
}

fn raw_errno(value: isize) -> Option<io::Error> {
    if value >= 0 {
        None
    } else {
        let raw = value
            .checked_neg()
            .and_then(|number| i32::try_from(number).ok())
            .unwrap_or(i32::MAX);
        Some(io::Error::from_raw_os_error(raw))
    }
}

fn errno_result(value: isize, operation: &str) -> Result<usize, String> {
    if let Some(error) = raw_errno(value) {
        return Err(format!("{operation}: {error}"));
    }
    usize::try_from(value).map_err(|_| format!("{operation}: invalid result {value}"))
}

#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    result
}

/// `mmap(2)` takes six arguments, and the sixth is `r9`.
///
/// A separate function rather than a widened `syscall5`: every existing caller
/// passes five, and appending a zero at each of them would have been a silent
/// edit to call sites this crate's confinement tests pin by exact text.
#[allow(unsafe_code)]
fn syscall6(
    number: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    result
}

fn close_raw(fd: RawFd) -> Result<(), String> {
    if fd < 0 {
        return Err(format!("refusing to close invalid descriptor {fd}"));
    }
    errno_result(syscall5(SYS_CLOSE, fd as usize, 0, 0, 0, 0), "close")?;
    Ok(())
}

fn fcntl(fd: RawFd, command: usize, argument: usize, operation: &str) -> Result<usize, String> {
    if !matches!(command, F_GETFL | F_SETFL) {
        return Err(format!(
            "{operation}: refusing unreviewed fcntl command {command:#x}"
        ));
    }
    if fd < 0 {
        return Err(format!("{operation}: invalid descriptor {fd}"));
    }
    errno_result(
        syscall5(SYS_FCNTL, fd as usize, command, argument, 0, 0),
        operation,
    )
}

/// Add `O_NONBLOCK` while retaining the complete prior file-status word. The
/// caller restores that word before dropping the endpoint so a retained
/// duplicate does not inherit a policy td was not given authority to change.
pub fn make_nonblocking(file: &impl AsRawFd) -> Result<usize, String> {
    let flags = fcntl(file.as_raw_fd(), F_GETFL, 0, "F_GETFL")?;
    fcntl(
        file.as_raw_fd(),
        F_SETFL,
        flags | O_NONBLOCK,
        "F_SETFL O_NONBLOCK",
    )?;
    Ok(flags)
}

pub fn restore_status_flags(file: &impl AsRawFd, flags: usize) -> Result<(), String> {
    fcntl(file.as_raw_fd(), F_SETFL, flags, "restore F_SETFL")?;
    Ok(())
}

/// The one `ioctl(2)` entry point. It refuses any request outside the reviewed
/// roster, so a mistyped or newly invented number cannot reach the kernel
/// without amending both this list and the confinement tests that pin it.
///
/// Unlike the two message wrappers this does not retry `EINTR`: none of the
/// six terminal and evdev requests sleeps interruptibly, so a retry loop here
/// would be dead code that reads like a live one. The DRM requests DO sleep
/// interruptibly and are issued through `drm_ioctl` below, which is that loop —
/// the distinction is per-request and not a property of this function.
fn ioctl_checked(
    fd: RawFd,
    request: usize,
    argument: usize,
    operation: &str,
) -> Result<isize, String> {
    if !matches!(
        request,
        TIOCSPTLCK
            | TIOCGPTPEER
            | TIOCSWINSZ
            | TIOCGWINSZ
            | EVIOCGABS_X
            | EVIOCGABS_Y
            | DRM_IOCTL_VERSION
            | DRM_IOCTL_MODE_GETRESOURCES
            | DRM_IOCTL_MODE_GETENCODER
            | DRM_IOCTL_MODE_GETCONNECTOR
            | DRM_IOCTL_DROP_MASTER
            | DRM_IOCTL_MODE_CREATE_DUMB
            | DRM_IOCTL_MODE_MAP_DUMB
            | DRM_IOCTL_MODE_DESTROY_DUMB
            | DRM_IOCTL_SET_MASTER
            | DRM_IOCTL_MODE_GETCRTC
            | DRM_IOCTL_MODE_SETCRTC
            | DRM_IOCTL_MODE_RMFB
            | DRM_IOCTL_MODE_ADDFB2
            | DRM_IOCTL_MODE_PAGE_FLIP
    ) {
        return Err(format!(
            "{operation}: refusing unreviewed ioctl request {request:#x}"
        ));
    }
    if fd < 0 {
        return Err(format!("{operation}: invalid descriptor {fd}"));
    }
    Ok(syscall5(SYS_IOCTL, fd as usize, request, argument, 0, 0))
}

/// The one `ioctl(2)` entry point, as every non-DRM caller sees it.
fn ioctl(fd: RawFd, request: usize, argument: usize, operation: &str) -> Result<usize, String> {
    errno_result(ioctl_checked(fd, request, argument, operation)?, operation)
}

/// A DRM ioctl, re-issued while the kernel says it was interrupted.
///
/// `drm_ioctl` takes the mode-config lock with `mutex_lock_interruptible`, so
/// EINTR and EAGAIN mean "ask again", not "it failed" — which is why libdrm's
/// `drmIoctl` is a loop. Sharing `ioctl_checked` keeps ONE allow-list: a
/// request reaching the kernel through here is a request pinned up there.
fn drm_ioctl(fd: RawFd, request: usize, argument: usize, operation: &str) -> Result<usize, String> {
    for _ in 0..DRM_RESTART_ATTEMPTS {
        let raw = ioctl_checked(fd, request, argument, operation)?;
        if raw == ERRNO_EINTR || raw == ERRNO_EAGAIN {
            continue;
        }
        return errno_result(raw, operation);
    }
    Err(format!(
        "{operation}: interrupted on every one of {DRM_RESTART_ATTEMPTS} attempts"
    ))
}

/// The uid sampled by the kernel when this Unix-stream peer connected.
///
/// The option and level are fixed here rather than accepted from a caller.
/// All three `struct ucred` words are present so the kernel's exact 12-byte
/// answer is checked before the uid word is believed.
pub fn peer_uid(stream: &UnixStream) -> Result<u32, String> {
    let mut credentials = [0u32; 3];
    let mut length = u32::try_from(std::mem::size_of_val(&credentials))
        .map_err(|_| "SO_PEERCRED buffer length exceeds u32".to_string())?;
    errno_result(
        syscall5(
            SYS_GETSOCKOPT,
            stream.as_raw_fd() as usize,
            SOL_SOCKET as usize,
            SO_PEERCRED as usize,
            (&mut credentials as *mut [u32; 3]) as usize,
            (&mut length as *mut u32) as usize,
        ),
        "getsockopt SO_PEERCRED",
    )?;
    let expected = u32::try_from(std::mem::size_of_val(&credentials))
        .map_err(|_| "SO_PEERCRED buffer length exceeds u32".to_string())?;
    if length != expected {
        return Err(format!(
            "getsockopt SO_PEERCRED returned {length} bytes, expected {expected}"
        ));
    }
    credentials
        .get(1)
        .copied()
        .ok_or_else(|| "SO_PEERCRED uid word is absent".to_string())
}

/// The kernel's `struct winsize`: four native-endian `u16` fields. The pixel
/// pair is reported as zero because td-term publishes a character grid.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub x_pixels: u16,
    pub y_pixels: u16,
}

/// The eight bytes the kernel actually reads and writes. `[u16; 4]` rather than
/// a `#[repr(C)]` struct because the language guarantees this layout, which
/// makes the field ORDER an ordinary tested function instead of an attribute
/// nobody can observe: a swapped pair is a well-formed resize to another size.
fn winsize_words(size: WindowSize) -> [u16; 4] {
    [size.rows, size.columns, size.x_pixels, size.y_pixels]
}

fn winsize_from_words(words: [u16; 4]) -> WindowSize {
    let [rows, columns, x_pixels, y_pixels] = words;
    WindowSize {
        rows,
        columns,
        x_pixels,
        y_pixels,
    }
}

/// Unlock a freshly opened `/dev/ptmx` master. `TIOCSPTLCK` takes a pointer to a
/// four-byte `int`; zero unlocks.
pub fn unlock_pty(master: &impl AsRawFd) -> Result<(), String> {
    let unlocked: i32 = 0;
    ioctl(
        master.as_raw_fd(),
        TIOCSPTLCK,
        (&unlocked as *const i32) as usize,
        "TIOCSPTLCK",
    )?;
    Ok(())
}

/// Obtain the master's slave. `TIOCGPTPEER` takes the open flags as an immediate
/// rather than a pointer and returns a new descriptor for the same peer the
/// master already names, so no `/dev/pts/N` name is resolved.
///
/// The returned number is adopted exactly once, through the same
/// `/proc/self/fd/N` duplication the received-descriptor path uses: a safe
/// `OwnedFd` conversion would be a second, differently-shaped `unsafe` surface
/// for a descriptor this crate can reopen by identity instead.
pub fn pty_peer(master: &impl AsRawFd) -> Result<File, String> {
    let raw = ioctl(
        master.as_raw_fd(),
        TIOCGPTPEER,
        PTY_PEER_FLAGS,
        "TIOCGPTPEER",
    )?;
    let fd = RawFd::try_from(raw)
        .map_err(|_| format!("TIOCGPTPEER returned invalid descriptor {raw}"))?;
    reopen_and_close(
        fd,
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY),
        "terminal peer",
    )
}

/// Publish a grid size on a terminal.
pub fn set_window_size(terminal: &impl AsRawFd, size: WindowSize) -> Result<(), String> {
    let words = winsize_words(size);
    ioctl(
        terminal.as_raw_fd(),
        TIOCSWINSZ,
        (&words as *const [u16; 4]) as usize,
        "TIOCSWINSZ",
    )?;
    Ok(())
}

/// Read back a terminal's grid size. Every published size is verified through
/// this call before the child is allowed to observe it.
pub fn window_size(terminal: &impl AsRawFd) -> Result<WindowSize, String> {
    let mut words = [0u16; 4];
    ioctl(
        terminal.as_raw_fd(),
        TIOCGWINSZ,
        (&mut words as *mut [u16; 4]) as usize,
        "TIOCGWINSZ",
    )?;
    Ok(winsize_from_words(words))
}

/// Which axis of an absolute device to ask about. An ENUM rather than a
/// request number at the call site, for `Disposition`'s reason in td-sh: the
/// two requests differ in one nibble, they are the only evdev requests on this
/// surface, and a caller that could name a number could name a third.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbsAxis {
    X,
    Y,
}

impl AbsAxis {
    fn request(self) -> (usize, &'static str) {
        match self {
            AbsAxis::X => (EVIOCGABS_X, "EVIOCGABS(ABS_X)"),
            AbsAxis::Y => (EVIOCGABS_Y, "EVIOCGABS(ABS_Y)"),
        }
    }
}

/// What an absolute axis reports: where it is NOW, and the span that says what
/// that number means. Only the three fields answering those two questions are
/// carried out; `fuzz`, `flat` and `resolution` describe filtering and physical
/// size, and a pointer being mapped to a screen wants neither.
///
/// `value` is the axis's position at the moment it was asked, which is the only
/// way to know where a device is BEFORE it has reported anything: the kernel
/// omits an axis whose value has not changed, so a device's first frame can
/// name one axis and say nothing about the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
}

/// The kernel's `struct input_absinfo`: six native-endian `i32` fields — value,
/// minimum, maximum, fuzz, flat, resolution. Pinned because `EVIOCGABS` copies
/// the smaller of the REQUEST NUMBER's own size field and this struct's size,
/// so an oversized number is harmless but a buffer shortened without the number
/// is 24 bytes written into less — an out-of-bounds kernel write from code the
/// compiler reads as safe. Both numbers above encode that same 24.
const ABSINFO_WORDS: usize = 6;

/// Ask an absolute device where one of its axes is and what range it reports
/// over.
///
/// `[i32; 6]` rather than a `#[repr(C)]` struct for `winsize`'s reason: the
/// language guarantees this layout, which makes the field ORDER a tested
/// function rather than an attribute nobody can observe. It matters more here
/// than there — `value`, `minimum` and `maximum` are three adjacent words of
/// the same type, so an index off by one is a well-formed range that maps every
/// report to the wrong place on screen.
pub fn absolute_info(device: &impl AsRawFd, axis: AbsAxis) -> Result<AbsInfo, String> {
    let (request, name) = axis.request();
    let mut words = [0i32; ABSINFO_WORDS];
    ioctl(
        device.as_raw_fd(),
        request,
        (&mut words as *mut [i32; ABSINFO_WORDS]) as usize,
        name,
    )?;
    Ok(absinfo(words))
}

fn absinfo(words: [i32; ABSINFO_WORDS]) -> AbsInfo {
    let [value, minimum, maximum, _fuzz, _flat, _resolution] = words;
    AbsInfo {
        value,
        minimum,
        maximum,
    }
}

/// The kernel's `struct drm_version`. Only the driver NAME is carried out: the
/// version triple is the driver's own interface revision and `date`/`desc` are
/// free text, while the name is what says which driver is behind this node and
/// is the one field a proof can assert against.
#[repr(C)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

impl DrmVersion {
    /// Every field zero and every pointer null. `drm_copy_field` reports a
    /// length whether or not it was given a buffer, which is what makes the
    /// null-pointer first call the count call.
    fn empty() -> DrmVersion {
        DrmVersion {
            version_major: 0,
            version_minor: 0,
            version_patchlevel: 0,
            name_len: 0,
            name: std::ptr::null_mut(),
            date_len: 0,
            date: std::ptr::null_mut(),
            desc_len: 0,
            desc: std::ptr::null_mut(),
        }
    }
}

/// A driver name longer than this is not one. `drm_copy_field` reports the
/// driver's full `strlen` regardless of the buffer it was given, so this bounds
/// an allocation made from a number the device chose.
const MAX_DRM_DRIVER_NAME: usize = 64;

/// The kernel's `struct drm_mode_card_res`.
#[repr(C)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

impl DrmModeCardRes {
    fn empty() -> DrmModeCardRes {
        DrmModeCardRes {
            fb_id_ptr: 0,
            crtc_id_ptr: 0,
            connector_id_ptr: 0,
            encoder_id_ptr: 0,
            count_fbs: 0,
            count_crtcs: 0,
            count_connectors: 0,
            count_encoders: 0,
            min_width: 0,
            max_width: 0,
            min_height: 0,
            max_height: 0,
        }
    }
}

/// The kernel's `struct drm_mode_get_connector`.
#[repr(C)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

impl DrmModeGetConnector {
    fn for_connector(connector_id: u32) -> DrmModeGetConnector {
        DrmModeGetConnector {
            encoders_ptr: 0,
            modes_ptr: 0,
            props_ptr: 0,
            prop_values_ptr: 0,
            count_modes: 0,
            count_props: 0,
            count_encoders: 0,
            encoder_id: 0,
            connector_id,
            connector_type: 0,
            connector_type_id: 0,
            connection: 0,
            mm_width: 0,
            mm_height: 0,
            subpixel: 0,
            pad: 0,
        }
    }
}

/// The kernel's `struct drm_mode_get_encoder`.
#[repr(C)]
struct DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

/// The kernel's `struct drm_mode_modeinfo`, one scanout timing.
///
/// The whole 68-byte layout is carried rather than the two fields td reads
/// today, because this struct is also what `DRM_IOCTL_MODE_SETCRTC` takes BACK:
/// a mode is read here and handed to the kernel unchanged, so dropping the
/// timings on the way through would leave a modeset asking for a resolution
/// with no clock to drive it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub mode_type: u32,
    pub name: [u8; DRM_DISPLAY_MODE_LEN],
}

impl DrmModeInfo {
    fn empty() -> DrmModeInfo {
        DrmModeInfo {
            clock: 0,
            hdisplay: 0,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 0,
            hskew: 0,
            vdisplay: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 0,
            vscan: 0,
            vrefresh: 0,
            flags: 0,
            mode_type: 0,
            name: [0; DRM_DISPLAY_MODE_LEN],
        }
    }

    /// The mode's name as the kernel left it: NUL-padded, and NOT promised a
    /// terminator in the last byte, so the whole field is scanned rather than
    /// a terminator being trusted to appear.
    pub fn name(&self) -> String {
        let end = self
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(DRM_DISPLAY_MODE_LEN);
        // Filtered to printable ASCII, not merely lossy-decoded. This string
        // is printed into a console line whose fields a boot check parses by
        // splitting on whitespace, so a name carrying a newline would truncate
        // the report and one carrying " output=" would shadow a later field.
        // The bytes come from a driver or an EDID rather than from an attacker,
        // which is why this is a correctness guard and not a security one.
        String::from_utf8_lossy(self.name.get(..end).unwrap_or(&[]))
            .chars()
            .filter(|c| c.is_ascii_graphic())
            .collect()
    }

    /// Whether the driver marked this the mode it would rather be asked for.
    pub fn is_preferred(&self) -> bool {
        self.mode_type & DRM_MODE_TYPE_PREFERRED != 0
    }
}

/// What a card says it has.
///
/// Two of the four object lists are deliberately not requested, and their
/// counts are written into the request struct but never read back out of it. Framebuffer
/// ids are the ids of THIS descriptor's own framebuffers, of which a
/// discovering process has none; and the card-wide encoder list is not the one
/// a selection wants — the encoders that can drive a given connector are the
/// connector's own list, and picking from the card's would consider encoders
/// wired to a different one. Their counts are still read, because the ioctl
/// reports them whether or not a buffer was offered.
pub struct DrmResources {
    pub crtcs: Vec<u32>,
    pub connectors: Vec<u32>,
}

/// One connector, as read.
pub struct DrmConnector {
    pub id: u32,
    pub connection: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    /// The encoder currently driving this connector, or zero for none.
    pub encoder_id: u32,
    pub encoders: Vec<u32>,
    pub modes: Vec<DrmModeInfo>,
}

/// One encoder, as read. `possible_crtcs` is a bitmask over INDEXES into
/// `DrmResources::crtcs`, not over CRTC ids — reading it as ids is the classic
/// way to modeset onto a CRTC the encoder cannot reach.
pub struct DrmEncoder {
    pub id: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
}

/// A count the device chose, checked before it is used as an allocation length.
fn drm_count(value: u32, ceiling: u32, what: &str) -> Result<usize, String> {
    if value > ceiling {
        return Err(format!(
            "DRM device reports {value} {what}, past the {ceiling} this compositor will allocate for"
        ));
    }
    usize::try_from(value).map_err(|_| format!("DRM {what} count {value} does not fit a usize"))
}

fn address_of<T>(slice: &mut [T]) -> u64 {
    if slice.is_empty() {
        return 0;
    }
    slice.as_mut_ptr() as usize as u64
}

/// Give back the mastership that opening a primary node granted.
///
/// `EINVAL` is the ordinary answer when this process is NOT master — another
/// compositor already holds it — and is not a failure: the post-condition
/// either way is that this descriptor is not the DRM master. Every other errno
/// is reported, because it means the state is not what the caller was told.
pub fn drm_drop_master(card: &impl AsRawFd) -> Result<(), String> {
    match drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_DROP_MASTER,
        0,
        "DRM_IOCTL_DROP_MASTER",
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.contains("Invalid argument") => Ok(()),
        Err(error) => Err(error),
    }
}

/// The kernel's `struct drm_mode_create_dumb`.
#[repr(C)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// The kernel's `struct drm_mode_map_dumb`.
#[repr(C)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

/// The kernel's `struct drm_mode_destroy_dumb`.
#[repr(C)]
struct DrmModeDestroyDumb {
    handle: u32,
}

/// What the kernel chose for one dumb buffer.
///
/// The PITCH is the kernel's, never `width * 4`. A driver may align a scanline
/// well past the pixel width, so a renderer computing its own stride would
/// write the second row into the middle of the first on any card that does.
/// Returned rather than assumed, for exactly that reason.
pub struct DrmDumbBuffer {
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

/// One mapping of one dumb buffer, owned as a value.
///
/// NOT `Clone` and not `Copy`. `Copy` the compiler refuses on its own for a
/// type with `Drop`; `Clone` it would accept, because `*mut u8` is `Copy` and a
/// derive would compile — two owners, two unmaps, and two `&mut [u8]` over the
/// same bytes. A confinement test pins the declaration so a derive cannot
/// appear above it unnoticed.
///
/// `UNSAFE.md` §6 budgeted this shape before the code existed, and it is a new
/// CLASS rather than another syscall on an existing surface. Every other unsafe
/// site in this crate is an instruction one-shot: it begins at `syscall` and
/// ends when it returns, and what returns is a number safe code then owns. A
/// scanout target inverts that. The mapping IS the destination — the renderer
/// writes pixels INTO kernel-owned memory — so a slice over that memory has to
/// cross a module boundary and stay valid while a frame is drawn.
///
/// The invariants, each pinned by a confinement test:
///
/// - `length` is the length the mapping was CREATED with, held here rather than
///   recomputed at each use. A length that can drift from its mapping is an
///   out-of-bounds write the compiler reads as safe — the `EVIOCGABS` failure
///   mode one layer up.
/// - `Drop` unmaps, and nothing else does, so the region's lifetime is this
///   value's. That is the whole of what the name promises.
/// - Neither `Clone` nor `Copy`: two owners would be two unmaps.
/// - The borrowed slice is the only way out. No raw pointer reaches a caller,
///   and the borrow cannot outlive the guard.
pub struct MappedRegion {
    address: *mut u8,
    length: usize,
}

impl MappedRegion {
    /// The pixels, borrowed for no longer than the mapping that holds them.
    #[allow(unsafe_code)]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `drm_map_dumb` is the sole constructor and returns only after
        // `mmap` answered a live mapping of exactly `length` bytes at
        // `address`. `Drop` is the only unmap and cannot have run while this
        // `&mut self` exists, and the returned borrow cannot outlive it.
        unsafe { core::slice::from_raw_parts_mut(self.address, self.length) }
    }

    /// The length the mapping was created with.
    pub fn len(&self) -> usize {
        self.length
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        // The pair `mmap` returned, unchanged since, unmapped exactly once.
        let _ = syscall5(SYS_MUNMAP, self.address as usize, self.length, 0, 0, 0);
    }
}

/// Allocate one dumb buffer on this card.
pub fn drm_create_dumb(
    card: &impl AsRawFd,
    width: u32,
    height: u32,
) -> Result<DrmDumbBuffer, String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "DRM_IOCTL_MODE_CREATE_DUMB: refusing a {width}x{height} buffer with no pixels"
        ));
    }
    let mut request = DrmModeCreateDumb {
        height,
        width,
        bpp: DUMB_BUFFER_BPP,
        flags: 0,
        handle: 0,
        pitch: 0,
        size: 0,
    };
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_CREATE_DUMB,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_CREATE_DUMB",
    )?;
    if request.size == 0 || request.pitch == 0 {
        // The handle is live whatever the geometry says, so it is released
        // before this returns rather than leaked to a caller that never saw it.
        let _ = drm_destroy_dumb(card, request.handle);
        return Err(format!(
            "DRM_IOCTL_MODE_CREATE_DUMB: the driver reported pitch {} and size {} for a {width}x{height} buffer",
            request.pitch, request.size
        ));
    }
    Ok(DrmDumbBuffer {
        handle: request.handle,
        pitch: request.pitch,
        size: request.size,
    })
}

/// Map one dumb buffer this crate created, at the offset the kernel reports.
///
/// The ONE construction site for `MappedRegion`, which is what makes the length
/// in that type the length of its mapping: both come from here, and there is no
/// second path on which they could disagree.
pub fn drm_map_dumb(card: &impl AsRawFd, buffer: &DrmDumbBuffer) -> Result<MappedRegion, String> {
    let length = mappable_length(buffer.size)?;
    let fd = card.as_raw_fd();
    if fd < 0 {
        return Err(format!("DRM_IOCTL_MODE_MAP_DUMB: invalid descriptor {fd}"));
    }
    let mut request = DrmModeMapDumb {
        handle: buffer.handle,
        pad: 0,
        offset: 0,
    };
    drm_ioctl(
        fd,
        DRM_IOCTL_MODE_MAP_DUMB,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_MAP_DUMB",
    )?;
    let offset = usize::try_from(request.offset).map_err(|_| {
        "DRM_IOCTL_MODE_MAP_DUMB: the kernel reported an offset outside this address space"
            .to_string()
    })?;
    let address = errno_result(
        syscall6(
            SYS_MMAP,
            0,
            length,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as usize,
            offset,
        ),
        "mmap",
    )?;
    // Built BEFORE it is judged, so the refusal below unmaps through `Drop`
    // rather than through a second copy of the unmap. An earlier revision
    // returned the error first and leaked the mapping on a path it called
    // impossible; a review found it, and "unreachable" is not a reason to own
    // a region on one path and not the other.
    let region = MappedRegion {
        address: address as *mut u8,
        length,
    };
    if region.address.is_null() {
        return Err("mmap: the kernel returned a null mapping".to_string());
    }
    Ok(region)
}

/// The length a device-reported size may be mapped at, or why it may not.
///
/// A size read from a driver becomes an `mmap` LENGTH, so this is the
/// `MAX_DRM_OBJECTS` problem again in a second place: an implausible number is
/// a request to size this process's address space. Pure, so the three refusals
/// are testable without a card — which is the whole reason it is a function
/// rather than three `if`s inside the caller.
fn mappable_length(size: u64) -> Result<usize, String> {
    if size == 0 {
        return Err("DRM_IOCTL_MODE_MAP_DUMB: refusing to map a zero-length buffer".to_string());
    }
    if size > MAX_DUMB_BUFFER_BYTES {
        return Err(format!(
            "DRM_IOCTL_MODE_MAP_DUMB: a buffer of {size} bytes is past the \
             {MAX_DUMB_BUFFER_BYTES} this compositor will map"
        ));
    }
    usize::try_from(size).map_err(|_| {
        format!("DRM_IOCTL_MODE_MAP_DUMB: a buffer of {size} bytes does not fit this address space")
    })
}

/// The kernel's `struct drm_mode_crtc`, 104 bytes: a `__u64` pointer, seven
/// `__u32`s, and a whole embedded `drm_mode_modeinfo`.
///
/// One type for BOTH `GETCRTC` and `SETCRTC`, because it is one struct in the
/// kernel too. That is what makes save-and-restore exact: the state read back
/// is handed to the setter unchanged, with no field this code had to know to
/// copy.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeInfo,
}

impl DrmModeCrtc {
    pub fn empty() -> DrmModeCrtc {
        DrmModeCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: 0,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: DrmModeInfo::empty(),
        }
    }
}

/// The kernel's `struct drm_mode_fb_cmd2`, 104 bytes.
///
/// Four planes' worth of handles, pitches, offsets and modifiers, of which a
/// linear XRGB8888 dumb buffer uses exactly one. The other three stay zero,
/// which is what the kernel reads as "not present" rather than as a plane at
/// offset zero.
#[repr(C)]
struct DrmModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

/// Become the DRM master of this card.
///
/// The authority to modeset, and the reason it is separate from opening:
/// `drm_ioctl.c` flags `MODE_SETCRTC` `DRM_MASTER` (`:675`), so without this
/// the modeset is refused. A caller takes it for a bounded window and gives it
/// back.
///
/// Holding it does not by itself blank anything — see `DRM_IOCTL_DROP_MASTER`
/// above for what it does and does not cost. The screen changes at the
/// `SETCRTC`, not here.
pub fn drm_set_master(card: &impl AsRawFd) -> Result<(), String> {
    drm_ioctl(card.as_raw_fd(), DRM_IOCTL_SET_MASTER, 0, "DRM_IOCTL_SET_MASTER")?;
    Ok(())
}

/// Read one CRTC's current mode, framebuffer and position.
pub fn drm_get_crtc(card: &impl AsRawFd, crtc_id: u32) -> Result<DrmModeCrtc, String> {
    let mut request = DrmModeCrtc::empty();
    request.crtc_id = crtc_id;
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_GETCRTC,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_GETCRTC",
    )?;
    Ok(request)
}

/// Drive `connectors` from `crtc` with `fb_id` at `mode`, or hand back a state
/// read from `drm_get_crtc` to restore it.
///
/// `connectors` is borrowed for the call and its address handed to the kernel,
/// so it must outlive the ioctl -- which is why it is a parameter rather than a
/// temporary built inside. An empty list with `mode_valid` clear is how a CRTC
/// is turned off, and is what a restore of a previously-dark CRTC sends.
pub fn drm_set_crtc(
    card: &impl AsRawFd,
    state: &DrmModeCrtc,
    connectors: &mut [u32],
) -> Result<(), String> {
    let mut request = *state;
    request.count_connectors = u32::try_from(connectors.len())
        .map_err(|_| format!("DRM_IOCTL_MODE_SETCRTC: {} connectors is not a u32", connectors.len()))?;
    request.set_connectors_ptr = address_of(connectors);
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_SETCRTC,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_SETCRTC",
    )?;
    Ok(())
}

/// `struct drm_mode_crtc_page_flip`, 24 bytes: four `u32` then a `u64`.
///
/// The kernel's handler actually takes `drm_mode_crtc_page_flip_target`,
/// which is the SAME 24 bytes with `reserved` repurposed as `sequence`, so
/// either name describes the identical wire layout and the UAPI request number
/// (0x18 in its size field) is right for both.
///
/// The reserved word must be zero, and the reason is the opposite of what an
/// earlier revision of this comment said. It claimed the field is read only
/// under a `TARGET` flag; in fact `drm_mode_page_flip_ioctl` reads it
/// UNCONDITIONALLY — `u32 target_vblank = page_flip->sequence;`
/// (`drm_plane.c:1389`) — and then refuses the request outright if it is
/// non-zero without a `TARGET` flag (`:1399`). Zero is required precisely
/// BECAUSE the field is always read, which is a reason to keep it zero rather
/// than a reason it would not matter.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeCrtcPageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

/// What a completed page flip reports back.
///
/// `user_data` is the whole reason this type exists. The kernel copies the
/// `u64` a flip was queued with into the event it delivers, so the completion
/// is self-identifying: a caller that queued frames 7 and 8 can tell which one
/// arrived without keeping a side table and without assuming the events come
/// back in order. `td-compositor/DESIGN.md` recorded "an identity neither
/// `Submission` nor `OutputEvent` carries" as the blocker for consuming these;
/// the identity was in the ABI the whole time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrmFlipCompletion {
    pub user_data: u64,
    pub sequence: u32,
    pub crtc_id: u32,
}

/// The fixed length of `struct drm_event_vblank`: an 8-byte `drm_event`
/// header, a `u64`, and four `u32`.
pub const DRM_EVENT_VBLANK_LEN: usize = 32;

/// Read one event out of a buffer of bytes read from the card.
///
/// Returns how many bytes the event occupied, and the completion if it was a
/// flip completion rather than some other event. A `None` completion with a
/// non-zero length is an event this crate does not consume — a vblank, a CRTC
/// sequence — which is SKIPPED by its own declared length rather than by this
/// code's idea of how long it should be. That is the difference between a
/// parser and an assumption: the kernel may deliver an event type this build
/// has never heard of, and the only safe way past it is the length it came
/// with.
///
/// `None` overall means the buffer does not yet hold a whole event, which on a
/// non-blocking descriptor is the ordinary answer and not an error.
pub fn parse_drm_event(bytes: &[u8]) -> Option<(usize, Option<DrmFlipCompletion>)> {
    let kind = read_u32(bytes, 0)?;
    let length = read_u32(bytes, 4)?;
    let length = usize::try_from(length).ok()?;
    // A length that does not cover its own header, or that the buffer does not
    // hold yet, is not something to advance past: the first would loop forever
    // on a zero and the second would read the next event's bytes as this one's.
    if length < 8 || bytes.len() < length {
        return None;
    }
    if kind != DRM_EVENT_FLIP_COMPLETE || length < DRM_EVENT_VBLANK_LEN {
        return Some((length, None));
    }
    let completion = DrmFlipCompletion {
        user_data: read_u64(bytes, 8)?,
        sequence: read_u32(bytes, 24)?,
        crtc_id: read_u32(bytes, 28)?,
    };
    Some((length, Some(completion)))
}

/// Read a little-endian `u32` at `offset`, or `None` if it is not all there.
///
/// Byte-at-a-time through `get` rather than a cast over the buffer: the read
/// is from a `Vec<u8>` with no alignment guarantee, and a slice that is one
/// byte short must answer `None` rather than read the byte after it.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let field: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(field))
}

/// Read a little-endian `u64` at `offset`. See `read_u32`.
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    let field: [u8; 8] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u64::from_le_bytes(field))
}

/// Queue a page flip on `crtc_id` to `fb_id`, tagged with `user_data`.
///
/// The CRTC must already be scanning out a framebuffer: `drm_mode_page_flip_ioctl`
/// answers `-EBUSY` when the primary plane has none (`drm_plane.c:1473`), so a
/// flip is a thing done to an established modeset rather than a way to
/// establish one. It also needs mastership — `drm_ioctl.c:694` flags it
/// `DRM_MASTER` — and the new framebuffer must cover the plane's source
/// rectangle.
pub fn drm_page_flip(
    card: &impl AsRawFd,
    crtc_id: u32,
    fb_id: u32,
    user_data: u64,
) -> Result<(), String> {
    let mut request = DrmModeCrtcPageFlip {
        crtc_id,
        fb_id,
        flags: DRM_MODE_PAGE_FLIP_EVENT,
        reserved: 0,
        user_data,
    };
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_PAGE_FLIP,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_PAGE_FLIP",
    )?;
    Ok(())
}

/// Register one linear XRGB8888 buffer as a scanout framebuffer.
pub fn drm_add_fb(
    card: &impl AsRawFd,
    width: u32,
    height: u32,
    pitch: u32,
    handle: u32,
) -> Result<u32, String> {
    let mut request = DrmModeFbCmd2 {
        fb_id: 0,
        width,
        height,
        pixel_format: DRM_FORMAT_XRGB8888,
        flags: 0,
        handles: [handle, 0, 0, 0],
        pitches: [pitch, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_ADDFB2,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_ADDFB2",
    )?;
    if request.fb_id == 0 {
        return Err("DRM_IOCTL_MODE_ADDFB2: the driver returned framebuffer id 0".to_string());
    }
    Ok(request.fb_id)
}

/// Unregister a framebuffer id.
pub fn drm_rm_fb(card: &impl AsRawFd, fb_id: u32) -> Result<(), String> {
    let mut id = fb_id;
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_RMFB,
        std::ptr::from_mut(&mut id) as usize,
        "DRM_IOCTL_MODE_RMFB",
    )?;
    Ok(())
}

/// Release one dumb buffer handle.
pub fn drm_destroy_dumb(card: &impl AsRawFd, handle: u32) -> Result<(), String> {
    let mut request = DrmModeDestroyDumb { handle };
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_DESTROY_DUMB,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_DESTROY_DUMB",
    )?;
    Ok(())
}

/// Ask which driver is behind this node.
pub fn drm_driver_name(card: &impl AsRawFd) -> Result<String, String> {
    let fd = card.as_raw_fd();
    let mut probe = DrmVersion::empty();
    drm_ioctl(
        fd,
        DRM_IOCTL_VERSION,
        std::ptr::from_mut(&mut probe) as usize,
        "DRM_IOCTL_VERSION length",
    )?;
    let length = probe.name_len;
    if length == 0 {
        return Err("DRM_IOCTL_VERSION: the driver reports an empty name".to_string());
    }
    if length > MAX_DRM_DRIVER_NAME {
        return Err(format!(
            "DRM_IOCTL_VERSION: driver name of {length} bytes is past the {MAX_DRM_DRIVER_NAME} this compositor will allocate for"
        ));
    }
    let mut name = vec![0u8; length];
    let mut fill = DrmVersion::empty();
    fill.name_len = length;
    fill.name = name.as_mut_ptr();
    drm_ioctl(
        fd,
        DRM_IOCTL_VERSION,
        std::ptr::from_mut(&mut fill) as usize,
        "DRM_IOCTL_VERSION",
    )?;
    // `drm_copy_field` copies min(strlen, the length it was given) and then
    // reports the full strlen, so a name that grew between the two calls is
    // reported longer than what was written. Believe the smaller number.
    name.truncate(length.min(fill.name_len));
    // Filtered exactly as `DrmModeInfo::name` is, and for the same reason: this
    // string goes into a whitespace-split probe report, so a driver name
    // carrying a newline would truncate the line and one carrying " output="
    // would shadow a later field. It also bounds the decoded LENGTH -- lossy
    // decoding expands one invalid byte to three, so 64 invalid bytes would
    // otherwise produce 192 characters and push the report past the bound the
    // boot check reads it within. An asymmetry a review found: the mode name
    // was filtered here and the driver name was not.
    Ok(String::from_utf8_lossy(&name)
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .collect())
}

/// Ask what the card has.
///
/// Two calls: one to learn the counts, one to fill buffers sized from them.
/// The counts can change between the two — a connector can appear — so a fill
/// that reports MORE than it was given is retried rather than believed.
pub fn drm_resources(card: &impl AsRawFd) -> Result<DrmResources, String> {
    let fd = card.as_raw_fd();
    for _ in 0..DRM_RESTART_ATTEMPTS {
        let mut probe = DrmModeCardRes::empty();
        drm_ioctl(
            fd,
            DRM_IOCTL_MODE_GETRESOURCES,
            std::ptr::from_mut(&mut probe) as usize,
            "DRM_IOCTL_MODE_GETRESOURCES counts",
        )?;
        let wanted_crtcs = drm_count(probe.count_crtcs, MAX_DRM_OBJECTS, "CRTCs")?;
        let wanted_connectors = drm_count(probe.count_connectors, MAX_DRM_OBJECTS, "connectors")?;

        let mut crtcs = vec![0u32; wanted_crtcs];
        let mut connectors = vec![0u32; wanted_connectors];

        let mut fill = DrmModeCardRes::empty();
        fill.count_crtcs = probe.count_crtcs;
        fill.count_connectors = probe.count_connectors;
        fill.crtc_id_ptr = address_of(&mut crtcs);
        fill.connector_id_ptr = address_of(&mut connectors);
        drm_ioctl(
            fd,
            DRM_IOCTL_MODE_GETRESOURCES,
            std::ptr::from_mut(&mut fill) as usize,
            "DRM_IOCTL_MODE_GETRESOURCES",
        )?;

        if fill.count_crtcs > probe.count_crtcs
            || fill.count_connectors > probe.count_connectors
        {
            continue;
        }
        crtcs.truncate(drm_count(fill.count_crtcs, MAX_DRM_OBJECTS, "CRTCs")?);
        connectors.truncate(drm_count(
            fill.count_connectors,
            MAX_DRM_OBJECTS,
            "connectors",
        )?);
        return Ok(DrmResources { crtcs, connectors });
    }
    Err(format!(
        "DRM_IOCTL_MODE_GETRESOURCES: the card's object counts changed on every one of {DRM_RESTART_ATTEMPTS} attempts"
    ))
}

/// Read one connector: its status, its modes, and the encoders that can drive it.
///
/// A zero `count_modes` asks the kernel to re-probe, but ONLY for the current
/// DRM master — `drm_mode_getconnector` demotes everyone else to a read-only
/// probe and says so in `drm_dbg_kms`. This process is deliberately not master,
/// so what comes back is the mode list the kernel already had, which on a
/// virtio-gpu driven by fbdev emulation is the list that produced the current
/// mode. Discovery reporting no modes is therefore a fact about mastership as
/// much as about the sink, and the caller says so rather than calling the
/// screen absent.
pub fn drm_connector(card: &impl AsRawFd, connector_id: u32) -> Result<DrmConnector, String> {
    let fd = card.as_raw_fd();
    for _ in 0..DRM_RESTART_ATTEMPTS {
        let mut probe = DrmModeGetConnector::for_connector(connector_id);
        // `count_modes = 1` with room for one mode is the COUNTING form the
        // UAPI documents. Zero is not a smaller version of it: `count_modes ==
        // 0` is the kernel's force-probe request, which re-reads EDID and, in
        // its own header's words, "can be slow, might cause flickering and the
        // ioctl will block". A discovery pass has no business doing that to a
        // screen somebody is looking at, and an earlier revision asked for it
        // on every boot and on every retry.
        let mut one_mode = [DrmModeInfo::empty(); 1];
        probe.count_modes = 1;
        probe.modes_ptr = address_of(&mut one_mode);
        drm_ioctl(
            fd,
            DRM_IOCTL_MODE_GETCONNECTOR,
            std::ptr::from_mut(&mut probe) as usize,
            "DRM_IOCTL_MODE_GETCONNECTOR counts",
        )?;
        let wanted_modes = drm_count(probe.count_modes, MAX_DRM_MODES, "modes")?;
        let wanted_encoders = drm_count(probe.count_encoders, MAX_DRM_OBJECTS, "encoders")?;

        let mut modes = vec![DrmModeInfo::empty(); wanted_modes];
        let mut encoders = vec![0u32; wanted_encoders];

        let mut fill = DrmModeGetConnector::for_connector(connector_id);
        fill.count_modes = probe.count_modes;
        fill.count_encoders = probe.count_encoders;
        fill.modes_ptr = address_of(&mut modes);
        fill.encoders_ptr = address_of(&mut encoders);
        drm_ioctl(
            fd,
            DRM_IOCTL_MODE_GETCONNECTOR,
            std::ptr::from_mut(&mut fill) as usize,
            "DRM_IOCTL_MODE_GETCONNECTOR",
        )?;

        // Both arrays are copied all-or-nothing: the kernel fills them only
        // when what it was given is at least what it has. A larger count back
        // means nothing was written, so the buffers are re-sized rather than
        // read.
        if fill.count_modes > probe.count_modes || fill.count_encoders > probe.count_encoders {
            continue;
        }
        modes.truncate(drm_count(fill.count_modes, MAX_DRM_MODES, "modes")?);
        encoders.truncate(drm_count(
            fill.count_encoders,
            MAX_DRM_OBJECTS,
            "encoders",
        )?);
        return Ok(DrmConnector {
            id: fill.connector_id,
            connection: fill.connection,
            connector_type: fill.connector_type,
            connector_type_id: fill.connector_type_id,
            mm_width: fill.mm_width,
            mm_height: fill.mm_height,
            encoder_id: fill.encoder_id,
            encoders,
            modes,
        });
    }
    Err(format!(
        "DRM_IOCTL_MODE_GETCONNECTOR: connector {connector_id} changed shape on every one of {DRM_RESTART_ATTEMPTS} attempts"
    ))
}

/// Read one encoder.
pub fn drm_encoder(card: &impl AsRawFd, encoder_id: u32) -> Result<DrmEncoder, String> {
    let mut request = DrmModeGetEncoder {
        encoder_id,
        encoder_type: 0,
        crtc_id: 0,
        possible_crtcs: 0,
        possible_clones: 0,
    };
    drm_ioctl(
        card.as_raw_fd(),
        DRM_IOCTL_MODE_GETENCODER,
        std::ptr::from_mut(&mut request) as usize,
        "DRM_IOCTL_MODE_GETENCODER",
    )?;
    Ok(DrmEncoder {
        id: request.encoder_id,
        crtc_id: request.crtc_id,
        possible_crtcs: request.possible_crtcs,
    })
}

fn align_cmsg(value: usize) -> Result<usize, String> {
    value
        .checked_add(CMSG_ALIGN - 1)
        .map(|sum| sum & !(CMSG_ALIGN - 1))
        .ok_or_else(|| "ancillary length overflow".to_string())
}

fn read_usize(bytes: &[u8]) -> Result<usize, String> {
    let raw: [u8; std::mem::size_of::<usize>()] = bytes
        .get(..std::mem::size_of::<usize>())
        .ok_or_else(|| "truncated ancillary usize".to_string())?
        .try_into()
        .map_err(|_| "truncated ancillary usize".to_string())?;
    Ok(usize::from_ne_bytes(raw))
}

fn read_i32(bytes: &[u8]) -> Result<i32, String> {
    let raw: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "truncated ancillary i32".to_string())?
        .try_into()
        .map_err(|_| "truncated ancillary i32".to_string())?;
    Ok(i32::from_ne_bytes(raw))
}

fn close_all(fds: &[RawFd]) {
    for fd in fds {
        let _ = close_raw(*fd);
    }
}

fn parse_fds(control: &[u8]) -> Result<Vec<RawFd>, String> {
    let mut fds = Vec::new();
    let mut refusal = None;
    let mut offset = 0usize;
    while offset < control.len() {
        let remaining = match control.get(offset..) {
            Some(value) => value,
            None => {
                close_all(&fds);
                return Err("ancillary offset escaped buffer".into());
            }
        };
        if remaining.len() < CMSG_HEADER {
            if remaining.iter().all(|byte| *byte == 0) {
                break;
            }
            close_all(&fds);
            return Err("truncated ancillary header".into());
        }
        let length = match read_usize(remaining) {
            Ok(value) => value,
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        if length < CMSG_HEADER || length > remaining.len() {
            close_all(&fds);
            return Err(format!("invalid ancillary length {length}"));
        }
        let level = match remaining
            .get(8..12)
            .ok_or_else(|| "missing cmsg level".to_string())
        {
            Ok(bytes) => match read_i32(bytes) {
                Ok(value) => value,
                Err(error) => {
                    close_all(&fds);
                    return Err(error);
                }
            },
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        let kind = match remaining
            .get(12..16)
            .ok_or_else(|| "missing cmsg type".to_string())
        {
            Ok(bytes) => match read_i32(bytes) {
                Ok(value) => value,
                Err(error) => {
                    close_all(&fds);
                    return Err(error);
                }
            },
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        let data = match remaining.get(CMSG_HEADER..length) {
            Some(value) => value,
            None => {
                close_all(&fds);
                return Err("ancillary data escaped message".into());
            }
        };
        if level != SOL_SOCKET || kind != SCM_RIGHTS {
            refusal.get_or_insert_with(|| {
                format!("unsupported ancillary message level={level} type={kind}")
            });
        } else {
            if data.is_empty() || data.len() % 4 != 0 {
                refusal.get_or_insert_with(|| {
                    format!("invalid SCM_RIGHTS payload length {}", data.len())
                });
            }
            for raw in data.as_chunks::<4>().0 {
                let fd = i32::from_ne_bytes(*raw);
                if fd >= 0 {
                    fds.push(fd);
                } else {
                    refusal.get_or_insert_with(|| format!("received invalid descriptor {fd}"));
                }
            }
        }
        let advance = match align_cmsg(length) {
            Ok(value) => value,
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        offset = match offset.checked_add(advance) {
            Some(value) => value,
            None => {
                close_all(&fds);
                return Err("ancillary offset overflow".into());
            }
        };
        if offset > control.len() {
            if length == remaining.len() {
                break;
            }
            close_all(&fds);
            return Err("aligned ancillary message escaped buffer".into());
        }
    }
    if let Some(error) = refusal {
        close_all(&fds);
        Err(error)
    } else {
        Ok(fds)
    }
}

pub struct Received {
    pub count: usize,
    pub fds: Vec<RawFd>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Disconnected,
    TimedOut,
    Failure(String),
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiveError::Disconnected => formatter.write_str("recvmsg: Wayland peer disconnected"),
            ReceiveError::TimedOut => formatter.write_str("recvmsg: Wayland receive timed out"),
            ReceiveError::Failure(error) => formatter.write_str(error),
        }
    }
}

pub fn write_peer_disconnected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

fn sendmsg_result(value: isize) -> io::Result<Option<usize>> {
    if let Some(error) = raw_errno(value) {
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    usize::try_from(value)
        .map(Some)
        .map_err(|_| io::Error::other(format!("sendmsg returned invalid result {value}")))
}

fn receive_result(value: isize) -> Result<usize, ReceiveError> {
    if let Some(error) = raw_errno(value) {
        if error.kind() == io::ErrorKind::ConnectionReset {
            return Err(ReceiveError::Disconnected);
        }
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return Err(ReceiveError::TimedOut);
        }
        return Err(ReceiveError::Failure(format!("recvmsg: {error}")));
    }
    usize::try_from(value)
        .map_err(|_| ReceiveError::Failure(format!("recvmsg: invalid result {value}")))
}

pub fn recv_with_fds(stream: &UnixStream, bytes: &mut [u8]) -> Result<Received, ReceiveError> {
    if bytes.is_empty() {
        return Err(ReceiveError::Failure("recv buffer is empty".into()));
    }
    let mut control = ControlBuffer([0u8; CONTROL_CAPACITY]);
    let mut iov = IoVec {
        base: bytes.as_mut_ptr(),
        len: bytes.len(),
    };
    let mut message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: control.0.as_mut_ptr(),
        control_len: control.0.len(),
        flags: 0,
    };
    let count = loop {
        let result = syscall5(
            SYS_RECVMSG,
            stream.as_raw_fd() as usize,
            (&mut message as *mut MsgHdr) as usize,
            MSG_CMSG_CLOEXEC as usize,
            0,
            0,
        );
        if result != ERRNO_EINTR {
            break receive_result(result)?;
        }
    };
    let control_len = message.control_len.min(control.0.len());
    let fds =
        parse_fds(control.0.get(..control_len).ok_or_else(|| {
            ReceiveError::Failure("kernel returned invalid ancillary length".into())
        })?)
        .map_err(ReceiveError::Failure)?;
    if message.flags & MSG_CTRUNC != 0 {
        close_all(&fds);
        return Err(ReceiveError::Failure(
            "ancillary descriptor data was truncated".into(),
        ));
    }
    Ok(Received { count, fds })
}

pub fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing descriptor-only Wayland message",
        ));
    }
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to send invalid descriptor {fd}"),
        ));
    }
    let mut control = ControlBuffer([0u8; 24]);
    let cmsg_len = 20usize;
    let len_bytes = cmsg_len.to_ne_bytes();
    control
        .0
        .get_mut(..8)
        .ok_or_else(|| io::Error::other("control header is too small"))?
        .copy_from_slice(&len_bytes);
    control
        .0
        .get_mut(8..12)
        .ok_or_else(|| io::Error::other("control header is too small"))?
        .copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    control
        .0
        .get_mut(12..16)
        .ok_or_else(|| io::Error::other("control header is too small"))?
        .copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    control
        .0
        .get_mut(16..20)
        .ok_or_else(|| io::Error::other("control data is too small"))?
        .copy_from_slice(&fd.to_ne_bytes());

    let mut iov = IoVec {
        base: bytes.as_ptr() as *mut u8,
        len: bytes.len(),
    };
    let message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: control.0.as_mut_ptr(),
        control_len: control.0.len(),
        flags: 0,
    };
    let sent = loop {
        let result = syscall5(
            SYS_SENDMSG,
            stream.as_raw_fd() as usize,
            (&message as *const MsgHdr) as usize,
            0,
            0,
            0,
        );
        if let Some(sent) = sendmsg_result(result)? {
            break sent;
        }
    };
    if sent == 0 || sent > bytes.len() {
        return Err(io::Error::other(format!(
            "sendmsg returned invalid byte count {sent}"
        )));
    }
    if sent < bytes.len() {
        let tail = bytes
            .get(sent..)
            .ok_or_else(|| io::Error::other("sendmsg byte count escaped message"))?;
        let mut borrowed = stream;
        borrowed.write_all(tail)?;
    }
    Ok(())
}

/// Adopt a raw descriptor by reopening it through `/proc/self/fd/N` and closing
/// the original. Both outcomes are reported: a duplicate that leaked its source
/// is as much a failure as one that never opened.
fn reopen_and_close(fd: RawFd, options: &OpenOptions, what: &str) -> Result<File, String> {
    if fd < 0 {
        return Err(format!("invalid {what} descriptor {fd}"));
    }
    let result = options
        .open(format!("/proc/self/fd/{fd}"))
        .map_err(|e| format!("duplicate fd {fd}: {e}"));
    let close = close_raw(fd);
    match (result, close) {
        (Ok(file), Ok(())) => Ok(file),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(open), Err(close)) => Err(format!("{open}; {close}")),
    }
}

pub fn duplicate_received(fd: RawFd) -> Result<File, String> {
    reopen_and_close(fd, OpenOptions::new().read(true), "received")
}

/// Own one descriptor obtained from `SCM_RIGHTS` without reopening it.
///
/// A `/proc/self/fd` reopen creates a different open-file description and can
/// lose offsets and status flags. Selection transfer endpoints must instead
/// travel to their source exactly as the destination supplied them.
pub struct ReceivedFd {
    fd: RawFd,
}

impl ReceivedFd {
    pub fn adopt(fd: RawFd) -> Result<ReceivedFd, String> {
        if fd < 0 {
            return Err(format!("invalid received descriptor {fd}"));
        }
        Ok(ReceivedFd { fd })
    }

    /// Transfer this exact open-file description into safe `File` ownership.
    /// Selection endpoints may be pipes or sockets, so reopening through
    /// `/proc/self/fd` is neither equivalent nor guaranteed to work.
    #[allow(unsafe_code)]
    pub fn into_file(self) -> File {
        let owned = ManuallyDrop::new(self);
        // SAFETY: `ReceivedFd::adopt` accepted one live SCM_RIGHTS descriptor,
        // this consumes its sole owner, and `ManuallyDrop` prevents the raw
        // close path from running after `File` assumes that ownership.
        unsafe { File::from_raw_fd(owned.fd) }
    }
}

impl AsRawFd for ReceivedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for ReceivedFd {
    fn drop(&mut self) {
        let _ = close_raw(self.fd);
    }
}

pub fn discard_received(fds: &[RawFd]) {
    close_all(fds);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// An ordinary scanout size maps.
    #[test]
    fn a_plausible_buffer_size_is_mappable() {
        assert_eq!(mappable_length(4_096_000), Ok(4_096_000));
        assert_eq!(mappable_length(1), Ok(1));
        assert_eq!(mappable_length(MAX_DUMB_BUFFER_BYTES), Ok(268_435_456));
    }

    /// Zero is refused rather than mapped: `mmap` with a zero length fails
    /// anyway, and a zero-length region would make every later bound vacuous.
    #[test]
    fn a_zero_length_buffer_is_not_mapped() {
        let error = mappable_length(0).unwrap_err();
        assert!(error.contains("zero-length"), "{error}");
    }

    /// A device-chosen size is an allocation request against this process's
    /// address space, so an implausible one is refused BEFORE it becomes an
    /// `mmap` length -- the ceiling `MAX_DRM_OBJECTS` applies to counts,
    /// applied here to bytes.
    #[test]
    fn an_implausible_buffer_size_is_refused_before_it_becomes_a_length() {
        let error = mappable_length(MAX_DUMB_BUFFER_BYTES + 1).unwrap_err();
        assert!(error.contains("past the"), "{error}");
        let error = mappable_length(u64::MAX).unwrap_err();
        assert!(error.contains("past the"), "{error}");
    }
    use std::io::Read;
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn descriptor_round_trip_preserves_bytes_and_file() {
        let (left, right) = UnixStream::pair().unwrap();
        let path = std::env::temp_dir().join(format!(
            "td-compositor-fd-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"pixels").unwrap();
        let source = File::open(&path).unwrap();
        send_with_fd(&left, b"wayland", source.as_raw_fd()).unwrap();
        let mut bytes = [0u8; 32];
        let received = recv_with_fds(&right, &mut bytes).unwrap();
        assert_eq!(received.count, 7);
        assert_eq!(bytes.get(..7).unwrap(), b"wayland");
        assert_eq!(received.fds.len(), 1);
        let mut duplicate = duplicate_received(*received.fds.first().unwrap()).unwrap();
        let mut content = Vec::new();
        duplicate.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"pixels");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn peer_uid_is_the_connectors_kernel_credential() {
        let (left, right) = UnixStream::pair().unwrap();
        let expected = fs::metadata("/proc/self").unwrap().uid();
        assert_eq!(peer_uid(&left).unwrap(), expected);
        assert_eq!(peer_uid(&right).unwrap(), expected);
        assert_eq!(std::mem::size_of::<[u32; 3]>(), 12);
    }

    #[test]
    fn write_peer_departure_error_kinds_are_explicit() {
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::ConnectionReset] {
            assert!(write_peer_disconnected(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(!write_peer_disconnected(&io::Error::from(kind)), "{kind:?}");
        }
    }

    #[test]
    fn interrupted_sendmsg_is_retried_but_other_errors_are_not() {
        assert_eq!(sendmsg_result(ERRNO_EINTR).unwrap(), None);
        assert!(sendmsg_result(ERRNO_EBADF).is_err());
        assert_eq!(sendmsg_result(7).unwrap(), Some(7));
    }

    #[test]
    fn ancillary_buffers_are_aligned_for_message_headers() {
        let control = ControlBuffer([0u8; 24]);
        assert_eq!((control.0.as_ptr() as usize) % CMSG_ALIGN, 0);
    }

    /// Which file this is, in terms that survive being unlinked.
    fn file_identity(file: &File) -> (u64, u64) {
        let metadata = file.metadata().unwrap();
        (metadata.dev(), metadata.ino())
    }

    /// Which file descriptor NUMBER `raw` names now, if any.
    ///
    /// `stat`, never `open`: once a number is closed it belongs to the process
    /// again and a parallel test may already hold it. This suite opens
    /// `/dev/ptmx`, so that number can name a character device, where an open
    /// can block and a read need never end. Stat opens nothing and reads
    /// nothing, so it can do neither, and it answers the only question a
    /// closed descriptor raises.
    fn identity_of_number(raw: RawFd) -> Option<(u64, u64)> {
        std::fs::metadata(format!("/proc/self/fd/{raw}"))
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
    }

    fn rejected_control_closes_its_descriptors(
        mut control: Vec<u8>,
        fd_offsets: &[usize],
    ) -> String {
        let mut handed_over = Vec::new();
        for fd_offset in fd_offsets {
            let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "td-compositor-rejected-fd-test-{}-{}",
                std::process::id(),
                sequence
            ));
            // Created and opened once: nothing reads this file, only its
            // identity is used. Cleared first so that this user's own leavings,
            // from a run that crashed at this pid and sequence, do not fail
            // `create_new`. The error is discarded because a path that is not
            // ours to remove is not ours to diagnose here either.
            let _ = fs::remove_file(&path);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            fs::remove_file(path).unwrap();
            let identity = file_identity(&file);
            // A second handle, kept until the check is done. The parser owns
            // and closes `raw`, which would drop the last reference to an
            // already-unlinked file and free its inode — and a freed inode
            // number can be handed straight to the next file created, which is
            // the one thing that could make this identity name something else.
            let pin = file.try_clone().unwrap();
            let raw = file.into_raw_fd();
            control[*fd_offset..*fd_offset + 4].copy_from_slice(&raw.to_ne_bytes());
            handed_over.push((raw, identity, pin));
        }

        let error = parse_fds(&control).unwrap_err();
        for (raw, identity, pin) in handed_over {
            // A parallel test may already hold `raw`, so its availability
            // settles nothing. What must be true is that the number no longer
            // names the file the parser had to close.
            // The pin is a second handle on the same file, so its own number
            // must still name that file. A negative below means nothing if the
            // oracle is mute or answers with somebody else's.
            assert_eq!(
                identity_of_number(pin.as_raw_fd()),
                Some(identity),
                "the pin must still name its own file"
            );
            assert_ne!(
                identity_of_number(raw),
                Some(identity),
                "the parser must close the descriptor it refused"
            );
            drop(pin);
        }
        error
    }

    #[test]
    fn unsupported_ancillary_still_closes_later_rights() {
        let mut control = vec![0u8; 40];
        control[..8].copy_from_slice(&16usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&99i32.to_ne_bytes());
        control[16..24].copy_from_slice(&20usize.to_ne_bytes());
        control[24..28].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[28..32].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors(control, &[32]),
            "unsupported ancillary message level=1 type=99"
        );
    }

    #[test]
    fn invalid_rights_payload_still_closes_later_rights() {
        let mut control = vec![0u8; 48];
        control[..8].copy_from_slice(&17usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        control[24..32].copy_from_slice(&20usize.to_ne_bytes());
        control[32..36].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[36..40].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors(control, &[40]),
            "invalid SCM_RIGHTS payload length 1"
        );
    }

    #[test]
    fn invalid_descriptor_still_closes_later_rights() {
        let mut control = vec![0u8; 48];
        control[..8].copy_from_slice(&20usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        control[16..20].copy_from_slice(&(-1i32).to_ne_bytes());
        control[24..32].copy_from_slice(&20usize.to_ne_bytes());
        control[32..36].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[36..40].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors(control, &[40]),
            "received invalid descriptor -1"
        );
    }

    #[test]
    fn invalid_descriptor_between_rights_closes_both_neighbors() {
        let mut control = vec![0u8; 32];
        control[..8].copy_from_slice(&28usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        control[20..24].copy_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors(control, &[16, 24]),
            "received invalid descriptor -1"
        );
    }

    #[test]
    fn the_first_ancillary_refusal_is_the_diagnostic() {
        let mut control = [0u8; 32];
        control[..8].copy_from_slice(&16usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&99i32.to_ne_bytes());
        control[16..24].copy_from_slice(&16usize.to_ne_bytes());
        control[24..28].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[28..32].copy_from_slice(&100i32.to_ne_bytes());
        assert_eq!(
            parse_fds(&control).unwrap_err(),
            "unsupported ancillary message level=1 type=99"
        );
    }

    #[test]
    fn structural_error_overrides_a_pending_content_refusal() {
        let mut control = [0u8; 20];
        control[..8].copy_from_slice(&16usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&99i32.to_ne_bytes());
        control[16..].fill(1);
        assert_eq!(parse_fds(&control).unwrap_err(), "truncated ancillary header");
    }

    /// The kernel reads eight bytes and takes the row count from the first
    /// field. Rows and columns are the pair a swap would silently exchange, so
    /// the order is asserted in both directions against distinct values.
    #[test]
    fn winsize_words_keep_the_kernel_field_order() {
        let size = WindowSize {
            rows: 24,
            columns: 80,
            x_pixels: 3,
            y_pixels: 4,
        };
        assert_eq!(winsize_words(size), [24, 80, 3, 4]);
        assert_eq!(winsize_from_words([24, 80, 3, 4]), size);
        assert_eq!(std::mem::size_of::<[u16; 4]>(), 8);
    }

    /// The roster is the confinement: a request outside it never reaches the
    /// kernel, whatever descriptor or argument the caller composed.
    #[test]
    fn an_unreviewed_ioctl_request_is_refused_before_the_syscall() {
        let error = ioctl(0, 0x5401, 0, "TCGETS").unwrap_err();
        assert!(error.contains("refusing unreviewed ioctl request 0x5401"), "{error}");
        for request in [
            TIOCSPTLCK,
            TIOCGPTPEER,
            TIOCSWINSZ,
            TIOCGWINSZ,
            EVIOCGABS_X,
            EVIOCGABS_Y,
        ] {
            let error = ioctl(-1, request, 0, "pinned").unwrap_err();
            assert!(error.contains("invalid descriptor -1"), "{error}");
        }
    }

    /// The DRM requests share that one roster, and reach the kernel only
    /// through the retrying entry point.
    /// Build one `struct drm_event_vblank` the way the kernel writes it.
    fn flip_event(kind: u32, length: u32, user_data: u64, sequence: u32, crtc_id: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&user_data.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // tv_sec
        bytes.extend_from_slice(&0u32.to_le_bytes()); // tv_usec
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&crtc_id.to_le_bytes());
        bytes
    }

    /// The completion carries back the cookie the flip was queued with, which
    /// is the whole correlation channel.
    #[test]
    fn a_flip_completion_reports_the_cookie_it_was_queued_with() {
        let bytes = flip_event(DRM_EVENT_FLIP_COMPLETE, 32, 0xdead_beef, 41, 29);
        let (length, completion) = parse_drm_event(&bytes).expect("a whole event");
        assert_eq!(length, 32);
        assert_eq!(
            completion,
            Some(DrmFlipCompletion {
                user_data: 0xdead_beef,
                sequence: 41,
                crtc_id: 29,
            })
        );
    }

    /// A vblank shares the payload and is NOT a flip completion. Skipped by
    /// its own length rather than refused: the kernel had something else to
    /// say, which is not evidence that a flip failed.
    #[test]
    fn a_vblank_is_skipped_rather_than_read_as_a_completion() {
        let bytes = flip_event(DRM_EVENT_VBLANK_KIND, 32, 0xdead_beef, 41, 29);
        let (length, completion) = parse_drm_event(&bytes).expect("a whole event");
        assert_eq!(length, 32);
        assert_eq!(completion, None);
    }

    /// An event type this build has never heard of is stepped over by the
    /// length it declared, not by this code's idea of how long it should be.
    /// That is what lets a newer kernel deliver something unknown without the
    /// parser losing its place in the stream.
    #[test]
    fn an_unknown_event_is_stepped_over_by_its_own_length() {
        let mut bytes = vec![0u8; 48];
        bytes.splice(..4, 0x99u32.to_le_bytes());
        bytes.splice(4..8, 48u32.to_le_bytes());
        let (length, completion) = parse_drm_event(&bytes).expect("a whole event");
        assert_eq!(length, 48);
        assert_eq!(completion, None);
    }

    /// A buffer holding less than a whole event is not yet an event. On a
    /// non-blocking descriptor this is the ordinary answer between reads.
    #[test]
    fn a_partial_event_is_not_yet_an_event() {
        let bytes = flip_event(DRM_EVENT_FLIP_COMPLETE, 32, 7, 1, 29);
        assert_eq!(parse_drm_event(bytes.get(..20).unwrap_or_default()), None);
        assert_eq!(parse_drm_event(&[]), None);
        assert_eq!(parse_drm_event(&[0u8; 7]), None);
    }

    /// A truncated event that is NOT a completion must not report a length
    /// either, and this is the case the buffer-length check actually guards.
    ///
    /// A truncated completion is already caught by its own field reads running
    /// off the end. A truncated vblank is not: every field this parser reads
    /// of it is in the header, so without the length check it would answer
    /// "skip 32 bytes" over a buffer holding 20, and the caller would resume
    /// parsing in the middle of the next event. Found by a mutation that
    /// survived the partial-completion test, which is what that test was
    /// wrongly assumed to cover.
    #[test]
    fn a_partial_non_completion_does_not_report_a_length_to_skip() {
        let bytes = flip_event(DRM_EVENT_VBLANK_KIND, 32, 0x1111, 1, 29);
        assert_eq!(parse_drm_event(bytes.get(..20).unwrap_or_default()), None);
    }

    /// A length that does not cover its own header would advance the cursor by
    /// zero and spin forever. Refused rather than trusted.
    #[test]
    fn an_event_shorter_than_its_header_is_refused() {
        for absurd in [0u32, 1, 7] {
            let bytes = flip_event(DRM_EVENT_FLIP_COMPLETE, absurd, 7, 1, 29);
            assert_eq!(parse_drm_event(&bytes), None, "length {absurd} was accepted");
        }
    }

    /// One read can deliver several events, and the completion may be behind
    /// something else. Consuming by declared length is what finds it.
    #[test]
    fn a_completion_behind_another_event_is_still_found() {
        let mut stream = flip_event(DRM_EVENT_VBLANK_KIND, 32, 0x1111, 1, 29);
        stream.extend(flip_event(DRM_EVENT_FLIP_COMPLETE, 32, 0x2222, 2, 29));
        let (first, none) = parse_drm_event(&stream).expect("the vblank");
        assert_eq!((first, none), (32, None));
        let (_, completion) = parse_drm_event(stream.get(first..).unwrap_or_default())
            .expect("the completion behind it");
        assert_eq!(completion.map(|c| c.user_data), Some(0x2222));
    }

    /// `DRM_EVENT_VBLANK`, named here rather than in the production roster:
    /// this crate never asks for one, and the parser's job is only to step
    /// over it.
    const DRM_EVENT_VBLANK_KIND: u32 = 0x01;

    #[test]
    fn a_drm_request_is_rostered_and_an_unrostered_one_is_refused() {
        for request in [
            DRM_IOCTL_VERSION,
            DRM_IOCTL_MODE_GETRESOURCES,
            DRM_IOCTL_MODE_GETENCODER,
            DRM_IOCTL_MODE_GETCONNECTOR,
            DRM_IOCTL_DROP_MASTER,
            DRM_IOCTL_MODE_CREATE_DUMB,
            DRM_IOCTL_MODE_MAP_DUMB,
            DRM_IOCTL_MODE_DESTROY_DUMB,
            DRM_IOCTL_SET_MASTER,
            DRM_IOCTL_MODE_GETCRTC,
            DRM_IOCTL_MODE_SETCRTC,
            DRM_IOCTL_MODE_RMFB,
            DRM_IOCTL_MODE_ADDFB2,
        ] {
            let error = drm_ioctl(-1, request, 0, "pinned").unwrap_err();
            assert!(error.contains("invalid descriptor -1"), "{error}");
        }
        // `DRM_IOCTL_MODE_SETPLANE` and `DRM_IOCTL_MODE_ATOMIC`. The next
        // landing's, and not this one's.
        //
        // This pin has now moved twice: it named SETCRTC until the modeset
        // landing rostered it, then PAGE_FLIP until this one did. That is the
        // pin doing its job rather than drifting — a request named "not ours"
        // after it is admitted asserts nothing while still passing, so it must
        // move to something genuinely unissued each time the roster grows.
        // SETPLANE is what an overlay plane needs and ATOMIC is what replaces
        // this legacy pair; neither is issued anywhere in this crate.
        for (request, spelled) in [
            (0xc030_64b7usize, "0xc03064b7"),
            (0xc038_64bcusize, "0xc03864bc"),
        ] {
            let error = drm_ioctl(0, request, 0, "absent").unwrap_err();
            assert!(
                error.contains(&format!("refusing unreviewed ioctl request {spelled}")),
                "{error}"
            );
        }
    }

    /// `_IOC` packs the argument's SIZE into bits 16..30, and the kernel copies
    /// exactly that many bytes to and from the pointer it is given. A Rust
    /// struct that drifts from the request it is issued with is therefore an
    /// out-of-bounds kernel write through code the compiler reads as safe —
    /// the `EVIOCGABS`/`ABSINFO_WORDS` pairing, generalised to four requests.
    #[test]
    fn the_drm_requests_encode_the_structs_they_carry() {
        fn argument_size(request: usize) -> usize {
            (request >> 16) & 0x3fff
        }
        assert_eq!(
            argument_size(DRM_IOCTL_VERSION),
            std::mem::size_of::<DrmVersion>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_GETRESOURCES),
            std::mem::size_of::<DrmModeCardRes>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_GETCONNECTOR),
            std::mem::size_of::<DrmModeGetConnector>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_GETENCODER),
            std::mem::size_of::<DrmModeGetEncoder>()
        );
        // The dumb trio, which ALLOCATES. The stakes are the same and the
        // consequence is more direct: `CREATE_DUMB` is copied back out, so a
        // request understating its struct leaves `size` and `pitch` unwritten
        // and the caller maps a buffer against numbers the kernel never set.
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_CREATE_DUMB),
            std::mem::size_of::<DrmModeCreateDumb>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_MAP_DUMB),
            std::mem::size_of::<DrmModeMapDumb>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_DESTROY_DUMB),
            std::mem::size_of::<DrmModeDestroyDumb>()
        );
        // The modeset requests. `GETCRTC` and `SETCRTC` share one struct
        // because the kernel shares it, which is what makes save-and-restore
        // exact rather than field-by-field.
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_GETCRTC),
            std::mem::size_of::<DrmModeCrtc>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_SETCRTC),
            std::mem::size_of::<DrmModeCrtc>()
        );
        assert_eq!(
            argument_size(DRM_IOCTL_MODE_ADDFB2),
            std::mem::size_of::<DrmModeFbCmd2>()
        );
        // `RMFB` carries a bare `unsigned int`, not a struct.
        assert_eq!(argument_size(DRM_IOCTL_MODE_RMFB), std::mem::size_of::<u32>());
        // `SET_MASTER` is a `DRM_IO`: no argument at all, so no size field.
        assert_eq!(argument_size(DRM_IOCTL_SET_MASTER), 0);
    }

    /// The kernel's own byte counts for Linux 7.1.4 on x86-64, written out
    /// rather than derived from the request numbers.
    ///
    /// Deriving them would make the test above tautological: both sides would
    /// move together and a struct that gained a field would still agree with
    /// itself. `DrmModeInfo` is here for a reason the request numbers cannot
    /// state at all — it is never an ioctl argument, it is the ELEMENT of the
    /// array `modes_ptr` points at, so its size is the kernel's copy STRIDE.
    /// A drift there walks a 68-byte record across a differently-sized slot
    /// and reports timings assembled from two modes.
    #[test]
    fn the_drm_structs_are_the_kernels_own_sizes() {
        assert_eq!(std::mem::size_of::<DrmVersion>(), 64);
        assert_eq!(std::mem::size_of::<DrmModeCardRes>(), 64);
        assert_eq!(std::mem::size_of::<DrmModeGetConnector>(), 80);
        assert_eq!(std::mem::size_of::<DrmModeGetEncoder>(), 20);
        assert_eq!(std::mem::size_of::<DrmModeInfo>(), 68);
        assert_eq!(std::mem::align_of::<DrmModeInfo>(), 4);
        // Six `__u32` then a `__u64`: 24 bytes of words, already 8-aligned, so
        // 32 with no padding between them.
        // 104 each, and for different reasons. `drm_mode_crtc` embeds a whole
        // 68-byte modeinfo after 36 bytes of ids; `drm_mode_fb_cmd2` reaches
        // 104 only because its four `__u64` modifiers force 4 bytes of padding
        // after the twelve `__u32`s of handles, pitches and offsets. Two
        // structs of equal size whose layouts agree nowhere is exactly the
        // case a shared request number would hide.
        assert_eq!(std::mem::size_of::<DrmModeCrtc>(), 104);
        assert_eq!(std::mem::size_of::<DrmModeFbCmd2>(), 104);
        assert_eq!(std::mem::size_of::<DrmModeCreateDumb>(), 32);
        assert_eq!(std::mem::size_of::<DrmModeMapDumb>(), 16);
        assert_eq!(std::mem::size_of::<DrmModeDestroyDumb>(), 4);
    }

    /// An empty list offers the kernel no pointer at all rather than a
    /// dangling-but-aligned one, which is what `Vec::as_mut_ptr` answers for a
    /// zero-length allocation.
    #[test]
    fn an_empty_object_list_is_offered_as_a_null_pointer() {
        let mut none: Vec<u32> = Vec::new();
        assert_eq!(address_of(&mut none), 0);
        let mut some = vec![0u32; 2];
        assert_ne!(address_of(&mut some), 0);
    }

    /// A count is read from the device and then used as an allocation length.
    #[test]
    fn an_implausible_object_count_is_refused_before_it_is_allocated_from() {
        let error = drm_count(MAX_DRM_OBJECTS + 1, MAX_DRM_OBJECTS, "connectors").unwrap_err();
        assert!(error.contains("past the 64"), "{error}");
        assert_eq!(drm_count(3, MAX_DRM_OBJECTS, "connectors"), Ok(3));
        assert_eq!(drm_count(0, MAX_DRM_OBJECTS, "connectors"), Ok(0));
    }

    /// The three words this reads are adjacent and the same type, so nothing
    /// about a wrong index is observable at runtime: it is a well-formed
    /// position and range that puts every report somewhere else on the screen.
    #[test]
    fn an_absinfo_reads_its_place_and_range_from_the_first_three_words() {
        assert_eq!(
            absinfo([11, 22, 33, 44, 55, 66]),
            AbsInfo {
                value: 11,
                minimum: 22,
                maximum: 33
            }
        );
        assert_eq!(std::mem::size_of::<[i32; ABSINFO_WORDS]>(), 24);
        // The size field of an evdev request number IS that length, so the two
        // pins agree or the kernel and this buffer disagree about the copy.
        for request in [EVIOCGABS_X, EVIOCGABS_Y] {
            assert_eq!((request >> 16) & 0x3fff, ABSINFO_WORDS * 4);
        }
        assert_eq!(AbsAxis::X.request().0, EVIOCGABS_X);
        assert_eq!(AbsAxis::Y.request().0, EVIOCGABS_Y);
    }

    /// Issued for real, against a descriptor that is not an evdev device: the
    /// gate has no absolute pointer, and every assertion above is about source
    /// text that a wrapper returning a plausible range would satisfy too.
    #[test]
    fn the_absinfo_wrapper_reaches_the_kernel() {
        let file = File::open("/dev/null").unwrap();
        let error = absolute_info(&file, AbsAxis::X).unwrap_err();
        assert!(
            error.contains("EVIOCGABS(ABS_X)"),
            "the failure is not the wrapper's: {error}"
        );
        // The entry point's own refusal names the operation too, so without
        // this the test would pass just as well with the request struck off
        // the roster and no syscall issued at all.
        assert!(
            !error.contains("refusing"),
            "the request never reached the kernel: {error}"
        );
    }

    #[test]
    fn recvmsg_connection_reset_is_a_typed_disconnect() {
        assert_eq!(
            receive_result(ERRNO_ECONNRESET),
            Err(ReceiveError::Disconnected)
        );
        assert_eq!(receive_result(ERRNO_EAGAIN), Err(ReceiveError::TimedOut));
        assert!(matches!(
            receive_result(ERRNO_ECONNABORTED),
            Err(ReceiveError::Failure(_))
        ));
    }
}
