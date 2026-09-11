#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    path: PathBuf,
    owner: u32,
    id: String,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-install-intake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("manifest"), b"approved manifest").unwrap();
        Self {
            owner: fs::metadata(&path).unwrap().uid(),
            path,
            id: crate::sha256::hex_digest(b"approved manifest"),
        }
    }
    fn body(&self) -> Vec<u8> {
        format!("{}{}", self.id, self.path.display()).into_bytes()
    }
    fn frame(&self) -> Vec<u8> {
        let body = self.body();
        [
            u16::try_from(body.len()).unwrap().to_be_bytes().as_slice(),
            &body,
        ]
        .concat()
    }
    fn ready(&self) -> Ready {
        Ready::capture(&self.body(), self.owner).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn captured_source_keeps_identity_and_manifest_id_is_not_a_path_claim() {
    let fixture = Fixture::new();
    let ready = fixture.ready();
    let original = ready.source.metadata().unwrap().ino();
    let renamed = fixture.path.join("old");
    fs::rename(fixture.path.join("manifest"), &renamed).unwrap();
    fs::write(fixture.path.join("manifest"), b"different manifest").unwrap();
    assert!(Ready::capture(&fixture.body(), fixture.owner).is_err());
    assert_eq!(ready.source.metadata().unwrap().ino(), original);
    fs::remove_file(fixture.path.join("manifest")).unwrap();
    std::os::unix::fs::symlink(&renamed, fixture.path.join("manifest")).unwrap();
    assert!(Ready::capture(&fixture.body(), fixture.owner).is_err());
    for path in ["relative", "/tmp/../secret", "/tmp/\0bad"] {
        assert!(normalized(path).is_err());
    }
    assert!(normalized(&format!("/{}", "a".repeat(4096))).is_err());
    assert!(Ready::capture(&fixture.body(), fixture.owner.wrapping_add(1)).is_err());
}

#[test]
fn real_transport_admits_only_one_bounded_descriptor_free_frame() {
    let fixture = Fixture::new();
    for extra in [false, true] {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut pending = Pending::new(server, fixture.owner).unwrap();
        pending.poll().unwrap();
        let mut greeting = [0; 8];
        client.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, GREETING);
        for byte in fixture.frame() {
            client.write_all(&[byte]).unwrap();
            pending.poll().unwrap();
        }
        for _ in 0..3 {
            pending.poll().unwrap();
        }
        assert!(pending.acknowledged);
        assert_eq!(pending.ready.as_ref().unwrap().deployment, fixture.id);
        let mut admitted = [0];
        client.read_exact(&mut admitted).unwrap();
        assert_eq!(admitted, [ADMITTED]);
        if extra {
            client.write_all(b"x").unwrap();
        } else {
            client.shutdown(std::net::Shutdown::Both).unwrap();
            drop(client);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while pending.poll().is_ok() {
            assert!(
                Instant::now() < deadline,
                "requester departure or extra traffic not observed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut pending = Pending::new(server, fixture.owner).unwrap();
    pending.poll().unwrap();
    let file = File::open(fixture.path.join("manifest")).unwrap();
    sys::send_descriptor(&client, &fixture.frame(), &file).unwrap();
    assert!(pending.poll().is_err());
    let mut greeting = [0; 8];
    client.read_exact(&mut greeting).unwrap();
}

#[test]
fn installation_needs_exact_presentation_then_a_single_commit() {
    let fixture = Fixture::new();
    let mut installation = Installation::start(1000, fixture.ready()).unwrap();
    let request = installation.request().clone();
    assert!(installation.commit(&request).is_err());
    let wrong = Request::new([99; 32], 1000, request.operation().clone()).unwrap();
    assert!(installation.presented(&wrong).is_err());
    installation.presented(&request).unwrap();
    assert!(installation.presented(&request).is_err());
    assert!(matches!(installation.poll().unwrap(), Event::Commit(_)));
    assert!(installation.commit(&wrong).is_err());
    installation.cancel("physical Escape").unwrap();
    assert!(installation.commit(&request).is_err());
    assert!(matches!(installation.poll().unwrap(), Event::Failed(_)));
    assert!(installation.source.is_none() && installation.child.is_none());
    let mut expired = Installation::start(1000, fixture.ready()).unwrap();
    expired.deadline = Instant::now();
    assert!(expired.presented(&expired.request().clone()).is_err());
    assert!(matches!(expired.poll().unwrap(), Event::Failed(_)));
}

#[test]
#[ignore = "exec-only delegated sender"]
fn delegated_sender() {
    let fd = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
    let mut stream = UnixStream::from(fd);
    stream.write_all(b"x").unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
fn a_delegated_connection_cannot_change_its_submitting_process() {
    let fixture = Fixture::new();
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut pending = Pending::new(server, fixture.owner).unwrap();
    pending.poll().unwrap();
    client.write_all(&[0, 65]).unwrap();
    pending.poll().unwrap();
    assert!(pending.peer.is_some());
    let descriptor: std::os::fd::OwnedFd = client.into();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "deployment::tests::delegated_sender",
            "--ignored",
        ])
        .stdin(Stdio::from(descriptor))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        if let Err(error) = pending.poll() {
            break error.to_string();
        }
        if Instant::now() >= deadline {
            break "no refusal".to_string();
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = child.kill();
    child.wait().unwrap();
    assert_eq!(result, "update sender changed");
}

#[test]
#[ignore = "exec-only installation child"]
fn installation_child() {
    assert!(fs::metadata("/proc/self/fd/0").unwrap().is_dir());
}

#[test]
fn commit_transfers_one_held_directory_and_completion_needs_the_child_exit() {
    let fixture = Fixture::new();
    for failure in [false, true] {
        let mut installation = Installation::start(1000, fixture.ready()).unwrap();
        let request = installation.request().clone();
        installation.presented(&request).unwrap();
        let mut calls = 0;
        installation
            .commit_with(&request, |source, id| {
                calls += 1;
                assert_eq!(id, fixture.id);
                assert_eq!(
                    source.metadata().unwrap().ino(),
                    fs::metadata(&fixture.path).unwrap().ino()
                );
                if failure {
                    return Err(io::Error::other("fixture spawn refusal"));
                }
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "deployment::tests::installation_child",
                        "--ignored",
                    ])
                    .stdin(Stdio::from(source))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            })
            .unwrap();
        installation.cancel("screen closed after commit").unwrap();
        assert!(installation
            .commit_with(&request, |_, _| {
                calls += 1;
                Err(io::Error::other("replay"))
            })
            .is_err());
        assert_eq!(calls, 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        let event = loop {
            let event = installation.poll().unwrap();
            if !matches!(event, Event::Waiting) {
                break event;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(matches!(event, Event::Complete), !failure);
        assert!(installation.child.is_none() && installation.source.is_none());
    }
}
