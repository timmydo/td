//! The workspace jail's launch (DESIGN.md §8): the tool host as td-jail's
//! `workspace` kind (APPLICATIONS.md §C), over one stream socket that is
//! both its frames and its lifetime.
//!
//! td-agent writes the kind's spec, one per instance, and td-jail admits
//! it: canonical directories, no overlap, nothing reserved. What td-agent
//! itself refuses on top of that (its own state, credential locations,
//! `workspace_root`) is the workspace's admission, before a spec exists.
//!
//! On a host the programs are the checkout's: `./agent` builds td-jail and
//! td-txt and names them in `TD_AGENT_JAIL` and `TD_AGENT_TXT`. Without
//! them there is no jail, and tools are refused by name; there is no
//! unconfined fallback.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::host::Client;
use crate::repo::{self, Task};

/// The variables `./agent` names its built td-jail and td-txt in.
pub const JAIL_VAR: &str = "TD_AGENT_JAIL";
pub const TXT_VAR: &str = "TD_AGENT_TXT";
/// Where td-jail binds the spec's programs, each under its file name.
const PROGRAM_DIR: &str = "/opt/workspace/bin";
/// The most of td-jail's own diagnostics kept for a report.
const DIAGNOSTIC_KEEP: usize = 4096;
/// The most a maintenance instance's answer may be.
const MAX_ANSWER: usize = 64 * 1024;

/// The programs a jailed tool host needs, outside the jail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Programs {
    pub jail: PathBuf,
    /// This program, the instance's entry.
    pub agent: PathBuf,
    pub txt: PathBuf,
}

impl Programs {
    /// From `./agent`'s variables, with this program as the entry.
    pub fn from_env() -> Result<Self, String> {
        let named = |var: &str| {
            std::env::var_os(var)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    format!(
                        "no workspace jail: {var} is not set, so tools cannot run confined \
                         and are refused (./agent builds td-jail and td-txt and sets it)"
                    )
                })
        };
        let agent = std::env::current_exe().map_err(|e| format!("this program's path: {e}"))?;
        Self::new(named(JAIL_VAR)?, agent, named(TXT_VAR)?)
    }

    /// Each must be an absolute path to an executable file; td-jail must
    /// be named `td-jail`, since its argv[0] selects its kind, and the
    /// entry and td-txt need distinct file names, since they share one
    /// directory inside.
    pub fn new(jail: PathBuf, agent: PathBuf, txt: PathBuf) -> Result<Self, String> {
        // Named as they resolve, so what is checked, protected from the
        // model's writes and run is one file, never a link to it.
        let canonical = |what: &str, path: PathBuf| {
            if !path.is_absolute() {
                return Err(format!(
                    "the workspace jail's {what} {} is not an absolute path",
                    path.display()
                ));
            }
            fs::canonicalize(&path)
                .map_err(|e| format!("the workspace jail's {what} {}: {e}", path.display()))
        };
        let jail = canonical("td-jail", jail)?;
        let agent = canonical("td-agent", agent)?;
        let txt = canonical("td-txt", txt)?;
        for (what, path) in [("td-jail", &jail), ("td-agent", &agent), ("td-txt", &txt)] {
            let executable = fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
            if !path.is_absolute() || !executable {
                return Err(format!(
                    "the workspace jail's {what} {} is not an absolute path to an \
                     executable file",
                    path.display()
                ));
            }
        }
        if jail.file_name() != Some("td-jail".as_ref()) {
            return Err(format!(
                "the workspace jail {} is not named td-jail",
                jail.display()
            ));
        }
        if agent.file_name() == txt.file_name() {
            return Err("td-agent and td-txt share a file name".into());
        }
        Ok(Self { jail, agent, txt })
    }

    fn inside(path: &Path) -> Result<PathBuf, String> {
        path.file_name()
            .map(|name| Path::new(PROGRAM_DIR).join(name))
            .ok_or_else(|| format!("{} has no file name", path.display()))
    }
}

/// What one instance binds (§8): its private home, the ready worktrees,
/// a repository workspace's checkouts and repositories with their git
/// mount chains, the store's objects they borrow, and the shared
/// directories, read-only or read-write.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Policy {
    pub home: PathBuf,
    pub worktrees: Vec<PathBuf>,
    /// Linked worktrees of `repositories`, each with its `.git` bound
    /// read-only over it.
    pub checkouts: Vec<PathBuf>,
    pub repositories: Vec<PathBuf>,
    /// The store `objects/` directories the repositories' alternates
    /// name, read-only and no root of the tool host's.
    pub objects: Vec<PathBuf>,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

