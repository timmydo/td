#![deny(unsafe_code)]
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-vmm requires x86-64 Linux");

mod boot;
mod kvm;
mod serial;

use std::{env, fs::File, io, os::unix::ffi::OsStrExt, process::ExitCode};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let usage = || invalid("usage: td-vmm KERNEL INITRAMFS 'kernel command line'");
    let kernel = args.next().ok_or_else(usage)?;
    let initrd = args.next().ok_or_else(usage)?;
    let append = args.next().ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage());
    }
    let mut memory = kvm::Ram::new()?;
    let entry = boot::load(
        memory.bytes(),
        File::open(kernel)?,
        File::open(initrd)?,
        append.as_bytes(),
    )?;
    kvm::execute(memory, entry)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-vmm: {e}");
            ExitCode::FAILURE
        }
    }
}
