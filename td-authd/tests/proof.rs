#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use crate::consent::{LoginStep, Operation, Proof, Slot};
use crate::revocation::tests::WAIT;
use std::thread;
use std::time::Duration;

const NONCE: [u8; 32] = [42; 32];
pub(crate) const A: Fingerprint = [0xa1; 4];
pub(crate) const B: Fingerprint = [0xb2; 4];
/// A header fido2 token's fingerprint.
pub(crate) const T: Fingerprint = [0x7d; 4];
/// The recovery key the compositor's field sends: 48 digits.
pub(crate) const RECOVERY: &[u8; 48] = b"012345678901234567890123456789012345678901234567";

/// An addition to one key authorized by `proof`.
pub(crate) fn add_by(proof: Proof, step: LoginStep) -> Operation {
    Operation::LoginAdd {
        account: 1000,
        before: 1,
        after: 2,
        step,
        proof: Some(proof),
    }
}

/// The removal of B from A and B by `proof`, which leaves one key and so
/// discloses on the proof's first step.
pub(crate) fn remove_by(proof: Proof, step: LoginStep) -> Operation {
    Operation::LoginRemove {
        account: 1000,
        before: 2,
        after: 1,
        removed: vec![Slot {
            position: 2,
            key: B,
        }],
        step,
        approval: None,
        proof: Some(proof),
    }
    .disclosed(crate::login::tests::approval())
}

pub(crate) fn described(operation: Operation) -> Request {
    Request::new(NONCE, 1000, operation).unwrap()
}

fn removal() -> Selection {
    Selection::Remove(vec![Slot {
        position: 2,
        key: B,
    }])
}

fn start(
    selection: Selection,
    protected: bool,
    login: &'static str,
    storage: &'static str,
) -> Proving {
    let approval = crate::login::tests::approval();
    Proving::begin(1000, NONCE, protected, selection, |owner, selection| {
        Login::spawn(
            owner,
            NONCE,
            approval,
            selection,
            true,
            crate::login::tests::fixture(login),
        )
    })
    .unwrap()
    .storage_with(Box::new(move |owner, nonce, first, baseline, deadline| {
        Storage::spawn_proof(
            owner,
            nonce,
            first,
            baseline,
            deadline,
            crate::storage::tests::fixture(storage),
        )
    }))
}

/// Root's side as the compositor drives it: every presentation
/// acknowledged, a recovery-key step given `RECOVERY` and every other
/// secret step `1234`, every commit sent.
fn drive(proving: &mut Proving) -> (Event, Vec<Request>) {
    let until = Instant::now() + WAIT;
    let mut presented = Vec::new();
    loop {
        match proving.poll().unwrap() {
            Event::Present(request) => {
                proving.presented(&request).unwrap();
                presented.push(request);
            }
            Event::Pin(request) => {
                let secret: &[u8] = if request.login_step() == Some(LoginStep::RecoveryKey) {
                    RECOVERY
                } else {
                    b"1234"
                };
                assert!(proving.pin(&request, Pin::new(secret).unwrap()).unwrap());
            }
            Event::Commit(request) => proving.commit(&request).unwrap(),
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => return (event, presented),
        }
        assert!(Instant::now() < until, "disk proof stalled");
    }
}

fn ended(kind: u8, detail: u8) -> Event {
    Event::Ended(End {
        uncertain: false,
        kind,
        detail,
    })
}

/// A recovery-key addition: the storage worker presents the recovery-key
/// step and takes the 48 digits; only after its success and exit does the
/// login worker take that step as its first frame and run the new key's
/// ceremony to its commit.
#[test]
fn a_recovery_key_addition_runs_the_proof_then_the_new_key() {
    let selection = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Add));
    let mut proving = start(selection, true, "proven-add", "proof-recovery");
    let (event, presented) = drive(&mut proving);
    assert_eq!(event, Event::Complete);
    let step = |step| described(add_by(Proof::RecoveryKey, step));
    let n = [0x4e; 4];
    assert_eq!(
        presented,
        [
            step(LoginStep::RecoveryKey),
            step(LoginStep::Connect),
            step(LoginStep::Create { retries: 7 }),
            step(LoginStep::Prove { key: n, retries: 7 }),
            step(LoginStep::Repeat { key: n, retries: 7 }),
            step(LoginStep::Probe { key: n }),
        ]
    );
    assert!(proving.stage_is_resumed());
}

