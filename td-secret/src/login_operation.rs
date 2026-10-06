//! The root login-key worker (td-login/TOKEN-LOGIN.md). This increment
//! implements session unlock only; nothing in production starts it yet.

use crate::consent::{Admitted, Fingerprint, LoginStep, Operation, Request};
use crate::fido_device::{self, Interruption};
use crate::fido_p256::PublicKey;
use crate::fido_pin::{LoginRefusal, Pin};
use crate::fido_transaction::{Channel, Error, LoginAssertion, LoginError, Status, Transaction};
use crate::login_record::{self, Phase, Record};
use crate::login_store::{self, Baseline, Cause, Owner, State};
use crate::operation::{self, remaining, Wire};
use crate::store;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// Unlock is one key ceremony (td-authd/DESIGN.md, "Login keys", Deadlines).
const UNLOCK_TIME: Duration = Duration::from_secs(120);
/// The worker's first frame: the login state it read, before any token I/O.
const BASELINE: u8 = 0x18;
/// The worker's typed failure frame.
const FAILURE: u8 = 0x15;
const UNENROLLED: u8 = 0;
const ENROLLED: u8 = 1;

/// Why an operation failed, as root shows it. The frame is `0x15`, the
/// kind byte, and for two kinds one more byte; td-secret/DESIGN.md pins them.
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
    /// than the one identify used.
    OneKey,
    Refused(LoginRefusal),
    /// The key denied presence.
    Denied,
    Timeout,
    NoRecord,
    Unavailable(Cause),
    /// The record no longer reads as the baseline the worker presented.
    Changed,
    /// Any other token or verification failure.
    Failed,
    /// td's own processes or channel, not the key.
    Internal,
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
            Self::Denied => (7, None),
            Self::Timeout => (8, None),
            Self::NoRecord => (9, None),
            Self::Unavailable(Cause::DirectoryDamaged) => (10, None),
            Self::Unavailable(Cause::RecordDamaged) => (11, None),
            Self::Unavailable(Cause::Unreadable) => (12, None),
            Self::Changed => (13, None),
            // 14 is UNCERTAIN, which only a write can report.
            Self::Failed => (15, None),
            Self::Internal => (16, None),
        };
        let mut frame = vec![FAILURE, kind];
        frame.extend(detail);
        frame
    }
}

/// Where the record lives and who must own it: root's in production.
struct Directory<'a> {
    path: &'a Path,
    owner: Owner,
}

/// Token access: the root USB transport, or virtual keys in tests.
trait Devices {
    type Node: Copy + Eq;
    type Channel: Channel;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String>;
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Channel, String>;
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
}

