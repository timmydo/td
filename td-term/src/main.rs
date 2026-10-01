//! td-term: a terminal window on td-ui (td-term/DESIGN.md).

#![forbid(unsafe_code)]

mod app;
mod ready;
mod session;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use td_ui::vt_terminfo as terminfo;

fn usage() -> String {
    "usage: td-term [--socket PATH] [--working-directory PATH] [--font-size POINTS] \
     [-e|--command|-- PROGRAM [ARG...]] | td-term -h|--help | \
     td-term run [--socket PATH] --ready-socket PATH [--working-directory PATH] \
     [--command PROGRAM [ARG...]] | td-term probe READY_SOCKET | td-term terminfo PATH | \
     td-term selftest"
        .into()
}

/// What an invocation asks for: a verb, or with none (or a flag first) the
/// desktop terminal.
enum Request {
    Terminal(app::Options),
    Probe(String),
    Terminfo(String),
    Selftest,
    Help,
}

fn request(args: &[OsString]) -> Result<Request, String> {
    let Some(first) = args.first() else {
        return parse_run(args, true).map(Request::Terminal);
    };
    let verb = first.to_str().ok_or_else(usage)?;
    let rest = args.get(1..).unwrap_or_default();
    let only = |count: usize| -> Result<Vec<String>, String> {
        let words = utf8_args(rest)?;
        if words.len() != count {
            return Err(usage());
        }
        Ok(words)
    };
    let one = || -> Result<String, String> { only(1)?.into_iter().next().ok_or_else(usage) };
    match verb {
        "run" => parse_run(rest, false).map(Request::Terminal),
        "probe" => one().map(Request::Probe),
        "terminfo" => one().map(Request::Terminfo),
        "selftest" => only(0).map(|_| Request::Selftest),
        "-h" | "--help" => only(0).map(|_| Request::Help),
        _ if verb.starts_with('-') => parse_run(args, true).map(Request::Terminal),
        _ => Err(usage()),
    }
}

/// `--command` (or, on a desktop, `-e` or `--`) ends td-term's own flags:
/// everything after it is the child's literal argv, so a program's flags can
/// never collide with the terminal's. The tail stays bytes; td-term's own
/// flags must be UTF-8. An empty command is refused rather than silently
/// meaning the shell. Under td's profile a relative program is refused here,
/// before the terminal dials anything, so a typo in a unit fails at parse
/// time rather than after painting a window; a desktop's is found on `PATH`.
fn split_command(args: &[OsString], desktop: bool) -> Result<(Vec<String>, Vec<OsString>), String> {
    let ends = |argument: &OsString| {
        argument == "--command" || (desktop && (argument == "-e" || argument == "--"))
    };
    let Some(index) = args.iter().position(ends) else {
        return Ok((utf8_args(args)?, Vec::new()));
    };
    let flags = utf8_args(args.get(..index).ok_or_else(usage)?)?;
    let command = args.get(index + 1..).ok_or_else(usage)?;
    let Some(program) = command.first() else {
        let flag = args
            .get(index)
            .map_or_else(String::new, |f| f.to_string_lossy().into());
        return Err(format!("{flag} requires a program"));
    };
    if !desktop && !Path::new(program).is_absolute() {
        return Err(format!(
            "terminal command '{}' is not absolute",
            program.to_string_lossy()
        ));
    }
    Ok((flags, command.to_vec()))
}

fn utf8_args(args: &[OsString]) -> Result<Vec<String>, String> {
    args.iter()
        .map(|argument| {
            argument
                .clone()
                .into_string()
                .map_err(|raw| format!("argument '{}' is not UTF-8", raw.to_string_lossy()))
        })
        .collect()
}

