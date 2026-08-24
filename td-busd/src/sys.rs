//! The raw-syscall layer, `UNSAFE.md` surface #10.
//!
//! Everything a broker does to a Unix socket that stable `std` already does
//! safely is done with `std`: `socket`, `bind`, `listen`, `accept`, and byte
//! I/O are `UnixListener`/`UnixStream`. Three things are left over, and they
//! are the whole of this module.
//!
//! * `recvmsg(2)` and `sendmsg(2)`, because stable Rust exposes no ancillary
//!   data API and SCM_RIGHTS is how D-Bus passes a descriptor at all.
//! * `getsockopt(2)`, for two value-pinned options and nothing else.
//!   `SO_PEERCRED` is the uid `EXTERNAL` admits by: `UnixStream::peer_cred`
//!   exists and is unstable (`peer_credentials_unix_socket`, rust#42839), and
//!   td builds on a pinned toolchain but ships no feature gates, so the
//!   credential this broker's identity model rests on is read here instead.
//!   `SO_PEERPIDFD` is the peer's own identity, and stable `std` has no
//!   spelling of it at all.
//!
//! `close(2)` is deliberately NOT here, and its absence is the point of the
//! second allow below. §D's draft roster carried it from an earlier design that
//! held a received descriptor as a bare integer in a hand-rolled owner whose
//! `Drop` closed it — which §D then rejected, because a hand-rolled owner is a
//! fresh chance at a double close in a type `std` already ships correct. Taking
//! the `OwnedFd` instead means `std` does every close, so the syscall roster is
//! three rather than four.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// x86-64 syscall numbers. td targets one architecture; a second would need
/// these per-target, which is an amendment rather than a `cfg`.
const SYS_SENDMSG: usize = 46;
const SYS_RECVMSG: usize = 47;
const SYS_GETSOCKOPT: usize = 55;

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;
/// `SO_PEERPIDFD`, Linux 6.5 and later; td pins a 7.x kernel. A kernel without
/// it answers `ENOPROTOOPT`, which `lineage` reads as an identity it could not
/// establish — so the old-kernel case denies rather than falling open.
const SO_PEERPIDFD: i32 = 77;
const SCM_RIGHTS: i32 = 1;

/// The kernel had more ancillary data than the control buffer could hold. The
/// delivery is PARTIAL, not absent: what fit is installed in this process like
/// any other descriptor, and only the excess is closed by the kernel.
const MSG_CTRUNC: i32 = 0x08;
/// Received descriptors are close-on-exec from the moment they exist. Without
/// it a descriptor is briefly inheritable, and this process starts no children,
/// which is exactly the kind of "cannot happen today" that stops being true.
const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;
/// A write to a peer that has gone away returns `EPIPE` rather than raising
/// `SIGPIPE`. Rust's runtime already ignores that signal, so this is the second
/// of two reasons the broker does not die when a client disconnects mid-reply;
/// it is pinned because relying on the runtime alone leaves the guarantee
/// somewhere this file cannot see.
const MSG_NOSIGNAL: i32 = 0x4000;

const ERRNO_EINTR: isize = -4;

/// `struct ucred`: `pid`, `uid`, `gid`, three native `i32`. The length is
/// passed to the kernel and read back, because `getsockopt` writes exactly
/// `sizeof(struct ucred)` through the pointer and a short buffer would be a
/// kernel write past the end of it.
const UCRED_WORDS: usize = 3;

/// The most descriptors one message may carry, and the size of the control
/// buffer that receives them. D-Bus's own limit is 16; this is deliberately
/// larger so that a message over the limit is REFUSED by the broker's own
/// accounting rather than silently truncated by a buffer, which is the
/// difference between an error and a `MSG_CTRUNC` mystery.
pub const MAX_FDS: usize = 64;

const CMSG_HEADER: usize = 16;
const CMSG_ALIGN: usize = 8;
const CONTROL_CAPACITY: usize = CMSG_HEADER + MAX_FDS * 4;

#[repr(align(8))]
struct ControlBuffer([u8; CONTROL_CAPACITY]);

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

