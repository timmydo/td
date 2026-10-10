//! td-fido's hidraw transport as td-secret runs it: its HID worker is this
//! program's `hid-worker` and `hid-worker-desktop` verbs. The transport's
//! own tests are td-fido's; the guest tests here drive it with td-secret's
//! TPM, stores and workers.

pub use td_fido::fido_device::*;

/// td-secret as its own HID worker. A test harness cannot serve that role,
/// so a test build's Session starts the source-built program a guest image
/// carries (td-secret/DESIGN.md, "Login-key worker guests").
pub struct Worker;
impl Program for Worker {
    const PATH: &'static str = if cfg!(test) {
        "/bin/td-secret"
    } else {
        "/proc/self/exe"
    };
    const ROOT: &'static str = "hid-worker";
    const DESKTOP: &'static str = "hid-worker-desktop";
}

/// A token operation through td-secret's worker.
pub type Session = td_fido::fido_device::Session<Worker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_test_build_starts_the_guest_program_as_its_worker() {
        let source = include_str!("fido_device.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(source.contains(
            "    const PATH: &'static str = if cfg!(test) {\n        \"/bin/td-secret\"\n    } else {\n        \"/proc/self/exe\"\n    };"
        ));
        assert_eq!(Worker::PATH, "/bin/td-secret");
        // lib.rs routes exactly these verbs to td-fido's two workers.
        let lib = include_str!("lib.rs").split("#[cfg(test)]").next().unwrap();
        assert!(lib.contains(
            "[command, index, inode, rdev] if command == \"hid-worker\" => {\n            fido_device::worker(index, inode, rdev)"
        ));
        assert!(lib.contains(
            "[command, index, inode, rdev, runtime] if command == \"hid-worker-desktop\" => {\n            fido_device::desktop_worker(index, inode, rdev, runtime)"
        ));
        assert_eq!(Worker::ROOT, "hid-worker");
        assert_eq!(Worker::DESKTOP, "hid-worker-desktop");
    }
}

#[cfg(test)]
pub(crate) mod vm_tests {
    use super::*;
    use crate::fido_uhid::{guard, Uhid, FIDO_DESCRIPTOR, FIDO_PRODUCT};
    use crate::{fido_hid as hid, store};
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const CHANNEL: u32 = 0x10203040;

    struct Token {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<(usize, usize)>>,
    }

    impl Token {
        fn start(expected: Vec<Vec<u8>>, response: Vec<u8>, keepalive: bool) -> Self {
            Self::serve(Duration::from_secs(15), keepalive, move |request, index| {
                assert_eq!(request, expected.get(index).unwrap());
                response.clone()
            })
        }

        fn serve(
            lifetime: Duration,
            keepalive: bool,
            mut reply: impl FnMut(&[u8], usize) -> Vec<u8> + Send + 'static,
        ) -> Self {
            let mut device = Uhid::create("td FIDO fixture", FIDO_PRODUCT, FIDO_DESCRIPTOR);
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                let deadline = Instant::now() + lifetime;
                let mut decoder = hid::Decoder::cbor(CHANNEL).unwrap();
                let mut waiting = false;
                let mut next_keepalive = Instant::now();
                let mut requests = 0;
                let mut keepalives = 0;
                while !stopped.load(Ordering::Relaxed) {
                    assert!(Instant::now() < deadline, "virtual HID fixture expired");
                    if let Some(report) = device.output() {
                        if report[..7] == [255, 255, 255, 255, 0x86, 0, 8] {
                            let mut reply = [0; 64];
                            reply[..7].copy_from_slice(&[255, 255, 255, 255, 0x86, 0, 17]);
                            reply[7..15].copy_from_slice(&report[7..15]);
                            reply[15..19].copy_from_slice(&CHANNEL.to_be_bytes());
                            reply[19..24].copy_from_slice(&[2, 1, 0, 0, 4]);
                            device.input(&reply);
                        } else if let hid::Event::Complete(request) = decoder.push(&report).unwrap()
                        {
                            let response = reply(request.as_ref(), requests);
                            requests += 1;
                            if keepalive {
                                waiting = true;
                            } else {
                                for report in hid::cbor(CHANNEL, &response).unwrap().as_ref() {
                                    device.input(report);
                                }
                            }
                            decoder = hid::Decoder::cbor(CHANNEL).unwrap();
                        }
                    }
                    if waiting && Instant::now() >= next_keepalive {
                        let mut report = [0; 64];
                        report[..4].copy_from_slice(&CHANNEL.to_be_bytes());
                        report[4..8].copy_from_slice(&[0xbb, 0, 1, 2]);
                        device.input(&report);
                        keepalives += 1;
                        next_keepalive = Instant::now() + Duration::from_millis(20);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                (requests, keepalives)
            });
            Self {
                stop,
                worker: Some(worker),
            }
        }

