//! The root application cache operation (td-software/DESIGN.md §3): root
//! selects an installed application's account from the immutable registry
//! and the ledger and runs the fixed `/bin/td-app` verb as that account.
//! No caller supplies an executable, UID, path or ref.

use crate::flathub_app::{self, Status};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// An install or update's bound, matching the forced-check unit's timeout
/// (svc_timeouts::APPLICATION_UPDATE) until the cutover retires the unit.
const CHECK_TIME: Duration = Duration::from_secs(3600);
/// Removing generations or an application's state.
const REMOVE_TIME: Duration = Duration::from_secs(600);
/// A status walks at most the two generations' deploys, on every poll; a
/// cold cache holding a platform runtime is the case this bound must fit.
const STATUS_TIME: Duration = Duration::from_secs(60);
/// The tail of td-app's diagnostics kept for the refusal this reports.
const DIAGNOSTIC_TAIL: usize = 512;
const WAIT_STEP: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verb {
    Install,
    Update,
    Remove,
    DeleteData,
    Status,
}

impl Verb {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "install" => Self::Install,
            "update" => Self::Update,
            "remove" => Self::Remove,
            "delete-data" => Self::DeleteData,
            "status" => Self::Status,
            _ => return None,
        })
    }

    fn word(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::DeleteData => "delete-data",
            Self::Status => "status",
        }
    }

    /// td-app's arguments after its verb's NAME. Install and update are one
    /// forced check: they differ only in whether a `current` exists.
    fn td_app(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Install | Self::Update => ("update", &["--force"]),
            Self::Remove => ("remove", &[]),
            Self::DeleteData => ("delete-data", &[]),
            Self::Status => ("status", &[]),
        }
    }

    fn lifetime(self) -> Duration {
        match self {
            Self::Install | Self::Update => CHECK_TIME,
            Self::Remove | Self::DeleteData => REMOVE_TIME,
            Self::Status => STATUS_TIME,
        }
    }
}

#[derive(Debug)]
struct Request {
    name: String,
    verb: Verb,
}

impl Request {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let [owner, name, verb] = arguments else {
            return Err("application cache requires OWNER APP VERB".into());
        };
        let verb = Verb::parse(verb).ok_or("invalid application cache arguments")?;
        if owner != "1000" || !crate::consent::application_name(name) {
            return Err("invalid application cache arguments".into());
        }
        Ok(Self {
            name: name.clone(),
            verb,
        })
    }

    /// The fixed credential drop into the application's account, which
    /// runs td-app with the account's own home and nothing of root's.
    fn command(&self, uid: u32) -> Command {
        let (verb, flags) = self.verb.td_app();
        let mut command = Command::new("/bin/td-login");
        command
            .args([
                "exec-service-as",
                &format!("tda{uid}"),
                "--",
                "/bin/td-app",
                verb,
                &self.name,
            ])
            .args(flags);
        command
    }
}

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the child wrote before EOF, and how it exited, both within the
/// deadline. `limit` refuses more output; `tail` keeps only the last bytes.
fn collect(
    mut child: Guard,
    mut wire: UnixStream,
    deadline: Instant,
    limit: Option<usize>,
) -> Result<(Vec<u8>, ExitStatus), String> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("td-app did not finish within its bound".into());
        }
        wire.set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        match wire.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                output.extend_from_slice(buffer.get(..count).ok_or("invalid read")?);
                match limit {
                    Some(limit) if output.len() > limit => {
                        return Err("td-app's answer exceeds its bound".into());
                    }
                    Some(_) => {}
                    None => {
                        let excess = output.len().saturating_sub(DIAGNOSTIC_TAIL);
                        output.drain(..excess);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("td-app did not finish within its bound".into());
            }
            Err(e) => return Err(format!("read td-app: {e}")),
        }
    }
    loop {
        if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
            // Reaped: the guard's kill and wait find nothing left.
            return Ok((output, status));
        }
        if Instant::now() >= deadline {
            return Err("td-app did not exit within its bound".into());
        }
        std::thread::sleep(WAIT_STEP);
    }
}

/// The last line of td-app's diagnostics as printable ASCII.
fn diagnostic(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let line = text
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .to_string();
    flathub_app::reason_text(&line)
}

