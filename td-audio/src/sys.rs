//! The confined raw-syscall layer — the whole confined surface of this crate.
//!
//! The crate root denies the keyword and exactly one item here carries a
//! scoped `#[allow]`: `syscall5`, the `syscall`-instruction body copied from
//! `td-util/src/sys.rs`. `syscall3` forwards to it and deliberately carries no
//! allowance of its own — the tests assert that, because a second one would be
//! a second place the confinement could move to. Everything else in the crate,
//! and every other function in this module, is ordinary safe Rust. This is the
//! THIRTEENTH target-side exception `UNSAFE.md` records.
//!
//! `APPLICATIONS.md` §I rung 25 and §K.5 both said "#11", which was already
//! `td-profiler` when the audio reversal reached the ladder and is now two
//! behind: `td-portal` took #12 in the meantime. Both are corrected in the same
//! landing as this file, because a surface number that names another crate is
//! the one kind of error the roster cannot absorb — it is the roster.
//!
//! The surface is THREE syscalls. `ioctl(2)` carries the eleven PCM requests
//! `APPLICATIONS.md` §K.4 pins; `poll(2)` is how the writer waits for the
//! device to make room without spinning; and `getsockopt(2)`, restricted to the
//! one `SOL_SOCKET`/`SO_PEERCRED` pair, is how §K.5's daemon learns who
//! connected before it reads a byte from them. Nothing else: `read(2)`,
//! `write(2)`, `open(2)` and `close(2)` all ride `std`, because an ALSA PCM in
//! `SNDRV_PCM_ACCESS_RW_INTERLEAVED` mode transfers through an ioctl rather
//! than through `write(2)`, and the file is an ordinary `std::fs::File`.
//!
//! # Why the mmap machinery is absent
//!
//! §K.4's central refusal: RW mode needs no mapped ring, no status or control
//! page, no `SYNC_PTR` and no boundary arithmetic over shared memory. At 48 kHz
//! stereo `S16_LE` the copy mmap would avoid is under 200 KiB/s. So there is no
//! `mmap(2)` here, and the absence is a design decision rather than an omission
//! — the confinement tests refuse the `SYNC_PTR` request by name.
//!
//! # Why the control device is absent
//!
//! The card control devices under `/dev/snd` — the `controlC*` nodes and the
//! whole `SNDRV_CTL_IOCTL_*` universe behind them — are never opened (§K.4):
//! output volume is multiplication in the mixer, not a mixer element on a card.
//! The confinement tests refuse the `/dev/snd/control` path literal and the
//! `SNDRV_CTL_IOCTL_*` names — which is narrower than refusing the control
//! device crate-wide, and it is the accurate claim. What closes the rest is the
//! call-site pin: every use of the raw entry point is a whole pinned literal,
//! so a control ioctl cannot be added without one of those pins moving.
//!
//! # The request numbers are COMPOSED from the pinned lengths
//!
//! `_IOWR('A', 0x11, struct snd_pcm_hw_params)` is `0xC2604111`, in which
//! `608 << 16` is `0x02600000`. Writing that constant out by hand lets the
//! length this crate allocates and the length the kernel copies drift apart in
//! silence. So `ioc()` below composes each request FROM the buffer type's own
//! length: change the length, get a different request number, and the kernel's
//! dispatch answers `ENOTTY` instead of copying a size nobody intended.
//!
//! That prevents exactly one of the two failures, and §K.4 is emphatic that an
//! earlier draft claimed it covered both. The kernel copies the number of bytes
//! the request encodes through whatever pointer it is handed; it cannot know how
//! large the caller's allocation is, so a correct request number with an
//! undersized buffer is still an out-of-bounds write. The second half is
//! discharged by TYPE: each variable-length request takes one newtype whose
//! payload is a fixed-size array, and no call site sizes a buffer or composes a
//! request by hand.
//!
//! # These constants are x86-64 facts, not merely Linux facts
//!
//! `snd_pcm_uframes_t` and `snd_pcm_sframes_t` are pointer-width, so the
//! 608-byte `snd_pcm_hw_params`, the `0xC2604111` that encodes it, `DELAY`'s
//! 8-byte argument and the pointer-bearing `snd_xferi` are all different on a
//! 32-bit target — and `_IOC`'s own bit layout differs on some architectures
//! again. A second architecture that inherited these numbers would issue
//! well-formed ioctls with the wrong size field, which is the out-of-bounds
//! case above. The `compile_error!` below is what makes that a build failure
//! rather than a discovery; the lengths are derived from the types on the
//! target being built, never shared across two.

use std::io;
use std::os::fd::RawFd;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-audio's PCM layer is x86_64-linux only (raw ioctl ABI and struct layout)");

const SYS_IOCTL: usize = 16;
const SYS_POLL: usize = 7;
const SYS_GETSOCKOPT: usize = 55;

/// `_IOC_WRITE` — the caller writes, the kernel reads.
const IOC_WRITE: u32 = 1;
/// `_IOC_READ` — the kernel writes back.
const IOC_READ: u32 = 2;
/// `'A'`, the ALSA PCM ioctl type byte.
const IOC_TYPE_A: u32 = 0x41;

