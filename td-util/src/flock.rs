//! File locks for shell scripts, on an inherited descriptor or around a command.
use crate::sys;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

const USAGE: &str =
    "usage: flock [-n] [-s|-x|-u] FD\n       flock [-n] [-s|-x] FILE COMMAND [ARG ...]\n";

fn diagnostic(message: impl std::fmt::Display, status: u8) -> u8 {
    let _ = writeln!(io::stderr().lock(), "flock: {message}");
    status
}

fn lock(fd: i32, mode: sys::LockMode, nonblocking: bool) -> Result<(), u8> {
    loop {
        match sys::lock_descriptor(fd, mode, nonblocking) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if nonblocking && e.kind() == io::ErrorKind::WouldBlock => return Err(1),
            Err(e) => return Err(diagnostic(e, 71)),
        }
    }
}

fn open_lock(path: &OsStr) -> io::Result<File> {
    File::open(path).or_else(|_| {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    })
}

pub fn run(mut args: std::env::ArgsOs) -> u8 {
    let mut mode = sys::LockMode::Exclusive;
    let mut nonblocking = false;
    let operand = loop {
        let Some(arg) = args.next() else {
            return diagnostic(USAGE, 64);
        };
        match arg.to_str() {
            Some("--help") => return u8::from(crate::emit(USAGE).is_err()),
            Some("--") => match args.next() {
                Some(path) => break path,
                None => return diagnostic(USAGE, 64),
            },
            Some("-n" | "--nonblock") => nonblocking = true,
            Some("-s" | "--shared") => mode = sys::LockMode::Shared,
            Some("-x" | "--exclusive") => mode = sys::LockMode::Exclusive,
            Some("-u" | "--unlock") => mode = sys::LockMode::Unlock,
            _ if arg.as_encoded_bytes().starts_with(b"-") => return diagnostic(USAGE, 64),
            _ => break arg,
        }
    };
    let Some(program) = args.next() else {
        let Some(fd) = operand
            .to_str()
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|v| *v >= 0)
        else {
            return diagnostic(USAGE, 64);
        };
        return lock(fd, mode, nonblocking).err().unwrap_or(0);
    };
    if matches!(mode, sys::LockMode::Unlock) {
        return diagnostic(USAGE, 64);
    }
    let file = match open_lock(&operand) {
        Ok(file) => file,
        Err(e) => return diagnostic(e, 66),
    };
    if let Err(code) = lock(file.as_raw_fd(), mode, nonblocking) {
        return code;
    }
    // Keep our copy through the wait. The command can also pass its copy to
    // descendants, just like a lock on an explicitly inherited descriptor.
    if let Err(e) = sys::inherit_lock(&file) {
        return diagnostic(e, 71);
    }
    match Command::new(program).args(args).status() {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(1),
        Err(e) if e.kind() == io::ErrorKind::NotFound => diagnostic(e, 127),
        Err(e) => diagnostic(e, 126),
    }
}
