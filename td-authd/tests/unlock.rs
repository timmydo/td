#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use std::os::fd::AsFd;
use std::thread;

fn write_request() -> Request {
    Request::new([42; 32], 1000, Operation::Set {
        application: "mail".into(), name: "main".into(), application_uid: 65537,
        requester: 1000, role: Role::Recovery,
    }).unwrap()
}

#[test]
#[ignore = "exec-only write child driven by the supervisor socket fixture"]
fn write_child() {
    let mut stream = child_stream();
    assert_eq!(read_frame(&mut stream), write_request().encode());
    assert_eq!(read_frame(&mut stream), vec![0xa5; MAX_SECRET]);
    for tag in [0x10, 0x12] {
        let mut frame = vec![tag];
        frame.extend_from_slice(&[tag; 32]);
        frame.extend_from_slice(&write_request().encode());
        send_frame(&mut stream, &frame);
        frame[0] += 1;
        assert_eq!(read_frame(&mut stream), frame);
    }
    send_frame(&mut stream, &[0x14]);
}

#[test]
fn write_input_requires_both_receipts_and_cancellation_never_locks_prior_release() {
    for stop in 0..3 {
        let request = write_request();
        assert_eq!(operation_verb(request.operation()).unwrap(), "write-operation");
        let mut worker = Unlock::spawn(request.clone(), fixture("write_child")).unwrap();
        worker.credential = Some(Credential(vec![0xa5; MAX_SECRET]));
        worker.phase = Phase::CredentialInput;
        assert_eq!(advance(&mut worker).unwrap(), Event::Present(request.clone()));
        if stop > 0 {
            worker.presented(&request).unwrap();
            assert_eq!(advance(&mut worker).unwrap(), Event::Commit(request.clone()));
        }
        if stop == 2 { worker.commit(&request).unwrap(); }
        else { worker.cancel("cancel write").unwrap(); }
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            match worker.poll_with(|_| panic!("write cancellation revoked prior runtime release")).unwrap() {
                Event::Waiting => (),
                Event::Complete if stop == 2 => break,
                Event::Failed(reason) if stop < 2 => { assert_eq!(reason, "cancel write"); break; }
                event => panic!("unexpected write event {event:?}"),
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(worker.child.is_none());
        assert!(worker.credential.is_none());
    }
}

fn request() -> Request {
    Request::new(
        [42; 32],
        1000,
        Operation::Unlock {
            role: Role::Primary,
        },
    )
    .unwrap()
}

fn fixture(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    sanitized(&mut command).args(["--exact", &format!("unlock::tests::{name}"), "--ignored"]);
    command
}

fn advance(unlock: &mut Unlock) -> Result<Event, String> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let event = unlock.poll_with(|_| fixture("cleanup_child"));
        if event != Ok(Event::Waiting) {
            return event;
        }
        assert!(Instant::now() < until, "supervisor stalled");
        thread::sleep(Duration::from_millis(1));
    }
}

fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
    let mut size = [0; 2];
    stream.read_exact(&mut size).unwrap();
    let mut frame = vec![0; usize::from(u16::from_be_bytes(size))];
    stream.read_exact(&mut frame).unwrap();
    frame
}

fn send_frame(stream: &mut UnixStream, frame: &[u8]) {
    stream
        .write_all(&(frame.len() as u16).to_be_bytes())
        .unwrap();
    stream.write_all(frame).unwrap();
}

fn child_stream() -> UnixStream {
    let stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
}

fn protocol(stream: &mut UnixStream) {
    assert_eq!(read_frame(stream), request().encode());
    for tag in [0x10, 0x12] {
        let mut frame = vec![tag];
        frame.extend_from_slice(&[tag; 32]);
        frame.extend_from_slice(&request().encode());
        send_frame(stream, &frame);
        frame[0] += 1;
        assert_eq!(read_frame(stream), frame);
    }
    send_frame(stream, &[0x14]);
}

#[test]
#[ignore = "exec-only child driven by the supervisor socket fixture"]
fn successful_child() {
    protocol(&mut child_stream());
}

#[test]
#[ignore = "exec-only failure after the success frame"]
fn failed_child() {
    protocol(&mut child_stream());
    std::process::exit(1);
}

#[test]
#[ignore = "exec-only stalled token stand-in"]
fn stalled_child() {
    let mut stream = child_stream();
    assert_eq!(read_frame(&mut stream), request().encode());
    thread::sleep(Duration::from_secs(30));
}

#[test]
#[ignore = "exec-only cleanup helper stand-in"]
fn cleanup_child() {
    assert_eq!(std::env::current_dir().unwrap(), std::path::Path::new("/"));
    assert_eq!(std::env::vars().count(), 0);
}