/// The only raw-syscall instruction in td-busd. Five arguments because
/// `getsockopt` takes five; the two message calls pass zero for the rest.
/// `options(nomem)` and `options(readonly)` are deliberately ABSENT, and are
/// load-bearing by their absence — the same statement `td-util`, `td-sh` and
/// `td-jail` make about their own bodies.
///
/// Nothing in Rust ever writes `MsgHdr::flags` or `MsgHdr::control_len` after
/// they are initialised; the KERNEL writes them, through a pointer. Only the
/// implied memory clobber of a bare `asm!` forces the compiler to treat the
/// buffers as written.
///
/// What adding `nomem` does was MEASURED rather than reasoned about, because
/// the first draft of this comment reasoned about it and got it wrong. It
/// claimed the crate would still pass its tests while silently leaking every
/// received descriptor. It does not: in release two tests fail, the source pin
/// below and `a_truncated_descriptor_set_is_refused_without_leaking_what_arrived`,
/// the latter with EFAULT from the test's own `sendmsg` — the compiler having
/// elided the stores that set up its `MsgHdr`. In debug only the pin fails,
/// and every descriptor test still passes.
///
/// So the honest statement is narrower and still sufficient: `nomem` is a
/// miscompilation of this body, it is not caught reliably by BEHAVIOUR — a
/// debug run notices nothing — and the pin below is what makes it a failure
/// rather than a thing that works until the optimiser changes its mind.
#[inline]
#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let result: isize;
    // SAFETY: the wrapper supplies the x86-64 syscall registers. Every caller
    // below keeps the buffers it points at live across the call.
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

/// The one descriptor adoption SITE, a scoped allow of a DIFFERENT shape from
/// the syscall layer above and recorded separately in `UNSAFE.md` §10. Two
/// callers reach it; the allow stays one, which is the property the
/// confinement test pins.
///
/// td-compositor reopens a received descriptor through `/proc/self/fd/N`
/// instead, and that is unavailable here: opening a `/proc/self/fd` entry that
/// names a SOCKET fails, and a broker's freight is whatever a client sends.
///
/// # Safety
///
/// `fd` must be a descriptor the kernel has just installed in this process and
/// that nothing else owns. Two callers, and they order the adoption against
/// their refusals OPPOSITELY — see each, and `UNSAFE.md` §10. `receive` adopts
/// every number `recvmsg` returned before it examines anything. `peer_pidfd`
/// refuses first, because the number it would adopt is itself in doubt.
#[allow(unsafe_code)]
fn adopt(fd: RawFd) -> OwnedFd {
    // SAFETY: as documented above — a freshly installed descriptor, adopted
    // once, immediately, before anything can have copied the number.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn raw_errno(value: isize) -> Option<io::Error> {
    if value >= 0 {
        return None;
    }
    let raw = value
        .checked_neg()
        .and_then(|number| i32::try_from(number).ok())
        .unwrap_or(i32::MAX);
    Some(io::Error::from_raw_os_error(raw))
}

fn errno_result(value: isize, operation: &str) -> io::Result<usize> {
    if let Some(error) = raw_errno(value) {
        return Err(io::Error::new(error.kind(), format!("{operation}: {error}")));
    }
    usize::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{operation}: invalid result {value}"),
        )
    })
}

/// What the kernel says the peer is. `pid` is what §D keys a jail instance on;
/// `uid` is what `EXTERNAL` admits by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PeerCredential {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// `SO_PEERCRED`, one of the two socket options this broker reads. Each is
/// pinned at its own call site rather than taken as an argument: a wrapper
/// that accepted a level and a name would be a general `getsockopt`, and the
/// roster names two reads.
pub fn peer_credential(stream: &UnixStream) -> io::Result<PeerCredential> {
    let mut ucred = [0i32; UCRED_WORDS];
    let mut length = u32::try_from(std::mem::size_of_val(&ucred)).unwrap_or(0);
    errno_result(
        syscall5(
            SYS_GETSOCKOPT,
            stream.as_raw_fd() as usize,
            SOL_SOCKET as usize,
            SO_PEERCRED as usize,
            ucred.as_mut_ptr() as usize,
            (&mut length as *mut u32) as usize,
        ),
        "getsockopt(SO_PEERCRED)",
    )?;
    decode_ucred(&ucred, length)
}

