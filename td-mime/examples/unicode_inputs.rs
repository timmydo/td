#![forbid(unsafe_code)]
//! Verify the approved offline Unicode generator/test corpus.
#[path = "../tools/unicode_inputs.rs"]
mod inputs;
use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};
fn run() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = args.next().map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: unicode_inputs DIRECTORY",
        )
    })?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: unicode_inputs DIRECTORY",
        ));
    }
    let verified = inputs::load(&directory)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for input in verified {
        writeln!(
            out,
            "verified {} bytes={} sha256={}",
            input.pin.name,
            input.text.len(),
            input.pin.sha256
        )?;
    }
    out.flush()
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "unicode_inputs: {error}");
            ExitCode::FAILURE
        }
    }
}
