//! Private, presented enrollment steps; no token material crosses the channel.

use crate::consent::{Enrollment, Operation, Platform, Recovery, Request};
use crate::fido_enroll::{Credential, Info, MakeCredential};
use crate::operation::{remaining, Wire, OPERATION_TIME};
use crate::{crypto, fido_device, fido_hid, fido_metadata, operation, store, tpm};
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

struct Plan {
    nonce: [u8; 32],
    uid: u32,
    recovery: Recovery,
}

impl Plan {
    fn decode(bytes: &[u8], uid: u32) -> Result<Self, String> {
        let request = Request::decode(bytes)?;
        let recovery = match request.operation() {
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery,
                step: Enrollment::CreatePrimary,
            } if request.owner() == uid => *recovery,
            _ => {
                return Err("private enrollment must begin with the configured primary step".into())
            }
        };
        Ok(Self {
            nonce: *request.nonce(),
            uid: request.owner(),
            recovery,
        })
    }

    fn description(&self, step: Enrollment) -> Result<Request, String> {
        Request::new(
            self.nonce,
            self.uid,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: self.recovery,
                step,
            },
        )
    }
}

fn challenge(request: &Request) -> [u8; 32] {
    let mut bytes = b"td-secret/presented-enrollment/v1\0".to_vec();
    bytes.extend_from_slice(&request.encode());
    crypto::digest(&bytes)
}

trait EnrollmentDevice {
    type Creation;
    type Credential;
    type Prepared;

    fn create(
        &mut self,
        primary: Option<&Self::Credential>,
        challenge: [u8; 32],
    ) -> Result<Self::Creation, String>;
    fn prove(
        &mut self,
        creation: Self::Creation,
        challenge: [u8; 32],
    ) -> Result<Self::Credential, String>;
    fn prepare(
        &mut self,
        primary: Self::Credential,
        recovery: Option<Self::Credential>,
    ) -> Result<Self::Prepared, String>;
    fn commit(&mut self, prepared: Self::Prepared) -> Result<(), String>;
}

fn present(wire: &mut Wire, plan: &Plan, step: Enrollment) -> Result<Request, String> {
    let request = plan.description(step)?;
    wire.acknowledge(0x10, 0x11, &request)?;
    Ok(request)
}

fn perform(wire: &mut Wire, plan: &Plan, device: &mut impl EnrollmentDevice) -> Result<(), String> {
    let request = present(wire, plan, Enrollment::CreatePrimary)?;
    let creation = device.create(None, challenge(&request))?;
    let request = present(wire, plan, Enrollment::ProvePrimary)?;
    let primary = device.prove(creation, challenge(&request))?;
    let (request, recovery) = match plan.recovery {
        Recovery::Unrecoverable => (request, None),
        Recovery::SecondToken => {
            let request = present(wire, plan, Enrollment::CreateRecovery)?;
            let creation = device.create(Some(&primary), challenge(&request))?;
            let request = present(wire, plan, Enrollment::ProveRecovery)?;
            let recovery = device.prove(creation, challenge(&request))?;
            (request, Some(recovery))
        }
    };
    let prepared = device.prepare(primary, recovery)?;
    wire.acknowledge(0x12, 0x13, &request)?;
    remaining(wire.deadline())?;
    device.commit(prepared)?;
    wire.send(&[0x14])
}

trait Token {
    type Reply: AsRef<[u8]>;
    fn cbor(&mut self, bytes: &[u8]) -> Result<Self::Reply, String>;
}

impl Token for fido_device::Session {
    type Reply = fido_hid::Message;
    fn cbor(&mut self, bytes: &[u8]) -> Result<Self::Reply, String> {
        fido_device::Session::cbor(self, bytes)
    }
}

trait Devices {
    type Node: Copy + Eq;
    type Token: Token;
    type Tpm: tpm::Transport;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String>;
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Token, String>;
    fn tpm(&mut self) -> Result<tpm::Client<Self::Tpm>, String>;
}

