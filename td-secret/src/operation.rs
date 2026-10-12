//! One private root-owned unlock operation, with separate presentation and commit.

use super::fido_pin::Pin;
use super::frames;
pub(super) use super::frames::{remaining, Wire};
use super::{consent, crypto, fido_device, fido_enroll, fido_metadata, store, tpm};
use std::fs::File;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Instant;

/// Root's PIN frame, the shared secret frame, as the tests spell it.
#[cfg(test)]
pub(super) const PIN: u8 = frames::SECRET;
pub(super) const OPERATION_TIME: std::time::Duration = fido_device::MAX_LIFETIME;

/// td-secret's frames over the shared endpoint (frames.rs): a credential,
/// a PIN, and the rounds that bind a consent description.
impl Wire {
    pub(super) fn receive_credential(&mut self) -> Result<Credential, String> {
        let deadline = self.frame_deadline()?;
        let mut header = [0; 2];
        self.read(&mut header, deadline)?;
        let length = usize::from(u16::from_be_bytes(header));
        if length == 0 || length > store::MAX_SECRET {
            return Err("invalid private credential length".into());
        }
        let mut credential = Credential(vec![0; length]);
        self.read(&mut credential.0, deadline)?;
        Ok(credential)
    }

    /// One PIN frame. A person types it, so its header waits for the
    /// operation deadline, or an open session's, rather than a frame's; the
    /// rest then has one frame time. The bytes stay in clearing owners and
    /// never enter a diagnostic.
    pub(super) fn receive_pin(&mut self) -> Result<Pin, String> {
        let mut pin = self.receive_secret()?;
        Pin::new(std::mem::take(&mut pin.0))
            .map_err(|_| "operation PIN is outside its profile".into())
    }

    pub(super) fn acknowledge(
        &mut self,
        tag: u8,
        answer: u8,
        request: &consent::Request,
    ) -> Result<(), String> {
        self.exchange(tag, answer, round()?, &request.encode(), None)
    }

    /// A login disclosure step's presentation round: root acknowledges it
    /// only once the person has read the disclosure and typed its approval
    /// key, with no token I/O open, so the answer's header waits for the
    /// operation deadline, or an open session's, as a PIN's does.
    pub(super) fn acknowledge_typed(
        &mut self,
        tag: u8,
        answer: u8,
        request: &consent::Request,
    ) -> Result<(), String> {
        let wait = Some(self.bound());
        self.exchange(tag, answer, round()?, &request.encode(), wait)
    }
}

/// One round's 32 random bytes, from the kernel.
fn round() -> Result<[u8; 32], String> {
    let mut round = [0; 32];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut round))
        .map_err(|_| "read operation round randomness")?;
    Ok(round)
}