/// `_IOC(dir, 'A', nr, size)` in the asm-generic bit layout x86-64 uses.
///
/// `size` is a `usize` rather than a literal precisely so that every call below
/// can pass a `size_of` — see the module docs. A length that does not fit the
/// 14-bit size field would silently alias another request, so it is rejected
/// here at compile time in the only way a `const fn` can: by arithmetic that
/// cannot be reached with a bad value (`assert!` in a const context is a
/// compile error, not a runtime panic, when the argument is a constant).
const fn ioc(dir: u32, nr: u32, size: usize) -> usize {
    assert!(
        size <= 0x3fff,
        "an ioctl argument does not fit _IOC_SIZEBITS"
    );
    ((dir << 30) | ((size as u32) << 16) | (IOC_TYPE_A << 8) | nr) as usize
}

/// `struct snd_pcm_info` — 288 bytes on x86-64.
pub const PCM_INFO_LEN: usize = 288;
/// `struct snd_pcm_hw_params` — 608 bytes on x86-64 (§K.4).
pub const HW_PARAMS_LEN: usize = 608;
/// `struct snd_pcm_sw_params` — 136 bytes on x86-64.
pub const SW_PARAMS_LEN: usize = 136;
/// `struct snd_xferi` — `{ sframes_t result; void *buf; uframes_t frames; }`.
const XFERI_LEN: usize = 24;
/// `snd_pcm_sframes_t`, the whole argument of `DELAY`.
const SFRAMES_LEN: usize = 8;
/// The `int` that `PVERSION` writes back.
const INT_LEN: usize = 4;
/// `struct pollfd` — `{ int fd; short events; short revents; }`.
const POLLFD_LEN: usize = 8;
/// `struct ucred` — `{ pid_t pid; uid_t uid; gid_t gid; }`, three 32-bit
/// fields. §K.5 pins the length: "`getsockopt(2)` restricted to
/// `SOL_SOCKET`/`SO_PEERCRED` with a pinned 12-byte `[i32; 3]`."
pub const UCRED_LEN: usize = 12;
/// `SOL_SOCKET`. The level is pinned because `getsockopt` is, like `ioctl`, one
/// syscall onto a wide space of operations: the surface is the (level, option)
/// pair and not the number in `rax`.
const SOL_SOCKET: usize = 1;
/// `SO_PEERCRED`. 17 on Linux; 16 is `SO_SNDTIMEO_OLD` and 18 is `SO_PASSCRED`,
/// either of which would answer with something this daemon would then read as
/// a uid.
const SO_PEERCRED: usize = 17;

/// `SNDRV_PCM_IOCTL_PVERSION` = `0x80044100`.
const PVERSION: usize = ioc(IOC_READ, 0x00, INT_LEN);
/// `SNDRV_PCM_IOCTL_INFO` = `0x81204101`.
///
/// NOT optional, and §K.4 says why: discovery reads a device number out of
/// `/proc/asound/pcm` and turns it into a path, and `INFO` is what confirms the
/// node just opened is the playback device that file named. A daemon that skips
/// it is trusting a path string on a real machine.
const INFO: usize = ioc(IOC_READ, 0x01, PCM_INFO_LEN);
/// `SNDRV_PCM_IOCTL_HW_REFINE` = `0xC2604110`.
const HW_REFINE: usize = ioc(IOC_READ | IOC_WRITE, 0x10, HW_PARAMS_LEN);
/// `SNDRV_PCM_IOCTL_HW_PARAMS` = `0xC2604111`.
const HW_PARAMS: usize = ioc(IOC_READ | IOC_WRITE, 0x11, HW_PARAMS_LEN);
/// `SNDRV_PCM_IOCTL_SW_PARAMS` = `0xC0884113`.
const SW_PARAMS: usize = ioc(IOC_READ | IOC_WRITE, 0x13, SW_PARAMS_LEN);
/// `SNDRV_PCM_IOCTL_DELAY` = `0x80084121`.
const DELAY: usize = ioc(IOC_READ, 0x21, SFRAMES_LEN);
/// `SNDRV_PCM_IOCTL_PREPARE` = `0x00004140`.
const PREPARE: usize = ioc(0, 0x40, 0);
/// `SNDRV_PCM_IOCTL_START` = `0x00004142`.
const START: usize = ioc(0, 0x42, 0);
/// `SNDRV_PCM_IOCTL_DROP` = `0x00004143`.
const DROP: usize = ioc(0, 0x43, 0);
/// `SNDRV_PCM_IOCTL_DRAIN` = `0x00004144`.
///
/// This drains and stops the shared mixed PCM, so it is a DEVICE-shutdown call
/// and never the implementation of a per-stream Pulse `DRAIN` — §K.3 is explicit
/// that draining one client's stream this way would silence every other app.
const DRAIN: usize = ioc(0, 0x44, 0);
/// `SNDRV_PCM_IOCTL_WRITEI_FRAMES` = `0x40184150`.
const WRITEI_FRAMES: usize = ioc(IOC_WRITE, 0x50, XFERI_LEN);

