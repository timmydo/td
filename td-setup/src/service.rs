//! The installer's one connection to the installation service, reached
//! through td-authd's setup intake (td-authd/DESIGN.md "Whole-disk
//! installation intake"); td-install/INSTALLER.md "Installation service
//! protocol" is the wire. A worker thread connects and does the blocking
//! exchange, one request at a time, so the window's turn loop never waits on
//! the service. The connection holds no authority: the service checks every
//! request and erases nothing without trusted consent. A device-bound
//! installation's recovery key crosses it twice, sent once and typed back;
//! every buffer that held its digits is zeroed (INSTALLER.md "Device-bound
//! records").

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use td_install::installation_plan::{Destination, Plan, Settings, Storage};
use td_install::installation_protocol::{
    self as protocol, Abandon, Failure, Phase, RecoveryDigits, Refusal, Reply, Request,
    ReviewNonce, State,
};

use crate::outcome;
use crate::recovery::Key;

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
#[derive(Debug, Eq, PartialEq)]
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
    /// The device-bound installation's recovery key, sent this once.
    RecoveryKey(Key),
    /// The key typed back matched; the installation goes on to make the
    /// disk bootable.
    Confirmed,
    /// The key typed back is not the one sent; it may be typed again.
    Mismatched,
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
    /// A device-bound installation, written and verified, holds its
    /// recovery key until it is typed back, and then until the partition
    /// table is written.
    Recovery,
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

    /// Asks for the recovery key of the installation `nonce` names; the
    /// service sends it once.
    pub fn recovery_key(&mut self, nonce: [u8; 32]) -> Result<(), String> {
        self.send(Request::RecoveryKey(ReviewNonce::new(nonce)?))
    }

    /// Sends `typed`, the key typed back, for the service to compare with
    /// the one it sent. The digits go to the worker in a value zeroed when
    /// dropped, sent or not.
    pub fn confirm_recovery(&mut self, nonce: [u8; 32], typed: &Key) -> Result<(), String> {
        let nonce = ReviewNonce::new(nonce)?;
        let digits = typed.with_digits(RecoveryDigits::new)?;
        self.send(Request::ConfirmRecovery(nonce, digits))
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
    // The review last executed and its storage: only a device-bound one
    // has a recovery key.
    let mut executed = None;
    loop {
        match requests.recv_timeout(IDLE_CHECK) {
            Ok(request) => {
                let answer = exchange(&mut stream, &request, &mut executed)?;
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

/// The review execute last named, and its storage.
type Executed = Option<(ReviewNonce, Storage)>;

fn exchange(
    stream: &mut UnixStream,
    request: &Request,
    executed: &mut Executed,
) -> Result<Answer, String> {
    if let Request::Execute(plan) = request {
        *executed = Some((ReviewNonce::from(plan), plan.storage()));
    }
    // A request may carry typed-back recovery digits: the encoded message
    // and its frame are both zeroed, sent or not.
    let mut encoded = request.encode();
    let framed = protocol::frame(&encoded, protocol::MAX_REQUEST_BYTES);
    protocol::scrub(&mut encoded);
    let mut bytes = framed?;
    let sent = stream.write_all(&bytes);
    protocol::scrub(&mut bytes);
    sent.map_err(|error| format!("installer service request: {error}"))?;
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .map_err(|error| format!("installer service reply: {error}"))?;
    let mut payload = vec![0; protocol::payload_len(header, protocol::MAX_REPLY_BYTES)?];
    // A reply cut short may still hold some of a recovery key's digits.
    let reply = stream
        .read_exact(&mut payload)
        .map_err(|error| format!("installer service reply: {error}"))
        .and_then(|()| Reply::decode(&payload));
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
            // The storage is the service's own operand's, which no
            // request chooses; the review page says which it is.
            if plan.destination() != destination || plan.settings() != settings {
                return Err("installer service review does not match the proposal".into());
            }
            Ok(Answer::Reviewed(plan))
        }
        // The key is sent once, for the device-bound installation this
        // connection executed; anything else, refusal 16 included, leaves
        // no key to type back and ends the connection, which withdraws
        // the installation.
        (Request::RecoveryKey(nonce), Reply::RecoveryKey(sent, digits))
            if sent == *nonce && bound(*executed, *nonce) =>
        {
            Key::from_digits(digits.as_bytes()).map(Answer::RecoveryKey)
        }
        (Request::RecoveryKey(_), Reply::Refused(refusal)) => Err(format!(
            "the installer service refused the recovery key: {}",
            refusal_text(refusal)
        )),
        (Request::RecoveryKey(_), _) => {
            Err("the installer service did not send the recovery key".into())
        }
        // A match is answered with the phase still running: the table is
        // written next. A mismatch keeps the phase for another try; any
        // other answer leaves the outcome unknown.
        (
            Request::ConfirmRecovery(nonce, _),
            Reply::Status(State::Running(running, Phase::RecoveryKey)),
        ) if running == *nonce && bound(*executed, *nonce) => Ok(Answer::Confirmed),
        (Request::ConfirmRecovery(nonce, _), Reply::Refused(Refusal::RecoveryKeyMismatch))
            if bound(*executed, *nonce) =>
        {
            Ok(Answer::Mismatched)
        }
        (Request::ConfirmRecovery(..), Reply::Refused(refusal)) => Err(format!(
            "the installer service refused the typed-back recovery key: {}",
            refusal_text(refusal)
        )),
        (Request::ConfirmRecovery(..), _) => {
            Err("the installer service did not confirm the recovery key".into())
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
        (_, Reply::Status(state)) => standing(state, *executed).map(Answer::Standing),
        (_, Reply::Refused(refusal)) => Ok(Answer::Refused(refusal_text(refusal))),
        // Only propose is answered with a review and the recovery-key
        // request with a key, and both are matched above.
        (_, Reply::Reviewed(_) | Reply::RecoveryKey(..)) => {
            Err("installer service reply is not one this installer reads".into())
        }
    }
}

/// Whether the review `nonce` names was executed here as device-bound.
fn bound(executed: Executed, nonce: ReviewNonce) -> bool {
    executed == Some((nonce, Storage::DeviceBound))
}

/// The recovery-key phase and its failure are a device-bound
/// installation's: reported of any other review, either ends the
/// connection.
fn standing(state: State, executed: Executed) -> Result<Standing, String> {
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
            Phase::RecoveryKey if bound(executed, nonce) => return recovery_phase(nonce),
            Phase::RecoveryKey => return Err(DEVICE_BOUND.into()),
        }),
        State::Complete(_) => Stage::Complete,
        State::Failed(_, failure) => Stage::Failed(match failure {
            Failure::DestinationChanged => outcome::Failure::DestinationChanged,
            Failure::InsufficientSpace => outcome::Failure::InsufficientSpace,
            Failure::WriteFailed => outcome::Failure::WriteFailed,
            Failure::VerificationFailed => outcome::Failure::VerificationFailed,
            Failure::SettingsFailed => outcome::Failure::SettingsFailed,
            Failure::RecoveryUnconfirmed if bound(executed, nonce) => {
                outcome::Failure::RecoveryUnconfirmed
            }
            Failure::RecoveryUnconfirmed => return Err(DEVICE_BOUND.into()),
        }),
        State::Abandoned(_, abandon) => Stage::Abandoned(abandon_text(abandon)),
    };
    Ok(Standing::Review {
        nonce: *nonce.as_bytes(),
        stage,
    })
}

