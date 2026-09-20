#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The clipboard's transfer owners over real pipes and sockets: the
//! outgoing writer's nonblocking mode and its restoration, its budget,
//! deadline, cancellation and refusals; the incoming reader's EOF-only
//! admission, budget, deadline and UTF-8 check; and the two ends of one
//! socket pair driving each other under the per-step bounds.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;
use td_ui::clipboard::{Incoming, Outgoing, MAX_BYTES};

const CHUNK: usize = 16 * 1024;

fn flags(file: &impl AsRawFd) -> usize {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).unwrap();
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .unwrap();
    usize::from_str_radix(flags, 8).unwrap()
}

#[test]
fn a_partial_read_waits_for_eof_and_the_text_arrives_whole() {
    let (mut incoming, mut peer) = Incoming::begin(0).unwrap();
    assert!(!incoming.step(1).unwrap());
    peer.write_all(&[0xc3]).unwrap();
    assert!(!incoming.step(2).unwrap());
    peer.write_all(&[0xa9, b'\r']).unwrap();
    assert!(!incoming.step(3).unwrap());
    peer.write_all(b"\n").unwrap();
    assert!(!incoming.step(4).unwrap());
    drop(peer);
    assert!(incoming.step(5).unwrap());
    assert!(incoming.step(u64::MAX).unwrap(), "complete stays complete");
    // The bytes are handed on as they came: a line ending is the
    // consumer's to normalize.
    assert_eq!(incoming.finish().unwrap(), "é\r\n");
}

#[test]
fn a_stalled_read_expires_and_no_prefix_is_admitted() {
    let (mut incoming, mut peer) = Incoming::begin(10).unwrap();
    peer.write_all(b"prefix").unwrap();
    assert!(!incoming.step(5009).unwrap());
    assert!(!incoming.expired(5009));
    assert!(incoming.expired(5010));
    assert!(incoming.step(5010).is_err());
    drop(peer);
    assert!(incoming.step(5011).is_err(), "failed stays failed");
    assert!(incoming.finish().is_err());
    let (incoming, _peer) = Incoming::begin(0).unwrap();
    assert!(incoming.finish().is_err(), "no EOF, no text");
    assert!(Incoming::begin(u64::MAX).is_err(), "clock exhausted");
}

#[test]
fn oversized_or_malformed_input_is_refused_whole() {
    /// Feeds `text` a chunk a step, then EOF, and steps to the end:
    /// what `finish` answers, and whether a step refused it first.
    fn feed(text: &[u8]) -> (bool, io::Result<String>) {
        let (mut incoming, mut peer) = Incoming::begin(0).unwrap();
        let mut refused = false;
        for (now, chunk) in text.chunks(CHUNK).enumerate() {
            peer.write_all(chunk).unwrap();
            if incoming.step(now as u64).is_err() {
                refused = true;
                break;
            }
        }
        drop(peer);
        if !refused {
            while !incoming.step(100).unwrap() {}
        }
        (refused, incoming.finish())
    }
    // One byte past the budget is refused as it arrives, and nothing of
    // it is handed on.
    let (refused, text) = feed(&vec![b'x'; MAX_BYTES + 1]);
    assert!(refused);
    let error = text.unwrap_err().to_string();
    assert!(error.contains("no successful EOF"), "{error}");
    // Bytes that are not UTF-8 reach EOF and are refused whole there.
    let (refused, text) = feed(&[0xc3]);
    assert!(!refused);
    let error = text.unwrap_err().to_string();
    assert!(error.contains("UTF-8"), "{error}");
    // Exactly the budget arrives whole.
    let (refused, text) = feed(&vec![b'x'; MAX_BYTES]);
    assert!(!refused);
    assert_eq!(text.unwrap(), "x".repeat(MAX_BYTES));
}

#[test]
fn the_pipe_writer_is_nonblocking_and_restores_the_shared_flags() {
    let (mut reader, writer) = io::pipe().unwrap();
    let mirror = writer.try_clone().unwrap();
    let original = flags(&mirror);
    let mut outgoing = Outgoing::begin(OwnedFd::from(writer), Arc::from("é\n"), 0).unwrap();
    assert_eq!(flags(&mirror), original | 0o4000);
    assert!(outgoing.step(0).unwrap());
    assert_eq!(flags(&mirror), original);
    drop(mirror);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, "é\n".as_bytes());
}

