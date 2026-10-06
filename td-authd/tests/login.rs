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

const SCRIPT: &str = "TD_LOGIN_SCRIPT";
const NONCE: [u8; 32] = [42; 32];
const A: Fingerprint = [0xa1; 4];
const B: Fingerprint = [0xb2; 4];
const C: Fingerprint = [0xc3; 4];
/// The keys a ceremony creates.
const N: Fingerprint = [0x4e; 4];
const M: Fingerprint = [0x5f; 4];

/// `login::tests::scripted_worker` playing `script` on its stdin socket.
pub(crate) fn fixture(script: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "login::tests::scripted_worker", "--ignored"])
        .env_clear()
        .env(SCRIPT, script)
        .current_dir("/")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn unlocking(count: u8, step: LoginStep) -> Operation {
    Operation::LoginUnlock {
        account: 1000,
        before: count,
        after: count,
        step,
    }
}

fn adding(before: u8, step: LoginStep) -> Operation {
    Operation::LoginAdd {
        account: 1000,
        before,
        after: before + 1,
        step,
    }
}

fn enrolling(after: u8, key: u8, step: LoginStep) -> Operation {
    Operation::LoginEnroll {
        account: 1000,
        before: 0,
        after,
        key,
        step,
    }
}

fn removing(before: u8, removed: &[Slot], step: LoginStep) -> Operation {
    Operation::LoginRemove {
        account: 1000,
        before,
        after: before - removed.len() as u8,
        removed: removed.to_vec(),
        step,
    }
}

fn request(operation: Operation) -> Request {
    Request::new(NONCE, 1000, operation).unwrap()
}

fn slots() -> Vec<Slot> {
    vec![
        Slot {
            position: 1,
            key: A,
        },
        Slot {
            position: 3,
            key: C,
        },
    ]
}