pub(super) fn run(uid: u32) -> Result<(), String> {
    let stream = operation::startup()?;
    let deadline = Instant::now()
        .checked_add(UNLOCK_TIME)
        .ok_or("login operation deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    let directory = Directory {
        path: Path::new(login_store::DIRECTORY),
        owner: Owner::ROOT,
    };
    let result = match File::open("/dev/urandom") {
        Ok(mut random) => operate(
            &mut wire,
            uid,
            &directory,
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
    uid: u32,
    directory: &Directory<'_>,
    devices: &mut D,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    memory: impl FnOnce() -> Result<(), String>,
) -> Result<(), Failure> {
    let result = memory()
        .map_err(|_| Failure::Internal)
        .and_then(|()| perform(wire, uid, directory, devices, entropy));
    report(wire, result)
}

fn report(wire: &mut Wire, result: Result<(), Failure>) -> Result<(), Failure> {
    if let Err(failure) = result {
        let _ = wire.send(&failure.frame());
    }
    result
}

fn perform<D: Devices>(
    wire: &mut Wire,
    uid: u32,
    directory: &Directory<'_>,
    devices: &mut D,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<(), Failure> {
    let state = login_store::read(directory.path, directory.owner, uid);
    let (record, presented) = match (&state, state.baseline()) {
        (State::Unavailable(cause), _) => return Err(Failure::Unavailable(*cause)),
        (State::Enrolled(record), Some(presented)) => (Some(record), presented),
        (_, Some(presented)) => (None, presented),
        (_, None) => return Err(Failure::Internal),
    };
    let fingerprints: Vec<Fingerprint> = record
        .map(|record| {
            record
                .slots()
                .iter()
                .map(|slot| slot.fingerprint())
                .collect()
        })
        .unwrap_or_default();
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
    // Enrollment, addition and removal arrive with the writes.
    if request.owner() != uid || !matches!(request.operation(), Operation::LoginUnlock { .. }) {
        return Err(Failure::Internal);
    }
    let record = record.ok_or(Failure::NoRecord)?;
    let begun = Request::begin_login(
        *request.nonce(),
        request.owner(),
        request.operation().clone(),
        &fingerprints,
    )
    .map_err(|_| Failure::Internal)?;
    if begun != request {
        return Err(Failure::Internal);
    }
    let ceremony = Ceremony {
        directory,
        uid,
        record,
        presented,
        fingerprints: &fingerprints,
    };
    ceremony.unlock(wire, &request, devices, entropy)
}

/// One unlock against the baseline the worker presented.
struct Ceremony<'a> {
    directory: &'a Directory<'a>,
    uid: u32,
    record: &'a Record,
    presented: Baseline,
    fingerprints: &'a [Fingerprint],
}

impl Ceremony<'_> {
    fn unlock<D: Devices>(
        &self,
        wire: &mut Wire,
        identify: &Request,
        devices: &mut D,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), Failure> {
        wire.acknowledge(0x10, 0x11, identify)
            .map_err(|_| framing(wire))?;
        self.unchanged()?;
        let node = one(devices)?;
        let random = fresh(entropy)?;
        let hash =
            login_record::client_data_hash(Phase::Identify, &identify.encode(), None, &random)
                .map_err(|_| Failure::Internal)?;
        let ids: Vec<&[u8]> = self
            .record
            .slots()
            .iter()
            .map(|slot| slot.credential())
            .collect();
        let selected = Transaction::new(open(devices, node, wire)?)
            .map_err(LoginError::from)
            .and_then(|transaction| transaction.identify(&ids, hash))
            .map_err(|error| token(error, None, wire))?;
        let slot = match selected {
            None => return Err(Failure::NotEnrolled),
            Some(index) => self.record.slots().get(index).ok_or(Failure::Internal)?,
        };
        // A key swapped or added since identify refuses before any PIN.
        if one(devices)? != node {
            return Err(Failure::OneKey);
        }
        let (x, y) = slot.key().coordinates();
        let key = PublicKey::from_coordinates(&x, &y).map_err(|_| Failure::Internal)?;
        let random = fresh(entropy)?;
        let channel = open(devices, node, wire)?;
        let mut step = None;
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
                        let presented =
                            self.present(wire, identify, slot.fingerprint(), retries, &random);
                        match presented {
                            Ok((request, pin, hash)) => {
                                step = Some(request);
                                reported = Some(retries);
                                Ok((pin, hash))
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
        let output = match result {
            Ok(output) => output,
            Err(LoginError::Failed(Error::PinInput)) => {
                return Err(cause.unwrap_or(Failure::Internal))
            }
            Err(error) => return Err(token(error, reported, wire)),
        };
        let verified = <&[u8; 32]>::try_from(output.bytes())
            .map_err(|_| Failure::Internal)
            .and_then(|output| {
                self.record
                    .check(slot.credential(), output)
                    .map_err(|_| Failure::Internal)
            })?;
        // The output is retired at once: unlocking releases nothing.
        drop(output);
        if !verified {
            return Err(Failure::Failed);
        }
        self.unchanged()?;
        let step = step.ok_or(Failure::Internal)?;
        wire.acknowledge(0x12, 0x13, &step)
            .map_err(|_| framing(wire))?;
        remaining(wire.deadline()).map_err(|_| Failure::Timeout)?;
        wire.send(&[0x14]).map_err(|_| framing(wire))
    }

    /// Presents the unlock step with the key's reported retries, takes the
    /// PIN, and returns the client-data hash bound to that exact step.
    fn present(
        &self,
        wire: &mut Wire,
        identify: &Request,
        key: Fingerprint,
        retries: u8,
        random: &[u8; 32],
    ) -> Result<(Request, Pin, [u8; 32]), Failure> {
        let Operation::LoginUnlock {
            account,
            before,
            after,
            ..
        } = identify.operation()
        else {
            return Err(Failure::Internal);
        };
        let request = Request::new(
            *identify.nonce(),
            identify.owner(),
            Operation::LoginUnlock {
                account: *account,
                before: *before,
                after: *after,
                step: LoginStep::Unlock { key, retries },
            },
        )
        .map_err(|_| Failure::Internal)?;
        // Root applies the same admission; the worker never presents a step it refuses.
        if identify.admit_login_step(&request, self.fingerprints, None) != Ok(Admitted::Last) {
            return Err(Failure::Internal);
        }
        wire.acknowledge(0x10, 0x11, &request)
            .map_err(|_| framing(wire))?;
        let pin = wire.receive_pin().map_err(|_| framing(wire))?;
        let hash = login_record::client_data_hash(Phase::Unlock, &request.encode(), None, random)
            .map_err(|_| Failure::Internal)?;
        Ok((request, pin, hash))
    }

    /// The record must still read as the presented baseline; an unavailable
    /// state never matches.
    fn unchanged(&self) -> Result<(), Failure> {
        let state = login_store::read(self.directory.path, self.directory.owner, self.uid);
        if state.baseline() == Some(self.presented) {
            Ok(())
        } else {
            Err(Failure::Changed)
        }
    }
}

/// Exactly one connected device, refused before any PIN otherwise.
fn one<D: Devices>(devices: &mut D) -> Result<D::Node, Failure> {
    match devices.discover().map_err(|_| Failure::Failed)?.as_slice() {
        [node] => Ok(*node),
        _ => Err(Failure::OneKey),
    }
}

fn open<D: Devices>(devices: &mut D, node: D::Node, wire: &Wire) -> Result<D::Channel, Failure> {
    devices
        .open(node, wire.deadline())
        .map_err(|_| expired_or(wire, Failure::Failed))
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

fn expired_or(wire: &Wire, failure: Failure) -> Failure {
    if remaining(wire.deadline()).is_err() {
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
    use crate::consent::Slot;
    use crate::crypto;
    use crate::fido_cbor::{self as cbor, Value};
    use crate::fido_ctap::fingerprint;
    use crate::fido_transaction::Enrollment;
    use crate::fido_virtual::{Config, Link, Script, Signing, Virtual};
    use crate::login_record::{NewKey, VERSION};
    use std::fs::{self, OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const UID: u32 = 1000;
    const NONCE: [u8; 32] = [42; 32];
    const PIN: &[u8] = b"1234";
    const WRONG: &[u8] = b"4321";

    /// A deterministic counter: draw N is SHA-256 of N, big-endian.
    fn entropy() -> impl FnMut(&mut [u8]) -> Result<(), String> {
        let mut count = 0u32;
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

        fn directory(&self) -> Directory<'_> {
            Directory {
                path: &self.dir,
                owner: self.owner,
            }
        }

        /// Publishes `bytes` as the record by rename, as the store does.
        fn seed(&self, bytes: &[u8]) {
            replace(&self.dir, bytes);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn replace(dir: &Path, bytes: &[u8]) {
        let temporary = dir.join("next");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .unwrap();
        file.set_permissions(Permissions::from_mode(0o600)).unwrap();
        file.write_all(bytes).unwrap();
        fs::rename(&temporary, dir.join(UID.to_string())).unwrap();
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
            let device = Virtual::new(Config::default(), Some(PIN), &format!("login-{seed}"));
            let salt = [seed ^ 0x33; 32];
            let created = Transaction::new(device.link())
                .unwrap()
                .login_create(
                    Enrollment {
                        challenge: [seed; 32],
                        user: [seed ^ 0x55; 32],
                        proof_challenge: [seed ^ 0xaa; 32],
                        salt,
                        excluded: &[],
                    },
                    &mut |_, _| Pin::new(PIN.into()),
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

        /// How many PIN tokens the key was asked for.
        fn pin_tokens(&self) -> usize {
            self.device
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

        /// Every getAssertion's client-data hash, in order.
        fn hashes(&self) -> Vec<Vec<u8>> {
            self.device
                .transcript()
                .iter()
                .filter(|(request, _)| request[0] == 2)
                .map(|(request, _)| {
                    cbor::decode(&request[1..])
                        .unwrap()
                        .required(&Value::Unsigned(2))
                        .unwrap()
                        .bytes()
                        .unwrap()
                        .to_vec()
                })
                .collect()
        }
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

    struct Keys<'a>(&'a [&'a Virtual]);
    impl Devices for Keys<'_> {
        type Node = usize;
        type Channel = Link;
        fn discover(&mut self) -> Result<Vec<usize>, String> {
            Ok((0..self.0.len()).collect())
        }
        fn open(&mut self, node: usize, _: Instant) -> Result<Link, String> {
            Ok(self.0[node].link())
        }
    }

    /// What root does beyond acknowledging every invitation it admits.
    #[derive(Clone, Default)]
    struct Plan {
        /// Raw bytes root writes after acknowledging the unlock step; none
        /// cancels there instead, closing the channel unacknowledged.
        pin: Option<Vec<u8>>,
        /// Answers nothing from the first invitation on.
        silent: bool,
        /// Replaces the record after the baseline, or at the unlock step.
        change: Option<(PathBuf, Vec<u8>, bool)>,
        /// Sent instead of the identify step root derives.
        description: Option<Vec<u8>>,
        /// Sends the PIN frame in place of this invitation's acknowledgement.
        early_pin: Option<usize>,
    }

    fn pin(bytes: &[u8]) -> Plan {
        Plan {
            pin: Some(pin_frame(bytes)),
            ..Plan::default()
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

    fn write_frame(stream: &mut UnixStream, bytes: &[u8]) {
        stream
            .write_all(&(bytes.len() as u16).to_be_bytes())
            .unwrap();
        stream.write_all(bytes).unwrap();
    }

    /// Root's side: every frame it saw, each invitation's round zeroed. It
    /// derives the identify step from the baseline and admits every later
    /// step with consent's own admission, as td-authd will.
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
        if let Some((dir, bytes, false)) = &plan.change {
            replace(dir, bytes);
        }
        let count = (keys.len() as u8).max(1);
        let mut current = identify(count);
        if !keys.is_empty() {
            assert_eq!(
                Request::begin_login(NONCE, UID, current.operation().clone(), &keys).unwrap(),
                current
            );
        }
        let description = plan.description.clone().unwrap_or(current.encode());
        write_frame(&mut stream, &description);
        let mut rounds: Vec<Vec<u8>> = Vec::new();
        let mut last = false;
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
            if tag == 0x10 && invitation != current {
                assert_eq!(
                    current.admit_login_step(&invitation, &keys, None),
                    Ok(Admitted::Last)
                );
                (current, last) = (invitation, true);
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
            if tag == 0x10 && last {
                if let Some((dir, bytes, true)) = &plan.change {
                    replace(dir, bytes);
                }
                if plan.pin.is_none() {
                    break;
                }
            }
            frame[0] += 1;
            write_frame(&mut stream, &frame);
            if let (0x10, true, Some(pin)) = (tag, last, &plan.pin) {
                stream.write_all(pin).unwrap();
                if pin.is_empty() {
                    break;
                }
            }
        }
        seen
    }

    fn identify(count: u8) -> Request {
        step(count, LoginStep::Identify)
    }

    fn step(count: u8, step: LoginStep) -> Request {
        Request::new(
            NONCE,
            UID,
            Operation::LoginUnlock {
                account: UID,
                before: count,
                after: count,
                step,
            },
        )
        .unwrap()
    }

    fn unlocking(count: u8, key: Fingerprint, retries: u8) -> Request {
        step(count, LoginStep::Unlock { key, retries })
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

    fn unlock(
        fixture: &Fixture,
        devices: &[&Virtual],
        plan: Plan,
    ) -> (Result<(), Failure>, Vec<Vec<u8>>) {
        let (worker, parent) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || root(parent, plan));
        let mut wire = Wire::new(worker, Instant::now() + Duration::from_secs(30)).unwrap();
        let result = operate(
            &mut wire,
            UID,
            &fixture.directory(),
            &mut Keys(devices),
            &mut entropy(),
            || Ok(()),
        );
        drop(wire);
        (result, root.join().unwrap())
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
            (Failure::Denied, &[0x15, 7]),
            (Failure::Timeout, &[0x15, 8]),
            (Failure::NoRecord, &[0x15, 9]),
            (Failure::Unavailable(Cause::DirectoryDamaged), &[0x15, 10]),
            (Failure::Unavailable(Cause::RecordDamaged), &[0x15, 11]),
            (Failure::Unavailable(Cause::Unreadable), &[0x15, 12]),
            (Failure::Changed, &[0x15, 13]),
            (Failure::Failed, &[0x15, 15]),
            (Failure::Internal, &[0x15, 16]),
        ] {
            assert_eq!(failure.frame(), frame, "{failure:?}");
        }
        // The new tags are distinct from every other private operation frame.
        assert_eq!([FAILURE, operation::PIN, BASELINE], [0x15, 0x16, 0x18]);
    }

    #[test]
    fn unlock_is_one_ceremony_within_the_transport_lifetime() {
        assert_eq!(UNLOCK_TIME, Duration::from_secs(120));
        assert!(UNLOCK_TIME <= fido_device::MAX_LIFETIME);
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
        // The key replays its previous signature over the fresh hash.
        key.device.script(Script {
            signing: Signing::Stale,
            ..Script::default()
        });
        let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
        assert_eq!(result, Err(Failure::Failed));
        assert_eq!(seen, refused);
    }

    #[test]
    fn an_unenrolled_or_unavailable_state_refuses_before_any_token() {
        let key = Key::new(12);
        let start = key.device.transcript().len();
        let fixture = Fixture::new();
        let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
        assert_eq!(result, Err(Failure::NoRecord));
        assert_eq!(seen, [vec![0x18, 0], vec![0x15, 9]]);
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
            let (result, seen) = unlock(&fixture, &[&key.device], pin(PIN));
            assert_eq!(result, Err(Failure::Unavailable(cause)));
            assert_eq!(seen, [Failure::Unavailable(cause).frame()]);
        }
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
                change: Some((fixture.dir.clone(), changed.clone(), false)),
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
                change: Some((fixture.dir.clone(), changed, true)),
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
    fn root_must_send_an_unlock_matching_the_baseline_for_this_account() {
        let key = Key::new(18);
        let fixture = Fixture::new();
        fixture.seed(&record(&[&key], None, None));
        let order = [key.fingerprint()];
        let login = |owner, count, step| {
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
        let remove = Request::new(
            NONCE,
            UID,
            Operation::LoginRemove {
                account: UID,
                before: 1,
                after: 0,
                removed: vec![Slot {
                    position: 1,
                    key: key.fingerprint(),
                }],
                step: LoginStep::Identify,
            },
        )
        .unwrap()
        .encode();
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
            login(1001, 1, LoginStep::Identify),
            login(UID, 2, LoginStep::Identify),
            login(
                UID,
                1,
                LoginStep::Unlock {
                    key: key.fingerprint(),
                    retries: 8,
                },
            ),
            remove,
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
        let mut wire = Wire::new(worker, Instant::now() + Duration::from_secs(30)).unwrap();
        let result = operate(
            &mut wire,
            UID,
            &fixture.directory(),
            &mut Keys(&[&key.device]),
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
}
