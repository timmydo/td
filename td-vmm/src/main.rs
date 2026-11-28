#![deny(unsafe_code)]
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-vmm requires x86-64 Linux");

mod block;
mod boot;
mod kvm;
mod serial;

use std::{
    env, fs::File, io, os::unix::ffi::OsStrExt, os::unix::net::UnixDatagram, path::Path,
    process::ExitCode,
};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A process owning the instance directory may explicitly cut power. A blocked
/// vCPU still has to be stoppable after its supervisor exits.
fn listen_control(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    td_fs::check_private_socket_parent(path)?;
    let socket = UnixDatagram::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new().spawn(move || {
        let mut buf = [0; 5];
        loop {
            match socket.recv(&mut buf) {
                Ok(4) if buf.get(..4) == Some(b"stop") => std::process::exit(0),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(_) => break,
            }
        }
    })?;
    Ok(())
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let usage = || {
        invalid(
            "usage: td-vmm KERNEL INITRAMFS 'kernel command line' [--disk-ro RAW | --disk-rw RAW] [--control SOCKET] [--memory-mib 256..2048]",
        )
    };
    let kernel = args.next().ok_or_else(usage)?;
    let initrd = args.next().ok_or_else(usage)?;
    let append = args.next().ok_or_else(usage)?;
    let mut disk = None;
    let mut control = None;
    let mut memory_mib = None;
    while let Some(flag) = args.next() {
        let path = args.next().ok_or_else(usage)?;
        if (flag == "--disk-rw" || flag == "--disk-ro") && disk.is_none() {
            disk = Some(block::Block::open(path.as_ref(), flag == "--disk-rw")?);
        } else if flag == "--control" && control.is_none() {
            control = Some(path);
        } else if flag == "--memory-mib" && memory_mib.is_none() {
            let mib = path
                .to_str()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|mib| (256..=2048).contains(mib))
                .ok_or_else(usage)?;
            memory_mib = Some(mib);
        } else {
            return Err(usage());
        }
    }
    let mut memory = kvm::Ram::new(memory_mib.unwrap_or(256) << 20)?;
    let entry = boot::load(
        memory.bytes(),
        File::open(kernel)?,
        File::open(initrd)?,
        append.as_bytes(),
    )?;
    kvm::execute(memory, entry, disk, control.as_deref().map(Path::new))
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
