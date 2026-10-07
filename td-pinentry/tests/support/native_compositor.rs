//! The real headless `td-compositor` driving td-pinentry's window. td's
//! shared native harness (`td-test-compositor`) launches it from the
//! `TD_TEST_COMPOSITOR` binary with input control and injects keys
//! through its seat; `Prompt` starts td-pinentry against its Wayland
//! socket, as an askpass program or as gpg-agent's pinentry, and reads
//! what it answers.

use super::*;

use std::process::ExitStatus;

use td_test_compositor::{Compositor, Controls};

const KEY_ESC: u32 = 1;
const KEY_5: u32 = 6;
const KEY_P: u32 = 25;
const KEY_ENTER: u32 = 28;
const KEY_A: u32 = 30;
const KEY_S: u32 = 31;
const KEY_LEFTSHIFT: u32 = 42;

/// `p`, `a`, `s` and Shift+5's `%`, the escape the protocol must carry.
fn type_secret(compositor: &mut Compositor) {
    for key in [KEY_P, KEY_A, KEY_S] {
        compositor.tap(key);
    }
    compositor.key(KEY_LEFTSHIFT, true);
    compositor.tap(KEY_5);
    compositor.key(KEY_LEFTSHIFT, false);
}

/// td-pinentry started as an ordinary Wayland client, its runtime inside
/// `directory`, its answer on a pipe.
struct Prompt {
    child: Child,
    log: PathBuf,
}

/// What td-pinentry says on td, where it asks for nothing.
const ON_TD: &str = "td-pinentry: td-pinentry asks for secrets on foreign desktops; td asks on its secure-attention path\n";

impl Prompt {
    fn start(directory: &Directory, display: &Path, args: &[&str]) -> Self {
        let log = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-pinentry"))
            .args(args)
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("HOME", &directory.0)
            .env("TD_UI_FACE", "bitmap")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn said(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits until the window has the keyboard; false when td-pinentry
    /// refused before mapping because this is td.
    fn mapped(&mut self, compositor: &Compositor) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if compositor.focused("td-pinentry") {
                return true;
            }
            if self.child.try_wait().unwrap().is_some() {
                assert_eq!(self.said(), ON_TD);
                return false;
            }
            assert!(
                Instant::now() < deadline,
                "the window maps with the keyboard within {TIMEOUT:?}\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Its whole standard output and exit status once it ends.
    fn finish(mut self) -> (String, ExitStatus) {
        drop(self.child.stdin.take());
        let mut stdout = self.child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        });
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "td-pinentry exits within {TIMEOUT:?}\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        (reader.join().unwrap(), status)
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ssh's askpass: the typed passphrase is the one line written, and
/// the program exits 0.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn askpass_writes_the_typed_passphrase() {
    let compositor_directory = Directory::new("td-pinentry-process");
    let mut compositor = Compositor::start(&compositor_directory.0, Controls::default());
    let client = Directory::new("td-pinentry-process");
    let mut prompt = Prompt::start(
        &client,
        &compositor.display(),
        &["Enter passphrase for key '/tmp/id': "],
    );
    if prompt.mapped(&compositor) {
        type_secret(&mut compositor);
        compositor.tap(KEY_ENTER);
        let (out, status) = prompt.finish();
        assert_eq!(out, "pas%\n");
        assert!(status.success());
    }
    compositor.stop();
}

/// Escape refuses: nothing is written and the program exits 1.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn askpass_escape_answers_nothing() {
    let compositor_directory = Directory::new("td-pinentry-process");
    let mut compositor = Compositor::start(&compositor_directory.0, Controls::default());
    let client = Directory::new("td-pinentry-process");
    let mut prompt = Prompt::start(&client, &compositor.display(), &["Password: "]);
    if prompt.mapped(&compositor) {
        type_secret(&mut compositor);
        compositor.tap(KEY_ESC);
        let (out, status) = prompt.finish();
        assert_eq!(out, "");
        assert_eq!(status.code(), Some(1));
    }
    compositor.stop();
}

/// gpg-agent's conversation: GETPIN opens the window, the typed
/// passphrase comes back escaped in a D line, and BYE ends it.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn pinentry_returns_the_passphrase_to_the_agent() {
    let compositor_directory = Directory::new("td-pinentry-process");
    let mut compositor = Compositor::start(&compositor_directory.0, Controls::default());
    let client = Directory::new("td-pinentry-process");
    let mut prompt = Prompt::start(&client, &compositor.display(), &["--display", ":0"]);
    let mut stdin = prompt.child.stdin.take().unwrap();
    stdin
        .write_all(b"SETDESC Unlock the key%0A\"Some One\"\nSETPROMPT Passphrase:\nGETPIN\n")
        .unwrap();
    if prompt.mapped(&compositor) {
        type_secret(&mut compositor);
        compositor.tap(KEY_ENTER);
        stdin.write_all(b"BYE\n").unwrap();
        drop(stdin);
        let (out, status) = prompt.finish();
        assert_eq!(
            out,
            "OK Pleased to meet you\nOK\nOK\nD pas%25\nOK\nOK closing connection\n"
        );
        assert!(status.success(), "{status:?}");
    }
    compositor.stop();
}