/// The flags, each at most once and each taking a value: `run`'s under
/// td's profile, a bare invocation's under the desktop's, which takes no
/// readiness socket but takes a font size, also spells a flag
/// `--flag=VALUE`, and takes a relative working directory from td-term's
/// own and a relative socket as a display name under `XDG_RUNTIME_DIR`, as
/// `WAYLAND_DISPLAY` is.
fn parse_run(args: &[OsString], desktop: bool) -> Result<app::Options, String> {
    let (flags, command) = split_command(args, desktop)?;
    let mut socket = None;
    let mut ready_socket = None;
    let mut working_directory = None;
    let mut font_size = None;
    let mut words = flags.iter();
    while let Some(word) = words.next() {
        let (flag, inline) = match word.split_once('=') {
            Some((flag, value)) if desktop && flag.starts_with("--") => (flag, Some(value)),
            _ => (word.as_str(), None),
        };
        let mut value = || match inline {
            Some(value) => Ok(value),
            None => words
                .next()
                .map(String::as_str)
                .ok_or_else(|| format!("{flag} requires a value")),
        };
        if flag == "--font-size" && desktop {
            if font_size.is_some() {
                return Err(format!("duplicate flag '{flag}'"));
            }
            font_size = Some(app::font_points(value()?)?);
            continue;
        }
        let slot = match flag {
            "--socket" => &mut socket,
            "--ready-socket" if !desktop => &mut ready_socket,
            "--working-directory" => &mut working_directory,
            _ => return Err(format!("unknown flag '{flag}': {}", usage())),
        };
        if slot.is_some() {
            return Err(format!("duplicate flag '{flag}'"));
        }
        let value = value()?;
        let path = PathBuf::from(value);
        if desktop {
            *slot = Some(if flag == "--working-directory" {
                std::path::absolute(&path).map_err(|e| format!("{flag} '{value}': {e}"))?
            } else {
                path
            });
            continue;
        }
        if !path.is_absolute() {
            return Err(format!("{flag} '{value}' is not absolute"));
        }
        *slot = Some(path);
    }
    let profile = if desktop {
        app::Profile::Desktop
    } else {
        app::Profile::Td {
            ready_socket: ready_socket.ok_or("--ready-socket is required")?,
        }
    };
    Ok(app::Options {
        socket,
        profile,
        working_directory,
        command,
        font_size,
    })
}

/// The packaged binary's self-checks, and the marker that says all ran.
fn selftest(out: &mut impl Write) -> Result<(), String> {
    td_ui::vt::selftest()?;
    td_ui::vt_keys::selftest()?;
    td_ui::vt_render::selftest()?;
    terminfo::selftest()?;
    td_ui::pty::selftest()?;
    session::selftest()?;
    ready::selftest()?;
    app::selftest()?;
    writeln!(out, "TD-TERM-SELFTEST-OK").map_err(|e| format!("write selftest marker: {e}"))
}

/// Write the compiled terminfo entry. ncurses finds an entry only under its
/// own first letter, so a path spelled otherwise is refused.
fn install_terminfo(path: &str) -> Result<(), String> {
    if !path.ends_with(terminfo::INSTALL_PATH) {
        return Err(format!(
            "{path} does not end with {}",
            terminfo::INSTALL_PATH
        ));
    }
    let bytes = terminfo::entry()?;
    std::fs::write(path, bytes).map_err(|error| format!("write {path}: {error}"))
}

