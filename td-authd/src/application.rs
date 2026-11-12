//! Fixed application execution after root-owned deployment admission.

use crate::launch;
use crate::{message, text};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long one wait for the check looks before draining its stderr again.
const READ_STEP: Duration = Duration::from_millis(10);
// An application account is the workload account ledger's.
use crate::workload_accounts::{Kind, APPLICATION_UIDS, WORKLOAD_UIDS};

/// One fixed td-firstboot run root asks before it acts for a workload: its
/// verb, the fixed words between the owner and the name, the prefix of its
/// one reply line, its bound, and the uids that line may name.
pub(crate) struct Check {
    /// What the check admits, as its errors name it.
    what: &'static str,
    verb: &'static str,
    words: &'static [&'static str],
    reply: &'static str,
    deadline: Duration,
    uid: fn(&str) -> Result<u32, String>,
}

/// The read-only launch admission: the account an active application runs
/// as, the human's for a personal one.
const LAUNCH: Check = Check {
    what: "application",
    verb: "check-launch-application",
    words: &[],
    reply: "TD-LAUNCH-APPLICATION-CHECK-OK\t",
    deadline: Duration::from_secs(2),
    uid: launch_uid,
};

/// An install's allocation (td-authd/DESIGN.md, "Application cache
/// operation"): a declared Flathub application's ledger account, added
/// when absent, its home, cgroup and runtime directory prepared. Its bound
/// covers synced writes and cgroup setup under the principal lock, which
/// another allocation may hold.
const ALLOCATE: Check = Check {
    what: "application",
    verb: "allocate-application",
    words: &[],
    reply: "TD-APPLICATION-ALLOCATED\t",
    deadline: Duration::from_secs(30),
    uid: service_uid,
};

/// A sandbox's account, allocated when td-sandbox creates the sandbox
/// through the workspace intake (td-authd/DESIGN.md, "Workload account
/// ledger"): its ledger row added from 98304 up when absent, or the
/// existing one returned, with its empty home and its delegation. The
/// intake's one path to a new row.
pub(crate) const ALLOCATE_SANDBOX: Check = Check {
    what: "sandbox",
    verb: "allocate-workload",
    words: &["sandbox"],
    reply: "TD-WORKLOAD-ALLOCATED\t",
    deadline: Duration::from_secs(30),
    uid: workload_uid,
};

/// A sandbox's account prepared for a launch (td-authd/DESIGN.md,
/// "Workspace launch intake"): its existing ledger row returned, never one
/// added, its home's mode restored and its cgroup delegation, which the
/// volatile hierarchy loses at shutdown, created again when absent, under
/// the principal lock. The same bound as an application's allocation, for
/// the same synced writes and lock.
pub(crate) const PREPARE_SANDBOX: Check = Check {
    what: "sandbox",
    verb: "prepare-workload",
    words: &["sandbox"],
    reply: "TD-WORKLOAD-PREPARED\t",
    deadline: Duration::from_secs(30),
    uid: workload_uid,
};

/// A td-agent workspace slot's account, allocated when td-agent creates
/// the slot through the workspace intake, as a sandbox's is: its ledger row
/// added from 98304 up when absent, or the existing one returned, with its
/// empty home, its scratch tree and its delegation.
pub(crate) const ALLOCATE_WORKSPACE: Check = Check {
    what: "workspace",
    verb: "allocate-workload",
    words: &["workspace"],
    reply: "TD-WORKLOAD-ALLOCATED\t",
    deadline: Duration::from_secs(30),
    uid: workload_uid,
};

/// A workspace slot's account prepared for its first instance of a boot
/// or a deletion, as a sandbox's is for each launch.
pub(crate) const PREPARE_WORKSPACE: Check = Check {
    what: "workspace",
    verb: "prepare-workload",
    words: &["workspace"],
    reply: "TD-WORKLOAD-PREPARED\t",
    deadline: Duration::from_secs(30),
    uid: workload_uid,
};

struct Request {
    owner: u32,
    name: String,
    arguments: Vec<String>,
}

fn decimal(text: &str, range: std::ops::RangeInclusive<u32>, what: &str) -> Result<u32, String> {
    let value = text
        .parse::<u32>()
        .map_err(|_| message(format_args!("invalid {what} identity")))?;
    if !range.contains(&value) {
        return Err(message(format_args!(
            "{what} identity is outside the supported range"
        )));
    }
    if value.to_string() != text {
        return Err(message(format_args!("noncanonical {what} identity")));
    }
    Ok(value)
}

