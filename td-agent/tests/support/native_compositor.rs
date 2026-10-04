//! The real headless compositor driving the td-agent window. A copy of
//! td-mail's native harness: `Compositor` launches the compositor from
//! the `TD_TEST_COMPOSITOR` binary and injects keys through its seat, and
//! `AgentProcess` launches td-agent against its Wayland socket with its
//! state and configuration inside the test's own directory. The cases
//! prove what the widget-state tests cannot: the chords reach td-agent
//! through a real keyboard, the window starts a conversation process per
//! open conversation, and each message lands in that conversation's log;
//! F10 opens the File menu, whose Set OpenRouter key… opens a masked
//! dialog that never shows what is typed; its Conversation menu's
//! Model… opens the picker over the models list, and the model chosen
//! there is logged by the open conversation; and, in the `test-key-root`
//! build (`fixture`), a key typed there and saved is stored mode 0600 and
//! handed to the running conversation, and appears nowhere else.

use super::*;

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::thread::JoinHandle;

use td_agent::store::{parse_log, Kind};
use td_ui::control::{frame, Decoder};

const KEY_ESC: u32 = 1;
const KEY_1: u32 = 2;
const KEY_MINUS: u32 = 12;
const KEY_R: u32 = 19;
const KEY_I: u32 = 23;
const KEY_O: u32 = 24;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_A: u32 = 30;
const KEY_S: u32 = 31;
const KEY_K: u32 = 37;
const KEY_H: u32 = 35;
const KEY_X: u32 = 45;
const KEY_C: u32 = 46;
const KEY_V: u32 = 47;
const KEY_B: u32 = 48;
const KEY_Y: u32 = 21;
const KEY_N: u32 = 49;
const KEY_F10: u32 = 68;
const KEY_PAGEDOWN: u32 = 109;
const KEY_DOWN: u32 = 108;
const KEY_RIGHT: u32 = 106;
const KEY_U: u32 = 22;

/// The key the cases type, and its keys on the seat.
const SECRET: &str = "sk-or-v1-abc";
const SECRET_KEYS: [u32; 12] = [
    KEY_S, KEY_K, KEY_MINUS, KEY_O, KEY_R, KEY_MINUS, KEY_V, KEY_1, KEY_MINUS, KEY_A, KEY_B, KEY_C,
];

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
/// configuration inside `directory`, with a control socket there when
/// asked for.
struct AgentProcess {
    child: Child,
    stderr: PathBuf,
    conversations: PathBuf,
    home: PathBuf,
    socket: Option<PathBuf>,
}
impl AgentProcess {
    fn launch(directory: &Directory, display: &Path, control: bool, env: &[(&str, &Path)]) -> Self {
        // A case may have put state there first.
        let home = directory.0.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let stderr = directory.0.join("stderr");
        let socket = control.then(|| directory.0.join("control"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-agent"));
        command.env_clear();
        if let Some(socket) = &socket {
            command.arg("--control-socket").arg(socket);
        }
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command
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
            home,
            socket,
        }
    }

    fn said(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// One request over the window's control socket: the seam's envelope
    /// out, the reply's fields after the version and the ID back.
    fn request(&self, id: u64, words: &[&str]) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        let socket = self.socket.as_ref().expect("a control socket");
        let mut stream = UnixStream::connect(socket).unwrap();
        let payload = format!("1\t{id}\t{}", words.join("\t"));
        write_until(&mut stream, &frame(payload.as_bytes()).unwrap(), deadline).unwrap();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 4096];
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline).unwrap()))
                .unwrap();
            let count = match stream.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.expect("control reply"),
            };
            if count == 0 {
                break;
            }
            decoder.push(&chunk[..count]).unwrap();
            if decoder.payload().is_some() {
                break;
            }
        }
        assert!(
            decoder.payload().is_some(),
            "request {id} {words:?}: no complete reply; stderr: {}",
            self.said()
        );
        let line = String::from_utf8(decoder.finish().unwrap()).unwrap();
        let fields: Vec<String> = line.split('\t').map(str::to_string).collect();
        assert_eq!(fields[0], "1", "{line}");
        assert_eq!(fields[1], id.to_string(), "{line}");
        fields[2..].to_vec()
    }

    /// The window's facts, as the driven seam's `state` gives them.
    fn state(&self) -> String {
        self.request(1, &["state"]).join("\t")
    }

    /// What the window shows, as the driven seam's `text` reads it.
    fn text(&self) -> String {
        let reply = self.request(2, &["text"]);
        String::from_utf8(td_ui::control::unhex(reply.last().unwrap()).unwrap()).unwrap()
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
/// Every file under `dir` whose bytes hold `needle`.
fn holding(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            found.extend(holding(&path, needle));
        } else if kind.is_file() {
            let bytes = std::fs::read(&path).unwrap_or_default();
            if bytes.windows(needle.len()).any(|w| w == needle) {
                found.push(path);
            }
        }
    }
    found
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

