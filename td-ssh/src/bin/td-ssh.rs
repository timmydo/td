//! `td-ssh`, also installed as `ssh`.
#![deny(unsafe_code)]

use std::ffi::OsString;
use td_ssh::client::{self, Parsed, EXIT_FAILURE};
use td_ssh::error::{diagnostic, primary_output, Error};

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let invocation = match client::parse_args(&args) {
        Ok(Parsed::Run(invocation)) => invocation,
        Ok(Parsed::PrintAndExit(text)) => match primary_output(&text) {
            Ok(()) => std::process::exit(0),
            Err(error) => fail(&error.into()),
        },
        Err(error) => fail(&error),
    };
    match client::run(*invocation) {
        Ok(code) => std::process::exit(code),
        Err(error) => fail(&error),
    }
}

fn fail(error: &Error) -> ! {
    diagnostic(&error.to_string());
    std::process::exit(EXIT_FAILURE);
}