pub(super) struct Credential(Vec<u8>);
impl Credential {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.0
    }
}
impl Drop for Credential {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(super) fn startup() -> Result<UnixStream, String> {
    store::require_root()?;
    let mut status = String::new();
    File::open("/proc/self/status")
        .and_then(|file| file.take(8193).read_to_string(&mut status))
        .map_err(|_| "read operation process identity")?;
    let input = std::fs::metadata("/proc/self/fd/0").map_err(|_| "missing operation endpoint")?;
    let log = |fd: u8| {
        std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .map_err(|_| "missing operation log descriptor")
    };
    let descriptors = [input, log(1)?, log(2)?];
    // The read_dir handle observes itself as fd 3; only 0/1/2 are inherited.
    let mut open = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd")
        .map_err(|_| "inspect operation descriptors")?
        .take(5)
    {
        open.push(
            entry
                .map_err(|_| "inspect operation descriptor")?
                .file_name(),
        );
    }
    frames::endpoint(frames::Process {
        status,
        descriptors,
        open,
    })
}

fn description(bytes: &[u8], owner: u32) -> Result<consent::Request, String> {
    let request = consent::Request::decode(bytes)?;
    if request.owner() != owner || !matches!(request.operation(), consent::Operation::Unlock { .. })
    {
        return Err("private unlock request does not match its configured session".into());
    }
    Ok(request)
}

fn challenge(request: &consent::Request) -> [u8; 32] {
    let mut bytes = b"td-secret/presented-unlock/v1\0".to_vec();
    bytes.extend_from_slice(&request.encode());
    crypto::digest(&bytes)
}

// These callbacks keep the same protocol on the real hardware and socket oracle.
fn perform<T>(
    wire: &mut Wire,
    request: &consent::Request,
    acquire: impl FnOnce([u8; 32], Instant) -> Result<T, String>,
    commit: impl FnOnce(T) -> Result<(), String>,
) -> Result<(), String> {
    wire.acknowledge(0x10, 0x11, request)?;
    let result = acquire(challenge(request), wire.deadline())?;
    wire.acknowledge(0x12, 0x13, request)?;
    remaining(wire.deadline())?;
    commit(result)?;
    wire.send(&[0x14])
}

pub fn run(uid: u32) -> Result<(), String> {
    let stream = startup()?;
    let deadline = Instant::now()
        .checked_add(OPERATION_TIME)
        .ok_or("operation deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    store::lock_session(uid)?;
    let request = description(&wire.receive()?, uid)?;
    let result = (|| {
        let store = super::owned_store(uid)?;
        if !store.token_protected()? {
            return Err("credential store is not token enrolled".into());
        }
        let role = match request.operation() {
            consent::Operation::Unlock {
                role: consent::Role::Primary,
            } => fido_metadata::Role::Primary,
            consent::Operation::Unlock {
                role: consent::Role::Recovery,
            } => fido_metadata::Role::Recovery,
            _ => return Err("unsupported private token operation".into()),
        };
        perform(
            &mut wire,
            &request,
            |challenge, deadline| {
                let mut devices = fido_device::Device::discover()?;
                if devices.len() != 1 {
                    return Err("connect exactly one token for this operation".into());
                }
                let device = devices
                    .pop()
                    .ok_or("token disappeared before initialization")?;
                let mut token = fido_device::Session::open(device, deadline)?;
                let info = fido_enroll::Info::parse(token.cbor(fido_enroll::GET_INFO)?.as_ref())?;
                let release = store.token_request(role, challenge, &info)?;
                let response = token.cbor(release.bytes())?;
                Ok((release, response))
            },
            |(release, response)| {
                store.release_token(
                    release,
                    response.as_ref(),
                    tpm::Client::new(tpm::Device::open()?),
                )
            },
        )
    })();
    if let Err(error) = result {
        return match store::lock_session(uid) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!("{error}; lock failed operation: {cleanup}")),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::FRAME_TIME;
    use std::io::Write;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    fn request() -> consent::Request {
        consent::Request::new(
            [42; 32],
            1000,
            consent::Operation::Unlock {
                role: consent::Role::Primary,
            },
        )
        .unwrap()
    }
    fn pair() -> (Wire, Wire) {
        let (one, two) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        (
            Wire::new(one, deadline).unwrap(),
            Wire::new(two, deadline).unwrap(),
        )
    }
    /// A worker's end and the raw peer root writes to.
    fn raw_pair() -> (Wire, UnixStream) {
        let (child, parent) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        (Wire::new(child, deadline).unwrap(), parent)
    }
    fn answer(wire: &mut Wire, tag: u8) {
        let mut prompt = wire.receive().unwrap();
        assert_eq!(prompt[0], tag);
        assert_eq!(&prompt[33..], request().encode());
        prompt[0] += 1;
        wire.send(&prompt).unwrap();
    }

    #[test]
    fn socket_controller_requires_both_exact_acknowledgements() {
        for stop in [0, 1, 2, 3, 4] {
            let (mut child, mut parent) = pair();
            let acquired = Arc::new(AtomicUsize::new(0));
            let committed = Arc::new(AtomicUsize::new(0));
            let a = acquired.clone();
            let c = committed.clone();
            let worker = std::thread::spawn(move || {
                perform(
                    &mut child,
                    &request(),
                    |hash, _| {
                        assert_eq!(hash, challenge(&request()));
                        a.fetch_add(1, Ordering::SeqCst);
                        Ok(42)
                    },
                    |value| {
                        assert_eq!(value, 42);
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
            });
            if stop == 0 {
                drop(parent);
            } else if stop == 1 {
                let mut prompt = parent.receive().unwrap();
                prompt[0] = 0x11;
                prompt[9] ^= 1;
                parent.send(&prompt).unwrap();
                drop(parent);
            } else {
                answer(&mut parent, 0x10);
                if stop == 2 {
                    drop(parent);
                } else if stop == 3 {
                    let mut prompt = parent.receive().unwrap();
                    prompt[0] = 0x13;
                    prompt[9] ^= 1;
                    parent.send(&prompt).unwrap();
                    drop(parent);
                } else {
                    answer(&mut parent, 0x12);
                    assert_eq!(parent.receive().unwrap(), [0x14]);
                }
            }
            assert_eq!(worker.join().unwrap().is_ok(), stop == 4);
            assert_eq!(acquired.load(Ordering::SeqCst), usize::from(stop >= 2));
            assert_eq!(committed.load(Ordering::SeqCst), usize::from(stop == 4));
        }
    }

    #[test]
    fn a_presentation_ack_cannot_be_reused_as_the_commit_ack() {
        let (mut child, mut parent) = pair();
        let committed = Arc::new(AtomicUsize::new(0));
        let output = committed.clone();
        let worker = std::thread::spawn(move || {
            perform(
                &mut child,
                &request(),
                |_, _| Ok(()),
                |()| {
                    output.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
        });
        let mut first = parent.receive().unwrap();
        first[0] = 0x11;
        parent.send(&first).unwrap();
        first[0] = 0x13;
        parent.send(&first).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(committed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn operation_identity_and_challenge_bind_the_complete_request() {
        let original = request();
        assert_eq!(
            challenge(&original),
            [
                0x48, 0xf5, 0xbf, 0x17, 0xed, 0x21, 0xd9, 0xf6, 0x1d, 0xf2, 0x3c, 0xd2, 0x7b, 0x01,
                0x42, 0x3a, 0xbc, 0x79, 0xa0, 0x31, 0x0e, 0x81, 0x2a, 0xb4, 0xc0, 0x17, 0xb2, 0x0a,
                0x89, 0xd5, 0x2e, 0xd0
            ]
        );
        assert_eq!(description(&original.encode(), 1000).unwrap(), original);
        assert!(description(&original.encode(), 1001).is_err());
        for changed in [
            consent::Request::new([43; 32], 1000, original.operation().clone()).unwrap(),
            consent::Request::new([42; 32], 1001, original.operation().clone()).unwrap(),
            consent::Request::new(
                [42; 32],
                1000,
                consent::Operation::Unlock {
                    role: consent::Role::Recovery,
                },
            )
            .unwrap(),
        ] {
            assert_ne!(challenge(&original), challenge(&changed));
        }
        let write = consent::Request::new(
            [42; 32],
            1000,
            consent::Operation::Set {
                role: consent::Role::Primary,
                application: "mail".into(),
                name: "main".into(),
                application_uid: 65537,
                requester: 1000,
            },
        )
        .unwrap();
        assert!(description(&write.encode(), 1000).is_err());
        for elevation in crate::elevation_descriptions(1000) {
            assert!(description(&elevation, 1000).is_err());
        }
    }

    #[test]
    #[ignore = "requires an explicitly marked disposable root VM and production td-secret"]
    fn initial_failures_remove_a_seeded_runtime_key_in_the_root_vm() {
        use std::os::{fd::OwnedFd, unix::fs::PermissionsExt};
        use std::{
            fs,
            process::{Command, Stdio},
        };
        assert!(fs::read_to_string("/proc/cmdline")
            .unwrap()
            .split_whitespace()
            .any(|word| word == "td.operation-fixture=1"));
        store::require_root().unwrap();
        assert!(!std::path::Path::new("/var/lib/td/secrets/1000").exists());
        for path in ["/run/td-secret", "/run/td-secret/1000"] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for command in ["unlock-operation", "enroll-operation"] {
            for case in 0..7 {
                let key = "/run/td-secret/1000/key";
                fs::write(key, [0u8; 64]).unwrap();
                fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
                std::os::unix::fs::fchown(File::open(key).unwrap(), Some(991), Some(991)).unwrap();
                let (mut parent, child) = UnixStream::pair().unwrap();
                let verb = if case == 5 { "lock-session" } else { command };
                let mut process = Command::new("/bin/td-secret")
                    .args([verb, "--uid", "1000"])
                    .stdin(Stdio::from(OwnedFd::from(child)))
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .env_clear()
                    .current_dir("/")
                    .spawn()
                    .unwrap();
                match case {
                    0 | 5 => {}
                    1 => parent.write_all(&[0, 47, 1]).unwrap(),
                    2 => parent.write_all(&[0, 1, 255]).unwrap(),
                    _ => {
                        let mut bytes = if command == "enroll-operation" {
                            consent::Request::new(
                                [42; 32],
                                1000,
                                consent::Operation::Enroll {
                                    platform: consent::Platform::TpmPcr7,
                                    recovery: consent::Recovery::SecondToken,
                                    step: consent::Enrollment::CreatePrimary,
                                },
                            )
                            .unwrap()
                            .encode()
                        } else {
                            request().encode()
                        };
                        if case == 3 {
                            bytes[43] = 233;
                        } else if case == 4 {
                            bytes[44] = 255;
                        }
                        parent
                            .write_all(&(bytes.len() as u16).to_be_bytes())
                            .unwrap();
                        parent.write_all(&bytes).unwrap();
                    }
                }
                drop(parent);
                assert_eq!(process.wait().unwrap().success(), case == 5);
                assert!(
                    !std::path::Path::new(key).exists(),
                    "command {command} case {case} retained a key"
                );
            }
        }
    }

    #[test]
    fn inherited_nonblocking_endpoint_waits_for_a_delayed_frame() {
        let (child, mut parent) = UnixStream::pair().unwrap();
        child.set_nonblocking(true).unwrap();
        let mut wire = Wire::new(child, Instant::now() + Duration::from_secs(3)).unwrap();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            parent.write_all(&[0, 1, 42]).unwrap();
        });
        assert_eq!(wire.receive().unwrap(), [42]);
        sender.join().unwrap();
    }

    /// A login disclosure's acknowledgement waits as a PIN does: past a
    /// frame time, within the operation deadline. Any other waits one frame
    /// time.
    #[test]
    fn a_typed_acknowledgement_waits_for_the_operation_deadline_not_a_frame_time() {
        std::thread::scope(|scope| {
            for typed in [true, false] {
                scope.spawn(move || {
                    let (child, parent) = UnixStream::pair().unwrap();
                    let mut wire = Wire::new(child, Instant::now() + FRAME_TIME * 2).unwrap();
                    let mut root = Wire::new(parent, Instant::now() + FRAME_TIME * 3).unwrap();
                    let answer = std::thread::spawn(move || {
                        let mut prompt = root.receive().unwrap();
                        std::thread::sleep(FRAME_TIME + Duration::from_millis(500));
                        prompt[0] += 1;
                        let _ = root.send(&prompt);
                    });
                    let result = if typed {
                        wire.acknowledge_typed(0x10, 0x11, &request())
                    } else {
                        wire.acknowledge(0x10, 0x11, &request())
                    };
                    assert_eq!(result.is_ok(), typed, "{result:?}");
                    assert_eq!(wire.expired(), !typed);
                    answer.join().unwrap();
                });
            }
        });
        // The operation deadline still bounds it.
        let (child, _parent) = UnixStream::pair().unwrap();
        let mut wire = Wire::new(child, Instant::now() + Duration::from_millis(30)).unwrap();
        assert!(wire.acknowledge_typed(0x10, 0x11, &request()).is_err());
        assert!(wire.expired());
    }

    #[test]
    fn a_pin_waits_for_the_operation_deadline_not_a_frame_time() {
        let (child, mut parent) = UnixStream::pair().unwrap();
        let mut wire = Wire::new(child, Instant::now() + FRAME_TIME * 2).unwrap();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(FRAME_TIME + Duration::from_millis(500));
            parent
                .write_all(&[0, 5, PIN, b'1', b'2', b'3', b'4'])
                .unwrap();
        });
        assert!(wire.receive_pin().is_ok());
        assert!(!wire.expired());
        sender.join().unwrap();
        // The operation deadline still bounds it.
        let (child, _parent) = UnixStream::pair().unwrap();
        let mut wire = Wire::new(child, Instant::now() + Duration::from_millis(30)).unwrap();
        assert_eq!(
            wire.receive_pin().err().unwrap(),
            "private token operation expired"
        );
        assert!(wire.expired());
    }

    #[test]
    fn an_open_session_bounds_every_wait_without_narrowing_the_operation() {
        let (child, mut parent) = UnixStream::pair().unwrap();
        let operation = Instant::now() + FRAME_TIME * 2;
        let mut wire = Wire::new(child, operation).unwrap();
        let session = Instant::now() + Duration::from_millis(50);
        wire.session(Some(session));
        assert_eq!(wire.bound(), session);
        assert_eq!(wire.deadline(), operation);
        assert_eq!(
            wire.receive_pin().err().unwrap(),
            "private token operation expired"
        );
        assert!(wire.expired());
        assert_eq!(
            wire.receive().err().unwrap(),
            "private token operation expired"
        );
        assert!(wire.send(&[0x15]).is_err());
        // Once the session ends, the operation deadline bounds waits again.
        wire.session(None);
        assert_eq!(wire.bound(), operation);
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            parent
                .write_all(&[0, 5, PIN, b'1', b'2', b'3', b'4'])
                .unwrap();
            parent
        });
        assert!(wire.receive_pin().is_ok());
        assert!(wire.send(&[0x14]).is_ok());
        drop(sender.join().unwrap());
        // A session past the operation deadline never extends it.
        wire.session(Some(operation + FRAME_TIME));
        assert_eq!(wire.bound(), operation);
    }

    #[test]
    fn pin_frames_are_bounded_and_never_echo_the_pin() {
        let mut largest = vec![0, 64, PIN];
        largest.extend([b'~'; 63]);
        let mut oversized = vec![0, 65, PIN];
        oversized.extend([b'~'; 64]);
        for (frame, accepted) in [
            (&[0, 5, PIN, b'1', b'2', b'3', b'4'][..], true),
            (&largest, true),
            (&oversized, false),
            (&[0, 1, PIN], false),
            (&[0, 0], false),
            (&[0, 5, 0x11, b'1', b'2', b'3', b'4'], false),
            (&[0, 4, PIN, b'1', b'2', b'3'], false),
            (&[0, 5, PIN, b's', b'e', b'c', 0x7f], false),
            // Truncated: the clearing owner drops a partial PIN.
            (&[0, 5, PIN, b'1', b'2'], false),
        ] {
            let (mut child, mut parent) = raw_pair();
            parent.write_all(frame).unwrap();
            drop(parent);
            match child.receive_pin() {
                Ok(_) => assert!(accepted, "{frame:?}"),
                Err(error) => {
                    assert!(!accepted, "{frame:?}");
                    assert!(!error.contains("sec"), "{error}");
                    assert!(!child.expired());
                }
            }
        }
        // A header beyond the bound refuses before waiting for its payload.
        let (mut child, mut parent) = raw_pair();
        parent.write_all(&[0, 65]).unwrap();
        assert_eq!(
            child.receive_pin().err().unwrap(),
            "invalid operation PIN frame length"
        );
        assert!(!child.expired());
    }
}
