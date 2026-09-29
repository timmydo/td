//! Packaging entry point; service commands are added with their implementations.
#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str =
    "Usage: td-mta --version | --help\nService commands are not available in this build.\n";

fn run() -> io::Result<ExitCode> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let first = args.next();
    let extra = args.next().is_some();
    if !extra && first.as_deref() == Some(std::ffi::OsStr::new("--version")) {
        writeln!(io::stdout().lock(), "td-mta {}", env!("CARGO_PKG_VERSION"))?;
        return Ok(ExitCode::SUCCESS);
    }
    if !extra && first.as_deref() == Some(std::ffi::OsStr::new("--help")) {
        io::stdout().lock().write_all(HELP.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }
    io::stderr().lock().write_all(HELP.as_bytes())?;
    Ok(ExitCode::from(2))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(_) => ExitCode::FAILURE,
    }
}