        fn finish(mut self) -> (usize, usize) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Token {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn discover_one() -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let devices = Device::discover().unwrap();
            if devices.len() == 1 {
                return devices[0];
            }
            assert!(
                Instant::now() < deadline,
                "virtual token was not discovered"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    // The production open: `/bin/td-secret hid-worker` in a test build.
    fn session(device: Device, lifetime: Duration) -> Session {
        Session::open(device, Instant::now() + lifetime).unwrap()
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn assertion_fixture() -> (Vec<u8>, crate::fido_ctap::Es256PublicKey, [u8; 32]) {
        // The independent OpenSSL fixture also used by the host TPM oracle.
        let mut cose = vec![0xa5, 1, 2, 3, 0x26, 0x20, 1, 0x21, 0x58, 0x20];
        cose.extend(hex(
            "ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a",
        ));
        cose.extend([0x22, 0x58, 0x20]);
        cose.extend(hex(
            "552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553",
        ));
        let key = crate::fido_ctap::Es256PublicKey::from_cose(&cose).unwrap();
        let challenge = hex("f3ad24f2731ea324507944e3ae1b9a172f14eaac6a57e004788390dc14a4c7ca")
            .try_into()
            .unwrap();
        let auth = hex(
            "34e2ef54cd9003d2930734cfb0402ccab6a44dcb5024fc367878c413c78ce2dd8100000007a16b6372656450726f7465637401",
        );
        let signature = hex(
            "3045022100fcd359f2e59ed2e63367ec882724beae6d78fd876d9208b9ec0900b4114aa98c02205c58e5c6d917e85e879fed77b43f0b73cf4394eb1caadb657855e362e541b4f7",
        );
        let mut response = crate::fido_cbor::Encoder::new();
        response.head(5, 2).unwrap();
        response.head(0, 2).unwrap();
        response.bytes(&auth).unwrap();
        response.head(0, 3).unwrap();
        response.bytes(&signature).unwrap();
        let mut bytes = vec![0];
        bytes.extend(response.finish().unwrap());
        (bytes, key, challenge)
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID and TPM devices"]
    fn qemu_hid_assertion_uses_production_worker_and_guest_tpm() {
        guard("fido-hid");
        assert!(Device::discover().unwrap().is_empty());
        let (response, key, challenge) = assertion_fixture();
        let request = crate::fido_ctap::AssertionRequest::new(&[7], challenge, 1024).unwrap();
        let mut changed = challenge;
        changed[0] ^= 1;
        let changed = crate::fido_ctap::AssertionRequest::new(&[7], changed, 1024).unwrap();
        let token = Token::start(
            vec![request.bytes().to_vec(), changed.bytes().to_vec()],
            response.clone(),
            false,
        );
        let mut session = session(discover_one(), Duration::from_secs(10));
        let worker = session.worker_id().unwrap();
        let mut tpm = crate::tpm::Client::new(crate::tpm::Device::open().unwrap());
        let received = session.cbor(request.bytes()).unwrap();
        assert_eq!(received.as_ref(), response);
        assert_eq!(
            request
                .verify(received.as_ref(), &key, &mut tpm)
                .unwrap()
                .counter,
            7
        );
        let replay = session.cbor(changed.bytes()).unwrap();
        assert_eq!(replay.as_ref(), response);
        let error = changed
            .verify(replay.as_ref(), &key, &mut tpm)
            .err()
            .unwrap();
        assert!(error.starts_with("TPM command 0x177 refused:"), "{error}");
        drop(session);
        assert!(!std::path::Path::new(&format!("/proc/{worker}")).exists());
        assert_eq!(token.finish(), (2, 0));
        assert!(Device::discover().unwrap().is_empty());
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_hid_keepalives_cannot_extend_the_worker_deadline() {
        guard("fido-deadline");
        assert!(Device::discover().unwrap().is_empty());
        let (response, _, challenge) = assertion_fixture();
        let request = crate::fido_ctap::AssertionRequest::new(&[7], challenge, 1024).unwrap();
        let token = Token::start(vec![request.bytes().to_vec()], response, true);
        let mut session = session(discover_one(), Duration::from_secs(5));
        let worker = session.worker_id().unwrap();
        let started = Instant::now();
        let error = session.cbor(request.bytes()).err().unwrap();
        assert!(error.contains("expired"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(7));
        assert!(session.worker_id().is_none());
        assert!(!std::path::Path::new(&format!("/proc/{worker}")).exists());
        let (requests, keepalives) = token.finish();
        assert_eq!(requests, 1);
        assert!(keepalives >= 5);
        assert!(Device::discover().unwrap().is_empty());
    }
    fn fresh() -> [u8; 32] {
        let mut bytes = [0; 32];
        File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut bytes)
            .unwrap();
        bytes
    }

    fn info_reply() -> Vec<u8> {
        let mut bytes = b"\0\xa2\x01\x81\x68FIDO_2_0\x03\x50".to_vec();
        bytes.extend_from_slice(&[0; 16]);
        bytes
    }

    struct VirtualCredential {
        id: Vec<u8>,
        signer: Arc<std::sync::Mutex<crate::tpm::tests::SigningKey>>,
    }

    impl VirtualCredential {
        fn new(id: u8) -> Self {
            Self {
                id: vec![id; 32],
                signer: Arc::new(std::sync::Mutex::new(crate::tpm::tests::SigningKey::new())),
            }
        }

        fn start(&self, expected: Vec<Vec<u8>>) -> Token {
            self.checked(move |request, index| {
                assert_eq!(request, expected.get(index).unwrap());
            })
        }

        fn checked(&self, mut check: impl FnMut(&[u8], usize) + Send + 'static) -> Token {
            use crate::fido_cbor::{self as cbor, Encoder, Value};
            let signer = Arc::clone(&self.signer);
            let id = self.id.clone();
            let mut counter = 0u32;
            Token::serve(Duration::from_secs(60), false, move |request, index| {
                check(request, index);
                if request == [4] {
                    return info_reply();
                }
                let value = cbor::decode(&request[1..]).unwrap();
                let mut auth = crate::crypto::digest(crate::fido_ctap::RP_ID.as_bytes()).to_vec();
                let mut out = Encoder::new();
                match request[0] {
                    1 => {
                        assert_eq!(
                            value
                                .required(&Value::Unsigned(2))
                                .unwrap()
                                .required(&Value::Text("id"))
                                .unwrap()
                                .text()
                                .unwrap(),
                            crate::fido_ctap::RP_ID
                        );
                        if let Some(Value::Array(excluded)) =
                            value.get(&Value::Unsigned(5)).unwrap()
                        {
                            if excluded.iter().any(|credential| {
                                credential
                                    .required(&Value::Text("id"))
                                    .unwrap()
                                    .bytes()
                                    .unwrap()
                                    == id
                            }) {
                                return vec![0x19]; // CTAP2_ERR_CREDENTIAL_EXCLUDED
                            }
                        }
                        auth.push(0x41);
                        auth.extend_from_slice(&0u32.to_be_bytes());
                        auth.extend_from_slice(&[0; 16]);
                        auth.extend_from_slice(&(id.len() as u16).to_be_bytes());
                        auth.extend_from_slice(&id);
                        auth.extend_from_slice(&signer.lock().unwrap().cose);
                        out.head(5, 3).unwrap();
                        out.head(0, 1).unwrap();
                        out.text("none").unwrap();
                        out.head(0, 2).unwrap();
                        out.bytes(&auth).unwrap();
                        out.head(0, 3).unwrap();
                        out.head(5, 0).unwrap();
                    }
                    2 => {
                        let challenge: [u8; 32] = value
                            .required(&Value::Unsigned(2))
                            .unwrap()
                            .bytes()
                            .unwrap()
                            .try_into()
                            .unwrap();
                        let canonical =
                            crate::fido_ctap::AssertionRequest::new(&id, challenge, 1024).unwrap();
                        assert_eq!(request, canonical.bytes());
                        counter += 1;
                        auth.push(1);
                        auth.extend_from_slice(&counter.to_be_bytes());
                        let mut signed = auth.clone();
                        signed.extend_from_slice(&challenge);
                        let signature =
                            signer.lock().unwrap().sign(&crate::crypto::digest(&signed));
                        out.head(5, 3).unwrap();
                        out.head(0, 1).unwrap();
                        out.head(5, 2).unwrap();
                        out.text("id").unwrap();
                        out.bytes(&id).unwrap();
                        out.text("type").unwrap();
                        out.text("public-key").unwrap();
                        out.head(0, 2).unwrap();
                        out.bytes(&auth).unwrap();
                        out.head(0, 3).unwrap();
                        out.bytes(&signature).unwrap();
                    }
                    other => panic!("unexpected virtual CTAP command {other}"),
                }
                let mut response = vec![0];
                response.extend_from_slice(&out.finish().unwrap());
                response
            })
        }

        fn enroll(
            &self,
            primary: Option<&crate::fido_enroll::Credential>,
        ) -> crate::fido_enroll::Credential {
            use crate::fido_enroll::{Info, MakeCredential, GET_INFO};
            let creation = fresh();
            let proof_hash = fresh();
            assert_ne!(creation, proof_hash);
            let make = match primary {
                Some(primary) => MakeCredential::recovery(
                    Info::parse(&info_reply()).unwrap(),
                    creation,
                    fresh(),
                    primary,
                ),
                None => {
                    MakeCredential::primary(Info::parse(&info_reply()).unwrap(), creation, fresh())
                }
            }
            .unwrap();
            let assertion =
                crate::fido_ctap::AssertionRequest::new(&self.id, proof_hash, 1024).unwrap();
            let token = self.start(vec![
                GET_INFO.to_vec(),
                make.bytes().to_vec(),
                assertion.bytes().to_vec(),
            ]);
            let mut session = session(discover_one(), Duration::from_secs(15));
            let info = session.cbor(GET_INFO).unwrap();
            assert_eq!(info.as_ref(), info_reply());
            assert_eq!(Info::parse(info.as_ref()).unwrap().max_message(), 1024);
            let response = session.cbor(make.bytes()).unwrap();
            let proof = make.proof(response.as_ref(), proof_hash).unwrap();
            let response = session.cbor(proof.bytes()).unwrap();
            let credential = proof
                .verify(
                    response.as_ref(),
                    &mut crate::tpm::Client::new(crate::tpm::Device::open().unwrap()),
                )
                .unwrap();
            assert_eq!(credential.id(), self.id);
            assert_eq!(credential.cose(), self.signer.lock().unwrap().cose);
            drop(session);
            assert_eq!(token.finish(), (3, 0));
            assert!(Device::discover().unwrap().is_empty());
            credential
        }

        fn exchange(&self, request: &[u8]) -> Vec<u8> {
            let token = self.start(vec![request.to_vec()]);
            let mut session = session(discover_one(), Duration::from_secs(15));
            let response = session.cbor(request).unwrap().as_ref().to_vec();
            drop(session);
            assert_eq!(token.finish(), (1, 0));
            assert!(Device::discover().unwrap().is_empty());
            response
        }
    }

    fn enrollment_and_release(second: bool) {
        use crate::fido_enroll::{Info, MakeCredential};
        use crate::fido_metadata::{Metadata, Recovery, Role};
        use crate::tpm::{Client, Device as Tpm, Pcrs};
        assert!(Device::discover().unwrap().is_empty());
        crate::tpm::tests::qemu_extend(&[9; 32]);
        let primary_token = VirtualCredential::new(42);
        let primary = primary_token.enroll(None);
        // Replugging the same virtual token must not bypass recovery exclusion.
        let excluded = MakeCredential::recovery(
            Info::parse(&info_reply()).unwrap(),
            fresh(),
            fresh(),
            &primary,
        )
        .unwrap();
        let response = primary_token.exchange(excluded.bytes());
        assert_eq!(response, [0x19]);
        assert_eq!(
            excluded.proof(&response, fresh()).err().unwrap(),
            "CTAP enrollment refused: 0x19"
        );
        let recovery_token = second.then(|| VirtualCredential::new(43));
        let recovery = recovery_token
            .as_ref()
            .map(|token| token.enroll(Some(&primary)));
        let metadata = Metadata::new(
            1000,
            &primary,
            match &recovery {
                Some(recovery) => Recovery::SecondToken(recovery),
                None => Recovery::Unrecoverable,
            },
        )
        .unwrap();
        let encoded = metadata.encode().unwrap();
        let metadata = Metadata::decode(&encoded, 1000).unwrap();
        assert_eq!(metadata.has_recovery(), second);
        let master = fresh();
        let sealed = Client::new(Tpm::open().unwrap())
            .seal_bound(
                1000,
                Pcrs::parse("7").unwrap(),
                &master,
                &metadata.binding().unwrap(),
            )
            .unwrap();
        let sealed = crate::tpm::BoundKey::decode(&sealed.encode().unwrap()).unwrap();
        let info = Info::parse(&info_reply()).unwrap();
        for (role, token) in [
            (Role::Primary, Some(&primary_token)),
            (Role::Recovery, recovery_token.as_ref()),
        ] {
            let Some(token) = token else {
                assert_eq!(
                    metadata.request(role, fresh(), &info).err().unwrap(),
                    "store is explicitly unrecoverable"
                );
                continue;
            };
            let challenge = fresh();
            let request = metadata.request(role, challenge, &info).unwrap();
            let response = token.exchange(request.bytes());
            assert_eq!(
                request
                    .unseal(&response, &sealed, Client::new(Tpm::open().unwrap()))
                    .unwrap(),
                master
            );
            let changed = fresh();
            assert_ne!(challenge, changed);
            let replay = metadata.request(role, changed, &info).unwrap();
            let error = replay
                .unseal(&response, &sealed, Client::new(Tpm::open().unwrap()))
                .err()
                .unwrap();
            assert!(error.starts_with("TPM command 0x177 refused:"), "{error}");
        }
        if let Some(token) = recovery_token {
            let impostor = VirtualCredential {
                id: primary_token.id.clone(),
                signer: token.signer,
            };
            let request = metadata.request(Role::Primary, fresh(), &info).unwrap();
            let response = impostor.exchange(request.bytes());
            let error = request
                .unseal(&response, &sealed, Client::new(Tpm::open().unwrap()))
                .err()
                .unwrap();
            assert!(error.starts_with("TPM command 0x177 refused:"), "{error}");
        }
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_hid_enrolls_unrecoverable_and_unseals_with_a_fresh_assertion() {
        guard("fido-enroll-single");
        enrollment_and_release(false);
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_hid_enrolls_recovery_and_refuses_replays_and_wrong_keys() {
        guard("fido-enroll-recovery");
        enrollment_and_release(true);
    }
    type Script = Arc<std::sync::Mutex<std::collections::VecDeque<ExpectedCtap>>>;
    enum ExpectedCtap {
        Info,
        Create {
            hash: [u8; 32],
            excluded: Option<Vec<u8>>,
            user: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
        },
        Assert {
            hash: [u8; 32],
            id: Vec<u8>,
        },
    }

    fn scripted(token: &VirtualCredential) -> (Token, Script) {
        use crate::fido_cbor::{self as cbor, Value};
        use crate::fido_enroll::{Info, MakeCredential};
        let script: Script = Arc::default();
        let input = Arc::clone(&script);
        let token = token.checked(move |request, _| {
            let expected = input
                .lock()
                .unwrap()
                .pop_front()
                .expect("token I/O before its presentation acknowledgement");
            match expected {
                ExpectedCtap::Info => assert_eq!(request, [4]),
                ExpectedCtap::Assert { hash, id } => {
                    let expected =
                        crate::fido_ctap::AssertionRequest::new(&id, hash, 1024).unwrap();
                    assert_eq!(request, expected.bytes());
                }
                ExpectedCtap::Create {
                    hash,
                    excluded,
                    user,
                } => {
                    assert_eq!(request[0], 1);
                    let actual = cbor::decode(&request[1..]).unwrap();
                    let handle: [u8; 32] = actual
                        .required(&Value::Unsigned(3))
                        .unwrap()
                        .required(&Value::Text("id"))
                        .unwrap()
                        .bytes()
                        .unwrap()
                        .try_into()
                        .unwrap();
                    let mut user = user.lock().unwrap();
                    if let Some(previous) = *user {
                        assert_eq!(previous, handle);
                    }
                    *user = Some(handle);
                    let base =
                        MakeCredential::primary(Info::parse(&info_reply()).unwrap(), hash, handle)
                            .unwrap();
                    let mut expected = cbor::decode(&base.bytes()[1..]).unwrap();
                    if let Some(id) = &excluded {
                        let Value::Map(entries) = &mut expected else {
                            panic!("make map")
                        };
                        entries.insert(
                            4,
                            (
                                Value::Unsigned(5),
                                Value::Array(vec![Value::Map(vec![
                                    (Value::Text("id"), Value::Bytes(id)),
                                    (Value::Text("type"), Value::Text("public-key")),
                                ])]),
                            ),
                        );
                    }
                    assert!(
                        actual == expected,
                        "makeCredential did not bind the presented step and exclusion"
                    );
                }
            }
        });
        (token, script)
    }

    struct OperationChild {
        child: std::process::Child,
        log: std::path::PathBuf,
    }
    impl OperationChild {
        fn start(command: &str) -> (Self, crate::operation::Wire) {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let log = std::path::PathBuf::from(format!(
                "/run/private-operation-{}.log",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let errors = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&log)
                .unwrap();
            let (parent, child) = UnixStream::pair().unwrap();
            let child = Command::new("/bin/td-secret")
                .args([command, "--uid", "1000"])
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::from(OwnedFd::from(child)))
                .stdout(Stdio::null())
                .stderr(errors)
                .spawn()
                .unwrap();
            let wire =
                crate::operation::Wire::new(parent, Instant::now() + Duration::from_secs(120))
                    .unwrap();
            (Self { child, log }, wire)
        }
        fn finish(mut self, error: Option<&str>) {
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline, "private worker failed to exit");
                thread::sleep(Duration::from_millis(10));
            };
            let mut bytes = Vec::new();
            File::open(&self.log)
                .unwrap()
                .take(65_537)
                .read_to_end(&mut bytes)
                .unwrap();
            assert!(
                bytes.len() <= 65_536,
                "private worker stderr exceeded 64 KiB"
            );
            let log = String::from_utf8(bytes).expect("private worker stderr was not UTF-8");
            assert_eq!(status.success(), error.is_none(), "{log}");
            if let Some(error) = error {
                assert!(log.contains(error), "{log}");
            } else {
                assert!(log.is_empty(), "{log}");
            }
            assert!(!std::path::Path::new(&format!("/proc/{}", self.child.id())).exists());
        }
    }
    impl Drop for OperationChild {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn invitation(
        wire: &mut crate::operation::Wire,
        tag: u8,
        request: &crate::consent::Request,
    ) -> Vec<u8> {
        let mut frame = wire.receive().unwrap();
        let encoded = request.encode();
        assert_eq!(
            frame.len(),
            33 + encoded.len(),
            "unexpected private invitation length"
        );
        assert_eq!(frame[0], tag);
        assert_eq!(&frame[33..], encoded);
        frame[0] += 1;
        frame
    }
    fn presented_hash(domain: &[u8], request: &crate::consent::Request) -> [u8; 32] {
        let mut bytes = domain.to_vec();
        bytes.extend_from_slice(&request.encode());
        crate::crypto::digest(&bytes)
    }
    fn no_release() {
        assert!(!std::path::Path::new("/run/td-secret/1000/key").exists());
    }
    fn sealed_bytes() -> Vec<u8> {
        fs::read("/var/lib/td/secrets/1000/sealed").unwrap()
    }

    fn prepare_operation_accounts() {
        use std::os::unix::fs::PermissionsExt;
        assert!(!std::path::Path::new("/etc/td-principals.tsv").exists());
        fs::create_dir_all("/etc").unwrap();
        for (name, text, mode) in [
            ("td-principals.tsv", "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\tmail\t65537\n", 0o444),
            ("passwd", "tester:x:1000:1000::/home/tester:/bin/false\ntda65537:x:65537:65537::/var/lib/td/applications/65537:/bin/false\n", 0o644),
            ("group", "tester:x:1000:\ntda65537:x:65537:\n", 0o644),
            ("shadow", "tester::0:0:99999:7:::\ntda65537:!td-service:0:0:99999:7:::\n", 0o600),
        ] {
            let path = std::path::Path::new("/etc").join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    fn prepare_operation_store() {
        prepare_operation_accounts();
        assert!(!store::user_path(1000).exists());
        fs::create_dir_all("/var/lib/td/secrets").unwrap();
        let store = store::Store::open_owned(&store::user_path(1000), 1000, 991, true).unwrap();
        store.set("mail", "main", b"firstboot fixture").unwrap();
        store.set("news", "main", b"untouched fixture").unwrap();
        assert!(store.application_secret("mail", "main").is_err());
    }

    fn enroll_worker(primary: &VirtualCredential, recovery: Option<&VirtualCredential>) {
        use crate::consent::{Enrollment, Operation, Platform, Recovery, Request};
        let mut request = Request::new(
            fresh(),
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: if recovery.is_some() {
                    Recovery::SecondToken
                } else {
                    Recovery::Unrecoverable
                },
                step: Enrollment::CreatePrimary,
            },
        )
        .unwrap();
        let old_master = fs::read(store::user_path(1000).join("master")).unwrap();
        let user = Arc::new(std::sync::Mutex::new(None));
        let (primary_hid, primary_script) = scripted(primary);
        let mut recovery_hid = None;
        let mut recovery_script: Option<Script> = None;
        let (child, mut wire) = OperationChild::start("enroll-operation");
        wire.send(&request.encode()).unwrap();
        loop {
            let reply = invitation(&mut wire, 0x10, &request);
            assert!(primary_script.lock().unwrap().is_empty());
            let Operation::Enroll { step, .. } = request.operation() else {
                panic!("enrollment")
            };
            if *step == Enrollment::CreateRecovery {
                let (hid, script) = scripted(recovery.unwrap());
                recovery_hid = Some(hid);
                recovery_script = Some(script);
            }
            let (script, token, excluded) = match step {
                Enrollment::CreatePrimary | Enrollment::ProvePrimary => {
                    (&primary_script, primary, None)
                }
                _ => (
                    recovery_script.as_ref().unwrap(),
                    recovery.unwrap(),
                    Some(primary.id.clone()),
                ),
            };
            assert!(script.lock().unwrap().is_empty());
            let hash = presented_hash(b"td-secret/presented-enrollment/v1\0", &request);
            let mut pending = script.lock().unwrap();
            match step {
                Enrollment::CreatePrimary | Enrollment::CreateRecovery => {
                    pending.push_back(ExpectedCtap::Info);
                    pending.push_back(ExpectedCtap::Create {
                        hash,
                        excluded,
                        user: Arc::clone(&user),
                    });
                }
                _ => pending.push_back(ExpectedCtap::Assert {
                    hash,
                    id: token.id.clone(),
                }),
            }
            drop(pending);
            no_release();
            assert_eq!(
                fs::read(store::user_path(1000).join("master")).unwrap(),
                old_master
            );
            wire.send(&reply).unwrap();
            let Some(next) = request.following_enrollment_step().unwrap() else {
                break;
            };
            request = next;
        }
        let commit = invitation(&mut wire, 0x12, &request);
        assert!(primary_script.lock().unwrap().is_empty());
        if let Some(script) = &recovery_script {
            assert!(script.lock().unwrap().is_empty());
        }
        assert!(!store::user_path(1000).join("sealed").exists());
        no_release();
        wire.send(&commit).unwrap();
        assert_eq!(wire.receive().unwrap(), [0x14]);
        drop(wire);
        child.finish(None);
        assert_eq!(primary_hid.finish(), (3, 0));
        if let Some(token) = recovery_hid {
            assert_eq!(token.finish(), (3, 0));
        }
        assert!(Device::discover().unwrap().is_empty());
        no_release();
        for retired in ["master", "mail.main", "news.main"] {
            assert!(!store::user_path(1000).join(retired).exists());
        }
        let store = crate::owned_store(1000).unwrap();
        assert!(store.token_protected().unwrap());
        assert!(store.application_secret("mail", "main").is_err());
    }

    fn operate(
        token: &VirtualCredential,
        role: crate::consent::Role,
        value: Option<&[u8]>,
        cancel: bool,
        absent_role: bool,
    ) {
        use crate::consent::{Operation, Request};
        let operation = if value.is_some() {
            Operation::Set {
                role,
                application: "mail".into(),
                name: "main".into(),
                application_uid: 65537,
                requester: 1000,
            }
        } else {
            Operation::Unlock { role }
        };
        let request = Request::new(fresh(), 1000, operation).unwrap();
        let domain = if value.is_some() {
            b"td-secret/presented-write/v1\0".as_slice()
        } else {
            b"td-secret/presented-unlock/v1\0".as_slice()
        };
        let before = sealed_bytes();
        no_release();
        let (hid, script) = scripted(token);
        let (child, mut wire) = OperationChild::start(if value.is_some() {
            "write-operation"
        } else {
            "unlock-operation"
        });
        wire.send(&request.encode()).unwrap();
        if let Some(value) = value {
            wire.send(value).unwrap();
        }
        let reply = invitation(&mut wire, 0x10, &request);
        script.lock().unwrap().push_back(ExpectedCtap::Info);
        if !absent_role {
            script.lock().unwrap().push_back(ExpectedCtap::Assert {
                hash: presented_hash(domain, &request),
                id: token.id.clone(),
            });
        }
        wire.send(&reply).unwrap();
        if absent_role {
            assert!(wire.receive().is_err());
            drop(wire);
            child.finish(Some("store is explicitly unrecoverable"));
        } else {
            let commit = invitation(&mut wire, 0x12, &request);
            assert!(script.lock().unwrap().is_empty());
            assert_eq!(sealed_bytes(), before);
            no_release();
            if !cancel {
                wire.send(&commit).unwrap();
                assert_eq!(wire.receive().unwrap(), [0x14]);
            }
            drop(wire);
            child.finish(cancel.then_some("operation authority disconnected"));
        }
        assert!(script.lock().unwrap().is_empty());
        assert_eq!(hid.finish(), (if absent_role { 1 } else { 2 }, 0));
        assert!(Device::discover().unwrap().is_empty());
        if cancel || absent_role || value.is_none() {
            assert_eq!(sealed_bytes(), before);
        } else {
            assert_ne!(
                sealed_bytes(),
                before,
                "successful write left the bundle unchanged"
            );
        }
        if cancel || absent_role || value.is_some() {
            no_release();
        } else {
            assert!(std::path::Path::new("/run/td-secret/1000/key").exists());
        }
    }

    fn read_records(expected: &[u8]) {
        let store = crate::owned_store(1000).unwrap();
        assert_eq!(
            store.application_secret("mail", "main").unwrap().unwrap(),
            expected
        );
        assert_eq!(
            store.application_secret("news", "main").unwrap().unwrap(),
            b"untouched fixture"
        );
    }

    fn private_operations(second: bool) {
        use crate::consent::Role;
        prepare_operation_store();
        crate::tpm::tests::qemu_extend(&[9; 32]);
        let primary = VirtualCredential::new(42);
        let recovery = second.then(|| VirtualCredential::new(43));
        enroll_worker(&primary, recovery.as_ref());
        operate(&primary, Role::Primary, None, true, false);
        operate(&primary, Role::Primary, None, false, false);
        read_records(b"firstboot fixture");
        store::lock_session(1000).unwrap();
        operate(
            &primary,
            Role::Primary,
            Some(b"cancelled fixture"),
            true,
            false,
        );
        operate(
            &primary,
            Role::Primary,
            Some(b"changed fixture"),
            false,
            false,
        );
        operate(&primary, Role::Primary, None, false, false);
        read_records(b"changed fixture");
        store::lock_session(1000).unwrap();
        if let Some(recovery) = recovery {
            operate(
                &recovery,
                Role::Recovery,
                Some(b"recovered fixture"),
                false,
                false,
            );
            operate(&recovery, Role::Recovery, None, false, false);
            read_records(b"recovered fixture");
            store::lock_session(1000).unwrap();
        } else {
            operate(&primary, Role::Recovery, None, false, true);
        }
        no_release();
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_private_workers_enroll_unlock_write_and_cancel_without_recovery() {
        guard("fido-operations-single");
        private_operations(false);
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_private_workers_enroll_unlock_and_write_with_recovery() {
        guard("fido-operations-recovery");
        private_operations(true);
    }

    pub(crate) mod desktop {
        use super::*;
        mod system {
            include!("system_vm.rs");
        }
        use std::os::unix::fs::{chown, PermissionsExt};
        use std::path::Path;
        use std::sync::atomic::AtomicUsize;

        pub(crate) struct Diagnostics;
        impl Drop for Diagnostics {
            fn drop(&mut self) {
                if !thread::panicking() {
                    return;
                }
                for log in [
                    "/run/desktop-authd.log",
                    "/run/desktop-compositor.log",
                    "/run/desktop-set.log",
                    "/run/desktop-busd.log",
                    "/run/desktop-portal.log",
                    "/run/desktop-mail.log",
                    "/run/desktop-news.log",
                    "/run/desktop-demo.log",
                ] {
                    if let Ok(file) = File::open(log) {
                        let mut bytes = Vec::new();
                        if file.take(65_536).read_to_end(&mut bytes).is_ok() {
                            eprintln!("{log}: {}", String::from_utf8_lossy(&bytes));
                        }
                    }
                }
            }
        }

        pub(crate) fn wait(label: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !done() {
                assert!(
                    Instant::now() < deadline,
                    "desktop fixture timed out: {label}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn read_released(expected: &[u8]) {
            // Key publication precedes the worker dropping its store lock.
            wait("released store admission", || {
                let Ok(store) = crate::owned_store(1000) else {
                    return false;
                };
                assert_eq!(
                    store.application_secret("mail", "main").unwrap().unwrap(),
                    expected
                );
                assert_eq!(
                    store.application_secret("news", "main").unwrap().unwrap(),
                    b"untouched fixture"
                );
                true
            });
        }

        const APP_TEST: &str = "fido_device::vm_tests::desktop::qemu_application_portal_client";
        const RUNTIME: &str = "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1";

        fn directory(path: impl AsRef<Path>, owner: u32, mode: u32) {
            let path = path.as_ref();
            fs::create_dir_all(path).unwrap();
            chown(path, Some(owner), Some(owner))
                .unwrap_or_else(|error| panic!("chown {}: {error}", path.display()));
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }

        fn application_home(uid: u32, app: &str) -> std::path::PathBuf {
            Path::new("/var/lib/td/applications")
                .join(uid.to_string())
                .join(".td/app")
                .join(app)
                .join("home")
        }

        fn setup_portal(reopen: bool) {
            for (path, owner, mode) in [
                ("/run/td-bus", 0, 0o755),
                ("/run/td-bus/1000", 992, 0o755),
                ("/run/td-portal", 0, 0o755),
                // This portal runtime path and the passwd fixture below are
                // split at the crate name with `concat!` so td-secret's source
                // joins no `td-portal` token to a slash: affected.rs's textual
                // reader matcher would otherwise read these runtime paths as a
                // source dependency on the bin-only td-portal crate (nothing
                // mounts or links it) and drag the whole compositor reader
                // closure into every td-ui edit. The runtime values are
                // unchanged.
                (concat!("/run/td-portal", "/1000"), 991, 0o700),
                ("/var/lib/td/applications", 0, 0o755),
                ("/var/home", 0, 0o755),
                ("/var/home/tester", 1000, 0o700),
                ("/etc/ssl/certs", 0, 0o755),
            ] {
                directory(path, owner, mode);
            }
            directory(format!("{RUNTIME}/files"), 0, 0o755);
            // Only the PEM envelope is admitted; this offline guest performs no TLS.
            fs::write(
                format!("{RUNTIME}/ca.pem"),
                b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
            )
            .unwrap();
            std::os::unix::fs::symlink(
                format!("{RUNTIME}/ca.pem"),
                "/etc/ssl/certs/ca-certificates.crt",
            )
            .unwrap();
            fs::write("/etc/td-app.conf", "format=1\npackage-root=/td/store\nstate-root=.td/app\nregistry=/etc/td-applications.tsv\nlauncher-table=/etc/td-launcher.tsv\ncgroup-root=/sys/fs/cgroup/td-user-1000\n").unwrap();
            fs::write("/etc/td-portal-settings", "format=1\ncolor-scheme=1\naccent-color=0.125,0.375,0.75\ncontrast=0\ngtk-theme=Adwaita\nicon-theme=Adwaita\ncursor-theme=Adwaita\ncursor-size=24\nfont-name=Sans 11\ndocument-font-name=Sans 11\nmonospace-font-name=Monospace 11\ntext-scaling-factor=1.0\n").unwrap();
            for (name, text) in [
                            ("passwd", concat!("tdb1000:x:992:992::/run/td-bus/1000:/bin/false\ntdp1000:x:991:991::/run/td-portal", "/1000:/bin/false\ntda65538:x:65538:65538::/var/lib/td/applications/65538:/bin/false\n")),
                            ("group", "tdb1000:x:992:\ntdp1000:x:991:\ntda65538:x:65538:\n"),
                            ("shadow", "tdb1000:!td-service:0:0:99999:7:::\ntdp1000:!td-service:0:0:99999:7:::\ntda65538:!td-service:0:0:99999:7:::\n"),
                            ("td-principals.tsv", "application\t1000\tnews\t65538\n"),
                        ] {
                            OpenOptions::new().append(true).open(format!("/etc/{name}"))
                                .unwrap().write_all(text.as_bytes()).unwrap();
                        }
            if reopen {
                assert_eq!(
                    fs::read("/etc/td-principals.tsv").unwrap(),
                    fs::read("/var/lib/td/principals.tsv").unwrap()
                );
            } else {
                fs::copy("/etc/td-principals.tsv", "/var/lib/td/principals.tsv").unwrap();
                fs::set_permissions(
                    "/var/lib/td/principals.tsv",
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
            fs::write(
                "/etc/td-bus-applications.tsv",
                "td-bus-applications-v1\t1000\n65537\tmail\t\n65538\tnews\t\n",
            )
            .unwrap();
            // Mount the real hierarchy and delegate each app's sibling leaves.
            fs::create_dir_all("/sys/fs/cgroup").unwrap();
            assert!(Command::new("/bin/td-init")
                .args(["mount", "-t", "cgroup2", "cgroup2", "/sys/fs/cgroup"])
                .status()
                .unwrap()
                .success());
            fs::write(
                "/sys/fs/cgroup/cgroup.subtree_control",
                "+cpu +memory +pids\n",
            )
            .unwrap();
            let mut registry = String::new();
            for (uid, app) in [(65537, "mail"), (65538, "news")] {
                directory(format!("/var/lib/td/applications/{uid}"), uid, 0o700);
                directory(format!("/run/user/{uid}"), uid, 0o700);
                let home = application_home(uid, app);
                for ancestor in home
                    .ancestors()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    directory(ancestor, uid, 0o700);
                }
                let cgroup = format!("/sys/fs/cgroup/td-app-{uid}");
                directory(&cgroup, uid, 0o755);
                fs::write(
                    format!("{cgroup}/cgroup.subtree_control"),
                    "+cpu +memory +pids\n",
                )
                .unwrap();
                directory(format!("{cgroup}/session"), 0, 0o755);
                for leaf in [
                    "cgroup.procs",
                    "cgroup.threads",
                    "cgroup.subtree_control",
                    "session/cgroup.procs",
                    "session/cgroup.threads",
                ] {
                    chown(format!("{cgroup}/{leaf}"), Some(uid), Some(uid)).unwrap();
                }
                let package = format!("/td/store/portal-{app}");
                directory(format!("{package}/files/bin"), 0, 0o755);
                fs::hard_link("/bin/td-secret-tests", format!("{package}/files/bin/probe"))
                    .unwrap();
                fs::hard_link("/bin/td-secret", format!("{package}/files/bin/td-secret")).unwrap();
                fs::write(
                    format!("{package}/manifest"),
                    "disposable source-built portal fixture\n",
                )
                .unwrap();
                // Both apps advertise mail: only the broker's UID binding counts.
                fs::write(format!("{package}/spec"), format!("format=1\nname={app}\nruntime={RUNTIME}\nentry=/app/bin/probe\n\n[Environment]\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\nFLATPAK_ID=mail\nHOME=/home/td\nWAYLAND_DISPLAY=wayland-0\nXDG_RUNTIME_DIR=/run/user/1000\n\n[Context]\nsockets=wayland\n")).unwrap();
                std::os::unix::fs::symlink("/bin/td-jail", format!("/bin/{app}")).unwrap();
                registry.push_str(&format!("{app}\t{package}\n"));
            }
            fs::write("/etc/td-applications.tsv", registry).unwrap();
            std::os::unix::fs::symlink("/bin/td-init", "/bin/umount").unwrap();
            application_files("prepare-application-files");
            if !reopen {
                let store = crate::owned_store(1000).unwrap();
                store.set("mail", "private", b"mail-only fixture").unwrap();
            }
        }

        fn application_files(operation: &str) {
            let output = Command::new("/bin/td-authd")
                .args([operation, "mail"])
                .env_clear()
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        struct Portal {
            broker: Process,
            supervisor: Process,
        }
        impl Portal {
            fn start() -> Self {
                let mut command = Command::new("/bin/td-login");
                command.args([
                    "exec-service-as",
                    "tdb1000",
                    "--",
                    "/bin/td-busd",
                    "run-session",
                ]);
                let mut broker = Process::start(command, "/run/desktop-busd.log");
                wait("session broker", || {
                    assert!(
                        broker.exited().is_none(),
                        "{}",
                        fs::read_to_string("/run/desktop-busd.log").unwrap()
                    );
                    fs::read_to_string("/run/desktop-busd.log")
                        .unwrap()
                        .contains("td-busd: listening on /run/td-bus/1000/bus as ")
                });
                let mut command = Command::new("/bin/td-portal");
                command.args([
                    "supervise",
                    "--bus",
                    "/run/td-bus/1000/bus",
                    "--settings",
                    "/etc/td-portal-settings",
                ]);
                let supervisor = Process::start(command, "/run/desktop-portal.log");
                let mut portal = Self { broker, supervisor };
                wait("activated portal", || {
                    portal.live();
                    let result = Command::new("/bin/td-secret")
                        .args(["get", "main"])
                        .env_clear()
                        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/td-bus/1000/bus")
                        .output()
                        .unwrap();
                    assert!(!result.status.success() && result.stdout.is_empty());
                    let error = String::from_utf8(result.stderr).unwrap();
                    if error.starts_with("td-secret: credential portal refused the request: org.freedesktop.DBus.Error.NameHasNoOwner") {
                                    return false;
                                }
                    assert_eq!(error, "td-secret: credential portal refused the request: org.freedesktop.portal.Error.NotAllowed: credential caller is not an authenticated application\n");
                    true
                });
                portal
            }
            fn live(&mut self) {
                assert!(self.broker.exited().is_none());
                assert!(
                    self.supervisor.exited().is_none(),
                    "{}",
                    fs::read_to_string("/run/desktop-portal.log").unwrap()
                );
            }
            fn finish(mut self) {
                self.live();
                self.broker.0.kill().unwrap();
                assert!(!self.broker.0.wait().unwrap().success());
                let mut status = None;
                wait("portal child and supervisor exit", || {
                    status = self.supervisor.exited();
                    status.is_some()
                });
                assert!(!status.unwrap().success());
            }
        }

        struct Application {
            process: Process,
            home: std::path::PathBuf,
            sequence: usize,
        }
        impl Application {
            fn start(uid: u32, app: &str) -> Self {
                let mut command = Command::new("/bin/td-login");
                command.args([
                    "exec-service-as",
                    &format!("tda{uid}"),
                    "--",
                    &format!("/bin/{app}"),
                    "--exact",
                    APP_TEST,
                    "--ignored",
                    "--test-threads=1",
                ]);
                let mut result = Self {
                    process: Process::start(command, &format!("/run/desktop-{app}.log")),
                    home: application_home(uid, app),
                    sequence: 0,
                };
                wait("jailed application startup", || {
                    assert!(
                        result.process.exited().is_none(),
                        "{}",
                        fs::read_to_string(format!("/run/desktop-{app}.log")).unwrap()
                    );
                    result.home.join("ready").exists()
                });
                result
            }
            fn retrieve(&mut self, name: &str, expected: &str) {
                self.sequence += 1;
                let sequence = self.sequence;
                fs::write(
                    self.home.join("request.next"),
                    format!("{sequence}\t{name}\t{expected}"),
                )
                .unwrap();
                // Root is the disposable fixture controller; the app only reads.
                fs::set_permissions(
                    self.home.join("request.next"),
                    fs::Permissions::from_mode(0o444),
                )
                .unwrap();
                fs::rename(self.home.join("request.next"), self.home.join("request")).unwrap();
                wait("application portal response", || {
                    assert!(self.process.exited().is_none());
                    let response =
                        fs::read_to_string(self.home.join("response")).unwrap_or_default();
                    if !response.starts_with(&format!("{sequence}\t")) {
                        return false;
                    }
                    assert_eq!(response, format!("{sequence}\tok"));
                    true
                });
            }
            fn finish(mut self) {
                fs::write(self.home.join("stop"), b"").unwrap();
                let mut status = None;
                wait("application exit", || {
                    status = self.process.exited();
                    status.is_some()
                });
                assert!(status.unwrap().success());
                for name in ["ready", "request", "response", "stop"] {
                    fs::remove_file(self.home.join(name)).unwrap();
                }
            }
        }

        #[test]
        #[ignore = "test-only application entry for the disposable desktop portal guest"]
        fn qemu_application_portal_client() {
            assert_eq!(fs::metadata("/proc/self").unwrap().uid(), 1000);
            assert!(!Path::new("/var/lib/td/secrets").exists());
            assert!(!Path::new("/run/td-secret").exists());
            let home = Path::new("/home/td");
            fs::write(home.join("ready"), b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(90);
            let mut previous = String::new();
            while !home.join("stop").exists() {
                assert!(Instant::now() < deadline);
                let request = fs::read_to_string(home.join("request")).unwrap_or_default();
                if !request.is_empty() && request != previous {
                    let parts: Vec<_> = request.split('\t').collect();
                    assert_eq!(parts.len(), 3);
                    let result = Command::new("/app/bin/td-secret")
                        .args(["get", parts[1]])
                        .output()
                        .unwrap();
                    let success = if parts[2] == "unavailable" {
                        !result.status.success() && result.stdout.is_empty()
                                        && std::str::from_utf8(&result.stderr).is_ok_and(|error|
                                            error == "td-secret: credential portal refused the request: org.freedesktop.portal.Error.Failed: credential is unavailable; enroll or unlock through secure attention\n")
                    } else {
                        result.status.success() && result.stdout == parts[2].as_bytes()
                    };
                    let response = if success {
                        "ok".into()
                    } else {
                        format!(
                            "failed: {} {}",
                            result.status,
                            String::from_utf8_lossy(&result.stderr)
                        )
                    };
                    fs::write(
                        home.join("response.next"),
                        format!("{}\t{response}", parts[0]),
                    )
                    .unwrap();
                    fs::rename(home.join("response.next"), home.join("response")).unwrap();
                    previous = request;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Standard keyboard: eight modifiers, padding, and six key usages.
        const KEYBOARD: &[u8] = &[
            5, 1, 9, 6, 0xa1, 1, 5, 7, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 8,
            0x81, 2, 0x95, 1, 0x75, 8, 0x81, 1, 0x95, 6, 0x75, 8, 0x15, 0, 0x25, 0x65, 5, 7, 0x19,
            0, 0x29, 0x65, 0x81, 0, 0xc0,
        ];

        pub(crate) struct Keyboard(Uhid);
        impl Keyboard {
            pub(crate) fn new() -> Self {
                let device = Uhid::create("td desktop keyboard", 2, KEYBOARD);
                wait("keyboard enumeration", || {
                    fs::read_dir("/sys/class/input").unwrap().any(|entry| {
                        let path = entry.unwrap().path();
                        path.file_name()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .starts_with("event")
                            && fs::read_to_string(path.join("device/name")).ok().as_deref()
                                == Some("td desktop keyboard\n")
                    })
                });
                Self(device)
            }
            pub(crate) fn report(&mut self, modifiers: u8, key: u8) {
                self.0.input(&[modifiers, 0, key, 0, 0, 0, 0, 0]);
                thread::sleep(Duration::from_millis(100));
            }
            pub(crate) fn key(&mut self, key: u8) {
                self.report(0, key);
                self.report(0, 0);
            }
            fn select(&mut self, key: u8) {
                // A fresh report drains the post-close input quarantine.
                self.key(0x39); // Caps Lock, outside the attention vocabulary.
                self.report(5, 0); // Left Ctrl + Left Alt.
                self.report(5, 0x29); // Escape.
                self.report(0, 0);
                self.key(key);
            }
            fn close(&mut self) {
                self.key(0x29);
            }
        }

        pub(crate) struct Process(std::process::Child);
        impl Process {
            pub(crate) fn start(mut command: Command, log: &str) -> Self {
                let errors = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(log)
                    .unwrap();
                command
                    .env_clear()
                    .current_dir("/")
                    .stdout(Stdio::null())
                    .stderr(errors);
                Self(command.spawn().unwrap())
            }
            pub(crate) fn exited(&mut self) -> Option<std::process::ExitStatus> {
                self.0.try_wait().unwrap()
            }
        }
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        pub(crate) struct Pair {
            compositor: Process,
            authority: Process,
        }
        impl Pair {
            pub(crate) fn start() -> Self {
                let (root, peer) = UnixStream::pair().unwrap();
                let mut command = Command::new("/bin/td-authd");
                command
                    .args([
                        "terminal-serve",
                        "--user",
                        "tester",
                        "--uid",
                        "1000",
                        "--peer-uid",
                        "993",
                    ])
                    .stdin(Stdio::from(OwnedFd::from(root)));
                let authority = Process::start(command, "/run/desktop-authd.log");
                let mut command = Command::new("/bin/td-login");
                command
                    .args([
                        "exec-service-as",
                        "tdc1000",
                        "--",
                        "/bin/td-compositor",
                        "run",
                        "--framebuffer",
                        "/dev/fb0",
                        "--input",
                        "/dev/input",
                        "--socket",
                        "/run/td-compositor/1000/wayland-0",
                        "--portal-socket",
                        "/run/td-compositor/1000/portal-wayland",
                        "--control-socket",
                        "/run/td-compositor/1000/td-control",
                        "--launcher-application",
                        "mail",
                        "--application-ready-socket",
                        "/run/td-compositor/1000/application-ready",
                        "--application-app-id",
                        "td.mail",
                        "--application-content-rgb-a",
                        "112233",
                        "--application-content-rgb-b",
                        "445566",
                        "--terminal-authority",
                        "stdin",
                    ])
                    .stdin(Stdio::from(OwnedFd::from(peer)));
                let compositor = Process::start(command, "/run/desktop-compositor.log");
                let mut pair = Self {
                    compositor,
                    authority,
                };
                wait("paired compositor startup", || {
                    assert!(
                        pair.authority.exited().is_none(),
                        "{}",
                        fs::read_to_string("/run/desktop-authd.log").unwrap()
                    );
                    assert!(
                        pair.compositor.exited().is_none(),
                        "{}",
                        fs::read_to_string("/run/desktop-compositor.log").unwrap()
                    );
                    fs::read_to_string("/run/desktop-compositor.log")
                        .unwrap()
                        .contains("software output")
                });
                no_release();
                pair
            }
            /// The paired authority's process, whose children are its workers.
            pub(crate) fn authority(&self) -> u32 {
                self.authority.0.id()
            }
            pub(crate) fn disconnect(mut self) {
                assert!(self.authority.exited().is_none());
                assert!(self.compositor.exited().is_none());
                self.compositor.0.kill().unwrap();
                assert!(!self.compositor.0.wait().unwrap().success());
                let mut status = None;
                wait("authority generation cleanup", || {
                    status = self.authority.exited();
                    status.is_some()
                });
                assert!(!status.unwrap().success());
                no_release();
                assert!(!Path::new("/run/td-authd/1000/set").exists());
            }
        }

        fn setup(reopen: bool) {
            if reopen {
                prepare_operation_accounts();
            } else {
                prepare_operation_store();
            }
            fs::write(
                "/etc/td-bus-applications.tsv",
                "td-bus-applications-v1\t1000\n65537\tmail\t\n",
            )
            .unwrap();
            fs::set_permissions(
                "/etc/td-bus-applications.tsv",
                fs::Permissions::from_mode(0o444),
            )
            .unwrap();
            for (name, text) in [
                (
                    "passwd",
                    "tdc1000:x:993:993::/run/td-compositor/1000:/bin/false\n",
                ),
                ("group", "tdc1000:x:993:\n"),
                ("shadow", "tdc1000:!td-service:0:0:99999:7:::\n"),
            ] {
                OpenOptions::new()
                    .append(true)
                    .open(format!("/etc/{name}"))
                    .unwrap()
                    .write_all(text.as_bytes())
                    .unwrap();
            }
            for (path, owner, mode) in [
                ("/run/td-compositor", 0, 0o755),
                ("/run/td-compositor/1000", 993, 0o755),
                ("/run/user", 0, 0o755),
                ("/run/user/1000", 1000, 0o700),
                ("/home/tester", 1000, 0o700),
            ] {
                fs::create_dir_all(path).unwrap();
                chown(path, Some(owner), Some(owner)).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            }
            for entry in fs::read_dir("/dev/input").unwrap() {
                let path = entry.unwrap().path();
                if !path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("event")
                {
                    continue;
                }
                assert!(fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_char_device());
                chown(&path, Some(993), Some(993)).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            chown("/dev/fb0", Some(993), Some(993)).unwrap();
            fs::set_permissions("/dev/fb0", fs::Permissions::from_mode(0o600)).unwrap();
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm with a disposable desktop and HID devices"]
        fn qemu_compositor_enrolls_unlocks_and_authorizes_public_credential_write() {
            guard("fido-desktop");
            desktop_roundtrip(false, false);
        }

        fn desktop_roundtrip(persistent: bool, recovery: bool) {
            assert!(!recovery || persistent);
            let _diagnostics = Diagnostics;
            assert!(Command::new("/bin/td-init")
                .args(["hostname", "td-secret-fixture"])
                .status()
                .unwrap()
                .success());
            let mut keyboard = Keyboard::new();
            // Mail's declared idmapped view needs a mountable backing filesystem.
            fs::create_dir_all("/var").unwrap();
            if persistent {
                mount_persistent_var(true);
            } else {
                applet(&[
                    "mount",
                    "-t",
                    "tmpfs",
                    "-o",
                    "nosuid,nodev,mode=0755",
                    "tmpfs",
                    "/var",
                ]);
            }
            setup(false);
            setup_portal(false);
            crate::tpm::tests::qemu_extend(&[9; 32]);
            let token = if persistent {
                persistent_token(true, false)
            } else {
                VirtualCredential::new(44)
            };
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&requests);
            let enrollment_user = Arc::new(std::sync::Mutex::new(None::<Vec<u8>>));
            let primary_user = Arc::clone(&enrollment_user);
            let mut challenges = std::collections::BTreeSet::new();
            let hid = token.checked(move |request, index| {
                eprintln!("desktop CTAP {index}: {}", request[0]);
                let expected = [4, 1, 2, 4, 2, 4, 2, 4, 2];
                assert_eq!(request[0], expected[index]);
                if request[0] != 4 {
                    use crate::fido_cbor::{self, Value};
                    let value = fido_cbor::decode(&request[1..]).unwrap();
                    if request[0] == 1 {
                        *primary_user.lock().unwrap() = Some(
                            value
                                .required(&Value::Unsigned(3))
                                .unwrap()
                                .required(&Value::Text("id"))
                                .unwrap()
                                .bytes()
                                .unwrap()
                                .to_vec(),
                        );
                    }
                    let field = if request[0] == 1 { 1 } else { 2 };
                    let hash: [u8; 32] = value
                        .required(&Value::Unsigned(field))
                        .unwrap()
                        .bytes()
                        .unwrap()
                        .try_into()
                        .unwrap();
                    assert_ne!(hash, [0; 32]);
                    assert!(challenges.insert(hash), "desktop reused a token challenge");
                    if persistent {
                        let path = format!("{COLD_STATE}/challenges");
                        if Path::new(&path).exists() {
                            let prior = fs::read(&path).unwrap();
                            assert!(!prior.as_chunks::<32>().0.contains(&hash));
                        }
                        OpenOptions::new()
                            .append(true)
                            .create(true)
                            .mode(0o600)
                            .open(path)
                            .unwrap()
                            .write_all(&hash)
                            .unwrap();
                    }
                }
                observed.store(index + 1, Ordering::SeqCst);
            });
            let pair = Pair::start();
            let mut portal = Portal::start();
            let mut mail = Application::start(65537, "mail");
            let mut news = Application::start(65538, "news");
            portal.live();
            mail.retrieve("main", "unavailable");
            keyboard.key(0x1b); // X outside attention must not enroll.
            thread::sleep(Duration::from_millis(500));
            assert_eq!(requests.load(Ordering::SeqCst), 0);
            assert!(!Path::new("/var/lib/td/secrets/1000/sealed").exists());
            keyboard.select(if recovery { 0x08 } else { 0x1b }); // E: recovery; X: unrecoverable.
            let recovery_hid = if recovery {
                wait("primary proof request", || {
                    requests.load(Ordering::SeqCst) == 3
                });
                let second = persistent_token(true, true);
                assert_ne!(
                    token.signer.lock().unwrap().cose,
                    second.signer.lock().unwrap().cose
                );
                Some(second.checked(move |request, index| {
                    use crate::fido_cbor::{self, Value};
                    assert_eq!(request[0], [4, 1, 2][index]);
                    if request[0] != 4 {
                        let value = fido_cbor::decode(&request[1..]).unwrap();
                        if request[0] == 1 {
                            let user = value
                                .required(&Value::Unsigned(3))
                                .unwrap()
                                .required(&Value::Text("id"))
                                .unwrap()
                                .bytes()
                                .unwrap();
                            assert_eq!(Some(user), enrollment_user.lock().unwrap().as_deref());
                            let Value::Array(excluded) =
                                value.required(&Value::Unsigned(5)).unwrap()
                            else {
                                panic!("recovery creation omitted the primary exclusion");
                            };
                            assert_eq!(excluded.len(), 1);
                            assert_eq!(
                                excluded[0]
                                    .required(&Value::Text("id"))
                                    .unwrap()
                                    .bytes()
                                    .unwrap(),
                                [44; 32]
                            );
                        }
                        let field = if request[0] == 1 { 1 } else { 2 };
                        let hash = value
                            .required(&Value::Unsigned(field))
                            .unwrap()
                            .bytes()
                            .unwrap();
                        assert_eq!(hash.len(), 32);
                        assert_ne!(hash, [0; 32]);
                        let path = format!("{COLD_STATE}/challenges");
                        let prior = fs::read(&path).unwrap();
                        assert!(!prior.as_chunks::<32>().0.iter().any(|old| old == hash));
                        OpenOptions::new()
                            .append(true)
                            .open(path)
                            .unwrap()
                            .write_all(hash)
                            .unwrap();
                    }
                }))
            } else {
                None
            };
            wait("desktop enrollment", || {
                Path::new("/var/lib/td/secrets/1000/sealed").exists()
            });
            no_release();
            if let Some(hid) = recovery_hid {
                assert_eq!(hid.finish(), (3, 0));
                wait("recovery device removal", || {
                    Device::discover().unwrap().len() == 1
                });
            }
            assert_eq!(requests.load(Ordering::SeqCst), 3);
            mail.retrieve("main", "unavailable");
            keyboard.close();
            keyboard.select(0x18); // U: fresh primary assertion.
            wait("desktop unlock", || {
                Path::new("/run/td-secret/1000/key").exists()
            });
            read_released(b"firstboot fixture");
            mail.retrieve("main", "firstboot fixture");
            mail.retrieve("private", "mail-only fixture");
            news.retrieve("main", "untouched fixture");
            news.retrieve("private", "unavailable");
            keyboard.close();
            let before = sealed_bytes();
            let mut command = Command::new("/bin/td-login");
            command
                .args([
                    "exec-as",
                    "tester",
                    "--",
                    "/bin/td-secret",
                    "set",
                    "mail/main",
                ])
                .stdin(Stdio::piped());
            let mut client = Process::start(command, "/run/desktop-set.log");
            client
                .0
                .stdin
                .take()
                .unwrap()
                .write_all(b"desktop fixture")
                .unwrap();
            wait("public credential queue", || {
                assert!(
                    client.exited().is_none(),
                    "{}",
                    fs::read_to_string("/run/desktop-set.log").unwrap()
                );
                fs::read_to_string("/run/desktop-set.log")
                    .unwrap()
                    .contains("then W")
            });
            thread::sleep(Duration::from_millis(500));
            assert!(client.exited().is_none());
            assert_eq!(sealed_bytes(), before);
            assert_eq!(requests.load(Ordering::SeqCst), 5);
            keyboard.select(0x1a); // W: authorize exactly the queued write.
            let mut status = None;
            wait("public credential completion", || {
                status = client.exited();
                status.is_some()
            });
            assert!(
                status.unwrap().success(),
                "{}",
                fs::read_to_string("/run/desktop-set.log").unwrap()
            );
            assert_ne!(sealed_bytes(), before);
            read_released(b"desktop fixture");
            assert_eq!(requests.load(Ordering::SeqCst), 7);
            mail.retrieve("main", "desktop fixture");
            news.retrieve("main", "untouched fixture");
            pair.disconnect();
            portal.live();
            mail.retrieve("main", "unavailable");
            // A fresh production generation must prepare while locked and
            // require another physical selection and assertion before release.
            let pair = Pair::start();
            keyboard.select(0x18);
            wait("replacement generation unlock", || {
                Path::new("/run/td-secret/1000/key").exists()
            });
            read_released(b"desktop fixture");
            assert_eq!(requests.load(Ordering::SeqCst), 9);
            mail.retrieve("main", "desktop fixture");
            pair.disconnect();
            portal.live();
            mail.retrieve("main", "unavailable");
            mail.finish();
            news.finish();
            portal.finish();
            application_files("release-application-files");
            assert_eq!(hid.finish(), (9, 0));
            if persistent {
                fs::write(
                    format!("{COLD_STATE}/bundle-hash"),
                    crate::crypto::digest(&sealed_bytes()),
                )
                .unwrap();
                fs::copy(
                    "/proc/sys/kernel/random/boot_id",
                    format!("{COLD_STATE}/boot-id"),
                )
                .unwrap();
                applet(&["umount", "/var"]);
            }
        }

        const COLD_STATE: &str = "/var/lib/td/secret-fixture";

        fn applet(args: &[&str]) {
            assert!(
                Command::new("/bin/td-init")
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            );
        }

        fn mount_persistent_var(create: bool) {
            assert!(fs::metadata("/dev/vda")
                .unwrap()
                .file_type()
                .is_block_device());
            fs::create_dir_all("/var").unwrap();
            if create {
                let mut prefix = [0; 4096];
                File::open("/dev/vda")
                    .unwrap()
                    .read_exact(&mut prefix)
                    .unwrap();
                assert_eq!(prefix, [0; 4096], "fixture disk is not fresh");
                assert!(Command::new("/bin/mkfs.btrfs")
                    .args(["-q", "/dev/vda"])
                    .status()
                    .unwrap()
                    .success());
                fs::create_dir("/volume").unwrap();
                applet(&[
                    "mount",
                    "-t",
                    "btrfs",
                    "-o",
                    "nosuid,nodev",
                    "/dev/vda",
                    "/volume",
                ]);
                assert!(Command::new("/bin/btrfs")
                    .args(["subvolume", "create", "/volume/@var"])
                    .status()
                    .unwrap()
                    .success());
                applet(&["umount", "/volume"]);
            }
            applet(&[
                "mount",
                "-t",
                "btrfs",
                "-o",
                "nosuid,nodev,subvol=@var",
                "/dev/vda",
                "/var",
            ]);
            assert!(fs::read_to_string("/proc/self/mountinfo")
                .unwrap()
                .lines()
                .any(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    fields.get(3) == Some(&"/@var")
                        && fields.get(4) == Some(&"/var")
                        && fields.get(5).is_some_and(|options| {
                            ["rw", "nosuid", "nodev"].iter().all(|required| {
                                options.split(',').any(|option| option == *required)
                            })
                        })
                        && line.contains(" - btrfs ")
                }));
        }

        fn persistent_token(create: bool, recovery: bool) -> VirtualCredential {
            let role = if recovery { "recovery" } else { "primary" };
            if create {
                directory(COLD_STATE, 0, 0o700);
                fs::write(format!("{COLD_STATE}/{role}-template"), fresh()).unwrap();
            }
            let seed: [u8; 32] = fs::read(format!("{COLD_STATE}/{role}-template"))
                .unwrap()
                .try_into()
                .unwrap();
            let signer = crate::tpm::tests::SigningKey::persistent(&seed);
            let public = format!("{COLD_STATE}/{role}-public");
            if create {
                fs::write(public, &signer.cose).unwrap();
            } else {
                assert_eq!(
                    fs::read(public).unwrap(),
                    signer.cose,
                    "cold token changed key"
                );
            }
            VirtualCredential {
                id: vec![if recovery { 45 } else { 44 }; 32],
                signer: Arc::new(std::sync::Mutex::new(signer)),
            }
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm with disposable persistent disk"]
        fn qemu_desktop_creates_persistent_store() {
            guard("fido-cold-create");
            desktop_roundtrip(true, false);
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm after the persistent creation guest"]
        fn qemu_desktop_reopens_persistent_store_locked() {
            guard("fido-cold-reopen");
            cold_reopen(false);
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm with a disposable recovery-policy disk"]
        fn qemu_desktop_creates_persistent_recovery_store() {
            guard("fido-cold-recovery-create");
            desktop_roundtrip(true, true);
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm after recovery-policy creation"]
        fn qemu_desktop_recovers_persistent_store_without_primary() {
            guard("fido-cold-recovery-reopen");
            cold_reopen(true);
        }

        fn cold_reopen(recovery: bool) {
            let _diagnostics = Diagnostics;
            applet(&["hostname", "td-secret-fixture"]);
            let mut keyboard = Keyboard::new();
            mount_persistent_var(false);
            assert_ne!(
                fs::read("/proc/sys/kernel/random/boot_id").unwrap(),
                fs::read(format!("{COLD_STATE}/boot-id")).unwrap()
            );
            let before = sealed_bytes();
            assert_eq!(
                crate::crypto::digest(&before).as_slice(),
                fs::read(format!("{COLD_STATE}/bundle-hash")).unwrap()
            );
            for name in ["master", "mail.main", "news.main", "mail.private"] {
                assert!(!store::user_path(1000).join(name).exists());
            }
            no_release();
            setup(true);
            setup_portal(true);
            assert_eq!(sealed_bytes(), before, "reopen setup rewrote the store");
            crate::tpm::tests::qemu_extend(&[9; 32]);
            let token = persistent_token(false, recovery);
            let prior = fs::read(format!("{COLD_STATE}/challenges")).unwrap();
            assert_eq!(prior.len(), if recovery { 7 * 32 } else { 5 * 32 });
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&requests);
            let hid = token.checked(move |request, index| {
                assert_eq!(request[0], [4, 2][index]);
                if request[0] == 2 {
                    use crate::fido_cbor::{self, Value};
                    let value = fido_cbor::decode(&request[1..]).unwrap();
                    let hash = value
                        .required(&Value::Unsigned(2))
                        .unwrap()
                        .bytes()
                        .unwrap();
                    assert_eq!(hash.len(), 32);
                    assert_ne!(hash, [0; 32]);
                    assert!(
                        !prior.as_chunks::<32>().0.iter().any(|old| old == hash),
                        "cold unlock replayed a challenge"
                    );
                }
                observed.store(index + 1, Ordering::SeqCst);
            });
            discover_one();
            let pair = Pair::start();
            let mut portal = Portal::start();
            let mut mail = Application::start(65537, "mail");
            let mut news = Application::start(65538, "news");
            mail.retrieve("main", "unavailable");
            news.retrieve("main", "unavailable");
            assert_eq!(requests.load(Ordering::SeqCst), 0);
            keyboard.select(if recovery { 0x15 } else { 0x18 }); // R or U.
            wait("cold desktop unlock", || {
                Path::new("/run/td-secret/1000/key").exists()
            });
            read_released(b"desktop fixture");
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            mail.retrieve("main", "desktop fixture");
            mail.retrieve("private", "mail-only fixture");
            news.retrieve("main", "untouched fixture");
            news.retrieve("private", "unavailable");
            pair.disconnect();
            portal.live();
            mail.retrieve("main", "unavailable");
            mail.finish();
            news.finish();
            portal.finish();
            application_files("release-application-files");
            assert_eq!(hid.finish(), (2, 0));
            assert_eq!(sealed_bytes(), before, "cold unlock rewrote the store");
            applet(&["umount", "/var"]);
        }
    }
}