/// A removal by disk key: identify, then the authorize step naming the
/// header token, then the login worker's commit over that step.
#[test]
fn a_disk_key_removal_commits_over_its_authorize_step() {
    let selection = Selection::Proven(Proof::DiskKey, Box::new(removal()));
    let mut proving = start(selection, true, "proven-remove", "proof-disk-remove");
    let (event, presented) = drive(&mut proving);
    assert_eq!(event, Event::Complete);
    let authorize = described(remove_by(
        Proof::DiskKey,
        LoginStep::DiskAuthorize { key: T, retries: 8 },
    ));
    let identify = described(remove_by(Proof::DiskKey, LoginStep::DiskIdentify));
    // The first step carries the disclosure's key.
    assert!(identify.login_approval().is_some());
    assert_eq!(presented, [identify, authorize.clone()]);
    assert_eq!(proving.request(), Some(&authorize));
}

/// A machine that is not protected has no disk proof: `86` before any
/// worker starts.
#[test]
fn an_unprotected_machine_ends_a_proof_before_any_worker() {
    let selection = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Add));
    let mut proving = start(selection, false, "proven-hold", "proof-hold");
    assert!(proving.login_pid().is_none() && proving.storage_pid().is_none());
    assert_eq!(drive(&mut proving).0, ended(NO_PROOF, 0));
    assert_eq!(NO_PROOF, 0x86);
    assert!(proving.request().is_none());
}

/// The storage worker's baseline refuses before it sees a description:
/// `20` for no encrypted td volume or a refused header, `21` for an
/// upgrading volume or one that does not offer the chosen proof. Both
/// workers are reaped.
#[test]
fn the_volume_refuses_a_proof_it_cannot_offer() {
    let add = |proof| Selection::Proven(proof, Box::new(Selection::Add));
    for (selection, storage, kind) in [
        (add(Proof::RecoveryKey), "proof-volume:0", 0x20),
        (add(Proof::RecoveryKey), "proof-volume:5", 0x20),
        (add(Proof::RecoveryKey), "proof-volume:2", 0x21),
        (add(Proof::RecoveryKey), "proof-no-recovery", 0x21),
        (add(Proof::DiskKey), "proof-no-tokens", 0x21),
    ] {
        let started = Instant::now();
        let mut proving = start(selection, true, "proven-hold", storage);
        let (event, presented) = drive(&mut proving);
        assert_eq!(event, ended(kind, 0), "{storage}");
        assert!(presented.is_empty() && proving.request().is_none());
        // Both workers hold for `WAIT`: root killed and reaped them,
        // rather than waiting them out.
        assert!(
            started.elapsed() < WAIT / 2,
            "{storage}: {:?}",
            started.elapsed()
        );
    }
}

/// The login record's refusals come first, before the storage worker
/// starts; a first enrollment on a protected machine is admitted with a
/// proof.
#[test]
fn the_login_record_refuses_first() {
    let enroll = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Enroll(1)));
    let mut proving = start(enroll, true, "proven-hold", "proof-hold");
    assert_eq!(drive(&mut proving).0, ended(crate::login::ENROLLED, 0));
    assert!(proving.storage_pid().is_none());
    let all = Selection::Proven(
        Proof::RecoveryKey,
        Box::new(Selection::Remove(vec![Slot {
            position: 1,
            key: A,
        }])),
    );
    let mut proving = start(all, true, "proven-hold", "proof-hold");
    assert_eq!(drive(&mut proving).0, ended(crate::login::LAST, 0));
    assert!(proving.storage_pid().is_none());
}

/// The storage worker's failures end the operation with their kinds and
/// details; a `28` before the keyslot test is definite once the worker
/// exits successfully. The login worker, which wrote nothing, is killed.
#[test]
fn the_storage_workers_end_is_the_operations() {
    let add = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Add));
    for (storage, end) in [
        ("proof-fail:26", ended(0x26, 0)),
        ("proof-fail:25", ended(0x25, 0)),
        ("proof-fail:20", ended(0x20, 0)),
        ("proof-fail:08", ended(0x08, 0)),
        ("proof-changed", ended(0x28, 0)),
    ] {
        let mut proving = start(add.clone(), true, "proven-hold", storage);
        assert_eq!(drive(&mut proving).0, end, "{storage}");
        assert!(!proving.stage_is_resumed());
    }
    let remove = Selection::Proven(Proof::DiskKey, Box::new(removal()));
    let mut proving = start(remove, true, "proven-hold-two", "proof-wrong-pin");
    assert_eq!(drive(&mut proving).0, ended(0x01, 7));
}

