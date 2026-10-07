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
    let mut session = Session::new(1000, "tester").unwrap();
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
        let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
        let mut session = Session::new(1000, "tester").unwrap();
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
        let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
            "root::1:0:99999:7:::\ntester::1:0:99999:7:::\n",
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
    prepare(&mut session);
    begin_login(&mut session, Selection::Unlock, "stall-pin");
    login_pin_step(&mut session);
    assert!(session.answer(Request::Cancel([7; 32])).is_err());
}

#[test]
fn a_pin_past_the_deadline_is_dropped_and_the_generation_lives_on() {
    let mut session = Session::new(1000, "tester").unwrap();
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

#[test]
fn production_refuses_login_writes_before_any_worker_starts() {
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    assert_eq!(session.begin_install().unwrap(), [0x99, 0]);
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    let mut session = Session::new(1000, "tester").unwrap();
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
    // Root's first step reaches the worker, which refuses to write while no
    // deployment carries the tier marker.
    let reply = end(&mut session, Selection::Enroll(1));
    assert_eq!(reply[..4], [0x91, 0x0d, 0x12, 0]);
    assert_eq!(
        Description::decode(&reply[4..]).unwrap().operation(),
        &Operation::LoginEnroll {
            account: 1000,
            before: 0,
            after: 1,
            key: 1,
            step: crate::consent::LoginStep::Connect,
        }
    );
    assert_eq!(state(&mut session), [0]);
    assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    session.close().unwrap();
}
