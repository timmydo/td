//! At most one concurrent secret operation per authenticated generation.

use crate::consent::{Recovery, Request as Description, Role};
use crate::inspection::{Event as InspectionEvent, Inspection};
use crate::login::{Login, Pin, Selection};
use crate::rollback::{Refusal, Rollback};
use crate::set_hostname::Change;
use crate::unlock::{Event, Unlock};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CLEANUP_TIME: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Prepare,
    Poll,
    Inspect,
    Write,
    Install,
    Begin(Role),
    Enroll(Recovery),
    Presented(Description),
    Commit(Description),
    Cancel([u8; 32]),
    /// `1a`: the human's login state.
    LoginState,
    /// `1b`: a login-key operation.
    Login(Selection),
    /// `1c`: the presented PIN step's description and its PIN.
    Pin(Box<Description>, Pin),
    /// `1d`: roll back to the previous deployment.
    Rollback,
    /// `1e`: the queued hostname change.
    Hostname,
    /// `1f`: whether a hostname change waits, and the intake's backoff.
    HostnameState,
}

impl Request {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [0x10] => Ok(Self::Prepare),
            [0x11] => Ok(Self::Poll),
            [0x17] => Ok(Self::Inspect),
            [0x18] => Ok(Self::Write),
            [0x19] => Ok(Self::Install),
            [0x1a] => Ok(Self::LoginState),
            [0x1d] => Ok(Self::Rollback),
            [0x1e] => Ok(Self::Hostname),
            [0x1f] => Ok(Self::HostnameState),
            [0x12, 1] => Ok(Self::Begin(Role::Primary)),
            [0x12, 2] => Ok(Self::Begin(Role::Recovery)),
            [0x16, 0] => Ok(Self::Enroll(Recovery::Unrecoverable)),
            [0x16, 1] => Ok(Self::Enroll(Recovery::SecondToken)),
            [0x13, rest @ ..] => Ok(Self::Presented(Description::decode(rest)?)),
            [0x14, rest @ ..] => Ok(Self::Commit(Description::decode(rest)?)),
            [0x15, rest @ ..] if rest.len() == 32 => {
                let nonce = rest.try_into().map_err(|_| "invalid cancellation nonce")?;
                if nonce == [0; 32] {
                    return Err("zero cancellation nonce".into());
                }
                Ok(Self::Cancel(nonce))
            }
            [0x1b, rest @ ..] => Ok(Self::Login(Selection::decode(rest)?)),
            [0x1c, length, rest @ ..] => {
                let (description, pin) = rest
                    .split_at_checked(usize::from(*length))
                    .ok_or("truncated login PIN request")?;
                Ok(Self::Pin(
                    Box::new(Description::decode(description)?),
                    Pin::new(pin)?,
                ))
            }
            _ => Err("invalid secret session request".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Start {
    Unlock(Role),
    Enroll(Recovery),
}

impl Start {
    fn begin(owner: u32, start: Self) -> Result<Unlock, String> {
        match start {
            Self::Unlock(role) => Unlock::start(owner, role),
            Self::Enroll(recovery) => Unlock::start_enrollment(owner, recovery),
        }
    }
}

enum Active {
    Secret(Box<Unlock>),
    Install(Box<crate::deployment::Installation>),
    DiskInstall(Box<crate::disk_install::Installation>),
    /// A login-key worker shares the one operation slot.
    Login(Box<Login>),
    Rollback(Box<Rollback>),
    Hostname(Box<Change>),
}
impl Active {
    /// None only for a login operation awaiting its worker's baseline.
    fn description(&self) -> Option<&Description> {
        match self {
            Self::Secret(op) => Some(op.request()),
            Self::Install(op) => Some(op.request()),
            Self::DiskInstall(op) => Some(op.request()),
            Self::Login(op) => op.request(),
            Self::Rollback(op) => Some(op.request()),
            Self::Hostname(op) => Some(op.request()),
        }
    }
    fn nonce(&self) -> &[u8; 32] {
        match self {
            Self::Secret(op) => op.request().nonce(),
            Self::Install(op) => op.request().nonce(),
            Self::DiskInstall(op) => op.request().nonce(),
            Self::Login(op) => op.nonce(),
            Self::Rollback(op) => op.request().nonce(),
            Self::Hostname(op) => op.request().nonce(),
        }
    }
    fn presented(&mut self, request: &Description) -> Result<(), String> {
        match self {
            Self::Secret(op) => op.presented(request),
            Self::Install(op) => op.presented(request),
            Self::DiskInstall(op) => op.presented(request),
            Self::Login(op) => op.presented(request),
            Self::Rollback(op) => op.presented(request),
            Self::Hostname(op) => op.presented(request),
        }
    }
    fn commit(&mut self, request: &Description) -> Result<(), String> {
        match self {
            Self::Secret(op) => op.commit(request),
            Self::Install(op) => op.commit(request),
            Self::DiskInstall(op) => op.commit(request),
            Self::Login(op) => op.commit(request),
            Self::Rollback(op) => op.commit(request),
            Self::Hostname(op) => op.commit(request),
        }
    }
    fn cancel(&mut self, reason: &str) -> Result<(), String> {
        match self {
            Self::Secret(op) => op.cancel(reason),
            Self::Install(op) => op.cancel(reason),
            Self::DiskInstall(op) => op.cancel(reason),
            // Every login cancellation is the person's: kill, reap, no relock.
            Self::Login(op) => op.cancel(),
            Self::Rollback(op) => op.cancel(),
            Self::Hostname(op) => op.cancel(),
        }
    }
    fn poll(&mut self) -> Result<Event, String> {
        match self {
            Self::Secret(op) => op.poll(),
            Self::Install(op) => op.poll(),
            Self::DiskInstall(op) => op.poll(),
            Self::Login(op) => op.poll(),
            Self::Rollback(op) => op.poll(),
            Self::Hostname(op) => op.poll(),
        }
    }
    fn reap_for_teardown(self) -> Result<(), String> {
        match self {
            Self::Secret(op) => op.reap_for_teardown(),
            Self::Install(op) => op.reap_for_teardown(),
            // The setup intake owns the service and stops it at teardown.
            Self::DiskInstall(_) => Ok(()),
            Self::Login(op) => op.reap_for_teardown(),
            Self::Rollback(op) => op.reap_for_teardown(),
            // Root saved the name itself: there is no child.
            Self::Hostname(_) => Ok(()),
        }
    }
}

struct Cleanup {
    child: Option<Child>,
    deadline: Instant,
    expired: bool,
    terminal: Option<Result<bool, String>>,
}

fn cleanup_command(owner: u32) -> Command {
    let mut command = Command::new("/bin/td-secret");
    command
        .args(["lock-session", "--uid", &owner.to_string()])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

impl Cleanup {
    fn start(mut command: Command) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(CLEANUP_TIME)
            .ok_or("session cleanup deadline overflow")?;
        let child = command
            .spawn()
            .map_err(|e| format!("start session cleanup: {e}"))?;
        Ok(Self {
            child: Some(child),
            deadline,
            expired: false,
            terminal: None,
        })
    }

    fn poll(&mut self) -> Result<bool, String> {
        if let Some(result) = &self.terminal {
            return result.clone();
        }
        let child = self.child.as_mut().ok_or("missing session cleanup child")?;
        if Instant::now() >= self.deadline && !self.expired {
            child
                .kill()
                .map_err(|e| format!("stop session cleanup: {e}"))?;
            self.expired = true;
        }
        let Some(status) = child
            .try_wait()
            .map_err(|e| format!("reap session cleanup: {e}"))?
        else {
            return Ok(false);
        };
        self.child = None;
        let result = if !status.success() || self.expired || Instant::now() >= self.deadline {
            Err("session cleanup failed; end authority generation".into())
        } else {
            Ok(true)
        };
        self.terminal = Some(result.clone());
        result
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) struct Session {
    owner: u32,
    intake: Option<crate::secret_intake::Intake>,
    writing: bool,
    installations: Option<crate::deployment::Intake>,
    installing: bool,
    /// On a live boot, in place of the update intake.
    setup: Option<crate::disk_install::Intake>,
    /// Beside the update intake; a live boot binds none.
    hostnames: Option<crate::set_hostname::Intake>,
    renaming: bool,
    activated: bool,
    prepared: bool,
    cleanup: Option<Cleanup>,
    operation: Option<Active>,
    event: Option<Event>,
    inspection: Option<Inspection>,
    inspection_event: Option<InspectionEvent>,
    login_state: crate::login_status::Status,
}

impl Session {
    /// Construct only after the root launch admission has succeeded, with
    /// the primary account's name.
    pub fn new(owner: u32, username: &str) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported secret session owner".into());
        }
        Ok(Self {
            owner,
            intake: None,
            writing: false,
            installations: None,
            installing: false,
            setup: None,
            hostnames: None,
            renaming: false,
            activated: false,
            prepared: false,
            cleanup: None,
            operation: None,
            event: None,
            inspection: None,
            inspection_event: None,
            login_state: crate::login_status::Status::new(owner, username)?,
        })
    }

    pub fn answer(&mut self, request: Request) -> Result<Vec<u8>, String> {
        if request == Request::Prepare {
            if self.activated {
                return Err("secret session already prepared or preparing".into());
            }
            self.intake = Some(crate::secret_intake::Intake::bind(self.owner)?);
            // A live boot installs disks, never updates itself.
            if crate::disk_install::live_boot()? {
                self.setup = Some(crate::disk_install::Intake::bind(self.owner)?);
            } else {
                self.installations = Some(crate::deployment::Intake::bind(self.owner)?);
                self.hostnames = Some(crate::set_hostname::Intake::bind(self.owner)?);
            }
        }
        let answer = self.answer_with(request, cleanup_command, Start::begin);
        self.send_disk_answer();
        answer
    }

    /// Forwards the disk installation's answer to its service.
    fn send_disk_answer(&mut self) {
        if let (Some(Active::DiskInstall(operation)), Some(setup)) =
            (&mut self.operation, &mut self.setup)
        {
            if let Some(answer) = operation.take_answer() {
                if setup.answer(answer).is_err() {
                    operation.fate(answer.nonce(), crate::disk_install::Fate::Ended);
                }
            }
        }
    }

    fn answer_with(
        &mut self,
        request: Request,
        cleanup: impl FnOnce(u32) -> Command,
        begin: impl FnOnce(u32, Start) -> Result<Unlock, String>,
    ) -> Result<Vec<u8>, String> {
        match request {
            Request::Prepare => {
                if self.activated {
                    return Err("secret session already prepared or preparing".into());
                }
                // Even a failed spawn leaves generation teardown responsible.
                self.activated = true;
                self.cleanup = Some(Cleanup::start(cleanup(self.owner))?);
                Ok(vec![0x90])
            }
            Request::Poll => {
                self.tick()?;
                self.reply()
            }
            Request::Inspect => self.inspect_with(Inspection::start),
            Request::Write => self.begin_write(),
            Request::Install => self.begin_install(),
            Request::Rollback => self.begin_rollback(
                crate::elevation::Table::load,
                Path::new(crate::login_tier::VOLUME),
            ),
            Request::Hostname => self.begin_hostname(crate::elevation::Table::load),
            // Beside any operation: the menu's row reads it.
            Request::HostnameState => {
                self.tick()?;
                Ok(self
                    .hostnames
                    .as_mut()
                    .map_or_else(|| vec![0x9f, 0, 0], |intake| intake.state()))
            }
            // Beside any operation, which it neither needs nor takes. Only
            // a live boot binds the setup intake.
            Request::LoginState if self.prepared => self.login_state.answer(self.setup.is_some()),
            Request::LoginState => Err("login state before session preparation".into()),
            Request::Begin(role) => self.begin(Start::Unlock(role), begin),
            Request::Enroll(recovery) => self.begin(Start::Enroll(recovery), begin),
            Request::Login(selection) => {
                self.begin_login(selection, crate::login::WRITES, Login::start)
            }
            Request::Pin(description, pin) => {
                let Some(Active::Login(operation)) = &mut self.operation else {
                    return Err("no login operation takes a PIN".into());
                };
                if !operation.pin(&description, pin)? {
                    return Ok(vec![0x9c, 1]);
                }
                self.event = Some(Event::Waiting);
                Ok(vec![0x9c, 0])
            }
            Request::Presented(description) => {
                self.operation
                    .as_mut()
                    .ok_or("no active operation")?
                    .presented(&description)?;
                self.event = Some(Event::Waiting);
                Ok(vec![0x93])
            }
            Request::Commit(description) => {
                if let Some(Active::DiskInstall(operation)) = &mut self.operation {
                    // The installer may have left, or the service ended the
                    // review, while the prompt waited for Enter: commit
                    // consents to nothing. A duplicate commit still refuses.
                    if let Some(setup) = &mut self.setup {
                        for (nonce, fate) in setup.tick() {
                            operation.fate(&nonce, fate);
                        }
                    }
                    if operation.ended() {
                        if operation.request() != &description {
                            return Err("stale installation consent".into());
                        }
                        self.event = Some(operation.poll()?);
                        return Ok(vec![0x94]);
                    }
                }
                if self.installing {
                    let intake = self
                        .installations
                        .as_mut()
                        .ok_or("missing installation intake")?;
                    intake.tick();
                    if !intake.selected_alive() {
                        self.operation
                            .as_mut()
                            .ok_or("no installation")?
                            .cancel("installation requester disappeared")?;
                        self.event = Some(Event::Waiting);
                        return Ok(vec![0x94]);
                    }
                }
                if self.renaming {
                    let intake = self.hostnames.as_mut().ok_or("missing hostname intake")?;
                    intake.tick();
                    if !intake.selected_alive() {
                        self.operation
                            .as_mut()
                            .ok_or("no hostname change")?
                            .cancel("hostname requester disappeared")?;
                        self.event = Some(Event::Waiting);
                        return Ok(vec![0x94]);
                    }
                }
                if self.writing {
                    self.intake
                        .as_mut()
                        .ok_or("missing credential intake")?
                        .tick();
                    if !self
                        .intake
                        .as_ref()
                        .is_some_and(|intake| intake.selected_alive())
                    {
                        self.operation
                            .as_mut()
                            .ok_or("no write operation")?
                            .cancel("credential requester disappeared")?;
                        self.event = Some(Event::Waiting);
                        return Ok(vec![0x94]);
                    }
                }
                self.operation
                    .as_mut()
                    .ok_or("no active operation")?
                    .commit(&description)?;
                if self.installing {
                    self.installations
                        .as_mut()
                        .ok_or("missing installation intake")?
                        .committed();
                }
                if self.renaming {
                    self.hostnames
                        .as_mut()
                        .ok_or("missing hostname intake")?
                        .committed();
                }
                self.event = Some(Event::Waiting);
                Ok(vec![0x94])
            }
            Request::Cancel(nonce) => {
                let operation = self.operation.as_mut().ok_or("no active operation")?;
                if operation.nonce() != &nonce {
                    return Err("stale consent cancellation".into());
                }
                match &self.event {
                    Some(Event::Complete) => Ok(vec![0x95, 1]),
                    Some(Event::Failed(_) | Event::Ended(_)) => Ok(vec![0x95, 2]),
                    _ => {
                        operation.cancel("physical attention cancelled")?;
                        self.event = Some(Event::Waiting);
                        Ok(vec![0x95, 0])
                    }
                }
            }
        }
    }

    fn begin_install(&mut self) -> Result<Vec<u8>, String> {
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Ok(vec![0x99, 0]);
        }
        if let Some(setup) = &mut self.setup {
            let Ok(request) = setup.select() else {
                return Ok(vec![0x99, 0]);
            };
            let nonce = *request.nonce();
            let operation = match crate::disk_install::Installation::start(request) {
                Ok(operation) => operation,
                Err(_) => {
                    let _ = setup.answer(crate::installation_consent::Answer::Declined(
                        nonce,
                        crate::installation_consent::NoConsent::Unavailable,
                    ));
                    return Ok(vec![0x99, 0]);
                }
            };
            let mut answer = vec![0x92];
            answer.extend_from_slice(&operation.request().encode());
            self.operation = Some(Active::DiskInstall(Box::new(operation)));
            self.event = Some(Event::Waiting);
            return Ok(answer);
        }
        let intake = self
            .installations
            .as_mut()
            .ok_or("missing installation intake")?;
        let ready = match intake.select() {
            Ok(ready) => ready,
            Err(_) => return Ok(vec![0x99, 0]),
        };
        // Amendment 8: on an enrolled or unavailable machine, a deployment
        // that cannot read the record never becomes current. Refused before
        // any description, its requester's completion byte is 00.
        if !self.login_state.admits(|| ready.reads()) {
            intake.finish(false);
            return Ok(vec![0x99, 1]);
        }
        let operation = match crate::deployment::Installation::start(self.owner, ready) {
            Ok(operation) => operation,
            Err(_) => {
                intake.finish(false);
                return Ok(vec![0x99, 0]);
            }
        };
        let mut answer = vec![0x92];
        answer.extend_from_slice(&operation.request().encode());
        self.operation = Some(Active::Install(Box::new(operation)));
        self.installing = true;
        self.event = Some(Event::Waiting);
        Ok(answer)
    }

