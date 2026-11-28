#![forbid(unsafe_code)]

mod transport;
mod viewer;

use std::env;
use std::process::ExitCode;
use td_remote::Host;

fn diagnostic(why: impl std::fmt::Display) -> String {
    td_alloc::format(format_args!("{why}")).unwrap_or_default()
}

fn run() -> Result<(), String> {
    let mut words = env::args_os().skip(1);
    let arg = words.next();
    if words.next().is_some() {
        return Err(diagnostic("usage: td-desktop [user@]host"));
    }
    match arg.as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--help") => {
            println!("usage: td-desktop [user@]host\nF8 opens remote apps and workspaces. F9 copies text from the desktop. F10 sends local clipboard text.");
            Ok(())
        }
        Some("--serve") => transport::serve().map_err(diagnostic),
        Some(host) => viewer::run(Host::parse(host)?),
        None => Err(diagnostic("usage: td-desktop [user@]host")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("td-desktop: {why}");
            ExitCode::FAILURE
        }
    }
}
