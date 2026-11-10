//! One captured credential, one presented request, one fresh token assertion.

use crate::app_policy::Policy;
use crate::consent::{Operation, Request, Role};
use crate::operation::{remaining, Credential, Wire, OPERATION_TIME};
use crate::{crypto, fido_device, fido_enroll, fido_metadata, operation, principals, store, tpm};
use std::time::Instant;

/// The request, if it writes for an application this session's image
/// declares at the uid the workload account ledger holds for it.
fn description(bytes: &[u8], uid: u32, applications: &Policy) -> Result<Request, String> {
    let request = Request::decode(bytes)?;
    match request.operation() {
        Operation::Set {
            application,
            application_uid,
            ..
        } if request.owner() == uid
            && applications.owner() == uid
            && applications.account(application) == Some(*application_uid) =>
        {
            Ok(request)
        }
        _ => Err("private write does not match its installed application and session".into()),
    }
}

fn challenge(request: &Request) -> [u8; 32] {
    let mut bytes = b"td-secret/presented-write/v1\0".to_vec();
    bytes.extend_from_slice(&request.encode());
    crypto::digest(&bytes)
}

fn perform<T>(
    wire: &mut Wire,
    request: &Request,
    credential: Credential,
    acquire: impl FnOnce([u8; 32], Instant) -> Result<T, String>,
    commit: impl FnOnce(T, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    wire.acknowledge(0x10, 0x11, request)?;
    let proof = acquire(challenge(request), wire.deadline())?;
    wire.acknowledge(0x12, 0x13, request)?;
    remaining(wire.deadline())?;
    commit(proof, credential.bytes())?;
    drop(credential);
    wire.send(&[0x14])
}

pub fn run(uid: u32) -> Result<(), String> {
    let stream = operation::startup()?;
    store::require_protected_memory()?;
    let deadline = Instant::now()
        .checked_add(OPERATION_TIME)
        .ok_or("operation deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    let registry = principals::Registry::load()?;
    registry.verify_installed_accounts(&registry)?;
    let applications = crate::app_policy::load()?;
    let request = description(&wire.receive()?, uid, &applications)?;
    let store = super::owned_store(uid)?;
    if !store.token_protected()? {
        return Err("credential store is not token enrolled".into());
    }
    let credential = wire.receive_credential()?;
    let (application, name, role) = match request.operation() {
        Operation::Set {
            application,
            name,
            role,
            ..
        } => (
            application,
            name,
            match role {
                Role::Primary => fido_metadata::Role::Primary,
                Role::Recovery => fido_metadata::Role::Recovery,
            },
        ),
        _ => return Err("unsupported private write operation".into()),
    };
    let client = tpm::Client::new(tpm::Device::open()?);
    perform(
        &mut wire,
        &request,
        credential,
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
            let proof = store.bound_token_request(role, challenge, &info)?;
            let response = token.cbor(proof.bytes())?;
            Ok((proof, response))
        },
        |(proof, response), bytes| {
            store.set_token(application, name, bytes, proof, response.as_ref(), client)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    fn request(role: Role) -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Set {
                role,
                application: "mail".into(),
                name: "Main".into(),
                application_uid: 65537,
                requester: 1000,
            },
        )
        .unwrap()
    }
    const DECLARED: &str = "td-bus-applications-v2\t1000\nmail\tpersonal\t\nnews\tservice\t\n";
    const LEDGER: &str = "td-accounts-v1\napplication\tmail\t65537\napplication\tnews\t65538\n";
    fn policy() -> Policy {
        Policy::from_tables(DECLARED, LEDGER).unwrap()
    }
    fn pair() -> (Wire, Wire, UnixStream) {
        let (one, two) = UnixStream::pair().unwrap();
        let raw = two.try_clone().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        (
            Wire::new(one, deadline).unwrap(),
            Wire::new(two, deadline).unwrap(),
            raw,
        )
    }
    fn input(raw: &mut UnixStream, bytes: &[u8]) {
        raw.write_all(&(bytes.len() as u16).to_be_bytes()).unwrap();
        raw.write_all(bytes).unwrap();
    }
    fn answer(parent: &mut Wire, tag: u8, expected: &Request) {
        let mut frame = parent.receive().unwrap();
        assert_eq!(frame[0], tag);
        assert_eq!(&frame[33..], expected.encode());
        frame[0] += 1;
        parent.send(&frame).unwrap();
    }

    #[test]
    fn captured_bytes_require_separate_exact_presentation_and_commit_rounds() {
        for role in [Role::Primary, Role::Recovery] {
            for stop in 0..5 {
                let (mut child, mut parent, mut raw) = pair();
                let mut value = vec![7; store::MAX_SECRET];
                input(&mut raw, &value);
                let credential = child.receive_credential().unwrap();
                // The parent source buffer no longer selects the committed bytes.
                value.fill(9);
                drop(raw);
                let acquired = Arc::new(AtomicUsize::new(0));
                let committed = Arc::new(AtomicUsize::new(0));
                let (a, c) = (acquired.clone(), committed.clone());
                let expected = request(role);
                let worker_request = expected.clone();
                let worker = std::thread::spawn(move || {
                    perform(
                        &mut child,
                        &worker_request,
                        credential,
                        |digest, _| {
                            assert_eq!(digest, challenge(&worker_request));
                            a.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                        |(), bytes| {
                            assert_eq!(bytes, vec![7; store::MAX_SECRET]);
                            c.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                });
                match stop {
                    0 => drop(parent),
                    1 => {
                        let mut frame = parent.receive().unwrap();
                        frame[0] = 0x11;
                        frame[9] ^= 1;
                        parent.send(&frame).unwrap();
                        drop(parent);
                    }
                    _ => {
                        answer(&mut parent, 0x10, &expected);
                        if stop == 2 {
                            drop(parent);
                        } else if stop == 3 {
                            let mut frame = parent.receive().unwrap();
                            frame[0] = 0x13;
                            frame[33] ^= 1;
                            parent.send(&frame).unwrap();
                            drop(parent);
                        } else {
                            answer(&mut parent, 0x12, &expected);
                            assert_eq!(parent.receive().unwrap(), [0x14]);
                        }
                    }
                }
                assert_eq!(worker.join().unwrap().is_ok(), stop == 4);
                assert_eq!(acquired.load(Ordering::SeqCst), usize::from(stop >= 2));
                assert_eq!(committed.load(Ordering::SeqCst), usize::from(stop == 4));
            }
        }
    }

    #[test]
    fn private_credential_frame_refuses_empty_oversized_truncated_and_expired_input() {
        for bytes in [vec![], vec![7; store::MAX_SECRET + 1]] {
            let (mut child, parent, mut raw) = pair();
            input(&mut raw, &bytes);
            drop(raw);
            drop(parent);
            assert!(child.receive_credential().is_err());
        }
        let (mut child, parent, mut raw) = pair();
        raw.write_all(&[0, 4, 7, 8]).unwrap();
        drop(raw);
        drop(parent);
        assert!(child.receive_credential().is_err());
        let (one, two) = UnixStream::pair().unwrap();
        let mut child = Wire::new(one, Instant::now()).unwrap();
        drop(two);
        assert!(child.receive_credential().is_err());
        let (mut child, _, mut raw) = pair();
        input(&mut raw, b"a");
        assert_eq!(child.receive_credential().unwrap().bytes(), b"a");
    }

    #[test]
    fn write_description_requires_the_installed_assignment_and_binds_every_argument() {
        let original = request(Role::Primary);
        for (declared, ledger) in [
            (DECLARED, "td-accounts-v1\napplication\tnews\t65538\n"),
            ("td-bus-applications-v2\t1000\nnews\tservice\t\n", LEDGER),
            (&*DECLARED.replace("v2\t1000", "v2\t1001"), LEDGER),
        ] {
            assert!(
                description(
                    &original.encode(),
                    1000,
                    &Policy::from_tables(declared, ledger).unwrap()
                )
                .is_err(),
                "an undeclared or unallocated application, or another session's, must refuse"
            );
        }
        assert_eq!(
            challenge(&original),
            [
                104, 40, 91, 107, 166, 106, 203, 16, 65, 141, 242, 5, 69, 36, 147, 139, 229, 64,
                27, 66, 42, 7, 221, 235, 159, 202, 38, 116, 237, 120, 72, 207
            ]
        );
        assert_eq!(
            description(&original.encode(), 1000, &policy()).unwrap(),
            original
        );
        assert!(description(&original.encode(), 1001, &policy()).is_err());
        let mut old_tag = original.encode();
        old_tag[44] = 3;
        assert!(Request::decode(&old_tag).is_err());
        let mut bad_role = original.encode();
        bad_role[45] = 0;
        assert!(Request::decode(&bad_role).is_err());
        for (app, name, app_uid, role) in [
            ("news", "Main", 65537, Role::Primary),
            ("mail", "other", 65537, Role::Primary),
            ("mail", "Main", 65538, Role::Primary),
            ("mail", "Main", 65537, Role::Recovery),
        ] {
            let changed = Request::new(
                [42; 32],
                1000,
                Operation::Set {
                    role,
                    application: app.into(),
                    name: name.into(),
                    application_uid: app_uid,
                    requester: 1000,
                },
            )
            .unwrap();
            assert_ne!(challenge(&changed), challenge(&original));
            assert_eq!(
                description(&changed.encode(), 1000, &policy()).is_ok(),
                app == "mail" && app_uid == 65537
            );
        }
        let changed = Request::new([43; 32], 1000, original.operation().clone()).unwrap();
        assert_ne!(challenge(&changed), challenge(&original));
        let unlock = Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        assert!(description(&unlock.encode(), 1000, &policy()).is_err());
        for elevation in crate::elevation_descriptions(1000) {
            assert!(description(&elevation, 1000, &policy()).is_err());
        }
        assert!(request(Role::Recovery)
            .lines()
            .iter()
            .any(|line| line == "TOUCH THE RECOVERY TOKEN"));
    }

    #[test]
    fn a_prequeued_presentation_round_cannot_commit_a_write() {
        let (mut child, mut parent, mut raw) = pair();
        input(&mut raw, b"credential");
        drop(raw);
        let credential = child.receive_credential().unwrap();
        let worker = std::thread::spawn(move || {
            perform(
                &mut child,
                &request(Role::Primary),
                credential,
                |_, _| Ok(()),
                |(), _| panic!("replayed round committed"),
            )
        });
        let mut first = parent.receive().unwrap();
        first[0] = 0x11;
        parent.send(&first).unwrap();
        first[0] = 0x13;
        parent.send(&first).unwrap();
        assert!(worker.join().unwrap().is_err());
    }
    #[test]
    #[ignore = "requires the explicitly marked disposable root VM"]
    fn root_worker_refuses_a_retained_reservation_without_an_installed_account() {
        use std::fs;
        use std::os::{fd::OwnedFd, unix::fs::PermissionsExt};
        use std::process::{Command, Stdio};
        assert!(fs::read_to_string("/proc/cmdline")
            .unwrap()
            .split_whitespace()
            .any(|word| word == "td.write-fixture=1"));
        store::require_root().unwrap();
        assert!(!std::path::Path::new("/etc/passwd").exists());
        fs::create_dir_all("/etc").unwrap();
        fs::set_permissions("/etc", fs::Permissions::from_mode(0o755)).unwrap();
        // mail is declared, but the ledger holds no account for it yet.
        fs::create_dir_all("/var/lib/td").unwrap();
        let ledger = "/var/lib/td/accounts.tsv";
        for (path, text, mode) in [
            (
                "/etc/td-principals.tsv",
                "td-principals-v1\nsession\t1000\t993\t992\t991\n",
                0o444,
            ),
            (
                "/etc/passwd",
                "tester::1000:1000::/home/tester:/bin/false\n",
                0o644,
            ),
            ("/etc/group", "tester:x:1000:\n", 0o644),
            ("/etc/td-bus-applications.tsv", DECLARED, 0o444),
            (ledger, "td-accounts-v1\napplication\tnews\t65538\n", 0o644),
        ] {
            fs::write(path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let registry = principals::Registry::load().unwrap();
        registry.verify_installed_accounts(&registry).unwrap();
        assert_eq!(crate::app_policy::load().unwrap().account("mail"), None);
        fs::create_dir_all("/run/td-secret/1000").unwrap();
        let key = "/run/td-secret/1000/key";
        fs::write(key, [9; 64]).unwrap();
        for role in [Role::Primary, Role::Recovery] {
            let (mut parent, child) = UnixStream::pair().unwrap();
            let process = Command::new("/bin/td-secret")
                .args(["write-operation", "--uid", "1000"])
                .stdin(Stdio::from(OwnedFd::from(child)))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env_clear()
                .current_dir("/")
                .spawn()
                .unwrap();
            input(&mut parent, &request(role).encode());
            let output = process.wait_with_output().unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8(output.stderr)
                .unwrap()
                .contains("private write does not match its installed application and session"));
            assert_eq!(fs::read(key).unwrap(), [9; 64]);
        }
        fs::write(ledger, LEDGER).unwrap();
        registry.verify_installed_accounts(&registry).unwrap();
        assert!(description(
            &request(Role::Primary).encode(),
            1000,
            &crate::app_policy::load().unwrap()
        )
        .is_ok());
    }
}