/// Read the three words the kernel wrote, but only if it wrote all three.
///
/// Split out from the call above so a short write is testable: no kernel this
/// runs on produces one, which is exactly why the check would otherwise be a
/// branch nobody can red. Anything but the full struct means the fields are
/// not where this code thinks they are, and a uid read from the wrong offset
/// is an identity this broker would then admit by — a zeroed one reads as
/// `root`.
fn decode_ucred(ucred: &[i32; UCRED_WORDS], length: u32) -> io::Result<PeerCredential> {
    if usize::try_from(length).unwrap_or(0) != std::mem::size_of_val(ucred) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("getsockopt(SO_PEERCRED): kernel returned {length} bytes"),
        ));
    }
    let word = |index: usize| ucred.get(index).copied().unwrap_or(0);
    Ok(PeerCredential {
        pid: word(0),
        uid: word(1) as u32,
        gid: word(2) as u32,
    })
}

/// `SO_PEERPIDFD`: a handle on the process that called `connect(2)`, rather
/// than the number `SO_PEERCRED` reports for it.
///
/// That distinction is the whole reason this option is on the roster.
/// `SO_PEERCRED` samples a `struct pid` at connect and `pid_vnr` reads a
/// number off it; the reference keeps the STRUCT alive and does NOT reserve
/// the number, so a peer that connects and is then reaped can have its pid
/// handed to some other process before the broker ever looks. This returns a
/// descriptor naming the process itself, and `lineage` uses it as a liveness
/// oracle: for as long as the kernel still reports a pid for this descriptor,
/// that process has not been reaped, so its number cannot have been recycled
/// underneath a `/proc` walk.
///
/// It is an oracle and not a reservation, which is the part that is easy to
/// get wrong and was: holding this descriptor does not stop the NUMBER being
/// reused once the process is reaped. Measured, not assumed — a pidfd held
/// across a reap saw its pid number handed out again after some thirty
/// thousand forks. What the descriptor gives is the ability to ASK.
///
/// **This call refuses BEFORE it adopts, which is the inverse of `receive`,
/// and the reason is narrower than a first draft of this comment claimed.**
/// What the order buys is that `adopt` is never handed a negative number:
/// `OwnedFd` has a validity niche, and constructing one from `-1` is
/// unsound in a way that has nothing to do with which descriptor gets closed.
/// It does NOT prevent adopting a descriptor this process never received, and
/// the first draft said it did.
///
/// The number cannot be a foreign descriptor by way of a short write, because
/// `number` starts at `-1` and the kernel fills an `i32` from the low byte up
/// on this architecture: any partial write leaves the top byte `0xFF` and the
/// value negative. It CAN be a foreign descriptor if the option number is
/// wrong, because a different option answers a whole `i32` of something else
/// — a mutation to `SO_PASSPIDFD` yields `0`, and adopting stdin aborts the
/// process on the double close. No ordering and no length check catches that.
/// The value pin on `SO_PEERPIDFD` and its confinement test are what do, and
/// the first draft of this comment used that abort to justify an ordering
/// change that would not have prevented it.
///
/// The length is checked because `getsockopt` clamps to whatever the caller
/// asks for. Measured on this kernel: ask for two bytes and it writes two,
/// reports two, and **installs the pidfd anyway** — a descriptor with no
/// recoverable number, leaked. So asking for exactly `size_of::<i32>()` is
/// what keeps the answer whole, and the check is what notices a kernel that
/// answers otherwise.
pub fn peer_pidfd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut number: i32 = -1;
    let mut length = u32::try_from(std::mem::size_of_val(&number)).unwrap_or(0);
    errno_result(
        syscall5(
            SYS_GETSOCKOPT,
            stream.as_raw_fd() as usize,
            SOL_SOCKET as usize,
            SO_PEERPIDFD as usize,
            (&mut number as *mut i32) as usize,
            (&mut length as *mut u32) as usize,
        ),
        "getsockopt(SO_PEERPIDFD)",
    )?;
    // Judged first, adopted after — see the inversion above. The whole
    // judgement is one call so that it is one thing to test and one thing to
    // keep ahead of the adoption; a second condition spelled out here would be
    // a second condition with no test able to reach it.
    check_pidfd_answer(number, length)?;
    Ok(adopt(number))
}

