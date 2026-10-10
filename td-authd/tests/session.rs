#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::consent::Operation;
use crate::login::{Login, Pin, Selection};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::thread;

fn fixture(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &format!("session::tests::{name}"), "--ignored"])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn description() -> Description {
    Description::new(
        [42; 32],
        1000,
        Operation::Unlock {
            role: Role::Recovery,
        },
    )
    .unwrap()
}

fn unexpected_begin(_: u32, _: Start) -> Result<Unlock, String> {
    panic!("operation began before admission")
}

/// A session whose revocation check never runs: the tests that are not
/// about revocation (`revoking` below is).
fn quiet() -> Session {
    let mut session = Session::new(1000, "tester").unwrap();
    session.revocation = None;
    session
}

fn prepare(session: &mut Session) {
    assert_eq!(
        session
            .answer_with(
                Request::Prepare,
                |_| fixture("cleanup_child"),
                unexpected_begin
            )
            .unwrap(),
        [0x90]
    );
    let until = Instant::now() + Duration::from_secs(3);
    while session.answer(Request::Poll).unwrap() != [0x91, 2] {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "exec-only generation cleanup stand-in"]
fn cleanup_child() {
    assert_eq!(std::env::vars().count(), 0);
    assert_eq!(std::env::current_dir().unwrap(), std::path::Path::new("/"));
}

#[test]
#[ignore = "exec-only cleanup failure"]
fn failing_cleanup_child() {
    std::process::exit(7);
}

#[test]
#[ignore = "exec-only hung cleanup"]
fn stalled_cleanup_child() {
    thread::sleep(Duration::from_secs(30));
}

fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
    let mut header = [0; 2];
    stream.read_exact(&mut header).unwrap();
    let mut bytes = vec![0; usize::from(u16::from_be_bytes(header))];
    stream.read_exact(&mut bytes).unwrap();
    bytes
}

fn send_frame(stream: &mut UnixStream, bytes: &[u8]) {
    stream
        .write_all(&(bytes.len() as u16).to_be_bytes())
        .unwrap();
    stream.write_all(bytes).unwrap();
}

#[test]
#[ignore = "exec-only bound unlock protocol peer"]
fn unlock_child() {
    let mut stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let request = read_frame(&mut stream);
    assert_eq!(request, description().encode());
    for tag in [0x10, 0x12] {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&[tag; 32]);
        bytes.extend_from_slice(&request);
        send_frame(&mut stream, &bytes);
        bytes[0] += 1;
        assert_eq!(read_frame(&mut stream), bytes);
    }
    send_frame(&mut stream, &[0x14]);
}

#[test]
fn wire_parser_refuses_ambiguity_and_arbitrary_arguments() {
    for bytes in [
        vec![],
        vec![0x10, 0],
        vec![0x11, 0],
        vec![0x12],
        vec![0x12, 0],
        vec![0x12, 3],
        vec![0x12, 1, 0],
        vec![0x15],
        vec![0x16],
    ] {
        assert!(Request::decode(&bytes).is_err());
    }
    assert_eq!(
        Request::decode(&[0x12, 2]).unwrap(),
        Request::Begin(Role::Recovery)
    );
    for tag in [0x13, 0x14] {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&description().encode());
        let value = Request::decode(&bytes).unwrap();
        assert_eq!(
            value,
            if tag == 0x13 {
                Request::Presented(description())
            } else {
                Request::Commit(description())
            }
        );
        bytes.push(0);
        assert!(Request::decode(&bytes).is_err());
    }
    assert!(Request::decode(&[&[0x15][..], &[0; 32]].concat()).is_err());
    assert_eq!(
        Request::decode(&[&[0x15][..], &[42; 32]].concat()).unwrap(),
        Request::Cancel([42; 32])
    );
}

#[test]
fn cleanup_must_finish_before_any_operation_and_prepare_is_single_use() {
    assert!(Session::new(1001, "tester").is_err());
    let mut session = quiet();
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 0]);
    assert!(session
        .answer_with(
            Request::Begin(Role::Primary),
            cleanup_command,
            unexpected_begin
        )
        .is_err());
    prepare(&mut session);
    assert!(session.answer(Request::Prepare).is_err());
    assert!(session.answer(Request::Presented(description())).is_err());
    assert!(session.answer(Request::Commit(description())).is_err());
    assert!(session.answer(Request::Cancel([42; 32])).is_err());
}

#[test]
fn failed_or_stalled_cleanup_never_admits_an_operation() {
    for name in ["failing_cleanup_child", "stalled_cleanup_child"] {
        let mut session = quiet();
        session
            .answer_with(Request::Prepare, |_| fixture(name), unexpected_begin)
            .unwrap();
        assert!(session
            .answer_with(
                Request::Begin(Role::Primary),
                cleanup_command,
                unexpected_begin
            )
            .is_err());
        if name == "stalled_cleanup_child" {
            session.cleanup.as_mut().unwrap().deadline = Instant::now();
        }
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if session.tick().is_err() {
                break;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!session.prepared);
        assert!(session
            .answer_with(
                Request::Begin(Role::Primary),
                cleanup_command,
                unexpected_begin
            )
            .is_err());
    }
}

fn poll_until(session: &mut Session, status: u8) -> Vec<u8> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let response = session.answer(Request::Poll).unwrap();
        if response[1] == status {
            return response;
        }
        assert_eq!(response[1], 3);
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}

