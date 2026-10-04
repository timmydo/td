//! The real headless `td-compositor` driving td-pinentry's window, after
//! td-pass's harness: `Compositor` launches it from the
//! `TD_TEST_COMPOSITOR` binary with input control and injects keys
//! through its seat; `Prompt` starts td-pinentry against its Wayland
//! socket, as an askpass program or as gpg-agent's pinentry, and reads
//! what it answers.

use super::*;

use std::io::{BufRead, BufReader};
use std::process::ExitStatus;
use std::sync::mpsc;
use std::thread::JoinHandle;

const KEY_ESC: u32 = 1;
const KEY_5: u32 = 6;
const KEY_P: u32 = 25;
const KEY_ENTER: u32 = 28;
const KEY_A: u32 = 30;
const KEY_S: u32 = 31;
const KEY_LEFTSHIFT: u32 = 42;

struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    output: Option<JoinHandle<()>>,
    /// Input receipts counted so far; each injection expects the next.
    action: u64,
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

fn identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Compositor {
    fn start(directory: &Directory) -> Self {
        let binary = PathBuf::from(
            std::env::var_os("TD_TEST_COMPOSITOR")
                .expect("set TD_TEST_COMPOSITOR to an explicitly built td-compositor"),
        );
        assert!(
            binary.is_absolute(),
            "compositor test tool must be an absolute path"
        );
        let session_dir = directory.0.join("session");
        let child = Command::new(binary)
            .arg("headless")
            .arg("--session-dir")
            .arg(&session_dir)
            .args([
                "--width",
                "800",
                "--height",
                "600",
                "--input-control",
                "enabled",
            ])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(directory.0.join("compositor.log")).unwrap())
            .spawn()
            .unwrap();
        // Establish cleanup before any later setup or readiness can unwind.
        let mut compositor = Self {
            child,
            directory: session_dir,
            session: String::new(),
            output: None,
            action: 0,
        };
        let stdout = compositor.child.stdout.take().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        compositor.output = Some(
            std::thread::Builder::new()
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    if reader.by_ref().take(4097).read_line(&mut line).is_ok() && line.len() <= 4096
                    {
                        let _ = send.send(line);
                    }
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                })
                .unwrap(),
        );
        let ready = receive
            .recv_timeout(TIMEOUT)
            .expect("compositor readiness deadline");
        let session = ready
            .strip_prefix("TD-COMPOSITOR-HEADLESS-READY version=2 session=")
            .and_then(|line| line.strip_suffix(" width=800 height=600 scale=1\n"))
            .expect("compositor readiness grammar");
        assert!(identity(session));
        compositor.session = session.to_string();
        compositor
    }

    fn display(&self) -> PathBuf {
        self.directory.join("wayland-0")
    }

    fn request(&self, line: &str, limit: usize) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = UnixStream::connect(self.directory.join("td-control")).unwrap();
        write_until(&mut stream, format!("{line}\n").as_bytes(), deadline).unwrap();
        let mut reply = Vec::new();
        let mut chunk = [0; 16384];
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline).unwrap()))
                .unwrap();
            let count = match stream.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.expect("compositor reply"),
            };
            if count == 0 {
                break;
            }
            assert!(reply.len() + count <= limit, "compositor reply byte bound");
            reply.extend_from_slice(&chunk[..count]);
        }
        reply
    }

    /// Whether a mapped toplevel carrying `app_id` has the keyboard.
    fn focused(&self, app_id: &str) -> bool {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window "))
            .any(|line| {
                field(line, "app_id=") == Some(app_id) && field(line, "focused=") == Some("true")
            })
    }

    /// One synthetic input request and its receipt.
    fn receipt(&mut self, line: &str) {
        let action = self.action + 1;
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(line, 1024), expected.as_bytes());
        self.action = action;
    }

    fn key(&mut self, key: u32, down: bool) {
        let line = format!(
            "key {} {} {key} {}",
            self.session,
            self.action + 1,
            if down { "down" } else { "up" }
        );
        self.receipt(&line);
    }

    fn tap(&mut self, key: u32) {
        self.key(key, true);
        self.key(key, false);
    }

    /// `p`, `a`, `s` and Shift+5's `%`, the escape the protocol must
    /// carry.
    fn type_secret(&mut self) {
        for key in [KEY_P, KEY_A, KEY_S] {
            self.tap(key);
        }
        self.key(KEY_LEFTSHIFT, true);
        self.tap(KEY_5);
        self.key(KEY_LEFTSHIFT, false);
    }

    fn stop(&mut self) {
        self.child.stdin.take();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "compositor owner-EOF deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!self.directory.exists());
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Some(parent) = self.directory.parent() {
                eprintln!(
                    "compositor log:\n{}",
                    std::fs::read_to_string(parent.join("compositor.log")).unwrap_or_default()
                );
            }
        }
        self.child.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
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
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client = Directory::new();
    let mut prompt = Prompt::start(
        &client,
        &compositor.display(),
        &["Enter passphrase for key '/tmp/id': "],
    );
    if prompt.mapped(&compositor) {
        compositor.type_secret();
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
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client = Directory::new();
    let mut prompt = Prompt::start(&client, &compositor.display(), &["Password: "]);
    if prompt.mapped(&compositor) {
        compositor.type_secret();
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
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client = Directory::new();
    let mut prompt = Prompt::start(&client, &compositor.display(), &["--display", ":0"]);
    let mut stdin = prompt.child.stdin.take().unwrap();
    stdin
        .write_all(b"SETDESC Unlock the key%0A\"Some One\"\nSETPROMPT Passphrase:\nGETPIN\n")
        .unwrap();
    if prompt.mapped(&compositor) {
        compositor.type_secret();
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
