#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = concat!(
    "td-editor --window [--keys=windows|emacs] [--] [FILE...]\n",
    "Window option: --dictionary PATH loads an explicit local English word list.\n",
    "Window option: --control-socket PATH enables private state/text and edits.\n",
    "Control edits and queued Save/Save As check expected revisions.\n",
    "Control Open queues a file job; state reports its resulting tab and revision.\n",
    "Control can read/edit tabs and answer live Close/Quit with Cancel/Discard.\n",
    "Remote Discard also answers human-opened dialogs, without physical focus.\n",
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
    "Format > Fill Column sets 20..=240 cells for that tab (default 72).\n",
    "Emacs M-x or Help > Command: exact editor names; Tab completes, Return runs.\n",
    "Commands include auto-fill-mode, fill-paragraph, set-fill-column, ispell-buffer.\n",
    "UTF-8 clipboard requires Wayland data-device v3 and window focus; limit 1 MiB.\n",
    "Windows clipboard: Ctrl+C/X/V. Emacs: M-w/C-w/C-y. Edit also has Copy/Cut/Paste.\n",
    "Copy/Cut needs a physical key or pointer press. Escape/Ctrl+G cancels Paste.\n",
    "Find: Windows Ctrl+F, F3, Shift+F3; Emacs C-s/C-r opens a directional prompt.\n",
    "Find is submitted literal, case-sensitive search, not incremental while typing.\n",
    "Return searches, Ctrl+U clears, Escape/Ctrl+G cancels.\n",
    "At the start/end, repeat the same search to wrap.\n",
    "Replace: Windows Ctrl+H or Edit menu; Tab switches Find/With fields.\n",
    "Return finds next; Alt+R replaces the selected match; Alt+A replaces all.\n",
    "Empty With deletes matches. Escape/Ctrl+G closes; completed edits stay edited.\n",
    "Go To Line: F6 or Edit menu in both profiles; enter a one-based logical line.\n",
    "F7 checks the whole document on demand; Escape/Ctrl+G cancels.\n",
    "Format: Dictionary, Check Spelling, Next/Previous Misspelling (no wrapping).\n",
    "Spelling underlines appear at completion; edits clear them without rechecking.\n",
    "Word list: UTF-8, one ASCII word per line; 16 MiB / 250,000 unique words.\n",
    "No bundled word list, GPU renderer, recovery or td-mail link.\n",
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

type WindowArgs = td_editor::wayland::FileWindowOptions;

