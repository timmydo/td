//! td-pass: a two-pane encrypted notebook (`DESIGN.md`). A token
//! presentation re-executes this binary as td-secret's token worker, so
//! that dispatch comes first; then the system's mode is admitted and the
//! window runs.
#![forbid(unsafe_code)]

mod app;
mod backend;
mod files;
mod frames;
mod mode;
mod plain;
mod protocol;
mod window;

use std::process::ExitCode;

fn main() -> ExitCode {
    // An argument that is not text is no worker's and no window's.
    let Some(args) = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().ok())
        .collect::<Option<Vec<String>>>()
    else {
        return exit(Err("usage: td-pass".to_owned()));
    };
    if let Some(result) = td_secret::pass::worker(&args) {
        return exit(result);
    }
    if !args.is_empty() {
        return exit(Err("usage: td-pass".to_owned()));
    }
    match mode::admit(os_release().as_deref()) {
        Ok(mode::Mode::Standalone) => exit(window::run()),
        Err(reason) => exit(Err(reason.to_owned())),
    }
}

/// The system's identity: `/etc/os-release`, or `/usr/lib/os-release`
/// only when the first does not exist, as os-release specifies; any other
/// failure reads as unknown.
fn os_release() -> Option<String> {
    let mut paths = ["/etc/os-release", "/usr/lib/os-release"].into_iter();
    loop {
        let path = paths.next()?;
        match std::fs::read_to_string(path) {
            Ok(text) => return Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
}

fn exit(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("td-pass: {message}");
            ExitCode::FAILURE
        }
    }
}