/// A status as root relays it: exactly td-app's canonical record for the
/// requested application, or root's own for one the image ships.
fn checked_status(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "td-app's status is not UTF-8")?;
    let status = Status::parse(text).map_err(|e| format!("td-app's status: {e}"))?;
    if status.name != name {
        return Err("td-app's status names another application".into());
    }
    if status.cache.is_none() {
        return Err("td-app's status is not a Flathub cache's".into());
    }
    Ok(status.render().into_bytes())
}

fn image_status(name: &str) -> Result<Vec<u8>, String> {
    let rendered = Status {
        name: name.to_string(),
        cache: None,
    }
    .render();
    // The record's name grammar is the registry's; refuse rather than
    // relay a record no reader parses if the two ever part.
    Status::parse(&rendered).map_err(|e| format!("image application status: {e}"))?;
    Ok(rendered.into_bytes())
}

/// Run one cache operation; for `status` the record to print.
pub(crate) fn run(arguments: &[String]) -> Result<Vec<u8>, String> {
    let request = Request::parse(arguments)?;
    crate::launch::require_operation_startup()
        .map_err(|e| format!("application cache startup: {e}"))?;
    let uid = crate::application::admitted_uid(&request.name)?;
    if flathub_app::flathub_source(&request.name).is_none() {
        return match request.verb {
            Verb::Status => image_status(&request.name),
            _ => Err(format!(
                "{} ships in the image; it has no cache to {}",
                request.name,
                request.verb.word()
            )),
        };
    }
    let deadline = Instant::now()
        .checked_add(request.verb.lifetime())
        .ok_or("application cache deadline overflow")?;
    let (parent, endpoint) = UnixStream::pair().map_err(|e| e.to_string())?;
    let endpoint = Stdio::from(OwnedFd::from(endpoint));
    let (stdout, stderr) = match request.verb {
        Verb::Status => (endpoint, Stdio::null()),
        _ => (Stdio::null(), endpoint),
    };
    // The temporary Command drops its copy of the endpoint, so EOF comes
    // when td-app exits.
    let child = Guard(
        request
            .command(uid)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("start td-app: {e}"))?,
    );
    let limit = (request.verb == Verb::Status).then_some(flathub_app::MAX_STATUS_BYTES);
    let (output, status) = collect(child, parent, deadline, limit)?;
    if !status.success() {
        // A status's stderr is null and its stdout is no diagnostic.
        let reason = match request.verb {
            Verb::Status => String::new(),
            _ => diagnostic(&output),
        };
        let mut error = format!("td-app {} {}: {status}", request.verb.word(), request.name);
        if !reason.is_empty() {
            error.push_str(": ");
            error.push_str(&reason);
        }
        return Err(error);
    }
    match request.verb {
        Verb::Status => checked_status(&request.name, &output),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn requests_name_only_an_owner_an_application_and_a_fixed_verb() {
        for (verb, expected) in [
            ("install", ["update", "firefox", "--force"].as_slice()),
            ("update", ["update", "firefox", "--force"].as_slice()),
            ("remove", ["remove", "firefox"].as_slice()),
            ("delete-data", ["delete-data", "firefox"].as_slice()),
            ("status", ["status", "firefox"].as_slice()),
        ] {
            let request = Request::parse(&arguments(&["1000", "firefox", verb])).unwrap();
            let command = request.command(65536);
            assert_eq!(command.get_program(), "/bin/td-login");
            let args: Vec<&str> = command.get_args().map(|s| s.to_str().unwrap()).collect();
            let mut wanted = vec!["exec-service-as", "tda65536", "--", "/bin/td-app"];
            wanted.extend_from_slice(expected);
            assert_eq!(args, wanted);
        }
        for bad in [
            vec!["1000", "firefox"],
            vec!["1000", "firefox", "status", "extra"],
            vec!["1001", "firefox", "status"],
            vec!["01000", "firefox", "status"],
            vec!["1000", "../firefox", "status"],
            vec!["1000", "Firefox", "status"],
            vec!["1000", "firefox", "ready"],
            vec!["1000", "firefox", "update --force"],
            vec!["1000", "firefox", "revalidate"],
        ] {
            assert!(Request::parse(&arguments(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_status_is_relayed_only_canonical_and_for_its_application() {
        let record = "format=1\nname=firefox\nkind=flathub\ncurrent=none\nprevious=none\n\
                      checked=never\nrefused=none\nprogress=none\n";
        assert_eq!(
            checked_status("firefox", record.as_bytes()).unwrap(),
            record.as_bytes()
        );
        for bad in [
            record.replace("name=firefox", "name=mail"),
            record.replace("kind=flathub\ncurrent=none\nprevious=none\nchecked=never\nrefused=none\nprogress=none\n", "kind=image\n"),
            record.replace("checked=never", "checked=0never"),
            format!("{record}extra\n"),
        ] {
            assert!(checked_status("firefox", bad.as_bytes()).is_err(), "{bad}");
        }
        assert!(checked_status("firefox", b"\xff").is_err());
        assert_eq!(
            image_status("mail").unwrap(),
            b"format=1\nname=mail\nkind=image\n"
        );
        assert_eq!(
            image_status("mail_box.v2").unwrap(),
            b"format=1\nname=mail_box.v2\nkind=image\n"
        );
        assert!(image_status("Mail").is_err());
        assert!(
            checked_status("firefox", b"format=1\nname=firefox\nkind=image\n")
                .unwrap_err()
                .contains("not a Flathub")
        );
    }

    /// What the fixture writes on its stdin socket, by selector.
    #[test]
    #[ignore = "exec-only application cache output fixture"]
    fn output_fixture() {
        use std::io::Write;
        use std::os::fd::AsFd;
        let mut wire = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
        let mut selector = [0];
        wire.read_exact(&mut selector).unwrap();
        match selector[0] {
            0 => {
                wire.write_all(b"one\ntwo\n").unwrap();
                std::process::exit(3);
            }
            1 => wire.write_all(&[0; 100]).unwrap(),
            2 => {
                wire.write_all(&[b'x'; 100_000]).unwrap();
                wire.write_all(b"\nlast\n").unwrap();
            }
            // Closes its end and keeps running: the same process, its
            // stdin replaced and the socket's duplicate closed on exec.
            4 => {
                use std::os::unix::process::CommandExt;
                let error = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "application_cache::tests::sleep_fixture",
                        "--ignored",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .exec();
                panic!("exec the sleep fixture: {error}");
            }
            _ => std::thread::sleep(Duration::from_secs(30)),
        }
    }

    #[test]
    #[ignore = "exec-only application cache sleep fixture"]
    fn sleep_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    fn child(selector: u8) -> (Guard, UnixStream) {
        use std::io::Write;
        let (mut parent, endpoint) = UnixStream::pair().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "application_cache::tests::output_fixture",
                "--ignored",
            ])
            .stdin(Stdio::from(OwnedFd::from(endpoint)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        parent.write_all(&[selector]).unwrap();
        (Guard(child), parent)
    }

    #[test]
    fn output_is_bounded_by_size_and_by_the_deadline() {
        let soon = || Instant::now() + Duration::from_secs(10);
        let (guard, wire) = child(0);
        let (output, status) = collect(guard, wire, soon(), Some(64)).unwrap();
        assert_eq!(output, b"one\ntwo\n");
        assert_eq!(status.code(), Some(3));
        assert_eq!(diagnostic(&output), "two");

        let (guard, wire) = child(1);
        assert!(collect(guard, wire, soon(), Some(64))
            .unwrap_err()
            .contains("exceeds"));

        // Diagnostics keep only their tail, however long they run.
        let (guard, wire) = child(2);
        let (output, status) = collect(guard, wire, soon(), None).unwrap();
        assert!(status.success());
        assert!(output.len() <= DIAGNOSTIC_TAIL);
        assert_eq!(diagnostic(&output), "last");

        let begin = Instant::now();
        let (guard, wire) = child(3);
        let pid = guard.0.id();
        let error = collect(
            guard,
            wire,
            Instant::now() + Duration::from_millis(200),
            None,
        )
        .unwrap_err();
        assert!(error.contains("within its bound"), "{error}");
        assert!(begin.elapsed() < Duration::from_secs(5));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());

        // EOF without an exit is killed at the deadline too.
        let begin = Instant::now();
        let (guard, wire) = child(4);
        let pid = guard.0.id();
        let error =
            collect(guard, wire, Instant::now() + Duration::from_secs(5), None).unwrap_err();
        assert!(error.contains("did not exit"), "{error}");
        assert!(begin.elapsed() < Duration::from_secs(15));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}