fn eight() -> Vec<Fingerprint> {
    (1..=8).map(|key| [key; 4]).collect()
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

    fn baseline(&mut self, keys: &[Fingerprint]) {
        let mut frame = vec![0x18, 1, keys.len() as u8];
        frame.extend(keys.iter().flatten());
        self.send(&frame);
    }

    /// Root's first step, which must be `expected`.
    fn description(&mut self, expected: Operation) -> Request {
        let description = Request::decode(&self.read()).unwrap();
        assert_eq!(description, request(expected));
        description
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

    fn present(&mut self, operation: Operation) {
        self.invite(0x10, &request(operation));
    }

    /// A PIN step: its round, then exactly the PIN root relays.
    fn pin_step(&mut self, operation: Operation) {
        self.present(operation);
        assert_eq!(self.read(), b"\x161234");
    }

    /// Left for root to kill.
    fn hold(&mut self) {
        thread::sleep(Duration::from_secs(30));
    }
}

fn unlock_through_pin(worker: &mut Worker) {
    worker.baseline(&[A, B]);
    worker.description(unlocking(2, LoginStep::Identify));
    worker.present(unlocking(2, LoginStep::Identify));
    worker.pin_step(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
}

fn addition_through_probe(worker: &mut Worker) {
    worker.baseline(&[A]);
    worker.description(adding(1, LoginStep::Identify));
    worker.present(adding(1, LoginStep::Identify));
    worker.pin_step(adding(1, LoginStep::Authorize { key: A, retries: 8 }));
    worker.present(adding(1, LoginStep::Connect));
    worker.pin_step(adding(1, LoginStep::Create { retries: 7 }));
    worker.pin_step(adding(1, LoginStep::Prove { key: N, retries: 7 }));
    worker.pin_step(adding(1, LoginStep::Repeat { key: N, retries: 7 }));
    worker.present(adding(1, LoginStep::Probe { key: N }));
}

#[test]
#[ignore = "exec-only scripted login worker"]
fn scripted_worker() {
    let script = std::env::var(SCRIPT).unwrap();
    let mut worker = Worker::new();
    let w = &mut worker;
    let unlock = request(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
    let probe = request(adding(1, LoginStep::Probe { key: N }));
    match script.as_str() {
        "unlock" | "unlock-exit" | "unlock-fails-after-commit" => {
            unlock_through_pin(w);
            w.invite(0x12, &unlock);
            match script.as_str() {
                "unlock" => w.send(&[0x14]),
                "unlock-exit" => {
                    w.send(&[0x14]);
                    std::process::exit(1);
                }
                _ => w.send(&[0x15, 0x0f]),
            }
        }
        "unlock-wrong-pin" => {
            unlock_through_pin(w);
            w.send(&[0x15, 0x01, 0x08]);
        }
        // As stall-pin, with a descendant that reports whether root wrote
        // anything more.
        witness if witness.starts_with("pin-witness:") => {
            w.baseline(&[A, B]);
            w.description(unlocking(2, LoginStep::Identify));
            w.present(unlocking(2, LoginStep::Identify));
            w.present(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
            let path = witness.trim_start_matches("pin-witness:");
            let mut holder = fixture(&format!("witness:{path}"));
            holder.stdin(Stdio::from(std::os::fd::OwnedFd::from(
                w.stream.try_clone().unwrap(),
            )));
            #[allow(
                clippy::zombie_processes,
                reason = "root kills the worker, leaving its descendant the channel"
            )]
            let _holder = holder.spawn().unwrap();
            w.hold();
        }
        witness if witness.starts_with("witness:") => {
            let path = witness.trim_start_matches("witness:");
            std::fs::write(format!("{path}.ready"), b"").unwrap();
            let mut byte = [0];
            let wrote = matches!(w.stream.read(&mut byte), Ok(1));
            std::fs::write(format!("{path}.tmp"), if wrote { "pin" } else { "none" }).unwrap();
            std::fs::rename(format!("{path}.tmp"), path).unwrap();
        }
        "stall-pin" => {
            w.baseline(&[A, B]);
            w.description(unlocking(2, LoginStep::Identify));
            w.present(unlocking(2, LoginStep::Identify));
            w.present(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
            w.hold();
        }
        "add" | "add-uncertain" | "add-lost" | "add-exit" | "add-stall" | "add-linger"
        | "add-trailing" | "add-trailing-hold" | "add-inherited" => {
            addition_through_probe(w);
            w.invite(0x12, &probe);
            match script.as_str() {
                "add" => w.send(&[0x14]),
                "add-uncertain" => w.send(&[0x15, 0x0e, 0x03]),
                "add-exit" => {
                    w.send(&[0x14]);
                    std::process::exit(1);
                }
                "add-stall" => w.hold(),
                // Success, then no exit.
                "add-linger" => {
                    w.send(&[0x14]);
                    w.hold();
                }
                // Success, then a failure, then a clean exit.
                "add-trailing" => {
                    w.send(&[0x14]);
                    w.send(&[0x15, 0x0f]);
                }
                // Success, then an exit that leaves the channel open in a
                // descendant.
                "add-inherited" => {
                    w.send(&[0x14]);
                    let mut holder = fixture("brief");
                    holder.stdin(Stdio::from(std::os::fd::OwnedFd::from(
                        w.stream.try_clone().unwrap(),
                    )));
                    #[allow(
                        clippy::zombie_processes,
                        reason = "the worker exits first, leaving its descendant the channel"
                    )]
                    let _holder = holder.spawn().unwrap();
                }
                // Success, a stray byte, and no exit.
                "add-trailing-hold" => {
                    w.send(&[0x14]);
                    w.stream.write_all(&[0]).unwrap();
                    w.hold();
                }
                _ => {}
            }
        }
        "unlock-trailing" => {
            unlock_through_pin(w);
            w.invite(0x12, &unlock);
            w.send(&[0x14]);
            w.send(&[0x14]);
        }
        "premature" => {
            w.baseline(&[A, B]);
            w.description(unlocking(2, LoginStep::Identify));
            w.present(unlocking(2, LoginStep::Identify));
            w.present(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
            // The commit invitation before the PIN it needs.
            let frame = w.frame(0x12, &unlock);
            w.send(&frame);
            w.hold();
        }
        "early-uncertain" => {
            w.baseline(&[A]);
            w.description(adding(1, LoginStep::Identify));
            w.send(&[0x15, 0x0e, 0x01]);
            w.hold();
        }
        "add-fails-before-commit" => {
            addition_through_probe(w);
            w.send(&[0x15, 0x0f]);
        }
        "enroll-two" => {
            w.send(&[0x18, 0]);
            w.description(enrolling(2, 1, LoginStep::Connect));
            for (ordinal, key) in [(1, N), (2, M)] {
                let step = |step| enrolling(2, ordinal, step);
                w.present(step(LoginStep::Connect));
                w.pin_step(step(LoginStep::Create { retries: 8 }));
                w.pin_step(step(LoginStep::Prove { key, retries: 8 }));
                w.pin_step(step(LoginStep::Repeat { key, retries: 8 }));
                w.present(step(LoginStep::Probe { key }));
            }
            w.invite(0x12, &request(enrolling(2, 2, LoginStep::Probe { key: M })));
            w.send(&[0x14]);
        }
        "remove" => {
            let authorize = removing(3, &slots(), LoginStep::Authorize { key: B, retries: 3 });
            w.baseline(&[A, B, C]);
            w.description(removing(3, &slots(), LoginStep::Identify));
            w.present(removing(3, &slots(), LoginStep::Identify));
            w.pin_step(authorize.clone());
            w.invite(0x12, &request(authorize));
            w.send(&[0x14]);
        }
        "baseline-eight" => {
            w.baseline(&eight());
            w.hold();
        }
        "baseline-none" => {
            w.send(&[0x18, 0]);
            w.hold();
        }
        "baseline-three" => {
            w.baseline(&[A, B, C]);
            w.hold();
        }
        "unavailable" => w.send(&[0x15, 0x0a]),
        "early-success" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            w.send(&[0x14]);
            w.hold();
        }
        "early-commit" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            w.present(unlocking(1, LoginStep::Identify));
            let frame = w.frame(0x12, &request(unlocking(1, LoginStep::Identify)));
            w.send(&frame);
            w.hold();
        }
        "skipped-step" => {
            w.baseline(&[A]);
            w.description(adding(1, LoginStep::Identify));
            w.present(adding(1, LoginStep::Identify));
            let frame = w.frame(0x10, &request(adding(1, LoginStep::Connect)));
            w.send(&frame);
            w.hold();
        }
        "changed-nonce" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            let changed = Request::new([7; 32], 1000, unlocking(1, LoginStep::Identify)).unwrap();
            let frame = w.frame(0x10, &changed);
            w.send(&frame);
            w.hold();
        }
        "foreign-key" | "zero-retries" => {
            let (key, retries) = if script == "foreign-key" {
                (B, 8)
            } else {
                (A, 0)
            };
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            w.present(unlocking(1, LoginStep::Identify));
            let unlock = request(unlocking(1, LoginStep::Unlock { key, retries }));
            let frame = w.frame(0x10, &unlock);
            w.send(&frame);
            w.hold();
        }
        "commit-changed" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            w.present(unlocking(1, LoginStep::Identify));
            w.pin_step(unlocking(1, LoginStep::Unlock { key: A, retries: 8 }));
            let changed = request(unlocking(1, LoginStep::Unlock { key: A, retries: 7 }));
            let frame = w.frame(0x12, &changed);
            w.send(&frame);
            w.hold();
        }
        "unsolicited" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            let frame = w.frame(0x10, &request(unlocking(1, LoginStep::Identify)));
            w.send(&frame);
            w.send(&frame);
            w.hold();
        }
        "unacknowledged" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            for step in [
                LoginStep::Identify,
                LoginStep::Unlock { key: A, retries: 8 },
            ] {
                let frame = w.frame(0x10, &request(unlocking(1, step)));
                w.send(&frame);
            }
            w.hold();
        }
        "second-baseline" => {
            w.baseline(&[A]);
            w.baseline(&[A]);
            w.hold();
        }
        "malformed-baseline" => {
            w.send(&[&[0x18, 1, 2][..], &A].concat());
            w.hold();
        }
        "bad-detail" => {
            w.baseline(&[A]);
            w.description(unlocking(1, LoginStep::Identify));
            w.send(&[0x15, 0x06, 0x07]);
            w.hold();
        }
        "unknown-kind" => {
            w.send(&[0x15, 0x13]);
            w.hold();
        }
        "kind-without-detail" => {
            w.send(&[0x15, 0x01]);
            w.hold();
        }
        "silent" => w.hold(),
        "brief" => thread::sleep(Duration::from_secs(2)),
        _ => panic!("unknown login script {script}"),
    }
}

