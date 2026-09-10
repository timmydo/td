//! Fixed application execution after root-owned deployment admission.

use crate::launch;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(2);
const APPLICATION_UIDS: std::ops::RangeInclusive<u32> = 65536..=2147483647;
const REPLY: &str = "TD-LAUNCH-APPLICATION-CHECK-OK\t";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    Direct,
    Terminal,
    Shell,
}

struct Request {
    owner: u32,
    name: String,
    presentation: Presentation,
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
            return Err("application launch requires OWNER APP direct|terminal|shell -- ARG...".into());
        };
        let owner = decimal(owner, 1000..=1000)?;
        if separator != "--"
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
        let presentation = match mode.as_str() {
            "direct" => Presentation::Direct,
            "terminal" => Presentation::Terminal,
            "shell" if name == "claude" && rest.is_empty() => Presentation::Shell,
            _ => return Err("application launch presentation must be direct, terminal, or argument-free Claude shell".into()),
        };
        Ok(Self {
            owner,
            name: name.clone(),
            presentation,
            arguments: rest.to_vec(),
        })
    }

    fn helper(&self, uid: u32) -> Command {
        let mut command = Command::new("/bin/td-login");
        command
            .args([
                "exec-service-as",
                &format!("tda{uid}"),
                "--",
                "/bin/td-authd",
                "application-exec",
                &uid.to_string(),
                &self.owner.to_string(),
                &self.name,
                self.mode(),
                "--",
            ])
            .args(&self.arguments);
        command
    }

    fn mode(&self) -> &'static str {
        match self.presentation {
            Presentation::Direct => "direct",
            Presentation::Terminal => "terminal",
            Presentation::Shell => "shell",
        }
    }

    fn application(&self, uid: u32) -> Command {
        let executable = format!("/bin/{}", self.name);
        let mut command = match self.presentation {
            Presentation::Direct | Presentation::Shell => Command::new(executable),
            Presentation::Terminal => {
                let mut command = Command::new("/bin/td-term");
                command.args([
                    "run",
                    "--socket",
                    &format!("/run/td-compositor/{}/wayland-0", self.owner),
                    "--ready-socket",
                    &format!("/run/user/{uid}/td-app-{}.ready", self.name),
                    "--command",
                    &executable,
                ]);
                command
            }
        };
        command
            .args(&self.arguments)
            .env_clear()
            .current_dir("/")
            .env("HOME", format!("/var/lib/td/applications/{uid}"))
            .env("USER", format!("tda{uid}"))
            .env("LOGNAME", format!("tda{uid}"))
            .env("SHELL", "/bin/false")
            .env("PATH", "/bin")
            .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
        command
    }
}

struct CheckedChild(Child);
impl Drop for CheckedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn reply_uid(text: &str) -> Result<u32, String> {
    let number = text
        .strip_prefix(REPLY)
        .and_then(|s| s.strip_suffix('\n'))
        .ok_or("invalid application admission response")?;
    decimal(number, APPLICATION_UIDS)
}

fn admit(owner: u32, name: &str) -> Result<u32, String> {
    let deadline = Instant::now()
        .checked_add(DEADLINE)
        .ok_or("application check deadline overflow")?;
    let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
    // The temporary Command drops its retained child endpoint here. Keeping
    // that Command alive would prevent the reply's required EOF.
    let mut check = CheckedChild(
        Command::new("/bin/td-firstboot")
            .args([
                "check-launch-application",
                &owner.to_string(),
                name,
            ])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(child)))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start application admission check: {e}"))?,
    );
    let reply = read_reply(parent, deadline)?;
    loop {
        if Instant::now() >= deadline {
            return Err("application admission check timed out".into());
        }
        match check.0.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => {
                let text = std::str::from_utf8(&reply)
                    .map_err(|_| "invalid application admission encoding")?;
                return reply_uid(text);
            }
            Some(status) => return Err(format!("application admission failed: {status}")),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn read_reply(mut parent: UnixStream, deadline: Instant) -> Result<Vec<u8>, String> {
    let mut reply = Vec::with_capacity(97);
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("application admission reply timed out".into());
        }
        if reply.len() > 96 {
            return Err("application admission response exceeded 96 bytes".into());
        }
        parent
            .set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        match parent.read(&mut byte) {
            Ok(0) => return Ok(reply),
            Ok(_) => reply.extend_from_slice(&byte),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("application admission reply timed out".into());
            }
            Err(e) => return Err(format!("application admission reply: {e}")),
        }
    }
}

