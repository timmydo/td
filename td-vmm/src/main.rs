#![deny(unsafe_code)]
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-vmm requires x86-64 Linux");

mod block;
mod boot;
mod kvm;
mod serial;

use std::{env, fs::File, io, os::unix::ffi::OsStrExt, process::ExitCode};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let usage = || {
        invalid(
            "usage: td-vmm KERNEL INITRAMFS 'kernel command line' [--disk-ro RAW | --disk-rw RAW]",
        )
    };
    let kernel = args.next().ok_or_else(usage)?;
    let initrd = args.next().ok_or_else(usage)?;
    let append = args.next().ok_or_else(usage)?;
    let disk = match args.next() {
        None => None,
        Some(mode) => {
            let write = if mode == "--disk-ro" {
                false
            } else if mode == "--disk-rw" {
                true
            } else {
                return Err(usage());
            };
            let path = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            Some(block::Block::open(path.as_ref(), write)?)
        }
    };
    let mut memory = kvm::Ram::new()?;
    let entry = boot::load(
        memory.bytes(),
        File::open(kernel)?,
        File::open(initrd)?,
        append.as_bytes(),
    )?;
    kvm::execute(memory, entry, disk)
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