/// Request `1d` (td-authd/DESIGN.md, "Elevation operations"): a busy slot,
/// then the principal table, then the selectors, each refused with its
/// `9d` byte before any description or operation exists; admitted, the
/// description of the pair under a fresh nonce and key, holding the slot.
#[test]
fn rollback_admits_by_table_then_selectors_before_any_description() {
    use crate::elevation::tests::V1;
    use crate::elevation::Table;
    use crate::rollback::tests::volume;
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let pair = volume(&a, &b);
    let mut session = quiet();
    assert_eq!(Request::decode(&[0x1d]).unwrap(), Request::Rollback);
    assert!(Request::decode(&[0x1d, 0]).is_err());
    // Not yet prepared: the slot is not free.
    let unasked = || -> Result<Table, String> { panic!("table read while busy") };
    assert_eq!(
        session.begin_rollback(unasked, pair.path()).unwrap(),
        [0x9d, 0]
    );
    prepare(&mut session);
    // No row for the owner, an unreadable table, or a row without the
    // operation: refused before the selectors are read.
    for table in [
        Table::parse("td-elevation-v1\n1001\tdeploy-rollback\n"),
        Table::parse("td-elevation-v1\n1000\tset-hostname\n"),
        Err("no table".to_string()),
    ] {
        assert_eq!(
            session
                .begin_rollback(|| table, &pair.path().join("absent"))
                .unwrap(),
            [0x9d, 1]
        );
        assert!(session.operation.is_none());
    }
    // One deployment named twice, or no volume.
    let one = volume(&a, &a);
    assert_eq!(
        session
            .begin_rollback(|| Table::parse(V1), one.path())
            .unwrap(),
        [0x9d, 2]
    );
    assert_eq!(
        session
            .begin_rollback(|| Table::parse(V1), &pair.path().join("absent"))
            .unwrap(),
        [0x9d, 2]
    );
    assert!(session.operation.is_none());
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    // Admitted.
    let answer = session
        .begin_rollback(|| Table::parse(V1), pair.path())
        .unwrap();
    assert_eq!(answer[0], 0x92);
    let described = Description::decode(&answer[1..]).unwrap();
    let Operation::DeployRollback {
        key,
        current,
        previous,
    } = described.operation()
    else {
        panic!("not a rollback");
    };
    assert_eq!((current, previous), (&a, &b));
    assert!(key
        .digits()
        .iter()
        .all(|digit| (b'2'..=b'9').contains(digit)));
    assert_eq!(described.owner(), 1000);
    // The slot is held: another selection, of any kind, is busy.
    assert_eq!(
        session.begin_rollback(unasked, pair.path()).unwrap(),
        [0x9d, 0]
    );
    assert_eq!(session.answer(Request::Install).unwrap(), [0x99, 0]);
    // Presented, then invited to commit; Escape cancels and fails it.
    assert_eq!(&poll_until(&mut session, 4)[2..], described.encode());
    session
        .answer(Request::Presented(described.clone()))
        .unwrap();
    assert_eq!(&poll_until(&mut session, 5)[2..], described.encode());
    assert_eq!(
        session.answer(Request::Cancel(*described.nonce())).unwrap(),
        [0x95, 0]
    );
    assert!(session.answer(Request::Commit(described.clone())).is_err());
    assert_eq!(&poll_until(&mut session, 7)[2..], described.encode());
    assert!(session.operation.is_none());
    // A new selection draws a new nonce.
    let again = session
        .begin_rollback(|| Table::parse(V1), pair.path())
        .unwrap();
    assert_ne!(
        Description::decode(&again[1..]).unwrap().nonce(),
        described.nonce()
    );
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// Request `1e` (td-authd/DESIGN.md, "Elevation operations"): a busy slot,
/// then the principal table for the owner, then the queued request and the
/// table for its requester, each refused with its `9e` byte before any
/// description; admitted, tag 12, counted by the backoff before it is
/// answered. A wrong digit, which the compositor sends as a cancellation,
/// writes nothing; a new request's commit saves the name and clears the
/// count. `1f` says what waits and the count throughout.
#[test]
fn a_hostname_change_is_admitted_by_the_table_then_described_then_saved_once() {
    use crate::elevation::tests::V1;
    use crate::elevation::Table;
    use crate::set_hostname::tests::{submit_as, Fixture};
    let fixture = Fixture::new("td");
    let mut session = quiet();
    assert_eq!(Request::decode(&[0x1e]).unwrap(), Request::Hostname);
    assert_eq!(Request::decode(&[0x1f]).unwrap(), Request::HostnameState);
    assert!(Request::decode(&[0x1e, 0]).is_err());
    assert!(Request::decode(&[0x1f, 0]).is_err());
    let unasked = || -> Result<Table, String> { panic!("table read while busy") };
    assert_eq!(session.begin_hostname(unasked).unwrap(), [0x9e, 0]);
    prepare(&mut session);
    // No intake, as on a live boot: nothing waits.
    assert_eq!(
        session.answer(Request::HostnameState).unwrap(),
        [0x9f, 0, 0]
    );
    assert_eq!(
        session.begin_hostname(|| Table::parse(V1)).unwrap(),
        [0x9e, 0]
    );
    session.hostnames = Some(fixture.intake(crate::set_hostname::tests::granted));
    assert_eq!(
        session.begin_hostname(|| Table::parse(V1)).unwrap(),
        [0x9e, 0]
    );
    let intake = session.hostnames.as_mut().unwrap();
    let mut first = submit_as(&fixture, intake, "my-laptop", 1000);
    // Counted at admission; the menu's count leaves the waiting one out.
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    assert_eq!(
        session.answer(Request::HostnameState).unwrap(),
        [0x9f, 1, 0]
    );
    // A table that does not grant the owner refuses before the queue is
    // touched or the backoff written again.
    for table in [
        Table::parse("td-elevation-v1\n1000\tdeploy-rollback\n"),
        Table::parse("td-elevation-v1\n1001\tset-hostname\n"),
        Err("no table".to_string()),
    ] {
        assert_eq!(session.begin_hostname(|| table).unwrap(), [0x9e, 1]);
        assert!(session.operation.is_none());
        assert_eq!(
            session.answer(Request::HostnameState).unwrap(),
            [0x9f, 1, 0]
        );
    }
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    let answer = session.begin_hostname(|| Table::parse(V1)).unwrap();
    assert_eq!(answer[0], 0x92);
    let described = Description::decode(&answer[1..]).unwrap();
    let Operation::SetHostname {
        key,
        requester,
        old,
        new,
    } = described.operation()
    else {
        panic!("not a hostname change");
    };
    assert_eq!(
        (old.as_str(), new.as_str(), *requester),
        ("td", "my-laptop", 1000)
    );
    assert!(key
        .digits()
        .iter()
        .all(|digit| (b'2'..=b'9').contains(digit)));
    assert_eq!(described.owner(), 1000);
    // Counted once, at admission, and still open.
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    assert_eq!(
        session.answer(Request::HostnameState).unwrap(),
        [0x9f, 0, 0]
    );
    // The slot is held.
    assert_eq!(session.begin_hostname(unasked).unwrap(), [0x9e, 0]);
    assert_eq!(
        session
            .begin_rollback(unasked, std::path::Path::new("/absent"))
            .unwrap(),
        [0x9d, 0]
    );
    // A wrong digit ends it as Escape does: nothing is written, the count
    // stays, and the requester reads 00.
    assert_eq!(&poll_until(&mut session, 4)[2..], described.encode());
    session
        .answer(Request::Presented(described.clone()))
        .unwrap();
    assert_eq!(&poll_until(&mut session, 5)[2..], described.encode());
    assert_eq!(
        session.answer(Request::Cancel(*described.nonce())).unwrap(),
        [0x95, 0]
    );
    assert!(session.answer(Request::Commit(described.clone())).is_err());
    assert_eq!(&poll_until(&mut session, 7)[2..], described.encode());
    let mut byte = [9];
    first.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0]);
    assert_eq!(fixture.saved(), b"td\n");
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    assert_eq!(
        session.answer(Request::HostnameState).unwrap(),
        [0x9f, 0, 1]
    );
    // Within the backoff a new request is refused at the intake, which
    // says why.
    let intake = session.hostnames.as_mut().unwrap();
    assert_eq!(
        fixture
            .submit(intake, &[&[5][..], &b"other"[..]].concat())
            .1,
        Some(3)
    );
    // A requester the table does not grant: refused, and retired.
    fixture.lapse();
    let intake = session.hostnames.as_mut().unwrap();
    let mut stranger = submit_as(&fixture, intake, "other", 1002);
    assert_eq!(
        session.begin_hostname(|| Table::parse(V1)).unwrap(),
        [0x9e, 1]
    );
    stranger.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0]);
    assert_eq!(
        session.answer(Request::HostnameState).unwrap(),
        [0x9f, 0, 2]
    );
    // A new request's right key: one commit saves the name and clears the
    // count.
    fixture.lapse();
    let intake = session.hostnames.as_mut().unwrap();
    let mut second = submit_as(&fixture, intake, "my-laptop", 1000);
    let answer = session.begin_hostname(|| Table::parse(V1)).unwrap();
    let described = Description::decode(&answer[1..]).unwrap();
    assert_eq!(fixture.backoff().read().unwrap().count(), 3);
    assert_eq!(&poll_until(&mut session, 4)[2..], described.encode());
    session
        .answer(Request::Presented(described.clone()))
        .unwrap();
    assert_eq!(&poll_until(&mut session, 5)[2..], described.encode());
    assert_eq!(
        session.answer(Request::Commit(described.clone())).unwrap(),
        [0x94]
    );
    assert!(session.answer(Request::Commit(described.clone())).is_err());
    assert_eq!(&poll_until(&mut session, 6)[2..], described.encode());
    second.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [1]);
    assert_eq!(fixture.saved(), b"my-laptop\n");
    assert_eq!(fixture.backoff().read().unwrap().count(), 0);
    assert_eq!(
        session.answer(Request::HostnameState).unwrap(),
        [0x9f, 0, 0]
    );
    // A requester gone before commit: the commit acts on nothing.
    let intake = session.hostnames.as_mut().unwrap();
    let third = submit_as(&fixture, intake, "third", 1000);
    let answer = session.begin_hostname(|| Table::parse(V1)).unwrap();
    let described = Description::decode(&answer[1..]).unwrap();
    assert_eq!(&poll_until(&mut session, 4)[2..], described.encode());
    session
        .answer(Request::Presented(described.clone()))
        .unwrap();
    drop(third);
    // Another test's child, between its fork and exec, can hold the
    // socket a moment longer: wait for root to see the requester gone.
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let intake = session.hostnames.as_mut().unwrap();
        intake.tick();
        if !intake.selected_alive() {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        session.answer(Request::Commit(described.clone())).unwrap(),
        [0x94]
    );
    assert_eq!(&poll_until(&mut session, 7)[2..], described.encode());
    assert_eq!(fixture.saved(), b"my-laptop\n");
    assert_eq!(fixture.backoff().read().unwrap().count(), 1);
    session
        .close_with(|_| self::fixture("cleanup_child"))
        .unwrap();
}

