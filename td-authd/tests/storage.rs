#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use std::io::Write;
use std::os::fd::AsFd;
use std::thread;

const SCRIPT: &str = "TD_STORAGE_SCRIPT";
const NONCE: [u8; 32] = [42; 32];
/// The running deployment's manifest ID in the scripted baselines.
const ID: [u8; 32] = [0xab; 32];

/// `storage::tests::scripted_worker` playing `script` on its stdin socket.
pub(crate) fn fixture(script: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "storage::tests::scripted_worker", "--ignored"])
        .env_clear()
        .env(SCRIPT, script)
        .current_dir("/")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The key `Storage::scripted` draws.
pub(crate) fn approval() -> ApprovalKey {
    ApprovalKey::new(*b"36").unwrap()
}

/// Root's description for `form` over `ID`.
pub(crate) fn update(form: SelectorForm) -> Request {
    Request::new(
        NONCE,
        1000,
        Operation::SelectorUpdate {
            key: approval(),
            form,
            current: "ab".repeat(32),
        },
    )
    .unwrap()
}

/// Root's upgrade description at `step` for `shape` and `count` over
/// `ID`: the review carries the key.
pub(crate) fn upgrade_at(shape: UpgradeShape, count: u8, step: UpgradeStep) -> Request {
    Request::new(
        NONCE,
        1000,
        Operation::Upgrade {
            account: 1000,
            shape,
            count,
            current: "ab".repeat(32),
            step,
            approval: (step == UpgradeStep::Review).then(approval),
        },
    )
    .unwrap()
}

/// Every step after the review of `count` new keys, in their only order,
/// the created credentials `0x0k0k0k0k`.
pub(crate) fn ceremony_steps(count: u8) -> Vec<UpgradeStep> {
    let mut steps = Vec::new();
    for key in 1..=count {
        let made = [key; 4];
        for step in [
            LoginStep::Connect,
            LoginStep::Create { retries: 8 },
            LoginStep::Prove {
                key: made,
                retries: 8,
            },
            LoginStep::Repeat {
                key: made,
                retries: 7,
            },
            LoginStep::Probe { key: made },
        ] {
            steps.push(UpgradeStep::Key { key, step });
        }
    }
    steps
}

/// Root's cancellation description.
pub(crate) fn cancellation() -> Request {
    Request::new(NONCE, 1000, Operation::UpgradeCancel { key: approval() }).unwrap()
}

/// A baseline frame: the volume byte, a recovery-key marker, the tokens,
/// each naming a keyslot, no tpm-pin, lockout or request, and the
/// standing.
pub(crate) fn baseline_frame(volume: u8, tokens: &[(u8, [u8; 4])], current: bool) -> Vec<u8> {
    let mut frame = vec![0x18, volume, 1, tokens.len() as u8];
    for (role, fingerprint) in tokens {
        frame.extend([*role, 1]);
        frame.extend_from_slice(fingerprint);
    }
    frame.extend([0, 0, 0]);
    if current {
        frame.push(0);
        frame.extend_from_slice(&ID);
    } else {
        frame.push(1);
    }
    frame
}

/// The worker's side of the frames, scripted.
struct Worker {
    stream: UnixStream,
    round: u8,
}

impl Worker {
    fn new() -> Self {
        let stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self { stream, round: 0 }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.stream
            .write_all(&(bytes.len() as u16).to_be_bytes())
            .unwrap();
        self.stream.write_all(bytes).unwrap();
    }

    fn read(&mut self) -> Vec<u8> {
        let mut header = [0; 2];
        self.stream.read_exact(&mut header).unwrap();
        let mut frame = vec![0; usize::from(u16::from_be_bytes(header))];
        self.stream.read_exact(&mut frame).unwrap();
        frame
    }

    fn frame(&mut self, tag: u8, request: &Request) -> Vec<u8> {
        self.round += 1;
        let mut frame = vec![tag];
        frame.extend_from_slice(&[self.round; 32]);
        frame.extend_from_slice(&request.encode());
        frame
    }

    /// One round, which root must acknowledge exactly.
    fn invite(&mut self, tag: u8, request: &Request) {
        let mut frame = self.frame(tag, request);
        self.send(&frame);
        frame[0] += 1;
        assert_eq!(self.read(), frame);
    }

    /// The baseline, then root's description, which must be `form`'s.
    fn begin(&mut self, volume: u8, form: SelectorForm) -> Request {
        self.send(&baseline_frame(volume, &[(1, [0xa1; 4])], true));
        let description = Request::decode(&self.read()).unwrap();
        assert_eq!(description, update(form));
        description
    }

    /// Through root's commit acknowledgement.
    fn committed(&mut self, volume: u8, form: SelectorForm) {
        let description = self.begin(volume, form);
        self.invite(0x10, &description);
        self.invite(0x12, &description);
    }

    /// Left for root to kill.
    fn hold(&mut self) {
        thread::sleep(Duration::from_secs(30));
    }
}

#[test]
#[ignore = "exec-only scripted storage worker"]
fn scripted_worker() {
    let script = std::env::var(SCRIPT).unwrap();
    let mut worker = Worker::new();
    let w = &mut worker;
    match script.as_str() {
        "staged" => {
            w.committed(3, SelectorForm::Staged);
            w.send(&[0x14]);
        }
        "device-bound" => {
            w.committed(1, SelectorForm::DeviceBound);
            w.send(&[0x14]);
        }
        "fido2" => {
            w.committed(4, SelectorForm::Fido2);
            w.send(&[0x14]);
        }
        "changed" => {
            w.committed(3, SelectorForm::Staged);
            w.send(&[0x15, 0x28]);
        }
        "changed-exit" => {
            w.committed(3, SelectorForm::Staged);
            w.send(&[0x15, 0x28]);
            std::process::exit(1);
        }
        "changed-trailing" => {
            w.committed(3, SelectorForm::Staged);
            w.send(&[0x15, 0x28]);
            w.send(&[0x14]);
        }
        "changed-early" => {
            let description = w.begin(3, SelectorForm::Staged);
            w.invite(0x10, &description);
            w.send(&[0x15, 0x28]);
            w.hold();
        }
        "uncertain-early" => {
            w.begin(3, SelectorForm::Staged);
            w.send(&[0x15, 0x0e]);
            w.hold();
        }
        "uncertain" => {
            w.committed(3, SelectorForm::Staged);
            w.send(&[0x15, 0x0e]);
        }
        "failed-after-commit" => {
            w.committed(1, SelectorForm::DeviceBound);
            w.send(&[0x15, 0x0f]);
        }
        "lost-after-commit" => w.committed(1, SelectorForm::DeviceBound),
        "success-exit" => {
            w.committed(3, SelectorForm::Staged);
            w.send(&[0x14]);
            std::process::exit(1);
        }
        "stall-after-commit" => {
            w.committed(3, SelectorForm::Staged);
            w.hold();
        }
        "deployment" | "esp" | "margin" => {
            w.begin(3, SelectorForm::Staged);
            w.send(&[
                0x15,
                match script.as_str() {
                    "deployment" => 0x23,
                    "esp" => 0x24,
                    _ => 0x08,
                },
            ]);
            w.hold();
        }
        "presented" => {
            let description = w.begin(3, SelectorForm::Staged);
            let frame = w.frame(0x10, &description);
            w.send(&frame);
            w.hold();
        }
        "changed-description" => {
            w.begin(3, SelectorForm::Staged);
            let other = update(SelectorForm::DeviceBound);
            let frame = w.frame(0x10, &other);
            w.send(&frame);
            w.hold();
        }
        "early-commit" => {
            let description = w.begin(3, SelectorForm::Staged);
            let frame = w.frame(0x12, &description);
            w.send(&frame);
            w.hold();
        }
        "early-success" => {
            let description = w.begin(3, SelectorForm::Staged);
            w.invite(0x10, &description);
            w.send(&[0x14]);
            w.hold();
        }
        refusal if refusal.starts_with("baseline:") => {
            let frame: Vec<u8> = match refusal.trim_start_matches("baseline:") {
                "none" => baseline_frame(0, &[], true),
                "refused" => baseline_frame(5, &[], true),
                "upgrading" => baseline_frame(2, &[], true),
                "not-current" => baseline_frame(3, &[(1, [1; 4])], false),
                // A volume byte past 05, a recovery byte past 01, five
                // tokens, a role past 02, a keyslot byte past 01, three
                // tpm-pin tokens, a lockout byte past 01, a request byte
                // past 03, a short ID and a trailing byte.
                "volume" => baseline_frame(6, &[], true),
                "recovery" => {
                    let mut frame = baseline_frame(1, &[], true);
                    frame[2] = 2;
                    frame
                }
                "tokens" => baseline_frame(4, &[(1, [1; 4]); 5], true),
                "role" => baseline_frame(4, &[(3, [1; 4])], true),
                "named" => {
                    let mut frame = baseline_frame(4, &[(1, [1; 4])], true);
                    frame[5] = 2;
                    frame
                }
                field @ ("tpm-pins" | "lockout" | "request") => {
                    let mut frame = baseline_frame(4, &[], true);
                    let (at, byte) = match field {
                        "tpm-pins" => (4, 3),
                        "lockout" => (5, 2),
                        _ => (6, 4),
                    };
                    frame[at] = byte;
                    frame
                }
                "short" => {
                    let mut frame = baseline_frame(1, &[], true);
                    frame.pop();
                    frame
                }
                "trailing" => {
                    let mut frame = baseline_frame(1, &[], false);
                    frame.push(0);
                    frame
                }
                other => panic!("unknown baseline {other}"),
            };
            w.send(&frame);
            w.hold();
        }
        proof if proof.starts_with("proof-") => proof_script(w, proof),
        protector if protector.starts_with("protector-") => protector_script(w, protector),
        // Holds the channel it inherited, then ends.
        "linger" => thread::sleep(Duration::from_millis(300)),
        "linger-long" => thread::sleep(Duration::from_millis(1500)),
        upgrade if upgrade.starts_with("upgrade-") || upgrade.starts_with("cancel") => {
            upgrade_script(w, upgrade)
        }
        _ => panic!("unknown storage script {script}"),
    }
}

