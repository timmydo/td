#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::test_temp::TempDir;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

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
        storage: wire::Storage::Unencrypted,
    })
    .unwrap()
}

/// A service over a socketpair, with its intake's directory; the returned
/// end plays the real service.
fn served_in() -> (Intake, UnixStream, TempDir) {
    let directory = TempDir::new("td-authd-setup").unwrap();
    let listener = UnixListener::bind(directory.path().join("setup")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    theirs
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let intake = Intake {
        listener,
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
    directory.cleanup().unwrap();
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
fn the_live_wizards_execution_is_acknowledged_without_attention() {
    let (mut intake, mut service) = served();
    greet(&mut intake, &mut service);
    report(&mut service, Report::Review(Box::new(review(7, "vda"))));
    reviewed(&mut intake);
    assert_eq!(answer(&mut service), Answer::Consent([7; 32]));
    report(&mut service, Report::Started([7; 32]));
    report(&mut service, Report::Finished([7; 32], Outcome::Complete));
    assert_eq!(
        fates(&mut intake, 2),
        vec![
            ([7; 32], Fate::Started),
            ([7; 32], Fate::Finished(Outcome::Complete))
        ]
    );
    assert!(intake.complete);
}

#[test]
fn reports_out_of_order_break_the_channel_and_kill_the_service() {
    use wire::Ended::Withdrawn;
    let opened = || Report::Review(Box::new(review(1, "vda")));
    for reports in [
        vec![Report::Started([1; 32])],
        vec![opened(), opened()],
        vec![opened(), Report::Ended([2; 32], Withdrawn)],
        vec![opened(), Report::Finished([1; 32], Outcome::Complete)],
        vec![opened(), Report::Started([1; 32]), Report::Started([1; 32])],
        vec![
            opened(),
            Report::Started([1; 32]),
            Report::Ended([1; 32], Withdrawn),
        ],
    ] {
        let (mut intake, mut service) = served();
        greet(&mut intake, &mut service);
        for each in reports {
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
    service.write_all(b"TDINA01\n").unwrap();
    retired(&mut intake);
}

#[test]
fn a_lost_service_ends_its_open_review() {
    let (mut intake, mut service) = served();
    greet(&mut intake, &mut service);
    report(&mut service, Report::Review(Box::new(review(3, "vda"))));
    reviewed(&mut intake);
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
    directory.cleanup().unwrap();
    let uid = fs::metadata("/proc/self").unwrap().uid();
    let (peer, _) = UnixStream::pair().unwrap();
    // Busy.
    intake.installer = uid;
    assert!(!intake.admits(&peer));
    intake.service = None;
    assert!(intake.admits(&peer));
    intake.installer = uid.wrapping_add(1);
    assert!(!intake.admits(&peer));
    assert_eq!(INSTALLER_UID, 990);
}

#[test]
fn another_installer_is_refused_while_one_is_served_or_after_completion() {
    let (mut intake, _service, directory) = served_in();
    intake.installer = fs::metadata("/proc/self").unwrap().uid();
    let refused = |intake: &mut Intake| {
        let mut installer = UnixStream::connect(directory.path().join("setup")).unwrap();
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
    directory.cleanup().unwrap();
}
