#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use std::io::Write;
use std::os::fd::AsFd;

#[test]
#[ignore = "exec-only private inspection reply fixture"]
fn child() {
    let mut wire = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    let mut selector = [0];
    wire.read_exact(&mut selector).unwrap();
    match selector[0] {
        0..=3 => wire.write_all(&[0x17, selector[0]]).unwrap(),
        4 => wire.write_all(&[0x17, 4]).unwrap(),
        5 => wire.write_all(&[0x17]).unwrap(),
        6 => wire.write_all(&[0x17, 2, 0]).unwrap(),
        7 => {
            wire.write_all(&[0x17, 2]).unwrap();
            std::process::exit(7);
        }
        8 => std::thread::sleep(Duration::from_secs(30)),
        9 => {
            wire.write_all(&[0x17, 2]).unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        // The login helper's results (td-secret/DESIGN.md, "Login state").
        LOGIN_DAMAGED => wire.write_all(&[0x1a, 0]).unwrap(),
        LOGIN_TWO => wire
            .write_all(&[&[0x1a, 1, 1, 2][..], &[0xa1; 4], &[0xa2; 4]].concat())
            .unwrap(),
        LOGIN_EIGHT => {
            let mut bytes = vec![0x1a, 1, 1, 8];
            for key in 1..=8 {
                bytes.extend_from_slice(&[key; 4]);
            }
            wire.write_all(&bytes).unwrap();
        }
        LOGIN_OVERSIZED => {
            let mut bytes = vec![0x1a, 1, 1, 8];
            bytes.extend_from_slice(&[7; 33]);
            wire.write_all(&bytes).unwrap();
        }
        LOGIN_FAILED => std::process::exit(1),
        LOGIN_LATER => wire
            .write_all(&[&[0x1a, 1, 2, 1][..], &[0xa1; 4]].concat())
            .unwrap(),
        HELD => std::thread::sleep(Duration::from_secs(3)),
        PRINTS => {
            let mut header = [0; 3];
            wire.read_exact(&mut header).unwrap();
            let [code, hang, length] = header;
            let mut bytes = vec![0; usize::from(length)];
            wire.read_exact(&mut bytes).unwrap();
            let cwd = std::env::current_dir().unwrap();
            if std::env::vars_os().next().is_some() || cwd != std::path::Path::new("/") {
                std::process::exit(9);
            }
            wire.write_all(&bytes).unwrap();
            if hang != 0 {
                std::thread::sleep(Duration::from_secs(30));
            }
            if code != 0 {
                std::process::exit(code.into());
            }
        }
        _ => panic!("unknown fixture"),
    }
}

pub(crate) const LOGIN_DAMAGED: u8 = 10;
pub(crate) const LOGIN_TWO: u8 = 11;
pub(crate) const LOGIN_EIGHT: u8 = 12;
pub(crate) const LOGIN_OVERSIZED: u8 = 13;
pub(crate) const LOGIN_FAILED: u8 = 14;
/// One key, in a record of version 2.
pub(crate) const LOGIN_LATER: u8 = 16;
/// Writes nothing and outlives every deadline.
pub(crate) const STALLED: u8 = 8;
/// Writes nothing and outlives a short deadline, then exits; `stuck`'s.
const HELD: u8 = 15;
/// Exits 9 unless its environment is empty and it runs in `/`; then
/// writes the bytes `printed` sends and exits with its code or outlives
/// every deadline.
const PRINTS: u8 = 17;

pub(crate) fn fixture(selector: u8) -> Inspection {
    with_limit(selector, STORE_RESULT)
}

/// The child as the login helper, with its result bound.
pub(crate) fn login_fixture(selector: u8) -> Inspection {
    with_limit(selector, LOGIN_RESULT)
}

/// The login child `selector`, whose deadline is `after` from now.
pub(crate) fn expiring(selector: u8, after: Duration) -> Inspection {
    let mut inspection = login_fixture(selector);
    inspection.deadline = Instant::now() + after;
    inspection
}

/// A login helper past its deadline in `after` whose SIGKILL never takes
/// effect, as for a child in uninterruptible sleep: `try_wait` keeps
/// answering that it runs, until `release` or its own exit three seconds on.
pub(crate) fn stuck(after: Duration) -> Inspection {
    let mut inspection = expiring(HELD, after);
    inspection.stop = |_| Ok(());
    inspection
}

/// The kernel's kill reaches a `stuck` helper at last.
pub(crate) fn release(inspection: &mut Inspection) {
    inspection.stop = Child::kill;
}

/// The helper's pid while it is unreaped.
pub(crate) fn pid(inspection: &Inspection) -> Option<u32> {
    inspection.child.as_ref().map(Child::id)
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn child_command() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "inspection::tests::child", "--ignored"]);
    command
}

