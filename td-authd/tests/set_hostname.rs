#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::backoff::tests::Scratch;
use std::fs;
use std::os::unix::fs::PermissionsExt;

/// A private directory holding the saved name `saved`, root's place for it
/// in `@var`, and the backoff's directory beside it; removed on drop.
pub(crate) struct Fixture {
    scratch: Scratch,
}

/// The table granting this test's UID, the requester's.
pub(crate) fn granted() -> Result<Table, String> {
    Ok(crate::elevation::tests::granting(
        fs::metadata("/proc/self").unwrap().uid(),
    ))
}

fn refused() -> Result<Table, String> {
    Table::parse("td-elevation-v1\n1000\tdeploy-rollback\n")
}

fn missing() -> Result<Table, String> {
    Err("no table".into())
}

impl Fixture {
    pub(crate) fn new(saved: &str) -> Self {
        let fixture = Self {
            scratch: Scratch::new(),
        };
        fixture.save(format!("{saved}\n").as_bytes());
        fixture
    }

    pub(crate) fn save(&self, bytes: &[u8]) {
        let path = self.saved_path();
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn saved_path(&self) -> PathBuf {
        self.scratch.path.join("hostname")
    }

    pub(crate) fn saved(&self) -> Vec<u8> {
        fs::read(self.saved_path()).unwrap()
    }

    pub(crate) fn backoff(&self) -> Backoff {
        self.scratch.backoff()
    }

    pub(crate) fn uid(&self) -> u32 {
        self.scratch.owner.0
    }

    fn places(&self, table: fn() -> Result<Table, String>) -> Places {
        Places {
            saved: self.saved_path(),
            owner: self.scratch.owner.0,
            backoff: self.scratch.backoff(),
            table,
        }
    }

    /// An intake of this fixture's owner listening in its directory.
    pub(crate) fn intake(&self, table: fn() -> Result<Table, String>) -> Intake {
        self.intake_for(self.uid(), table)
    }

    fn intake_for(&self, owner: u32, table: fn() -> Result<Table, String>) -> Intake {
        let socket = self.scratch.path.join("intake");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let metadata = fs::symlink_metadata(&socket).unwrap();
        let places = self.places(table);
        let status = Status {
            entry: places.backoff.read(),
            unwritten: None,
        };
        Intake {
            listener,
            owner,
            places,
            pending: None,
            in_flight: false,
            identity: (metadata.dev(), metadata.ino()),
            status,
        }
    }

    /// A requester connected to `intake` that sends `frame`, and root's
    /// answer: admission byte 02, a refusal's 03 or 04, or none when root
    /// closed the connection.
    pub(crate) fn submit(&self, intake: &mut Intake, frame: &[u8]) -> (UnixStream, Option<u8>) {
        let mut client = UnixStream::connect(self.scratch.path.join("intake")).unwrap();
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

fn frame(name: &str) -> Vec<u8> {
    [&[u8::try_from(name.len()).unwrap()][..], name.as_bytes()].concat()
}

fn queued(fixture: &Fixture, name: &str) -> (Intake, UnixStream) {
    let mut intake = fixture.intake(granted);
    let (client, answer) = fixture.submit(&mut intake, &frame(name));
    assert_eq!(answer, Some(ADMITTED));
    (intake, client)
}

/// `name` admitted at `intake`, its requester then standing as UID
/// `requester`: the session's owner 1000, when a test runs as another UID.
pub(crate) fn submit_as(
    fixture: &Fixture,
    intake: &mut Intake,
    name: &str,
    requester: u32,
) -> UnixStream {
    let (client, answer) = fixture.submit(intake, &frame(name));
    assert_eq!(answer, Some(ADMITTED));
    let peer = intake
        .pending
        .as_mut()
        .and_then(|pending| pending.peer.as_mut())
        .unwrap();
    peer.credentials.uid = requester;
    client
}

impl Fixture {
    /// The backoff's refusal over, its count kept.
    pub(crate) fn lapse(&self) {
        let count = self.backoff().read().unwrap().count();
        self.scratch.write(
            format!(
                "td-authd-backoff-v1\nhostname\t{count}\t{}\n",
                backoff::now().saturating_sub(1)
            )
            .as_bytes(),
        );
    }
}

/// The intake admits only its owner, UID 1000 in production: a peer of
/// another UID is closed before the greeting.
#[test]
fn a_requester_of_another_uid_is_refused() {
    let fixture = Fixture::new("td");
    assert!(Intake::bind(1001).is_err());
    assert!(Intake::bind(0).is_err());
    let (server, _client) = UnixStream::pair().unwrap();
    assert_eq!(
        Pending::new(server, fixture.uid().wrapping_add(1))
            .err()
            .unwrap()
            .to_string(),
        "hostname requester has the wrong UID"
    );
    let mut intake = fixture.intake_for(fixture.uid().wrapping_add(1), granted);
    let (_client, answer) = fixture.submit(&mut intake, &frame("my-laptop"));
    assert_eq!(answer, None);
    assert!(intake.pending.is_none());
    assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
}

/// Every frame `Hostname::parse` does not admit, or that is not one
/// length byte and that many bytes, is refused without admission.
#[test]
fn a_malformed_name_is_refused_at_the_intake() {
    let fixture = Fixture::new("td");
    for bad in [
        vec![0],
        [&[64][..], &[b'a'; 64]].concat(),
        frame("UPPER"),
        frame("my laptop"),
        frame("1host"),
        frame("host-"),
        frame("a..b"),
        frame("a/b"),
        vec![2, 0xc3, 0xa9],
        vec![3, b'a', 0, b'b'],
    ] {
        let mut intake = fixture.intake(granted);
        let (_client, answer) = fixture.submit(&mut intake, &bad);
        assert_eq!(answer, None, "{bad:?}");
        assert!(intake.pending.is_none());
        assert_eq!(intake.state(), [0x9f, 0, 0]);
    }
    // The saved name itself is no change.
    let mut intake = fixture.intake(granted);
    assert_eq!(fixture.submit(&mut intake, &frame("td")).1, Some(REFUSED));
    // Nor is a name when nothing is saved, or the saved name is malformed.
    for saved in [None, Some(&b"UPPER\n"[..])] {
        let _ = fs::remove_file(fixture.saved_path());
        if let Some(bytes) = saved {
            fixture.save(bytes);
        }
        let mut intake = fixture.intake(granted);
        assert_eq!(
            fixture.submit(&mut intake, &frame("my-laptop")).1,
            Some(REFUSED)
        );
    }
    // Refusals at the intake count nothing.
    assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
}

#[test]
fn a_requester_the_table_does_not_grant_is_refused() {
    let fixture = Fixture::new("td");
    for table in [refused as fn() -> Result<Table, String>, missing] {
        let mut intake = fixture.intake(table);
        assert_eq!(
            fixture.submit(&mut intake, &frame("my-laptop")).1,
            Some(REFUSED)
        );
    }
    assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
    let mut intake = fixture.intake(granted);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(ADMITTED)
    );
}

/// One queued request for the whole intake: a second requester is not
/// served while one waits, and none while one is under review.
#[test]
fn one_request_waits_at_a_time_and_the_state_says_so() {
    let fixture = Fixture::new("td");
    let (mut intake, _first) = queued(&fixture, "my-laptop");
    assert_eq!(intake.state(), [0x9f, 1, 0]);
    let (_second, answer) = fixture.submit(&mut intake, &frame("other"));
    assert_eq!(answer, None);
    let ready = intake.select().unwrap();
    assert_eq!(ready.name.name(), "my-laptop");
    assert_eq!(ready.requester(), fixture.uid());
    assert!(intake.selected_alive());
    assert_eq!(intake.state(), [0x9f, 0, 0]);
    assert!(intake.select().is_err());
    let (_third, answer) = fixture.submit(&mut intake, &frame("other"));
    assert_eq!(answer, None);
    intake.finish(false);
    assert!(!intake.selected_alive());
    assert_eq!(intake.state(), [0x9f, 0, 1]);
    fixture.lapse();
    let (_fourth, answer) = fixture.submit(&mut intake, &frame("other"));
    assert_eq!(answer, Some(ADMITTED));
    assert_eq!(intake.state(), [0x9f, 1, 1]);
}

/// Within the backoff a request is refused; past it, admitted. A backoff
/// root cannot read refuses the intake and the state says so.
#[test]
fn the_backoff_refuses_and_an_unreadable_one_refuses_the_intake() {
    let fixture = Fixture::new("td");
    let now = backoff::now();
    fixture.scratch.write(
        format!(
            "td-authd-backoff-v1\nhostname\t2\t{}\n",
            now.saturating_add(600)
        )
        .as_bytes(),
    );
    let mut intake = fixture.intake(granted);
    assert_eq!(intake.state(), [0x9f, 0, 2]);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(BACKING_OFF)
    );
    assert_eq!(intake.state(), [0x9f, 0, 2]);
    // The refusal changed nothing.
    assert_eq!(fixture.backoff().read().unwrap().count(), 2);
    fixture.scratch.write(
        format!(
            "td-authd-backoff-v1\nhostname\t2\t{}\n",
            now.saturating_sub(1)
        )
        .as_bytes(),
    );
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(ADMITTED)
    );
    // Counted at admission; the menu's count leaves it out while it waits.
    assert_eq!(fixture.backoff().read().unwrap().count(), 3);
    assert_eq!(intake.state(), [0x9f, 1, 2]);
    // Malformed: refused, and the state names it.
    let fixture = Fixture::new("td");
    fixture.scratch.write(b"td-authd-backoff-v1\nhostname\t2\n");
    let mut intake = fixture.intake(granted);
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(REFUSED)
    );
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    fixture.scratch.write(b"td-authd-backoff-v1\n");
    fs::set_permissions(fixture.scratch.file(), fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(REFUSED)
    );
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    // Repaired: `1f` reads it again, and the intake admits.
    fs::set_permissions(fixture.scratch.file(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(intake.state(), [0x9f, 0, 0]);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(ADMITTED)
    );
}

