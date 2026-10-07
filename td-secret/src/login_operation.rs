//! The root login-key worker (td-login/TOKEN-LOGIN.md): session unlock,
//! first enrollment, key addition and key removal. Nothing in production
//! starts it yet.

use crate::consent::{
    Admitted, Fingerprint, LoginStep, Operation, Request, Slot, LOGIN_TWO_CEREMONIES,
};
use crate::fido_ctap;
use crate::fido_device::{self, Interruption};
use crate::fido_p256::PublicKey;
use crate::fido_pin::{EnrolledCredential, LoginRefusal, Pin};
use crate::fido_transaction::{
    Channel, Error, LoginAssertion, LoginCreation, LoginError, LoginPin, Status, Transaction,
};
use crate::login_record::{self, NewKey, Phase, Record};
use crate::login_store::{self, Baseline, Cause, Outcome, Owner, State, Store};
use crate::login_tier;
use crate::operation::{self, remaining, Wire};
use crate::portable::VerificationKey;
use crate::store;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// How often a connect step looks for the new key.
const POLL: Duration = Duration::from_millis(100);
/// What a commit round must leave of the operation deadline, at its
/// invitation and again after root's acknowledgement: the write and the
/// success frame then finish while root still waits for them.
const COMMIT_MARGIN: Duration = Duration::from_secs(5);
/// How long each retained deployment's marker read may take: the worker
/// reads two, well inside its 120-second ceiling (TOKEN-LOGIN.md,
/// "Deployments").
const RETAINED_GIVE_UP: Duration = Duration::from_secs(10);
/// The worker's first frame: the login state it read, before any token I/O.
const BASELINE: u8 = 0x18;
/// The worker's typed failure frame.
const FAILURE: u8 = 0x15;
const UNENROLLED: u8 = 0;
const ENROLLED: u8 = 1;

/// Why an operation failed, as root shows it. The frame is `0x15`, the
/// kind byte, and for some kinds one more byte; td-secret/DESIGN.md pins them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    /// The key's retry count as it reported it before this attempt; td
    /// never infers what remains from a status.
    WrongPin(u8),
    PinAuthBlocked,
    PinBlocked,
    /// The connected key's identify found no enrolled credential.
    NotEnrolled,
    /// None or several FIDO devices were connected, or a device other
    /// than the one this key's sessions used.
    OneKey,
    Refused(LoginRefusal),
    /// The new key's silent probe did not select its credential, as a
    /// credProtect default hides it.
    Unprobed,
    /// The key denied presence.
    Denied,
    Timeout,
    NoRecord,
    Unavailable(Cause),
    /// The record no longer reads as the baseline the worker presented.
    Changed,
    /// The rename or unlink was attempted: what the re-read found.
    Uncertain(Found),
    /// Any other token or verification failure.
    Failed,
    /// td's own processes or channel, not the key.
    Internal,
    /// The key already holds a credential this operation excludes: an
    /// enrolled one, or one created earlier in it.
    Excluded,
    /// No record version this build and both retained deployments read.
    Version,
}

/// What the re-read after an uncertain write found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Absent,
    /// The baseline record.
    Old,
    /// The record this operation built.
    New,
    /// Neither.
    Other,
    Unavailable,
}

impl Failure {
    fn frame(self) -> Vec<u8> {
        let (kind, detail) = match self {
            Self::WrongPin(retries) => (1, Some(retries)),
            Self::PinAuthBlocked => (2, None),
            Self::PinBlocked => (3, None),
            Self::NotEnrolled => (4, None),
            Self::OneKey => (5, None),
            Self::Refused(refusal) => (
                6,
                Some(match refusal {
                    LoginRefusal::NoHmacSecret => 1,
                    LoginRefusal::AlwaysUv => 2,
                    LoginRefusal::PinUnsupported => 3,
                    LoginRefusal::PinNotSet => 4,
                    LoginRefusal::ListTooSmall => 5,
                }),
            ),
            // Detail 6 is reserved for Unprobed: a new refusal skips it.
            Self::Unprobed => (6, Some(6)),
            Self::Denied => (7, None),
            Self::Timeout => (8, None),
            Self::NoRecord => (9, None),
            Self::Unavailable(Cause::DirectoryDamaged) => (10, None),
            Self::Unavailable(Cause::RecordDamaged) => (11, None),
            Self::Unavailable(Cause::Unreadable) => (12, None),
            Self::Changed => (13, None),
            Self::Uncertain(found) => (
                14,
                Some(match found {
                    Found::Absent => 1,
                    Found::Old => 2,
                    Found::New => 3,
                    Found::Other => 4,
                    Found::Unavailable => 5,
                }),
            ),
            Self::Failed => (15, None),
            Self::Internal => (16, None),
            Self::Excluded => (17, None),
            Self::Version => (18, None),
        };
        let mut frame = vec![FAILURE, kind];
        frame.extend(detail);
        frame
    }
}

/// What the worker reads besides root's frames and the keys.
struct Context<'a> {
    uid: u32,
    /// Where the record lives and who must own it: root's in production.
    path: &'a Path,
    owner: Owner,
    /// The operation's deadline counts from here.
    started: Instant,
    /// Each token session's longest life: the transport's in production.
    lifetime: Duration,
    /// What a commit round must leave of the deadline: `COMMIT_MARGIN` in
    /// production.
    margin: Duration,
    /// The record versions the current and previous deployments read.
    retained: &'a dyn Retained,
    /// The store's write; tests inject failures at its stages.
    write: &'a dyn Fn(&Store, Baseline, Change<'_>) -> Outcome,
}

/// Where the record versions the retained deployments read come from.
trait Retained {
    /// The current deployment's, then the previous one's; a deployment
    /// whose marker does not verify reads none.
    fn reads(&self) -> [Vec<u8>; 2];
}

/// The tier markers of the deployments the volume's `current` and
/// `previous` selectors name (TOKEN-LOGIN.md, "Deployments"), their files
/// owned by `owner`: `/run/td-volume/td` and root in production.
struct Volume<'a> {
    path: &'a Path,
    owner: u32,
}

impl Retained for Volume<'_> {
    fn reads(&self) -> [Vec<u8>; 2] {
        [login_tier::CURRENT, login_tier::PREVIOUS].map(|slot| {
            login_tier::retained(self.path, slot, self.owner, RETAINED_GIVE_UP).unwrap_or_default()
        })
    }
}

/// What a commit writes against the baseline.
enum Change<'a> {
    /// The record, through a temporary named by these random bytes.
    Publish(&'a Record, [u8; 16]),
    /// Removing every key unlinks the record.
    Remove,
}

fn write(store: &Store, baseline: Baseline, change: Change<'_>) -> Outcome {
    match change {
        Change::Publish(record, suffix) => store.publish(baseline, record, &mut &suffix[..]),
        Change::Remove => store.remove(baseline),
    }
}

/// Token access: the root USB transport, or virtual keys in tests.
trait Devices {
    type Node: Copy + Eq;
    type Channel: Channel;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String>;
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Channel, String>;
    /// Waits between a connect step's discoveries.
    fn pause(&mut self, time: Duration);
}

/// Each session's HID worker holds `/run/td-fido/operation.lock`.
struct Physical;
impl Devices for Physical {
    type Node = fido_device::Device;
    type Channel = fido_device::Session;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String> {
        fido_device::Device::discover()
    }
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Channel, String> {
        fido_device::Session::open(node, deadline)
    }
    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}

pub(super) fn run(uid: u32) -> Result<(), String> {
    let stream = operation::startup()?;
    let started = Instant::now();
    // The longest ceiling until the description names the operation.
    let deadline = started
        .checked_add(LOGIN_TWO_CEREMONIES)
        .ok_or("login operation deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    let context = Context {
        uid,
        path: Path::new(login_store::DIRECTORY),
        owner: Owner::ROOT,
        started,
        lifetime: fido_device::MAX_LIFETIME,
        margin: COMMIT_MARGIN,
        retained: &Volume {
            path: Path::new(login_tier::VOLUME),
            owner: 0,
        },
        write: &write,
    };
    let result = match File::open("/dev/urandom") {
        Ok(mut random) => operate(
            &mut wire,
            &context,
            &mut Physical,
            &mut |bytes| {
                random
                    .read_exact(bytes)
                    .map_err(|_| "read kernel entropy".into())
            },
            store::require_protected_memory,
        ),
        Err(_) => report(&mut wire, Err(Failure::Internal)),
    };
    result.map_err(|failure| format!("login operation failed: {failure:?}"))
}

/// The operation, then its failure frame. Root may be gone, so that frame
/// is best effort; after the operation deadline it is not sent at all.
/// `memory` is the no-swap, zero-core-dump check a PIN holder needs.
fn operate<D: Devices>(
    wire: &mut Wire,
    context: &Context<'_>,
    devices: &mut D,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    memory: impl FnOnce() -> Result<(), String>,
) -> Result<(), Failure> {
    let result = memory()
        .map_err(|_| Failure::Internal)
        .and_then(|()| perform(wire, context, devices, entropy));
    report(wire, result)
}

fn report(wire: &mut Wire, result: Result<(), Failure>) -> Result<(), Failure> {
    // An ended session no longer bounds the failure frame.
    wire.session(None);
    if let Err(failure) = result {
        let _ = wire.send(&failure.frame());
    }
    result
}

fn perform<D: Devices>(
    wire: &mut Wire,
    context: &Context<'_>,
    devices: &mut D,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<(), Failure> {
    let state = login_store::read(context.path, context.owner, context.uid);
    let presented = match (&state, state.baseline()) {
        (State::Unavailable(cause), _) => return Err(Failure::Unavailable(*cause)),
        (_, Some(presented)) => presented,
        (_, None) => return Err(Failure::Internal),
    };
    let record = match state {
        State::Enrolled(record) => Some(record),
        State::Unenrolled | State::Unavailable(_) => None,
    };
    let fingerprints: Vec<Fingerprint> = record
        .iter()
        .flat_map(|record| record.slots().iter().map(|slot| slot.fingerprint()))
        .collect();
    let mut frame = vec![BASELINE];
    match record {
        None => frame.push(UNENROLLED),
        Some(_) => {
            frame.push(ENROLLED);
            frame.push(u8::try_from(fingerprints.len()).map_err(|_| Failure::Internal)?);
            frame.extend(fingerprints.iter().flatten());
        }
    }
    wire.send(&frame).map_err(|_| framing(wire))?;
    let frame = wire.receive_cleared().map_err(|_| framing(wire))?;
    let request = Request::decode(frame.bytes()).map_err(|_| Failure::Internal)?;
    drop(frame);
    if request.owner() != context.uid {
        return Err(Failure::Internal);
    }
    let operation = request.operation().clone();
    // One ceremony, or two for an addition and a two-key enrollment
    // (td-authd/DESIGN.md, "Login keys", Deadlines).
    let ceiling = request.login_ceiling().ok_or(Failure::Internal)?;
    wire.limit(
        context
            .started
            .checked_add(ceiling)
            .ok_or(Failure::Internal)?,
    );
    let enrolling = matches!(operation, Operation::LoginEnroll { .. });
    if record.is_none() && !enrolling {
        return Err(Failure::NoRecord);
    }
    let begun = Request::begin_login(
        *request.nonce(),
        request.owner(),
        operation.clone(),
        &fingerprints,
    )
    .map_err(|_| Failure::Internal)?;
    if begun != request {
        return Err(Failure::Internal);
    }
    let ceremony = Ceremony {
        context,
        presented,
        fingerprints: &fingerprints,
        current: request,
        last: false,
    };
    match (operation, record) {
        (Operation::LoginUnlock { .. }, Some(record)) => {
            ceremony.unlock(wire, &record, devices, entropy)
        }
        (Operation::LoginEnroll { after, .. }, None) => {
            let version = version(context)?;
            ceremony.enroll(wire, after, version, devices, entropy)
        }
        (Operation::LoginAdd { .. }, Some(record)) => {
            let version = version(context)?;
            ceremony.add(wire, record, version, devices, entropy)
        }
        (Operation::LoginRemove { after, removed, .. }, Some(record)) => {
            // Removing every key unlinks the record and writes no version.
            let version = if after == 0 {
                None
            } else {
                Some(version(context)?)
            };
            ceremony.remove(wire, record, &removed, version, devices, entropy)
        }
        _ => Err(Failure::Internal),
    }
}

/// The version a write that leaves a record uses, chosen before any token
/// I/O from the retained deployments' markers, read only here: an unlock
/// and a removal of every key need none.
fn version(context: &Context<'_>) -> Result<u8, Failure> {
    let [current, previous] = context.retained.reads();
    login_record::write_version(&current, &previous).map_err(|_| Failure::Version)
}

/// A step to present, as root will admit it.
#[derive(Clone, Copy)]
struct Step {
    step: LoginStep,
    /// An enrollment's new-key ordinal; one otherwise.
    ordinal: u8,
    /// The credential this key's ceremony created, from its prove step on.
    created: Option<Fingerprint>,
    /// `Last` exactly for the operation's final step.
    admitted: Admitted,
}

impl Step {
    fn of(step: LoginStep, admitted: Admitted) -> Self {
        Self {
            step,
            ordinal: 1,
            created: None,
            admitted,
        }
    }
}

/// One new key's ceremony: the device its connect step found, its
/// enrollment ordinal and what its creation must exclude.
struct Fresh<'a, N> {
    node: N,
    ordinal: u8,
    excluded: &'a [&'a [u8]],
    /// Whether its probe is the operation's final step.
    admitted: Admitted,
}

/// One operation against the baseline the worker presented.
struct Ceremony<'a> {
    context: &'a Context<'a>,
    presented: Baseline,
    /// The record's slot fingerprints in canonical order; none when unenrolled.
    fingerprints: &'a [Fingerprint],
    /// The last step presented, which root admits every next one against.
    current: Request,
    /// Whether that step was the operation's final one, after which only
    /// its commit round may follow.
    last: bool,
}

