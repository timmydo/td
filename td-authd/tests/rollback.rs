#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::elevation::tests::{Etc, V1};
use crate::login_tier::tests::Volume;
use crate::login_tier::{CURRENT, PREVIOUS};

fn id(fill: char) -> String {
    fill.to_string().repeat(64)
}

/// A volume whose selectors name `current` and `previous`.
pub(crate) fn volume(current: &str, previous: &str) -> Volume {
    let volume = Volume::new();
    volume.select(CURRENT, &format!("../deployments/{current}"));
    volume.select(PREVIOUS, &format!("../deployments/{previous}"));
    volume
}

fn granted() -> Result<Table, String> {
    Table::parse(V1)
}

fn started(volume: &Volume) -> Rollback {
    let selectors = admit(1000, granted(), volume.path()).unwrap();
    Rollback::drawn(1000, selectors, [7; 32], [0x00, 0xfd]).unwrap()
}

#[test]
fn the_table_is_consulted_before_the_selectors_and_both_before_a_description() {
    let pair = volume(&id('a'), &id('b'));
    let refused = |table: Result<Table, String>, uid: u32| admit(uid, table, pair.path()).err();
    // A missing or malformed table, a row for another account or for
    // another operation, refuses as the principal table's.
    let missing = Etc::new(None);
    assert_eq!(refused(missing.load(), 1000), Some(Refusal::Principal));
    assert_eq!(
        refused(Table::parse("td-elevation-v1\n1000\tsudo\n"), 1000),
        Some(Refusal::Principal)
    );
    assert_eq!(
        refused(
            Table::parse("td-elevation-v1\n1000\tset-hostname\tdeploy-publish\n"),
            1000
        ),
        Some(Refusal::Principal)
    );
    assert_eq!(refused(granted(), 1001), Some(Refusal::Principal));
    // The table's refusal comes first, even with no volume at all.
    assert_eq!(
        admit(1001, granted(), &pair.path().join("absent")).err(),
        Some(Refusal::Principal)
    );
    // Then the selectors: no volume, one deployment named twice, a
    // malformed or missing selector.
    assert_eq!(
        admit(1000, granted(), &pair.path().join("absent")).err(),
        Some(Refusal::Selectors)
    );
    let one = volume(&id('a'), &id('a'));
    assert_eq!(
        admit(1000, granted(), one.path()).err(),
        Some(Refusal::Selectors)
    );
    let malformed = volume(&id('a'), &id('b'));
    malformed.select(PREVIOUS, "../deployments/B");
    assert_eq!(
        admit(1000, granted(), malformed.path()).err(),
        Some(Refusal::Selectors)
    );
    let absent = Volume::new();
    absent.select(CURRENT, &format!("../deployments/{}", id('a')));
    assert_eq!(
        admit(1000, granted(), absent.path()).err(),
        Some(Refusal::Selectors)
    );
    // Each refusal is its `9d` byte.
    assert_eq!(Refusal::Busy.answer(), [0x9d, 0]);
    assert_eq!(Refusal::Principal.answer(), [0x9d, 1]);
    assert_eq!(Refusal::Selectors.answer(), [0x9d, 2]);
    // Admitted: the pair the selectors name.
    let selectors = admit(1000, granted(), pair.path()).unwrap();
    assert_eq!(
        (selectors.current.as_str(), selectors.previous.as_str()),
        (id('a').as_str(), id('b').as_str())
    );
}

#[test]
fn each_digit_is_two_plus_its_own_byte_modulo_eight() {
    let mut seen = [0usize; 8];
    for byte in 0..=u8::MAX {
        let key = approval_key([byte, byte.wrapping_add(3)]).unwrap();
        let [first, second] = key.digits();
        assert_eq!(first, b'2' + byte % 8);
        assert_eq!(second, b'2' + byte.wrapping_add(3) % 8);
        seen[usize::from(first - b'2')] += 1;
    }
    // Every digit from 2 to 9 equally likely: no byte is rejected.
    assert_eq!(seen, [32; 8]);
}

#[test]
fn the_description_is_the_pair_under_its_nonce_and_key() {
    let pair = volume(&id('c'), &id('d'));
    let rollback = started(&pair);
    let request = rollback.request();
    assert_eq!(request.nonce(), &[7; 32]);
    assert_eq!(request.owner(), 1000);
    let Operation::DeployRollback {
        key,
        current,
        previous,
    } = request.operation()
    else {
        panic!("not a rollback");
    };
    assert_eq!(key.digits(), *b"27");
    assert_eq!((current, previous), (&id('c'), &id('d')));
    assert_eq!(Request::decode(&request.encode()).unwrap(), request.clone());
    // Production draws a fresh nonce and key each time.
    let first = Rollback::start(1000, admit(1000, granted(), pair.path()).unwrap()).unwrap();
    let second = Rollback::start(1000, admit(1000, granted(), pair.path()).unwrap()).unwrap();
    assert_ne!(first.request().nonce(), second.request().nonce());
}

