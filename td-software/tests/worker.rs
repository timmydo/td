//! The worker's conversation with a compositor the test plays.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread;

use td_software::snapshot::{Operation, Record};
use td_software::worker::{self, Failure, Job, Reply, Worker};

/// A compositor that reads one line and writes `answer`.
fn serve(path: &Path, answer: Vec<u8>) -> thread::JoinHandle<String> {
    let lines = serve_each(path, vec![answer]);
    thread::spawn(move || lines.join().unwrap().concat())
}

/// A compositor that takes one conversation per answer, in turn.
fn serve_each(path: &Path, answers: Vec<Vec<u8>>) -> thread::JoinHandle<Vec<String>> {
    let listener = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        let mut lines = Vec::new();
        for answer in answers {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            stream.read_to_string(&mut line).unwrap();
            let _ = stream.write_all(&answer);
            lines.push(line);
        }
        lines
    })
}

#[test]
fn a_status_is_one_line_and_one_bounded_answer() {
    let dir = td_fs::TempDir::new("td-software-worker").unwrap();
    let socket = dir.path().join("control");
    let record = "format=1\nname=firefox\nkind=image\n";
    let answer = format!("ok\noperation=remove\nlast=none\nrecord=current\n{record}");
    let compositor = serve(&socket, answer.into_bytes());
    let reply = worker::run(Some(&socket), Job::Status("firefox".into()));
    assert_eq!(compositor.join().unwrap(), "status firefox\n");
    let Reply::Status(name, snapshot) = reply else {
        panic!("{reply:?}");
    };
    assert_eq!(name, "firefox");
    let snapshot = (*snapshot).unwrap();
    assert_eq!(snapshot.operation, Some(Operation::Remove));
    assert!(matches!(snapshot.record, Record::Current(_)));
}

#[test]
fn refusals_and_oversized_or_cut_answers_are_errors() {
    let dir = td_fs::TempDir::new("td-software-worker").unwrap();
    // An `error` says root did not act; the others leave it open.
    for (index, (answer, expected, definite)) in [
        (
            b"error an operation on firefox is already running\n".to_vec(),
            "an operation on firefox is already running",
            true,
        ),
        (
            b"unavailable the program authority did not answer in time\n".to_vec(),
            "the program authority did not answer in time",
            false,
        ),
        (
            b"ok".to_vec(),
            "the compositor's answer was cut short",
            false,
        ),
        (
            vec![b'x'; td_software::snapshot::MAX_ANSWER_BYTES + 2],
            "the compositor's answer is too large",
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let socket = dir.path().join(format!("control-{index}"));
        let compositor = serve(&socket, answer);
        let reply = worker::run(
            Some(&socket),
            Job::Operation(Operation::DeleteData, "firefox".into()),
        );
        assert_eq!(compositor.join().unwrap(), "delete-data firefox\n");
        assert_eq!(
            reply,
            Reply::Operation(
                Operation::DeleteData,
                "firefox".into(),
                Err(Failure {
                    reason: expected.into(),
                    definite,
                })
            )
        );
    }
}

#[test]
fn the_thread_answers_in_order() {
    let dir = td_fs::TempDir::new("td-software-worker").unwrap();
    let socket = dir.path().join("control");
    let compositor = serve_each(
        &socket,
        vec![
            b"ok\n".to_vec(),
            b"error mail has no launcher entry\n".to_vec(),
        ],
    );
    let worker = Worker::start(Some(socket)).unwrap();
    worker.send(Job::Launch("firefox".into())).unwrap();
    worker.send(Job::Launch("mail".into())).unwrap();
    let mut replies = Vec::new();
    while replies.len() < 2 {
        match worker.try_recv().unwrap() {
            Some(reply) => replies.push(reply),
            None => thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    assert_eq!(
        compositor.join().unwrap(),
        ["launch firefox\n", "launch mail\n"]
    );
    assert_eq!(
        replies,
        [
            Reply::Launch("firefox".into(), Ok(())),
            Reply::Launch("mail".into(), Err("mail has no launcher entry".into())),
        ]
    );
}

#[test]
fn a_compositor_that_never_answers_is_a_timeout_not_an_os_error() {
    let dir = td_fs::TempDir::new("td-software-worker").unwrap();
    let socket = dir.path().join("control");
    let listener = UnixListener::bind(&socket).unwrap();
    // Accepted and read, never answered, for longer than the bound.
    let silent = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        stream.read_to_string(&mut line).unwrap();
        thread::sleep(std::time::Duration::from_secs(8));
        drop(stream);
    });
    let reply = worker::run(Some(&socket), Job::Status("firefox".into()));
    assert_eq!(
        reply,
        Reply::Status(
            "firefox".into(),
            Box::new(Err("the compositor did not answer in time".into()))
        )
    );
    silent.join().unwrap();
}