struct Physical;
impl Devices for Physical {
    type Node = fido_device::Device;
    type Token = fido_device::Session;
    type Tpm = tpm::Device;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String> {
        fido_device::Device::discover()
    }
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Token, String> {
        fido_device::Session::open(node, deadline)
    }
    fn tpm(&mut self) -> Result<tpm::Client<Self::Tpm>, String> {
        Ok(tpm::Client::new(tpm::Device::open()?))
    }
}

struct Creation<T: Token> {
    token: T,
    request: MakeCredential,
    response: T::Reply,
}

struct Hardware<'a, D: Devices> {
    store: &'a store::Store,
    uid: u32,
    deadline: Instant,
    user: [u8; 32],
    primary: Option<D::Node>,
    devices: D,
}

impl<D: Devices> Drop for Hardware<'_, D> {
    fn drop(&mut self) {
        self.user.fill(0);
    }
}

impl<D: Devices> EnrollmentDevice for Hardware<'_, D> {
    type Creation = Creation<D::Token>;
    type Credential = Credential;
    type Prepared = fido_metadata::Metadata;

    fn create(
        &mut self,
        primary: Option<&Credential>,
        challenge: [u8; 32],
    ) -> Result<Self::Creation, String> {
        let previous = if primary.is_some() {
            self.primary
        } else {
            None
        };
        let device = wait_device(&mut self.devices, previous, self.deadline)?;
        if primary.is_none() {
            self.primary = Some(device);
        }
        let mut token = self.devices.open(device, self.deadline)?;
        let info = Info::parse(token.cbor(crate::fido_enroll::GET_INFO)?.as_ref())?;
        let request = match primary {
            Some(primary) => MakeCredential::recovery(info, challenge, self.user, primary)?,
            None => MakeCredential::primary(info, challenge, self.user)?,
        };
        let response = token.cbor(request.bytes())?;
        Ok(Creation {
            token,
            request,
            response,
        })
    }

    fn prove(
        &mut self,
        creation: Self::Creation,
        challenge: [u8; 32],
    ) -> Result<Credential, String> {
        let Creation {
            mut token,
            request,
            response,
        } = creation;
        let proof = request.proof(response.as_ref(), challenge)?;
        let response = token.cbor(proof.bytes())?;
        remaining(self.deadline)?;
        proof.verify(response.as_ref(), &mut self.devices.tpm()?)
    }

    fn prepare(
        &mut self,
        primary: Credential,
        recovery: Option<Credential>,
    ) -> Result<Self::Prepared, String> {
        remaining(self.deadline)?;
        let recovery = match &recovery {
            Some(credential) => fido_metadata::Recovery::SecondToken(credential),
            None => fido_metadata::Recovery::Unrecoverable,
        };
        fido_metadata::Metadata::new(self.uid, &primary, recovery)
    }

    fn commit(&mut self, metadata: Self::Prepared) -> Result<(), String> {
        remaining(self.deadline)?;
        self.store.enroll_tokens(metadata, tpm::Pcrs::parse("7")?)
    }
}

fn wait_device<D: Devices>(
    devices: &mut D,
    previous: Option<D::Node>,
    deadline: Instant,
) -> Result<D::Node, String> {
    loop {
        remaining(deadline)?;
        let mut candidates = devices.discover()?;
        candidates.retain(|node| Some(*node) != previous);
        if candidates.len() > 1 {
            return Err("connect exactly one token for enrollment".into());
        }
        if let Some(device) = candidates.pop() {
            remaining(deadline)?;
            return Ok(device);
        }
        // Discovery is read-only. A CTAP command is never retried after a reply is uncertain.
        std::thread::sleep(Duration::from_millis(100).min(remaining(deadline)?));
    }
}