/// Polls `login`, acknowledging every presentation and commit and relaying
/// the PIN, until it ends: its end and every step it presented.
fn drive(login: &mut Login) -> (Event, Vec<Request>) {
    let until = Instant::now() + Duration::from_secs(10);
    let mut presented = Vec::new();
    loop {
        match login.poll().unwrap() {
            Event::Present(request) => {
                login.presented(&request).unwrap();
                presented.push(request);
            }
            Event::Pin(request) => {
                assert!(login.pin(&request, Pin::new(b"1234").unwrap()).unwrap())
            }
            Event::Commit(request) => login.commit(&request).unwrap(),
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => return (event, presented),
        }
        assert!(Instant::now() < until, "login supervisor stalled");
    }
}

fn ended(uncertain: bool, kind: u8, detail: u8) -> Event {
    Event::Ended(End {
        uncertain,
        kind,
        detail,
    })
}

fn scripted(selection: Selection, script: &str) -> Login {
    Login::scripted(selection, script).unwrap()
}

#[test]
fn an_unlock_relays_its_pin_and_completes_only_with_success_and_exit() {
    let mut login = scripted(Selection::Unlock, "unlock");
    let (event, presented) = drive(&mut login);
    assert_eq!(event, Event::Complete);
    assert_eq!(
        presented,
        [
            request(unlocking(2, LoginStep::Identify)),
            request(unlocking(2, LoginStep::Unlock { key: A, retries: 8 })),
        ]
    );
    assert!(login.child.is_none() && login.wire.is_none());
    assert_eq!(login.poll(), Ok(Event::Complete));
    // An unlock writes nothing: no end after its commit is uncertain.
    for (script, end) in [
        ("unlock-exit", ended(false, INTERNAL, 0)),
        ("unlock-fails-after-commit", ended(false, 0x0f, 0)),
        ("unlock-wrong-pin", ended(false, 0x01, 8)),
    ] {
        let mut login = scripted(Selection::Unlock, script);
        assert_eq!(drive(&mut login).0, end, "{script}");
        assert!(login.child.is_none());
    }
}