/// The window opens with no conversation and makes none; Control-N
/// starts one, where `hi` and Return send `hi` to its log and title it;
/// Control-N starts another, where `yo`, Shift-Return and `o` sent with
/// Control-Return log `yo` and `o` on two lines and title it `yo`, and
/// lists it first; Control-PageDown opens the first again in a fresh
/// process, which goes on from its log. Every step is a chord through
/// the compositor's seat, and every result is read from the store.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn messages_typed_into_the_window_land_in_each_conversations_log() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    let agent = AgentProcess::launch(
        &client_directory,
        &compositor.directory.join("wayland-0"),
        true,
        &[],
    );
    wait(&agent, "the td-agent window maps with the keyboard", || {
        compositor.focused("td-agent").then_some(())
    });
    let first = |users: &[&str]| {
        (
            "conversation".to_string(),
            "hi".to_string(),
            users.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        )
    };
    wait(&agent, "the window opens no conversation", || {
        agent
            .state()
            .contains("conversations=0\tactive=none")
            .then_some(())
    });
    assert!(agent.conversations().is_empty());

    compositor.chord(&[KEY_LEFTCTRL], KEY_N);
    wait(&agent, "Control-N starts a conversation", || {
        (agent.conversations().len() == 1).then_some(())
    });
    compositor.chord(&[], KEY_H);
    compositor.chord(&[], KEY_I);
    compositor.chord(&[], KEY_ENTER);
    wait(&agent, "hi lands in its log, titled", || {
        (agent.conversations() == [first(&["hi"])]).then_some(())
    });

    compositor.chord(&[KEY_LEFTCTRL], KEY_N);
    compositor.chord(&[], KEY_Y);
    compositor.chord(&[], KEY_O);
    // Shift-Return is a newline in the composer, not a send; Control-
    // Return sends as Return does.
    compositor.chord(&[KEY_LEFTSHIFT], KEY_ENTER);
    compositor.chord(&[], KEY_O);
    compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
    let conversation = (
        "conversation".to_string(),
        "yo".to_string(),
        vec!["yo\no".to_string()],
    );
    wait(&agent, "yo lands in a new conversation, titled", || {
        (agent.conversations() == [first(&["hi"]), conversation.clone()]).then_some(())
    });

    // The newer is listed first, so the first is below it.
    compositor.chord(&[KEY_LEFTCTRL], KEY_PAGEDOWN);
    compositor.chord(&[], KEY_X);
    compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
    wait(
        &agent,
        "x lands after hi in the first conversation's log",
        || (agent.conversations() == [first(&["hi", "x"]), conversation.clone()]).then_some(()),
    );

    drop(agent);
    compositor.stop();
}

