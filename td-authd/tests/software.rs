#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::thread;

const RECORD: &[u8] = b"format=1\nname=firefox\nkind=flathub\ncurrent=none\nprevious=none\n\
checked=never\nrefused=none\nprogress=none\n";

#[test]
fn a_request_is_a_verb_byte_and_a_name_in_the_registry_grammar() {
    for (byte, verb) in [
        (0, Verb::Status),
        (1, Verb::Install),
        (2, Verb::Update),
        (3, Verb::Remove),
        (4, Verb::DeleteData),
    ] {
        let request = Request::decode(&[&[byte][..], b"firefox"].concat()).unwrap();
        assert_eq!(request.verb, verb);
        assert_eq!(request.name, "firefox");
        assert_eq!(verb_byte(verb), byte);
    }
    let long = "a".repeat(65);
    for bad in [
        &[][..],
        &[0],
        &[5, b'f'],
        &[0xff, b'f'],
        b"\0Firefox",
        b"\0../firefox",
        b"\0fire fox",
        b"\0-firefox",
        b"\0firefox\n",
        &[0, 0xff],
        [&[0][..], long.as_bytes()].concat().as_slice(),
    ] {
        assert!(Request::decode(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_operation_is_roots_own_with_only_the_name_from_the_peer() {
    let command = operation(1000, "firefox", Verb::DeleteData);
    assert_eq!(command.get_program(), "/bin/td-authd");
    let args: Vec<&str> = command.get_args().map(|s| s.to_str().unwrap()).collect();
    assert_eq!(
        args,
        ["application-cache", "1000", "firefox", "delete-data"]
    );
    assert_eq!(
        operation(1000, "mail", Verb::Install)
            .get_args()
            .map(|s| s.to_str().unwrap())
            .collect::<Vec<_>>(),
        ["application-cache", "1000", "mail", "install"]
    );
}

#[test]
fn a_refusal_is_the_operations_last_line_without_its_prefix() {
    assert_eq!(
        reason(b"noise\ntd-authd: td-app update firefox: exit status: 1: no route\n"),
        "td-app update firefox: exit status: 1: no route"
    );
    assert_eq!(reason(b""), "the operation failed");
    assert_eq!(reason(b"a\tb\x1b\n"), "a?b?");
    assert_eq!(reason(&[b'x'; 900]).len(), REASON_BYTES);
}

/// What the fixture writes on its stdin socket, by selector.
#[test]
#[ignore = "exec-only application request output fixture"]
fn output_fixture() {
    use std::os::fd::AsFd;
    let mut wire = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    let mut selector = [0];
    wire.read_exact(&mut selector).unwrap();
    match selector[0] {
        0 => wire.write_all(RECORD).unwrap(),
        1 => {
            wire.write_all(b"td-authd: td-app update firefox: exit status: 1: refused\n")
                .unwrap();
            std::process::exit(1);
        }
        3 => wire
            .write_all(
                String::from_utf8_lossy(RECORD)
                    .replace("name=firefox", "name=mail")
                    .as_bytes(),
            )
            .unwrap(),
        4 => wire.write_all(&[b'x'; 5000]).unwrap(),
        5 => {}
        6 => {
            // Leaves a process holding the output open, then exits: the
            // leftover is init's to reap.
            wire.write_all(RECORD).unwrap();
            #[allow(clippy::zombie_processes)]
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "software::tests::output_fixture", "--ignored"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
        }
        7 => {
            wire.write_all(&[b'.'; 3 * READ_BUDGET]).unwrap();
            wire.write_all(b"\ntd-authd: the last line\n").unwrap();
            std::process::exit(1);
        }
        _ => thread::sleep(Duration::from_secs(30)),
    }
}

/// The fixture standing in for the operation: by name and verb, a record,
/// a refusal, a run that does not end, another's record, an oversized one,
/// a quiet success, a record whose output a leftover process holds, or a
/// refusal after much noise.
fn fixture(_owner: u32, name: &str, verb: Verb) -> Result<Run, String> {
    let selector = match (name, verb) {
        ("firefox", Verb::Status) => 0,
        ("broken", _) => 1,
        ("other", Verb::Status) => 3,
        ("big", Verb::Status) => 4,
        ("quick", _) => 5,
        ("lingering", _) => 6,
        ("noisy", _) => 7,
        _ => 2,
    };
    let (mut wire, endpoint) = UnixStream::pair().unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "software::tests::output_fixture", "--ignored"])
        .stdin(Stdio::from(OwnedFd::from(endpoint)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wire.write_all(&[selector]).unwrap();
    Run::new(child, wire, verb)
}

fn status(name: &str) -> Request {
    Request {
        verb: Verb::Status,
        name: name.into(),
    }
}

fn install(name: &str) -> Request {
    Request {
        verb: Verb::Install,
        name: name.into(),
    }
}

/// Gives `name` a recent read that shows no writer, as a status would.
fn fresh(requests: &mut Requests, name: &str) {
    let entry = requests.entry(name).unwrap();
    entry.record = Record::Read(RECORD.to_vec(), Instant::now(), false);
    entry.finished = Some(Instant::now());
}

/// Asks until the answer is no longer `from`, for at most ten seconds.
fn answer_after(requests: &mut Requests, request: &Request, from: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let answer = requests.answer(1000, request.clone());
        if answer != from {
            return answer;
        }
        assert!(Instant::now() < deadline, "still {from:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reaps until nothing runs for `name`, for at most ten seconds.
fn settle(requests: &mut Requests, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !requests.entries[name].idle() {
        assert!(Instant::now() < deadline, "{name} still runs");
        thread::sleep(Duration::from_millis(10));
        requests.advance();
    }
}

/// Kills every fixture still running.
fn stop(requests: &mut Requests) {
    for entry in requests.entries.values_mut() {
        for run in entry
            .operation
            .iter_mut()
            .map(|(_, run)| run)
            .chain(entry.reading.iter_mut())
        {
            let _ = run.child.kill();
            let _ = run.child.wait();
        }
    }
}

#[test]
fn a_status_answers_the_last_read_while_the_next_runs() {
    let mut requests = Requests::with_starter(fixture);
    // Nothing read yet; the read is under way.
    assert_eq!(
        requests.answer(1000, status("firefox")),
        [0x87, 0, 0, 0, 0, 0]
    );
    let answer = answer_after(&mut requests, &status("firefox"), &[0x87, 0, 0, 0, 0, 0]);
    assert_eq!(answer, [&[0x87, 0, 0, 0, 0, 1][..], RECORD].concat());
    // Within the interval no read starts again.
    assert!(requests.entries["firefox"].reading.is_none());
    assert_eq!(requests.answer(1000, status("firefox")), answer);
    assert!(requests.entries["firefox"].reading.is_none());
    // Once it is over, one does.
    requests.entries.get_mut("firefox").unwrap().began = Some(Instant::now() - STATUS_INTERVAL);
    assert_eq!(requests.answer(1000, status("firefox")), answer);
    assert!(requests.entries["firefox"].reading.is_some());
    stop(&mut requests);
}

#[test]
fn only_a_record_for_the_name_that_fits_a_frame_is_relayed() {
    let mut requests = Requests::with_starter(fixture);
    for name in ["other", "big"] {
        requests.answer(1000, status(name));
        assert_eq!(
            answer_after(&mut requests, &status(name), &[0x87, 0, 0, 0, 0, 0]),
            [0x87, 0, 0, 0, 0, 3],
            "{name}"
        );
    }
    assert!(checked("firefox", RECORD).is_some());
    assert!(checked("firefox", b"format=1\nname=firefox\nkind=image\n").is_some());
    assert!(checked("mail", RECORD).is_none());
    assert!(checked("firefox", &RECORD[1..]).is_none());
}

#[test]
fn one_operation_runs_per_name_and_its_outcome_stays_to_be_read() {
    let mut requests = Requests::with_starter(fixture);
    fresh(&mut requests, "firefox");
    assert_eq!(requests.answer(1000, install("firefox")), [0x86]);
    assert_eq!(requests.answer(1000, install("firefox")), [0xff, 1]);
    let update = Request {
        verb: Verb::Update,
        name: "firefox".into(),
    };
    assert_eq!(requests.answer(1000, update), [0xff, 1]);
    // A status read runs beside it, as td-app's takes no lock.
    let answer = answer_after(&mut requests, &status("firefox"), &[0x87, 1, 0, 0, 0, 0]);
    assert_eq!(answer, [&[0x87, 1, 0, 0, 0, 1][..], RECORD].concat());
    // Another name is not held by it; its success is read back.
    fresh(&mut requests, "quick");
    assert_eq!(requests.answer(1000, install("quick")), [0x86]);
    settle(&mut requests, "quick");
    requests.answer(1000, status("quick"));
    settle(&mut requests, "quick");
    // The quiet fixture's empty record is no record.
    assert_eq!(
        requests.answer(1000, status("quick")),
        [0x87, 0, 1, 0, 0, 3]
    );
    stop(&mut requests);
}

#[test]
fn a_failed_operation_reports_its_refusal_and_frees_the_name() {
    let mut requests = Requests::with_starter(fixture);
    fresh(&mut requests, "broken");
    assert_eq!(requests.answer(1000, install("broken")), [0x86]);
    settle(&mut requests, "broken");
    let reason = b"td-app update firefox: exit status: 1: refused";
    let answer = requests.answer(1000, status("broken"));
    let mut wanted = vec![0x87, 0, 1, 1, reason.len() as u8];
    wanted.extend_from_slice(reason);
    assert_eq!(answer.get(..wanted.len()), Some(wanted.as_slice()));
    // Free again: the next request starts on a newer read.
    settle(&mut requests, "broken");
    fresh(&mut requests, "broken");
    assert_eq!(requests.answer(1000, install("broken")), [0x86]);
    stop(&mut requests);
}

#[test]
fn a_record_read_before_an_operation_ended_is_marked_older_and_read_again() {
    let mut requests = Requests::with_starter(fixture);
    requests.answer(1000, status("firefox"));
    answer_after(&mut requests, &status("firefox"), &[0x87, 0, 0, 0, 0, 0]);
    let entry = requests.entries.get_mut("firefox").unwrap();
    entry.last = Some((Verb::Install, None));
    entry.ended = Some(Instant::now());
    // The answer is the old record, marked, and a read starts at once.
    let answer = requests.answer(1000, status("firefox"));
    assert_eq!(answer, [&[0x87, 0, 1, 0, 0, 2][..], RECORD].concat());
    assert!(requests.entries["firefox"].reading.is_some());
    let fresh = answer_after(&mut requests, &status("firefox"), &answer);
    assert_eq!(fresh, [&[0x87, 0, 1, 0, 0, 1][..], RECORD].concat());
    stop(&mut requests);
}

#[test]
fn the_idlest_idle_name_makes_room_and_busy_ones_never_do() {
    let mut requests = Requests::with_starter(fixture);
    let names: Vec<String> = (0..NAMES).map(|n| format!("busy{n}")).collect();
    for name in &names {
        fresh(&mut requests, name);
        assert_eq!(requests.answer(1000, install(name)), [0x86]);
    }
    assert_eq!(requests.answer(1000, status("firefox")), [0xff, 4]);
    assert!(!requests.entries.contains_key("firefox"));
    stop(&mut requests);
    // Ended, but named within the interval: still kept.
    requests.advance();
    assert!(requests.entries.values().all(Entry::idle));
    assert_eq!(requests.answer(1000, status("firefox")), [0xff, 4]);
    // Once unnamed for the interval, the oldest is forgotten for the new
    // name.
    for (age, name) in names.iter().rev().enumerate() {
        let used = Instant::now() - STATUS_INTERVAL - Duration::from_millis(age as u64);
        requests.entries.get_mut(name).unwrap().used = used;
    }
    requests.answer(1000, status("firefox"));
    assert!(requests.entries.contains_key("firefox"));
    assert!(!requests.entries.contains_key("busy0"));
    assert_eq!(requests.entries.len(), NAMES);
    stop(&mut requests);
}

#[test]
fn an_operation_starts_only_on_a_recent_read_that_shows_no_writer() {
    let mut requests = Requests::with_starter(fixture);
    // Nothing read: refused, and the read starts.
    assert_eq!(requests.answer(1000, install("firefox")), [0xff, 5]);
    assert!(requests.entries["firefox"].reading.is_some());
    answer_after(&mut requests, &status("firefox"), &[0x87, 0, 0, 0, 0, 0]);
    let entry = requests.entries.get_mut("firefox").unwrap();
    // A writer root did not start, `0e`'s check or td-jail's.
    entry.record = Record::Read(RECORD.to_vec(), Instant::now(), true);
    entry.finished = Some(Instant::now());
    assert_eq!(requests.answer(1000, install("firefox")), [0xff, 1]);
    // Root's own operation, ended since that read, was the writer.
    let entry = requests.entries.get_mut("firefox").unwrap();
    entry.reading = None;
    entry.ended = Some(Instant::now());
    entry.began = Some(Instant::now());
    assert_eq!(requests.answer(1000, install("firefox")), [0x86]);
    stop(&mut requests);
    // A read that ended longer than FRESH ago is no ground either,
    // however long it ran.
    let mut requests = Requests::with_starter(fixture);
    fresh(&mut requests, "firefox");
    let entry = requests.entry("firefox").unwrap();
    entry.record = Record::Read(RECORD.to_vec(), Instant::now() - 2 * FRESH, false);
    entry.finished = Some(Instant::now() - FRESH);
    assert_eq!(requests.answer(1000, install("firefox")), [0xff, 5]);
    stop(&mut requests);
    // One that began long ago but ended just now is.
    let mut requests = Requests::with_starter(fixture);
    fresh(&mut requests, "firefox");
    let entry = requests.entry("firefox").unwrap();
    entry.record = Record::Read(RECORD.to_vec(), Instant::now() - 2 * FRESH, false);
    assert_eq!(requests.answer(1000, install("firefox")), [0x86]);
    stop(&mut requests);
}

#[test]
fn a_recent_failed_read_admits_an_operation_so_remove_can_repair() {
    let mut requests = Requests::with_starter(fixture);
    // The broken fixture's status fails: refused until that read ends.
    assert_eq!(requests.answer(1000, install("broken")), [0xff, 5]);
    settle(&mut requests, "broken");
    assert!(matches!(requests.entries["broken"].record, Record::Failed));
    let remove = Request {
        verb: Verb::Remove,
        name: "broken".into(),
    };
    assert_eq!(requests.answer(1000, remove.clone()), [0x86]);
    settle(&mut requests, "broken");
    // Long after, a failed read is no ground.
    requests.entries.get_mut("broken").unwrap().finished = Some(Instant::now() - FRESH);
    assert_eq!(requests.answer(1000, remove), [0xff, 5]);
    stop(&mut requests);
}

#[test]
fn a_record_held_open_after_its_operation_exits_ends_after_the_grace() {
    let mut requests = Requests::with_starter(fixture);
    let began = Instant::now();
    requests.answer(1000, status("lingering"));
    let answer = answer_after(&mut requests, &status("lingering"), &[0x87, 0, 0, 0, 0, 0]);
    // Whether it read all of it is unknown, so it is no record; the name
    // was never held until the backstop.
    assert_eq!(answer, [0x87, 0, 0, 0, 0, 3]);
    assert!(began.elapsed() >= EXIT_GRACE);
    stop(&mut requests);
}

#[test]
fn a_noisy_refusal_keeps_its_tail_and_reports_the_last_line() {
    let mut requests = Requests::with_starter(fixture);
    fresh(&mut requests, "noisy");
    assert_eq!(requests.answer(1000, install("noisy")), [0x86]);
    settle(&mut requests, "noisy");
    let entry = &requests.entries["noisy"];
    assert_eq!(
        entry.last,
        Some((Verb::Install, Some("the last line".to_string())))
    );
}

/// Writes a record on stdout and a refusal on stderr, as the operation
/// does, through `Run::start`'s own descriptors.
#[test]
#[ignore = "exec-only application request streams fixture"]
fn streams_fixture() {
    std::io::stdout().write_all(RECORD).unwrap();
    std::io::stdout().flush().unwrap();
    std::io::stderr()
        .write_all(b"td-authd: on stderr\n")
        .unwrap();
    std::process::exit(0);
}

#[test]
fn a_status_reads_stdout_and_an_operation_stderr() {
    let streams = || {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "software::tests::streams_fixture",
            "--ignored",
            "--nocapture",
        ]);
        command
    };
    let ended = |verb| {
        let mut run = Run::start(streams(), verb).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match run.advance() {
                Ended::Running => {}
                Ended::Exited(output, success) => break (output, success),
                Ended::Lost => panic!("lost"),
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    };
    let (out, success) = ended(Verb::Status);
    assert!(success);
    assert!(out.ends_with(RECORD), "{}", String::from_utf8_lossy(&out));
    assert!(!out.windows(9).any(|w| w == b"on stderr"));
    let (err, _) = ended(Verb::Remove);
    assert_eq!(err, b"td-authd: on stderr\n");
}
