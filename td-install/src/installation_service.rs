//! The root installation service core: one held review and its disk claim,
//! answered under INSTALLER.md "Installation service protocol" over one
//! stream, with consent sought over the channel of "Installation consent
//! channel". Execution runs on its own thread through an `Execute`;
//! production has none yet, and without a consent channel execute is
//! refused as consent unavailable once the held disk rechecks.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::installation_consent::{self as consent, Answer, Ended, NoConsent, Outcome, Report};
use crate::installation_plan::{Candidates, Destination, Plan, Settings, Zones};
use crate::installation_protocol::{
    check_greeting, frame, payload_len, Abandon, Failure, Phase, Refusal, Reply, Request,
    ReviewNonce, State, GREETING, MAX_REPLY_BYTES, MAX_REQUEST_BYTES,
};

/// What the service observes and holds. Production reads the machine;
/// tests supply fakes. A refusal leaves nothing written.
pub(crate) trait Host {
    /// Held until the review ends; dropping it releases the disk.
    type Claim;
    fn candidates(&mut self) -> Result<Candidates, Refusal>;
    /// The catalog `check_settings` admits time zones from.
    fn timezones(&mut self) -> Result<Zones, Refusal>;
    fn check_settings(&mut self, settings: &Settings) -> Result<(), Refusal>;
    fn claim(&mut self, destination: &Destination) -> Result<Self::Claim, Refusal>;
    /// The authenticated deployment manifest digest.
    fn authenticate_source(&mut self) -> Result<[u8; 32], Refusal>;
    fn recheck(&mut self, destination: &Destination, claim: &mut Self::Claim) -> bool;
    /// A proposal nonce and volume UUID. Failure ends the service.
    fn entropy(&mut self) -> io::Result<([u8; 32], [u8; 16])>;
}

/// Writes a consented installation under its claim, on its own thread.
pub(crate) trait Execute<C>: Send + Sync + 'static {
    /// `progress` is told each phase as it begins.
    fn execute(
        &self,
        plan: &Plan,
        claim: C,
        progress: &mut dyn FnMut(Phase),
    ) -> Result<(), Failure>;
}

/// No execution exists yet, so production can name none.
pub(crate) enum NoExecution {}

impl<C> Execute<C> for NoExecution {
    fn execute(&self, _: &Plan, _: C, _: &mut dyn FnMut(Phase)) -> Result<(), Failure> {
        match *self {}
    }
}

enum Held<C> {
    Idle,
    Reviewed {
        plan: Box<Plan>,
        claim: C,
    },
    /// Its review report is sent; nothing written.
    AwaitingConsent {
        plan: Box<Plan>,
        claim: C,
    },
    /// The claim moved to the execution.
    Running(ReviewNonce, Phase),
    Complete(ReviewNonce),
    Failed(ReviewNonce, Failure),
    /// A review that ended before any write; its claim is released.
    Abandoned(ReviewNonce, Abandon),
}

/// A consented review, handed to its execution with the claim.
pub(crate) struct Start<C, E> {
    plan: Box<Plan>,
    claim: C,
    execution: Arc<E>,
}

pub(crate) struct Service<H: Host, E> {
    host: H,
    held: Held<H::Claim>,
    execution: Option<Arc<E>>,
    /// Whether td-authd's consent channel is open; only with an execution.
    consent: bool,
    /// Reports for td-authd, in order.
    reports: Vec<Report>,
}

impl<H: Host> Service<H, NoExecution> {
    pub(crate) fn new(host: H) -> Self {
        Self {
            host,
            held: Held::Idle,
            execution: None,
            consent: false,
            reports: Vec::new(),
        }
    }
}

impl<H: Host, E: Execute<H::Claim>> Service<H, E> {
    #[cfg(test)]
    pub(crate) fn with_execution(host: H, execution: E) -> Self {
        Self {
            host,
            held: Held::Idle,
            execution: Some(Arc::new(execution)),
            consent: false,
            reports: Vec::new(),
        }
    }

    pub(crate) fn state(&self) -> State {
        match &self.held {
            Held::Idle => State::Idle,
            Held::Reviewed { plan, .. } => State::Reviewed(ReviewNonce::from(&**plan)),
            Held::AwaitingConsent { plan, .. } => {
                State::AwaitingConsent(ReviewNonce::from(&**plan))
            }
            Held::Running(nonce, phase) => State::Running(*nonce, *phase),
            Held::Complete(nonce) => State::Complete(*nonce),
            Held::Failed(nonce, failure) => State::Failed(*nonce, *failure),
            Held::Abandoned(nonce, cause) => State::Abandoned(*nonce, *cause),
        }
    }

    /// One reply per request. An error ends the service and its claim.
    pub(crate) fn answer(&mut self, request: Request) -> io::Result<Reply> {
        Ok(match request {
            Request::Status => Reply::Status(self.state()),
            Request::Destinations => match self.host.candidates() {
                Ok(candidates) => Reply::Destinations(candidates),
                Err(refusal) => Reply::Refused(refusal),
            },
            Request::Propose {
                destination,
                settings,
            } => match self.propose(destination, settings)? {
                Ok(plan) => Reply::Reviewed(Box::new(plan)),
                Err(refusal) => Reply::Refused(refusal),
            },
            Request::Execute(plan) => self.execute(&plan),
            Request::Withdraw(nonce) => self.withdraw(nonce),
            Request::Timezones => match self.host.timezones() {
                Ok(zones) => Reply::Timezones(zones),
                Err(refusal) => Reply::Refused(refusal),
            },
        })
    }