#[test]
fn every_operation_presents_exactly_the_steps_root_derives() {
    let mut login = scripted(Selection::Add, "add");
    let (event, presented) = drive(&mut login);
    assert_eq!(event, Event::Complete);
    let steps = [
        LoginStep::Identify,
        LoginStep::Authorize { key: A, retries: 8 },
        LoginStep::Connect,
        LoginStep::Create { retries: 7 },
        LoginStep::Prove { key: N, retries: 7 },
        LoginStep::Repeat { key: N, retries: 7 },
        LoginStep::Probe { key: N },
    ];
    let expected: Vec<_> = steps
        .into_iter()
        .map(|step| request(adding(1, step)))
        .collect();
    assert_eq!(presented, expected);

    let mut login = scripted(Selection::Enroll(2), "enroll-two");
    let (event, presented) = drive(&mut login);
    assert_eq!(event, Event::Complete);
    assert_eq!(presented.len(), 10);
    assert_eq!(
        presented.last(),
        Some(&request(enrolling(2, 2, LoginStep::Probe { key: M })))
    );

    let mut login = scripted(Selection::Remove(slots()), "remove");
    let (event, presented) = drive(&mut login);
    assert_eq!(event, Event::Complete);
    assert_eq!(
        presented,
        [
            request(removing(3, &slots(), LoginStep::Identify)),
            request(removing(
                3,
                &slots(),
                LoginStep::Authorize { key: B, retries: 3 }
            )),
        ]
    );
}

