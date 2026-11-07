//! Who may log in. Accounts parse fail-closed through td-login's own `db.rs`, so
//! both programs read the same tables by the same rules (DESIGN.md section 5).

use super::authkeys::{self, Grant, KeyFileError};
use super::config::{valid_account_name, Effective, ServerConfig};
use super::credentials::{self, Credentials};
use super::ipc::{self, UntilDeadline};
use super::log;
use super::login_db as db;
use crate::sys;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const READ_USER_FILE_FLAG: &str = "--read-user-file";
pub const READ_AS_FLAG: &str = "--read-as";
/// A stuck filesystem or a stopped reader costs the monitor at most this.
const USER_FILE_DEADLINE: Duration = Duration::from_secs(5);
/// Tells the monitor the file is simply not there, which is ordinary.
const READER_MISSING: i32 = 3;
const REAP_POLL: Duration = Duration::from_millis(2);
const REAP_AFTER_KILL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: String,
    pub shell: String,
}

/// `--single-user NAME`: the calling uid under one name. Only a non-root process
/// reaches this; `main` refuses the flag as root (DESIGN.md section 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingleUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

/// The account fences, apart from what reaches them so that each can be tested.
/// No path to a program or a read as root goes around this.
pub fn uid_may_log_in(uid: u32) -> bool {
    uid != 0
}

fn secret_admits(secret: &db::Secret) -> bool {
    match secret {
        db::Secret::NoPassword | db::Secret::Hashed => true,
        db::Secret::Locked | db::Secret::Service => false,
    }
}

/// `--single-user` is for an unprivileged process only. As root it refuses:
/// otherwise a stray flag would turn every jail off.
pub fn check_single_user(uid: u32) -> Result<(), String> {
    if uid == 0 {
        return Err("--single-user is refused as root".to_string());
    }
    Ok(())
}

/// Admission's account half. Every refusal is reported the same way to the peer;
/// the reason goes to the log only.
pub fn resolve(
    name: &str,
    config: &ServerConfig,
    single_user: Option<&SingleUser>,
) -> Result<Account, String> {
    if !valid_account_name(name) {
        return Err("bad account name".to_string());
    }
    if !config.admits_name(name) {
        return Err("not listed in AllowUsers".to_string());
    }
    if let Some(single) = single_user {
        if single.name != name {
            return Err("only the single-user account is admitted".to_string());
        }
        return Ok(Account {
            name: single.name.clone(),
            uid: single.uid,
            gid: single.gid,
            groups: Vec::new(),
            home: single.home.clone(),
            shell: single.shell.clone(),
        });
    }
    let account = db::account(name)?;
    if !uid_may_log_in(account.uid) {
        return Err("root is never admitted".to_string());
    }
    if !secret_admits(&db::secret(name)?) {
        return Err("account is locked".to_string());
    }
    let shell_runs = std::fs::metadata(&account.shell)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if !shell_runs {
        return Err(format!("shell {} is not executable", account.shell));
    }
    Ok(Account {
        groups: db::supplementary(name)?,
        name: account.name,
        uid: account.uid,
        gid: account.gid,
        home: account.home,
        shell: account.shell,
    })
}

/// The account's key policy: every file its configuration names, in order. A
/// file that fails its checks counts as empty and says why in the log. `deadline`
/// is the connection's own, which no file may outlast.
pub fn find_key(
    account: &Account,
    effective: &Effective,
    key: &[u8; 32],
    peer: &str,
    single_user: bool,
    deadline: Instant,
) -> Option<Grant> {
    for file in &effective.authorized_keys {
        let bytes = if file.starts_with('/') {
            // Root-owned all the way up: the monitor is the one reading it.
            let owner = if single_user { account.uid } else { 0 };
            authkeys::read_checked(Path::new(file), owner)
        } else {
            read_as_account(account, file, single_user, deadline)
        };
        match bytes {
            Ok(bytes) => {
                if let Some(grant) = authkeys::find(&bytes, key, peer) {
                    return Some(grant);
                }
            }
            Err(KeyFileError::Missing) => {}
            Err(KeyFileError::Refused(reason)) => log::line(format_args!(
                "td-sshd: authorized keys for {}: {reason}",
                account.name
            )),
        }
    }
    None
}

/// The reader's `--read-as` value: `UID:GID:GROUP,GROUP`.
fn identity_argument(account: &Account) -> String {
    let groups: Vec<String> = account.groups.iter().map(u32::to_string).collect();
    format!("{}:{}:{}", account.uid, account.gid, groups.join(","))
}

