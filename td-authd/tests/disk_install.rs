#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use std::os::unix::net::UnixStream;

/// Stands in for `td-install serve`: idles until killed.
#[test]
#[ignore = "exec-only installation service stand-in"]
fn idle_service() {
    std::thread::sleep(Duration::from_secs(60));
}

fn idle_child() -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "disk_install::tests::idle_service", "--ignored"])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

pub(crate) fn review(nonce: u8, disk: &str) -> wire::Review {
    wire::Review::new(wire::ReviewFields {
        nonce: [nonce; 32],
        disk,
        capacity: 8 << 30,
        model: Some("QEMU HARDDISK"),
        serial: Some("se rial\u{e9}"),
        hostname: "td-laptop",
        username: "alice",
        deployment: [0xab; 32],
    })
    .unwrap()
}

/// A service over a socketpair, with its intake's directory; the returned
/// end plays the real service.
fn served_in() -> (Intake, UnixStream, std::path::PathBuf) {
    let directory = std::env::temp_dir().join(format!(
        "td-authd-setup-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).unwrap();
    let listener = UnixListener::bind(directory.join("setup")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    theirs
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let intake = Intake {
        listener,
        owner: 1000,
        installer: INSTALLER_UID,
        identity: (0, 0),
        service: Some(Service::over(idle_child(), ours).unwrap()),
        stopping: None,
        complete: false,
    };
    (intake, theirs, directory)
}

pub(crate) fn served() -> (Intake, UnixStream) {
    let (intake, service, directory) = served_in();
    let _ = fs::remove_dir_all(&directory);
    (intake, service)
}

fn greet(intake: &mut Intake, service: &mut UnixStream) {
    // td-authd writes its greeting when it next ticks.
    intake.tick();
    let mut greeting = [0; 8];
    service.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting, wire::GREETING);
    service.write_all(wire::GREETING).unwrap();
}

pub(crate) fn report(service: &mut UnixStream, report: Report) {
    service
        .write_all(&wire::frame(&report.encode()).unwrap())
        .unwrap();
}

pub(crate) fn answer(service: &mut UnixStream) -> Answer {
    let mut header = [0; 4];
    service.read_exact(&mut header).unwrap();
    let mut payload = vec![0; wire::payload_len(header).unwrap()];
    service.read_exact(&mut payload).unwrap();
    Answer::decode(&payload).unwrap()
}

/// Ticks until the service has an open review.
fn reviewed(intake: &mut Intake) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while intake.service.as_ref().unwrap().open.is_none() {
        assert!(Instant::now() < deadline);
        intake.tick();
        std::thread::yield_now();
    }
}

/// Ticks until the service is retired, returning the fates on the way.
fn retired(intake: &mut Intake) -> Vec<([u8; 32], Fate)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while intake.service.is_some() {
        assert!(Instant::now() < deadline);
        seen.extend(intake.tick());
        std::thread::yield_now();
    }
    seen
}

/// Ticks until `want` fates have arrived.
fn fates(intake: &mut Intake, want: usize) -> Vec<([u8; 32], Fate)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while seen.len() < want {
        assert!(Instant::now() < deadline, "only {seen:?}");
        seen.extend(intake.tick());
        std::thread::yield_now();
    }
    seen
}

fn selected(intake: &mut Intake) -> Request {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match intake.select() {
            Ok(request) => return request,
            Err(why) => assert!(Instant::now() < deadline, "{why}"),
        }
        std::thread::yield_now();
    }
}

#[test]
fn only_one_td_live_1_token_marks_a_live_boot() {
    for (cmdline, live) in [
        ("console=ttyS0 quiet", Some(false)),
        ("", Some(false)),
        ("td.live=1", Some(true)),
        ("root=x td.live=1 td.trust=ab", Some(true)),
        ("td.live=0", None),
        ("td.live=", None),
        ("td.live=11", None),
        ("td.live=1 td.live=1", None),
        ("td.live=1 td.live=0", None),
    ] {
        assert_eq!(live_marker(cmdline).ok(), live, "{cmdline}");
    }
    // A substring is not a token.
    assert!(!live_marker("xtd.live=1").unwrap());
}