/// Every admitted request counts at admission, its refusal running past
/// its 180-second life; one that ends unselected, by expiry or its
/// requester's departure, writes nothing more. A refusal counts nothing.
#[test]
fn an_admitted_request_counts_at_admission_however_it_ends() {
    let fixture = Fixture::new("td");
    let before = backoff::now();
    let (mut intake, client) = queued(&fixture, "my-laptop");
    let entry = fixture.backoff().read().unwrap();
    assert_eq!(entry.count(), 1);
    assert!(entry.refuses(before.saturating_add(209)));
    assert!(!entry.refuses(backoff::now().saturating_add(211)));
    intake.pending.as_mut().unwrap().deadline = Some(Instant::now());
    intake.tick();
    assert!(intake.pending.is_none());
    assert_eq!(fixture.backoff().read().unwrap(), entry);
    assert_eq!(intake.state(), [0x9f, 0, 1]);
    drop(client);
    // Departure.
    let fixture = Fixture::new("td");
    let (mut intake, client) = queued(&fixture, "my-laptop");
    drop(client);
    let until = Instant::now() + Duration::from_secs(2);
    while intake.pending.is_some() {
        assert!(Instant::now() < until);
        intake.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    // A request refused before admission counts nothing.
    let fixture = Fixture::new("td");
    let mut intake = fixture.intake(granted);
    let (client, _) = fixture.submit(&mut intake, &frame("td"));
    drop(client);
    intake.tick();
    assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
}

/// A selected request's description: tag 12, the saved name beside the
/// new one, its requester, a fresh key; the backoff, which counted it at
/// admission, is not written again.
#[test]
fn the_description_names_both_names_and_writes_nothing() {
    let fixture = Fixture::new("td");
    let (mut intake, _client) = queued(&fixture, "my-laptop");
    let admitted = fixture.backoff().read().unwrap();
    let ready = intake.select().unwrap();
    let change = intake.describe(fixture.uid(), ready).unwrap();
    assert_eq!(fixture.backoff().read().unwrap(), admitted);
    assert_eq!(admitted.count(), 1);
    let Operation::SetHostname {
        key,
        requester,
        old,
        new,
    } = change.request().operation()
    else {
        panic!("not a hostname change");
    };
    assert_eq!((old.as_str(), new.as_str()), ("td", "my-laptop"));
    assert_eq!(*requester, fixture.uid());
    assert!(key
        .digits()
        .iter()
        .all(|digit| (b'2'..=b'9').contains(digit)));
    assert_eq!(change.request().owner(), fixture.uid());
}

fn drawn(fixture: &Fixture) -> Change {
    let places = fixture.places(granted);
    let ready = Ready {
        name: Hostname::parse("my-laptop").unwrap(),
        requester: fixture.uid(),
    };
    Change::drawn(
        fixture.uid(),
        ready,
        Hostname::parse("td").unwrap(),
        &places,
        [9; 32],
        [0, 7],
    )
    .unwrap()
}

#[test]
fn each_digit_is_two_plus_its_own_byte_modulo_eight() {
    for byte in 0..=u8::MAX {
        let key = approval_key([byte, byte.wrapping_add(5)]).unwrap();
        assert_eq!(
            key.digits(),
            [b'2' + byte % 8, b'2' + byte.wrapping_add(5) % 8]
        );
    }
}

/// Presentation, then one commit, which saves the new name canonically
/// and clears the backoff; a retry or replay is refused.
#[test]
fn the_commit_saves_the_new_name_once_and_clears_the_backoff() {
    let fixture = Fixture::new("td");
    fixture
        .backoff()
        .admitted(Entry::default(), backoff::now())
        .unwrap();
    let mut change = drawn(&fixture);
    let request = change.request().clone();
    let Operation::SetHostname { key, .. } = request.operation() else {
        panic!("not a hostname change");
    };
    assert_eq!(key.digits(), *b"29");
    assert!(change.commit(&request).is_err());
    assert_eq!(fixture.saved(), b"td\n");
    let wrong = Request::new([8; 32], fixture.uid(), request.operation().clone()).unwrap();
    assert!(change.presented(&wrong).is_err());
    change.presented(&request).unwrap();
    assert!(change.presented(&request).is_err());
    assert!(matches!(change.poll().unwrap(), Event::Commit(_)));
    assert!(change.commit(&wrong).is_err());
    change.commit(&request).unwrap();
    assert!(change.commit(&request).is_err());
    assert!(matches!(change.poll().unwrap(), Event::Complete));
    assert_eq!(fixture.saved(), b"my-laptop\n");
    let metadata = fs::metadata(fixture.saved_path()).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o644);
    assert_eq!(metadata.uid(), fixture.uid());
    assert_eq!(fixture.backoff().read().unwrap(), Entry::default());
}