impl Request {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let [owner, name, mode, separator, rest @ ..] = arguments else {
            return Err("application launch requires OWNER APP direct -- ARG...".into());
        };
        let owner = decimal(owner, 1000..=1000, "application")?;
        if mode != "direct"
            || separator != "--"
            || !crate::consent::application_name(name)
            || rest.len() > 128
            || rest.iter().any(|arg| arg.contains('\0'))
            || rest
                .iter()
                .try_fold(0usize, |n, arg| {
                    n.checked_add(arg.len()).and_then(|n| n.checked_add(1))
                })
                .is_none_or(|n| n > 32768)
        {
            return Err("invalid application launch arguments".into());
        }
        Ok(Self {
            owner,
            name: name.clone(),
            arguments: rest.to_vec(),
        })
    }

    fn helper(&self, uid: u32) -> Command {
        let mut command = Command::new("/bin/td-login");
        if uid == self.owner {
            command.args(["exec-primary", "--"]);
        } else {
            command.args(["exec-service-as", &format!("tda{uid}"), "--"]);
        }
        command
            .args([
                "/bin/td-authd",
                "application-exec",
                &uid.to_string(),
                &self.owner.to_string(),
                &self.name,
                "direct",
                "--",
            ])
            .args(&self.arguments);
        command
    }

    /// td-jail selects the application by its argv[0], so the launch needs
    /// no `/bin/APP` link.
    fn application(&self, uid: u32) -> Result<Command, String> {
        let (home, user) = if uid == self.owner {
            let account = crate::primary_account::load().map_err(|error| error.to_string())?;
            (account.home(), account.name().to_owned())
        } else {
            (
                format!("/var/lib/td/applications/{uid}").into(),
                format!("tda{uid}"),
            )
        };
        let mut command = Command::new("/bin/td-jail");
        command
            .arg0(&self.name)
            .args(&self.arguments)
            .env_clear()
            .current_dir("/")
            .env("HOME", home)
            .env("USER", &user)
            .env("LOGNAME", &user)
            .env("SHELL", "/bin/false")
            .env("PATH", "/bin")
            .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
        Ok(command)
    }
}

struct CheckedChild(Child);
impl Drop for CheckedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn launch_uid(text: &str) -> Result<u32, String> {
    if text == "1000" {
        Ok(1000)
    } else {
        decimal(text, APPLICATION_UIDS, "application")
    }
}

/// An application's own account, never the human's.
fn service_uid(text: &str) -> Result<u32, String> {
    decimal(text, APPLICATION_UIDS, "application")
}

/// A sandbox's or workspace's account, never the human's or an
/// application's.
pub(crate) fn workload_uid(text: &str) -> Result<u32, String> {
    decimal(text, WORKLOAD_UIDS, "workload")
}

fn reply_uid(check: &Check, text: &str) -> Result<u32, String> {
    let number = text
        .strip_prefix(check.reply)
        .and_then(|s| s.strip_suffix('\n'))
        .ok_or_else(|| message(format_args!("invalid {} admission response", check.what)))?;
    (check.uid)(number)
}

pub(crate) fn admit(check: &Check, owner: u32, name: &str) -> Result<u32, String> {
    let deadline = Instant::now()
        .checked_add(check.deadline)
        .ok_or_else(|| message(format_args!("{} check deadline overflow", check.what)))?;
    let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
    let (mut said, said_end) = crate::stderr_tail::Tail::pair()?;
    // The temporary Command drops its retained child endpoints here. Keeping
    // that Command alive would prevent the reply's required EOF.
    let mut checking = CheckedChild(
        Command::new("/bin/td-firstboot")
            .arg(check.verb)
            .arg(owner.to_string())
            .args(check.words)
            .arg(name)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(child)))
            .stderr(Stdio::from(said_end))
            .spawn()
            .map_err(|e| format!("start td-firstboot {}: {e}", check.verb))?,
    );
    let checked = (|| {
        let reply = read_reply(parent, deadline, &mut said, check.what)?;
        loop {
            said.drain();
            if Instant::now() >= deadline {
                return Err(format!("td-firstboot {} timed out", check.verb));
            }
            match checking.0.try_wait().map_err(|e| e.to_string())? {
                Some(status) if status.success() => {
                    let text = std::str::from_utf8(&reply).map_err(|_| {
                        message(format_args!("invalid {} admission encoding", check.what))
                    })?;
                    return reply_uid(check, text);
                }
                Some(status) => {
                    return Err(format!("td-firstboot {} failed: {status}", check.verb))
                }
                None => std::thread::sleep(READ_STEP),
            }
        }
    })();
    checked.map_err(|why: String| format!("{why}{}", said.said()))
}

