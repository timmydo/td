//! Fixed application execution after root-owned deployment admission.

use crate::launch;
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
use crate::workload_accounts::APPLICATION_UIDS;

/// One fixed td-firstboot run root asks before it acts for an application:
/// its verb, the prefix of its one reply line, its bound, and the uids that
/// line may name.
struct Check {
    verb: &'static str,
    reply: &'static str,
    deadline: Duration,
    uid: fn(&str) -> Result<u32, String>,
}

/// The read-only launch admission: the account an active application runs
/// as, the human's for a personal one.
const LAUNCH: Check = Check {
    verb: "check-launch-application",
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
    verb: "allocate-application",
    reply: "TD-APPLICATION-ALLOCATED\t",
    deadline: Duration::from_secs(30),
    uid: service_uid,
};

struct Request {
    owner: u32,
    name: String,
    arguments: Vec<String>,
}

fn decimal(text: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    let value = text
        .parse::<u32>()
        .map_err(|_| "invalid application identity")?;
    if !range.contains(&value) {
        return Err("application identity is outside the supported range".into());
    }
    if value.to_string() != text {
        return Err("noncanonical application identity".into());
    }
    Ok(value)
}

impl Request {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let [owner, name, mode, separator, rest @ ..] = arguments else {
            return Err("application launch requires OWNER APP direct -- ARG...".into());
        };
        let owner = decimal(owner, 1000..=1000)?;
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
        decimal(text, APPLICATION_UIDS)
    }
}

/// An application's own account, never the human's.
fn service_uid(text: &str) -> Result<u32, String> {
    decimal(text, APPLICATION_UIDS)
}

fn reply_uid(check: &Check, text: &str) -> Result<u32, String> {
    let number = text
        .strip_prefix(check.reply)
        .and_then(|s| s.strip_suffix('\n'))
        .ok_or("invalid application admission response")?;
    (check.uid)(number)
}

fn admit(check: &Check, owner: u32, name: &str) -> Result<u32, String> {
    let deadline = Instant::now()
        .checked_add(check.deadline)
        .ok_or("application check deadline overflow")?;
    let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
    let (mut said, said_end) = crate::stderr_tail::Tail::pair()?;
    // The temporary Command drops its retained child endpoints here. Keeping
    // that Command alive would prevent the reply's required EOF.
    let mut checking = CheckedChild(
        Command::new("/bin/td-firstboot")
            .args([check.verb, &owner.to_string(), name])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(child)))
            .stderr(Stdio::from(said_end))
            .spawn()
            .map_err(|e| format!("start td-firstboot {}: {e}", check.verb))?,
    );
    let checked = (|| {
        let reply = read_reply(parent, deadline, &mut said)?;
        loop {
            said.drain();
            if Instant::now() >= deadline {
                return Err(format!("td-firstboot {} timed out", check.verb));
            }
            match checking.0.try_wait().map_err(|e| e.to_string())? {
                Some(status) if status.success() => {
                    let text = std::str::from_utf8(&reply)
                        .map_err(|_| "invalid application admission encoding")?;
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
) -> Result<Vec<u8>, String> {
    let mut reply = Vec::with_capacity(97);
    let mut byte = [0u8; 1];
    loop {
        said.drain();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("application admission reply timed out".into());
        }
        if reply.len() > 96 {
            return Err("application admission response exceeded 96 bytes".into());
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
            Err(e) => return Err(format!("application admission reply: {e}")),
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

fn bounded(path: &str) -> Result<String, String> {
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

fn require_process(uid: u32, status: &str, cgroup: &str) -> Result<(), String> {
    let expected = uid.to_string();
    for key in ["Uid:", "Gid:"] {
        let values = status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .ok_or("application process lacks identity fields")?;
        if values.split_ascii_whitespace().collect::<Vec<_>>() != [expected.as_str(); 4] {
            return Err("application process has the wrong credentials".into());
        }
    }
    let groups = status
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .ok_or("application process lacks its group set")?;
    if uid != 1000 && groups.split_ascii_whitespace().collect::<Vec<_>>() != [expected.as_str()] {
        return Err("application process has extra or missing groups".into());
    }
    for key in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
        if status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
            != Some("0000000000000000")
        {
            return Err("application process retained capabilities".into());
        }
    }
    let role = if uid == 1000 { "user" } else { "app" };
    if cgroup != format!("0::/td-{role}-{uid}/session\n") {
        return Err("application process is outside its assigned session cgroup".into());
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
            let result = read_reply(reader, Instant::now() + Duration::from_secs(1), &mut said());
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
        let error = read_reply(reader, begin + Duration::from_millis(40), &mut said()).unwrap_err();
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
            &mut said()
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