fn with_limit(selector: u8, limit: usize) -> Inspection {
    let mut inspection =
        Inspection::spawn(child_command(), limit, LIFETIME, Writes::Stdin).unwrap();
    inspection
        .wire
        .as_mut()
        .unwrap()
        .write_all(&[selector])
        .unwrap();
    inspection
}

fn finish(inspection: &mut Inspection) -> Event {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let result = inspection.poll().unwrap();
        if result != Event::Waiting {
            return result;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn bounded_result_needs_eof_and_successful_observed_exit() {
    for selector in 0..=7 {
        let mut operation = fixture(selector);
        let result = finish(&mut operation);
        if selector <= 3 {
            assert_eq!(
                result,
                Event::State(State::decode(&[0x17, selector]).unwrap())
            );
        } else {
            assert_eq!(result, Event::Unavailable);
        }
        assert!(operation.child.is_none());
        assert_eq!(operation.poll().unwrap(), result);
    }
}

#[test]
fn stalled_or_late_results_are_killed_and_reaped_before_unavailable() {
    for selector in [8, 9] {
        let mut operation = fixture(selector);
        operation.deadline = Instant::now();
        assert_eq!(finish(&mut operation), Event::Unavailable);
        assert!(operation.child.is_none());
    }
    let mut late = fixture(2);
    std::thread::sleep(Duration::from_millis(100));
    late.deadline = Instant::now();
    assert_eq!(finish(&mut late), Event::Unavailable);
    assert!(late.child.is_none());
}

#[test]
fn complete_bytes_alone_and_an_open_endpoint_never_report_state() {
    let (parent, _held) = UnixStream::pair().unwrap();
    parent.set_nonblocking(true).unwrap();
    let mut operation = Inspection {
        child: None,
        wire: Some(parent),
        bytes: vec![0x17, 3],
        limit: STORE_RESULT,
        eof: false,
        deadline: Instant::now() + LIFETIME,
        failed: false,
        exit_failed: false,
        missed: false,
        terminal: None,
        stop: Child::kill,
    };
    assert_eq!(operation.poll().unwrap(), Event::Waiting);
    operation.deadline = Instant::now();
    assert_eq!(operation.poll().unwrap(), Event::Unavailable);
}

#[test]
fn production_factory_refuses_wrong_owner_missing_binary_and_missing_reply() {
    assert!(Inspection::start(1001).is_err());
    assert!(Inspection::login(1001).is_err());
    assert!(Inspection::spawn(
        Command::new("/missing-inspection-fixture"),
        STORE_RESULT,
        LIFETIME,
        Writes::Stdin
    )
    .is_err());
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--list"]);
    let mut operation = Inspection::spawn(command, STORE_RESULT, LIFETIME, Writes::Stdin).unwrap();
    assert_eq!(finish(&mut operation), Event::Unavailable);
    assert!(operation.child.is_none());
}

#[test]
fn the_login_helper_answers_whole_bounded_bytes_or_nothing() {
    let mut damaged = login_fixture(LOGIN_DAMAGED);
    assert_eq!(damaged.wait(), Some(vec![0x1a, 0]));
    assert!(!damaged.missed());
    assert!(damaged.reaped());
    let two = login_fixture(LOGIN_TWO).wait().unwrap();
    assert_eq!(two.len(), 12);
    // The longest result, eight keys, is exactly the bound.
    let eight = login_fixture(LOGIN_EIGHT).wait().unwrap();
    assert_eq!(eight.len(), LOGIN_RESULT);
    // One byte past it, a failed exit after nothing, a store-sized bound
    // over a login result, and a child that is not the helper: nothing.
    for mut inspection in [
        login_fixture(LOGIN_OVERSIZED),
        login_fixture(LOGIN_FAILED),
        fixture(LOGIN_TWO),
        login_fixture(7),
    ] {
        inspection.deadline = Instant::now() + Duration::from_secs(4);
        assert_eq!(inspection.wait(), None);
        // The helper, not the deadline, ended these.
        assert!(!inspection.missed());
    }
}

#[test]
fn a_login_helper_past_its_two_second_deadline_is_killed_and_reaped() {
    assert_eq!(LIFETIME, Duration::from_secs(2));
    let started = Instant::now();
    let mut inspection = login_fixture(STALLED);
    let pid = inspection.child.as_ref().unwrap().id();
    assert_eq!(inspection.wait(), None);
    let waited = started.elapsed();
    assert!(waited >= LIFETIME, "{waited:?}");
    assert!(waited < LIFETIME + Duration::from_secs(1), "{waited:?}");
    assert!(inspection.missed());
    let until = Instant::now() + Duration::from_secs(1);
    while !inspection.reaped() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!alive(pid));
}

#[test]
fn a_stuck_helper_is_answered_at_its_deadline_and_kept_for_reaping() {
    let deadline = Duration::from_millis(200);
    let started = Instant::now();
    let mut inspection = stuck(deadline);
    let pid = inspection.child.as_ref().unwrap().id();
    assert_eq!(inspection.wait(), None);
    let waited = started.elapsed();
    assert!(waited >= deadline, "{waited:?}");
    assert!(waited < deadline + Duration::from_secs(1), "{waited:?}");
    assert!(inspection.missed());
    // Still running, and still owned: reaping never waits for it.
    for _ in 0..3 {
        let started = Instant::now();
        assert!(!inspection.reaped());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(alive(pid));
    }
    release(&mut inspection);
    let until = Instant::now() + Duration::from_secs(1);
    while !inspection.reaped() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!alive(pid));
}