/// `POLLIN` — a client wrote, or the listener has a connection waiting.
const POLLIN: i16 = 0x0001;
/// `POLLOUT` — the device has room for at least `avail_min` frames.
const POLLOUT: i16 = 0x0004;
/// `POLLERR` — an ALSA PCM signals an underrun here, not through `POLLOUT`.
const POLLERR: i16 = 0x0008;
/// `POLLHUP`.
const POLLHUP: i16 = 0x0010;
/// `POLLNVAL` — the descriptor is not open, which is a bug rather than an event.
const POLLNVAL: i16 = 0x0020;

/// `EAGAIN`: a non-blocking transfer found no room.
pub const EAGAIN: i32 = 11;
/// `EPIPE`: an underrun on playback. The stream must be prepared again.
pub const EPIPE: i32 = 32;
/// `ESTRPIPE`: the device was suspended. Recovery is `PREPARE`, same as `EPIPE`,
/// because this crate never asks for `RESUME`.
pub const ESTRPIPE: i32 = 86;

/// `struct snd_pcm_info` as opaque bytes.
///
/// The layout knowledge lives in `pcm.rs`, next to the readback that checks it;
/// this module hands the kernel a buffer and gives one back. The newtype is the
/// buffer-size half of the §K.4 obligation: `info()` cannot be called with
/// anything but 288 bytes because there is nothing else to call it with.
pub struct PcmInfo(pub [u8; PCM_INFO_LEN]);

/// `struct snd_pcm_hw_params` as opaque bytes — 608 of them, always.
pub struct HwParams(pub [u8; HW_PARAMS_LEN]);

/// `struct snd_pcm_sw_params` as opaque bytes — 136 of them, always.
pub struct SwParams(pub [u8; SW_PARAMS_LEN]);

impl PcmInfo {
    pub fn zeroed() -> Self {
        Self([0u8; PCM_INFO_LEN])
    }
}

impl HwParams {
    pub fn zeroed() -> Self {
        Self([0u8; HW_PARAMS_LEN])
    }
}

impl SwParams {
    pub fn zeroed() -> Self {
        Self([0u8; SW_PARAMS_LEN])
    }
}

/// What a `poll(2)` on a playback PCM came back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// The timeout expired with nothing ready.
    Timeout,
    /// There is room to write.
    Writable,
    /// `POLLERR`, which on an ALSA playback stream means an underrun: the
    /// caller re-prepares rather than treating it as fatal.
    Broken,
    /// `POLLHUP` or `POLLNVAL` — the device went away.
    Gone,
}

/// The single raw-syscall entry point (x86-64 SysV syscall ABI), copied from
/// `td-util/src/sys.rs`. Its body is the ONLY confined region in the crate.
/// The scoped `#[allow]` covers where that region may appear, not what may be
/// passed here —
/// this fn is safe to CALL, so its confinement is module privacy plus the typed
/// wrappers below being its only callers.
///
/// Five argument registers because `getsockopt(2)` takes five. The roster's
/// ioctls and `poll` reach it through `syscall3`, which supplies zeros: an
/// unused argument register is not a widening, and one register mapping is one
/// thing to audit rather than two.
#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax; the
    // args are plain integers or a pointer-as-usize whose pointee the caller
    // keeps live and correctly sized across the call. `options(nomem)` is
    // deliberately ABSENT and load-bearing by its absence: INFO, HW_REFINE,
    // HW_PARAMS, SW_PARAMS, DELAY, WRITEI_FRAMES, poll and getsockopt all have
    // the kernel WRITE through one of those pointers, and promising the compiler
    // this asm touches no memory would let it keep a stale buffer across the
    // call.
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

/// The three-argument form, for the roster's ioctls and `poll`.
///
/// A forwarder rather than a second entry point: two inline-assembly blocks
/// would be two register mappings to audit, and the whole value of pinning the
/// block whole is that there is one place where an argument can land in the
/// wrong register.
#[inline]
fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> isize {
    syscall5(n, a1, a2, a3, 0, 0)
}

/// Turn a raw syscall return into a `Result`, mirroring `td-util`'s `check`.
fn check(ret: isize) -> io::Result<()> {
    value(ret).map(|_| ())
}

/// The non-negative return, or the errno as an `io::Error`.
fn value(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        // A syscall errno is a small positive number; anything else is the
        // kernel returning a legitimately negative-looking large value, which
        // none of this roster does.
        let errno = ret.checked_neg().unwrap_or(i32::MAX as isize);
        let errno = i32::try_from(errno).unwrap_or(i32::MAX);
        Err(io::Error::from_raw_os_error(errno))
    } else {
        Ok(ret)
    }
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_PVERSION, &mut int)` — the PCM protocol version.
///
/// Read first and carried into `sw_params.proto`, which is what alsa-lib does
/// and what lets the kernel interpret the later fields of that struct.
pub fn pversion(fd: RawFd) -> io::Result<u32> {
    let mut out = [0u8; INT_LEN];
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        PVERSION,
        out.as_mut_ptr() as usize,
    ))?;
    Ok(u32::from_ne_bytes(out))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_INFO, &mut snd_pcm_info)`.