impl Policy {
    /// The directories the tool host may act in: the worktrees, the
    /// checkouts and the shared directories, never the home, a
    /// repository or the store.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.worktrees
            .iter()
            .chain(&self.checkouts)
            .chain(&self.read)
            .chain(&self.write)
            .cloned()
            .collect()
    }
}

/// The spec's text: td-jail's exact, ordered keyfile.
pub fn spec_text(programs: &Programs, policy: &Policy) -> Result<String, String> {
    let mut text = String::from("format=1\n");
    let mut line = |key: &str, path: &Path| -> Result<(), String> {
        let value = path
            .to_str()
            .filter(|value| path.is_absolute() && !value.contains(['\n', '\r', '\0']))
            .ok_or_else(|| format!("{} cannot be named to the jail", path.display()))?;
        text.push_str(key);
        text.push('=');
        text.push_str(value);
        text.push('\n');
        Ok(())
    };
    line("entry", &programs.agent)?;
    line("program", &programs.txt)?;
    line("home", &policy.home)?;
    for tree in &policy.worktrees {
        line("worktree", tree)?;
    }
    for tree in &policy.checkouts {
        line("checkout", tree)?;
    }
    for tree in &policy.repositories {
        line("repository", tree)?;
    }
    for tree in policy.objects.iter().chain(&policy.read) {
        line("read", tree)?;
    }
    for tree in &policy.write {
        line("write", tree)?;
    }
    Ok(text)
}