#[test]
fn a_review_is_shown_once_as_its_escaped_summary() {
    let (mut intake, mut service) = served();
    greet(&mut intake, &mut service);
    assert!(intake.select().is_err());
    report(&mut service, Report::Review(Box::new(review(7, "vda"))));
    let request = selected(&mut intake);
    assert_eq!(request.nonce(), &[7; 32]);
    let Operation::InstallDisk {
        requester,
        disk,
        capacity,
        model,
        serial,
        hostname,
        username,
        deployment,
    } = request.operation()
    else {
        panic!("{request:?}")
    };
    assert_eq!(*requester, 1000);
    assert_eq!(disk, "vda");
    assert_eq!(*capacity, 8 << 30);
    assert_eq!(model, &Some(Label::model(b"QEMU HARDDISK")));
    assert_eq!(serial, &Some(Label::serial("se rial\u{e9}".as_bytes())));
    assert_eq!(hostname, "td-laptop");
    assert_eq!(username, "alice");
    assert_eq!(deployment, &[0xab; 8]);
    // Selected once; a second selection would show it twice.
    assert!(intake.select().is_err());
}

#[test]
fn a_review_consent_cannot_show_is_declined_as_unavailable() {
    let (mut intake, mut service) = served();
    greet(&mut intake, &mut service);
    // Within the channel's grammar, but not a name consent shows.
    let unshowable = wire::Review::new(wire::ReviewFields {
        nonce: [8; 32],
        disk: "vda",
        capacity: 1,
        model: None,
        serial: None,
        hostname: "td laptop",
        username: "alice",
        deployment: [0; 32],
    })
    .unwrap();
    report(&mut service, Report::Review(Box::new(unshowable)));
    reviewed(&mut intake);
    assert!(intake.select().is_err());
    assert!(
        intake
            .service
            .as_ref()
            .unwrap()
            .open
            .as_ref()
            .unwrap()
            .selected
    );
    assert_eq!(
        answer(&mut service),
        Answer::Declined([8; 32], NoConsent::Unavailable)
    );
}

#[test]
fn reports_out_of_order_break_the_channel_and_kill_the_service() {
    use wire::Ended::Withdrawn;
    let opened = || Report::Review(Box::new(review(1, "vda")));
    // Each case: reports before an answer, the answer, reports after.
    for (before, consent, after) in [
        (vec![Report::Started([1; 32])], None, vec![]),
        (vec![opened(), opened()], None, vec![]),
        (
            vec![opened(), Report::Ended([2; 32], Withdrawn)],
            None,
            vec![],
        ),
        // Started or finished without consent.
        (vec![opened(), Report::Started([1; 32])], None, vec![]),
        (
            vec![opened(), Report::Finished([1; 32], Outcome::Complete)],
            None,
            vec![],
        ),
        (
            vec![opened()],
            Some(Answer::Declined([1; 32], NoConsent::Declined)),
            vec![Report::Started([1; 32])],
        ),
        // Finished before started, started twice, ended after started.
        (
            vec![opened()],
            Some(Answer::Consent([1; 32])),
            vec![Report::Finished([1; 32], Outcome::Complete)],
        ),
        (
            vec![opened()],
            Some(Answer::Consent([1; 32])),
            vec![Report::Started([1; 32]), Report::Started([1; 32])],
        ),
        (
            vec![opened()],
            Some(Answer::Consent([1; 32])),
            vec![Report::Started([1; 32]), Report::Ended([1; 32], Withdrawn)],
        ),
    ] {
        let (mut intake, mut service) = served();
        greet(&mut intake, &mut service);
        for each in before {
            report(&mut service, each);
        }
        if let Some(sent) = consent {
            reviewed(&mut intake);
            intake.answer(sent).unwrap();
            assert_eq!(answer(&mut service), sent);
        }
        for each in after {
            report(&mut service, each);
        }
        let seen = retired(&mut intake);
        assert!(
            !seen.contains(&([1; 32], Fate::Finished(Outcome::Complete))),
            "{seen:?}"
        );
        assert!(!intake.complete);
        // The broken service was killed, and is reaped without blocking.
        let deadline = Instant::now() + Duration::from_secs(20);
        while intake.stopping.is_some() {
            assert!(Instant::now() < deadline);
            intake.tick();
            std::thread::yield_now();
        }
    }
    // A wrong greeting too.
    let (mut intake, mut service) = served();
    service.write_all(b"TDINA02\n").unwrap();
    retired(&mut intake);
}