pub fn info(fd: RawFd, out: &mut PcmInfo) -> io::Result<()> {
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        INFO,
        out.0.as_mut_ptr() as usize,
    ))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_HW_REFINE, &mut snd_pcm_hw_params)`.
///
/// Narrows the caller's constraints against what the device can actually do and
/// writes the result back in place. This is how the daemon learns a period and
/// buffer size the hardware accepts instead of asserting one.
pub fn hw_refine(fd: RawFd, params: &mut HwParams) -> io::Result<()> {
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        HW_REFINE,
        params.0.as_mut_ptr() as usize,
    ))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &mut snd_pcm_hw_params)`.
///
/// Commits the configuration, and writes back what the kernel actually chose —
/// which the caller MUST read, because nothing observable distinguishes a mask
/// the kernel narrowed from one it honoured until the pitch is wrong (§K.4).
pub fn hw_params(fd: RawFd, params: &mut HwParams) -> io::Result<()> {
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        HW_PARAMS,
        params.0.as_mut_ptr() as usize,
    ))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &mut snd_pcm_sw_params)`.
///
/// The kernel writes `boundary` back, so this is `&mut` even though every other
/// field is an input.
pub fn sw_params(fd: RawFd, params: &mut SwParams) -> io::Result<()> {
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        SW_PARAMS,
        params.0.as_mut_ptr() as usize,
    ))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_PREPARE)` — leave `SETUP`/`XRUN` for `PREPARED`.
pub fn prepare(fd: RawFd) -> io::Result<()> {
    check(syscall3(SYS_IOCTL, fd as usize, PREPARE, 0))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_START)` — begin playback explicitly.
///
/// Explicit rather than by `start_threshold`: the mixer primes a full buffer and
/// then starts, and a corked stream resuming needs the same deterministic edge.
pub fn start(fd: RawFd) -> io::Result<()> {
    check(syscall3(SYS_IOCTL, fd as usize, START, 0))
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_DROP)` — stop now and discard what is queued.
pub fn drop_pcm(fd: RawFd) -> io::Result<()> {
    check(syscall3(SYS_IOCTL, fd as usize, DROP, 0))
}

/// What `DRAIN` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Draining {
    /// The device played out and stopped before the ioctl returned.
    Finished,
    /// The stream moved to `DRAINING` and the caller must wait for it. This is
    /// what a NON-BLOCKING descriptor gets, always.
    Started,
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_DRAIN)` — play out what is queued, then stop.
///
/// Device shutdown only. See the constant's own note.
///
/// `EAGAIN` is not a failure here and this is the trap: the kernel's
/// `snd_pcm_drain` moves the stream to `SNDRV_PCM_STATE_DRAINING` and then, if
/// the descriptor is non-blocking — which this daemon's always is, because §K.4
/// pairs RW transfers with `poll` — returns `-EAGAIN` rather than waiting. A
/// caller that treated that as an error would end every successful run with a
/// failure and close the descriptor while the tail was still playing, which
/// truncates exactly the audio `DRAIN` was asked to preserve.
pub fn drain(fd: RawFd) -> io::Result<Draining> {
    match check(syscall3(SYS_IOCTL, fd as usize, DRAIN, 0)) {
        Ok(()) => Ok(Draining::Finished),
        Err(error) if error.raw_os_error() == Some(EAGAIN) => Ok(Draining::Started),
        Err(error) => Err(error),
    }
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_DELAY, &mut sframes)` — frames still ahead of the
/// last written one, in the kernel buffer and the device.
///
/// This is the hardware term of the §K.3 latency sum, and the only one this
/// module can answer. It is negative on some devices when the stream has
/// underrun, so the caller gets the signed value rather than a clamped one.
pub fn delay(fd: RawFd) -> io::Result<i64> {
    let mut out = [0u8; SFRAMES_LEN];
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        DELAY,
        out.as_mut_ptr() as usize,
    ))?;
    Ok(i64::from_ne_bytes(out))
}

/// The size of one frame, as the KERNEL reported it.
///
/// A newtype with a private field, and the only way to make one is
/// [`FrameBytes::from_frame_bits`], which takes the `frame_bits` interval read
/// back out of `snd_pcm_hw_params` after `HW_PARAMS` succeeded. That matters
/// for memory safety rather than for tidiness: `writei` bounds its slice with
/// `frames * frame_bytes`, but the kernel reads `frames * <the size configured
/// on this fd>`. A caller that could pass any `usize` could pass a size smaller
/// than the device's and get a kernel read past the end of the buffer while
/// every check here passed. Deriving it from the confirmed parameters is what
/// makes the two the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameBytes(usize);

impl FrameBytes {
    /// From the `frame_bits` the device reported. `None` if that is not a whole
    /// number of bytes, or is zero — neither is a frame size a transfer could
    /// be bounded by.
    pub fn from_frame_bits(frame_bits: u32) -> Option<Self> {
        if frame_bits == 0 || !frame_bits.is_multiple_of(8) {
            return None;
        }
        Some(Self(usize::try_from(frame_bits / 8).ok()?))
    }