/// The storage worker's side of the upgrade's and the cancellation's
/// tests: `upgrade-SHAPE-COUNT-...`, or `cancel...`.
fn upgrade_script(w: &mut Worker, script: &str) {
    if let Some(rest) = script.strip_prefix("cancel") {
        let volume = match rest.strip_prefix("-baseline:") {
            Some(volume) => volume.parse().unwrap(),
            None => 2,
        };
        w.send(&baseline_frame(volume, &[], true));
        if volume != 2 {
            return w.hold();
        }
        let cancel = cancellation();
        assert_eq!(Request::decode(&w.read()).unwrap(), cancel);
        match rest {
            "" => {
                w.invite(0x10, &cancel);
                w.invite(0x12, &cancel);
                w.send(&[0x14]);
            }
            "-past-u0" => {
                w.send(&[0x15, 0x21]);
                w.hold();
            }
            "-changed" => {
                w.invite(0x10, &cancel);
                w.invite(0x12, &cancel);
                w.send(&[0x15, 0x28]);
            }
            // Its read finds the header changed: definite, before any
            // presentation, though a process it leaves holds its channel
            // a moment past its exit.
            "-changed-view" => w.send(&[0x15, 0x28]),
            "-changed-held" | "-changed-held-long" => {
                w.send(&[0x15, 0x28]);
                #[allow(
                    clippy::zombie_processes,
                    reason = "left holding the channel past this worker's exit"
                )]
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "storage::tests::scripted_worker", "--ignored"])
                    .env_clear()
                    .env(
                        SCRIPT,
                        if rest == "-changed-held" {
                            "linger"
                        } else {
                            "linger-long"
                        },
                    )
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
            }
            // A selector update's kind, which no cancellation sends.
            "-esp" => {
                w.send(&[0x15, 0x24]);
                w.hold();
            }
            _ => panic!("unknown cancellation script {script}"),
        }
        return;
    }
    let mut parts = script.split('-').skip(1);
    let shape = match parts.next() {
        Some("tpm") => UpgradeShape::Tpm,
        _ => UpgradeShape::Fido2,
    };
    let count: u8 = parts.next().unwrap().parse().unwrap();
    let rest: Vec<&str> = parts.collect();
    let rest = rest.join("-");
    let tokens: &[(u8, [u8; 4])] = if rest == "collides" {
        &[(1, [1; 4])]
    } else {
        &[]
    };
    let volume = rest
        .strip_prefix("baseline:")
        .map_or(1, |volume| volume.parse().unwrap());
    w.send(&baseline_frame(volume, tokens, rest != "not-current"));
    if volume != 1 || rest == "not-current" {
        return w.hold();
    }
    let review = upgrade_at(shape, count, UpgradeStep::Review);
    assert_eq!(Request::decode(&w.read()).unwrap(), review);
    let step = |step| upgrade_at(shape, count, step);
    let steps = ceremony_steps(count);
    // Through the ceremony's steps before `upto`, each PIN step's secret
    // read.
    let ceremony = |w: &mut Worker, upto: usize| {
        for &key_step in steps.iter().take(upto) {
            w.invite(0x10, &step(key_step));
            if UpgradeStep::login(key_step).is_some_and(LoginStep::asks_pin) {
                assert_eq!(w.read(), b"\x161234");
            }
        }
    };
    match rest.as_str() {
        "" => {
            w.invite(0x10, &review);
            ceremony(w, steps.len());
            let last = steps.last().map_or(review.clone(), |&last| step(last));
            w.invite(0x12, &last);
            w.send(&[0x14]);
        }
        "changed-view" => {
            w.send(&[0x15, 0x28]);
        }
        "tpm-refused" => {
            w.send(&[0x15, 0x27, 0x02]);
            w.hold();
        }
        "wrong-pin" => {
            w.invite(0x10, &review);
            ceremony(w, 2);
            w.send(&[0x15, 0x01, 7]);
        }
        "excluded" => {
            w.invite(0x10, &review);
            ceremony(w, 1);
            w.send(&[0x15, 0x11]);
        }
        // Kinds out of their place: the TPM's and a read's during a key's
        // ceremony, and a key's before the review is presented.
        "tpm-at-key" | "read-at-key" => {
            w.invite(0x10, &review);
            ceremony(w, 1);
            w.send(if rest == "tpm-at-key" {
                &[0x15, 0x27, 0x02]
            } else {
                &[0x15, 0x21]
            });
            w.hold();
        }
        "key-at-review" => {
            w.send(&[0x15, 0x01, 7]);
            w.hold();
        }
        // The connect step presented, then held: root takes no secret for
        // it.
        "hold-connect" => {
            w.invite(0x10, &review);
            let frame = w.frame(0x10, &step(steps[0]));
            w.send(&frame);
            w.hold();
        }
        // The second key's prove names the first key's credential.
        "reused" => {
            w.invite(0x10, &review);
            ceremony(w, 7);
            let frame = w.frame(
                0x10,
                &step(UpgradeStep::Key {
                    key: 2,
                    step: LoginStep::Prove {
                        key: [1; 4],
                        retries: 8,
                    },
                }),
            );
            w.send(&frame);
            w.hold();
        }
        "uncertain" => {
            w.invite(0x10, &review);
            ceremony(w, steps.len());
            let last = steps.last().map_or(review.clone(), |&last| step(last));
            w.invite(0x12, &last);
            w.send(&[0x15, 0x0e]);
        }
        // Out of root's order: a skipped connect, an early commit, a
        // credential the header names, a repeat of another credential and
        // a commit naming the review after the keys.
        "skip" => {
            w.invite(0x10, &review);
            let frame = w.frame(0x10, &step(steps[1]));
            w.send(&frame);
            w.hold();
        }
        "early-commit" => {
            w.invite(0x10, &review);
            let frame = w.frame(0x12, &review);
            w.send(&frame);
            w.hold();
        }
        "collides" => {
            w.invite(0x10, &review);
            ceremony(w, 2);
            let frame = w.frame(0x10, &step(steps[2]));
            w.send(&frame);
            w.hold();
        }
        "switched" => {
            w.invite(0x10, &review);
            ceremony(w, 3);
            let frame = w.frame(
                0x10,
                &step(UpgradeStep::Key {
                    key: 1,
                    step: LoginStep::Repeat {
                        key: [9; 4],
                        retries: 7,
                    },
                }),
            );
            w.send(&frame);
            w.hold();
        }
        "stale-commit" => {
            w.invite(0x10, &review);
            ceremony(w, steps.len());
            let frame = w.frame(0x12, &review);
            w.send(&frame);
            w.hold();
        }
        _ => panic!("unknown upgrade script {script}"),
    }
}

