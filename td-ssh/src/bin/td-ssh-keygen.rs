//! `td-ssh-keygen`, also installed as `ssh-keygen`.
#![deny(unsafe_code)]

use std::ffi::OsString;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match td_ssh::keygen::run(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            td_ssh::error::diagnostic(&error.to_string());
            std::process::exit(1);
        }
    }
}
