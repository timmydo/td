//! The unprivileged installer's typed channel to the root installation
//! service. Decoding grants no authority: the service authenticates the
//! source, establishes eligibility and holds its disk claim itself, and
//! consent reaches it only from the compositor, never through this channel.

use crate::installation_plan::{
    put_destination, put_settings, read_destination, read_settings, Candidates, Destination, Plan,
    Reader, Settings, MAX_BYTES, MAX_CANDIDATE_BYTES,
};

/// Sent and required by both ends before the first frame. A change to any
/// message or its bytes changes the greeting; there is no negotiation.
pub const GREETING: &[u8; 8] = b"TDINS01\n";
/// Root admits only this much from the unprivileged side.
pub const MAX_REQUEST_BYTES: usize = 1 + MAX_BYTES;
pub const MAX_REPLY_BYTES: usize = 1 + MAX_CANDIDATE_BYTES;

const DESTINATIONS: u8 = 0x01;
const PROPOSE: u8 = 0x02;
const EXECUTE: u8 = 0x03;
const STATUS: u8 = 0x04;
const WITHDRAW: u8 = 0x05;
// Replies set the high bit, so a reflected frame never decodes.
const DESTINATIONS_REPLY: u8 = 0x81;
const REVIEWED: u8 = 0x82;
const STATUS_REPLY: u8 = 0x83;
const REFUSED: u8 = 0x84;

pub fn check_greeting(received: &[u8; 8]) -> Result<(), String> {
    if received == GREETING {
        Ok(())
    } else {
        Err("unsupported installation protocol greeting".into())
    }
}

/// One frame: a big-endian u32 length, then the payload.
pub fn frame(payload: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    if payload.is_empty() || payload.len() > limit {
        return Err("installation frame outside its bound".into());
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| "installation frame outside its bound")?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// The admitted payload length, checked before the caller allocates for it.
pub fn payload_len(header: [u8; 4], limit: usize) -> Result<usize, String> {
    match usize::try_from(u32::from_be_bytes(header)) {
        Ok(length) if length != 0 && length <= limit => Ok(length),
        _ => Err("installation frame outside its bound".into()),
    }
}

/// Names one review the service retains: the nonce of its plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewNonce([u8; 32]);

impl ReviewNonce {
    pub fn new(bytes: [u8; 32]) -> Result<Self, String> {
        if bytes == [0; 32] {
            return Err("installation review nonce cannot be zero".into());
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<&Plan> for ReviewNonce {
    fn from(plan: &Plan) -> Self {
        // Plan admission already refuses a zero nonce.
        Self(*plan.nonce())
    }
}

/// The operation a running installation reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    PreparingDisk,
    WritingFilesystems,
    PublishingDeployment,
    ApplyingSettings,
    VerifyingBoot,
}

impl Phase {
    pub const ALL: &[Self] = &[
        Self::PreparingDisk,
        Self::WritingFilesystems,
        Self::PublishingDeployment,
        Self::ApplyingSettings,
        Self::VerifyingBoot,
    ];

    fn code(self) -> u8 {
        match self {
            Self::PreparingDisk => 1,
            Self::WritingFilesystems => 2,
            Self::PublishingDeployment => 3,
            Self::ApplyingSettings => 4,
            Self::VerifyingBoot => 5,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::PreparingDisk),
            2 => Ok(Self::WritingFilesystems),
            3 => Ok(Self::PublishingDeployment),
            4 => Ok(Self::ApplyingSettings),
            5 => Ok(Self::VerifyingBoot),
            _ => Err("unknown installation phase".into()),
        }
    }
}

/// Why an installation stopped after destructive writes may have begun.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    DestinationChanged,
    InsufficientSpace,
    WriteFailed,
    VerificationFailed,
    SettingsFailed,
}

impl Failure {
    pub const ALL: &[Self] = &[
        Self::DestinationChanged,
        Self::InsufficientSpace,
        Self::WriteFailed,
        Self::VerificationFailed,
        Self::SettingsFailed,
    ];

    fn code(self) -> u8 {
        match self {
            Self::DestinationChanged => 1,
            Self::InsufficientSpace => 2,
            Self::WriteFailed => 3,
            Self::VerificationFailed => 4,
            Self::SettingsFailed => 5,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::DestinationChanged),
            2 => Ok(Self::InsufficientSpace),
            3 => Ok(Self::WriteFailed),
            4 => Ok(Self::VerificationFailed),
            5 => Ok(Self::SettingsFailed),
            _ => Err("unknown installation failure".into()),
        }
    }
}

/// Why a review ended before any destructive write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Abandon {
    Withdrawn,
    ConsentDeclined,
    ConsentExpired,
    /// The compositor's consent path was lost while it was displayed.
    ConsentUnavailable,
    /// The disk changed or vanished before the first write.
    DestinationChanged,
}