/// Escape, expiry, or the person's wrong digit, which the compositor sends
/// as a cancellation, write nothing: the saved name stays and the count
/// written at admission remains.
#[test]
fn an_unapproved_change_writes_nothing() {
    let fixture = Fixture::new("td");
    fixture
        .backoff()
        .admitted(Entry::default(), backoff::now())
        .unwrap();
    let mut cancelled = drawn(&fixture);
    let request = cancelled.request().clone();
    cancelled.presented(&request).unwrap();
    cancelled.cancel().unwrap();
    assert!(cancelled.commit(&request).is_err());
    assert!(matches!(cancelled.poll().unwrap(), Event::Failed(_)));
    let mut expired = drawn(&fixture);
    expired.deadline = Instant::now();
    assert!(expired.presented(&expired.request().clone()).is_err());
    assert!(matches!(expired.poll().unwrap(), Event::Failed(_)));
    assert_eq!(fixture.saved(), b"td\n");
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
}

/// The saved name is read again at commit: one changed since it was shown
/// is not replaced.
#[test]
fn a_saved_name_changed_since_the_prompt_is_not_replaced() {
    let fixture = Fixture::new("td");
    let mut change = drawn(&fixture);
    let request = change.request().clone();
    change.presented(&request).unwrap();
    fixture.save(b"elsewhere\n");
    change.commit(&request).unwrap();
    assert!(matches!(change.poll().unwrap(), Event::Failed(_)));
    assert_eq!(fixture.saved(), b"elsewhere\n");
    let fixture = Fixture::new("td");
    let mut change = drawn(&fixture);
    let request = change.request().clone();
    change.presented(&request).unwrap();
    fs::set_permissions(fixture.saved_path(), fs::Permissions::from_mode(0o666)).unwrap();
    change.commit(&request).unwrap();
    assert!(matches!(change.poll().unwrap(), Event::Failed(_)));
    assert_eq!(fixture.saved(), b"td\n");
}

