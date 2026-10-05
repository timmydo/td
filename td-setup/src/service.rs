//! The installer's one connection to the installation service, reached
//! through td-authd's setup intake (td-authd/DESIGN.md "Whole-disk
//! installation intake"); td-install/INSTALLER.md "Installation service
//! protocol" is the wire. A worker thread connects and does the blocking
//! exchange, one request at a time, so the window's turn loop never waits on
//! the service. The connection holds no authority: the service checks every
//! request and erases nothing without trusted consent.

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use td_install::installation_plan::{Destination, Plan, Settings, Storage};
use td_install::installation_protocol::{
    self as protocol, Abandon, Failure, Phase, Refusal, Reply, Request, ReviewNonce, State,
};

use crate::outcome;

/// How a complete installation's live session ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ending {
    Restart,
    PowerOff,
}

impl Ending {
    pub const ALL: &[Self] = &[Self::Restart, Self::PowerOff];

    fn wire(self) -> protocol::Ending {
        match self {
            Self::Restart => protocol::Ending::Restart,
            Self::PowerOff => protocol::Ending::PowerOff,
        }
    }

    fn from_wire(ending: protocol::Ending) -> Self {
        match ending {
            protocol::Ending::Restart => Self::Restart,
            protocol::Ending::PowerOff => Self::PowerOff,
        }
    }
}

/// td-authd's setup intake; it starts one service per connection.
pub const SOCKET: &str = "/run/td-authd/1000/setup";
/// A service that stops reading for this long, or does not greet within it,
/// ends the connection. A reply has no limit: the person can leave.
const PEER_TIME: Duration = Duration::from_secs(10);
/// How often an idle worker looks for the service having gone.
const IDLE_CHECK: Duration = Duration::from_millis(250);

/// What the service answered, as the window needs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Answer {
    /// Its eligible disks, freshly observed.
    Destinations(Vec<Destination>),
    /// The time zones it would admit, in ascending order.
    Timezones(Vec<String>),
    /// Its review of a proposal, made while it holds the disk's claim; it
    /// carries exactly the proposed disk and settings.
    Reviewed(Box<Plan>),
    /// The review named was withdrawn and its claim released.
    Withdrawn,
    /// The service's state, after execute or status.
    Standing(Standing),
    /// Execute refused because the service holds no review.
    Unheld(&'static str),
    /// Execute refused because the service holds another review than the
    /// one shown; it keeps that review and its claim.
    Stale(&'static str),
    /// A refusal, as text for the person.
    Refused(&'static str),
    /// The supervisor accepted the restart or power-off of the completed
    /// installation.
    Ending(Ending),
}

/// The service's state, as the installer shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Standing {
    /// No review, or none this connection made.
    Idle,
    /// The review a nonce names, at a stage.
    Review { nonce: [u8; 32], stage: Stage },
}

/// Where a held review stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Reviewed,
    AwaitingConsent,
    Running(outcome::Phase),
    Complete,
    Failed(outcome::Failure),
    /// Ended before any write, with the reason in the installer's words.
    Abandoned(&'static str),
}

/// The connection; dropping it ends the worker and, with it, the service.
pub struct Service {
    requests: Sender<Request>,
    answers: Receiver<Result<Answer, String>>,
    link: Arc<Mutex<Link>>,
    pending: bool,
}

/// What the window and its worker share about the socket: whichever of
/// dropping and connecting comes second sees the other.
#[derive(Default)]
struct Link {
    closed: bool,
    stream: Option<UnixStream>,
}

impl Service {
    /// An intake that is absent fails at once; any other connection error
    /// arrives as an answer.
    pub fn connect(intake: &Path) -> Result<Self, String> {
        std::fs::symlink_metadata(intake)
            .map_err(|error| format!("{}: {error}", intake.display()))?;
        let intake = intake.to_path_buf();
        let (requests, inbox) = mpsc::channel();
        let (outbox, answers) = mpsc::channel();
        let link = Arc::new(Mutex::new(Link::default()));
        let shared = Arc::clone(&link);
        std::thread::Builder::new()
            .name("td-setup-service".into())
            .spawn(move || {
                if let Err(why) = serve(&intake, &shared, &inbox, &outbox) {
                    let _ = outbox.send(Err(why));
                }
            })
            .map_err(|error| format!("start the service worker: {error}"))?;
        Ok(Self {
            requests,
            answers,
            link,
            pending: false,
        })
    }

    /// Asks for the eligible disks.
    pub fn destinations(&mut self) -> Result<(), String> {
        self.send(Request::Destinations)
    }

    /// Proposes installing to `destination` with `settings`; the service
    /// reviews it against its own observation, or refuses.
    pub fn propose(&mut self, destination: Destination, settings: Settings) -> Result<(), String> {
        self.send(Request::Propose {
            destination,
            settings,
        })
    }

    /// Releases the review `nonce` names, and its disk claim.
    pub fn withdraw(&mut self, nonce: [u8; 32]) -> Result<(), String> {
        self.send(Request::Withdraw(ReviewNonce::new(nonce)?))
    }

    /// Asks the service to seek consent for its review `plan`, which must
    /// be exactly the review it holds.
    pub fn execute(&mut self, plan: Plan) -> Result<(), String> {
        self.send(Request::Execute(plan))
    }

    /// Asks for the service's state.
    pub fn status(&mut self) -> Result<(), String> {
        self.send(Request::Status)
    }

    /// Asks for the time zone catalog.
    pub fn timezones(&mut self) -> Result<(), String> {
        self.send(Request::Timezones)
    }

    /// Asks the service to restart or power off the computer; it admits
    /// this only while the installation `nonce` names is complete.
    pub fn end(&mut self, nonce: [u8; 32], ending: Ending) -> Result<(), String> {
        self.send(Request::End(ending.wire(), ReviewNonce::new(nonce)?))
    }

    /// Whether a request awaits its answer.
    pub fn pending(&self) -> bool {
        self.pending
    }

    fn send(&mut self, request: Request) -> Result<(), String> {
        if self.pending {
            return Err("an installer service request is outstanding".into());
        }
        self.requests
            .send(request)
            .map_err(|_| "the installer service connection ended".to_string())?;
        self.pending = true;
        Ok(())
    }

    /// The answer to the outstanding request once it arrives, or the
    /// connection's end, even while idle. After an error the connection is
    /// unusable.
    pub fn poll(&mut self) -> Option<Result<Answer, String>> {
        let answer = match self.answers.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err("the installer service connection ended".to_string())
            }
        };
        self.pending = false;
        Some(answer)
    }
}

