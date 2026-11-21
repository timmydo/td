//! td-web, td's own document browser (td-web/DESIGN.md): HTML, text,
//! PNG, JPEG, GIF, WebP and AVIF over `http` and `https`, fetched through td's fetch
//! service and shown in a td-ui window. No script runs.
#![forbid(unsafe_code)]

mod app;
mod document;
mod fetch;
#[cfg(test)]
mod made;
mod memory;
mod picture;
mod pictures;
mod save;
mod subresource;
mod worker;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: td-web [--control-socket PATH] [URL]
       td-web --help actions

td's document browser: HTML, text, PNG, JPEG, GIF, WebP and AVIF over http and https,
fetched through td's fetch service. No script runs. An address without
a scheme is taken as https. --control-socket serves td-ui's driven seam
on a private socket at PATH, an absolute path in a directory of mode 0700;
--help actions lists its actions.
";

/// What the command line asks.
enum Command {
    Run {
        start: Option<String>,
        control: Option<PathBuf>,
    },
    Help,
    Actions,
    Version,
}

fn parse(args: Vec<OsString>) -> Option<Command> {
    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => return Some(Command::Help),
        [flag, what] if flag == "--help" && what == "actions" => return Some(Command::Actions),
        [flag] if flag == "--version" => return Some(Command::Version),
        _ => {}
    }
    let mut start = None;
    let mut control = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--control-socket" && control.is_none() {
            control = Some(PathBuf::from(args.next()?));
        } else if start.is_none() && !arg.to_str()?.starts_with('-') {
            start = Some(arg.into_string().ok()?);
        } else {
            return None;
        }
    }
    Some(Command::Run { start, control })
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let (start, control) = match parse(args) {
        Some(Command::Run { start, control }) => (start, control),
        Some(Command::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(Command::Actions) => {
            print!("{}", td_ui::driven::help(app::control::BINDINGS));
            return ExitCode::SUCCESS;
        }
        Some(Command::Version) => {
            println!("td-web {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        None => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match app::run(start, control) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("td-web: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn run(list: &[&str]) -> Option<(Option<String>, Option<PathBuf>)> {
        match parse(args(list))? {
            Command::Run { start, control } => Some((start, control)),
            _ => panic!("not a run: {list:?}"),
        }
    }

    #[test]
    fn the_command_line_takes_an_address_and_a_socket_in_either_order() {
        assert_eq!(run(&[]), Some((None, None)));
        assert_eq!(
            run(&["example.org"]),
            Some((Some("example.org".into()), None))
        );
        let both = Some((Some("example.org".into()), Some(PathBuf::from("/run/s"))));
        assert_eq!(run(&["--control-socket", "/run/s", "example.org"]), both);
        assert_eq!(run(&["example.org", "--control-socket", "/run/s"]), both);
        assert!(matches!(parse(args(&["--help"])), Some(Command::Help)));
        assert!(matches!(parse(args(&["-h"])), Some(Command::Help)));
        assert!(matches!(
            parse(args(&["--help", "actions"])),
            Some(Command::Actions)
        ));
        assert!(matches!(
            parse(args(&["--version"])),
            Some(Command::Version)
        ));
    }

    #[test]
    fn a_command_line_past_its_grammar_is_refused() {
        for list in [
            &["--control-socket"][..],
            &["--control-socket", "/a", "--control-socket", "/b"],
            &["a", "b"],
            &["-x"],
            &["--help", "keys"],
            &["a", "--help"],
        ] {
            assert!(parse(args(list)).is_none(), "{list:?}");
        }
        // A socket's path is the OS's bytes; an address must be UTF-8.
        let raw = OsString::from_vec(vec![b'/', 0xff]);
        let parsed = parse(vec![OsString::from("--control-socket"), raw.clone()]);
        assert!(
            matches!(parsed, Some(Command::Run { control: Some(path), .. }) if path.as_os_str() == raw)
        );
        assert!(parse(vec![OsString::from_vec(vec![0xff])]).is_none());
    }
}
