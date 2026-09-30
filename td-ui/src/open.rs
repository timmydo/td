//! Following a link: the browser command a program is configured with,
//! else `$BROWSER`, else `xdg-open`, run directly with the link as one
//! argument and never through a shell, its standard streams on
//! `/dev/null`, without the program's `WAYLAND_SOCKET`, and its exit
//! reaped on a thread of its own, so a slow browser never holds the
//! window's loop.

use std::path::Path;
use std::process::{Command, Stdio};

/// Where a command puts the link among its words.
const PLACEHOLDER: &str = "{url}";

/// The browser when neither the program nor the environment names one.
const FALLBACK: &str = "xdg-open";

/// Runs the browser on `link`, which must be one link whole
/// (`links::whole`): `command` when it names one, else `$BROWSER`, else
/// `xdg-open`. Answers the program started, for a status line, or why
/// none was.
pub fn link(link: &str, command: Option<&str>) -> Result<String, String> {
    link_on(link, command, None)
}

/// As `link`, the browser told the display it is to open on: a program
/// that dials a socket its environment does not name (td-term) names it
/// here, and the browser's `WAYLAND_DISPLAY` is that path.
pub fn link_on(
    link: &str,
    command: Option<&str>,
    display: Option<&Path>,
) -> Result<String, String> {
    let environment = std::env::var("BROWSER").ok();
    let (program, mut process) = prepare(link, command, environment.as_deref(), display)?;
    let mut child = process.spawn().map_err(|e| format!("{program}: {e}"))?;
    // A thread that cannot be made leaves the browser unreaped until this
    // process exits; the link is open either way.
    let _ = std::thread::Builder::new()
        .name("td-ui-open".into())
        .spawn(move || child.wait());
    Ok(program)
}

/// The browser's process for `link`, not yet started, and its program.
fn prepare(
    link: &str,
    command: Option<&str>,
    browser: Option<&str>,
    display: Option<&Path>,
) -> Result<(String, Command), String> {
    let argv = argv(link, command, browser)?;
    let (program, arguments) = argv.split_first().ok_or("no browser command")?;
    let mut process = Command::new(program);
    process
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // The program's own display connection is not the browser's.
        .env_remove("WAYLAND_SOCKET");
    if let Some(display) = display {
        process.env("WAYLAND_DISPLAY", display);
    }
    Ok((program.clone(), process))
}

/// The words run for `link`: the first command of `command`, `browser`
/// (the environment's) and `xdg-open` that has a word, split on
/// whitespace. A word holding `{url}` has it replaced by the link, quotes
/// put around the bare placeholder taken off as a shell would, and a
/// command without one gets the link as its last word.
fn argv(link: &str, command: Option<&str>, browser: Option<&str>) -> Result<Vec<String>, String> {
    if !crate::links::whole(link) {
        return Err(format!("not a link: {link}"));
    }
    let command = [command, browser, Some(FALLBACK)]
        .into_iter()
        .flatten()
        .find(|command| !command.trim().is_empty())
        .unwrap_or(FALLBACK);
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or(FALLBACK);
    if program.contains(PLACEHOLDER) {
        return Err(format!("the browser command names no program: {command}"));
    }
    let mut argv = vec![program.to_string()];
    let mut placed = false;
    for word in words {
        let word = match word {
            "\"{url}\"" | "'{url}'" => PLACEHOLDER,
            word => word,
        };
        placed |= word.contains(PLACEHOLDER);
        argv.push(word.replace(PLACEHOLDER, link));
    }
    if !placed {
        argv.push(link.to_string());
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://e.example/a?b=c&d=$x;*|y%20{z}";

    fn words(command: Option<&str>, browser: Option<&str>) -> Vec<String> {
        argv(LINK, command, browser).unwrap()
    }

    #[test]
    fn the_configured_command_then_the_environment_then_xdg_open() {
        assert_eq!(words(Some("firefox"), Some("chromium")), ["firefox", LINK]);
        assert_eq!(words(None, Some("chromium")), ["chromium", LINK]);
        assert_eq!(words(Some("  "), Some("chromium")), ["chromium", LINK]);
        assert_eq!(words(None, None), ["xdg-open", LINK]);
        assert_eq!(words(None, Some("")), ["xdg-open", LINK]);
    }

    #[test]
    fn the_link_is_one_word_where_the_command_puts_it() {
        for command in [
            "firefox --new-tab {url}",
            "firefox --new-tab \"{url}\"",
            "firefox --new-tab '{url}'",
        ] {
            assert_eq!(
                words(Some(command), None),
                ["firefox", "--new-tab", LINK],
                "{command}"
            );
        }
        assert_eq!(
            words(Some("b --url={url} --x"), None),
            ["b", &format!("--url={LINK}"), "--x"]
        );
        assert_eq!(
            words(Some("firefox --new-tab"), None),
            ["firefox", "--new-tab", LINK]
        );
    }

    #[test]
    fn a_command_whose_program_is_the_placeholder_is_refused() {
        assert!(argv(LINK, Some("{url}"), None).is_err());
    }

    #[test]
    fn only_a_whole_link_is_opened() {
        // Past the finder's bound either side of a press, still one link.
        let long = format!(
            "https://e.example/{}",
            "a".repeat(3 * crate::links::MAX_BYTES)
        );
        assert!(argv(&long, Some("b"), None).is_ok());
        for text in [
            "",
            "-x",
            "--help",
            "e.example",
            "ftp://x",
            "http://",
            " https://x",
            "https://x y",
            "https://x.",
        ] {
            assert!(argv(text, Some("firefox"), None).is_err(), "{text}");
        }
    }

    #[test]
    fn the_browser_has_the_display_named_and_not_the_programs_socket() {
        use std::ffi::OsStr;
        let display = Path::new("/run/td/wayland-1");
        let (program, process) = prepare(LINK, Some("b"), None, Some(display)).unwrap();
        assert_eq!(program, "b");
        let envs: Vec<_> = process.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("WAYLAND_SOCKET"), None)));
        assert!(envs.contains(&(OsStr::new("WAYLAND_DISPLAY"), Some(display.as_os_str()))));
        let (_, process) = prepare(LINK, Some("b"), None, None).unwrap();
        let envs: Vec<_> = process.get_envs().collect();
        assert_eq!(envs, [(OsStr::new("WAYLAND_SOCKET"), None)]);
    }

    #[test]
    fn a_browser_that_cannot_start_says_which() {
        let error = link("https://e.example/", Some("/nonexistent/td-ui-browser")).unwrap_err();
        assert!(error.starts_with("/nonexistent/td-ui-browser: "), "{error}");
    }
}