fn parse_identity(text: &str) -> Option<Credentials> {
    let mut fields = text.split(':');
    let uid = fields.next()?.parse().ok()?;
    // Root reading an account's path is what the reader stage exists to prevent.
    if !uid_may_log_in(uid) {
        return None;
    }
    let gid = fields.next()?.parse().ok()?;
    let groups = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let groups: Vec<u32> = if groups.is_empty() {
        Vec::new()
    } else {
        groups
            .split(',')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?
    };
    Some(Credentials::new(uid, gid, &groups))
}

/// Reads a path relative to the account's home in a child that becomes that
/// account before it opens anything, so root never opens a path the account
/// controls. In single-user mode there is no identity to take.
fn read_as_account(
    account: &Account,
    relative: &str,
    single_user: bool,
    deadline: Instant,
) -> Result<Vec<u8>, KeyFileError> {
    let path = Path::new(&account.home).join(relative);
    let mut command = Command::new("/proc/self/exe");
    command.arg(READ_USER_FILE_FLAG).arg(&path);
    if !single_user {
        command.arg(READ_AS_FLAG).arg(identity_argument(account));
    }
    let deadline = deadline.min(Instant::now() + USER_FILE_DEADLINE);
    collect_reader(command, deadline).map_err(|error| match error {
        ReaderFailure::Missing => KeyFileError::Missing,
        ReaderFailure::Refused => KeyFileError::Refused(format!(
            "{}: bad ownership or modes, or not readable as the account",
            path.display()
        )),
        ReaderFailure::Failed(reason) => {
            KeyFileError::Refused(format!("{}: {reason}", path.display()))
        }
    })
}

enum ReaderFailure {
    Missing,
    Refused,
    Failed(String),
}

/// Runs a reader, and takes its output under `deadline`. The account can stop this
/// child, so nothing in the monitor may wait on it without one. Its stderr is null
/// because it runs as the account: what it says cannot reach root's log.
fn collect_reader(mut command: Command, deadline: Instant) -> Result<Vec<u8>, ReaderFailure> {
    let spawned = |error: std::io::Error| ReaderFailure::Failed(format!("key reader: {error}"));
    let (ours, theirs) = UnixStream::pair().map_err(spawned)?;
    let mut child = command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawned)?;
    // Command keeps the Stdio it was given: its copy would hold the EOF back.
    drop(command);
    let mut bytes = Vec::new();
    let read = UntilDeadline::new(&ours, deadline)
        .take(authkeys::MAX_KEY_FILE + 1)
        .read_to_end(&mut bytes);
    if let Err(error) = read {
        end_child(&mut child);
        return Err(ReaderFailure::Failed(if ipc::is_timeout(&error) {
            "key reader timed out".to_string()
        } else {
            format!("key reader: {error}")
        }));
    }
    if bytes.len() as u64 > authkeys::MAX_KEY_FILE {
        end_child(&mut child);
        return Err(ReaderFailure::Failed("key file is too long".to_string()));
    }
    let Some(status) = wait_until(&mut child, deadline) else {
        end_child(&mut child);
        return Err(ReaderFailure::Failed("key reader timed out".to_string()));
    };
    if status.success() {
        Ok(bytes)
    } else if status.code() == Some(READER_MISSING) {
        Err(ReaderFailure::Missing)
    } else {
        Err(ReaderFailure::Refused)
    }
}

fn wait_until(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(REAP_POLL);
    }
}

/// A child in uninterruptible sleep on a hung filesystem would not be reaped by a
/// wait, so this does not wait long. The zombie is gone when the monitor is.
fn end_child(child: &mut Child) {
    let _ = child.kill();
    let _ = wait_until(child, Instant::now() + REAP_AFTER_KILL);
}