/// Runs what was asked; answers the status to exit with, which is the
/// desktop terminal's child's.
fn dispatch(args: &[OsString]) -> Result<u8, String> {
    match request(args)? {
        Request::Terminal(options) => app::run(options),
        Request::Probe(socket) => ready::probe(Path::new(&socket)).map(|()| 0),
        Request::Terminfo(path) => install_terminfo(&path).map(|()| 0),
        Request::Selftest => selftest(&mut std::io::stdout()).map(|()| 0),
        Request::Help => writeln!(std::io::stdout(), "{}", usage())
            .map(|()| 0)
            .map_err(|e| format!("write usage: {e}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match dispatch(&args) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "td-term: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn run_flags_are_absolute_once_each_and_the_command_ends_them() {
        let options = parse_run(
            &words(&[
                "--socket",
                "/run/w",
                "--ready-socket",
                "/run/r",
                "--command",
                "/bin/mail",
                "--socket",
            ]),
            false,
        )
        .unwrap();
        assert_eq!(options.socket, Some(PathBuf::from("/run/w")));
        assert_eq!(
            options.profile,
            app::Profile::Td {
                ready_socket: PathBuf::from("/run/r")
            }
        );
        assert_eq!(options.command, words(&["/bin/mail", "--socket"]));
        let bare = parse_run(&words(&["--ready-socket", "/run/r"]), false).unwrap();
        assert_eq!(bare.socket, None);
        assert!(bare.command.is_empty());
        for refused in [
            &["--socket", "/run/w"][..],
            &["--ready-socket", "run/r"],
            &["--ready-socket", "/a", "--ready-socket", "/b"],
            &["--ready-socket"],
            &["--ready-socket", "/r", "--bogus", "/x"],
            &["--ready-socket", "/r", "--command"],
            &["--ready-socket", "/r", "--command", "sh"],
            &["--ready-socket", "/r", "--working-directory", "home"],
        ] {
            assert!(parse_run(&words(refused), false).is_err(), "{refused:?}");
        }
    }

    /// A desktop's flags: no readiness socket, relative paths taken from
    /// td-term's own directory, and `-e` or `--command` ending them with a
    /// program found on PATH.
    #[test]
    fn desktop_flags_resolve_relative_paths_and_take_a_bare_program() {
        let bare = parse_run(&[], true).unwrap();
        assert_eq!(bare.profile, app::Profile::Desktop);
        assert_eq!((bare.socket, bare.working_directory), (None, None));
        assert!(bare.command.is_empty());
        let here = std::env::current_dir().unwrap();
        let options = parse_run(
            &words(&["--working-directory", "src", "-e", "htop", "-d", "10"]),
            true,
        )
        .unwrap();
        assert_eq!(options.working_directory, Some(here.join("src")));
        assert_eq!(options.command, words(&["htop", "-d", "10"]));
        let long = parse_run(&words(&["--socket", "w", "--command", "vi", "-e"]), true).unwrap();
        // A display name, resolved under XDG_RUNTIME_DIR when dialled.
        assert_eq!(long.socket, Some(PathBuf::from("w")));
        assert_eq!(long.command, words(&["vi", "-e"]));
        let joined = parse_run(
            &words(&[
                "--working-directory=/srv",
                "--socket=/run/w",
                "--",
                "ls",
                "-e",
            ]),
            true,
        )
        .unwrap();
        assert_eq!(joined.working_directory, Some(PathBuf::from("/srv")));
        assert_eq!(joined.socket, Some(PathBuf::from("/run/w")));
        assert_eq!(joined.command, words(&["ls", "-e"]));
        for refused in [
            &["--ready-socket", "/run/r"][..],
            &["--ready-socket=/run/r"],
            &["--working-directory"],
            &["--bogus", "x"],
            &["--socket", "/a", "--socket=/b"],
        ] {
            assert!(parse_run(&words(refused), true).is_err(), "{refused:?}");
        }
        // A font size is the desktop's, in points, within the face's bounds.
        assert_eq!(bare.font_size, None);
        let sized = parse_run(&words(&["--font-size", "9", "-e", "vi"]), true).unwrap();
        assert_eq!(sized.font_size, Some(9.0));
        let joined = parse_run(&words(&["--font-size=10.5"]), true).unwrap();
        assert_eq!(joined.font_size, Some(10.5));
        for refused in [
            &["--font-size"][..],
            &["--font-size", "4"],
            &["--font-size", "193"],
            &["--font-size", "nine"],
            &["--font-size", "NaN"],
            &["--font-size", "inf"],
            &["--font-size", "9", "--font-size=10"],
        ] {
            assert!(parse_run(&words(refused), true).is_err(), "{refused:?}");
        }
        assert!(parse_run(&words(&["--font-size", "4.5"]), true).is_ok());
        assert!(parse_run(&words(&["--font-size", "192"]), true).is_ok());
        assert!(parse_run(&words(&["--ready-socket", "/r", "--font-size", "9"]), false).is_err());
        for (alone, said) in [("-e", "-e"), ("--", "--"), ("--command", "--command")] {
            let error = parse_run(&words(&[alone]), true).err().unwrap();
            assert_eq!(error, format!("{said} requires a program"));
        }
        assert!(parse_run(&words(&["--bogus"]), true)
            .err()
            .unwrap()
            .starts_with("unknown flag '--bogus'"));
        // `-e` is the desktop's alone: td's profile reads it as a flag.
        assert!(parse_run(&words(&["--ready-socket", "/r", "-e", "/bin/sh"]), false).is_err());
    }

    /// A bare invocation, or a flag first, is the desktop; a verb is its
    /// own; `run` is td's profile and takes no `--flag=VALUE` spelling.
    #[test]
    fn requests_route_bare_and_flag_first_invocations_to_the_desktop() {
        let profile = |list: &[&str]| match request(&words(list)) {
            Ok(Request::Terminal(options)) => Some(options.profile),
            _ => None,
        };
        assert_eq!(profile(&[]), Some(app::Profile::Desktop));
        assert_eq!(profile(&["-e", "htop"]), Some(app::Profile::Desktop));
        assert_eq!(
            profile(&["--working-directory", "/"]),
            Some(app::Profile::Desktop)
        );
        assert_eq!(
            profile(&["run", "--ready-socket", "/r"]),
            Some(app::Profile::Td {
                ready_socket: PathBuf::from("/r")
            })
        );
        assert_eq!(profile(&["--ready-socket", "/r"]), None);
        assert_eq!(profile(&["run", "--ready-socket=/r"]), None);
        assert_eq!(profile(&["htop"]), None);
        assert!(matches!(request(&words(&["--help"])), Ok(Request::Help)));
        assert!(matches!(request(&words(&["-h"])), Ok(Request::Help)));
        assert!(request(&words(&["-h", "x"])).is_err());
        assert!(matches!(
            request(&words(&["selftest"])),
            Ok(Request::Selftest)
        ));
        assert!(matches!(request(&words(&["probe", "/s"])), Ok(Request::Probe(s)) if s == "/s"));
        assert!(
            matches!(request(&words(&["terminfo", "/t"])), Ok(Request::Terminfo(t)) if t == "/t")
        );
        assert!(request(&words(&["probe"])).is_err());
    }

    #[test]
    fn the_child_argv_is_bytes_but_the_flags_are_text() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'/', 0xff]);
        let mut args = words(&["--ready-socket", "/r", "--command", "/bin/cat"]);
        args.push(raw.clone());
        assert_eq!(parse_run(&args, false).unwrap().command.last(), Some(&raw));
        let mut flags = words(&["--ready-socket"]);
        flags.push(raw);
        assert!(parse_run(&flags, false).is_err());
    }

    #[test]
    fn terminfo_is_written_only_where_ncurses_looks() {
        assert!(install_terminfo("/tmp/share/terminfo/x/td-term").is_err());
        let directory = std::env::temp_dir().join(format!("td-term-info-{}", std::process::id()));
        let path = directory.join(terminfo::INSTALL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        install_terminfo(path.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), terminfo::entry().unwrap());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_selftest_names_itself_once_everything_ran() {
        let mut out = Vec::new();
        selftest(&mut out).unwrap();
        assert_eq!(out, b"TD-TERM-SELFTEST-OK\n");
    }

    #[test]
    fn unknown_verbs_and_extra_words_are_refused() {
        for refused in [
            &[][..],
            &["help"],
            &["selftest", "x"],
            &["probe"],
            &["terminfo", "/a", "/b"],
        ] {
            assert!(dispatch(&words(refused)).is_err(), "{refused:?}");
        }
    }
}
