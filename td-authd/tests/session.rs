#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::consent::Operation;
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

fn unexpected_begin(_: u32, _: Role) -> Result<Unlock, String> {
    panic!("operation began before admission")
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
    assert!(Session::new(1001).is_err());
    let mut session = Session::new(1000).unwrap();
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
        let mut session = Session::new(1000).unwrap();
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

#[test]
fn one_operation_retains_its_bound_description_until_terminal_delivery() {
    let mut session = Session::new(1000).unwrap();
    prepare(&mut session);
    let response = session
        .answer_with(
            Request::Begin(Role::Recovery),
            cleanup_command,
            |owner, role| {
                assert_eq!(owner, 1000);
                assert_eq!(role, Role::Recovery);
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
    let mut session = Session::new(1000).unwrap();
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
    let mut session = Session::new(1000).unwrap();
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
    session.answer(Request::Begin(Role::Primary)).unwrap();
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
    let mut session = Session::new(1000).unwrap();
    prepare(&mut session);
    session.operation =
        Some(Unlock::fixture(description(), fixture("silent_unlock_child")).unwrap());
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
    let mut session = Session::new(1000).unwrap();
    prepare(&mut session);
    session.operation =
        Some(Unlock::fixture(description(), fixture("silent_unlock_child")).unwrap());
    let pid = session.operation.as_ref().unwrap().fixture_pid().unwrap();
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
    let mut session = Session::new(1000).unwrap();
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