/// `dir`, created if need be, made the caller's alone, and named as it
/// resolves, since td-jail takes only canonical paths.
fn private_dir(dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Writes `text` as a fresh spec in `dir`, which only the caller can
/// write, and returns its path.
fn write_spec(dir: &Path, text: &str) -> Result<PathBuf, String> {
    let dir = private_dir(dir)?;
    let name = crate::store::random_hex(8).map_err(|e| format!("a spec name: {e}"))?;
    let path = dir.join(format!("spec-{name}"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Starts a tool host in a fresh instance of `policy`, its spec written
/// in `spec_dir`. The instance ends when the client is dropped, and with
/// this process: td-jail ties itself to the thread that starts it, so a
/// conversation launches from its main thread (§8).
pub fn launch(programs: &Programs, policy: &Policy, spec_dir: &Path) -> Result<Client, String> {
    let policy = Policy {
        home: private_dir(&policy.home)?,
        ..policy.clone()
    };
    let spec = write_spec(spec_dir, &spec_text(programs, &policy)?)?;
    let launched = start(programs, &policy, &spec);
    if launched.is_err() {
        let _ = fs::remove_file(&spec);
    }
    launched
}

fn start(programs: &Programs, policy: &Policy, spec: &Path) -> Result<Client, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| format!("the jail's channel: {e}"))?;
    let channel = |e: io::Error| format!("the jail's channel: {e}");
    let theirs_in = theirs.try_clone().map_err(channel)?;
    let reader = ours.try_clone().map_err(channel)?;
    let mut command = Command::new(&programs.jail);
    // td-jail gives the instance its own fixed environment; nothing of
    // this process's reaches even its outer stages.
    command
        .env_clear()
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(spec)
        .arg("tool-host")
        .arg("--txt")
        .arg(Programs::inside(&programs.txt)?);
    for root in policy.roots() {
        command.arg("--root").arg(root);
    }
    let mut child = command
        .stdin(Stdio::from(OwnedFd::from(theirs_in)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", programs.jail.display()))?;
    drop(command);
    let tail = Arc::new(Tail::default());
    let reading = child.stderr.take().and_then(|mut stderr| {
        let kept = Arc::clone(&tail);
        std::thread::Builder::new()
            .name("td-jail-stderr".into())
            .spawn(move || kept.keep(&mut stderr))
            .ok()
    });
    if reading.is_none() {
        tail.end();
    }
    Ok(Client::over(reader, ours).owning(child, spec.to_path_buf(), tail))
}

/// Runs `task` in a maintenance instance of `policy` (DESIGN.md §9), its
/// spec written in `spec_dir`, and returns its answer. It blocks its
/// thread, to which td-jail ties itself, for at most `time`; past it the
/// instance is killed, which takes every process in it.
pub fn maintain(
    programs: &Programs,
    policy: &Policy,
    spec_dir: &Path,
    task: &Task,
    time: Duration,
) -> Result<String, String> {
    let policy = Policy {
        home: private_dir(&policy.home)?,
        ..policy.clone()
    };
    let spec = write_spec(spec_dir, &spec_text(programs, &policy)?)?;
    let answered = run_maintenance(programs, &spec, task, time);
    let _ = fs::remove_file(&spec);
    answered
}

fn run_maintenance(
    programs: &Programs,
    spec: &Path,
    task: &Task,
    time: Duration,
) -> Result<String, String> {
    let now = Instant::now();
    let deadline = now.checked_add(time).unwrap_or(now);
    let (mut ours, theirs) = UnixStream::pair().map_err(|e| format!("the jail's channel: {e}"))?;
    let theirs_in = theirs
        .try_clone()
        .map_err(|e| format!("the jail's channel: {e}"))?;
    let mut child = Command::new(&programs.jail)
        .env_clear()
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(spec)
        .arg(repo::MAINTAIN)
        .args(task.args())
        .stdin(Stdio::from(OwnedFd::from(theirs_in)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", programs.jail.display()))?;
    let tail = Arc::new(Tail::default());
    let reading = child.stderr.take().and_then(|mut stderr| {
        let kept = Arc::clone(&tail);
        std::thread::Builder::new()
            .name("td-jail-stderr".into())
            .spawn(move || kept.keep(&mut stderr))
            .ok()
    });
    if reading.is_none() {
        tail.end();
    }
    let said = |what: String| match tail.text(Duration::from_secs(2)) {
        Some(text) if !text.trim().is_empty() => format!("{what}; td-jail said: {}", text.trim()),
        _ => what,
    };
    // The answer is what the channel gives until every end of it closes,
    // as the instance's last process exits.
    let mut answer = Vec::new();
    let mut buffer = [0u8; 4096];
    let read = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err(format!("maintenance took more than {time:?}"));
        }
        if let Err(e) = ours.set_read_timeout(Some(left)) {
            break Err(format!("the jail's channel: {e}"));
        }
        match ours.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                answer.extend_from_slice(buffer.get(..n).unwrap_or_default());
                if answer.len() > MAX_ANSWER {
                    break Err("the maintenance instance said too much".into());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => break Err(format!("the jail's channel: {e}")),
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if read.is_ok() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    read.map_err(said)?;
    let answered = repo::read_answer(&answer);
    match status {
        Some(status) if status.success() => answered,
        Some(status) => Err(said(match answered {
            Err(why) => why,
            Ok(_) => format!("td-jail ended with {status}"),
        })),
        None => Err(said(format!("maintenance took more than {time:?}"))),
    }
}

/// td-jail's standard error as it is read: its last `DIAGNOSTIC_KEEP`
/// bytes, and whether it has ended.
#[derive(Default)]
pub struct Tail {
    state: Mutex<(Vec<u8>, bool)>,
    ended: Condvar,
}

impl Tail {
    /// Reads `input` to its end, keeping its tail.
    fn keep(&self, input: &mut impl Read) {
        let mut buffer = [0u8; 1024];
        loop {
            let read = match input.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let Ok(mut state) = self.state.lock() else {
                break;
            };
            let bytes = &mut state.0;
            bytes.extend_from_slice(buffer.get(..read).unwrap_or_default());
            let over = bytes.len().saturating_sub(DIAGNOSTIC_KEEP);
            bytes.drain(..over);
        }
        self.end();
    }

    fn end(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.1 = true;
        }
        self.ended.notify_all();
    }

    /// The tail as text once it has ended, or as it stands after `wait`;
    /// none when there is none. Decoded whole, so a character split
    /// between reads survives; one the bound cut into is dropped.
    pub fn text(&self, wait: Duration) -> Option<String> {
        let state = self.state.lock().ok()?;
        let (state, _) = self
            .ended
            .wait_timeout_while(state, wait, |state| !state.1)
            .ok()?;
        let bytes = &state.0;
        let start = bytes
            .iter()
            .position(|byte| byte & 0xC0 != 0x80)
            .unwrap_or(bytes.len());
        let text = String::from_utf8_lossy(bytes.get(start..).unwrap_or_default());
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn programs() -> Programs {
        Programs {
            jail: "/w/target/td-jail".into(),
            agent: "/w/target/td-agent".into(),
            txt: "/w/target/td-txt".into(),
        }
    }

    #[test]
    fn the_spec_is_td_jails_ordered_keyfile() {
        let policy = Policy {
            home: "/s/jail/a/home".into(),
            worktrees: vec!["/w/a".into(), "/w/b".into()],
            checkouts: vec!["/t/r".into()],
            repositories: vec!["/ws/r.git".into()],
            objects: vec!["/store/r.git/objects".into()],
            read: vec!["/d".into()],
            write: vec!["/e".into()],
        };
        assert_eq!(
            spec_text(&programs(), &policy).unwrap(),
            "format=1\nentry=/w/target/td-agent\nprogram=/w/target/td-txt\n\
             home=/s/jail/a/home\nworktree=/w/a\nworktree=/w/b\ncheckout=/t/r\n\
             repository=/ws/r.git\nread=/store/r.git/objects\nread=/d\nwrite=/e\n"
        );
        // Neither a repository nor the store is the tool host's to act in.
        assert_eq!(
            policy.roots(),
            [
                PathBuf::from("/w/a"),
                "/w/b".into(),
                "/t/r".into(),
                "/d".into(),
                "/e".into()
            ]
        );
        for bad in ["relative", "/a\nworktree=/", "/a\rb"] {
            let policy = Policy {
                home: "/h".into(),
                worktrees: vec![bad.into()],
                ..Policy::default()
            };
            assert!(spec_text(&programs(), &policy).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_programs_are_named_inside_by_file_name() {
        assert_eq!(
            Programs::inside(Path::new("/w/target/td-txt")).unwrap(),
            Path::new("/opt/workspace/bin/td-txt")
        );
        let file = std::env::current_exe().unwrap();
        assert!(Programs::new(file.clone(), file.clone(), file.clone()).is_err());
        assert!(Programs::new("relative".into(), file.clone(), "/x".into()).is_err());
        let dir = std::env::temp_dir().join(format!(
            "td-agent-jail-programs-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        let jail = dir.join("td-jail");
        let txt = dir.join("td-txt");
        fs::copy(&file, &jail).unwrap();
        fs::copy(&file, &txt).unwrap();
        assert!(Programs::new(jail.clone(), file.clone(), txt.clone()).is_ok());
        // Named otherwise, td-jail would take its argv[0] as an application.
        let renamed = dir.join("td-jail2");
        fs::copy(&file, &renamed).unwrap();
        let refused = Programs::new(renamed.clone(), file.clone(), txt.clone()).unwrap_err();
        assert!(refused.contains("not named td-jail"), "{refused}");
        // A link named td-jail is taken as what it names.
        let link = dir.join("link/td-jail");
        fs::create_dir_all(dir.join("link")).unwrap();
        std::os::unix::fs::symlink(&renamed, &link).unwrap();
        assert!(Programs::new(link, file.clone(), txt.clone()).is_err());
        let through = dir.join("through");
        std::os::unix::fs::symlink(&dir, &through).unwrap();
        assert_eq!(
            Programs::new(through.join("td-jail"), file.clone(), txt.clone())
                .unwrap()
                .jail,
            jail
        );
        fs::set_permissions(&txt, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Programs::new(jail, file, txt).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_spec_is_private_and_fresh() {
        let dir = std::env::temp_dir().join(format!(
            "td-agent-jail-spec-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        let one = write_spec(&dir, "format=1\n").unwrap();
        let two = write_spec(&dir, "format=1\n").unwrap();
        assert_ne!(one, two);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&one).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::read_to_string(&one).unwrap(), "format=1\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_diagnostic_keeps_its_tail() {
        // A character across a read boundary, and one the bound cuts.
        let text = format!(
            "{}é{}é{}",
            "a".repeat(1023),
            "b".repeat(2000),
            "c".repeat(DIAGNOSTIC_KEEP - 1)
        );
        let tail = Tail::default();
        tail.keep(&mut text.as_bytes());
        let kept = tail.text(Duration::ZERO).unwrap();
        assert!(kept.len() < DIAGNOSTIC_KEEP);
        assert!(!kept.contains('\u{FFFD}'), "{kept}");
        assert_eq!(kept, "c".repeat(DIAGNOSTIC_KEEP - 1));
        let whole = Tail::default();
        whole.keep(&mut format!("{}é{}", "a".repeat(1023), "b".repeat(10)).as_bytes());
        assert!(whole.text(Duration::ZERO).unwrap().contains("aéb"));
        // An unended tail is read as it stands once the wait is over.
        let open = Tail::default();
        assert_eq!(open.text(Duration::from_millis(10)), None);
    }
}