    pub fn get(self) -> usize {
        self.0
    }
}

/// `ioctl(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &snd_xferi)` — interleaved write.
///
/// `frames`, not bytes: the kernel is told a frame count and reads
/// `frames * frame_bytes` through `buf`, so the caller's slice must be at least
/// that long. That is checked here rather than trusted, because getting it wrong
/// is a kernel read past the end of a Rust allocation — and the frame size is a
/// [`FrameBytes`], so the number checked against is the one the device
/// confirmed rather than one the caller chose.
///
/// Returns the frames actually accepted, which in non-blocking mode is fewer
/// than asked for whenever the ring filled.
///
/// The count comes out of `xferi.result`, NOT out of the ioctl return. The
/// kernel's handler ends `__put_user(result, &_xferi->result); return result < 0
/// ? result : 0;` — so a successful short write returns 0 from the syscall and
/// reports the frames through the struct. Reading the return value instead would
/// make every successful write look like zero frames accepted, and the writer
/// would spin forever re-offering audio the device had already taken. That also
/// makes this an `_IOW` the kernel writes back through, which is why the buffer
/// is passed as a mutable pointer.
pub fn writei(fd: RawFd, frame_bytes: FrameBytes, buf: &[u8], frames: usize) -> io::Result<usize> {
    let needed = frames
        .checked_mul(frame_bytes.get())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame count overflows"))?;
    if needed > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WRITEI_FRAMES would read past the end of the transfer buffer",
        ));
    }
    let mut xferi = [0u8; XFERI_LEN];
    // result at 0 (kernel-written), buf at 8, frames at 16.
    write_usize(&mut xferi, 8, buf.as_ptr() as usize)?;
    write_usize(&mut xferi, 16, frames)?;
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        WRITEI_FRAMES,
        xferi.as_mut_ptr() as usize,
    ))?;
    accepted_frames(read_i64(&xferi, 0)?, frames)
}

/// Validate the kernel-written transfer result before it reaches any clock.
fn accepted_frames(result: i64, offered: usize) -> io::Result<usize> {
    let accepted = usize::try_from(result)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative accepted frame count"))?;
    if accepted > offered {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WRITEI_FRAMES accepted more frames than were offered",
        ));
    }
    Ok(accepted)
}

/// `poll(&pollfd, 1, timeout_ms)` on a playback PCM.
///
/// The kernel paces the writer here: RW mode plus `poll` is the whole of §K.4's
/// "no mmap" claim, and this is the wait half of it.
pub fn poll_writable(fd: RawFd, timeout_ms: i32) -> io::Result<Ready> {
    let mut pollfd = [0u8; POLLFD_LEN];
    write_i32(&mut pollfd, 0, fd)?;
    write_i16(&mut pollfd, 4, POLLOUT)?;
    let ready = value(syscall3(
        SYS_POLL,
        pollfd.as_mut_ptr() as usize,
        1,
        timeout_ms as usize,
    ))?;
    if ready == 0 {
        return Ok(Ready::Timeout);
    }
    Ok(classify(read_i16(&pollfd, 6)?))
}

/// Turn `revents` into an outcome.
///
/// Separated from the syscall because the interesting cases are the ones a test
/// cannot conjure safely. `POLLNVAL` needs a closed descriptor, and a descriptor
/// number that was closed can be reopened by another thread between the close
/// and the poll — so the mapping is proved here, exhaustively, and the syscall
/// is proved separately against a descriptor whose answer is known.
///
/// Order matters: a PCM that has both underrun and been unplugged is gone, and
/// an underrun that also reports `POLLOUT` is still an underrun, because the
/// room the kernel is offering is in a stream that has stopped.
fn classify(revents: i16) -> Ready {
    if revents & (POLLNVAL | POLLHUP) != 0 {
        return Ready::Gone;
    }
    if revents & POLLERR != 0 {
        return Ready::Broken;
    }
    if revents & POLLOUT != 0 {
        return Ready::Writable;
    }
    Ready::Timeout
}

/// Who is on the other end of a connected Unix socket.
///
/// §K.5 makes this the authorization decision: "the **directory is traversable
/// (0755, owned by `audio`) and the socket is 0666**, with authorization done
/// by the daemon on `SO_PEERCRED` — accept uid 1000 and the audio uid, refuse
/// everything else — rather than by mode bits. That puts the decision in code
/// that can say why it refused."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// `getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &mut ucred, &mut len)`.
///
/// The kernel sets these from the peer's credentials at `connect(2)` time and a
/// process cannot forge them, which is exactly why §K.3 authenticates this way
/// rather than by the cookie — a cookie is a file any process that can read it
/// can replay.
///
/// The returned length is checked, not assumed. `getsockopt` writes back how
/// much it filled, and a kernel that filled less would leave the tail of the
/// buffer at its initial zeros — which would read as uid 0 if the field
/// happened to be there, and uid 0 is the one answer that must never be
/// invented.
pub fn peer_credentials(fd: RawFd) -> io::Result<Peer> {
    let mut cred = [0u8; UCRED_LEN];
    let mut len = (UCRED_LEN as u32).to_ne_bytes();
    check(syscall5(
        SYS_GETSOCKOPT,
        fd as usize,
        SOL_SOCKET,
        SO_PEERCRED,
        cred.as_mut_ptr() as usize,
        len.as_mut_ptr() as usize,
    ))?;
    if usize::try_from(u32::from_ne_bytes(len)).unwrap_or(0) != UCRED_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned a short credential structure",
        ));
    }
    Ok(Peer {
        pid: read_i32(&cred, 0)?,
        uid: read_u32(&cred, 4)?,
        gid: read_u32(&cred, 8)?,
    })
}