/// Reads the check's reply, draining `said` between reads so a check that
/// writes its stderr full never stalls the reply.
fn read_reply(
    mut parent: UnixStream,
    deadline: Instant,
    said: &mut crate::stderr_tail::Tail,
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut reply = Vec::with_capacity(97);
    let mut byte = [0u8; 1];
    loop {
        said.drain();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(message(format_args!("{what} admission reply timed out")));
        }
        if reply.len() > 96 {
            return Err(message(format_args!(
                "{what} admission response exceeded 96 bytes"
            )));
        }
        parent
            .set_read_timeout(Some(remaining.min(READ_STEP)))
            .map_err(|e| e.to_string())?;
        match parent.read(&mut byte) {
            Ok(0) => return Ok(reply),
            Ok(_) => reply.extend_from_slice(&byte),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(message(format_args!("{what} admission reply: {e}"))),
        }
    }
}

pub(crate) fn admitted_uid(name: &str) -> Result<u32, String> {
    if !crate::consent::application_name(name) {
        return Err("invalid application name".into());
    }
    admit(&LAUNCH, 1000, name)
}

/// The account td-firstboot allocates, or finds, for the declared Flathub
/// application `name` that an install is about to fetch.
pub(crate) fn allocated_uid(name: &str) -> Result<u32, String> {
    if !crate::consent::application_name(name) {
        return Err("invalid application name".into());
    }
    admit(&ALLOCATE, 1000, name)
}

pub(crate) fn start(arguments: &[String]) -> Result<(), String> {
    let request = Request::parse(arguments)?;
    launch::require_launch_startup().map_err(|e| format!("application startup: {e}"))?;
    let uid = admit(&LAUNCH, request.owner, &request.name)?;
    // An application's own account gets the views its grants name, before
    // its first process; one running as the human needs none.
    if uid != request.owner {
        crate::grant_views::publish(Kind::Application, &request.name, uid)?;
    }
    // No root log or inherited descriptor reaches the credential helper or
    // application.
    let error = request
        .helper(uid)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .exec();
    Err(format!("exec application credential helper: {error}"))
}

pub(crate) fn bounded(path: &str) -> Result<String, String> {
    let mut text = String::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > 8192 {
        return Err("application process record exceeds its bound".into());
    }
    Ok(text)
}

/// The process about to exec a workload's jail: `uid` in every column with
/// no other group, no capability, and in its account's session leaf, the
/// human's `td-user-1000` or the ledger account's own delegation, which
/// td-login placed it in.
pub(crate) fn require_process(uid: u32, status: &str, cgroup: &str) -> Result<(), String> {
    let what = if WORKLOAD_UIDS.contains(&uid) {
        "workload"
    } else {
        "application"
    };
    let expected = uid.to_string();
    for key in ["Uid:", "Gid:"] {
        let values = status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .ok_or_else(|| message(format_args!("{what} process lacks identity fields")))?;
        if values.split_ascii_whitespace().collect::<Vec<_>>() != [expected.as_str(); 4] {
            return Err(message(format_args!(
                "{what} process has the wrong credentials"
            )));
        }
    }
    let groups = status
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .ok_or_else(|| message(format_args!("{what} process lacks its group set")))?;
    if uid != 1000 && groups.split_ascii_whitespace().collect::<Vec<_>>() != [expected.as_str()] {
        return Err(message(format_args!(
            "{what} process has extra or missing groups"
        )));
    }
    for key in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
        if status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
            != Some("0000000000000000")
        {
            return Err(message(format_args!(
                "{what} process retained capabilities"
            )));
        }
    }
    // Exactly `0::/DELEGATION/session` and a newline.
    let leaf = cgroup
        .strip_prefix("0::/")
        .and_then(|rest| rest.strip_suffix("/session\n"));
    let inside = if uid == 1000 {
        leaf == Some("td-user-1000")
    } else {
        let delegation = crate::workload_accounts::delegation(uid)
            .ok_or("the process has no session delegation")?;
        leaf == Some(text(format_args!("{delegation}"))?.as_str())
    };
    if !inside {
        return Err(message(format_args!(
            "{what} process is outside its assigned session cgroup"
        )));
    }
    Ok(())
}

