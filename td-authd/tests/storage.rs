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

/// A baseline frame: the volume byte, a recovery-key marker, the tokens
/// and the standing.
pub(crate) fn baseline_frame(volume: u8, tokens: &[(u8, [u8; 4])], current: bool) -> Vec<u8> {
    let mut frame = vec![0x18, volume, 1, tokens.len() as u8];
    for (role, fingerprint) in tokens {
        frame.push(*role);
        frame.extend_from_slice(fingerprint);
    }
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
                // tokens, a role past 02, a short ID and a trailing byte.
                "volume" => baseline_frame(6, &[], true),
                "recovery" => {
                    let mut frame = baseline_frame(1, &[], true);
                    frame[2] = 2;
                    frame
                }
                "tokens" => baseline_frame(4, &[(1, [1; 4]); 5], true),
                "role" => baseline_frame(4, &[(3, [1; 4])], true),
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
        _ => panic!("unknown storage script {script}"),
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
fn only_operation_byte_one_is_a_selection_this_build_runs() {
    assert_eq!(Selection::decode(&[1]), Ok(Selection::SelectorUpdate));
    for byte in 2..=0x0a {
        assert_eq!(Selection::decode(&[byte]), Ok(Selection::Later(byte)));
        assert!(Storage::start(1000, Selection::Later(byte)).is_err());
    }
    for bad in [&[][..], &[0], &[0x0b], &[0xff], &[1, 0]] {
        assert!(Selection::decode(bad).is_err(), "{bad:?}");
    }
    assert!(Storage::start(1001, Selection::SelectorUpdate).is_err());
}
