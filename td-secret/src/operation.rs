//! One private root-owned unlock operation, with separate presentation and commit.

use super::{consent, crypto, fido_device, fido_enroll, fido_metadata, store, tpm};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const LIMIT: usize = 289;
const FRAME_TIME: Duration = Duration::from_secs(5);
const OPERATION_TIME: Duration = fido_device::MAX_LIFETIME;

struct Wire {
    stream: UnixStream,
    deadline: Instant,
}

impl Wire {
    fn new(stream: UnixStream, deadline: Instant) -> Result<Self, String> {
        stream
            .set_nonblocking(false)
            .map_err(|_| "set blocking operation endpoint")?;
        Ok(Self { stream, deadline })
    }

    fn frame_remaining(&self, deadline: Instant) -> Result<Duration, String> {
        remaining(self.deadline)?;
        remaining(deadline).map_err(|_| "private operation frame timed out".into())
    }

    fn frame_deadline(&self) -> Result<Instant, String> {
        Instant::now()
            .checked_add(FRAME_TIME)
            .map(|deadline| deadline.min(self.deadline))
            .ok_or_else(|| "operation deadline overflow".into())
    }

    fn read(&mut self, mut bytes: &mut [u8], deadline: Instant) -> Result<(), String> {
        while !bytes.is_empty() {
            self.stream
                .set_read_timeout(Some(self.frame_remaining(deadline)?))
                .map_err(|_| "set operation read timeout")?;
            let count = match self.stream.read(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.frame_remaining(deadline)?;
                    return Err("private operation frame timed out".into());
                }
                result => result.map_err(|_| "read private operation frame")?,
            };
            if count == 0 {
                return Err("operation authority disconnected".into());
            }
            bytes = bytes
                .get_mut(count..)
                .ok_or("invalid operation read count")?;
        }
        self.frame_remaining(deadline).map(|_| ())
    }

    fn receive(&mut self) -> Result<Vec<u8>, String> {
        let deadline = self.frame_deadline()?;
        let mut header = [0; 2];
        self.read(&mut header, deadline)?;
        let length = usize::from(u16::from_be_bytes(header));
        if length == 0 || length > LIMIT {
            return Err("invalid operation frame length".into());
        }
        let mut bytes = vec![0; length];
        self.read(&mut bytes, deadline)?;
        Ok(bytes)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > LIMIT {
            return Err("invalid operation frame length".into());
        }
        let deadline = self.frame_deadline()?;
        let mut frame = Vec::with_capacity(bytes.len() + 2);
        frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(bytes);
        let mut pending = frame.as_slice();
        while !pending.is_empty() {
            self.stream
                .set_write_timeout(Some(self.frame_remaining(deadline)?))
                .map_err(|_| "set operation write timeout")?;
            let count = match self.stream.write(pending) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.frame_remaining(deadline)?;
                    return Err("private operation frame timed out".into());
                }
                result => result.map_err(|_| "write private operation frame")?,
            };
            if count == 0 {
                return Err("operation authority stopped reading".into());
            }
            pending = pending
                .get(count..)
                .ok_or("invalid operation write count")?;
        }
        self.frame_remaining(deadline).map(|_| ())
    }

    fn acknowledge(
        &mut self,
        tag: u8,
        answer: u8,
        request: &consent::Request,
    ) -> Result<(), String> {
        let mut round = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut round))
            .map_err(|_| "read operation round randomness")?;
        let mut message = vec![tag];
        message.extend_from_slice(&round);
        message.extend_from_slice(&request.encode());
        self.send(&message)?;
        let reply = self.receive()?;
        if reply.first() != Some(&answer) || reply.get(1..) != message.get(1..) {
            return Err("operation acknowledgement does not match its request and round".into());
        }
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "private token operation expired".into())
}