#[test]
fn a_lost_service_ends_its_open_review() {
    let (mut intake, mut service) = served();
    greet(&mut intake, &mut service);
    report(&mut service, Report::Review(Box::new(review(3, "vda"))));
    selected(&mut intake);
    drop(service);
    assert_eq!(retired(&mut intake), [([3; 32], Fate::Ended)]);
    // Closing the channel is not a violation: the child is left to exit,
    // and no other service starts until it is reaped.
    let stopping = intake.stopping.as_mut().unwrap();
    assert!(!stopping.exited());
}

#[test]
fn a_finished_report_survives_the_service_closing_or_exiting() {
    // A service that closes with td-authd's answer unread resets the
    // channel: that is a close, not a violation.
    for end in ["close", "reset", "exit"] {
        let (mut intake, mut service) = served();
        greet(&mut intake, &mut service);
        report(&mut service, Report::Review(Box::new(review(2, "vda"))));
        reviewed(&mut intake);
        intake.answer(Answer::Consent([2; 32])).unwrap();
        if end != "reset" {
            assert_eq!(answer(&mut service), Answer::Consent([2; 32]));
        }
        report(&mut service, Report::Started([2; 32]));
        report(&mut service, Report::Finished([2; 32], Outcome::Complete));
        if end == "exit" {
            let child = &mut intake.service.as_mut().unwrap().child;
            child.kill().unwrap();
            child.wait().unwrap();
        } else {
            drop(service);
        }
        // The end follows the reports; a sibling test's fork may hold the
        // channel for a moment, so it may come a tick later.
        assert_eq!(
            retired(&mut intake),
            [
                ([2; 32], Fate::Started),
                ([2; 32], Fate::Finished(Outcome::Complete))
            ],
            "{end}"
        );
        assert!(intake.complete);
        if end != "exit" {
            // Retired, not killed: a killed child would be gone by now.
            std::thread::sleep(Duration::from_millis(300));
            assert!(!intake.stopping.as_mut().unwrap().exited(), "{end}");
        }
    }
}

/// Only the wizard's identity is served: a peer of the session owner, the
/// requester its consent names, is not, even when the intake is idle.
#[test]
fn only_the_installer_identity_is_served() {
    let (mut intake, _service, directory) = served_in();
    let _ = fs::remove_dir_all(&directory);
    let uid = fs::metadata("/proc/self").unwrap().uid();
    let (peer, _) = UnixStream::pair().unwrap();
    // Busy.
    intake.installer = uid;
    assert!(!intake.admits(&peer));
    intake.service = None;
    assert!(intake.admits(&peer));
    intake.owner = uid;
    intake.installer = uid.wrapping_add(1);
    assert!(!intake.admits(&peer));
    assert_eq!(INSTALLER_UID, 990);
}

#[test]
fn another_installer_is_refused_while_one_is_served_or_after_completion() {
    let (mut intake, _service, directory) = served_in();
    intake.installer = fs::metadata("/proc/self").unwrap().uid();
    let refused = |intake: &mut Intake| {
        let mut installer = UnixStream::connect(directory.join("setup")).unwrap();
        intake.tick();
        installer
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        // Closed unanswered.
        assert_eq!(installer.read(&mut [0; 1]).unwrap(), 0);
    };
    // Busy with a service.
    refused(&mut intake);
    assert!(intake.service.is_some());
    // Busy reaping one.
    intake.stopping = intake.service.take();
    refused(&mut intake);
    // Done.
    intake.stopping = None;
    intake.complete = true;
    refused(&mut intake);
    assert!(intake.service.is_none());
    let _ = fs::remove_dir_all(&directory);
}