/// A request counts when it is admitted: a generation torn down while it
/// waits, before selection or expiry, leaves the next generation's intake
/// refusing and its count kept.
#[test]
fn a_teardown_before_selection_still_counts_the_request() {
    let fixture = Fixture::new("td");
    let (intake, client) = queued(&fixture, "my-laptop");
    drop(intake);
    drop(client);
    let mut next = fixture.intake(granted);
    assert_eq!(next.state(), [0x9f, 0, 1]);
    assert_eq!(
        fixture.submit(&mut next, &frame("my-laptop")).1,
        Some(BACKING_OFF)
    );
}

/// While the backoff cannot be written nothing is admitted and `1f` says
/// so, a request's end and a successful read in between included; the
/// first write that succeeds admits again.
#[test]
fn a_backoff_root_cannot_write_refuses_until_a_write_succeeds() {
    let fixture = Fixture::new("td");
    fixture.scratch.write(b"td-authd-backoff-v1\n");
    // A directory where the write's temporary goes: reads succeed, writes
    // fail.
    let blocker = fixture.scratch.path.join("authd").join("backoff.new");
    fs::create_dir(&blocker).unwrap();
    let mut intake = fixture.intake(granted);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(REFUSED)
    );
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    intake.finish(false);
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    // A frame refused before any write leaves the failure standing.
    assert_eq!(fixture.submit(&mut intake, &frame("td")).1, Some(REFUSED));
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(REFUSED)
    );
    assert_eq!(intake.state(), [0x9f, 2, 0]);
    fs::remove_dir(&blocker).unwrap();
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(ADMITTED)
    );
    assert_eq!(intake.state(), [0x9f, 1, 0]);
}

