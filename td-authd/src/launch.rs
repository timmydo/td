//! Fixed terminal launches for the separately owned compositor.

use crate::channel::Channel;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const LIMIT: usize = 16;
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION: &[u8] = b"TDLA002\n";
const TASK_DIRECTORY: &str = "/home/tester/src/td-vm/work";

pub(crate) struct Config {
    user: String,
    owner: u32,
    compositor: u32,
}

impl Config {
    // Keep the name grammar aligned with td-firstboot check-launch-session.
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        let [user_flag, user, owner_flag, owner, peer_flag, compositor] = arguments else {
            return Err("terminal-serve requires --user USER --uid UID --peer-uid UID".into());
        };
        if user_flag != "--user"
            || owner_flag != "--uid"
            || peer_flag != "--peer-uid"
            || user.is_empty()
            || user.starts_with('-')
            || user.len() > 32
            || !user
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
        {
            return Err("invalid terminal launch configuration".into());
        }
        Ok(Self {
            user: user.clone(),
            owner: number(owner, 1000..=1000)?,
            compositor: number(compositor, 1..=999)?,
        })
    }

    pub fn peer_uid(&self) -> u32 {
        self.compositor
    }

    fn checker(&self) -> Command {
        let mut command = Command::new("/bin/td-firstboot");
        command.args([
            "check-launch-session",
            &self.user,
            &self.owner.to_string(),
            &self.compositor.to_string(),
        ]);
        command
    }

    fn terminal(&self, generation: &str, handle: u64, terminal: Terminal) -> Command {
        let mut command = Command::new("/bin/td-login");
        command.args([
                "exec-as",
                &self.user,
                "--",
                "/bin/td-authd",
                "terminal-exec",
                &self.owner.to_string(),
                generation,
                &handle.to_string(),
            ]);
        if let Some(selection) = terminal.selection() {
            command.arg(selection);
        }
        command.process_group(0);
        command
    }
}

fn number(text: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    let value = text
        .parse::<u32>()
        .map_err(|_| "invalid terminal launch uid")?;
    if !range.contains(&value) || value.to_string() != text {
        return Err("noncanonical terminal launch uid".into());
    }
    Ok(value)
}

/// Before Channel duplicates stdin or installs a sender pidfd.
pub(crate) fn require_launch_startup() -> Result<(), String> {
    require_root_status(&bounded_file("/proc/self/status")?)?;
    require_no_terminal(&bounded_file("/proc/self/stat")?)?;
    audit_descriptors(
        |fd| {
            std::fs::metadata(format!("/proc/self/fd/{fd}"))
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|e| format!("missing standard descriptor {fd}: {e}"))
        },
        || {
            let mut descriptors = Vec::new();
            for entry in std::fs::read_dir("/proc/self/fd").map_err(|e| e.to_string())? {
                let name = entry.map_err(|e| e.to_string())?.file_name();
                descriptors.push(
                    name.to_str()
                        .ok_or("invalid inherited descriptor name")?
                        .parse::<u32>()
                        .map_err(|_| "invalid inherited descriptor")?,
                );
                if descriptors.len() > 4 {
                    return Err("terminal authority inherited extra descriptors".into());
                }
            }
            Ok(descriptors)
        },
    )
}

fn require_no_terminal(stat: &str) -> Result<(), String> {
    if stat
        .rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_whitespace().nth(4))
        != Some("0")
    {
        return Err("terminal authority must have no controlling terminal".into());
    }
    Ok(())
}

fn require_root_status(status: &str) -> Result<(), String> {
    if status.len() > 8192
        || !["Uid:", "Gid:"].iter().all(|key| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .is_some_and(|v| v.split_whitespace().collect::<Vec<_>>() == ["0", "0", "0", "0"])
        })
        || !status.lines().any(|line| {
            line.strip_prefix("Threads:")
                .is_some_and(|v| v.trim() == "1")
        })
    {
        return Err("terminal authority requires one root thread".into());
    }
    Ok(())
}

fn audit_descriptors(
    mut standard: impl FnMut(u32) -> Result<(u64, u64), String>,
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
) -> Result<(), String> {
    // Prove each standard fd exists BEFORE read_dir can reuse a missing one.
    // Single-threaded startup installs no handler that changes descriptors.
    let channel = standard(0)?;
    for fd in [1, 2] {
        if standard(fd)? == channel {
            return Err("terminal authority log aliases its private channel".into());
        }
    }
    let mut descriptors = enumerate()?;
    descriptors.sort_unstable();
    if descriptors != [0, 1, 2, 3] {
        return Err("terminal authority requires only standard descriptors".into());
    }
    Ok(())
}