#[test]
fn after_a_writes_commit_acknowledgement_every_other_end_is_uncertain() {
    for (script, end) in [
        ("add-uncertain", ended(true, 0x0e, 3)),
        ("add-lost", ended(true, INTERNAL, 0)),
        ("add-exit", ended(true, INTERNAL, 0)),
        // Before the commit acknowledgement nothing was written.
        ("add-fails-before-commit", ended(false, 0x0f, 0)),
    ] {
        let mut login = scripted(Selection::Add, script);
        assert_eq!(drive(&mut login).0, end, "{script}");
        assert!(login.child.is_none());
    }
    // Root's own deadline, and a cancellation, after the acknowledgement.
    for cancel in [false, true] {
        let mut login = scripted(Selection::Add, "add-stall");
        let until = Instant::now() + Duration::from_secs(10);
        while login.phase != Phase::Completion {
            match login.poll().unwrap() {
                Event::Present(request) => login.presented(&request).unwrap(),
                Event::Pin(request) => {
                    login.pin(&request, Pin::new(b"1234").unwrap()).unwrap();
                }
                Event::Commit(request) => login.commit(&request).unwrap(),
                Event::Waiting => thread::sleep(Duration::from_millis(1)),
                event => panic!("unexpected {event:?}"),
            }
            assert!(Instant::now() < until);
        }
        if cancel {
            login.cancel().unwrap();
        } else {
            login.deadline = Instant::now();
        }
        let kind = if cancel { CANCELLED } else { TIMEOUT };
        assert_eq!(drive(&mut login).0, ended(true, kind, 0));
        assert!(login.child.is_none());
    }
}

