//! The root application and runtime cache operations (td-software/DESIGN.md
//! §3, WORKLOADS.md "Runtimes"): root selects an installed application's
//! account from the immutable registry and the ledger, allocating it in the
//! ledger when an install first needs it, or the compiled runtime account,
//! and runs a fixed `/bin/td-app` verb as it. No caller supplies an
//! executable, UID, path or ref.

use crate::flathub_app::{
    self, CacheStatus, FlathubRuntime, FlathubSource, RuntimeStatus, Status, RUNTIME_ACCOUNT,
};
use crate::stderr_tail;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// An install or update's bound: resolving the runtime's ref and then the
/// application's and, when either moved, downloading and materializing a
/// runtime or application of about a gigabyte on a slow link, then the
/// dynamic validator over both.
const CHECK_TIME: Duration = Duration::from_secs(3600);
/// Removing generations or an application's state, and a runtime no
/// installed application names.
const REMOVE_TIME: Duration = Duration::from_secs(600);
/// A status walks at most the two generations' deploys, on every poll; a
/// cold cache holding a platform runtime is the case this bound must fit.
const STATUS_TIME: Duration = Duration::from_secs(60);
const WAIT_STEP: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verb {
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

    pub(crate) fn word(self) -> &'static str {
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

    pub(crate) fn lifetime(self) -> Duration {
        match self {
            Self::Install | Self::Update => CHECK_TIME,
            Self::Remove | Self::DeleteData => REMOVE_TIME,
            Self::Status => STATUS_TIME,
        }
    }
}

/// What `runtime-cache` asks of one runtime's store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeVerb {
    Install,
    Update,
    Remove,
    Status,
}

impl RuntimeVerb {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "install" => Self::Install,
            "update" => Self::Update,
            "remove" => Self::Remove,
            "status" => Self::Status,
            _ => return None,
        })
    }

    /// td-app's runtime verb and the flags after the runtime's name.
    fn td_app(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Install | Self::Update => ("update", &["--force"]),
            Self::Remove => ("remove", &[]),
            Self::Status => ("status", &[]),
        }
    }

    fn lifetime(self) -> Duration {
        match self {
            Self::Install | Self::Update => CHECK_TIME,
            Self::Remove => REMOVE_TIME,
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
}

#[derive(Debug)]
struct RuntimeRequest {
    runtime: FlathubRuntime,
    verb: RuntimeVerb,
}

impl RuntimeRequest {
    /// RUNTIME is a declared runtime's td name; the account is compiled.
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let [runtime, verb] = arguments else {
            return Err("runtime cache requires RUNTIME VERB".into());
        };
        let verb = RuntimeVerb::parse(verb).ok_or("invalid runtime cache arguments")?;
        let runtime =
            flathub_app::flathub_runtime(runtime).ok_or("invalid runtime cache arguments")?;
        Ok(Self { runtime, verb })
    }
}

/// The fixed credential drop into `account`, which runs td-app with the
/// account's own home and nothing of root's: an admitted application's
/// account, or the runtime account.
fn td_app(account: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new("/bin/td-login");
    command
        .args(["exec-service-as", account, "--", "/bin/td-app"])
        .args(arguments);
    command
}

/// td-app's `verb` for application `name`, as its admitted account `uid`.
fn application_command(uid: u32, verb: Verb, name: &str) -> Command {
    let (word, flags) = verb.td_app();
    let mut arguments = vec![word, name];
    arguments.extend_from_slice(flags);
    td_app(&format!("tda{uid}"), &arguments)
}

/// td-app's runtime `verb` for `runtime`, as the runtime account.
fn runtime_command(runtime: FlathubRuntime, verb: RuntimeVerb) -> Command {
    let (word, flags) = verb.td_app();
    let mut arguments = vec!["runtime", word, runtime.name];
    arguments.extend_from_slice(flags);
    td_app(RUNTIME_ACCOUNT, &arguments)
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
                    None => stderr_tail::push(&mut output, &[]),
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
    flathub_app::reason_text(&stderr_tail::last_reason(tail).unwrap_or_default())
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

/// A runtime store's status as root relays it: exactly td-app's canonical
/// record for the requested runtime.
fn checked_runtime_status(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "td-app's status is not UTF-8")?;
    let status = RuntimeStatus::parse(text).map_err(|e| format!("td-app's status: {e}"))?;
    if status.name != name {
        return Err("td-app's status names another runtime".into());
    }
    Ok(status.render().into_bytes())
}