fn bounded_file(path: &str) -> Result<String, String> {
    let mut text = String::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > 8192 {
        return Err("terminal process record is oversized".into());
    }
    Ok(text)
}

fn require_session_process(uid: u32, status: &str, cgroup: &str) -> Result<(), String> {
    let expected = uid.to_string();
    if !["Uid:", "Gid:"].iter().all(|key| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .is_some_and(|value| {
                value.split_whitespace().collect::<Vec<_>>() == [expected.as_str(); 4]
            })
    }) || cgroup != format!("0::/td-user-{uid}/session\n")
    {
        return Err("terminal is not in its checked human session".into());
    }
    Ok(())
}

fn terminal_command(uid: u32, generation: &str, handle: u64, terminal: Terminal) -> Command {
    let mut command = Command::new("/bin/td-term");
    command.env(
        "TD_CONTROL_SOCKET",
        format!("/run/td-compositor/{uid}/td-control"),
    );
    command.args([
        "run",
        "--socket",
        &format!("/run/td-compositor/{uid}/wayland-0"),
        "--ready-socket",
        &format!("/run/user/{uid}/td-auth-terminal-{generation}-{handle}.ready"),
    ]);
    if terminal != Terminal::Home {
        command.args(["--working-directory", TASK_DIRECTORY]);
    }
    match terminal {
        Terminal::Codex => {
            command.args(["--command", "/bin/cttyhack", "--stdin", "/bin/codex"]);
        }
        Terminal::Claude => {
            command.args(["--command", "/bin/cttyhack", "--stdin", "/bin/claude"]);
        }
        Terminal::Home | Terminal::Task => {}
    }
    command
}

/// Runs only after td-login dropped credentials, before any terminal code.
pub(crate) fn terminal_exec(arguments: &[String]) -> Result<(), String> {
    let (uid, generation, handle, terminal) = match arguments {
        [uid, generation, handle] => (uid, generation, handle, Terminal::Home),
        [uid, generation, handle, task] if task == "task" => {
            (uid, generation, handle, Terminal::Task)
        }
        [uid, generation, handle, agent] if agent == "codex" => {
            (uid, generation, handle, Terminal::Codex)
        }
        [uid, generation, handle, agent] if agent == "claude" => {
            (uid, generation, handle, Terminal::Claude)
        }
        _ => return Err("terminal-exec requires UID GENERATION HANDLE [task|codex|claude]".into()),
    };
    let uid = number(uid, 1000..=1000)?;
    if generation.len() != 32
        || !generation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid terminal generation".into());
    }
    let value = handle
        .parse::<u64>()
        .map_err(|_| "invalid terminal handle")?;
    if value == 0 || value.to_string() != *handle {
        return Err("noncanonical terminal handle".into());
    }
    require_session_process(
        uid,
        &bounded_file("/proc/self/status")?,
        &bounded_file("/proc/self/cgroup")?,
    )?;
    Err(format!(
        "exec session terminal: {}",
        terminal_command(uid, generation, value, terminal).exec()
    ))
}

fn generation() -> Result<String, String> {
    let mut nonce = [0u8; 16];
    File::open("/dev/urandom")
        .map_err(|e| e.to_string())?
        .read_exact(&mut nonce)
        .map_err(|e| e.to_string())?;
    let mut text = String::with_capacity(32);
    use std::fmt::Write;
    for byte in nonce {
        write!(text, "{byte:02x}").map_err(|e| e.to_string())?;
    }
    Ok(text)
}

fn spawn(command: &mut Command) -> Result<Child, String> {
    // The original private endpoint is fd 0, even though Channel's clone is
    // CLOEXEC. Replace all standard descriptors for every child, validator
    // included; no endpoint or root log descriptor crosses either exec.
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start terminal authority child: {e}"))
}

fn check_session(config: &Config) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(CHECK_TIMEOUT)
        .ok_or("launch check deadline overflow")?;
    let mut child = spawn(&mut config.checker())?;
    wait_check(&mut child, deadline)
}

fn wait_check(child: &mut Child, deadline: Instant) -> Result<(), String> {
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if Instant::now() >= deadline {
                    Err("launch session reservation check timed out".into())
                } else if status.success() {
                    Ok(())
                } else {
                    Err(format!("launch session reservation check failed: {status}"))
                }
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err("launch session reservation check timed out".into()),
            Err(error) => break Err(format!("wait for launch session check: {error}")),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Start(Terminal),
    Poll(u64),
    Heartbeat,
    Secret(crate::session::Request),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Home,
    Task,
    Codex,
    Claude,
}