/// The reader stage: spawned as root, it becomes the account through the one
/// credential switch and its readback, then reads. Non-dumpable, so the account
/// cannot reach into it. It reports through its exit status alone. Without
/// `read_as` this is single-user mode, which has no identity to change.
pub fn read_user_file_main(path: &str, read_as: Option<&str>) -> i32 {
    let owner = match read_as {
        Some(identity) => {
            let Some(want) = parse_identity(identity) else {
                return 1;
            };
            let Ok(mut status) = credentials::open_status() else {
                return 1;
            };
            if credentials::switch(&want, &mut status).is_err() {
                return 1;
            }
            want.uid
        }
        // Root reading an account's path is exactly what this stage prevents.
        None => match own_uid() {
            Some(uid) if uid != 0 => uid,
            _ => return 1,
        },
    };
    if sys::set_not_dumpable().is_err() {
        return 1;
    }
    match authkeys::read_checked(Path::new(path), owner) {
        Ok(bytes) => {
            let mut stdout = std::io::stdout();
            if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                return 1;
            }
            0
        }
        Err(KeyFileError::Missing) => READER_MISSING,
        Err(KeyFileError::Refused(_)) => 1,
    }
}

fn own_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn config(extra: &str) -> ServerConfig {
        let text =
            format!("Port 22\nListenAddress 127.0.0.1\nHostKey /k\nAuthorizedKeysFile /a\n{extra}");
        ServerConfig::parse(&text, false).unwrap()
    }

    fn single() -> SingleUser {
        SingleUser {
            name: "tester".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/tester".into(),
            shell: "/bin/sh".into(),
        }
    }

    #[test]
    fn root_is_fenced_at_every_point_it_could_come_through() {
        assert!(!uid_may_log_in(0));
        assert!(uid_may_log_in(1));
        check_single_user(1000).unwrap();
        assert!(check_single_user(0)
            .unwrap_err()
            .contains("refused as root"));
    }

    #[test]
    fn only_a_password_state_that_can_log_in_is_admitted() {
        assert!(secret_admits(&db::Secret::NoPassword));
        assert!(secret_admits(&db::Secret::Hashed));
        assert!(!secret_admits(&db::Secret::Locked));
        assert!(!secret_admits(&db::Secret::Service));
    }

    #[test]
    fn single_user_mode_admits_only_its_name() {
        let policy = config("");
        assert!(resolve("tester", &policy, Some(&single())).is_ok());
        assert!(resolve("other", &policy, Some(&single())).is_err());
    }

    #[test]
    fn allow_users_and_name_shape_come_before_any_table() {
        let policy = config("AllowUsers alice\n");
        assert!(resolve("tester", &policy, Some(&single())).is_err());
        assert!(resolve("../etc", &config(""), Some(&single())).is_err());
        assert!(resolve("", &config(""), Some(&single())).is_err());
    }

    #[test]
    fn the_readers_identity_round_trips_and_a_bad_one_is_refused() {
        let account = Account {
            name: "alice".into(),
            uid: 1000,
            gid: 100,
            groups: vec![5, 7],
            home: "/home/alice".into(),
            shell: "/bin/sh".into(),
        };
        assert_eq!(
            parse_identity(&identity_argument(&account)),
            Some(Credentials::new(1000, 100, &[5, 7]))
        );
        assert_eq!(
            parse_identity("1000:100:"),
            Some(Credentials::new(1000, 100, &[]))
        );
        for bad in [
            "", "1000", "1000:100", "x:100:", "1000:y:", "1:2:3:4", "1:2:z", "0:0:", "0:100:5",
        ] {
            assert_eq!(parse_identity(bad), None, "{bad}");
        }
    }

    /// What an account could do to its own reader: stop it holding the stream.
    #[test]
    fn a_stopped_reader_costs_the_monitor_only_its_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "kill -STOP $$"]);
        let start = Instant::now();
        let result = collect_reader(command, start + Duration::from_millis(300));
        assert!(
            matches!(&result, Err(ReaderFailure::Failed(reason)) if reason.contains("timed out")),
            "took {:?}",
            start.elapsed()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    /// Stopped after it has already closed the output would still hold the wait.
    #[test]
    fn a_reader_that_will_not_exit_is_bounded_too() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec 1>&-; kill -STOP $$"]);
        let start = Instant::now();
        let result = collect_reader(command, start + Duration::from_millis(300));
        assert!(matches!(&result, Err(ReaderFailure::Failed(_))));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_readers_exit_status_is_its_verdict() {
        let run = |script: &str| {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            collect_reader(command, Instant::now() + Duration::from_secs(10))
        };
        assert_eq!(run("echo keys").ok().as_deref(), Some(&b"keys\n"[..]));
        assert!(matches!(
            run(&format!("exit {READER_MISSING}")),
            Err(ReaderFailure::Missing)
        ));
        assert!(matches!(run("exit 1"), Err(ReaderFailure::Refused)));
    }
}