pub fn run(uid: u32) -> Result<(), String> {
    let stream = operation::startup()?;
    let deadline = Instant::now()
        .checked_add(OPERATION_TIME)
        .ok_or("enrollment deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    store::lock_session(uid)?;
    let result = (|| {
        let plan = Plan::decode(&wire.receive()?, uid)?;
        let store = crate::owned_store(uid)?;
        if store.token_protected()? {
            return Err("credential store is already token enrolled".into());
        }
        let mut hardware = Hardware {
            store: &store,
            uid,
            deadline,
            user: [0; 32],
            primary: None,
            devices: Physical,
        };
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut hardware.user))
            .map_err(|_| "read enrollment user-handle randomness")?;
        perform(&mut wire, &plan, &mut hardware)
    })();
    // Enrollment remains locked even after successful persistent publication.
    let locked = store::lock_session(uid);
    match (result, locked) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; lock enrollment: {cleanup}")),
        (result, locked) => result.and(locked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    fn plan(recovery: Recovery) -> Plan {
        let request = Request::new(
            [42; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery,
                step: Enrollment::CreatePrimary,
            },
        )
        .unwrap();
        Plan::decode(&request.encode(), 1000).unwrap()
    }

    type Event = (u8, [u8; 32]);
    struct Fake {
        events: Arc<Mutex<Vec<Event>>>,
        reject_preparation: bool,
        commit_barrier: Option<Arc<std::sync::Barrier>>,
    }
    impl EnrollmentDevice for Fake {
        type Creation = u8;
        type Credential = u8;
        type Prepared = (u8, Option<u8>);
        fn create(&mut self, primary: Option<&u8>, challenge: [u8; 32]) -> Result<u8, String> {
            let step = match primary {
                None => 1,
                Some(&1) => 3,
                _ => return Err("recovery did not retain the proved primary".into()),
            };
            self.events.lock().unwrap().push((step, challenge));
            Ok(step)
        }
        fn prove(&mut self, creation: u8, challenge: [u8; 32]) -> Result<u8, String> {
            self.events.lock().unwrap().push((creation + 1, challenge));
            Ok(creation)
        }
        fn prepare(&mut self, primary: u8, recovery: Option<u8>) -> Result<Self::Prepared, String> {
            assert_eq!(primary, 1);
            assert!(matches!(recovery, None | Some(3)));
            if self.reject_preparation {
                return Err("fixture metadata refused".into());
            }
            Ok((primary, recovery))
        }
        fn commit(&mut self, _: Self::Prepared) -> Result<(), String> {
            self.events.lock().unwrap().push((5, [0; 32]));
            if let Some(barrier) = &self.commit_barrier {
                barrier.wait();
            }
            Ok(())
        }
    }

    #[test]
    fn every_step_needs_its_own_exact_presentation_before_device_io() {
        for recovery in [Recovery::Unrecoverable, Recovery::SecondToken] {
            let steps = if recovery == Recovery::SecondToken {
                4
            } else {
                2
            };
            // Omit or alter each acknowledgement, then exercise the complete sequence.
            for stop in 0..=steps + 1 {
                for altered in [false, true] {
                    let events = Arc::new(Mutex::new(Vec::new()));
                    let mut fake = Fake {
                        events: Arc::clone(&events),
                        reject_preparation: false,
                        commit_barrier: None,
                    };
                    let (child, parent) = UnixStream::pair().unwrap();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut parent = Wire::new(parent, deadline).unwrap();
                    let worker = std::thread::spawn(move || {
                        let mut child = Wire::new(child, deadline).unwrap();
                        perform(&mut child, &plan(recovery), &mut fake)
                    });
                    let plan = plan(recovery);
                    let ordered = [
                        Enrollment::CreatePrimary,
                        Enrollment::ProvePrimary,
                        Enrollment::CreateRecovery,
                        Enrollment::ProveRecovery,
                    ];
                    let mut previous_round = None;
                    for index in 0..=steps {
                        let mut prompt = parent.receive().unwrap();
                        assert_eq!(prompt[0], if index == steps { 0x12 } else { 0x10 });
                        let step = ordered[if index == steps { steps - 1 } else { index }];
                        assert_eq!(&prompt[33..], plan.description(step).unwrap().encode());
                        assert_ne!(previous_round.as_deref(), Some(&prompt[1..33]));
                        previous_round = Some(prompt[1..33].to_vec());
                        assert_eq!(events.lock().unwrap().len(), index);
                        if index == stop {
                            if altered {
                                prompt[0] += 1;
                                *prompt.last_mut().unwrap() ^= 1;
                                parent.send(&prompt).unwrap();
                            }
                            break;
                        }
                        prompt[0] += 1;
                        parent.send(&prompt).unwrap();
                    }
                    if stop == steps + 1 {
                        assert_eq!(parent.receive().unwrap(), [0x14]);
                    }
                    drop(parent);
                    assert_eq!(worker.join().unwrap().is_ok(), stop == steps + 1);
                    let events = events.lock().unwrap();
                    assert_eq!(events.len(), stop);
                    for (index, (_, actual)) in events.iter().take(steps).enumerate() {
                        assert_eq!(
                            *actual,
                            challenge(&plan.description(ordered[index]).unwrap())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn challenge_matches_independent_sha256_and_binds_the_recovery_policy() {
        let second = plan(Recovery::SecondToken);
        let actual = challenge(&second.description(Enrollment::CreatePrimary).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        // Python hashlib over the literal domain and independently encoded TDCONS01.
        assert_eq!(
            actual,
            "2a71ece890ccff196c08b780f5f6de9943f23fee7496f80a58191f774ab081cb"
        );
        assert_ne!(
            challenge(&second.description(Enrollment::CreatePrimary).unwrap()),
            challenge(
                &plan(Recovery::Unrecoverable)
                    .description(Enrollment::CreatePrimary)
                    .unwrap()
            )
        );
        assert_ne!(
            challenge(&second.description(Enrollment::CreatePrimary).unwrap()),
            challenge(&second.description(Enrollment::ProvePrimary).unwrap())
        );
        let other = Request::new(
            [43; 32],
            1000,
            second
                .description(Enrollment::CreatePrimary)
                .unwrap()
                .operation()
                .clone(),
        )
        .unwrap();
        assert_ne!(
            challenge(&second.description(Enrollment::CreatePrimary).unwrap()),
            challenge(&other)
        );
        let other = Request::new(
            [42; 32],
            1001,
            second
                .description(Enrollment::CreatePrimary)
                .unwrap()
                .operation()
                .clone(),
        )
        .unwrap();
        assert_ne!(
            challenge(&second.description(Enrollment::CreatePrimary).unwrap()),
            challenge(&other)
        );
    }

    #[test]
    fn enrollment_refuses_other_operations_owners_and_noninitial_steps() {
        let plan = plan(Recovery::SecondToken);
        assert!(Plan::decode(
            &plan
                .description(Enrollment::CreatePrimary)
                .unwrap()
                .encode(),
            1001
        )
        .is_err());
        for step in [
            Enrollment::ProvePrimary,
            Enrollment::CreateRecovery,
            Enrollment::ProveRecovery,
        ] {
            assert!(Plan::decode(&plan.description(step).unwrap().encode(), 1000).is_err());
        }
        let unlock = Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: crate::consent::Role::Primary,
            },
        )
        .unwrap();
        assert!(Plan::decode(&unlock.encode(), 1000).is_err());
    }

    #[test]
    fn metadata_refusal_precedes_commit_and_lost_completion_does_not_undo_publication() {
        for reject_preparation in [true, false] {
            let (child, parent) = UnixStream::pair().unwrap();
            let disconnect = parent.try_clone().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let events = Arc::new(Mutex::new(Vec::new()));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let mut fake = Fake {
                events: Arc::clone(&events),
                reject_preparation,
                commit_barrier: (!reject_preparation).then(|| Arc::clone(&barrier)),
            };
            let worker = std::thread::spawn(move || {
                perform(
                    &mut Wire::new(child, deadline).unwrap(),
                    &plan(Recovery::SecondToken),
                    &mut fake,
                )
            });
            let mut parent = Wire::new(parent, deadline).unwrap();
            for _ in 0..4 {
                let mut frame = parent.receive().unwrap();
                assert_eq!(frame[0], 0x10);
                frame[0] = 0x11;
                parent.send(&frame).unwrap();
            }
            if reject_preparation {
                assert!(parent.receive().is_err());
                drop(parent);
                assert_eq!(
                    worker.join().unwrap().unwrap_err(),
                    "fixture metadata refused"
                );
                assert_eq!(events.lock().unwrap().len(), 4);
            } else {
                let mut frame = parent.receive().unwrap();
                assert_eq!(frame[0], 0x12);
                frame[0] = 0x13;
                parent.send(&frame).unwrap();
                // Shutdown also covers transient aliases inherited by other tests' forks.
                disconnect.shutdown(std::net::Shutdown::Both).unwrap();
                drop(parent);
                barrier.wait();
                assert!(worker.join().unwrap().is_err());
                assert_eq!(
                    events.lock().unwrap().len(),
                    5,
                    "publication was incorrectly undone"
                );
            }
        }
    }

    // These fixtures exercise production wiring, not cryptographic verification.
    // The separate pinned TPM oracles verify real signatures.
    struct FixtureTpm;
    impl tpm::Transport for FixtureTpm {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
            let mut body = Vec::new();
            match code {
                0x167 => {
                    assert_eq!(&command[10..12], &[0, 0]);
                    let size = u16::from_be_bytes(command[12..14].try_into().unwrap()) as usize;
                    let public = &command[14..14 + size];
                    body.extend_from_slice(&0x80000000u32.to_be_bytes());
                    body.extend_from_slice(&34u16.to_be_bytes());
                    body.extend_from_slice(&0x000bu16.to_be_bytes());
                    body.extend_from_slice(&crypto::digest(public));
                }
                0x177 => body.extend_from_slice(&[0x80, 0x22, 0x40, 0, 0, 7, 0, 0]),
                0x165 => (),
                _ => panic!("unexpected fixture TPM command {code:x}"),
            }
            let mut response = vec![0x80, 1];
            response.extend_from_slice(&((10 + body.len()) as u32).to_be_bytes());
            response.extend_from_slice(&[0; 4]);
            response.extend(body);
            Ok(response)
        }
    }

    fn hex(bytes: &str) -> Vec<u8> {
        bytes
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    type Calls = Arc<Mutex<Vec<Vec<u8>>>>;
    type Nodes = Arc<Mutex<Vec<u8>>>;
    struct FixtureToken {
        node: u8,
        calls: Calls,
        closed: Nodes,
    }
    impl Drop for FixtureToken {
        fn drop(&mut self) {
            self.closed.lock().unwrap().push(self.node);
        }
    }
    impl Token for FixtureToken {
        type Reply = Vec<u8>;
        fn cbor(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
            use crate::fido_cbor::Encoder;
            self.calls.lock().unwrap().push(bytes.to_vec());
            if bytes == [4] {
                return Ok(hex(
                    "00a20181684649444f5f325f30035000000000000000000000000000000000",
                ));
            }
            let mut data = crypto::digest(crate::fido_ctap::RP_ID.as_bytes()).to_vec();
            data.push(if bytes[0] == 1 { 0x41 } else { 1 });
            data.extend_from_slice(&[0; 4]);
            let mut out = Encoder::new();
            match bytes[0] {
                1 => {
                    data.extend_from_slice(&[0; 16]);
                    data.extend_from_slice(&[0, 1, self.node]);
                    data.extend(hex(concat!(
                        "a5010203262001215820",
                        "ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a",
                        "225820552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553"
                    )));
                    out.head(5, 3).unwrap();
                    out.head(0, 1).unwrap();
                    out.text("none").unwrap();
                    out.head(0, 2).unwrap();
                    out.bytes(&data).unwrap();
                    out.head(0, 3).unwrap();
                    out.head(5, 0).unwrap();
                }
                2 => {
                    out.head(5, 2).unwrap();
                    out.head(0, 2).unwrap();
                    out.bytes(&data).unwrap();
                    out.head(0, 3).unwrap();
                    out.bytes(&hex("3006020101020101")).unwrap();
                }
                _ => panic!("unexpected fixture CTAP command"),
            }
            Ok([vec![0], out.finish().unwrap()].concat())
        }
    }
    struct FixtureDevices {
        discoveries: std::collections::VecDeque<Vec<u8>>,
        calls: Calls,
        opened: Nodes,
        closed: Nodes,
    }
    impl FixtureDevices {
        fn new(discoveries: Vec<Vec<u8>>) -> Self {
            Self {
                discoveries: discoveries.into(),
                calls: Arc::new(Mutex::new(Vec::new())),
                opened: Arc::new(Mutex::new(Vec::new())),
                closed: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }
    impl Devices for FixtureDevices {
        type Node = u8;
        type Token = FixtureToken;
        type Tpm = FixtureTpm;
        fn discover(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.discoveries.pop_front().unwrap_or_default())
        }
        fn open(&mut self, node: u8, _: Instant) -> Result<FixtureToken, String> {
            self.opened.lock().unwrap().push(node);
            Ok(FixtureToken {
                node,
                calls: Arc::clone(&self.calls),
                closed: Arc::clone(&self.closed),
            })
        }
        fn tpm(&mut self) -> Result<tpm::Client<FixtureTpm>, String> {
            Ok(tpm::Client::new(FixtureTpm))
        }
    }

    #[test]
    fn discovery_ignores_the_primary_but_refuses_ambiguous_new_tokens() {
        for (rows, previous, expected) in [
            (vec![vec![1]], None, Ok(1)),
            (vec![vec![1, 2]], Some(1), Ok(2)),
            (vec![vec![], vec![1], vec![1, 2]], Some(1), Ok(2)),
            (vec![vec![1, 2]], None, Err(())),
            (vec![vec![1, 2, 3]], Some(1), Err(())),
        ] {
            let mut devices = FixtureDevices::new(rows);
            let result = wait_device(
                &mut devices,
                previous,
                Instant::now() + Duration::from_secs(2),
            );
            assert_eq!(result.map_err(|_| ()), expected);
            assert!(devices.calls.lock().unwrap().is_empty());
            assert!(devices.opened.lock().unwrap().is_empty());
        }
        let mut devices = FixtureDevices::new(vec![]);
        assert!(wait_device(&mut devices, None, Instant::now()).is_err());
    }

    #[test]
    fn hardware_recovery_keeps_primary_exclusion_and_uses_a_separate_token_session() {
        use crate::fido_cbor::{self as cbor, Value};
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!(
            "td-enroll-wiring-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let owner = std::fs::metadata(&root).unwrap().uid();
        let store = store::Store::open_owned(&root.join("store"), 1000, owner, true).unwrap();
        let mut hardware = Hardware {
            store: &store,
            uid: 1000,
            deadline: Instant::now() + Duration::from_secs(2),
            user: [9; 32],
            primary: None,
            devices: FixtureDevices::new(vec![vec![1], vec![1, 2]]),
        };
        let creation = hardware.create(None, [1; 32]).unwrap();
        let primary = hardware.prove(creation, [2; 32]).unwrap();
        assert_eq!(primary.id(), [1]);
        assert_eq!(*hardware.devices.closed.lock().unwrap(), [1]);
        let creation = hardware.create(Some(&primary), [3; 32]).unwrap();
        let calls = hardware.devices.calls.lock().unwrap();
        let makes: Vec<_> = calls.iter().filter(|bytes| bytes[0] == 1).collect();
        assert_eq!(makes.len(), 2);
        let first = cbor::decode(&makes[0][1..]).unwrap();
        assert!(first.get(&Value::Unsigned(5)).unwrap().is_none());
        let second = cbor::decode(&makes[1][1..]).unwrap();
        let Value::Array(excluded) = second.required(&Value::Unsigned(5)).unwrap() else {
            panic!("missing exclusion");
        };
        assert_eq!(excluded.len(), 1);
        assert_eq!(
            excluded[0]
                .required(&Value::Text("id"))
                .unwrap()
                .bytes()
                .unwrap(),
            primary.id()
        );
        drop(calls);
        assert_eq!(*hardware.devices.opened.lock().unwrap(), [1, 2]);
        let recovery = hardware.prove(creation, [4; 32]).unwrap();
        assert_eq!(recovery.id(), [2]);
        assert_eq!(*hardware.devices.closed.lock().unwrap(), [1, 2]);
        // The stand-ins intentionally share a key: metadata refuses before commit.
        assert!(hardware.prepare(primary, Some(recovery)).is_err());
        assert!(!root.join("store/sealed").exists());
        drop(hardware);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