/// What one descriptor in a `PollSet` is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
    pub readable: bool,
    pub writable: bool,
}

impl Interest {
    pub const READ: Self = Self {
        readable: true,
        writable: false,
    };
    pub const WRITE: Self = Self {
        readable: false,
        writable: true,
    };
    pub const BOTH: Self = Self {
        readable: true,
        writable: true,
    };

    fn events(self) -> i16 {
        let mut events = 0;
        if self.readable {
            events |= POLLIN;
        }
        if self.writable {
            events |= POLLOUT;
        }
        events
    }
}

/// What one descriptor came back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Readiness {
    pub readable: bool,
    pub writable: bool,
    /// `POLLERR` — on a PCM this is an underrun; on a socket it is an error the
    /// next read will report properly.
    pub errored: bool,
    /// `POLLHUP` or `POLLNVAL` — the other end is gone.
    pub gone: bool,
}

/// A reusable `poll(2)` argument array.
///
/// The daemon waits on the listener, every client, and the PCM at once, so the
/// one-descriptor `poll_writable` above is not enough. The byte buffer is owned
/// and reused rather than built per wait: this runs once per period, and
/// `AGENTS.md` asks for allocation outside hot loops.
///
/// No `#[repr(C)]` struct is used for `struct pollfd`. The layout is written
/// field by field at pinned offsets, the same way every ioctl argument in this
/// module is, so the crate makes no assumption a compiler is free to break.
#[derive(Debug, Default)]
pub struct PollSet {
    buffer: Vec<u8>,
    count: usize,
}

impl PollSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.count = 0;
    }

    /// Add a descriptor. Its index in the set is the index `readiness` answers.
    pub fn push(&mut self, fd: RawFd, interest: Interest) -> io::Result<usize> {
        let at = self.buffer.len();
        self.buffer.resize(at.saturating_add(POLLFD_LEN), 0);
        write_i32(&mut self.buffer, at, fd)?;
        write_i16(&mut self.buffer, at.saturating_add(4), interest.events())?;
        write_i16(&mut self.buffer, at.saturating_add(6), 0)?;
        let index = self.count;
        self.count = self.count.saturating_add(1);
        Ok(index)
    }

    /// Wait. Returns how many descriptors are ready; 0 is a timeout.
    ///
    /// `EINTR` is a timeout rather than an error: a signal arriving during the
    /// wait means the caller should go round the loop again, and turning it
    /// into an `Err` would tear down every client for a `SIGWINCH`.
    pub fn wait(&mut self, timeout_ms: i32) -> io::Result<usize> {
        if self.count == 0 {
            return Ok(0);
        }
        let ready = syscall3(
            SYS_POLL,
            self.buffer.as_mut_ptr() as usize,
            self.count,
            timeout_ms as usize,
        );
        match value(ready) {
            Ok(ready) => Ok(usize::try_from(ready).unwrap_or(0)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(error) => Err(error),
        }
    }

    /// What the descriptor at `index` came back with. An index that was never
    /// pushed is not ready, which is the answer that makes a caller skip it.
    pub fn readiness(&self, index: usize) -> Readiness {
        let at = index.saturating_mul(POLLFD_LEN).saturating_add(6);
        let Ok(revents) = read_i16(&self.buffer, at) else {
            return Readiness::default();
        };
        Readiness {
            readable: revents & POLLIN != 0,
            writable: revents & POLLOUT != 0,
            errored: revents & POLLERR != 0,
            gone: revents & (POLLHUP | POLLNVAL) != 0,
        }
    }
}

fn out_of_bounds() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "syscall argument offset is out of bounds",
    )
}

fn write_usize(bytes: &mut [u8], at: usize, value: usize) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(8))
        .ok_or_else(out_of_bounds)?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn write_i32(bytes: &mut [u8], at: usize, value: i32) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(4))
        .ok_or_else(out_of_bounds)?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn write_i16(bytes: &mut [u8], at: usize, value: i16) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(2))
        .ok_or_else(out_of_bounds)?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn read_i16(bytes: &[u8], at: usize) -> io::Result<i16> {
    let slice = bytes
        .get(at..at.saturating_add(2))
        .ok_or_else(out_of_bounds)?;
    let array: [u8; 2] = slice
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short read"))?;
    Ok(i16::from_ne_bytes(array))
}

