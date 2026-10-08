#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::backoff::tests::Scratch;
use crate::backoff::Entry;
pub(crate) use crate::login_tier::tests::marker;
use crate::set_hostname::tests::granted;
use std::sync::atomic::{AtomicU64, Ordering};

/// A requester's built deployment and the update queue's backoff, each in
/// a private directory removed on drop.
pub(crate) struct Fixture {
    path: PathBuf,
    owner: u32,
    id: String,
    scratch: Scratch,
}
impl Fixture {
    fn directory() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-install-intake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    pub(crate) fn new() -> Self {
        let path = Self::directory();
        fs::write(path.join("manifest"), b"approved manifest").unwrap();
        Self {
            owner: fs::metadata(&path).unwrap().uid(),
            path,
            id: crate::sha256::hex_digest(b"approved manifest"),
            scratch: Scratch::new(),
        }
    }
    /// A built deployment whose initramfs carries `tier` as its marker.
    pub(crate) fn marked(tier: Option<&[u8]>) -> Self {
        use crate::login_tier::tests::{bundle, initramfs};
        let path = Self::directory();
        let id = bundle(&path, &initramfs(tier));
        Self {
            owner: fs::metadata(&path).unwrap().uid(),
            path,
            id,
            scratch: Scratch::new(),
        }
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
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
    /// The update queue's row of this fixture's backoff.
    pub(crate) fn backoff(&self) -> Backoff {
        self.scratch.backoff_for(Row::Update)
    }
    /// This fixture's update described under a fresh nonce and key.
    fn installation(&self) -> Installation {
        Installation::start(1000, self.ready(), self.backoff()).unwrap()
    }
    /// The backoff's refusal lapsed, its count kept.
    pub(crate) fn lapse(&self) {
        let count = self.backoff().read().unwrap().count();
        if count != 0 {
            self.scratch.write(
                format!(
                    "td-authd-backoff-v1\nupdate\t{count}\t{}\n",
                    backoff::now().saturating_sub(1)
                )
                .as_bytes(),
            );
        }
    }
    /// An intake of this fixture's owner listening in its directory,
    /// whose table grants the owner.
    fn intake(&self) -> Intake {
        self.intake_with(granted)
    }
    /// An intake of this fixture's owner reading `table`.
    fn intake_with(&self, table: fn() -> Result<Table, String>) -> Intake {
        let socket = self.path.join("intake");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let metadata = fs::symlink_metadata(&socket).unwrap();
        Intake {
            listener,
            owner: self.owner,
            backoff: self.backoff(),
            table,
            pending: None,
            in_flight: false,
            identity: (metadata.dev(), metadata.ino()),
        }
    }
    /// A requester connected to `intake` that sends `frame`, and root's
    /// answer: admission byte 02, a refusal's 03 or 04, or none when root
    /// closed the connection.
    fn submit(&self, intake: &mut Intake, frame: &[u8]) -> (UnixStream, Option<u8>) {
        let mut client = UnixStream::connect(self.path.join("intake")).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        for _ in 0..4 {
            intake.tick();
        }
        let mut greeting = [0; 8];
        if client.read_exact(&mut greeting).is_err() {
            return (client, None);
        }
        assert_eq!(&greeting, GREETING);
        client.write_all(frame).unwrap();
        for _ in 0..8 {
            intake.tick();
        }
        let mut answer = [0];
        let answer = client.read_exact(&mut answer).ok().map(|()| answer[0]);
        (client, answer)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An intake holding `fixture`'s update, admitted and acknowledged as
/// the real transport does, and its requester's end.
pub(crate) fn queued(fixture: &Fixture) -> (Intake, UnixStream) {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut pending = Pending::new(server, fixture.owner).unwrap();
    let backoff = fixture.backoff();
    let admission = || {
        let backoff = backoff.clone();
        move |body: &[u8]| admit(&backoff, granted, fixture.owner, body)
    };
    pending.poll(admission()).unwrap();
    let mut greeting = [0; 8];
    client.read_exact(&mut greeting).unwrap();
    client.write_all(&fixture.frame()).unwrap();
    for _ in 0..8 {
        pending.poll(admission()).unwrap();
    }
    assert!(pending.acknowledged);
    let mut admitted = [0];
    client.read_exact(&mut admitted).unwrap();
    assert_eq!(admitted, [ADMITTED]);
    let socket = fixture.path.join("intake");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let metadata = fs::symlink_metadata(&socket).unwrap();
    let intake = Intake {
        listener,
        owner: fixture.owner,
        backoff,
        table: granted,
        pending: Some(pending),
        in_flight: false,
        identity: (metadata.dev(), metadata.ino()),
    };
    (intake, client)
}

/// An admission a test's frame never reaches.
fn unasked(_: &[u8]) -> Result<Ready, Refused> {
    panic!("admission asked")
}

/// Gives `intake`'s queued update's marker read `give_up` in place of
/// `MARKER_GIVE_UP`.
pub(crate) fn hurry(intake: &mut Intake, give_up: Duration) {
    let pending = intake.pending.as_mut().unwrap();
    pending.ready.as_mut().unwrap().give_up = give_up;
}

#[test]
fn a_queued_update_reads_its_marker_through_the_held_directory() {
    use crate::login_tier::tests::{bundle, fifo, initramfs, marker};
    let fixture = Fixture::marked(Some(&marker(&[1, 2])));
    let ready = fixture.ready();
    assert_eq!(ready.reads(), [1, 2]);
    // Read afresh at selection, against the admitted ID, not the path.
    bundle(&fixture.path, &initramfs(Some(&marker(&[1]))));
    assert_eq!(ready.reads(), [] as [u8; 0]);
    let swapped = Fixture::marked(None);
    assert_eq!(swapped.ready().reads(), [] as [u8; 0]);
    // The requester's files only, never a FIFO or a file past its bound.
    let fixture = Fixture::marked(Some(&marker(&[1])));
    let ready = fixture.ready();
    assert_eq!(ready.reads(), [1]);
    let foreign = Ready {
        source: ready.source.try_clone().unwrap(),
        deployment: ready.deployment.clone(),
        owner: ready.owner.wrapping_add(1),
        give_up: ready.give_up,
    };
    assert_eq!(ready.give_up, MARKER_GIVE_UP);
    assert_eq!(foreign.reads(), [] as [u8; 0]);
    let archive = fixture.path.join("initramfs.cpio");
    let bytes = fs::read(&archive).unwrap();
    fs::remove_file(&archive).unwrap();
    if fifo(&archive) {
        assert_eq!(ready.reads(), [] as [u8; 0]);
        fs::remove_file(&archive).unwrap();
    }
    File::create(&archive)
        .unwrap()
        .set_len(crate::login_tier::ARCHIVE_LIMIT + 1)
        .unwrap();
    assert_eq!(ready.reads(), [] as [u8; 0]);
    fs::remove_file(&archive).unwrap();
    std::os::unix::fs::symlink(fixture.path.join("elsewhere"), &archive).unwrap();
    fs::write(fixture.path.join("elsewhere"), &bytes).unwrap();
    assert_eq!(ready.reads(), [] as [u8; 0]);
    fs::remove_file(&archive).unwrap();
    fs::write(&archive, &bytes).unwrap();
    assert_eq!(ready.reads(), [1]);
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
        fixture.lapse();
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut pending = Pending::new(server, fixture.owner).unwrap();
        pending.poll(unasked).unwrap();
        let mut greeting = [0; 8];
        client.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, GREETING);
        let backoff = fixture.backoff();
        for byte in fixture.frame() {
            client.write_all(&[byte]).unwrap();
            pending
                .poll(|body| admit(&backoff, granted, fixture.owner, body))
                .unwrap();
        }
        for _ in 0..3 {
            pending.poll(unasked).unwrap();
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
        while pending.poll(unasked).is_ok() {
            assert!(
                Instant::now() < deadline,
                "requester departure or extra traffic not observed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut pending = Pending::new(server, fixture.owner).unwrap();
    pending.poll(unasked).unwrap();
    let file = File::open(fixture.path.join("manifest")).unwrap();
    sys::send_descriptor(&client, &fixture.frame(), &file).unwrap();
    assert!(pending.poll(unasked).is_err());
    let mut greeting = [0; 8];
    client.read_exact(&mut greeting).unwrap();
}

#[test]
fn installation_needs_exact_presentation_then_a_single_commit() {
    let fixture = Fixture::new();
    let mut installation = fixture.installation();
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
    let mut expired = fixture.installation();
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
    pending.poll(unasked).unwrap();
    client.write_all(&[0, 65]).unwrap();
    pending.poll(unasked).unwrap();
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
        if let Err(error) = pending.poll(unasked) {
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
        let mut installation = fixture.installation();
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

/// Tag 5 carries a fresh approval key, each digit 2 to 9, beside a fresh
/// nonce; L4's keyless description is gone.
#[test]
fn each_installation_draws_its_own_nonce_and_key() {
    let fixture = Fixture::new();
    let mut keys = std::collections::BTreeSet::new();
    let mut nonces = std::collections::BTreeSet::new();
    for _ in 0..64 {
        let installation = fixture.installation();
        let request = installation.request();
        let Description::Install {
            key,
            deployment,
            requester,
        } = request.operation()
        else {
            panic!("not an installation");
        };
        assert_eq!((deployment.as_str(), *requester), (fixture.id(), 1000));
        assert!(key
            .digits()
            .iter()
            .all(|digit| (b'2'..=b'9').contains(digit)));
        keys.insert(key.digits());
        nonces.insert(*request.nonce());
    }
    assert_eq!(nonces.len(), 64);
    assert!(keys.len() > 1, "the key never varied");
    for (bytes, key) in [([0, 7], *b"29"), ([8, 255], *b"29"), ([3, 4], *b"56")] {
        assert_eq!(approval_key(bytes).unwrap().digits(), key);
    }
}

/// The intake refuses within the backoff with 03 and writes nothing;
/// otherwise it counts each admitted request, synced, before its 02, and a
/// source or manifest that does not check is refused with 04 uncounted.
#[test]
fn the_update_queue_counts_each_admission_and_refuses_while_backing_off() {
    let fixture = Fixture::new();
    let mut intake = fixture.intake();
    let (first, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(ADMITTED));
    let counted = fixture.backoff().read().unwrap();
    assert_eq!(counted.count(), 1);
    assert!(counted.refuses(backoff::now()));
    // One at a time: a second connection is not served while one waits.
    let (_second, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, None);
    drop(first);
    let until = Instant::now() + Duration::from_secs(5);
    while intake.pending.is_some() {
        intake.tick();
        assert!(Instant::now() < until, "requester departure not observed");
        std::thread::sleep(Duration::from_millis(1));
    }
    // Ended unapproved: the refusal stands, says so, and writes nothing.
    let before = fs::read(fixture.scratch.file()).unwrap();
    let (_refused, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(BACKING_OFF));
    assert_eq!(fs::read(fixture.scratch.file()).unwrap(), before);
    assert!(intake.pending.is_none());
    // Lapsed: a manifest that does not match is refused uncounted.
    fixture.lapse();
    let mut wrong = fixture.frame();
    let last = wrong.len() - fixture.path.as_os_str().len() - 1;
    wrong[last] ^= 1;
    let (_wrong, answer) = fixture.submit(&mut intake, &wrong);
    assert_eq!(answer, Some(REFUSED));
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    // A malformed frame is closed without a reply.
    let (_malformed, answer) = fixture.submit(&mut intake, &[0, 1, b'a']);
    assert_eq!(answer, None);
    // The next admission doubles the delay past the request's life.
    let now = backoff::now();
    let (_third, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(ADMITTED));
    let counted = fixture.backoff().read().unwrap();
    assert_eq!(counted.count(), 2);
    assert!(counted.refuses(now + 180 + 59) && !counted.refuses(now + 180 + 61));
}

/// A table that does not grant the requester `deploy-publish`, or none
/// at all, refuses the update at admission with 04, uncounted, as the
/// hostname intake does; the owner's row admits it.
#[test]
fn the_update_queue_admits_only_a_requester_the_table_grants() {
    fn other() -> Result<Table, String> {
        Table::parse("td-elevation-v1\n1000\tdeploy-rollback\tset-hostname\n")
    }
    fn missing() -> Result<Table, String> {
        Err("no table".into())
    }
    let fixture = Fixture::new();
    for table in [other as fn() -> Result<Table, String>, missing] {
        let mut intake = fixture.intake_with(table);
        let (_client, answer) = fixture.submit(&mut intake, &fixture.frame());
        assert_eq!(answer, Some(REFUSED));
        assert!(intake.pending.is_none());
        assert!(!fixture.scratch.file().exists());
    }
    let mut intake = fixture.intake_with(granted);
    let (_client, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(ADMITTED));
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
}

/// A backoff root cannot read refuses with 04 and says so; nothing
/// replaces it. One it cannot write refuses too, until a write succeeds.
#[test]
fn an_unusable_backoff_refuses_the_update_queue() {
    let fixture = Fixture::new();
    let mut intake = fixture.intake();
    fixture
        .scratch
        .write(b"td-authd-backoff-v1\nupdate\tlots\t5\n");
    let (_client, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(REFUSED));
    assert_eq!(
        fs::read(fixture.scratch.file()).unwrap(),
        b"td-authd-backoff-v1\nupdate\tlots\t5\n"
    );
    fs::remove_file(fixture.scratch.file()).unwrap();
    fs::create_dir(fixture.scratch.path.join("authd").join("backoff.new")).unwrap();
    let (_client, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(REFUSED));
    assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
    fs::remove_dir(fixture.scratch.path.join("authd").join("backoff.new")).unwrap();
    let (_client, answer) = fixture.submit(&mut intake, &fixture.frame());
    assert_eq!(answer, Some(ADMITTED));
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
}

/// Only an approval clears the queue's count: a cancelled or expired
/// installation leaves it, a commit clears it before anything starts, and
/// a commit whose clear fails starts nothing.
#[test]
fn only_a_commit_clears_the_update_backoff() {
    let fixture = Fixture::new();
    let counted = fixture
        .backoff()
        .admitted(Entry::default(), backoff::now())
        .unwrap();
    let mut cancelled = fixture.installation();
    let request = cancelled.request().clone();
    cancelled.presented(&request).unwrap();
    cancelled.cancel("physical Escape").unwrap();
    assert_eq!(fixture.backoff().read().unwrap(), counted);
    let mut approved = fixture.installation();
    let request = approved.request().clone();
    approved.presented(&request).unwrap();
    approved
        .commit_with(&request, |_, _| {
            assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
            Err(io::Error::other("fixture spawn refusal"))
        })
        .unwrap();
    assert!(matches!(approved.poll().unwrap(), Event::Failed(_)));
    fixture
        .backoff()
        .admitted(Entry::default(), backoff::now())
        .unwrap();
    fs::create_dir(fixture.scratch.path.join("authd").join("backoff.new")).unwrap();
    let mut unwritable = fixture.installation();
    let request = unwritable.request().clone();
    unwritable.presented(&request).unwrap();
    unwritable
        .commit_with(&request, |_, _| panic!("started without clearing"))
        .unwrap();
    assert!(matches!(unwritable.poll().unwrap(), Event::Failed(_)));
    assert!(unwritable.source.is_none() && unwritable.child.is_none());
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
}