/// Polls `storage`, acknowledging the presentation and the commit, until it
/// ends: its end and what it presented.
fn drive(storage: &mut Storage) -> (Event, Vec<Request>) {
    let until = Instant::now() + Duration::from_secs(10);
    let mut presented = Vec::new();
    loop {
        match storage.poll().unwrap() {
            Event::Present(request) => {
                storage.presented(&request).unwrap();
                presented.push(request);
            }
            Event::Commit(request) => storage.commit(&request).unwrap(),
            Event::Pin(request) => {
                let secret = secret_of(&request).unwrap();
                assert!(storage
                    .pin(&request, Pin::entry(&secret[1..]).unwrap())
                    .unwrap());
            }
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => return (event, presented),
        }
        assert!(Instant::now() < until, "storage supervisor stalled");
    }
}

fn ended(uncertain: bool, kind: u8) -> Event {
    Event::Ended(End {
        uncertain,
        kind,
        detail: 0,
    })
}

fn scripted(script: &str) -> Storage {
    Storage::scripted(script).unwrap()
}

#[test]
fn a_selector_update_takes_its_form_from_the_volume_and_completes_on_exit() {
    for (script, form) in [
        ("staged", SelectorForm::Staged),
        ("device-bound", SelectorForm::DeviceBound),
        ("fido2", SelectorForm::Fido2),
    ] {
        let mut storage = scripted(script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, Event::Complete, "{script}");
        assert_eq!(presented, [update(form)]);
        assert!(storage.child.is_none() && storage.wire.is_none());
        assert_eq!(storage.poll(), Ok(Event::Complete));
    }
}

