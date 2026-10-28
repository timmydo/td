//! Supervised bounded copy rounds; status is historical operational evidence.
use super::{arguments, copy_error, read_limits, Operation, Options};
use crate::postgres::{report_error, scrub_state::FileState, with_addresses_timeout};
use std::{
    ffi::OsString,
    io::{self, Write},
    os::unix::fs::DirBuilderExt,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use td_json::{json, Json};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    outbound_credentials,
    outbound_http::Client,
    postgres_input::Loaded,
    postgres_probe::{Mode, Request},
    postgres_replica_source::Source,
    replica_copy::{Local, Outcome, Remote, Worker},
};
const COMMAND: &str = "postgres-replica-follow";
const STATUS: &str = "postgres-replica-follow-status";
type Failure = (&'static str, &'static str);
struct Settings {
    copy: Options,
    directory: String,
    idle: u64,
}
fn settings(mut args: impl Iterator<Item = OsString>) -> Option<Settings> {
    let (mut directory, mut idle) = (None, None);
    let mut copy = Vec::new();
    while let Some(flag) = args.next() {
        let value = args.next()?;
        match flag.to_str()? {
            "--state-dir" if directory.is_none() => directory = Some(value.into_string().ok()?),
            "--idle-seconds" if idle.is_none() => {
                idle = Some(super::number(&value.into_string().ok()?)?);
            }
            "--state-dir" | "--idle-seconds" | "--steps" | "--timeout-seconds" => return None,
            _ if copy.len() < 24 => copy.extend([flag, value]),
            _ => return None,
        }
    }
    let idle = idle.unwrap_or(30);
    if !(1..=3600).contains(&idle) {
        return None;
    }
    Some(Settings {
        copy: arguments(Operation::Copy, copy.into_iter())?,
        directory: directory?,
        idle,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Starting,
    Copying,
    Idle,
    Backoff,
    Stopped,
}
impl Phase {
    fn text(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Copying => "copying",
            Self::Idle => "idle",
            Self::Backoff => "backoff",
            Self::Stopped => "stopped",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        [
            Self::Starting,
            Self::Copying,
            Self::Idle,
            Self::Backoff,
            Self::Stopped,
        ]
        .into_iter()
        .find(|phase| phase.text() == value)
    }
}
struct State {
    epoch: StoreEpoch,
    account: AccountId,
    source: Source,
    observed: u64,
    contact: u64,
    idle: u64,
    phase: Phase,
    error: Option<String>,
    applied: u64,
    downloaded: u64,
    local_sequence: u64,
    empty_sequence: Option<u64>,
    pending_mailboxes: u64,
}
impl State {
    fn new(options: &Options) -> Result<Self, &'static str> {
        let source = options.source.as_ref().ok_or("source-required")?;
        if source.epoch() == options.epoch {
            return Err("invalid-binding");
        }
        Ok(Self {
            epoch: options.epoch,
            account: options.account,
            source: Source::new(source.origin(), source.epoch()).map_err(|_| "source-invalid")?,
            observed: 0,
            contact: 0,
            idle: 0,
            phase: Phase::Starting,
            error: None,
            applied: 0,
            downloaded: 0,
            local_sequence: 0,
            empty_sequence: None,
            pending_mailboxes: 0,
        })
    }
    fn observe(&mut self, now: u64) -> Result<(), &'static str> {
        if now == 0 || now < self.observed {
            return Err("clock-backward");
        }
        self.observed = now;
        Ok(())
    }
    fn apply(&mut self, outcome: Outcome, now: u64) -> Result<(), &'static str> {
        self.observe(now)?;
        self.applied = self
            .applied
            .checked_add(outcome.applied as u64)
            .ok_or("counter-capacity")?;
        self.downloaded = self
            .downloaded
            .checked_add(outcome.downloaded_bytes)
            .ok_or("counter-capacity")?;
        self.contact = now;
        self.error = None;
        self.local_sequence = outcome.local_sequence;
        self.pending_mailboxes = outcome.pending_mailbox_removals as u64;
        self.phase = if let Some(empty) = outcome.empty_changes {
            self.idle = now;
            self.empty_sequence = Some(empty.sequence.number());
            Phase::Idle
        } else {
            Phase::Copying
        };
        Ok(())
    }
    fn json(&self) -> Json {
        json!({"version":1,"epoch":self.epoch.to_string(),"account":self.account.to_string(),"source_origin":self.source.origin(),"source_epoch":self.source.epoch().to_string(),"observed":self.observed,"last_contact":self.contact,"last_idle":self.idle,"phase":self.phase.text(),"error":self.error.clone(),"applied":self.applied,"downloaded_bytes":self.downloaded,"local_sequence":self.local_sequence,"empty_sequence":self.empty_sequence,"pending_mailbox_removals":self.pending_mailboxes})
    }
    fn parse(text: &str, epoch: StoreEpoch, account: AccountId) -> Result<Self, &'static str> {
        let value = td_json::parse(text).map_err(|_| "status-json")?;
        let number = |key| value.get(key).and_then(Json::as_u64).ok_or("status-value");
        let string = |key| value.get(key).and_then(Json::as_str).ok_or("status-value");
        if value.as_object().map(|fields| fields.len()) != Some(15)
            || number("version")? != 1
            || string("epoch")? != epoch.to_string()
            || string("account")? != account.to_string()
        {
            return Err("status-scope");
        }
        let source = Source::new(
            string("source_origin")?,
            StoreEpoch::parse(string("source_epoch")?).map_err(|_| "status-scope")?,
        )
        .map_err(|_| "status-scope")?;
        if source.epoch() == epoch || source.origin() != string("source_origin")? {
            return Err("status-scope");
        }
        let error = match value.get("error") {
            Some(Json::Null) => None,
            Some(Json::Str(error))
                if !error.is_empty()
                    && error.len() <= 64
                    && error
                        .bytes()
                        .all(|v| v.is_ascii_lowercase() || v.is_ascii_digit() || v == b'-') =>
            {
                Some(error.clone())
            }
            _ => return Err("status-value"),
        };
        let empty_sequence = match value.get("empty_sequence") {
            Some(Json::Null) => None,
            Some(value) => Some(value.as_u64().ok_or("status-value")?),
            None => return Err("status-value"),
        };
        let state = Self {
            epoch,
            account,
            source,
            observed: number("observed")?,
            contact: number("last_contact")?,
            idle: number("last_idle")?,
            phase: Phase::parse(string("phase")?).ok_or("status-value")?,
            error,
            applied: number("applied")?,
            downloaded: number("downloaded_bytes")?,
            local_sequence: number("local_sequence")?,
            empty_sequence,
            pending_mailboxes: number("pending_mailbox_removals")?,
        };
        if state.contact > state.observed
            || state.idle > state.contact
            || state.pending_mailboxes > 256
            || (state.idle == 0) != state.empty_sequence.is_none()
            || (state.phase == Phase::Backoff) != state.error.is_some()
        {
            return Err("status-value");
        }
        Ok(state)
    }
    fn recent(&self, now: u64, maximum_age: u64) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Copying)
            && now >= self.observed
            && self.contact != 0
            && now
                .checked_sub(self.contact)
                .is_some_and(|age| age <= maximum_age)
    }
}
fn now() -> Result<u64, Failure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| ("clock", "invalid-time"))
}
fn stopped() -> Result<bool, Failure> {
    crate::signal_sys::requested().map_err(|_| ("signals", "io"))
}
fn pause(seconds: u64) -> Result<bool, Failure> {
    let until = Instant::now() + Duration::from_secs(seconds);
    loop {
        if stopped()? {
            return Ok(true);
        }
        if Instant::now() >= until {
            return Ok(false);
        }
        std::thread::sleep(
            Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
        );
    }
}
fn round(options: &Options) -> Result<Outcome, Failure> {
    let loaded = Loaded::open(&options.connection, "replica-follow")
        .map_err(|error| ("bootstrap", error.code()))?;
    let credentials = outbound_credentials::Loaded::open(
        options.username.as_deref().unwrap_or(""),
        options.password.as_deref().unwrap_or(""),
        options.ca.as_deref(),
    )
    .map_err(|code| ("source-credentials", code))?;
    let source = options
        .source
        .as_ref()
        .ok_or(("arguments", "source-required"))?;
    let executable = std::env::current_exe().map_err(|_| ("resolver", "executable"))?;
    with_addresses_timeout(&loaded, Duration::from_secs(60), |request, end| {
        let request = Request {
            mode: Mode::Runtime,
            ..*request
        };
        let mut worker = Worker::new(
            Source::new(source.origin(), source.epoch())
                .map_err(|_| super::fail("source-invalid"))?,
            options.account,
            options.epoch,
            Local {
                request: &request,
                trust: &loaded.trust,
            },
        )
        .map_err(|_| super::fail("invalid-binding"))?;
        let result = worker
            .step(Remote {
                client: Client {
                    executable: &executable,
                    origin: source.origin(),
                    trust: &credentials.trust,
                    deadline: end,
                },
                authorization: &credentials.authorization,
                limits: read_limits(),
            })
            .map_err(copy_error);
        worker.retire();
        result
    })
}
fn save(file: &FileState, state: &State) -> Result<(), Failure> {
    file.save_text(&state.json().to_string())
        .map_err(|_| ("state", "status-write-failed"))
}
fn report(name: &str, state: &State, recent: bool) -> io::Result<()> {
    writeln!(
        io::stdout().lock(),
        "{}",
        json!({"schema":1,"command":name,"status":"ok","historical":true,"recent_success":recent,"copy_complete":false,"recovery_verified":false,"observation":state.json()})
    )
}
fn observer(directory: &str) -> Result<FileState, &'static str> {
    td_mta::store_fs::PrivateRoot::open(directory).map_err(|_| "status-root")?;
    let path = std::path::Path::new(directory).join("observer");
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(_) => return Err("status-observer-create"),
    }
    FileState::open(path.to_str().ok_or("status-path")?, Duration::ZERO)
        .map_err(|_| "status-observer-lock")
}
fn observe_status(
    mut state: State,
    previous: Option<&str>,
    time: u64,
) -> Result<State, &'static str> {
    if let Some(previous) = previous {
        let previous = State::parse(previous, state.epoch, state.account)?;
        if previous.source != state.source {
            return Err("status-source-mismatch");
        }
        if time < previous.observed {
            return Err("clock-backward");
        }
    }
    state.observe(time)?;
    Ok(state)
}
fn run_inner(settings: Settings) -> Result<(), Failure> {
    let file = FileState::open_follow(&settings.directory)
        .map_err(|_| ("state", "status-root-or-lock"))?;
    let mut state = match file
        .load_text()
        .map_err(|_| ("state", "status-read-failed"))?
    {
        Some(text) => State::parse(&text, settings.copy.epoch, settings.copy.account)
            .map_err(|code| ("state", code))?,
        None => State::new(&settings.copy).map_err(|code| ("state", code))?,
    };
    if Some(&state.source) != settings.copy.source.as_ref() {
        return Err(("state", "status-source-mismatch"));
    }
    state.observe(now()?).map_err(|code| ("clock", code))?;
    state.phase = Phase::Starting;
    state.error = None;
    save(&file, &state)?;
    crate::signal_sys::block().map_err(|_| ("signals", "io"))?;
    let mut retry = 5u64;
    let mut emitted = None::<Instant>;
    while !stopped()? {
        let delay = match round(&settings.copy) {
            Ok(outcome) => {
                state
                    .apply(outcome, now()?)
                    .map_err(|code| ("state", code))?;
                retry = 5;
                if state.phase == Phase::Idle {
                    settings.idle
                } else {
                    0
                }
            }
            Err((_, code)) => {
                state.observe(now()?).map_err(|code| ("clock", code))?;
                state.phase = Phase::Backoff;
                state.error = Some(code.to_owned());
                let delay = retry;
                retry = retry.saturating_mul(2).min(300);
                delay
            }
        };
        save(&file, &state)?;
        if emitted.is_none_or(|last| last.elapsed() >= Duration::from_secs(60)) {
            report(
                COMMAND,
                &state,
                state.recent(now()?, settings.idle.max(120)),
            )
            .map_err(|_| ("output", "status-write-failed"))?;
            emitted = Some(Instant::now());
        }
        if pause(delay)? {
            break;
        }
    }
    state.observe(now()?).map_err(|code| ("clock", code))?;
    state.phase = Phase::Stopped;
    state.error = None;
    save(&file, &state)?;
    report(COMMAND, &state, false).map_err(|_| ("output", "status-write-failed"))
}
pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(settings) = settings(args) else {
        report_error(COMMAND, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    match run_inner(settings) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err((stage, code)) => {
            report_error(COMMAND, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
pub(super) fn status(mut args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let parse = (|| {
        let mut values = std::collections::BTreeMap::new();
        while let Some(flag) = args.next() {
            let flag = flag.into_string().ok()?;
            if !matches!(
                flag.as_str(),
                "--state-dir" | "--epoch" | "--account" | "--maximum-age-seconds"
            ) || values
                .insert(flag, args.next()?.into_string().ok()?)
                .is_some()
            {
                return None;
            }
        }
        let epoch = StoreEpoch::parse(values.get("--epoch")?).ok()?;
        let account = AccountId::parse(values.get("--account")?).ok()?;
        let age = super::number(
            values
                .get("--maximum-age-seconds")
                .map(String::as_str)
                .unwrap_or("120"),
        )?;
        if !(1..=86400).contains(&age) {
            return None;
        }
        Some((values.remove("--state-dir")?, epoch, account, age))
    })();
    let Some((directory, epoch, account, age)) = parse else {
        report_error(STATUS, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let result = (|| {
        let observer = observer(&directory)?;
        let previous = observer.load_text().map_err(|_| "status-observer-read")?;
        let text = FileState::read_snapshot(&directory)
            .map_err(|_| "status-read-failed")?
            .ok_or("status-missing")?;
        let state = State::parse(&text, epoch, account)?;
        let time = now().map_err(|(_, code)| code)?;
        let state = observe_status(state, previous.as_deref(), time)?;
        observer
            .save_text(&state.json().to_string())
            .map_err(|_| "status-observer-write")?;
        Ok::<_, &'static str>((state.recent(time, age), state))
    })();
    match result {
        Ok((recent, state)) => {
            report(STATUS, &state, recent)?;
            Ok(if recent {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(code) => {
            report_error(STATUS, "state", code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn arguments() -> Vec<OsString> {
        [
            "--connection-file",
            "private/local",
            "--epoch",
            "11111111111111111111111111111111",
            "--account",
            "22222222222222222222222222222222",
            "--source-origin",
            "https://source.example.test",
            "--source-epoch",
            "33333333333333333333333333333333",
            "--username",
            "user",
            "--password-file",
            "private/password",
            "--state-dir",
            "private/status",
        ]
        .map(OsString::from)
        .to_vec()
    }
    #[test]
    fn supervised_rounds_keep_fixed_work_budgets_and_explicit_status_scope() {
        let options = settings(arguments().into_iter()).unwrap();
        assert_eq!(options.idle, 30);
        assert_eq!(options.copy.steps, 1);
        assert_eq!(options.copy.timeout, 60);
        for extra in [
            vec!["--steps", "2"],
            vec!["--timeout-seconds", "3600"],
            vec!["--idle-seconds", "0"],
            vec!["--idle-seconds", "3601"],
            vec!["--state-dir", "other"],
        ] {
            let mut args = arguments();
            args.extend(extra.into_iter().map(OsString::from));
            assert!(settings(args.into_iter()).is_none());
        }
    }
    #[test]
    fn observations_keep_freshness_separate_from_completeness_and_refuse_foreign_state() {
        let settings = settings(arguments().into_iter()).unwrap();
        let mut state = State::new(&settings.copy).unwrap();
        state.observe(100).unwrap();
        assert!(!state.recent(100, 120));
        let binding = td_mta::sync::DataState {
            epoch: state.source.epoch(),
            account: state.account,
            kind: td_mta::sync::DataType::Email,
            sequence: td_mta::format::Sequence::from_u64(7),
        };
        let outcome = Outcome {
            applied: 2,
            changed: 2,
            downloaded_bytes: 1000,
            advanced: true,
            empty_changes: Some(binding),
            mailbox_state: binding,
            pending_mailbox_removals: 0,
            local_sequence: 9,
        };
        state.apply(outcome, 110).unwrap();
        assert!(state.recent(120, 120));
        assert!(!state.recent(109, 120));
        assert!(!state.recent(231, 120));
        let snapshot = state.json().to_string();
        let observed = observe_status(
            State::parse(&snapshot, state.epoch, state.account).unwrap(),
            None,
            1000,
        )
        .unwrap();
        assert!(!observed.recent(1000, 120));
        assert!(observe_status(
            State::parse(&snapshot, state.epoch, state.account).unwrap(),
            Some(&observed.json().to_string()),
            120
        )
        .is_err());
        let restored = State::parse(&state.json().to_string(), state.epoch, state.account).unwrap();
        assert_eq!(restored.downloaded, 1000);
        assert_eq!(restored.empty_sequence, Some(7));
        assert!(State::parse(
            &state.json().to_string(),
            state.source.epoch(),
            state.account
        )
        .is_err());
        assert_eq!(state.observe(109), Err("clock-backward"));
        state.phase = Phase::Backoff;
        state.error = Some("invalid-utf8".into());
        assert!(State::parse(&state.json().to_string(), state.epoch, state.account).is_ok());
        assert!(!state.recent(120, 120));
        state.error = None;
        assert!(State::parse(&state.json().to_string(), state.epoch, state.account).is_err());
        state.phase = Phase::Stopped;
        assert!(!state.recent(120, 120));
    }
}
