#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [arg] if arg == "--replay" => td_editor::replay::run(&mut io::stdin().lock(), &mut io::stdout().lock()),
        [arg] if arg == "--preview" => td_editor::render::preview(&mut io::stdout().lock()),
        [arg] if arg == "--window-preview" => td_editor::wayland::preview(),
        [arg] if arg == "--font-license" => {
            let mut output = io::stdout().lock();
            [td_editor::render::FONT_PROVENANCE, td_editor::render::FONT_COPYING,
                td_editor::render::FONT_LICENSE].iter().try_for_each(|notice| output.write_all(notice.as_bytes()))
        }
        [arg] if arg == "--help" => io::stdout().lock().write_all(b"td-editor --replay | --preview | --window-preview | --font-license\nHeadless replay, P6 PPM fixture, read-only Wayland window, or font notices.\nWindow input and file I/O are not implemented yet. Do not use as $EDITOR.\n"),
        _ => Err(io::Error::other("use --replay, --preview, --window-preview, --font-license or --help; file editing is not connected yet")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "td-editor: {error}");
            ExitCode::FAILURE
        }
    }
}