impl Terminal {
    fn selection(self) -> Option<&'static str> {
        match self {
            Self::Home => None,
            Self::Task => Some("task"),
            Self::Codex => Some("codex"),
            Self::Claude => Some("claude"),
        }
    }
}

fn request(bytes: &[u8]) -> Result<Request, String> {
    match bytes {
        [1] => Ok(Request::Start(Terminal::Home)),
        [4] => Ok(Request::Start(Terminal::Task)),
        [5] => Ok(Request::Start(Terminal::Codex)),
        [6] => Ok(Request::Start(Terminal::Claude)),
        [2, rest @ ..] if rest.len() == 8 => {
            let handle =
                u64::from_be_bytes(rest.try_into().map_err(|_| "invalid terminal handle")?);
            if handle == 0 {
                return Err("zero terminal handle".into());
            }
            Ok(Request::Poll(handle))
        }
        [3] => Ok(Request::Heartbeat),
        [0x10..=0x19, ..] => Ok(Request::Secret(crate::session::Request::decode(bytes)?)),
        _ => Err("invalid terminal authority request".into()),
    }
}

struct Launches {
    children: BTreeMap<u64, Child>,
    next: u64,
    generation: String,
}

impl Launches {
    fn new(generation: String) -> Self {
        Self {
            children: BTreeMap::new(),
            next: 1,
            generation,
        }
    }

    fn answer(&mut self, config: &Config, request: Request) -> Result<Vec<u8>, String> {
        match request {
            Request::Secret(_) => Err("secret request reached terminal dispatcher".into()),
            Request::Heartbeat => Ok(vec![0x83]),
            Request::Start(terminal) => {
                if self.children.len() >= LIMIT {
                    return Ok(vec![0xff, 1]);
                }
                let handle = self.next;
                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or("terminal handle space exhausted")?;
                let child = match spawn(&mut config.terminal(&self.generation, handle, terminal)) {
                    Ok(child) => child,
                    Err(why) => {
                        let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
                        return Ok(vec![0xff, 2]);
                    }
                };
                self.children.insert(handle, child);
                let mut response = vec![0x81];
                response.extend_from_slice(&handle.to_be_bytes());
                Ok(response)
            }
            Request::Poll(handle) => {
                let child = self
                    .children
                    .get_mut(&handle)
                    .ok_or("unknown terminal handle")?;
                let status = child
                    .try_wait()
                    .map_err(|e| format!("wait for terminal: {e}"))?;
                let code = match status {
                    None => 0,
                    Some(status) if status.success() => 1,
                    Some(_) => 2,
                };
                if status.is_some() {
                    self.children.remove(&handle);
                }
                Ok(vec![0x82, code])
            }
        }
    }
}

pub(crate) fn serve(mut channel: Channel, config: Config) -> Result<(), String> {
    // The channel greeting has already pinned the actual compositor sender.
    channel.send(VERSION).map_err(|e| e.to_string())?;
    if channel.receive().map_err(|e| e.to_string())? != VERSION {
        return Err("unsupported terminal authority protocol".into());
    }
    check_session(&config)?;
    let mut launches = Launches::new(generation()?);
    channel.send(&[0x80]).map_err(|e| e.to_string())?;
    let mut secrets = crate::session::Session::new(config.owner)?;
    let result = (|| -> Result<(), String> {
        loop {
            let request = request(&channel.receive().map_err(|e| e.to_string())?)?;
            let answer = match request {
                Request::Secret(request) => secrets.answer(request)?,
                request => {
                    secrets.tick()?;
                    launches.answer(&config, request)?
                }
            };
            channel.send(&answer).map_err(|e| e.to_string())?;
        }
    })();
    let cleanup = secrets.close();
    match (result, cleanup) {
        (Err(reason), Err(cleanup)) => Err(format!("{reason}; {cleanup}")),
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup)) => Err(cleanup),
    }
}

#[cfg(test)]
#[path = "../tests/launch.rs"]
mod tests;

#[cfg(test)]
#[test]
fn enrollment_dispatch_reaches_the_secret_decoder() -> Result<(), String> {
    use crate::consent::Recovery;
    for (tag, recovery) in [(0, Recovery::Unrecoverable), (1, Recovery::SecondToken)] {
        assert_eq!(request(&[0x16, tag])?, Request::Secret(crate::session::Request::Enroll(recovery)));
    }
    assert_eq!(request(&[0x17])?, Request::Secret(crate::session::Request::Inspect));
    assert_eq!(request(&[0x18])?, Request::Secret(crate::session::Request::Write));
    assert_eq!(request(&[0x19])?, Request::Secret(crate::session::Request::Install));
    assert!(request(&[0x1a]).is_err());
    assert!(request(&[0x16, 2]).is_err());
    Ok(())
}