#[test]
fn consent_flows_to_the_service_and_its_reports_to_the_operation() {
    let (mut intake, mut service) = served();
    greet(&mut intake, &mut service);
    report(&mut service, Report::Review(Box::new(review(4, "vda"))));
    let request = selected(&mut intake);
    let mut operation = Installation::start(request.clone()).unwrap();
    assert_eq!(operation.poll().unwrap(), Event::Present(request.clone()));
    // Enter before presentation does not consent.
    assert!(operation.commit(&request).is_err());
    operation.presented(&request).unwrap();
    assert!(operation.presented(&request).is_err());
    assert_eq!(operation.poll().unwrap(), Event::Commit(request.clone()));
    operation.commit(&request).unwrap();
    assert!(operation.commit(&request).is_err());
    intake.answer(operation.take_answer().unwrap()).unwrap();
    assert!(operation.take_answer().is_none());
    intake.tick();
    assert_eq!(answer(&mut service), Answer::Consent([4; 32]));
    // Escape after commit revokes nothing.
    operation.cancel("physical attention cancelled").unwrap();
    assert!(operation.take_answer().is_none());
    assert_eq!(operation.poll().unwrap(), Event::Waiting);
    report(&mut service, Report::Started([4; 32]));
    report(&mut service, Report::Finished([4; 32], Outcome::Complete));
    for (nonce, fate) in fates(&mut intake, 2) {
        operation.fate(&nonce, fate);
    }
    assert_eq!(operation.poll().unwrap(), Event::Complete);
    // The generation installs once: no new service is started.
    assert!(intake.complete);
}

#[test]
fn escape_or_expiry_before_commit_declines() {
    let request = Request::new([5; 32], 1000, {
        let review = review(5, "vda");
        summary(&review, 1000).unwrap().operation().clone()
    })
    .unwrap();
    let mut operation = Installation::start(request.clone()).unwrap();
    operation.presented(&request).unwrap();
    operation.cancel("physical attention cancelled").unwrap();
    assert_eq!(
        operation.take_answer(),
        Some(Answer::Declined([5; 32], NoConsent::Declined))
    );
    assert!(matches!(operation.poll().unwrap(), Event::Failed(_)));
    assert!(operation.ended());

    let mut operation = Installation::start(request.clone()).unwrap();
    operation.deadline = Instant::now();
    assert!(matches!(operation.poll().unwrap(), Event::Failed(_)));
    assert_eq!(
        operation.take_answer(),
        Some(Answer::Declined([5; 32], NoConsent::Expired))
    );
    // A late presentation changes nothing, and Enter consents to nothing.
    operation.presented(&request).unwrap();
    assert!(operation.commit(&request).is_err());
    assert!(operation.take_answer().is_none());
}

#[test]
fn only_its_own_review_ends_the_operation() {
    let request = summary(&review(6, "vda"), 1000).unwrap();
    let mut operation = Installation::start(request.clone()).unwrap();
    operation.fate(&[9; 32], Fate::Ended);
    operation.fate(&[6; 32], Fate::Started);
    assert!(!operation.ended());
    operation.fate(&[6; 32], Fate::Ended);
    assert!(operation.ended());
    assert!(matches!(operation.poll().unwrap(), Event::Failed(_)));
    // No answer is owed to a review the service already ended.
    assert!(operation.take_answer().is_none());
    let mut failed = Installation::start(request.clone()).unwrap();
    failed.fate(&[6; 32], Fate::Finished(Outcome::Failed));
    assert!(matches!(failed.poll().unwrap(), Event::Failed(_)));
    // Only committed consent completes.
    let mut uncommitted = Installation::start(request.clone()).unwrap();
    uncommitted.fate(&[6; 32], Fate::Finished(Outcome::Complete));
    assert!(matches!(uncommitted.poll().unwrap(), Event::Failed(_)));
    // Presenting a review the service already ended does nothing; poll
    // reports the failure.
    let mut ended = Installation::start(request.clone()).unwrap();
    ended.fate(&[6; 32], Fate::Ended);
    ended.presented(&request).unwrap();
    assert!(ended.commit(&request).is_err());
    assert!(matches!(ended.poll().unwrap(), Event::Failed(_)));
    // Only a whole-disk summary is an installation of this kind.
    let unlock = Request::new(
        [1; 32],
        1000,
        Operation::Unlock {
            role: crate::consent::Role::Primary,
        },
    )
    .unwrap();
    assert!(Installation::start(unlock).is_err());
}
