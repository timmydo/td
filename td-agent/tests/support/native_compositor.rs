//! The real headless compositor driving the td-agent window. A copy of
//! td-mail's native harness: `Compositor` launches the compositor from
//! the `TD_TEST_COMPOSITOR` binary and injects keys through its seat, and
//! `AgentProcess` launches td-agent against its Wayland socket with its
//! state and configuration inside the test's own directory. The one case
//! proves what the widget-state tests cannot: the chords reach td-agent
//! through a real keyboard, the window starts a conversation process per
//! open conversation, and each message lands in that conversation's log.

use super::*;

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::thread::JoinHandle;

use td_agent::store::{parse_log, Kind};

const KEY_I: u32 = 23;
const KEY_O: u32 = 24;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_H: u32 = 35;
const KEY_X: u32 = 45;
const KEY_Y: u32 = 21;
const KEY_N: u32 = 49;
const KEY_PAGEUP: u32 = 104;

struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    action: u64,
    output: Option<JoinHandle<()>>,
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
                .expect("set TD_TEST_COMPOSITOR to an explicitly built compositor"),
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
            action: 0,
            output: None,
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

    /// Whether a toplevel carrying `app_id` is mapped and has the
    /// keyboard, which the seat's keys then reach.
    fn focused(&self, app_id: &str) -> bool {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window "))
            .any(|line| {
                field(line, "app_id=") == Some(app_id) && field(line, "focused=") == Some("true")
            })
    }

    fn key(&mut self, key: u32, down: bool) {
        // Timestamps follow the receipt counter; callers do not send action IDs.
        let time = self.action + 1;
        let line = format!(
            "key {} {time} {key} {}",
            self.session,
            if down { "down" } else { "up" }
        );
        self.receipt(&line);
    }

    fn receipt(&mut self, line: &str) {
        let action = self.action + 1;
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(line, 1024), expected.as_bytes());
        self.action = action;
    }

    /// `key` tapped with every one of `modifiers` held, pressed in order
    /// and released in reverse.
    fn chord(&mut self, modifiers: &[u32], key: u32) {
        for modifier in modifiers {
            self.key(*modifier, true);
        }
        self.key(key, true);
        self.key(key, false);
        for modifier in modifiers.iter().rev() {
            self.key(*modifier, false);
        }
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

/// td-agent launched as an ordinary Wayland client, its home, state and
/// configuration inside `directory`.
struct AgentProcess {
    child: Child,
    stderr: PathBuf,
    conversations: PathBuf,
}
impl AgentProcess {
    fn start(directory: &Directory, display: &Path) -> Self {
        let home = directory.0.join("home");
        std::fs::create_dir(&home).unwrap();
        let stderr = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-agent"))
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .env("HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("TD_UI_FACE", "bitmap")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stderr,
            conversations: home.join("state/td-agent/conversations"),
        }
    }

    fn said(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// Each conversation's role, title and the human's messages in its
    /// log, as the store holds them now.
    fn conversations(&self) -> Vec<(String, String, Vec<String>)> {
        let Ok(entries) = std::fs::read_dir(&self.conversations) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(meta) = std::fs::read_to_string(dir.join("meta")) else {
                continue;
            };
            let field = |name: &str| {
                let key = format!("\"{name}\":\"");
                meta.split_once(&key)
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(value, _)| value.to_string())
                    .unwrap_or_default()
            };
            let log = std::fs::read(dir.join("log")).unwrap_or_default();
            let (events, _) = parse_log(&log).unwrap();
            let users = events
                .into_iter()
                .filter_map(|e| match e.kind {
                    Kind::User { text, .. } => Some(text),
                    _ => None,
                })
                .collect();
            found.push((field("role"), field("title"), users));
        }
        found.sort();
        found
    }
}
impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ready` is polled until it answers, each poll spaced so the client it
/// waits on has the CPU; past `TIMEOUT` the wait fails with `what`.
fn wait<T>(agent: &AgentProcess, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} within {TIMEOUT:?}\n{}",
            agent.said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The window opens the orchestrator, creating it; `hi` and Control-Return
/// sends it to the orchestrator's log; Control-N starts a conversation
/// whose process logs `yo` and titles it so; Control-PageUp opens the
/// orchestrator again in a fresh process, which goes on from its log.
/// Every step is a chord through the compositor's seat, and every result
/// is read from the store.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn messages_typed_into_the_window_land_in_each_conversations_log() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    let agent = AgentProcess::start(&client_directory, &compositor.directory.join("wayland-0"));
    wait(&agent, "the td-agent window maps with the keyboard", || {
        compositor.focused("td-agent").then_some(())
    });
    let orchestrator = |users: &[&str]| {
        (
            "orchestrator".to_string(),
            "Orchestrator".to_string(),
            users.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
    };
    wait(&agent, "the orchestrator is created", || {
        (agent.conversations() == [orchestrator(&[])]).then_some(())
    });

    compositor.chord(&[], KEY_H);
    compositor.chord(&[], KEY_I);
    compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
    wait(&agent, "hi lands in the orchestrator's log", || {
        (agent.conversations() == [orchestrator(&["hi"])]).then_some(())
    });

    compositor.chord(&[KEY_LEFTCTRL], KEY_N);
    compositor.chord(&[], KEY_Y);
    compositor.chord(&[], KEY_O);
    // Return is a newline in the composer, not a send.
    compositor.chord(&[], KEY_ENTER);
    compositor.chord(&[], KEY_O);
    compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
    let conversation = (
        "conversation".to_string(),
        "yo".to_string(),
        vec!["yo\no".to_string()],
    );
    wait(&agent, "yo lands in a new conversation, titled", || {
        (agent.conversations() == [conversation.clone(), orchestrator(&["hi"])]).then_some(())
    });

    compositor.chord(&[KEY_LEFTCTRL], KEY_PAGEUP);
    compositor.chord(&[], KEY_X);
    compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
    wait(&agent, "x lands after hi in the orchestrator's log", || {
        (agent.conversations() == [conversation.clone(), orchestrator(&["hi", "x"])]).then_some(())
    });

    drop(agent);
    compositor.stop();
}
