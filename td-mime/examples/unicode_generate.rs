#![forbid(unsafe_code)]
//! Print deterministic Rust tables from an already-provisioned Unicode corpus.
#[path = "../tools/unicode_generate.rs"]
mod generator;
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
            "usage: unicode_generate DIRECTORY",
        )
    })?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: unicode_generate DIRECTORY",
        ));
    }
    let output = generator::generate(&directory)?;
    io::stdout().lock().write_all(output.as_bytes())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "unicode_generate: {error}");
            ExitCode::FAILURE
        }
    }
}
