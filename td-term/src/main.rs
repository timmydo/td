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
    "usage: td-term run [--socket PATH] --ready-socket PATH [--working-directory PATH] \
     [--command PROGRAM [ARG...]] | td-term probe READY_SOCKET | td-term terminfo PATH | \
     td-term selftest"
        .into()
}

/// `--command` ends td-term's own flags: everything after it is the child's
/// literal argv, so a program's flags can never collide with the terminal's.
/// The tail stays bytes; td-term's own flags must be UTF-8. An empty command
/// is refused rather than silently meaning the shell, and a relative program
/// is refused here, before the terminal dials anything, so a typo in a unit
/// fails at parse time rather than after painting a window.
fn split_command(args: &[OsString]) -> Result<(Vec<String>, Vec<OsString>), String> {
    let Some(index) = args.iter().position(|argument| argument == "--command") else {
        return Ok((utf8_args(args)?, Vec::new()));
    };
    let flags = utf8_args(args.get(..index).ok_or_else(usage)?)?;
    let command = args.get(index + 1..).ok_or_else(usage)?;
    let Some(program) = command.first() else {
        return Err("--command requires a program".to_string());
    };
    if !Path::new(program).is_absolute() {
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

/// The run flags, each at most once and each taking a value.
fn parse_run(args: &[OsString]) -> Result<app::Options, String> {
    let (flags, command) = split_command(args)?;
    let mut socket = None;
    let mut ready_socket = None;
    let mut working_directory = None;
    let mut pairs = flags.chunks(2);
    for pair in &mut pairs {
        let [flag, value] = pair else {
            return Err(format!(
                "{} requires a value",
                pair.first().map_or("", String::as_str)
            ));
        };
        let slot = match flag.as_str() {
            "--socket" => &mut socket,
            "--ready-socket" => &mut ready_socket,
            "--working-directory" => &mut working_directory,
            _ => return Err(format!("unknown flag '{flag}': {}", usage())),
        };
        if slot.is_some() {
            return Err(format!("duplicate flag '{flag}'"));
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(format!("{flag} '{value}' is not absolute"));
        }
        *slot = Some(path);
    }
    Ok(app::Options {
        socket,
        ready_socket: ready_socket.ok_or("--ready-socket is required")?,
        working_directory,
        command,
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

fn dispatch(args: &[OsString]) -> Result<(), String> {
    let verb = args
        .first()
        .and_then(|word| word.to_str())
        .ok_or_else(usage)?;
    let rest = args.get(1..).unwrap_or_default();
    let only = |count: usize| -> Result<Vec<String>, String> {
        let words = utf8_args(rest)?;
        if words.len() != count {
            return Err(usage());
        }
        Ok(words)
    };
    match verb {
        "run" => app::run(parse_run(rest)?),
        "probe" => {
            let words = only(1)?;
            let socket = words.first().ok_or_else(usage)?;
            ready::probe(Path::new(socket))
        }
        "terminfo" => {
            let words = only(1)?;
            install_terminfo(words.first().ok_or_else(usage)?)
        }
        "selftest" => {
            only(0)?;
            selftest(&mut std::io::stdout())
        }
        _ => Err(usage()),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
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
        let options = parse_run(&words(&[
            "--socket",
            "/run/w",
            "--ready-socket",
            "/run/r",
            "--command",
            "/bin/mail",
            "--socket",
        ]))
        .unwrap();
        assert_eq!(options.socket, Some(PathBuf::from("/run/w")));
        assert_eq!(options.ready_socket, PathBuf::from("/run/r"));
        assert_eq!(options.command, words(&["/bin/mail", "--socket"]));
        let bare = parse_run(&words(&["--ready-socket", "/run/r"])).unwrap();
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
            assert!(parse_run(&words(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn the_child_argv_is_bytes_but_the_flags_are_text() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'/', 0xff]);
        let mut args = words(&["--ready-socket", "/r", "--command", "/bin/cat"]);
        args.push(raw.clone());
        assert_eq!(parse_run(&args).unwrap().command.last(), Some(&raw));
        let mut flags = words(&["--ready-socket"]);
        flags.push(raw);
        assert!(parse_run(&flags).is_err());
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