/// The status of a declared Flathub application with no account: it was
/// never installed here, so it is a cache with nothing in it, which root
/// answers without td-app.
fn not_installed_status(name: &str) -> Vec<u8> {
    Status {
        name: name.to_string(),
        cache: Some(CacheStatus {
            current: None,
            previous: None,
            checked: None,
            refused: None,
            progress: None,
        }),
    }
    .render()
    .into_bytes()
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

/// What an operation reaches outside root's own code: td-app runs, the
/// admission and allocation of an application's account, whether it has
/// one, and the image's registry. The operations run on `System`; their
/// tests on a script.
trait Host {
    fn td_app(
        &mut self,
        command: Command,
        status: bool,
        deadline: Instant,
        what: &str,
    ) -> Result<Vec<u8>, String>;
    fn admitted_uid(&mut self, name: &str) -> Result<u32, String>;
    /// The account an install of `name` fetches as: its ledger row,
    /// allocated when it has none.
    fn allocate(&mut self, name: &str) -> Result<u32, String>;
    /// The ledger account of the declared application `name`, or none
    /// when nothing ever installed it here.
    fn account(&mut self, name: &str) -> Result<Option<u32>, String>;
    fn registry(&mut self) -> Result<Vec<String>, String>;
}

struct System;

impl Host for System {
    fn td_app(
        &mut self,
        command: Command,
        status: bool,
        deadline: Instant,
        what: &str,
    ) -> Result<Vec<u8>, String> {
        run_td_app(command, status, deadline, what)
    }

    fn admitted_uid(&mut self, name: &str) -> Result<u32, String> {
        crate::application::admitted_uid(name)
    }

    fn allocate(&mut self, name: &str) -> Result<u32, String> {
        crate::application::allocated_uid(name)
    }

    /// An undeclared name is an error, not "no account": the image builds
    /// its software registry and its declarations from one application
    /// list, so only an inconsistent image reaches it, and there the
    /// runtime check keeps the runtime (`unwrap_or(true)`) rather than
    /// guess that nothing of that application's needs it.
    fn account(&mut self, name: &str) -> Result<Option<u32>, String> {
        let policy = crate::app_policy::load()?;
        if !policy
            .declarations()
            .rows()
            .iter()
            .any(|row| row.application == name)
        {
            return Err(format!("the deployment does not declare {name}"));
        }
        Ok(policy.account(name))
    }

    fn registry(&mut self) -> Result<Vec<String>, String> {
        crate::software::load_registry()
    }
}

/// Run one application cache operation; for `status` the record to print.
pub(crate) fn run(arguments: &[String]) -> Result<Vec<u8>, String> {
    let request = Request::parse(arguments)?;
    crate::launch::require_operation_startup()
        .map_err(|e| format!("application cache startup: {e}"))?;
    let Some(source) = flathub_app::flathub_source(&request.name) else {
        // An application the image ships has no cache, and its account is
        // the one the deployment carries.
        crate::application::admitted_uid(&request.name)?;
        return match request.verb {
            Verb::Status => image_status(&request.name),
            _ => Err(format!(
                "{} ships in the image; it has no cache to {}",
                request.name,
                request.verb.word()
            )),
        };
    };
    let deadline = Instant::now()
        .checked_add(request.verb.lifetime())
        .ok_or("application cache deadline overflow")?;
    operate(&mut System, &request, source, deadline)
}

/// A Flathub application's operation. An install first allocates the
/// application's account, or finds the one an earlier install allocated,
/// which is its admission; every other verb is admitted only for an
/// application with an account, and one without was never installed here.
/// An install or update then runs the declared runtime's check, as the
/// runtime account: the application is validated against the runtime
/// store's `current`. A removal keeps the account and its home, and
/// removes the runtime when no other installed application names it.
fn operate(
    host: &mut impl Host,
    request: &Request,
    source: FlathubSource,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let uid = match request.verb {
        Verb::Install => host.allocate(&request.name)?,
        _ => match host.account(&request.name)? {
            Some(_) => host.admitted_uid(&request.name)?,
            None if request.verb == Verb::Status => return Ok(not_installed_status(&request.name)),
            None => return Err(format!("{} is not installed", request.name)),
        },
    };
    let what = format!("{} {}", request.verb.word(), request.name);
    let command = || application_command(uid, request.verb, &request.name);
    match request.verb {
        Verb::Install | Verb::Update => {
            host.td_app(
                runtime_command(source.runtime, RuntimeVerb::Update),
                false,
                deadline,
                &format!("runtime update {}", source.runtime.name),
            )?;
            host.td_app(command(), false, deadline, &what)?;
        }
        Verb::Remove => {
            host.td_app(command(), false, deadline, &what)?;
            if !named_elsewhere(
                host,
                flathub_app::FLATHUB_SOURCES,
                source.runtime,
                Some(&request.name),
                deadline,
            ) {
                host.td_app(
                    runtime_command(source.runtime, RuntimeVerb::Remove),
                    false,
                    deadline,
                    &format!("runtime remove {}", source.runtime.name),
                )
                .map_err(|e| {
                    format!(
                        "{} is removed, but its runtime {} is not: {e}",
                        request.name, source.runtime.name
                    )
                })?;
            }
        }
        Verb::DeleteData => {
            host.td_app(command(), false, deadline, &what)?;
        }
        Verb::Status => {
            return checked_status(
                &request.name,
                &host.td_app(command(), true, deadline, &what)?,
            )
        }
    }
    Ok(Vec::new())
}

/// Run one runtime cache operation as the runtime account; for `status`
/// the record to print.
pub(crate) fn run_runtime(arguments: &[String]) -> Result<Vec<u8>, String> {
    let request = RuntimeRequest::parse(arguments)?;
    crate::launch::require_operation_startup()
        .map_err(|e| format!("runtime cache startup: {e}"))?;
    let deadline = Instant::now()
        .checked_add(request.verb.lifetime())
        .ok_or("runtime cache deadline overflow")?;
    operate_runtime(&mut System, &request, deadline)
}

/// A removal refuses while an application naming the runtime is installed
/// or its account's operation runs.
fn operate_runtime(
    host: &mut impl Host,
    request: &RuntimeRequest,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let runtime = request.runtime;
    let (word, _) = request.verb.td_app();
    let what = format!("runtime {word} {}", runtime.name);
    if request.verb == RuntimeVerb::Remove
        && named_elsewhere(host, flathub_app::FLATHUB_SOURCES, runtime, None, deadline)
    {
        return Err(format!(
            "runtime {} stays while an installed application names it",
            runtime.name
        ));
    }
    let status = request.verb == RuntimeVerb::Status;
    let output = host.td_app(
        runtime_command(runtime, request.verb),
        status,
        deadline,
        &what,
    )?;
    if status {
        return checked_runtime_status(runtime.name, &output);
    }
    Ok(Vec::new())
}

/// Whether an application of `sources` other than `except` that names
/// `runtime` may need it: its account's status shows a `current`, or an
/// operation that holds its cache's lock, an install's included. One the
/// image's registry does not list, or that has no account because nothing
/// installed it here, needs nothing; one whose registry, account,
/// admission or status cannot be read counts, so a runtime is never
/// removed on a guess.
///
/// This is a check, then an act: an install of another application that
/// starts after it, while its runtime half has not yet reached the
/// application's cache, can lose the runtime it just installed. That
/// install then fails, or a launch refuses until the owner updates the
/// application, which installs the runtime again. Only root could close
/// the window, by holding every account's cache lock across the removal;
/// td-app cannot, since no account reads another's cache.
fn named_elsewhere(
    host: &mut impl Host,
    sources: &[FlathubSource],
    runtime: FlathubRuntime,
    except: Option<&str>,
    deadline: Instant,
) -> bool {
    let registry = host.registry();
    sources
        .iter()
        .filter(|other| other.runtime.name == runtime.name && Some(other.name) != except)
        .any(|other| match &registry {
            Ok(names) if !names.iter().any(|name| name == other.name) => false,
            Ok(_) => occupied(host, other.name, deadline).unwrap_or(true),
            Err(_) => true,
        })
}

/// Whether `name`'s cache has a `current` or a writer. An application with
/// no account has no cache.
fn occupied(host: &mut impl Host, name: &str, deadline: Instant) -> Result<bool, String> {
    if host.account(name)?.is_none() {
        return Ok(false);
    }
    let uid = host.admitted_uid(name)?;
    let record = host.td_app(
        application_command(uid, Verb::Status, name),
        true,
        deadline,
        &format!("status {name}"),
    )?;
    let text = std::str::from_utf8(&record).map_err(|_| "td-app's status is not UTF-8")?;
    let status = Status::parse(text).map_err(|e| format!("td-app's status: {e}"))?;
    if status.name != name {
        return Err("td-app's status names another application".into());
    }
    Ok(status
        .cache
        .is_some_and(|cache| cache.current.is_some() || cache.progress.is_some()))
}

/// Run one td-app `command` to its end within `deadline`, with nothing of
/// root's: for a status its stdout, bounded to a record, and a null
/// stderr; otherwise a null stdout and its stderr's tail, whose last line
/// a failed exit reports. `what` names the run in that report.
fn run_td_app(
    mut command: Command,
    status: bool,
    deadline: Instant,
    what: &str,
) -> Result<Vec<u8>, String> {
    let (parent, endpoint) = UnixStream::pair().map_err(|e| e.to_string())?;
    let endpoint = Stdio::from(OwnedFd::from(endpoint));
    let (stdout, stderr) = if status {
        (endpoint, Stdio::null())
    } else {
        (Stdio::null(), endpoint)
    };
    let child = Guard(
        command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("start td-app: {e}"))?,
    );
    // The command holds its copy of the endpoint until it goes, so EOF
    // comes when td-app exits.
    drop(command);
    let limit = status.then_some(flathub_app::MAX_STATUS_BYTES);
    let (output, exit) = collect(child, parent, deadline, limit)?;
    if !exit.success() {
        // A status's stderr is null and its stdout is no diagnostic.
        let reason = if status {
            String::new()
        } else {
            diagnostic(&output)
        };
        let mut error = format!("td-app {what}: {exit}");
        if !reason.is_empty() {
            error.push_str(": ");
            error.push_str(&reason);
        }
        return Err(error);
    }
    Ok(output)
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
            let command = application_command(65536, request.verb, &request.name);
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

    /// `runtime-cache` names a declared runtime and a verb; the account is
    /// the compiled runtime account's, never the caller's.
    #[test]
    fn runtime_requests_name_only_a_declared_runtime_and_a_fixed_verb() {
        let runtime = flathub_app::flathub_source("firefox").unwrap().runtime;
        for (verb, expected) in [
            ("install", ["update", runtime.name, "--force"].as_slice()),
            ("update", ["update", runtime.name, "--force"].as_slice()),
            ("remove", ["remove", runtime.name].as_slice()),
            ("status", ["status", runtime.name].as_slice()),
        ] {
            let request = RuntimeRequest::parse(&arguments(&[runtime.name, verb])).unwrap();
            assert_eq!(request.runtime, runtime);
            let command = runtime_command(request.runtime, request.verb);
            assert_eq!(command.get_program(), "/bin/td-login");
            let args: Vec<&str> = command.get_args().map(|s| s.to_str().unwrap()).collect();
            let mut wanted = vec![
                "exec-service-as",
                "td-runtimes",
                "--",
                "/bin/td-app",
                "runtime",
            ];
            wanted.extend_from_slice(expected);
            assert_eq!(args, wanted, "{verb}");
        }
        for bad in [
            vec![runtime.name],
            vec![runtime.name, "status", "extra"],
            vec!["firefox", "status"],
            vec!["../freedesktop-platform-25-08", "status"],
            vec![runtime.name, "delete-data"],
            vec![runtime.name, "ready"],
            vec![runtime.name, "revalidate"],
            vec!["1000", runtime.name, "status"],
        ] {
            assert!(RuntimeRequest::parse(&arguments(&bad)).is_err(), "{bad:?}");
        }
        // An install or update runs as long as an application's, since it
        // carries the runtime's download.
        assert_eq!(RuntimeVerb::Install.lifetime(), Verb::Install.lifetime());
        assert_eq!(RuntimeVerb::Remove.lifetime(), Verb::Remove.lifetime());
        assert_eq!(RuntimeVerb::Status.lifetime(), Verb::Status.lifetime());
    }

    #[test]
    fn a_runtime_status_is_relayed_only_canonical_and_for_its_runtime() {
        let record = "format=1\nname=freedesktop-platform-25-08\nkind=runtime\ncurrent=none\n\
                      previous=none\nchecked=never\nrefused=none\nprogress=none\n";
        assert_eq!(
            checked_runtime_status("freedesktop-platform-25-08", record.as_bytes()).unwrap(),
            record.as_bytes()
        );
        for bad in [
            record.replace("name=freedesktop", "name=other"),
            record.replace("kind=runtime", "kind=flathub"),
            format!("{record}extra\n"),
        ] {
            assert!(
                checked_runtime_status("freedesktop-platform-25-08", bad.as_bytes()).is_err(),
                "{bad}"
            );
        }
    }

    /// A host that records each td-app run as its account and td-app's
    /// arguments, and each allocation as `allocate NAME`, in order, and
    /// answers from a script.
    struct Script {
        registry: Result<Vec<String>, String>,
        admitted: Result<u32, String>,
        allocated: Result<u32, String>,
        account: fn(&str) -> Result<Option<u32>, String>,
        answer: fn(&[String]) -> Result<Vec<u8>, String>,
        runs: Vec<Vec<String>>,
        admissions: usize,
    }

    impl Script {
        fn new(answer: fn(&[String]) -> Result<Vec<u8>, String>) -> Self {
            Self {
                registry: Ok(vec!["firefox".into(), "mail".into()]),
                admitted: Ok(65536),
                allocated: Ok(65536),
                account: |_| Ok(Some(65536)),
                answer,
                runs: Vec::new(),
                admissions: 0,
            }
        }
    }

    impl Host for Script {
        fn td_app(
            &mut self,
            command: Command,
            _status: bool,
            _deadline: Instant,
            _what: &str,
        ) -> Result<Vec<u8>, String> {
            assert_eq!(command.get_program(), "/bin/td-login");
            let args: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap().to_string())
                .collect();
            let [login, account, separator, program, rest @ ..] = args.as_slice() else {
                panic!("{args:?}");
            };
            assert_eq!(
                [login, separator, program],
                ["exec-service-as", "--", "/bin/td-app"]
            );
            let mut run = vec![account.clone()];
            run.extend_from_slice(rest);
            self.runs.push(run.clone());
            (self.answer)(&run)
        }

        fn admitted_uid(&mut self, name: &str) -> Result<u32, String> {
            assert!(["firefox", "photo"].contains(&name), "{name}");
            self.admissions += 1;
            self.admitted.clone()
        }

        fn allocate(&mut self, name: &str) -> Result<u32, String> {
            self.runs.push(vec!["allocate".into(), name.into()]);
            self.allocated.clone()
        }

        fn account(&mut self, name: &str) -> Result<Option<u32>, String> {
            (self.account)(name)
        }

        fn registry(&mut self) -> Result<Vec<String>, String> {
            self.registry.clone()
        }
    }

    fn runs(lines: &[&str]) -> Vec<Vec<String>> {
        lines
            .iter()
            .map(|line| line.split(' ').map(str::to_string).collect())
            .collect()
    }

    /// A recorded run's td-app verb, after its account.
    fn verb_of(run: &[String]) -> &str {
        run.get(1).map_or("", String::as_str)
    }

    fn ok(_: &[String]) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    const RUNTIME_UPDATE: &str = "td-runtimes runtime update freedesktop-platform-25-08 --force";
    const RUNTIME_REMOVE: &str = "td-runtimes runtime remove freedesktop-platform-25-08";
    const STATUS: &str = "tda65536 status firefox";

    fn operation(verb: Verb, host: &mut Script) -> Result<Vec<u8>, String> {
        let request = Request {
            name: "firefox".into(),
            verb,
        };
        let source = flathub_app::flathub_source("firefox").unwrap();
        let far = Instant::now() + Duration::from_secs(60);
        operate(host, &request, source, far)
    }

    fn runtime_removal(host: &mut Script) -> Result<Vec<u8>, String> {
        let request = RuntimeRequest {
            runtime: flathub_app::flathub_source("firefox").unwrap().runtime,
            verb: RuntimeVerb::Remove,
        };
        operate_runtime(host, &request, Instant::now() + Duration::from_secs(60))
    }

    /// An install or update checks the runtime before the application,
    /// and the application's half runs only once the runtime's succeeded;
    /// a removal removes the runtime only after the application's
    /// removal succeeded, and then only because no other declaration
    /// names it (only Firefox names its runtime today).
    #[test]
    fn the_runtime_comes_first_and_goes_last() {
        for (verb, first) in [
            (Verb::Install, &["allocate firefox"][..]),
            (Verb::Update, &[][..]),
        ] {
            let mut host = Script::new(ok);
            operation(verb, &mut host).unwrap();
            let mut wanted = first.to_vec();
            wanted.extend([RUNTIME_UPDATE, "tda65536 update firefox --force"]);
            assert_eq!(host.runs, runs(&wanted));
            let mut host = Script::new(|run| match verb_of(run) {
                "runtime" => Err("offline".into()),
                _ => Ok(Vec::new()),
            });
            assert!(operation(verb, &mut host).unwrap_err().contains("offline"));
            let mut wanted = first.to_vec();
            wanted.push(RUNTIME_UPDATE);
            assert_eq!(host.runs, runs(&wanted));
        }
        let mut host = Script::new(ok);
        operation(Verb::Remove, &mut host).unwrap();
        assert_eq!(
            host.runs,
            runs(&["tda65536 remove firefox", RUNTIME_REMOVE])
        );
        // Firefox's own admission; no other application names the runtime.
        assert_eq!(host.admissions, 1);
        let mut host = Script::new(|run| match verb_of(run) {
            "remove" => Err("held".into()),
            _ => Ok(Vec::new()),
        });
        assert!(operation(Verb::Remove, &mut host).is_err());
        assert_eq!(host.runs, runs(&["tda65536 remove firefox"]));
        let mut host = Script::new(|run| match verb_of(run) {
            "runtime" => Err("busy".into()),
            _ => Ok(Vec::new()),
        });
        assert_eq!(
            operation(Verb::Remove, &mut host).unwrap_err(),
            "firefox is removed, but its runtime freedesktop-platform-25-08 is not: busy"
        );
        for verb in [Verb::DeleteData, Verb::Status] {
            let mut host = Script::new(|_| {
                Ok(
                    b"format=1\nname=firefox\nkind=flathub\ncurrent=none\nprevious=none\n\
                     checked=never\nrefused=none\nprogress=none\n"
                        .to_vec(),
                )
            });
            operation(verb, &mut host).unwrap();
            assert_eq!(host.runs.len(), 1, "{verb:?}");
            assert_eq!(
                host.runs.iter().map(|run| verb_of(run)).collect::<Vec<_>>(),
                [verb.word()]
            );
        }
        let others: Vec<&str> = flathub_app::FLATHUB_SOURCES
            .iter()
            .filter(|other| other.runtime.name == "freedesktop-platform-25-08")
            .map(|other| other.name)
            .collect();
        assert_eq!(others, ["firefox"]);
    }

    fn record(current: bool, progress: bool) -> Vec<u8> {
        let deploy = |exact_ref: &str| flathub_app::DeployStatus {
            row: flathub_app::Admitted {
                exact_ref: exact_ref.into(),
                commit: "a".repeat(64),
                timestamp: 1,
            },
            bytes: Some(1),
        };
        Status {
            name: "firefox".into(),
            cache: Some(flathub_app::CacheStatus {
                current: current.then(|| flathub_app::GenerationStatus {
                    generation: 1,
                    serves: true,
                    application: deploy("app/org.mozilla.firefox/x86_64/stable"),
                    runtime: deploy("runtime/org.freedesktop.Platform/x86_64/25.08"),
                }),
                previous: None,
                checked: None,
                refused: None,
                progress: progress.then(|| flathub_app::Progress::idle(flathub_app::Stage::Fetch)),
            }),
        }
        .render()
        .into_bytes()
    }

    /// A runtime is removed only when every other application naming it
    /// has no `current` and no writer, or is not in this image at all;
    /// anything root cannot read keeps it.
    #[test]
    fn a_runtime_stays_while_an_application_naming_it_is_installed_or_installing() {
        fn answer(run: &[String], status: Vec<u8>) -> Result<Vec<u8>, String> {
            match verb_of(run) {
                "status" => Ok(status),
                _ => Ok(Vec::new()),
            }
        }
        // Neither a current nor a writer: the runtime goes.
        let mut host = Script::new(|run| answer(run, record(false, false)));
        runtime_removal(&mut host).unwrap();
        assert_eq!(host.runs, runs(&[STATUS, RUNTIME_REMOVE]));
        // An install in flight, a current, or both: it stays.
        for answered in [
            (|run: &[String]| answer(run, record(false, true))) as fn(&[String]) -> _,
            |run| answer(run, record(true, false)),
            |run| answer(run, record(true, true)),
            |run| answer(run, b"format=1\nname=mail\nkind=image\n".to_vec()),
            |run| answer(run, b"not a record".to_vec()),
            |run| match verb_of(run) {
                "status" => Err("timed out".into()),
                _ => Ok(Vec::new()),
            },
        ] {
            let mut host = Script::new(answered);
            assert_eq!(
                runtime_removal(&mut host).unwrap_err(),
                "runtime freedesktop-platform-25-08 stays while an installed application names it"
            );
            assert_eq!(host.runs, runs(&[STATUS]));
        }
        // An application this image does not list has no account here: it
        // is neither admitted nor asked, and keeps nothing.
        let mut host = Script::new(|run| answer(run, record(true, true)));
        host.registry = Ok(vec!["mail".into()]);
        runtime_removal(&mut host).unwrap();
        assert_eq!(host.runs, runs(&[RUNTIME_REMOVE]));
        assert_eq!(host.admissions, 0);
        // A registry, account or admission root cannot read keeps it.
        let mut host = Script::new(ok);
        host.registry = Err("unreadable".into());
        assert!(runtime_removal(&mut host).is_err());
        assert!(host.runs.is_empty());
        let mut host = Script::new(ok);
        host.admitted = Err("application admission failed".into());
        assert!(runtime_removal(&mut host).is_err());
        assert!(host.runs.is_empty());
        assert_eq!(host.admissions, 1);
        let mut host = Script::new(ok);
        host.account = |_| Err("the account ledger is unreadable".into());
        assert!(runtime_removal(&mut host).is_err());
        assert!(host.runs.is_empty());
        assert_eq!(host.admissions, 0);
        // An application with no account was never installed here: it is
        // neither admitted nor asked, and keeps nothing.
        let mut host = Script::new(|run| answer(run, record(true, true)));
        host.account = |_| Ok(None);
        runtime_removal(&mut host).unwrap();
        assert_eq!(host.runs, runs(&[RUNTIME_REMOVE]));
        assert_eq!(host.admissions, 0);
    }

    /// Another Flathub application naming the same runtime keeps it only
    /// while it has an account and its cache holds something; one with no
    /// account is not installed, so removing the last installed
    /// application removes the runtime.
    #[test]
    fn a_sibling_without_an_account_keeps_no_runtime() {
        let firefox = flathub_app::flathub_source("firefox").unwrap();
        let photo = FlathubSource {
            name: "photo",
            application: "app/org.example.Photo/x86_64/stable",
            runtime: firefox.runtime,
            entry: "/app/bin/photo",
        };
        let sources = [firefox, photo];
        let far = Instant::now() + Duration::from_secs(60);
        let mut host = Script::new(|_| {
            let photo = String::from_utf8(record(true, false)).unwrap();
            Ok(photo.replace("name=firefox", "name=photo").into_bytes())
        });
        host.registry = Ok(vec!["firefox".into(), "photo".into()]);
        host.account = |name| Ok((name == "firefox").then_some(65536));
        assert!(!named_elsewhere(
            &mut host,
            &sources,
            firefox.runtime,
            Some("firefox"),
            far
        ));
        assert!(host.runs.is_empty());
        assert_eq!(host.admissions, 0);
        // With an account, its status decides.
        host.account = |_| Ok(Some(66560));
        host.admitted = Ok(66560);
        assert!(named_elsewhere(
            &mut host,
            &sources,
            firefox.runtime,
            Some("firefox"),
            far
        ));
        assert_eq!(host.runs, runs(&["tda66560 status photo"]));
    }

    /// An install allocates the account before anything runs, then fetches
    /// the runtime and the application as it; a failed allocation runs
    /// nothing. A removal keeps the account, so a reinstall runs as the
    /// same one.
    #[test]
    fn an_install_allocates_its_account_first_and_a_reinstall_reuses_it() {
        let mut host = Script::new(ok);
        host.account = |_| Ok(None);
        host.allocated = Ok(66560);
        operation(Verb::Install, &mut host).unwrap();
        assert_eq!(
            host.runs,
            runs(&[
                "allocate firefox",
                RUNTIME_UPDATE,
                "tda66560 update firefox --force"
            ])
        );
        assert_eq!(host.admissions, 0);
        let mut host = Script::new(ok);
        host.allocated = Err("td-firstboot allocate-application failed".into());
        assert!(operation(Verb::Install, &mut host).is_err());
        assert_eq!(host.runs, runs(&["allocate firefox"]));
        // Remove, then install again: the row stays, the uid with it.
        let mut host = Script::new(ok);
        host.account = |_| Ok(Some(66560));
        host.admitted = Ok(66560);
        host.allocated = Ok(66560);
        operation(Verb::Remove, &mut host).unwrap();
        operation(Verb::Install, &mut host).unwrap();
        assert_eq!(
            host.runs,
            runs(&[
                "tda66560 remove firefox",
                RUNTIME_REMOVE,
                "allocate firefox",
                RUNTIME_UPDATE,
                "tda66560 update firefox --force"
            ])
        );
    }

    /// With no account a Flathub application was never installed: its
    /// status is an empty cache's, answered without td-app, and every
    /// other verb but install refuses without running anything.
    #[test]
    fn an_application_without_an_account_is_not_installed() {
        let mut host = Script::new(ok);
        host.account = |_| Ok(None);
        assert_eq!(
            operation(Verb::Status, &mut host).unwrap(),
            b"format=1\nname=firefox\nkind=flathub\ncurrent=none\nprevious=none\n\
              checked=never\nrefused=none\nprogress=none\n"
        );
        for verb in [Verb::Update, Verb::Remove, Verb::DeleteData] {
            assert_eq!(
                operation(verb, &mut host).unwrap_err(),
                "firefox is not installed",
                "{verb:?}"
            );
        }
        assert!(host.runs.is_empty());
        assert_eq!(host.admissions, 0);
        // The record is the one an empty cache's td-app writes.
        let empty = not_installed_status("firefox");
        assert_eq!(checked_status("firefox", &empty).unwrap(), empty);
        // An account root cannot read is no answer at all.
        let mut host = Script::new(ok);
        host.account = |_| Err("undeclared".into());
        assert!(operation(Verb::Status, &mut host).is_err());
        assert!(host.runs.is_empty());
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
        assert!(output.len() <= stderr_tail::TAIL_BYTES);
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
