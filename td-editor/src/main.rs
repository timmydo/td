#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = concat!(
    "td-editor --window [--keys=windows|emacs] [--] [FILE...]\n",
    "Experimental Wayland file editor. Do not use as $EDITOR yet.\n",
    "Windows files: Ctrl+O, Ctrl+S, Ctrl+Shift+S. Emacs: C-x C-f, C-x C-s, C-x C-w.\n",
    "Open/Save As path entry: Return submits, Escape/Ctrl+G cancels, Ctrl+U clears.\n",
    "Paths are literal; Save As requires a new destination. No shell expansion.\n",
    "Close asks per dirty tab: Ctrl+S saves, Ctrl+D discards, Escape/Ctrl+G cancels.\n",
    "Cancelling close does not cancel a pending write; completed saves stay saved.\n",
    "Save conflict: Ctrl+R Reload, Ctrl+S Save As, Escape/Ctrl+G Cancel.\n",
    "Dirty Reload additionally requires Ctrl+D and clears undo history.\n",
    "Mouse: select/drag, tab clicks/close marks, wheel/touchpad scrolling.\n",
    "Menus: click a header or F10; arrows navigate, Return selects, Escape cancels.\n",
    "Edit switches key profiles. Format: Soft Wrap, Auto Fill, Fill Paragraph.\n",
    "UTF-8 clipboard requires Wayland data-device v3 and window focus; limit 1 MiB.\n",
    "Windows clipboard: Ctrl+C/X/V. Emacs: M-w/C-w/C-y. Edit also has Copy/Cut/Paste.\n",
    "Copy/Cut needs a physical key or pointer press. Escape/Ctrl+G cancels Paste.\n",
    "Find: Windows Ctrl+F, F3, Shift+F3; Emacs C-s/C-r opens a directional prompt.\n",
    "Find is submitted literal, case-sensitive search, not incremental while typing.\n",
    "Return searches, Ctrl+U clears, Escape/Ctrl+G cancels.\n",
    "At the start/end, repeat the same search to wrap.\n",
    "Go To Line: F6 or Edit menu in both profiles; enter a one-based logical line.\n",
    "No GPU renderer, spelling UI, control socket, crash recovery or tmc integration.\n",
    "Fixtures: --replay | --preview\n",
    "Scratch: --window-preview [--keys=windows|emacs]\n",
    "Scratch window has no file I/O.\n",
    "Scratch close: Ctrl+D discards ALL scratch edits; Escape/Ctrl+G cancels.\n",
    "Other: --font-license | --help\n",
);

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
        [arg] if arg == "--help" => io::stdout().lock().write_all(HELP.as_bytes()),
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
    fn help_describes_native_adapters_without_claiming_editor_integration() {
        assert_eq!(td_editor::clipboard::MAX_BYTES, 1024 * 1024);
        for feature in [
            "data-device v3",
            "limit 1 MiB",
            "physical key or pointer press",
            "Ctrl+F, F3, Shift+F3",
            "F6 or Edit menu",
            "not incremental",
            "At the start/end, repeat the same search to wrap",
            "Do not use as $EDITOR yet",
            "No GPU renderer, spelling UI",
        ] {
            assert!(HELP.contains(feature), "{feature}");
        }
        assert!(!HELP.contains("No clipboard"));
        assert!(HELP.ends_with('\n'));
        assert!(HELP.lines().all(|line| line.len() <= 80));
        assert!(HELP
            .contains("Scratch close: Ctrl+D discards ALL scratch edits; Escape/Ctrl+G cancels."));
    }
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