impl Ceremony<'_> {
    fn unlock<D: Devices>(
        mut self,
        wire: &mut Wire,
        record: &Record,
        devices: &mut D,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), Failure> {
        self.begin(wire)?;
        self.enrolled(wire, record, devices, entropy, Admitted::Last)?;
        self.unchanged()?;
        self.round(wire)?;
        wire.send(&[0x14]).map_err(|_| framing(wire))
    }

    /// Each new key in turn, then the record they make.
    fn enroll<D: Devices>(
        mut self,
        wire: &mut Wire,
        after: u8,
        version: u8,
        devices: &mut D,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), Failure> {
        self.begin(wire)?;
        let mut made: Vec<EnrolledCredential> = Vec::with_capacity(usize::from(after));
        let mut previous = None;
        for ordinal in 1..=after {
            // Root's own first step is the first key's connect.
            if ordinal > 1 {
                let connect = Step {
                    ordinal,
                    ..Step::of(LoginStep::Connect, Admitted::Next)
                };
                self.present(wire, connect)?;
            }
            let node = connected(devices, previous, wire)?;
            let excluded: Vec<&[u8]> = made.iter().map(EnrolledCredential::id).collect();
            let admitted = if ordinal == after {
                Admitted::Last
            } else {
                Admitted::Next
            };
            let key = Fresh {
                node,
                ordinal,
                excluded: &excluded,
                admitted,
            };
            let created = self.new_key(wire, devices, key, entropy)?;
            made.push(created);
            previous = Some(node);
        }
        let id = fresh(entropy)?;
        let keys = made.iter().map(slot).collect::<Result<Vec<_>, _>>()?;
        let record =
            Record::enroll(self.context.uid, id, version, keys).map_err(|_| Failure::Failed)?;
        drop(made);
        self.commit(wire, Some(&record), entropy)
    }

    /// Authorized by an enrolled key, then one new key.
    fn add<D: Devices>(
        mut self,
        wire: &mut Wire,
        record: Record,
        version: u8,
        devices: &mut D,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), Failure> {
        self.begin(wire)?;
        let authorizing = self.enrolled(wire, &record, devices, entropy, Admitted::Next)?;
        self.present(wire, Step::of(LoginStep::Connect, Admitted::Next))?;
        let node = connected(devices, Some(authorizing), wire)?;
        let excluded: Vec<&[u8]> = record
            .slots()
            .iter()
            .map(|slot| slot.credential())
            .collect();
        let key = Fresh {
            node,
            ordinal: 1,
            excluded: &excluded,
            admitted: Admitted::Last,
        };
        let created = self.new_key(wire, devices, key, entropy)?;
        drop(excluded);
        let record = record
            .with_key(version, slot(&created)?)
            .map_err(|_| Failure::Failed)?;
        drop(created);
        self.commit(wire, Some(&record), entropy)
    }

    /// Authorized by an enrolled key, which may be among those removed.
    fn remove<D: Devices>(
        mut self,
        wire: &mut Wire,
        record: Record,
        removed: &[Slot],
        version: Option<u8>,
        devices: &mut D,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), Failure> {
        self.begin(wire)?;
        self.enrolled(wire, &record, devices, entropy, Admitted::Last)?;
        let Some(version) = version else {
            return self.commit(wire, None, entropy);
        };
        // begin_login matched each position's fingerprint to the baseline.
        let credentials = removed
            .iter()
            .map(|slot| {
                usize::from(slot.position)
                    .checked_sub(1)
                    .and_then(|index| record.slots().get(index))
                    .map(|slot| slot.credential().to_vec())
                    .ok_or(Failure::Internal)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let credentials: Vec<&[u8]> = credentials.iter().map(Vec::as_slice).collect();
        let record = record
            .without(version, &credentials)
            .map_err(|_| Failure::Internal)?;
        self.commit(wire, Some(&record), entropy)
    }

    /// Presents root's own first step, then requires the record unchanged
    /// before any token I/O.
    fn begin(&mut self, wire: &mut Wire) -> Result<(), Failure> {
        wire.acknowledge(0x10, 0x11, &self.current)
            .map_err(|_| framing(wire))?;
        self.unchanged()
    }

    /// Presents `next`, admitted exactly as root admits it: the worker never
    /// presents a step root would refuse.
    fn present(&mut self, wire: &mut Wire, next: Step) -> Result<(), Failure> {
        let request = Request::new(
            *self.current.nonce(),
            self.current.owner(),
            stepped(self.current.operation(), next.step, next.ordinal)?,
        )
        .map_err(|_| Failure::Internal)?;
        if self
            .current
            .admit_login_step(&request, self.fingerprints, next.created)
            != Ok(next.admitted)
        {
            return Err(Failure::Internal);
        }
        wire.acknowledge(0x10, 0x11, &request)
            .map_err(|_| framing(wire))?;
        self.current = request;
        self.last = next.admitted == Admitted::Last;
        Ok(())
    }

    /// Presents a PIN step with the key's reported retries, takes the PIN,
    /// and returns the client-data hash bound to that exact step.
    fn pin(
        &mut self,
        wire: &mut Wire,
        next: Step,
        phase: Phase,
        record: Option<&Record>,
        random: &[u8; 32],
    ) -> Result<(Pin, [u8; 32]), Failure> {
        self.present(wire, next)?;
        let pin = wire.receive_pin().map_err(|_| framing(wire))?;
        let hash = login_record::client_data_hash(phase, &self.current.encode(), record, random)
            .map_err(|_| Failure::Internal)?;
        Ok((pin, hash))
    }

    /// Identify, then the selected slot's PIN assertion: an unlock, or an
    /// addition's or removal's authorization, whose hash binds the record.
    /// Returns the device both sessions used.
    fn enrolled<D: Devices>(
        &mut self,
        wire: &mut Wire,
        record: &Record,
        devices: &mut D,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
        admitted: Admitted,
    ) -> Result<D::Node, Failure> {
        let node = one(devices)?;
        let random = fresh(entropy)?;
        let hash =
            login_record::client_data_hash(Phase::Identify, &self.current.encode(), None, &random)
                .map_err(|_| Failure::Internal)?;
        let ids: Vec<&[u8]> = record
            .slots()
            .iter()
            .map(|slot| slot.credential())
            .collect();
        let channel = open(devices, node, wire, self.context.lifetime)?;
        let selected = Transaction::new(channel)
            .map_err(LoginError::from)
            .and_then(|transaction| transaction.identify(&ids, hash));
        let selected = ended(wire, selected, None, None)?;
        let slot = match selected {
            None => return Err(Failure::NotEnrolled),
            Some(index) => record.slots().get(index).ok_or(Failure::Internal)?,
        };
        // A key swapped or added since identify refuses before any PIN.
        if one(devices)? != node {
            return Err(Failure::OneKey);
        }
        let (x, y) = slot.key().coordinates();
        let key = PublicKey::from_coordinates(&x, &y).map_err(|_| Failure::Internal)?;
        let random = fresh(entropy)?;
        let unlock = matches!(self.current.operation(), Operation::LoginUnlock { .. });
        let fingerprint = slot.fingerprint();
        let channel = open(devices, node, wire, self.context.lifetime)?;
        let mut reported = None;
        let mut cause = None;
        let result = Transaction::new(channel)
            .map_err(LoginError::from)
            .and_then(|transaction| {
                transaction.login_assertion(
                    LoginAssertion {
                        credential: slot.credential(),
                        key,
                        salt: *slot.salt(),
                    },
                    &mut |_, retries| {
                        let (step, phase, bound) = if unlock {
                            let step = LoginStep::Unlock {
                                key: fingerprint,
                                retries,
                            };
                            (step, Phase::Unlock, None)
                        } else {
                            let step = LoginStep::Authorize {
                                key: fingerprint,
                                retries,
                            };
                            (step, Phase::Authorize, Some(record))
                        };
                        let next = Step::of(step, admitted);
                        match self.pin(wire, next, phase, bound, &random) {
                            Ok(answer) => {
                                reported = Some(retries);
                                Ok(answer)
                            }
                            Err(failure) => {
                                cause = Some(failure);
                                Err("login PIN step failed".into())
                            }
                        }
                    },
                    entropy,
                )
            });
        let output = ended(wire, result, cause, reported)?;
        let verified = <&[u8; 32]>::try_from(output.bytes())
            .map_err(|_| Failure::Internal)
            .and_then(|output| {
                record
                    .check(slot.credential(), output)
                    .map_err(|_| Failure::Internal)
            })?;
        // The output is retired at once: nothing is released.
        drop(output);
        if !verified {
            return Err(Failure::Failed);
        }
        Ok(node)
    }

    /// Create and prove on one session, repeat on a new one, then probe:
    /// every key is all four before anything is published.
    fn new_key<D: Devices>(
        &mut self,
        wire: &mut Wire,
        devices: &mut D,
        new: Fresh<'_, D::Node>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<EnrolledCredential, Failure> {
        let Fresh {
            node,
            ordinal,
            excluded,
            admitted,
        } = new;
        let user = fresh(entropy)?;
        let salt = fresh(entropy)?;
        let (create_random, prove_random) = (fresh(entropy)?, fresh(entropy)?);
        let channel = open(devices, node, wire, self.context.lifetime)?;
        let mut reported = None;
        let mut cause = None;
        let result = Transaction::new(channel)
            .map_err(LoginError::from)
            .and_then(|transaction| {
                transaction.login_create(
                    LoginCreation {
                        user,
                        salt,
                        excluded,
                    },
                    &mut |pin, retries| {
                        let (next, phase, random) = match pin {
                            LoginPin::Creation => {
                                let step = LoginStep::Create { retries };
                                (
                                    Step::of(step, Admitted::Next),
                                    Phase::Create,
                                    &create_random,
                                )
                            }
                            LoginPin::Proof(credential) => {
                                let key = fido_ctap::fingerprint(credential);
                                let step = LoginStep::Prove { key, retries };
                                let next = Step {
                                    created: Some(key),
                                    ..Step::of(step, Admitted::Next)
                                };
                                (next, Phase::Prove, &prove_random)
                            }
                        };
                        let next = Step { ordinal, ..next };
                        match self.pin(wire, next, phase, None, random) {
                            Ok(answer) => {
                                reported = Some(retries);
                                Ok(answer)
                            }
                            Err(failure) => {
                                cause = Some(failure);
                                Err("login PIN step failed".into())
                            }
                        }
                    },
                    entropy,
                )
            });
        let created = ended(wire, result, cause, reported)?;
        let key = fido_ctap::fingerprint(created.id());
        let made = |step, admitted| Step {
            ordinal,
            created: Some(key),
            ..Step::of(step, admitted)
        };
        // The repeat: a new session must reproduce the identical secret.
        if one(devices)? != node {
            return Err(Failure::OneKey);
        }
        let public = public(&created)?;
        let random = fresh(entropy)?;
        let channel = open(devices, node, wire, self.context.lifetime)?;
        let mut reported = None;
        let mut cause = None;
        let result = Transaction::new(channel)
            .map_err(LoginError::from)
            .and_then(|transaction| {
                transaction.login_assertion(
                    LoginAssertion {
                        credential: created.id(),
                        key: public,
                        salt,
                    },
                    &mut |_, retries| {
                        let next = made(LoginStep::Repeat { key, retries }, Admitted::Next);
                        match self.pin(wire, next, Phase::Repeat, None, &random) {
                            Ok(answer) => {
                                reported = Some(retries);
                                Ok(answer)
                            }
                            Err(failure) => {
                                cause = Some(failure);
                                Err("login PIN step failed".into())
                            }
                        }
                    },
                    entropy,
                )
            });
        let output = ended(wire, result, cause, reported)?;
        let repeated = same(output.bytes(), created.output().bytes());
        drop(output);
        if !repeated {
            return Err(Failure::Failed);
        }
        // The probe: a later unlock must be able to select this credential
        // silently, which a credProtect default prevents. It runs over the
        // excluded credentials too, in record order, so for an addition and
        // an enrollment's last key it meets the batches an unlock of the
        // record holding it would send.
        self.present(wire, made(LoginStep::Probe { key }, admitted))?;
        if one(devices)? != node {
            return Err(Failure::OneKey);
        }
        let random = fresh(entropy)?;
        let hash =
            login_record::client_data_hash(Phase::Probe, &self.current.encode(), None, &random)
                .map_err(|_| Failure::Internal)?;
        let mut ids: Vec<&[u8]> = excluded.to_vec();
        ids.push(created.id());
        ids.sort_unstable();
        let new = ids.iter().position(|id| *id == created.id());
        let channel = open(devices, node, wire, self.context.lifetime)?;
        let selected = Transaction::new(channel)
            .map_err(LoginError::from)
            .and_then(|transaction| transaction.identify(&ids, hash));
        if ended(wire, selected, None, None)? != new {
            return Err(Failure::Unprobed);
        }
        Ok(created)
    }

    /// The commit round over the final step, then the write: the new record
    /// published, or none, which unlinks it. Never retried.
    fn commit(
        mut self,
        wire: &mut Wire,
        record: Option<&Record>,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), Failure> {
        self.round(wire)?;
        let mut suffix = [0; 16];
        entropy(&mut suffix).map_err(|_| Failure::Internal)?;
        // Immediately before publication, as before any token I/O. A store
        // that cannot be opened has not been shown to differ.
        let store = Store::open(self.context.path, self.context.owner, self.context.uid)
            .map_err(Failure::Unavailable)?;
        if store.read().baseline() != Some(self.presented) {
            return Err(Failure::Changed);
        }
        let change = match record {
            Some(record) => Change::Publish(record, suffix),
            None => Change::Remove,
        };
        match (self.context.write)(&store, self.presented, change) {
            Outcome::Committed => wire.send(&[0x14]).map_err(|_| framing(wire)),
            // Nothing changed at the record name: a record that no longer
            // reads as the baseline changed under the write.
            Outcome::Rejected(_) => Err(if self.unchanged().is_ok() {
                Failure::Failed
            } else {
                Failure::Changed
            }),
            Outcome::Uncertain(_) => Err(Failure::Uncertain(
                self.found(record.map(|record| record.digest())),
            )),
        }
    }

    /// The commit round over the operation's final step, sent and
    /// acknowledged with the context's margin of the deadline left.
    fn round(&mut self, wire: &mut Wire) -> Result<(), Failure> {
        if !self.last {
            return Err(Failure::Internal);
        }
        margin(wire, self.context.margin)?;
        wire.acknowledge(0x12, 0x13, &self.current)
            .map_err(|_| framing(wire))?;
        margin(wire, self.context.margin)
    }

    /// The record must still read as the presented baseline; an unavailable
    /// state never matches.
    fn unchanged(&self) -> Result<(), Failure> {
        let state = login_store::read(self.context.path, self.context.owner, self.context.uid);
        if state.baseline() == Some(self.presented) {
            Ok(())
        } else {
            Err(Failure::Changed)
        }
    }

    /// What a re-read finds after an uncertain write; `new` is the digest of
    /// the record it published, none for an unlink.
    fn found(&self, new: Option<[u8; 32]>) -> Found {
        match login_store::read(self.context.path, self.context.owner, self.context.uid) {
            State::Unavailable(_) => Found::Unavailable,
            State::Unenrolled => Found::Absent,
            State::Enrolled(record) if Baseline::Digest(record.digest()) == self.presented => {
                Found::Old
            }
            State::Enrolled(record) if Some(record.digest()) == new => Found::New,
            State::Enrolled(_) => Found::Other,
        }
    }
}

/// The operation with its step, and an enrollment's new-key ordinal, replaced.
fn stepped(operation: &Operation, step: LoginStep, ordinal: u8) -> Result<Operation, Failure> {
    Ok(match operation.clone() {
        Operation::LoginUnlock {
            account,
            before,
            after,
            ..
        } => Operation::LoginUnlock {
            account,
            before,
            after,
            step,
        },
        Operation::LoginEnroll {
            account,
            before,
            after,
            ..
        } => Operation::LoginEnroll {
            account,
            before,
            after,
            key: ordinal,
            step,
        },
        Operation::LoginAdd {
            account,
            before,
            after,
            ..
        } => Operation::LoginAdd {
            account,
            before,
            after,
            step,
        },
        Operation::LoginRemove {
            account,
            before,
            after,
            removed,
            ..
        } => Operation::LoginRemove {
            account,
            before,
            after,
            removed,
            step,
        },
        _ => return Err(Failure::Internal),
    })
}

/// A proved key as a record slot; its output stays in `created`'s owner.
fn slot(created: &EnrolledCredential) -> Result<NewKey<'_>, Failure> {
    Ok(NewKey {
        credential: created.id().to_vec(),
        key: public(created)?,
        salt: *created.salt(),
        output: <&[u8; 32]>::try_from(created.output().bytes()).map_err(|_| Failure::Failed)?,
    })
}

/// The new credential's key, in the notebook's canonical COSE form.
fn public(created: &EnrolledCredential) -> Result<PublicKey, Failure> {
    VerificationKey::from_cose(created.cose())
        .and_then(|key| key.public_key())
        .map_err(|_| Failure::Failed)
}

/// Constant-time equality of two hmac-secret outputs.
fn same(a: &[u8], b: &[u8]) -> bool {
    let difference = a
        .iter()
        .zip(b)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    a.len() == b.len() && std::hint::black_box(difference) == 0
}

/// Exactly one connected device, refused before any PIN otherwise.
fn one<D: Devices>(devices: &mut D) -> Result<D::Node, Failure> {
    match devices.discover().map_err(|_| Failure::Failed)?.as_slice() {
        [node] => Ok(*node),
        _ => Err(Failure::OneKey),
    }
}

/// A connect step's key: waits within the deadline until exactly one device
/// is connected and it is not the one the previous session used. Several
/// refuse, as everywhere; none, or only the previous key, wait.
fn connected<D: Devices>(
    devices: &mut D,
    previous: Option<D::Node>,
    wire: &Wire,
) -> Result<D::Node, Failure> {
    loop {
        let left = remaining(wire.deadline()).map_err(|_| Failure::Timeout)?;
        match devices.discover().map_err(|_| Failure::Failed)?.as_slice() {
            [node] if Some(*node) != previous => return Ok(*node),
            [] | [_] => devices.pause(POLL.min(left)),
            _ => return Err(Failure::OneKey),
        }
    }
}

/// A session within its lifetime and the operation deadline. Until
/// `ended`, its deadline bounds every wait on root, and a failure past it
/// is a timeout, a key that stalls initialization included.
fn open<D: Devices>(
    devices: &mut D,
    node: D::Node,
    wire: &mut Wire,
    lifetime: Duration,
) -> Result<D::Channel, Failure> {
    let deadline = Instant::now()
        .checked_add(lifetime)
        .map_or(wire.deadline(), |lifetime| lifetime.min(wire.deadline()));
    wire.session(Some(deadline));
    devices.open(node, deadline).map_err(|_| {
        let failure = expired_or(wire, Failure::Failed);
        wire.session(None);
        failure
    })
}