/// F10 through the seat opens the File menu; Down and Return choose Set
/// OpenRouter key…, whose dialog takes the keys typed on the seat into a
/// masked entry the window shows as bullets and the control socket's
/// state as a length; Escape closes it, emptied. The key is in nothing
/// the window shows or says.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn the_file_menu_opens_a_masked_key_dialog() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    let agent = AgentProcess::launch(
        &client_directory,
        &compositor.directory.join("wayland-0"),
        true,
        &[],
    );
    wait(&agent, "the td-agent window maps with the keyboard", || {
        compositor.focused("td-agent").then_some(())
    });
    wait(&agent, "the window says there is no key", || {
        agent.state().contains("no key: File").then_some(())
    });
    compositor.chord(&[], KEY_F10);
    wait(&agent, "F10 opens the File menu", || {
        agent.state().contains("menu=open").then_some(())
    });
    assert!(
        agent.text().contains("Set OpenRouter key"),
        "{}",
        agent.text()
    );
    // Set OpenRouter key…, below the three New items.
    for _ in 0..3 {
        compositor.chord(&[], KEY_DOWN);
    }
    compositor.chord(&[], KEY_ENTER);
    wait(&agent, "the item opens the key dialog", || {
        agent
            .state()
            .contains("menu=closed\tdialog=entry")
            .then_some(())
    });
    for key in SECRET_KEYS {
        compositor.chord(&[], key);
    }
    let length = format!("entry={}", SECRET.len());
    wait(&agent, "the keys reach the masked entry", || {
        agent.state().contains(&length).then_some(())
    });
    let shown = agent.text();
    assert!(shown.contains(&"\u{2022}".repeat(SECRET.len())), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(!agent.state().contains(SECRET));
    compositor.chord(&[], KEY_ESC);
    wait(&agent, "Escape closes the dialog, emptied", || {
        agent.state().contains("dialog=none\tentry=0").then_some(())
    });
    assert!(!agent.home.join("config/td-agent/openrouter.key").exists());
    assert!(!agent.said().contains(SECRET));
    assert!(holding(&client_directory.0, SECRET.as_bytes()).is_empty());
    drop(agent);
    compositor.stop();
}

/// Conversation → Model… opens the picker over the cached models list;
/// typed into and chosen, the model is logged by the open conversation.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn the_conversation_menu_chooses_a_model_the_conversation_logs() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    // The models list as the window would have cached it.
    let state = td_agent::store::StateDir::at(client_directory.0.join("home/state/td-agent"));
    state.ensure().unwrap();
    td_agent::models::Models::from_provider(include_bytes!("../fixtures/openrouter/models.json"))
        .unwrap()
        .save(state.root())
        .unwrap();
    let agent = AgentProcess::launch(
        &client_directory,
        &compositor.directory.join("wayland-0"),
        true,
        &[],
    );
    wait(&agent, "the td-agent window maps with the keyboard", || {
        compositor.focused("td-agent").then_some(())
    });
    compositor.chord(&[KEY_LEFTCTRL], KEY_N);
    wait(
        &agent,
        "Control-N starts a conversation and opens it",
        || {
            let state = agent.state();
            (!state.contains("active=none") && state.contains("model=anthropic/claude-sonnet-5.5"))
                .then_some(())
        },
    );
    compositor.chord(&[], KEY_F10);
    compositor.chord(&[], KEY_RIGHT);
    wait(&agent, "Right moves to the Conversation menu", || {
        agent.text().contains("Model\u{2026}").then_some(())
    });
    compositor.chord(&[], KEY_ENTER);
    wait(
        &agent,
        "Model… opens the picker on the conversation's model",
        || {
            agent
                .state()
                .contains("picker=anthropic/claude-sonnet-5.5")
                .then_some(())
        },
    );
    for key in [KEY_H, KEY_A, KEY_I, KEY_K, KEY_U] {
        compositor.chord(&[], key);
    }
    wait(&agent, "typing filters to the model", || {
        agent
            .state()
            .contains("picker=anthropic/claude-haiku-4.5\tpicking=conversation\tquery=haiku")
            .then_some(())
    });
    compositor.chord(&[], KEY_ENTER);
    wait(&agent, "the conversation logs the choice", || {
        agent
            .state()
            .contains("model=anthropic/claude-haiku-4.5")
            .then_some(())
    });
    let chosen = std::fs::read_dir(&agent.conversations)
        .unwrap()
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join("log")).ok())
        .flat_map(|log| parse_log(&log).unwrap().0)
        .find_map(|event| match event.kind {
            Kind::Choice { model, .. } => Some(model),
            _ => None,
        });
    assert_eq!(chosen, Some(Some("anthropic/claude-haiku-4.5".to_string())));
    assert!(agent.state().contains("picker=none"));
    drop(agent);
    compositor.stop();
}