fn read_i32(bytes: &[u8], at: usize) -> io::Result<i32> {
    let slice = bytes
        .get(at..at.saturating_add(4))
        .ok_or_else(out_of_bounds)?;
    let array: [u8; 4] = slice
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short read"))?;
    Ok(i32::from_ne_bytes(array))
}

fn read_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let slice = bytes
        .get(at..at.saturating_add(4))
        .ok_or_else(out_of_bounds)?;
    let array: [u8; 4] = slice
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short read"))?;
    Ok(u32::from_ne_bytes(array))
}

fn read_i64(bytes: &[u8], at: usize) -> io::Result<i64> {
    let slice = bytes
        .get(at..at.saturating_add(8))
        .ok_or_else(out_of_bounds)?;
    let array: [u8; 8] = slice
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short read"))?;
    Ok(i64::from_ne_bytes(array))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::fd::AsRawFd;

    /// The composed request numbers equal the values the pinned UAPI header
    /// produces, transcribed from a compile of `<sound/asound.h>`.
    ///
    /// Composition (see the module docs) makes the length and the request
    /// impossible to drift apart; this makes a WRONG length impossible to hold
    /// still. Both halves are needed: `HW_PARAMS_LEN = 600` composes a perfectly
    /// consistent `0xC2584111` that the kernel would answer `ENOTTY`, which is a
    /// far worse day than a failing assertion here.
    #[test]
    fn every_request_number_matches_the_uapi_header() {
        assert_eq!(PVERSION, 0x8004_4100);
        assert_eq!(INFO, 0x8120_4101);
        assert_eq!(HW_REFINE, 0xc260_4110);
        assert_eq!(HW_PARAMS, 0xc260_4111);
        assert_eq!(SW_PARAMS, 0xc088_4113);
        assert_eq!(PREPARE, 0x0000_4140);
        assert_eq!(START, 0x0000_4142);
        assert_eq!(DROP, 0x0000_4143);
        assert_eq!(DRAIN, 0x0000_4144);
        assert_eq!(DELAY, 0x8008_4121);
        assert_eq!(WRITEI_FRAMES, 0x4018_4150);
    }

    /// The struct lengths, pinned on their own.
    ///
    /// The test above would also catch a wrong length, but only as a wrong
    /// request number, which reads as an ioctl-numbering bug rather than as what
    /// it is: this crate and the kernel disagreeing about how many bytes cross
    /// the boundary. `608` is the one §K.4 derives field by field.
    #[test]
    fn the_struct_lengths_are_the_x86_64_ones() {
        assert_eq!(PCM_INFO_LEN, 288);
        assert_eq!(HW_PARAMS_LEN, 608);
        assert_eq!(SW_PARAMS_LEN, 136);
        assert_eq!(XFERI_LEN, 24);
        assert_eq!(SFRAMES_LEN, std::mem::size_of::<usize>());
        assert_eq!(PCM_INFO_LEN, std::mem::size_of::<PcmInfo>());
        assert_eq!(HW_PARAMS_LEN, std::mem::size_of::<HwParams>());
        assert_eq!(SW_PARAMS_LEN, std::mem::size_of::<SwParams>());
    }

    /// The size field really is derived: recomposing with a different length
    /// produces a different request, which is the property the composition buys.
    #[test]
    fn a_changed_length_changes_the_request_number() {
        assert_ne!(
            ioc(IOC_READ | IOC_WRITE, 0x11, HW_PARAMS_LEN),
            ioc(IOC_READ | IOC_WRITE, 0x11, HW_PARAMS_LEN - 8)
        );
        assert_eq!(ioc(IOC_READ | IOC_WRITE, 0x11, 600), 0xc258_4111);
    }

    /// The syscalls are really ISSUED, and really fail for a non-PCM file.
    ///
    /// Every other assertion about this module is about source TEXT or arithmetic;
    /// a wrapper that returned `Ok(())` without issuing anything would satisfy all
    /// of them. A regular file is not a PCM, so the kernel answers ENOTTY — which
    /// proves the request reached it.
    #[test]
    fn the_ioctl_is_issued_and_the_kernel_answers() {
        let path = std::env::temp_dir().join(format!("td-audio-sys-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let fd = file.as_raw_fd();
        let err = pversion(fd).unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(25),
            "a regular file must answer ENOTTY (25), not {err}"
        );
        let mut params = HwParams::zeroed();
        assert_eq!(
            hw_params(fd, &mut params).unwrap_err().raw_os_error(),
            Some(25)
        );
        assert_eq!(prepare(fd).unwrap_err().raw_os_error(), Some(25));
        assert_eq!(delay(fd).unwrap_err().raw_os_error(), Some(25));
        let _ = std::fs::remove_file(&path);
    }

    /// A bad descriptor is EBADF, so the fd argument lands in the right register.
    #[test]
    fn the_descriptor_argument_reaches_the_kernel() {
        assert_eq!(pversion(-1).unwrap_err().raw_os_error(), Some(9));
        assert_eq!(start(-1).unwrap_err().raw_os_error(), Some(9));
    }

    /// `poll(2)` is issued, and a regular file is always writable.
    ///
    /// This is the half that needs the kernel: a `Writable` answer here can
    /// only come from `revents` actually carrying `POLLOUT`, which proves both
    /// that the request left and that the field is read at offset 6 rather than
    /// at `events`' offset 4.
    #[test]
    fn poll_is_issued_and_answers_for_a_real_descriptor() {
        let path = std::env::temp_dir().join(format!("td-audio-poll-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        assert_eq!(poll_writable(file.as_raw_fd(), 0).unwrap(), Ready::Writable);
        let _ = std::fs::remove_file(&path);
        // A NEGATIVE descriptor is not an error and not POLLNVAL: `poll(2)`
        // ignores it and reports nothing ready. Asserted because the obvious
        // expectation is `Gone`, and a caller that treated a timeout as a dead
        // device would tear the daemon down over a descriptor bug.
        assert_eq!(poll_writable(-1, 0).unwrap(), Ready::Timeout);
    }

    /// Every `revents` combination, mapped.
    #[test]
    fn the_poll_outcome_is_decided_by_precedence_not_by_luck() {
        assert_eq!(classify(0), Ready::Timeout);
        assert_eq!(classify(POLLOUT), Ready::Writable);
        assert_eq!(classify(POLLERR), Ready::Broken);
        assert_eq!(classify(POLLHUP), Ready::Gone);
        assert_eq!(classify(POLLNVAL), Ready::Gone);
        // An ALSA underrun sets POLLERR alongside POLLOUT: the room being
        // offered belongs to a stream that has stopped, so it is an underrun.
        assert_eq!(classify(POLLOUT | POLLERR), Ready::Broken);
        // ...and a device that is gone is gone, whatever else it says.
        assert_eq!(classify(POLLOUT | POLLERR | POLLHUP), Ready::Gone);
        // Nothing this daemon does not ask for is mistaken for readiness.
        assert_eq!(
            classify(0x0001),
            Ready::Timeout,
            "POLLIN is not writability"
        );
    }

    /// `writei` refuses a frame count its buffer cannot back, BEFORE the kernel
    /// is handed a pointer. This is the buffer-size half of §K.4's obligation
    /// for the one request whose length is not fixed by its type.
    #[test]
    fn writei_refuses_to_let_the_kernel_read_past_the_buffer() {
        let buf = [0u8; 16];
        let stereo_s16 = FrameBytes::from_frame_bits(32).unwrap();
        let err = writei(-1, stereo_s16, &buf, 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("past the end"));
        // Exactly-fitting is allowed through to the kernel, which answers EBADF.
        assert_eq!(
            writei(-1, stereo_s16, &buf, 4).unwrap_err().raw_os_error(),
            Some(9)
        );
        // ...and an overflowing product is refused rather than wrapped. The
        // largest frame this type can describe is still small enough that the
        // multiplication needs an absurd frame count to overflow, which is
        // exactly the point: the check cannot be defeated by a caller choosing
        // the size.
        let widest = FrameBytes::from_frame_bits(u32::MAX - 7).unwrap();
        assert!(writei(-1, widest, &buf, usize::MAX / 2).is_err());
    }

    #[test]
    fn writei_refuses_an_impossible_kernel_result() {
        assert_eq!(accepted_frames(0, 4).unwrap(), 0);
        assert_eq!(accepted_frames(4, 4).unwrap(), 4);
        assert_eq!(
            accepted_frames(-1, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            accepted_frames(5, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    /// The frame size can only be built from what the DEVICE reported, and two
    /// classes of readback are refused outright: zero, and a frame that is not
    /// a whole number of bytes.
    ///
    /// Not every implausible width — a device claiming a 536 MB frame is
    /// accepted, because the type's job is to keep `writei`'s bound and the
    /// kernel's own read length the SAME number, not to second-guess a
    /// readback. An absurd width makes the transfer fail; a width that
    /// disagreed with the kernel's would not.
    ///
    /// This is the type that keeps `writei`'s bound and the kernel's own read
    /// length the same number. A plain `usize` parameter let a caller pass a
    /// size smaller than the configured one, which passes every check here and
    /// still has the kernel read past the end of the buffer.
    #[test]
    fn a_frame_size_can_only_come_from_the_devices_own_answer() {
        assert_eq!(
            FrameBytes::from_frame_bits(32).map(FrameBytes::get),
            Some(4)
        );
        assert_eq!(
            FrameBytes::from_frame_bits(16).map(FrameBytes::get),
            Some(2)
        );
        assert_eq!(FrameBytes::from_frame_bits(8).map(FrameBytes::get), Some(1));
        // Zero is not a frame size: every transfer bound would be zero and the
        // kernel would read whatever the device's real size says.
        assert_eq!(FrameBytes::from_frame_bits(0), None);
        // Nor is a partial byte. A 24-bit sample lives in a 32-bit frame slot;
        // a frame that is not a whole number of bytes is a readback this daemon
        // does not understand, and guessing would set the bound wrong.
        assert_eq!(FrameBytes::from_frame_bits(12), None);
        assert_eq!(FrameBytes::from_frame_bits(20), None);
    }
}