/// A session's result, classified while its deadline still bounds the wire,
/// and the session ended. A PIN step's own failure is `cause`.
fn ended<T>(
    wire: &mut Wire,
    result: Result<T, LoginError>,
    cause: Option<Failure>,
    reported: Option<u8>,
) -> Result<T, Failure> {
    let result = match result {
        Ok(value) => Ok(value),
        Err(LoginError::Failed(Error::PinInput)) => Err(cause.unwrap_or(Failure::Internal)),
        Err(error) => Err(token(error, reported, wire)),
    };
    wire.session(None);
    result
}

/// TIMEOUT unless `margin` of the operation deadline remains.
fn margin(wire: &Wire, margin: Duration) -> Result<(), Failure> {
    match remaining(wire.deadline()) {
        Ok(left) if left >= margin => Ok(()),
        _ => Err(Failure::Timeout),
    }
}

fn fresh(entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>) -> Result<[u8; 32], Failure> {
    let mut random = [0; 32];
    entropy(&mut random).map_err(|_| Failure::Internal)?;
    Ok(random)
}

/// A failed frame: a deadline is a timeout, anything else root's fault or loss.
fn framing(wire: &Wire) -> Failure {
    if wire.expired() {
        Failure::Timeout
    } else {
        Failure::Internal
    }
}

/// A deadline that has passed, an open session's included, is a timeout.
fn expired_or(wire: &Wire, failure: Failure) -> Failure {
    if remaining(wire.bound()).is_err() {
        Failure::Timeout
    } else {
        failure
    }
}

