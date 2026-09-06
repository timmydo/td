#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [arg] if arg == "--replay" => td_editor::replay::run(&mut io::stdin().lock(), &mut io::stdout().lock()),
        [arg] if arg == "--preview" => td_editor::render::preview(&mut io::stdout().lock()),
        [arg] if arg == "--window-preview" => td_editor::wayland::preview(),
        [arg, keys] if arg == "--window-preview" && keys == "--keys=emacs" => td_editor::wayland::preview_with_profile(td_editor::keys::Profile::Emacs),
        [arg, keys] if arg == "--window-preview" && keys == "--keys=windows" => td_editor::wayland::preview(),
        [arg, rest @ ..] if arg == "--window" => file_window(rest),
        [arg] if arg == "--font-license" => {
            let mut output = io::stdout().lock();
            [td_editor::render::FONT_PROVENANCE, td_editor::render::FONT_COPYING,
                td_editor::render::FONT_LICENSE].iter().try_for_each(|notice| output.write_all(notice.as_bytes()))
        }
        [arg] if arg == "--help" => io::stdout().lock().write_all(b"td-editor --window [--keys=windows|emacs] [--] [FILE...]\nExperimental file editing: Open, Save and Save As to a new path.\nWindows: Ctrl+O, Ctrl+S, Ctrl+Shift+S. Emacs: C-x C-f, C-x C-s, C-x C-w.\nPath entry: Return submits, Escape/Ctrl+G cancels, Ctrl+U clears. No shell expansion.\nClose asks per dirty tab: Ctrl+S saves, Ctrl+D discards, Escape/Ctrl+G cancels closing.\nCancelling close during Save does not cancel the write; completed saves stay saved.\nSave conflict: Ctrl+R Reload, Ctrl+S Save As, Escape/Ctrl+G Cancel.\nDirty Reload additionally requires Ctrl+D; it clears undo history.\nMouse: select/drag, tab clicks/close marks, wheel/touchpad scrolling.\nNo clickable menus, clipboard, spelling or recovery yet. Do not use as $EDITOR.\nFixtures: --replay | --preview | --window-preview [--keys=windows|emacs]\nScratch window close: Ctrl+D discards ALL scratch edits; Escape/Ctrl+G cancels.\nOther: --font-license | --help\n"),
        _ => Err(io::Error::other("use --window, --replay, --preview, --window-preview, --font-license or --help; ordinary $EDITOR invocation is not ready")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "td-editor: {error}");
            ExitCode::FAILURE
        }
    }
}

fn window_args(
    args: &[std::ffi::OsString],
) -> io::Result<(td_editor::keys::Profile, Vec<std::path::PathBuf>)> {
    use std::os::unix::ffi::OsStrExt;
    let mut profile = td_editor::keys::Profile::Windows;
    let mut literal = false;
    let mut paths = Vec::new();
    for arg in args {
        if !literal && arg == "--" {
            literal = true;
        } else if !literal && arg == "--keys=emacs" {
            profile = td_editor::keys::Profile::Emacs;
        } else if !literal && arg == "--keys=windows" {
            profile = td_editor::keys::Profile::Windows;
        } else if !literal && arg.as_bytes().starts_with(b"-") {
            return Err(io::Error::other(
                "unknown window option; use -- before dash-prefixed paths",
            ));
        } else {
            if paths.len() == 64 {
                return Err(io::Error::other("at most 64 input paths"));
            }
            if arg.as_bytes().len() > 4096 {
                return Err(io::Error::other(
                    "each input path must be at most 4096 bytes",
                ));
            }
            paths.push(std::path::PathBuf::from(arg));
        }
    }
    Ok((profile, paths))
}

fn file_window(args: &[std::ffi::OsString]) -> io::Result<()> {
    let (profile, paths) = window_args(args)?;
    td_editor::wayland::file_window(profile, paths)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    #[test]
    fn literal_paths_and_options() {
        let raw = OsString::from_vec(b"file-\xff".to_vec());
        let args = vec![
            "--keys=emacs".into(),
            "--".into(),
            "-file".into(),
            raw.clone(),
        ];
        let (profile, paths) = window_args(&args).unwrap();
        assert_eq!(profile, td_editor::keys::Profile::Emacs);
        assert_eq!(paths, vec![std::path::PathBuf::from("-file"), raw.into()]);
        assert!(window_args(&["--keys=other".into()]).is_err());
        assert!(window_args(&vec!["file".into(); 65]).is_err());
        assert!(window_args(&["a".repeat(4097).into()]).is_err());
    }
}