#[test]
fn a_rollback_needs_exact_presentation_then_a_single_commit() {
    let pair = volume(&id('a'), &id('b'));
    let mut rollback = started(&pair);
    let request = rollback.request().clone();
    assert!(matches!(rollback.poll().unwrap(), Event::Present(_)));
    assert!(rollback
        .commit_with(&request, |_, _| panic!("unpresented"))
        .is_err());
    let wrong = Request::new([9; 32], 1000, request.operation().clone()).unwrap();
    assert!(rollback.presented(&wrong).is_err());
    rollback.presented(&request).unwrap();
    assert!(rollback.presented(&request).is_err());
    assert!(matches!(rollback.poll().unwrap(), Event::Commit(_)));
    assert!(rollback
        .commit_with(&wrong, |_, _| panic!("wrong request"))
        .is_err());
    // Escape, or a wrong digit's cancellation, before commit ends it.
    rollback.cancel().unwrap();
    assert!(rollback
        .commit_with(&request, |_, _| panic!("cancelled"))
        .is_err());
    assert!(matches!(rollback.poll().unwrap(), Event::Failed(_)));
    assert!(rollback.child.is_none());
    // Expiry, 120 seconds after selection.
    let mut expired = started(&pair);
    expired.deadline = Instant::now();
    assert!(expired.presented(&expired.request().clone()).is_err());
    assert!(matches!(expired.poll().unwrap(), Event::Failed(_)));
    assert_eq!(CONSENT, Duration::from_secs(120));
}

#[test]
#[ignore = "exec-only rollback helper"]
fn rollback_child() {
    let code = std::env::var("TD_ROLLBACK_CHILD_EXIT").map_or(0, |code| code.parse().unwrap());
    std::process::exit(code);
}

fn helper(exit: i32) -> io::Result<Child> {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rollback::tests::rollback_child", "--ignored"])
        .env("TD_ROLLBACK_CHILD_EXIT", exit.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn settle(rollback: &mut Rollback) -> Event {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = rollback.poll().unwrap();
        if !matches!(event, Event::Waiting) {
            return event;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The commit runs the helper once, on exactly the approved pair; only
/// its successful exit completes, a failing or unstarted one fails, and
/// the confirmation is consumed either way.
#[test]
fn commit_runs_the_helper_once_on_the_approved_pair() {
    for (exit, spawned, complete) in [(0, true, true), (3, true, false), (0, false, false)] {
        let pair = volume(&id('e'), &id('f'));
        let mut rollback = started(&pair);
        let request = rollback.request().clone();
        rollback.presented(&request).unwrap();
        let mut calls = Vec::new();
        rollback
            .commit_with(&request, |current, previous| {
                calls.push((current.to_string(), previous.to_string()));
                if spawned {
                    helper(exit)
                } else {
                    Err(io::Error::other("fixture spawn refusal"))
                }
            })
            .unwrap();
        // Closing the screen after commit revokes nothing; a replay refuses.
        rollback.cancel().unwrap();
        assert!(rollback
            .commit_with(&request, |_, _| panic!("replayed"))
            .is_err());
        assert_eq!(calls, [(id('e'), id('f'))]);
        assert_eq!(matches!(settle(&mut rollback), Event::Complete), complete);
        assert!(rollback.child.is_none());
    }
}

/// The race the pair closes from root's side: an update or another
/// rollback that moves either selector between selection and commit
/// leaves the helper unstarted; the commit is still consumed, so the
/// request can only fail.
#[test]
fn selectors_moved_after_selection_start_no_helper() {
    for (slot, moved) in [(CURRENT, id('9')), (PREVIOUS, id('8')), (CURRENT, id('b'))] {
        let pair = volume(&id('a'), &id('b'));
        let mut rollback = started(&pair);
        let request = rollback.request().clone();
        rollback.presented(&request).unwrap();
        pair.select(slot, &format!("../deployments/{moved}"));
        rollback
            .commit_with(&request, |_, _| panic!("the helper ran on a moved pair"))
            .unwrap();
        assert!(rollback
            .commit_with(&request, |_, _| panic!("replayed"))
            .is_err());
        assert!(matches!(rollback.poll().unwrap(), Event::Failed(_)));
    }
    // A selector removed since is no pair at all.
    let pair = volume(&id('a'), &id('b'));
    let mut rollback = started(&pair);
    let request = rollback.request().clone();
    rollback.presented(&request).unwrap();
    std::fs::remove_file(pair.path().join("boot").join(PREVIOUS)).unwrap();
    rollback
        .commit_with(&request, |_, _| panic!("the helper ran without a pair"))
        .unwrap();
    assert!(matches!(rollback.poll().unwrap(), Event::Failed(_)));
}

#[test]
fn teardown_kills_and_reaps_a_running_helper() {
    let pair = volume(&id('a'), &id('b'));
    let mut rollback = started(&pair);
    let request = rollback.request().clone();
    rollback.presented(&request).unwrap();
    rollback
        .commit_with(&request, |_, _| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "rollback::tests::stalled_child", "--ignored"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        })
        .unwrap();
    assert!(matches!(rollback.poll().unwrap(), Event::Waiting));
    rollback.reap_for_teardown().unwrap();
}

#[test]
#[ignore = "exec-only hung helper"]
fn stalled_child() {
    std::thread::sleep(Duration::from_secs(30));
}