fn window_args(args: &[std::ffi::OsString]) -> io::Result<WindowArgs> {
    use std::os::unix::ffi::OsStrExt;
    let mut profile = td_editor::keys::Profile::Windows;
    let mut literal = false;
    let mut paths = Vec::new();
    let mut dictionary = None;
    let mut control = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if !literal && arg == "--" {
            literal = true;
        } else if !literal && arg == "--keys=emacs" {
            profile = td_editor::keys::Profile::Emacs;
        } else if !literal && arg == "--keys=windows" {
            profile = td_editor::keys::Profile::Windows;
        } else if !literal && arg == "--control-socket" {
            if control.is_some() {
                return Err(io::Error::other(
                    "--control-socket may be specified only once",
                ));
            }
            let path = args.next().ok_or_else(|| {
                io::Error::other("--control-socket needs an absolute literal path")
            })?;
            if !std::path::Path::new(path).is_absolute()
                || path.as_bytes().len() > 107
                || path.as_bytes().contains(&0)
            {
                return Err(io::Error::other(
                    "control socket path must be absolute, non-NUL and at most 107 bytes",
                ));
            }
            control = Some(path.into());
        } else if !literal && arg == "--dictionary" {
            if dictionary.is_some() {
                return Err(io::Error::other("--dictionary may be specified only once"));
            }
            let path = args
                .next()
                .ok_or_else(|| io::Error::other("--dictionary needs a literal path"))?;
            if path.as_bytes().is_empty()
                || path.as_bytes().len() > 4096
                || path.as_bytes().contains(&0)
            {
                return Err(io::Error::other(
                    "dictionary path must contain 1..=4096 non-NUL bytes",
                ));
            }
            dictionary = Some(path.into());
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
    Ok(WindowArgs {
        profile,
        paths,
        dictionary,
        control,
    })
}

fn file_window(args: &[std::ffi::OsString]) -> io::Result<()> {
    td_editor::wayland::file_window(window_args(args)?)
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
            "Alt+R replaces the selected match; Alt+A replaces all",
            "Emacs M-x or Help > Command",
            "not incremental",
            "At the start/end, repeat the same search to wrap",
            "Do not use as $EDITOR yet",
            "F7 checks the whole document on demand",
            "No bundled word list, GPU renderer",
            "private state/text and edits",
            "queued Save/Save As check expected revisions",
            "Control Open queues a file job",
            "answer live Close/Quit with Cancel/Discard",
            "human-opened dialogs, without physical focus",
        ] {
            assert!(HELP.contains(feature), "{feature}");
        }
        assert!(!HELP.contains("No clipboard"));
        assert!(!HELP.contains("remote file I/O"));
        assert!(HELP.ends_with('\n'));
        assert!(HELP.lines().all(|line| line.len() <= 80));
        assert!(HELP
            .contains("Scratch close: Ctrl+D discards ALL scratch edits; Escape/Ctrl+G cancels."));
    }
    #[test]
    fn dictionary_option_is_explicit_bounded_literal_and_not_a_document() {
        let raw = OsString::from_vec(b"words-\xff".to_vec());
        let parsed = window_args(&["--dictionary".into(), raw.clone(), "text".into()]).unwrap();
        assert_eq!(parsed.dictionary, Some(raw.into()));
        assert_eq!(parsed.paths, vec![std::path::PathBuf::from("text")]);
        let parsed = window_args(&[
            "--dictionary".into(),
            "-words".into(),
            "--".into(),
            "--dictionary".into(),
        ])
        .unwrap();
        assert_eq!(parsed.dictionary, Some("-words".into()));
        assert_eq!(parsed.paths, vec![std::path::PathBuf::from("--dictionary")]);
        for args in [
            vec!["--dictionary".into()],
            vec!["--dictionary".into(), "".into()],
            vec!["--dictionary".into(), "x".repeat(4097).into()],
            vec!["--dictionary".into(), OsString::from_vec(b"a\0b".to_vec())],
            vec![
                "--dictionary".into(),
                "a".into(),
                "--dictionary".into(),
                "b".into(),
            ],
        ] {
            assert!(window_args(&args).is_err());
        }
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
        let WindowArgs {
            profile,
            paths,
            dictionary,
            control,
        } = window_args(&args).unwrap();
        assert!(dictionary.is_none());
        assert!(control.is_none());
        assert_eq!(profile, td_editor::keys::Profile::Emacs);
        assert_eq!(paths, vec![std::path::PathBuf::from("-file"), raw.into()]);
        assert!(window_args(&["--keys=other".into()]).is_err());
        assert!(window_args(&vec!["file".into(); 65]).is_err());
        assert!(window_args(&["a".repeat(4097).into()]).is_err());
    }

    #[test]
    fn control_socket_is_opt_in_literal_bounded_and_separate_from_documents() {
        let raw = OsString::from_vec(b"/tmp/private/control-\xff".to_vec());
        let args = window_args(&[
            "--control-socket".into(),
            raw.clone(),
            "--dictionary".into(),
            "words".into(),
            "--keys=emacs".into(),
            "--".into(),
            "--control-socket".into(),
            "document".into(),
        ])
        .unwrap();
        assert_eq!(args.control, Some(raw.into()));
        assert_eq!(args.dictionary, Some("words".into()));
        assert_eq!(args.profile, td_editor::keys::Profile::Emacs);
        assert_eq!(
            args.paths,
            vec![
                std::path::PathBuf::from("--control-socket"),
                "document".into()
            ]
        );
        for input in [
            vec!["--control-socket=/tmp/control".into()],
            vec!["--control-socket".into()],
            vec!["--control-socket".into(), "relative".into()],
            vec!["--control-socket".into(), "/tmp/a\0b".into()],
            vec![
                "--control-socket".into(),
                format!("/{}", "x".repeat(107)).into(),
            ],
            vec![
                "--control-socket".into(),
                "/tmp/a".into(),
                "--control-socket".into(),
                "/tmp/b".into(),
            ],
        ] {
            assert!(window_args(&input).is_err(), "{input:?}");
        }
        let boundary = format!("/{}", "x".repeat(106));
        assert!(window_args(&["--control-socket".into(), boundary.into()]).is_ok());
    }
}
