//! The real headless `td-compositor` driving the td-mail window. td's
//! shared native harness (`td-test-compositor`) launches the compositor
//! from the `TD_TEST_COMPOSITOR` binary and injects keys through its seat,
//! and `MailProcess` launches td-mail offline against its Wayland socket
//! with every directory it reads or writes inside the test's own. The one
//! case proves what the in-process session tests cannot: the chords reach
//! td-mail through a real keyboard, the finder opens over the draft, and
//! the file chosen lands in the draft's sidecar with its tag in the draft.

use super::*;

use td_test_compositor::{Compositor, Controls};

const KEY_E: u32 = 18;
const KEY_R: u32 = 19;
const KEY_P: u32 = 25;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_A: u32 = 30;
const KEY_S: u32 = 31;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_C: u32 = 46;

/// td-mail launched offline as an ordinary Wayland client, its home,
/// configuration, state and cache inside `directory`: one placeholder
/// account, never connected, since offline asks for no connection.
struct MailProcess {
    child: Child,
    log: PathBuf,
    home: PathBuf,
    state: PathBuf,
}
impl MailProcess {
    fn start(directory: &Directory, display: &Path) -> Self {
        let home = directory.0.join("home");
        let config = directory.0.join("config.toml");
        let state = home.join(".local/state");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(
            &config,
            "[account.test]\n\
             well_known_url = \"https://mail.invalid/.well-known/jmap\"\n\
             username = \"me@mail.invalid\"\n\
             password_command = \"false\"\n",
        )
        .unwrap();
        let log = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-mail"))
            .arg("--offline")
            .arg(format!("--config={}", config.display()))
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .env("HOME", &home)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .env("TD_UI_FACE", "bitmap")
            .spawn()
            .unwrap();
        Self {
            child,
            log,
            home,
            state,
        }
    }

    /// What td-mail said: its stderr and its log.
    fn said(&self) -> String {
        format!(
            "stderr: {}\nlog: {}",
            std::fs::read_to_string(&self.log).unwrap_or_default(),
            std::fs::read_to_string(self.state.join("td-mail/td-mail.log")).unwrap_or_default()
        )
    }

    /// The drafts retained, named as td-mail names them.
    fn drafts(&self) -> Vec<PathBuf> {
        let Ok(drafts) = std::fs::read_dir(self.state.join("td-mail/drafts")) else {
            return Vec::new();
        };
        drafts
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("td-mail-draft-") && name.ends_with(".eml")
                    })
            })
            .collect()
    }
}
impl Drop for MailProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ready` is polled until it answers, each poll spaced so the client it
/// waits on has the CPU; past `TIMEOUT` the wait fails with `what`.
fn wait<T>(mail: &MailProcess, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} within {TIMEOUT:?}\n{}",
            mail.said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// c composes a draft; Ctrl-Shift-A opens the finder over it in td-mail's
/// home; `rep` filters it to the one file and Return chooses it; the
/// backend copies the file into the draft's sidecar and the view tags it
/// in the draft, which Ctrl-S writes. Its path is typed nowhere: each step
/// is a chord through the compositor's seat, and each result is read from
/// the files td-mail retains.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn a_file_chosen_in_the_finder_is_attached_to_the_draft() {
    let compositor_directory = Directory::new("td-mail-process");
    let mut compositor = Compositor::start(&compositor_directory.0, Controls::default());
    let client_directory = Directory::new("td-mail-process");
    let mail = MailProcess::start(&client_directory, &compositor.display());
    std::fs::write(mail.home.join("report.pdf"), b"%PDF-1.4 native").unwrap();
    std::fs::write(mail.home.join("notes.txt"), b"not this one").unwrap();
    wait(&mail, "the td-mail window maps with the keyboard", || {
        compositor.focused("td-mail").then_some(())
    });

    // The draft is created and then written, so it is taken once its
    // template is whole: its From to the separator and empty line the
    // template ends with.
    compositor.chord(&[], KEY_C);
    let (draft, template) = wait(&mail, "c retains one draft, written", || {
        let [draft] = mail.drafts().try_into().ok()?;
        let text = std::fs::read_to_string(&draft).ok()?;
        (text.starts_with("From: me@mail.invalid\n")
            && text.ends_with("--text follows this line--\n\n"))
        .then_some((draft, text))
    });

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTSHIFT], KEY_A);
    for key in [KEY_R, KEY_E, KEY_P] {
        compositor.chord(&[], key);
    }
    compositor.chord(&[], KEY_ENTER);
    let name = draft.file_name().unwrap().to_str().unwrap();
    let id = name
        .strip_prefix("td-mail-draft-")
        .and_then(|name| name.strip_suffix(".eml"))
        .unwrap();
    let copy = draft
        .parent()
        .unwrap()
        .join(format!("td-mail-att-{id}"))
        .join("report.pdf");
    wait(&mail, "the chosen file is copied into the sidecar", || {
        (std::fs::read(&copy).ok()? == b"%PDF-1.4 native").then_some(())
    });
    let copied: Vec<_> = std::fs::read_dir(copy.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(copied, ["report.pdf"], "only the file chosen");

    // The tag goes into the pane once the backend's answer is in, a moment
    // after the copy: each Ctrl-S writes the pane, the tag in it or not yet.
    let tag = format!(
        "<#part type=\"application/pdf\" filename=\"{}\" disposition=\"attachment\">\n<#/part>\n",
        copy.display()
    );
    let saved = wait(&mail, "Ctrl-S writes the tagged draft", || {
        compositor.chord(&[KEY_LEFTCTRL], KEY_S);
        std::thread::sleep(Duration::from_millis(50));
        let text = std::fs::read_to_string(&draft).ok()?;
        text.contains(&tag).then_some(text)
    });
    assert_eq!(saved, format!("{template}{tag}"));

    drop(mail);
    compositor.stop();
}