    /// Request `1d`: the principal table, then both selectors through the
    /// held volume, before any description exists.
    fn begin_rollback(
        &mut self,
        table: impl FnOnce() -> Result<crate::elevation::Table, String>,
        volume: &Path,
    ) -> Result<Vec<u8>, String> {
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Ok(Refusal::Busy.answer());
        }
        let selectors = match crate::rollback::admit(self.owner, table(), volume) {
            Ok(selectors) => selectors,
            Err(refusal) => return Ok(refusal.answer()),
        };
        let operation = Rollback::start(self.owner, selectors)?;
        let mut answer = vec![0x92];
        answer.extend_from_slice(&operation.request().encode());
        self.operation = Some(Active::Rollback(Box::new(operation)));
        self.event = Some(Event::Waiting);
        Ok(answer)
    }

    /// Request `1e`: the principal table for the owner, then the queued
    /// request and the table for its requester, before any description.
    fn begin_hostname(
        &mut self,
        table: impl FnOnce() -> Result<crate::elevation::Table, String>,
    ) -> Result<Vec<u8>, String> {
        use crate::elevation::Operation::SetHostname;
        use crate::set_hostname::Refusal;
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Ok(Refusal::Nothing.answer());
        }
        let Some(intake) = &mut self.hostnames else {
            return Ok(Refusal::Nothing.answer());
        };
        let Ok(table) = table() else {
            return Ok(Refusal::Principal.answer());
        };
        if !table.grants(self.owner, SetHostname) {
            return Ok(Refusal::Principal.answer());
        }
        let Ok(ready) = intake.select() else {
            return Ok(Refusal::Nothing.answer());
        };
        if !table.grants(ready.requester(), SetHostname) {
            intake.finish(false);
            return Ok(Refusal::Principal.answer());
        }
        let operation = match intake.describe(self.owner, ready) {
            Ok(operation) => operation,
            Err(_) => {
                intake.finish(false);
                return Ok(Refusal::Nothing.answer());
            }
        };
        let mut answer = vec![0x92];
        answer.extend_from_slice(&operation.request().encode());
        self.operation = Some(Active::Hostname(Box::new(operation)));
        self.renaming = true;
        self.event = Some(Event::Waiting);
        Ok(answer)
    }

    fn begin_write(&mut self) -> Result<Vec<u8>, String> {
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Ok(vec![0x98, 0]);
        }
        let intake = self.intake.as_mut().ok_or("missing credential intake")?;
        let (description, credential) = match intake.select() {
            Ok(value) => value,
            Err(_) => return Ok(vec![0x98, 0]),
        };
        let operation = match Unlock::start_write(self.owner, description, credential) {
            Ok(operation) => operation,
            Err(_) => {
                intake.finish(false);
                return Ok(vec![0x98, 0]);
            }
        };
        let mut answer = vec![0x92];
        answer.extend_from_slice(&operation.request().encode());
        self.operation = Some(Active::Secret(Box::new(operation)));
        self.writing = true;
        self.event = Some(Event::Waiting);
        Ok(answer)
    }

    fn inspect_with(
        &mut self,
        start: impl FnOnce(u32) -> Result<Inspection, String>,
    ) -> Result<Vec<u8>, String> {
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Err("secret session is not ready for inspection".into());
        }
        self.inspection = Some(start(self.owner)?);
        self.inspection_event = Some(InspectionEvent::Waiting);
        Ok(vec![0x97])
    }

    fn begin(
        &mut self,
        start: Start,
        begin: impl FnOnce(u32, Start) -> Result<Unlock, String>,
    ) -> Result<Vec<u8>, String> {
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Err("secret session is not ready for a new operation".into());
        }
        let operation = begin(self.owner, start)?;
        let mut answer = vec![0x92];
        answer.extend_from_slice(&operation.request().encode());
        self.operation = Some(Active::Secret(Box::new(operation)));
        self.event = Some(Event::Waiting);
        Ok(answer)
    }

    /// A login operation from the worker's baseline on. Until activation a
    /// production build refuses every one that may write (`active` false)
    /// before any worker starts.
    fn begin_login(
        &mut self,
        selection: Selection,
        active: bool,
        start: impl FnOnce(u32, Selection) -> Result<Login, String>,
    ) -> Result<Vec<u8>, String> {
        if selection.writes() && !active {
            return Ok(vec![0x9b, 0]);
        }
        if !self.prepared
            || self.cleanup.is_some()
            || self.operation.is_some()
            || self.inspection.is_some()
        {
            return Err("secret session is not ready for a login operation".into());
        }
        let operation = start(self.owner, selection)?;
        let mut answer = vec![0x9b, 1];
        answer.extend_from_slice(operation.nonce());
        self.operation = Some(Active::Login(Box::new(operation)));
        self.event = Some(Event::Waiting);
        Ok(answer)
    }

    /// Terminal traffic also advances watchdogs without consuming events.
    pub fn tick(&mut self) -> Result<(), String> {
        if let Some(intake) = &mut self.intake {
            intake.tick();
        }
        if let Some(intake) = &mut self.installations {
            intake.tick();
        }
        if let Some(intake) = &mut self.hostnames {
            intake.tick();
        }
        if let Some(setup) = &mut self.setup {
            for (nonce, fate) in setup.tick() {
                if let Some(Active::DiskInstall(operation)) = &mut self.operation {
                    operation.fate(&nonce, fate);
                }
            }
        }
        if self.installing
            && !self
                .installations
                .as_ref()
                .is_some_and(|intake| intake.selected_alive())
        {
            if let Some(operation) = &mut self.operation {
                operation.cancel("installation requester disappeared")?;
            }
        }
        if self.writing
            && !self
                .intake
                .as_ref()
                .is_some_and(|intake| intake.selected_alive())
        {
            if let Some(operation) = &mut self.operation {
                operation.cancel("credential requester disappeared")?;
            }
        }
        if self.renaming
            && !self
                .hostnames
                .as_ref()
                .is_some_and(|intake| intake.selected_alive())
        {
            if let Some(operation) = &mut self.operation {
                operation.cancel("hostname requester disappeared")?;
            }
        }
        if let Some(cleanup) = &mut self.cleanup {
            if cleanup.poll()? {
                self.cleanup = None;
                self.prepared = true;
            }
        }
        if let Some(operation) = &mut self.operation {
            self.event = Some(operation.poll()?);
        }
        self.send_disk_answer();
        if let Some(inspection) = &mut self.inspection {
            self.inspection_event = Some(inspection.poll()?);
        }
        self.login_state.tick();
        Ok(())
    }

    fn reply(&mut self) -> Result<Vec<u8>, String> {
        if self.inspection.is_some() {
            let event = self.inspection_event.ok_or("missing inspection event")?;
            let answer = match event {
                InspectionEvent::Waiting => vec![0x91, 8],
                InspectionEvent::State(state) => vec![0x91, 9, state.tag()],
                InspectionEvent::Unavailable => vec![0x91, 10],
            };
            if event != InspectionEvent::Waiting {
                self.inspection = None;
                self.inspection_event = None;
            }
            return Ok(answer);
        }
        let Some(operation) = &self.operation else {
            return Ok(vec![
                0x91,
                if self.prepared {
                    2
                } else if self.activated {
                    1
                } else {
                    0
                },
            ]);
        };
        let mut answer = vec![0x91];
        let event = self
            .event
            .as_ref()
            .ok_or("missing secret operation event")?;
        let description = operation.description();
        match (event, description) {
            // A login operation before its worker's baseline.
            (Event::Waiting, None) => answer.push(0x0b),
            (Event::Waiting, Some(_)) => answer.push(3),
            (Event::Present(_), _) => answer.push(4),
            (Event::Commit(_), _) => answer.push(5),
            (Event::Complete, _) => answer.push(6),
            (Event::Failed(_), _) => answer.push(7),
            (Event::Pin(_), _) => answer.push(0x0c),
            (Event::Ended(end), _) => answer.extend_from_slice(&[
                if end.uncertain { 0x0e } else { 0x0d },
                end.kind,
                end.detail,
            ]),
        }
        if let Some(description) = description {
            answer.extend_from_slice(&description.encode());
        }
        if matches!(event, Event::Complete | Event::Failed(_) | Event::Ended(_)) {
            if self.writing {
                if let Some(intake) = &mut self.intake {
                    intake.finish(matches!(event, Event::Complete));
                }
                self.writing = false;
            }
            if self.installing {
                if let Some(intake) = &mut self.installations {
                    intake.finish(matches!(event, Event::Complete));
                }
                self.installing = false;
            }
            if self.renaming {
                if let Some(intake) = &mut self.hostnames {
                    intake.finish(matches!(event, Event::Complete));
                }
                self.renaming = false;
            }
            // However it ended, the next `1a` reads the login state afresh.
            if matches!(operation, Active::Login(_)) {
                self.login_state.operation_ended();
            }
            self.operation = None;
            self.event = None;
        }
        Ok(answer)
    }

    /// The peer is already failed. Never admit another request during teardown.
    pub fn close(&mut self) -> Result<(), String> {
        self.close_with(cleanup_command)
    }

    fn close_with(&mut self, command: impl FnOnce(u32) -> Command) -> Result<(), String> {
        if !self.activated {
            return Ok(());
        }
        if let Some(operation) = self.operation.take() {
            // The prior operation may retain a fatal cleanup result. Only
            // proven child death, not replaying that result, permits relocking.
            operation.reap_for_teardown()?;
        }
        self.intake = None;
        self.installations = None;
        self.hostnames = None;
        self.renaming = false;
        // Stops the installation service, as an update helper is stopped.
        self.setup = None;
        if let Some(inspection) = self.inspection.take() {
            inspection.reap_for_teardown();
        }
        self.inspection_event = None;
        // Dropping pending generation cleanup also reaps before replacement.
        self.cleanup = None;
        self.prepared = false;
        let mut cleanup = Cleanup::start(command(self.owner))?;
        while !cleanup.poll()? {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;