#[test]
fn completion_needs_both_bound_acknowledgements_and_successful_exit() {
    for (fixture_name, success) in [("successful_child", true), ("failed_child", false)] {
        let mut unlock = Unlock::spawn(request(), fixture(fixture_name)).unwrap();
        assert_eq!(advance(&mut unlock), Ok(Event::Present(request())));
        unlock.presented(&request()).unwrap();
        assert_eq!(advance(&mut unlock), Ok(Event::Commit(request())));
        assert!(unlock.presented(&request()).is_err());
        unlock.commit(&request()).unwrap();
        assert_eq!(advance(&mut unlock).unwrap() == Event::Complete, success);
        assert!(unlock.child.is_none());
        assert!(unlock.phase == Phase::Done);
    }
}

#[test]
fn stalled_child_polls_promptly_and_is_reaped_before_cleanup() {
    let mut unlock = Unlock::spawn(request(), fixture("stalled_child")).unwrap();
    let started = Instant::now();
    for _ in 0..100 {
        assert_eq!(unlock.poll(), Ok(Event::Waiting));
    }
    assert!(started.elapsed() < Duration::from_millis(500));
    let pid = unlock.child.as_ref().unwrap().id();
    unlock.cancel("physical cancellation").unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let mut cleanups = 0;
    let error = loop {
        match unlock.poll_with(|_| {
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "cleanup started before the prior child was reaped"
            );
            cleanups += 1;
            fixture("cleanup_child")
        }) {
            Ok(Event::Failed(error)) => break error,
            other => assert_eq!(other, Ok(Event::Waiting)),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(cleanups, 1);
    assert_eq!(error, "physical cancellation");
    assert!(unlock.child.is_none());
}

#[test]
fn stale_nonce_and_expired_receipt_withhold_commit() {
    for expired in [false, true] {
        let mut unlock = Unlock::spawn(request(), fixture("successful_child")).unwrap();
        assert_eq!(advance(&mut unlock), Ok(Event::Present(request())));
        let ack = if expired {
            unlock.acknowledgement_deadline = Some(Instant::now());
            request()
        } else {
            Request::new([43; 32], 1000, request().operation().clone()).unwrap()
        };
        assert!(unlock.presented(&ack).is_err());
        assert!(matches!(advance(&mut unlock).unwrap(), Event::Failed(_)));
        assert!(unlock.child.is_none());
    }
}

#[test]
fn partial_frames_and_deadlines_do_not_block_the_poll_loop() {
    let (parent, mut child) = UnixStream::pair().unwrap();
    let mut wire = Wire::new(parent).unwrap();
    for byte in [0, 1, 42] {
        child.write_all(&[byte]).unwrap();
        let frame = wire.poll().unwrap();
        assert_eq!(frame, if byte == 42 { Some(vec![42]) } else { None });
    }
    child.write_all(&[0]).unwrap();
    assert_eq!(wire.poll().unwrap(), None);
    wire.deadline = Some(Instant::now());
    assert!(wire.poll().is_err());
}

#[test]
fn cleanup_spawn_failure_is_a_stable_terminal_escalation() {
    let mut unlock = Unlock::spawn(request(), fixture("stalled_child")).unwrap();
    unlock.cancel("physical cancellation").unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let error = loop {
        match unlock.poll_with(|_| Command::new("/no-such-td-unlock-cleanup")) {
            Err(error) => break error,
            Ok(event) => assert_eq!(event, Event::Waiting),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    };
    assert!(error.contains("session cleanup failed; end authority generation"));
    assert!(unlock.phase == Phase::Done);
    assert!(unlock.child.is_none());
    assert_eq!(unlock.poll().unwrap_err(), error);
}

#[test]
#[ignore = "exec-only failing cleanup helper"]
fn failed_cleanup_child() {
    std::process::exit(7);
}

#[test]
#[ignore = "exec-only stalled cleanup helper"]
fn stalled_cleanup_child() {
    thread::sleep(Duration::from_secs(30));
}

#[test]
fn cleanup_failure_and_expiration_are_fatal_not_recoverable_events() {
    for name in ["failed_cleanup_child", "stalled_cleanup_child"] {
        let mut unlock = Unlock::spawn(request(), fixture("stalled_child")).unwrap();
        unlock.cancel("physical cancellation").unwrap();
        let until = Instant::now() + Duration::from_secs(4);
        let error = loop {
            match unlock.poll_with(|_| fixture(name)) {
                Err(error) => break error,
                event => assert_eq!(event, Ok(Event::Waiting)),
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        };
        assert!(error.contains("session cleanup failed; end authority generation"));
        assert!(unlock.phase == Phase::Done);
        assert!(unlock.child.is_none());
        assert_eq!(unlock.poll().unwrap_err(), error);
        assert_eq!(unlock.cleanup_expired, name == "stalled_cleanup_child");
    }
}

#[test]
#[ignore = "requires the explicitly marked disposable root VM and production td-secret"]
fn root_supervisor_relocks_after_the_production_worker_refuses() {
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
    fs::write(key, [0u8; 64]).unwrap();
    fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::fchown(File::open(key).unwrap(), Some(991), Some(991)).unwrap();
    let mut unlock = Unlock::start(1000, Role::Primary).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match unlock.poll().unwrap() {
            Event::Waiting => (),
            Event::Failed(_) => break,
            event => panic!("missing persistent store unexpectedly authorized {event:?}"),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(unlock.child.is_none());
    assert!(!Path::new(key).exists());
}

fn enrollment_request(recovery: Recovery, nonce: u8) -> Request {
    Request::new(
        [nonce; 32],
        1000,
        Operation::Enroll {
            platform: Platform::TpmPcr7,
            recovery,
            step: Enrollment::CreatePrimary,
        },
    )
    .unwrap()
}

#[test]
#[ignore = "exec-only complete enrollment protocol peer"]
fn enrollment_child() {
    let mut stream = child_stream();
    let mut request = Request::decode(&read_frame(&mut stream)).unwrap();
    loop {
        let mut frame = vec![0x10];
        frame.extend_from_slice(&[16; 32]);
        frame.extend_from_slice(&request.encode());
        send_frame(&mut stream, &frame);
        frame[0] = 0x11;
        assert_eq!(read_frame(&mut stream), frame);
        let Some(next) = request.following_enrollment_step().unwrap() else {
            break;
        };
        request = next;
    }
    let mut frame = vec![0x12];
    frame.extend_from_slice(&[18; 32]);
    frame.extend_from_slice(&request.encode());
    send_frame(&mut stream, &frame);
    frame[0] = 0x13;
    assert_eq!(read_frame(&mut stream), frame);
    send_frame(&mut stream, &[0x14]);
}

#[test]
fn enrollment_requires_every_step_without_renewing_the_operation_deadline() {
    for recovery in [Recovery::Unrecoverable, Recovery::SecondToken] {
        let initial = enrollment_request(recovery, 42);
        let mut supervisor = Unlock::spawn(initial.clone(), fixture("enrollment_child")).unwrap();
        let deadline = supervisor.deadline;
        let mut steps = vec![Enrollment::CreatePrimary, Enrollment::ProvePrimary];
        if recovery == Recovery::SecondToken {
            steps.extend([Enrollment::CreateRecovery, Enrollment::ProveRecovery]);
        }
        let mut last = initial;
        for step in steps {
            let expected = Request::new(
                [42; 32],
                1000,
                Operation::Enroll {
                    platform: Platform::TpmPcr7,
                    recovery,
                    step,
                },
            )
            .unwrap();
            assert_eq!(
                advance(&mut supervisor).unwrap(),
                Event::Present(expected.clone())
            );
            assert_eq!(supervisor.request(), &expected);
            assert_eq!(supervisor.deadline, deadline);
            assert!(supervisor.commit(&expected).is_err());
            supervisor.presented(&expected).unwrap();
            last = expected;
        }
        assert_eq!(
            advance(&mut supervisor).unwrap(),
            Event::Commit(last.clone())
        );
        supervisor.commit(&last).unwrap();
        assert_eq!(advance(&mut supervisor).unwrap(), Event::Complete);
    }
}

#[test]
#[ignore = "exec-only forged enrollment step peer"]
fn changed_enrollment_child() {
    let mut stream = child_stream();
    let initial = Request::decode(&read_frame(&mut stream)).unwrap();
    let mut frame = vec![0x10];
    frame.extend_from_slice(&[16; 32]);
    frame.extend_from_slice(&initial.encode());
    send_frame(&mut stream, &frame);
    frame[0] = 0x11;
    assert_eq!(read_frame(&mut stream), frame);
    // 1 skipped step; 2 changed recovery; 3 changed nonce; 4 early commit;
    // 5 repeated step; 6 changed owner; 7 correct next description/wrong tag.
    let case = initial.nonce()[0];
    let step = match case {
        1 => Enrollment::CreateRecovery,
        5 => Enrollment::CreatePrimary,
        _ => Enrollment::ProvePrimary,
    };
    let changed = Request::new(
        if case == 3 {
            [99; 32]
        } else {
            *initial.nonce()
        },
        if case == 6 { 1001 } else { 1000 },
        Operation::Enroll {
            platform: Platform::TpmPcr7,
            recovery: if case == 2 {
                Recovery::Unrecoverable
            } else {
                Recovery::SecondToken
            },
            step,
        },
    )
    .unwrap();
    let mut frame = vec![if matches!(case, 4 | 7) { 0x12 } else { 0x10 }];
    frame.extend_from_slice(&[17; 32]);
    frame.extend_from_slice(&if case == 4 {
        initial.encode()
    } else {
        changed.encode()
    });
    send_frame(&mut stream, &frame);
    let _ = stream.read(&mut [0]);
}

#[test]
fn skipped_repeated_changed_or_early_commit_steps_are_reaped_and_relocked() {
    for case in 1..=7 {
        let initial = enrollment_request(Recovery::SecondToken, case);
        let mut supervisor =
            Unlock::spawn(initial.clone(), fixture("changed_enrollment_child")).unwrap();
        assert_eq!(
            advance(&mut supervisor).unwrap(),
            Event::Present(initial.clone())
        );
        supervisor.presented(&initial).unwrap();
        assert_eq!(
            advance(&mut supervisor).unwrap(),
            Event::Failed("private token prompt changed its request or skipped a step".into(),)
        );
        assert_eq!(supervisor.request(), &initial);
        assert!(supervisor.child.is_none());
    }
}

#[test]
fn a_previous_step_receipt_cannot_authorize_the_next_step() {
    let initial = enrollment_request(Recovery::SecondToken, 42);
    let mut supervisor = Unlock::spawn(initial.clone(), fixture("enrollment_child")).unwrap();
    assert_eq!(
        advance(&mut supervisor).unwrap(),
        Event::Present(initial.clone())
    );
    supervisor.presented(&initial).unwrap();
    let next = initial.following_enrollment_step().unwrap().unwrap();
    assert_eq!(advance(&mut supervisor).unwrap(), Event::Present(next));
    assert!(supervisor.presented(&initial).is_err());
    assert!(matches!(
        advance(&mut supervisor).unwrap(),
        Event::Failed(_)
    ));
    assert!(supervisor.child.is_none());
}

#[test]
fn canonical_operation_selects_the_fixed_worker_verb() {
    assert_eq!(
        operation_verb(&Operation::Unlock {
            role: Role::Primary
        })
        .unwrap(),
        "unlock-operation"
    );
    for recovery in [Recovery::Unrecoverable, Recovery::SecondToken] {
        let initial = enrollment_request(recovery, 42);
        assert_eq!(
            operation_verb(initial.operation()).unwrap(),
            "enroll-operation"
        );
        assert!(operation_verb(
            initial
                .following_enrollment_step()
                .unwrap()
                .unwrap()
                .operation()
        )
        .is_err());
    }
}

#[test]
#[ignore = "exec-only final proof with a presentation tag instead of commit"]
fn wrong_final_enrollment_tag_child() {
    let mut stream = child_stream();
    let initial = Request::decode(&read_frame(&mut stream)).unwrap();
    let last = initial.following_enrollment_step().unwrap().unwrap();
    for request in [&initial, &last] {
        let mut frame = vec![0x10];
        frame.extend_from_slice(&[16; 32]);
        frame.extend_from_slice(&request.encode());
        send_frame(&mut stream, &frame);
        frame[0] = 0x11;
        assert_eq!(read_frame(&mut stream), frame);
    }
    let mut frame = vec![0x10];
    frame.extend_from_slice(&[18; 32]);
    frame.extend_from_slice(&last.encode());
    send_frame(&mut stream, &frame);
    let _ = stream.read(&mut [0]);
}

#[test]
fn final_proof_description_with_a_presentation_tag_is_refused() {
    let initial = enrollment_request(Recovery::Unrecoverable, 42);
    let last = initial.following_enrollment_step().unwrap().unwrap();
    let mut supervisor =
        Unlock::spawn(initial.clone(), fixture("wrong_final_enrollment_tag_child")).unwrap();
    assert_eq!(
        advance(&mut supervisor).unwrap(),
        Event::Present(initial.clone())
    );
    supervisor.presented(&initial).unwrap();
    assert_eq!(
        advance(&mut supervisor).unwrap(),
        Event::Present(last.clone())
    );
    supervisor.presented(&last).unwrap();
    assert!(matches!(
        advance(&mut supervisor).unwrap(),
        Event::Failed(_)
    ));
    assert!(supervisor.child.is_none());
}