/// Everything that has to be true before the number is a descriptor, split
/// out for the reason `decode_ucred` is split out: neither branch is one a
/// kernel this runs on takes, and a branch that cannot be redded is one nobody
/// can claim is right.
///
/// A short answer means the kernel is not the one this code was written
/// against, and nothing may be concluded from a number it only partly wrote.
/// The descriptor it installed alongside that short write is LEAKED and cannot
/// be otherwise — its number was never delivered — which is why the request
/// length is pinned at the call rather than repaired here.
///
/// The negative case is here rather than at the call site for the same reason
/// the length is: `adopt` must never see it, and a condition inlined into
/// `peer_pidfd` would be one no test could reach.
fn check_pidfd_answer(number: i32, length: u32) -> io::Result<()> {
    if usize::try_from(length).unwrap_or(0) != std::mem::size_of_val(&number) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("getsockopt(SO_PEERPIDFD): kernel returned {length} bytes"),
        ));
    }
    if number < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("getsockopt(SO_PEERPIDFD): kernel returned descriptor {number}"),
        ));
    }
    Ok(())
}

/// One `recvmsg`'s worth: the bytes read, and every descriptor that came with
/// them — already owned.
pub struct Received {
    pub count: usize,
    pub fds: Vec<OwnedFd>,
}

/// Read bytes and any descriptors accompanying them.
///
/// **Every descriptor the kernel returns is adopted before anything else
/// happens**, including before `MSG_CTRUNC` is examined. The kernel installs
/// them in this process before `recvmsg` returns, so a refusal that bailed out
/// first would leak exactly the descriptors that came with a malformed message
/// — the attacker's case. Adopted first, a refusal is an early return and the
/// `Drop`s do the rest.
pub fn receive(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<Received> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receive buffer is empty",
        ));
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
            break errno_result(result, "recvmsg")?;
        }
    };
    let used = message.control_len.min(control.0.len());
    let fds: Vec<OwnedFd> = harvest(control.0.get(..used).unwrap_or(&[]))
        .into_iter()
        .map(adopt)
        .collect();
    if message.flags & MSG_CTRUNC != 0 {
        // `fds` holds what fit and drops it here. What did not fit the kernel
        // closed itself, and neither half is the problem: the message is now a
        // lie about what accompanies it, and an `h` index into that is how a
        // caller gets a descriptor from somebody else's message.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ancillary descriptor data was truncated",
        ));
    }
    Ok(Received { count, fds })
}

/// Walk the control buffer and return every `SCM_RIGHTS` descriptor number in
/// it. Nothing here adopts: the caller does, immediately, so that the adoption
/// is one function the confinement test can name.
fn harvest(control: &[u8]) -> Vec<RawFd> {
    let mut found = Vec::new();
    let mut at = 0usize;
    while at + CMSG_HEADER <= control.len() {
        let len = match control.get(at..at + 8).and_then(|bytes| bytes.try_into().ok()) {
            Some(bytes) => usize::from_ne_bytes(bytes),
            None => break,
        };
        // `len` is eight bytes read out of the buffer, so it is whatever
        // the kernel — or, in a test, a hostile fixture — put there. The
        // stride below was already `checked_add`; this is the same arithmetic
        // one line earlier, and an `at + len` that wraps is a panic under
        // `overflow-checks` and a walk past the end without it.
        let end = match at.checked_add(len) {
            Some(end) => end,
            None => break,
        };
        if len < CMSG_HEADER || end > control.len() {
            break;
        }
        let word = |offset: usize| -> i32 {
            control
                .get(at + offset..at + offset + 4)
                .and_then(|bytes| bytes.try_into().ok())
                .map(i32::from_ne_bytes)
                .unwrap_or(-1)
        };
        if word(8) == SOL_SOCKET && word(12) == SCM_RIGHTS {
            let mut offset = at + CMSG_HEADER;
            while offset + 4 <= end {
                if let Some(bytes) = control.get(offset..offset + 4) {
                    if let Ok(four) = bytes.try_into() {
                        found.push(RawFd::from_ne_bytes(four));
                    }
                }
                offset += 4;
            }
        }
        // Advance by the aligned length, which is how the kernel laid them out.
        let stride = len.div_ceil(CMSG_ALIGN) * CMSG_ALIGN;
        match at.checked_add(stride.max(CMSG_ALIGN)) {
            Some(next) => at = next,
            None => break,
        }
    }
    found
}