#[test]
fn one_operation_retains_its_bound_description_until_terminal_delivery() {
    let mut session = quiet();
    prepare(&mut session);
    let response = session
        .answer_with(
            Request::Begin(Role::Recovery),
            cleanup_command,
            |owner, role| {
                assert_eq!(owner, 1000);
                assert_eq!(role, Start::Unlock(Role::Recovery));
                Unlock::fixture(description(), fixture("unlock_child"))
            },
        )
        .unwrap();
    assert_eq!(&response[1..], description().encode());
    assert!(session
        .answer_with(
            Request::Begin(Role::Primary),
            cleanup_command,
            unexpected_begin
        )
        .is_err());
    assert_eq!(&poll_until(&mut session, 4)[2..], description().encode());
    assert!(session.answer(Request::Cancel([43; 32])).is_err());
    session.answer(Request::Presented(description())).unwrap();
    assert_eq!(&poll_until(&mut session, 5)[2..], description().encode());
    assert!(session.answer(Request::Presented(description())).is_err());
    session.answer(Request::Commit(description())).unwrap();
    // Terminal heartbeats can advance completion without retiring the record.
    let until = Instant::now() + Duration::from_secs(3);
    while session.event != Some(Event::Complete) {
        session.tick().unwrap();
        assert!(session.operation.is_some());
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(session
        .answer_with(
            Request::Begin(Role::Primary),
            cleanup_command,
            unexpected_begin
        )
        .is_err());
    assert_eq!(
        session.answer(Request::Cancel([42; 32])).unwrap(),
        [0x95, 1]
    );
    assert_eq!(session.event, Some(Event::Complete));
    assert_eq!(&poll_until(&mut session, 6)[2..], description().encode());
    assert!(session.operation.is_none());
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
}

#[test]
fn missing_cleanup_cannot_be_retried_as_a_fresh_prepare() {
    let mut session = quiet();
    assert!(session
        .answer_with(
            Request::Prepare,
            |_| Command::new("/no-td-session-cleanup"),
            unexpected_begin
        )
        .is_err());
    assert!(session.activated);
    assert!(!session.prepared);
    assert!(session.answer(Request::Prepare).is_err());
    assert!(session
        .answer_with(
            Request::Begin(Role::Primary),
            cleanup_command,
            unexpected_begin
        )
        .is_err());
}

#[test]
#[ignore = "requires the explicitly marked disposable root VM and production td-secret"]
fn root_session_preparation_failure_and_generation_exit_relock() {
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, path::Path};
    assert!(fs::read_to_string("/proc/cmdline")
        .unwrap()
        .split_whitespace()
        .any(|word| word == "td.operation-fixture=1"));
    let status = fs::read_to_string("/proc/self/status").unwrap();
    assert!(status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| ids.split_whitespace().collect::<Vec<_>>() == ["0"; 4]));
    assert!(!Path::new("/var/lib/td/secrets/1000").exists());
    for path in ["/run/td-secret", "/run/td-secret/1000"] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let key = "/run/td-secret/1000/key";
    let seed = || {
        fs::write(key, [0u8; 64]).unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::fchown(fs::File::open(key).unwrap(), Some(991), Some(991)).unwrap();
    };
    seed();
    let mut session = quiet();
    session.answer(Request::Prepare).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = session.answer(Request::Poll).unwrap();
        if reply == [0x91, 2] {
            break;
        }
        assert_eq!(reply, [0x91, 1]);
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!Path::new(key).exists());
    for request in [
        Request::Begin(Role::Primary),
        Request::Enroll(Recovery::Unrecoverable),
        Request::Enroll(Recovery::SecondToken),
    ] {
        seed();
        session.answer(request).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let reply = session.answer(Request::Poll).unwrap();
            if reply[1] == 7 {
                break;
            }
            assert_eq!(reply[1], 3, "missing store reached presentation/release");
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(session.operation.is_none());
        assert!(!Path::new(key).exists());
    }
    seed();
    session.close().unwrap();
    assert!(!Path::new(key).exists());
    assert!(!session.prepared);
}

#[test]
#[ignore = "exec-only worker that never presents"]
fn silent_unlock_child() {
    let mut stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    assert_eq!(read_frame(&mut stream), description().encode());
    thread::sleep(Duration::from_secs(30));
}