/// The recovery-key phase of the review `nonce` names.
fn recovery_phase(nonce: ReviewNonce) -> Result<Standing, String> {
    Ok(Standing::Review {
        nonce: *nonce.as_bytes(),
        stage: Stage::Recovery,
    })
}

const DEVICE_BOUND: &str =
    "the installer service reports a recovery key for a review not executed as device-bound";

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
    use td_install::installation_plan::{Basis, Candidates, DestinationObservation, Zones};

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
        stored(destination, settings, Storage::Unencrypted)
    }

    /// A device-bound service's review of `destination` with `settings`.
    pub(crate) fn bound_plan(destination: &Destination, settings: &Settings) -> Plan {
        stored(destination, settings, Storage::DeviceBound)
    }

    fn stored(destination: &Destination, settings: &Settings, storage: Storage) -> Plan {
        let uuid = [0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 0];
        Plan::new(
            [7; 32],
            destination.clone(),
            [9; 32],
            uuid,
            storage,
            Basis::default(),
            settings.clone(),
        )
        .unwrap()
    }

    /// td-protector's pinned counting key, the one the device-bound fake
    /// sends.
    pub(crate) const KEY: &[u8; 48] = b"000013005150010290015439020571025716030859035998";
    /// Its display form.
    pub(crate) const SHOWN: &str = "000013-005150-010290-015439-020571-025716-030859-035998";
    /// Another key whose every group checks.
    pub(crate) const OTHER_KEY: &[u8; 48] = b"000000000000000000000000000000000000000000000000";

    pub(crate) fn key(digits: &[u8]) -> Key {
        Key::from_digits(digits).unwrap()
    }

    /// As `listing`, but a device-bound service core's: its reviews are
    /// device-bound; executed, the first state asked is verifying boot,
    /// then the recovery key's phase, whose key it sends once (16 after)
    /// and compares with what is typed back (17 when it differs). Once a
    /// key typed back matches it reports the phase once more, then
    /// complete.
    pub(crate) fn recovering() -> impl FnMut(&Request) -> Option<Vec<u8>> + Send + 'static {
        let nonce = ReviewNonce::new([7; 32]).unwrap();
        let mut asked = 0;
        let mut sent = false;
        // Once confirmed: whether the phase was reported since.
        let mut finishing = None;
        move |request| match request {
            Request::Propose {
                destination,
                settings,
            } => framed(&Reply::Reviewed(Box::new(bound_plan(
                destination,
                settings,
            )))),
            Request::Status => {
                asked += 1;
                let state = match finishing {
                    Some(true) => State::Complete(nonce),
                    Some(false) => {
                        finishing = Some(true);
                        State::Running(nonce, Phase::RecoveryKey)
                    }
                    None if asked == 1 => State::Running(nonce, Phase::VerifyingBoot),
                    None => State::Running(nonce, Phase::RecoveryKey),
                };
                framed(&Reply::Status(state))
            }
            Request::RecoveryKey(named) if *named == nonce => {
                if std::mem::replace(&mut sent, true) {
                    framed(&Reply::Refused(Refusal::RecoveryKeySent))
                } else {
                    framed(&Reply::RecoveryKey(
                        nonce,
                        RecoveryDigits::new(KEY).unwrap(),
                    ))
                }
            }
            Request::ConfirmRecovery(named, digits)
                if *named == nonce && sent && finishing.is_none() =>
            {
                if *digits == RecoveryDigits::new(KEY).unwrap() {
                    // The next state is the phase once more, then complete.
                    finishing = Some(false);
                    framed(&Reply::Status(State::Running(nonce, Phase::RecoveryKey)))
                } else {
                    framed(&Reply::Refused(Refusal::RecoveryKeyMismatch))
                }
            }
            Request::RecoveryKey(_) | Request::ConfirmRecovery(..) => {
                framed(&Reply::Refused(Refusal::Busy))
            }
            _ => listing(request),
        }
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

    /// A device-bound review is shown as the service made it; the
    /// recovery key's phase or its failure, reported of a review not
    /// executed here as device-bound, ends the connection.
    #[test]
    fn a_recovery_phase_belongs_only_to_a_device_bound_execution() {
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        let socket = intake("bound-review", usize::MAX, recovering());
        let mut service = Service::connect(&socket).unwrap();
        service.propose(disk(), settings.clone()).unwrap();
        assert_eq!(
            answer(&mut service),
            Ok(Answer::Reviewed(Box::new(bound_plan(&disk(), &settings))))
        );
        let nonce = ReviewNonce::new([7; 32]).unwrap();
        for (name, executed) in [
            ("bound-unasked", None),
            ("bound-unencrypted", Some(plan(&disk(), &settings))),
        ] {
            for state in [
                State::Running(nonce, Phase::RecoveryKey),
                State::Failed(nonce, Failure::RecoveryUnconfirmed),
            ] {
                let socket = intake(name, usize::MAX, move |request| match request {
                    Request::Status => framed(&Reply::Status(state)),
                    _ => listing(request),
                });
                let mut service = Service::connect(&socket).unwrap();
                if let Some(plan) = executed.clone() {
                    service.execute(plan).unwrap();
                    assert!(answer(&mut service).is_ok());
                }
                service.status().unwrap();
                assert_eq!(answer(&mut service).unwrap_err(), DEVICE_BOUND, "{name}");
            }
        }
        // Executed as device-bound, the phase and its failure are said.
        let socket = intake("bound-failed", usize::MAX, |request| match request {
            Request::Status => framed(&Reply::Status(State::Failed(
                ReviewNonce::new([7; 32]).unwrap(),
                Failure::RecoveryUnconfirmed,
            ))),
            _ => listing(request),
        });
        let mut service = Service::connect(&socket).unwrap();
        service.execute(bound_plan(&disk(), &settings)).unwrap();
        assert!(answer(&mut service).is_ok());
        service.status().unwrap();
        assert_eq!(
            answer(&mut service),
            Ok(Answer::Standing(Standing::Review {
                nonce: [7; 32],
                stage: Stage::Failed(outcome::Failure::RecoveryUnconfirmed),
            }))
        );
    }

    /// The key is asked for once and arrives in its groups; a key typed
    /// back that differs is a mismatch to try again, and the right one is
    /// confirmed with the phase still running.
    #[test]
    fn the_recovery_key_is_sent_once_and_confirmed_when_typed_back() {
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        let socket = intake("recovery", usize::MAX, recovering());
        let mut service = Service::connect(&socket).unwrap();
        service.execute(bound_plan(&disk(), &settings)).unwrap();
        assert!(answer(&mut service).is_ok());
        let recovering = Ok(Answer::Standing(Standing::Review {
            nonce: [7; 32],
            stage: Stage::Recovery,
        }));
        service.status().unwrap();
        assert!(answer(&mut service).is_ok());
        service.status().unwrap();
        assert_eq!(answer(&mut service), recovering);
        // A zero nonce names no installation and is not sent.
        assert!(service.recovery_key([0; 32]).is_err());
        service.recovery_key([7; 32]).unwrap();
        let Ok(Answer::RecoveryKey(sent)) = answer(&mut service) else {
            panic!("no recovery key");
        };
        assert_eq!(sent, key(KEY));
        assert_eq!(sent.display().as_str(), SHOWN);
        assert_eq!(format!("{sent:?}"), "Key(..)");
        service.confirm_recovery([7; 32], &key(OTHER_KEY)).unwrap();
        assert_eq!(answer(&mut service), Ok(Answer::Mismatched));
        service.status().unwrap();
        assert_eq!(answer(&mut service), recovering);
        service.confirm_recovery([7; 32], &sent).unwrap();
        assert_eq!(answer(&mut service), Ok(Answer::Confirmed));
        // The phase stands until the table is written, then completes.
        service.status().unwrap();
        assert_eq!(answer(&mut service), recovering);
        service.status().unwrap();
        assert_eq!(
            answer(&mut service),
            Ok(Answer::Standing(Standing::Review {
                nonce: [7; 32],
                stage: Stage::Complete,
            }))
        );
    }

    /// Anything but the key, sent once for this device-bound execution, or
    /// anything but a match or a mismatch for the key typed back, ends the
    /// connection: refusal 16 among them, since no key is left to show.
    #[test]
    fn a_recovery_answer_out_of_place_ends_the_connection() {
        let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC").unwrap();
        let nonce = ReviewNonce::new([7; 32]).unwrap();
        let mut wrong_check = *KEY;
        if let Some(last) = wrong_check.last_mut() {
            *last = b'9';
        }
        type Ask = fn(&mut Service) -> Result<(), String>;
        let ask_key: Ask = |service| service.recovery_key([7; 32]);
        let confirm: Ask = |service| service.confirm_recovery([7; 32], &key(KEY));
        let cases: Vec<(&str, Ask, Reply, &str)> = vec![
            (
                "key-sent",
                ask_key,
                Reply::Refused(Refusal::RecoveryKeySent),
                "the recovery key was already shown",
            ),
            (
                "key-busy",
                ask_key,
                Reply::Refused(Refusal::Busy),
                "refused the recovery key",
            ),
            (
                "key-other",
                ask_key,
                Reply::RecoveryKey(
                    ReviewNonce::new([8; 32]).unwrap(),
                    RecoveryDigits::new(KEY).unwrap(),
                ),
                "did not send the recovery key",
            ),
            (
                "key-check",
                ask_key,
                Reply::RecoveryKey(nonce, RecoveryDigits::new(&wrong_check).unwrap()),
                "group 8 has a wrong check digit",
            ),
            (
                "key-status",
                ask_key,
                Reply::Status(State::Running(nonce, Phase::RecoveryKey)),
                "does not answer its request",
            ),
            (
                "confirm-busy",
                confirm,
                Reply::Refused(Refusal::Busy),
                "refused the typed-back recovery key",
            ),
            (
                "confirm-stale",
                confirm,
                Reply::Refused(Refusal::StaleReview),
                "the review is out of date",
            ),
            (
                "confirm-phase",
                confirm,
                Reply::Status(State::Running(nonce, Phase::VerifyingBoot)),
                "did not confirm",
            ),
            (
                "confirm-complete",
                confirm,
                Reply::Status(State::Complete(nonce)),
                "did not confirm",
            ),
            (
                "confirm-other",
                confirm,
                Reply::Status(State::Running(
                    ReviewNonce::new([8; 32]).unwrap(),
                    Phase::RecoveryKey,
                )),
                "did not confirm",
            ),
        ];
        for (name, ask, reply, said) in cases {
            let bytes = framed(&reply);
            let socket = intake(name, usize::MAX, move |request| match request {
                Request::RecoveryKey(_) | Request::ConfirmRecovery(..) => bytes.clone(),
                _ => listing(request),
            });
            let mut service = Service::connect(&socket).unwrap();
            service.execute(bound_plan(&disk(), &settings)).unwrap();
            assert!(answer(&mut service).is_ok());
            ask(&mut service).unwrap();
            let ended = answer(&mut service).unwrap_err();
            assert!(ended.contains(said), "{name}: {ended}");
            assert!(!ended.contains("000013"), "{name}: {ended}");
        }
        // The key of a review executed unencrypted is no key to show, and
        // a key typed back for it is not confirmed.
        for (name, ask) in [
            ("key-unencrypted", ask_key),
            ("confirm-unencrypted", confirm),
        ] {
            let socket = intake(name, usize::MAX, move |request| match request {
                Request::RecoveryKey(_) => framed(&Reply::RecoveryKey(
                    ReviewNonce::new([7; 32]).unwrap(),
                    RecoveryDigits::new(KEY).unwrap(),
                )),
                Request::ConfirmRecovery(..) => framed(&Reply::Status(State::Running(
                    ReviewNonce::new([7; 32]).unwrap(),
                    Phase::RecoveryKey,
                ))),
                _ => listing(request),
            });
            let mut service = Service::connect(&socket).unwrap();
            service.execute(plan(&disk(), &settings)).unwrap();
            assert!(answer(&mut service).is_ok());
            ask(&mut service).unwrap();
            assert!(answer(&mut service).is_err(), "{name}");
        }
        // A mismatch is one only for the device-bound review executed
        // here: refused of an unencrypted execution, of another nonce or
        // before any execution, it ends the connection.
        type Before = fn(&mut Service, &Settings);
        let unencrypted: Before = |service, settings| {
            service.execute(plan(&disk(), settings)).unwrap();
            assert!(answer(service).is_ok());
        };
        let device_bound: Before = |service, settings| {
            service.execute(bound_plan(&disk(), settings)).unwrap();
            assert!(answer(service).is_ok());
        };
        let nothing: Before = |_, _| {};
        for (name, before, named) in [
            ("mismatch-unencrypted", unencrypted, [7; 32]),
            ("mismatch-other", device_bound, [8; 32]),
            ("mismatch-unexecuted", nothing, [7; 32]),
        ] {
            let socket = intake(name, usize::MAX, |request| match request {
                Request::ConfirmRecovery(..) => {
                    framed(&Reply::Refused(Refusal::RecoveryKeyMismatch))
                }
                _ => listing(request),
            });
            let mut service = Service::connect(&socket).unwrap();
            before(&mut service, &settings);
            service.confirm_recovery(named, &key(KEY)).unwrap();
            let ended = answer(&mut service).unwrap_err();
            assert!(ended.contains("refused the typed-back"), "{name}: {ended}");
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
                standing(state, None),
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