impl Drop for Service {
    /// Shuts the worker's socket, which ends it; a worker still connecting
    /// finds the link closed once connected and ends then.
    fn drop(&mut self) {
        if let Ok(mut link) = self.link.lock() {
            link.closed = true;
            if let Some(stream) = &link.stream {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

fn serve(
    intake: &PathBuf,
    shared: &Mutex<Link>,
    requests: &Receiver<Request>,
    answers: &Sender<Result<Answer, String>>,
) -> Result<(), String> {
    let mut stream = UnixStream::connect(intake)
        .map_err(|error| format!("connect {}: {error}", intake.display()))?;
    stream
        .set_write_timeout(Some(PEER_TIME))
        .map_err(|error| error.to_string())?;
    {
        let mut link = shared
            .lock()
            .map_err(|_| "the installer service link is poisoned".to_string())?;
        if link.closed {
            return Ok(());
        }
        link.stream = Some(stream.try_clone().map_err(|error| error.to_string())?);
    }
    greet(&mut stream)?;
    loop {
        match requests.recv_timeout(IDLE_CHECK) {
            Ok(request) => {
                let answer = exchange(&mut stream, &request)?;
                if answers.send(Ok(answer)).is_err() {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => idle(&mut stream)?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn greet(stream: &mut UnixStream) -> Result<(), String> {
    stream
        .write_all(protocol::GREETING)
        .map_err(|error| format!("greet the installer service: {error}"))?;
    stream
        .set_read_timeout(Some(PEER_TIME))
        .map_err(|error| error.to_string())?;
    let mut greeting = [0; 8];
    stream
        .read_exact(&mut greeting)
        .map_err(|error| format!("installer service greeting: {error}"))?;
    stream
        .set_read_timeout(None)
        .map_err(|error| error.to_string())?;
    protocol::check_greeting(&greeting)
}

/// Between requests the service sends nothing; a close or a stray byte
/// ends the connection.
fn idle(stream: &mut UnixStream) -> Result<(), String> {
    stream
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let mut byte = [0; 1];
    let read = stream.read(&mut byte);
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    match read {
        Ok(0) => Err("the installer service closed the connection".into()),
        Ok(_) => Err("the installer service sent an unrequested reply".into()),
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
            Ok(())
        }
        Err(error) => Err(format!("installer service connection: {error}")),
    }
}

fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Answer, String> {
    stream
        .write_all(&protocol::frame(
            &request.encode(),
            protocol::MAX_REQUEST_BYTES,
        )?)
        .map_err(|error| format!("installer service request: {error}"))?;
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .map_err(|error| format!("installer service reply: {error}"))?;
    let mut payload = vec![0; protocol::payload_len(header, protocol::MAX_REPLY_BYTES)?];
    stream
        .read_exact(&mut payload)
        .map_err(|error| format!("installer service reply: {error}"))?;
    let reply = Reply::decode(&payload);
    protocol::scrub(&mut payload);
    let reply = reply?;
    if !reply.answers(request) {
        return Err("installer service reply does not answer its request".into());
    }
    // The pairing is the protocol's; the content is checked here. A reply
    // that does not answer what was sent ends the connection, and with it
    // any review the service holds.
    match (request, reply) {
        (Request::Withdraw(nonce), Reply::Status(State::Abandoned(ended, Abandon::Withdrawn)))
            if ended == *nonce =>
        {
            Ok(Answer::Withdrawn)
        }
        (Request::Withdraw(_), _) => Err("the installer service did not release the review".into()),
        (Request::End(ending, nonce), Reply::Status(State::Complete(complete)))
            if complete == *nonce =>
        {
            Ok(Answer::Ending(Ending::from_wire(*ending)))
        }
        (Request::End(..), Reply::Status(_)) => {
            Err("the installer service did not end the installation's session".into())
        }
        (
            Request::Propose {
                destination,
                settings,
            },
            Reply::Reviewed(plan),
        ) => {
            if plan.destination() != destination || plan.settings() != settings {
                return Err("installer service review does not match the proposal".into());
            }
            // This window discloses unencrypted storage only; td-authd
            // starts no device-bound service, so another is not its review.
            if plan.storage() != Storage::Unencrypted {
                return Err("installer service review is not unencrypted".into());
            }
            Ok(Answer::Reviewed(plan))
        }
        (_, Reply::Destinations(candidates)) => {
            Ok(Answer::Destinations(candidates.as_slice().to_vec()))
        }
        (_, Reply::Timezones(zones)) => Ok(Answer::Timezones(zones.as_slice().to_vec())),
        // Busy means consent is sought or an installation runs: nothing
        // this installer shows is settled, so the connection ends.
        (Request::Execute(_), Reply::Refused(Refusal::Busy)) => {
            Err("the installer service is already seeking consent or installing".into())
        }
        (Request::Execute(_), Reply::Refused(Refusal::NoReview)) => {
            Ok(Answer::Unheld(refusal_text(Refusal::NoReview)))
        }
        (Request::Execute(_), Reply::Refused(Refusal::StaleReview)) => {
            Ok(Answer::Stale(refusal_text(Refusal::StaleReview)))
        }
        (_, Reply::Status(state)) => standing(state).map(Answer::Standing),
        (_, Reply::Refused(refusal)) => Ok(Answer::Refused(refusal_text(refusal))),
        // Only propose is answered with a review, and it is matched above;
        // this installer never asks for a recovery key.
        (_, Reply::Reviewed(_) | Reply::RecoveryKey(..)) => {
            Err("installer service reply is not one this installer reads".into())
        }
    }
}

/// The recovery-key phase and its failure are a device-bound
/// installation's, which this installer never reviews, so either ends the
/// connection.
fn standing(state: State) -> Result<Standing, String> {
    let Some(nonce) = state.review() else {
        return Ok(Standing::Idle);
    };
    let stage = match state {
        State::Idle | State::Reviewed(_) => Stage::Reviewed,
        State::AwaitingConsent(_) => Stage::AwaitingConsent,
        State::Running(_, phase) => Stage::Running(match phase {
            Phase::PreparingDisk => outcome::Phase::PreparingDisk,
            Phase::WritingFilesystems => outcome::Phase::WritingFilesystems,
            Phase::PublishingDeployment => outcome::Phase::PublishingDeployment,
            Phase::ApplyingSettings => outcome::Phase::ApplyingSettings,
            Phase::VerifyingBoot => outcome::Phase::VerifyingBoot,
            Phase::RecoveryKey => return Err(DEVICE_BOUND.into()),
        }),
        State::Complete(_) => Stage::Complete,
        State::Failed(_, failure) => Stage::Failed(match failure {
            Failure::DestinationChanged => outcome::Failure::DestinationChanged,
            Failure::InsufficientSpace => outcome::Failure::InsufficientSpace,
            Failure::WriteFailed => outcome::Failure::WriteFailed,
            Failure::VerificationFailed => outcome::Failure::VerificationFailed,
            Failure::SettingsFailed => outcome::Failure::SettingsFailed,
            Failure::RecoveryUnconfirmed => return Err(DEVICE_BOUND.into()),
        }),
        State::Abandoned(_, abandon) => Stage::Abandoned(abandon_text(abandon)),
    };
    Ok(Standing::Review {
        nonce: *nonce.as_bytes(),
        stage,
    })
}

const DEVICE_BOUND: &str = "the installer service reports a device-bound installation";

/// Plain text for each way a review ends before any write.
fn abandon_text(abandon: Abandon) -> &'static str {
    match abandon {
        Abandon::Withdrawn => "the review was withdrawn",
        Abandon::ConsentDeclined => "the installation was declined at the secure prompt",
        Abandon::ConsentExpired => "the secure prompt expired without consent",
        Abandon::ConsentUnavailable => "the secure prompt was unavailable",
        Abandon::DestinationChanged => "the selected disk changed",
    }
}

/// Plain text for each refusal (td-install/INSTALLER.md lists them).
fn refusal_text(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Busy => "the installer service is busy",
        Refusal::SourceUnavailable => "the installation source is unavailable",
        Refusal::DiscoveryFailed => "the disks could not be examined",
        Refusal::DestinationChanged => "the selected disk changed",
        Refusal::DestinationBusy => "the selected disk is in use",
        Refusal::InsufficientSpace => "the selected disk is too small",
        Refusal::InvalidUsername => "the username is not allowed",
        Refusal::InvalidHostname => "the hostname is not allowed",
        Refusal::UnsupportedKeyboard => "the keyboard layout is not supported",
        Refusal::UnsupportedTimezone => "the time zone is not supported",
        Refusal::StaleReview => "the review is out of date",
        Refusal::NoReview => "there is no review",
        Refusal::ConsentUnavailable => "trusted consent is unavailable",
        Refusal::TimezonesUnavailable => "the time zones could not be read",
        Refusal::PowerUnavailable => "the computer could not be restarted or powered off",
        Refusal::RecoveryKeySent => "the recovery key was already shown",
        Refusal::RecoveryKeyMismatch => "the recovery key does not match",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;
    use td_install::installation_plan::{Candidates, DestinationObservation, Zones};

    pub(crate) fn disk() -> Destination {
        Destination::new(DestinationObservation {
            name: "vda",
            major: 254,
            minor: 0,
            sequence: 7,
            capacity: 16_000_000_000,
            sector: 512,
            removable: false,
            model: Some("QEMU HARDDISK"),
            serial: None,
            wwid: None,
        })
        .unwrap()
    }

    pub(crate) fn framed(reply: &Reply) -> Option<Vec<u8>> {
        Some(protocol::frame(&reply.encode(), protocol::MAX_REPLY_BYTES).unwrap())
    }

    pub(crate) fn zones() -> Vec<String> {
        ["America/New_York", "Etc/UTC", "Europe/London"]
            .map(String::from)
            .to_vec()
    }

    /// The service's review of `destination` with `settings`.
    pub(crate) fn plan(destination: &Destination, settings: &Settings) -> Plan {
        let uuid = [0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 0];
        Plan::new(
            [7; 32],
            destination.clone(),
            [9; 32],
            uuid,
            Storage::Unencrypted,
            settings.clone(),
        )
        .unwrap()
    }

    /// Answers each request as a service with one disk would, reviewing
    /// every proposal.
    pub(crate) fn listing(request: &Request) -> Option<Vec<u8>> {
        match request {
            Request::Destinations => {
                framed(&Reply::Destinations(Candidates::new(vec![disk()]).unwrap()))
            }
            Request::Timezones => framed(&Reply::Timezones(Zones::new(zones()).unwrap())),
            Request::Propose {
                destination,
                settings,
            } => framed(&Reply::Reviewed(Box::new(plan(destination, settings)))),
            Request::Withdraw(nonce) => {
                framed(&Reply::Status(State::Abandoned(*nonce, Abandon::Withdrawn)))
            }
            Request::Execute(plan) => framed(&Reply::Status(State::AwaitingConsent(
                ReviewNonce::new(*plan.nonce()).unwrap(),
            ))),
            Request::Status => framed(&Reply::Status(State::Idle)),
            Request::End(_, nonce) => framed(&Reply::Status(State::Complete(*nonce))),
            Request::RecoveryKey(_) | Request::ConfirmRecovery(..) => {
                framed(&Reply::Refused(Refusal::NoReview))
            }
        }
    }

    /// As `listing`, but executing the review it gives: consent is
    /// awaited once more, then the disk is prepared, then complete.
    pub(crate) fn installing() -> impl FnMut(&Request) -> Option<Vec<u8>> + Send + 'static {
        let nonce = ReviewNonce::new([7; 32]).unwrap();
        let mut states = vec![
            State::Complete(nonce),
            State::Running(nonce, Phase::PreparingDisk),
            State::AwaitingConsent(nonce),
        ];
        move |request| match request {
            Request::Status => framed(&Reply::Status(states.pop().unwrap_or(State::Idle))),
            _ => listing(request),
        }
    }

    /// As `listing`, but holding each review as the service core does:
    /// a proposal while one is held is busy, and only its nonce withdraws.
    pub(crate) fn holding() -> impl FnMut(&Request) -> Option<Vec<u8>> + Send + 'static {
        let mut held = None;
        move |request| match request {
            Request::Propose {
                destination,
                settings,
            } => {
                if held.is_some() {
                    return framed(&Reply::Refused(Refusal::Busy));
                }
                let plan = plan(destination, settings);
                held = Some(ReviewNonce::new(*plan.nonce()).unwrap());
                framed(&Reply::Reviewed(Box::new(plan)))
            }
            Request::Withdraw(nonce) => match held {
                Some(review) if review == *nonce => {
                    held = None;
                    framed(&Reply::Status(State::Abandoned(*nonce, Abandon::Withdrawn)))
                }
                Some(_) => framed(&Reply::Refused(Refusal::StaleReview)),
                None => framed(&Reply::Refused(Refusal::NoReview)),
            },
            _ => listing(request),
        }
    }

    /// As `listing`, but the catalog comes only after a pause.
    pub(crate) fn slow_catalog(request: &Request) -> Option<Vec<u8>> {
        if matches!(request, Request::Timezones) {
            std::thread::sleep(Duration::from_millis(300));
        }
        listing(request)
    }

    /// Never answers; with unbounded replies the connection stays open.
    pub(crate) fn silent(_: &Request) -> Option<Vec<u8>> {
        Some(Vec::new())
    }

    /// A one-connection stand-in for the intake and its service: it greets
    /// with `greeting`, answers up to `replies` requests with the bytes
    /// `reply` gives (None closes unanswered), then closes.
    pub(crate) fn fake(
        name: &str,
        greeting: &'static [u8; 8],
        replies: usize,
        mut reply: impl FnMut(&Request) -> Option<Vec<u8>> + Send + 'static,
    ) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("td-setup-service-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("setup");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = std::fs::remove_dir_all(&directory);
            let mut theirs = [0; 8];
            stream.read_exact(&mut theirs).unwrap();
            assert_eq!(&theirs, protocol::GREETING);
            stream.write_all(greeting).unwrap();
            for _ in 0..replies {
                let mut header = [0; 4];
                if stream.read_exact(&mut header).is_err() {
                    return;
                }
                let mut payload =
                    vec![0; protocol::payload_len(header, protocol::MAX_REQUEST_BYTES).unwrap()];
                stream.read_exact(&mut payload).unwrap();
                let request = Request::decode(&payload).unwrap();
                let Some(bytes) = reply(&request) else {
                    return;
                };
                stream.write_all(&bytes).unwrap();
            }
        });
        socket
    }

    /// The intake of a service that greets correctly.
    pub(crate) fn intake(
        name: &str,
        replies: usize,
        reply: impl FnMut(&Request) -> Option<Vec<u8>> + Send + 'static,
    ) -> PathBuf {
        fake(name, protocol::GREETING, replies, reply)
    }

    fn answer(service: &mut Service) -> Result<Answer, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(answer) = service.poll() {
                return answer;
            }
            assert!(Instant::now() < deadline, "no answer");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn the_service_lists_its_destinations_one_request_at_a_time() {
        let socket = fake("list", protocol::GREETING, usize::MAX, listing);
        let mut service = Service::connect(&socket).unwrap();
        assert!(!service.pending());
        service.destinations().unwrap();
        assert!(service.pending());
        assert!(service.destinations().is_err());
        assert_eq!(answer(&mut service), Ok(Answer::Destinations(vec![disk()])));
        assert!(!service.pending());
        // The connection serves again, and other requests too.
        service.destinations().unwrap();
        assert_eq!(answer(&mut service), Ok(Answer::Destinations(vec![disk()])));
        service.timezones().unwrap();
        assert!(service.destinations().is_err());
        assert_eq!(answer(&mut service), Ok(Answer::Timezones(zones())));
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        service.propose(disk(), settings.clone()).unwrap();
        assert_eq!(
            answer(&mut service),
            Ok(Answer::Reviewed(Box::new(plan(&disk(), &settings))))
        );
        service.withdraw([7; 32]).unwrap();
        assert_eq!(answer(&mut service), Ok(Answer::Withdrawn));
        // A zero nonce names no review and is not sent.
        assert!(service.withdraw([0; 32]).is_err());
        assert!(!service.pending());
    }

    #[test]
    fn a_refusal_is_plain_text() {
        let socket = fake("refused", protocol::GREETING, 1, |_| {
            framed(&Reply::Refused(Refusal::DiscoveryFailed))
        });
        let mut service = Service::connect(&socket).unwrap();
        service.destinations().unwrap();
        assert_eq!(
            answer(&mut service),
            Ok(Answer::Refused("the disks could not be examined"))
        );
        // A reply that answers another request than the one sent.
        let socket = fake("unpaired", protocol::GREETING, 1, |_| {
            framed(&Reply::Timezones(Zones::new(zones()).unwrap()))
        });
        let mut service = Service::connect(&socket).unwrap();
        service.destinations().unwrap();
        let refused = answer(&mut service).unwrap_err();
        assert!(refused.contains("does not answer"), "{refused}");
    }

    #[test]
    fn a_broken_or_absent_service_ends_the_connection() {
        // Busy, or refused by td-authd: closed unanswered.
        let socket = fake("closed", protocol::GREETING, 1, |_| None);
        let mut service = Service::connect(&socket).unwrap();
        service.destinations().unwrap();
        assert!(answer(&mut service).is_err());
        // Another protocol's greeting.
        let socket = fake("greeting", b"TDINS01\n", usize::MAX, silent);
        let mut service = Service::connect(&socket).unwrap();
        let refused = answer(&mut service).unwrap_err();
        assert!(
            refused.contains("unsupported installation protocol greeting"),
            "{refused}"
        );
        // A reply of the wrong kind.
        let socket = fake("kind", protocol::GREETING, 1, |_| {
            framed(&Reply::Status(State::Idle))
        });
        let mut service = Service::connect(&socket).unwrap();
        service.destinations().unwrap();
        assert!(answer(&mut service).is_err());
        // A reply longer than any the protocol admits.
        let socket = fake("long", protocol::GREETING, 1, |_| {
            let length = u32::try_from(protocol::MAX_REPLY_BYTES + 1).unwrap();
            Some(length.to_be_bytes().to_vec())
        });
        let mut service = Service::connect(&socket).unwrap();
        service.destinations().unwrap();
        assert!(answer(&mut service).is_err());
        // No intake at all fails at once.
        assert!(Service::connect(Path::new("/nonexistent/td-setup/setup")).is_err());
    }

    #[test]
    fn a_review_or_release_that_does_not_answer_what_was_sent_ends_it() {
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        // A review of other settings than those proposed.
        let socket = fake("other-review", protocol::GREETING, 1, |_| {
            let other = Settings::new("mallory", "tdhost", "us", "Etc/UTC").unwrap();
            framed(&Reply::Reviewed(Box::new(plan(&disk(), &other))))
        });
        let mut service = Service::connect(&socket).unwrap();
        service.propose(disk(), settings.clone()).unwrap();
        let refused = answer(&mut service).unwrap_err();
        assert!(refused.contains("does not match"), "{refused}");
        // A withdraw refused, answered for another review, or answered
        // with a state other than withdrawn.
        for (name, reply) in [
            ("withdraw-refused", Reply::Refused(Refusal::StaleReview)),
            (
                "withdraw-other",
                Reply::Status(State::Abandoned(
                    ReviewNonce::new([8; 32]).unwrap(),
                    Abandon::Withdrawn,
                )),
            ),
            (
                "withdraw-held",
                Reply::Status(State::Reviewed(ReviewNonce::new([7; 32]).unwrap())),
            ),
            (
                "withdraw-running",
                Reply::Status(State::Running(
                    ReviewNonce::new([7; 32]).unwrap(),
                    Phase::PreparingDisk,
                )),
            ),
        ] {
            let bytes = framed(&reply);
            let socket = fake(name, protocol::GREETING, 1, move |_| bytes.clone());
            let mut service = Service::connect(&socket).unwrap();
            service.withdraw([7; 32]).unwrap();
            let refused = answer(&mut service).unwrap_err();
            assert!(refused.contains("did not release"), "{name}: {refused}");
        }
    }

    #[test]
    fn a_service_that_closes_while_idle_ends_the_connection() {
        let socket = fake("idle", protocol::GREETING, 1, listing);
        let mut service = Service::connect(&socket).unwrap();
        service.destinations().unwrap();
        assert_eq!(answer(&mut service), Ok(Answer::Destinations(vec![disk()])));
        // Nothing is asked, yet the close arrives.
        assert!(answer(&mut service).is_err());
    }

    #[test]
    fn dropping_the_connection_closes_it_while_a_reply_is_awaited() {
        let directory = std::env::temp_dir().join(format!("td-setup-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let ours = directory.join("setup");
        let listener = UnixListener::bind(&ours).unwrap();
        let mut service = Service::connect(&ours).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        let mut theirs = [0; 8];
        peer.read_exact(&mut theirs).unwrap();
        peer.write_all(protocol::GREETING).unwrap();
        service.destinations().unwrap();
        let mut header = [0; 4];
        peer.read_exact(&mut header).unwrap();
        let mut request =
            vec![0; protocol::payload_len(header, protocol::MAX_REQUEST_BYTES).unwrap()];
        peer.read_exact(&mut request).unwrap();
        // The worker now waits on a reply that never comes.
        drop(service);
        peer.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn a_connection_dropped_while_connecting_sends_no_request() {
        let directory = std::env::temp_dir().join(format!("td-setup-early-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let ours = directory.join("setup");
        let listener = UnixListener::bind(&ours).unwrap();
        let mut service = Service::connect(&ours).unwrap();
        service.destinations().unwrap();
        // Most likely before the worker connects; either way the request
        // it was given must never reach a service.
        drop(service);
        let (mut peer, _) = listener.accept().unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        // Greet at once, so only the close keeps the request from coming.
        let _ = peer.write_all(protocol::GREETING);
        peer.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut received = Vec::new();
        // A reset, for the greeting the worker never read, is an end too.
        let _ = peer.read_to_end(&mut received);
        assert!(protocol::GREETING.starts_with(&received), "{received:?}");
    }

    /// A device-bound review, its recovery-key phase or that phase's
    /// failure is no review this installer shows: each ends the connection.
    #[test]
    fn device_bound_replies_end_the_connection() {
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        let unencrypted = plan(&disk(), &settings);
        let bound = Plan::new(
            *unencrypted.nonce(),
            disk(),
            *unencrypted.deployment(),
            *unencrypted.volume_uuid(),
            Storage::DeviceBound,
            settings.clone(),
        )
        .unwrap();
        let socket = intake("bound-review", usize::MAX, move |request| match request {
            Request::Propose { .. } => framed(&Reply::Reviewed(Box::new(bound.clone()))),
            _ => listing(request),
        });
        let mut service = Service::connect(&socket).unwrap();
        service.propose(disk(), settings.clone()).unwrap();
        assert_eq!(
            answer(&mut service).unwrap_err(),
            "installer service review is not unencrypted"
        );
        let nonce = ReviewNonce::new(*unencrypted.nonce()).unwrap();
        for state in [
            State::Running(nonce, Phase::RecoveryKey),
            State::Failed(nonce, Failure::RecoveryUnconfirmed),
        ] {
            let socket = intake("bound-status", usize::MAX, move |request| match request {
                Request::Status => framed(&Reply::Status(state)),
                _ => listing(request),
            });
            let mut service = Service::connect(&socket).unwrap();
            service.status().unwrap();
            assert_eq!(answer(&mut service).unwrap_err(), DEVICE_BOUND);
        }
    }

    #[test]
    fn execute_and_status_answer_the_service_state() {
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        let reviewed = plan(&disk(), &settings);
        let socket = fake("execute", protocol::GREETING, usize::MAX, listing);
        let mut service = Service::connect(&socket).unwrap();
        service.execute(reviewed).unwrap();
        assert_eq!(
            answer(&mut service),
            Ok(Answer::Standing(Standing::Review {
                nonce: [7; 32],
                stage: Stage::AwaitingConsent
            }))
        );
        service.status().unwrap();
        assert_eq!(answer(&mut service), Ok(Answer::Standing(Standing::Idle)));
        // A review not held is said as such; busy ends the connection.
        for (name, refusal) in [
            ("execute-none", Refusal::NoReview),
            ("execute-stale", Refusal::StaleReview),
            ("execute-busy", Refusal::Busy),
            ("execute-consent", Refusal::ConsentUnavailable),
        ] {
            let bytes = framed(&Reply::Refused(refusal));
            let socket = fake(name, protocol::GREETING, 1, move |_| bytes.clone());
            let mut service = Service::connect(&socket).unwrap();
            service.execute(plan(&disk(), &settings)).unwrap();
            let answered = answer(&mut service);
            match refusal {
                Refusal::Busy => assert!(answered.unwrap_err().contains("installing")),
                Refusal::ConsentUnavailable => {
                    assert_eq!(answered, Ok(Answer::Refused(refusal_text(refusal))))
                }
                Refusal::StaleReview => {
                    assert_eq!(answered, Ok(Answer::Stale(refusal_text(refusal))))
                }
                _ => assert_eq!(answered, Ok(Answer::Unheld(refusal_text(refusal)))),
            }
        }
        // Every state maps; the review's nonce goes with it.
        let nonce = ReviewNonce::new([3; 32]).unwrap();
        for (state, stage) in [
            (State::Reviewed(nonce), Stage::Reviewed),
            (
                State::Running(nonce, Phase::ApplyingSettings),
                Stage::Running(outcome::Phase::ApplyingSettings),
            ),
            (State::Complete(nonce), Stage::Complete),
            (
                State::Failed(nonce, Failure::WriteFailed),
                Stage::Failed(outcome::Failure::WriteFailed),
            ),
            (
                State::Abandoned(nonce, Abandon::ConsentDeclined),
                Stage::Abandoned("the installation was declined at the secure prompt"),
            ),
        ] {
            assert_eq!(
                standing(state),
                Ok(Standing::Review {
                    nonce: [3; 32],
                    stage
                })
            );
        }
        let texts: std::collections::BTreeSet<_> = Abandon::ALL
            .iter()
            .map(|abandon| abandon_text(*abandon))
            .collect();
        assert_eq!(texts.len(), Abandon::ALL.len());
    }

    #[test]
    fn every_refusal_has_its_own_text() {
        let texts: std::collections::BTreeSet<_> = Refusal::ALL
            .iter()
            .map(|refusal| refusal_text(*refusal))
            .collect();
        assert_eq!(texts.len(), Refusal::ALL.len());
        assert!(texts.iter().all(|text| text.is_ascii() && !text.is_empty()));
    }
}
