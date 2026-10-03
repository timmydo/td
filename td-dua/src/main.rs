//! td-dua: a disk usage analyzer over td-ui's widget window
//! (`DESIGN.md`). Its argument is the directory to scan, else the working
//! directory.
#![forbid(unsafe_code)]

mod window;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "td-dua [DIRECTORY]
  Open the disk usage analyzer over DIRECTORY, else the working directory.
td-dua --preview WIDTHxHEIGHT [DIRECTORY [SELECT]]
  Scan, then write the window's first frame as a PPM to standard output,
  with SELECT (a path under DIRECTORY) revealed and selected.
td-dua --font-license
  Print the embedded font notices.

The list (top) is sorted by size; click a heading to sort by it. Clicking
the treemap (bottom) selects that file and opens the list to it.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("td-dua: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The directory to scan, absolute and with links resolved.
fn root(argument: Option<PathBuf>) -> Result<PathBuf, String> {
    let path = match argument {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|error| format!("cannot read the working directory: {error}"))?,
    };
    let root = std::fs::canonicalize(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(root)
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    match first.as_ref().and_then(|arg| arg.to_str()) {
        Some("--help" | "-h") => {
            let _ = writeln!(
                std::io::stdout().lock(),
                "{USAGE}\n\nKeys: {}",
                td_dua::app::hint()
            );
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
            let root = root(args.next().map(PathBuf::from))?;
            let select = args.next().map(PathBuf::from);
            if args.next().is_some() {
                return Err(USAGE.to_owned());
            }
            let ppm = window::preview(root, width, height, select)?;
            std::io::stdout()
                .lock()
                .write_all(&ppm)
                .map_err(|error| error.to_string())
        }
        Some(arg) if arg.starts_with('-') => Err(USAGE.to_owned()),
        _ => {
            if args.next().is_some() {
                return Err(USAGE.to_owned());
            }
            window::run(root(first.map(PathBuf::from))?)
        }
    }
}