fn startup() -> Result<UnixStream, String> {
    store::require_root()?;
    let mut status = String::new();
    File::open("/proc/self/status")
        .and_then(|file| file.take(8193).read_to_string(&mut status))
        .map_err(|_| "read operation process identity")?;
    if status.len() > 8192
        || !status.lines().any(|line| {
            line.strip_prefix("Threads:")
                .is_some_and(|value| value.trim() == "1")
        })
    {
        return Err("token operation requires single-threaded startup".into());
    }
    let input = std::fs::metadata("/proc/self/fd/0").map_err(|_| "missing operation endpoint")?;
    if input.uid() != 0 {
        return Err("operation endpoint must be root owned".into());
    }
    for fd in [1, 2] {
        let output = std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .map_err(|_| "missing operation log descriptor")?;
        if (input.dev(), input.ino()) == (output.dev(), output.ino()) {
            return Err("operation log aliases its private endpoint".into());
        }
    }
    // The read_dir handle observes itself as fd 3; only 0/1/2 are inherited.
    let mut descriptors = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd")
        .map_err(|_| "inspect operation descriptors")?
        .take(5)
    {
        descriptors.push(
            entry
                .map_err(|_| "inspect operation descriptor")?
                .file_name(),
        );
    }
    descriptors.sort();
    if descriptors != ["0", "1", "2", "3"].map(std::ffi::OsString::from) {
        return Err("token operation inherited extra descriptors".into());
    }
    let stream = UnixStream::from(
        std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| "retain private operation endpoint")?,
    );
    if !stream
        .local_addr()
        .map_err(|_| "inspect operation endpoint")?
        .is_unnamed()
        || !stream
            .peer_addr()
            .map_err(|_| "inspect operation peer")?
            .is_unnamed()
    {
        return Err("token operation requires an unnamed socketpair".into());
    }
    Ok(stream)
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
    let result = acquire(challenge(request), wire.deadline)?;
    wire.acknowledge(0x12, 0x13, request)?;
    remaining(wire.deadline)?;
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

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
                application: "mail".into(),
                name: "main".into(),
                application_uid: 65537,
                requester: 1000,
            },
        )
        .unwrap();
        assert!(description(&write.encode(), 1000).is_err());
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
        for case in 0..5 {
            let key = "/run/td-secret/1000/key";
            fs::write(key, [0u8; 64]).unwrap();
            fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
            std::os::unix::fs::fchown(File::open(key).unwrap(), Some(991), Some(991)).unwrap();
            let (mut parent, child) = UnixStream::pair().unwrap();
            let mut process = Command::new("/bin/td-secret")
                .args(["unlock-operation", "--uid", "1000"])
                .stdin(Stdio::from(OwnedFd::from(child)))
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .env_clear()
                .current_dir("/")
                .spawn()
                .unwrap();
            match case {
                0 => {}
                1 => parent.write_all(&[0, 47, 1]).unwrap(),
                2 => parent.write_all(&[0, 1, 255]).unwrap(),
                _ => {
                    let mut bytes = request().encode();
                    if case == 3 {
                        bytes[43] = 233;
                    } else {
                        bytes[44] = 255;
                    }
                    parent
                        .write_all(&(bytes.len() as u16).to_be_bytes())
                        .unwrap();
                    parent.write_all(&bytes).unwrap();
                }
            }
            drop(parent);
            assert!(!process.wait().unwrap().success());
            assert!(
                !std::path::Path::new(key).exists(),
                "initial failure {case} retained a key"
            );
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

    #[test]
    fn frame_timeout_diagnostic_distinguishes_operation_expiration() {
        let (mut child, _parent) = pair();
        assert_eq!(
            child.frame_remaining(Instant::now()).unwrap_err(),
            "private operation frame timed out"
        );
        child.deadline = Instant::now();
        assert_eq!(
            child.frame_remaining(Instant::now()).unwrap_err(),
            "private token operation expired"
        );
    }

    #[test]
    fn socket_timeout_preserves_the_overall_expiration_diagnostic() {
        let (mut child, _parent) = pair();
        child.deadline = Instant::now() + Duration::from_millis(30);
        assert_eq!(
            child.receive().unwrap_err(),
            "private token operation expired"
        );
    }

    #[test]
    fn frame_bounds_and_expiration_refuse_before_processing() {
        let (mut child, mut parent) = pair();
        parent.stream.write_all(&290u16.to_be_bytes()).unwrap();
        assert!(child.receive().is_err());
        let (mut child, mut parent) = pair();
        parent.stream.write_all(&0u16.to_be_bytes()).unwrap();
        assert!(child.receive().is_err());
        let (mut child, _parent) = pair();
        child.deadline = Instant::now();
        assert!(child.receive().is_err());
        assert!(child.send(&[1]).is_err());
        let (mut child, _parent) = pair();
        assert!(child.send(&vec![1; LIMIT + 1]).is_err());
        let (mut child, _parent) = pair();
        assert!(child.send(&[]).is_err());
    }
}