    fn propose(
        &mut self,
        destination: Destination,
        settings: Settings,
    ) -> io::Result<Result<Plan, Refusal>> {
        match &self.held {
            // One boot installs once.
            Held::Reviewed { .. }
            | Held::AwaitingConsent { .. }
            | Held::Running(..)
            | Held::Complete(_) => return Ok(Err(Refusal::Busy)),
            Held::Idle | Held::Failed(..) | Held::Abandoned(..) => {}
        }
        if let Err(refusal) = self.host.check_settings(&settings) {
            return Ok(Err(refusal));
        }
        // The installer's destination only selects: it must equal the
        // service's own observation, labels included, and that observation
        // is what the review carries.
        let destination = match self.host.candidates() {
            Ok(candidates) => match candidates.as_slice().iter().find(|d| **d == destination) {
                Some(observed) => observed.clone(),
                None => return Ok(Err(Refusal::DestinationChanged)),
            },
            Err(refusal) => return Ok(Err(refusal)),
        };
        let mut claim = match self.host.claim(&destination) {
            Ok(claim) => claim,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let deployment = match self.host.authenticate_source() {
            Ok(deployment) => deployment,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let (nonce, uuid) = self.host.entropy()?;
        let plan = Plan::new(nonce, destination, deployment, uuid, settings)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // Source authentication can take a while; the disk must not have
        // changed under the claim meanwhile.
        if !self.host.recheck(plan.destination(), &mut claim) {
            return Ok(Err(Refusal::DestinationChanged));
        }
        self.held = Held::Reviewed {
            plan: Box::new(plan.clone()),
            claim,
        };
        Ok(Ok(plan))
    }

    fn execute(&mut self, echoed: &Plan) -> Reply {
        let (plan, claim) = match &mut self.held {
            Held::Reviewed { plan, claim } => (plan, claim),
            Held::AwaitingConsent { .. } | Held::Running(..) => {
                return Reply::Refused(Refusal::Busy)
            }
            Held::Idle | Held::Complete(_) | Held::Failed(..) | Held::Abandoned(..) => {
                return Reply::Refused(Refusal::NoReview)
            }
        };
        if **plan != *echoed {
            return Reply::Refused(Refusal::StaleReview);
        }
        let nonce = ReviewNonce::from(&**plan);
        if !self.host.recheck(plan.destination(), claim) {
            self.held = Held::Abandoned(nonce, Abandon::DestinationChanged);
            return Reply::Status(self.state());
        }
        if !self.consent {
            return Reply::Refused(Refusal::ConsentUnavailable);
        }
        let review = match consent_review(plan) {
            Ok(review) => review,
            Err(error) => {
                let _ = writeln!(io::stderr(), "td-install serve: consent: {error}");
                return Reply::Refused(Refusal::ConsentUnavailable);
            }
        };
        let Held::Reviewed { plan, claim } = std::mem::replace(&mut self.held, Held::Idle) else {
            return Reply::Refused(Refusal::NoReview);
        };
        self.held = Held::AwaitingConsent { plan, claim };
        self.reports.push(Report::Review(Box::new(review)));
        Reply::Status(State::AwaitingConsent(nonce))
    }

    fn withdraw(&mut self, nonce: ReviewNonce) -> Reply {
        let (plan, sent) = match &self.held {
            Held::Reviewed { plan, .. } => (plan, false),
            Held::AwaitingConsent { plan, .. } => (plan, true),
            Held::Running(..) => return Reply::Refused(Refusal::Busy),
            Held::Idle | Held::Complete(_) | Held::Failed(..) | Held::Abandoned(..) => {
                return Reply::Refused(Refusal::NoReview)
            }
        };
        if ReviewNonce::from(&**plan) != nonce {
            return Reply::Refused(Refusal::StaleReview);
        }
        if sent {
            self.reports
                .push(Report::Ended(*nonce.as_bytes(), Ended::Withdrawn));
        }
        self.held = Held::Abandoned(nonce, Abandon::Withdrawn);
        Reply::Status(self.state())
    }

    /// td-authd's answer. An answer naming no open review crossed its
    /// ended report and is ignored; a second answer to the open one is an
    /// error, which ends the consent channel.
    pub(crate) fn consented(&mut self, answer: Answer) -> io::Result<Option<Start<H::Claim, E>>> {
        let open = match &self.held {
            Held::AwaitingConsent { plan, .. } => ReviewNonce::from(&**plan),
            Held::Running(nonce, _) if nonce.as_bytes() == answer.nonce() => {
                return Err(invalid("second answer to a started installation".into()))
            }
            _ => return Ok(None),
        };
        if open.as_bytes() != answer.nonce() {
            return Ok(None);
        }
        // A review is only sent with an execution to run it.
        let Some(execution) = self.execution.clone() else {
            return Err(invalid("consent without an execution".into()));
        };
        let Held::AwaitingConsent { plan, mut claim } =
            std::mem::replace(&mut self.held, Held::Idle)
        else {
            return Ok(None);
        };
        let cause = match answer {
            Answer::Consent(_) => {
                // The person may have looked at the prompt for a while.
                if self.host.recheck(plan.destination(), &mut claim) {
                    self.held = Held::Running(open, Phase::PreparingDisk);
                    self.reports.push(Report::Started(*open.as_bytes()));
                    return Ok(Some(Start {
                        plan,
                        claim,
                        execution,
                    }));
                }
                self.reports
                    .push(Report::Ended(*open.as_bytes(), Ended::DestinationChanged));
                Abandon::DestinationChanged
            }
            Answer::Declined(_, why) => {
                self.reports
                    .push(Report::Ended(*open.as_bytes(), Ended::NotConsented));
                match why {
                    NoConsent::Declined => Abandon::ConsentDeclined,
                    NoConsent::Expired => Abandon::ConsentExpired,
                    NoConsent::Unavailable => Abandon::ConsentUnavailable,
                }
            }
        };
        self.held = Held::Abandoned(open, cause);
        Ok(None)
    }

    /// The consent channel is gone: a displayed review is abandoned, and
    /// later executes are refused as consent unavailable. A running
    /// installation continues.
    pub(crate) fn consent_lost(&mut self) {
        self.consent = false;
        self.reports.clear();
        if let Held::AwaitingConsent { plan, .. } = &self.held {
            self.held = Held::Abandoned(ReviewNonce::from(&**plan), Abandon::ConsentUnavailable);
        }
    }

    /// The installer is gone. Until started its review ends with it;
    /// returns whether an installation is still running.
    pub(crate) fn installer_lost(&mut self) -> bool {
        match &self.held {
            Held::Running(..) => return true,
            Held::AwaitingConsent { plan, .. } => {
                let nonce = ReviewNonce::from(&**plan);
                self.reports
                    .push(Report::Ended(*nonce.as_bytes(), Ended::InstallerLost));
                self.held = Held::Abandoned(nonce, Abandon::Withdrawn);
            }
            Held::Reviewed { plan, .. } => {
                self.held = Held::Abandoned(ReviewNonce::from(&**plan), Abandon::Withdrawn);
            }
            Held::Idle | Held::Complete(_) | Held::Failed(..) | Held::Abandoned(..) => {}
        }
        false
    }

    /// A consented start whose started report could not reach td-authd:
    /// nothing was written, and the claim went with the start.
    pub(crate) fn unstarted(&mut self) {
        if let Held::Running(nonce, _) = self.held {
            self.held = Held::Abandoned(nonce, Abandon::ConsentUnavailable);
        }
    }

    pub(crate) fn progress(&mut self, phase: Phase) {
        if let Held::Running(_, current) = &mut self.held {
            *current = phase;
        }
    }

    pub(crate) fn finished(&mut self, result: Result<(), Failure>) {
        let Held::Running(nonce, _) = self.held else {
            return;
        };
        let (held, outcome) = match result {
            Ok(()) => (Held::Complete(nonce), Outcome::Complete),
            Err(failure) => (Held::Failed(nonce, failure), Outcome::Failed),
        };
        self.held = held;
        self.reports
            .push(Report::Finished(*nonce.as_bytes(), outcome));
    }

    fn take_reports(&mut self) -> Vec<Report> {
        std::mem::take(&mut self.reports)
    }
}

/// The display facts of a held review, or why the channel cannot carry
/// them.
fn consent_review(plan: &Plan) -> Result<consent::Review, String> {
    let destination = plan.destination();
    consent::Review::new(consent::ReviewFields {
        nonce: *plan.nonce(),
        disk: destination.name(),
        capacity: destination.capacity(),
        model: destination.model(),
        serial: destination.serial(),
        hostname: plan.settings().hostname(),
        username: plan.settings().username(),
        deployment: *plan.deployment(),
    })
}

/// What reaches the serving thread.
enum Event {
    Request(Box<Request>),
    /// The installer closed between frames (`Ok`) or broke the protocol.
    InstallerClosed(io::Result<()>),
    Answer(Answer),
    ConsentClosed,
    Progress(Phase),
    Done(Result<(), Failure>),
}

/// How long a write to either peer, or td-authd's greeting, may take. A
/// peer that stops reading must not stall the other.
const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve one installer over `stream` until it closes, seeking consent over
/// `consent` when there is one and the service has an execution. A
/// malformed installer frame or message ends the installer channel without
/// a reply, and is the result; a malformed answer ends only the consent
/// channel. Until an installation starts the service ends with the
/// installer channel, releasing any held claim; once started, it runs to
/// its finished report first.
pub(crate) fn serve<H, E>(
    mut stream: UnixStream,
    service: Service<H, E>,
    consent: Option<UnixStream>,
) -> io::Result<()>
where
    H: Host,
    H::Claim: Send + 'static,
    E: Execute<H::Claim>,
{
    let result = drive(&mut stream, service, consent);
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn drive<H, E>(
    stream: &mut UnixStream,
    mut service: Service<H, E>,
    consent: Option<UnixStream>,
) -> io::Result<()>
where
    H: Host,
    H::Claim: Send + 'static,
    E: Execute<H::Claim>,
{
    stream.write_all(GREETING)?;
    stream.flush()?;
    let mut greeting = [0; 8];
    stream.read_exact(&mut greeting)?;
    check_greeting(&greeting).map_err(invalid)?;
    stream.set_write_timeout(Some(PEER_TIMEOUT))?;
    let (events, inbox) = mpsc::channel();
    // The installer reader reads one request, then waits for its reply to
    // be written, so an installer that does not read cannot queue more.
    let (replied, resume) = mpsc::channel();
    let mut replied = Some(replied);
    let installer = stream.try_clone()?;
    let installer_events = events.clone();
    spawn("installer", move || {
        read_installer(installer, &installer_events, &resume)
    })?;
    let mut channel = None;
    if let Some(opened) = consent.filter(|_| service.execution.is_some()) {
        match open_consent(&opened, &events) {
            Ok(()) => {
                channel = Some(opened);
                service.consent = true;
            }
            Err(error) => {
                let _ = writeln!(io::stderr(), "td-install serve: consent: {error}");
                let _ = opened.shutdown(Shutdown::Both);
            }
        }
    }
    let mut installer_result = None;
    let result = loop {
        let Ok(event) = inbox.recv() else {
            break Err(invalid("service events ended".into()));
        };
        match event {
            // Once the installer is gone, a running installation is all
            // that is left to serve.
            Event::Request(_) | Event::InstallerClosed(_) if installer_result.is_some() => {}
            Event::Request(request) => {
                let written = service.answer(*request).and_then(|reply| {
                    let bytes = frame(&reply.encode(), MAX_REPLY_BYTES).map_err(invalid)?;
                    stream.write_all(&bytes)?;
                    stream.flush()
                });
                match written {
                    Ok(()) => {
                        if let Some(replied) = &replied {
                            let _ = replied.send(());
                        }
                    }
                    Err(error) => {
                        // Neither a service error nor a lost reply may stop
                        // a started installation mid-write.
                        replied = None;
                        let _ = stream.shutdown(Shutdown::Both);
                        if !service.installer_lost() {
                            send_reports(&mut service, &mut channel);
                            break Err(error);
                        }
                        installer_result = Some(Err(error));
                    }
                }
            }
            Event::InstallerClosed(closed) => {
                replied = None;
                let _ = stream.shutdown(Shutdown::Both);
                if !service.installer_lost() {
                    send_reports(&mut service, &mut channel);
                    break closed;
                }
                installer_result = Some(closed);
            }
            Event::Answer(answer) => match service.consented(answer) {
                Ok(Some(start)) => {
                    // Started reaches td-authd before the first write can.
                    send_reports(&mut service, &mut channel);
                    if channel.is_none() {
                        service.unstarted();
                    } else {
                        let done = events.clone();
                        if let Err(error) = spawn("execution", move || run(start, &done)) {
                            let _ = writeln!(io::stderr(), "td-install serve: execution: {error}");
                            service.finished(Err(Failure::WriteFailed));
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = writeln!(io::stderr(), "td-install serve: consent: {error}");
                    end_consent(&mut service, &mut channel);
                }
            },
            Event::ConsentClosed => end_consent(&mut service, &mut channel),
            Event::Progress(phase) => service.progress(phase),
            Event::Done(outcome) => {
                service.finished(outcome);
                if let Some(closed) = installer_result.take() {
                    send_reports(&mut service, &mut channel);
                    break closed;
                }
            }
        }
        send_reports(&mut service, &mut channel);
    };
    if let Some(channel) = channel {
        let _ = channel.shutdown(Shutdown::Both);
    }
    result
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(body)
        .map(drop)
}

/// Greets td-authd within the peer timeout and starts its reader.
fn open_consent(channel: &UnixStream, events: &mpsc::Sender<Event>) -> io::Result<()> {
    let mut greeter = channel;
    greeter.set_write_timeout(Some(PEER_TIMEOUT))?;
    greeter.set_read_timeout(Some(PEER_TIMEOUT))?;
    greeter.write_all(consent::GREETING)?;
    greeter.flush()?;
    let mut greeting = [0; 8];
    greeter.read_exact(&mut greeting)?;
    consent::check_greeting(&greeting).map_err(invalid)?;
    // An answer may be a long time coming.
    greeter.set_read_timeout(None)?;
    let reader = channel.try_clone()?;
    let answers = events.clone();
    spawn("consent", move || read_consent(reader, &answers))
}

fn send_reports<H: Host, E: Execute<H::Claim>>(
    service: &mut Service<H, E>,
    channel: &mut Option<UnixStream>,
) {
    let reports = service.take_reports();
    let Some(open) = channel else {
        return;
    };
    for report in reports {
        let written = consent::frame(&report.encode())
            .map_err(invalid)
            .and_then(|bytes| open.write_all(&bytes))
            .and_then(|()| open.flush());
        if written.is_err() {
            end_consent(service, channel);
            return;
        }
    }
}

fn end_consent<H: Host, E: Execute<H::Claim>>(
    service: &mut Service<H, E>,
    channel: &mut Option<UnixStream>,
) {
    if let Some(open) = channel.take() {
        let _ = open.shutdown(Shutdown::Both);
    }
    service.consent_lost();
}

/// Reports an execution's outcome even if it unwinds.
struct Unfinished(Option<mpsc::Sender<Event>>);

impl Unfinished {
    fn finish(mut self, outcome: Result<(), Failure>) {
        if let Some(events) = self.0.take() {
            let _ = events.send(Event::Done(outcome));
        }
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if let Some(events) = self.0.take() {
            let _ = events.send(Event::Done(Err(Failure::WriteFailed)));
        }
    }
}

fn run<C, E: Execute<C>>(start: Start<C, E>, events: &mpsc::Sender<Event>) {
    let unfinished = Unfinished(Some(events.clone()));
    let mut progress = |phase| {
        let _ = events.send(Event::Progress(phase));
    };
    let Start {
        plan,
        claim,
        execution,
    } = start;
    let outcome = execution.execute(&plan, claim, &mut progress);
    unfinished.finish(outcome);
}

fn read_installer(
    mut stream: UnixStream,
    events: &mpsc::Sender<Event>,
    replied: &mpsc::Receiver<()>,
) {
    let closed = loop {
        match read_request(&mut stream) {
            Ok(Some(request)) => {
                if events.send(Event::Request(Box::new(request))).is_err()
                    || replied.recv().is_err()
                {
                    return;
                }
            }
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let _ = events.send(Event::InstallerClosed(closed));
}

/// A request, or `None` when the peer closed between frames.
fn read_request(stream: &mut UnixStream) -> io::Result<Option<Request>> {
    let Some(header) = read_header(stream)? else {
        return Ok(None);
    };
    let length = payload_len(header, MAX_REQUEST_BYTES).map_err(invalid)?;
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    Request::decode(&payload).map(Some).map_err(invalid)
}

fn read_consent(mut channel: UnixStream, events: &mpsc::Sender<Event>) {
    loop {
        let answer = (|| {
            let Some(header) = read_header(&mut channel)? else {
                return Ok(None);
            };
            let mut payload = vec![0; consent::payload_len(header).map_err(invalid)?];
            channel.read_exact(&mut payload)?;
            Answer::decode(&payload).map(Some).map_err(invalid)
        })();
        match answer {
            Ok(Some(answer)) => {
                if events.send(Event::Answer(answer)).is_err() {
                    return;
                }
            }
            // td-authd closed between frames.
            Ok(None) => break,
            Err(error) => {
                let _ = writeln!(io::stderr(), "td-install serve: consent: {error}");
                break;
            }
        }
    }
    let _ = events.send(Event::ConsentClosed);
}

/// A frame header, or `None` when the peer closed between frames.
fn read_header(stream: &mut impl Read) -> io::Result<Option<[u8; 4]>> {
    let mut header = [0; 4];
    let mut filled = 0;
    while let Some(rest) = header.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match stream.read(rest) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(header))
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::installation_plan::DestinationObservation;
    use std::sync::Mutex;

    fn disk(name: &str, sequence: u64, model: Option<&str>) -> Destination {
        Destination::new(DestinationObservation {
            name,
            major: 253,
            minor: sequence as u32,
            sequence,
            capacity: 8 << 30,
            sector: 512,
            removable: false,
            model,
            serial: None,
            wwid: None,
        })
        .unwrap()
    }
    fn settings() -> Settings {
        Settings::new("alice", "td-laptop", "us", "Etc/UTC").unwrap()
    }

    /// Records every host call; `live` counts claims not yet dropped.
    #[derive(Default)]
    struct Log {
        calls: Vec<&'static str>,
        live: usize,
    }
    struct Claim(Arc<Mutex<Log>>);
    impl Drop for Claim {
        fn drop(&mut self) {
            self.0.lock().unwrap().live -= 1;
        }
    }
    struct Fake {
        log: Arc<Mutex<Log>>,
        disks: Vec<Destination>,
        settings: Result<(), Refusal>,
        claim: Result<(), Refusal>,
        source: Result<[u8; 32], Refusal>,
        rechecks: Vec<bool>,
        entropy: bool,
        zones: Result<(), Refusal>,
        /// Proposals so far; each draws its own nonce.
        drawn: u8,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                log: Arc::default(),
                disks: vec![disk("vda", 1, Some("Disk")), disk("vdb", 2, None)],
                settings: Ok(()),
                claim: Ok(()),
                source: Ok([0xab; 32]),
                rechecks: Vec::new(),
                entropy: true,
                zones: Ok(()),
                drawn: 0,
            }
        }
        fn call(&self, name: &'static str) {
            self.log.lock().unwrap().calls.push(name);
        }
    }
    impl Host for Fake {
        type Claim = Claim;
        fn candidates(&mut self) -> Result<Candidates, Refusal> {
            self.call("candidates");
            Candidates::new(self.disks.clone()).map_err(|_| Refusal::DiscoveryFailed)
        }
        fn timezones(&mut self) -> Result<Zones, Refusal> {
            self.call("timezones");
            self.zones?;
            Zones::new(vec!["America/Los_Angeles".into(), "Etc/UTC".into()])
                .map_err(|_| Refusal::TimezonesUnavailable)
        }
        fn check_settings(&mut self, _: &Settings) -> Result<(), Refusal> {
            self.call("settings");
            self.settings
        }
        fn claim(&mut self, _: &Destination) -> Result<Claim, Refusal> {
            self.call("claim");
            self.claim?;
            self.log.lock().unwrap().live += 1;
            Ok(Claim(Arc::clone(&self.log)))
        }
        fn authenticate_source(&mut self) -> Result<[u8; 32], Refusal> {
            self.call("source");
            // The source is authenticated while the disk is held.
            assert_eq!(self.log.lock().unwrap().live, 1);
            self.source
        }
        fn recheck(&mut self, _: &Destination, _: &mut Claim) -> bool {
            self.call("recheck");
            if self.rechecks.is_empty() {
                true
            } else {
                self.rechecks.remove(0)
            }
        }
        fn entropy(&mut self) -> io::Result<([u8; 32], [u8; 16])> {
            self.call("entropy");
            if !self.entropy {
                return Err(io::ErrorKind::Other.into());
            }
            let mut uuid = [0x11; 16];
            uuid[6] = 0x41;
            uuid[8] = 0x81;
            let mut nonce = [0x5au8; 32];
            nonce[31] = nonce[31].wrapping_add(self.drawn);
            self.drawn += 1;
            Ok((nonce, uuid))
        }
    }

    fn propose(disk: Destination) -> Request {
        Request::Propose {
            destination: disk,
            settings: settings(),
        }
    }

    fn reviewed<E: Execute<Claim>>(service: &mut Service<Fake, E>) -> Plan {
        match service
            .answer(propose(disk("vda", 1, Some("Disk"))))
            .unwrap()
        {
            Reply::Reviewed(plan) => *plan,
            other => panic!("{other:?}"),
        }
    }

    fn live<E: Execute<Claim>>(service: &Service<Fake, E>) -> usize {
        service.host.log.lock().unwrap().live
    }

    #[test]
    fn propose_reviews_the_service_observation_under_its_claim() {
        let mut service = Service::new(Fake::new());
        let plan = reviewed(&mut service);
        assert_eq!(plan.destination(), &disk("vda", 1, Some("Disk")));
        assert_eq!(plan.deployment(), &[0xab; 32]);
        assert_eq!(plan.nonce(), &[0x5a; 32]);
        assert_eq!(plan.settings(), &settings());
        assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
        assert_eq!(live(&service), 1);
        assert_eq!(
            service.host.log.lock().unwrap().calls,
            [
                "settings",
                "candidates",
                "claim",
                "source",
                "entropy",
                "recheck"
            ]
        );
    }

    #[test]
    fn a_destination_differing_in_any_field_is_refused_before_a_claim() {
        let observed = DestinationObservation {
            name: "vda",
            major: 253,
            minor: 1,
            sequence: 1,
            capacity: 8 << 30,
            sector: 512,
            removable: false,
            model: Some("Disk"),
            serial: None,
            wwid: None,
        };
        let changes: [fn(&mut DestinationObservation<'_>); 12] = [
            |d| d.name = "vdc",
            |d| d.major = 254,
            |d| d.minor = 2,
            |d| d.sequence = 9,
            |d| d.capacity = 16 << 30,
            |d| d.sector = 4096,
            |d| d.removable = true,
            |d| d.model = None,
            |d| d.model = Some(""),
            |d| d.model = Some("Disk "),
            |d| d.serial = Some(""),
            |d| d.wwid = Some("naa.1"),
        ];
        assert_eq!(
            Destination::new(DestinationObservation { ..observed }).unwrap(),
            disk("vda", 1, Some("Disk"))
        );
        for change in changes {
            let mut differing = DestinationObservation { ..observed };
            change(&mut differing);
            let chosen = Destination::new(differing).unwrap();
            let mut service = Service::new(Fake::new());
            assert_eq!(
                service.answer(propose(chosen)).unwrap(),
                Reply::Refused(Refusal::DestinationChanged)
            );
            assert!(!service.host.log.lock().unwrap().calls.contains(&"claim"));
            assert_eq!(service.state(), State::Idle);
        }
    }

    #[test]
    fn each_refusal_stops_before_the_next_step_and_releases_the_claim() {
        let cases: [(fn(&mut Fake), Refusal, &[&str]); 5] = [
            (
                |f| f.settings = Err(Refusal::InvalidUsername),
                Refusal::InvalidUsername,
                &["settings"],
            ),
            (
                |f| f.disks = vec![disk("vda", 1, None), disk("vda", 2, None)],
                Refusal::DiscoveryFailed,
                &["settings", "candidates"],
            ),
            (
                |f| f.claim = Err(Refusal::DestinationBusy),
                Refusal::DestinationBusy,
                &["settings", "candidates", "claim"],
            ),
            (
                |f| f.source = Err(Refusal::SourceUnavailable),
                Refusal::SourceUnavailable,
                &["settings", "candidates", "claim", "source"],
            ),
            (
                |f| f.rechecks = vec![false],
                Refusal::DestinationChanged,
                &[
                    "settings",
                    "candidates",
                    "claim",
                    "source",
                    "entropy",
                    "recheck",
                ],
            ),
        ];
        for (setup, refusal, calls) in cases {
            let mut fake = Fake::new();
            setup(&mut fake);
            let mut service = Service::new(fake);
            assert_eq!(
                service
                    .answer(propose(disk("vda", 1, Some("Disk"))))
                    .unwrap(),
                Reply::Refused(refusal)
            );
            assert_eq!(service.host.log.lock().unwrap().calls, calls);
            assert_eq!(live(&service), 0, "{refusal:?}");
            assert_eq!(service.state(), State::Idle);
        }
    }

    #[test]
    fn failed_entropy_ends_the_service_and_releases_the_claim() {
        let mut fake = Fake::new();
        fake.entropy = false;
        let mut service = Service::new(fake);
        assert!(service
            .answer(propose(disk("vda", 1, Some("Disk"))))
            .is_err());
        assert_eq!(live(&service), 0);
    }

    #[test]
    fn execute_rechecks_then_refuses_until_consent_exists() {
        let mut service = Service::new(Fake::new());
        assert_eq!(
            service.answer(Request::Execute(sample_plan())).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
        let plan = reviewed(&mut service);
        assert_eq!(
            service.answer(Request::Execute(sample_plan())).unwrap(),
            Reply::Refused(Refusal::StaleReview)
        );
        for _ in 0..2 {
            assert_eq!(
                service.answer(Request::Execute(plan.clone())).unwrap(),
                Reply::Refused(Refusal::ConsentUnavailable)
            );
            assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
            assert_eq!(live(&service), 1);
        }
        service.host.rechecks = vec![false];
        let abandoned = State::Abandoned(ReviewNonce::from(&plan), Abandon::DestinationChanged);
        assert_eq!(
            service.answer(Request::Execute(plan.clone())).unwrap(),
            Reply::Status(abandoned)
        );
        assert_eq!(live(&service), 0);
        assert_eq!(service.state(), abandoned);
        assert_eq!(
            service.answer(Request::Execute(plan)).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
        // Abandoned admits a new review.
        reviewed(&mut service);
        assert_eq!(live(&service), 1);
    }

    #[test]
    fn withdraw_releases_only_the_held_review() {
        let mut service = Service::new(Fake::new());
        let other = ReviewNonce::new([9; 32]).unwrap();
        assert_eq!(
            service.answer(Request::Withdraw(other)).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
        let plan = reviewed(&mut service);
        assert_eq!(
            service.answer(Request::Withdraw(other)).unwrap(),
            Reply::Refused(Refusal::StaleReview)
        );
        assert_eq!(live(&service), 1);
        let nonce = ReviewNonce::from(&plan);
        let withdrawn = State::Abandoned(nonce, Abandon::Withdrawn);
        assert_eq!(
            service.answer(Request::Withdraw(nonce)).unwrap(),
            Reply::Status(withdrawn)
        );
        assert_eq!(live(&service), 0);
        assert_eq!(
            service.answer(Request::Withdraw(nonce)).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
    }

    #[test]
    fn propose_is_busy_while_reviewed_and_admitted_after_abandonment() {
        let mut service = Service::new(Fake::new());
        reviewed(&mut service);
        service.host.log.lock().unwrap().calls.clear();
        assert_eq!(
            service.answer(propose(disk("vdb", 2, None))).unwrap(),
            Reply::Refused(Refusal::Busy)
        );
        // Busy wins: nothing else was consulted.
        assert!(service.host.log.lock().unwrap().calls.is_empty());
        assert_eq!(live(&service), 1);

        for cause in Abandon::ALL {
            let mut service = Service::new(Fake::new());
            service.held = Held::Abandoned(nonce(), *cause);
            let reply = service.answer(propose(disk("vdb", 2, None))).unwrap();
            assert!(matches!(reply, Reply::Reviewed(_)), "{cause:?}");
        }
    }

    fn nonce() -> ReviewNonce {
        ReviewNonce::new([3; 32]).unwrap()
    }

    #[test]
    fn status_and_destinations_answer_in_every_state() {
        let mut held = Service::new(Fake::new());
        let plan = reviewed(&mut held);
        let mut services = vec![
            (State::Idle, Service::new(Fake::new())),
            (State::Reviewed(ReviewNonce::from(&plan)), held),
        ];
        let mut displayed = Service::new(Fake::new());
        let plan = reviewed(&mut displayed);
        if let Held::Reviewed { plan, claim } = std::mem::replace(&mut displayed.held, Held::Idle) {
            displayed.held = Held::AwaitingConsent { plan, claim };
        }
        services.push((State::AwaitingConsent(ReviewNonce::from(&plan)), displayed));
        let mut later = vec![
            Held::Running(nonce(), Phase::PublishingDeployment),
            Held::Complete(nonce()),
        ];
        later.extend(Failure::ALL.iter().map(|f| Held::Failed(nonce(), *f)));
        later.extend(Abandon::ALL.iter().map(|c| Held::Abandoned(nonce(), *c)));
        for held in later {
            let mut service = Service::new(Fake::new());
            service.held = held;
            services.push((service.state(), service));
        }
        assert_eq!(
            services.len(),
            3 + 2 + Failure::ALL.len() + Abandon::ALL.len()
        );
        for (state, mut service) in services {
            assert_eq!(
                service.answer(Request::Status).unwrap(),
                Reply::Status(state)
            );
            let reply = service.answer(Request::Destinations).unwrap();
            assert!(matches!(reply, Reply::Destinations(_)), "{state:?}");
            assert_eq!(service.state(), state);
            let reply = service.answer(Request::Timezones).unwrap();
            assert!(matches!(reply, Reply::Timezones(_)), "{state:?}");
            assert_eq!(service.state(), state);
            // A catalog the host cannot read refuses, changing nothing.
            service.host.zones = Err(Refusal::TimezonesUnavailable);
            assert_eq!(
                service.answer(Request::Timezones).unwrap(),
                Reply::Refused(Refusal::TimezonesUnavailable)
            );
            assert_eq!(service.state(), state);
        }
    }

    fn sample_plan() -> Plan {
        let mut uuid = [0x22; 16];
        uuid[6] = 0x42;
        uuid[8] = 0x82;
        Plan::new(
            [0x33; 32],
            disk("vda", 1, Some("Disk")),
            [0xab; 32],
            uuid,
            settings(),
        )
        .unwrap()
    }

    fn exchange(stream: &mut UnixStream, request: &Request) -> Reply {
        stream
            .write_all(&frame(&request.encode(), MAX_REQUEST_BYTES).unwrap())
            .unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let mut payload = vec![0; payload_len(header, MAX_REPLY_BYTES).unwrap()];
        stream.read_exact(&mut payload).unwrap();
        Reply::decode(&payload).unwrap()
    }

    /// The serve result, and the claims still live after it returned.
    fn served(client: impl FnOnce(&mut UnixStream) + Send + 'static) -> (io::Result<()>, usize) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            let mut theirs = theirs;
            client(&mut theirs);
        });
        let fake = Fake::new();
        let log = Arc::clone(&fake.log);
        let result = serve(ours, Service::new(fake), None);
        peer.join().unwrap();
        let live = log.lock().unwrap().live;
        (result, live)
    }

    fn greet(stream: &mut UnixStream) {
        let mut greeting = [0; 8];
        stream.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, GREETING);
        stream.write_all(GREETING).unwrap();
    }

    #[test]
    fn the_stream_greets_answers_and_releases_the_claim_at_close() {
        let (result, live) = served(|stream| {
            greet(stream);
            assert_eq!(
                exchange(stream, &Request::Status),
                Reply::Status(State::Idle)
            );
            let reply = exchange(stream, &propose(disk("vda", 1, Some("Disk"))));
            assert!(matches!(reply, Reply::Reviewed(_)), "{reply:?}");
        });
        result.unwrap();
        assert_eq!(live, 0);
    }

    #[test]
    fn a_bad_greeting_or_frame_ends_the_channel_without_a_reply() {
        let clients: [fn(&mut UnixStream); 5] = [
            |s| s.write_all(b"TDINS01\n").unwrap(),
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 0, 0]].concat())
                    .unwrap()
            },
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 8, 2]].concat())
                    .unwrap()
            },
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 0, 1, 0x7f]].concat())
                    .unwrap()
            },
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 0, 2, 0x04, 0x00]].concat())
                    .unwrap()
            },
        ];
        for client in clients {
            let (result, _) = served(move |stream| {
                client(stream);
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).unwrap();
                // Only the service's own greeting was ever sent.
                assert_eq!(rest, GREETING);
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn a_frame_cut_short_is_an_error_but_a_close_between_frames_is_not() {
        let (result, _) = served(|stream| {
            greet(stream);
            stream.write_all(&[0, 0]).unwrap();
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let (result, _) = served(greet);
        result.unwrap();
    }

    /// Runs on the execution thread: reports two phases, waits for the
    /// test's release when gated, and records the claim it was handed.
    struct FakeExecution {
        outcome: Result<(), Failure>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
        ran: Arc<Mutex<Vec<[u8; 32]>>>,
    }
    impl FakeExecution {
        fn new(outcome: Result<(), Failure>) -> Self {
            Self {
                outcome,
                release: Mutex::new(None),
                ran: Arc::default(),
            }
        }
    }
    impl Execute<Claim> for FakeExecution {
        fn execute(
            &self,
            plan: &Plan,
            claim: Claim,
            progress: &mut dyn FnMut(Phase),
        ) -> Result<(), Failure> {
            assert_eq!(claim.0.lock().unwrap().live, 1);
            progress(Phase::WritingFilesystems);
            if let Some(release) = self.release.lock().unwrap().take() {
                let _ = release.recv();
            }
            progress(Phase::VerifyingBoot);
            self.ran.lock().unwrap().push(*plan.nonce());
            self.outcome
        }
    }

    fn consenting(outcome: Result<(), Failure>) -> Service<Fake, FakeExecution> {
        let mut service = Service::with_execution(Fake::new(), FakeExecution::new(outcome));
        service.consent = true;
        service
    }

    /// A reviewed service whose review td-authd now displays.
    fn awaiting(outcome: Result<(), Failure>) -> (Service<Fake, FakeExecution>, Plan) {
        let mut service = consenting(outcome);
        let plan = reviewed(&mut service);
        let nonce = ReviewNonce::from(&plan);
        assert_eq!(
            service.answer(Request::Execute(plan.clone())).unwrap(),
            Reply::Status(State::AwaitingConsent(nonce))
        );
        (service, plan)
    }

    #[test]
    fn execute_with_consent_sends_the_review_and_holds_it() {
        let (mut service, plan) = awaiting(Ok(()));
        let reports = service.take_reports();
        let [Report::Review(review)] = reports.as_slice() else {
            panic!("{reports:?}")
        };
        assert_eq!(review.nonce(), plan.nonce());
        assert_eq!(review.disk(), "vda");
        assert_eq!(review.capacity(), 8 << 30);
        assert_eq!(review.model(), Some("Disk"));
        assert_eq!(review.serial(), None);
        assert_eq!(review.hostname(), "td-laptop");
        assert_eq!(review.username(), "alice");
        assert_eq!(review.deployment(), &[0xab; 32]);
        // Held and displayed: nothing replaces or repeats it.
        for request in [
            Request::Execute(plan.clone()),
            propose(disk("vdb", 2, None)),
        ] {
            assert_eq!(
                service.answer(request).unwrap(),
                Reply::Refused(Refusal::Busy)
            );
        }
        assert_eq!(live(&service), 1);
        assert!(service.take_reports().is_empty());
    }

    #[test]
    fn consent_rechecks_then_hands_the_claim_to_the_execution() {
        let (mut service, plan) = awaiting(Ok(()));
        service.take_reports();
        let nonce = ReviewNonce::from(&plan);
        let start = service
            .consented(Answer::Consent(*plan.nonce()))
            .unwrap()
            .unwrap();
        assert_eq!(start.plan.nonce(), plan.nonce());
        assert_eq!(service.state(), State::Running(nonce, Phase::PreparingDisk));
        assert_eq!(service.take_reports(), [Report::Started(*plan.nonce())]);
        // A started installation is neither withdrawn nor replaced.
        for request in [
            Request::Withdraw(nonce),
            Request::Execute(plan.clone()),
            propose(disk("vdb", 2, None)),
        ] {
            assert_eq!(
                service.answer(request).unwrap(),
                Reply::Refused(Refusal::Busy)
            );
        }
        assert!(service.consented(Answer::Consent(*plan.nonce())).is_err());
        assert_eq!(live(&service), 1);
        drop(start);
        assert_eq!(live(&service), 0);
        service.progress(Phase::ApplyingSettings);
        assert_eq!(
            service.state(),
            State::Running(nonce, Phase::ApplyingSettings)
        );
        service.finished(Ok(()));
        assert_eq!(service.state(), State::Complete(nonce));
        assert_eq!(
            service.take_reports(),
            [Report::Finished(*plan.nonce(), Outcome::Complete)]
        );
        // One boot installs once.
        assert_eq!(
            service.answer(propose(disk("vdb", 2, None))).unwrap(),
            Reply::Refused(Refusal::Busy)
        );
        for request in [Request::Execute(plan), Request::Withdraw(nonce)] {
            assert_eq!(
                service.answer(request).unwrap(),
                Reply::Refused(Refusal::NoReview)
            );
        }
    }

    #[test]
    fn a_failed_installation_admits_a_new_review() {
        for failure in Failure::ALL {
            let (mut service, plan) = awaiting(Err(*failure));
            let start = service.consented(Answer::Consent(*plan.nonce())).unwrap();
            drop(start);
            service.take_reports();
            service.finished(Err(*failure));
            let nonce = ReviewNonce::from(&plan);
            assert_eq!(service.state(), State::Failed(nonce, *failure));
            assert_eq!(
                service.take_reports(),
                [Report::Finished(*plan.nonce(), Outcome::Failed)]
            );
            let reply = service.answer(propose(disk("vdb", 2, None))).unwrap();
            assert!(matches!(reply, Reply::Reviewed(_)), "{failure:?}");
        }
    }

    #[test]
    fn a_disk_changed_under_consent_is_abandoned_before_any_write() {
        let (mut service, plan) = awaiting(Ok(()));
        service.take_reports();
        service.host.rechecks = vec![false];
        assert!(service
            .consented(Answer::Consent(*plan.nonce()))
            .unwrap()
            .is_none());
        let nonce = ReviewNonce::from(&plan);
        assert_eq!(
            service.state(),
            State::Abandoned(nonce, Abandon::DestinationChanged)
        );
        assert_eq!(
            service.take_reports(),
            [Report::Ended(*plan.nonce(), Ended::DestinationChanged)]
        );
        assert_eq!(live(&service), 0);
    }

    #[test]
    fn each_decline_abandons_the_review_as_not_consented() {
        for (why, cause) in [
            (NoConsent::Declined, Abandon::ConsentDeclined),
            (NoConsent::Expired, Abandon::ConsentExpired),
            (NoConsent::Unavailable, Abandon::ConsentUnavailable),
        ] {
            let (mut service, plan) = awaiting(Ok(()));
            service.take_reports();
            assert!(service
                .consented(Answer::Declined(*plan.nonce(), why))
                .unwrap()
                .is_none());
            assert_eq!(
                service.state(),
                State::Abandoned(ReviewNonce::from(&plan), cause)
            );
            assert_eq!(
                service.take_reports(),
                [Report::Ended(*plan.nonce(), Ended::NotConsented)]
            );
            assert_eq!(live(&service), 0);
        }
        assert_eq!(NoConsent::ALL.len(), 3);
    }

    #[test]
    fn an_answer_naming_no_open_review_is_ignored() {
        let other = [0x44; 32];
        // Before any review was sent, and for another review while one is.
        let mut service = consenting(Ok(()));
        assert!(service.consented(Answer::Consent(other)).unwrap().is_none());
        let plan = reviewed(&mut service);
        assert!(service
            .consented(Answer::Consent(*plan.nonce()))
            .unwrap()
            .is_none());
        assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
        let (mut service, plan) = awaiting(Ok(()));
        assert!(service.consented(Answer::Consent(other)).unwrap().is_none());
        assert_eq!(
            service.state(),
            State::AwaitingConsent(ReviewNonce::from(&plan))
        );
        // An answer that crossed the withdrawal's ended report.
        let nonce = ReviewNonce::from(&plan);
        service.take_reports();
        assert_eq!(
            service.answer(Request::Withdraw(nonce)).unwrap(),
            Reply::Status(State::Abandoned(nonce, Abandon::Withdrawn))
        );
        assert_eq!(
            service.take_reports(),
            [Report::Ended(*plan.nonce(), Ended::Withdrawn)]
        );
        assert!(service
            .consented(Answer::Consent(*plan.nonce()))
            .unwrap()
            .is_none());
        assert_eq!(service.state(), State::Abandoned(nonce, Abandon::Withdrawn));
        assert_eq!(live(&service), 0);
    }

    #[test]
    fn losing_the_installer_ends_only_a_review_that_has_not_started() {
        let mut service = consenting(Ok(()));
        reviewed(&mut service);
        assert!(!service.installer_lost());
        assert_eq!(live(&service), 0);
        assert!(service.take_reports().is_empty());

        let (mut service, plan) = awaiting(Ok(()));
        service.take_reports();
        assert!(!service.installer_lost());
        assert_eq!(
            service.take_reports(),
            [Report::Ended(*plan.nonce(), Ended::InstallerLost)]
        );
        assert_eq!(live(&service), 0);

        let (mut service, plan) = awaiting(Ok(()));
        let _start = service.consented(Answer::Consent(*plan.nonce())).unwrap();
        service.take_reports();
        assert!(service.installer_lost());
        assert!(matches!(service.state(), State::Running(..)));
        assert!(service.take_reports().is_empty());
    }

    #[test]
    fn losing_consent_abandons_a_displayed_review_and_refuses_later_ones() {
        let (mut service, plan) = awaiting(Ok(()));
        service.consent_lost();
        let nonce = ReviewNonce::from(&plan);
        assert_eq!(
            service.state(),
            State::Abandoned(nonce, Abandon::ConsentUnavailable)
        );
        // Its review report was never delivered, so nothing is owed.
        assert!(service.take_reports().is_empty());
        assert_eq!(live(&service), 0);
        let plan = reviewed(&mut service);
        assert_eq!(
            service.answer(Request::Execute(plan.clone())).unwrap(),
            Reply::Refused(Refusal::ConsentUnavailable)
        );
        assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
    }

    #[test]
    fn a_review_the_channel_cannot_carry_is_refused_and_kept() {
        let wide = "n".repeat(32);
        let mut service = consenting(Ok(()));
        service.host.disks = vec![disk(&wide, 1, None)];
        let plan = match service.answer(propose(disk(&wide, 1, None))).unwrap() {
            Reply::Reviewed(plan) => *plan,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            service.answer(Request::Execute(plan.clone())).unwrap(),
            Reply::Refused(Refusal::ConsentUnavailable)
        );
        assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
        assert!(service.take_reports().is_empty());
        assert_eq!(live(&service), 1);
    }

    /// td-authd's end of a consent channel, greeted.
    fn authority(stream: &mut UnixStream) {
        let mut greeting = [0; 8];
        stream.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, consent::GREETING);
        stream.write_all(consent::GREETING).unwrap();
    }

    fn report(stream: &mut UnixStream) -> Report {
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let mut payload = vec![0; consent::payload_len(header).unwrap()];
        stream.read_exact(&mut payload).unwrap();
        Report::decode(&payload).unwrap()
    }

    fn answer(stream: &mut UnixStream, answer: Answer) {
        stream
            .write_all(&consent::frame(&answer.encode()).unwrap())
            .unwrap();
    }

    /// Serves `installer` and `authd` against a consenting service; the
    /// result, the claims still live, and the reviews executed.
    fn served_with_consent(
        execution: FakeExecution,
        installer: impl FnOnce(&mut UnixStream) + Send + 'static,
        authd: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> (io::Result<()>, usize, Vec<[u8; 32]>) {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let (channel, mut authority_end) = UnixStream::pair().unwrap();
        let installer = std::thread::spawn(move || installer(&mut theirs));
        let authd = std::thread::spawn(move || authd(&mut authority_end));
        let ran = Arc::clone(&execution.ran);
        let fake = Fake::new();
        let log = Arc::clone(&fake.log);
        let result = serve(
            ours,
            Service::with_execution(fake, execution),
            Some(channel),
        );
        installer.join().unwrap();
        authd.join().unwrap();
        let live = log.lock().unwrap().live;
        let ran = ran.lock().unwrap().clone();
        (result, live, ran)
    }

    fn execute_awaiting(stream: &mut UnixStream) -> Plan {
        greet(stream);
        let Reply::Reviewed(plan) = exchange(stream, &propose(disk("vda", 1, Some("Disk")))) else {
            panic!("no review")
        };
        assert_eq!(
            exchange(stream, &Request::Execute((*plan).clone())),
            Reply::Status(State::AwaitingConsent(ReviewNonce::from(&*plan)))
        );
        *plan
    }

    /// Polls status until `want`; anything but the running states on the
    /// way is a failure.
    fn status_until(stream: &mut UnixStream, want: State) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "never {want:?}");
            let Reply::Status(state) = exchange(stream, &Request::Status) else {
                panic!("status refused")
            };
            if state == want {
                return;
            }
            assert!(
                matches!(state, State::AwaitingConsent(_) | State::Running(..)),
                "{state:?} before {want:?}"
            );
            std::thread::yield_now();
        }
    }

    fn gated(outcome: Result<(), Failure>) -> (FakeExecution, mpsc::Sender<()>) {
        let (release, gate) = mpsc::channel();
        let execution = FakeExecution::new(outcome);
        *execution.release.lock().unwrap() = Some(gate);
        (execution, release)
    }

    #[test]
    fn a_consented_installation_runs_while_the_installer_watches() {
        let (execution, release) = gated(Ok(()));
        let (result, live, ran) = served_with_consent(
            execution,
            move |stream| {
                let plan = execute_awaiting(stream);
                let nonce = ReviewNonce::from(&plan);
                // The execution thread holds the disk at its gate.
                status_until(stream, State::Running(nonce, Phase::WritingFilesystems));
                release.send(()).unwrap();
                status_until(stream, State::Complete(nonce));
            },
            |stream| {
                authority(stream);
                let Report::Review(review) = report(stream) else {
                    panic!("no review")
                };
                answer(stream, Answer::Consent(*review.nonce()));
                assert_eq!(report(stream), Report::Started(*review.nonce()));
                assert_eq!(
                    report(stream),
                    Report::Finished(*review.nonce(), Outcome::Complete)
                );
            },
        );
        result.unwrap();
        assert_eq!(live, 0);
        assert_eq!(ran, [[0x5a; 32]]);
    }

    #[test]
    fn a_started_installation_outlives_its_installer() {
        // A clean close is no error; a malformed frame is the result, but
        // only once the installation has finished.
        for malformed in [false, true] {
            let (execution, release) = gated(Err(Failure::WriteFailed));
            let (gone, installer_gone) = mpsc::channel();
            let (result, live, ran) = served_with_consent(
                execution,
                move |stream| {
                    let plan = execute_awaiting(stream);
                    let nonce = ReviewNonce::from(&plan);
                    status_until(stream, State::Running(nonce, Phase::WritingFilesystems));
                    if malformed {
                        stream.write_all(&[0, 0, 0, 1, 0x7f]).unwrap();
                    }
                    stream.shutdown(std::net::Shutdown::Write).unwrap();
                    let mut rest = Vec::new();
                    stream.read_to_end(&mut rest).unwrap();
                    assert!(rest.is_empty());
                    gone.send(()).unwrap();
                },
                move |stream| {
                    authority(stream);
                    let Report::Review(review) = report(stream) else {
                        panic!("no review")
                    };
                    answer(stream, Answer::Consent(*review.nonce()));
                    assert_eq!(report(stream), Report::Started(*review.nonce()));
                    installer_gone.recv().unwrap();
                    release.send(()).unwrap();
                    assert_eq!(
                        report(stream),
                        Report::Finished(*review.nonce(), Outcome::Failed)
                    );
                },
            );
            assert_eq!(
                result.map_err(|error| error.kind()),
                if malformed {
                    Err(io::ErrorKind::InvalidData)
                } else {
                    Ok(())
                }
            );
            assert_eq!(live, 0);
            assert_eq!(ran, [[0x5a; 32]]);
        }
    }

    #[test]
    fn an_installer_lost_before_consent_ends_the_review_and_the_service() {
        // Closed between frames, or with a request whose reply is lost.
        for in_flight in [false, true] {
            let (result, live, ran) = served_with_consent(
                FakeExecution::new(Ok(())),
                move |stream| {
                    execute_awaiting(stream);
                    if in_flight {
                        stream
                            .write_all(
                                &frame(&Request::Status.encode(), MAX_REQUEST_BYTES).unwrap(),
                            )
                            .unwrap();
                        stream.shutdown(std::net::Shutdown::Both).unwrap();
                    }
                },
                |stream| {
                    authority(stream);
                    let Report::Review(review) = report(stream) else {
                        panic!("no review")
                    };
                    assert_eq!(
                        report(stream),
                        Report::Ended(*review.nonce(), Ended::InstallerLost)
                    );
                },
            );
            if !in_flight {
                result.unwrap();
            }
            assert_eq!(live, 0);
            assert!(ran.is_empty());
        }
    }

    #[test]
    fn a_malformed_answer_ends_only_the_consent_channel() {
        let (result, live, ran) = served_with_consent(
            FakeExecution::new(Ok(())),
            |stream| {
                let plan = execute_awaiting(stream);
                let nonce = ReviewNonce::from(&plan);
                status_until(stream, State::Abandoned(nonce, Abandon::ConsentUnavailable));
                let Reply::Reviewed(plan) =
                    exchange(stream, &propose(disk("vda", 1, Some("Disk"))))
                else {
                    panic!("no second review")
                };
                assert_eq!(
                    exchange(stream, &Request::Execute(*plan)),
                    Reply::Refused(Refusal::ConsentUnavailable)
                );
            },
            |stream| {
                authority(stream);
                assert!(matches!(report(stream), Report::Review(_)));
                // A report's tag where an answer belongs.
                stream.write_all(&[0, 0, 0, 33, 0x03]).unwrap();
                stream.write_all(&[7; 32]).unwrap();
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).unwrap();
                assert!(rest.is_empty());
            },
        );
        result.unwrap();
        assert_eq!(live, 0);
        assert!(ran.is_empty());
    }

    #[test]
    fn busy_wins_over_a_stale_record_or_nonce() {
        let (mut service, plan) = awaiting(Ok(()));
        let other = ReviewNonce::new([9; 32]).unwrap();
        assert_eq!(
            service.answer(Request::Execute(sample_plan())).unwrap(),
            Reply::Refused(Refusal::Busy)
        );
        // Withdraw is admitted while awaiting, so a stale nonce is stale.
        assert_eq!(
            service.answer(Request::Withdraw(other)).unwrap(),
            Reply::Refused(Refusal::StaleReview)
        );
        let _start = service.consented(Answer::Consent(*plan.nonce())).unwrap();
        for request in [Request::Execute(sample_plan()), Request::Withdraw(other)] {
            assert_eq!(
                service.answer(request).unwrap(),
                Reply::Refused(Refusal::Busy)
            );
        }
    }

    #[test]
    fn a_start_that_td_authd_never_heard_of_is_abandoned_unwritten() {
        let (mut service, plan) = awaiting(Ok(()));
        let start = service.consented(Answer::Consent(*plan.nonce())).unwrap();
        service.consent_lost();
        service.unstarted();
        drop(start);
        assert_eq!(
            service.state(),
            State::Abandoned(ReviewNonce::from(&plan), Abandon::ConsentUnavailable)
        );
        assert_eq!(live(&service), 0);
        let reply = service.answer(propose(disk("vdb", 2, None))).unwrap();
        assert!(matches!(reply, Reply::Reviewed(_)), "{reply:?}");
    }

    #[test]
    fn the_installer_reader_holds_one_request_until_its_reply() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        for _ in 0..3 {
            writer
                .write_all(&frame(&Request::Status.encode(), MAX_REQUEST_BYTES).unwrap())
                .unwrap();
        }
        drop(writer);
        let (events, inbox) = mpsc::channel();
        let (replied, resume) = mpsc::channel();
        let thread = std::thread::spawn(move || read_installer(reader, &events, &resume));
        for _ in 0..3 {
            assert!(matches!(
                inbox.recv_timeout(Duration::from_secs(20)).unwrap(),
                Event::Request(_)
            ));
            // Nothing more is read until this one is answered.
            assert!(inbox.recv_timeout(Duration::from_millis(50)).is_err());
            replied.send(()).unwrap();
        }
        assert!(matches!(
            inbox.recv_timeout(Duration::from_secs(20)).unwrap(),
            Event::InstallerClosed(Ok(()))
        ));
        thread.join().unwrap();
    }

    struct Panicking;
    impl Execute<Claim> for Panicking {
        fn execute(
            &self,
            _: &Plan,
            _: Claim,
            progress: &mut dyn FnMut(Phase),
        ) -> Result<(), Failure> {
            progress(Phase::WritingFilesystems);
            panic!("the execution failed by unwinding")
        }
    }

    #[test]
    fn an_execution_that_unwinds_still_reports_finished() {
        let mut service = Service::with_execution(Fake::new(), Panicking);
        service.consent = true;
        let plan = reviewed(&mut service);
        service.answer(Request::Execute(plan.clone())).unwrap();
        let start = service
            .consented(Answer::Consent(*plan.nonce()))
            .unwrap()
            .unwrap();
        let (events, inbox) = mpsc::channel();
        let thread = std::thread::spawn(move || run(start, &events));
        assert!(thread.join().is_err());
        assert!(matches!(
            inbox.recv().unwrap(),
            Event::Progress(Phase::WritingFilesystems)
        ));
        assert!(matches!(
            inbox.recv().unwrap(),
            Event::Done(Err(Failure::WriteFailed))
        ));
        assert_eq!(live(&service), 0);
    }

    #[test]
    fn a_reply_lost_while_running_is_an_installer_lost() {
        let (execution, release) = gated(Ok(()));
        let (gone, installer_gone) = mpsc::channel();
        let (_, live, ran) = served_with_consent(
            execution,
            move |stream| {
                let plan = execute_awaiting(stream);
                let nonce = ReviewNonce::from(&plan);
                status_until(stream, State::Running(nonce, Phase::WritingFilesystems));
                // A request whose reply has nowhere to go: writing to a
                // peer that shut its read side fails.
                stream.shutdown(std::net::Shutdown::Read).unwrap();
                stream
                    .write_all(&frame(&Request::Status.encode(), MAX_REQUEST_BYTES).unwrap())
                    .unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                gone.send(()).unwrap();
            },
            move |stream| {
                authority(stream);
                let Report::Review(review) = report(stream) else {
                    panic!("no review")
                };
                answer(stream, Answer::Consent(*review.nonce()));
                assert_eq!(report(stream), Report::Started(*review.nonce()));
                installer_gone.recv().unwrap();
                release.send(()).unwrap();
                assert_eq!(
                    report(stream),
                    Report::Finished(*review.nonce(), Outcome::Complete)
                );
            },
        );
        assert_eq!(live, 0);
        assert_eq!(ran, [[0x5a; 32]]);
    }

    #[test]
    fn a_decline_and_a_crossing_consent_start_nothing() {
        let (withdrawn, after_withdrawal) = mpsc::channel();
        let (result, live, ran) = served_with_consent(
            FakeExecution::new(Ok(())),
            move |stream| {
                let plan = execute_awaiting(stream);
                let nonce = ReviewNonce::from(&plan);
                status_until(stream, State::Abandoned(nonce, Abandon::ConsentDeclined));
                // A second review, withdrawn while td-authd consents to it.
                let plan = execute_awaiting_again(stream);
                let nonce = ReviewNonce::from(&plan);
                assert_eq!(
                    exchange(stream, &Request::Withdraw(nonce)),
                    Reply::Status(State::Abandoned(nonce, Abandon::Withdrawn))
                );
                withdrawn.send(()).unwrap();
                // A third review, so the crossing consent has been read
                // before the decline of this one is.
                let plan = execute_awaiting_again(stream);
                let nonce = ReviewNonce::from(&plan);
                status_until(stream, State::Abandoned(nonce, Abandon::ConsentExpired));
            },
            move |stream| {
                authority(stream);
                let Report::Review(first) = report(stream) else {
                    panic!("no review")
                };
                answer(
                    stream,
                    Answer::Declined(*first.nonce(), NoConsent::Declined),
                );
                assert_eq!(
                    report(stream),
                    Report::Ended(*first.nonce(), Ended::NotConsented)
                );
                let Report::Review(second) = report(stream) else {
                    panic!("no second review")
                };
                // Consent sent after the withdrawal, before its ended report
                // is read: the two cross.
                after_withdrawal.recv().unwrap();
                answer(stream, Answer::Consent(*second.nonce()));
                assert_eq!(
                    report(stream),
                    Report::Ended(*second.nonce(), Ended::Withdrawn)
                );
                let Report::Review(third) = report(stream) else {
                    panic!("no third review")
                };
                answer(stream, Answer::Declined(*third.nonce(), NoConsent::Expired));
                assert_eq!(
                    report(stream),
                    Report::Ended(*third.nonce(), Ended::NotConsented)
                );
            },
        );
        result.unwrap();
        assert_eq!(live, 0);
        assert!(ran.is_empty());
    }

    /// Proposes and executes again on a greeted channel.
    fn execute_awaiting_again(stream: &mut UnixStream) -> Plan {
        let Reply::Reviewed(plan) = exchange(stream, &propose(disk("vda", 1, Some("Disk")))) else {
            panic!("no review")
        };
        assert_eq!(
            exchange(stream, &Request::Execute((*plan).clone())),
            Reply::Status(State::AwaitingConsent(ReviewNonce::from(&*plan)))
        );
        *plan
    }

    #[test]
    fn without_a_greeted_channel_and_an_execution_consent_is_unavailable() {
        let refused = |stream: &mut UnixStream| {
            greet(stream);
            let Reply::Reviewed(plan) = exchange(stream, &propose(disk("vda", 1, Some("Disk"))))
            else {
                panic!("no review")
            };
            assert_eq!(
                exchange(stream, &Request::Execute(*plan)),
                Reply::Refused(Refusal::ConsentUnavailable)
            );
        };
        // td-authd answers with the wrong greeting.
        let (result, live, ran) =
            served_with_consent(FakeExecution::new(Ok(())), refused, |stream| {
                let mut greeting = [0; 8];
                stream.read_exact(&mut greeting).unwrap();
                stream.write_all(b"TDINA02\n").unwrap();
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).unwrap();
                assert!(rest.is_empty());
            });
        result.unwrap();
        assert_eq!(live, 0);
        assert!(ran.is_empty());
        // A service with no execution never greets a channel it is given.
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let (channel, mut authority_end) = UnixStream::pair().unwrap();
        let installer = std::thread::spawn(move || refused(&mut theirs));
        serve(ours, Service::new(Fake::new()), Some(channel)).unwrap();
        installer.join().unwrap();
        let mut rest = Vec::new();
        authority_end.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn a_start_td_authd_cannot_be_told_of_writes_nothing() {
        let (result, live, ran) = served_with_consent(
            FakeExecution::new(Ok(())),
            |stream| {
                let plan = execute_awaiting(stream);
                let nonce = ReviewNonce::from(&plan);
                status_until(stream, State::Abandoned(nonce, Abandon::ConsentUnavailable));
            },
            |stream| {
                authority(stream);
                let Report::Review(review) = report(stream) else {
                    panic!("no review")
                };
                // The started report has nowhere to go.
                stream.shutdown(std::net::Shutdown::Read).unwrap();
                answer(stream, Answer::Consent(*review.nonce()));
            },
        );
        result.unwrap();
        assert_eq!(live, 0);
        assert!(ran.is_empty());
    }
}
