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
td-dua report [--json] [--apparent] [--top N] [--stale-days DAYS]
              [--min-size SIZE] [DIRECTORY]
  Scan, then print what is worth cleaning up, deleting nothing:
  directories their owners usually recreate (CACHEDIR.TAG caches, node_modules,
  __pycache__, .cache, the trash), the largest and the stale files outside
  them, and the top level. --json writes one JSON document for a program;
  --apparent measures lengths rather than allocated blocks. Each section
  lists N entries (default 20); a file is stale after DAYS days unmodified
  (default 365); the file sections list files of SIZE or more (bytes, or
  with K, M, G or T; default 1M). A directory named report opens in the
  window as ./report.
td-dua --font-license
  Print the embedded font notices.

The list (top) is sorted by size; click a heading to sort by it. Clicking
the treemap (bottom) selects that file and opens the list to it. F1, or
clicking the status row while it starts with F1: keys, shows the keys.";

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

/// `td-dua report`: scan on this thread, then print the report.
fn report(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    use td_dua::report::{self, Options};
    let mut options = Options::default();
    let mut json = false;
    let mut directory = None;
    let number = |value: Option<std::ffi::OsString>, flag: &str| -> Result<u64, String> {
        value
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| format!("{flag} expects a whole number"))
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                let _ = writeln!(std::io::stdout().lock(), "{USAGE}");
                return Ok(());
            }
            Some("--json") => json = true,
            Some("--apparent") => options.measure = td_dua::tree::Measure::Apparent,
            Some("--top") => {
                let top = number(args.next(), "--top")?;
                options.top = usize::try_from(top)
                    .ok()
                    .filter(|top| *top <= report::MAX_TOP)
                    .ok_or_else(|| format!("--top is at most {}", report::MAX_TOP))?;
            }
            Some("--stale-days") => options.stale_days = number(args.next(), "--stale-days")?,
            Some("--min-size") => {
                options.min_size = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .and_then(|value| report::parse_size(&value))
                    .ok_or("--min-size expects bytes, or a number with K, M, G or T")?;
            }
            Some(flag) if flag.starts_with('-') => return Err(USAGE.to_owned()),
            _ if directory.is_none() => directory = Some(PathBuf::from(arg)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let root = root(directory)?;
    let tree = td_dua::scan::scan(&root, &td_dua::scan::Progress::default())
        .map_err(|error| format!("cannot scan {}: {error}", root.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let report = report::build(&tree, options, now, &mut td_dua::scan::cachedir_tagged);
    let text = if json {
        let mut text = report.to_json().to_string();
        text.push('\n');
        text
    } else {
        report.to_text()
    };
    std::io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())
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
        Some("report") => report(args),
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