/// Write bytes, attaching descriptors if any are given. Returns how many bytes
/// the kernel took, which for a large message may be fewer than offered — so a
/// caller resuming a partial write must not attach the descriptors again, and
/// `transport.rs` is where that rule lives.
pub fn send(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to send an empty frame",
        ));
    }
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to send {} descriptors at once", fds.len()),
        ));
    }
    let mut control = ControlBuffer([0u8; CONTROL_CAPACITY]);
    let payload = fds.len() * 4;
    let control_len = if fds.is_empty() { 0 } else { CMSG_HEADER + payload };
    if !fds.is_empty() {
        let header = CMSG_HEADER + payload;
        if let Some(slot) = control.0.get_mut(..8) {
            slot.copy_from_slice(&header.to_ne_bytes());
        }
        if let Some(slot) = control.0.get_mut(8..12) {
            slot.copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        }
        if let Some(slot) = control.0.get_mut(12..16) {
            slot.copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        }
        for (index, fd) in fds.iter().enumerate() {
            let at = CMSG_HEADER + index * 4;
            if let Some(slot) = control.0.get_mut(at..at + 4) {
                slot.copy_from_slice(&fd.to_ne_bytes());
            }
        }
    }
    let mut iov = IoVec {
        base: bytes.as_ptr() as *mut u8,
        len: bytes.len(),
    };
    let mut message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: if fds.is_empty() {
            std::ptr::null_mut()
        } else {
            control.0.as_mut_ptr()
        },
        control_len,
        flags: 0,
    };
    loop {
        let result = syscall5(
            SYS_SENDMSG,
            stream.as_raw_fd() as usize,
            (&mut message as *mut MsgHdr) as usize,
            MSG_NOSIGNAL as usize,
            0,
            0,
        );
        if result != ERRNO_EINTR {
            return errno_result(result, "sendmsg");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::path::Path;

    #[test]
    fn a_peer_credential_is_this_process() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let cred = peer_credential(&a).expect("SO_PEERCRED");
        assert_eq!(cred.pid, std::process::id() as i32);
        assert_eq!(cred.uid, current_uid());
        // Both ends of a pair are this process, so both answers agree. That is
        // the property the handshake rests on: the kernel's answer, not the
        // peer's.
        let other = peer_credential(&_b).expect("SO_PEERCRED");
        assert_eq!(cred, other);
    }

    /// This process's uid, read without a syscall of its own: procfs owns
    /// `/proc/self` as the process does.
    fn current_uid() -> u32 {
        let entry = std::fs::metadata("/proc/self").expect("procfs");
        std::os::unix::fs::MetadataExt::uid(&entry)
    }

    #[test]
    fn bytes_cross_without_descriptors() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        assert_eq!(send(&a, b"hello", &[]).expect("sendmsg"), 5);
        let mut buffer = [0u8; 16];
        let got = receive(&b, &mut buffer).expect("recvmsg");
        assert_eq!(got.count, 5);
        assert!(got.fds.is_empty(), "descriptors arrived uninvited");
        assert_eq!(&buffer[..5], b"hello");
    }

    #[test]
    fn a_descriptor_crosses_and_is_owned_on_arrival() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        let (mut near, far) = UnixStream::pair().expect("freight");
        send(&a, b"x", &[far.as_fd().as_raw_fd()]).expect("sendmsg");
        drop(far);

        let mut buffer = [0u8; 8];
        let got = receive(&b, &mut buffer).expect("recvmsg");
        assert_eq!(got.count, 1);
        assert_eq!(got.fds.len(), 1, "the descriptor did not arrive");

        // It is a live descriptor, not a number: write through the copy the
        // kernel installed and read it out of the original's partner.
        let mut arrived = UnixStream::from(got.fds.into_iter().next().expect("one fd"));
        arrived.write_all(b"through").expect("write");
        let mut echo = [0u8; 7];
        near.read_exact(&mut echo).expect("read");
        assert_eq!(&echo, b"through");
    }

    #[test]
    fn several_descriptors_arrive_in_order() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        let held: Vec<UnixStream> = (0..4)
            .map(|_| UnixStream::pair().expect("freight").0)
            .collect();
        let numbers: Vec<RawFd> = held.iter().map(|stream| stream.as_raw_fd()).collect();
        send(&a, b"four", &numbers).expect("sendmsg");
        let mut buffer = [0u8; 8];
        let got = receive(&b, &mut buffer).expect("recvmsg");
        assert_eq!(got.fds.len(), 4);
        // Distinct numbers: the kernel installs four descriptors, not one
        // repeated, and a harvest that mis-strides would report otherwise.
        let mut seen: Vec<RawFd> = got.fds.iter().map(|fd| fd.as_raw_fd()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 4, "the harvest folded descriptors together");
    }

    #[test]
    fn an_empty_frame_and_an_oversized_fd_set_are_refused() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        assert!(send(&a, b"", &[]).is_err(), "an empty frame was sent");
        let too_many = vec![a.as_raw_fd(); MAX_FDS + 1];
        assert!(send(&a, b"x", &too_many).is_err(), "{} sent", too_many.len());
        let mut nothing: [u8; 0] = [];
        assert!(receive(&a, &mut nothing).is_err(), "an empty read was made");
    }

    /// The control buffer holds exactly `MAX_FDS`, and the walk stops at the
    /// end of what the kernel said it wrote rather than at the end of the
    /// buffer.
    #[test]
    fn the_harvest_stops_where_the_kernel_stopped() {
        assert_eq!(CONTROL_CAPACITY, CMSG_HEADER + MAX_FDS * 4);
        assert!(harvest(&[]).is_empty());
        // A header claiming more than the slice holds yields nothing rather
        // than reading past it.
        let mut lying = [0u8; CMSG_HEADER];
        lying[..8].copy_from_slice(&(CMSG_HEADER + 4).to_ne_bytes());
        lying[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        lying[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert!(harvest(&lying).is_empty(), "walked past the buffer");
        // A length that overflows the walk's own arithmetic. The enormous
        // header is the SECOND one, because the first draft of this fixture
        // put it first — where `at` is 0, `0 + usize::MAX` does not overflow,
        // and the case it meant to build never happened. One valid cmsg ahead
        // of it moves `at` to 16, and 16 + `usize::MAX` does.
        //
        // Unchecked, that is a panic under `overflow-checks` — which AGENTS.md
        // forbids in production code — and a wrapped `end` without them.
        let mut overflowing = [0u8; CMSG_HEADER * 2 + 4];
        overflowing[..8].copy_from_slice(&CMSG_HEADER.to_ne_bytes());
        overflowing[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        overflowing[12..16].copy_from_slice(&2i32.to_ne_bytes()); // not SCM_RIGHTS
        overflowing[16..24].copy_from_slice(&usize::MAX.to_ne_bytes());
        overflowing[24..28].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        overflowing[28..32].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        overflowing[32..36].copy_from_slice(&9i32.to_ne_bytes());
        assert!(
            harvest(&overflowing).is_empty(),
            "a cmsg length that overflows the walk was mined anyway"
        );
        // A length below the header is refused rather than looped on.
        let mut short = [0u8; CMSG_HEADER];
        short[..8].copy_from_slice(&4usize.to_ne_bytes());
        assert!(harvest(&short).is_empty());
        // A header whose claimed length puts descriptor payload PAST the end
        // of the slice. Clamping the length to what is there instead of
        // refusing would mine the bytes that happen to be present and hand
        // back whatever integer they spell — here a plausible-looking 7.
        let mut overrun = [0u8; CMSG_HEADER + 4];
        overrun[..8].copy_from_slice(&(CMSG_HEADER + 8).to_ne_bytes());
        overrun[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        overrun[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        overrun[16..20].copy_from_slice(&7i32.to_ne_bytes());
        assert!(
            harvest(&overrun).is_empty(),
            "a descriptor was mined from a cmsg that overruns the buffer"
        );
    }

    /// How many descriptors this process holds open ONTO `path`.
    ///
    /// Counting all of `/proc/self/fd` instead is the obvious version and is
    /// wrong: the other tests in this binary run concurrently and open sockets
    /// and files of their own, so a process-wide count measures their churn as
    /// well. The first draft did that and failed ten times out of ten in the
    /// suite while passing ten out of ten alone.
    fn descriptors_onto(path: &Path) -> usize {
        let entries = match std::fs::read_dir("/proc/self/fd") {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| std::fs::read_link(entry.path()).ok().as_deref() == Some(path))
            .count()
    }

    /// Send `fds` in one `sendmsg` with a control buffer sized to fit them,
    /// bypassing `send`'s cap. Nothing in production may do this; the test
    /// needs a peer that oversteps, because the receiver's behaviour when one
    /// does is the property under test.
    fn send_past_the_cap(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> isize {
        let payload = fds.len() * 4;
        let mut control = vec![0u8; CMSG_HEADER + payload];
        control[..8].copy_from_slice(&(CMSG_HEADER + payload).to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        for (index, fd) in fds.iter().enumerate() {
            let at = CMSG_HEADER + index * 4;
            control[at..at + 4].copy_from_slice(&fd.to_ne_bytes());
        }
        let mut iov = IoVec {
            base: bytes.as_ptr() as *mut u8,
            len: bytes.len(),
        };
        let mut message = MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &mut iov,
            iov_len: 1,
            control: control.as_mut_ptr(),
            control_len: control.len(),
            flags: 0,
        };
        syscall5(
            SYS_SENDMSG,
            stream.as_raw_fd() as usize,
            (&mut message as *mut MsgHdr) as usize,
            MSG_NOSIGNAL as usize,
            0,
            0,
        )
    }

    /// The ordering rule of `APPLICATIONS.md` §D, measured rather than read.
    ///
    /// A peer sends more descriptors than the control buffer holds. The kernel
    /// installs the ones that fit, closes the excess, and raises `MSG_CTRUNC`.
    /// The receive must FAIL — the message is a lie about what accompanies it
    /// — and it must fail having already adopted what arrived, so this process
    /// holds no more descriptors afterwards than before. Checking the flag
    /// ahead of the adoption passes every other test in this file and leaks
    /// `MAX_FDS` descriptors per malformed message.
    #[test]
    fn a_truncated_descriptor_set_is_refused_without_leaking_what_arrived() {
        // A file only this test opens, so the count below sees this test's
        // descriptors and nobody else's.
        let path = std::env::temp_dir().join(format!(
            "td-busd-ctrunc-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"x").expect("scratch file");
        let spare = std::fs::File::open(&path).expect("open scratch");
        assert_eq!(descriptors_onto(&path), 1, "the count does not see its file");

        let (a, b) = UnixStream::pair().expect("socketpair");
        let too_many = vec![spare.as_raw_fd(); MAX_FDS + 8];
        let sent = send_past_the_cap(&b, b"x", &too_many);
        assert!(sent > 0, "the oversized send failed: {sent}");

        let mut buffer = [0u8; 16];
        let refusal = match receive(&a, &mut buffer) {
            Ok(received) => panic!("a truncated set was accepted: {}", received.count),
            Err(refusal) => refusal,
        };
        assert_eq!(refusal.kind(), io::ErrorKind::InvalidData);
        let held = descriptors_onto(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            held, 1,
            "the refusal leaked {} descriptors",
            held.saturating_sub(1)
        );
    }

    /// A kernel that wrote fewer than three words is not believed. The uid
    /// this would otherwise read is a ZERO from the untouched buffer, which is
    /// `root` — so the failure mode of skipping this is not a wrong answer but
    /// the most privileged one.
    #[test]
    fn a_short_peer_credential_is_refused_rather_than_read() {
        let full = std::mem::size_of::<[i32; UCRED_WORDS]>();
        let words = [4321i32, 1000, 1000];
        let decoded = decode_ucred(&words, full as u32).expect("a full ucred");
        assert_eq!(decoded.uid, 1000);
        assert_eq!(decoded.pid, 4321);
        for short in [0u32, 4, 8, full as u32 - 1] {
            let zeroed = [0i32; UCRED_WORDS];
            match decode_ucred(&zeroed, short) {
                Ok(credential) => panic!(
                    "{short} bytes was read as uid {}, which is root",
                    credential.uid
                ),
                Err(refusal) => assert_eq!(refusal.kind(), io::ErrorKind::InvalidData),
            }
        }
        // A LONGER answer is refused too: it means the struct grew, so the
        // three words are no longer the whole of what the kernel is saying.
        assert!(decode_ucred(&words, full as u32 + 4).is_err());
    }

    /// What the kernel has to have said before the number is treated as a
    /// descriptor.
    ///
    /// Unlike the `ucred` case above, neither branch is reachable through the
    /// real call — every kernel td runs on writes the whole `i32` and a
    /// non-negative descriptor, or fails — so the function is checked directly
    /// and the ORDER is checked by a source-level pin in `main`. Between them
    /// they cover what believing a bad answer would do: adopt an arbitrary
    /// number as a descriptor and close a socket this broker never received.
    #[test]
    fn a_pidfd_answer_the_kernel_did_not_fully_give_is_refused() {
        let full = std::mem::size_of::<i32>() as u32;
        check_pidfd_answer(9, full).expect("a whole i32 naming a descriptor");
        for short in [0u32, 1, 2, 3] {
            match check_pidfd_answer(9, short) {
                Ok(()) => panic!("{short} bytes was read as a descriptor"),
                Err(refusal) => assert_eq!(refusal.kind(), io::ErrorKind::InvalidData),
            }
        }
        // A LONGER answer means the option no longer returns what this code
        // thinks it returns.
        assert!(check_pidfd_answer(9, full + 4).is_err());
        // And a negative number is not a descriptor however long it is.
        // `adopt(-1)` would be a descriptor the kernel never installed.
        for absent in [-1i32, i32::MIN] {
            assert!(
                check_pidfd_answer(absent, full).is_err(),
                "{absent} was read as a descriptor"
            );
        }
    }

    /// Every received descriptor arrives close-on-exec. Read back from
    /// `/proc/self/fdinfo`, which is an ordinary file — asking `fcntl` would
    /// put a fourth syscall on the roster to check a flag the kernel already
    /// publishes.
    #[test]
    fn a_received_descriptor_arrives_close_on_exec() {
        const O_CLOEXEC: u64 = 0o2_000_000;
        let (a, b) = UnixStream::pair().expect("socketpair");
        let spare = std::fs::File::open("/dev/null").expect("/dev/null");
        send(&b, b"x", &[spare.as_raw_fd()]).expect("send a descriptor");
        let mut buffer = [0u8; 16];
        let received = receive(&a, &mut buffer).expect("receive");
        let fd = received.fds.first().expect("one descriptor").as_raw_fd();
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .expect("fdinfo");
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|rest| u64::from_str_radix(rest.trim(), 8).ok())
            .expect("a flags line");
        assert!(
            flags & O_CLOEXEC != 0,
            "a forwarded descriptor would survive an exec: flags {flags:o}"
        );
    }

    /// A cmsg that is not SCM_RIGHTS carries no descriptors, and must not be
    /// read as though it did. `SCM_CREDENTIALS` is the one that would arrive
    /// here in practice, and §D refuses it.
    #[test]
    fn only_scm_rights_yields_descriptors() {
        let mut other = [0u8; CMSG_HEADER + 4];
        other[..8].copy_from_slice(&(CMSG_HEADER + 4).to_ne_bytes());
        other[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        other[12..16].copy_from_slice(&2i32.to_ne_bytes()); // SCM_CREDENTIALS
        other[16..20].copy_from_slice(&7i32.to_ne_bytes());
        assert!(harvest(&other).is_empty(), "a non-SCM_RIGHTS cmsg was mined");
    }
}