#[test]
fn dropping_a_stuck_helper_never_waits_for_it() {
    // Running, and past its deadline and answered.
    for answered in [false, true] {
        let mut inspection = stuck(Duration::from_millis(50));
        if answered {
            assert_eq!(inspection.wait(), None);
        }
        let started = Instant::now();
        drop(inspection);
        let dropped = started.elapsed();
        assert!(dropped < Duration::from_secs(1), "{dropped:?}");
    }
}

/// A helper standing in for one that prints its result, as the
/// revocation's render and reboot request do: libtest's own report
/// occupies a fixture's standard output, so this one writes `bytes` to
/// its wire, then exits `code`, or outlives every deadline if `hang`.
pub(crate) fn printed(
    bytes: &[u8],
    code: u8,
    hang: bool,
    limit: usize,
    lifetime: Duration,
) -> Result<Inspection, String> {
    let mut inspection = Inspection::spawn(child_command(), limit, lifetime, Writes::Stdin)?;
    let length = u8::try_from(bytes.len()).map_err(|e| e.to_string())?;
    let header = [PRINTS, code, u8::from(hang), length];
    inspection
        .wire
        .as_mut()
        .ok_or("no wire")?
        .write_all(&[&header[..], bytes].concat())
        .map_err(|e| e.to_string())?;
    Ok(inspection)
}

/// `printing` reads the helper's standard output, under the same bound,
/// empty environment and working directory as every helper: libtest's
/// listing of one test is a fixed line on standard output.
#[test]
fn a_printing_helpers_result_is_its_standard_output() {
    let listing = b"inspection::tests::child: test\n";
    let list = || {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--list",
            "--format",
            "terse",
            "--exact",
            "inspection::tests::child",
            "--ignored",
        ]);
        command
    };
    let finished = |mut helper: Inspection| {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(result) = helper.finished().unwrap() {
                return result;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    let helper = Inspection::printing(list(), listing.len(), LIFETIME).unwrap();
    assert_eq!(finished(helper), Some(listing.to_vec()));
    let helper = Inspection::printing(list(), listing.len() - 1, LIFETIME).unwrap();
    assert_eq!(finished(helper), None, "one byte past the bound");
    // Its stdin is not the wire: the stdin fixture's selector never
    // arrives, and it writes nothing.
    let helper = Inspection::printing(child_command(), 2, LIFETIME).unwrap();
    assert_eq!(finished(helper), None);
    // The fixture's checks: an empty environment and `/`.
    let helper = printed(b"x\n", 0, false, 2, LIFETIME).unwrap();
    assert_eq!(finished(helper), Some(b"x\n".to_vec()));
    assert!(Inspection::printing(list(), READ_SIZE, LIFETIME).is_err());
}

/// `answered` reports a failed exit's output with its failure, which
/// `finished` never does; a helper the deadline ended is no answer, and
/// says it missed.
#[test]
fn an_answer_carries_a_failed_exits_output() {
    let answered = |mut helper: Inspection| {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(result) = helper.answered().unwrap() {
                return (result, helper.missed());
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    let helper = printed(b"error: no\n", 1, false, 16, LIFETIME).unwrap();
    assert_eq!(
        answered(helper),
        (Some((b"error: no\n".to_vec(), false)), false)
    );
    let helper = printed(b"", 1, false, 16, LIFETIME).unwrap();
    assert_eq!(answered(helper), (Some((Vec::new(), false)), false));
    let helper = printed(b"ok\n", 0, false, 16, LIFETIME).unwrap();
    assert_eq!(answered(helper), (Some((b"ok\n".to_vec(), true)), false));
    let helper = printed(b"x\n", 0, true, 16, Duration::from_millis(300)).unwrap();
    assert_eq!(answered(helper), (None, true));
    let mut helper = printed(b"error: no\n", 1, false, 16, LIFETIME).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    let result = loop {
        if let Some(result) = helper.finished().unwrap() {
            break result;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(result, None);
}