impl Abandon {
    pub const ALL: &[Self] = &[
        Self::Withdrawn,
        Self::ConsentDeclined,
        Self::ConsentExpired,
        Self::ConsentUnavailable,
        Self::DestinationChanged,
    ];

    fn code(self) -> u8 {
        match self {
            Self::Withdrawn => 1,
            Self::ConsentDeclined => 2,
            Self::ConsentExpired => 3,
            Self::ConsentUnavailable => 4,
            Self::DestinationChanged => 5,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::Withdrawn),
            2 => Ok(Self::ConsentDeclined),
            3 => Ok(Self::ConsentExpired),
            4 => Ok(Self::ConsentUnavailable),
            5 => Ok(Self::DestinationChanged),
            _ => Err("unknown installation abandonment".into()),
        }
    }
}

/// Why the service refused a request. Nothing was written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Busy,
    SourceUnavailable,
    DiscoveryFailed,
    DestinationChanged,
    DestinationBusy,
    InsufficientSpace,
    InvalidUsername,
    InvalidHostname,
    UnsupportedKeyboard,
    UnsupportedTimezone,
    StaleReview,
    NoReview,
    /// No seat, compositor or trusted consent path can present the review.
    ConsentUnavailable,
}

impl Refusal {
    pub const ALL: &[Self] = &[
        Self::Busy,
        Self::SourceUnavailable,
        Self::DiscoveryFailed,
        Self::DestinationChanged,
        Self::DestinationBusy,
        Self::InsufficientSpace,
        Self::InvalidUsername,
        Self::InvalidHostname,
        Self::UnsupportedKeyboard,
        Self::UnsupportedTimezone,
        Self::StaleReview,
        Self::NoReview,
        Self::ConsentUnavailable,
    ];

    fn code(self) -> u8 {
        match self {
            Self::Busy => 1,
            Self::SourceUnavailable => 2,
            Self::DiscoveryFailed => 3,
            Self::DestinationChanged => 4,
            Self::DestinationBusy => 5,
            Self::InsufficientSpace => 6,
            Self::InvalidUsername => 7,
            Self::InvalidHostname => 8,
            Self::UnsupportedKeyboard => 9,
            Self::UnsupportedTimezone => 10,
            Self::StaleReview => 11,
            Self::NoReview => 12,
            Self::ConsentUnavailable => 13,
        }
    }

    // A repeated code would silently shadow a variant.
    #[deny(unreachable_patterns)]
    fn from_code(code: u8) -> Result<Self, String> {
        match code {
            1 => Ok(Self::Busy),
            2 => Ok(Self::SourceUnavailable),
            3 => Ok(Self::DiscoveryFailed),
            4 => Ok(Self::DestinationChanged),
            5 => Ok(Self::DestinationBusy),
            6 => Ok(Self::InsufficientSpace),
            7 => Ok(Self::InvalidUsername),
            8 => Ok(Self::InvalidHostname),
            9 => Ok(Self::UnsupportedKeyboard),
            10 => Ok(Self::UnsupportedTimezone),
            11 => Ok(Self::StaleReview),
            12 => Ok(Self::NoReview),
            13 => Ok(Self::ConsentUnavailable),
            _ => Err("unknown installation refusal".into()),
        }
    }
}

/// What the installer asks. None of it names a path, executable, source or
/// consent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Eligible whole disks, as the service observes them.
    Destinations,
    /// Review one destination with these settings. The service chooses the
    /// nonce, volume UUID and authenticated source, and claims the disk.
    Propose {
        destination: Destination,
        settings: Settings,
    },
    /// Ask the service to seek consent for the review it retains. The plan
    /// must equal that review; equality is a precondition, not consent.
    Execute(Plan),
    Status,
    /// Release a review that has not started, and its disk claim.
    Withdraw(ReviewNonce),
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Destinations => out.push(DESTINATIONS),
            Self::Propose {
                destination,
                settings,
            } => {
                out.push(PROPOSE);
                put_destination(&mut out, destination);
                put_settings(&mut out, settings);
            }
            Self::Execute(plan) => {
                out.push(EXECUTE);
                out.extend_from_slice(&plan.encode());
            }
            Self::Status => out.push(STATUS),
            Self::Withdraw(nonce) => {
                out.push(WITHDRAW);
                out.extend_from_slice(nonce.as_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("installation request exceeds wire bound".into());
        }
        let (&tag, body) = bytes.split_first().ok_or("empty installation request")?;
        let mut reader = Reader::new(body, "request");
        let request = match tag {
            DESTINATIONS => Self::Destinations,
            PROPOSE => Self::Propose {
                destination: read_destination(&mut reader)?,
                settings: read_settings(&mut reader)?,
            },
            // The plan record checks its own version, bound and end.
            EXECUTE => return Ok(Self::Execute(Plan::decode(body)?)),
            STATUS => Self::Status,
            WITHDRAW => Self::Withdraw(ReviewNonce::new(reader.array()?)?),
            _ => return Err("unknown installation request".into()),
        };
        reader.finish()?;
        Ok(request)
    }
}