#[test]
fn the_baseline_refuses_before_any_description() {
    for (script, kind) in [
        ("baseline:none", VOLUME),
        ("baseline:refused", VOLUME),
        ("baseline:upgrading", STATE),
        ("baseline:not-current", NOT_CURRENT),
        // A malformed baseline is root's protocol failure.
        ("baseline:volume", INTERNAL),
        ("baseline:recovery", INTERNAL),
        ("baseline:tokens", INTERNAL),
        ("baseline:role", INTERNAL),
        ("baseline:named", INTERNAL),
        ("baseline:tpm-pins", INTERNAL),
        ("baseline:lockout", INTERNAL),
        ("baseline:request", INTERNAL),
        ("baseline:short", INTERNAL),
        ("baseline:trailing", INTERNAL),
    ] {
        let started = Instant::now();
        let mut storage = scripted(script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, ended(false, kind), "{script}");
        assert!(presented.is_empty() && storage.request.is_none());
        // The worker holds for thirty seconds: root killed and reaped it.
        assert!(storage.child.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

/// Amendment 10: after root's `13`, only success and a `28` the worker
/// follows with its successful exit are definite.
#[test]
fn after_the_commit_only_a_changed_disk_with_a_clean_exit_is_definite() {
    for (script, end) in [
        ("changed", ended(false, CHANGED)),
        ("changed-exit", ended(true, INTERNAL)),
        ("changed-trailing", ended(true, INTERNAL)),
        ("uncertain", ended(true, UNCERTAIN)),
        ("failed-after-commit", ended(true, FAILED)),
        ("lost-after-commit", ended(true, INTERNAL)),
        ("success-exit", ended(true, INTERNAL)),
    ] {
        let mut storage = scripted(script);
        assert_eq!(drive(&mut storage).0, end, "{script}");
        assert!(storage.child.is_none());
    }
    // Root's deadline and a cancellation after the acknowledgement.
    for cancel in [false, true] {
        let mut storage = scripted("stall-after-commit");
        let until = Instant::now() + Duration::from_secs(10);
        while storage.phase != Phase::Completion {
            match storage.poll().unwrap() {
                Event::Present(request) => storage.presented(&request).unwrap(),
                Event::Commit(request) => storage.commit(&request).unwrap(),
                Event::Waiting => thread::sleep(Duration::from_millis(1)),
                event => panic!("unexpected {event:?}"),
            }
            assert!(Instant::now() < until);
        }
        if cancel {
            storage.cancel().unwrap();
        } else {
            storage.expire();
        }
        let kind = if cancel { CANCELLED } else { TIMEOUT };
        assert_eq!(drive(&mut storage).0, ended(true, kind));
    }
}

#[test]
fn before_the_commit_ends_are_definite_and_out_of_order_frames_fail() {
    for (script, end) in [
        ("deployment", ended(false, 0x23)),
        ("esp", ended(false, 0x24)),
        ("margin", ended(false, TIMEOUT)),
        // `28` and `0e` exist only after root's commit acknowledgement.
        ("changed-early", ended(false, INTERNAL)),
        ("uncertain-early", ended(false, INTERNAL)),
        ("changed-description", ended(false, INTERNAL)),
        ("early-commit", ended(false, INTERNAL)),
        ("early-success", ended(false, INTERNAL)),
    ] {
        let mut storage = scripted(script);
        assert_eq!(drive(&mut storage).0, end, "{script}");
        assert!(storage.child.is_none());
    }
}

/// The receipt follows the typed key, which may come at any time before
/// root's deadline: one after it ends the operation as TIMEOUT, and one
/// for an ended operation is dropped.
#[test]
fn a_late_receipt_ends_the_operation_and_a_stale_one_is_dropped() {
    let mut storage = scripted("presented");
    let until = Instant::now() + Duration::from_secs(10);
    let description = loop {
        match storage.poll().unwrap() {
            Event::Present(request) => break request,
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
        assert!(Instant::now() < until);
    };
    // The presentation waits for the typed key, past the frame window.
    assert_eq!(storage.acknowledgement_deadline, Some(storage.deadline));
    storage.expire();
    storage.presented(&description).unwrap();
    assert_eq!(drive(&mut storage).0, ended(false, TIMEOUT));
    storage.presented(&description).unwrap();
    // A receipt for another description fails.
    let mut storage = scripted("presented");
    while storage.phase != Phase::Presented {
        storage.poll().unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    assert!(storage
        .presented(&update(SelectorForm::DeviceBound))
        .is_err());
    assert_eq!(drive(&mut storage).0, ended(false, INTERNAL));
}

#[test]
fn operation_bytes_one_to_nine_are_the_selections_this_build_runs() {
    assert_eq!(Selection::decode(&[1]), Ok(Selection::SelectorUpdate));
    for (shape, byte, counts) in [
        (UpgradeShape::Tpm, 1, 0..=2),
        (UpgradeShape::Fido2, 2, 1..=2),
    ] {
        for count in counts {
            assert_eq!(
                Selection::decode(&[2, byte, count]),
                Ok(Selection::Upgrade { shape, count })
            );
        }
    }
    assert_eq!(Selection::decode(&[3]), Ok(Selection::UpgradeCancel));
    // Protector management with its operands; Secure Boot enrollment
    // is refused by this build.
    let request = |request| {
        Selection::Protector(Managed::Request {
            proof: Proof::RecoveryKey,
            request,
        })
    };
    for (bytes, selection, runs) in [
        (&[4, 1, 2][..], Selection::Protector(ADD), true),
        (&[4, 2, 2], Selection::Protector(ADD_BY_KEY), true),
        (&[5, 2], Selection::Protector(REMOVE), true),
        (&[6], Selection::Protector(Managed::RecoveryRemove), true),
        (&[7, 1], request(DiskRequest::PinChange), true),
        (&[8, 1], request(DiskRequest::TpmClear), true),
        (&[9, 1], request(DiskRequest::Reenroll), true),
        (&[0x0a], Selection::Later(0x0a), false),
    ] {
        assert_eq!(Selection::decode(bytes), Ok(selection), "{bytes:?}");
        assert_eq!(selection.runs(), runs, "{bytes:?}");
        if !runs {
            assert!(Storage::start(1000, selection).is_err());
        }
    }
    // A shape or count outside its range, a missing operand and a
    // trailing byte.
    for bad in [
        &[][..],
        &[0],
        &[0x0b],
        &[0xff],
        &[1, 0],
        &[2],
        &[2, 1],
        &[2, 0, 0],
        &[2, 3, 1],
        &[2, 1, 3],
        &[2, 2, 0],
        &[2, 2, 3],
        &[2, 1, 0, 0],
        &[3, 0],
        &[4],
        &[4, 1],
        &[4, 0, 1],
        &[4, 3, 1],
        &[4, 1, 0],
        &[4, 1, 3],
        &[4, 1, 1, 0],
        &[5],
        &[5, 0],
        &[5, 3],
        &[5, 1, 0],
        &[6, 0],
        &[7],
        &[8, 0],
        &[9, 3],
        &[9, 1, 0],
        &[0x0a, 0],
    ] {
        assert!(Selection::decode(bad).is_err(), "{bad:?}");
    }
    assert!(Storage::start(1001, Selection::SelectorUpdate).is_err());
}

fn upgrading(shape: UpgradeShape, count: u8, script: &str) -> Storage {
    Storage::scripted_as(Selection::Upgrade { shape, count }, true, script).unwrap()
}

fn detailed(kind: u8, detail: u8) -> Event {
    Event::Ended(End {
        uncertain: false,
        kind,
        detail,
    })
}

/// Amendment 10's upgrade: the review, each key's ceremony admitted as
/// root derives it with every PIN step's secret, the commit naming the
/// last step, and success on a clean exit.
#[test]
fn an_upgrade_presents_its_review_and_each_ceremony_then_commits() {
    for (shape, count, script) in [
        (UpgradeShape::Tpm, 0, "upgrade-tpm-0"),
        (UpgradeShape::Tpm, 2, "upgrade-tpm-2"),
        (UpgradeShape::Fido2, 1, "upgrade-fido2-1"),
        (UpgradeShape::Fido2, 2, "upgrade-fido2-2"),
    ] {
        let mut storage = upgrading(shape, count, script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, Event::Complete, "{script}");
        let mut expected = vec![upgrade_at(shape, count, UpgradeStep::Review)];
        expected.extend(
            ceremony_steps(count)
                .into_iter()
                .map(|step| upgrade_at(shape, count, step)),
        );
        assert_eq!(presented, expected, "{script}");
        assert!(storage.child.is_none() && storage.wire.is_none());
    }
    // Root's ceiling is the selection's.
    let storage = upgrading(UpgradeShape::Fido2, 2, "upgrade-fido2-2-baseline:3");
    assert!(storage.deadline > Instant::now() + Duration::from_secs(470));
}

/// An upgrade without an enrolled login state ends as `87` before any
/// worker starts; the baseline refuses before any description; the
/// worker's reads and ceremonies end it definitely with their details
/// before the commit, and anything out of root's order fails.
#[test]
fn an_upgrade_refuses_at_each_point_before_its_commit() {
    let fido2 = Selection::Upgrade {
        shape: UpgradeShape::Fido2,
        count: 1,
    };
    let mut storage = Storage::scripted_as(fido2, false, "upgrade-fido2-1").unwrap();
    assert!(storage.child.is_none() && storage.wire.is_none());
    assert_eq!(storage.poll(), Ok(ended(false, UNENROLLED)));
    // Each with the steps root presented before it ended.
    for (script, end, shown) in [
        ("upgrade-fido2-1-baseline:0", ended(false, VOLUME), 0),
        ("upgrade-fido2-1-baseline:5", ended(false, VOLUME), 0),
        ("upgrade-fido2-1-baseline:2", ended(false, STATE), 0),
        ("upgrade-fido2-1-baseline:3", ended(false, STATE), 0),
        ("upgrade-fido2-1-baseline:4", ended(false, STATE), 0),
        ("upgrade-fido2-1-not-current", ended(false, NOT_CURRENT), 0),
        ("upgrade-fido2-1-changed-view", ended(false, CHANGED), 0),
        ("upgrade-fido2-1-wrong-pin", detailed(0x01, 7), 3),
        ("upgrade-fido2-1-excluded", ended(false, 0x11), 2),
        // The TPM's kind is the TPM shape's alone, and every kind has its
        // place.
        ("upgrade-fido2-1-tpm-refused", ended(false, INTERNAL), 0),
        ("upgrade-tpm-1-tpm-at-key", ended(false, INTERNAL), 2),
        ("upgrade-fido2-1-read-at-key", ended(false, INTERNAL), 2),
        ("upgrade-fido2-1-key-at-review", ended(false, INTERNAL), 0),
        ("upgrade-fido2-1-skip", ended(false, INTERNAL), 1),
        ("upgrade-fido2-1-early-commit", ended(false, INTERNAL), 1),
        ("upgrade-fido2-1-collides", ended(false, INTERNAL), 3),
        ("upgrade-fido2-1-switched", ended(false, INTERNAL), 4),
        ("upgrade-fido2-1-stale-commit", ended(false, INTERNAL), 6),
        ("upgrade-fido2-2-reused", ended(false, INTERNAL), 8),
    ] {
        let mut parts = script.split('-').skip(1);
        let shape = match parts.next() {
            Some("tpm") => UpgradeShape::Tpm,
            _ => UpgradeShape::Fido2,
        };
        let count = parts.next().unwrap().parse().unwrap();
        let mut storage = upgrading(shape, count, script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, end, "{script}");
        assert_eq!(presented.len(), shown, "{script}");
        assert!(storage.child.is_none(), "{script}");
    }
    // A secret for a step that asks none ends the operation.
    let mut storage = upgrading(UpgradeShape::Fido2, 1, "upgrade-fido2-1-hold-connect");
    let connect = upgrade_at(UpgradeShape::Fido2, 1, ceremony_steps(1)[0]);
    let until = Instant::now() + Duration::from_secs(10);
    while storage.request() != Some(&connect) || storage.phase != Phase::Running {
        match storage.poll().unwrap() {
            Event::Present(request) => storage.presented(&request).unwrap(),
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
        assert!(Instant::now() < until);
    }
    assert!(storage.pin(&connect, Pin::new(b"1234").unwrap()).is_err());
    assert_eq!(drive(&mut storage).0, ended(false, INTERNAL));
    let mut storage = upgrading(UpgradeShape::Tpm, 0, "upgrade-tpm-0-tpm-refused");
    assert_eq!(drive(&mut storage).0, detailed(0x27, 2));
    // After root's `13` the selector update's rules.
    let mut storage = upgrading(UpgradeShape::Tpm, 0, "upgrade-tpm-0-uncertain");
    assert_eq!(drive(&mut storage).0, ended(true, UNCERTAIN));
}

/// Tag 18: one presentation and a commit round over an upgrading volume,
/// `21` past U0 and a definite `28` after the commit.
#[test]
fn a_cancellation_takes_an_upgrading_volume_and_one_presentation() {
    for (script, end) in [
        ("cancel", Event::Complete),
        ("cancel-past-u0", ended(false, STATE)),
        ("cancel-changed", ended(false, CHANGED)),
        ("cancel-changed-view", ended(false, CHANGED)),
        ("cancel-changed-held", ended(false, CHANGED)),
        // Past root's grace the open channel is an unclean end.
        ("cancel-changed-held-long", ended(false, INTERNAL)),
        ("cancel-esp", ended(false, INTERNAL)),
        ("cancel-baseline:0", ended(false, VOLUME)),
        ("cancel-baseline:5", ended(false, VOLUME)),
        ("cancel-baseline:1", ended(false, STATE)),
        ("cancel-baseline:3", ended(false, STATE)),
    ] {
        let mut storage = Storage::scripted_as(Selection::UpgradeCancel, true, script).unwrap();
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, end, "{script}");
        if script == "cancel" {
            assert_eq!(presented, [cancellation()]);
        }
        assert!(storage.child.is_none(), "{script}");
    }
}

/// A disk proof's baseline: the volume byte, the recovery-key marker and
/// the fido2 tokens.
fn proof_baseline(volume: u8, recovery: bool, tokens: &[(u8, [u8; 4])]) -> Vec<u8> {
    let mut frame = baseline_frame(volume, tokens, true);
    frame[2] = u8::from(recovery);
    frame
}

/// The storage worker's side of `crate::proof::tests`.
fn proof_script(w: &mut Worker, script: &str) {
    use crate::consent::{LoginStep, Proof};
    use crate::proof::tests::{add_by, described, remove_by, A, RECOVERY, T};
    let recovery = described(add_by(Proof::RecoveryKey, LoginStep::RecoveryKey));
    let identify = described(remove_by(Proof::DiskKey, LoginStep::DiskIdentify));
    let authorize = |key| {
        described(remove_by(
            Proof::DiskKey,
            LoginStep::DiskAuthorize { key, retries: 8 },
        ))
    };
    let first = |w: &mut Worker, frame: Vec<u8>, expected: &Request| {
        w.send(&frame);
        assert_eq!(&Request::decode(&w.read()).unwrap(), expected);
    };
    let recovered = |w: &mut Worker| {
        first(w, proof_baseline(3, true, &[(1, T)]), &recovery);
        w.invite(0x10, &recovery);
        assert_eq!(w.read(), [&[0x16][..], &RECOVERY[..]].concat());
    };
    let identified = |w: &mut Worker, token| {
        first(w, proof_baseline(4, false, &[(1, token)]), &identify);
        w.invite(0x10, &identify);
    };
    match script {
        "proof-recovery" => {
            recovered(w);
            w.send(&[0x14]);
        }
        "proof-changed" => {
            recovered(w);
            w.send(&[0x15, 0x28]);
        }
        fail if fail.starts_with("proof-fail:") => {
            recovered(w);
            let kind = u8::from_str_radix(fail.trim_start_matches("proof-fail:"), 16).unwrap();
            w.send(&[0x15, kind]);
            std::process::exit(1);
        }
        "proof-disk-remove" => {
            identified(w, T);
            w.invite(0x10, &authorize(T));
            assert_eq!(w.read(), b"\x161234");
            w.send(&[0x14]);
        }
        "proof-wrong-pin" => {
            identified(w, T);
            w.invite(0x10, &authorize(T));
            assert_eq!(w.read(), b"\x161234");
            w.send(&[0x15, 0x01, 7]);
            std::process::exit(1);
        }
        // A disk security key that names a login key, and one the header
        // does not hold.
        "proof-login-key" | "proof-stranger" => {
            let (token, key) = if script == "proof-login-key" {
                (A, A)
            } else {
                (T, [0x99; 4])
            };
            identified(w, token);
            let frame = w.frame(0x10, &authorize(key));
            w.send(&frame);
            w.hold();
        }
        // Success before any secret: the identify step proves nothing.
        "proof-identify-success" => {
            identified(w, T);
            w.send(&[0x14]);
            w.hold();
        }
        "proof-hold" => {
            w.send(&proof_baseline(3, true, &[(1, T)]));
            w.hold();
        }
        "proof-disk-hold" => {
            w.send(&proof_baseline(4, false, &[(1, T)]));
            w.hold();
        }
        "proof-no-recovery" => {
            w.send(&proof_baseline(3, false, &[(1, T)]));
            w.hold();
        }
        "proof-no-tokens" => {
            w.send(&proof_baseline(3, true, &[]));
            w.hold();
        }
        volume if volume.starts_with("proof-volume:") => {
            let volume = volume.trim_start_matches("proof-volume:").parse().unwrap();
            w.send(&proof_baseline(volume, true, &[(1, T)]));
            w.hold();
        }
        // Presents the recovery-key step, then reads until root closes the
        // channel, which it reports before it exits on its own.
        witness if witness.starts_with("proof-close-witness:") => {
            first(w, proof_baseline(3, true, &[(1, T)]), &recovery);
            let frame = w.frame(0x10, &recovery);
            w.send(&frame);
            w.stream.set_read_timeout(None).unwrap();
            let mut byte = [0];
            while matches!(w.stream.read(&mut byte), Ok(1)) {}
            let path = witness.trim_start_matches("proof-close-witness:");
            std::fs::write(format!("{path}.tmp"), b"closed").unwrap();
            std::fs::rename(format!("{path}.tmp"), path).unwrap();
        }
        "proof-ignore" => {
            recovered_until_pin(w, &recovery);
            w.hold();
        }
        _ => panic!("unknown proof script {script}"),
    }
}

/// Through the recovery-key step's acknowledgement, without its secret.
fn recovered_until_pin(w: &mut Worker, recovery: &Request) {
    use crate::proof::tests::T;
    w.send(&proof_baseline(3, true, &[(1, T)]));
    assert_eq!(&Request::decode(&w.read()).unwrap(), recovery);
    w.invite(0x10, recovery);
}

/// The secret frame root forwards for `step`: the recovery key, a choose
/// step's digit `1`, or a PIN.
fn secret_of(step: &Request) -> Option<Vec<u8>> {
    use crate::proof::tests::RECOVERY;
    let mut frame = vec![0x16];
    if step.asks_recovery_key() {
        frame.extend_from_slice(RECOVERY);
    } else if step.protector_step() == Some(LoginStep::Choose) {
        frame.push(b'1');
    } else if step.ceremony_step().is_some_and(LoginStep::asks_pin) {
        frame.extend_from_slice(b"1234");
    } else {
        return None;
    }
    Some(frame)
}

const P: [u8; 4] = [0x51; 4];
const Q: [u8; 4] = [0x52; 4];
const R: [u8; 4] = [0x53; 4];
/// An orphan's: a fido2 token naming no keyslot.
const O: [u8; 4] = [0x54; 4];
/// The credential an addition's ceremony creates.
const N: [u8; 4] = [0x0e; 4];

const ADD: Managed = Managed::Add {
    proof: Proof::RecoveryKey,
    role: DiskRole::Recovery,
};
const ADD_BY_KEY: Managed = Managed::Add {
    proof: Proof::DiskKey,
    role: DiskRole::Recovery,
};
const REMOVE: Managed = Managed::Remove {
    proof: Proof::DiskKey,
};

/// A protected baseline: the volume byte, the marker, each fido2 token's
/// role, keyslot byte and fingerprint, the tpm-pin count and the lockout,
/// no request, and the standing.
fn protected_frame(
    volume: u8,
    marker: bool,
    tokens: &[(u8, u8, [u8; 4])],
    tpm_pins: u8,
    lockout: bool,
) -> Vec<u8> {
    let mut frame = vec![0x18, volume, u8::from(marker), tokens.len() as u8];
    for (role, named, key) in tokens {
        frame.extend([*role, *named]);
        frame.extend_from_slice(key);
    }
    frame.extend([tpm_pins, u8::from(lockout), 0, 0]);
    frame.extend_from_slice(&ID);
    frame
}

/// The TPM shape: a recovery key on a keyslot beside an orphan, a tpm-pin
/// token and the lockout.
fn tpm_volume() -> Vec<u8> {
    protected_frame(3, true, &[(2, 1, R), (2, 0, O)], 1, true)
}

/// The TPM shape with a pending TPM clear.
fn pending_volume() -> Vec<u8> {
    let mut frame = tpm_volume();
    let at = frame.len() - ID.len() - 2;
    frame[at] = 2;
    frame
}

fn requesting(proof: Proof, request: DiskRequest, pending: bool, step: LoginStep) -> Request {
    described(Operation::ProtectorRequest {
        account: 1000,
        proof,
        request,
        pending: pending.then_some(DiskRequest::TpmClear),
        step,
    })
}

/// A PIN change by the recovery key: its one step is its last.
fn pin_change() -> Vec<Request> {
    vec![requesting(
        Proof::RecoveryKey,
        DiskRequest::PinChange,
        false,
        LoginStep::RecoveryKey,
    )]
}

/// A re-enrollment by the recovery key's security key, replacing the
/// pending TPM clear.
fn reenrollment() -> Vec<Request> {
    [
        LoginStep::DiskIdentify,
        LoginStep::DiskAuthorize { key: R, retries: 8 },
    ]
    .map(|step| requesting(Proof::DiskKey, DiskRequest::Reenroll, true, step))
    .to_vec()
}

const PIN_CHANGE: Managed = Managed::Request {
    proof: Proof::RecoveryKey,
    request: DiskRequest::PinChange,
};
const REENROLL: Managed = Managed::Request {
    proof: Proof::DiskKey,
    request: DiskRequest::Reenroll,
};

/// The FIDO2 shape: two primary keys and a recovery key.
fn fido_volume() -> Vec<u8> {
    protected_frame(4, true, &[(1, 1, P), (1, 1, Q), (2, 1, R)], 0, false)
}

fn described(operation: Operation) -> Request {
    Request::new(NONCE, 1000, operation).unwrap()
}

fn adding(proof: Proof, step: LoginStep) -> Request {
    described(Operation::ProtectorAdd {
        account: 1000,
        proof,
        role: DiskRole::Recovery,
        step,
    })
}

/// An addition by the recovery key, every step in its order.
fn addition() -> Vec<Request> {
    [
        LoginStep::RecoveryKey,
        LoginStep::Connect,
        LoginStep::Create { retries: 8 },
        LoginStep::Prove { key: N, retries: 8 },
        LoginStep::Repeat { key: N, retries: 7 },
        LoginStep::Probe { key: N },
    ]
    .map(|step| adding(Proof::RecoveryKey, step))
    .to_vec()
}

fn removing(chosen: u8, step: LoginStep) -> Request {
    removing_by(Proof::DiskKey, &fido_volume(), chosen, step)
}

/// A removal by `proof` over `volume`'s list.
fn removing_by(proof: Proof, volume: &[u8], chosen: u8, step: LoginStep) -> Request {
    let mut removal = baseline(volume).unwrap().protected.unwrap().removal();
    removal.chosen = chosen;
    described(Operation::ProtectorRemove {
        account: 1000,
        proof,
        removal,
        step,
    })
}

/// The FIDO2 shape without the marker: one primary key, which no
/// removal may take, and two recovery keys.
fn unmarked_volume() -> Vec<u8> {
    protected_frame(4, false, &[(1, 1, P), (2, 1, Q), (2, 1, R)], 0, false)
}

/// A removal of the first primary key by the second.
fn removal() -> Vec<Request> {
    vec![
        removing(0, LoginStep::Choose),
        removing(1, LoginStep::DiskIdentify),
        removing(1, LoginStep::DiskAuthorize { key: Q, retries: 8 }),
    ]
}

fn recovery_removing(step: LoginStep, keyed: bool) -> Request {
    described(Operation::RecoveryRemove {
        account: 1000,
        step,
        approval: keyed.then(approval),
    })
}

/// The recovery key's removal: tag 21's identify step with its key, then
/// the first primary key's authorize step.
fn recovery_removal() -> Vec<Request> {
    vec![
        recovery_removing(LoginStep::DiskIdentify, true),
        recovery_removing(LoginStep::DiskAuthorize { key: P, retries: 8 }, false),
    ]
}

/// Each step presented in turn, with the secret root forwards for it.
fn walk(w: &mut Worker, steps: &[Request]) {
    for step in steps {
        w.invite(0x10, step);
        if let Some(secret) = secret_of(step) {
            assert_eq!(w.read(), secret);
        }
    }
}

/// The storage worker's side of protector management's tests:
/// `protector-baseline:NAME`, or an addition's, a removal's or a
/// recovery-key removal's script.
fn protector_script(w: &mut Worker, script: &str) {
    let rest = script.trim_start_matches("protector-");
    if let Some(name) = rest.strip_prefix("baseline:") {
        let frame = match name {
            "none" => baseline_frame(0, &[], true),
            "device-bound" => baseline_frame(1, &[], true),
            "upgrading" => baseline_frame(2, &[], true),
            "tpm" => tpm_volume(),
            "fido" => fido_volume(),
            "fido-unmarked" => {
                let mut frame = fido_volume();
                frame[2] = 0;
                frame
            }
            other => panic!("unknown protector baseline {other}"),
        };
        w.send(&frame);
        return w.hold();
    }
    let (volume, steps) = if rest == "add-key" {
        let authorize = LoginStep::DiskAuthorize { key: R, retries: 8 };
        let mut steps = vec![
            adding(Proof::DiskKey, LoginStep::DiskIdentify),
            adding(Proof::DiskKey, authorize),
        ];
        steps.extend(
            addition()
                .iter()
                .skip(1)
                .map(|step| adding(Proof::DiskKey, step.protector_step().unwrap())),
        );
        (tpm_volume(), steps)
    } else if rest.starts_with("add") {
        (tpm_volume(), addition())
    } else if rest == "remove-recovery" {
        let volume = fido_volume();
        let steps = vec![
            removing_by(Proof::RecoveryKey, &volume, 0, LoginStep::Choose),
            removing_by(Proof::RecoveryKey, &volume, 1, LoginStep::RecoveryKey),
        ];
        (volume, steps)
    } else if rest == "remove-unmarked-hold" {
        let volume = unmarked_volume();
        let steps = vec![removing_by(Proof::DiskKey, &volume, 0, LoginStep::Choose)];
        (volume, steps)
    } else if rest.starts_with("remove") {
        (fido_volume(), removal())
    } else if rest.starts_with("request-pin") {
        (tpm_volume(), pin_change())
    } else if rest == "request-clear" {
        let steps = vec![requesting(
            Proof::RecoveryKey,
            DiskRequest::TpmClear,
            false,
            LoginStep::RecoveryKey,
        )];
        (tpm_volume(), steps)
    } else if rest.starts_with("request-reenroll") {
        (pending_volume(), reenrollment())
    } else if rest == "orphan" {
        (
            tpm_volume(),
            vec![adding(Proof::DiskKey, LoginStep::DiskIdentify)],
        )
    } else {
        (fido_volume(), recovery_removal())
    };
    w.send(&volume);
    assert_eq!(Request::decode(&w.read()).unwrap(), steps[0]);
    let last = steps.last().unwrap().clone();
    // Presents `request` out of root's order, then holds.
    let stray = |w: &mut Worker, tag, request: &Request| {
        let frame = w.frame(tag, request);
        w.send(&frame);
        w.hold();
    };
    match rest {
        "add" | "add-key" | "remove" | "remove-recovery" | "recovery-remove" | "request-pin"
        | "request-reenroll" | "request-clear" => {
            walk(w, &steps);
            w.invite(0x12, &last);
            w.send(&[0x14]);
        }
        "add-uncertain" | "add-changed" => {
            walk(w, &steps);
            w.invite(0x12, &last);
            w.send(&[0x15, if rest == "add-uncertain" { 0x0e } else { 0x28 }]);
        }
        "add-excluded" => {
            walk(w, &steps[..3]);
            w.send(&[0x15, 0x11]);
            w.hold();
        }
        // A new key's own kinds: a wrong PIN at its create step and an
        // unselected probe.
        "add-wrong-pin" | "add-unprobed" => {
            let (shown, frame) = if rest == "add-wrong-pin" {
                (3, &[0x15, 0x01, 7])
            } else {
                (6, &[0x15, 0x06, 0x06])
            };
            walk(w, &steps[..shown]);
            w.send(frame);
            w.hold();
        }
        "add-excluded-early" | "add-mistyped" | "add-uncertain-early" => {
            walk(w, &steps[..1]);
            w.send(&[
                0x15,
                match rest {
                    "add-excluded-early" => 0x11,
                    "add-mistyped" => 0x25,
                    _ => 0x0e,
                },
            ]);
            w.hold();
        }
        "add-hold" | "remove-hold" | "remove-unmarked-hold" | "recovery-remove-hold" => {
            stray(w, 0x10, &steps[0])
        }
        "remove-authorize-hold" => {
            walk(w, &steps[..2]);
            stray(w, 0x10, &steps[2]);
        }
        // Kinds before the proof's first step: at root's description, at
        // the choose step, and the chosen key's outside a removal.
        "add-kind-at-description" => {
            w.send(&[0x15, 0x26]);
            w.hold();
        }
        // A request's keyslot without a bounded cost, before anything is
        // presented; its dump failing after the test.
        "request-pin-uncosted" => {
            w.send(&[0x15, FAILED]);
            w.hold();
        }
        "request-pin-undumped" => {
            walk(w, &steps);
            w.send(&[0x15, FAILED]);
            w.hold();
        }
        "request-pin-early-commit" => stray(w, 0x12, &steps[0]),
        "request-pin-hold" => stray(w, 0x10, &steps[0]),
        // A commit asked after the one step's receipt, its key unsent.
        "request-pin-keyless-commit" => {
            w.invite(0x10, &steps[0]);
            stray(w, 0x12, &steps[0]);
        }
        // A disk key's wrong PIN, and the dump past its deadline.
        "request-reenroll-wrong-pin" | "request-reenroll-undumped" => {
            walk(w, &steps);
            w.send(if rest == "request-reenroll-wrong-pin" {
                &[0x15, 0x01, 7]
            } else {
                &[0x15, TIMEOUT]
            });
            w.hold();
        }
        "remove-kind-at-choose" | "remove-chosen-at-choose" => {
            walk(w, &steps[..1]);
            w.send(if rest == "remove-kind-at-choose" {
                &[0x15, 0x25]
            } else {
                &[0x15, 0x04, 0x01]
            });
            w.hold();
        }
        "add-chosen" => {
            walk(w, &steps[..1]);
            w.send(&[0x15, 0x04, 0x01]);
            w.hold();
        }
        "remove-chosen" => {
            walk(w, &steps[..2]);
            w.send(&[0x15, 0x04, 0x01]);
            w.hold();
        }
        // Out of root's order: a position the digit did not name, the
        // chosen key authorizing, an early commit, a new key naming a
        // protector's credential, a description root did not give, and
        // an orphan's key.
        "remove-unchosen" => {
            walk(w, &steps[..1]);
            stray(w, 0x10, &removing(2, LoginStep::DiskIdentify));
        }
        "remove-authorize-chosen" => {
            walk(w, &steps[..2]);
            let chosen = LoginStep::DiskAuthorize { key: P, retries: 8 };
            stray(w, 0x10, &removing(1, chosen));
        }
        "remove-early-commit" => {
            walk(w, &steps[..2]);
            stray(w, 0x12, &steps[1]);
        }
        "add-reuses" => {
            walk(w, &steps[..3]);
            let reused = LoginStep::Prove { key: R, retries: 8 };
            stray(w, 0x10, &adding(Proof::RecoveryKey, reused));
        }
        "changed-description" => stray(w, 0x10, &removal()[0]),
        "orphan" => {
            walk(w, &steps);
            let orphan = LoginStep::DiskAuthorize { key: O, retries: 8 };
            stray(w, 0x10, &adding(Proof::DiskKey, orphan));
        }
        _ => panic!("unknown protector script {script}"),
    }
}

fn managing(managed: Managed, script: &str) -> Storage {
    Storage::scripted_as(Selection::Protector(managed), true, script).unwrap()
}

/// Polls until root presents its first step, unacknowledged.
fn first_presented(storage: &mut Storage) -> Request {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match storage.poll().unwrap() {
            Event::Present(request) => return request,
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
        assert!(Instant::now() < until);
    }
}

#[test]
fn a_protected_baseline_gives_the_volume_protector_management_reads() {
    let parsed = baseline(&tpm_volume()).unwrap();
    assert_eq!(parsed.tokens, [R, O]);
    assert_eq!(
        parsed.protected,
        Some(Protected {
            shape: UpgradeShape::Tpm,
            marker: true,
            tpm_pin: true,
            lockout: true,
            request: None,
            tokens: vec![
                (DiskRole::Recovery, true, R),
                (DiskRole::Recovery, false, O)
            ],
        })
    );
    // The request byte, after the tokens, the tpm-pin count and the
    // lockout.
    let mut pending = tpm_volume();
    pending[18] = 2;
    assert_eq!(
        baseline(&pending).unwrap().protected.unwrap().request,
        Some(DiskRequest::TpmClear)
    );
    let fido = baseline(&fido_volume()).unwrap().protected.unwrap();
    assert_eq!(fido.shape, UpgradeShape::Fido2);
    assert!(!fido.tpm_pin && !fido.lockout);
    // Only a protected volume has one.
    for volume in [0, 1, 2, 5] {
        assert_eq!(
            baseline(&baseline_frame(volume, &[], true))
                .unwrap()
                .protected,
            None
        );
    }
}

/// Amendment 10: each step admitted as root derives it, every secret
/// forwarded, the commit naming the last step and success on a clean
/// exit.
#[test]
fn protector_management_presents_each_step_then_commits() {
    for (managed, script, steps) in [
        (ADD, "protector-add", addition()),
        (REMOVE, "protector-remove", removal()),
        (
            Managed::RecoveryRemove,
            "protector-recovery-remove",
            recovery_removal(),
        ),
    ] {
        let mut storage = managing(managed, script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, Event::Complete, "{script}");
        assert_eq!(presented, steps, "{script}");
        assert!(storage.child.is_none() && storage.wire.is_none());
    }
    // The requests: a PIN change by the recovery key, whose one step is
    // its last, and a re-enrollment by a security key replacing the
    // pending TPM clear, each under the request's ceiling.
    for (managed, script, steps) in [
        (PIN_CHANGE, "protector-request-pin", pin_change()),
        (
            Managed::Request {
                proof: Proof::RecoveryKey,
                request: DiskRequest::TpmClear,
            },
            "protector-request-clear",
            vec![requesting(
                Proof::RecoveryKey,
                DiskRequest::TpmClear,
                false,
                LoginStep::RecoveryKey,
            )],
        ),
        (REENROLL, "protector-request-reenroll", reenrollment()),
    ] {
        let mut storage = managing(managed, script);
        assert!(storage.deadline < Instant::now() + Duration::from_secs(241));
        assert!(storage.deadline > Instant::now() + Duration::from_secs(230));
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, Event::Complete, "{script}");
        assert_eq!(presented, steps, "{script}");
        assert!(storage.child.is_none() && storage.wire.is_none());
    }
    // An addition by a disk key, and a removal by the recovery key, whose
    // recovery-key step is its last.
    for (managed, script, count) in [
        (ADD_BY_KEY, "protector-add-key", 7),
        (
            Managed::Remove {
                proof: Proof::RecoveryKey,
            },
            "protector-remove-recovery",
            2,
        ),
    ] {
        let mut storage = managing(managed, script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, Event::Complete, "{script}");
        assert_eq!(presented.len(), count, "{script}");
        assert!(storage.child.is_none() && storage.wire.is_none());
    }
    // Root's ceiling is the selection's, and only tag 21's first step
    // waits for a typed key.
    let mut storage = managing(ADD, "protector-add-hold");
    assert!(storage.deadline > Instant::now() + Duration::from_secs(350));
    first_presented(&mut storage);
    assert!(storage.acknowledgement_deadline.unwrap() < storage.deadline);
    let mut storage = managing(Managed::RecoveryRemove, "protector-recovery-remove-hold");
    first_presented(&mut storage);
    assert_eq!(storage.acknowledgement_deadline, Some(storage.deadline));
}

#[test]
fn protector_management_refuses_from_the_baseline_before_any_description() {
    for (managed, script, end) in [
        (ADD, "protector-baseline:none", ended(false, VOLUME)),
        (ADD, "protector-baseline:device-bound", ended(false, STATE)),
        (REMOVE, "protector-baseline:upgrading", ended(false, STATE)),
        (
            Managed::Add {
                proof: Proof::RecoveryKey,
                role: DiskRole::Primary,
            },
            "protector-baseline:tpm",
            detailed(STATE, 1),
        ),
        (
            Managed::RecoveryRemove,
            "protector-baseline:tpm",
            detailed(STATE, 2),
        ),
        (ADD, "protector-baseline:fido-unmarked", detailed(STATE, 0)),
        // A PIN change outside the TPM shape.
        (PIN_CHANGE, "protector-baseline:fido", detailed(STATE, 3)),
    ] {
        let mut storage = managing(managed, script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, end, "{script}");
        assert!(presented.is_empty() && storage.request.is_none());
        assert!(storage.child.is_none());
    }
}

#[test]
fn protector_managements_kinds_stand_where_a_disk_proofs_do() {
    for (managed, script, end, shown) in [
        (ADD, "protector-add-excluded", ended(false, EXCLUDED), 3),
        (ADD, "protector-add-wrong-pin", detailed(0x01, 7), 3),
        (ADD, "protector-add-unprobed", detailed(0x06, 6), 6),
        (ADD, "protector-add-mistyped", ended(false, MISTYPED), 1),
        (ADD, "protector-add-uncertain", ended(true, UNCERTAIN), 6),
        (ADD, "protector-add-changed", ended(false, CHANGED), 6),
        (REMOVE, "protector-remove-chosen", detailed(0x04, 1), 2),
        // A new key's kind at the proof, `0e` before the commit, and
        // steps out of root's order.
        (
            ADD,
            "protector-add-excluded-early",
            ended(false, INTERNAL),
            1,
        ),
        (
            ADD,
            "protector-add-uncertain-early",
            ended(false, INTERNAL),
            1,
        ),
        (
            REMOVE,
            "protector-remove-unchosen",
            ended(false, INTERNAL),
            1,
        ),
        (
            REMOVE,
            "protector-remove-authorize-chosen",
            ended(false, INTERNAL),
            2,
        ),
        (
            REMOVE,
            "protector-remove-early-commit",
            ended(false, INTERNAL),
            2,
        ),
        (ADD, "protector-add-reuses", ended(false, INTERNAL), 3),
        (
            Managed::RecoveryRemove,
            "protector-changed-description",
            ended(false, INTERNAL),
            0,
        ),
        (ADD_BY_KEY, "protector-orphan", ended(false, INTERNAL), 1),
        (
            ADD,
            "protector-add-kind-at-description",
            ended(false, INTERNAL),
            0,
        ),
        (
            REMOVE,
            "protector-remove-kind-at-choose",
            ended(false, INTERNAL),
            1,
        ),
        (
            REMOVE,
            "protector-remove-chosen-at-choose",
            ended(false, INTERNAL),
            1,
        ),
        (ADD, "protector-add-chosen", ended(false, INTERNAL), 1),
        // A request's `0f` before its first presentation and after its
        // proof; a commit asked before its one step was presented.
        (
            PIN_CHANGE,
            "protector-request-pin-uncosted",
            ended(false, FAILED),
            0,
        ),
        (
            PIN_CHANGE,
            "protector-request-pin-undumped",
            ended(false, FAILED),
            1,
        ),
        (
            PIN_CHANGE,
            "protector-request-pin-early-commit",
            ended(false, INTERNAL),
            0,
        ),
        (
            REENROLL,
            "protector-request-reenroll-wrong-pin",
            detailed(0x01, 7),
            2,
        ),
        (
            REENROLL,
            "protector-request-reenroll-undumped",
            ended(false, TIMEOUT),
            2,
        ),
    ] {
        let mut storage = managing(managed, script);
        let (event, presented) = drive(&mut storage);
        assert_eq!(event, end, "{script}");
        assert_eq!(presented.len(), shown, "{script}");
        assert!(storage.child.is_none(), "{script}");
    }
}

/// A choose step's `1c` is one digit naming a removable token, and a
/// recovery-key step's 48 digits; anything else ends the generation, and
/// the stopped worker has its grace for its header copy.
#[test]
fn a_secret_outside_its_steps_profile_ends_the_operation() {
    for (managed, script, entry) in [
        (REMOVE, "protector-remove-hold", &b"0"[..]),
        (REMOVE, "protector-remove-hold", b"4"),
        (REMOVE, "protector-remove-hold", b"12"),
        // Listed, but the only primary key.
        (REMOVE, "protector-remove-unmarked-hold", b"1"),
        (ADD, "protector-add-hold", b"1234"),
        (PIN_CHANGE, "protector-request-pin-hold", b"1234"),
    ] {
        let mut storage = managing(managed, script);
        let step = first_presented(&mut storage);
        storage.presented(&step).unwrap();
        assert_eq!(storage.poll(), Ok(Event::Pin(step.clone())));
        assert!(storage.pin(&step, Pin::entry(entry).unwrap()).is_err());
        assert!(storage.grace.is_some() && storage.chosen.is_none());
        assert_eq!(drive(&mut storage).0, ended(false, INTERNAL));
    }
}

/// A PIN step's secret is at least four bytes, though `1c` decodes one.
#[test]
fn a_storage_pin_step_takes_no_short_secret() {
    let mut storage = managing(REMOVE, "protector-remove-authorize-hold");
    let authorize = &removal()[2];
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match storage.poll().unwrap() {
            Event::Present(request) => storage.presented(&request).unwrap(),
            Event::Pin(request) if request == *authorize => break,
            Event::Pin(request) => {
                let secret = secret_of(&request).unwrap();
                assert!(storage
                    .pin(&request, Pin::entry(&secret[1..]).unwrap())
                    .unwrap());
            }
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
        assert!(Instant::now() < until);
    }
    assert!(storage.pin(authorize, Pin::entry(b"123").unwrap()).is_err());
    assert!(storage.pin.is_none() && storage.grace.is_some());
    assert_eq!(drive(&mut storage).0, ended(false, INTERNAL));
}

/// Tag 21's receipt follows its typed key, which may come at any time
/// before root's deadline: one after it ends the operation as TIMEOUT.
#[test]
fn a_late_tag_21_receipt_ends_the_operation() {
    let mut storage = managing(Managed::RecoveryRemove, "protector-recovery-remove-hold");
    let first = first_presented(&mut storage);
    storage.expire();
    storage.presented(&first).unwrap();
    assert_eq!(drive(&mut storage).0, ended(false, TIMEOUT));
}

/// A commit asked while root still waits for the step's secret, or before
/// it reached the worker, is out of order: root sends `13` only once the
/// secret is written.
#[test]
fn a_commit_before_the_steps_secret_ends_the_operation() {
    let mut storage = managing(PIN_CHANGE, "protector-request-pin-keyless-commit");
    let step = first_presented(&mut storage);
    storage.presented(&step).unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    let event = loop {
        match storage.poll().unwrap() {
            Event::Pin(_) | Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => break event,
        }
        assert!(Instant::now() < until);
    };
    assert_eq!(event, ended(false, INTERNAL));
}