pub(crate) fn exec(arguments: &[String]) -> Result<(), String> {
    let (uid, rest) = arguments
        .split_first()
        .ok_or("application-exec requires UID and request")?;
    let uid = launch_uid(uid)?;
    let request = Request::parse(rest)?;
    require_process(
        uid,
        &bounded("/proc/self/status")?,
        &bounded("/proc/self/cgroup")?,
    )?;
    Err(format!(
        "exec application: {}",
        request.application(uid)?.exec()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The launcher, the shared consent codec and the ledger agree on what
    /// an application uid is; a uid past the ledger's range is no launch.
    #[test]
    fn every_application_uid_check_is_the_ledgers_range() {
        assert_eq!(crate::consent::APPLICATION_UIDS, APPLICATION_UIDS);
        assert_eq!(launch_uid("65536"), Ok(65536));
        assert_eq!(launch_uid("98303"), Ok(98303));
        for text in ["65535", "98304", "131071", "2147483647"] {
            assert!(launch_uid(text).is_err(), "{text}");
        }
    }

    fn request() -> Vec<String> {
        ["1000", "mail", "direct", "--", "literal;argument"]
            .map(str::to_string)
            .to_vec()
    }

    #[test]
    fn validator_reply_requires_eof_and_rejects_overflow_or_trailing_records() {
        use std::io::Write;
        for bytes in [
            b"TD-LAUNCH-APPLICATION-CHECK-OK\t65537\n".to_vec(),
            vec![b'x'; 97],
            b"TD-LAUNCH-APPLICATION-CHECK-OK\t65537\nextra".to_vec(),
        ] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&bytes).unwrap();
            drop(writer);
            let result = read_reply(
                reader,
                Instant::now() + Duration::from_secs(1),
                &mut said(),
                "application",
            );
            if bytes.len() > 96 {
                assert!(result.unwrap_err().contains("96 bytes"));
            } else {
                let result = result.unwrap();
                assert_eq!(
                    reply_uid(&LAUNCH, std::str::from_utf8(&result).unwrap()).is_ok(),
                    !bytes.ends_with(b"extra")
                );
            }
        }
    }

    /// A stderr tail whose child end is already closed.
    fn said() -> crate::stderr_tail::Tail {
        crate::stderr_tail::Tail::pair().unwrap().0
    }

    #[test]
    fn trickling_validator_bytes_spend_one_deadline_and_a_retained_writer_times_out() {
        use std::io::Write;
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..100 {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let begin = Instant::now();
        let error = read_reply(
            reader,
            begin + Duration::from_millis(40),
            &mut said(),
            "application",
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(begin.elapsed() < Duration::from_secs(1));
        worker.join().unwrap();
        let (reader, mut retained) = UnixStream::pair().unwrap();
        retained
            .write_all(b"TD-LAUNCH-APPLICATION-CHECK-OK\t65537\n")
            .unwrap();
        assert!(read_reply(
            reader,
            Instant::now() + Duration::from_millis(20),
            &mut said(),
            "application"
        )
        .unwrap_err()
        .contains("timed out"));
    }

    #[test]
    fn failed_admission_guard_kills_and_reaps_its_owned_validator() {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "application::tests::validator_sleep_fixture",
                "--ignored",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let child = CheckedChild(child);
        drop(child);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    #[ignore = "exec-only validator owned and reaped by its parent test"]
    fn validator_sleep_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn typed_launch_preserves_literal_arguments_and_fixes_program_and_account() {
        let request = Request::parse(&request()).unwrap();
        let helper = request.helper(65537);
        assert_eq!(helper.get_program(), "/bin/td-login");
        assert_eq!(
            helper
                .get_args()
                .map(|s| s.to_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "exec-service-as",
                "tda65537",
                "--",
                "/bin/td-authd",
                "application-exec",
                "65537",
                "1000",
                "mail",
                "direct",
                "--",
                "literal;argument"
            ]
        );
        let command = request.application(65537).unwrap();
        assert_eq!(command.get_program(), "/bin/td-jail");
        assert_eq!(
            command
                .get_args()
                .map(|s| s.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["literal;argument"]
        );
        // std exposes no argv[0] getter; its Debug output, not a stable
        // format, shows a replaced one as `[PROGRAM] "ARGV0"`.
        assert!(format!("{command:?}").ends_with(r#"["/bin/td-jail"] "mail" "literal;argument""#));
        assert!(command
            .get_envs()
            .all(|(key, _)| key != "TD_CONTROL_SOCKET"));
    }

    #[test]
    fn request_rejects_identity_paths_modes_and_oversized_arguments() {
        for (index, value) in [
            (0, "01000"),
            (0, "1001"),
            (1, "../mail"),
            (1, "Mail"),
            (1, ""),
            (2, "shell"),
            (2, "terminal"),
            (3, "arguments"),
        ] {
            let mut wrong = request();
            *wrong.get_mut(index).unwrap() = value.into();
            assert!(Request::parse(&wrong).is_err());
        }
        let mut wrong = request();
        wrong.push("x".repeat(32768));
        assert!(Request::parse(&wrong).is_err());
        wrong = request();
        wrong.push("embedded\0byte".into());
        assert!(Request::parse(&wrong).is_err());
        wrong = request();
        wrong.extend(std::iter::repeat_n("x".into(), 129));
        assert!(Request::parse(&wrong).is_err());
    }

    #[test]
    fn personal_launch_uses_the_human_helper_and_session_delegation() {
        let request = Request::parse(&request()).unwrap();
        let helper = request.helper(1000);
        let args: Vec<_> = helper.get_args().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            args.get(..3),
            Some(["exec-primary", "--", "/bin/td-authd"].as_slice())
        );
        assert_eq!(
            reply_uid(&LAUNCH, "TD-LAUNCH-APPLICATION-CHECK-OK\t1000\n").unwrap(),
            1000
        );
        let status = "Uid: 1000 1000 1000 1000\nGid: 1000 1000 1000 1000\nGroups: 1000\nCapInh: 0000000000000000\nCapPrm: 0000000000000000\nCapEff: 0000000000000000\nCapAmb: 0000000000000000\n";
        require_process(1000, status, "0::/td-user-1000/session\n").unwrap();
        assert!(require_process(1000, status, "0::/td-app-1000/session\n").is_err());
        assert!(require_process(
            1000,
            &status.replace("Gid: 1000", "Gid: 0"),
            "0::/td-user-1000/session\n"
        )
        .is_err());
    }

    #[test]
    fn replies_are_one_exact_bounded_canonical_application_identity() {
        assert_eq!(
            reply_uid(&LAUNCH, "TD-LAUNCH-APPLICATION-CHECK-OK\t65537\n").unwrap(),
            65537
        );
        for text in [
            "65537\n",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t1001\n",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t065537\n",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t65537\nextra",
            "TD-APPLICATION-ALLOCATED\t65537\n",
        ] {
            assert!(reply_uid(&LAUNCH, text).is_err(), "{text:?}");
        }
    }

    /// An allocation names the account td-firstboot made or found, which is
    /// always an application's own: never the human, nor a launch's reply.
    #[test]
    fn an_allocation_reply_names_one_application_account() {
        assert_eq!(ALLOCATE.verb, "allocate-application");
        assert_eq!(
            reply_uid(&ALLOCATE, "TD-APPLICATION-ALLOCATED\t66560\n").unwrap(),
            66560
        );
        for text in [
            "TD-APPLICATION-ALLOCATED\t1000\n",
            "TD-APPLICATION-ALLOCATED\t98304\n",
            "TD-APPLICATION-ALLOCATED\t066560\n",
            "TD-APPLICATION-ALLOCATED\t66560\nextra",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t66560\n",
        ] {
            assert!(reply_uid(&ALLOCATE, text).is_err(), "{text:?}");
        }
    }

    /// A sandbox's launch asks td-firstboot to prepare its existing row,
    /// whose reply names a sandbox's or workspace's account and nothing
    /// else.
    #[test]
    fn a_sandbox_preparation_reply_names_one_workload_account() {
        assert_eq!(PREPARE_SANDBOX.verb, "prepare-workload");
        assert_eq!(PREPARE_SANDBOX.words, ["sandbox"]);
        assert_eq!(
            reply_uid(&PREPARE_SANDBOX, "TD-WORKLOAD-PREPARED\t98304\n").unwrap(),
            98304
        );
        for text in [
            "TD-WORKLOAD-PREPARED\t1000\n",
            "TD-WORKLOAD-PREPARED\t65536\n",
            "TD-WORKLOAD-PREPARED\t131072\n",
            "TD-WORKLOAD-PREPARED\t098304\n",
            "TD-WORKLOAD-ALLOCATED\t98304\n",
            "TD-APPLICATION-ALLOCATED\t98304\n",
        ] {
            let refused = reply_uid(&PREPARE_SANDBOX, text).unwrap_err();
            assert!(!refused.contains("application"), "{text:?}: {refused}");
        }
    }

    /// Creating a sandbox asks td-firstboot to allocate its row, whose
    /// reply names a sandbox's or workspace's account and nothing else;
    /// it is the one Check whose verb adds a workload row.
    #[test]
    fn a_sandbox_allocation_reply_names_one_workload_account() {
        assert_eq!(ALLOCATE_SANDBOX.verb, "allocate-workload");
        assert_eq!(ALLOCATE_SANDBOX.words, ["sandbox"]);
        assert_eq!(
            reply_uid(&ALLOCATE_SANDBOX, "TD-WORKLOAD-ALLOCATED\t98304\n").unwrap(),
            98304
        );
        for text in [
            "TD-WORKLOAD-ALLOCATED\t1000\n",
            "TD-WORKLOAD-ALLOCATED\t65536\n",
            "TD-WORKLOAD-ALLOCATED\t098304\n",
            "TD-WORKLOAD-PREPARED\t98304\n",
            "TD-APPLICATION-ALLOCATED\t98304\n",
        ] {
            assert!(reply_uid(&ALLOCATE_SANDBOX, text).is_err(), "{text:?}");
        }
        for check in [&LAUNCH, &ALLOCATE, &PREPARE_SANDBOX] {
            assert_ne!(check.verb, "allocate-workload");
        }
    }

    /// A sandbox's account runs its jail from its own `td-workload-UID`
    /// leaf, the shared delegation's spelling, never an application's.
    #[test]
    fn a_workload_process_is_in_its_own_delegation() {
        let status = "Uid: 98304 98304 98304 98304\nGid: 98304 98304 98304 98304\nGroups: 98304\nCapInh: 0000000000000000\nCapPrm: 0000000000000000\nCapEff: 0000000000000000\nCapAmb: 0000000000000000\n";
        require_process(98304, status, "0::/td-workload-98304/session\n").unwrap();
        for membership in [
            "0::/td-app-98304/session\n",
            "0::/td-workload-98305/session\n",
            "0::/td-user-1000/session\n",
        ] {
            let refused = require_process(98304, status, membership).unwrap_err();
            assert_eq!(
                refused,
                "workload process is outside its assigned session cgroup"
            );
        }
        // A uid outside the ledger has no delegation at all.
        let other = status.replace("98304", "200000");
        assert!(require_process(200000, &other, "0::/td-app-200000/session\n").is_err());
    }

    #[test]
    fn credential_or_placement_mismatch_refuses_before_application_exec() {
        let status="Uid: 65537 65537 65537 65537\nGid: 65537 65537 65537 65537\nGroups: 65537\nCapInh: 0000000000000000\nCapPrm: 0000000000000000\nCapEff: 0000000000000000\nCapAmb: 0000000000000000\n";
        require_process(65537, status, "0::/td-app-65537/session\n").unwrap();
        for membership in [
            "0::/td-app-65536/session\n",
            "0::/td-user-1000/session\n",
            "0::/\n",
        ] {
            assert!(require_process(65537, status, membership).is_err());
        }
        for wrong in [
            status.replace("Uid: 65537", "Uid: 0"),
            status.replace("Groups: 65537", "Groups: 65537 0"),
            status.replace("CapEff: 0000000000000000", "CapEff: 0000000000000001"),
        ] {
            assert!(require_process(65537, &wrong, "0::/td-app-65537/session\n").is_err());
        }
    }
}