/// A disk authorize step naming a login key, a step the volume does not
/// offer, and any frame from the login worker during the proof each end
/// the operation as INTERNAL.
#[test]
fn a_proof_step_root_does_not_admit_ends_the_operation() {
    let remove = || Selection::Proven(Proof::DiskKey, Box::new(removal()));
    for (login, storage) in [
        ("proven-hold-two", "proof-login-key"),
        ("proven-hold-two", "proof-stranger"),
        ("proven-early", "proof-disk-hold"),
        // A success before the proof's secret was sent.
        ("proven-hold-two", "proof-identify-success"),
    ] {
        let mut proving = start(remove(), true, login, storage);
        assert_eq!(
            drive(&mut proving).0,
            ended(INTERNAL, 0),
            "{login} {storage}"
        );
    }
}

/// The recovery key reaches root as exactly 48 digits, which it forwards
/// unparsed; anything else is a protocol violation.
#[test]
fn a_recovery_key_secret_must_be_48_digits() {
    for secret in [
        &RECOVERY[..47],
        b"0123456789012345678901234567890123456789012345678",
        b"01234567890123456789012345678901234567890123456a",
        b"012345-78901234567890123456789012345678901234567",
    ] {
        let add = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Add));
        let mut proving = start(add, true, "proven-hold", "proof-recovery");
        let until = Instant::now() + WAIT;
        let request = loop {
            match proving.poll().unwrap() {
                Event::Present(request) => proving.presented(&request).unwrap(),
                Event::Pin(request) => break request,
                Event::Waiting => thread::sleep(Duration::from_millis(1)),
                event => panic!("unexpected {event:?}"),
            }
            assert!(Instant::now() < until);
        };
        assert!(proving.pin(&request, Pin::new(secret).unwrap()).is_err());
        assert_eq!(drive(&mut proving).0, ended(INTERNAL, 0));
    }
}

/// A cancellation closes the storage worker's channel and gives it its
/// grace: a worker that reads the close exits on its own, as the keyslot
/// test's reaping needs; one that ignores it is killed once the grace
/// ends.
#[test]
fn a_cancelled_proof_closes_the_storage_channel_before_any_kill() {
    let dir = std::env::temp_dir().join(format!("td-authd-proof-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let witness = dir.join("closed");
    let script: &'static str =
        Box::leak(format!("proof-close-witness:{}", witness.display()).into_boxed_str());
    for (storage, closes) in [(script, true), ("proof-ignore", false)] {
        let add = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Add));
        let mut proving = start(add, true, "proven-hold", storage);
        let until = Instant::now() + WAIT;
        loop {
            match proving.poll().unwrap() {
                Event::Present(request) => proving.presented(&request).unwrap(),
                Event::Pin(_) => break,
                Event::Waiting => thread::sleep(Duration::from_millis(1)),
                event => panic!("unexpected {event:?}"),
            }
            assert!(Instant::now() < until);
        }
        let stopped = Instant::now();
        proving.cancel().unwrap();
        assert_eq!(drive(&mut proving).0, ended(CANCELLED, 0));
        let took = stopped.elapsed();
        if closes {
            assert_eq!(std::fs::read(&witness).unwrap(), b"closed");
            assert!(took < Duration::from_millis(900), "{took:?}");
        } else {
            assert!(took >= Duration::from_secs(1), "{took:?}");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Teardown gives a proof's storage worker the same grace.
#[test]
fn teardown_closes_the_storage_channel_first() {
    let dir = std::env::temp_dir().join(format!("td-authd-teardown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let witness = dir.join("closed");
    let script: &'static str =
        Box::leak(format!("proof-close-witness:{}", witness.display()).into_boxed_str());
    let add = Selection::Proven(Proof::RecoveryKey, Box::new(Selection::Add));
    let mut proving = start(add, true, "proven-hold", script);
    let until = Instant::now() + WAIT;
    while !matches!(proving.poll().unwrap(), Event::Present(_)) {
        thread::sleep(Duration::from_millis(1));
        assert!(Instant::now() < until);
    }
    let pid = proving.storage_pid().unwrap();
    proving.reap_for_teardown().unwrap();
    assert_eq!(std::fs::read(&witness).unwrap(), b"closed");
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