/// A clock set back leaves a deadline far ahead: the intake cuts it to the
/// longest one admission could cause, 180 seconds and then 960, and
/// writes that back, so the refusal ends.
#[test]
fn a_clock_set_back_refuses_no_longer_than_the_bound() {
    let fixture = Fixture::new("td");
    let now = backoff::now();
    fixture.scratch.write(
        format!(
            "td-authd-backoff-v1\nhostname\t2\t{}\n",
            now.saturating_add(100_000)
        )
        .as_bytes(),
    );
    let mut intake = fixture.intake(granted);
    assert_eq!(
        fixture.submit(&mut intake, &frame("my-laptop")).1,
        Some(BACKING_OFF)
    );
    let text = fs::read_to_string(fixture.scratch.file()).unwrap();
    let until: u64 = text
        .trim_end()
        .rsplit('\t')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(until <= backoff::now().saturating_add(180 + 960), "{text}");
    assert!(until >= now.saturating_add(180 + 960), "{text}");
    assert!(
        text.starts_with("td-authd-backoff-v1\nhostname\t2\t"),
        "{text}"
    );
}

#[test]
fn the_client_refuses_a_malformed_name_before_connecting() {
    for name in ["", "UPPER", "a b", &"a".repeat(64)] {
        assert!(request(name).is_err());
    }
    assert_eq!(Refusal::Nothing.answer(), [0x9e, 0]);
    assert_eq!(Refusal::Principal.answer(), [0x9e, 1]);
}