pub(crate) fn admitted_uid(name: &str) -> Result<u32, String> {
    if !crate::consent::application_name(name) { return Err("invalid application name".into()); }
    admit(1000, name)
}

pub(crate) fn start(arguments: &[String]) -> Result<(), String> {
    let request = Request::parse(arguments)?;
    launch::require_launch_startup().map_err(|e| format!("application startup: {e}"))?;
    let uid = admit(request.owner, &request.name)?;
    let input = if request.presentation == Presentation::Shell {
        if uid != crate::application_shell::UID {
            return Err("unexpected Claude assignment".into());
        }
        crate::application_shell::bind().map_err(|e| e.to_string())?
    } else {
        Stdio::null()
    };
    // Only the newly bound shell listener can cross this handoff; no root
    // log or ambient descriptor reaches the credential helper.
    let error = request
        .helper(uid)
        .env_clear()
        .current_dir("/")
        .stdin(input)
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
    if groups.split_ascii_whitespace().collect::<Vec<_>>() != [expected.as_str()] {
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
    if cgroup != format!("0::/td-app-{uid}/session\n") {
        return Err("application process is outside its assigned session cgroup".into());
    }
    Ok(())
}

pub(crate) fn exec(arguments: &[String]) -> Result<(), String> {
    let (uid, rest) = arguments
        .split_first()
        .ok_or("application-exec requires UID and request")?;
    let uid = decimal(uid, APPLICATION_UIDS)?;
    let request = Request::parse(rest)?;
    require_process(
        uid,
        &bounded("/proc/self/status")?,
        &bounded("/proc/self/cgroup")?,
    )?;
    if request.presentation == Presentation::Shell {
        if uid != crate::application_shell::UID {
            return Err("unexpected Claude assignment".into());
        }
        return crate::application_shell::serve().map_err(|e| e.to_string());
    }
    Err(format!(
        "exec application: {}",
        request.application(uid).exec()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn request() -> Vec<String> {
        ["1000", "mail", "terminal", "--", "literal;argument"]
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
            let result = read_reply(reader, Instant::now() + Duration::from_secs(1));
            if bytes.len() > 96 {
                assert!(result.unwrap_err().contains("96 bytes"));
            } else {
                let result = result.unwrap();
                assert_eq!(
                    reply_uid(std::str::from_utf8(&result).unwrap()).is_ok(),
                    !bytes.ends_with(b"extra")
                );
            }
        }
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
        let error = read_reply(reader, begin + Duration::from_millis(40)).unwrap_err();
        assert!(error.contains("timed out"));
        assert!(begin.elapsed() < Duration::from_secs(1));
        worker.join().unwrap();
        let (reader, mut retained) = UnixStream::pair().unwrap();
        retained
            .write_all(b"TD-LAUNCH-APPLICATION-CHECK-OK\t65537\n")
            .unwrap();
        assert!(
            read_reply(reader, Instant::now() + Duration::from_millis(20))
                .unwrap_err()
                .contains("timed out")
        );
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
                "terminal",
                "--",
                "literal;argument"
            ]
        );
        let command = request.application(65537);
        assert_eq!(command.get_program(), "/bin/td-term");
        assert_eq!(command.get_args().last().unwrap(), "literal;argument");
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
    fn replies_are_one_exact_bounded_canonical_application_identity() {
        assert_eq!(
            reply_uid("TD-LAUNCH-APPLICATION-CHECK-OK\t65537\n").unwrap(),
            65537
        );
        for text in [
            "65537\n",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t1000\n",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t065537\n",
            "TD-LAUNCH-APPLICATION-CHECK-OK\t65537\nextra",
        ] {
            assert!(reply_uid(text).is_err());
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