#[test]
fn generation_cleanup_runs_after_an_internal_cleanup_error() {
    let mut session = quiet();
    prepare(&mut session);
    session.operation = Some(Active::Secret(Box::new(
        Unlock::fixture(description(), fixture("silent_unlock_child")).unwrap(),
    )));
    session.event = Some(Event::Waiting);
    assert_eq!(
        session.answer(Request::Cancel([42; 32])).unwrap(),
        [0x95, 0]
    );
    // The host has no production /bin/td-secret cleanup helper. The retained
    // fatal supervisor result must not suppress generation cleanup afterwards.
    assert!(!std::path::Path::new("/bin/td-secret").exists());
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if session.tick().is_err() {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    let mut called = false;
    session
        .close_with(|owner| {
            assert_eq!(owner, 1000);
            called = true;
            fixture("cleanup_child")
        })
        .unwrap();
    assert!(called);
    assert!(session.operation.is_none());
}

#[test]
fn generation_exit_reaps_a_live_worker_before_cleanup() {
    let mut session = quiet();
    prepare(&mut session);
    session.operation = Some(Active::Secret(Box::new(
        Unlock::fixture(description(), fixture("silent_unlock_child")).unwrap(),
    )));
    let pid = match session.operation.as_ref().unwrap() {
        Active::Secret(op) => op.fixture_pid().unwrap(),
        _ => panic!("expected secret worker"),
    };
    let started = Instant::now();
    session
        .close_with(|_| {
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
            fixture("cleanup_child")
        })
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(session.operation.is_none());
    assert!(!session.prepared);
}

#[test]
fn cleanup_requires_timely_observation_and_preserves_its_failure() {
    let mut cleanup = Cleanup::start(fixture("cleanup_child")).unwrap();
    thread::sleep(Duration::from_millis(50));
    cleanup.deadline = Instant::now();
    let until = Instant::now() + Duration::from_secs(3);
    let error = loop {
        match cleanup.poll() {
            Err(error) => break error,
            Ok(false) => (),
            Ok(true) => panic!("late observation accepted"),
        }
        assert!(Instant::now() < until);
    };
    assert_eq!(cleanup.poll(), Err(error));
}

#[test]
fn generation_exit_cleans_an_unpolled_completion() {
    let mut session = quiet();
    prepare(&mut session);
    session
        .answer_with(Request::Begin(Role::Recovery), cleanup_command, |_, _| {
            Unlock::fixture(description(), fixture("unlock_child"))
        })
        .unwrap();
    poll_until(&mut session, 4);
    session.answer(Request::Presented(description())).unwrap();
    poll_until(&mut session, 5);
    session.answer(Request::Commit(description())).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while session.event != Some(Event::Complete) {
        session.tick().unwrap();
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    let mut called = false;
    session
        .close_with(|_| {
            called = true;
            fixture("cleanup_child")
        })
        .unwrap();
    assert!(called);
    assert!(session.operation.is_none());
}

fn enrollment_description(recovery: Recovery, step: crate::consent::Enrollment) -> Description {
    Description::new(
        [42; 32],
        1000,
        Operation::Enroll {
            platform: crate::consent::Platform::TpmPcr7,
            recovery,
            step,
        },
    )
    .unwrap()
}

#[test]
#[ignore = "exec-only paired enrollment protocol peer"]
fn enrollment_child() {
    use crate::consent::Enrollment;
    let mut stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let initial = Description::decode(&read_frame(&mut stream)).unwrap();
    let recovery = match initial.operation() {
        Operation::Enroll { recovery, .. } => *recovery,
        _ => panic!("expected enrollment"),
    };
    assert_eq!(
        initial,
        enrollment_description(recovery, Enrollment::CreatePrimary)
    );
    let mut steps = vec![Enrollment::CreatePrimary, Enrollment::ProvePrimary];
    if recovery == Recovery::SecondToken {
        steps.extend([Enrollment::CreateRecovery, Enrollment::ProveRecovery]);
    }
    let mut last = initial;
    for step in steps {
        last = enrollment_description(recovery, step);
        let mut bytes = vec![0x10];
        bytes.extend_from_slice(&[16; 32]);
        bytes.extend_from_slice(&last.encode());
        send_frame(&mut stream, &bytes);
        bytes[0] = 0x11;
        assert_eq!(read_frame(&mut stream), bytes);
    }
    let mut bytes = vec![0x12];
    bytes.extend_from_slice(&[18; 32]);
    bytes.extend_from_slice(&last.encode());
    send_frame(&mut stream, &bytes);
    bytes[0] = 0x13;
    assert_eq!(read_frame(&mut stream), bytes);
    send_frame(&mut stream, &[0x14]);
}

fn secret_event(session: &mut Session) -> Vec<u8> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let response = session.answer(Request::Poll).unwrap();
        if response.get(1) != Some(&3) {
            return response;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn paired_enrollment_fixes_recovery_and_returns_each_required_presentation() {
    use crate::consent::Enrollment;
    for (flag, recovery) in [(0, Recovery::Unrecoverable), (1, Recovery::SecondToken)] {
        let start = Request::decode(&[0x16, flag]).unwrap();
        assert_eq!(start, Request::Enroll(recovery));
        let mut session = quiet();
        assert!(session
            .answer_with(start, cleanup_command, unexpected_begin)
            .is_err());
        prepare(&mut session);
        let initial = enrollment_description(recovery, Enrollment::CreatePrimary);
        let response = session
            .answer_with(
                Request::Enroll(recovery),
                cleanup_command,
                |owner, start| {
                    assert_eq!(owner, 1000);
                    assert_eq!(start, Start::Enroll(recovery));
                    Unlock::fixture(initial.clone(), fixture("enrollment_child"))
                },
            )
            .unwrap();
        assert_eq!(
            response,
            [&[0x92][..], initial.encode().as_slice()].concat()
        );
        assert!(session
            .answer_with(Request::Enroll(recovery), cleanup_command, unexpected_begin)
            .is_err());
        let mut steps = vec![Enrollment::CreatePrimary, Enrollment::ProvePrimary];
        if recovery == Recovery::SecondToken {
            steps.extend([Enrollment::CreateRecovery, Enrollment::ProveRecovery]);
        }
        let mut last = initial;
        for step in steps {
            last = enrollment_description(recovery, step);
            assert_eq!(
                secret_event(&mut session),
                [&[0x91, 4][..], last.encode().as_slice()].concat()
            );
            assert_eq!(
                session.answer(Request::Presented(last.clone())).unwrap(),
                [0x93]
            );
        }
        assert_eq!(
            secret_event(&mut session),
            [&[0x91, 5][..], last.encode().as_slice()].concat()
        );
        assert_eq!(
            session.answer(Request::Commit(last.clone())).unwrap(),
            [0x94]
        );
        assert_eq!(
            secret_event(&mut session),
            [&[0x91, 6][..], last.encode().as_slice()].concat()
        );
        assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    }
    for bytes in [vec![0x16], vec![0x16, 2], vec![0x16, 255], vec![0x16, 0, 0]] {
        assert!(Request::decode(&bytes).is_err());
    }
}

#[test]
fn invalid_enrollment_receipts_require_generation_teardown() {
    use crate::consent::Enrollment;
    for stale in [false, true] {
        let recovery = Recovery::Unrecoverable;
        let initial = enrollment_description(recovery, Enrollment::CreatePrimary);
        let mut session = quiet();
        prepare(&mut session);
        session
            .answer_with(Request::Enroll(recovery), cleanup_command, |_, _| {
                Unlock::fixture(initial.clone(), fixture("enrollment_child"))
            })
            .unwrap();
        assert_eq!(secret_event(&mut session).get(1), Some(&4));
        let rejected = if stale {
            session.answer(Request::Presented(initial.clone())).unwrap();
            assert_eq!(secret_event(&mut session).get(1), Some(&4));
            session.answer(Request::Presented(initial))
        } else {
            session.answer(Request::Commit(initial))
        };
        assert!(rejected.is_err());
        let pid = match session.operation.as_ref().unwrap() {
            Active::Secret(op) => op.fixture_pid().unwrap(),
            _ => panic!("expected secret worker"),
        };
        session
            .close_with(|_| {
                assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
                fixture("cleanup_child")
            })
            .unwrap();
        assert!(session.operation.is_none());
    }
}

#[test]
fn inspection_requires_idle_preparation_and_retains_its_result_until_poll() {
    assert_eq!(Request::decode(&[0x17]).unwrap(), Request::Inspect);
    assert!(Request::decode(&[0x17, 0]).is_err());
    let mut session = quiet();
    let start = |_| Ok(crate::inspection::tests::fixture(3));
    assert!(session.inspect_with(start).is_err());
    prepare(&mut session);
    assert_eq!(session.inspect_with(start).unwrap(), [0x97]);
    assert!(session.inspect_with(start).is_err());
    assert!(session
        .answer_with(
            Request::Begin(Role::Primary),
            |_| fixture("cleanup_child"),
            unexpected_begin
        )
        .is_err());
    let until = Instant::now() + Duration::from_secs(4);
    while session.inspection_event == Some(InspectionEvent::Waiting) {
        session.tick().unwrap();
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(session.inspect_with(start).is_err());
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 9, 3]);
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    session
        .inspect_with(|_| Ok(crate::inspection::tests::fixture(7)))
        .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let answer = session.answer(Request::Poll).unwrap();
        if answer != [0x91, 8] {
            assert_eq!(answer, [0x91, 10]);
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    session
        .inspect_with(|_| Ok(crate::inspection::tests::fixture(8)))
        .unwrap();
    session.close_with(|_| fixture("cleanup_child")).unwrap();
    assert!(session.inspection.is_none());
}

#[test]
#[ignore = "requires the marked disposable root VM and production inspector"]
fn root_inspection_observes_file_state_without_publishing_or_repairing() {
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, path::Path};
    assert!(fs::read_to_string("/proc/cmdline")
        .unwrap()
        .split_whitespace()
        .any(|word| word == "td.operation-fixture=1"));
    assert!(fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| ids.split_whitespace().collect::<Vec<_>>() == ["0"; 4]));
    assert!(!Path::new("/etc/td-principals.tsv").exists());
    assert!(!Path::new("/var/lib/td/secrets/1000").exists());
    fs::create_dir_all("/etc").unwrap();
    for (path, bytes, mode) in [
        (
            "/etc/td-principals.tsv",
            "td-principals-v1\nsession\t1000\t993\t992\t991\n",
            0o444,
        ),
        (
            "/etc/passwd",
            "root:x:0:0:root:/root:/bin/false\ntester:x:1000:1000:Test:/home/tester:/bin/false\n",
            0o644,
        ),
        ("/etc/group", "root:x:0:\ntester:x:1000:\n", 0o644),
        (
            "/etc/shadow",
            "root:!:1:0:99999:7:::\ntester::1:0:99999:7:::\n",
            0o600,
        ),
    ] {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    let path = Path::new("/var/lib/td/secrets/1000");
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::chown(path, Some(991), Some(991)).unwrap();
    for (name, bytes) in [("master", &[42; 32][..]), ("lock", &[][..])] {
        fs::write(path.join(name), bytes).unwrap();
        fs::set_permissions(path.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::chown(path.join(name), Some(991), Some(991)).unwrap();
    }
    let mut session = quiet();
    session.answer(Request::Prepare).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while session.answer(Request::Poll).unwrap() != [0x91, 2] {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    let inspect = |session: &mut Session| {
        assert_eq!(session.answer(Request::Inspect).unwrap(), [0x97]);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let answer = session.answer(Request::Poll).unwrap();
            if answer != [0x91, 8] {
                break answer;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        }
    };
    assert_eq!(inspect(&mut session), [0x91, 9, 0]);
    assert_eq!(fs::read(path.join("master")).unwrap(), [42; 32]);
    assert!(!Path::new("/run/td-secret/1000/key").exists());
    fs::remove_file(path.join("lock")).unwrap();
    assert_eq!(inspect(&mut session), [0x91, 10]);
    assert!(!path.join("lock").exists());
    assert_eq!(fs::read(path.join("master")).unwrap(), [42; 32]);
    session.close().unwrap();
}

/// A prepared session on a live boot, serving through a fake service.
fn live_session() -> (Session, UnixStream) {
    let mut session = quiet();
    prepare(&mut session);
    let (setup, mut service) = crate::disk_install::tests::served();
    session.setup = Some(setup);
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    let mut greeting = [0; 8];
    service.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting, crate::installation_consent::GREETING);
    service
        .write_all(crate::installation_consent::GREETING)
        .unwrap();
    (session, service)
}

fn select_disk(session: &mut Session) -> Description {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let answer = session.answer(Request::Install).unwrap();
        if answer != [0x99, 0] {
            assert_eq!(answer[0], 0x92);
            return Description::decode(&answer[1..]).unwrap();
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_live_session_shows_the_service_review_and_forwards_consent() {
    use crate::disk_install::tests::{answer, report, review};
    use crate::installation_consent::{Answer, Outcome, Report};
    let (mut session, mut service) = live_session();
    // No review yet: nothing to select.
    assert_eq!(session.answer(Request::Install).unwrap(), [0x99, 0]);
    report(&mut service, Report::Review(Box::new(review(4, "vda"))));
    let description = select_disk(&mut session);
    assert!(matches!(
        description.operation(),
        Operation::InstallDisk { .. }
    ));
    assert_eq!(poll_until(&mut session, 4)[2..], description.encode());
    assert_eq!(
        session
            .answer(Request::Presented(description.clone()))
            .unwrap(),
        [0x93]
    );
    poll_until(&mut session, 5);
    assert_eq!(
        session
            .answer(Request::Commit(description.clone()))
            .unwrap(),
        [0x94]
    );
    assert_eq!(answer(&mut service), Answer::Consent([4; 32]));
    report(&mut service, Report::Started([4; 32]));
    report(&mut service, Report::Finished([4; 32], Outcome::Complete));
    // A duplicate commit after the outcome refuses rather than answering.
    assert!(session.answer(Request::Commit(description)).is_err());
    poll_until(&mut session, 6);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

#[test]
fn escape_before_enter_declines_the_review_over_the_channel() {
    use crate::disk_install::tests::{answer, report, review};
    use crate::installation_consent::{Answer, NoConsent, Report};
    let (mut session, mut service) = live_session();
    report(&mut service, Report::Review(Box::new(review(6, "vda"))));
    let description = select_disk(&mut session);
    poll_until(&mut session, 4);
    session
        .answer(Request::Presented(description.clone()))
        .unwrap();
    assert_eq!(session.answer(Request::Cancel([6; 32])).unwrap(), [0x95, 0]);
    assert_eq!(
        answer(&mut service),
        Answer::Declined([6; 32], NoConsent::Declined)
    );
    poll_until(&mut session, 7);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

#[test]
fn an_installer_lost_before_enter_fails_the_prompt_not_the_generation() {
    use crate::disk_install::tests::{report, review};
    use crate::installation_consent::{Ended, Report};
    let (mut session, mut service) = live_session();
    report(&mut service, Report::Review(Box::new(review(5, "vda"))));
    let description = select_disk(&mut session);
    poll_until(&mut session, 4);
    session
        .answer(Request::Presented(description.clone()))
        .unwrap();
    report(&mut service, Report::Ended([5; 32], Ended::InstallerLost));
    // Enter arrives after the service ended the review: commit reads the
    // report first, sends no consent, and the session survives.
    assert_eq!(
        session.answer(Request::Commit(description)).unwrap(),
        [0x94]
    );
    assert_eq!(session.answer(Request::Poll).unwrap()[1], 7);
    service
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut rest = [0; 1];
    assert!(service.read(&mut rest).is_err());
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

fn login_step(before: u8, step: crate::consent::LoginStep) -> Description {
    Description::new(
        [42; 32],
        1000,
        Operation::LoginUnlock {
            account: 1000,
            before,
            after: before,
            step,
        },
    )
    .unwrap()
}

/// Polls past the statuses that need no answer: a login operation before
/// its baseline, and a worker at work.
fn login_event(session: &mut Session) -> Vec<u8> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = session.answer(Request::Poll).unwrap();
        if ![0x0b, 3].contains(&reply[1]) {
            return reply;
        }
        assert!(Instant::now() < until, "login operation stalled");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Answers every presentation, PIN step and commit until the operation ends.
fn login_end(session: &mut Session) -> Vec<u8> {
    loop {
        let reply = login_event(session);
        let description = || Description::decode(&reply[2..]).unwrap();
        match reply[1] {
            4 => assert_eq!(
                session.answer(Request::Presented(description())).unwrap(),
                [0x93]
            ),
            0x0c => assert_eq!(
                session
                    .answer(Request::Pin(
                        Box::new(description()),
                        Pin::new(b"1234").unwrap()
                    ))
                    .unwrap(),
                [0x9c, 0]
            ),
            5 => assert_eq!(
                session.answer(Request::Commit(description())).unwrap(),
                [0x94]
            ),
            _ => return reply,
        }
    }
}

/// Presents every step until the PIN step: its description.
fn login_pin_step(session: &mut Session) -> Description {
    loop {
        let reply = login_event(session);
        let description = Description::decode(&reply[2..]).unwrap();
        if reply[1] == 0x0c {
            return description;
        }
        assert_eq!(reply[1], 4);
        session.answer(Request::Presented(description)).unwrap();
    }
}

fn begin_login(session: &mut Session, selection: Selection, script: &'static str) {
    let answer = session
        .begin_login(selection, true, |owner, selection| {
            assert_eq!(owner, 1000);
            Login::scripted(selection, script)
        })
        .unwrap();
    assert_eq!(answer, [&[0x9b, 1][..], &[42; 32]].concat());
}

#[test]
fn login_requests_have_one_encoding_and_a_pin_never_prints() {
    assert_eq!(
        Request::decode(&[0x1b, 8, 2]).unwrap(),
        Request::Login(Selection::Enroll(2))
    );
    let unlock = login_step(
        2,
        crate::consent::LoginStep::Unlock {
            key: [0xa1; 4],
            retries: 8,
        },
    );
    let encoded = unlock.encode();
    let mut bytes = vec![0x1c, encoded.len() as u8];
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(b"1234");
    let request = Request::decode(&bytes).unwrap();
    assert_eq!(
        request,
        Request::Pin(Box::new(unlock.clone()), Pin::new(b"1234").unwrap())
    );
    assert!(format!("{request:?}").ends_with(", Pin(..))"));
    for bad in [
        vec![0x1b],
        vec![0x1b, 8, 3],
        vec![0x1c],
        vec![0x1c, 0xff],
        bytes[..bytes.len() - 1].to_vec(),
        [&bytes[..], b"\n"].concat(),
        [&[0x1c, encoded.len() as u8 + 1][..], &encoded, b"1234"].concat(),
    ] {
        assert!(Request::decode(&bad).is_err());
    }
}

#[test]
fn a_login_unlock_runs_through_the_paired_session() {
    use crate::consent::LoginStep;
    let mut session = quiet();
    prepare(&mut session);
    begin_login(&mut session, Selection::Unlock, "unlock");
    let identify = login_step(2, LoginStep::Identify);
    let unlock = login_step(
        2,
        LoginStep::Unlock {
            key: [0xa1; 4],
            retries: 8,
        },
    );
    let with = |status: u8, description: &Description| {
        [&[0x91, status][..], &description.encode()].concat()
    };
    assert_eq!(login_event(&mut session), with(4, &identify));
    assert_eq!(
        session.answer(Request::Presented(identify)).unwrap(),
        [0x93]
    );
    assert_eq!(login_event(&mut session), with(4, &unlock));
    assert_eq!(
        session.answer(Request::Presented(unlock.clone())).unwrap(),
        [0x93]
    );
    assert_eq!(login_event(&mut session), with(0x0c, &unlock));
    assert_eq!(
        session
            .answer(Request::Pin(
                Box::new(unlock.clone()),
                Pin::new(b"1234").unwrap()
            ))
            .unwrap(),
        [0x9c, 0]
    );
    assert_eq!(login_event(&mut session), with(5, &unlock));
    assert_eq!(
        session.answer(Request::Commit(unlock.clone())).unwrap(),
        [0x94]
    );
    assert_eq!(login_event(&mut session), with(6, &unlock));
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    assert!(session.operation.is_none() && session.cleanup.is_none());
}

#[test]
fn login_ends_are_typed_and_a_cancel_relocks_nothing() {
    use crate::consent::LoginStep;
    let mut session = quiet();
    prepare(&mut session);
    // Refused from the baseline: no description, then idle.
    begin_login(&mut session, Selection::Add, "baseline-eight");
    assert_eq!(login_end(&mut session), [0x91, 0x0d, 0x81, 0]);
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    // A lost channel after a write's commit acknowledgement is uncertain.
    begin_login(&mut session, Selection::Add, "add-lost");
    let probe = Description::new(
        [42; 32],
        1000,
        Operation::LoginAdd {
            account: 1000,
            before: 1,
            after: 2,
            step: LoginStep::Probe { key: [0x4e; 4] },
        },
    )
    .unwrap();
    assert_eq!(
        login_end(&mut session),
        [&[0x91, 0x0e, 0x10, 0][..], &probe.encode()].concat()
    );
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    // The person cancels at the PIN step: killed, reaped, nothing relocked.
    begin_login(&mut session, Selection::Unlock, "stall-pin");
    let unlock = login_pin_step(&mut session);
    assert_eq!(
        session.answer(Request::Cancel([42; 32])).unwrap(),
        [0x95, 0]
    );
    assert_eq!(
        login_end(&mut session),
        [&[0x91, 0x0d, 0x80, 0][..], &unlock.encode()].concat()
    );
    assert!(session.cleanup.is_none() && session.prepared);
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
    // A PIN for no login operation, or a cancel for another, ends the
    // generation.
    assert!(session
        .answer(Request::Pin(Box::new(unlock), Pin::new(b"1234").unwrap()))
        .is_err());
    let mut session = quiet();
    prepare(&mut session);
    begin_login(&mut session, Selection::Unlock, "stall-pin");
    login_pin_step(&mut session);
    assert!(session.answer(Request::Cancel([7; 32])).is_err());
}

#[test]
fn a_pin_past_the_deadline_is_dropped_and_the_generation_lives_on() {
    let mut session = quiet();
    prepare(&mut session);
    begin_login(&mut session, Selection::Unlock, "stall-pin");
    let unlock = login_pin_step(&mut session);
    match session.operation.as_mut().unwrap() {
        Active::Login(op) => op.expire(),
        _ => panic!("expected a login worker"),
    }
    assert_eq!(
        session
            .answer(Request::Pin(
                Box::new(unlock.clone()),
                Pin::new(b"1234").unwrap()
            ))
            .unwrap(),
        [0x9c, 1]
    );
    assert_eq!(
        login_end(&mut session),
        [&[0x91, 0x0d, 0x08, 0][..], &unlock.encode()].concat()
    );
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
}

/// A disclosure's receipt, which follows its typed key, past root's
/// deadline: answered, the operation ended as TIMEOUT, and the generation
/// goes on, as for a late PIN.
#[test]
fn a_late_disclosure_receipt_is_answered_and_the_generation_lives_on() {
    let mut session = quiet();
    prepare(&mut session);
    begin_login(&mut session, Selection::Enroll(1), "enroll-disclosure");
    let reply = login_event(&mut session);
    assert_eq!(reply[1], 4);
    let connect = Description::decode(&reply[2..]).unwrap();
    assert!(connect.login_approval().is_some());
    match session.operation.as_mut().unwrap() {
        Active::Login(op) => op.expire(),
        _ => panic!("expected a login worker"),
    }
    assert_eq!(
        session.answer(Request::Presented(connect.clone())).unwrap(),
        [0x93]
    );
    assert_eq!(
        login_end(&mut session),
        [&[0x91, 0x0d, 0x08, 0][..], &connect.encode()].concat()
    );
    assert_eq!(session.answer(Request::Poll).unwrap(), [0x91, 2]);
}

#[test]
fn production_refuses_login_writes_before_any_worker_starts() {
    let mut session = quiet();
    prepare(&mut session);
    for selection in [
        Selection::Enroll(1),
        Selection::Enroll(2),
        Selection::Add,
        Selection::Remove(vec![crate::consent::Slot {
            position: 1,
            key: [1; 4],
        }]),
    ] {
        assert_eq!(
            session
                .begin_login(selection, false, |_, _| panic!("a login write started"))
                .unwrap(),
            [0x9b, 0]
        );
        assert!(session.operation.is_none());
    }
    // An unlock is wired; with no record it refuses from the baseline.
    let answer = session
        .begin_login(Selection::Unlock, false, |_, selection| {
            Login::scripted(selection, "baseline-none")
        })
        .unwrap();
    assert_eq!(answer[..2], [0x9b, 1]);
    assert_eq!(login_end(&mut session), [0x91, 0x0d, 0x09, 0]);
}

#[test]
fn a_login_operation_occupies_the_one_operation_slot() {
    let mut session = quiet();
    assert!(session
        .begin_login(Selection::Unlock, true, |_, _| panic!("unprepared"))
        .is_err());
    prepare(&mut session);
    begin_login(&mut session, Selection::Unlock, "silent");
    assert!(session
        .begin_login(Selection::Unlock, true, |_, _| panic!("second login"))
        .is_err());
    assert!(session
        .answer_with(
            Request::Begin(Role::Primary),
            cleanup_command,
            unexpected_begin
        )
        .is_err());
    assert!(session.inspect_with(|_| panic!("inspection")).is_err());
    assert_eq!(session.begin_write().unwrap(), [0x98, 0]);
    assert_eq!(
        session
            .begin_install(|| panic!("table read while busy"))
            .unwrap(),
        [0x99, 0]
    );
    // Teardown reaps the worker before the generation's own cleanup.
    let pid = match session.operation.as_ref().unwrap() {
        Active::Login(op) => op.fixture_pid().unwrap(),
        _ => panic!("expected a login worker"),
    };
    session
        .close_with(|_| {
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
            fixture("cleanup_child")
        })
        .unwrap();
    assert!(session.operation.is_none());
    // And a secret operation keeps a login out.
    let mut session = quiet();
    prepare(&mut session);
    session.operation = Some(Active::Secret(Box::new(
        Unlock::fixture(description(), fixture("silent_unlock_child")).unwrap(),
    )));
    session.event = Some(Event::Waiting);
    assert!(session
        .begin_login(Selection::Unlock, true, |_, _| panic!(
            "login beside unlock"
        ))
        .is_err());
}

#[test]
fn login_state_needs_preparation_but_never_the_operation_slot() {
    use crate::login_status::tests::{answer, never, Root};
    assert_eq!(Request::decode(&[0x1a]).unwrap(), Request::LoginState);
    assert!(Request::decode(&[0x1a, 0]).is_err());
    let root = Root::new();
    let mut session = quiet();
    session.login_state = root.status(never());
    assert!(session.answer(Request::LoginState).is_err());
    let unenrolled = answer(&[0], "td-laptop");
    prepare(&mut session);
    assert_eq!(session.answer(Request::LoginState).unwrap(), unenrolled);
    // Beside a login operation, which keeps its slot.
    begin_login(&mut session, Selection::Unlock, "silent");
    assert_eq!(session.answer(Request::LoginState).unwrap(), unenrolled);
    assert!(matches!(session.operation, Some(Active::Login(_))));
    assert!(session
        .begin_login(Selection::Unlock, true, |_, _| panic!("second login"))
        .is_err());
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

#[test]
fn a_login_operations_end_refreshes_the_cached_state() {
    use crate::inspection::tests::LOGIN_TWO;
    use crate::login_status::tests::{answer, counting, Root};
    let root = Root::new();
    root.enroll();
    let (helper, runs) = counting(LOGIN_TWO);
    let mut session = quiet();
    session.login_state = root.status(helper);
    prepare(&mut session);
    let enrolled = answer(&[&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat(), "td-laptop");
    assert_eq!(session.answer(Request::LoginState).unwrap(), enrolled);
    root.unenroll();
    // An operation still running leaves the cache as it was.
    begin_login(&mut session, Selection::Unlock, "baseline-none");
    assert_eq!(session.answer(Request::LoginState).unwrap(), enrolled);
    assert_eq!(login_end(&mut session), [0x91, 0x0d, 0x09, 0]);
    // Its end, once delivered, reads the state afresh.
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        answer(&[0], "td-laptop")
    );
    // A secret operation's end does not.
    root.enroll();
    session
        .answer_with(Request::Begin(Role::Recovery), cleanup_command, |_, _| {
            Unlock::fixture(description(), fixture("unlock_child"))
        })
        .unwrap();
    poll_until(&mut session, 4);
    session.answer(Request::Presented(description())).unwrap();
    poll_until(&mut session, 5);
    session.answer(Request::Commit(description())).unwrap();
    poll_until(&mut session, 6);
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        answer(&[0], "td-laptop")
    );
    assert_eq!(runs.get(), 1);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

#[test]
fn a_live_session_answers_unenrolled_without_reading() {
    use crate::login_status::tests::{answer, never, Root};
    let root = Root::new();
    root.enroll();
    let (mut session, _service) = live_session();
    session.login_state = root.status(never());
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        answer(&[0], "td-laptop")
    );
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// The image's v1 principal table.
fn v1() -> Result<crate::elevation::Table, String> {
    crate::elevation::Table::parse(crate::elevation::tests::V1)
}

/// What the queued update's requester reads once root is done with it: its
/// completion byte, then the end.
fn retired(requester: &mut UnixStream) -> Vec<u8> {
    requester
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut rest = Vec::new();
    requester.read_to_end(&mut rest).unwrap();
    rest
}

#[test]
fn an_update_that_cannot_read_the_record_is_refused_before_any_description() {
    use crate::deployment::tests::{marker, queued, Fixture};
    use crate::inspection::tests::{LOGIN_DAMAGED, LOGIN_FAILED, LOGIN_LATER, LOGIN_TWO};
    use crate::login_status::tests::{counting, never, Root};
    #[derive(Clone, Copy, Debug)]
    enum Machine {
        Unenrolled,
        /// The helper's result, version 1 or 2.
        Enrolled(u8),
        RecordDamaged,
        DirectoryDamaged,
        Unreadable,
    }
    let unmarked = None;
    let (one, two, both) = (marker(&[1]), marker(&[2]), marker(&[1, 2]));
    let cases: &[(Machine, Option<&[u8]>, bool)] = &[
        // Unenrolled: unchanged, whatever the deployment carries.
        (Machine::Unenrolled, unmarked, true),
        (Machine::Unenrolled, Some(&two), true),
        (Machine::Unenrolled, Some(&one), true),
        // Enrolled: the marker must list the record's version.
        (Machine::Enrolled(1), unmarked, false),
        (Machine::Enrolled(1), Some(&two), false),
        (Machine::Enrolled(1), Some(&one), true),
        (Machine::Enrolled(1), Some(&both), true),
        (Machine::Enrolled(2), Some(&one), false),
        (Machine::Enrolled(2), Some(&both), true),
        // Unavailable: any marker, since the record cannot be read.
        (Machine::RecordDamaged, unmarked, false),
        (Machine::RecordDamaged, Some(&two), true),
        (Machine::DirectoryDamaged, unmarked, false),
        (Machine::DirectoryDamaged, Some(&one), true),
        (Machine::Unreadable, unmarked, false),
        (Machine::Unreadable, Some(&two), true),
    ];
    for &(machine, tier, admitted) in cases {
        let root = Root::new();
        let helper = match machine {
            Machine::Unenrolled | Machine::DirectoryDamaged => never(),
            Machine::Enrolled(1) => counting(LOGIN_TWO).0,
            Machine::Enrolled(_) => counting(LOGIN_LATER).0,
            Machine::RecordDamaged => counting(LOGIN_DAMAGED).0,
            Machine::Unreadable => counting(LOGIN_FAILED).0,
        };
        match machine {
            Machine::Unenrolled => (),
            Machine::DirectoryDamaged => root.damage(),
            _ => root.enroll(),
        }
        let mut session = quiet();
        session.login_state = root.status(helper);
        prepare(&mut session);
        let update = Fixture::marked(tier);
        let (intake, mut requester) = queued(&update);
        session.installations = Some(intake);
        let answer = session.begin_install(v1).unwrap();
        let case = format!("{machine:?} {tier:?}");
        if admitted {
            assert_eq!(answer[0], 0x92, "{case}");
            let description = Description::decode(&answer[1..]).unwrap();
            let Operation::Install {
                key: _,
                deployment,
                requester,
            } = description.operation()
            else {
                panic!("not an installation: {case}");
            };
            assert_eq!(
                (deployment.as_str(), *requester),
                (update.id(), 1000),
                "{case}"
            );
            assert!(matches!(session.operation, Some(Active::Install(_))));
            assert!(session.installing);
        } else {
            // 99 01: no description, nothing to present or commit, and the
            // requester's completion byte is 00.
            assert_eq!(answer, [0x99, 1], "{case}");
            assert!(session.operation.is_none() && session.event.is_none());
            assert!(!session.installing);
            assert!(!session.installations.as_ref().unwrap().selected_alive());
            assert_eq!(retired(&mut requester), [0], "{case}");
            // Retired: there is nothing left to select.
            assert_eq!(session.begin_install(v1).unwrap(), [0x99, 0]);
        }
        session.close_with(|_| fixture("cleanup_child")).unwrap();
    }
}

#[test]
fn a_marker_read_past_its_budget_refuses_inside_the_channels_receive() {
    use crate::deployment::tests::{hurry, marker, queued, Fixture};
    use crate::inspection::tests::LOGIN_TWO;
    use crate::login_status::tests::{counting, Root};
    let root = Root::new();
    root.enroll();
    let mut session = quiet();
    session.login_state = root.status(counting(LOGIN_TWO).0);
    prepare(&mut session);
    // A marker this record admits, read with its budget already spent:
    // the read reads no version, and the request refuses rather than
    // outlasting the compositor's five-second receive.
    let update = Fixture::marked(Some(&marker(&[1])));
    let (mut intake, mut requester) = queued(&update);
    hurry(&mut intake, Duration::ZERO);
    session.installations = Some(intake);
    let started = Instant::now();
    let answer = session.begin_install(v1).unwrap();
    let took = started.elapsed();
    assert_eq!(answer, [0x99, 1]);
    assert!(took < Duration::from_secs(4), "{took:?}");
    assert_eq!(retired(&mut requester), [0]);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

#[test]
fn an_updates_fresh_read_refreshes_the_cached_state() {
    use crate::deployment::tests::{marker, queued, Fixture};
    use crate::inspection::tests::LOGIN_TWO;
    use crate::login_status::tests::{answer, counting, Root};
    let root = Root::new();
    root.enroll();
    let (helper, runs) = counting(LOGIN_TWO);
    let mut session = quiet();
    session.login_state = root.status(helper);
    prepare(&mut session);
    let enrolled = answer(&[&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat(), "td-laptop");
    assert_eq!(session.answer(Request::LoginState).unwrap(), enrolled);
    assert_eq!(runs.get(), 1);
    // The cache holds enrolled; request 19 reads afresh, and runs the
    // helper again.
    let update = Fixture::marked(Some(&marker(&[1])));
    let (intake, _requester) = queued(&update);
    session.installations = Some(intake);
    assert_eq!(session.begin_install(v1).unwrap()[0], 0x92);
    assert_eq!(runs.get(), 2);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
    // The record gone: request 19's read is unenrolled, refusing nothing,
    // and the next 1a answers it from the cache without the helper.
    let root = Root::new();
    root.enroll();
    let (helper, runs) = counting(LOGIN_TWO);
    let mut session = quiet();
    session.login_state = root.status(helper);
    prepare(&mut session);
    assert_eq!(session.answer(Request::LoginState).unwrap(), enrolled);
    root.unenroll();
    let update = Fixture::marked(None);
    let (intake, _requester) = queued(&update);
    session.installations = Some(intake);
    assert_eq!(session.begin_install(v1).unwrap()[0], 0x92);
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        answer(&[0], "td-laptop")
    );
    assert_eq!(runs.get(), 1);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

#[test]
#[ignore = "requires the explicitly marked disposable root VM and production td-secret"]
fn root_login_supervision_meets_the_production_worker() {
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, path::Path};
    assert!(fs::read_to_string("/proc/cmdline")
        .unwrap()
        .split_whitespace()
        .any(|word| word == "td.operation-fixture=1"));
    assert!(fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| ids.split_whitespace().collect::<Vec<_>>() == ["0"; 4]));
    let directory = Path::new("/var/lib/td/login");
    assert!(!directory.exists());
    let mut session = quiet();
    session.answer(Request::Prepare).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while session.answer(Request::Poll).unwrap() != [0x91, 2] {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    let end = |session: &mut Session, selection| {
        let answer = session.answer(Request::Login(selection)).unwrap();
        assert_eq!((answer.len(), &answer[..2]), (34, &[0x9b, 1][..]));
        login_end(session)
    };
    // Request 1a's state bytes; the guest's hostname is whatever it is.
    let state = |session: &mut Session| {
        let answer = session.answer(Request::LoginState).unwrap();
        let names = answer
            .windows(7)
            .position(|window| window == b"\x06tester")
            .unwrap();
        assert_eq!((answer[0], answer.last()), (0x9a, Some(&0)));
        answer[1..names].to_vec()
    };
    // No directory: the predicate's damage, and the worker's own typed
    // refusal, before any baseline.
    assert_eq!(state(&mut session), [2, 0x0a]);
    assert_eq!(end(&mut session, Selection::Unlock), [0x91, 0x0d, 0x0a, 0]);
    fs::create_dir_all(directory).unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    // The operation's end refreshed the cache: unenrolled, with no helper.
    assert_eq!(state(&mut session), [0]);
    // Unenrolled: root refuses these from the worker's baseline.
    assert_eq!(end(&mut session, Selection::Unlock), [0x91, 0x0d, 0x09, 0]);
    assert_eq!(end(&mut session, Selection::Add), [0x91, 0x0d, 0x09, 0]);
    // A record name: the production inspect-login helper reads it as a
    // damaged record, and the cache keeps that until an operation ends.
    let record = directory.join("1000");
    fs::write(&record, b"not a login record").unwrap();
    fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(state(&mut session), [2, 0x0b]);
    fs::remove_file(&record).unwrap();
    assert_eq!(state(&mut session), [2, 0x0b]);
    // Root's first step reaches the worker, which refuses to write: this
    // guest has no /run/td-volume, so no retained deployment's marker reads.
    let reply = end(&mut session, Selection::Enroll(1));
    assert_eq!(reply[..4], [0x91, 0x0d, 0x12, 0]);
    // Its disclosure carries a key root drew.
    assert!(matches!(
        Description::decode(&reply[4..]).unwrap().operation(),
        &Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after: 1,
            key: 1,
            step: crate::consent::LoginStep::Connect,
            approval: Some(_),
        }
    ));
    assert_eq!(state(&mut session), [0]);
    assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    session.close().unwrap();
}

/// Request 19 (td-authd/DESIGN.md, "Elevation operations"): the principal
/// table's `deploy-publish` row for the owner, before the queued update is
/// taken or any description exists; a miss, or a table that cannot be
/// read, answers 99 02 and leaves the request queued for a granted
/// selection, whose tag 5 carries the approval key. A live boot's disk
/// review consults no table (`select_disk`).
#[test]
fn an_update_needs_the_deploy_publish_row_before_any_description() {
    use crate::deployment::tests::{queued, Fixture};
    use crate::elevation::Table;
    use crate::login_status::tests::{never, Root};
    let root = Root::new();
    let mut session = quiet();
    session.login_state = root.status(never());
    prepare(&mut session);
    let update = Fixture::new();
    let (intake, _requester) = queued(&update);
    session.installations = Some(intake);
    for table in [
        Table::parse("td-elevation-v1\n1000\tdeploy-rollback\tset-hostname\n"),
        Table::parse("td-elevation-v1\n1001\tdeploy-publish\n"),
        Err("no table".to_string()),
    ] {
        assert_eq!(session.begin_install(|| table).unwrap(), [0x99, 2]);
        assert!(session.operation.is_none() && !session.installing);
    }
    assert_eq!(update.backoff().read().unwrap().count(), 1);
    let answer = session.begin_install(v1).unwrap();
    assert_eq!(answer[0], 0x92);
    let described = Description::decode(&answer[1..]).unwrap();
    let Operation::Install { key, .. } = described.operation() else {
        panic!("not an installation");
    };
    assert!(key
        .digits()
        .iter()
        .all(|digit| (b'2'..=b'9').contains(digit)));
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// A session whose login state reads under `root` with `helper` and whose
/// revocation runs on `machine`, its render printing `form`.
fn revoking(
    root: &crate::login_status::tests::Root,
    helper: crate::login_status::Helper,
    machine: &crate::revocation::tests::Machine,
    form: &str,
) -> Session {
    let mut session = quiet();
    session.login_state = root.status(helper);
    session.revocation = Some(machine.revocation(form));
    session
}

/// `9a` for `state` on the test root, ending in `revocation`.
fn revoked(state: &[u8], revocation: u8) -> Vec<u8> {
    let mut answer = crate::login_status::tests::answer(state, "td-laptop");
    *answer.last_mut().unwrap() = revocation;
    answer
}

/// Asks `1a` until its revocation byte is `byte`.
fn login_state_until(session: &mut Session, byte: u8) -> Vec<u8> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let answer = session.answer(Request::LoginState).unwrap();
        if answer.last() == Some(&byte) {
            return answer;
        }
        assert!(Instant::now() < until, "revocation byte never {byte}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// The generation's first `1a` computes the reduced state, begins the
/// check and then answers, already `01`; the cutover runs beside the
/// slot, and later `1a`s advance it to `00` without a second check.
#[test]
fn the_first_login_state_begins_the_check_before_it_answers() {
    use crate::cutover::Reduced;
    use crate::login_status::tests::{never, Root};
    use crate::revocation::tests::{Behavior, Machine, ID};
    let root = Root::new();
    let machine = Machine::new();
    machine.record(ID, Reduced::Enforced);
    let svc = machine.svc(Behavior::default());
    let mut session = revoking(&root, never(), &machine, "unenrolled");
    assert!(session.answer(Request::LoginState).is_err());
    prepare(&mut session);
    for _ in 0..5 {
        session.answer(Request::Poll).unwrap();
    }
    assert_eq!(machine.renders(), 0, "a check before the first 1a");
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&[0], 1)
    );
    // Beside the slot: a login operation begins while it runs.
    begin_login(&mut session, Selection::Unlock, "silent");
    assert_eq!(login_state_until(&mut session, 0), revoked(&[0], 0));
    assert_eq!(
        machine.recorded().unwrap(),
        format!("td-login-cutover-v1\n{ID}\nunenrolled\n")
    );
    assert_eq!(machine.renders(), 1);
    assert!(matches!(session.operation, Some(Active::Login(_))));
    for _ in 0..5 {
        assert_eq!(
            session.answer(Request::LoginState).unwrap(),
            revoked(&[0], 0)
        );
    }
    assert_eq!(machine.renders(), 1);
    assert!(svc.requests().contains(&"restart greeter".to_string()));
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// A login operation's end begins the check before its terminal status
/// is delivered, so the `1a` after that status already reads `01`.
#[test]
fn a_login_operations_end_begins_the_check_before_its_status() {
    use crate::cutover::Reduced;
    use crate::inspection::tests::LOGIN_TWO;
    use crate::login_status::tests::{counting, Root};
    use crate::revocation::tests::{Behavior, Machine, ID};
    let root = Root::new();
    let machine = Machine::new();
    machine.record(ID, Reduced::Unenrolled);
    let _svc = machine.svc(Behavior::default());
    let mut session = revoking(&root, counting(LOGIN_TWO).0, &machine, "enforced");
    prepare(&mut session);
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&[0], 0)
    );
    begin_login(&mut session, Selection::Unlock, "baseline-none");
    root.enroll();
    assert_eq!(machine.renders(), 0, "a check before the operation ended");
    assert_eq!(login_end(&mut session), [0x91, 0x0d, 0x09, 0]);
    assert_eq!(
        session.revocation.as_ref().unwrap().status(),
        crate::revocation::Status::Pending
    );
    let enrolled = [&[1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat();
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&enrolled, 1)
    );
    assert_eq!(login_state_until(&mut session, 0), revoked(&enrolled, 0));
    assert_eq!(
        machine.recorded().unwrap(),
        format!("td-login-cutover-v1\n{ID}\nenforced\n")
    );
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// The reduced state is the predicate's alone: an unenrolled machine
/// whose helper fails runs no helper and no cutover, and a record name
/// the failing helper cannot read is enforced, not a cutover either.
#[test]
fn a_failing_state_helper_never_causes_a_cutover() {
    use crate::cutover::Reduced;
    use crate::inspection::tests::LOGIN_FAILED;
    use crate::login_status::tests::{counting, Root};
    use crate::revocation::tests::{Behavior, Machine, ID};
    let root = Root::new();
    let machine = Machine::new();
    let svc = machine.svc(Behavior::default());
    machine.record(ID, Reduced::Unenrolled);
    let (helper, runs) = counting(LOGIN_FAILED);
    let mut session = revoking(&root, helper, &machine, "enforced");
    prepare(&mut session);
    for _ in 0..3 {
        assert_eq!(
            session.answer(Request::LoginState).unwrap(),
            revoked(&[0], 0)
        );
    }
    assert_eq!(runs.get(), 0);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
    root.enroll();
    machine.record(ID, Reduced::Enforced);
    let (helper, runs) = counting(LOGIN_FAILED);
    let mut session = revoking(&root, helper, &machine, "unenrolled");
    prepare(&mut session);
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&[2, 0x0c], 0)
    );
    assert_eq!(runs.get(), 1);
    assert_eq!(machine.renders(), 0);
    assert!(svc.requests().is_empty());
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// A failed revocation reads `02`; every request that takes the slot is
/// then a protocol violation; the reboot is requested only once the
/// login operation holding the slot has delivered its end, and once.
#[test]
fn a_failed_revocation_refuses_the_slot_and_reboots_once_it_is_free() {
    use crate::cutover::Reduced;
    use crate::login_status::tests::{never, Root};
    use crate::revocation::tests::{Behavior, Machine, ID};
    let root = Root::new();
    let machine = Machine::new();
    machine.record(ID, Reduced::Enforced);
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut session = revoking(&root, never(), &machine, "unenrolled");
    prepare(&mut session);
    begin_login(&mut session, Selection::Unlock, "silent");
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&[0], 1)
    );
    assert_eq!(login_state_until(&mut session, 2), revoked(&[0], 2));
    assert_eq!(
        std::fs::read_to_string(machine.guard()).unwrap(),
        format!("{ID}\n")
    );
    for _ in 0..20 {
        session.answer(Request::Poll).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(machine.reboots(), 0, "a reboot beside the login operation");
    for request in [
        Request::Inspect,
        Request::Write,
        Request::Install,
        Request::Rollback,
        Request::Hostname,
        Request::Begin(Role::Primary),
        Request::Enroll(Recovery::SecondToken),
        Request::Login(Selection::Unlock),
        Request::Login(Selection::Enroll(1)),
    ] {
        let refused = session.answer(request).unwrap_err();
        assert!(refused.contains("revocation"), "{refused}");
    }
    assert!(matches!(session.operation, Some(Active::Login(_))));
    // The operation's own requests are not the slot's.
    assert_eq!(
        session.answer(Request::Cancel([42; 32])).unwrap(),
        [0x95, 0]
    );
    assert_eq!(login_end(&mut session), [0x91, 0x0d, 0x80, 0]);
    let until = Instant::now() + Duration::from_secs(5);
    while machine.reboots() == 0 {
        session.answer(Request::Poll).unwrap();
        assert!(Instant::now() < until, "no reboot request");
        thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..50 {
        assert_eq!(
            session.answer(Request::LoginState).unwrap(),
            revoked(&[0], 2)
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(machine.reboots(), 1);
    assert_eq!(machine.recorded(), None, "removed as the cutover began");
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// A reboot request td-svc does not accept ends the generation.
#[test]
fn a_refused_reboot_request_ends_the_generation() {
    use crate::cutover::Reduced;
    use crate::login_status::tests::{never, Root};
    use crate::revocation::tests::{prints, Behavior, Machine, ID};
    let root = Root::new();
    let machine = Machine::new();
    machine.record(ID, Reduced::Enforced);
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut session = quiet();
    session.login_state = root.status(never());
    session.revocation =
        Some(machine.revocation_with(prints("unenrolled\n"), prints("error: refused\n").exits(1)));
    prepare(&mut session);
    let until = Instant::now() + Duration::from_secs(10);
    let error = loop {
        match session.answer(Request::LoginState) {
            Ok(_) => {}
            Err(error) => break error,
        }
        assert!(Instant::now() < until, "the generation lived on");
        thread::sleep(Duration::from_millis(2));
    };
    assert!(error.contains("reboot"), "{error}");
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// A live boot runs no check, whatever its record says.
#[test]
fn a_live_session_runs_no_revocation_check() {
    use crate::cutover::Reduced;
    use crate::login_status::tests::{never, Root};
    use crate::revocation::tests::{Machine, OTHER};
    let root = Root::new();
    root.enroll();
    let machine = Machine::new();
    machine.record(OTHER, Reduced::Unenrolled);
    let (mut session, _service) = live_session();
    session.login_state = root.status(never());
    session.revocation = Some(machine.revocation("enforced"));
    for _ in 0..3 {
        assert_eq!(
            session.answer(Request::LoginState).unwrap(),
            revoked(&[0], 0)
        );
    }
    assert_eq!(machine.renders(), 0);
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}

/// Teardown abandons a cutover in flight without its record, so the next
/// generation's first `1a` begins it again.
#[test]
fn teardown_abandons_the_cutover_and_the_next_generation_repeats_it() {
    use crate::cutover::Reduced;
    use crate::login_status::tests::{never, Root};
    use crate::revocation::tests::{accepted, prints, Behavior, Machine, ID};
    let root = Root::new();
    let machine = Machine::new();
    machine.record(ID, Reduced::Enforced);
    let _svc = machine.svc(Behavior::default());
    let mut session = quiet();
    session.login_state = root.status(never());
    session.revocation = Some(machine.revocation_with(prints("").hangs(), accepted()));
    prepare(&mut session);
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&[0], 1)
    );
    session.close_with(|_| fixture("cleanup_child")).unwrap();
    assert_eq!(machine.recorded(), None);
    let mut session = revoking(&root, never(), &machine, "unenrolled");
    prepare(&mut session);
    assert_eq!(
        session.answer(Request::LoginState).unwrap(),
        revoked(&[0], 1)
    );
    assert_eq!(login_state_until(&mut session, 0), revoked(&[0], 0));
    session.close_with(|_| fixture("cleanup_child")).unwrap();
}