/// The cases the `test-key-root` build runs, where the key file's walk
/// checks from the test's own directory rather than its shared `/`.
#[cfg(feature = "test-key-root")]
mod fixture {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// The outcome of each turn the conversations' logs have finished.
    fn outcomes(agent: &AgentProcess) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&agent.conversations) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let log = std::fs::read(entry.path().join("log")).unwrap_or_default();
            let (events, _) = parse_log(&log).unwrap();
            found.extend(events.into_iter().filter_map(|e| match e.kind {
                Kind::Finished { outcome, .. } => Some(outcome),
                _ => None,
            }));
        }
        found
    }

    /// A key typed into the dialog and saved with Return is stored as
    /// `config/td-agent/openrouter.key`, mode 0600, in a directory made
    /// mode 0700; the status row stops asking for one; the conversation's
    /// process, running all along, is handed it, so its next turn no
    /// longer stops for want of a key; and the key is in no other file
    /// the window or its conversations wrote, nor on standard error.
    #[test]
    #[ignore = "ready supplies the disposable native compositor"]
    fn a_key_saved_from_the_dialog_is_stored_0600_and_used() {
        let compositor_directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let client_directory = Directory::new();
        // The configuration home, which the dialog does not make.
        let config = client_directory.0.join("home/config");
        // Canonical, so a linked `/tmp` cannot leave the walk checking
        // nothing above the root it is given.
        let root = std::fs::canonicalize(&client_directory.0).unwrap();
        let agent = AgentProcess::launch(
            &client_directory,
            &compositor.directory.join("wayland-0"),
            true,
            &[("TD_AGENT_TEST_KEY_ROOT", &root)],
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&config)
            .unwrap();
        wait(&agent, "the td-agent window maps with the keyboard", || {
            compositor.focused("td-agent").then_some(())
        });
        compositor.chord(&[KEY_LEFTCTRL], KEY_N);
        wait(&agent, "Control-N starts a conversation", || {
            (agent.conversations().len() == 1).then_some(())
        });
        // Without a key, a turn stops for want of one.
        compositor.chord(&[], KEY_H);
        compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
        wait(&agent, "a turn without a key stops so", || {
            let outcomes = outcomes(&agent);
            (outcomes.len() == 1).then_some(outcomes)
        })
        .iter()
        .for_each(|outcome| assert!(outcome.starts_with("no API key"), "{outcome}"));

        compositor.chord(&[], KEY_F10);
        // Set OpenRouter key…, below the three New items.
        for _ in 0..3 {
            compositor.chord(&[], KEY_DOWN);
        }
        compositor.chord(&[], KEY_ENTER);
        wait(&agent, "the item opens the key dialog", || {
            agent.state().contains("dialog=entry").then_some(())
        });
        for key in SECRET_KEYS {
            compositor.chord(&[], key);
        }
        let length = format!("entry={}", SECRET.len());
        wait(&agent, "the keys reach the masked entry", || {
            agent.state().contains(&length).then_some(())
        });
        compositor.chord(&[], KEY_ENTER);
        let path = config.join("td-agent/openrouter.key");
        wait(
            &agent,
            "Return stores the key and closes the dialog",
            || agent.state().contains("dialog=none\tentry=0").then_some(()),
        );
        let state = agent.state();
        assert!(!state.contains("no key"), "{state}");
        assert!(state.contains("the key is stored in"), "{state}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("{SECRET}\n").as_bytes()
        );
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!((meta.mode() & 0o7777, meta.nlink()), (0o600, 1));
        let dir = std::fs::metadata(config.join("td-agent")).unwrap();
        assert_eq!(dir.permissions().mode() & 0o7777, 0o700);
        assert!(!config.join("td-agent/openrouter.key.tmp").exists());

        // The running conversation has the key: its next turn goes on past
        // it, to the price check, which stops it, the models list never
        // fetched here.
        compositor.chord(&[], KEY_X);
        compositor.chord(&[KEY_LEFTCTRL], KEY_ENTER);
        let outcomes = wait(&agent, "a turn with the key goes past it", || {
            let outcomes = outcomes(&agent);
            (outcomes.len() == 2).then_some(outcomes)
        });
        assert!(
            outcomes[1].starts_with("no price is known for"),
            "{outcomes:?}"
        );

        drop(agent);
        compositor.stop();
        let stderr = std::fs::read_to_string(client_directory.0.join("stderr")).unwrap();
        assert!(!stderr.contains(SECRET), "{stderr}");
        assert_eq!(holding(&client_directory.0, SECRET.as_bytes()), [path]);
    }
}