#[test]
fn refusals_from_the_baseline_send_the_worker_no_step() {
    let remove = |position, key| Selection::Remove(vec![Slot { position, key }]);
    for (selection, script, kind) in [
        // Consent cannot encode a ninth key.
        (Selection::Add, "baseline-eight", FULL),
        (Selection::Unlock, "baseline-none", NO_RECORD),
        (Selection::Add, "baseline-none", NO_RECORD),
        (remove(1, A), "baseline-none", NO_RECORD),
        (Selection::Enroll(1), "baseline-three", ENROLLED),
        (Selection::Enroll(2), "baseline-three", ENROLLED),
        (remove(1, B), "baseline-three", SELECTION),
        (remove(4, A), "baseline-three", SELECTION),
        (Selection::Unlock, "unavailable", 0x0a),
    ] {
        let started = Instant::now();
        let mut login = scripted(selection, script);
        let (event, presented) = drive(&mut login);
        assert_eq!(event, ended(false, kind, 0), "{script}");
        assert!(presented.is_empty() && login.request.is_none());
        // The worker holds for thirty seconds: root killed and reaped it.
        assert!(login.child.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
    // An addition below eight keys gets its first step.
    let mut login = scripted(Selection::Add, "baseline-three");
    let until = Instant::now() + Duration::from_secs(10);
    while login.request.is_none() {
        assert_eq!(login.poll(), Ok(Event::Waiting));
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(login.request, Some(request(adding(3, LoginStep::Identify))));
}

#[test]
fn malformed_or_out_of_order_worker_frames_end_the_operation() {
    for (selection, script) in [
        (Selection::Unlock, "early-success"),
        (Selection::Unlock, "early-commit"),
        (Selection::Add, "skipped-step"),
        (Selection::Unlock, "changed-nonce"),
        (Selection::Unlock, "foreign-key"),
        (Selection::Unlock, "zero-retries"),
        (Selection::Unlock, "commit-changed"),
        (Selection::Unlock, "unsolicited"),
        (Selection::Unlock, "second-baseline"),
        (Selection::Unlock, "malformed-baseline"),
        (Selection::Unlock, "bad-detail"),
        (Selection::Unlock, "unknown-kind"),
        (Selection::Unlock, "kind-without-detail"),
        // UNCERTAIN before root let any write begin.
        (Selection::Add, "early-uncertain"),
    ] {
        let started = Instant::now();
        let mut login = scripted(selection, script);
        assert_eq!(drive(&mut login).0, ended(false, INTERNAL, 0), "{script}");
        assert!(login.child.is_none());
        assert!(started.elapsed() < Duration::from_secs(10), "{script}");
    }
    // The next step, admissible in itself, while root still waits for the
    // compositor to present the step before it.
    let mut login = scripted(Selection::Unlock, "unacknowledged");
    let until = Instant::now() + Duration::from_secs(10);
    let end = loop {
        match login.poll().unwrap() {
            Event::Present(_) | Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => break event,
        }
        assert!(Instant::now() < until);
    };
    assert_eq!(end, ended(false, INTERNAL, 0));
}

#[test]
fn a_cancel_kills_and_reaps_the_worker_and_relocks_nothing() {
    let started = Instant::now();
    let mut login = scripted(Selection::Unlock, "stall-pin");
    let until = Instant::now() + Duration::from_secs(10);
    let pin = loop {
        match login.poll().unwrap() {
            Event::Present(request) => login.presented(&request).unwrap(),
            Event::Pin(request) => break request,
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
        assert!(Instant::now() < until);
    };
    login.cancel().unwrap();
    assert_eq!(drive(&mut login).0, ended(false, CANCELLED, 0));
    assert!(login.child.is_none() && login.wire.is_none());
    assert!(started.elapsed() < Duration::from_secs(10));
    // A PIN for the ended operation is dropped, not an error.
    assert_eq!(login.pin(&pin, Pin::new(b"1234").unwrap()), Ok(false));
    // Cancelling again changes nothing.
    login.cancel().unwrap();
    assert_eq!(login.poll(), Ok(ended(false, CANCELLED, 0)));
}

#[test]
fn acknowledgements_and_pins_must_name_the_current_step_in_time() {
    let presenting = |login: &mut Login| {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            match login.poll().unwrap() {
                Event::Present(request) => break request,
                Event::Waiting => thread::sleep(Duration::from_millis(1)),
                event => panic!("unexpected {event:?}"),
            }
            assert!(Instant::now() < until);
        }
    };
    // A changed description, a commit for a presentation, and a PIN before
    // its step's acknowledgement each stop the worker.
    let other = request(unlocking(2, LoginStep::Unlock { key: B, retries: 8 }));
    for case in 0..3 {
        let mut login = scripted(Selection::Unlock, "stall-pin");
        let identify = presenting(&mut login);
        let result = match case {
            0 => login.presented(&other),
            1 => login.commit(&identify),
            _ => login.pin(&identify, Pin::new(b"1234").unwrap()).map(|_| ()),
        };
        assert!(result.is_err());
        assert_eq!(drive(&mut login).0, ended(false, INTERNAL, 0));
    }
    // An acknowledgement window that expired.
    let mut login = scripted(Selection::Unlock, "stall-pin");
    let identify = presenting(&mut login);
    login.acknowledgement_deadline = Some(Instant::now());
    assert!(login.presented(&identify).is_err());
    let mut login = scripted(Selection::Unlock, "stall-pin");
    presenting(&mut login);
    login.acknowledgement_deadline = Some(Instant::now());
    assert_eq!(drive(&mut login).0, ended(false, TIMEOUT, 0));
    // Root's operation deadline before any baseline.
    let mut login = scripted(Selection::Unlock, "silent");
    login.deadline = Instant::now();
    assert_eq!(drive(&mut login).0, ended(false, TIMEOUT, 0));
}

#[test]
fn deadlines_are_the_workers_ceilings_counted_from_before_its_start() {
    for (selection, seconds) in [
        (Selection::Unlock, 120),
        (Selection::Remove(slots()), 120),
        (Selection::Enroll(1), 120),
        (Selection::Enroll(2), 240),
        (Selection::Add, 240),
    ] {
        assert_eq!(selection.ceiling(), Duration::from_secs(seconds));
        let before = Instant::now();
        let login = scripted(selection, "silent");
        let ceiling = Duration::from_secs(seconds);
        assert!(login.deadline >= before + ceiling);
        assert!(login.deadline <= Instant::now() + ceiling);
    }
}

#[test]
fn selections_and_pins_have_one_exact_encoding() {
    assert_eq!(Selection::decode(&[7]), Ok(Selection::Unlock));
    assert_eq!(Selection::decode(&[8, 1]), Ok(Selection::Enroll(1)));
    assert_eq!(Selection::decode(&[8, 2]), Ok(Selection::Enroll(2)));
    assert_eq!(Selection::decode(&[9]), Ok(Selection::Add));
    assert_eq!(
        Selection::decode(&[&[10, 2, 1][..], &A, &[3], &C].concat()),
        Ok(Selection::Remove(slots()))
    );
    let eight: Vec<u8> = (1..=8u8)
        .flat_map(|position| [position, 0, 0, 0, 0])
        .collect();
    assert!(Selection::decode(&[&[10, 8][..], &eight].concat()).is_ok());
    for bad in [
        &[][..],
        &[6],
        &[7, 0],
        &[8],
        &[8, 0],
        &[8, 3],
        &[9, 0],
        &[10],
        &[10, 0],
        &[10, 1, 1, 0, 0, 0],
        &[10, 1, 1, 0, 0, 0, 0, 0],
        &[10, 1, 0, 0, 0, 0, 0],
        &[10, 1, 9, 0, 0, 0, 0],
        &[10, 2, 2, 0, 0, 0, 0, 1, 0, 0, 0, 0],
        &[10, 2, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0],
        &[10, 9, 1, 0, 0, 0, 0],
    ] {
        assert!(Selection::decode(bad).is_err(), "{bad:?}");
    }
    assert!(Selection::Add.writes() && Selection::Enroll(1).writes());
    assert!(Selection::Remove(slots()).writes() && !Selection::Unlock.writes());
    for pin in [&b"123"[..], &[b'1'; 64], b"12\n4", b"12\x7f4"] {
        assert!(Pin::new(pin).is_err());
    }
    let pin = Pin::new(b"secret pin ~").unwrap();
    assert_eq!(format!("{pin:?}"), "Pin(..)");
    assert!(Pin::new(&[b'~'; 63]).is_ok());
}

/// Polls, acknowledging and relaying, until `phase`.
fn until_phase(login: &mut Login, phase: Phase) {
    let until = Instant::now() + Duration::from_secs(10);
    while login.phase != phase {
        match login.poll().unwrap() {
            Event::Present(request) => login.presented(&request).unwrap(),
            Event::Pin(request) => {
                assert!(login.pin(&request, Pin::new(b"1234").unwrap()).unwrap())
            }
            Event::Commit(request) => login.commit(&request).unwrap(),
            Event::Waiting => thread::sleep(Duration::from_millis(1)),
            event => panic!("unexpected {event:?}"),
        }
        assert!(Instant::now() < until, "never reached {phase:?}");
    }
}

#[test]
fn a_pin_after_the_deadline_ends_the_operation_not_the_generation() {
    let mut login = scripted(Selection::Unlock, "stall-pin");
    until_phase(&mut login, Phase::Pin);
    let unlock = request(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
    login.deadline = Instant::now();
    assert_eq!(login.pin(&unlock, Pin::new(b"1234").unwrap()), Ok(false));
    assert!(login.pin.is_none());
    assert_eq!(drive(&mut login).0, ended(false, TIMEOUT, 0));
    // In time, a PIN for another step still stops the worker.
    let mut login = scripted(Selection::Unlock, "stall-pin");
    until_phase(&mut login, Phase::Pin);
    let other = request(unlocking(2, LoginStep::Unlock { key: A, retries: 7 }));
    assert!(login.pin(&other, Pin::new(b"1234").unwrap()).is_err());
    assert_eq!(drive(&mut login).0, ended(false, INTERNAL, 0));
}

#[test]
fn at_the_deadline_only_a_success_already_in_the_socket_counts() {
    // The 14 and the exit were both there when the deadline was seen.
    let mut login = scripted(Selection::Add, "add");
    until_phase(&mut login, Phase::Completion);
    login.wire.as_mut().unwrap().flush().unwrap();
    let pid = login.fixture_pid().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.contains(") Z "))
    {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    login.deadline = Instant::now();
    assert_eq!(login.poll(), Ok(Event::Complete));
    // The 14 arrived, the exit did not: uncertain.
    let mut login = scripted(Selection::Add, "add-linger");
    until_phase(&mut login, Phase::Exit);
    login.deadline = Instant::now();
    assert_eq!(drive(&mut login).0, ended(true, TIMEOUT, 0));
    // Neither: uncertain.
    let mut login = scripted(Selection::Add, "add-stall");
    until_phase(&mut login, Phase::Completion);
    login.deadline = Instant::now();
    assert_eq!(drive(&mut login).0, ended(true, TIMEOUT, 0));
}

#[test]
fn a_frame_before_the_pin_is_written_or_after_success_is_refused() {
    let mut login = scripted(Selection::Unlock, "premature");
    until_phase(&mut login, Phase::Pin);
    login.wire.as_mut().unwrap().flush().unwrap();
    // Let the early commit invitation reach the socket, unread.
    thread::sleep(Duration::from_millis(300));
    let unlock = request(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
    assert_eq!(login.pin(&unlock, Pin::new(b"1234").unwrap()), Ok(true));
    login.poll().unwrap();
    // Refused before the PIN was queued.
    assert!(login.pin.is_none() && login.wire.is_none());
    assert_eq!(drive(&mut login).0, ended(false, INTERNAL, 0));
    // Anything after 14, even a clean exit, is no success.
    for (selection, script, uncertain) in [
        (Selection::Add, "add-trailing", true),
        (Selection::Add, "add-trailing-hold", true),
        (Selection::Add, "add-inherited", true),
        (Selection::Unlock, "unlock-trailing", false),
    ] {
        let mut login = scripted(selection, script);
        assert_eq!(
            drive(&mut login).0,
            ended(uncertain, INTERNAL, 0),
            "{script}"
        );
    }
}

#[test]
fn a_pin_still_queued_at_the_deadline_is_never_written() {
    let wait = |path: &std::path::Path| {
        let until = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < until, "no {}", path.display());
            thread::sleep(Duration::from_millis(1));
        }
    };
    // In time the queued PIN is written; at the deadline it is not.
    for late in [false, true] {
        let path = std::env::temp_dir().join(format!(
            "td-authd-pin-witness-{}-{late}",
            std::process::id()
        ));
        let ready = path.with_extension("ready");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ready);
        let mut login = scripted(
            Selection::Unlock,
            &format!("pin-witness:{}", path.display()),
        );
        until_phase(&mut login, Phase::Pin);
        login.wire.as_mut().unwrap().flush().unwrap();
        wait(&ready);
        let unlock = request(unlocking(2, LoginStep::Unlock { key: A, retries: 8 }));
        assert_eq!(login.pin(&unlock, Pin::new(b"1234").unwrap()), Ok(true));
        // Queued, not yet written.
        assert_eq!(login.poll().unwrap(), Event::Waiting);
        assert!(login.pin.is_none() && !login.wire.as_ref().unwrap().idle());
        if late {
            login.expire();
            assert_eq!(drive(&mut login).0, ended(false, TIMEOUT, 0));
        } else {
            assert_eq!(login.poll().unwrap(), Event::Waiting);
        }
        wait(&path);
        let seen = std::fs::read_to_string(&path).unwrap();
        assert_eq!(seen, if late { "none" } else { "pin" });
        if !late {
            login.cancel().unwrap();
            drive(&mut login);
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ready);
    }
}
