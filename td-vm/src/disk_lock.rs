use std::{fs::File, io};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-vm disk exclusion requires x86-64 Linux");

#[repr(C)]
struct Flock {
    kind: i16,
    whence: i16,
    padding: u32,
    start: i64,
    length: i64,
    pid: i32,
    tail: u32,
}
const _: () = assert!(size_of::<Flock>() == 32 && std::mem::offset_of!(Flock, pid) == 24);

enum Operation {
    Hold,
    Check,
}

pub(super) fn idle(file: &File) -> io::Result<()> {
    apply(file, Operation::Hold, 1, 0, 0)
}

pub(super) fn template(file: &File) -> io::Result<()> {
    // QEMU holds read locks to announce permissions and probes the opposite
    // bytes for conflicts. Reserve ours first so concurrent opens see them.
    apply(file, Operation::Hold, 0, 100, 1)?; // consistent read
    apply(file, Operation::Hold, 0, 201, 3)?; // deny writes and resize
    apply(file, Operation::Check, 1, 200, 1)?; // another denies reads
    apply(file, Operation::Check, 1, 101, 3) // another writes or resizes
}

#[allow(unsafe_code)]
fn apply(file: &File, operation: Operation, kind: i16, start: i64, length: i64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    let mut lock = Flock {
        kind,
        whence: 0,
        padding: 0,
        start,
        length,
        pid: 0,
        tail: 0,
    };
    let cmd = match operation {
        Operation::Hold => 37,
        Operation::Check => 36,
    };
    // SAFETY: F_OFD_SETLK and F_OFD_GETLK synchronously use one live,
    // initialized, writable x86-64 flock on this borrowed descriptor.
    if unsafe { fcntl(file.as_raw_fd(), cmd, &mut lock as *mut Flock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if matches!(operation, Operation::Check) && lock.kind != 2 {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }
    Ok(())
}
