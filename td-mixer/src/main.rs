//! td-mixer: the sound panel (APPLICATIONS.md §K.5). It takes no argument
//! but these.
#![forbid(unsafe_code)]

mod window;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "td-mixer
  Open the sound panel: the playing output's level, every output, and
  each application's level, from td-audio's socket
  /run/td-audio/native. Up/Down move, Left/Right step a level by 5%,
  m mutes it, Return makes an output the one that plays.
td-mixer --preview WIDTHxHEIGHT
  Write the window's first frame as a PPM to standard output, without
  asking the daemon anything.
td-mixer --font-license
  Print the embedded font notices.

F1, or a click on the status row, lists the keys.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "td-mixer: {message}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> String {
    td_alloc::string(USAGE).unwrap_or_default()
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    match first.as_ref().and_then(|arg| arg.to_str()) {
        Some("--help" | "-h") => {
            let _ = writeln!(std::io::stdout().lock(), "{USAGE}");
            Ok(())
        }
        Some("--font-license") => {
            let _ = write!(
                std::io::stdout().lock(),
                "{}\n{}\n{}\n{}",
                td_ui::notices::FONT_PROVENANCE,
                td_ui::notices::FONT_COPYING,
                td_ui::notices::FONT_LICENSE,
                td_ui::notices::OUTLINE_FACE
            );
            Ok(())
        }
        Some("--preview") => {
            let size = args
                .next()
                .and_then(|arg| arg.into_string().ok())
                .ok_or_else(usage)?;
            let (width, height) = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
                .ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            let ppm = window::preview(width, height)?;
            std::io::stdout()
                .lock()
                .write_all(&ppm)
                .map_err(|error| td_alloc::format(format_args!("{error}")).unwrap_or_default())
        }
        Some(_) => Err(usage()),
        None => window::run(),
    }
}