#[test]
fn a_stalled_pipe_times_out_and_cancel_or_drop_restores_the_flags() {
    for action in 0..3 {
        let (_reader, writer) = io::pipe().unwrap();
        let mirror = writer.try_clone().unwrap();
        let original = flags(&mirror);
        let mut outgoing =
            Outgoing::begin(OwnedFd::from(writer), Arc::from("x".repeat(MAX_BYTES)), 0).unwrap();
        assert!(!outgoing.step(0).unwrap());
        assert!(!outgoing.step(4999).unwrap());
        assert!(outgoing.expired(5000));
        match action {
            0 => assert!(outgoing.step(5000).is_err()),
            1 => outgoing.cancel().unwrap(),
            _ => drop(outgoing),
        }
        assert_eq!(flags(&mirror), original);
    }
}

#[test]
fn the_two_ends_of_a_socket_pair_drive_each_other_under_the_step_bounds() {
    let (mut incoming, peer) = Incoming::begin(0).unwrap();
    let mut outgoing =
        Outgoing::begin(OwnedFd::from(peer), Arc::from("x".repeat(MAX_BYTES)), 0).unwrap();
    assert!(!outgoing.step(0).unwrap());
    assert!(!incoming.step(0).unwrap());
    let mut done = false;
    for now in 1..200 {
        if !done {
            done = outgoing.step(now).unwrap();
        }
        if incoming.step(now).unwrap() {
            assert!(done);
            let text = incoming.finish().unwrap();
            assert_eq!(text.len(), MAX_BYTES);
            assert!(text.bytes().all(|byte| byte == b'x'));
            return;
        }
    }
    panic!("the bounded socket transfer did not finish");
}

#[test]
fn the_fourth_write_finishes_in_the_same_turn_before_the_deadline() {
    let (mut reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut outgoing =
        Outgoing::begin(OwnedFd::from(writer), Arc::from("x".repeat(4 * CHUNK)), 0).unwrap();
    assert!(outgoing.step(4999).unwrap());
    assert!(outgoing.step(5000).unwrap());
    let mut received = Vec::new();
    reader.read_to_end(&mut received).unwrap();
    assert_eq!(received, vec![b'x'; 4 * CHUNK]);
}

#[test]
fn a_dropped_reader_breaks_the_writer_and_other_destinations_are_refused() {
    let (reader, writer) = io::pipe().unwrap();
    drop(reader);
    let mut outgoing = Outgoing::begin(OwnedFd::from(writer), Arc::from("x"), 0).unwrap();
    assert_eq!(
        outgoing.step(0).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    let (reader, _writer) = io::pipe().unwrap();
    assert!(Outgoing::begin(OwnedFd::from(reader), Arc::from("x"), 0).is_err());
    for path in [
        "/dev/null",
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
    ] {
        let file = File::open(path).unwrap();
        assert!(Outgoing::begin(OwnedFd::from(file), Arc::from("x"), 0).is_err());
    }
    let (_reader, writer) = io::pipe().unwrap();
    assert!(
        Outgoing::begin(
            OwnedFd::from(writer),
            Arc::from("x".repeat(MAX_BYTES + 1)),
            0
        )
        .is_err(),
        "the text budget"
    );
}

#[test]
fn a_clock_reversal_and_the_terminal_states_cannot_revive_a_transfer() {
    let (mut incoming, peer) = Incoming::begin(10).unwrap();
    assert!(incoming.step(9).is_err());
    drop(peer);
    assert!(incoming.step(10).is_err());
    assert!(incoming.finish().is_err());
    let (_reader, writer) = io::pipe().unwrap();
    let mirror = writer.try_clone().unwrap();
    let original = flags(&mirror);
    let mut outgoing = Outgoing::begin(OwnedFd::from(writer), Arc::from(""), 10).unwrap();
    assert!(outgoing.step(9).is_err());
    assert!(outgoing.step(10).is_err());
    assert_eq!(flags(&mirror), original);
    assert!(outgoing.cancel().is_ok());
    let (mut incoming, peer) = Incoming::begin(0).unwrap();
    let mut outgoing = Outgoing::begin(OwnedFd::from(peer), Arc::from(""), 0).unwrap();
    assert!(outgoing.step(0).unwrap());
    assert!(outgoing.step(u64::MAX).unwrap());
    assert!(incoming.step(1).unwrap());
    assert_eq!(incoming.finish().unwrap(), "");
}

#[test]
fn an_originally_nonblocking_destination_keeps_its_flags() {
    let (mut reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    writer.set_nonblocking(true).unwrap();
    let mirror = writer.try_clone().unwrap();
    let original = flags(&mirror);
    assert_ne!(original & 0o4000, 0);
    let mut outgoing = Outgoing::begin(writer.into(), Arc::from("x"), 0).unwrap();
    assert!(outgoing.step(0).unwrap());
    assert_eq!(flags(&mirror), original);
    drop(mirror);
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert_eq!(text, "x");
}