/// The service's view of the one review it may hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Idle,
    /// Reviewed and claimed; nothing written.
    Reviewed(ReviewNonce),
    /// Waiting for compositor-owned consent; nothing written.
    AwaitingConsent(ReviewNonce),
    Running(ReviewNonce, Phase),
    /// Durable publication and verified boot artifacts.
    Complete(ReviewNonce),
    /// Stopped; the disk may be incomplete, and retrying needs a new review.
    Failed(ReviewNonce, Failure),
    /// Ended before any destructive write.
    Abandoned(ReviewNonce, Abandon),
}

impl State {
    pub fn review(&self) -> Option<ReviewNonce> {
        match *self {
            Self::Idle => None,
            Self::Reviewed(nonce)
            | Self::AwaitingConsent(nonce)
            | Self::Running(nonce, _)
            | Self::Complete(nonce)
            | Self::Failed(nonce, _)
            | Self::Abandoned(nonce, _) => Some(nonce),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    Destinations(Candidates),
    /// The service's review, made while it holds the destination claim.
    Reviewed(Box<Plan>),
    Status(State),
    Refused(Refusal),
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Destinations(candidates) => {
                out.push(DESTINATIONS_REPLY);
                out.extend_from_slice(&candidates.encode());
            }
            Self::Reviewed(plan) => {
                out.push(REVIEWED);
                out.extend_from_slice(&plan.encode());
            }
            Self::Status(state) => {
                out.push(STATUS_REPLY);
                let (code, detail) = match *state {
                    State::Idle => (0, None),
                    State::Reviewed(_) => (1, None),
                    State::AwaitingConsent(_) => (2, None),
                    State::Running(_, phase) => (3, Some(phase.code())),
                    State::Complete(_) => (4, None),
                    State::Failed(_, failure) => (5, Some(failure.code())),
                    State::Abandoned(_, cause) => (6, Some(cause.code())),
                };
                out.push(code);
                if let Some(nonce) = state.review() {
                    out.extend_from_slice(nonce.as_bytes());
                }
                out.extend(detail);
            }
            Self::Refused(refusal) => {
                out.push(REFUSED);
                out.push(refusal.code());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_REPLY_BYTES {
            return Err("installation reply exceeds wire bound".into());
        }
        let (&tag, body) = bytes.split_first().ok_or("empty installation reply")?;
        let reply = match tag {
            DESTINATIONS_REPLY => Self::Destinations(Candidates::decode(body)?),
            REVIEWED => Self::Reviewed(Box::new(Plan::decode(body)?)),
            STATUS_REPLY => Self::Status(read_state(body)?),
            REFUSED => {
                let mut reader = Reader::new(body, "reply");
                let code = byte(&mut reader)?;
                reader.finish()?;
                Self::Refused(Refusal::from_code(code)?)
            }
            _ => return Err("unknown installation reply".into()),
        };
        Ok(reply)
    }

    /// Whether this reply has a shape `request` admits. The caller still
    /// compares the review nonce and the proposed or echoed fields.
    pub fn answers(&self, request: &Request) -> bool {
        match request {
            Request::Destinations => matches!(self, Self::Destinations(_) | Self::Refused(_)),
            Request::Propose { .. } => matches!(self, Self::Reviewed(_) | Self::Refused(_)),
            Request::Execute(_) | Request::Withdraw(_) => {
                matches!(self, Self::Status(_) | Self::Refused(_))
            }
            Request::Status => matches!(self, Self::Status(_)),
        }
    }
}

fn read_state(body: &[u8]) -> Result<State, String> {
    let mut reader = Reader::new(body, "reply");
    let code = byte(&mut reader)?;
    if code > 6 {
        return Err("unknown installation state".into());
    }
    let state = if code == 0 {
        State::Idle
    } else {
        let nonce = ReviewNonce::new(reader.array()?)?;
        match code {
            1 => State::Reviewed(nonce),
            2 => State::AwaitingConsent(nonce),
            3 => State::Running(nonce, Phase::from_code(byte(&mut reader)?)?),
            4 => State::Complete(nonce),
            5 => State::Failed(nonce, Failure::from_code(byte(&mut reader)?)?),
            6 => State::Abandoned(nonce, Abandon::from_code(byte(&mut reader)?)?),
            _ => return Err("unknown installation state".into()),
        }
    };
    reader.finish()?;
    Ok(state)
}

fn byte(reader: &mut Reader<'_>) -> Result<u8, String> {
    let [value] = reader.array()?;
    Ok(value)
}
