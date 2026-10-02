//! Following a link, or opening a local file a program wrote: the browser
//! command a program is configured with, else `$BROWSER`, else
//! `xdg-open`, run directly with the link or the file's URL as one
//! argument and never through a shell, its standard streams on
//! `/dev/null`, without the program's `WAYLAND_SOCKET`, and its exit
//! reaped on a thread of its own, so a slow browser never holds the
//! window's loop.

use std::fmt::Write;
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
    if !crate::links::whole(link) {
        return Err(format!("not a link: {link}"));
    }
    start(link, command, display)
}

/// Runs the browser, as `link` does, on a URL the program took from
/// markup or a feed rather than found in shown text (td-news's links and
/// an article's own link), whose ends the markup gives: an `http://` or
/// `https://` scheme in any case with at least one byte after it and no
/// whitespace or control character, so it is one word and no option however
/// it ends.
pub fn url(url: &str, command: Option<&str>) -> Result<String, String> {
    url_on(url, command, None)
}

/// As `url`, the browser told the display it is to open on, as
/// `link_on` tells it: td-term's OSC 8 links, whose URI the child named.
pub fn url_on(url: &str, command: Option<&str>, display: Option<&Path>) -> Result<String, String> {
    if !is_url(url) {
        return Err(format!("not a link: {url}"));
    }
    start(url, command, display)
}

/// Whether `url` is one `url` opens: an `http://` or `https://` scheme in
/// any case, at least one byte after it, and no whitespace or control
/// character.
pub fn is_url(url: &str) -> bool {
    let rest = ["https://", "http://"].into_iter().find_map(|scheme| {
        url.get(..scheme.len())
            .filter(|head| head.eq_ignore_ascii_case(scheme))
            .and_then(|_| url.get(scheme.len()..))
    });
    !rest.is_none_or(str::is_empty) && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Runs the browser, as `link` does, on a local file the program wrote
/// (td-news's digest), given as a `file://` URL: `path` must be absolute
/// and UTF-8.
pub fn file(path: &Path, command: Option<&str>) -> Result<String, String> {
    start(&file_url(path)?, command, None)
}

/// `path` as a `file://` URL, every byte but an unreserved one or `/`
/// percent-encoded: one word with no whitespace, and no option, since it
/// starts with the scheme.
fn file_url(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .filter(|_| path.is_absolute())
        .ok_or_else(|| format!("not an absolute UTF-8 path: {}", path.display()))?;
    let mut url = String::with_capacity(text.len() + 7);
    url.push_str("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            url.push(char::from(byte));
        } else {
            // Writing to a String cannot fail.
            let _ = write!(url, "%{byte:02X}");
        }
    }
    Ok(url)
}

/// Starts the browser on `target`, a whole link or a file's URL.
fn start(target: &str, command: Option<&str>, display: Option<&Path>) -> Result<String, String> {
    let environment = std::env::var("BROWSER").ok();
    let (program, mut process) = prepare(target, command, environment.as_deref(), display)?;
    let mut child = process.spawn().map_err(|e| format!("{program}: {e}"))?;
    // A thread that cannot be made leaves the browser unreaped until this
    // process exits; the link is open either way.
    let _ = std::thread::Builder::new()
        .name("td-ui-open".into())
        .spawn(move || child.wait());
    Ok(program)
}

/// The browser's process for `target`, not yet started, and its program.
fn prepare(
    target: &str,
    command: Option<&str>,
    browser: Option<&str>,
    display: Option<&Path>,
) -> Result<(String, Command), String> {
    let argv = argv(target, command, browser)?;
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

/// The words run for `target`, a whole link or a file's URL: the first
/// command of `command`, `browser` (the environment's) and `xdg-open`
/// that has a word, split on whitespace. A word holding `{url}` has it
/// replaced by the target, quotes put around the bare placeholder taken
/// off as a shell would, and a command without one gets the target as its
/// last word.
fn argv(target: &str, command: Option<&str>, browser: Option<&str>) -> Result<Vec<String>, String> {
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
        argv.push(word.replace(PLACEHOLDER, target));
    }
    if !placed {
        argv.push(target.to_string());
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
        const UNSTARTABLE: Option<&str> = Some("/nonexistent/td-ui-browser");
        // Past the finder's bound either side of a press, still one link:
        // refused only by the browser that cannot start.
        let long = format!(
            "https://e.example/{}",
            "a".repeat(3 * crate::links::MAX_BYTES)
        );
        let error = link(&long, UNSTARTABLE).unwrap_err();
        assert!(error.starts_with("/nonexistent/td-ui-browser: "), "{error}");
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
            let error = link(text, UNSTARTABLE).unwrap_err();
            assert_eq!(error, format!("not a link: {text}"));
        }
    }

    #[test]
    fn a_listed_url_is_opened_however_it_ends() {
        const UNSTARTABLE: Option<&str> = Some("/nonexistent/td-ui-browser");
        // What the text rule leaves out, a listed URL's ends are given.
        for listed in [
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "https://e.example/it's",
            "https://e.example/a.",
            "http://e.example/[x]?q=\"y\"",
            "HTTPS://E.example/a",
            "Http://e.example/",
        ] {
            assert!(!crate::links::whole(listed), "{listed}");
            let error = url(listed, UNSTARTABLE).unwrap_err();
            assert!(
                error.starts_with("/nonexistent/td-ui-browser: "),
                "{listed}: {error}"
            );
        }
        for text in [
            "",
            "-x",
            "https://",
            "ftp://x",
            "HTTPS://",
            "httpé://x",
            " https://x",
            "https://x y",
            "https://x\ty",
            "https://x\u{7f}",
            "https://x\u{85}",
        ] {
            let error = url(text, UNSTARTABLE).unwrap_err();
            assert_eq!(error, format!("not a link: {text}"));
        }
    }

    #[test]
    fn a_file_is_its_absolute_path_as_one_percent_encoded_url() {
        assert_eq!(
            file_url(Path::new("/tmp/td-news digest/ä#?$(x)'.html")).unwrap(),
            "file:///tmp/td-news%20digest/%C3%A4%23%3F%24%28x%29%27.html"
        );
        assert_eq!(
            file_url(Path::new("/a/B_9.~-/c")).unwrap(),
            "file:///a/B_9.~-/c"
        );
        for relative in ["", "a.html", "-x", "./a"] {
            assert!(file_url(Path::new(relative)).is_err(), "{relative}");
        }
        let url = file_url(Path::new("/tmp/d.html")).unwrap();
        assert_eq!(
            argv(&url, Some("firefox --new-tab {url}"), None).unwrap(),
            ["firefox", "--new-tab", "file:///tmp/d.html"]
        );
        let error = file(Path::new("/tmp/d.html"), Some("/nonexistent/td-ui-browser")).unwrap_err();
        assert!(error.starts_with("/nonexistent/td-ui-browser: "), "{error}");
        assert!(file(Path::new("d.html"), Some("firefox")).is_err());
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