/// Typed CTAP statuses select the kind, never diagnostic text. A wrong PIN
/// is one only after this operation asked for it.
fn token(error: LoginError, reported: Option<u8>, wire: &Wire) -> Failure {
    match error {
        LoginError::Refused(refusal) => Failure::Refused(refusal),
        LoginError::Failed(error) => match error {
            Error::Status(Status::PinInvalid) => {
                reported.map_or(Failure::Failed, Failure::WrongPin)
            }
            Error::Status(Status::PinAuthBlocked) => Failure::PinAuthBlocked,
            Error::Status(Status::PinBlocked) => Failure::PinBlocked,
            Error::Status(Status::Denied) => Failure::Denied,
            Error::Status(Status::CredentialExcluded) => Failure::Excluded,
            Error::Status(Status::TouchTimeout | Status::ActionTimeout)
            | Error::Interrupted(Interruption::Expired) => Failure::Timeout,
            Error::Entropy | Error::PinInput => Failure::Internal,
            Error::Status(_) | Error::Interrupted(_) | Error::Transport | Error::Protocol(_) => {
                expired_or(wire, Failure::Failed)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent::LOGIN_CEREMONY;
    use crate::crypto;
    use crate::fido_cbor::{self as cbor, Value};
    use crate::fido_ctap::fingerprint;
    use crate::fido_virtual::{Config, Link, Output, Presence, Script, Signing, Virtual};
    use crate::login_record::{READS, VERSION};
    use std::fs::{self, OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Fixed read sets, the current deployment's then the previous one's.
    impl Retained for (&[u8], &[u8]) {
        fn reads(&self) -> [Vec<u8>; 2] {
            [self.0.to_vec(), self.1.to_vec()]
        }
    }

    const UID: u32 = 1000;
    const NONCE: [u8; 32] = [42; 32];
    const PIN: &[u8] = b"1234";
    /// Each operation's deadline: tens of seconds beyond what any needs,
    /// even under load.
    const TIME: Duration = Duration::from_secs(120);
    const WRONG: &[u8] = b"4321";

    /// A deterministic counter: draw N is SHA-256 of N, big-endian.
    fn entropy() -> impl FnMut(&mut [u8]) -> Result<(), String> {
        entropy_after(0)
    }

    /// `entropy`, its first draw `start + 1`: another operation's.
    fn entropy_after(start: u32) -> impl FnMut(&mut [u8]) -> Result<(), String> {
        let mut count = start;
        move |out| {
            for chunk in out.chunks_mut(32) {
                count += 1;
                chunk.copy_from_slice(&draw(count)[..chunk.len()]);
            }
            Ok(())
        }
    }

    fn draw(count: u32) -> [u8; 32] {
        crypto::digest(&count.to_be_bytes())
    }

    struct Fixture {
        root: PathBuf,
        dir: PathBuf,
        owner: Owner,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-login-operation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let dir = root.join("login");
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            let meta = fs::metadata(&root).unwrap();
            let owner = Owner {
                uid: meta.uid(),
                gid: meta.gid(),
            };
            Self { root, dir, owner }
        }

        /// Both retained deployments read this build's versions.
        fn context(&self) -> Context<'_> {
            Context {
                uid: UID,
                path: &self.dir,
                owner: self.owner,
                started: Instant::now(),
                lifetime: fido_device::MAX_LIFETIME,
                margin: COMMIT_MARGIN,
                retained: &(READS, READS),
                write: &write,
            }
        }

        /// Publishes `bytes` as the record by rename, as the store does.
        fn seed(&self, bytes: &[u8]) {
            replace(&self.dir, Some(bytes));
        }

        fn state(&self) -> State {
            login_store::read(&self.dir, self.owner, UID)
        }

        /// The stored slots' credentials in canonical order; none unenrolled.
        fn stored(&self) -> Option<Vec<Vec<u8>>> {
            match self.state() {
                State::Unenrolled => None,
                State::Enrolled(record) => Some(
                    record
                        .slots()
                        .iter()
                        .map(|slot| slot.credential().to_vec())
                        .collect(),
                ),
                State::Unavailable(cause) => panic!("login state unavailable: {cause:?}"),
            }
        }

        fn bytes(&self) -> Option<Vec<u8>> {
            fs::read(self.dir.join(UID.to_string())).ok()
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn fingerprints(&self) -> Vec<Fingerprint> {
            self.stored()
                .unwrap_or_default()
                .iter()
                .map(|id| fingerprint(id))
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Replaces the record by rename, or removes it.
    fn replace(dir: &Path, bytes: Option<&[u8]>) {
        let path = dir.join(UID.to_string());
        let Some(bytes) = bytes else {
            let _ = fs::remove_file(path);
            return;
        };
        let temporary = dir.join("next");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .unwrap();
        file.set_permissions(Permissions::from_mode(0o600)).unwrap();
        file.write_all(bytes).unwrap();
        fs::rename(&temporary, path).unwrap();
    }

    /// A virtual key holding one login credential, and what a record keeps of it.
    struct Key {
        device: Virtual,
        id: Vec<u8>,
        x: [u8; 32],
        y: [u8; 32],
        salt: [u8; 32],
        output: [u8; 32],
    }

    impl Key {
        fn new(seed: u8) -> Self {
            let device = blank(&format!("login-{seed}"));
            let salt = [seed ^ 0x33; 32];
            let created = Transaction::new(device.link())
                .unwrap()
                .login_create(
                    LoginCreation {
                        user: [seed ^ 0x55; 32],
                        salt,
                        excluded: &[],
                    },
                    &mut |step, _| {
                        let hash = match step {
                            LoginPin::Creation => [seed; 32],
                            LoginPin::Proof(_) => [seed ^ 0xaa; 32],
                        };
                        Ok((Pin::new(PIN.into())?, hash))
                    },
                    &mut entropy(),
                )
                .unwrap();
            let cose = cbor::decode(created.cose()).unwrap();
            let coordinate = |key| -> [u8; 32] {
                cose.required(&Value::Negative(key))
                    .unwrap()
                    .bytes()
                    .unwrap()
                    .try_into()
                    .unwrap()
            };
            Self {
                id: created.id().to_vec(),
                x: coordinate(1),
                y: coordinate(2),
                salt,
                output: created.output().bytes().try_into().unwrap(),
                device,
            }
        }

        fn fingerprint(&self) -> Fingerprint {
            fingerprint(&self.id)
        }

        fn pin_tokens(&self) -> usize {
            pin_tokens(&self.device)
        }

        fn hashes(&self) -> Vec<Vec<u8>> {
            sent(&self.device, 2, 2)
        }
    }

    /// A key with its PIN set and no credential.
    fn blank(seed: &str) -> Virtual {
        Virtual::new(Config::default(), Some(PIN), seed)
    }

    /// The newest credential a key holds.
    fn newest(device: &Virtual) -> Vec<u8> {
        device.state().credentials.last().unwrap().id.clone()
    }

    /// How many PIN tokens the key was asked for.
    fn pin_tokens(device: &Virtual) -> usize {
        device
            .transcript()
            .iter()
            .filter(|(request, _)| {
                request[0] == 6
                    && matches!(
                        cbor::decode(&request[1..])
                            .unwrap()
                            .required(&Value::Unsigned(2))
                            .unwrap()
                            .unsigned(),
                        Ok(5 | 9)
                    )
            })
            .count()
    }

    /// One byte-string field of every request with this command byte, in
    /// order: makeCredential's (1) or getAssertion's (2) client-data hash.
    fn sent(device: &Virtual, command: u8, key: u64) -> Vec<Vec<u8>> {
        device
            .transcript()
            .iter()
            .filter(|(request, _)| request[0] == command)
            .map(|(request, _)| {
                cbor::decode(&request[1..])
                    .unwrap()
                    .required(&Value::Unsigned(key))
                    .unwrap()
                    .bytes()
                    .unwrap()
                    .to_vec()
            })
            .collect()
    }

    /// The allow list of every getAssertion the key received, in order.
    fn allow_lists(device: &Virtual) -> Vec<Vec<Vec<u8>>> {
        device
            .transcript()
            .iter()
            .filter(|(request, _)| request[0] == 2)
            .map(|(request, _)| {
                let value = cbor::decode(&request[1..]).unwrap();
                let Value::Array(items) = value.required(&Value::Unsigned(3)).unwrap() else {
                    panic!("an allow list")
                };
                items
                    .iter()
                    .map(|item| {
                        item.required(&Value::Text("id"))
                            .unwrap()
                            .bytes()
                            .unwrap()
                            .to_vec()
                    })
                    .collect()
            })
            .collect()
    }

    /// Whether `hash` is `phase`'s client-data hash over exactly `request`,
    /// with one of the worker's draws.
    fn binds(hash: &[u8], phase: Phase, request: &Request, record: Option<&Record>) -> bool {
        (1..=400).any(|count| {
            login_record::client_data_hash(phase, &request.encode(), record, &draw(count)).unwrap()
                == hash
        })
    }

    /// One slot per key; `verifier` and `public` replace what each slot keeps.
    fn record(keys: &[&Key], verifier: Option<[u8; 32]>, public: Option<&Key>) -> Vec<u8> {
        let outputs: Vec<[u8; 32]> = keys
            .iter()
            .map(|key| verifier.unwrap_or(key.output))
            .collect();
        let slots = keys
            .iter()
            .zip(&outputs)
            .map(|(key, output)| {
                let public = public.unwrap_or(key);
                NewKey {
                    credential: key.id.clone(),
                    key: PublicKey::from_coordinates(&public.x, &public.y).unwrap(),
                    salt: key.salt,
                    output,
                }
            })
            .collect();
        Record::enroll(UID, [7; 32], VERSION, slots)
            .unwrap()
            .encode()
            .unwrap()
    }

    /// Fingerprints in the record's canonical credential order.
    fn canonical(keys: &[&Key]) -> Vec<Fingerprint> {
        let mut ids: Vec<&[u8]> = keys.iter().map(|key| key.id.as_slice()).collect();
        ids.sort();
        ids.into_iter().map(fingerprint).collect()
    }

    /// Virtual keys behind device nodes. Several nodes may share one key, as
    /// a key removed and inserted again gets a new node.
    struct Keys<'a> {
        nodes: Vec<&'a Virtual>,
        /// The nodes connected before session N opens; the last repeats.
        connected: Vec<Vec<usize>>,
        /// Discovery answers a connect step sees first, by sessions opened.
        waits: Vec<(usize, Vec<usize>)>,
        /// Applied to a node's key as session N opens.
        scripts: Vec<(usize, Script)>,
        /// Session N's initialization fails: at its deadline when true, as a
        /// key stalling it does, otherwise at once.
        stall: Option<(usize, bool)>,
        opened: usize,
        pauses: usize,
        deadlines: Vec<Instant>,
    }

    impl<'a> Keys<'a> {
        /// These nodes, every one connected throughout.
        fn all(nodes: &[&'a Virtual]) -> Self {
            Self::sessions(nodes, vec![(0..nodes.len()).collect()])
        }

        fn sessions(nodes: &[&'a Virtual], connected: Vec<Vec<usize>>) -> Self {
            Self {
                nodes: nodes.to_vec(),
                connected,
                waits: Vec::new(),
                scripts: Vec::new(),
                stall: None,
                opened: 0,
                pauses: 0,
                deadlines: Vec::new(),
            }
        }
    }

    impl Devices for Keys<'_> {
        type Node = usize;
        type Channel = Link;
        fn discover(&mut self) -> Result<Vec<usize>, String> {
            if let Some(at) = self.waits.iter().position(|(n, _)| *n == self.opened) {
                return Ok(self.waits.remove(at).1);
            }
            Ok(self
                .connected
                .get(self.opened)
                .or(self.connected.last())
                .cloned()
                .unwrap_or_default())
        }
        fn open(&mut self, node: usize, deadline: Instant) -> Result<Link, String> {
            for (at, script) in &self.scripts {
                if *at == self.opened {
                    self.nodes[node].script(*script);
                }
            }
            self.opened += 1;
            self.deadlines.push(deadline);
            if let Some((at, wait)) = self.stall {
                if at + 1 == self.opened {
                    if wait {
                        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    }
                    return Err("HID initialization failed".into());
                }
            }
            Ok(self.nodes[node].link())
        }
        fn pause(&mut self, _: Duration) {
            self.pauses += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum At {
        /// Right after the baseline frame, before the description.
        Baseline,
        /// At the operation's final presented step.
        Last,
        /// At the commit invitation.
        Commit,
    }

    /// What root does beyond acknowledging every invitation it admits.
    #[derive(Clone, Default)]
    struct Plan {
        /// The operation root begins: an unlock of the baseline's keys when none.
        operation: Option<Operation>,
        /// Raw bytes root writes after acknowledging each PIN step; none
        /// cancels at the first PIN step instead, closing the channel
        /// unacknowledged.
        pin: Option<Vec<u8>>,
        /// The PIN step that gets the wrong PIN instead.
        wrong: Option<LoginStep>,
        /// Answers nothing from the first invitation on.
        silent: bool,
        /// Replaces the record (none removes it) at that point.
        change: Option<(PathBuf, Option<Vec<u8>>, At)>,
        /// Sent instead of the first step root derives.
        description: Option<Vec<u8>>,
        /// Sends the PIN frame in place of this invitation's acknowledgement.
        early_pin: Option<usize>,
        /// Closes the channel instead of acknowledging this invitation.
        cancel: Option<usize>,
        /// Holds each PIN this long after its step's acknowledgement, sending
        /// it only if the worker has not ended the operation by then.
        pin_delay: Option<Duration>,
        /// Makes the record directory group-readable, which damages it, at
        /// that point.
        damage: Option<(PathBuf, At)>,
        /// Called with each step the worker presents after root's own
        /// first, once root admits it and before its acknowledgement: a
        /// guest swaps its keys at a connect step.
        admitted: Option<OnStep>,
    }

    type OnStep = std::sync::Arc<dyn Fn(&Request) + Send + Sync>;

    fn pin(bytes: &[u8]) -> Plan {
        Plan {
            pin: Some(pin_frame(bytes)),
            ..Plan::default()
        }
    }

    fn begins(operation: Operation) -> Plan {
        Plan {
            operation: Some(operation),
            ..pin(PIN)
        }
    }

    fn pin_frame(pin: &[u8]) -> Vec<u8> {
        let mut frame = (pin.len() as u16 + 1).to_be_bytes().to_vec();
        frame.push(operation::PIN);
        frame.extend_from_slice(pin);
        frame
    }

    fn read_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
        let mut header = [0; 2];
        stream.read_exact(&mut header).ok()?;
        let mut frame = vec![0; usize::from(u16::from_be_bytes(header))];
        stream.read_exact(&mut frame).ok()?;
        Some(frame)
    }

    /// A frame that arrives within `time`, `Some(None)` at a close, or none
    /// when nothing came; the frame wait is then restored.
    fn first_within(stream: &mut UnixStream, time: Duration) -> Option<Option<Vec<u8>>> {
        stream.set_read_timeout(Some(time)).unwrap();
        let mut header = [0; 2];
        let arrived = match stream.read(&mut header[..1]) {
            Ok(0) => Some(None),
            Ok(_) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(20)))
                    .unwrap();
                stream.read_exact(&mut header[1..]).unwrap();
                let mut frame = vec![0; usize::from(u16::from_be_bytes(header))];
                stream.read_exact(&mut frame).unwrap();
                Some(Some(frame))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                None
            }
            Err(_) => Some(None),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        arrived
    }

    fn write_frame(stream: &mut UnixStream, bytes: &[u8]) {
        stream
            .write_all(&(bytes.len() as u16).to_be_bytes())
            .unwrap();
        stream.write_all(bytes).unwrap();
    }

    /// Root's side: every frame it saw, each invitation's round zeroed. It
    /// begins the operation from the baseline and admits every later step
    /// with consent's own admission, taking a prove step's key as the
    /// created credential, as td-authd will.
    fn root(mut stream: UnixStream, plan: Plan) -> Vec<Vec<u8>> {
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut seen = Vec::new();
        let Some(baseline) = read_frame(&mut stream) else {
            return seen;
        };
        seen.push(baseline.clone());
        if baseline[0] != BASELINE {
            return seen;
        }
        let keys: Vec<Fingerprint> = baseline
            .get(3..)
            .unwrap_or_default()
            .chunks(4)
            .map(|key| key.try_into().unwrap())
            .collect();
        let change = |at| {
            if let Some((dir, bytes, when)) = &plan.change {
                if *when == at {
                    replace(dir, bytes.as_deref());
                }
            }
            if let Some((dir, when)) = &plan.damage {
                if *when == at {
                    fs::set_permissions(dir, Permissions::from_mode(0o750)).unwrap();
                }
            }
        };
        change(At::Baseline);
        let count = (keys.len() as u8).max(1);
        let operation = plan.operation.clone().unwrap_or(Operation::LoginUnlock {
            account: UID,
            before: count,
            after: count,
            step: LoginStep::Identify,
        });
        let mut current = Request::new(NONCE, UID, operation.clone()).unwrap();
        if let Ok(begun) = Request::begin_login(NONCE, UID, operation, &keys) {
            assert_eq!(begun, current);
        }
        let description = plan.description.clone().unwrap_or(current.encode());
        write_frame(&mut stream, &description);
        let mut rounds: Vec<Vec<u8>> = Vec::new();
        let mut last = false;
        let mut created = None;
        let mut invitations = 0;
        while let Some(mut frame) = read_frame(&mut stream) {
            let tag = frame[0];
            if tag != 0x10 && tag != 0x12 {
                seen.push(frame);
                break;
            }
            let round = frame[1..33].to_vec();
            assert!(!rounds.contains(&round));
            rounds.push(round);
            let invitation = Request::decode(&frame[33..]).unwrap();
            let fresh = tag == 0x10 && invitation != current;
            if fresh {
                assert!(!last);
                created = match invitation.login_step().unwrap() {
                    LoginStep::Prove { key, .. } => Some(key),
                    LoginStep::Repeat { .. } | LoginStep::Probe { .. } => created,
                    _ => None,
                };
                let admitted = current
                    .admit_login_step(&invitation, &keys, created)
                    .unwrap();
                (current, last) = (invitation, admitted == Admitted::Last);
            } else {
                assert_eq!(invitation, current);
                assert_eq!(tag == 0x12, last);
            }
            let mut normalized = frame.clone();
            normalized[1..33].fill(0);
            seen.push(normalized);
            if plan.silent {
                continue;
            }
            invitations += 1;
            if plan.early_pin == Some(invitations - 1) {
                stream.write_all(&pin_frame(PIN)).unwrap();
                continue;
            }
            if plan.cancel == Some(invitations - 1) {
                break;
            }
            if tag == 0x10 && last {
                change(At::Last);
            }
            if tag == 0x12 {
                change(At::Commit);
            }
            if let (true, Some(admitted)) = (fresh, &plan.admitted) {
                admitted(&current);
            }
            let step = current.login_step().unwrap();
            let pin_step = tag == 0x10 && step.asks_pin();
            if pin_step && plan.pin.is_none() {
                break;
            }
            frame[0] += 1;
            write_frame(&mut stream, &frame);
            if pin_step {
                let wrong = plan.wrong.is_some_and(|wrong| {
                    std::mem::discriminant(&wrong) == std::mem::discriminant(&step)
                });
                let pin = match (wrong, &plan.pin) {
                    (true, _) => pin_frame(WRONG),
                    (false, pin) => pin.clone().unwrap_or_default(),
                };
                if let Some(delay) = plan.pin_delay {
                    // The worker's failure frame or close, first, ends it.
                    match first_within(&mut stream, delay) {
                        Some(Some(frame)) => {
                            seen.push(frame);
                            break;
                        }
                        Some(None) => break,
                        None => {}
                    }
                }
                stream.write_all(&pin).unwrap();
                if pin.is_empty() {
                    break;
                }
            }
        }
        seen
    }

    fn login(operation: Operation) -> Request {
        Request::new(NONCE, UID, operation).unwrap()
    }

    fn identify(count: u8) -> Request {
        step(count, LoginStep::Identify)
    }

    fn step(count: u8, step: LoginStep) -> Request {
        login(Operation::LoginUnlock {
            account: UID,
            before: count,
            after: count,
            step,
        })
    }

    fn unlocking(count: u8, key: Fingerprint, retries: u8) -> Request {
        step(count, LoginStep::Unlock { key, retries })
    }

    fn enrolling(after: u8, key: u8, step: LoginStep) -> Operation {
        Operation::LoginEnroll {
            account: UID,
            before: 0,
            after,
            key,
            step,
        }
    }

    fn adding(before: u8, step: LoginStep) -> Operation {
        Operation::LoginAdd {
            account: UID,
            before,
            after: before + 1,
            step,
        }
    }

    fn removing(before: u8, removed: &[Slot], step: LoginStep) -> Operation {
        Operation::LoginRemove {
            account: UID,
            before,
            after: before - removed.len() as u8,
            removed: removed.to_vec(),
            step,
        }
    }

    fn baseline(keys: &[Fingerprint]) -> Vec<u8> {
        let mut frame = vec![0x18, 1, keys.len() as u8];
        frame.extend(keys.iter().flatten());
        frame
    }

    fn invitation(tag: u8, request: &Request) -> Vec<u8> {
        let mut frame = vec![tag];
        frame.extend([0; 32]);
        frame.extend(request.encode());
        frame
    }

    /// Root's view of an unlock that reached `key`'s unlock step.
    fn through_unlock(keys: &[Fingerprint], key: Fingerprint, retries: u8) -> Vec<Vec<u8>> {
        let count = keys.len() as u8;
        vec![
            baseline(keys),
            invitation(0x10, &identify(count)),
            invitation(0x10, &unlocking(count, key, retries)),
        ]
    }

    fn succeeded(keys: &[Fingerprint], key: Fingerprint, retries: u8) -> Vec<Vec<u8>> {
        let mut frames = through_unlock(keys, key, retries);
        frames.push(invitation(0x12, &unlocking(keys.len() as u8, key, retries)));
        frames.push(vec![0x14]);
        frames
    }

    fn failed(mut frames: Vec<Vec<u8>>, failure: Failure) -> Vec<Vec<u8>> {
        frames.push(failure.frame());
        frames
    }

    /// The presentations of one new key's ceremony with full retries.
    fn ceremony(make: impl Fn(LoginStep) -> Operation, key: Fingerprint) -> Vec<Vec<u8>> {
        [
            LoginStep::Create { retries: 8 },
            LoginStep::Prove { key, retries: 8 },
            LoginStep::Repeat { key, retries: 8 },
            LoginStep::Probe { key },
        ]
        .into_iter()
        .map(|step| invitation(0x10, &login(make(step))))
        .collect()
    }

    /// The commit round over `last` and the success frame.
    fn committed(mut frames: Vec<Vec<u8>>, last: Operation) -> Vec<Vec<u8>> {
        frames.push(invitation(0x12, &login(last)));
        frames.push(vec![0x14]);
        frames
    }

    /// Root's view of an addition authorized by `by` up to its connect step.
    fn authorized_addition(keys: &[Fingerprint], by: Fingerprint) -> Vec<Vec<u8>> {
        let before = keys.len() as u8;
        vec![
            baseline(keys),
            invitation(0x10, &login(adding(before, LoginStep::Identify))),
            invitation(
                0x10,
                &login(adding(
                    before,
                    LoginStep::Authorize {
                        key: by,
                        retries: 8,
                    },
                )),
            ),
            invitation(0x10, &login(adding(before, LoginStep::Connect))),
        ]
    }

    fn addition(keys: &[Fingerprint], by: Fingerprint, new: Fingerprint) -> Vec<Vec<u8>> {
        let before = keys.len() as u8;
        let mut frames = authorized_addition(keys, by);
        frames.extend(ceremony(|step| adding(before, step), new));
        committed(frames, adding(before, LoginStep::Probe { key: new }))
    }

    fn removal(keys: &[Fingerprint], removed: &[Slot], by: Fingerprint) -> Vec<Vec<u8>> {
        let before = keys.len() as u8;
        let authorize = removing(
            before,
            removed,
            LoginStep::Authorize {
                key: by,
                retries: 8,
            },
        );
        committed(
            vec![
                baseline(keys),
                invitation(0x10, &login(removing(before, removed, LoginStep::Identify))),
                invitation(0x10, &login(authorize.clone())),
            ],
            authorize,
        )
    }

    fn slots(keys: &[Fingerprint], positions: &[u8]) -> Vec<Slot> {
        positions
            .iter()
            .map(|position| Slot {
                position: *position,
                key: keys[usize::from(*position) - 1],
            })
            .collect()
    }

    fn run(
        context: &Context<'_>,
        keys: &mut Keys<'_>,
        plan: Plan,
        time: Duration,
    ) -> (Result<(), Failure>, Vec<Vec<u8>>) {
        let (result, seen, _) = timed(context, keys, plan, time);
        (result, seen)
    }

    /// `run`, and how long the worker took.
    fn timed(
        context: &Context<'_>,
        keys: &mut Keys<'_>,
        plan: Plan,
        time: Duration,
    ) -> (Result<(), Failure>, Vec<Vec<u8>>, Duration) {
        drawn(context, keys, plan, time, 0)
    }

    /// `timed`, the worker's draws starting after `start`.
    fn drawn(
        context: &Context<'_>,
        keys: &mut Keys<'_>,
        plan: Plan,
        time: Duration,
        start: u32,
    ) -> (Result<(), Failure>, Vec<Vec<u8>>, Duration) {
        let (worker, parent) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || root(parent, plan));
        let started = Instant::now();
        let mut wire = Wire::new(worker, started + time).unwrap();
        let mut entropy = entropy_after(start);
        let result = operate(&mut wire, context, keys, &mut entropy, || Ok(()));
        let took = started.elapsed();
        drop(wire);
        (result, root.join().unwrap(), took)
    }

    fn operation(
        fixture: &Fixture,
        keys: &mut Keys<'_>,
        plan: Plan,
    ) -> (Result<(), Failure>, Vec<Vec<u8>>) {
        run(&fixture.context(), keys, plan, TIME)
    }

    fn unlock(
        fixture: &Fixture,
        devices: &[&Virtual],
        plan: Plan,
    ) -> (Result<(), Failure>, Vec<Vec<u8>>) {
        operation(fixture, &mut Keys::all(devices), plan)
    }

    #[test]
    fn failure_frames_are_pinned() {
        for (failure, frame) in [
            (Failure::WrongPin(7), &[0x15, 1, 7][..]),
            (Failure::PinAuthBlocked, &[0x15, 2]),
            (Failure::PinBlocked, &[0x15, 3]),
            (Failure::NotEnrolled, &[0x15, 4]),
            (Failure::OneKey, &[0x15, 5]),
            (Failure::Refused(LoginRefusal::NoHmacSecret), &[0x15, 6, 1]),
            (Failure::Refused(LoginRefusal::AlwaysUv), &[0x15, 6, 2]),
            (
                Failure::Refused(LoginRefusal::PinUnsupported),
                &[0x15, 6, 3],
            ),
            (Failure::Refused(LoginRefusal::PinNotSet), &[0x15, 6, 4]),
            (Failure::Refused(LoginRefusal::ListTooSmall), &[0x15, 6, 5]),
            (Failure::Unprobed, &[0x15, 6, 6]),
            (Failure::Denied, &[0x15, 7]),
            (Failure::Timeout, &[0x15, 8]),
            (Failure::NoRecord, &[0x15, 9]),
            (Failure::Unavailable(Cause::DirectoryDamaged), &[0x15, 10]),
            (Failure::Unavailable(Cause::RecordDamaged), &[0x15, 11]),
            (Failure::Unavailable(Cause::Unreadable), &[0x15, 12]),
            (Failure::Changed, &[0x15, 13]),
            (Failure::Uncertain(Found::Absent), &[0x15, 14, 1]),
            (Failure::Uncertain(Found::Old), &[0x15, 14, 2]),
            (Failure::Uncertain(Found::New), &[0x15, 14, 3]),
            (Failure::Uncertain(Found::Other), &[0x15, 14, 4]),
            (Failure::Uncertain(Found::Unavailable), &[0x15, 14, 5]),
            (Failure::Failed, &[0x15, 15]),
            (Failure::Internal, &[0x15, 16]),
            (Failure::Excluded, &[0x15, 17]),
            (Failure::Version, &[0x15, 18]),
        ] {
            assert_eq!(failure.frame(), frame, "{failure:?}");
        }
        // The new tags are distinct from every other private operation frame.
        assert_eq!([FAILURE, operation::PIN, BASELINE], [0x15, 0x16, 0x18]);
    }

    #[test]
    fn deadlines_are_the_designs_and_every_session_the_transports() {
        assert_eq!(LOGIN_CEREMONY, Duration::from_secs(120));
        assert_eq!(LOGIN_TWO_CEREMONIES, Duration::from_secs(240));
        assert!(LOGIN_CEREMONY <= fido_device::MAX_LIFETIME);
        let slot = Slot {
            position: 1,
            key: [1; 4],
        };
        for (operation, ceiling) in [
            (
                step(1, LoginStep::Identify).operation().clone(),
                LOGIN_CEREMONY,
            ),
            (removing(1, &[slot], LoginStep::Identify), LOGIN_CEREMONY),
            (enrolling(1, 1, LoginStep::Connect), LOGIN_CEREMONY),
            (enrolling(2, 1, LoginStep::Connect), LOGIN_TWO_CEREMONIES),
            (adding(1, LoginStep::Identify), LOGIN_TWO_CEREMONIES),
        ] {
            assert_eq!(login(operation).login_ceiling(), Some(ceiling));
        }
        let store = Operation::Unlock {
            role: crate::consent::Role::Primary,
        };
        assert_eq!(
            Request::new(NONCE, UID, store).unwrap().login_ceiling(),
            None
        );
        // A session opened under a longer deadline keeps the transport's lifetime.
        let key = blank("lifetime");
        let mut keys = Keys::all(&[&key]);
        let (worker, _parent) = UnixStream::pair().unwrap();
        let mut wire = Wire::new(worker, Instant::now() + LOGIN_TWO_CEREMONIES).unwrap();
        let before = Instant::now();
        let lifetime = fido_device::MAX_LIFETIME;
        assert!(super::open(&mut keys, 0, &mut wire, lifetime).is_ok());
        assert!(keys.deadlines[0] <= Instant::now() + fido_device::MAX_LIFETIME);
        assert!(keys.deadlines[0] >= before + fido_device::MAX_LIFETIME);
        // The open session bounds the wire, not the operation, until it ends.
        assert_eq!(wire.bound(), keys.deadlines[0]);
        assert!(wire.deadline() > keys.deadlines[0] + LOGIN_CEREMONY / 2);
        assert_eq!(super::ended(&mut wire, Ok(()), None, None), Ok(()));
        assert_eq!(wire.bound(), wire.deadline());
        // Knowing the operation narrows the deadline, never extends it.
        wire.limit(before + LOGIN_CEREMONY);
        assert!(super::open(&mut keys, 0, &mut wire, lifetime).is_ok());
        assert_eq!(keys.deadlines[1], before + LOGIN_CEREMONY);
        wire.limit(before + LOGIN_TWO_CEREMONIES);
        assert_eq!(wire.deadline(), before + LOGIN_CEREMONY);
        // Production sessions live as long as the transport allows.
        let run = include_str!("login_operation.rs")
            .split("pub(super) fn run(")
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        assert_eq!(
            run.matches("lifetime: fido_device::MAX_LIFETIME,").count(),
            1
        );
        assert_eq!(run.matches("margin: COMMIT_MARGIN,").count(), 1);
        assert_eq!(COMMIT_MARGIN, Duration::from_secs(5));
        // Production reads the retained deployments' markers through the
        // read-only volume, as root's files.
        assert_eq!(run.matches("retained: &Volume {").count(), 1);
        assert_eq!(
            run.matches("path: Path::new(login_tier::VOLUME),").count(),
            1
        );
        assert_eq!(run.matches("owner: 0,").count(), 1);
        assert_eq!(login_tier::VOLUME, "/run/td-volume/td");
        assert_eq!(RETAINED_GIVE_UP, Duration::from_secs(10));
        let fixture = Fixture::new();
        let production = Context {
            retained: &(&[][..], &[][..]),
            ..fixture.context()
        };
        assert_eq!(super::version(&production), Err(Failure::Version));
        assert_eq!(super::version(&fixture.context()), Ok(VERSION));
    }

    #[test]
    fn a_write_takes_its_version_from_both_retained_markers() {
        use crate::login_tier::tests::{fifo, marker, Volume as Tree};
        let fixture = Fixture::new();
        let version = |tree: &Tree| {
            let volume = Volume {
                path: tree.path(),
                owner: tree.scratch.owner,
            };
            (
                volume.reads(),
                super::version(&Context {
                    retained: &volume,
                    ..fixture.context()
                }),
            )
        };
        let (this, later) = (marker(READS), marker(&[VERSION, VERSION + 1]));
        // Both markers read this build's version.
        let tree = Tree::new();
        tree.retain(Some(&this), Some(&later));
        assert_eq!(
            version(&tree),
            ([READS.to_vec(), vec![VERSION, VERSION + 1]], Ok(VERSION))
        );
        // Either one missing, or neither listing this build's version.
        for (current, previous) in [
            (Some(&this), None),
            (None, Some(&this)),
            (None, None),
            (Some(&marker(&[VERSION + 1])), Some(&this)),
        ] {
            let tree = Tree::new();
            tree.retain(current.map(Vec::as_slice), previous.map(Vec::as_slice));
            assert_eq!(version(&tree).1, Err(Failure::Version));
        }
        // A malformed selector, a deployment that is a link, a FIFO archive
        // and no volume at all read nothing.
        let tree = Tree::new();
        tree.retain(Some(&this), Some(&this));
        let id = tree.deploy(Some(&marker(&[VERSION, 2])));
        tree.select(
            login_tier::PREVIOUS,
            &format!("../deployments/{}", id.to_uppercase()),
        );
        assert_eq!(
            version(&tree),
            ([READS.to_vec(), vec![]], Err(Failure::Version))
        );
        let deployments = tree.path().join("deployments");
        let link = deployments.join("a".repeat(64));
        std::os::unix::fs::symlink(deployments.join(&id), &link).unwrap();
        tree.select(
            login_tier::PREVIOUS,
            &format!("../deployments/{}", "a".repeat(64)),
        );
        assert_eq!(
            version(&tree),
            ([READS.to_vec(), vec![]], Err(Failure::Version))
        );
        tree.select(login_tier::PREVIOUS, &format!("../deployments/{id}"));
        assert_eq!(
            version(&tree),
            ([READS.to_vec(), vec![VERSION, 2]], Ok(VERSION))
        );
        let archive = deployments.join(&id).join("initramfs.cpio");
        fs::remove_file(&archive).unwrap();
        if fifo(&archive) {
            assert_eq!(
                version(&tree),
                ([READS.to_vec(), vec![]], Err(Failure::Version))
            );
        }
        let absent = Volume {
            path: &tree.path().join("absent"),
            owner: tree.scratch.owner,
        };
        assert_eq!(absent.reads(), [vec![], vec![]]);
        // Through the worker: a first enrollment reads both markers before
        // any token and refuses, the record untouched, while one is
        // missing; an unlock reads none.
        let key = blank("tier");
        let unmarked = Tree::new();
        unmarked.retain(Some(&this), None);
        let volume = Volume {
            path: unmarked.path(),
            owner: unmarked.scratch.owner,
        };
        let context = Context {
            retained: &volume,
            ..fixture.context()
        };
        let start = key.transcript().len();
        let (result, seen) = run(
            &context,
            &mut Keys::all(&[&key]),
            begins(enrolling(1, 1, LoginStep::Connect)),
            TIME,
        );
        assert_eq!(result, Err(Failure::Version));
        assert_eq!(seen, [vec![0x18, 0], Failure::Version.frame()]);
        assert_eq!(key.transcript().len(), start);
        assert!(matches!(fixture.state(), State::Unenrolled));
        let marked = Tree::new();
        marked.retain(Some(&this), Some(&this));
        let volume = Volume {
            path: marked.path(),
            owner: marked.scratch.owner,
        };
        let context = Context {
            retained: &volume,
            ..fixture.context()
        };
        let mut keys = Keys::all(&[&key]);
        let (result, _) = run(
            &context,
            &mut keys,
            begins(enrolling(1, 1, LoginStep::Connect)),
            TIME,
        );
        assert_eq!(result, Ok(()));
        assert!(matches!(fixture.state(), State::Enrolled(_)));
    }

    #[test]
    fn each_slot_of_a_multi_key_record_unlocks_with_its_own_bound_step() {
        let keys = [Key::new(1), Key::new(2), Key::new(3)];
        let all: Vec<&Key> = keys.iter().collect();
        let fixture = Fixture::new();
        fixture.seed(&record(&all, None, None));
        let order = canonical(&all);
        for key in &keys {
            let (tokens, hashes) = (key.pin_tokens(), key.hashes().len());
            let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
            assert_eq!(result, Ok(()));
            assert_eq!(seen, succeeded(&order, key.fingerprint(), 8));
            // Identify, then the PIN assertion: each hash binds its exact step.
            let hash = |phase, request: &Request, random| {
                login_record::client_data_hash(phase, &request.encode(), None, &draw(random))
                    .unwrap()
                    .to_vec()
            };
            assert_eq!(
                key.hashes()[hashes..],
                [
                    hash(Phase::Identify, &identify(3), 1),
                    hash(Phase::Unlock, &unlocking(3, key.fingerprint(), 8), 2)
                ]
            );
            assert_eq!(key.pin_tokens(), tokens + 1);
        }
    }

    #[test]
    fn a_wrong_pin_reports_the_falling_count_and_a_later_operation_succeeds() {
        let keys = [Key::new(4), Key::new(5)];
        let all: Vec<&Key> = keys.iter().collect();
        let fixture = Fixture::new();
        fixture.seed(&record(&all, None, None));
        let order = canonical(&all);
        let key = &keys[1];
        for retries in [8, 7] {
            let (result, seen) = unlock(&fixture, &[&key.device], pin(WRONG));
            assert_eq!(result, Err(Failure::WrongPin(retries)));
            assert_eq!(
                seen,
                failed(
                    through_unlock(&order, key.fingerprint(), retries),
                    Failure::WrongPin(retries)
                )
            );
        }
        let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
        assert_eq!(result, Ok(()));
        assert_eq!(seen, succeeded(&order, key.fingerprint(), 6));
        assert_eq!(key.device.state().retries, 8);
    }

    #[test]
    fn three_wrong_pins_block_until_reinsertion_and_none_left_blocks_for_good() {
        let key = Key::new(6);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let wrong = |retries| {
            failed(
                through_unlock(&order, key.fingerprint(), retries),
                Failure::WrongPin(retries),
            )
        };
        assert_eq!(unlock(&fixture, &[&key.device], pin(WRONG)).1, wrong(8));
        assert_eq!(unlock(&fixture, &[&key.device], pin(WRONG)).1, wrong(7));
        let (result, seen) = unlock(&fixture, &[&key.device], pin(WRONG));
        assert_eq!(result, Err(Failure::PinAuthBlocked));
        assert_eq!(
            seen,
            failed(
                through_unlock(&order, key.fingerprint(), 6),
                Failure::PinAuthBlocked
            )
        );
        // Until reinserted the key gets no PIN step at all.
        let identified = vec![baseline(&order), invitation(0x10, &identify(1))];
        let tokens = key.pin_tokens();
        let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
        assert_eq!(result, Err(Failure::PinAuthBlocked));
        assert_eq!(seen, failed(identified.clone(), Failure::PinAuthBlocked));
        assert_eq!(key.pin_tokens(), tokens);
        key.device.power_cycle();
        // One left: a wrong PIN blocks the key, which then gets no PIN step.
        key.device.with_state(|state| state.retries = 1);
        let (result, seen) = unlock(&fixture, &[&key.device], pin(WRONG));
        assert_eq!(result, Err(Failure::PinBlocked));
        assert_eq!(
            seen,
            failed(
                through_unlock(&order, key.fingerprint(), 1),
                Failure::PinBlocked
            )
        );
        for _ in 0..2 {
            let tokens = key.pin_tokens();
            let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
            assert_eq!(result, Err(Failure::PinBlocked));
            assert_eq!(seen, failed(identified.clone(), Failure::PinBlocked));
            assert_eq!(key.pin_tokens(), tokens);
            key.device.power_cycle();
        }
    }

    #[test]
    fn a_key_not_in_the_record_and_any_count_but_one_refuse_before_a_pin() {
        let (mine, other, stranger) = (Key::new(7), Key::new(8), Key::new(9));
        let fixture = Fixture::new();
        fixture.seed(&record(&[&mine, &other], None, None));
        let order = canonical(&[&mine, &other]);
        let identified = vec![baseline(&order), invitation(0x10, &identify(2))];
        let tokens = stranger.pin_tokens();
        let (result, seen) = unlock(&fixture, &[&stranger.device], pin(PIN));
        assert_eq!(result, Err(Failure::NotEnrolled));
        assert_eq!(seen, failed(identified.clone(), Failure::NotEnrolled));
        assert_eq!(stranger.pin_tokens(), tokens);
        for devices in [&[][..], &[&mine.device, &other.device]] {
            let before = [mine.device.transcript(), other.device.transcript()];
            let (result, seen) = unlock(&fixture, devices, pin(PIN));
            assert_eq!(result, Err(Failure::OneKey));
            assert_eq!(seen, failed(identified.clone(), Failure::OneKey));
            assert_eq!(
                [mine.device.transcript(), other.device.transcript()],
                before
            );
        }
    }

    #[test]
    fn a_tampered_record_or_a_stale_signature_never_unlocks() {
        let (key, other) = (Key::new(10), Key::new(11));
        let order = [key.fingerprint()];
        let mut flipped = key.output;
        flipped[0] ^= 1;
        let refused = failed(
            through_unlock(&order, key.fingerprint(), 8),
            Failure::Failed,
        );
        for bytes in [
            record(&[&key], Some(flipped), None),
            record(&[&key], None, Some(&other)),
        ] {
            let fixture = Fixture::new();
            fixture.seed(&bytes);
            let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
            assert_eq!(result, Err(Failure::Failed));
            assert_eq!(seen, refused);
        }
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        assert_eq!(unlock(&fixture, &[&key.device], pin(PIN)).0, Ok(()));
        // A signature over other data, here this operation's identify's.
        key.device.script(Script {
            signing: Signing::Stale,
            ..Script::default()
        });
        let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
        assert_eq!(result, Err(Failure::Failed));
        assert_eq!(seen, refused);
        // A replay: this assertion's data signed over the earlier unlock's
        // client-data hash, in an operation drawing its own challenge.
        key.device.script(Script {
            signing: Signing::Replayed,
            ..Script::default()
        });
        let (result, seen, _) = drawn(
            &fixture.context(),
            &mut Keys::all(&[&key.device]),
            pin(PIN),
            TIME,
            1000,
        );
        assert_eq!(result, Err(Failure::Failed));
        assert_eq!(seen, refused);
    }

    #[test]
    fn each_unlock_signs_a_challenge_of_its_own_so_a_replay_fails() {
        let key = Key::new(16);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let unlocked = succeeded(&order, key.fingerprint(), 8);
        let after = |start| {
            let mut keys = Keys::all(&[&key.device]);
            let (result, seen, _) = drawn(&fixture.context(), &mut keys, pin(PIN), TIME, start);
            (result, seen)
        };
        assert_eq!(after(0), (Ok(()), unlocked.clone()));
        key.device.script(Script {
            signing: Signing::Replayed,
            ..Script::default()
        });
        // The same draws, as a worker whose challenge was not fresh would
        // make: the earlier unlock's signed hash verifies again.
        assert_eq!(after(0), (Ok(()), unlocked));
        // The operation's own draws: it no longer does.
        assert_eq!(
            after(1000),
            (
                Err(Failure::Failed),
                failed(
                    through_unlock(&order, key.fingerprint(), 8),
                    Failure::Failed
                )
            )
        );
    }

    #[test]
    fn an_unenrolled_or_unavailable_state_refuses_before_any_token() {
        let key = Key::new(12);
        let start = key.device.transcript().len();
        let fixture = Fixture::new();
        let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
        assert_eq!(result, Err(Failure::NoRecord));
        assert_eq!(seen, [vec![0x18, 0], vec![0x15, 9]]);
        // An addition or removal needs a record too; an enrollment needs none.
        let slot = Slot {
            position: 1,
            key: key.fingerprint(),
        };
        for operation in [
            adding(1, LoginStep::Identify),
            removing(1, &[slot], LoginStep::Identify),
        ] {
            let (result, seen) = unlock(&fixture, &[&key.device], begins(operation));
            assert_eq!(result, Err(Failure::NoRecord));
            assert_eq!(seen, [vec![0x18, 0], vec![0x15, 9]]);
        }
        // Damage sends no baseline, so no description can be admitted against it.
        let bytes = record(&[&key], None, None);
        for (damage, cause) in [
            (0, Cause::RecordDamaged),
            (1, Cause::RecordDamaged),
            (2, Cause::DirectoryDamaged),
            (3, Cause::DirectoryDamaged),
        ] {
            let fixture = Fixture::new();
            fixture.seed(&bytes);
            let path = fixture.dir.join(UID.to_string());
            match damage {
                0 => fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap(),
                1 => fixture.seed(&bytes[..bytes.len() - 1]),
                2 => fs::set_permissions(&fixture.dir, Permissions::from_mode(0o755)).unwrap(),
                _ => fs::remove_dir_all(&fixture.dir).unwrap(),
            }
            let plan = begins(enrolling(1, 1, LoginStep::Connect));
            let (result, seen) = unlock(&fixture, &[&key.device], plan);
            assert_eq!(result, Err(Failure::Unavailable(cause)));
            assert_eq!(seen, [Failure::Unavailable(cause).frame()]);
        }
        // Root never begins an enrollment over a record it was shown.
        let fixture = Fixture::new();
        fixture.seed(&bytes);
        let plan = begins(enrolling(1, 1, LoginStep::Connect));
        let (result, seen) = unlock(&fixture, &[&key.device], plan);
        assert_eq!(result, Err(Failure::Internal));
        assert_eq!(seen, [baseline(&[key.fingerprint()]), vec![0x15, 16]]);
        assert_eq!(key.device.transcript().len(), start);
    }

    #[test]
    fn a_record_changed_after_its_baseline_refuses_before_tokens_or_success() {
        let (key, other) = (Key::new(13), Key::new(14));
        let order = canonical(&[&key, &other]);
        let changed = record(&[&key], None, None);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key, &other], None, None));
        let start = key.device.transcript().len();
        let (result, seen) = unlock(
            &fixture,
            &[&key.device],
            Plan {
                change: Some((fixture.dir.clone(), Some(changed.clone()), At::Baseline)),
                ..pin(PIN)
            },
        );
        assert_eq!(result, Err(Failure::Changed));
        assert_eq!(
            seen,
            failed(
                vec![baseline(&order), invitation(0x10, &identify(2))],
                Failure::Changed
            )
        );
        assert_eq!(key.device.transcript().len(), start);
        // A change during the ceremony refuses after a valid assertion, before commit.
        fixture.seed(&record(&[&key, &other], None, None));
        let tokens = key.pin_tokens();
        let (result, seen) = unlock(
            &fixture,
            &[&key.device],
            Plan {
                change: Some((fixture.dir.clone(), Some(changed), At::Last)),
                ..pin(PIN)
            },
        );
        assert_eq!(result, Err(Failure::Changed));
        assert_eq!(
            seen,
            failed(
                through_unlock(&order, key.fingerprint(), 8),
                Failure::Changed
            )
        );
        assert_eq!(key.pin_tokens(), tokens + 1);
    }

    #[test]
    fn root_that_never_acknowledges_times_out_before_any_token() {
        let key = Key::new(15);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let start = key.device.transcript().len();
        let (result, seen) = unlock(
            &fixture,
            &[&key.device],
            Plan {
                silent: true,
                ..pin(PIN)
            },
        );
        assert_eq!(result, Err(Failure::Timeout));
        assert_eq!(
            seen,
            failed(
                vec![
                    baseline(&[key.fingerprint()]),
                    invitation(0x10, &identify(1))
                ],
                Failure::Timeout
            )
        );
        assert_eq!(key.device.transcript().len(), start);
    }

    #[test]
    fn root_cancelling_at_the_pin_step_spends_no_pin() {
        let key = Key::new(16);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let tokens = key.pin_tokens();
        // Before acknowledging the unlock step, then after it but before a PIN.
        for plan in [
            Plan::default(),
            Plan {
                pin: Some(Vec::new()),
                ..Plan::default()
            },
        ] {
            let (result, seen) = unlock(&fixture, &[&key.device], plan);
            assert_eq!(result, Err(Failure::Internal));
            assert_eq!(seen, through_unlock(&order, key.fingerprint(), 8));
        }
        assert_eq!(key.pin_tokens(), tokens);
        assert_eq!(key.device.state().retries, 8);
    }

    #[test]
    fn a_pin_frame_outside_its_bounds_is_refused_before_the_key_sees_it() {
        let key = Key::new(17);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let refused = failed(
            through_unlock(&order, key.fingerprint(), 8),
            Failure::Internal,
        );
        let mut oversized = 65u16.to_be_bytes().to_vec();
        oversized.push(operation::PIN);
        oversized.extend([b'1'; 64]);
        let mut wrong_tag = pin_frame(PIN);
        wrong_tag[2] = 0x13;
        let tokens = key.pin_tokens();
        for frame in [
            oversized,
            vec![0, 1, operation::PIN],
            vec![0, 0],
            wrong_tag,
            pin_frame(b"123"),
            pin_frame(b"12\n4"),
        ] {
            let (result, seen) = unlock(
                &fixture,
                &[&key.device],
                Plan {
                    pin: Some(frame),
                    ..Plan::default()
                },
            );
            assert_eq!(result, Err(Failure::Internal));
            assert_eq!(seen, refused);
        }
        assert_eq!(key.pin_tokens(), tokens);
        // The largest PIN frame reaches the key.
        let (result, _) = unlock(&fixture, &[&key.device], pin(&[b'7'; 63]));
        assert_eq!(result, Err(Failure::WrongPin(8)));
        assert_eq!(key.pin_tokens(), tokens + 1);
    }

    #[test]
    fn root_must_send_an_operation_matching_the_baseline_for_this_account() {
        let key = Key::new(18);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let unlocks = |owner, count, step| {
            Request::new(
                NONCE,
                owner,
                Operation::LoginUnlock {
                    account: owner,
                    before: count,
                    after: count,
                    step,
                },
            )
            .unwrap()
            .encode()
        };
        // A removal naming a slot the baseline does not hold at that position.
        let elsewhere = Slot {
            position: 1,
            key: [0; 4],
        };
        let store = Request::new(
            NONCE,
            UID,
            Operation::Unlock {
                role: crate::consent::Role::Primary,
            },
        )
        .unwrap()
        .encode();
        let start = key.device.transcript().len();
        for description in [
            unlocks(1001, 1, LoginStep::Identify),
            unlocks(UID, 2, LoginStep::Identify),
            unlocks(
                UID,
                1,
                LoginStep::Unlock {
                    key: key.fingerprint(),
                    retries: 8,
                },
            ),
            login(removing(1, &[elsewhere], LoginStep::Identify)).encode(),
            login(adding(1, LoginStep::Connect)).encode(),
            login(adding(2, LoginStep::Identify)).encode(),
            store,
            vec![1, 2, 3],
            [&[operation::PIN][..], PIN].concat(),
        ] {
            let (result, seen) = unlock(
                &fixture,
                &[&key.device],
                Plan {
                    description: Some(description),
                    ..pin(PIN)
                },
            );
            assert_eq!(result, Err(Failure::Internal));
            assert_eq!(seen, [baseline(&order), Failure::Internal.frame()]);
        }
        assert_eq!(key.device.transcript().len(), start);
    }

    #[test]
    fn a_pin_in_place_of_an_acknowledgement_is_refused_and_spends_nothing() {
        let key = Key::new(19);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let identified = vec![baseline(&order), invitation(0x10, &identify(1))];
        let mut committing = through_unlock(&order, key.fingerprint(), 8);
        committing.push(invitation(0x12, &unlocking(1, key.fingerprint(), 8)));
        let start = key.device.transcript().len();
        let tokens = key.pin_tokens();
        for (at, frames) in [
            (0, identified),
            (1, through_unlock(&order, key.fingerprint(), 8)),
            (2, committing),
        ] {
            let (result, seen) = unlock(
                &fixture,
                &[&key.device],
                Plan {
                    early_pin: Some(at),
                    ..pin(PIN)
                },
            );
            assert_eq!(result, Err(Failure::Internal), "{at}");
            assert_eq!(seen, failed(frames, Failure::Internal), "{at}");
            if at == 0 {
                assert_eq!(key.device.transcript().len(), start);
            }
        }
        // Only the commit case sent its PIN in the proper frame first.
        assert_eq!(key.pin_tokens(), tokens + 1);
        // Every frame this worker reads lands in clearing storage.
        let production = |source: &'static str| source.split("#[cfg(test)]").next().unwrap();
        let worker = production(include_str!("login_operation.rs"));
        assert!(!worker.contains(".receive()"));
        assert_eq!(worker.matches(".receive_cleared()").count(), 1);
        let acknowledge = production(include_str!("operation.rs"))
            .split("pub(super) fn acknowledge(")
            .nth(1)
            .unwrap()
            .split("\n    }\n")
            .next()
            .unwrap();
        assert!(acknowledge.contains("self.receive_cleared()?"));
        assert!(!acknowledge.contains("self.receive()"));
    }

    #[test]
    fn unprotected_memory_refuses_before_the_baseline_or_any_token() {
        let key = Key::new(20);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let start = key.device.transcript().len();
        let (worker, parent) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || root(parent, pin(PIN)));
        let mut wire = Wire::new(worker, Instant::now() + TIME).unwrap();
        let result = operate(
            &mut wire,
            &fixture.context(),
            &mut Keys::all(&[&key.device]),
            &mut entropy(),
            || Err("active swap".into()),
        );
        drop(wire);
        assert_eq!(result, Err(Failure::Internal));
        assert_eq!(root.join().unwrap(), [Failure::Internal.frame()]);
        assert_eq!(key.device.transcript().len(), start);
        // Production passes the store's check, the one the write worker runs.
        let run = include_str!("login_operation.rs")
            .split("pub(super) fn run(")
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        assert_eq!(run.matches("store::require_protected_memory,").count(), 1);
    }

    #[test]
    fn a_one_key_enrollment_publishes_a_record_its_key_then_unlocks() {
        let key = blank("enroll-one");
        let fixture = Fixture::new();
        let mut keys = Keys::all(&[&key]);
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            begins(enrolling(1, 1, LoginStep::Connect)),
        );
        assert_eq!(result, Ok(()));
        let id = newest(&key);
        let new = fingerprint(&id);
        let make = |step| enrolling(1, 1, step);
        let mut frames = vec![
            vec![0x18, 0],
            invitation(0x10, &login(make(LoginStep::Connect))),
        ];
        frames.extend(ceremony(make, new));
        assert_eq!(seen, committed(frames, make(LoginStep::Probe { key: new })));
        // Create and prove on one session, repeat and probe on new ones.
        assert_eq!(keys.opened, 3);
        assert_eq!(pin_tokens(&key), 3);
        assert_eq!(fixture.stored(), Some(vec![id.clone()]));
        assert_eq!(fixture.names(), ["1000"]);
        // Each hash binds its exact step, the one presented with its retries.
        let creations = sent(&key, 1, 1);
        let [create] = creations.as_slice() else {
            panic!("one creation")
        };
        assert!(binds(
            create,
            Phase::Create,
            &login(make(LoginStep::Create { retries: 8 })),
            None
        ));
        let assertions = sent(&key, 2, 2);
        let [prove, repeat, probe] = assertions.as_slice() else {
            panic!("three assertions")
        };
        for (hash, phase, step) in [
            (
                prove,
                Phase::Prove,
                LoginStep::Prove {
                    key: new,
                    retries: 8,
                },
            ),
            (
                repeat,
                Phase::Repeat,
                LoginStep::Repeat {
                    key: new,
                    retries: 8,
                },
            ),
            (probe, Phase::Probe, LoginStep::Probe { key: new }),
        ] {
            assert!(binds(hash, phase, &login(make(step)), None), "{phase:?}");
        }
        // The published record unlocks with its key.
        let (result, seen) = unlock(&fixture, &[&key], pin(PIN));
        assert_eq!(result, Ok(()));
        assert_eq!(seen, succeeded(&[new], new, 8));
    }

    #[test]
    fn a_two_key_enrollment_waits_for_the_swap_and_excludes_the_first_key() {
        let (primary, backup) = (blank("enroll-primary"), blank("enroll-backup"));
        let fixture = Fixture::new();
        // The primary's three sessions, then the backup's; at the second
        // connect step the primary is still in, then nothing, then the backup.
        let mut keys = Keys::sessions(
            &[&primary, &backup],
            vec![vec![0], vec![0], vec![0], vec![1]],
        );
        keys.waits = vec![(3, vec![0]), (3, vec![])];
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            begins(enrolling(2, 1, LoginStep::Connect)),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(keys.pauses, 2);
        let (first, second) = (newest(&primary), newest(&backup));
        let (one, two) = (fingerprint(&first), fingerprint(&second));
        let mut frames = vec![
            vec![0x18, 0],
            invitation(0x10, &login(enrolling(2, 1, LoginStep::Connect))),
        ];
        frames.extend(ceremony(|step| enrolling(2, 1, step), one));
        frames.push(invitation(
            0x10,
            &login(enrolling(2, 2, LoginStep::Connect)),
        ));
        frames.extend(ceremony(|step| enrolling(2, 2, step), two));
        assert_eq!(
            seen,
            committed(frames, enrolling(2, 2, LoginStep::Probe { key: two }))
        );
        let mut both = vec![first.clone(), second];
        both.sort();
        assert_eq!(fixture.stored(), Some(both));
        // The backup's creation excluded the primary's credential.
        let creation = backup
            .transcript()
            .into_iter()
            .find(|(request, _)| request[0] == 1)
            .unwrap()
            .0;
        assert!(creation.windows(first.len()).any(|window| window == first));
        for device in [&primary, &backup] {
            assert_eq!(unlock(&fixture, &[device], pin(PIN)).0, Ok(()));
        }
        // Several keys at a connect step refuse; a key that never comes times out.
        let fixture = Fixture::new();
        let mut keys = Keys::sessions(
            &[&primary, &backup],
            vec![vec![0], vec![0], vec![0], vec![0, 1]],
        );
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            begins(enrolling(2, 1, LoginStep::Connect)),
        );
        assert_eq!(result, Err(Failure::OneKey));
        assert_eq!(seen.last(), Some(&Failure::OneKey.frame()));
        assert_eq!(fixture.stored(), None);
        let mut keys = Keys::sessions(&[&primary], vec![vec![0]]);
        let (worker, _parent) = UnixStream::pair().unwrap();
        let wire = Wire::new(worker, Instant::now() + Duration::from_millis(300)).unwrap();
        assert_eq!(
            super::connected(&mut keys, Some(0), &wire),
            Err(Failure::Timeout)
        );
        assert!(keys.pauses > 0);
        assert_eq!(keys.opened, 0);
    }

    #[test]
    fn a_second_enrollment_key_that_is_the_first_is_excluded() {
        let key = blank("enroll-twice");
        let fixture = Fixture::new();
        // The same key, removed and inserted again under a new node.
        let mut keys = Keys::sessions(&[&key, &key], vec![vec![0], vec![0], vec![0], vec![1]]);
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            begins(enrolling(2, 1, LoginStep::Connect)),
        );
        assert_eq!(result, Err(Failure::Excluded));
        let first = fingerprint(&key.state().credentials[0].id);
        let mut frames = vec![
            vec![0x18, 0],
            invitation(0x10, &login(enrolling(2, 1, LoginStep::Connect))),
        ];
        frames.extend(ceremony(|step| enrolling(2, 1, step), first));
        frames.push(invitation(
            0x10,
            &login(enrolling(2, 2, LoginStep::Connect)),
        ));
        frames.push(invitation(
            0x10,
            &login(enrolling(2, 2, LoginStep::Create { retries: 8 })),
        ));
        assert_eq!(seen, failed(frames, Failure::Excluded));
        assert_eq!(key.state().credentials.len(), 1);
        assert_eq!(fixture.stored(), None);
        assert!(fixture.names().is_empty());
    }

    #[test]
    fn additions_reach_eight_keys_and_a_ninth_refuses_before_any_token() {
        let first = Key::new(30);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&first], None, None));
        let mut added = Vec::new();
        for count in 1..8u8 {
            let order = fixture.fingerprints();
            let new = blank(&format!("added-{count}"));
            let mut keys = Keys::sessions(&[&first.device, &new], vec![vec![0], vec![0], vec![1]]);
            let (result, seen) = operation(
                &fixture,
                &mut keys,
                begins(adding(count, LoginStep::Identify)),
            );
            assert_eq!(result, Ok(()), "{count}");
            let id = newest(&new);
            assert_eq!(
                seen,
                addition(&order, first.fingerprint(), fingerprint(&id))
            );
            assert_eq!(keys.opened, 5);
            // The authorization's hash binds the record it changes.
            if count == 1 {
                let shown = Record::decode(&record(&[&first], None, None), UID).unwrap();
                let other = record(&[&first], Some([0; 32]), None);
                let other = Record::decode(&other, UID).unwrap();
                let authorize = first.hashes().last().unwrap().clone();
                let step = LoginStep::Authorize {
                    key: first.fingerprint(),
                    retries: 8,
                };
                let request = login(adding(1, step));
                assert!(binds(&authorize, Phase::Authorize, &request, Some(&shown)));
                assert!(!binds(&authorize, Phase::Authorize, &request, Some(&other)));
            }
            added.push((new, id));
            let mut expected: Vec<Vec<u8>> = added.iter().map(|(_, id)| id.clone()).collect();
            expected.push(first.id.clone());
            expected.sort();
            assert_eq!(fixture.stored(), Some(expected));
        }
        // Every added key unlocks.
        for (device, _) in &added {
            assert_eq!(unlock(&fixture, &[device], pin(PIN)).0, Ok(()));
        }
        // At eight no addition can be described: root sends none, or one
        // that does not match, and no key is touched.
        let order = fixture.fingerprints();
        assert_eq!(order.len(), 8);
        assert!(Request::new(NONCE, UID, adding(8, LoginStep::Identify)).is_err());
        let ninth = blank("ninth");
        let start = (first.device.transcript().len(), ninth.transcript().len());
        for plan in [
            Plan {
                description: Some(login(adding(7, LoginStep::Identify)).encode()),
                ..pin(PIN)
            },
            Plan {
                description: Some(Vec::new()),
                ..pin(PIN)
            },
        ] {
            let mut keys =
                Keys::sessions(&[&first.device, &ninth], vec![vec![0], vec![0], vec![1]]);
            let (result, seen) = operation(&fixture, &mut keys, plan);
            assert_eq!(result, Err(Failure::Internal));
            assert_eq!(seen, [baseline(&order), Failure::Internal.frame()]);
            assert_eq!(keys.opened, 0);
        }
        assert_eq!(
            (first.device.transcript().len(), ninth.transcript().len()),
            start
        );
        assert_eq!(fixture.fingerprints(), order);
    }

    #[test]
    fn removals_of_one_several_the_authorizing_and_every_key() {
        let keys = [Key::new(40), Key::new(41), Key::new(42), Key::new(43)];
        let all: Vec<&Key> = keys.iter().collect();
        let fixture = Fixture::new();
        fixture.seed(&record(&all, None, None));
        let by = &keys[0];
        // One key: the authorizing key stays.
        let order = fixture.fingerprints();
        let position = |key: &Key| {
            order
                .iter()
                .position(|fp| *fp == key.fingerprint())
                .unwrap() as u8
                + 1
        };
        let removed = slots(&order, &[position(&keys[1])]);
        let (result, seen) = unlock(
            &fixture,
            &[&by.device],
            begins(removing(4, &removed, LoginStep::Identify)),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(seen, removal(&order, &removed, by.fingerprint()));
        let mut left = vec![keys[0].id.clone(), keys[2].id.clone(), keys[3].id.clone()];
        left.sort();
        assert_eq!(fixture.stored(), Some(left));
        // Several, including the authorizing key itself.
        let order = fixture.fingerprints();
        let position = |key: &Key| {
            order
                .iter()
                .position(|fp| *fp == key.fingerprint())
                .unwrap() as u8
                + 1
        };
        let mut positions = vec![position(&keys[0]), position(&keys[3])];
        positions.sort();
        let removed = slots(&order, &positions);
        let (result, seen) = unlock(
            &fixture,
            &[&by.device],
            begins(removing(3, &removed, LoginStep::Identify)),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(seen, removal(&order, &removed, by.fingerprint()));
        assert_eq!(fixture.stored(), Some(vec![keys[2].id.clone()]));
        // The removed key no longer unlocks; the one left does.
        assert_eq!(
            unlock(&fixture, &[&by.device], pin(PIN)).0,
            Err(Failure::NotEnrolled)
        );
        assert_eq!(unlock(&fixture, &[&keys[2].device], pin(PIN)).0, Ok(()));
        // The last key removes every key: the record is unlinked, even with
        // no deployment marked, since no version is written.
        let order = fixture.fingerprints();
        let removed = slots(&order, &[1]);
        let context = Context {
            retained: &(&[][..], &[][..]),
            ..fixture.context()
        };
        let (result, seen) = run(
            &context,
            &mut Keys::all(&[&keys[2].device]),
            begins(removing(1, &removed, LoginStep::Identify)),
            TIME,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(seen, removal(&order, &removed, keys[2].fingerprint()));
        assert!(matches!(fixture.state(), State::Unenrolled));
        assert!(fixture.names().is_empty());
    }

    #[test]
    fn a_pin_wait_ends_with_its_session_not_the_operation() {
        let key = Key::new(70);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let tokens = key.pin_tokens();
        // Root holds the PIN far longer than the session lives, and sends
        // it only if the worker has not ended the operation first: it ends
        // at the session deadline, in about its lifetime, not when the PIN
        // would come or at the operation deadline.
        let held = Duration::from_secs(20);
        let context = Context {
            lifetime: Duration::from_secs(5),
            ..fixture.context()
        };
        let (result, seen, took) = timed(
            &context,
            &mut Keys::all(&[&key.device]),
            Plan {
                pin_delay: Some(held),
                ..pin(PIN)
            },
            TIME,
        );
        assert_eq!(result, Err(Failure::Timeout));
        assert!(took < held, "{took:?}");
        assert_eq!(
            seen,
            failed(
                through_unlock(&order, key.fingerprint(), 8),
                Failure::Timeout
            )
        );
        assert_eq!(key.pin_tokens(), tokens);
        assert_eq!(key.device.state().retries, 8);
        // A PIN held briefly within a long session unlocks.
        let context = Context {
            lifetime: Duration::from_secs(60),
            ..fixture.context()
        };
        let (result, seen) = run(
            &context,
            &mut Keys::all(&[&key.device]),
            Plan {
                pin_delay: Some(Duration::from_millis(50)),
                ..pin(PIN)
            },
            TIME,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(seen, succeeded(&order, key.fingerprint(), 8));
    }

    #[test]
    fn a_key_stalling_initialization_past_its_session_is_a_timeout() {
        let key = Key::new(71);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let identified = vec![
            baseline(&[key.fingerprint()]),
            invitation(0x10, &identify(1)),
        ];
        let context = Context {
            lifetime: Duration::from_secs(2),
            ..fixture.context()
        };
        // Stalled to the session deadline, well within the operation's; a
        // failure at once is no timeout.
        for (wait, failure) in [(true, Failure::Timeout), (false, Failure::Failed)] {
            let mut keys = Keys::all(&[&key.device]);
            keys.stall = Some((0, wait));
            let (result, seen) = run(&context, &mut keys, pin(PIN), TIME);
            assert_eq!(result, Err(failure), "{wait}");
            assert_eq!(seen, failed(identified.clone(), failure), "{wait}");
            assert_eq!(keys.opened, 1);
        }
    }

    #[test]
    fn a_commit_round_needs_its_margin_for_the_write() {
        // A margin longer than the whole operation: the invitation is never
        // sent and nothing is written, for a write and for an unlock.
        let key = blank("margin");
        let make = |step| enrolling(1, 1, step);
        let fixture = Fixture::new();
        let context = Context {
            margin: TIME * 2,
            ..fixture.context()
        };
        let (result, seen) = run(
            &context,
            &mut Keys::all(&[&key]),
            begins(make(LoginStep::Connect)),
            TIME,
        );
        assert_eq!(result, Err(Failure::Timeout));
        let new = fingerprint(&newest(&key));
        let mut frames = vec![
            vec![0x18, 0],
            invitation(0x10, &login(make(LoginStep::Connect))),
        ];
        frames.extend(ceremony(make, new));
        assert_eq!(seen, failed(frames, Failure::Timeout));
        assert_eq!(fixture.stored(), None);
        let enrolled = Key::new(72);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&enrolled], None, None));
        let order = [enrolled.fingerprint()];
        let context = Context {
            margin: TIME * 2,
            ..fixture.context()
        };
        let (result, seen) = run(
            &context,
            &mut Keys::all(&[&enrolled.device]),
            pin(PIN),
            TIME,
        );
        assert_eq!(result, Err(Failure::Timeout));
        assert_eq!(
            seen,
            failed(
                through_unlock(&order, enrolled.fingerprint(), 8),
                Failure::Timeout
            )
        );
        // Enough margin at the invitation, too little once root has
        // acknowledged it: the write is never attempted. The commit runs
        // alone, so only its own round races the clock.
        let bytes = record(&[&enrolled], None, None);
        let writes = std::cell::Cell::new(0);
        let counted = |store: &Store, baseline: Baseline, change: Change<'_>| {
            writes.set(writes.get() + 1);
            write(store, baseline, change)
        };
        let context = Context {
            margin: Duration::from_secs(5),
            write: &counted,
            ..fixture.context()
        };
        let presented = fixture.state().baseline().unwrap();
        let removal = removing(1, &slots(&order, &[1]), LoginStep::Identify);
        let ceremony = Ceremony {
            context: &context,
            presented,
            fingerprints: &order,
            current: login(removal),
            last: true,
        };
        let (worker, mut parent) = UnixStream::pair().unwrap();
        // The acknowledgement comes after 3 seconds, within a frame's time,
        // when 2 are all the deadline had beyond the margin.
        let root = std::thread::spawn(move || {
            let mut frame = read_frame(&mut parent).unwrap();
            assert_eq!(frame[0], 0x12);
            std::thread::sleep(Duration::from_secs(3));
            frame[0] = 0x13;
            write_frame(&mut parent, &frame);
            read_frame(&mut parent)
        });
        let mut wire = Wire::new(worker, Instant::now() + Duration::from_secs(7)).unwrap();
        let result = ceremony.commit(&mut wire, None, &mut entropy());
        drop(wire);
        assert_eq!(result, Err(Failure::Timeout));
        assert_eq!(root.join().unwrap(), None);
        assert_eq!(writes.get(), 0);
        assert_eq!(fixture.bytes(), Some(bytes));
    }

    #[test]
    fn the_probe_sends_the_batches_an_unlock_of_the_new_record_would() {
        let (one, two) = (Key::new(73), Key::new(74));
        let fixture = Fixture::new();
        fixture.seed(&record(&[&one, &two], None, None));
        // A key whose allow list holds two IDs: three credentials take two batches.
        let new = Virtual::new(
            Config {
                max_list: Some(2),
                ..Config::default()
            },
            Some(PIN),
            "batched",
        );
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, _) = operation(&fixture, &mut keys, begins(adding(2, LoginStep::Identify)));
        assert_eq!(result, Ok(()));
        let id = newest(&new);
        let mut ids = vec![one.id.clone(), two.id.clone(), id.clone()];
        ids.sort();
        assert_eq!(fixture.stored(), Some(ids.clone()));
        // Prove and repeat name the new credential alone; the probe sends the
        // record's batches in order until the one holding it.
        let lists = allow_lists(&new);
        let (ceremony, probe) = lists.split_at(2);
        assert_eq!(ceremony, [vec![id.clone()], vec![id.clone()]]);
        let batches: Vec<Vec<Vec<u8>>> = ids.chunks(2).map(<[Vec<u8>]>::to_vec).collect();
        let reached = batches
            .iter()
            .position(|batch| batch.contains(&id))
            .unwrap();
        assert_eq!(probe, &batches[..=reached]);
        // The new key then unlocks the record it joined.
        assert_eq!(unlock(&fixture, &[&new], pin(PIN)).0, Ok(()));
    }

    #[test]
    fn a_credprotect_default_fails_the_probe_and_publishes_nothing() {
        let key = Virtual::new(
            Config {
                cred_protect: Some(3),
                ..Config::default()
            },
            Some(PIN),
            "protected",
        );
        let fixture = Fixture::new();
        let (result, seen) = unlock(
            &fixture,
            &[&key],
            begins(enrolling(1, 1, LoginStep::Connect)),
        );
        assert_eq!(result, Err(Failure::Unprobed));
        let new = fingerprint(&newest(&key));
        let mut frames = vec![
            vec![0x18, 0],
            invitation(0x10, &login(enrolling(1, 1, LoginStep::Connect))),
        ];
        frames.extend(ceremony(|step| enrolling(1, 1, step), new));
        assert_eq!(seen, failed(frames, Failure::Unprobed));
        assert_eq!(fixture.stored(), None);
    }

    #[test]
    fn denied_presence_and_always_uv_end_the_operation_with_their_kinds() {
        let denied = Script {
            presence: Presence::Denied,
            ..Script::default()
        };
        let make = |step| enrolling(1, 1, step);
        let connected = vec![
            vec![0x18, 0],
            invitation(0x10, &login(make(LoginStep::Connect))),
        ];
        // alwaysUv is refused at the new key's getInfo, before any PIN.
        let fixture = Fixture::new();
        let strict = Virtual::new(
            Config {
                always_uv: Some(true),
                ..Config::default()
            },
            Some(PIN),
            "always-uv",
        );
        let (result, seen) = unlock(&fixture, &[&strict], begins(make(LoginStep::Connect)));
        let refused = Failure::Refused(LoginRefusal::AlwaysUv);
        assert_eq!(result, Err(refused));
        assert_eq!(seen, failed(connected.clone(), refused));
        assert_eq!(seen.last(), Some(&vec![0x15, 6, 2]));
        assert_eq!(pin_tokens(&strict), 0);
        assert!(sent(&strict, 1, 1).is_empty());
        assert!(fixture.names().is_empty());
        // Presence denied at the creation, after its PIN: no credential, and
        // the key's count still 8.
        let new = blank("denied-create");
        new.script(denied);
        let (result, seen) = unlock(&fixture, &[&new], begins(make(LoginStep::Connect)));
        assert_eq!(result, Err(Failure::Denied));
        let mut frames = connected.clone();
        frames.push(invitation(
            0x10,
            &login(make(LoginStep::Create { retries: 8 })),
        ));
        assert_eq!(seen, failed(frames, Failure::Denied));
        assert_eq!(seen.last(), Some(&vec![0x15, 7]));
        assert!(new.state().credentials.is_empty());
        assert_eq!(new.state().retries, 8);
        assert!(fixture.names().is_empty());
        // alwaysUv advertised false is admitted: the key enrolls.
        let relaxed = Virtual::new(
            Config {
                always_uv: Some(false),
                ..Config::default()
            },
            Some(PIN),
            "never-uv",
        );
        let (result, seen) = unlock(&fixture, &[&relaxed], begins(make(LoginStep::Connect)));
        assert_eq!(result, Ok(()));
        let key = fingerprint(&newest(&relaxed));
        let mut frames = connected;
        frames.extend(ceremony(make, key));
        assert_eq!(seen, committed(frames, make(LoginStep::Probe { key })));
        // Presence denied at its unlock assertion, after the PIN: DENIED,
        // the count reported 8 again; granted, the key unlocks.
        relaxed.script(denied);
        let (result, seen) = unlock(&fixture, &[&relaxed], pin(PIN));
        assert_eq!(result, Err(Failure::Denied));
        assert_eq!(
            seen,
            failed(through_unlock(&[key], key, 8), Failure::Denied)
        );
        assert_eq!(relaxed.state().retries, 8);
        relaxed.script(Script::default());
        let (result, seen) = unlock(&fixture, &[&relaxed], pin(PIN));
        assert_eq!(result, Ok(()));
        assert_eq!(seen, succeeded(&[key], key, 8));
        // The enrolled key later configured to require UV everywhere is
        // refused at the unlock's identify, before any PIN step.
        relaxed.configure(|config| config.always_uv = Some(true));
        let tokens = pin_tokens(&relaxed);
        let (result, seen) = unlock(&fixture, &[&relaxed], pin(PIN));
        assert_eq!(result, Err(refused));
        assert_eq!(
            seen,
            [
                baseline(&[key]),
                invitation(0x10, &identify(1)),
                vec![0x15, 6, 2]
            ]
        );
        assert_eq!(pin_tokens(&relaxed), tokens);
        relaxed.configure(|config| config.always_uv = Some(false));
        assert_eq!(unlock(&fixture, &[&relaxed], pin(PIN)).0, Ok(()));
    }

    #[test]
    fn a_key_whose_list_cannot_hold_the_exclusions_is_refused_before_a_pin() {
        let (one, two) = (Key::new(50), Key::new(51));
        let fixture = Fixture::new();
        let bytes = record(&[&one, &two], None, None);
        fixture.seed(&bytes);
        let order = fixture.fingerprints();
        let small = Virtual::new(
            Config {
                max_list: Some(1),
                ..Config::default()
            },
            Some(PIN),
            "small",
        );
        let mut keys = Keys::sessions(&[&one.device, &small], vec![vec![0], vec![0], vec![1]]);
        let (result, seen) = operation(&fixture, &mut keys, begins(adding(2, LoginStep::Identify)));
        let refused = Failure::Refused(LoginRefusal::ListTooSmall);
        assert_eq!(result, Err(refused));
        assert_eq!(
            seen,
            failed(authorized_addition(&order, one.fingerprint()), refused)
        );
        assert_eq!(pin_tokens(&small), 0);
        assert_eq!(fixture.bytes(), Some(bytes));
    }

    #[test]
    fn an_enrolled_key_offered_as_the_new_one_is_excluded() {
        let (one, two) = (Key::new(52), Key::new(53));
        let fixture = Fixture::new();
        let bytes = record(&[&one, &two], None, None);
        fixture.seed(&bytes);
        let order = fixture.fingerprints();
        // The second enrolled key, connected after the authorizing one.
        let mut keys = Keys::sessions(&[&one.device, &two.device], vec![vec![0], vec![0], vec![1]]);
        let (result, seen) = operation(&fixture, &mut keys, begins(adding(2, LoginStep::Identify)));
        assert_eq!(result, Err(Failure::Excluded));
        let mut frames = authorized_addition(&order, one.fingerprint());
        frames.push(invitation(
            0x10,
            &login(adding(2, LoginStep::Create { retries: 8 })),
        ));
        assert_eq!(seen, failed(frames, Failure::Excluded));
        assert_eq!(two.device.state().credentials.len(), 1);
        assert_eq!(fixture.bytes(), Some(bytes));
    }

    #[test]
    fn a_repeat_that_does_not_reproduce_the_secret_fails() {
        let key = blank("wrong-repeat");
        let fixture = Fixture::new();
        let mut keys = Keys::all(&[&key]);
        // The repeat's session: the key flips its output from then on.
        keys.scripts = vec![(
            1,
            Script {
                output: Output::Wrong,
                ..Script::default()
            },
        )];
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            begins(enrolling(1, 1, LoginStep::Connect)),
        );
        assert_eq!(result, Err(Failure::Failed));
        let new = fingerprint(&newest(&key));
        let mut frames = vec![
            vec![0x18, 0],
            invitation(0x10, &login(enrolling(1, 1, LoginStep::Connect))),
        ];
        let mut steps = ceremony(|step| enrolling(1, 1, step), new);
        steps.pop();
        frames.extend(steps);
        assert_eq!(seen, failed(frames, Failure::Failed));
        assert_eq!(fixture.stored(), None);
    }

    #[test]
    fn a_store_changed_before_token_io_or_before_publication_refuses() {
        let (one, two, other) = (Key::new(54), Key::new(55), Key::new(56));
        let bytes = record(&[&one, &two], None, None);
        let changed = record(&[&one, &two, &other], None, None);
        // An addition: changed before any token I/O.
        let fixture = Fixture::new();
        fixture.seed(&bytes);
        let order = fixture.fingerprints();
        let new = blank("changed-new");
        let start = one.device.transcript().len();
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            Plan {
                change: Some((fixture.dir.clone(), Some(changed.clone()), At::Baseline)),
                ..begins(adding(2, LoginStep::Identify))
            },
        );
        assert_eq!(result, Err(Failure::Changed));
        assert_eq!(
            seen,
            failed(
                vec![
                    baseline(&order),
                    invitation(0x10, &login(adding(2, LoginStep::Identify)))
                ],
                Failure::Changed
            )
        );
        assert_eq!(one.device.transcript().len(), start);
        assert_eq!(keys.opened, 0);
        // Changed, or removed, at the commit: refused after the whole
        // ceremony, without attempting the write.
        let writes = std::cell::Cell::new(0);
        let counted = |store: &Store, baseline: Baseline, change: Change<'_>| {
            writes.set(writes.get() + 1);
            write(store, baseline, change)
        };
        for replacement in [Some(changed.clone()), None] {
            fixture.seed(&bytes);
            let new = blank("changed-late");
            let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
            let context = Context {
                write: &counted,
                ..fixture.context()
            };
            let (result, seen) = run(
                &context,
                &mut keys,
                Plan {
                    change: Some((fixture.dir.clone(), replacement.clone(), At::Commit)),
                    ..begins(adding(2, LoginStep::Identify))
                },
                TIME,
            );
            assert_eq!(result, Err(Failure::Changed));
            let mut frames = addition(&order, one.fingerprint(), fingerprint(&newest(&new)));
            frames.pop();
            assert_eq!(seen, failed(frames, Failure::Changed));
            assert_eq!(fixture.bytes(), replacement);
            assert_eq!(writes.get(), 0);
        }
        // A directory that cannot be opened at the commit has not been shown
        // to differ: it is unavailable.
        fixture.seed(&bytes);
        let new = blank("damaged-late");
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let context = Context {
            write: &counted,
            ..fixture.context()
        };
        let (result, seen) = run(
            &context,
            &mut keys,
            Plan {
                damage: Some((fixture.dir.clone(), At::Commit)),
                ..begins(adding(2, LoginStep::Identify))
            },
            TIME,
        );
        let damaged = Failure::Unavailable(Cause::DirectoryDamaged);
        assert_eq!(result, Err(damaged));
        let mut frames = addition(&order, one.fingerprint(), fingerprint(&newest(&new)));
        frames.pop();
        assert_eq!(seen, failed(frames, damaged));
        assert_eq!(writes.get(), 0);
        fs::set_permissions(&fixture.dir, Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fixture.bytes(), Some(bytes.clone()));
        // A first enrollment over a record that appeared since its baseline.
        let fixture = Fixture::new();
        let key = blank("changed-enroll");
        let (result, seen) = unlock(
            &fixture,
            &[&key],
            Plan {
                change: Some((fixture.dir.clone(), Some(bytes.clone()), At::Commit)),
                ..begins(enrolling(1, 1, LoginStep::Connect))
            },
        );
        assert_eq!(result, Err(Failure::Changed));
        assert_eq!(seen.last(), Some(&Failure::Changed.frame()));
        assert_eq!(fixture.bytes(), Some(bytes));
    }

    #[test]
    fn a_version_no_deployment_shares_refuses_before_any_token() {
        let (key, other) = (Key::new(57), Key::new(58));
        let one = record(&[&key], None, None);
        let two = record(&[&key, &other], None, None);
        let writes = std::cell::Cell::new(0);
        let counted = |store: &Store, baseline: Baseline, change: Change<'_>| {
            writes.set(writes.get() + 1);
            write(store, baseline, change)
        };
        // Production's own empty sets first.
        for (current, previous) in [
            (&[][..], &[][..]),
            (&[], &[VERSION]),
            (&[VERSION], &[]),
            (&[2], &[2]),
            (&[VERSION], &[2]),
        ] {
            let start = key.device.transcript().len();
            // An addition, a removal that leaves a key, and a first enrollment.
            let fixture = Fixture::new();
            fixture.seed(&two);
            let order = fixture.fingerprints();
            let at = order
                .iter()
                .position(|fp| *fp == key.fingerprint())
                .unwrap() as u8
                + 1;
            let removal = removing(2, &slots(&order, &[at]), LoginStep::Identify);
            for (bytes, plan, frame) in [
                (
                    Some(one.clone()),
                    begins(adding(1, LoginStep::Identify)),
                    baseline(&[key.fingerprint()]),
                ),
                (Some(two.clone()), begins(removal), baseline(&order)),
                (
                    None,
                    begins(enrolling(1, 1, LoginStep::Connect)),
                    vec![0x18, 0],
                ),
            ] {
                replace(&fixture.dir, bytes.as_deref());
                let context = Context {
                    retained: &(current, previous),
                    write: &counted,
                    ..fixture.context()
                };
                let (result, seen) = run(&context, &mut Keys::all(&[&key.device]), plan, TIME);
                assert_eq!(result, Err(Failure::Version));
                assert_eq!(seen, [frame, Failure::Version.frame()]);
                assert_eq!(writes.get(), 0);
                assert_eq!(fixture.bytes(), bytes);
            }
            assert_eq!(key.device.transcript().len(), start);
        }
    }

    #[test]
    fn store_failures_are_typed_and_an_uncertain_write_reports_its_re_read() {
        let (one, two) = (Key::new(59), Key::new(60));
        let bytes = record(&[&one, &two], None, None);
        let stages = [
            "Created",
            "Written",
            "FileSynced",
            "RenameAttempted",
            "Renamed",
            "DirectorySynced",
        ];
        // A publication over a present record: an addition.
        for stage in stages {
            let fixture = Fixture::new();
            fixture.seed(&bytes);
            let fail = move |at: &str| {
                if at == stage {
                    Err("injected".to_string())
                } else {
                    Ok(())
                }
            };
            let injected = |store: &Store, baseline: Baseline, change: Change<'_>| match change {
                Change::Publish(record, suffix) => {
                    store.publish_at(baseline, record, &mut &suffix[..], &mut |at| fail(at))
                }
                Change::Remove => store.remove_at(baseline, &mut |at| fail(at)),
            };
            let context = Context {
                write: &injected,
                ..fixture.context()
            };
            let new = blank(&format!("stage-{stage:?}"));
            let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
            let (result, seen) = run(
                &context,
                &mut keys,
                begins(adding(2, LoginStep::Identify)),
                TIME,
            );
            let expected = match stage {
                "Created" | "Written" | "FileSynced" => Failure::Failed,
                "RenameAttempted" => Failure::Uncertain(Found::Old),
                _ => Failure::Uncertain(Found::New),
            };
            assert_eq!(result, Err(expected), "{stage:?}");
            assert_eq!(seen.last(), Some(&expected.frame()), "{stage:?}");
            let published = matches!(stage, "Renamed" | "DirectorySynced");
            assert_eq!(
                fixture.bytes() == Some(bytes.clone()),
                !published,
                "{stage:?}"
            );
            assert_eq!(fixture.stored().unwrap().len(), 2 + usize::from(published));
        }
        // A first enrollment's publication from no record.
        for (stage, expected) in [
            ("Created", Failure::Failed),
            ("RenameAttempted", Failure::Uncertain(Found::Absent)),
            ("Renamed", Failure::Uncertain(Found::New)),
        ] {
            let fixture = Fixture::new();
            let injected = |store: &Store, baseline: Baseline, change: Change<'_>| match change {
                Change::Publish(record, suffix) => {
                    store.publish_at(baseline, record, &mut &suffix[..], &mut |at| {
                        if at == stage {
                            Err("injected".into())
                        } else {
                            Ok(())
                        }
                    })
                }
                Change::Remove => Outcome::Rejected("unexpected".into()),
            };
            let context = Context {
                write: &injected,
                ..fixture.context()
            };
            let key = blank("stage-enroll");
            let (result, _) = run(
                &context,
                &mut Keys::all(&[&key]),
                begins(enrolling(1, 1, LoginStep::Connect)),
                TIME,
            );
            assert_eq!(result, Err(expected), "{stage:?}");
        }
        // Removing every key: an attempted unlink is uncertain.
        for (stage, expected) in [
            ("UnlinkAttempted", Failure::Uncertain(Found::Old)),
            ("Unlinked", Failure::Uncertain(Found::Absent)),
            ("DirectorySynced", Failure::Uncertain(Found::Absent)),
        ] {
            let fixture = Fixture::new();
            let only = record(&[&one], None, None);
            fixture.seed(&only);
            let injected = |store: &Store, baseline: Baseline, change: Change<'_>| match change {
                Change::Remove => store.remove_at(baseline, &mut |at| {
                    if at == stage {
                        Err("injected".into())
                    } else {
                        Ok(())
                    }
                }),
                Change::Publish(..) => Outcome::Rejected("unexpected".into()),
            };
            let context = Context {
                write: &injected,
                ..fixture.context()
            };
            let removed = [Slot {
                position: 1,
                key: one.fingerprint(),
            }];
            let (result, seen) = run(
                &context,
                &mut Keys::all(&[&one.device]),
                begins(removing(1, &removed, LoginStep::Identify)),
                TIME,
            );
            assert_eq!(result, Err(expected), "{stage:?}");
            assert_eq!(seen.last(), Some(&expected.frame()));
        }
        // A rejection by a record that changed under the write is RECORD
        // CHANGED; one that was replaced after the rename is another record.
        let fixture = Fixture::new();
        fixture.seed(&bytes);
        let other = record(&[&two], None, None);
        let dir = fixture.dir.clone();
        let racing = |store: &Store, baseline: Baseline, change: Change<'_>| match change {
            Change::Publish(record, suffix) => {
                store.publish_at(baseline, record, &mut &suffix[..], &mut |at| {
                    if at == "FileSynced" {
                        replace(&dir, Some(&other));
                    }
                    Ok(())
                })
            }
            Change::Remove => Outcome::Rejected("unexpected".into()),
        };
        let context = Context {
            write: &racing,
            ..fixture.context()
        };
        let new = blank("racing");
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, _) = run(
            &context,
            &mut keys,
            begins(adding(2, LoginStep::Identify)),
            TIME,
        );
        assert_eq!(result, Err(Failure::Changed));
        assert_eq!(fixture.bytes(), Some(other.clone()));
        let fixture = Fixture::new();
        fixture.seed(&bytes);
        let dir = fixture.dir.clone();
        let replaced = |store: &Store, baseline: Baseline, change: Change<'_>| match change {
            Change::Publish(record, suffix) => {
                store.publish_at(baseline, record, &mut &suffix[..], &mut |at| {
                    if at == "Renamed" {
                        replace(&dir, Some(&other));
                        return Err("injected".into());
                    }
                    Ok(())
                })
            }
            Change::Remove => Outcome::Rejected("unexpected".into()),
        };
        let context = Context {
            write: &replaced,
            ..fixture.context()
        };
        let new = blank("replaced");
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, _) = run(
            &context,
            &mut keys,
            begins(adding(2, LoginStep::Identify)),
            TIME,
        );
        assert_eq!(result, Err(Failure::Uncertain(Found::Other)));
        // A directory damaged after the rename is unavailable on the re-read.
        let fixture = Fixture::new();
        fixture.seed(&bytes);
        let dir = fixture.dir.clone();
        let damaged = |store: &Store, baseline: Baseline, change: Change<'_>| match change {
            Change::Publish(record, suffix) => {
                store.publish_at(baseline, record, &mut &suffix[..], &mut |at| {
                    if at == "Renamed" {
                        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
                        return Err("injected".into());
                    }
                    Ok(())
                })
            }
            Change::Remove => Outcome::Rejected("unexpected".into()),
        };
        let context = Context {
            write: &damaged,
            ..fixture.context()
        };
        let new = blank("damaged");
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, _) = run(
            &context,
            &mut keys,
            begins(adding(2, LoginStep::Identify)),
            TIME,
        );
        assert_eq!(result, Err(Failure::Uncertain(Found::Unavailable)));
        fs::set_permissions(&fixture.dir, Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn root_cancelling_mid_ceremony_publishes_nothing() {
        let one = Key::new(61);
        let fixture = Fixture::new();
        let bytes = record(&[&one], None, None);
        fixture.seed(&bytes);
        // At the connect step, at the create step before its PIN, at the
        // repeat after a credential exists, and at the commit round.
        for (cancel, created) in [(2, false), (3, false), (5, true), (7, true)] {
            let new = blank(&format!("cancelled-{cancel}"));
            let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
            let (result, seen) = operation(
                &fixture,
                &mut keys,
                Plan {
                    cancel: Some(cancel),
                    ..begins(adding(1, LoginStep::Identify))
                },
            );
            assert_eq!(result, Err(Failure::Internal), "{cancel}");
            assert_eq!(seen.len(), cancel + 2, "{cancel}");
            assert_eq!(new.state().credentials.len(), usize::from(created));
            assert_eq!(fixture.bytes(), Some(bytes.clone()));
        }
    }

    #[test]
    fn a_wrong_pin_while_authorizing_or_creating_ends_the_operation() {
        let one = Key::new(62);
        let fixture = Fixture::new();
        let bytes = record(&[&one], None, None);
        fixture.seed(&bytes);
        let order = [one.fingerprint()];
        let new = blank("wrong-create");
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            Plan {
                wrong: Some(LoginStep::Authorize {
                    key: [0; 4],
                    retries: 0,
                }),
                ..begins(adding(1, LoginStep::Identify))
            },
        );
        assert_eq!(result, Err(Failure::WrongPin(8)));
        let mut frames = authorized_addition(&order, one.fingerprint());
        frames.pop();
        assert_eq!(seen, failed(frames, Failure::WrongPin(8)));
        assert_eq!(one.device.state().retries, 7);
        assert_eq!(keys.opened, 2);
        let mut keys = Keys::sessions(&[&one.device, &new], vec![vec![0], vec![0], vec![1]]);
        let (result, seen) = operation(
            &fixture,
            &mut keys,
            Plan {
                wrong: Some(LoginStep::Create { retries: 0 }),
                ..begins(adding(1, LoginStep::Identify))
            },
        );
        assert_eq!(result, Err(Failure::WrongPin(8)));
        let mut frames = authorized_addition(&order, one.fingerprint());
        frames[2] = invitation(
            0x10,
            &login(adding(
                1,
                LoginStep::Authorize {
                    key: one.fingerprint(),
                    retries: 7,
                },
            )),
        );
        frames.push(invitation(
            0x10,
            &login(adding(1, LoginStep::Create { retries: 8 })),
        ));
        assert_eq!(seen, failed(frames, Failure::WrongPin(8)));
        assert_eq!(new.state().retries, 7);
        assert!(new.state().credentials.is_empty());
        assert_eq!(fixture.bytes(), Some(bytes));
    }

    /// The qemu-secret login guests, which share this module's root and keys.
    mod vm {
        include!("login_vm.rs");
    }
}
