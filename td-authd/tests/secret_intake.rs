#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::consent::Role;
use std::io::Read;

fn pair() -> (UnixStream, Pending) {
    let (mut client, server) = UnixStream::pair().unwrap();
    let owner = sys::peer_uid(&server).unwrap();
    let mut pending = Pending::new(server, owner).unwrap();
    pending.poll().unwrap();
    let mut greeting = [0; 8];
    client.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting, GREETING);
    (client, pending)
}

fn admit(target: &Target, _: u32) -> io::Result<Operation> {
    target.operation(1000, 65537).map_err(io::Error::other)
}

fn frame() -> Vec<u8> {
    let bytes = Target::parse("mail/main", Role::Recovery).unwrap().encode();
    [(bytes.len() as u16).to_be_bytes().as_slice(), &bytes].concat()
}

fn credential(bytes: &[u8]) -> File {
    let mut file = sys::create_credential().unwrap();
    file.write_all(bytes).unwrap();
    sys::seal_credential(&file).unwrap();
    file
}

#[test]
fn capture_retains_exact_bytes_and_selection_is_one_shot() {
    let (client, mut pending) = pair();
    let mut file = credential(&vec![0xa5; MAX_SECRET]);
    sys::send_descriptor(&client, &frame(), &file).unwrap();
    pending.poll_with(admit).unwrap();
    assert!(!pending.selected);
    assert!(file.write_all(b"replacement").is_err());
    let (operation, captured) = pending.capture().unwrap();
    assert_eq!(operation, Target::parse("mail/main", Role::Recovery).unwrap().operation(1000, 65537).unwrap());
    assert_eq!(captured.0, vec![0xa5; MAX_SECRET]);
    assert!(pending.capture().is_err());
    drop(client);
    let until = Instant::now() + Duration::from_secs(1);
    while pending.poll().is_ok() {
        assert!(Instant::now() < until, "closed requester stayed connected");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn descriptor_validation_precedes_application_admission() {
    for mode in 0..4 {
        let (client, mut pending) = pair();
        let file = match mode {
            0 => credential(&[]),
            1 => credential(&vec![42; MAX_SECRET + 1]),
            2 => { let mut file = sys::create_credential().unwrap(); file.write_all(b"mutable").unwrap(); file },
            _ => File::open("/dev/null").unwrap(),
        };
        sys::send_descriptor(&client, &frame(), &file).unwrap();
        assert!(pending.poll_with(|_, _| panic!("admitted invalid descriptor")).is_err());
        assert!(pending.operation.is_none());
    }
}

#[test]
fn missing_multiple_and_late_descriptors_are_refused() {
    let (mut client, mut pending) = pair();
    client.write_all(&frame()).unwrap();
    assert!(pending.poll_with(|_, _| panic!("admitted missing descriptor")).is_err());

    let (client, mut pending) = pair();
    let frame = frame();
    let file = credential(b"secret");
    sys::send_descriptor(&client, &frame[..2], &file).unwrap();
    sys::send_descriptor(&client, &frame[2..], &file).unwrap();
    assert!(pending.poll_with(|_, _| panic!("admitted multiple descriptors")).is_err());

    let (client, mut pending) = pair();
    sys::send_descriptor(&client, &frame, &file).unwrap();
    pending.poll_with(admit).unwrap();
    sys::send_descriptor(&client, b"extra", &file).unwrap();
    assert!(pending.poll().is_err());
}

#[test]
fn identity_and_expiration_are_checked_before_capture() {
    let (client, mut pending) = pair();
    sys::send_descriptor(&client, &frame(), &credential(b"secret")).unwrap();
    pending.owner = pending.owner.saturating_add(1);
    assert!(pending.poll_with(|_, _| panic!("admitted wrong sender")).is_err());

    let (client, mut pending) = pair();
    sys::send_descriptor(&client, &frame(), &credential(b"secret")).unwrap();
    pending.poll_with(admit).unwrap();
    pending.deadline = Instant::now();
    assert!(pending.capture().is_err());
    assert!(pending.poll().is_err());
}

#[test]
#[ignore = "exec-only delegated sender fixture"]
fn delegated_sender() {
    let mut stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.write_all(&frame()[2..]).unwrap();
    let _ = stream.read(&mut [0]);
}

#[test]
fn inherited_connection_cannot_replace_the_pinned_requester() {
    use std::os::fd::OwnedFd;
    use std::process::{Command, Stdio};
    let (client, mut pending) = pair();
    sys::send_descriptor(&client, &frame()[..2], &credential(b"secret")).unwrap();
    pending.poll_with(admit).unwrap();
    assert!(pending.peer.is_some());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "secret_intake::tests::delegated_sender", "--ignored"])
        .stdin(Stdio::from(OwnedFd::from(client))).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let refused = loop {
        if pending.poll_with(|_, _| panic!("admitted delegated sender")).is_err() { break true; }
        if Instant::now() >= until { break false; }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = child.kill();
    child.wait().unwrap();
    assert!(refused);
    assert!(pending.operation.is_none());
}

#[test]
#[ignore = "root-only disposable VM oracle for the actual public client"]
fn root_public_client_uses_the_human_identity_and_immutable_descriptor() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    assert!(fs::read_to_string("/proc/cmdline").unwrap().split_whitespace()
        .any(|word| word == "td.write-intake-fixture=1"));
    assert!(!std::path::Path::new("/etc/passwd").exists());
    fs::create_dir_all("/etc").unwrap();
    fs::set_permissions("/etc", fs::Permissions::from_mode(0o755)).unwrap();
    for (name, text, mode) in [
        ("td-principals.tsv", "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\tmail\t65537\n", 0o444),
        ("passwd", "tester:x:1000:1000::/home/tester:/bin/false\ntda65537:x:65537:65537::/var/lib/td/applications/65537:/bin/false\n", 0o644),
        ("group", "tester:x:1000:\ntda65537:x:65537:\n", 0o644),
        ("shadow", "tester::0:0:99999:7:::\ntda65537:!td-service:0:0:99999:7:::\n", 0o600),
    ] {
        let path = format!("/etc/{name}");
        fs::write(&path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    let mut intake = Intake::bind(1000).unwrap();
    assert!(intake.select().is_err());
    let metadata = fs::symlink_metadata(SOCKET).unwrap();
    assert_eq!((metadata.uid(), metadata.gid(), metadata.mode() & 0o7777), (1000, 1000, 0o600));
    for (uid, recovery) in [(1000, false), (1000, true), (65537, false), (0, false)] {
        let mut command = Command::new("/bin/td-secret");
        command.arg("set");
        if recovery { command.arg("--recovery"); }
        let mut child = command.arg("mail/main").uid(uid).gid(uid).env_clear().current_dir("/")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(b"exact credential\nbytes\0");
        drop(stdin);
        let until = Instant::now() + Duration::from_secs(10);
        let mut captured = false;
        loop {
            intake.tick();
            if intake.pending.as_ref().is_some_and(|pending| pending.operation.is_some()) {
                assert_eq!(uid, 1000, "nonhuman requester reached admission");
                assert!(!captured);
                let (operation, credential) = intake.select().unwrap();
                assert_eq!(credential.0, b"exact credential\nbytes\0");
                let role = if recovery { Role::Recovery } else { Role::Primary };
                assert_eq!(operation, Target::parse("mail/main", role).unwrap().operation(1000, 65537).unwrap());
                assert!(intake.select().is_err());
                captured = true;
                // Transport fixture only: no store write or hardware claim.
                intake.finish(true);
            }
            if child.try_wait().unwrap().is_some() { break; }
            if Instant::now() >= until {
                child.kill().unwrap(); child.wait().unwrap(); panic!("public client stalled");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(captured, uid == 1000, "client uid {uid}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.status.success(), uid == 1000, "client uid {uid}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.windows(16).any(|bytes| bytes == b"exact credential"));
    }
    drop(intake);
    assert!(!std::path::Path::new(SOCKET).exists());
}
