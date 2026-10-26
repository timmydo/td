//! td-software: the image's applications, installed, updated and removed
//! through the compositor (`DESIGN.md`). It takes no argument but these.
#![forbid(unsafe_code)]

mod window;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "td-software [--select APP]
  Open the application manager: the applications the image declares,
  the Flathub ones installable on request, each with its status, and
  Install, Check now, Remove and Delete data. Requests go to the
  compositor's control socket ($TD_CONTROL_SOCKET), which asks root.
  --select opens it at APP, a registry name, when the image declares
  one; the launcher's Install row passes it.
td-software --preview WIDTHxHEIGHT
  Write the window's first frame as a PPM to standard output, without
  asking the compositor anything.
td-software --font-license
  Print the embedded font notices.

F1, or a click on the status row, lists the keys.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("td-software: {message}");
            ExitCode::FAILURE
        }
    }
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
                .ok_or("--preview expects WIDTHxHEIGHT")?;
            let (width, height) = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
                .ok_or("--preview expects WIDTHxHEIGHT")?;
            if args.next().is_some() {
                return Err(USAGE.to_owned());
            }
            let ppm = window::preview(width, height)?;
            std::io::stdout()
                .lock()
                .write_all(&ppm)
                .map_err(|error| error.to_string())
        }
        Some("--select") => {
            let name = args
                .next()
                .and_then(|arg| arg.into_string().ok())
                .filter(|name| td_software::registry::valid_name(name))
                .ok_or("--select expects an application name")?;
            if args.next().is_some() {
                return Err(USAGE.to_owned());
            }
            window::run(Some(&name))
        }
        Some(_) => Err(USAGE.to_owned()),
        None => window::run(None),
    }
}
